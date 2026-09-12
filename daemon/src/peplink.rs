// Peplink / Pepwave — vendor 2 of the managed-router flow (routers.rs; owner: "Cradlepoint first,
// Peplink right after"). The hub signs in to the router's local "Router API" (fw 8.x), reads it on
// the poll cadence and reports for it exactly as it does for a Cradlepoint: the Snapshot and the
// `modem.measurement` params are the shared shapes in routers.rs, so the cloud and the app never
// learn which vendor answered.
//
// Contract — a straight port of the app's drivers/peplink.ts, which was pinned to the documented
// response shapes (fw 8.5.0 PDF), NOT to a bench capture; there is no Peplink on the bench:
//   POST /api/login  {username, password}   → cookie session (Set-Cookie)
//   GET  /api/status.system.info            → identity
//   GET  /api/status.wan.connection         → WAN map + the cellular block
//   GET  /api/info.location                 → GPS
// Every response is `{stat:'ok', response:{…}}`; failures are `{stat:'fail', message}` (and a
// dropped session answers `stat:'fail'` with code 401 / "Unauthorized", or HTTP 401 — either way
// the client signs in again once and retries). https on 443 with a factory self-signed
// certificate; plain http only when the owner pointed the hub at :80.
//
// What the LOCAL api does not offer, and the hub therefore refuses plainly rather than guesses:
// APN read/write and reboot (both live behind the write-capable admin API the app driver marks
// unsupported, capabilities.apnRead/apnWrite/reboot = false), and a router-side GPS switch — a
// Peplink's GPS is either on the model or not (a Balance One has no GPS hardware at all; a MAX
// does), so "GPS enabled" for a Peplink is hub-side reporting only: the hub reports fixes when the
// owner asked for them AND the router returns one.

use serde_json::Value;
use tokio::sync::Mutex;

use crate::gps::{parse_peplink_gps, GpsFix};
use crate::hub_config::RouterConfig;
use crate::routers::{as_f64, reachability, str_of, ModemStatus, Probe, WanStatus};

/// PURE: the base URL. Peplink admin is https by default (self-signed); http only on :80. Port 0
/// means "unset" and is 443 — the same rule as the app's peplinkBase.
pub fn peplink_base(host: &str, port: u16) -> String {
    let p = if port == 0 { 443 } else { port };
    let scheme = if p == 80 { "http" } else { "https" };
    if p == 80 || p == 443 {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:{p}")
    }
}

// --- Pure parsers (mirror peplink.ts; fixtures = peplink.test.ts) ---------------------------------

/// The envelope: `{stat:'ok', response}` → the payload; `{stat:'fail', message}` → the message.
pub fn peplink_response(body: &Value) -> Result<&Value, String> {
    let Some(obj) = body.as_object() else {
        return Err("the router did not answer with JSON".into());
    };
    match obj.get("stat").and_then(|s| s.as_str()) {
        Some("ok") => Ok(obj.get("response").unwrap_or(body)),
        Some("fail") => Err(str_of(obj.get("message")).unwrap_or_else(|| "the router refused the request".into())),
        _ => Err("unexpected response from the router".into()),
    }
}

/// Does this failure mean "you are not signed in (any more)"? The documented shape is
/// `{stat:'fail', code:401, message:'Unauthorized'}`; the message is matched loosely because the
/// wording is the one thing firmware releases change.
pub fn session_expired(body: &Value) -> bool {
    let Some(obj) = body.as_object() else { return false };
    if obj.get("stat").and_then(|s| s.as_str()) != Some("fail") {
        return false;
    }
    if obj.get("code").and_then(as_f64) == Some(401.0) {
        return true;
    }
    let msg = str_of(obj.get("message")).unwrap_or_default().to_ascii_lowercase();
    msg.contains("unauthori") || msg.contains("login") || msg.contains("session")
}

/// `response` when the envelope is present, else the body itself (the parsers accept both, as the
/// app's do, so a test fixture and a live payload read the same).
fn payload(body: &Value) -> Option<&Value> {
    let r = body.get("response").unwrap_or(body);
    if r.is_object() { Some(r) } else { None }
}

/// `GET /api/status.system.info` → identity.
pub fn parse_probe(body: &Value) -> Option<Probe> {
    let r = payload(body)?;
    let model = str_of(r.get("productName")).or_else(|| str_of(r.get("model")));
    let firmware = str_of(r.get("firmwareVersion")).or_else(|| str_of(r.get("firmware")));
    let mac = str_of(r.get("mac"));
    if model.is_none() && firmware.is_none() && mac.is_none() {
        return None;
    }
    Some(Probe { model, firmware, mac })
}

/// The WAN map: every object value except `order` (the display ordering array).
fn wan_entries(r: &Value) -> Vec<&Value> {
    r.as_object()
        .map(|m| m.iter().filter(|(k, v)| k.as_str() != "order" && v.is_object()).map(|(_, v)| v).collect())
        .unwrap_or_default()
}

/// "Connected" is `statusLed: 'green'` or a message starting `Connected` — firmware varies, so
/// both are accepted.
fn is_up(w: &Value) -> bool {
    w.get("statusLed").and_then(|v| v.as_str()) == Some("green")
        || w.get("message")
            .and_then(|v| v.as_str())
            .map(|m| m.to_ascii_lowercase().starts_with("connected"))
            .unwrap_or(false)
}

/// `GET /api/status.wan.connection` → the ACTIVE uplink, classified by its `type`.
pub fn parse_wan(body: &Value) -> Option<WanStatus> {
    let entries = wan_entries(payload(body)?);
    if entries.is_empty() {
        return None;
    }
    let Some(w) = entries.iter().copied().find(|w| is_up(w)) else {
        return Some(WanStatus { wan: "none".into(), up: false, ip: None });
    };
    let t = w.get("type").and_then(|v| v.as_str()).unwrap_or("").to_ascii_lowercase();
    let wan = if t.contains("cellular") || t.contains("modem") {
        "lte"
    } else if t.contains("wifi") {
        "repeater"
    } else {
        "wired"
    };
    Some(WanStatus { wan: wan.into(), up: true, ip: str_of(w.get("ip")) })
}

/// Cellular detail from the same payload — Peplink nests it under `cellular`. None on a model
/// with no modem (a Balance One).
pub fn parse_modem(body: &Value) -> Option<ModemStatus> {
    let entry = wan_entries(payload(body)?)
        .into_iter()
        .find(|w| w.get("cellular").map(|c| c.is_object()).unwrap_or(false))?;
    let c = entry.get("cellular")?;
    let sim_raw = str_of(c.get("simStatus")).or_else(|| str_of(c.get("sim"))).unwrap_or_default().to_ascii_lowercase();
    let sim = match sim_raw.as_str() {
        "sim card is ready" | "ready" => "ok",
        "no sim card detected" | "no sim" => "missing",
        "sim card is locked" | "locked" => "locked",
        _ => "unknown",
    };
    let signal = c.get("signal").or_else(|| c.get("signalLevel")).filter(|s| s.is_object());
    let level = |k: &str| signal.and_then(|s| s.get(k)).or_else(|| c.get(k)).and_then(as_f64);
    Some(ModemStatus {
        sim: sim.into(),
        carrier: str_of(c.get("carrier")).or_else(|| str_of(c.get("operator"))),
        mode: str_of(c.get("dataTechnology")).or_else(|| str_of(c.get("network"))),
        rssi: level("rssi"),
        rsrp: level("rsrp"),
        rsrq: level("rsrq"),
        sinr: level("sinr"),
        connected: Some(is_up(entry)),
        ip: str_of(entry.get("ip")),
        // Usage lives in InControl2, not the local API — no counters, so no dataMb / plan burn.
        tx_bytes: None,
        rx_bytes: None,
    })
}

/// `GET /api/info.location` → whether the unit reports GPS at all (`gps`, when present — a
/// Balance One has no GPS hardware and says so) and the fix, if it has a lock. Accepts the full
/// envelope or the unwrapped `response`, like every parser here.
pub fn parse_location(body: &Value) -> (Option<bool>, Option<GpsFix>) {
    let gps = payload(body).and_then(|r| r.get("gps")).and_then(|v| v.as_bool());
    (gps, parse_peplink_gps(body))
}

// --- Transport ------------------------------------------------------------------------------------

/// One router, one cookie session. The cookie is taken on the first call and kept for the life of
/// the client (one per poll, one per action); a call that comes back "not signed in" signs in
/// again once and retries, so a router that expired the session between polls costs one round
/// trip, not a failed read.
pub struct Peplink<'a> {
    client: &'a reqwest::Client,
    base: String,
    user: String,
    pass: String,
    cookie: Mutex<Option<String>>,
}

impl<'a> Peplink<'a> {
    pub fn new(client: &'a reqwest::Client, host: &str, port: u16, user: &str, pass: &str) -> Self {
        Peplink {
            client,
            base: peplink_base(host.trim(), port),
            user: if user.trim().is_empty() { "admin".into() } else { user.trim().into() },
            pass: pass.to_string(),
            cookie: Mutex::new(None),
        }
    }

    pub fn for_router(client: &'a reqwest::Client, cfg: &RouterConfig) -> Self {
        Peplink::new(client, &cfg.host, cfg.port, &cfg.username, &cfg.password)
    }

    /// Test seam: a mock router serves plain http on an ephemeral port, which peplink_base would
    /// (correctly) address as https.
    #[cfg(test)]
    fn at_base(client: &'a reqwest::Client, base: String, user: &str, pass: &str) -> Self {
        Peplink { client, base, user: user.into(), pass: pass.into(), cookie: Mutex::new(None) }
    }

    /// `POST /api/login` → the session cookie. A refused sign-in is the one error the owner can
    /// fix, so it is named as such; the credential itself never appears in a message.
    pub async fn login(&self) -> Result<(), String> {
        let res = self
            .client
            .post(format!("{}/api/login", self.base))
            .json(&serde_json::json!({ "username": self.user, "password": self.pass }))
            .send()
            .await
            .map_err(reachability)?;
        let code = res.status().as_u16();
        if code == 401 || code == 403 {
            return Err("the router refused the sign-in — check the admin username and password".into());
        }
        if !res.status().is_success() {
            return Err(format!("the router answered HTTP {code}"));
        }
        // Read the cookie BEFORE the body consumes the response.
        let cookie: Vec<String> = res
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.split(';').next())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let body: Value = res.json().await.map_err(|_| "the router did not answer with JSON".to_string())?;
        if let Err(why) = peplink_response(&body) {
            return Err(format!("the router refused the sign-in — {why}"));
        }
        // An absent cookie is not fatal — some firmware accepts the session implicitly for status
        // reads — so keep going and let the first read fail if it must.
        *self.cookie.lock().await = if cookie.is_empty() { Some(String::new()) } else { Some(cookie.join("; ")) };
        Ok(())
    }

    async fn get_once(&self, path: &str) -> Result<Result<Value, Value>, String> {
        let mut req = self.client.get(format!("{}{path}", self.base));
        if let Some(c) = self.cookie.lock().await.as_deref().filter(|c| !c.is_empty()) {
            req = req.header(reqwest::header::COOKIE, c);
        }
        let res = req.send().await.map_err(reachability)?;
        let code = res.status().as_u16();
        if code == 401 || code == 403 {
            // Expired (or never established) — reported as the documented failure envelope so one
            // path handles both.
            return Ok(Err(serde_json::json!({ "stat": "fail", "code": 401, "message": "Unauthorized" })));
        }
        if !res.status().is_success() {
            return Err(format!("the router answered HTTP {code}"));
        }
        let body: Value = res.json().await.map_err(|_| "the router did not answer with JSON".to_string())?;
        if session_expired(&body) {
            return Ok(Err(body));
        }
        Ok(Ok(body))
    }

    /// GET a status path; returns the unwrapped `response`. Signs in first when there is no
    /// session yet, and once more when the router says the session is gone.
    pub async fn get(&self, path: &str) -> Result<Value, String> {
        if self.cookie.lock().await.is_none() {
            self.login().await?;
        }
        let body = match self.get_once(path).await? {
            Ok(b) => b,
            Err(_expired) => {
                self.login().await?;
                match self.get_once(path).await? {
                    Ok(b) => b,
                    Err(_) => return Err("the router refused the sign-in — check the admin username and password".into()),
                }
            }
        };
        peplink_response(&body).cloned()
    }

    /// Prove the sign-in and read identity.
    pub async fn probe(&self) -> Result<Probe, String> {
        let info = self.get("/api/status.system.info").await?;
        parse_probe(&info).ok_or_else(|| "the router answered, but its model could not be read".into())
    }

    /// The raw WAN map — both `parse_modem` and `parse_wan` read it, one request.
    pub async fn wan_connection(&self) -> Result<Value, String> {
        self.get("/api/status.wan.connection").await
    }

    /// `(gps reported?, fix)`; `Ok((_, None))` = reachable, no lock (or no GPS hardware).
    pub async fn location(&self) -> Result<(Option<bool>, Option<GpsFix>), String> {
        Ok(parse_location(&self.get("/api/info.location").await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Fixtures = the app's peplink.test.ts, ported verbatim.

    #[test]
    fn envelope_recognises_the_failure_shape_and_passes_success_through() {
        assert_eq!(peplink_response(&json!({ "stat": "ok", "response": {} })).unwrap(), &json!({}));
        assert_eq!(peplink_response(&json!({ "stat": "fail", "message": "Invalid password" })).unwrap_err(), "Invalid password");
        assert!(peplink_response(&json!({ "stat": "fail" })).unwrap_err().contains("refused"));
        assert!(peplink_response(&json!(null)).unwrap_err().contains("did not answer with JSON"));
        assert!(peplink_response(&json!({ "nothing": true })).unwrap_err().to_ascii_lowercase().contains("unexpected"));
    }

    #[test]
    fn an_expired_session_is_told_apart_from_any_other_fault() {
        assert!(session_expired(&json!({ "stat": "fail", "code": 401, "message": "Unauthorized" })));
        assert!(session_expired(&json!({ "stat": "fail", "message": "Please login first" })));
        assert!(!session_expired(&json!({ "stat": "fail", "message": "Invalid password" })));
        assert!(!session_expired(&json!({ "stat": "ok", "response": {} })));
    }

    #[test]
    fn probe_reads_model_and_firmware() {
        let p = parse_probe(&json!({ "stat": "ok", "response": { "productName": "Balance One", "firmwareVersion": "8.5.2", "mac": "00:11:22:33:44:55" } })).unwrap();
        assert_eq!(p, Probe { model: Some("Balance One".into()), firmware: Some("8.5.2".into()), mac: Some("00:11:22:33:44:55".into()) });
        let alt = parse_probe(&json!({ "response": { "model": "MAX Transit", "firmware": "8.4.0" } })).unwrap();
        assert_eq!((alt.model.as_deref(), alt.firmware.as_deref()), (Some("MAX Transit"), Some("8.4.0")));
        assert!(parse_probe(&json!({ "response": {} })).is_none());
    }

    fn wired() -> Value {
        json!({ "name": "WAN 1", "type": "ethernet", "message": "Connected", "statusLed": "green", "ip": "10.0.0.5" })
    }
    fn down_cell() -> Value {
        json!({ "name": "Cellular", "type": "cellular", "message": "Disconnected", "statusLed": "red" })
    }

    #[test]
    fn wan_picks_the_active_uplink_and_classifies_it() {
        let w = parse_wan(&json!({ "stat": "ok", "response": { "1": wired(), "2": down_cell(), "order": [1, 2] } })).unwrap();
        assert_eq!(w, WanStatus { wan: "wired".into(), up: true, ip: Some("10.0.0.5".into()) });
    }

    #[test]
    fn wan_classifies_cellular_as_lte_and_wifi_as_repeater() {
        let cell = json!({ "type": "cellular", "message": "Connected", "statusLed": "green" });
        assert_eq!(parse_wan(&json!({ "response": { "1": cell } })).unwrap().wan, "lte");
        let wifi = json!({ "type": "wifi", "statusLed": "green" });
        assert_eq!(parse_wan(&json!({ "response": { "1": wifi } })).unwrap().wan, "repeater");
    }

    #[test]
    fn wan_reports_none_when_nothing_is_up_and_nothing_without_a_map() {
        assert_eq!(parse_wan(&json!({ "response": { "1": down_cell() } })).unwrap(), WanStatus { wan: "none".into(), up: false, ip: None });
        assert!(parse_wan(&json!({ "response": {} })).is_none());
        assert!(parse_wan(&json!(null)).is_none());
    }

    #[test]
    fn modem_normalizes_the_nested_cellular_block() {
        let body = json!({
            "stat": "ok",
            "response": {
                "1": { "type": "ethernet", "statusLed": "green" },
                "2": {
                    "type": "cellular", "message": "Connected", "statusLed": "green", "ip": "100.64.1.2",
                    "cellular": { "simStatus": "SIM card is ready", "carrier": "T-Mobile", "dataTechnology": "LTE",
                                  "signal": { "rssi": -70, "rsrp": -95, "sinr": 9 } }
                }
            }
        });
        assert_eq!(
            parse_modem(&body).unwrap(),
            ModemStatus {
                sim: "ok".into(),
                carrier: Some("T-Mobile".into()),
                mode: Some("LTE".into()),
                rssi: Some(-70.0),
                rsrp: Some(-95.0),
                rsrq: None,
                sinr: Some(9.0),
                connected: Some(true),
                ip: Some("100.64.1.2".into()),
                tx_bytes: None,
                rx_bytes: None,
            }
        );
    }

    #[test]
    fn modem_maps_sim_problems_and_is_none_on_a_model_with_no_modem() {
        assert_eq!(parse_modem(&json!({ "response": { "1": { "cellular": { "simStatus": "No SIM card detected" } } } })).unwrap().sim, "missing");
        assert_eq!(parse_modem(&json!({ "response": { "1": { "cellular": { "simStatus": "SIM card is locked" } } } })).unwrap().sim, "locked");
        assert!(parse_modem(&json!({ "response": { "1": { "type": "ethernet", "statusLed": "green" } } })).is_none());
    }

    #[test]
    fn location_reads_the_gps_flag_beside_the_fix() {
        let (gps, fix) = parse_location(&json!({ "stat": "ok", "response": { "gps": true, "location": { "latitude": 37.8044, "longitude": -122.2712 } } }));
        assert_eq!(gps, Some(true));
        assert_eq!(fix.map(|f| (f.lat, f.lon)), Some((37.8044, -122.2712)));
        let (gps, fix) = parse_location(&json!({ "stat": "ok", "response": { "gps": false } }));
        assert_eq!((gps, fix), (Some(false), None));
        // The transport hands the parser the unwrapped `response`; it reads the same.
        let (gps, fix) = parse_location(&json!({ "gps": true, "location": { "latitude": 37.8, "longitude": -122.3 } }));
        assert_eq!((gps, fix.map(|f| f.lat)), (Some(true), Some(37.8)));
    }

    #[test]
    fn the_base_url_is_https_unless_pointed_at_80() {
        assert_eq!(peplink_base("192.168.50.1", 0), "https://192.168.50.1");
        assert_eq!(peplink_base("192.168.50.1", 443), "https://192.168.50.1");
        assert_eq!(peplink_base("192.168.50.1", 80), "http://192.168.50.1");
        assert_eq!(peplink_base("192.168.50.1", 8443), "https://192.168.50.1:8443");
    }

    /// The measurement a Peplink produces goes through the SAME modem_params as a Cradlepoint —
    /// the names the cloud reads are vendor-blind, and the counters a Peplink lacks simply do not
    /// appear rather than reading as zero.
    #[test]
    fn measurement_params_are_vendor_blind() {
        let body = json!({ "response": { "1": { "type": "cellular", "statusLed": "green", "ip": "100.64.1.2",
            "cellular": { "simStatus": "ready", "carrier": "T-Mobile", "dataTechnology": "LTE", "signal": { "rsrp": -95, "sinr": 9 } } } } });
        let m = parse_modem(&body).unwrap();
        let w = parse_wan(&body);
        let p = Probe { model: Some("MAX Transit".into()), firmware: Some("8.5.2".into()), mac: None };
        let params = crate::routers::modem_params(&m, w.as_ref(), Some(&p), None);
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("up"), Some("1"));
        assert_eq!(get("rsrp"), Some("-95"));
        assert_eq!(get("carrier"), Some("T-Mobile"));
        assert_eq!(get("sim"), Some("ok"));
        assert_eq!(get("wan"), Some("lte"));
        assert_eq!(get("ip"), Some("100.64.1.2"));
        assert_eq!(get("model"), Some("MAX Transit"));
        assert_eq!(get("dataMb"), None);
        assert_eq!(get("wanKb_cellular"), None);
    }

    /// A mock router: `/api/login` hands out a fresh `bauth` cookie per sign-in; the FIRST session
    /// is treated as expired on its first status read, so the client must sign in again and retry.
    #[tokio::test]
    async fn peplink_client_signs_in_keeps_the_cookie_and_relogins_once_on_an_expired_session() {
        use axum::{extract::State, routing::{get, post}, Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let logins = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/api/login",
                post(|State(n): State<Arc<AtomicUsize>>, Json(body): Json<Value>| async move {
                    if body.get("username").and_then(|v| v.as_str()) == Some("admin") && body.get("password").and_then(|v| v.as_str()) == Some("secret") {
                        let id = n.fetch_add(1, Ordering::SeqCst) + 1;
                        (
                            [(axum::http::header::SET_COOKIE, format!("bauth=s{id}; Path=/; HttpOnly"))],
                            Json(json!({ "stat": "ok", "response": { "permission": { "GET": true } } })),
                        )
                            .into_response()
                    } else {
                        Json(json!({ "stat": "fail", "message": "Invalid password" })).into_response()
                    }
                }),
            )
            .route(
                "/api/status.system.info",
                get(|headers: axum::http::HeaderMap| async move {
                    let cookie = headers.get("cookie").and_then(|v| v.to_str().ok()).unwrap_or("");
                    if cookie.contains("bauth=s1") || cookie.is_empty() {
                        Json(json!({ "stat": "fail", "code": 401, "message": "Unauthorized" }))
                    } else {
                        Json(json!({ "stat": "ok", "response": { "productName": "MAX Transit", "firmwareVersion": "8.5.2", "mac": "aa" } }))
                    }
                }),
            )
            .with_state(logins.clone());
        use axum::response::IntoResponse;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = crate::routers::lan_client();

        let base = format!("http://127.0.0.1:{port}");
        let ok = Peplink::at_base(&client, base.clone(), "admin", "secret");
        let p = ok.probe().await.unwrap();
        assert_eq!(p.model.as_deref(), Some("MAX Transit"));
        assert_eq!(logins.load(Ordering::SeqCst), 2, "s1 was refused, so exactly one re-login");
        // The second session is kept: another read costs no sign-in.
        ok.probe().await.unwrap();
        assert_eq!(logins.load(Ordering::SeqCst), 2);

        let bad = Peplink::at_base(&client, base, "admin", "wrong");
        let why = bad.probe().await.unwrap_err();
        assert!(why.contains("refused the sign-in") && why.contains("Invalid password"), "{why}");
    }
}
