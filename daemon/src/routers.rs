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
// Vendor 1 is Cradlepoint NCOS, in this file. Every parser mirrors the app's drivers/cradlepoint.ts,
// which was pinned to payloads captured from the bench CBA850 (fw 7.0.50) on 2026-08-17 — the two
// must agree on every shape, so the fixtures below are the same captures. Vendor 2 is Peplink
// (peplink.rs; owner: "Cradlepoint first, Peplink right after"). Vendor 3 is Starlink
// (starlink.rs) — not a router but the uplink itself, read over the dish's local gRPC with no
// sign-in, and reported through the same door so the cloud sees one more `modem.measurement`
// source. `Driver` below is the one door the poll loop and hub_server go through, so the shared
// shapes here — Snapshot, report_params — are filled identically whichever vendor answered.

use serde::Serialize;
use serde_json::Value;

use crate::gps::{cradlepoint_base, parse_cradlepoint_gps, GpsFix};
use crate::hub_config::RouterConfig;
use crate::peplink::Peplink;
use crate::starlink::{DishStatus, Starlink};

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
    /// A vendor serial / device id, for gear that has one and no MAC to offer (a Starlink's
    /// `ut01…` id) — the app derives a STABLE device id from it the way it does from a MAC.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
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
    /// `lte` | `wired` | `repeater` | `starlink` | `none`.
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
    /// A Starlink's own report (obstruction, outage, latency, alerts) — set only for that vendor,
    /// for which `modem` is never set: a dish has no SIM and no signal in dBm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dish: Option<DishStatus>,
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

pub(crate) fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn as_u64(v: &Value) -> Option<u64> {
    as_f64(v).filter(|n| *n >= 0.0).map(|n| n as u64)
}

pub(crate) fn str_of(v: Option<&Value>) -> Option<String> {
    let t = v?.as_str()?.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

/// The most a `read` may answer. Under the relay's reply cap with room for the envelope.
pub const MAX_READ_BYTES: usize = 256 * 1024;

/// Validate a path for `read`: NCOS status or config only, one clean segment list, nothing that
/// could be a query, a fragment, a traversal or a control endpoint.
pub fn readable_path(raw: &str) -> Result<String, String> {
    let p = raw.trim();
    let ok_prefix = p.starts_with("/api/status/") || p.starts_with("/api/config/") || p == "/api/status" || p == "/api/config";
    if !ok_prefix {
        return Err("read takes an /api/status/… or /api/config/… path".into());
    }
    if p.contains("..") || p.contains("//") || p.contains('?') || p.contains('#') || p.ends_with('/') {
        return Err("that is not a plain path".into());
    }
    if !p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.')) {
        return Err("a path is letters, digits, '/', '_', '-' and '.'".into());
    }
    Ok(p.to_string())
}

/// Blank anything that looks like a credential, recursively, before a config read leaves the hub.
/// A config tree carries the admin password hash, Wi-Fi keys, VPN secrets and SNMP communities;
/// the person reading is control-or-above, and still has no business seeing those in a diagnostic.
pub fn scrub_secrets(v: &mut Value) {
    const NEEDLES: [&str; 9] = ["password", "passwd", "secret", "wpapsk", "psk", "private_key", "community", "token", "shared_key"];
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                let lk = k.to_ascii_lowercase();
                if NEEDLES.iter().any(|n| lk.contains(n)) && !val.is_null() && !val.is_object() && !val.is_array() {
                    *val = Value::String("•••".into());
                } else {
                    scrub_secrets(val);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(scrub_secrets),
        _ => {}
    }
}

/// NCOS envelope: `{success:true, data}` on success; anything else is a fault. Returns the payload.
pub fn ncos_data(body: &Value) -> Result<&Value, String> {
    let Some(obj) = body.as_object() else {
        return Err("the router did not answer with JSON".into());
    };
    if obj.get("success") == Some(&Value::Bool(false)) {
        // NCOS says WHY in `reason`, or in `data` — and a config write that fails validation puts
        // the per-field complaint in `data` as an OBJECT, not a string. Carry it whatever its shape:
        // "the router refused the request" told nobody what the CBA850 disliked about the APN write.
        let why = obj.get("reason").or_else(|| obj.get("data")).filter(|v| !v.is_null());
        return Err(match why {
            Some(Value::String(s)) if !s.trim().is_empty() => s.trim().to_string(),
            Some(v) if !v.is_string() => {
                let d = v.to_string();
                if d.chars().count() > 300 {
                    format!("the router refused the request: {}…", d.chars().take(300).collect::<String>())
                } else {
                    format!("the router refused the request: {d}")
                }
            }
            _ => "the router refused the request".into(),
        });
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
    Some(Probe { model, firmware, mac, serial: None })
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

/// `/api/config/wan/rules2` → the index of the rule that actually carries the modem's APN, and that
/// setting in the app's vocabulary (`auto` | `manual` | `select`).
///
/// 🔴 NCOS keeps SEVERAL modem rules — generic class rules ("LTE-only Modems", `type|is|mdm%tech|is|lte`)
/// with no `modem` subtree, and a per-modem rule NCOS creates when the owner configures that SIM
/// (`…%uid|is|3a201cd3`) which holds the real `apn_mode`/`manual_apn`. Bench 2026-09-12, MVP's CBA850:
/// the per-modem rule was fifth in the list and held `manual` `mw01.VZWSTATIC` — a Verizon STATIC-IP
/// APN. The first version of this parser took the first `mdm` rule, found no subtree, reported the
/// modem as AUTOMATIC, and pointed APN writes at the generic rule; an owner pressing Apply on what the
/// panel showed would have been asking to drop the static IP. So: among `mdm` rules, one with a
/// `modem` subtree wins, and a uid-specific one beats a class rule.
///
/// NCOS spells automatic `default` (options: `default` | `manual` | `select`); `auto` is the app's word
/// and is translated at this boundary in both directions.
pub fn parse_apn(rules: &Value) -> Option<(usize, ApnConfig)> {
    let list = rules.as_array()?;
    let trigger = |r: &Value| r.get("trigger_string").and_then(|t| t.as_str()).unwrap_or("").to_string();
    let score = |r: &Value| -> Option<u8> {
        let t = trigger(r);
        let has_modem = r.get("modem").map(|m| m.is_object()).unwrap_or(false);
        if !t.contains("mdm") && !has_modem {
            return None;
        }
        Some(u8::from(has_modem) * 4 + u8::from(t.contains("uid|is|")) * 2 + u8::from(t.contains("mdm")))
    };
    let (idx, rule) = list
        .iter()
        .enumerate()
        .filter_map(|(i, r)| score(r).map(|sc| (sc, i, r)))
        // Highest score; on a tie the EARLIEST rule, which is NCOS's own list order.
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
        .map(|(_, i, r)| (i, r))?;
    let modem = rule.get("modem");
    let manual = modem.and_then(|m| str_of(m.get("manual_apn")));
    let mode = match modem.and_then(|m| str_of(m.get("apn_mode"))).map(|m| m.to_ascii_lowercase()) {
        Some(m) if m == "default" || m == "auto" => "auto".to_string(),
        Some(m) => m,
        None if manual.is_some() => "manual".into(),
        None => "auto".into(),
    };
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

/// The `modem.measurement` params for a Starlink — the same event and the same vendor-blind names
/// where they apply (`up`, `wan`, `model`, `fw`, `av`), plus the dish's own fields. No `sim`, no
/// `rsrp`, no `dataMb`: a dish has none, and inventing zeros would read as a broken modem. Usage
/// is not metered here either — a Starlink plan is not the cellular plan the KB deltas feed.
pub fn dish_params(d: &DishStatus, wan: &WanStatus, probe: Option<&Probe>) -> Vec<(String, String)> {
    let mut p: Vec<(String, String)> = vec![("up".into(), if wan.up { "1" } else { "0" }.into()), ("wan".into(), wan.wan.clone())];
    let mut push = |k: &str, v: Option<String>| {
        if let Some(v) = v {
            p.push((k.to_string(), v));
        }
    };
    let num = |v: Option<f64>| v.map(|n| if n.fract() == 0.0 { format!("{}", n as i64) } else { format!("{n:.1}") });
    push("model", probe.and_then(|p| p.model.clone()));
    push("fw", probe.and_then(|p| p.firmware.clone()));
    push("av", Some(format!("hub-{}", env!("CARGO_PKG_VERSION"))));
    push("uptime", d.uptime_s.map(|s| s.to_string()));
    push("outage", d.outage.clone());
    push("obstruction", num(d.obstruction_pct));
    push("obstructed", d.obstructed.map(|b| if b { "1" } else { "0" }.into()));
    push("latency", num(d.latency_ms));
    push("loss", num(d.loss_pct));
    push("downMbps", num(d.down_mbps));
    push("upMbps", num(d.up_mbps));
    push("signal", num(d.signal_pct));
    push("sats", d.gps_sats.map(|n| n.to_string()));
    if !d.alerts.is_empty() {
        push("alerts", Some(d.alerts.join(", ")));
    }
    p
}

/// What the poll loop reports for a snapshot, whichever vendor filled it: a modem's params, a
/// dish's params, or nothing (a read that learned neither reports nothing — never an empty
/// measurement that would look like a router with no modem).
pub fn report_params(snap: &Snapshot, wan_kb_delta: Option<u64>) -> Option<Vec<(String, String)>> {
    if let Some(m) = &snap.modem {
        return Some(modem_params(m, snap.wan.as_ref(), snap.probe.as_ref(), wan_kb_delta));
    }
    if let (Some(d), Some(w)) = (&snap.dish, &snap.wan) {
        return Some(dish_params(d, w, snap.probe.as_ref()));
    }
    None
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

    /// Write the APN: `manual` with a name, or `auto` (the modem picks the carrier profile).
    ///
    /// One PUT per LEAF, never the `modem` object: bench 2026-09-12 (CBA850 fw 7.0.50) — a PUT of
    /// `{"apn_mode":"auto"}` to `/api/config/wan/rules2/<i>/modem` came back `success:false`, the
    /// same way `/api/config/system/gps/enabled` is written as its own leaf and works. The name is
    /// written FIRST so the mode flip never points at an empty name.
    pub async fn set_apn(&self, cfg: &ApnConfig) -> Result<ApnConfig, String> {
        let (idx, _) = self.apn().await?;
        let modem = format!("/api/config/wan/rules2/{idx}/modem");
        if cfg.mode == "manual" {
            let apn = cfg.apn.as_deref().map(str::trim).filter(|a| !a.is_empty()).ok_or("an APN name is required for manual mode")?;
            self.put(&format!("{modem}/manual_apn"), &Value::String(apn.to_string())).await?;
            self.put(&format!("{modem}/apn_mode"), &Value::String("manual".into())).await?;
        } else {
            // NCOS's word for automatic is `default` — `auto` is refused as not one of the options.
            self.put(&format!("{modem}/apn_mode"), &Value::String("default".into())).await?;
        }
        Ok(self.apn().await?.1)
    }

    /// Read any `/api/status/…` or `/api/config/…` path — the diagnostic behind the app's router
    /// "read a path" tool and the way a driver for a new model is bench-checked before it is written.
    /// Read-only by construction (GET), scrubbed of anything that looks like a secret before it
    /// leaves the hub, and capped so a whole config tree cannot be pulled through the relay.
    pub async fn read_path(&self, path: &str) -> Result<Value, String> {
        let path = readable_path(path)?;
        let mut v = self.get(&path).await?;
        scrub_secrets(&mut v);
        let size = v.to_string().len();
        if size > MAX_READ_BYTES {
            return Err(format!("that path answers {size} bytes — ask for a narrower one (limit {MAX_READ_BYTES})"));
        }
        Ok(v)
    }

    pub async fn reboot(&self) -> Result<(), String> {
        self.put("/api/control/system", &serde_json::json!({ "reboot": true })).await.map(|_| ())
    }
}

pub(crate) fn reachability(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "the router did not answer (timed out) — is the hub on the same network?".into()
    } else if e.is_connect() {
        "the router could not be reached at that address".into()
    } else {
        e.without_url().to_string()
    }
}

// --- The vendor door ------------------------------------------------------------------------------

/// What one read of a router's GPS side learned: `enabled` is the router's own report (NCOS's
/// System → GPS switch; a Peplink's `gps` flag — whether the unit has GPS at all), `fix` the lock.
#[derive(Debug, Default)]
pub struct GpsRead {
    pub enabled: Option<bool>,
    pub fix: Option<GpsFix>,
}

/// One read of a device's status side — what `poll` and `probe` fill the Snapshot from. A router
/// fills `modem`/`wan`; a Starlink fills `dish`/`wan`.
#[derive(Debug, Default)]
pub struct StatusRead {
    pub modem: Option<ModemStatus>,
    pub wan: Option<WanStatus>,
    pub dish: Option<DishStatus>,
}

/// One signed-in device of whichever vendor. hub_server and `poll` speak only to this, so adding a
/// vendor is one module plus one arm per method — never a branch in the loop or the actions.
pub enum Driver<'a> {
    Cradlepoint(Ncos<'a>),
    Peplink(Peplink<'a>),
    Starlink(Starlink),
}

/// The vendors this hub manages — what `probe`/`add` accept, and what the app offers.
pub fn vendor_supported(v: &str) -> bool {
    matches!(v, "cradlepoint" | "peplink" | "starlink")
}

/// PURE: does this vendor sign in at all? A dish answers its LAN with no credential, so `add`
/// must not demand one and the app shows no sign-in fields.
pub fn needs_password(vendor: &str) -> bool {
    vendor != "starlink"
}

/// PURE: the port a config's `0` means for this vendor.
pub fn default_port(vendor: &str) -> u16 {
    if vendor == "starlink" { crate::starlink::DEFAULT_PORT } else { 443 }
}

/// PURE: what the hub can do for this vendor — the app gates its panel on this rather than on the
/// vendor name (the driver-abstraction rule). `modem`/`dish` say which status block the snapshot
/// carries; `gpsSwitch` that the device's own GNSS can be switched from here; `apn`, `reboot`,
/// `read` are the actions; `signin` that a username/password exists to rotate.
pub fn capabilities(vendor: &str) -> &'static [&'static str] {
    match vendor {
        "cradlepoint" => &["modem", "wan", "gps", "gpsSwitch", "apn", "reboot", "read", "signin"],
        "peplink" => &["modem", "wan", "gps", "signin"],
        "starlink" => &["dish", "wan", "gps", "reboot"],
        _ => &[],
    }
}

impl<'a> Driver<'a> {
    pub fn new(client: &'a reqwest::Client, vendor: &str, host: &str, port: u16, user: &str, pass: &str) -> Result<Self, String> {
        match vendor {
            "cradlepoint" => Ok(Driver::Cradlepoint(Ncos::new(client, host, port, user, pass))),
            "peplink" => Ok(Driver::Peplink(Peplink::new(client, host, port, user, pass))),
            "starlink" => Ok(Driver::Starlink(Starlink::new(host, port))),
            other => Err(format!("this hub cannot manage a '{other}' router yet")),
        }
    }

    pub fn for_router(client: &'a reqwest::Client, cfg: &RouterConfig) -> Result<Self, String> {
        Driver::new(client, &cfg.vendor, &cfg.host, cfg.port, &cfg.username, &cfg.password)
    }

    pub fn vendor(&self) -> &'static str {
        match self {
            Driver::Cradlepoint(_) => "cradlepoint",
            Driver::Peplink(_) => "peplink",
            Driver::Starlink(_) => "starlink",
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Driver::Cradlepoint(_) => "Cradlepoint",
            Driver::Peplink(_) => "Peplink",
            Driver::Starlink(_) => "Starlink",
        }
    }

    /// PURE: the refusal for a `do_routers` action this vendor cannot carry out through the hub,
    /// or None when it can. Peplink's local API offers no APN, no reboot, and the `read`
    /// diagnostic is an NCOS path reader; a Starlink has no APN, no sign-in to rotate, and no
    /// path reader either.
    pub fn unsupported_action(&self, action: &str) -> Option<String> {
        let what = match (self, action) {
            (Driver::Peplink(_), "apn") => "reading or setting the APN",
            (Driver::Peplink(_), "reboot") => "rebooting",
            (Driver::Peplink(_), "read") => "the read diagnostic",
            (Driver::Starlink(_), "apn") => "an APN — a dish has none;",
            (Driver::Starlink(_), "read") => "the read diagnostic",
            (Driver::Starlink(_), "password") => "a sign-in — a dish answers its LAN with no password;",
            _ => return None,
        };
        Some(unsupported(self.label(), what))
    }

    /// Prove the sign-in (where there is one) and read identity.
    pub async fn probe(&self) -> Result<Probe, String> {
        match self {
            Driver::Cradlepoint(n) => n.probe().await,
            Driver::Peplink(p) => p.probe().await,
            Driver::Starlink(s) => s.probe().await,
        }
    }

    /// The status side — one request on every vendor.
    pub async fn status(&self) -> Result<StatusRead, String> {
        match self {
            Driver::Cradlepoint(n) => {
                let d = n.wan_devices().await?;
                Ok(StatusRead { modem: parse_modem(&d), wan: parse_wan(&d), dish: None })
            }
            Driver::Peplink(p) => {
                let d = p.wan_connection().await?;
                Ok(StatusRead { modem: crate::peplink::parse_modem(&d), wan: crate::peplink::parse_wan(&d), dish: None })
            }
            Driver::Starlink(s) => {
                let d = crate::starlink::parse_status(&s.status().await?);
                Ok(StatusRead { modem: None, wan: Some(crate::starlink::wan_of(&d)), dish: Some(d) })
            }
        }
    }

    /// The GPS side. `want_fix` is the hub's own setting: a fix is fetched only when the owner
    /// asked for this router's position. Best-effort — a router that will not say is a router
    /// without GPS, not a failed poll.
    pub async fn gps(&self, want_fix: bool) -> GpsRead {
        match self {
            Driver::Cradlepoint(n) => GpsRead {
                enabled: n.gps_enabled().await.ok(),
                fix: if want_fix { n.gps_fix().await.ok().flatten() } else { None },
            },
            // No router-side switch to read; the `gps` flag arrives with the location, so one
            // request answers both — and none is made when the owner did not ask.
            Driver::Peplink(p) => {
                if !want_fix {
                    return GpsRead::default();
                }
                match p.location().await {
                    Ok((enabled, fix)) => GpsRead { enabled, fix },
                    Err(_) => GpsRead::default(),
                }
            }
            // There is no switch to read — Starlink's plan policy decides (starlink.rs header) —
            // and the only way to learn it is to ask for the position: a refusal is `enabled: false`.
            Driver::Starlink(s) => {
                if !want_fix {
                    return GpsRead::default();
                }
                match s.location().await {
                    Ok(fix) => GpsRead { enabled: Some(true), fix },
                    Err(_) => GpsRead { enabled: Some(false), fix: None },
                }
            }
        }
    }

    /// Switch the device's own GNSS. A Peplink has no such switch in its local API — GPS is on the
    /// model or it is not — so for it this is the hub-side setting alone, and succeeds without a
    /// request (the owner's intent is recorded; the poll reports fixes when the router has them).
    /// A Starlink has no switch at all — its plan policy decides (starlink.rs header): switching ON
    /// here asks the dish once so a refusal carries the reason; OFF is hub-side only.
    pub async fn set_gps_enabled(&self, on: bool) -> Result<(), String> {
        match self {
            Driver::Cradlepoint(n) => n.set_gps_enabled(on).await,
            Driver::Peplink(_) => Ok(()),
            Driver::Starlink(s) => {
                if on {
                    s.location().await.map(|_| ())
                } else {
                    Ok(())
                }
            }
        }
    }

    pub async fn apn(&self) -> Result<ApnConfig, String> {
        match self {
            Driver::Cradlepoint(n) => n.apn().await.map(|(_, a)| a),
            _ => Err(unsupported(self.label(), "reading the APN")),
        }
    }

    pub async fn set_apn(&self, cfg: &ApnConfig) -> Result<ApnConfig, String> {
        match self {
            Driver::Cradlepoint(n) => n.set_apn(cfg).await,
            _ => Err(unsupported(self.label(), "setting the APN")),
        }
    }

    pub async fn reboot(&self) -> Result<(), String> {
        match self {
            Driver::Cradlepoint(n) => n.reboot().await,
            Driver::Peplink(_) => Err(unsupported(self.label(), "rebooting")),
            Driver::Starlink(s) => s.reboot().await,
        }
    }
}

fn unsupported(vendor: &str, what: &str) -> String {
    format!("{what} is not supported on a {vendor} through the hub — use the device's own app or admin pages")
}

/// One full read of a device — the poll loop's unit of work and the `refresh` action.
pub async fn poll(client: &reqwest::Client, cfg: &RouterConfig, prev: Option<&Snapshot>) -> Snapshot {
    let now = crate::hub_server::now_ms();
    let mut snap = prev.cloned().unwrap_or_default();
    snap.at_ms = now;
    let drv = match Driver::for_router(client, cfg) {
        Ok(d) => d,
        Err(why) => {
            snap.error = Some(why);
            return snap;
        }
    };
    let status = match drv.status().await {
        Ok(s) => s,
        Err(why) => {
            snap.error = Some(why);
            return snap;
        }
    };
    snap.modem = status.modem;
    snap.wan = status.wan;
    snap.dish = status.dish;
    if snap.probe.is_none() {
        snap.probe = drv.probe().await.ok();
    }
    let gps = drv.gps(cfg.gps_enabled).await;
    snap.gps_enabled = gps.enabled;
    snap.fix = gps.fix.as_ref().map(FixOut::from);
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
    fn apn_reads_the_per_modem_rule_not_the_first_class_rule_bench_cba850() {
        // Verbatim shape of MVP's CBA850 /api/config/wan/rules2, 2026-09-12 (ids trimmed).
        let rules = json!([
            {"priority": 1, "trigger_name": "Ethernet", "trigger_string": "type|is|ethernet"},
            {"priority": 2, "trigger_name": "LTE-only Modems", "trigger_string": "type|is|mdm%tech|is|lte"},
            {"priority": 2.5, "trigger_name": "LTE/3G Multi-mode Modems", "trigger_string": "type|is|mdm%tech|is|lte/3g"},
            {"priority": 5, "trigger_name": "3G-only Modems", "trigger_string": "type|is|mdm%tech|is|3g"},
            {"modem": {"apn_mode": "manual", "manual_apn": "mw01.VZWSTATIC"}, "priority": 2.25,
             "trigger_name": "Modem-3a201cd3", "trigger_string": "type|is|mdm%tech|is|lte/3g%uid|is|3a201cd3"}
        ]);
        let (idx, apn) = parse_apn(&rules).unwrap();
        assert_eq!(idx, 4, "the write must target the rule that holds the APN");
        assert_eq!((apn.mode.as_str(), apn.apn.as_deref()), ("manual", Some("mw01.VZWSTATIC")));
    }

    #[test]
    fn ncos_default_is_the_apps_auto() {
        let rules = json!([{"trigger_string": "type|is|mdm%uid|is|x", "modem": {"apn_mode": "default", "manual_apn": "stale"}}]);
        let (_, a) = parse_apn(&rules).unwrap();
        assert_eq!((a.mode.as_str(), a.apn), ("auto", None));
        // No per-modem rule yet: the first class rule, automatic.
        let (i, b) = parse_apn(&json!([{"trigger_string": "type|is|ethernet"}, {"trigger_string": "type|is|mdm%tech|is|lte"}])).unwrap();
        assert_eq!((i, b.mode.as_str()), (1, "auto"));
    }

    #[test]
    fn measurement_params_match_hub_lite_names() {
        let m = parse_modem(&bench_devices()).unwrap();
        let w = parse_wan(&bench_devices());
        let p = Probe { model: Some("CBA850".into()), firmware: Some("7.0.50".into()), mac: None, serial: None };
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
    fn vendors_dispatch_and_each_refuses_what_its_api_lacks() {
        let client = lan_client();
        assert!(vendor_supported("cradlepoint") && vendor_supported("peplink") && vendor_supported("starlink") && !vendor_supported("teltonika"));
        assert!(Driver::new(&client, "teltonika", "h", 0, "", "").is_err());
        let cp = Driver::new(&client, "cradlepoint", "h", 0, "", "").unwrap();
        let pl = Driver::new(&client, "peplink", "h", 0, "", "").unwrap();
        let sl = Driver::new(&client, "starlink", "", 0, "", "").unwrap();
        assert_eq!((cp.vendor(), pl.vendor(), sl.vendor()), ("cradlepoint", "peplink", "starlink"));
        for a in ["apn", "reboot", "read"] {
            assert!(cp.unsupported_action(a).is_none(), "{a}");
            assert!(pl.unsupported_action(a).unwrap().contains("not supported on a Peplink"), "{a}");
        }
        for a in ["refresh", "gps", "password"] {
            assert!(pl.unsupported_action(a).is_none(), "{a}");
        }
        for a in ["apn", "read", "password"] {
            assert!(sl.unsupported_action(a).unwrap().contains("not supported on a Starlink"), "{a}");
        }
        for a in ["refresh", "gps", "reboot"] {
            assert!(sl.unsupported_action(a).is_none(), "{a}");
        }
        // The pure vendor facts the app and hub_server gate on.
        assert!(needs_password("cradlepoint") && needs_password("peplink") && !needs_password("starlink"));
        assert_eq!((default_port("cradlepoint"), default_port("peplink"), default_port("starlink")), (443, 443, 9200));
        assert!(capabilities("starlink").contains(&"dish") && !capabilities("starlink").contains(&"modem") && !capabilities("starlink").contains(&"signin"));
        assert!(capabilities("cradlepoint").contains(&"apn") && !capabilities("peplink").contains(&"apn"));
        assert!(capabilities("teltonika").is_empty());
    }

    #[tokio::test]
    async fn a_starlink_polls_through_the_same_door_and_reports_dish_params() {
        let port = crate::starlink::tests::mock_dish(false).await;
        let cfg = RouterConfig { vendor: "starlink".into(), host: "127.0.0.1".into(), port, gps_enabled: true, ..Default::default() };
        let snap = poll(&lan_client(), &cfg, None).await;
        assert_eq!(snap.error, None);
        assert!(snap.modem.is_none());
        let d = snap.dish.as_ref().expect("dish");
        assert_eq!(d.obstruction_pct, Some(0.2));
        assert_eq!(snap.wan.as_ref().map(|w| (w.wan.as_str(), w.up)), Some(("starlink", true)));
        assert_eq!(snap.probe.as_ref().and_then(|p| p.serial.as_deref()), Some("ut4088918f-05f0691c-19b97ab8"));
        // Location refused by the dish's policy: reported as GPS off at the dish, not as a failed poll.
        assert_eq!((snap.gps_enabled, snap.fix.is_none()), (Some(false), true));
        let params = report_params(&snap, None).unwrap();
        let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("up"), Some("1"));
        assert_eq!(get("wan"), Some("starlink"));
        assert_eq!(get("model"), Some("Starlink mini1_panda_proto1"));
        assert_eq!(get("uptime"), Some("91907"));
        assert_eq!(get("obstruction"), Some("0.2"));
        assert_eq!(get("latency"), Some("29.3"));
        assert_eq!(get("signal"), Some("100"));
        assert_eq!(get("sats"), Some("25"));
        assert_eq!(get("alerts"), Some("lower signal than predicted"));
        assert_eq!((get("sim"), get("rsrp"), get("dataMb"), get("outage")), (None, None, None, None));
        // The GPS switch: ON asks the dish and surfaces its refusal; OFF is hub-side only.
        let client = lan_client();
        let drv = Driver::for_router(&client, &cfg).unwrap();
        assert!(drv.set_gps_enabled(true).await.unwrap_err().contains("another position source"));
        assert!(drv.set_gps_enabled(false).await.is_ok());
        // Nothing to report when a read learned neither a modem nor a dish.
        assert!(report_params(&Snapshot::default(), None).is_none());
    }

    #[tokio::test]
    async fn peplink_gps_switch_is_hub_side_only_and_never_calls_the_router() {
        // Port 1 on localhost: any request would fail to connect, so Ok proves none was made.
        let client = lan_client();
        let pl = Driver::new(&client, "peplink", "127.0.0.1", 1, "admin", "x").unwrap();
        assert!(pl.set_gps_enabled(true).await.is_ok());
        assert!(pl.set_gps_enabled(false).await.is_ok());
        let off = pl.gps(false).await;
        assert!(off.enabled.is_none() && off.fix.is_none());
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

#[cfg(test)]
mod read_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_read_takes_status_or_config_paths_and_nothing_else() {
        assert!(readable_path("/api/status/lan/clients").is_ok());
        assert!(readable_path(" /api/config/wlan ").is_ok());
        assert!(readable_path("/api/config/wan/rules2/0/modem").is_ok());
        // Control endpoints reboot routers; the read tool must not reach them even with a GET.
        assert!(readable_path("/api/control/system").is_err());
        assert!(readable_path("/api/config/../control/system").is_err());
        assert!(readable_path("/api/config/lan?x=1").is_err());
        assert!(readable_path("/api/config/lan/").is_err());
        assert!(readable_path("/api/status//lan").is_err());
        assert!(readable_path("/api/config/lan;rm").is_err());
        assert!(readable_path("").is_err());
    }

    #[test]
    fn a_config_read_leaves_the_hub_without_its_secrets() {
        let mut v = json!({
            "system": { "admin": { "password": "$1$abc", "username": "admin" } },
            "wlan": { "radio": [ { "bss": [ { "ssid": "Boat", "wpapsk": "hunter2", "enabled": true } ] } ] },
            "vpn": { "ipsec": { "shared_key": "k" }, "sections": [] },
            "snmp": { "community": "public" },
            "nested": { "passwordPolicy": { "min": 8 } }
        });
        scrub_secrets(&mut v);
        assert_eq!(v["system"]["admin"]["password"], "•••");
        assert_eq!(v["system"]["admin"]["username"], "admin");
        assert_eq!(v["wlan"]["radio"][0]["bss"][0]["wpapsk"], "•••");
        assert_eq!(v["wlan"]["radio"][0]["bss"][0]["ssid"], "Boat");
        assert_eq!(v["vpn"]["ipsec"]["shared_key"], "•••");
        assert_eq!(v["snmp"]["community"], "•••");
        // An OBJECT under a secret-looking key is descended into, not blanked — a policy is not a secret.
        assert_eq!(v["nested"]["passwordPolicy"]["min"], 8);
    }

    #[test]
    fn a_refusal_carries_the_routers_reason_whatever_its_shape() {
        let s = ncos_data(&json!({"success": false, "reason": "bad value"})).unwrap_err();
        assert_eq!(s, "bad value");
        let o = ncos_data(&json!({"success": false, "data": {"apn_mode": "invalid choice"}})).unwrap_err();
        assert!(o.contains("apn_mode") && o.contains("invalid choice"), "{o}");
        let bare = ncos_data(&json!({"success": false})).unwrap_err();
        assert_eq!(bare, "the router refused the request");
    }
}
