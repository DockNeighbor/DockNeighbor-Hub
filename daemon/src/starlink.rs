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
/// A dish answers get_status in well under a second on the LAN; a call that has not come back in
/// this long is a dish that is not there.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outage: Option<String>,
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

/// PURE: the uplink the dish IS. Up means not in an outage — every outage cause (booting, no
/// satellites, obstructed, stowed…) reads as down, which is what the Connectivity page needs.
pub fn wan_of(d: &DishStatus) -> WanStatus {
    WanStatus { wan: "starlink".into(), up: d.outage.is_none(), ip: None }
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
    Some(GpsFix { lat, lon, acc: r.sigma_m.filter(|a| a.is_finite() && *a >= 0.0) })
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

/// PURE: a non-OK gRPC status → what the owner can act on. 7 (PERMISSION_DENIED) is the one that
/// matters: `get_location` answers "Disabled due to policy" (bench 2026-09-13) until location
/// access is switched on in the Starlink app, and saying that is the difference between a fixable
/// setting and a mystery.
pub fn grpc_error(code: i32, message: &str) -> String {
    match code {
        7 => "the dish refused the request — in the Starlink app, switch on Advanced → Debug data → “Allow access on local network”".into(),
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
}

impl Starlink {
    pub fn new(host: &str, port: u16) -> Self {
        let h = host.trim();
        Starlink {
            host: if h.is_empty() { DEFAULT_HOST.into() } else { h.into() },
            port: if port == 0 { DEFAULT_PORT } else { port },
        }
    }

    async fn call(&self, req: pb::Request) -> Result<pb::Response, String> {
        tokio::time::timeout(CALL_TIMEOUT, self.call_inner(req))
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
        assert_eq!(wan_of(&d), WanStatus { wan: "starlink".into(), up: true, ip: None });
    }

    #[test]
    fn an_outage_names_its_cause_and_reads_as_down() {
        let mut s = bench_status();
        s.outage = Some(pb::DishOutage { cause: Some(5), start_timestamp_ns: None, duration_ns: Some(30_000_000_000), did_switch: None });
        s.pop_ping_drop_rate = Some(0.0125);
        let d = parse_status(&s);
        assert_eq!(d.outage.as_deref(), Some("no satellites"));
        assert_eq!(d.loss_pct, Some(1.3)); // 1.25 rounds half away from zero
        assert!(!wan_of(&d).up);
        assert_eq!(outage_label(6), "obstructed");
        assert_eq!(outage_label(13), "sky search");
        assert_eq!(outage_label(12), "unknown"); // reserved on api 43
        assert_eq!(outage_label(99), "unknown");
        // A bare status, as a dish that has just booted answers: nothing invented.
        let bare = parse_status(&pb::DishGetStatusResponse::default());
        assert_eq!(bare, DishStatus::default());
        assert!(wan_of(&bare).up, "no outage reported is up — the dish says so by omission");
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
        assert!(grpc_error(7, "Failed to get location: Disabled due to policy").contains("Allow access on local network"));
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
    /// status and reboot succeed; location is PERMISSION_DENIED in the trailers (the factory
    /// setting, bench 2026-09-13: "Disabled due to policy") until the test flips `allow_location`.
    pub(crate) async fn mock_dish(allow_location: bool) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { break };
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
                            Some(pb::request::Kind::GetStatus(_)) => Some(pb::response::Kind::DishGetStatus(bench_status())),
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
        assert!(why.contains("Allow access on local network"), "{why}");

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
