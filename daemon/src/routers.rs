// Managed routers — the hub as the router's administrator (owner, 2026-09-12: "The hub should do
// all configuration/monitoring of the router not the UI/App. When a router has a GPS enabled,
// create a device in the GPS section").
//
// A GL.iNet IS the hub (it runs hub-lite), so it is not managed here. A Cradlepoint or Peplink
// cannot run the hub, so a separate box — this daemon — signs in to it over the LAN, reads its
// modem/WAN/GPS on a timer, reports as the ROUTER device with the router's own agent token (the
// same `modem.measurement` shape hub-lite sends, so the cloud and the app cannot tell the two
// apart), and carries out the owner's writes (APN, GPS on/off, reboot) on request. The app never
// talks to the router; it talks to the hub — over the LAN aboard, through the relay from shore.
//
// Vendor 1 is Cradlepoint NCOS. Every parser mirrors the app's drivers/cradlepoint.ts, which was
// pinned to payloads captured from the bench CBA850 (fw 7.0.50) on 2026-08-17 — the two must agree
// on every shape, so the fixtures below are the same captures. Peplink is next (owner: "Cradlepoint
// first, Peplink right after").

use serde::Serialize;
use serde_json::Value;

use crate::gps::{cradlepoint_base, parse_cradlepoint_gps, GpsFix};
use crate::hub_config::RouterConfig;

/// How often a router is read when the owner set nothing. Two minutes: signal and data use move
/// slowly, and a router's API is not free to hit.
pub const DEFAULT_POLL_SECS: u64 = 120;
/// The floor. NCOS answers `/api/status/wan/devices` in well under a second, but 30 s is plenty of
/// cadence for a signal bar and keeps a mis-set config from hammering the router.
pub const POLL_FLOOR_SECS: u64 = 30;

/// The poll cadence a config resolves to.
pub fn poll_secs(cfg: &RouterConfig) -> u64 {
    if cfg.poll_secs == 0 { DEFAULT_POLL_SECS } else { u64::from(cfg.poll_secs).max(POLL_FLOOR_SECS) }
}

/// An HTTP client for LAN gear. ⚠️ ACCEPTS SELF-SIGNED CERTIFICATES on purpose: a Cradlepoint
/// serves its NCOS API on 443 with a factory self-signed certificate (owner: "cradlepoints have
/// https redirects by default, ssl/443 should be default"), and there is no CA on a boat LAN to
/// vouch for it. This client is used ONLY for addresses the owner typed into the hub; the cloud
/// client in hub_server keeps full verification.
pub fn lan_client() -> reqwest::Client {
    reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("reqwest client")
}

// --- Normalized shapes (mirror the app's networkDevices.ts) ---------------------------------------

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Probe {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModemStatus {
    /// `ok` | `missing` | `locked` | `unknown` — the app's SimState vocabulary.
    pub sim: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rssi: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rsrp: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rsrq: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sinr: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connected: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    /// Lifetime byte counters for the current connection, as the router reports them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_bytes: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WanStatus {
    /// `lte` | `wired` | `repeater` | `none`.
    pub wan: String,
    pub up: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
}

/// The modem's APN setting: `auto` (the carrier profile the modem picks) or `manual` with a name.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApnConfig {
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apn: Option<String>,
}

/// Everything the last poll learned about one router — what `/api/hub/routers` and the console
/// show. `error` is set when the poll failed (unreachable, refused sign-in) and the rest is what
/// was last known, so a router that just went dark still shows its identity.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// Epoch ms of the last poll attempt.
    pub at_ms: i64,
    /// Epoch ms of the last SUCCESSFUL poll.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<Probe>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modem: Option<ModemStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wan: Option<WanStatus>,
    /// Whether GNSS is switched on in the router's own settings (NCOS System → GPS).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gps_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<FixOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apn: Option<ApnConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FixOut {
    pub lat: f64,
    pub lon: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acc: Option<f64>,
}

impl From<&GpsFix> for FixOut {
    fn from(f: &GpsFix) -> Self {
        FixOut { lat: f.lat, lon: f.lon, acc: f.acc }
    }
}

// --- Pure parsers (Cradlepoint NCOS; fixtures = the 2026-08-17 CBA850 capture) --------------------

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn as_u64(v: &Value) -> Option<u64> {
    as_f64(v).filter(|n| *n >= 0.0).map(|n| n as u64)
}

fn str_of(v: Option<&Value>) -> Option<String> {
    let t = v?.as_str()?.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

/// NCOS envelope: `{success:true, data}` on success; anything else is a fault. Returns the payload.
pub fn ncos_data(body: &Value) -> Result<&Value, String> {
    let Some(obj) = body.as_object() else {
        return Err("the router did not answer with JSON".into());
    };
    if obj.get("success") == Some(&Value::Bool(false)) {
        let why = obj
            .get("reason")
            .or_else(|| obj.get("data"))
            .and_then(|v| v.as_str())
            .unwrap_or("the router refused the request");
        return Err(why.to_string());
    }
    match obj.get("data") {
        Some(d) => Ok(d),
        None if obj.get("success") == Some(&Value::Bool(true)) => Ok(body),
        None => Err("unexpected response from the router".into()),
    }
}

/// `/api/status/product_info` + `/api/status/fw_info` → identity.
pub fn parse_probe(product: &Value, fw: Option<&Value>) -> Option<Probe> {
    let model = str_of(product.get("product_name")).or_else(|| str_of(Some(product)));
    let firmware = fw.and_then(|f| {
        if f.is_object() && f.get("major_version").and_then(as_f64).is_some() {
            let parts: Vec<String> = ["major_version", "minor_version", "patch_version"]
                .iter()
                .filter_map(|k| f.get(*k))
                .filter_map(|v| as_f64(v).map(|n| format!("{}", n as i64)))
                .collect();
            Some(parts.join("."))
        } else {
            str_of(Some(f))
        }
    });
    let mac = str_of(product.get("mac0"));
    if model.is_none() && firmware.is_none() {
        return None;
    }
    Some(Probe { model, firmware, mac })
}

/// `/api/status/wan/devices` → the entries `(uid, entry)` that are objects.
fn wan_entries(devices: &Value) -> Vec<(&str, &Value)> {
    devices
        .as_object()
        .map(|m| m.iter().filter(|(_, v)| v.is_object()).map(|(k, v)| (k.as_str(), v)).collect())
        .unwrap_or_default()
}

fn connected(entry: &Value) -> bool {
    entry
        .get("status")
        .and_then(|s| s.get("connection_state"))
        .and_then(|v| v.as_str())
        .map(|s| s.eq_ignore_ascii_case("connected"))
        .unwrap_or(false)
}

fn ip_of(entry: &Value) -> Option<String> {
    str_of(entry.get("status").and_then(|s| s.get("ipinfo")).and_then(|i| i.get("ip_address")))
}

/// Bench truths: SIM state is PIN_STATUS ('READY' / 'NOSIM'), the service label is SERDIS ('LTE'),
/// CARRID arrives with a trailing space, every signal number is a STRING, and a dual-SIM unit
/// reports one entry per slot — the connected one is the modem.
pub fn parse_modem(devices: &Value) -> Option<ModemStatus> {
    let cellular: Vec<&Value> = wan_entries(devices)
        .into_iter()
        .map(|(_, v)| v)
        .filter(|v| {
            v.get("diagnostics").and_then(|d| d.as_object()).map_or(false, |d| {
                ["CARRID", "RSRP", "HOMECARRID", "MDN"].iter().any(|k| d.contains_key(*k))
            })
        })
        .collect();
    let entry = cellular.iter().copied().find(|v| connected(v)).or_else(|| cellular.first().copied())?;
    let g = entry.get("diagnostics")?;
    let sim_raw = str_of(g.get("PIN_STATUS"))
        .or_else(|| str_of(g.get("SIM")))
        .or_else(|| str_of(g.get("SIM_STATUS")))
        .unwrap_or_default()
        .to_ascii_lowercase();
    let sim = match sim_raw.as_str() {
        "ready" | "ok" => "ok",
        "sim absent" | "absent" | "missing" | "nosim" => "missing",
        "locked" | "pin locked" | "sim locked" => "locked",
        _ => "unknown",
    };
    let stats = entry.get("stats");
    Some(ModemStatus {
        sim: sim.to_string(),
        carrier: str_of(g.get("CARRID")).or_else(|| str_of(g.get("HOMECARRID"))),
        mode: str_of(g.get("SERDIS")).or_else(|| str_of(g.get("MODEMSYSMODE"))).or_else(|| str_of(g.get("SRVC_TYPE"))),
        rssi: g.get("DBM").or_else(|| g.get("RSSI")).and_then(as_f64),
        rsrp: g.get("RSRP").and_then(as_f64),
        rsrq: g.get("RSRQ").and_then(as_f64),
        sinr: g.get("SINR").and_then(as_f64),
        connected: if connected(entry) { Some(true) } else { None },
        ip: ip_of(entry),
        tx_bytes: stats.and_then(|s| s.get("out")).and_then(as_u64),
        rx_bytes: stats.and_then(|s| s.get("in")).and_then(as_u64),
    })
}

/// Classify the connected WAN by its uid prefix: `mdm-` cellular, `wwan-` repeater, else wired.
pub fn parse_wan(devices: &Value) -> Option<WanStatus> {
    let entries = wan_entries(devices);
    if entries.is_empty() {
        return None;
    }
    let Some((key, v)) = entries.iter().find(|(_, v)| connected(v)) else {
        return Some(WanStatus { wan: "none".into(), up: false, ip: None });
    };
    let lower = key.to_ascii_lowercase();
    let wan = if lower.starts_with("mdm") {
        "lte"
    } else if lower.starts_with("wwan") {
        "repeater"
    } else {
        "wired"
    };
    Some(WanStatus { wan: wan.into(), up: true, ip: ip_of(v) })
}

/// `/api/config/wan/rules2` → the index of the cellular rule and its APN setting. NCOS keeps one
/// rule per WAN class; the modem's is the one whose `trigger_string` names `mdm`. `apn_mode` is
/// `auto` unless the owner pinned `manual_apn`.
pub fn parse_apn(rules: &Value) -> Option<(usize, ApnConfig)> {
    let list = rules.as_array()?;
    let (idx, rule) = list
        .iter()
        .enumerate()
        .find(|(_, r)| {
            r.get("trigger_string")
                .and_then(|t| t.as_str())
                .map(|t| t.contains("mdm"))
                .unwrap_or(false)
        })
        .or_else(|| list.iter().enumerate().find(|(_, r)| r.get("modem").is_some()))?;
    let modem = rule.get("modem");
    let manual = modem.and_then(|m| str_of(m.get("manual_apn")));
    let mode = modem
        .and_then(|m| str_of(m.get("apn_mode")))
        .map(|m| m.to_ascii_lowercase())
        .unwrap_or_else(|| if manual.is_some() { "manual".into() } else { "auto".into() });
    Some((idx, ApnConfig { apn: if mode == "manual" { manual } else { None }, mode }))
}

/// The `modem.measurement` params — THE SAME NAMES hub-lite's push_modem sends, so the cloud's
/// sensorState doc and the app's parseCachedModem read a hub-managed Cradlepoint exactly as they
/// read a GL.iNet. `wan_kb_delta` is the plan-burn increment since the last poll (the cloud sums
/// per-source KB deltas — wanUsage.ts), absent on the first poll or after a counter reset.
pub fn modem_params(
    m: &ModemStatus,
    wan: Option<&WanStatus>,
    probe: Option<&Probe>,
    wan_kb_delta: Option<u64>,
) -> Vec<(String, String)> {
    let mut p: Vec<(String, String)> = vec![("up".into(), if m.connected == Some(true) { "1" } else { "0" }.into())];
    let mut push = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            p.push((k.to_string(), v));
        }
    };
    let num = |v: Option<f64>| v.map(|n| if n.fract() == 0.0 { format!("{}", n as i64) } else { format!("{n:.1}") });
    push("mode", m.mode.clone());
    push("rssi", num(m.rssi));
    push("rsrp", num(m.rsrp));
    push("sinr", num(m.sinr));
    push("rsrq", num(m.rsrq));
    push("carrier", m.carrier.clone());
    push("sim", Some(m.sim.clone()));
    if let (Some(tx), Some(rx)) = (m.tx_bytes, m.rx_bytes) {
        push("dataMb", Some(((tx + rx) / 1_048_576).to_string()));
    }
    push("wan", wan.map(|w| w.wan.clone()));
    push("ip", m.ip.clone().or_else(|| wan.and_then(|w| w.ip.clone())));
    push("model", probe.and_then(|p| p.model.clone()));
    push("fw", probe.and_then(|p| p.firmware.clone()));
    // `av` is what the fleet console steers rollouts by — for a hub-managed router it is the hub.
    push("av", Some(format!("hub-{}", env!("CARGO_PKG_VERSION"))));
    if let Some(kb) = wan_kb_delta.filter(|kb| *kb > 0) {
        push("wanKb_cellular", Some(kb.to_string()));
    }
    p
}

/// PURE: the plan-burn delta between two lifetime counters, in KB. None when there is no earlier
/// sample or the counter went BACKWARDS (a reboot or modem reset zeroes it — reporting the whole
/// new total as a delta would charge the plan for bytes it never used).
pub fn wan_kb_delta(prev: Option<(u64, u64)>, now: (u64, u64)) -> Option<u64> {
    let (ptx, prx) = prev?;
    if now.0 < ptx || now.1 < prx {
        return None;
    }
    Some(((now.0 - ptx) + (now.1 - prx)) / 1024)
}

// --- NCOS transport -------------------------------------------------------------------------------

/// One signed-in router. Basic auth per request — NCOS has no session to establish.
pub struct Ncos<'a> {
    client: &'a reqwest::Client,
    base: String,
    user: String,
    pass: String,
}

impl<'a> Ncos<'a> {
    pub fn new(client: &'a reqwest::Client, host: &str, port: u16, user: &str, pass: &str) -> Self {
        Ncos {
            client,
            base: cradlepoint_base(host.trim(), port),
            user: if user.trim().is_empty() { "admin".into() } else { user.trim().into() },
            pass: pass.to_string(),
        }
    }

    pub fn for_router(client: &'a reqwest::Client, cfg: &RouterConfig) -> Self {
        Ncos::new(client, &cfg.host, cfg.port, &cfg.username, &cfg.password)
    }

    async fn finish(res: reqwest::Response) -> Result<Value, String> {
        let code = res.status().as_u16();
        if code == 401 || code == 403 {
            return Err("the router refused the sign-in — check the admin username and password".into());
        }
        if !res.status().is_success() {
            return Err(format!("the router answered HTTP {code}"));
        }
        let body: Value = res.json().await.map_err(|_| "the router did not answer with JSON".to_string())?;
        ncos_data(&body).map(|d| d.clone())
    }

    /// GET a status/config path; returns the unwrapped `data`.
    pub async fn get(&self, path: &str) -> Result<Value, String> {
        let res = self
            .client
            .get(format!("{}{path}", self.base))
            .basic_auth(&self.user, Some(&self.pass))
            .send()
            .await
            .map_err(|e| reachability(e))?;
        Self::finish(res).await
    }

    /// PUT a config/control path. NCOS takes the JSON as a form field: `data=<json>`.
    pub async fn put(&self, path: &str, value: &Value) -> Result<Value, String> {
        let res = self
            .client
            .put(format!("{}{path}", self.base))
            .basic_auth(&self.user, Some(&self.pass))
            .form(&[("data", value.to_string())])
            .send()
            .await
            .map_err(|e| reachability(e))?;
        Self::finish(res).await
    }

    /// Prove the sign-in and read identity.
    pub async fn probe(&self) -> Result<Probe, String> {
        let product = self.get("/api/status/product_info").await?;
        let fw = self.get("/api/status/fw_info").await.ok();
        parse_probe(&product, fw.as_ref()).ok_or_else(|| "the router answered, but its model could not be read".into())
    }

    pub async fn wan_devices(&self) -> Result<Value, String> {
        self.get("/api/status/wan/devices").await
    }

    /// `Ok(None)` = reachable, no lock yet.
    pub async fn gps_fix(&self) -> Result<Option<GpsFix>, String> {
        let d = self.get("/api/status/gps").await?;
        Ok(parse_cradlepoint_gps(&d))
    }

    pub async fn gps_enabled(&self) -> Result<bool, String> {
        let d = self.get("/api/config/system/gps/enabled").await?;
        d.as_bool().ok_or_else(|| "the router's GPS setting could not be read".into())
    }

    pub async fn set_gps_enabled(&self, on: bool) -> Result<(), String> {
        self.put("/api/config/system/gps/enabled", &Value::Bool(on)).await.map(|_| ())
    }

    pub async fn apn(&self) -> Result<(usize, ApnConfig), String> {
        let rules = self.get("/api/config/wan/rules2").await?;
        parse_apn(&rules).ok_or_else(|| "the router reports no cellular WAN rule to read an APN from".into())
    }

    /// Write the APN: `manual` with a name, or `auto` (clears the pinned name).
    pub async fn set_apn(&self, cfg: &ApnConfig) -> Result<ApnConfig, String> {
        let (idx, _) = self.apn().await?;
        let body = if cfg.mode == "manual" {
            let apn = cfg.apn.as_deref().map(str::trim).filter(|a| !a.is_empty()).ok_or("an APN name is required for manual mode")?;
            serde_json::json!({ "apn_mode": "manual", "manual_apn": apn })
        } else {
            serde_json::json!({ "apn_mode": "auto" })
        };
        self.put(&format!("/api/config/wan/rules2/{idx}/modem"), &body).await?;
        Ok(self.apn().await?.1)
    }

    pub async fn reboot(&self) -> Result<(), String> {
        self.put("/api/control/system", &serde_json::json!({ "reboot": true })).await.map(|_| ())
    }
}

fn reachability(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "the router did not answer (timed out) — is the hub on the same network?".into()
    } else if e.is_connect() {
        "the router could not be reached at that address".into()
    } else {
        e.without_url().to_string()
    }
}

/// One full read of a router — the poll loop's unit of work and the `refresh` action.
pub async fn poll(client: &reqwest::Client, cfg: &RouterConfig, prev: Option<&Snapshot>) -> Snapshot {
    let now = crate::hub_server::now_ms();
    let mut snap = prev.cloned().unwrap_or_default();
    snap.at_ms = now;
    let ncos = Ncos::for_router(client, cfg);
    let devices = match ncos.wan_devices().await {
        Ok(d) => d,
        Err(why) => {
            snap.error = Some(why);
            return snap;
        }
    };
    snap.modem = parse_modem(&devices);
    snap.wan = parse_wan(&devices);
    if snap.probe.is_none() {
        snap.probe = ncos.probe().await.ok();
    }
    snap.gps_enabled = ncos.gps_enabled().await.ok();
    snap.fix = if cfg.gps_enabled {
        match ncos.gps_fix().await {
            Ok(f) => f.as_ref().map(FixOut::from),
            Err(_) => None,
        }
    } else {
        None
    };
    snap.error = None;
    snap.ok_at_ms = Some(now);
    snap
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The 2026-08-17 CBA850 capture, trimmed to the fields the parsers read.
    fn bench_devices() -> Value {
        json!({
            "mdm-1a2b": {
                "info": { "type": "mdm" },
                "status": { "connection_state": "connected", "ipinfo": { "ip_address": "100.64.3.9" } },
                "diagnostics": { "CARRID": "Verizon ", "HOMECARRID": "Verizon", "SERDIS": "LTE", "DBM": "-71",
                                  "RSRP": "-101", "RSRQ": "-12", "SINR": "7.4", "PIN_STATUS": "READY", "MDN": "5551234567" },
                "stats": { "in": 1234567, "out": 234567 }
            },
            "mdm-3c4d": {
                "info": { "type": "mdm" },
                "status": { "connection_state": "unplugged" },
                "diagnostics": { "PIN_STATUS": "NOSIM", "RSRP": "" }
            },
            "ethernet-wan": { "info": { "type": "ethernet" }, "status": { "connection_state": "disconnected" } }
        })
    }

    #[test]
    fn envelope_unwraps_success_and_names_a_fault() {
        assert_eq!(ncos_data(&json!({"success": true, "data": {"a": 1}})).unwrap(), &json!({"a": 1}));
        assert_eq!(ncos_data(&json!({"success": false, "reason": "unauthorized"})).unwrap_err(), "unauthorized");
        assert!(ncos_data(&json!("nope")).is_err());
    }

    #[test]
    fn probe_joins_the_firmware_triple_and_keeps_the_mac() {
        let p = parse_probe(
            &json!({"product_name": "CBA850", "mac0": "00:30:44:aa:bb:cc"}),
            Some(&json!({"major_version": 7, "minor_version": 0, "patch_version": 50})),
        )
        .unwrap();
        assert_eq!(p.model.as_deref(), Some("CBA850"));
        assert_eq!(p.firmware.as_deref(), Some("7.0.50"));
        assert_eq!(p.mac.as_deref(), Some("00:30:44:aa:bb:cc"));
        assert!(parse_probe(&json!({}), None).is_none());
    }

    #[test]
    fn modem_picks_the_connected_sim_and_normalizes_the_strings() {
        let m = parse_modem(&bench_devices()).unwrap();
        assert_eq!(m.sim, "ok");
        assert_eq!(m.carrier.as_deref(), Some("Verizon")); // trailing space gone
        assert_eq!(m.mode.as_deref(), Some("LTE"));
        assert_eq!(m.rssi, Some(-71.0));
        assert_eq!(m.rsrp, Some(-101.0));
        assert_eq!(m.sinr, Some(7.4));
        assert_eq!(m.connected, Some(true));
        assert_eq!(m.ip.as_deref(), Some("100.64.3.9"));
        assert_eq!((m.tx_bytes, m.rx_bytes), (Some(234567), Some(1234567)));
    }

    #[test]
    fn modem_reports_sim_absence_honestly_and_none_without_cellular() {
        let m = parse_modem(&json!({"mdm-x": {"status": {}, "diagnostics": {"PIN_STATUS": "NOSIM", "RSRP": ""}}})).unwrap();
        assert_eq!(m.sim, "missing");
        assert!(parse_modem(&json!({"ethernet-wan": {"status": {}}})).is_none());
    }

    #[test]
    fn wan_classifies_by_uid_prefix() {
        let w = parse_wan(&bench_devices()).unwrap();
        assert_eq!((w.wan.as_str(), w.up, w.ip.as_deref()), ("lte", true, Some("100.64.3.9")));
        let none = parse_wan(&json!({"ethernet-wan": {"status": {"connection_state": "disconnected"}}})).unwrap();
        assert_eq!((none.wan.as_str(), none.up), ("none", false));
        assert!(parse_wan(&json!({})).is_none());
    }

    #[test]
    fn apn_finds_the_modem_rule_and_reads_manual_or_auto() {
        let rules = json!([
            {"trigger_string": "type|is|ethernet", "trigger_name": "Ethernet"},
            {"trigger_string": "type|is|mdm", "modem": {"apn_mode": "manual", "manual_apn": "vzwinternet"}}
        ]);
        let (idx, apn) = parse_apn(&rules).unwrap();
        assert_eq!(idx, 1);
        assert_eq!((apn.mode.as_str(), apn.apn.as_deref()), ("manual", Some("vzwinternet")));
        let auto = json!([{"trigger_string": "type|is|mdm", "modem": {"apn_mode": "auto", "manual_apn": "stale"}}]);
        let (_, a) = parse_apn(&auto).unwrap();
        assert_eq!((a.mode.as_str(), a.apn), ("auto", None));
        assert!(parse_apn(&json!([{"trigger_string": "type|is|ethernet"}])).is_none());
    }

    #[test]
    fn measurement_params_match_hub_lite_names() {
        let m = parse_modem(&bench_devices()).unwrap();
        let w = parse_wan(&bench_devices());
        let p = Probe { model: Some("CBA850".into()), firmware: Some("7.0.50".into()), mac: None };
        let params = modem_params(&m, w.as_ref(), Some(&p), Some(512));
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("up"), Some("1"));
        assert_eq!(get("rsrp"), Some("-101"));
        assert_eq!(get("sinr"), Some("7.4"));
        assert_eq!(get("carrier"), Some("Verizon"));
        assert_eq!(get("sim"), Some("ok"));
        assert_eq!(get("dataMb"), Some("1")); // (1234567+234567)/1048576
        assert_eq!(get("wan"), Some("lte"));
        assert_eq!(get("model"), Some("CBA850"));
        assert_eq!(get("wanKb_cellular"), Some("512"));
        assert!(get("av").unwrap().starts_with("hub-"));
    }

    #[test]
    fn plan_burn_delta_never_charges_a_counter_reset() {
        assert_eq!(wan_kb_delta(None, (10, 10)), None);
        assert_eq!(wan_kb_delta(Some((1024, 2048)), (2048, 4096)), Some(3));
        assert_eq!(wan_kb_delta(Some((5000, 5000)), (10, 10)), None); // rebooted modem
    }

    #[test]
    fn poll_cadence_floors_and_defaults() {
        let mut c = RouterConfig::default();
        assert_eq!(poll_secs(&c), DEFAULT_POLL_SECS);
        c.poll_secs = 5;
        assert_eq!(poll_secs(&c), POLL_FLOOR_SECS);
        c.poll_secs = 600;
        assert_eq!(poll_secs(&c), 600);
    }

    #[tokio::test]
    async fn ncos_client_signs_in_with_basic_auth_and_unwraps_the_envelope() {
        use axum::{routing::get, Router};
        let app = Router::new().route(
            "/api/status/product_info",
            get(|headers: axum::http::HeaderMap| async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                // admin:secret
                if auth == "Basic YWRtaW46c2VjcmV0" {
                    axum::Json(serde_json::json!({"success": true, "data": {"product_name": "CBA850", "mac0": "aa"}}))
                } else {
                    axum::Json(serde_json::json!({"success": false, "reason": "unauthorized"}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = lan_client();
        let ok = Ncos::new(&client, "127.0.0.1", port, "admin", "secret");
        let p = ok.probe().await.unwrap();
        assert_eq!(p.model.as_deref(), Some("CBA850"));
        let bad = Ncos::new(&client, "127.0.0.1", port, "admin", "wrong");
        assert_eq!(bad.probe().await.unwrap_err(), "unauthorized");
    }
}
