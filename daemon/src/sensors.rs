// Sensor wiring — the hub finishes a sleepy sensor's setup (owner, 2026-09-12: "make it wait and
// wire automatically. the hub should be doing this", and "once they are configured, the bluetooth
// should turn off until factory reset"; then: "fix the provisioning process, i told you i didn't
// want to have to do that second stupid step").
//
// A battery Shelly (Flood, H&T, …) is provisioned over Bluetooth from the phone, joins the vessel's
// Wi-Fi, and goes straight back to sleep — often before the phone has learned its address, and
// sometimes while the phone is still on a different network. Its ALERT DESTINATIONS (webhooks) can
// only be written over HTTP while it is awake, which is how a sensor ended up "added" with nothing
// wired and a flood trip that reached nobody. So the app hands the job to the hub: what the sensor
// is called, where it might answer, which hooks it must carry, and whether to switch its Bluetooth
// off once done. The hub is always aboard and always on the LAN; it hunts the sensor every few
// seconds for as long as it takes (a sensor wakes on its button, on an event, and on its own
// schedule), and the moment it answers, the hub writes and VERIFIES the hooks, switches BLE off,
// and records `wired` — which is what the wizard waits for before it lets the user leave.
//
// The Shelly Gen2 RPC client here mirrors the app's shellyRpc.ts (digest auth over the JSON body,
// SHA-256, `dummy_method:dummy_uri` HA2 — verified on hardware there) and its sleepyProvision.ts
// (write, READ BACK, one-url-per-hook fallback for the Flood G4, which keeps only the first url).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// How often the hub looks for a pending sensor. A woken sleepy sensor stays up for tens of
/// seconds, so a few seconds between attempts catches every wake without hammering the LAN.
pub const HUNT_SECS: u64 = 4;
/// Per-attempt HTTP timeout: an asleep sensor does not answer at all; waiting long buys nothing.
const RPC_TIMEOUT: Duration = Duration::from_secs(3);

/// One hook the sensor must carry: an event and the urls it fires, hub url FIRST (the Flood G4
/// keeps only the first url of a hook — the hub, the path that works with the uplink down, must
/// never be the one that falls off).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct DesiredHook {
    pub event: String,
    pub urls: Vec<String>,
}

/// A wiring job as the app hands it over.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SensorJob {
    /// The Shelly device id (`shellyfloodg4-d885acea3914`) — what `Shelly.GetDeviceInfo` must say.
    pub id: String,
    /// Where it might answer: the last IP the phone learned, its mDNS name, in that order. The
    /// hub adds the address a report arrives FROM (see note_report_from) as it learns it.
    pub hosts: Vec<String>,
    pub hooks: Vec<DesiredHook>,
    /// The device's admin password when the vessel secures its sensors. Never returned.
    pub password: String,
    /// Switch Bluetooth off once the hooks are verified (owner: "until factory reset").
    pub ble_off: bool,
    /// Epoch ms the job was accepted.
    pub added_at: i64,
}

/// What the hub knows about a job — what `/api/hub/sensors` answers and the wizard polls.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct SensorState {
    pub id: String,
    /// `pending` (hunting) · `wired` (verified) · `failed` (answered, but the hooks would not take).
    pub state: String,
    pub added_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wired_at: Option<i64>,
    /// Attempts so far — visible progress while the sensor sleeps.
    pub attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The urls confirmed on the sensor after the read-back.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub confirmed: Vec<String>,
    pub ble_off: bool,
    /// Bluetooth was switched off on the device.
    pub ble_disabled: bool,
}

impl SensorState {
    pub fn pending(job: &SensorJob) -> Self {
        SensorState { id: job.id.clone(), state: "pending".into(), added_at: job.added_at, ble_off: job.ble_off, ..Default::default() }
    }
}

// --- Shelly Gen2 RPC over HTTP, with body-digest auth ---------------------------------------------

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// PURE: the `auth` object a secured Gen2 device wants on the retry (mirrors shellyRpc.ts).
pub fn digest_auth(realm: &str, nonce: &Value, password: &str, cnonce: u64) -> Value {
    let nonce_s = match nonce {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let ha1 = sha256_hex(&format!("admin:{realm}:{password}"));
    let ha2 = sha256_hex("dummy_method:dummy_uri");
    let nc = 1;
    let response = sha256_hex(&format!("{ha1}:{nonce_s}:{nc}:{cnonce}:auth:{ha2}"));
    json!({ "realm": realm, "username": "admin", "nonce": nonce.clone(), "cnonce": cnonce, "response": response, "algorithm": "SHA-256" })
}

/// PURE: the realm/nonce out of a 401's JSON body (`message` is a JSON string on Gen2).
pub fn parse_challenge(body: &Value) -> Option<(String, Value)> {
    let msg = body.get("message")?.as_str()?;
    let c: Value = serde_json::from_str(msg).ok()?;
    let realm = c.get("realm")?.as_str()?.to_string();
    let nonce = c.get("nonce")?.clone();
    Some((realm, nonce))
}

/// One RPC call. `Err` carries a short reason. Auth is transparent: a 401 challenge is answered
/// once with the password and the retry's result returned.
pub async fn rpc(client: &reqwest::Client, host: &str, method: &str, params: Value, password: &str) -> Result<Value, String> {
    let url = format!("http://{host}/rpc");
    let call = |auth: Option<Value>| {
        let mut body = json!({ "id": 1, "method": method, "params": params });
        if let Some(a) = auth {
            body["auth"] = a;
        }
        client.post(&url).timeout(RPC_TIMEOUT).json(&body).send()
    };
    let res = call(None).await.map_err(|e| e.without_url().to_string())?;
    let status = res.status().as_u16();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    let body = if status == 401 {
        if password.is_empty() {
            return Err("the sensor is password-protected and no password was given".into());
        }
        let (realm, nonce) = parse_challenge(&body).ok_or("auth challenge not understood")?;
        let cnonce = u64::from(rand_u32());
        let res = call(Some(digest_auth(&realm, &nonce, password, cnonce))).await.map_err(|e| e.without_url().to_string())?;
        if res.status().as_u16() == 401 {
            return Err("the sensor refused the password".into());
        }
        res.json().await.unwrap_or(Value::Null)
    } else {
        body
    };
    if let Some(e) = body.get("error") {
        return Err(e.get("message").and_then(|m| m.as_str()).unwrap_or("rpc error").to_string());
    }
    Ok(body.get("result").cloned().unwrap_or(body))
}

fn rand_u32() -> u32 {
    // Unpredictable enough for a client nonce (the same bar the app sets) without a new crate.
    let u = uuid::Uuid::new_v4();
    u32::from_le_bytes(u.as_bytes()[..4].try_into().unwrap())
}

/// PURE: the app may not know the hub's LAN address (a phone that has only ever reached the hub
/// through the cloud), so it writes `__HUB__` where the host goes and the hub — which does know —
/// fills it in. A url without the placeholder passes through untouched.
pub fn substitute_hub_host(hooks: &mut [DesiredHook], lan_ip: &str) {
    for h in hooks.iter_mut() {
        for u in h.urls.iter_mut() {
            if u.contains("__HUB__") {
                *u = u.replace("__HUB__", lan_ip);
            }
        }
    }
}

/// PURE: is this `Shelly.GetDeviceInfo` reply the sensor we are hunting?
pub fn is_target(info: &Value, id: &str) -> bool {
    let want = id.to_ascii_lowercase();
    ["id", "mac"].iter().any(|k| {
        info.get(*k).and_then(|v| v.as_str()).map(|s| {
            let s = s.to_ascii_lowercase();
            s == want || want.ends_with(&s.replace(':', "")) || s.ends_with(&want)
        }).unwrap_or(false)
    })
}

/// PURE: which desired urls are NOT carried by the device's hooks yet (grouped by event).
pub fn missing_urls(hooks: &[Value], desired: &[DesiredHook]) -> Vec<DesiredHook> {
    desired
        .iter()
        .filter_map(|d| {
            let have: Vec<String> = hooks
                .iter()
                .filter(|h| h.get("event").and_then(|e| e.as_str()) == Some(d.event.as_str()))
                .flat_map(|h| h.get("urls").and_then(|u| u.as_array()).cloned().unwrap_or_default())
                .filter_map(|u| u.as_str().map(str::to_string))
                .collect();
            let urls: Vec<String> = d.urls.iter().filter(|u| !have.contains(u)).cloned().collect();
            if urls.is_empty() { None } else { Some(DesiredHook { event: d.event.clone(), urls }) }
        })
        .collect()
}

async fn read_hooks(client: &reqwest::Client, host: &str, pw: &str) -> Vec<Value> {
    rpc(client, host, "Webhook.List", json!({}), pw)
        .await
        .ok()
        .and_then(|l| l.get("hooks").and_then(|h| h.as_array()).cloned())
        .unwrap_or_default()
}

/// Write the hooks the way sleepyProvision.ts does: one hook per event with every url, READ BACK,
/// then one hook per url for whatever the device dropped. Returns the confirmed urls and the gaps.
pub async fn wire_hooks(client: &reqwest::Client, host: &str, pw: &str, desired: &[DesiredHook]) -> (Vec<String>, Vec<String>) {
    let mut hooks = read_hooks(client, host, pw).await;
    for d in desired {
        let existing = hooks.iter().find(|h| h.get("event").and_then(|e| e.as_str()) == Some(d.event.as_str())).cloned();
        let mut merged = d.urls.clone();
        if let Some(ex) = &existing {
            for u in ex.get("urls").and_then(|u| u.as_array()).cloned().unwrap_or_default() {
                if let Some(s) = u.as_str() {
                    if !merged.iter().any(|m| m == s) { merged.push(s.to_string()); }
                }
            }
        }
        let _ = match existing.as_ref().and_then(|ex| ex.get("id")).cloned() {
            Some(id) => rpc(client, host, "Webhook.Update", json!({ "id": id, "enable": true, "urls": merged }), pw).await,
            None => rpc(client, host, "Webhook.Create", json!({ "cid": 0, "enable": true, "event": d.event, "name": "dockneighbor", "urls": merged }), pw).await,
        };
    }
    hooks = read_hooks(client, host, pw).await;
    let mut gaps = missing_urls(&hooks, desired);
    if !gaps.is_empty() {
        // The Flood G4 keeps only the FIRST url of a hook — give each remaining url its own hook.
        for g in &gaps {
            for url in &g.urls {
                let _ = rpc(client, host, "Webhook.Create", json!({ "cid": 0, "enable": true, "event": g.event, "name": "dockneighbor", "urls": [url] }), pw).await;
            }
        }
        hooks = read_hooks(client, host, pw).await;
        gaps = missing_urls(&hooks, desired);
    }
    let missing: Vec<String> = gaps.iter().flat_map(|g| g.urls.clone()).collect();
    let confirmed: Vec<String> = desired.iter().flat_map(|d| d.urls.clone()).filter(|u| !missing.contains(u)).collect();
    (confirmed, missing)
}

/// Switch the device's Bluetooth off. The change needs a restart on Gen2; a sleepy sensor is
/// rebooted at once so it comes back with BLE off rather than at some later wake.
pub async fn ble_off(client: &reqwest::Client, host: &str, pw: &str) -> Result<(), String> {
    let r = rpc(client, host, "BLE.SetConfig", json!({ "config": { "enable": false, "rpc": { "enable": false } } }), pw).await?;
    if r.get("restart_required").and_then(|v| v.as_bool()).unwrap_or(false) {
        let _ = rpc(client, host, "Shelly.Reboot", json!({}), pw).await;
    }
    Ok(())
}

/// One attempt at one job against one host: identify, wire, verify, BLE off. `Ok(None)` = the
/// sensor did not answer here (asleep or wrong address); `Ok(Some(state))` = it answered and
/// this is the outcome (wired or failed); `Err` = it answered but could not be identified.
pub async fn attempt(client: &reqwest::Client, job: &SensorJob, host: &str, now_ms: i64) -> Result<Option<SensorState>, String> {
    let info = match rpc(client, host, "Shelly.GetDeviceInfo", json!({}), &job.password).await {
        Ok(i) => i,
        Err(_) => return Ok(None),
    };
    if !is_target(&info, &job.id) {
        return Err(format!("{host} answered as a different device"));
    }
    let (confirmed, missing) = wire_hooks(client, host, &job.password, &job.hooks).await;
    let mut st = SensorState::pending(job);
    st.host = Some(host.to_string());
    st.confirmed = confirmed;
    if missing.is_empty() {
        st.state = "wired".into();
        st.wired_at = Some(now_ms);
        if job.ble_off {
            match ble_off(client, host, &job.password).await {
                Ok(()) => st.ble_disabled = true,
                Err(e) => st.last_error = Some(format!("hooks verified; Bluetooth could not be switched off: {e}")),
            }
        }
    } else {
        st.state = "failed".into();
        st.last_error = Some(format!("the sensor kept {} of {} destinations — its hook slots may be full", st.confirmed.len(), st.confirmed.len() + missing.len()));
    }
    Ok(Some(st))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_matches_the_app_s_recipe() {
        // ha1 = sha256("admin:realm:pw"), ha2 = sha256("dummy_method:dummy_uri"), response = sha256("ha1:nonce:1:cnonce:auth:ha2")
        let a = digest_auth("shellyflood-1", &json!(1721234567), "pw", 42);
        assert_eq!(a["username"], "admin");
        assert_eq!(a["nonce"], json!(1721234567));
        assert_eq!(a["algorithm"], "SHA-256");
        let ha1 = sha256_hex("admin:shellyflood-1:pw");
        let ha2 = sha256_hex("dummy_method:dummy_uri");
        assert_eq!(a["response"], json!(sha256_hex(&format!("{ha1}:1721234567:1:42:auth:{ha2}"))));
    }

    #[test]
    fn challenge_is_read_out_of_the_gen2_body() {
        let body = json!({ "code": 401, "message": "{\"auth_type\":\"digest\",\"nonce\":1721234567,\"nc\":1,\"realm\":\"shellyfloodg4-d885acea3914\",\"algorithm\":\"SHA-256\"}" });
        let (realm, nonce) = parse_challenge(&body).unwrap();
        assert_eq!(realm, "shellyfloodg4-d885acea3914");
        assert_eq!(nonce, json!(1721234567));
        assert!(parse_challenge(&json!({ "message": "nope" })).is_none());
    }

    #[test]
    fn hub_placeholder_becomes_the_hub_s_lan_address() {
        let mut hooks = vec![DesiredHook { event: "flood.alarm".into(), urls: vec!["http://__HUB__:8722/api/hub/shelly?vid=v".into(), "https://api.dockneighbor.com/api/shelly?vid=v".into()] }];
        substitute_hub_host(&mut hooks, "172.31.0.105");
        assert_eq!(hooks[0].urls, vec!["http://172.31.0.105:8722/api/hub/shelly?vid=v".to_string(), "https://api.dockneighbor.com/api/shelly?vid=v".to_string()]);
    }

    #[test]
    fn target_matches_by_id_or_mac_case_insensitively() {
        assert!(is_target(&json!({ "id": "shellyfloodg4-d885acea3914" }), "shellyfloodg4-d885acea3914"));
        assert!(is_target(&json!({ "id": "ShellyFloodG4-D885ACEA3914" }), "shellyfloodg4-d885acea3914"));
        assert!(is_target(&json!({ "mac": "D8:85:AC:EA:39:14" }), "shellyfloodg4-d885acea3914"));
        assert!(!is_target(&json!({ "id": "shellyplusuni-aaaa" }), "shellyfloodg4-d885acea3914"));
    }

    #[test]
    fn missing_urls_ignores_what_the_device_already_carries() {
        let desired = vec![
            DesiredHook { event: "flood.alarm".into(), urls: vec!["http://hub/a".into(), "https://cloud/a".into()] },
            DesiredHook { event: "flood.alarm_off".into(), urls: vec!["http://hub/b".into()] },
        ];
        let hooks = vec![json!({ "id": 1, "event": "flood.alarm", "urls": ["http://hub/a"] })];
        let gaps = missing_urls(&hooks, &desired);
        assert_eq!(gaps, vec![
            DesiredHook { event: "flood.alarm".into(), urls: vec!["https://cloud/a".into()] },
            DesiredHook { event: "flood.alarm_off".into(), urls: vec!["http://hub/b".into()] },
        ]);
        let full = vec![
            json!({ "id": 1, "event": "flood.alarm", "urls": ["http://hub/a"] }),
            json!({ "id": 2, "event": "flood.alarm", "urls": ["https://cloud/a"] }),
            json!({ "id": 3, "event": "flood.alarm_off", "urls": ["http://hub/b"] }),
        ];
        assert!(missing_urls(&full, &desired).is_empty());
    }

    /// A stand-in Flood G4: unauthenticated, keeps only the FIRST url of a hook (the bench truth
    /// sleepyProvision.ts pins), and reports BLE.SetConfig as needing a restart.
    async fn fake_sensor() -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        use axum::{routing::post, Json, Router};
        use std::sync::{Arc, Mutex};
        let hooks: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let calls: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let (h2, c2) = (hooks.clone(), calls.clone());
        let app = Router::new().route(
            "/rpc",
            post(move |Json(body): Json<Value>| {
                let (hooks, calls) = (h2.clone(), c2.clone());
                async move {
                    calls.lock().unwrap().push(body.clone());
                    let method = body["method"].as_str().unwrap_or("");
                    let p = &body["params"];
                    let result = match method {
                        "Shelly.GetDeviceInfo" => json!({ "id": "shellyfloodg4-d885acea3914", "mac": "D885ACEA3914", "model": "S4SN-0071A" }),
                        "Webhook.List" => json!({ "hooks": hooks.lock().unwrap().clone() }),
                        "Webhook.Create" => {
                            let mut h = hooks.lock().unwrap();
                            let id = h.len() as u64 + 1;
                            let first = p["urls"].as_array().and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
                            h.push(json!({ "id": id, "event": p["event"], "enable": true, "urls": [first] }));
                            json!({ "id": id, "rev": id })
                        }
                        "Webhook.Update" => {
                            let mut h = hooks.lock().unwrap();
                            if let Some(x) = h.iter_mut().find(|x| x["id"] == p["id"]) {
                                let first = p["urls"].as_array().and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
                                x["urls"] = json!([first]);
                            }
                            json!({ "rev": 9 })
                        }
                        "BLE.SetConfig" => json!({ "restart_required": true }),
                        "Shelly.Reboot" => json!(null),
                        _ => return Json(json!({ "id": 1, "error": { "code": 404, "message": "no such method" } })),
                    };
                    Json(json!({ "id": 1, "result": result }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("127.0.0.1:{}", addr.port()), calls)
    }

    #[tokio::test]
    async fn wires_a_flood_g4_that_keeps_one_url_per_hook_then_switches_ble_off() {
        let (host, calls) = fake_sensor().await;
        let client = reqwest::Client::new();
        let job = SensorJob {
            id: "shellyfloodg4-d885acea3914".into(),
            hosts: vec![host.clone()],
            hooks: vec![
                DesiredHook { event: "flood.alarm".into(), urls: vec!["http://172.31.0.105:8722/api/hub/shelly?e=alarm".into(), "https://api.dockneighbor.com/api/shelly?e=alarm".into()] },
                DesiredHook { event: "flood.alarm_off".into(), urls: vec!["http://172.31.0.105:8722/api/hub/shelly?e=off".into(), "https://api.dockneighbor.com/api/shelly?e=off".into()] },
            ],
            password: String::new(),
            ble_off: true,
            added_at: 1,
        };
        let st = attempt(&client, &job, &host, 5).await.unwrap().expect("the sensor answered");
        assert_eq!(st.state, "wired", "{:?}", st.last_error);
        assert_eq!(st.confirmed.len(), 4);
        assert!(st.ble_disabled);
        assert_eq!(st.host.as_deref(), Some(host.as_str()));
        // The hub url is the FIRST url of each hook — the one the device keeps.
        let c = calls.lock().unwrap();
        let creates: Vec<&Value> = c.iter().filter(|b| b["method"] == "Webhook.Create").collect();
        assert!(creates.iter().any(|b| b["params"]["urls"][0].as_str().unwrap().starts_with("http://172.31.0.105:8722")));
        assert!(c.iter().any(|b| b["method"] == "BLE.SetConfig"));
        assert!(c.iter().any(|b| b["method"] == "Shelly.Reboot"));
    }

    #[tokio::test]
    async fn a_silent_address_is_not_an_outcome_and_the_wrong_device_is_named() {
        let client = reqwest::Client::new();
        let job = SensorJob { id: "shellyfloodg4-d885acea3914".into(), ..Default::default() };
        // Nothing listens here: asleep / wrong address ⇒ keep hunting.
        assert!(attempt(&client, &job, "127.0.0.1:9", 1).await.unwrap().is_none());
        let (host, _) = fake_sensor().await;
        let other = SensorJob { id: "shellyht-0000".into(), ..Default::default() };
        assert!(attempt(&client, &other, &host, 1).await.is_err());
    }
}
