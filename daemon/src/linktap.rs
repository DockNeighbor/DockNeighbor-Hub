//! LinkTap gateway client — LOCAL HTTP ONLY, no MQTT broker, no LinkTap cloud.
//!
//! PORTED from the TypeScript reference (`hub/src/linktap.ts`, frozen 2026-08-19 when the project
//! converged on this daemon) against its fixtures. The one-contract rule applies: the cases in
//! `hub/test/linktap.test.ts` are mirrored in this file's tests, and the unit truths below were
//! MEASURED on hardware — do not re-derive them, this protocol has already produced two inverted
//! unit bugs in opposite directions.
//!
//! WHY HTTP IS ENOUGH, from the vendor's own document: `LinkTap_Gateway_MQTT_Client_Integration.pdf`
//! v2.1 §4.2 — commands are plain HTTP POSTs of JSON to `http://<gateway>/api.shtml`, and the
//! reachable set is defined BY DIRECTION rather than by an allowlist ("message definitions ...
//! where 'Message direction' is 'App->Broker->GW'"). §4 is explicit that this works with neither
//! internet nor broker available.

use std::time::Duration;

const GATEWAY_TIMEOUT: Duration = Duration::from_secs(15);

/// The idle-latch guard, carried over verbatim. A CLOSED valve reports GARBAGE in `volume` — the
/// live GW-02 sat at 15,886,307.00 while idle. Anything past this bound is that latch, not water,
/// and must never reach a limit comparison or the usage history.
pub const MAX_PLAUSIBLE_CYCLE_VOLUME: f64 = 100_000.0;

pub const LITRES_PER_GALLON: f64 = 3.785_411_784;

/// The gateway's configured volume unit (`cmd 16` → `vol_unit`). MVP's gateway is `gal`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolUnit {
    Gal,
    Litre,
}

impl VolUnit {
    /// Litres from a raw reading in this unit.
    pub fn to_litres(self, raw: f64) -> f64 {
        match self {
            VolUnit::Gal => raw * LITRES_PER_GALLON,
            VolUnit::Litre => raw,
        }
    }

    /// The inverse — a litre cap expressed in the gateway's own unit, for `volume_limit`.
    pub fn from_litres(self, litres: f64) -> f64 {
        match self {
            VolUnit::Gal => litres / LITRES_PER_GALLON,
            VolUnit::Litre => litres,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Gateway {
    pub host: String,
    pub gw_id: String,
}

/// The gateway's full return-code table, recovered from the Hubitat MQTT driver — the only
/// published decoding of this field. `5` and `7` are the two that actually bite: 5 means the RF
/// join failed (valve unpowered / out of range / not in pairing mode), 7 means a watering PLAN is
/// blocking the operation, which is why adopting a gateway clears its plans.
pub fn describe_ret(ret: i64) -> &'static str {
    match ret {
        0 => "Success",
        1 => "Message format error",
        2 => "CMD message not supported",
        3 => "Gateway ID not matched",
        4 => "End device ID error",
        5 => "End device ID not found",
        6 => "Gateway internal error",
        7 => "Conflict with watering plan",
        _ => "unknown gateway error",
    }
}

/// LinkTap device ids are 16 hex characters; a printed label may carry a suffix. Normalising on
/// the way in stops a pasted label from failing to match what the gateway reports back.
pub fn normalize_dev_id(id: &str) -> String {
    id.trim().chars().take(16).collect()
}

/// The gateway wraps its reply in HTML unless that is disabled in its admin page
/// (`<!--#RET-->{...}` inside a `<body>`). We cannot assume an operator changed that setting.
pub fn extract_json(raw: &str) -> &str {
    if raw.contains("<html") || raw.contains("<body") {
        if let (Some(a), Some(b)) = (raw.find('{'), raw.rfind('}')) {
            if b > a {
                return &raw[a..=b];
            }
        }
    }
    raw
}

/// LinkTap's "is watering" flag arrives as true/'true'/1/'1' across firmwares and sources.
pub fn coerce_watering(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        serde_json::Value::String(s) => s == "true" || s == "1",
        _ => false,
    }
}

/// THE VALVE'S OWN ANSWER to "are you still running", out of a `cmd 3` reply — `Some(true)` open,
/// `Some(false)` shut, `None` when this reply does not say.
///
/// 🔴 `None` IS NOT `Some(false)`, AND THE DIFFERENCE IS THE WHOLE VALUE OF THIS FUNCTION. It is the
/// confirmation source for a close (close_watch.rs): a gateway that timed out, answered HTTP 500,
/// returned junk, or replied about a valve it cannot reach over RF has told us NOTHING about the
/// valve. Folding that into "not watering" would confirm a close that never happened — silently, and
/// in the direction of looking correct. Callers must treat `None` as "still open, keep trying".
///
/// Reads the same `dev_stat[0]`-or-bare-object shape and the same `is_watering` field, through the
/// same `coerce_watering`, that the poll loop already feeds to the cycle machine — so a close is
/// confirmed by exactly the fact the rest of the hub calls "the valve is open".
pub fn watering_from_status(data: &serde_json::Value) -> Option<bool> {
    let d = data
        .get("dev_stat")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .unwrap_or(data);
    d.get("is_watering").map(coerce_watering)
}

/// Does this valve actually METER flow? `is_flm_plugin` is authoritative when present. LinkTap
/// meters on the G2/G2S only, so a non-metering valve can only ever be bounded by TIME.
pub fn reports_volume(data: &serde_json::Value) -> bool {
    if let Some(b) = data.get("is_flm_plugin").and_then(|v| v.as_bool()) {
        return b;
    }
    data.get("volume").and_then(|v| v.as_f64()).is_some()
}

/// Litres delivered THIS CYCLE.
///
/// ⚠️ MEASURED ON HARDWARE 2026-08-18. `volume` is the CYCLE TOTAL in the gateway's configured
/// unit and RESETS each cycle — NOT LinkTap's cloud `vol` (millilitres, absent from the LAN
/// payload) and NOT a lifetime counter in thousandths. Reading it as a counter under-reported by
/// ~1000x, which is the DANGEROUS direction: a software cutoff compares against this number, so
/// it could never arm.
pub fn cycle_volume_litres(data: &serde_json::Value, unit: VolUnit) -> f64 {
    let raw = data.get("volume").and_then(|v| v.as_f64()).unwrap_or(f64::NAN);
    if !raw.is_finite() || raw <= 0.0 || raw > MAX_PLAUSIBLE_CYCLE_VOLUME {
        return 0.0;
    }
    unit.to_litres(raw)
}

// --- command builders (pure) --------------------------------------------------------------------

pub fn build_status(gw: &Gateway, dev_id: &str) -> serde_json::Value {
    serde_json::json!({ "cmd": 3, "gw_id": gw.gw_id, "dev_id": dev_id })
}

pub fn build_get_configuration(gw: &Gateway) -> serde_json::Value {
    serde_json::json!({ "cmd": 16, "gw_id": gw.gw_id })
}

/// Instantaneous flow in LITRES PER MINUTE from a status payload.
///
/// `speed` is reported in the GATEWAY's configured unit per minute — gal/min on a `gal` gateway —
/// exactly like `volume`, so it converts the same way. Feeds the cutoff's lead time
/// (`cycle::cutoff_trigger_l`), and returns 0 for anything missing, non-finite or negative, which
/// disables the lead rather than corrupting it.
pub fn flow_rate_litres_per_min(data: &serde_json::Value, unit: VolUnit) -> f64 {
    data.get("speed")
        .and_then(|v| v.as_f64())
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(|s| unit.to_litres(s))
        .unwrap_or(0.0)
}

/// Start a cycle. `duration` is SECONDS.
///
/// ⚠️ `volume_limit` IS SENT BUT MUST NOT BE TRUSTED — measured: "LinkTap hardware often ignores
/// volume limits passed to cmd: 6". The caller watches `volume` and issues its own STOP; sending
/// the cap is belt-and-braces, the software cutoff is the actual enforcement. Pass `None` for a
/// washdown: time-only by owner spec, and the key is OMITTED rather than sent as zero.
pub fn build_start(gw: &Gateway, dev_id: &str, duration_secs: u64, volume_limit: Option<f64>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "cmd": 6,
        "gw_id": gw.gw_id,
        "dev_id": dev_id,
        "duration": duration_secs.max(1),
    });
    if let Some(cap) = volume_limit {
        if cap > 0.0 {
            v["volume_limit"] = serde_json::json!((cap * 100.0).round() / 100.0);
        }
    }
    v
}

pub fn build_stop(gw: &Gateway, dev_id: &str) -> serde_json::Value {
    serde_json::json!({ "cmd": 7, "gw_id": gw.gw_id, "dev_id": dev_id })
}

/// Delete any watering plan the gateway holds for this valve. The HUB owns the schedule (owner
/// ruling 2026-08-19), so a plan left behind by the LinkTap app would fire on its own clock with
/// nothing reconciling it. CMD 4 (write plan) is deliberately NOT offered.
pub fn build_delete_plan(gw: &Gateway, dev_id: &str) -> serde_json::Value {
    serde_json::json!({ "cmd": 5, "gw_id": gw.gw_id, "dev_id": dev_id })
}

/// Register a valve WITHOUT the LinkTap app.
///
/// ⚠️ `end_dev` is an ARRAY — every other command takes a singular `dev_id`. Source: the Hubitat
/// MQTT driver's 'register device' message, valid over HTTP because §4.2 defines both transports
/// as carrying the same messages. UNPROVEN over HTTP by anyone; bench-verify on the lab gateway
/// before this becomes a product claim, and never against MVP.
pub fn build_add_valve(gw: &Gateway, dev_ids: &[String]) -> serde_json::Value {
    let ids: Vec<String> = dev_ids.iter().map(|d| normalize_dev_id(d)).collect();
    serde_json::json!({ "cmd": 1, "gw_id": gw.gw_id, "end_dev": ids })
}

pub fn build_remove_valve(gw: &Gateway, dev_ids: &[String]) -> serde_json::Value {
    let ids: Vec<String> = dev_ids.iter().map(|d| normalize_dev_id(d)).collect();
    serde_json::json!({ "cmd": 2, "gw_id": gw.gw_id, "end_dev": ids })
}

// --- transport -----------------------------------------------------------------------------------

#[derive(Debug)]
pub struct GatewayReply {
    pub ok: bool,
    pub ret: Option<i64>,
    pub data: serde_json::Value,
    pub error: Option<String>,
}

/// POST one command and parse the reply. Never returns Err — an unplugged, rebooting or
/// mid-RF-retry gateway must degrade to `ok: false`, because the caller's poll loop has nothing
/// better to do than try again.
pub async fn post_command(
    client: &reqwest::Client,
    gw: &Gateway,
    body: &serde_json::Value,
) -> GatewayReply {
    // ⚠️ NOT reqwest. THE GATEWAY IS NOT AN HTTP/1.1 SERVER — it answers HTTP/1.0 with no
    // Content-Length and delimits the body by closing the connection, which hyper rejects outright
    // ("connection closed before message completed"). While this used reqwest the hub could not
    // complete a SINGLE request against real hardware: not a poll, not an open, not a close, not
    // the flood shutoff. Measured against MVP's GW-02, 2026-08-28. See gateway_http.rs.
    //
    // `client` is kept in the signature deliberately: every caller already threads one through, and
    // removing it would be a wide mechanical change on top of a behavioural fix. It is unused here.
    let _ = client;
    let body_str = body.to_string();
    let res = match crate::gateway_http::post_json(&gw.host, "/api.shtml", &body_str, GATEWAY_TIMEOUT).await {
        Ok(r) => r,
        Err(e) => {
            return GatewayReply { ok: false, ret: None, data: serde_json::Value::Null, error: Some(e) }
        }
    };
    if !(200..300).contains(&res.status) {
        let s = res.status;
        return GatewayReply {
            ok: false, ret: None, data: serde_json::Value::Null,
            error: Some(format!("gateway returned HTTP {s}")),
        };
    }
    let text = res.body;
    let parsed: serde_json::Value = match serde_json::from_str(extract_json(&text)) {
        Ok(v) => v,
        Err(_) => {
            return GatewayReply {
                ok: false, ret: None, data: serde_json::Value::Null,
                error: Some("gateway reply was not JSON".into()),
            }
        }
    };
    // `ret` is absent on status replies (cmd 3), which carry the payload instead — absence is not
    // failure. Only an explicitly non-zero ret is.
    let ret = parsed.get("ret").and_then(|v| v.as_i64());
    match ret {
        Some(r) if r != 0 => GatewayReply {
            ok: false, ret, data: parsed, error: Some(describe_ret(r).to_string()),
        },
        _ => GatewayReply { ok: true, ret, data: parsed, error: None },
    }
}

/// Did the GATEWAY ITSELF answer, whatever it thought of the command?
///
/// Deliberately NOT `reply.ok`. A non-zero `ret` is the gateway TALKING — `ret: 5` means the end
/// device was not found (valve unpowered, out of RF range, dead battery) and `ret: 7` means a
/// watering plan is in the way; both are replies from a healthy gateway. Reading either as
/// "unreachable" would raise a gateway-offline notice about a flat valve battery, which is the
/// wrong device, the wrong owner action, and the wrong repair.
///
/// Unreachable is the transport failing: no connection, a non-2xx, or a body that is not JSON —
/// the three cases `post_command` turns into `ret: None`.
pub fn reply_reached_gateway(reply: &GatewayReply) -> bool {
    reply.ok || reply.ret.is_some()
}

/// Read the gateway's volume unit. Defaults to GALLONS when unreadable — guessing litres
/// under-reports a cap by 3.79x, and the software cutoff compares against that number.
pub async fn read_vol_unit(client: &reqwest::Client, gw: &Gateway) -> VolUnit {
    let reply = post_command(client, gw, &build_get_configuration(gw)).await;
    match reply.data.get("vol_unit").and_then(|v| v.as_str()) {
        Some("L") => VolUnit::Litre,
        _ => VolUnit::Gal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn gw() -> Gateway {
        Gateway { host: "192.168.1.107".into(), gw_id: "CCCCDDDDEEEEFFFF".into() }
    }
    const DEV: &str = "aaaabbbbccccdddd";

    #[test]
    fn unwraps_the_html_wrapper_the_gateway_uses_by_default() {
        let html = "<html><head><title>api</title></head><body>\n  <!--#RET-->{\"cmd\":6,\"gw_id\":\"G\",\"ret\":0}\n</body></html>";
        let v: serde_json::Value = serde_json::from_str(extract_json(html)).unwrap();
        assert_eq!(v["ret"], 0);
    }

    #[test]
    fn passes_bare_json_through_untouched() {
        assert_eq!(extract_json("{\"cmd\":3}"), "{\"cmd\":3}");
    }

    #[test]
    fn coerces_every_watering_spelling_the_firmware_uses() {
        for v in [json!(true), json!("true"), json!(1), json!("1")] {
            assert!(coerce_watering(&v), "{v:?} should read as watering");
        }
        for v in [json!(false), json!("false"), json!(0), json!("0"), json!(null)] {
            assert!(!coerce_watering(&v), "{v:?} should not read as watering");
        }
    }

    #[test]
    fn a_gateway_that_refuses_a_command_is_still_a_gateway_that_answered() {
        // The gateway-offline watch keys off this: `ret: 5` (valve unpowered / out of RF range) is
        // a HEALTHY gateway reporting a sick valve. Calling it an outage would alert the owner
        // about the wrong device entirely.
        let refused = GatewayReply { ok: false, ret: Some(5), data: json!({"ret":5}), error: Some("x".into()) };
        assert!(reply_reached_gateway(&refused));
        let good = GatewayReply { ok: true, ret: Some(0), data: json!({"ret":0}), error: None };
        assert!(reply_reached_gateway(&good));
        // A cmd-3 status reply carries no `ret` at all — absence is not failure.
        let status = GatewayReply { ok: true, ret: None, data: json!({"dev_stat":[]}), error: None };
        assert!(reply_reached_gateway(&status));
        // The three transport failures post_command produces: no connection, non-2xx, not JSON.
        let dead = GatewayReply { ok: false, ret: None, data: serde_json::Value::Null, error: Some("timed out".into()) };
        assert!(!reply_reached_gateway(&dead));
    }

    #[test]
    fn the_valves_own_state_is_read_from_a_status_reply_and_silence_is_not_shut() {
        // Both shapes the gateway uses, and the `1`/`"true"` spellings coerce_watering accepts.
        assert_eq!(watering_from_status(&json!({ "dev_stat": [{ "is_watering": true }] })), Some(true));
        assert_eq!(watering_from_status(&json!({ "dev_stat": [{ "is_watering": "0" }] })), Some(false));
        assert_eq!(watering_from_status(&json!({ "is_watering": 1 })), Some(true));
        assert_eq!(watering_from_status(&json!({ "is_watering": false })), Some(false));
        // 🔴 SILENCE IS NOT "SHUT". A reply that does not carry the field — a `ret: 5` refusal, a
        // transport failure's Null, an empty dev_stat — must be None, because close_watch.rs reads
        // `Some(false)` as "the close is CONFIRMED" and would otherwise confirm on a timeout.
        assert_eq!(watering_from_status(&json!({ "ret": 5 })), None);
        assert_eq!(watering_from_status(&json!({ "dev_stat": [] })), None);
        assert_eq!(watering_from_status(&serde_json::Value::Null), None);
        assert_eq!(watering_from_status(&json!({ "dev_stat": [{ "volume": 1.0 }] })), None);
    }

    #[test]
    fn reports_volume_trusts_the_flag_over_the_field() {
        assert!(reports_volume(&json!({ "is_flm_plugin": true })));
        assert!(!reports_volume(&json!({ "is_flm_plugin": false, "volume": 1.2 })));
        assert!(reports_volume(&json!({ "volume": 1.2 })));
        assert!(!reports_volume(&json!({})));
    }

    #[test]
    fn cycle_volume_reads_the_cycle_total_in_the_gateway_unit() {
        // 0.63 gal was the real opening reading captured on MVP's GW-02 mid-cycle.
        let v = cycle_volume_litres(&json!({ "volume": 0.63 }), VolUnit::Gal);
        assert!((v - 0.63 * LITRES_PER_GALLON).abs() < 1e-9);
        assert_eq!(cycle_volume_litres(&json!({ "volume": 2.0 }), VolUnit::Litre), 2.0);
    }

    #[test]
    fn cycle_volume_rejects_the_idle_garbage_latch() {
        // Measured: a closed GW-02 sat at 15,886,307.00. Letting that through would instantly
        // "exceed" any cap.
        assert_eq!(cycle_volume_litres(&json!({ "volume": 15_886_307.0 }), VolUnit::Gal), 0.0);
        assert_eq!(cycle_volume_litres(&json!({ "volume": -1.0 }), VolUnit::Gal), 0.0);
        assert_eq!(cycle_volume_litres(&json!({}), VolUnit::Gal), 0.0);
    }

    #[test]
    fn start_is_cmd_6_with_seconds_and_an_omitted_cap_for_washdown() {
        let b = build_start(&gw(), DEV, 900, Some(50.0));
        assert_eq!(b["cmd"], 6);
        assert_eq!(b["duration"], 900);
        assert_eq!(b["volume_limit"], 50.0);
        // Washdown: time-only by owner spec — the key is OMITTED, never sent as zero.
        let w = build_start(&gw(), DEV, 7200, None);
        assert!(w.get("volume_limit").is_none());
        let z = build_start(&gw(), DEV, 7200, Some(0.0));
        assert!(z.get("volume_limit").is_none());
    }

    #[test]
    fn never_sends_a_zero_second_run() {
        assert_eq!(build_start(&gw(), DEV, 0, None)["duration"], 1);
    }

    #[test]
    fn pairing_uses_end_dev_as_an_array_not_dev_id() {
        // The single field nobody would guess — every other command takes a singular dev_id.
        let a = build_add_valve(&gw(), &[DEV.to_string()]);
        assert_eq!(a["cmd"], 1);
        assert_eq!(a["end_dev"], json!([DEV]));
        assert!(a.get("dev_id").is_none());
        let r = build_remove_valve(&gw(), &[DEV.to_string()]);
        assert_eq!(r["cmd"], 2);
        assert!(r.get("dev_id").is_none());
    }

    #[test]
    fn normalises_a_pasted_label_to_the_canonical_sixteen() {
        assert_eq!(normalize_dev_id("  aaaabbbbccccddddEXTRA "), DEV);
        assert_eq!(
            build_add_valve(&gw(), &["aaaabbbbccccddddEXTRA".into()])["end_dev"],
            json!([DEV])
        );
    }

    #[test]
    fn plan_delete_is_offered_but_plan_write_is_not() {
        assert_eq!(build_delete_plan(&gw(), DEV)["cmd"], 5);
    }

    #[test]
    fn ret_codes_decode_including_the_two_that_bite() {
        assert_eq!(describe_ret(0), "Success");
        assert_eq!(describe_ret(5), "End device ID not found"); // the RF join failed
        assert_eq!(describe_ret(7), "Conflict with watering plan");
        assert_eq!(describe_ret(42), "unknown gateway error");
    }

    #[test]
    fn unit_conversion_round_trips() {
        let litres = 100.0;
        let in_gal = VolUnit::Gal.from_litres(litres);
        assert!((VolUnit::Gal.to_litres(in_gal) - litres).abs() < 1e-9);
        // 27 gal must exceed a 100 L cap even though 27 < 100 — the conversion IS the safety.
        assert!(VolUnit::Gal.to_litres(27.0) > 100.0);
        assert!(VolUnit::Gal.to_litres(26.0) < 100.0);
    }
}
