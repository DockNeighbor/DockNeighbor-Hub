// THE BATCH WIRE SHAPES — every item and envelope the daemon puts on `POST /api/agent/batch`, in ONE
// module so the contract has one place to change.
//
// Contract of record: DockNeighbor-Cloud `src/agentBatch.ts` header at commit 4edb08b
// (feat/hub-cadence-15min), "THE CONSOLIDATED PAYLOAD". Summary of what this file must honour:
//
//   POST /api/agent/batch?vid=&device=<hub_id>&t=<hub token>[&ack=<ids>][&anchorsig=<sig>]
//   { "v": 1, "seq": N, "boot": "<per-process id>", "kind": "keyframe" | "delta",
//     "agent": { "av": "<daemon version>", "tier": "hub" }, "items": [{device, event, params}], "ok": [] }
//
//   * kind "keyframe" — the 15-minute payload. EVERY item is the device's COMPLETE current reading; the
//     cloud overwrites sensorState without a prev read. Exactly one `hub.status` item, device = the hub id.
//   * kind "delta" — immediate single events and outage replays; today's merge semantics.
//   * `wanKb_cellular|wifi|wired` are KB DELTAS since the last SENT report, summed across skipped sends.
//   * limits: ≤ 50 items, ≤ 24 params per item, values ≤ 256 chars, names ≤ 64, every value a string.
//     A keyframe that exceeds 50 items is split into several posts.
//   * reply 200: {status, processed, failed, touched, skipped, duplicate?, commands?, anchor?, linktap?}.
//
// Param NAMES are the daemon's established ones (routers.rs `modem_params`: `wan`, `up`; the valve's
// `battery`, `signal`, `rf`), not renamed to the contract's illustrative examples (`wanSrc`, `batt`):
// the app reads `wan` (connectivityStatus.ts) and `battery` (hubValveReading.ts) off sensorState, and
// the keyframe's rule is "every field the hub holds", which these are.

use serde_json::{json, Map, Value};

pub const MAX_ITEMS: usize = 50;
pub const MAX_PARAMS: usize = 24;
pub const MAX_VALUE_LEN: usize = 256;
pub const MAX_NAME_LEN: usize = 64;

/// The hub's own status item (cloud events.ts HUB_STATUS_EVENT) — classified as device state.
pub const HUB_STATUS_EVENT: &str = "hub.status";
/// The armed "checks in OK" item (cloud gpsFeed.ts GPS_HEARTBEAT_EVENT) — intercepted, never an alert.
pub const GPS_HEARTBEAT_EVENT: &str = "gps.heartbeat";
pub const GPS_FIX_EVENT: &str = "gps.measurement";
/// The tier this daemon declares in `agent.tier`.
pub const TIER: &str = "hub";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Keyframe,
    Delta,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Keyframe => "keyframe",
            Kind::Delta => "delta",
        }
    }
}

/// One batch item. Params keep wire order; duplicates are resolved last-wins by `clamp_item`.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub device: String,
    pub event: String,
    pub params: Vec<(String, String)>,
}

fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// PURE: make an item legal under the contract limits. Empty values are dropped (the cloud's
/// `str()` refuses a zero-length value and would reject the WHOLE batch), duplicate keys keep the
/// LAST value in the FIRST key's position, values are cut to 256 chars on a char boundary, and params
/// beyond 24 are dropped. Returns the item and how many params were dropped for the 24 cap.
pub fn clamp_item(item: &Item) -> (Item, usize) {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in &item.params {
        if k.is_empty() || k.len() > MAX_NAME_LEN || v.is_empty() {
            continue;
        }
        let v = cut(v, MAX_VALUE_LEN);
        match out.iter_mut().find(|(ok, _)| ok == k) {
            Some(slot) => slot.1 = v,
            None => out.push((k.clone(), v)),
        }
    }
    let dropped = out.len().saturating_sub(MAX_PARAMS);
    out.truncate(MAX_PARAMS);
    (Item { device: cut(&item.device, MAX_NAME_LEN), event: cut(&item.event, MAX_NAME_LEN), params: out }, dropped)
}

/// PURE: the JSON envelope for one post. Items must already be ≤ MAX_ITEMS (see `split`).
///
/// `seq` is OPTIONAL on the wire and the daemon sets it only on the spooled EVENT queue (kind delta),
/// which is strictly serialized and resends a failed post with the SAME seq — so a retry the cloud
/// already took is dropped whole instead of re-firing its alarms. Keyframes and heartbeats carry no
/// seq on purpose: they are idempotent full state, and a seq on them would race the event queue — a
/// heartbeat accepted as seq 11 would make a still-retrying event post with seq 10 read as a replay
/// (agentBatch.ts isNewSeq) and its alarms would be silently discarded.
pub fn envelope(kind: Kind, seq: Option<u64>, boot: &str, items: &[Item], av: &str) -> Value {
    let items: Vec<Value> = items
        .iter()
        .map(|it| {
            let (it, _) = clamp_item(it);
            let mut params = Map::new();
            for (k, v) in it.params {
                params.insert(k, Value::String(v));
            }
            json!({ "device": it.device, "event": it.event, "params": Value::Object(params) })
        })
        .collect();
    let mut body = json!({
        "v": 1,
        "boot": boot,
        "kind": kind.as_str(),
        "agent": { "av": av, "tier": TIER },
        "items": items,
        "ok": [],
    });
    if let Some(seq) = seq {
        body["seq"] = json!(seq);
    }
    body
}

/// PURE: split a payload into posts of at most MAX_ITEMS. The hub.status item (if present) rides the
/// FIRST post. An empty payload is one empty post — never zero posts, so a keyframe with nothing but
/// the hub still checks in.
pub fn split(items: Vec<Item>) -> Vec<Vec<Item>> {
    if items.is_empty() {
        return vec![Vec::new()];
    }
    let mut items = items;
    if let Some(i) = items.iter().position(|it| it.event == HUB_STATUS_EVENT) {
        let hs = items.remove(i);
        items.insert(0, hs);
    }
    items.chunks(MAX_ITEMS).map(|c| c.to_vec()).collect()
}

/// PURE: the batch URL. The only place the hub token meets a batch URL.
pub fn batch_url(worker_base: &str, vid: &str, hub_id: &str, token: &str, ack: Option<&str>, anchorsig: u64) -> Result<String, String> {
    let base = worker_base.trim_end_matches('/');
    let mut u = url::Url::parse(&format!("{base}/api/agent/batch")).map_err(|e| e.to_string())?;
    u.query_pairs_mut().append_pair("vid", vid).append_pair("device", hub_id).append_pair("t", token);
    if let Some(a) = ack.filter(|a| !a.is_empty()) {
        u.query_pairs_mut().append_pair("ack", a);
    }
    u.query_pairs_mut().append_pair("anchorsig", &anchorsig.to_string());
    Ok(u.to_string())
}

/// The `hub.status` item: `{name, ver, platform, update?, anchorsig}`.
pub fn hub_status_item(hub_id: &str, name: &str, ver: &str, platform: &str, update: Option<&str>, anchorsig: u64) -> Item {
    let mut params =
        vec![("name".to_string(), name.to_string()), ("ver".to_string(), ver.to_string()), ("platform".to_string(), platform.to_string())];
    if let Some(u) = update.filter(|u| !u.is_empty()) {
        params.push(("update".into(), u.to_string()));
    }
    params.push(("anchorsig".into(), anchorsig.to_string()));
    Item { device: hub_id.to_string(), event: HUB_STATUS_EVENT.into(), params }
}

/// The `gps.measurement` params: `lat lon acc? sog? sats? hdop? anchorsig`. `anchorsig` tells the
/// cloud this sender runs its own geofence (gpsFeed.ts shouldPersistGpsFix trusts it).
pub fn gps_fix_params(fix: &crate::gps::GpsFix, anchorsig: u64) -> Vec<(String, String)> {
    let mut p = vec![("lat".to_string(), format!("{:.6}", fix.lat)), ("lon".to_string(), format!("{:.6}", fix.lon))];
    if let Some(acc) = fix.acc {
        p.push(("acc".into(), format!("{acc:.1}")));
    }
    if let Some(sog) = fix.sog_kn {
        p.push(("sog".into(), format!("{sog:.1}")));
    }
    if let Some(n) = fix.sats {
        p.push(("sats".into(), n.to_string()));
    }
    if let Some(h) = fix.hdop {
        p.push(("hdop".into(), format!("{h:.1}")));
    }
    p.push(("anchorsig".into(), anchorsig.to_string()));
    p
}

pub fn gps_fix_item(device: &str, fix: &crate::gps::GpsFix, anchorsig: u64) -> Item {
    Item { device: device.to_string(), event: GPS_FIX_EVENT.into(), params: gps_fix_params(fix, anchorsig) }
}

pub fn heartbeat_item(device: &str, params: Vec<(String, String)>) -> Item {
    Item { device: device.to_string(), event: GPS_HEARTBEAT_EVENT.into(), params }
}

// ── STATE vs LIVE-ONLY telemetry (owner ruling, Jonathan 2026-09-19) ────────────────────────────
//
// Noisy live telemetry is NOT sent to the cloud unless someone is watching live (a watch LEASE is
// active, hub_server::leased). The hub keeps sampling and caching these values — the LAN API and the
// app on the boat still see them — but outside a lease a report carries only STATE.
//
// ⚠️ THE LIST IS THE CLOUD'S: DockNeighbor-Cloud `src/liveTelemetryFields.ts` (LIVE_TELEMETRY). Change
// it there first, then here and in hub-lite (`LIVE_ONLY_MODEM`/`LIVE_ONLY_LINKTAP`). The worker strips
// these fields itself whatever a hub sends, so a hub that sends too much costs a write, never a leak.
//
// UNKNOWN FIELDS ARE STATE. Only a field NAMED below is held back; anything a vendor adds tomorrow is
// sent, matching the worker's stance that an unclassified field fails its test rather than vanishing
// silently from the reading.
//
// `wanKb_*` is NOT here on purpose, although the Cloud list marks it live-only by prefix: the worker
// accounts every delta into the billing-cycle total (`private/wanusage_{device}`) from the RAW report
// before its own strip runs, so a delta must go out whenever there is one, leased or not. Only its copy
// on the reading document is dropped, and the worker does that.
//
// Change detection never reads a live-only field: cadence::router_is_event looks at `up`/`wan` only,
// cadence::valve_transition at `watering`/`rf`/the fault flags.

/// `modem.measurement` (routers, cellular/wired/Wi-Fi uplinks, a Starlink dish): sent only while leased.
pub const MODEM_LIVE_ONLY: &[&str] = &[
    "rssi", "rsrp", "rsrq", "sinr", // cellular signal
    "signal",                       // Starlink signal quality %
    "latency", "ping", "loss",      // Starlink PoP latency, ping, drop rate
    "obstruction", "obstructed",    // Starlink obstruction % and "obstructed right now"
    "uptime",                       // device uptime counter
    "downMbps", "upMbps",           // data-rate figures
    "sats",                         // GPS satellites in view (a router's; gps.measurement is another event)
];
/// `modem.measurement` STATE — always sent. Listed for the tests and the reader; the rule itself is
/// "everything not live-only", so a field missing here is still sent.
/// `upSrc`, `reason` and `atMs` are STATE and must never be stripped: `upSrc=unread` is the whole
/// reason an unreadable router is not reported as a down one (router_health), so losing it outside
/// a lease would put back exactly the defect it was added to fix. `atMs` is listed to keep this
/// list name-for-name with the cloud's (`liveTelemetryFields.ts`), but 0.3.54 does not emit it —
/// a fresh timestamp on every poll would rewrite the reading document on every check-in, which is
/// the cost the live-only rule exists to avoid; the worker falls back to arrival time.
pub const MODEM_STATE: &[&str] = &[
    "up", "upSrc", "reason", "atMs", "outage", "alerts", "carrier", "mode", "sim", "wan", "wanSrc", "dataMb", "ip", "model", "fw",
    "av", "update", "released",
];
/// `linktap.measurement`: the valve's radio signal is the one live-only field.
pub const LINKTAP_LIVE_ONLY: &[&str] = &["signal"];
/// `linktap.measurement` STATE — always sent (see MODEM_STATE).
pub const LINKTAP_STATE: &[&str] = &[
    "watering", "vol_l", "meters", "battery", "rf", "broken", "leak", "clog", "cutoff", "flow_lpm", "day", "day_vol_l", "mode", "dur_s",
    "cap_l", "remain_s", "prov",
];

/// PURE: the live-only fields of `event` (empty for any event the ruling does not cover).
pub fn live_only_fields(event: &str) -> &'static [&'static str] {
    match event {
        "modem.measurement" => MODEM_LIVE_ONLY,
        "linktap.measurement" => LINKTAP_LIVE_ONLY,
        _ => &[],
    }
}

/// PURE: the item as it may go on the wire. Leased ⇒ unchanged. Not leased ⇒ its live-only fields
/// removed; every other field (state, `wanKb_*`, anything unclassified) kept in order.
pub fn for_the_wire(item: Item, leased: bool) -> Item {
    let live = live_only_fields(&item.event);
    if leased || live.is_empty() {
        return item;
    }
    Item { params: item.params.into_iter().filter(|(k, _)| !live.contains(&k.as_str())).collect(), ..item }
}

/// A router's `modem.measurement`, with the KB accumulated since the last SENT report (only when
/// non-zero — "never send totals", and a zero delta is noise).
pub fn router_item(device: &str, params: &[(String, String)], wan_kb_cellular: u64) -> Item {
    let mut p: Vec<(String, String)> = params.iter().filter(|(k, _)| !k.starts_with("wanKb_")).cloned().collect();
    if wan_kb_cellular > 0 {
        p.push(("wanKb_cellular".into(), wan_kb_cellular.to_string()));
    }
    Item { device: device.to_string(), event: "modem.measurement".into(), params: p }
}

pub fn valve_item(dev_id: &str, params: &[(String, String)]) -> Item {
    Item { device: format!("lt_{dev_id}"), event: "linktap.measurement".into(), params: params.to_vec() }
}

pub fn from_report(r: &crate::linktap_runtime::Report) -> Item {
    Item { device: r.device.clone(), event: r.event.clone(), params: r.params.clone() }
}

/// Param names whose VALUE the debug log never prints (substring match, case-insensitive).
const SECRET_NAME_PARTS: &[&str] = &["token", "secret", "password", "passwd", "psk", "apikey", "api_key", "auth"];

/// PURE: the hub-log line for one outgoing post when `debug_log_batches` is on (hub_config.rs) —
/// the URL with the hub token (`t=`) redacted, then the JSON body with any secret-looking param
/// value redacted. The token lives only in the URL today; the body scrub is a guard for a future
/// field, not a known leak.
pub fn debug_line(url: &str, body: &Value) -> String {
    const REDACTED: &str = "[redacted]";
    let secret = |k: &str| {
        let k = k.to_ascii_lowercase();
        k == "t" || k == "k" || SECRET_NAME_PARTS.iter().any(|p| k.contains(p))
    };
    let shown_url = match url::Url::parse(url) {
        Ok(mut u) => {
            let pairs: Vec<(String, String)> = u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
            u.query_pairs_mut().clear();
            for (k, v) in pairs {
                u.query_pairs_mut().append_pair(&k, if secret(&k) { REDACTED } else { &v });
            }
            u.to_string()
        }
        Err(_) => REDACTED.to_string(), // never print a URL we could not scrub
    };
    let mut body = body.clone();
    if let Some(items) = body.get_mut("items").and_then(Value::as_array_mut) {
        for it in items {
            if let Some(params) = it.get_mut("params").and_then(Value::as_object_mut) {
                for (k, v) in params.iter_mut() {
                    if secret(k) {
                        *v = Value::String(REDACTED.into());
                    }
                }
            }
        }
    }
    format!("batch debug: POST {shown_url} {body}")
}

/// PURE: a note for the log when the worker accepted a batch but rejected some of its items.
///
/// 🔴 WHY THIS EXISTS (2026-09-16): a Peplink GPS source's underway fixes were posted, the batch was
/// answered 200, and nothing was stored — and the hub logged nothing, because it only logged a WHOLE
/// batch refused or queued. The reply's `failed` count was the only trace, and nobody read it. The worker
/// counts per-item failures but does not name them, so this lists what the post carried (event@device,
/// de-duplicated, capped) beside the count: enough to tell which source to look at. `None` when nothing
/// failed or the reply has no usable count.
pub fn failed_items_note(reply: &Value, items: &[Item]) -> Option<String> {
    let failed = match reply.get("failed") {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse::<u64>().unwrap_or(0),
        _ => 0,
    };
    if failed == 0 {
        return None;
    }
    const MAX_LISTED: usize = 12;
    let mut seen: Vec<String> = Vec::new();
    for it in items {
        let tag = format!("{}@{}", it.event, it.device);
        if !seen.contains(&tag) {
            seen.push(tag);
        }
    }
    let more = seen.len().saturating_sub(MAX_LISTED);
    seen.truncate(MAX_LISTED);
    let mut list = seen.join(", ");
    if more > 0 {
        list.push_str(&format!(", +{more} more"));
    }
    Some(format!("cloud rejected {failed} of {} report(s) in a delivered batch: {list}", items.len()))
}

/// Real readings from the real param builders, for the live-only tests here and in hub_server.
#[cfg(test)]
pub mod fixtures {
    use crate::routers::{self, ModemStatus, Probe, WanStatus};
    use crate::starlink::DishStatus;

    fn probe(model: &str) -> Probe {
        Probe { model: Some(model.into()), firmware: Some("7.0.50".into()), ..Default::default() }
    }

    /// A Cradlepoint on LTE — every modem field the builder emits.
    pub fn lte() -> Vec<(String, String)> {
        let m = ModemStatus {
            sim: "ok".into(), carrier: Some("Verizon".into()), mode: Some("LTE".into()),
            rssi: Some(-71.0), rsrp: Some(-101.0), rsrq: Some(-9.0), sinr: Some(12.0), connected: Some(true),
            ip: Some("100.64.3.9".into()), tx_bytes: Some(400 << 20), rx_bytes: Some(900 << 20),
        };
        let w = WanStatus { wan: "lte".into(), up: true, up_known: true, ip: None, uptime_s: Some(3600) };
        routers::modem_params(&m, Some(&w), Some(&probe("CBA850")), None)
    }

    /// A Starlink dish in an obstruction outage — every dish field the builder emits.
    pub fn dish() -> Vec<(String, String)> {
        let d = DishStatus {
            uptime_s: Some(86_400), obstruction_pct: Some(2.5), obstructed: Some(true), outage: Some("obstructed".into()),
            latency_ms: Some(31.0), loss_pct: Some(0.4), down_mbps: Some(180.2), up_mbps: Some(14.1), signal_pct: Some(93.0),
            alerts: vec!["thermal throttle".into()], gps_sats: Some(14), ..Default::default()
        };
        let w = crate::starlink::wan_of(&d);
        routers::dish_params(&d, &w, Some(&probe("UTA-212")))
    }

    /// A wired Peplink Balance — up/wan, identity and its uplink uptime.
    pub fn wired() -> Vec<(String, String)> {
        let w = WanStatus { wan: "wired".into(), up: true, up_known: true, ip: Some("203.0.113.7".into()), uptime_s: Some(7200) };
        routers::wan_params(&w, Some(&probe("Balance One")))
    }

    /// The gateway payload a watering LinkTap valve answers with (fields linktap_runtime reads).
    pub fn valve_payload() -> serde_json::Value {
        serde_json::json!({"is_watering":1,"vol":3.2,"speed":5.5,"battery":93,"signal":69,"is_rf_linked":true,
            "is_broken":false,"is_leak":false,"is_clog":false,"is_cutoff":false})
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn failed_items_note_names_what_the_post_carried_only_when_something_failed() {
        let it = |d: &str, e: &str| Item { device: d.into(), event: e.into(), params: vec![] };
        let items = vec![it("brv_gps_a", "gps.measurement"), it("brv_gps_a", "gps.measurement"), it("hub_1", "hub.status")];
        assert_eq!(failed_items_note(&json!({"status":"ok","processed":3,"failed":0}), &items), None);
        assert_eq!(failed_items_note(&json!({"status":"ok"}), &items), None, "no count is not a failure");
        let note = failed_items_note(&json!({"status":"ok","processed":1,"failed":2}), &items).expect("a failure is logged");
        assert_eq!(note, "cloud rejected 2 of 3 report(s) in a delivered batch: gps.measurement@brv_gps_a, hub.status@hub_1");
        assert!(failed_items_note(&json!({"failed":"1"}), &items).is_some(), "a string count still counts");
    }

    #[test]
    fn failed_items_note_caps_the_list() {
        let items: Vec<Item> = (0..20).map(|n| Item { device: format!("sh_{n}"), event: "temperature.measurement".into(), params: vec![] }).collect();
        let note = failed_items_note(&json!({"failed": 20}), &items).unwrap();
        assert!(note.ends_with(", +8 more"), "{note}");
    }
    use super::*;
    use crate::gps::GpsFix;

    fn keyframe_items(n_shellys: usize) -> Vec<Item> {
        let mut items = vec![hub_status_item("hub_8f39", "CENTRAL", "0.3.49", "linux", Some("0.3.50"), 0)];
        let modem: Vec<(String, String)> = [
            ("up", "1"),
            ("mode", "LTE"),
            ("rssi", "-71"),
            ("rsrp", "-101"),
            ("sinr", "12"),
            ("rsrq", "-9"),
            ("carrier", "Verizon"),
            ("sim", "ok"),
            ("dataMb", "1234"),
            ("wan", "lte"),
            ("ip", "100.64.3.9"),
            ("model", "CBA850"),
            ("fw", "7.0.50"),
            ("av", "hub-0.3.49"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        items.push(router_item("brv_net_cp850", &modem, 812));
        items.push(valve_item("ABC123", &[("watering".into(), "0".into()), ("battery".into(), "92".into()), ("rf".into(), "1".into())]));
        let fix = GpsFix { lat: 41.4929, lon: -81.6943, acc: Some(4.0), hdop: Some(0.9), sats: Some(11), sog_kn: Some(0.1) };
        items.push(gps_fix_item("brv_gps_a", &fix, 0));
        for i in 0..n_shellys {
            items.push(Item {
                device: format!("shellyht-{i}"),
                event: "temperature.measurement".into(),
                params: vec![("tC".into(), "21.5".into()), ("rh".into(), "60".into())],
            });
        }
        items
    }

    fn assert_within_limits(body: &Value) {
        assert_eq!(body["v"], 1);
        let items = body["items"].as_array().unwrap();
        assert!(items.len() <= MAX_ITEMS, "{} items", items.len());
        assert!(body["ok"].as_array().unwrap().is_empty());
        assert_eq!(body["agent"]["tier"], "hub");
        for it in items {
            let dev = it["device"].as_str().unwrap();
            let ev = it["event"].as_str().unwrap();
            assert!(!dev.is_empty() && dev.len() <= MAX_NAME_LEN && !ev.is_empty() && ev.len() <= MAX_NAME_LEN);
            let params = it["params"].as_object().unwrap();
            assert!(params.len() <= MAX_PARAMS, "{dev} carries {} params", params.len());
            for (k, v) in params {
                let s = v.as_str().unwrap_or_else(|| panic!("{dev}.{k} is not a string"));
                assert!(!s.is_empty() && s.len() <= MAX_VALUE_LEN && k.len() <= MAX_NAME_LEN, "{dev}.{k}");
            }
        }
    }

    #[test]
    fn a_keyframe_serialises_inside_the_contract_limits_and_splits_past_fifty_items() {
        // A realistic boat: one post.
        let posts = split(keyframe_items(5));
        assert_eq!(posts.len(), 1);
        let body = envelope(Kind::Keyframe, None, "a1b2c3d4", &posts[0], "0.3.49");
        assert_within_limits(&body);
        assert_eq!(body["kind"], "keyframe");
        assert!(body.get("seq").is_none(), "keyframes carry no seq (see envelope)");
        assert_eq!(envelope(Kind::Delta, Some(1201), "a1b2c3d4", &[], "0.3.49")["seq"], 1201);
        assert_eq!(body["boot"], "a1b2c3d4");
        let items = body["items"].as_array().unwrap();
        assert_eq!(items.iter().filter(|i| i["event"] == HUB_STATUS_EVENT).count(), 1, "exactly one hub.status");
        assert_eq!(items[0]["device"], "hub_8f39", "hub.status device is the hub's own id");
        assert_eq!(items[0]["params"]["update"], "0.3.50");
        assert_eq!(items[1]["params"]["wanKb_cellular"], "812");
        assert_eq!(items[3]["params"]["sats"], "11");
        assert_eq!(items[3]["params"]["hdop"], "0.9");
        assert_eq!(items[3]["params"]["sog"], "0.1");

        // A marina facility with 120 sensors: three posts, each legal, hub.status only in the first.
        let posts = split(keyframe_items(120));
        assert_eq!(posts.len(), 3);
        for (i, p) in posts.iter().enumerate() {
            let body = envelope(Kind::Keyframe, None, "b", p, "0.3.49");
            assert_within_limits(&body);
            let hs = body["items"].as_array().unwrap().iter().filter(|x| x["event"] == HUB_STATUS_EVENT).count();
            assert_eq!(hs, usize::from(i == 0));
        }
        assert_eq!(split(Vec::new()).len(), 1, "an empty keyframe is still one check-in");
    }

    #[test]
    fn an_oversized_item_is_clamped_rather_than_rejecting_the_whole_batch() {
        let mut params: Vec<(String, String)> = (0..30).map(|i| (format!("k{i}"), "v".to_string())).collect();
        params.push(("k0".into(), "last-wins".into()));
        params.push(("empty".into(), String::new()));
        params.push(("long".into(), "é".repeat(300)));
        let (it, dropped) = clamp_item(&Item { device: "d".into(), event: "e".into(), params });
        assert_eq!(it.params.len(), MAX_PARAMS);
        assert_eq!(dropped, 7, "31 distinct non-empty keys, 24 kept");
        assert_eq!(it.params[0], ("k0".into(), "last-wins".into()));
        assert!(it.params.iter().all(|(_, v)| !v.is_empty()));
        let body = envelope(
            Kind::Delta,
            Some(1),
            "b",
            &[Item { device: "d".into(), event: "e".into(), params: vec![("long".into(), "é".repeat(300))] }],
            "x",
        );
        assert_within_limits(&body);
    }

    #[test]
    fn wan_deltas_are_never_totals_and_a_zero_delta_is_omitted() {
        let it = router_item("r", &[("wanKb_cellular".into(), "999".into()), ("up".into(), "1".into())], 0);
        assert!(!it.params.iter().any(|(k, _)| k.starts_with("wanKb_")), "a stale delta in the latest params is never re-sent");
        let it = router_item("r", &[("up".into(), "1".into())], 40);
        assert_eq!(it.params.last().unwrap(), &("wanKb_cellular".to_string(), "40".to_string()));
    }

    fn keys(it: &Item) -> Vec<&str> {
        it.params.iter().map(|(k, _)| k.as_str()).collect()
    }

    /// The relationship, not a value: unleased = exactly the input minus the live-only fields, in
    /// order; leased = the input untouched. Plus: something WAS stripped, and every STATE field the
    /// reading had survives.
    fn assert_strips_live_only(event: &str, params: Vec<(String, String)>, live: &[&str], state: &[&str]) {
        let it = Item { device: "d".into(), event: event.into(), params: params.clone() };
        let had_live: Vec<&str> = params.iter().map(|(k, _)| k.as_str()).filter(|k| live.contains(k)).collect();
        assert!(!had_live.is_empty(), "the fixture must carry live-only fields to prove anything: {params:?}");
        let unleased = for_the_wire(it.clone(), false);
        for k in keys(&unleased) {
            assert!(!live.contains(&k), "{event}: live-only `{k}` went out with no lease");
        }
        let expected: Vec<(String, String)> = params.iter().filter(|(k, _)| !live.contains(&k.as_str())).cloned().collect();
        assert_eq!(unleased.params, expected, "{event}: everything else is kept, in order");
        for (k, _) in &params {
            if state.contains(&k.as_str()) {
                assert!(keys(&unleased).contains(&k.as_str()), "{event}: state `{k}` must always go out");
            }
        }
        assert_eq!(for_the_wire(it.clone(), true), it, "{event}: a lease sends everything");
    }

    #[test]
    fn an_unleased_lte_modem_dish_and_wired_router_send_state_only_and_a_lease_sends_everything() {
        use super::fixtures::*;
        for p in [lte(), dish(), wired()] {
            assert_strips_live_only("modem.measurement", p, MODEM_LIVE_ONLY, MODEM_STATE);
        }
        // Spot the ones that matter on the card, so a fixture that stopped emitting them fails here.
        let lte = for_the_wire(Item { device: "r".into(), event: "modem.measurement".into(), params: lte() }, false);
        assert_eq!(keys(&lte), ["up", "upSrc", "mode", "carrier", "sim", "dataMb", "wan", "ip", "model", "fw", "av"]);
        let dish = for_the_wire(Item { device: "s".into(), event: "modem.measurement".into(), params: dish() }, false);
        assert_eq!(keys(&dish), ["up", "upSrc", "wan", "model", "fw", "av", "outage", "alerts"]);
        let wired = for_the_wire(Item { device: "w".into(), event: "modem.measurement".into(), params: wired() }, false);
        assert_eq!(keys(&wired), ["up", "upSrc", "wan", "ip", "model", "fw", "av"]);
        // 🔴 `upSrc` IS STATE: strip it outside a lease and an unreadable router reads as a down one
        // again — the whole of D1. Proven on a HELD reading, which is the one that carries `unread`.
        let held = crate::router_health::unread_params(Some(&super::fixtures::wired()), Some("timeout"), &[]);
        let out = for_the_wire(Item { device: "w".into(), event: "modem.measurement".into(), params: held }, false);
        assert!(out.params.contains(&("upSrc".to_string(), "unread".to_string())), "{:?}", keys(&out));
        assert!(out.params.contains(&("reason".to_string(), "timeout".to_string())), "{:?}", keys(&out));
    }

    #[test]
    fn a_wan_delta_goes_out_unleased_whenever_there_is_one() {
        use super::fixtures::lte;
        let it = for_the_wire(router_item("r", &lte(), 812), false);
        assert_eq!(it.params.last().unwrap(), &("wanKb_cellular".to_string(), "812".to_string()), "the worker accounts every delta");
        assert!(!keys(&it).contains(&"rsrp"));
        let it = for_the_wire(router_item("r", &lte(), 0), false);
        assert!(!keys(&it).iter().any(|k| k.starts_with("wanKb_")), "no delta, nothing to account");
    }

    #[test]
    fn a_valve_sends_its_signal_only_while_leased() {
        let params: Vec<(String, String)> = [
            ("watering", "1"), ("vol_l", "3.20"), ("meters", "1"), ("battery", "93"), ("signal", "69"), ("rf", "1"),
            ("broken", "0"), ("leak", "0"), ("clog", "0"), ("cutoff", "0"), ("flow_lpm", "5.5"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_strips_live_only("linktap.measurement", params, LINKTAP_LIVE_ONLY, LINKTAP_STATE);
    }

    #[test]
    fn unclassified_fields_and_other_events_are_sent_as_they_are() {
        // A field a vendor adds tomorrow is STATE until the Cloud list says otherwise.
        let it = Item { device: "r".into(), event: "modem.measurement".into(), params: vec![("band".into(), "B13".into()), ("rsrp".into(), "-99".into())] };
        assert_eq!(for_the_wire(it, false).params, vec![("band".to_string(), "B13".to_string())]);
        // `sats` is live-only on a ROUTER's reading, never on a GPS fix (another event).
        let fix = crate::gps::GpsFix { lat: 41.5, lon: -81.7, sats: Some(11), ..Default::default() };
        let gps = gps_fix_item("brv_gps_a", &fix, 0);
        assert_eq!(for_the_wire(gps.clone(), false), gps);
        let t = Item { device: "sh".into(), event: "temperature.measurement".into(), params: vec![("signal".into(), "-60".into())] };
        assert_eq!(for_the_wire(t.clone(), false), t);
    }

    #[test]
    fn the_lists_agree_with_each_other() {
        for (live, state) in [(MODEM_LIVE_ONLY, MODEM_STATE), (LINKTAP_LIVE_ONLY, LINKTAP_STATE)] {
            for k in live {
                assert!(!state.contains(k), "`{k}` classified twice");
                assert!(!k.starts_with("wanKb_"), "a WAN delta is never held back by the hub");
            }
        }
        assert_eq!(live_only_fields("modem.measurement"), MODEM_LIVE_ONLY);
        assert_eq!(live_only_fields("linktap.measurement"), LINKTAP_LIVE_ONLY);
        assert!(live_only_fields("hub.status").is_empty());
    }

    #[test]
    fn the_debug_line_never_prints_the_hub_token_or_a_secret_param() {
        let url = batch_url("https://api.example.com", "v1", "hub_1", "hubtok-SECRET", Some("c1"), 7).unwrap();
        let mut body = envelope(Kind::Keyframe, None, "b", &[router_item("r", &[("up".into(), "1".into())], 5)], "0.3.53");
        body["items"][0]["params"]["apiToken"] = json!("agt-SECRET");
        body["items"][0]["params"]["password"] = json!("pw-SECRET");
        let line = debug_line(&url, &body);
        assert!(!line.contains("SECRET"), "{line}");
        assert!(line.contains("t=%5Bredacted%5D") || line.contains("t=[redacted]"), "{line}");
        assert!(line.contains("vid=v1") && line.contains("anchorsig=7") && line.contains("\"wanKb_cellular\":\"5\""), "the rest is shown: {line}");
        assert!(!debug_line("not a url ?t=hubtok-SECRET", &body).contains("SECRET"), "an unparseable URL is not printed");
    }

    #[test]
    fn the_batch_url_carries_the_hub_identity_ack_and_anchorsig() {
        let u = url::Url::parse(&batch_url("https://api.example.com/", "v1", "hub_1", "tok", Some("c1,c2"), 1234).unwrap()).unwrap();
        assert_eq!(u.path(), "/api/agent/batch");
        let q: std::collections::HashMap<String, String> = u.query_pairs().into_owned().collect();
        assert_eq!((q["vid"].as_str(), q["device"].as_str(), q["t"].as_str()), ("v1", "hub_1", "tok"));
        assert_eq!((q["ack"].as_str(), q["anchorsig"].as_str()), ("c1,c2", "1234"));
        let u = batch_url("https://api.example.com", "v1", "hub_1", "tok", Some(""), 0).unwrap();
        assert!(!u.contains("ack="));
    }
}
