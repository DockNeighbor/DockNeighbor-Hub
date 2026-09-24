// Starlink — vendor 3 of the managed-router flow (routers.rs; owner 2026-09-13: "build the peplink
// and starlink drivers", after Peplink). The hub reads the dish over its LOCAL gRPC API and reports
// for it the way it reports for a Cradlepoint or a Peplink: the Snapshot is the shared shape in
// routers.rs, and the `modem.measurement` the cloud sees carries the dish's own fields
// (routers::dish_params) under the same event, so a Starlink lands on the Connectivity page like any
// other uplink. Monitoring is the whole first cut (owner: "monitoring first, management second";
// a Starlink outage is a status change, NOT a critical alert); the one management verb here is
// reboot, the same armed button the routers have. Stow is deliberately absent — it is a physical
// action on a moving vehicle and inherits the valve's safety posture (docs/DESIGN-DEVICES.md), not
// a settings toggle to ship beside a status card.
//
// GPS, and why the owner's dish refuses it. `get_location` answers PERMISSION_DENIED "Disabled due
// to policy" on MVP's Starlink Mini (2026-09-13). That is not a switch the owner missed: Starlink
// removed position from the local API on 2026-05-20 for standard plans, restored it in July 2026
// for Priority plans only, and the Mini/V4 never show the old "Starlink Location" toggle
// (pds.codes/posts/starlink-removing-gps-from-local-api). The driver still asks — a Priority plan
// or a future firmware may answer — and reports the refusal as GPS off at the dish with the
// reason, so the owner picks another position source rather than hunting for a setting.
//
// The interface. A dish answers gRPC on 192.168.100.1:9200 with no sign-in: one service,
// `SpaceX.API.Device.Device/Handle`, one `Request` oneof in, one `Response` oneof out. It is
// ⚠️ UNDOCUMENTED and SpaceX changes it without notice — the same risk class as the vendor clouds
// this platform has been bitten by. The field numbers below are PINNED TO THE OWNER'S DISH: pulled
// by reflection on 2026-09-13 from a Starlink Mini (`mini1_panda_proto1`, fw 2026.08.31.mr85832,
// api_version 43, behind the Peplink in bypass). They differ from the community-extracted protos
// on GitHub (`state` is reserved now, `outage` moved 1011→1014, `gps_stats` 1012→1015, the alerts
// were renumbered) — which is why every field is Optional and a wrong number reads as "absent",
// never as a wrong value, and why `field_numbers_are_the_dish_protos` below pins them on the wire.
//
// Why prost derive + h2 rather than tonic: the daemon has no HTTP/2 client at all (reqwest is
// built without `http2`), and a gRPC unary call is one h2 stream — a 5-byte frame in, a 5-byte
// frame out, `grpc-status` in the trailers. Hand-declaring the dozen messages we read keeps the
// build free of protoc and the binary free of the hundred messages we never touch.

use bytes::Bytes;
use prost::Message;
use serde::Serialize;
use std::time::Duration;
use tokio::net::TcpStream;

use crate::gps::GpsFix;
use crate::routers::{Probe, WanStatus};

/// The dish's fixed management address — every Starlink answers here regardless of the router in
/// front of it, provided that router routes to it (bypass-mode installs need a static route).
pub const DEFAULT_HOST: &str = "192.168.100.1";
pub const DEFAULT_PORT: u16 = 9200;
const GRPC_PATH: &str = "/SpaceX.API.Device.Device/Handle";
/// The per-call deadline for an INTERACTIVE call (an app action — probe, refresh, reboot — waiting on
/// the answer, inside the relay's own 30 s call deadline, hub_relay CALL_TIMEOUT). Unchanged at 10 s.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// The per-call deadline for the BACKGROUND poll (router_poll_loop), where nobody is waiting and a slow
/// answer is still an answer: 30 s, raised from 10 s (owner ruling 2026-09-17 — the dish's gRPC can be
/// slow, and a slow answer must never read as a dish that is down; router_health decides down).
pub const POLL_CALL_TIMEOUT: Duration = Duration::from_secs(30);

// --- The wire messages (spacex.api.device, trimmed to what the hub reads) -------------------------

pub mod pb {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Empty {}

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Request {
        #[prost(oneof = "request::Kind", tags = "1001, 1004, 1008, 1017")]
        pub kind: Option<request::Kind>,
    }
    pub mod request {
        #[derive(Clone, PartialEq, prost::Oneof)]
        pub enum Kind {
            #[prost(message, tag = "1001")]
            Reboot(super::Empty),
            #[prost(message, tag = "1004")]
            GetStatus(super::Empty),
            #[prost(message, tag = "1008")]
            GetDeviceInfo(super::Empty),
            #[prost(message, tag = "1017")]
            GetLocation(super::Empty),
        }
    }

    /// `SpaceX.API.Status.Status` — a gRPC-style code and message the dish sets on a refusal that
    /// still comes back as a Response (most refusals arrive as gRPC trailers instead).
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Status {
        #[prost(int32, optional, tag = "1")]
        pub code: Option<i32>,
        #[prost(string, optional, tag = "2")]
        pub message: Option<String>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Response {
        #[prost(message, optional, tag = "2")]
        pub status: Option<Status>,
        #[prost(uint64, optional, tag = "3")]
        pub api_version: Option<u64>,
        #[prost(oneof = "response::Kind", tags = "1001, 1004, 1017, 2004")]
        pub kind: Option<response::Kind>,
    }
    pub mod response {
        #[derive(Clone, PartialEq, prost::Oneof)]
        pub enum Kind {
            #[prost(message, tag = "1001")]
            Reboot(super::Empty),
            #[prost(message, tag = "1004")]
            GetDeviceInfo(super::GetDeviceInfoResponse),
            #[prost(message, tag = "1017")]
            GetLocation(super::GetLocationResponse),
            #[prost(message, tag = "2004")]
            DishGetStatus(super::DishGetStatusResponse),
        }
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct DeviceInfo {
        /// The dish's own id (`ut4088918f-…`) — the stable identity a MAC would otherwise give.
        #[prost(string, optional, tag = "1")]
        pub id: Option<String>,
        #[prost(string, optional, tag = "2")]
        pub hardware_version: Option<String>,
        #[prost(string, optional, tag = "3")]
        pub software_version: Option<String>,
        #[prost(string, optional, tag = "4")]
        pub country_code: Option<String>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct DeviceState {
        #[prost(uint64, optional, tag = "1")]
        pub uptime_s: Option<u64>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct GetDeviceInfoResponse {
        #[prost(message, optional, tag = "1")]
        pub device_info: Option<DeviceInfo>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct LlaPosition {
        #[prost(double, optional, tag = "1")]
        pub lat: Option<f64>,
        #[prost(double, optional, tag = "2")]
        pub lon: Option<f64>,
        #[prost(double, optional, tag = "3")]
        pub alt: Option<f64>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct GetLocationResponse {
        #[prost(message, optional, tag = "1")]
        pub lla: Option<LlaPosition>,
        /// Position uncertainty in metres — the fix's accuracy radius.
        #[prost(double, optional, tag = "4")]
        pub sigma_m: Option<f64>,
    }

    /// The dish's own alarms. Numbers are the dish's (2026-09-13 reflection); 7, 12, 13 and 15 are
    /// reserved there and deliberately absent here.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct DishAlerts {
        #[prost(bool, optional, tag = "1")]
        pub motors_stuck: Option<bool>,
        #[prost(bool, optional, tag = "2")]
        pub thermal_shutdown: Option<bool>,
        #[prost(bool, optional, tag = "3")]
        pub thermal_throttle: Option<bool>,
        #[prost(bool, optional, tag = "4")]
        pub unexpected_location: Option<bool>,
        #[prost(bool, optional, tag = "5")]
        pub mast_not_near_vertical: Option<bool>,
        #[prost(bool, optional, tag = "6")]
        pub slow_ethernet_speeds: Option<bool>,
        #[prost(bool, optional, tag = "8")]
        pub install_pending: Option<bool>,
        #[prost(bool, optional, tag = "9")]
        pub is_heating: Option<bool>,
        #[prost(bool, optional, tag = "10")]
        pub power_supply_thermal_throttle: Option<bool>,
        #[prost(bool, optional, tag = "11")]
        pub is_power_save_idle: Option<bool>,
        #[prost(bool, optional, tag = "16")]
        pub low_motor_current: Option<bool>,
        #[prost(bool, optional, tag = "17")]
        pub lower_signal_than_predicted: Option<bool>,
        #[prost(bool, optional, tag = "18")]
        pub slow_ethernet_speeds_100: Option<bool>,
        #[prost(bool, optional, tag = "19")]
        pub obstruction_map_reset: Option<bool>,
        #[prost(bool, optional, tag = "20")]
        pub dish_water_detected: Option<bool>,
        #[prost(bool, optional, tag = "21")]
        pub router_water_detected: Option<bool>,
        #[prost(bool, optional, tag = "23")]
        pub no_ethernet_link: Option<bool>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct DishObstructionStats {
        /// Fraction of the sky the dish has found obstructed since it last reset the map.
        #[prost(float, optional, tag = "1")]
        pub fraction_obstructed: Option<f32>,
        #[prost(float, optional, tag = "4")]
        pub valid_s: Option<f32>,
        #[prost(bool, optional, tag = "5")]
        pub currently_obstructed: Option<bool>,
        /// Fraction of the time the dish spent obstructed over the valid window.
        #[prost(float, optional, tag = "9")]
        pub time_obstructed: Option<f32>,
    }

    /// Present while the dish is in an outage; `cause` is DishOutage.Cause (outage_label).
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct DishOutage {
        #[prost(int32, optional, tag = "1")]
        pub cause: Option<i32>,
        #[prost(int64, optional, tag = "2")]
        pub start_timestamp_ns: Option<i64>,
        #[prost(uint64, optional, tag = "3")]
        pub duration_ns: Option<u64>,
        #[prost(bool, optional, tag = "4")]
        pub did_switch: Option<bool>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct DishGpsStats {
        #[prost(bool, optional, tag = "1")]
        pub gps_valid: Option<bool>,
        #[prost(uint32, optional, tag = "2")]
        pub gps_sats: Option<u32>,
    }

    #[derive(Clone, PartialEq, prost::Message)]
    pub struct DishGetStatusResponse {
        #[prost(message, optional, tag = "1")]
        pub device_info: Option<DeviceInfo>,
        #[prost(message, optional, tag = "2")]
        pub device_state: Option<DeviceState>,
        #[prost(float, optional, tag = "1003")]
        pub pop_ping_drop_rate: Option<f32>,
        #[prost(message, optional, tag = "1004")]
        pub obstruction_stats: Option<DishObstructionStats>,
        #[prost(message, optional, tag = "1005")]
        pub alerts: Option<DishAlerts>,
        #[prost(float, optional, tag = "1007")]
        pub downlink_throughput_bps: Option<f32>,
        #[prost(float, optional, tag = "1008")]
        pub uplink_throughput_bps: Option<f32>,
        #[prost(float, optional, tag = "1009")]
        pub pop_ping_latency_ms: Option<f32>,
        #[prost(bool, optional, tag = "1010")]
        pub stow_requested: Option<bool>,
        #[prost(message, optional, tag = "1014")]
        pub outage: Option<DishOutage>,
        #[prost(message, optional, tag = "1015")]
        pub gps_stats: Option<DishGpsStats>,
        #[prost(int32, optional, tag = "1016")]
        pub eth_speed_mbps: Option<i32>,
        #[prost(bool, optional, tag = "1018")]
        pub is_snr_above_noise_floor: Option<bool>,
        /// 0..1 — the dish's own one-number signal quality (1057 on api 43).
        #[prost(float, optional, tag = "1057")]
        pub signal_quality: Option<f32>,
    }
}

// --- Normalized shape (what the Snapshot and the app see) ----------------------------------------

/// What one `get_status` said about the dish, in the words the app shows. There is no "state" —
/// the dish's api 43 reserved it — so "up" is the absence of an `outage`.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DishStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<u64>,
    /// Percent of the sky the dish finds obstructed (one decimal).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub obstruction_pct: Option<f64>,
    /// Obstructed RIGHT NOW (the map above is the long-run fraction).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub obstructed: Option<bool>,
    /// The cause of the outage the dish is in, when it is in one (`outage_label`); absent when up.
    /// Reported whether or not the outage has lasted long enough to be a WAN outage (OUTAGE_MIN_MS),
    /// so the history reads "stowed" for three hours and "obstructed" for four minutes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outage: Option<String>,
    /// How long the CURRENT outage has lasted, in ms. As parsed it is the dish's own measurement
    /// (DishOutage.duration_ns); `carry_outage` then reconciles it with the run the hub has watched
    /// itself, and `wan_of` compares the result against OUTAGE_MIN_MS. Hub-internal — it is also
    /// what lets router_health count a dish's down sample without adding its own 45 s window.
    #[serde(skip)]
    pub outage_ms: Option<i64>,
    /// When the hub first saw the outage now in progress, epoch ms; None while the dish reports no
    /// outage. Hub-internal, carried from the previous Snapshot by `carry_outage` so a dish that
    /// reports an outage with no duration of its own is still timed. Lost across a hub restart —
    /// the dish's own measured duration is what covers that case.
    #[serde(skip)]
    pub outage_since_ms: Option<i64>,
    /// The raw DishOutage.Cause behind `outage` (`outage` carries its label on the wire). Hub-internal:
    /// its PRESENCE is what says the dish is in an outage at all. Some(0) is an outage whose cause the
    /// dish did not give — still an outage, per the owner's ruling (see OUTAGE_MIN_MS).
    #[serde(skip)]
    pub outage_cause: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
    /// Ping loss to the point of presence, percent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loss_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub down_mbps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up_mbps: Option<f64>,
    /// The dish's own signal quality, 0–100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal_pct: Option<f64>,
    /// The dish's own alarms, as short labels; empty when none.
    pub alerts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stow_requested: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gps_valid: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gps_sats: Option<u32>,
}

/// PURE: DishOutage.Cause → the word the owner sees (api 43: 12 is reserved, 13 and 14 are new).
pub fn outage_label(cause: i32) -> &'static str {
    match cause {
        1 => "booting",
        2 => "stowed",
        3 => "thermal shutdown",
        4 => "no schedule",
        5 => "no satellites",
        6 => "obstructed",
        7 => "no downlink",
        8 => "no pings",
        9 => "actuator activity",
        10 => "cable test",
        11 => "sleeping",
        13 => "sky search",
        14 => "RF inhibited",
        _ => "unknown",
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// PURE: the alarms that are set, as labels, in the proto's order.
pub fn alert_labels(a: &pb::DishAlerts) -> Vec<String> {
    let flags: [(Option<bool>, &str); 17] = [
        (a.motors_stuck, "motors stuck"),
        (a.thermal_shutdown, "thermal shutdown"),
        (a.thermal_throttle, "thermal throttle"),
        (a.unexpected_location, "unexpected location"),
        (a.mast_not_near_vertical, "mast not near vertical"),
        (a.slow_ethernet_speeds, "slow ethernet"),
        (a.install_pending, "install pending"),
        (a.is_heating, "heating"),
        (a.power_supply_thermal_throttle, "power supply thermal throttle"),
        (a.is_power_save_idle, "power-save idle"),
        (a.low_motor_current, "low motor current"),
        (a.lower_signal_than_predicted, "lower signal than predicted"),
        (a.slow_ethernet_speeds_100, "ethernet at 100 Mbps"),
        (a.obstruction_map_reset, "obstruction map reset"),
        (a.dish_water_detected, "water detected in the dish"),
        (a.router_water_detected, "water detected in the router"),
        (a.no_ethernet_link, "no ethernet link"),
    ];
    flags.iter().filter(|(on, _)| *on == Some(true)).map(|(_, l)| l.to_string()).collect()
}

/// PURE: `get_status` → DishStatus. Fractions become percents, bps becomes Mbps, enums become words.
pub fn parse_status(r: &pb::DishGetStatusResponse) -> DishStatus {
    let obs = r.obstruction_stats.as_ref();
    let f = |v: Option<f32>| v.map(f64::from).filter(|n| n.is_finite());
    DishStatus {
        uptime_s: r.device_state.as_ref().and_then(|s| s.uptime_s),
        obstruction_pct: f(obs.and_then(|o| o.fraction_obstructed)).map(|n| round1(n * 100.0)),
        obstructed: obs.and_then(|o| o.currently_obstructed),
        outage: r.outage.as_ref().map(|o| outage_label(o.cause.unwrap_or(0)).to_string()),
        outage_ms: r.outage.as_ref().and_then(|o| o.duration_ns).map(|ns| i64::try_from(ns / 1_000_000).unwrap_or(i64::MAX)),
        // Filled by `carry_outage` from the previous poll: one `get_status` cannot know it.
        outage_since_ms: None,
        outage_cause: r.outage.as_ref().map(|o| o.cause.unwrap_or(0)),
        latency_ms: f(r.pop_ping_latency_ms).map(round1),
        loss_pct: f(r.pop_ping_drop_rate).map(|n| round1(n * 100.0)),
        down_mbps: f(r.downlink_throughput_bps).map(|n| round1(n / 1_000_000.0)),
        up_mbps: f(r.uplink_throughput_bps).map(|n| round1(n / 1_000_000.0)),
        signal_pct: f(r.signal_quality).map(|n| round1(n.clamp(0.0, 1.0) * 100.0)),
        alerts: r.alerts.as_ref().map(alert_labels).unwrap_or_default(),
        stow_requested: r.stow_requested,
        gps_valid: r.gps_stats.as_ref().and_then(|g| g.gps_valid),
        gps_sats: r.gps_stats.as_ref().and_then(|g| g.gps_sats),
    }
}

/// PURE: the dish as a Probe. `model` carries the hardware revision (`mini1_panda_proto1` and the
/// like — the only model word the dish gives), `serial` its id, so the app's device id is stable
/// across re-adds without a MAC.
pub fn parse_probe(info: &pb::DeviceInfo) -> Probe {
    let hw = info.hardware_version.as_deref().map(str::trim).filter(|s| !s.is_empty());
    Probe {
        model: Some(match hw {
            Some(h) => format!("Starlink {h}"),
            None => "Starlink".into(),
        }),
        firmware: info.software_version.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from),
        mac: None,
        serial: info.id.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(String::from),
    }
}

/// WHEN A DISH OUTAGE IS A WAN OUTAGE — the one rule, and the only place `up=0` is decided for a
/// dish. An outage is TWO MINUTES LONG, not a particular cause.
///
/// THE OWNER'S RULING (Jonathan, 2026-09-24). Asked what an outage is, he said an outage is
/// "no internet for over 2 minutes". Asked which of the causes the dish reports should count, he
/// said "all of them except connectd". So EVERY cause the dish gives is a WAN outage once it has
/// been continuous for MORE THAN two minutes — booting, stowed, thermal shutdown, no schedule, no
/// satellites, obstructed, no downlink, no pings, actuator activity, cable test, sleeping, sky
/// search, RF inhibited, a code this hub has never seen, and an outage the dish will not explain at
/// all (`Some(0)`). The ONE non-outage is the dish reporting no outage at all: connected.
///
/// 🔴 THIS REPLACES THE INTERIM TABLE SHIPPED IN 0.3.54, which split the causes into "no path to
/// the internet" (5, 6, 7, 8 → `up=0`) and "the dish's own condition" (everything else → `up=1`,
/// however long it lasted) while the ruling was pending. There is no such split any more: the clock
/// decides, not the cause. A dish that has been stowed for three hours has had no internet for
/// three hours, and the owner wants that logged as the outage it is.
///
/// The cause is still reported either way, above the threshold and below it (`outage`, DishStatus),
/// so the history reads "stowed" over three hours and "obstructed" over four minutes rather than a
/// bare down — nothing is hidden, and nothing is invented.
///
/// "Over 2 minutes" is strict: an outage measured at exactly 120 s is not yet one.
pub const OUTAGE_MIN_MS: i64 = 120_000;

/// PURE: how long the dish's CURRENT outage has lasted, folded across polls.
///
/// `d` is this poll's reading, `prev` the last reading of the SAME dish from a poll that succeeded,
/// and `now_ms` when this poll was taken. On return `d.outage_ms` is the outage's age and
/// `d.outage_since_ms` is when this run began, for the next poll to carry.
///
/// TWO CLOCKS, AND WHY BOTH. The dish's own measurement (DishOutage.duration_ns, already in
/// `outage_ms` as parsed) is what lets a hub that RESTARTED mid-outage count it at once instead of
/// starting a fresh two minutes — it has watched the outage the hub did not. The hub's own run is
/// what covers a dish that reports an outage with no duration at all. The age is the greater of the
/// two: neither clock may shorten an outage the other has already seen.
///
/// A run continues while the dish reports ANY outage, so a cause that CHANGES mid-run (sky search
/// → no satellites) does not restart the two minutes — the internet was out across both — and the
/// label reported is always the current one. A poll the hub could not make ends the run instead of
/// extending it (`prev` is passed only for a poll that succeeded): the hub cannot claim continuity
/// it did not observe, and where the dish measured the outage itself that measurement still counts.
pub fn carry_outage(d: &mut DishStatus, prev: Option<&DishStatus>, now_ms: i64) {
    if d.outage_cause.is_none() {
        d.outage_since_ms = None;
        return;
    }
    let since = prev.filter(|p| p.outage_cause.is_some()).and_then(|p| p.outage_since_ms).unwrap_or(now_ms);
    d.outage_since_ms = Some(since);
    let hub_ms = now_ms.saturating_sub(since).max(0);
    d.outage_ms = Some(d.outage_ms.filter(|m| *m >= 0).map_or(hub_ms, |m| m.max(hub_ms)));
}

/// PURE: the uplink the dish IS. Down when the dish is in an outage — ANY outage, whatever its
/// cause — AND that outage has lasted more than OUTAGE_MIN_MS. An outage the dish has not held for
/// two minutes is not one yet, and no outage at all is up.
///
/// `d.outage_ms` is the age `carry_outage` reconciled. An outage with no age yet is on its first
/// sighting by a hub the dish told nothing about it, which is below the threshold by definition.
pub fn wan_of(d: &DishStatus) -> WanStatus {
    let up = !(d.outage_cause.is_some() && d.outage_ms.is_some_and(|ms| ms > OUTAGE_MIN_MS));
    WanStatus { wan: "starlink".into(), up, up_known: true, ip: None, uptime_s: None }
}

/// PURE: `get_location` → a fix, or None when the dish has no valid position yet. The dish reports
/// (0,0) before a lock, which is rejected the same way the router parsers reject it; `sigma_m` is
/// the accuracy radius when the dish gives one.
pub fn parse_location(r: &pb::GetLocationResponse) -> Option<GpsFix> {
    let lla = r.lla.as_ref()?;
    let (lat, lon) = (lla.lat?, lla.lon?);
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 || (lat == 0.0 && lon == 0.0) {
        return None;
    }
    Some(GpsFix { lat, lon, acc: r.sigma_m.filter(|a| a.is_finite() && *a >= 0.0), ..Default::default() })
}

// --- gRPC framing ---------------------------------------------------------------------------------

/// gRPC length-prefixed message: 1 byte compressed flag (0) + u32 big-endian length + the message.
pub fn frame(msg: &impl Message) -> Bytes {
    let len = msg.encoded_len();
    let mut buf = Vec::with_capacity(5 + len);
    buf.push(0);
    buf.extend_from_slice(&(len as u32).to_be_bytes());
    msg.encode(&mut buf).expect("Vec never fails to grow");
    Bytes::from(buf)
}

/// The message inside one frame. A dish never compresses; a flag of 1 is refused rather than
/// guessed at.
pub fn unframe(body: &[u8]) -> Result<&[u8], String> {
    if body.len() < 5 {
        return Err("the dish sent an empty answer".into());
    }
    if body[0] != 0 {
        return Err("the dish sent a compressed answer, which this hub does not read".into());
    }
    let len = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
    body.get(5..5 + len).ok_or_else(|| "the dish's answer was cut short".into())
}

/// `grpc-status` from a header block, when it carries one.
fn grpc_status(h: &http::HeaderMap) -> Option<i32> {
    h.get("grpc-status")?.to_str().ok()?.trim().parse().ok()
}

/// `grpc-message` is percent-encoded UTF-8.
fn grpc_message(h: &http::HeaderMap) -> String {
    let raw = h.get("grpc-message").and_then(|v| v.to_str().ok()).unwrap_or("");
    let mut out = Vec::with_capacity(raw.len());
    let b = raw.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).trim().to_string()
}

/// What `get_location`'s PERMISSION_DENIED becomes (grpc_error) — also how gps_support_of_refusal
/// recognises it, so the two cannot drift apart.
pub const LOCATION_REFUSED: &str = "the dish will not share its position — Starlink switched off local GPS access in May 2026 (Priority plans got it back in July; the Mini and V4 have no setting for it). Use another position source for this vessel";

/// PURE: a failed `location()` → GpsSupport. The dish's PERMISSION_DENIED ("Disabled due to policy",
/// MVP's Mini, bench 2026-09-13) is the dish saying it will not give the hub a position: `no`.
/// Anything else (unreachable, rebooting, an unknown request) says nothing about GPS: `unknown`.
pub fn gps_support_of_refusal(why: &str) -> crate::routers::GpsSupport {
    if why == LOCATION_REFUSED {
        crate::routers::GpsSupport::No
    } else {
        crate::routers::GpsSupport::Unknown
    }
}

/// PURE: a non-OK gRPC status → what the owner can act on. 7 (PERMISSION_DENIED) is the one that
/// matters: `get_location` answers "Disabled due to policy" (bench 2026-09-13), which is Starlink's
/// plan policy, not a setting — saying so is the difference between "pick another position source"
/// and a hunt for a switch that is not there (see the header).
pub fn grpc_error(code: i32, message: &str) -> String {
    match code {
        7 => LOCATION_REFUSED.into(),
        12 => "the dish does not know that request — its firmware has moved; this hub needs updating".into(),
        14 => "the dish is not available right now — it may be rebooting".into(),
        _ if message.is_empty() => format!("the dish answered gRPC status {code}"),
        _ => format!("the dish answered: {message} (gRPC {code})"),
    }
}

// --- Transport ------------------------------------------------------------------------------------

/// One dish. No session and no credential: a connection per call, which is what the dish's own
/// app does and costs nothing on a LAN.
pub struct Starlink {
    host: String,
    port: u16,
    timeout: Duration,
}

impl Starlink {
    pub fn new(host: &str, port: u16) -> Self {
        let h = host.trim();
        Starlink {
            host: if h.is_empty() { DEFAULT_HOST.into() } else { h.into() },
            port: if port == 0 { DEFAULT_PORT } else { port },
            timeout: CALL_TIMEOUT,
        }
    }

    /// The same dish with another per-call deadline (the background poll's POLL_CALL_TIMEOUT).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    async fn call(&self, req: pb::Request) -> Result<pb::Response, String> {
        tokio::time::timeout(self.timeout, self.call_inner(req))
            .await
            .map_err(|_| "the dish did not answer (timed out) — is 192.168.100.1 routed from the hub's network?".to_string())?
    }

    async fn call_inner(&self, req: pb::Request) -> Result<pb::Response, String> {
        let addr = format!("{}:{}", self.host, self.port);
        let tcp = TcpStream::connect(&addr).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::ConnectionRefused {
                "the dish refused the connection at that address".to_string()
            } else {
                "the dish could not be reached at that address".to_string()
            }
        })?;
        let (mut client, conn) = h2::client::handshake(tcp).await.map_err(|e| format!("the dish did not speak HTTP/2: {e}"))?;
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri(format!("http://{addr}{GRPC_PATH}"))
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .body(())
            .map_err(|e| e.to_string())?;
        let (response, mut send) = client.send_request(request, false).map_err(|e| format!("the dish dropped the request: {e}"))?;
        send.send_data(frame(&req), true).map_err(|e| format!("the dish dropped the request: {e}"))?;
        let response = response.await.map_err(|e| format!("the dish dropped the request: {e}"))?;
        let (parts, mut body) = response.into_parts();
        if parts.status != http::StatusCode::OK {
            return Err(format!("the dish answered HTTP {}", parts.status.as_u16()));
        }
        // A "trailers-only" refusal puts grpc-status in the headers and sends no body.
        if let Some(code) = grpc_status(&parts.headers).filter(|c| *c != 0) {
            return Err(grpc_error(code, &grpc_message(&parts.headers)));
        }
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.map_err(|e| format!("the dish's answer was cut short: {e}"))?;
            let _ = body.flow_control().release_capacity(chunk.len());
            buf.extend_from_slice(&chunk);
        }
        if let Some(trailers) = body.trailers().await.map_err(|e| format!("the dish's answer was cut short: {e}"))? {
            if let Some(code) = grpc_status(&trailers).filter(|c| *c != 0) {
                return Err(grpc_error(code, &grpc_message(&trailers)));
            }
        }
        let msg = unframe(&buf)?;
        let res = pb::Response::decode(msg).map_err(|e| format!("the dish's answer could not be decoded: {e}"))?;
        if let Some(code) = res.status.as_ref().and_then(|s| s.code).filter(|c| *c != 0) {
            return Err(grpc_error(code, res.status.as_ref().and_then(|s| s.message.as_deref()).unwrap_or("")));
        }
        Ok(res)
    }

    /// `get_status` → the dish's own report.
    pub async fn status(&self) -> Result<pb::DishGetStatusResponse, String> {
        let res = self.call(pb::Request { kind: Some(pb::request::Kind::GetStatus(pb::Empty {})) }).await?;
        match res.kind {
            Some(pb::response::Kind::DishGetStatus(s)) => Ok(s),
            _ => Err("the dish answered, but not with its status".into()),
        }
    }

    /// Identity. `get_status` carries it too, so this is the probe's one call.
    pub async fn probe(&self) -> Result<Probe, String> {
        let s = self.status().await?;
        let info = s.device_info.ok_or_else(|| "the dish answered, but did not identify itself".to_string())?;
        Ok(parse_probe(&info))
    }

    /// `get_location` → the fix. `Ok(None)` = permitted, no lock yet; Err(PERMISSION_DENIED) names
    /// the Starlink-app switch that fixes it.
    pub async fn location(&self) -> Result<Option<GpsFix>, String> {
        let res = self.call(pb::Request { kind: Some(pb::request::Kind::GetLocation(pb::Empty {})) }).await?;
        match res.kind {
            Some(pb::response::Kind::GetLocation(l)) => Ok(parse_location(&l)),
            _ => Err("the dish answered, but not with its position".into()),
        }
    }

    pub async fn reboot(&self) -> Result<(), String> {
        self.call(pb::Request { kind: Some(pb::request::Kind::Reboot(pb::Empty {})) }).await.map(|_| ())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The owner's Starlink Mini, `get_status` on 2026-09-13 (grpcurl capture), trimmed to what the
    /// hub reads. Idle boat: 9 kbps down, 311 kbps up, 0.16 % of the sky obstructed, one alert.
    pub(crate) fn bench_status() -> pb::DishGetStatusResponse {
        pb::DishGetStatusResponse {
            device_info: Some(pb::DeviceInfo {
                id: Some("ut4088918f-05f0691c-19b97ab8".into()),
                hardware_version: Some("mini1_panda_proto1".into()),
                software_version: Some("2026.08.31.mr85832".into()),
                country_code: Some("US".into()),
            }),
            device_state: Some(pb::DeviceState { uptime_s: Some(91_907) }),
            pop_ping_drop_rate: None,
            obstruction_stats: Some(pb::DishObstructionStats {
                fraction_obstructed: Some(0.001_583_113_5),
                valid_s: Some(91_849.0),
                currently_obstructed: None,
                time_obstructed: Some(2.264_574_7e-5),
            }),
            alerts: Some(pb::DishAlerts { lower_signal_than_predicted: Some(true), ..Default::default() }),
            downlink_throughput_bps: Some(9_110.028),
            uplink_throughput_bps: Some(310_898.6),
            pop_ping_latency_ms: Some(29.295_387),
            stow_requested: None,
            outage: None,
            gps_stats: Some(pb::DishGpsStats { gps_valid: Some(true), gps_sats: Some(25) }),
            eth_speed_mbps: Some(1000),
            is_snr_above_noise_floor: Some(true),
            signal_quality: Some(1.0),
        }
    }

    #[test]
    fn status_normalizes_the_bench_capture() {
        let d = parse_status(&bench_status());
        assert_eq!(d.uptime_s, Some(91_907));
        assert_eq!(d.obstruction_pct, Some(0.2));
        assert_eq!(d.obstructed, None);
        assert_eq!(d.outage, None);
        assert_eq!(d.latency_ms, Some(29.3));
        assert_eq!(d.loss_pct, None);
        assert_eq!((d.down_mbps, d.up_mbps), (Some(0.0), Some(0.3)));
        assert_eq!(d.signal_pct, Some(100.0));
        assert_eq!(d.alerts, vec!["lower signal than predicted".to_string()]);
        assert_eq!((d.gps_valid, d.gps_sats), (Some(true), Some(25)));
        assert_eq!(wan_of(&d), WanStatus { wan: "starlink".into(), up: true, up_known: true, ip: None, uptime_s: None });
    }

    #[test]
    fn an_outage_names_its_cause_and_carries_the_duration_the_dish_measured() {
        let mut s = bench_status();
        s.outage = Some(pb::DishOutage { cause: Some(5), start_timestamp_ns: None, duration_ns: Some(30_000_000_000), did_switch: None });
        s.pop_ping_drop_rate = Some(0.0125);
        let d = parse_status(&s);
        assert_eq!(d.outage.as_deref(), Some("no satellites"));
        assert_eq!(d.outage_cause, Some(5));
        assert_eq!(d.outage_ms, Some(30_000));
        assert_eq!(d.loss_pct, Some(1.3)); // 1.25 rounds half away from zero
        assert!(wan_of(&d).up, "thirty seconds is not yet 'no internet for over 2 minutes'");
        assert_eq!(outage_label(6), "obstructed");
        assert_eq!(outage_label(13), "sky search");
        assert_eq!(outage_label(12), "unknown"); // reserved on api 43
        assert_eq!(outage_label(99), "unknown");
        // A bare status, as a dish that has just booted answers: nothing invented.
        let bare = parse_status(&pb::DishGetStatusResponse::default());
        assert_eq!(bare, DishStatus::default());
        assert!(wan_of(&bare).up, "no outage reported is up — the dish says so by omission");
    }

    /// Every DishOutage.Cause the dish can name, plus a code this hub has never seen and an outage
    /// the dish would not explain at all (`Some(0)` — `cause` absent). The owner's ruling is "all of
    /// them except connectd", so this list is the whole of it.
    const EVERY_CAUSE: [i32; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 99];

    /// One reading of a dish in `cause`, with the duration the dish itself measured (ns) when it
    /// gives one. `outage_since_ms` is unset — `carry_outage` is what fills it.
    fn dish_in(cause: i32, ns: Option<u64>) -> DishStatus {
        let mut s = bench_status();
        s.outage = Some(pb::DishOutage { cause: Some(cause), start_timestamp_ns: None, duration_ns: ns, did_switch: None });
        parse_status(&s)
    }

    /// Poll a dish that reports an outage with NO duration of its own at each of `secs`, folding the
    /// hub's own clock forward; returns the `up` each reading produced.
    fn hub_timed(cause: i32, secs: &[i64]) -> Vec<bool> {
        let mut prev: Option<DishStatus> = None;
        let mut ups = Vec::new();
        for t in secs {
            let mut d = dish_in(cause, None);
            carry_outage(&mut d, prev.as_ref(), t * 1_000);
            ups.push(wan_of(&d).up);
            prev = Some(d);
        }
        ups
    }

    fn param<'a>(p: &'a [(String, String)], k: &str) -> Option<&'a str> {
        p.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    /// THE OWNER'S RULING (Jonathan, 2026-09-24): an outage is "no internet for over 2 minutes", and
    /// which causes count is "all of them except connectd". 119 s is not an outage, 121 s is, and
    /// EVERY cause behaves the same way — there is no table any more.
    #[test]
    fn every_cause_is_an_outage_after_two_minutes_and_none_of_them_before() {
        for cause in EVERY_CAUSE {
            assert_eq!(
                hub_timed(cause, &[0, 119, 120, 121, 3 * 3600]),
                vec![true, true, true, false, false],
                "cause {cause} ({}): 119 s is not an outage, 121 s is, and 120 s exactly is not OVER two minutes",
                outage_label(cause)
            );
        }
    }

    /// The cause is reported EITHER WAY — below the threshold and above it — so the history reads
    /// "stowed" over three hours and "obstructed" over four minutes, never a bare down.
    #[test]
    fn the_dishs_own_cause_is_reported_whether_or_not_it_is_yet_an_outage() {
        for cause in EVERY_CAUSE {
            let label = outage_label(cause);
            let mut young = dish_in(cause, None);
            carry_outage(&mut young, None, 0);
            let p = crate::routers::dish_params(&young, &wan_of(&young), None);
            assert_eq!(param(&p, "outage"), Some(label), "cause {cause} below the threshold: {p:?}");
            assert_eq!(param(&p, "up"), Some("1"), "cause {cause} below the threshold is not a down");

            let mut old = dish_in(cause, Some(3 * 3600 * 1_000_000_000));
            carry_outage(&mut old, None, 0);
            let p = crate::routers::dish_params(&old, &wan_of(&old), None);
            assert_eq!(param(&p, "outage"), Some(label), "cause {cause} past the threshold still names itself: {p:?}");
            assert_eq!(param(&p, "up"), Some("0"), "cause {cause} ({label}) over two minutes IS a WAN outage");
        }
    }

    /// A duration THE DISH measured past two minutes counts at once — a hub that restarted
    /// mid-outage does not restart the clock.
    #[test]
    fn a_duration_the_dish_measured_past_two_minutes_counts_at_once() {
        // No previous reading at all: the hub has just come up, and the dish is the only clock.
        let mut stowed = dish_in(2, Some(3 * 3600 * 1_000_000_000));
        carry_outage(&mut stowed, None, 9_999_999);
        assert_eq!(stowed.outage_ms, Some(3 * 3600 * 1_000));
        assert!(!wan_of(&stowed).up, "the dish had already watched the two minutes elapse");

        // The same boundary, measured by the dish instead of by the hub.
        for (ns, up) in [(119_000_000_000u64, true), (120_000_000_000, true), (121_000_000_000, false)] {
            let mut d = dish_in(6, Some(ns));
            carry_outage(&mut d, None, 0);
            assert_eq!(wan_of(&d).up, up, "a dish-measured {ns} ns");
        }
        // Neither clock may shorten what the other has already seen: an hour of hub-watched outage
        // is not undone by a dish that reports the current cause as seconds old.
        let mut first = dish_in(13, None);
        carry_outage(&mut first, None, 0);
        let mut later = dish_in(5, Some(4_000_000_000));
        carry_outage(&mut later, Some(&first), 3_600_000);
        assert_eq!(later.outage_ms, Some(3_600_000), "the cause changed mid-run; the internet never came back");
        assert!(!wan_of(&later).up);
    }

    /// No outage at all is up, and it ENDS the run: the next outage starts its own two minutes.
    #[test]
    fn no_outage_at_all_is_up_and_clears_the_run() {
        let mut out = dish_in(6, None);
        carry_outage(&mut out, None, 0);
        let mut still = dish_in(6, None);
        carry_outage(&mut still, Some(&out), 200_000);
        assert!(!wan_of(&still).up, "200 s of one unbroken run");

        let mut clear = parse_status(&bench_status());
        carry_outage(&mut clear, Some(&still), 210_000);
        assert_eq!((clear.outage_cause, clear.outage_since_ms, clear.outage_ms), (None, None, None));
        assert!(wan_of(&clear).up);

        let mut again = dish_in(6, None);
        carry_outage(&mut again, Some(&clear), 211_000);
        assert!(wan_of(&again).up, "a new outage starts its own two minutes");
        assert_eq!(again.outage_since_ms, Some(211_000));
    }

    /// HOW THE TWO THRESHOLDS MEET. The dish's own two minutes decide; router_health's 45 s window
    /// never adds to them, because the first down sample a dish can produce already carries a
    /// measured duration past 45 s, which trips the measured-duration shortcut in the same poll.
    #[test]
    fn a_dish_down_sample_skips_the_45_s_router_grace_instead_of_adding_to_it() {
        use crate::router_health::{sample_of, RouterHealth, Sample, Verdict};
        let mut d = dish_in(2, None); // stowed, and the dish will not say for how long
        carry_outage(&mut d, None, 0);
        let mut down = dish_in(2, None);
        carry_outage(&mut down, Some(&d), 121_000);
        assert_eq!(down.outage_ms, Some(121_000));
        let params = crate::routers::dish_params(&down, &wan_of(&down), None);
        let sample = sample_of(Some(&params), down.outage_ms);
        assert_eq!(sample, Sample::ReportedDown { measured_ms: Some(121_000) });
        // THE RELATIONSHIP, not the number: whatever the two thresholds are set to, a dish's down
        // sample always arrives already past the router window, so the window cannot add to it.
        let measured = match sample {
            Sample::ReportedDown { measured_ms } => measured_ms.expect("a dish down sample always carries its age"),
            other => panic!("a dish past two minutes is a down sample, not {other:?}"),
        };
        assert!(measured >= crate::router_health::DOWN_GRACE_MS, "{measured} ms is not past the {} ms router window", crate::router_health::DOWN_GRACE_MS);
        let mut h = RouterHealth::default();
        assert_eq!(h.observe(sample, 121_000, 121_000), Verdict::WentDown, "down at ~2 min, not at 2 min 45 s");

        // And below the threshold there is no bad sample at all, so no window is ever opened.
        let params = crate::routers::dish_params(&d, &wan_of(&d), None);
        assert_eq!(sample_of(Some(&params), d.outage_ms), Sample::Up);
        let mut g = RouterHealth::default();
        assert_eq!(g.observe(Sample::Up, 0, 0), Verdict::Report { recovered: false });
        assert_eq!(g, RouterHealth::default());
    }

    /// An outage the dish will not explain (`cause` absent) is an outage all the same — the owner's
    /// ruling admits only one non-outage, and it is the dish reporting no outage at all.
    #[test]
    fn an_outage_the_dish_did_not_explain_is_still_an_outage() {
        let mut s = bench_status();
        s.outage = Some(pb::DishOutage { cause: None, start_timestamp_ns: None, duration_ns: Some(600_000_000_000), did_switch: None });
        let mut d = parse_status(&s);
        carry_outage(&mut d, None, 0);
        assert_eq!(d.outage_cause, Some(0));
        assert_eq!(d.outage.as_deref(), Some("unknown"));
        assert!(!wan_of(&d).up, "ten minutes of an unexplained outage is ten minutes with no internet");
    }

    #[test]
    fn probe_carries_the_revision_as_model_and_the_dish_id_as_serial() {
        let p = parse_probe(bench_status().device_info.as_ref().unwrap());
        assert_eq!(p.model.as_deref(), Some("Starlink mini1_panda_proto1"));
        assert_eq!(p.firmware.as_deref(), Some("2026.08.31.mr85832"));
        assert_eq!(p.serial.as_deref(), Some("ut4088918f-05f0691c-19b97ab8"));
        assert_eq!(p.mac, None);
        assert_eq!(parse_probe(&pb::DeviceInfo::default()).model.as_deref(), Some("Starlink"));
    }

    #[test]
    fn location_rejects_the_pre_lock_origin_and_keeps_sigma_as_accuracy() {
        let at = |lat: f64, lon: f64| pb::GetLocationResponse { lla: Some(pb::LlaPosition { lat: Some(lat), lon: Some(lon), alt: Some(3.0) }), sigma_m: Some(4.5) };
        let f = parse_location(&at(45.0625, -83.4321)).unwrap();
        assert_eq!((f.lat, f.lon, f.acc), (45.0625, -83.4321, Some(4.5)));
        assert!(parse_location(&at(0.0, 0.0)).is_none());
        assert!(parse_location(&at(91.0, 0.0)).is_none());
        assert!(parse_location(&pb::GetLocationResponse { lla: None, sigma_m: None }).is_none());
    }

    /// The field numbers are the whole contract with an undocumented API, so they are pinned on
    /// the wire, not just in the derive: tag 2004 wire type 2 is the varint 16034 = A2 7D.
    #[test]
    fn field_numbers_are_the_dish_protos() {
        let req = frame(&pb::Request { kind: Some(pb::request::Kind::GetStatus(pb::Empty {})) });
        // get_status = 1004: (1004 << 3) | 2 = 8034 = E2 3E, then a zero-length Empty.
        assert_eq!(&req[..], &[0, 0, 0, 0, 3, 0xE2, 0x3E, 0x00]);
        let res = pb::Response { status: None, api_version: Some(43), kind: Some(pb::response::Kind::DishGetStatus(bench_status())) };
        let bytes = res.encode_to_vec();
        assert_eq!(&bytes[..2], &[0x18, 43], "api_version = 3");
        assert_eq!(&bytes[2..4], &[0xA2, 0x7D], "dish_get_status = 2004");
        let back = pb::Response::decode(&bytes[..]).unwrap();
        assert_eq!(back, res);
        // Inside the status: outage = 1014 → (1014<<3)|2 = 8114 = B2 3F; gps_stats = 1015 → BA 3F.
        let s = pb::DishGetStatusResponse {
            outage: Some(pb::DishOutage { cause: Some(6), ..Default::default() }),
            gps_stats: Some(pb::DishGpsStats { gps_sats: Some(9), gps_valid: None }),
            ..Default::default()
        };
        assert_eq!(s.encode_to_vec(), vec![0xB2, 0x3F, 2, 0x08, 6, 0xBA, 0x3F, 2, 0x10, 9]);
        // get_location = 1017: (1017 << 3) | 2 = 8138 = CA 3F.
        let loc = frame(&pb::Request { kind: Some(pb::request::Kind::GetLocation(pb::Empty {})) });
        assert_eq!(&loc[5..], &[0xCA, 0x3F, 0x00]);
        // reboot = 1001: (1001 << 3) | 2 = 8010 = CA 3E.
        let rb = frame(&pb::Request { kind: Some(pb::request::Kind::Reboot(pb::Empty {})) });
        assert_eq!(&rb[5..], &[0xCA, 0x3E, 0x00]);
    }

    #[test]
    fn framing_round_trips_and_refuses_what_it_cannot_read() {
        let f = frame(&pb::Empty {});
        assert_eq!(&f[..], &[0, 0, 0, 0, 0]);
        assert_eq!(unframe(&f).unwrap(), &[] as &[u8]);
        assert!(unframe(&[0, 0, 0]).unwrap_err().contains("empty"));
        assert!(unframe(&[1, 0, 0, 0, 0]).unwrap_err().contains("compressed"));
        assert!(unframe(&[0, 0, 0, 0, 9, 1, 2]).unwrap_err().contains("cut short"));
    }

    #[test]
    fn grpc_refusals_say_what_the_owner_can_do() {
        let denied = grpc_error(7, "Failed to get location: Disabled due to policy");
        assert!(denied.contains("Starlink switched off local GPS access") && denied.contains("another position source"), "{denied}");
        assert!(grpc_error(14, "").contains("rebooting"));
        assert_eq!(grpc_error(3, "bad arg"), "the dish answered: bad arg (gRPC 3)");
        assert_eq!(grpc_error(3, ""), "the dish answered gRPC status 3");
        let mut h = http::HeaderMap::new();
        h.insert("grpc-message", "not%20allowed%3A%20local".parse().unwrap());
        assert_eq!(grpc_message(&h), "not allowed: local");
        h.insert("grpc-status", "7".parse().unwrap());
        assert_eq!(grpc_status(&h), Some(7));
    }

    /// A mock dish: an h2 server that decodes the Request and answers the way the real one does —
    /// status and reboot succeed; location is PERMISSION_DENIED in the trailers (Starlink's policy
    /// on MVP's Mini, bench 2026-09-13: "Disabled due to policy") unless the test flips
    /// `allow_location` — a Priority-plan dish.
    pub(crate) async fn mock_dish(allow_location: bool) -> u16 {
        mock_dish_with(allow_location, None, None).await
    }

    /// The same mock, reporting an outage in every `get_status`: `cause` is a DishOutage.Cause and
    /// `duration_ns` the duration the dish itself measures, or None for a dish that reports the
    /// outage without saying how long it has lasted (the case the hub has to time itself).
    pub(crate) async fn mock_dish_with(allow_location: bool, cause: Option<i32>, duration_ns: Option<u64>) -> u16 {
        let outage = cause.map(|c| pb::DishOutage { cause: Some(c), start_timestamp_ns: None, duration_ns, did_switch: None });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { break };
                let outage = outage.clone();
                tokio::spawn(async move {
                    let mut conn = h2::server::handshake(sock).await.unwrap();
                    while let Some(Ok((req, mut respond))) = conn.accept().await {
                        assert_eq!(req.uri().path(), GRPC_PATH);
                        assert_eq!(req.headers().get("content-type").unwrap(), "application/grpc");
                        let mut body = req.into_body();
                        let mut buf = Vec::new();
                        while let Some(Ok(c)) = body.data().await {
                            let _ = body.flow_control().release_capacity(c.len());
                            buf.extend_from_slice(&c);
                        }
                        let r = pb::Request::decode(unframe(&buf).unwrap()).unwrap();
                        let mut trailers = http::HeaderMap::new();
                        let answer = match r.kind {
                            Some(pb::request::Kind::GetStatus(_)) => {
                                Some(pb::response::Kind::DishGetStatus(pb::DishGetStatusResponse { outage: outage.clone(), ..bench_status() }))
                            }
                            Some(pb::request::Kind::Reboot(_)) => Some(pb::response::Kind::Reboot(pb::Empty {})),
                            Some(pb::request::Kind::GetLocation(_)) if allow_location => Some(pb::response::Kind::GetLocation(pb::GetLocationResponse {
                                lla: Some(pb::LlaPosition { lat: Some(45.0625), lon: Some(-83.4321), alt: Some(180.0) }),
                                sigma_m: Some(6.0),
                            })),
                            Some(pb::request::Kind::GetLocation(_)) => {
                                trailers.insert("grpc-status", "7".parse().unwrap());
                                trailers.insert("grpc-message", "Failed%20to%20get%20location%3A%20Disabled%20due%20to%20policy".parse().unwrap());
                                None
                            }
                            _ => {
                                trailers.insert("grpc-status", "12".parse().unwrap());
                                None
                            }
                        };
                        let response = http::Response::builder().status(200).header("content-type", "application/grpc").body(()).unwrap();
                        let mut send = respond.send_response(response, false).unwrap();
                        if let Some(kind) = answer {
                            send.send_data(frame(&pb::Response { status: None, api_version: Some(43), kind: Some(kind) }), false).unwrap();
                            trailers.insert("grpc-status", "0".parse().unwrap());
                        }
                        send.send_trailers(trailers).unwrap();
                    }
                });
            }
        });
        port
    }

    #[tokio::test]
    async fn the_client_speaks_grpc_to_a_mock_dish_and_reads_the_trailers() {
        let port = mock_dish(false).await;
        let dish = Starlink::new("127.0.0.1", port);
        let s = parse_status(&dish.status().await.unwrap());
        assert_eq!((s.outage, s.obstruction_pct), (None, Some(0.2)));
        let p = dish.probe().await.unwrap();
        assert_eq!(p.model.as_deref(), Some("Starlink mini1_panda_proto1"));
        dish.reboot().await.unwrap();
        let why = dish.location().await.unwrap_err();
        assert!(why.contains("another position source"), "{why}");

        let port = mock_dish(true).await;
        let dish = Starlink::new("127.0.0.1", port);
        let fix = dish.location().await.unwrap().unwrap();
        assert_eq!((fix.lat, fix.lon, fix.acc), (45.0625, -83.4321, Some(6.0)));
    }

    #[tokio::test]
    async fn an_absent_dish_is_a_plain_reachability_error() {
        let dish = Starlink::new("127.0.0.1", 1);
        let why = dish.status().await.unwrap_err();
        assert!(why.contains("refused the connection") || why.contains("could not be reached"), "{why}");
        assert_eq!((Starlink::new("", 0).host.as_str(), Starlink::new("", 0).port), (DEFAULT_HOST, DEFAULT_PORT));
    }

    /// Against the REAL dish — run by hand from a machine that routes to 192.168.100.1:
    /// `cargo test --lib starlink::tests::live_dish -- --ignored --nocapture`. Prints what it read.
    #[tokio::test]
    #[ignore]
    async fn live_dish() {
        let dish = Starlink::new(DEFAULT_HOST, 0);
        let raw = dish.status().await.expect("get_status");
        let d = parse_status(&raw);
        println!("probe: {:?}", raw.device_info.as_ref().map(parse_probe));
        println!("status: {}", serde_json::to_string_pretty(&d).unwrap());
        println!("wan: {:?}", wan_of(&d));
        println!("location: {:?}", dish.location().await);
        assert!(d.uptime_s.is_some() && d.latency_ms.is_some(), "a live dish reports uptime and latency");
    }
}
