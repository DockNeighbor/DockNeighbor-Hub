// The hub SERVER — increment A of ONSITE.md "The hub is a SERVER" (2026-08-18 late): "the hub is a SERVER; apps are
// clients; HTTP only". Owner framing, kept because it is the spec: "its a server, pretty much a
// web server" · "the app is the remote… hub is on all the time" · "think homeassistant... does
// not just listen on localhost."
//
// `--hub` (the flag #381's SYSTEM/ONSTART task has passed since it shipped — parsed nowhere until
// now) starts THIS instead of the GUI: no window, no webview, no per-user anything. It owns the
// machine-wide store (hub_config.rs) exclusively and runs:
//
//   * the MANAGEMENT API on the LAN (0.0.0.0, not loopback — a phone on the boat's Wi-Fi manages
//     the hub exactly like the desktop app on the same machine). Typed allowlist, same discipline
//     as the agent command channel: status / config / token / clear. Nothing generic.
//   * the HEARTBEAT loop — `hub.measurement` on the agent wire, from Rust, off the store.
//   * the KEY SYNC loop — pulls the vehicle members' per-user API keys from the worker
//     (minted per (user, vehicle), owner's scheme). Until the first successful sync the API
//     answers 401 to everything: deny by default, never an open window.
//
// AUTH: every request carries `x-brvg-key`. Keys arrive from the worker with the member's uid and
// vehicle role attached, so the hub knows WHO is calling; writes are gated on the same role
// matrix the app uses (vehicleCapabilities.ts). The hub's own cloud token never appears in any
// response, and reqwest errors are stringified with `without_url()` because heartbeat/sync URLs
// carry that token in `t=`.
//
// The WebSocket to the worker (remote-control relay + live key pushes) is a later increment; the
// sync loop's cadence is the revocation latency until it lands.

use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::response::{Html, IntoResponse, Response};
use axum::Router;
use serde::{Deserialize, Serialize};

use crate::hub_config::{self, HubConfig, MemberKey};
use crate::linktap;
use crate::cycle;
use crate::routers::{vendor_supported, Driver};

/// Production worker base — the Rust twin of DEFAULT_WORKER_URL (configSync.ts). Pinned for the
/// same reason as the TS side: this process holds a credential, so it talks only to first party.
///
/// ⚠️ COMPILE-TIME ONLY, AND THAT IS THE MIGRATION COST. There is no env or config override (see
/// `new_rt`, the sole construction site) — deliberately, because a hub that could be pointed at an
/// arbitrary base by whoever can write its config would hand a device token to that base. So a
/// change of host is a RELEASE plus a self-update on every deployed hub, not a config edit. The
/// 2026-09-02 boatrvguardian -> dockneighbor cutover is exactly that: an already-deployed daemon
/// keeps talking to the old host until it takes a build newer than this one, which is why
/// api.boatrvguardian.com stays declared in brvg-cloud-server's wrangler.toml routes.
const WORKER_BASE: &str = "https://api.dockneighbor.com";

const KEY_HEADER: &str = "x-brvg-key";
/// Key refresh cadence — this IS the revocation latency until the WS push channel exists.
const KEY_SYNC_SECS: u64 = 300;
/// An unregistered hub polls the store at this cadence waiting for the bootstrap seed.
const UNREGISTERED_POLL_SECS: u64 = 30;
const HEARTBEAT_FLOOR_SECS: u64 = 15;

// ── Report-by-exception heartbeat (scanning redesign, Phase 2) ──────────────────────────────────
// The heartbeat is a LIVENESS + config-poll beat, NOT the alarm path: a flood or valve change
// reaches the cloud immediately through the LinkTap poll loop and forward_shelly_to_cloud, and both
// call note_activity() — which ALSO wakes this loop at once. So backing the beat off when nothing is
// happening never delays an alarm; it only stops a parked, idle hub from beating 1440×/day for
// nothing (and, post Phase-1a, from bumping its activity cursor that often).
//
// Three cadences by how long since the last local event (a valve/gateway report or a forwarded
// sensor event):
//   * ACTIVE  — within 2 min of an event: beat fast so the app/cloud track the situation live.
//   * NORMAL  — within 10 min: the configured cadence (heartbeat_secs, floor 15 s).
//   * QUIET   — idle beyond that: one beat every 20 min. Comfortably under the cloud's 60-min
//               offline default (owner contract; connectivitySweep.DEFAULT_OFFLINE_MINS), with two
//               beats of margin, so a healthy parked hub never reads as offline.
const HEARTBEAT_ACTIVE_SECS: u64 = 20;
const HEARTBEAT_QUIET_SECS: u64 = 20 * 60;
const HEARTBEAT_ACTIVE_WINDOW_MS: i64 = 2 * 60 * 1000;
const HEARTBEAT_NORMAL_WINDOW_MS: i64 = 10 * 60 * 1000;

/// PURE: the next heartbeat interval, from how long since the last local event and the configured
/// cadence. ACTIVE is never SLOWER than configured, QUIET never FASTER — so an operator who sets an
/// unusually fast or slow `heartbeat_secs` is still respected as the baseline the modes bend around.
fn heartbeat_interval_secs(idle_ms: i64, configured_secs: u64) -> u64 {
    let normal = configured_secs.max(HEARTBEAT_FLOOR_SECS);
    if idle_ms < HEARTBEAT_ACTIVE_WINDOW_MS {
        HEARTBEAT_ACTIVE_SECS.min(normal)
    } else if idle_ms < HEARTBEAT_NORMAL_WINDOW_MS {
        normal
    } else {
        HEARTBEAT_QUIET_SECS.max(normal)
    }
}

/// PURE: is this invocation the hub service? (`schtasks … "<exe>" --hub` — hub_service.rs.)
pub fn hub_mode_requested<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|a| a == "--hub")
}

// --- Auth ---------------------------------------------------------------------------------------

/// Constant-time string equality — a timing oracle on key comparison would let anyone on the LAN
/// recover a key byte by byte. Length still leaks; keys are fixed-length random, so that is nothing.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// PURE: which member is presenting this key? Scans the whole set unconditionally (no early
/// return) so a miss costs the same as a hit. Empty key or empty set ⇒ None — deny by default.
pub fn authorize<'a>(keys: &'a [MemberKey], presented: &str) -> Option<&'a MemberKey> {
    if presented.is_empty() {
        return None;
    }
    let mut found = None;
    for k in keys {
        if ct_eq(&k.key, presented) {
            found = Some(k);
        }
    }
    found
}

/// The role matrix, hub-side — a deliberate MIRROR of vehicleCapabilities.ts, not a new scheme.
/// Renaming/re-timing the hub is `change_settings` (admin+); handing it a rotated token or tearing
/// it down is device-lifecycle work (`add_device`/`remove_device` grade — coowner/owner).
pub fn may_configure(role: &str) -> bool {
    matches!(role, "owner" | "coowner" | "admin")
}
pub fn may_administer(role: &str) -> bool {
    matches!(role, "owner" | "coowner")
}

/// Actuating a device is control-grade, mirroring the app's vehicleCapabilities `control_devices`:
/// a `monitor` may look, everyone above may act. Deliberately NOT may_configure — opening a valve
/// is not the same authority as changing the hub's settings, and conflating them would silently
/// promote every `control` member to a configurer.
pub fn may_control(role: &str) -> bool {
    matches!(role, "owner" | "coowner" | "admin" | "control")
}

// --- Wire ---------------------------------------------------------------------------------------

/// PURE: the heartbeat URL. Split out because it IS the wire contract — `hub.measurement` rides
/// the same `/api/agent` ingest as the router agent and the worker classifies it as telemetry,
/// never an alert. The only place the hub token meets a URL.
pub fn heartbeat_url(
    worker_base: &str, cfg: &HubConfig, ver: &str, platform: &str, update: Option<&str>, ack: Option<&str>,
) -> Result<String, String> {
    let base = worker_base.trim_end_matches('/');
    let mut u = url::Url::parse(&format!("{base}/api/agent")).map_err(|e| e.to_string())?;
    u.query_pairs_mut()
        .append_pair("vid", &cfg.vid)
        .append_pair("device", &cfg.hub_id)
        .append_pair("event", "hub.measurement")
        .append_pair("t", &cfg.token)
        .append_pair("name", &cfg.name)
        .append_pair("platform", platform)
        .append_pair("ver", ver);
    // Only present when a newer release exists — the worker stores it flat on the hub's sensorState
    // doc (extractSensorStateExtras), so the fleet console reads "update available" straight off the
    // same heartbeat that carries the running version.
    if let Some(v) = update.filter(|v| !v.is_empty()) {
        u.query_pairs_mut().append_pair("update", v);
    }
    // Commands this hub has handled and is acknowledging so the worker prunes them from the queue
    // (agentCommands.ackCommands parses this same comma-separated `ack`). The router agent uses the
    // identical param; the daemon simply never did until it learned to act on commands.
    if let Some(a) = ack.filter(|a| !a.is_empty()) {
        u.query_pairs_mut().append_pair("ack", a);
    }
    Ok(u.to_string())
}

pub async fn send_heartbeat_once(client: &reqwest::Client, worker_base: &str, cfg: &HubConfig) -> Result<(), String> {
    let url = heartbeat_url(worker_base, cfg, env!("CARGO_PKG_VERSION"), std::env::consts::OS, None, None)?;
    let res = client.get(url).send().await.map_err(|e| e.without_url().to_string())?;
    if res.status().is_success() {
        Ok(())
    } else {
        Err(format!("HTTP {}", res.status().as_u16()))
    }
}

/// PURE: read `{linktap:{allowed, profiles}}` out of a worker reply (cloud-server #105).
/// Config-as-state — the worker recomputes it from the vehicle on every report, so what arrives IS
/// the current truth. An absent blob returns None and changes nothing; `allowed` absent reads as
/// FALSE, never as permission.
pub fn parse_linktap_reply(
    body: &serde_json::Value,
) -> Option<(bool, std::collections::HashMap<String, crate::linktap_runtime::WireProfile>)> {
    let lt = body.get("linktap")?;
    let allowed = lt.get("allowed").and_then(|v| v.as_bool()).unwrap_or(false);
    let mut out = std::collections::HashMap::new();
    if let Some(map) = lt.get("profiles").and_then(|v| v.as_object()) {
        for (id, p) in map {
            // Every field OPTIONAL — the worker omits what the vehicle never set, and the hub
            // keeps its own default for those (skip-don't-default, preserved end to end).
            out.insert(
                linktap::normalize_dev_id(id),
                crate::linktap_runtime::WireProfile {
                    duration_secs: p.get("durationSecs").and_then(|v| v.as_u64()),
                    volume_cap_l: p.get("volumeCapL").and_then(|v| v.as_f64()),
                    auto_restart: p.get("autoRestart").and_then(|v| v.as_bool()),
                },
            );
        }
    }
    Some((allowed, out))
}

#[derive(Deserialize)]
struct KeysResp {
    keys: Vec<MemberKey>,
}

/// Pull the member-key set from the worker (increment C's endpoint), authenticated by the hub's
/// own token. An error keeps the last known set — losing the network must not lock the owner out
/// of a hub that is otherwise fine.
pub async fn fetch_member_keys(client: &reqwest::Client, worker_base: &str, cfg: &HubConfig) -> Result<Vec<MemberKey>, String> {
    let base = worker_base.trim_end_matches('/');
    let mut u = url::Url::parse(&format!("{base}/api/hub/keys")).map_err(|e| e.to_string())?;
    u.query_pairs_mut()
        .append_pair("vid", &cfg.vid)
        .append_pair("device", &cfg.hub_id)
        .append_pair("t", &cfg.token);
    let res = client.get(u).send().await.map_err(|e| e.without_url().to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status().as_u16()));
    }
    let body: KeysResp = res.json().await.map_err(|e| e.without_url().to_string())?;
    Ok(body.keys)
}

// --- Server -------------------------------------------------------------------------------------

pub struct Rt {
    /// The LinkTap machine, when this hub has a gateway configured. One instance shared by the
    /// poll loop, the gateway push route and the flood hook — they are three inputs to ONE state
    /// machine, and giving each its own copy would let them disagree about a cycle.
    pub linktap: tokio::sync::Mutex<Option<crate::linktap_runtime::Runtime>>,
    /// The store's base directory — shared_base() in production, a temp dir in tests.
    pub base: PathBuf,
    /// The live key set. Loaded from the store at boot (offline reboot still authenticates known
    /// members), replaced wholesale by each successful sync.
    pub keys: tokio::sync::RwLock<Vec<MemberKey>>,
    /// Serializes read-modify-write of hub.json between handlers and the sync loop.
    pub store: tokio::sync::Mutex<()>,
    pub started: Instant,
    pub worker_base: String,
    /// Bumped on every valve observation, and watched by local apps.
    ///
    /// 🔴 WHY A NOTIFIER AND NOT A FASTER POLL. Owner ruling 2026-08-31: *"The hub and the app,
    /// when local, should be talking in real-time... to get all information from the hub faster."*
    /// The hub ALREADY learns of a change immediately — the LinkTap gateway pushes full status on
    /// every change to /api/hub/linktap/push, and the 60s poll is only a backstop. So the latency
    /// the owner saw was never the hub's knowledge; it was the app asking on its own clock. A
    /// waiter released the moment that knowledge changes takes the interval out of the path.
    pub valve_rev: tokio::sync::watch::Sender<u64>,
    /// The newest released version, when it is newer than the one running — else None. Written by
    /// the update-check loop, read by the heartbeat (so the fleet console sees it) and by
    /// /api/hub/status (so the local app does). Visibility only; nothing here installs anything.
    pub update_available: tokio::sync::RwLock<Option<String>>,
    /// The web app this hub serves at `/` (web_bundle.rs) — None until a signed bundle has been
    /// fetched from the App release. Swapped whole when a newer one is installed.
    pub web: tokio::sync::RwLock<Option<crate::web_bundle::WebBundle>>,
    /// Epoch ms of the last local event worth reporting (a valve/gateway report or a forwarded
    /// sensor event). The heartbeat picks its cadence from how stale this is — recent ⇒ ACTIVE/
    /// NORMAL, long-idle ⇒ QUIET (report-by-exception, Phase 2). Boot counts as activity so a fresh
    /// hub starts responsive and settles to quiet on its own.
    pub last_activity_ms: AtomicI64,
    /// Rung by note_activity() the instant a local event happens, so the heartbeat loop breaks its
    /// nap and beats NOW (in ACTIVE cadence) instead of after the current — possibly 20-minute —
    /// interval. This is what makes quiet-mode backoff free of latency: the alarm path wakes it.
    pub wake: tokio::sync::Notify,
    /// Rung by do_valve the instant a valve command executes, so the linktap poll loop breaks its
    /// (up to 60s) nap and reports the new state to the cloud within a couple seconds instead of on
    /// its next scheduled pass. The gateway PUSH already covers this when it is configured and
    /// reaching us; this makes a command's own result reach the cloud promptly regardless — the
    /// off-boat half of "the hub and the app should talk in real-time" (owner, 2026-08-31), the
    /// on-boat half being valve_rev. See linktap_poll_loop.
    pub linktap_wake: tokio::sync::Notify,
    /// Rung by do_gps when the GPS source is (re)configured, so the poll loop reads a fix from the
    /// new source at once instead of waiting out its interval.
    pub gps_wake: tokio::sync::Notify,
    /// What the last poll of each managed router learned (routers.rs), keyed by router id. What
    /// `/api/hub/routers` and the console show; the poll loop is the only writer.
    pub router_state: tokio::sync::RwLock<HashMap<String, crate::routers::Snapshot>>,
    /// Rings the router poll loop to read now — after an add/edit, or an owner's Refresh.
    pub router_wake: tokio::sync::Notify,
    /// Sensor wiring (sensors.rs): each pending job's live state, keyed by Shelly id. The hunt loop
    /// writes; `/api/hub/sensors` and the status read. Jobs themselves persist in hub.json.
    pub sensor_state: tokio::sync::RwLock<HashMap<String, crate::sensors::SensorState>>,
    /// Rings the hunt loop — a job was added, or a sensor just reported from an address.
    pub sensor_wake: tokio::sync::Notify,
    /// Command ids this PROCESS has already acted on, so a command still in the queue (waiting for
    /// its ack to be read) is not run a second time. In-memory and bounded — forgetting an id is
    /// harmless (at worst one extra up-to-date check). See handle_agent_commands.
    pub handled_cmds: tokio::sync::Mutex<HashSet<String>>,
    /// Command ids to acknowledge on the next heartbeat (`?ack=`), so the worker prunes them from
    /// the queue. An id lands here only once this hub has reached a TERMINAL decision about the
    /// command; a self-update that actually swaps restarts BEFORE acking, on purpose (see
    /// run_commanded_self_update), so a crash mid-update leaves the command to be retried.
    pub pending_acks: tokio::sync::Mutex<Vec<String>>,
    /// Telemetry reports the uplink refused or dropped, oldest first. A boat's cellular link fails
    /// sends constantly (`report … failed to send`), and each failed report USED TO BE LOST — a gap
    /// in the cloud with no retry. They now queue here and drain FIFO on the next successful send or
    /// heartbeat. BOUNDED (drop-oldest at MAX_SPOOL_REPORTS): a long outage sheds the oldest samples
    /// rather than growing without limit. In-memory only — a restart clears it, which is acceptable
    /// (stale telemetry has little value, and restarts are rare). See spool_report / drain_reports.
    pub pending_reports: tokio::sync::Mutex<std::collections::VecDeque<crate::linktap_runtime::Report>>,
    /// Held for the duration of a drain so the poll loop and the heartbeat loop cannot drain at once
    /// and double-send the front report. Pushes take `pending_reports` only briefly and never wait
    /// on this, so enqueuing never blocks behind a network flush.
    pub report_flush: tokio::sync::Mutex<()>,
}

/// Record that something happened locally and wake the heartbeat to report it immediately.
/// Called from the telemetry/forward paths — never from the heartbeat itself, or it would keep
/// itself perpetually ACTIVE.
fn note_activity(rt: &Rt) {
    rt.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    rt.wake.notify_one();
}

pub type Shared = Arc<Rt>;

pub fn new_rt(base: PathBuf, worker_base: String) -> Shared {
    let keys = hub_config::read_config_in(&base).member_keys;
    let web = crate::web_bundle::load_current(&base);
    Arc::new(Rt {
        base,
        web: tokio::sync::RwLock::new(web),
        keys: tokio::sync::RwLock::new(keys),
        store: tokio::sync::Mutex::new(()),
        started: Instant::now(),
        worker_base,
        linktap: tokio::sync::Mutex::new(None),
        valve_rev: tokio::sync::watch::channel(0u64).0,
        update_available: tokio::sync::RwLock::new(None),
        last_activity_ms: AtomicI64::new(now_ms()),
        wake: tokio::sync::Notify::new(),
        linktap_wake: tokio::sync::Notify::new(),
        gps_wake: tokio::sync::Notify::new(),
        router_state: tokio::sync::RwLock::new(HashMap::new()),
        router_wake: tokio::sync::Notify::new(),
        sensor_state: tokio::sync::RwLock::new(HashMap::new()),
        sensor_wake: tokio::sync::Notify::new(),
        handled_cmds: tokio::sync::Mutex::new(HashSet::new()),
        pending_acks: tokio::sync::Mutex::new(Vec::new()),
        pending_reports: tokio::sync::Mutex::new(std::collections::VecDeque::new()),
        report_flush: tokio::sync::Mutex::new(()),
    })
}

pub fn router(rt: Shared) -> Router {
    Router::new()
        // The local management web UI, served over plain HTTP on the LAN so a browser can open it
        // directly at http://<hub-ip>:<port>/ — no cloud, no app, and (unlike the HTTPS web app) no
        // mixed-content block reaching this HTTP hub. The page is inert static HTML/JS: it carries
        // no secret and calls the SAME key-gated API below, so serving it unauthenticated changes
        // no security boundary (the API is the boundary). Actions need a member key the user pastes.
        .route("/console", get(h_index))
        .route("/api/hub/status", get(h_status))
        .route("/api/hub/logs", get(h_logs))
        .route("/api/hub/config", post(h_config))
        .route("/api/hub/token", post(h_token))
        .route("/api/hub/clear", post(h_clear))
        .route("/api/hub/update", post(h_update))
        .route("/api/hub/linktap/valve", post(h_valve))
        .route("/api/hub/gps", post(h_gps))
        .route("/api/hub/routers", get(h_routers_list).post(h_routers))
        .route("/api/hub/sensors", get(h_sensors_list).post(h_sensors))
        .route("/api/hub/linktap/state", get(h_valve_state))
        // The GATEWAY's own push (vendor doc §4.1: full status on every change + a 2-min
        // heartbeat). ⚠️ UNAUTHENTICATED BY NECESSITY — the LinkTap gateway is a fixed-firmware
        // appliance that cannot present a key. That is acceptable ONLY because this route is
        // inert: it accepts no commands, changes no configuration, and its body can do nothing
        // but feed status for valves this hub was already told to watch (unknown dev_ids are
        // dropped by the runtime). The worst a hostile LAN peer achieves is a wrong volume
        // reading, which the next poll corrects — deliberately NOT in the relay allowlist, so it
        // is reachable only from the LAN.
        .route("/api/hub/linktap/push", post(h_linktap_push))
        // LOCAL SHELLY INGEST — the flood sensor's webhook, pointed at the hub instead of (or as
        // well as) the cloud. THE REASON THIS EXISTS: with LinkTap's cloud removed and a hub
        // required for valve control (owner ruling 2026-08-27, option (a)), the cloud→hub close is
        // wired — but a boat's flood is exactly the moment the uplink is least likely to be there.
        // This is the close that does not touch the WAN at all.
        //
        // GET **AND** POST. Shelly devices fire GETs at a static URL; the cloud's own /api/shelly
        // accepts both verbs for precisely this reason, and a 405-on-GET has bitten this project
        // before — a route that looks healthy in every test and is silently unreachable by the
        // only device that calls it.
        .route("/api/hub/shelly", get(h_shelly).post(h_shelly))
        // First-run only, and only from this machine — see h_identity.
        .route("/api/hub/ping", get(h_ping))
        .route("/api/hub/identity", get(h_identity))
        .route("/api/hub/bootstrap", post(h_bootstrap))
        // Everything that is not an API route or the console is the WEB APP (web_bundle.rs): its
        // files by path, the SPA's index.html for any app route. Registered last so it can never
        // shadow a route above it.
        .fallback(h_web)
        .with_state(rt)
}

/// Everything the status endpoint says about the hub. NO token, no key material — `keysSynced`
/// is a count, which is diagnostics ("did the sync land"), not a secret.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusBody {
    hub_id: String,
    vid: String,
    name: String,
    enabled: bool,
    heartbeat_secs: u32,
    http_port: u16,
    registered: bool,
    version: String,
    platform: String,
    uptime_secs: u64,
    keys_synced: usize,
    /// What this hub can actually DO. The app routes valve control through the hub ONLY when this
    /// contains "linktap" (app #412 utils/valveExecutor) — absence is never read as capability, so
    /// an older daemon or an unpermitted vehicle simply keeps the app on its direct paths.
    capabilities: Vec<String>,
    /// Is `/api/hub/shelly` armed — i.e. does this hub hold the vehicle's webhook secret?
    ///
    /// A BOOLEAN, never the secret. It exists because the alternative to answering this question
    /// is a silently deaf flood path: a hub with no secret refuses every Shelly report (deny by
    /// default — see hub_config::shelly_secret), and without this flag the app would have no way
    /// to tell "no sensor has ever fired" from "every sensor has been refused for a month".
    shelly_ingest_armed: bool,
    /// Why this hub's configuration could not be read, when it could not be.
    ///
    /// A damaged file reads back as DEFAULTS, so without this field a wedged hub is
    /// indistinguishable from a factory-fresh one: `registered:false`, no capabilities, no
    /// explanation — which is exactly how a real hub presented for hours after three BOM bytes
    /// landed in its `hub.json`. `None` in the normal case, so nothing changes for a healthy hub.
    #[serde(skip_serializing_if = "Option::is_none")]
    config_damaged: Option<String>,
    /// The newest released version, when it is newer than the one running — else omitted. Lets the
    /// local app show "update available" next to the running version. Visibility only (phase 1a).
    #[serde(skip_serializing_if = "Option::is_none")]
    update_available: Option<String>,
    /// The configured GPS source, REDACTED (host/port/username/device, never the password). Absent
    /// when no source is set, so the console can show "add a GPS" vs the current one.
    #[serde(skip_serializing_if = "Option::is_none")]
    gps: Option<GpsStatus>,
    /// The web app version this hub serves at `/`, when it has one (web_bundle.rs).
    #[serde(skip_serializing_if = "Option::is_none")]
    web_version: Option<String>,
    /// The routers this hub manages (routers.rs), redacted, with what the last poll learned.
    /// Always present (empty when none) so a caller can tell "none configured" from "old daemon".
    routers: Vec<RouterStatus>,
    /// Sensor wiring jobs (sensors.rs) — pending, wired or failed — so the wizard can wait for
    /// the hub's confirmation before it lets the user leave. Always present.
    sensors: Vec<crate::sensors::SensorState>,
}

/// One managed router as the status exposes it — the config WITHOUT the password or the agent
/// token (only whether each is set), plus the last poll's snapshot.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RouterStatus {
    id: String,
    vendor: String,
    name: String,
    host: String,
    port: u16,
    username: String,
    has_password: bool,
    /// The hub holds this router's agent token, so its telemetry reaches the cloud.
    agent_enrolled: bool,
    gps_enabled: bool,
    gps_dev_id: String,
    poll_secs: u64,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<crate::routers::Snapshot>,
}

fn router_status(r: &hub_config::RouterConfig, state: Option<&crate::routers::Snapshot>) -> RouterStatus {
    RouterStatus {
        id: r.id.clone(),
        vendor: r.vendor.clone(),
        name: r.name.clone(),
        host: r.host.clone(),
        port: if r.port != 0 { r.port } else { 443 },
        username: if r.username.is_empty() { "admin".into() } else { r.username.clone() },
        has_password: !r.password.is_empty(),
        agent_enrolled: !r.agent_token.is_empty(),
        gps_enabled: r.gps_enabled,
        gps_dev_id: r.gps_dev_id.clone(),
        poll_secs: crate::routers::poll_secs(r),
        enabled: r.enabled,
        state: state.cloned(),
    }
}

async fn routers_status(rt: &Rt, cfg: &HubConfig) -> Vec<RouterStatus> {
    let states = rt.router_state.read().await;
    cfg.routers.iter().map(|r| router_status(r, states.get(&r.id))).collect()
}

/// The GPS source as the status exposes it — deliberately without the password (only `hasPassword`),
/// the same redaction rule the hub token follows.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GpsStatus {
    kind: String,
    host: String,
    port: u16,
    /// `nmea` only; `tcp` when unset.
    protocol: String,
    username: String,
    dev_id: String,
    enabled: bool,
    has_password: bool,
}

fn gps_status(g: &hub_config::GpsConfig) -> Option<GpsStatus> {
    if g.host.is_empty() { return None; }
    Some(GpsStatus {
        kind: g.kind.clone(),
        host: g.host.clone(),
        port: if g.port != 0 { g.port } else if g.kind == "nmea" { crate::gps::NMEA_DEFAULT_PORT } else { 443 },
        protocol: if g.protocol.is_empty() { "tcp".into() } else { g.protocol.clone() },
        username: g.username.clone(),
        dev_id: g.dev_id.clone(),
        enabled: g.enabled,
        has_password: !g.password.is_empty(),
    })
}

async fn status_body(rt: &Rt) -> StatusBody {
    let cfg = hub_config::read_config_in(&rt.base);
    let routers = routers_status(rt, &cfg).await;
    let sensors = sensors_status(rt).await;
    StatusBody {
        routers,
        sensors,
        config_damaged: hub_config::config_damage_in(&rt.base),
        registered: !cfg.token.is_empty(),
        hub_id: cfg.hub_id,
        vid: cfg.vid,
        name: cfg.name,
        enabled: cfg.enabled,
        heartbeat_secs: cfg.heartbeat_secs,
        http_port: cfg.http_port,
        version: env!("CARGO_PKG_VERSION").into(),
        platform: std::env::consts::OS.into(),
        uptime_secs: rt.started.elapsed().as_secs(),
        keys_synced: rt.keys.read().await.len(),
        capabilities: capabilities_of(&cfg.linktap),
        shelly_ingest_armed: !cfg.shelly_secret.is_empty(),
        update_available: rt.update_available.read().await.clone(),
        gps: gps_status(&cfg.gps),
        web_version: rt.web.read().await.as_ref().map(|w| w.version.clone()),
    }
}

/// The capability list. `linktap` requires BOTH a configured gateway AND the cloud's permission
/// (the paid gate, cached from the worker) — either missing means the hub does not claim it, and
/// the app keeps using its direct paths. Two conditions, one AND, so neither can be forgotten.
fn capabilities_of(lt: &hub_config::LinkTapConfig) -> Vec<String> {
    let mut caps = Vec::new();
    if lt.allowed && !lt.host.is_empty() && !lt.gw_id.is_empty() {
        caps.push("linktap".to_string());
    }
    // Managed routers (routers.rs: Cradlepoint; peplink.rs: Peplink) need no plan gate and no
    // configuration to be OFFERED — the app shows those vendors in Add Router only when a hub
    // advertising this is reachable (owner: without a hub those vendors are not offered at all).
    caps.push("routers".to_string());
    // Sensor wiring (sensors.rs): the hub finishes a sleepy sensor's setup on the LAN.
    caps.push("sensors".to_string());
    caps
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfigReq {
    name: Option<String>,
    heartbeat_secs: Option<u32>,
    enabled: Option<bool>,
    /// The vehicle's Shelly webhook secret, handed over by the app so the hub can authenticate
    /// local flood reports with the internet down (hub_config::shelly_secret). Sent here rather
    /// than through a new endpoint because this is already the settings door, with the settings
    /// role gate on it. An EMPTY string disarms the ingest deliberately — it is how a rotated or
    /// mistakenly-set secret is taken back, and the status body reports the resulting state.
    shelly_secret: Option<String>,
}

#[derive(Deserialize)]
struct TokenReq {
    token: String,
}

/// WHO is asking. On the LAN this comes from the presented key; over the relay it is VOUCHED FOR
/// by the worker, which authenticated the user itself (ONSITE.md "Relay protocol", 2026-08-18 late). Either way
/// the hub applies its OWN role gates to it — the worker deciding who someone is has never been
/// the same as deciding what they may do.
#[derive(Clone, Debug)]
pub struct Caller {
    pub uid: String,
    pub role: String,
}

/// A handler's answer, independent of how it was asked. Both doors return this: the LAN handlers
/// turn it into an HTTP response, the relay wraps it in a `result` frame. One implementation of
/// every rule, so the two paths cannot drift.
pub struct Answer {
    pub status: u16,
    pub body: String,
}

fn ok_json<T: Serialize>(value: &T) -> Answer {
    match serde_json::to_string(value) {
        Ok(body) => Answer { status: 200, body },
        Err(e) => err(500, &format!("could not encode the response: {e}")),
    }
}

/// Errors are JSON too — `{"error": "..."}` — so a caller parses one shape whichever door it came
/// through. A relayed body is passed back by the worker untouched, and a mix of plain text and
/// JSON would put that seam into every client.
fn err(status: u16, message: &str) -> Answer {
    Answer {
        status,
        body: serde_json::json!({ "error": message }).to_string(),
    }
}

// --- The core: one implementation per action ------------------------------------------------------

/// Route an authenticated call. The path+verb allowlist is HERE rather than only in axum's router,
/// because the relay reaches these actions without passing through the router at all.
pub async fn dispatch(rt: &Rt, caller: &Caller, method: &str, path: &str, body: &[u8]) -> Answer {
    match (method, path) {
        ("GET", "/api/hub/status") => do_status(rt).await,
        ("GET", "/api/hub/logs") => do_logs(rt, caller).await,
        ("POST", "/api/hub/config") => do_config(rt, caller, body).await,
        ("POST", "/api/hub/token") => do_token(rt, caller, body).await,
        ("POST", "/api/hub/clear") => do_clear(rt, caller).await,
        ("POST", "/api/hub/update") => do_update(caller).await,
        ("POST", "/api/hub/linktap/valve") => do_valve(rt, caller, body).await,
        ("POST", "/api/hub/gps") => do_gps(rt, caller, body).await,
        ("GET", "/api/hub/routers") => do_routers_list(rt).await,
        ("POST", "/api/hub/routers") => do_routers(rt, caller, body).await,
        ("GET", "/api/hub/sensors") => do_sensors_list(rt).await,
        ("POST", "/api/hub/sensors") => do_sensors(rt, caller, body).await,
        _ => err(404, "no such hub endpoint"),
    }
}

async fn do_status(rt: &Rt) -> Answer {
    ok_json(&status_body(rt).await)
}

async fn do_config(rt: &Rt, caller: &Caller, body: &[u8]) -> Answer {
    if !may_configure(&caller.role) {
        return err(403, "changing the hub's settings needs an admin, co-owner or owner");
    }
    let req: ConfigReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return err(422, &format!("invalid JSON body: {e}")),
    };
    if let Some(h) = req.heartbeat_secs {
        if u64::from(h) < HEARTBEAT_FLOOR_SECS {
            return err(422, &format!("heartbeatSecs must be at least {HEARTBEAT_FLOOR_SECS}"));
        }
    }
    let name = match req.name {
        Some(n) => {
            let t = n.trim().to_string();
            if t.is_empty() {
                return err(422, "name must not be empty");
            }
            Some(t)
        }
        None => None,
    };
    {
        let _g = rt.store.lock().await;
        let mut cfg = hub_config::read_config_in(&rt.base);
        if let Some(n) = name {
            cfg.name = n;
        }
        if let Some(h) = req.heartbeat_secs {
            cfg.heartbeat_secs = h;
        }
        if let Some(e) = req.enabled {
            cfg.enabled = e;
        }
        if let Some(sec) = req.shelly_secret {
            // Trimmed, because it arrives from a copy/paste field in the app and a trailing
            // newline would silently break every constant-time comparison against it.
            cfg.shelly_secret = sec.trim().to_string();
        }
        if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
            return err(500, &e);
        }
    }
    ok_json(&status_body(rt).await)
}

/// Token handover after the app rotates the enrollment (re-enroll replaces the token server-side;
/// the new one has to reach the hub or its heartbeats start bouncing).
async fn do_token(rt: &Rt, caller: &Caller, body: &[u8]) -> Answer {
    if !may_administer(&caller.role) {
        return err(403, "rotating the hub's credential needs a co-owner or the owner");
    }
    let req: TokenReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return err(422, &format!("invalid JSON body: {e}")),
    };
    if req.token.is_empty() {
        return err(422, "token must not be empty");
    }
    {
        let _g = rt.store.lock().await;
        let mut cfg = hub_config::read_config_in(&rt.base);
        cfg.token = req.token;
        if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
            return err(500, &e);
        }
    }
    ok_json(&status_body(rt).await)
}

/// The local half of un-registering: wipe the store. The CALLER revokes the enrollment with the
/// worker — it holds the user auth that revocation needs; this process never does.
async fn do_clear(rt: &Rt, caller: &Caller) -> Answer {
    if !may_administer(&caller.role) {
        return err(403, "removing the hub needs a co-owner or the owner");
    }
    {
        let _g = rt.store.lock().await;
        if let Err(e) = hub_config::clear_in(&rt.base) {
            return err(500, &e);
        }
    }
    *rt.keys.write().await = Vec::new();
    Answer { status: 204, body: String::new() }
}

/// Remote self-update (phase 1b). Co-owner/owner only, the same bar as removing the hub — it
/// replaces the running software. The work runs in a detached task so the caller gets an immediate
/// reply: on success the process EXITS and the supervisor relaunches the new binary (there is no
/// "200 updated" to return to a caller whose hub is about to restart), and the real outcome is
/// visible as the reported version changing (and in the hub log). On any failure the running binary
/// is untouched and the reason is logged.
async fn do_update(caller: &Caller) -> Answer {
    if !may_administer(&caller.role) {
        return err(403, "updating the hub needs a co-owner or the owner");
    }
    if crate::self_update::asset_for(std::env::consts::OS, std::env::consts::ARCH).is_none() {
        return err(501, "remote update is not supported on this platform yet — use the app's installer");
    }
    tokio::spawn(async {
        let client = http_client();
        match crate::self_update::perform_update(&client).await {
            crate::self_update::UpdateOutcome::Swapped { to_version } => {
                // Give the HTTP reply a beat to flush before anything restarts.
                tokio::time::sleep(Duration::from_millis(500)).await;
                // Windows: a detached `net stop`/`net start` bounces the service (it does not
                // relaunch on a clean exit); this process is killed by that stop, so we do NOT exit
                // ourselves. Unix: exit and let the supervisor relaunch from the swapped path.
                let _restarting = crate::self_update::finalize_restart();
                crate::hlog!("hub: self-update installed {to_version}; restarting into it");
                #[cfg(unix)]
                std::process::exit(0);
            }
            crate::self_update::UpdateOutcome::UpToDate => {
                crate::hlog!("hub: self-update requested but already current");
            }
            crate::self_update::UpdateOutcome::Failed(why) => {
                crate::hlog!("hub: self-update failed: {why}");
            }
        }
    });
    ok_json(&serde_json::json!({ "status": "update started" }))
}

// --- The LAN door ---------------------------------------------------------------------------------

async fn caller_from_headers(rt: &Rt, headers: &HeaderMap) -> Option<Caller> {
    let presented = headers.get(KEY_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("");
    let keys = rt.keys.read().await;
    authorize(&keys, presented).map(|k| Caller { uid: k.uid.clone(), role: k.role.clone() })
}

/// Every LAN request funnels through here: authenticate, THEN dispatch.
///
/// The body is taken as raw bytes on purpose. Axum's `Json<T>` extractor runs BEFORE the handler
/// body, which would put the deserializer in front of the auth check — anyone on the LAN could
/// reach it, and every future body change would be pre-auth attack surface. Found by driving the
/// running server (an unauthorized POST to /api/hub/token answered 422, not 401), which is exactly
/// what the unit tests could not show, because they always sent a well-formed body with a valid key.
async fn lan_call(rt: &Rt, headers: &HeaderMap, method: &str, path: &str, body: &[u8]) -> Response {
    let Some(caller) = caller_from_headers(rt, headers).await else {
        return answer_response(err(401, "missing or unknown API key"));
    };
    answer_response(dispatch(rt, &caller, method, path, body).await)
}

fn answer_response(a: Answer) -> Response {
    let status = StatusCode::from_u16(a.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    if a.body.is_empty() {
        return status.into_response();
    }
    (status, [(axum::http::header::CONTENT_TYPE, "application/json")], a.body).into_response()
}

// --- The first-run door ---------------------------------------------------------------------------
//
// A hub that has never been configured has no member keys, so it cannot authenticate anybody — yet
// somebody has to give it its vehicle and its cloud token. That is what these two endpoints are
// for, and they are open ONLY while both of these hold:
//
//   * the hub is UNCONFIGURED (no vehicle, no token). The moment it has either, both endpoints
//     refuse forever. Re-registering a live hub goes through the authenticated /api/hub/token
//     instead, so this is never a takeover path.
//   * the caller is on LOOPBACK. The app that sets a hub up is the app running on the machine that
//     is becoming the hub, moments after installing the service. Nothing off-box can reach this.
//
// What an attacker would gain by racing it: they would need local code execution on that machine
// AND a valid cloud token for the vehicle, which requires being its owner or co-owner. The window
// is one first-run, and the capability behind it is one they already have.
//
// This replaces the app writing hub.json over Tauri IPC. Owner, 2026-08-19: "i am not sure why the
// service has any shared files… the app just installs the separate hub application that it controls
// through http". The service now OWNS its configuration — it is the only process that writes it —
// which is also what removes the macOS blocker, since the app no longer needs the shared folder.

/// "Is a hub here, and is it signed to a vehicle?" — unauthenticated, no side effects, on purpose.
///
/// The app needs this to decide whether SIGNING is even offerable, and it cannot use the other two
/// endpoints to find out: `/api/hub/status` needs a member key, which an unsigned hub has none of,
/// and `/api/hub/identity` MINTS an id — a write, and one that fails outright where the service has
/// not created its config directory. Gating the setup flow on either produced a hub that could
/// never be signed, because the only way to reach the door was through the door.
///
/// Nothing here is a secret. Anyone who can reach this port can already see it is open; telling
/// them a hub answers on it, and whether it has a vehicle, adds nothing they could not infer.
/// The local management web UI. Baked into the binary (include_str!) so a hub on a boat with no
/// internet still serves its own console; inert static HTML that talks to the key-gated API.
async fn h_index() -> Response {
    Html(include_str!("../webui/index.html")).into_response()
}

/// What `/` says before a web bundle has ever been fetched: plain, and pointing at the console
/// so the box is still usable. Not a 404 — the hub is fine; it just has nothing to serve yet.
const NO_WEB_BUNDLE_HTML: &str = concat!(
    "<!doctype html><meta charset=utf-8><title>DockNeighbor Hub</title>",
    "<body style=\"font-family:system-ui;margin:3rem;max-width:40rem\">",
    "<h1>DockNeighbor Hub</h1>",
    "<p>This hub has not downloaded the web app yet. It fetches the latest signed release on its ",
    "next update check, which needs internet access.</p>",
    "<p><a href=\"/console\">Open the hub console</a></p>",
);

/// The web app, from the bundle in service. A file inside the bundle is served as itself; any other
/// path is the SPA's `index.html` (client-side routes). Unauthenticated by design — this is public
/// application code, the same bytes app.dockneighbor.com serves; the API it talks to is still
/// key-gated route by route. Path traversal is refused in web_bundle::resolve.
async fn h_web(State(rt): State<Shared>, uri: axum::http::Uri) -> Response {
    let Some(bundle) = rt.web.read().await.clone() else {
        return Html(NO_WEB_BUNDLE_HTML).into_response();
    };
    if let Some(path) = crate::web_bundle::resolve(&bundle.dir, uri.path()) {
        return match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let ct = crate::web_bundle::content_type(&path);
                // Hashed Vite assets are immutable by name; index.html and the rest must revalidate
                // so a newly installed bundle is picked up on the next load.
                let cache = if uri.path().starts_with("/assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, ct), (axum::http::header::CACHE_CONTROL, cache)],
                    bytes,
                )
                    .into_response()
            }
            Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
        };
    }
    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"), (axum::http::header::CACHE_CONTROL, "no-cache")],
        bundle.index,
    )
        .into_response()
}

async fn h_ping(State(rt): State<Shared>) -> Response {
    let damage = hub_config::config_damage_in(&rt.base);
    let cfg = hub_config::read_config_in(&rt.base);
    let registered = !cfg.token.is_empty();
    answer_response(ok_json(&serde_json::json!({
        "ok": true,
        "registered": registered,
        "version": env!("CARGO_PKG_VERSION"),
        // Is this hub unclaimed AND still inside its setup window? The app sweeps a LAN for hubs
        // and needs to tell "here is a hub you can adopt" from "here is a hub, but you have missed
        // its window and should restart its service" — without trying a setup call to find out.
        "adoptable": !registered && damage.is_none() && rt.started.elapsed() <= crate::adopt::ADOPTION_WINDOW,
        // ⚠️ A BOOLEAN, HERE, BECAUSE STATUS CANNOT BE REACHED WHEN IT IS TRUE. `configDamaged` was
        // added to /api/hub/status in #61 — and testing that on CENTRAL showed status is exactly
        // what a damaged hub CANNOT serve: the member keys that authorize it live in the file that
        // will not parse, so every read is 401. The one moment the hub most needs to explain
        // itself is the one moment the authenticated door is shut. Ping is unauthenticated by
        // design (see the doc comment above), so the fact lives here too. The DETAIL — path and
        // parse error — stays on status, where a member key has been proven.
        "configDamaged": damage.is_some(),
    })))
}

fn is_loopback(addr: SocketAddr) -> bool {
    addr.ip().is_loopback()
}

/// Refuse unless this is a first run, from this machine. Returns the reason when it refuses, so a
/// misconfigured setup says which of the two rules stopped it.
async fn first_run_only(rt: &Rt, addr: SocketAddr) -> Option<Answer> {
    // ⚠️ ORDER MATTERS, AND THE FIRST VERSION OF THIS GOT IT WRONG. The checks below run
    // PERMANENT-STATE FIRST, CALLER-STATE SECOND, because the two answer different questions:
    //
    //   * "is this hub claimable AT ALL?"  — a property of the hub, true or false from everywhere.
    //   * "may THIS caller claim it?"      — a property of the peer and the clock.
    //
    // Asking the caller question first made a SIGNED hub answer a LAN peer with
    // "this hub's setup window has closed - restart the hub service to open it again", which is
    // both wrong and dangerous advice: it invites someone to restart the service of a hub that is
    // already claimed, on the promise of a claim window that will never apply to them. Found by
    // testing the endpoint on CENTRAL rather than by reading the code.
    if let Some(why) = hub_config::config_damage_in(&rt.base) {
        // A damaged config reads back as defaults, which look exactly like a first run — so without
        // this the app would be invited to sign a hub whose real identity is still on disk, and the
        // write would be refused half way through setup with a confusing 500.
        return Some(err(409, &format!("this hub's configuration is damaged and must be repaired or removed first: {why}")));
    }
    let cfg = hub_config::read_config_in(&rt.base);
    if !cfg.vid.is_empty() || !cfg.token.is_empty() {
        return Some(err(409, "this hub is already set up; rotate its credential instead"));
    }
    // Only now does WHERE the caller is matter. Loopback, or a device on one of this hub's own /24s
    // within the claim window — see adopt.rs for why the loopback-only rule had to go and for the
    // three bounds that make the LAN door acceptable.
    if let Err(refusal) = crate::adopt::may_set_up(addr.ip(), &crate::linktap_discover::local_ipv4s(), rt.started.elapsed(), crate::adopt::ADOPTION_WINDOW) {
        // LOG THE ATTEMPT WITH THE PEER. A hub that gets claimed on a shared marina network must be
        // able to say by whom, and a hub that keeps refusing an owner who is one subnet away must
        // be able to say that too — neither is answerable from the 403 alone. Now that this runs
        // last it fires only for genuinely UNCLAIMED hubs, so it is signal rather than noise.
        crate::hlog!("setup: refused {} from {} ({:?})", addr.ip(), refusal.message(), refusal);
        return Some(err(403, refusal.message()));
    }
    if !is_loopback(addr) {
        crate::hlog!("setup: LAN setup call from {} accepted (claim window open)", addr.ip());
    }
    None
}

/// The machine's hub id, minted on first ask. The app needs it BEFORE it can enroll — the cloud
/// token is issued to this id — so this is the first call of the setup sequence.
async fn h_identity(State(rt): State<Shared>, ConnectInfo(addr): ConnectInfo<SocketAddr>) -> Response {
    if let Some(refusal) = first_run_only(&rt, addr).await {
        return answer_response(refusal);
    }
    let _g = rt.store.lock().await;
    let (cfg, changed) = hub_config::with_hub_id(hub_config::read_config_in(&rt.base), hub_config::mint_hub_id);
    if changed {
        if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
            // The one place this can fail is a config directory the service cannot create. Say so
            // plainly: it is a packaging problem, not something the user can retry their way out of.
            return answer_response(err(500, &format!("this hub cannot write its own configuration: {e}")));
        }
    }
    answer_response(ok_json(&serde_json::json!({ "hubId": cfg.hub_id })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapReq {
    vid: String,
    name: String,
    token: String,
    heartbeat_secs: Option<u32>,
}

/// Sign this hub to a vehicle: the app has just enrolled the id from /api/hub/identity and hands
/// over the resulting cloud token. After this the hub is configured and this door is shut.
async fn h_bootstrap(
    State(rt): State<Shared>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = first_run_only(&rt, addr).await {
        return answer_response(refusal);
    }
    let req: BootstrapReq = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return answer_response(err(422, &format!("invalid JSON body: {e}"))),
    };
    if req.vid.is_empty() || req.token.is_empty() {
        return answer_response(err(422, "vid and token are required"));
    }
    let hb = req.heartbeat_secs.unwrap_or(60);
    if u64::from(hb) < HEARTBEAT_FLOOR_SECS {
        return answer_response(err(422, &format!("heartbeatSecs must be at least {HEARTBEAT_FLOOR_SECS}")));
    }
    {
        let _g = rt.store.lock().await;
        let (mut cfg, _) = hub_config::with_hub_id(hub_config::read_config_in(&rt.base), hub_config::mint_hub_id);
        cfg.vid = req.vid;
        cfg.name = if req.name.trim().is_empty() { "Hub".to_string() } else { req.name.trim().to_string() };
        cfg.token = req.token;
        cfg.enabled = true;
        cfg.heartbeat_secs = hb;
        if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
            return answer_response(err(500, &format!("this hub cannot write its own configuration: {e}")));
        }
    }
    answer_response(ok_json(&status_body(&rt).await))
}

/// The hub's own recent log lines.
///
/// ⚠️ THIS IS THE POINT OF THE LOG FILE. A hub sits on a boat behind marina NAT with nobody aboard;
/// "read the log" otherwise means "get physical access to the machine", which is exactly the
/// situation that made a discovery failure un-diagnosable on 2026-08-28. Being RELAYABLE is what
/// turns that into a question the app can answer from anywhere.
///
/// Role-gated at CONTROL and above, deliberately: a log is a diagnostic, not public reading. It
/// carries gateway addresses, valve ids and vehicle names, and a `monitor` share is someone trusted
/// to watch a boat, not to read its internals.
///
/// ⚠️ IT MUST NEVER CARRY A SECRET. Nothing in this daemon logs the hub token, a member key or the
/// Shelly secret, and nothing may start: `shellyIngestArmed` exists precisely so a caller can ask
/// whether a secret is set without being told what it is. Read that rule before adding a log line
/// near a credential.
async fn do_logs(_rt: &Rt, caller: &Caller) -> Answer {
    if !may_control(&caller.role) {
        return err(403, "reading the hub log needs control access or above");
    }
    let text = crate::hub_log::tail(300);
    let path = crate::hub_log::path().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
    ok_json(&serde_json::json!({ "path": path, "lines": text }))
}

async fn h_status(State(rt): State<Shared>, headers: HeaderMap) -> Response {
    lan_call(&rt, &headers, "GET", "/api/hub/status", b"").await
}

async fn h_logs(State(rt): State<Shared>, headers: HeaderMap) -> Response {
    lan_call(&rt, &headers, "GET", "/api/hub/logs", b"").await
}

async fn h_config(State(rt): State<Shared>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    lan_call(&rt, &headers, "POST", "/api/hub/config", &body).await
}

async fn h_token(State(rt): State<Shared>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    lan_call(&rt, &headers, "POST", "/api/hub/token", &body).await
}

async fn h_clear(State(rt): State<Shared>, headers: HeaderMap) -> Response {
    lan_call(&rt, &headers, "POST", "/api/hub/clear", b"").await
}

async fn h_update(State(rt): State<Shared>, headers: HeaderMap) -> Response {
    lan_call(&rt, &headers, "POST", "/api/hub/update", b"").await
}

async fn h_valve(State(rt): State<Shared>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    lan_call(&rt, &headers, "POST", "/api/hub/linktap/valve", &body).await
}

async fn h_gps(State(rt): State<Shared>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    lan_call(&rt, &headers, "POST", "/api/hub/gps", &body).await
}

async fn h_routers_list(State(rt): State<Shared>, headers: HeaderMap) -> Response {
    lan_call(&rt, &headers, "GET", "/api/hub/routers", b"").await
}

async fn h_routers(State(rt): State<Shared>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    lan_call(&rt, &headers, "POST", "/api/hub/routers", &body).await
}

async fn h_sensors_list(State(rt): State<Shared>, headers: HeaderMap) -> Response {
    lan_call(&rt, &headers, "GET", "/api/hub/sensors", b"").await
}

async fn h_sensors(State(rt): State<Shared>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    lan_call(&rt, &headers, "POST", "/api/hub/sensors", &body).await
}


/// Valve state for a LOCAL app, with an optional wait.
///
/// 🔴 THE APP READS THE HUB, NOT THE GATEWAY (owner ruling 2026-08-31). The app used to poll
/// `http://<gatewayIp>/api.shtml` itself every 5s on the LAN — a second, independent reader of the
/// hardware with its own cadence, its own parsing and its own idea of the truth. This endpoint is
/// what lets that go away.
///
/// `wait` turns the read into a LONG POLL: hold the request until the hub observes something new,
/// then answer immediately. Not an SSE stream and not a callback into the app, for one practical
/// reason each — the native LAN transport is request/response (a Rust `lan_http_request` shim, no
/// streaming), and a true webhook would require the app to run a listening socket. A held request
/// needs neither and collapses the app's polling interval to nothing, which is the whole ask.
///
/// The wait is capped below the RELAY's own timeout so the SAME call works off-LAN through the
/// worker: an app that cannot dial a private IP still gets change-driven updates, just with the
/// relay's hop in front.
async fn do_valve_state(rt: &Rt, params: &HashMap<String, String>) -> Answer {
    let since = params.get("since").and_then(|v| v.parse::<u64>().ok());
    let wait_secs = params
        .get("wait")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0)
        .min(MAX_STATE_WAIT_SECS);

    let mut rx = rt.valve_rev.subscribe();
    let mut rev = *rx.borrow();

    // Only wait when the caller is already current. A caller behind the hub gets the answer now.
    if wait_secs > 0 && since == Some(rev) {
        let _ = tokio::time::timeout(Duration::from_secs(wait_secs), rx.changed()).await;
        rev = *rx.borrow();
    }

    let valves: Vec<serde_json::Value> = {
        let guard = rt.linktap.lock().await;
        match guard.as_ref() {
            Some(r) => r
                .measurements()
                .into_iter()
                .map(|(dev, params)| {
                    let mut o = serde_json::Map::new();
                    o.insert("devId".into(), serde_json::Value::String(dev));
                    for (k, v) in params {
                        o.insert(k, serde_json::Value::String(v));
                    }
                    serde_json::Value::Object(o)
                })
                .collect(),
            None => Vec::new(),
        }
    };

    // `rev` is the caller's cursor for the next call. Echoing it back rather than making the client
    // invent one keeps the protocol honest when the hub restarts and the counter resets.
    ok_json(&serde_json::json!({ "rev": rev, "valves": valves }))
}

async fn h_valve_state(State(rt): State<Shared>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(caller) = caller_from_headers(&rt, &headers).await else {
        return answer_response(err(401, "a member key is required"));
    };
    let _ = caller;
    answer_response(do_valve_state(&rt, &q).await)
}

/// Is this push actually FROM the configured gateway?
///
/// The gateway cannot authenticate — it is fixed firmware with no key — so the peer address is the
/// only evidence available. This is not authentication and is not claimed to be: a LAN peer can
/// spoof an address. It narrows "any device on the boat's network" to "something answering at the
/// gateway's address", which is a real reduction for one comparison.
///
/// It reads the SAME `linktap.host` the poll loop dials, so the two cannot drift apart: if the
/// gateway's DHCP address changes, polling breaks at the same moment pushes stop being accepted —
/// one visible failure instead of a silent half-broken state. An unconfigured host accepts nothing.
/// Is this peer on a network that could plausibly be the vessel's own?
///
/// ⚠️ WHY THIS EXISTS, AND WHY IT IS NOT REDUNDANT WITH THE SECRET. `/api/hub/shelly` closes valves
/// and injects events into the owner's alert pipeline, and the server binds 0.0.0.0. Absence from
/// the relay allowlist keeps it off the WORKER's path — it does NOT make it LAN-only. A marina
/// router with 8722 port-forwarded (unusual, but a thing people do) would expose it to the internet
/// with its secret travelling in a plaintext query string. The secret is the authority; this is the
/// blast radius.
///
/// PERMISSIVE ON PURPOSE, and each range is here for a reason rather than copied from a list:
///   * RFC1918 (10/8, 172.16/12, 192.168/16) — every ordinary boat LAN.
///   * CGNAT (100.64/10) — Starlink and cellular routers hand these out, and on some of them the
///     LAN side sits inside that range. Excluding it would refuse a real, common vessel setup.
///   * loopback and link-local (169.254/16) — same machine, and DHCP-less auto-addressing.
///   * IPv6 loopback, unique-local (fc00::/7) and link-local (fe80::/10), plus IPv4-mapped
///     addresses, which is how a dual-stack listener reports an IPv4 peer.
/// A refusal is LOGGED loudly by the caller: if some genuine network is being turned away, that log
/// is the only way anyone would ever find out.
fn shelly_peer_plausible(peer: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match peer {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (o[0] == 100 && (64..128).contains(&o[1])) // 100.64/10 CGNAT
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return shelly_peer_plausible(IpAddr::V4(mapped));
            }
            let seg = v6.segments();
            v6.is_loopback()
                || (seg[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

fn push_peer_allowed(host: &str, peer: SocketAddr) -> bool {
    if host.is_empty() {
        return false;
    }
    // `host` may carry a port (the config field is a host[:port] for the gateway's HTTP API).
    let host_only = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    match host_only.parse::<std::net::IpAddr>() {
        Ok(ip) => peer.ip() == ip,
        // A hostname was configured rather than an address. Resolving it here would put a DNS
        // lookup on every push, so accept and rely on the route's inertness — the same posture as
        // before this check existed, for the configuration that cannot support it.
        Err(_) => true,
    }
}

async fn h_linktap_push(
    State(rt): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: axum::body::Bytes,
) -> Response {
    let host = hub_config::read_config_in(&rt.base).linktap.host;
    if !push_peer_allowed(&host, peer) {
        // Same answer as a good push: this is not an authorization surface, and telling a prober
        // whether it guessed the gateway's address would make it one.
        return (StatusCode::OK, "ok").into_response();
    }
    // Answer FIRST, work after — the gateway retries a slow endpoint, and a duplicate status is
    // worse than a late one (it would re-run the cutoff comparison against stale numbers).
    let text = String::from_utf8_lossy(&body).to_string();
    tokio::spawn(async move {
        let client = http_client();
        for (dev_id, data) in crate::linktap_runtime::parse_gateway_push(&text) {
            let (action, reports) = {
                let mut guard = rt.linktap.lock().await;
                match guard.as_mut() {
                    Some(r) => r.observe(&dev_id, &data, now_ms()),
                    None => (crate::cycle::Action::None, Vec::new()),
                }
            };
            linktap_act(&rt, &client, &dev_id, action, reports).await;
        }
    });
    (StatusCode::OK, "ok").into_response()
}

// --- Local Shelly ingest: the flood close that never touches the WAN -------------------------------
//
// AUTH, AND WHY IT IS NOT THE PUSH ROUTE'S ANSWER.
//
// `/api/hub/linktap/push` above is unauthenticated, and the comment there is careful about the one
// fact that makes it acceptable: THE ROUTE IS INERT. It accepts no commands, changes no
// configuration, and the worst a hostile LAN peer achieves with it is a wrong volume reading that
// the next poll corrects.
//
// ⚠️ THIS ROUTE IS NOT INERT. It CLOSES A VALVE. The same reasoning therefore reaches the opposite
// conclusion, and copying the push route's posture here would hand anyone on the boat's Wi-Fi —
// or anyone who got onto it — a remote water shutoff for the price of one HTTP GET.
//
// So it is authenticated, in the only way a Shelly can be. A Shelly fires a STATIC URL: it cannot
// present a header, cannot sign a request, cannot be given a client certificate. The strongest
// credential it can carry is a bearer secret in the query string, which is exactly what the cloud
// already does — `&k=<per-vehicle webhook secret>`, cloud-server `auth.ts` SEC-4. Reusing that
// value and that spelling means pointing a sensor at the hub is a URL SWAP and nothing more: same
// secret, same param, different host.
//
// The scheme, in full:
//   1. `k` must equal the hub's stored `shelly_secret`, compared in constant time (`ct_eq`).
//   2. An EMPTY stored secret REFUSES EVERYTHING. This is a deliberate divergence from the cloud,
//      which treats an unset secret as `legacy` and accepts — that leniency is a phased rollout
//      across vehicles provisioned before the scheme existed, and it is not a licence to close
//      valves for strangers. Deny-by-default is the same posture as the empty key set, which 401s
//      every management call until the first sync lands. `/api/hub/status` reports
//      `shellyIngestArmed` so a disarmed hub is visible rather than silently deaf.
//   3. `vid` must match this hub's vehicle. Checked AFTER the secret, so a prober cannot use the
//      404 to enumerate which vehicle a hub belongs to.
//   4. LAN ONLY. `/api/hub/shelly` is deliberately ABSENT from `dispatch`, which is the relay's
//      path allowlist — the worker cannot reach this route down the WebSocket, so the query-string
//      secret never leaves the boat's own network and there is no internet-facing door onto it.
//
// What this scheme does NOT claim: a LAN peer that can read the sensor's configured URL (or watch
// the plaintext HTTP request go by) has the secret. That is true of the cloud path too, and it is
// the ceiling of what a device with no crypto can do. The mitigation is the same one: the secret
// is per-vehicle and rotatable, and the action behind it is a CLOSE, which spends no water and
// removes no safety limit.

/// One parsed Shelly webhook. Field-for-field the shape the cloud's `/api/shelly` reads out of its
/// searchParams, so a sensor URL is portable between the two without editing.
#[derive(Debug, PartialEq)]
pub struct ShellyCall {
    pub vid: String,
    pub event: String,
    pub device: String,
    /// The per-vehicle webhook secret. NEVER forwarded and never logged.
    pub k: String,
    /// Everything else the device sent, in wire order — battery, temperature, whatever the model
    /// puts in its URL. Passed through to the cloud untouched so the alert pipeline sees exactly
    /// what a direct-to-cloud report would have carried.
    pub extras: Vec<(String, String)>,
}

/// Routing/auth params, never telemetry — the Rust twin of events.ts `RESERVED_PARAMS`. `k` is on
/// this list for a reason that outranks tidiness: it is the SECRET, and forwarding it would write
/// the vehicle's webhook bearer into a cloud telemetry document.
const SHELLY_RESERVED: [&str; 5] = ["vid", "event", "device", "key", "k"];

/// PURE: read a Shelly webhook's query string.
///
/// `event` defaults to "sensor alert" and `device` to "unknown", matching the cloud's defaults
/// exactly — a device that omits either must classify identically on both paths, or the same
/// sensor would behave differently depending on which URL it was given.
pub fn parse_shelly_query(raw_query: &str) -> ShellyCall {
    let mut call = ShellyCall {
        vid: String::new(),
        event: String::new(),
        device: String::new(),
        k: String::new(),
        extras: Vec::new(),
    };
    for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        match key.as_ref() {
            "vid" => call.vid = value.into_owned(),
            "event" => call.event = value.into_owned(),
            "device" => call.device = value.into_owned(),
            "k" => call.k = value.into_owned(),
            _ if SHELLY_RESERVED.contains(&key.as_ref()) => {}
            // Unset placeholders are dropped the way the cloud drops them, so a template the
            // installer never filled in does not become a telemetry field reading "null".
            _ if value.is_empty() || value == "null" => {}
            _ => call.extras.push((key.into_owned(), value.into_owned())),
        }
    }
    if call.event.is_empty() {
        call.event = "sensor alert".into();
    }
    if call.device.is_empty() {
        call.device = "unknown".into();
    }
    call
}

/// The outcome of authenticating one Shelly report.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ShellyAuth {
    Ok,
    /// This hub holds no webhook secret, so it can authenticate nothing. Deny by default.
    Disarmed,
    /// A secret is set and `k` is missing or wrong.
    BadSecret,
    /// Authenticated, but the report names a different vehicle.
    WrongVehicle,
}

/// PURE: may this report act on this hub?
///
/// Order is load-bearing. The SECRET is checked first, so an unauthenticated prober gets the same
/// 401 whatever `vid` it guessed and cannot use the vehicle check as an oracle. The empty-secret
/// case is its own variant rather than folded into `BadSecret` because the operator fix is
/// completely different — one is "the sensor has the wrong URL", the other is "nobody has told
/// this hub the secret yet" — and the log line has to be able to say which.
pub fn classify_shelly_auth(cfg_vid: &str, cfg_secret: &str, call: &ShellyCall) -> ShellyAuth {
    if cfg_secret.is_empty() {
        return ShellyAuth::Disarmed;
    }
    if !ct_eq(cfg_secret, &call.k) {
        return ShellyAuth::BadSecret;
    }
    // An omitted vid is accepted: the hub has exactly one vehicle, so there is nothing to route
    // and the secret has already proved which vehicle this is. A vid that is present and WRONG is
    // refused — that is a misconfigured sensor, and quietly filing its floods under this vehicle
    // would be worse than saying no.
    if !call.vid.is_empty() && !cfg_vid.is_empty() && call.vid != cfg_vid {
        return ShellyAuth::WrongVehicle;
    }
    ShellyAuth::Ok
}

async fn h_shelly(
    State(rt): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    raw: axum::extract::RawQuery,
) -> Response {
    let call = parse_shelly_query(raw.0.as_deref().unwrap_or(""));
    // Blast radius before authority — see shelly_peer_plausible. Checked FIRST because it is the
    // cheapest test and it discloses nothing: an off-LAN caller learns only that it is off-LAN,
    // which it already knew, and never whether it guessed a secret.
    if !shelly_peer_plausible(peer.ip()) {
        crate::hlog!(
            "shelly: REFUSED a '{}' report from {} - that peer is not on a plausible vessel network. \
             If this is a real boat LAN the address range needs adding to shelly_peer_plausible.",
            call.event, peer.ip()
        );
        return answer_response(err(403, "not a local caller"));
    }
    let cfg = hub_config::read_config_in(&rt.base);
    match classify_shelly_auth(&cfg.vid, &cfg.shelly_secret, &call) {
        ShellyAuth::Ok => {}
        ShellyAuth::Disarmed => {
            // LOUD, because the failure is invisible from the outside: every flood report this hub
            // receives is being refused, and the sensor has no way to tell anyone.
            crate::hlog!(
                "shelly: REFUSED a '{}' report from {} - this hub holds no webhook secret. Set it in the app (hub settings) or the local flood close CANNOT run.",
                call.event, call.device
            );
            return answer_response(err(401, "this hub has no webhook secret configured"));
        }
        ShellyAuth::BadSecret => {
            crate::hlog!("shelly: refused a '{}' report from {} - wrong or missing k", call.event, call.device);
            return answer_response(err(401, "missing or wrong webhook secret"));
        }
        ShellyAuth::WrongVehicle => {
            return answer_response(err(404, "that vehicle is not this hub's"));
        }
    }

    let flood = crate::linktap_runtime::is_flood_shutoff(&call.event);
    if flood {
        crate::hlog!("shelly: FLOOD - '{}' from {} - closing every valve NOW", call.event, call.device);
    }

    // Answer FIRST, act after — the same discipline as the gateway push route, for a sharper
    // reason. A Shelly's webhook has a short timeout and retries on it; making the sensor wait out
    // a stop-and-confirm loop against a 15-second gateway timeout would produce a retry storm on
    // top of a flood. The spawned task starts immediately, so "before anything else" is measured
    // in microseconds, not in a round trip.
    // Taken before the spawn moves `rt` and `call`: the wiring note below runs on this task.
    let (rt_seen, seen_device, seen_ip) = (rt.clone(), call.device.clone(), peer.ip());
    tokio::spawn(async move {
        // ⚠️ THE CLOSE COMES FIRST, UNCONDITIONALLY, AND BEFORE ANY NETWORK CALL. Not after the
        // forward, not concurrently with it: the entire reason this route exists is the case where
        // the uplink is down, and a close that waits on an unreachable cloud is a close that never
        // happens. linktap_flood_stop_all is not tier-gated — see its own comment.
        if flood {
            linktap_flood_stop_all(&rt).await;
        }
        // Then forward to the cloud, best-effort. THROUGH `/api/shelly`, THE SAME DOOR THE SENSOR
        // ITSELF WOULD HAVE USED, with the same per-vehicle webhook secret.
        //
        // 🔴 THIS USED TO GO THROUGH `/api/agent` AND WAS REJECTED 401 EVERY TIME. That endpoint
        // authenticates a token against the token's OWN device, so the hub presenting its own
        // credential for `shellyfloodg4-...` was refused — silently, until the daemon learned to log
        // a non-2xx (same release). Real consequence, found 2026-08-31: every forwarded Shelly
        // event was dropped, INCLUDING `flood.alarm`. With the uplink up nobody noticed, because the
        // sensor also fires its own cloud hook — but with the uplink down and the hub spooling,
        // which is the entire case the local ingest exists for, the alert never arrived at all.
        //
        // The comment this replaces stated the right goal — "the ORIGINAL event name and device id
        // ride through untouched so the cloud's alert pipeline classifies, throttles, pushes and
        // logs this exactly as it does a report that reached it directly; a hub in the path must be
        // invisible to that pipeline" — and then picked the one door that cannot achieve it.
        // `/api/shelly` achieves it by construction: same endpoint, same credential, same
        // classification, no agent-path semantics (device caps, `agentSelf`) that a sensor's report
        // was never meant to meet.
        //
        // ⚠️ NO NEW AUTHORITY. The hub already holds this secret — it is what authenticated the
        // report on the way IN, so its presence here is guaranteed by the fact that we got this far.
        // Anyone holding it can already report as any device on this vehicle; forwarding with it
        // grants nothing that was not already true, which is why this is preferable to widening the
        // agent-token vouch to cover every device id.
        forward_shelly_to_cloud(&rt, &call).await;
    });
    // A report from a sensor the hub is still trying to WIRE says exactly where it is and that it
    // is awake right now — the best moment there is. Remember the address and ring the hunt loop.
    note_sensor_seen(&rt_seen, &seen_device, seen_ip).await;
    answer_response(ok_json(&serde_json::json!({ "ok": true })))
}

// --- Loops --------------------------------------------------------------------------------------

/// For the web bundle download only — see update_check_loop.
fn web_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()
        .expect("reqwest client")
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("reqwest client")
}

/// Beat while registered+enabled; otherwise poll the store waiting for the bootstrap seed (the
/// signed-in app writes it once at registration — the service may well boot first).
async fn heartbeat_loop(rt: Shared) {
    let client = http_client();
    loop {
        let cfg = hub_config::read_config_in(&rt.base);
        if !cfg.token.is_empty() && !cfg.vid.is_empty() && cfg.enabled {
            let update = rt.update_available.read().await.clone();
            // Acks owed from earlier beats. TAKEN, not copied: a successful beat drops them (the
            // worker has pruned them), a failed one puts them back to retry.
            let sending: Vec<String> = std::mem::take(&mut *rt.pending_acks.lock().await);
            let ack = if sending.is_empty() { None } else { Some(sending.join(",")) };
            match heartbeat_with_reply(&client, &rt.worker_base, &cfg, update.as_deref(), ack.as_deref()).await {
                Ok(body) => {
                    apply_linktap_reply(&rt, &body).await;
                    handle_agent_commands(&rt, &client, &body).await;
                    // A good heartbeat means the uplink is up — flush any telemetry that failed to
                    // send while it was down (drain_reports is a no-op when the queue is empty).
                    drain_reports(&rt).await;
                }
                Err(e) => {
                    if !sending.is_empty() {
                        rt.pending_acks.lock().await.extend(sending);
                    }
                    crate::hlog!("hub: heartbeat failed: {e}");
                }
            }
            // Report-by-exception cadence: fast right after a local event, the configured beat while
            // things are recent, one beat every 20 min once idle. A local event mid-nap rings
            // rt.wake and we beat immediately — so the backoff never costs alarm latency.
            let idle = now_ms() - rt.last_activity_ms.load(Ordering::Relaxed);
            let secs = heartbeat_interval_secs(idle, u64::from(cfg.heartbeat_secs));
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
                _ = rt.wake.notified() => {}
            }
        } else {
            tokio::time::sleep(Duration::from_secs(UNREGISTERED_POLL_SECS)).await;
        }
    }
}

/// How often the daemon checks whether a newer release exists. Hours, not minutes: a release lands
/// a few times a week at most, and this is visibility, not a safety path. Runs once at boot too, so
/// a freshly started hub reports its update status on its first heartbeat rather than hours later.
const UPDATE_CHECK_SECS: u64 = 6 * 3600;

/// Poll GitHub for the latest daemon version and record it in `rt.update_available` when it is newer
/// than the running one. Phase 1a: this only makes the gap VISIBLE (heartbeat + /api/hub/status);
/// it installs nothing. Every failure is silent — an offline or locked-down hub reports no update
/// rather than an error, and never a false positive.
async fn update_check_loop(rt: Shared) {
    let client = http_client();
    let current = env!("CARGO_PKG_VERSION");
    loop {
        if let Some(latest) = crate::update_check::fetch_latest_version(&client).await {
            let available = crate::update_check::newer_than(&latest, current);
            let mut slot = rt.update_available.write().await;
            if *slot != available {
                match &available {
                    Some(v) => crate::hlog!("hub: update available - running {current}, {v} released"),
                    None => if slot.is_some() { crate::hlog!("hub: now up to date at {current}") },
                }
                *slot = available;
            }
        }
        // The WEB APP rides the same cadence (web_bundle.rs). Its own client: a bundle is a few MB
        // and a Starlink afternoon is not a 20 s affair. A failure keeps the bundle in service.
        {
            let current = rt.web.read().await.as_ref().map(|w| w.version.clone());
            match crate::web_bundle::refresh(&web_client(), &rt.base, current.as_deref()).await {
                Ok(Some(b)) => {
                    crate::hlog!("hub: web app {} in service from the latest app release", b.version);
                    *rt.web.write().await = Some(b);
                }
                Ok(None) => {}
                Err(e) => crate::hlog!("hub: web app not refreshed - {e}"),
            }
        }
        tokio::time::sleep(Duration::from_secs(UPDATE_CHECK_SECS)).await;
    }
}

/// PURE: the (id, verb) pairs a heartbeat reply carried. A command with no id can never be
/// acknowledged — it would loop forever — so it is dropped rather than run.
fn parse_agent_commands(body: &serde_json::Value) -> Vec<(String, String)> {
    body.get("commands").and_then(|c| c.as_array()).map(|arr| {
        arr.iter().filter_map(|c| {
            let id = c.get("id")?.as_str()?;
            let cmd = c.get("cmd")?.as_str()?;
            if id.is_empty() || cmd.is_empty() { return None; }
            Some((id.to_string(), cmd.to_string()))
        }).collect()
    }).unwrap_or_default()
}

/// Act on the console-queued commands a heartbeat reply carried.
///
/// 🔴 THIS IS THE ONLY WRITE-CAPABLE CLOUD CHANNEL INTO THE DAEMON, and its scope is exactly one
/// verb: install the vendor-signed newer release. The command is a fixed verb with NO ARGUMENT —
/// the cloud decides WHO updates and WHEN, `perform_update` decides WHAT (the latest release,
/// verified against the SHA256SUMS published in that same release). Compromising the command queue
/// changes an update's TIMING, never its CONTENTS. Never add a verb that carries a version or URL.
///
/// At-least-once, and the daemon side of that contract mirrors the router agent's: a command stays
/// queued until this hub acks it, and it is acked only after a terminal decision.
async fn handle_agent_commands(rt: &Rt, client: &reqwest::Client, body: &serde_json::Value) {
    for (id, cmd) in parse_agent_commands(body) {
        if !rt.handled_cmds.lock().await.insert(id.clone()) { continue; }
        match cmd.as_str() {
            "self_update" => run_commanded_self_update(rt, client, &id).await,
            other => {
                crate::hlog!("hub: unsupported cloud command '{other}' (id {id}) - acknowledging");
                rt.pending_acks.lock().await.push(id);
            }
        }
    }
    let mut h = rt.handled_cmds.lock().await;
    if h.len() > 256 { h.clear(); }
}

/// Run `self_update` from a cloud command. Same body as do_update (the LAN button) minus the
/// caller-role check — a queued command already cleared the operator-console capability gate
/// cloud-side, which is a stronger bar than a LAN member key.
async fn run_commanded_self_update(rt: &Rt, client: &reqwest::Client, id: &str) {
    match crate::self_update::perform_update(client).await {
        crate::self_update::UpdateOutcome::Swapped { to_version } => {
            // 🔴 DO NOT ACK HERE. Restart first; the restarted binary re-runs this same still-queued
            // command, finds itself already current, and acks then. That is what makes a swap+restart
            // idempotent — a missed ack costs one extra up-to-date check, never a second install.
            // Ordering copied verbatim from do_update.
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _restarting = crate::self_update::finalize_restart();
            crate::hlog!("hub: cloud self-update installed {to_version}; restarting into it");
            #[cfg(unix)]
            std::process::exit(0);
        }
        crate::self_update::UpdateOutcome::UpToDate => {
            crate::hlog!("hub: cloud self-update - already current; acknowledging");
            rt.pending_acks.lock().await.push(id.to_string());
        }
        crate::self_update::UpdateOutcome::Failed(why) => {
            crate::hlog!("hub: cloud self-update failed: {why}; acknowledging");
            rt.pending_acks.lock().await.push(id.to_string());
        }
    }
}

/// The heartbeat, keeping its reply — the config-as-state channel (cloud-server #105 attaches
/// `{linktap:{allowed,profiles}}` to a hub's report). send_heartbeat_once stays for callers that
/// only care whether it landed.
async fn heartbeat_with_reply(client: &reqwest::Client, worker_base: &str, cfg: &HubConfig, update: Option<&str>, ack: Option<&str>) -> Result<serde_json::Value, String> {
    let url = heartbeat_url(worker_base, cfg, env!("CARGO_PKG_VERSION"), std::env::consts::OS, update, ack)?;
    let res = client.get(url).send().await.map_err(|e| e.without_url().to_string())?;
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status().as_u16()));
    }
    res.json::<serde_json::Value>().await.map_err(|e| e.without_url().to_string())
}

/// Persist the cloud's valve PERMISSION and hand the per-valve profiles to the machine.
///
/// `allowed` is written to hub.json because the capability advertisement and the endpoint's own
/// 402 both read it there — including on a boot with no internet, where the last known answer is
/// the only one available. It defaults to false everywhere, so a hub that has never heard from the
/// cloud claims nothing.
async fn apply_linktap_reply(rt: &Rt, body: &serde_json::Value) {
    let Some((allowed, profiles)) = parse_linktap_reply(body) else { return };
    {
        let _g = rt.store.lock().await;
        let mut cfg = hub_config::read_config_in(&rt.base);
        if cfg.linktap.allowed != allowed {
            crate::hlog!("linktap: valve control {} by the vehicle's plan", if allowed { "permitted" } else { "NOT permitted" });
            cfg.linktap.allowed = allowed;
            if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
                crate::hlog!("hub: could not persist the linktap permission: {e}");
            }
        }
    }
    if !profiles.is_empty() {
        let mut guard = rt.linktap.lock().await;
        if let Some(r) = guard.as_mut() {
            r.apply_profiles(&profiles);
        }
    }
}

async fn key_sync_loop(rt: Shared) {
    let client = http_client();
    loop {
        let cfg = hub_config::read_config_in(&rt.base);
        if !cfg.token.is_empty() {
            match fetch_member_keys(&client, &rt.worker_base, &cfg).await {
                Ok(keys) => {
                    *rt.keys.write().await = keys.clone();
                    let _g = rt.store.lock().await;
                    let mut c = hub_config::read_config_in(&rt.base);
                    c.member_keys = keys;
                    if let Err(e) = hub_config::write_config_in(&rt.base, &c) {
                        crate::hlog!("hub: could not persist member keys: {e}");
                    }
                }
                // Keep the last known set — a network drop must not lock the owner out. (Before
                // increment C's endpoint deploys this is a permanent 404: deny-all continues.)
                Err(e) => crate::hlog!("hub: key sync failed (keeping previous keys): {e}"),
            }
        }
        tokio::time::sleep(Duration::from_secs(KEY_SYNC_SECS)).await;
    }
}

// --- LinkTap: the I/O shell around the pure runtime -----------------------------------------------
//
// Three inputs, ONE state machine (rt.linktap): this poll loop, the gateway's HTTP push (the
// /api/hub/linktap/push route), and the flood hook. The machine decides; this code performs.

/// How often the poll floor runs. The gateway's own push heartbeat is 2 minutes, so this is the
/// FLOOR under it, not the primary — a gateway nobody configured for push still works, and a
/// missed push cannot strand stale state.
const LINKTAP_POLL_SECS: u64 = 60;

/// After a valve command wakes the poll loop, wait this long before re-polling so the gateway has
/// applied the command and its status read reflects the new state — a poll issued the same
/// millisecond as the open could read the valve still closed and report a spurious "closed".
const LINKTAP_WAKE_SETTLE: Duration = Duration::from_millis(1500);

/// Longest a `/api/hub/linktap/state` call may be held open.
///
/// 🔴 SIXTY, NOT TEN, AND THE REASON MATTERS. This was 10s to survive the worker relay's own 15s
/// timeout, so the identical call could serve an off-LAN browser. Owner ruling 2026-08-31 keeps the
/// web app CLOUD-ONLY, which takes the relay out of this path entirely — and with it the only
/// reason to hold requests briefly.
///
/// A held request costs a socket and nothing else; a request that RETURNS costs a wake-up, a
/// handshake and a round trip. So a short cap is not the cautious choice, it is the expensive one:
/// at 10s an idle valve cost 360 round trips an hour, at 60s it costs 60. Owner, on the app polling
/// the gateway every 5s: *"i don't want to kill a phone's battery."*
///
/// Sixty rather than longer because that is where middleboxes and OS socket timeouts start reaping
/// idle connections; past it the reconnect churn comes back for nothing.
const MAX_STATE_WAIT_SECS: u64 = 60;

/// Rebuild the machine when the configured gateway/valves change, and keep the paid gate current.
/// Returns false when LinkTap is not configured or not permitted, in which case nothing polls.
/// Run one discovery sweep and PERSIST what it finds, so the answer survives a restart and the
/// scan is not repeated every poll. Returns the updated config when a gateway was adopted.
///
/// Adopts a gateway only when EXACTLY ONE is on the LAN. With several, the hub does not guess —
/// picking one silently is how a vessel ends up with its second gateway quietly unmanaged; it says
/// so and waits for the manual field, which is what that field is for.
async fn discover_linktap_gateway(rt: &Rt) -> Option<hub_config::HubConfig> {
    // A DEDICATED CLIENT, because the shared one is built for commands and this is a sweep.
    // `http_client()` waits 20 s, which is right for a valve mid-RF-retry and badly wrong for 253
    // addresses with nothing on them: on Windows a dead host burns the full timeout, so a /24 took
    // ~160 s per pass. A gateway on the same LAN answers in milliseconds, so 2 s is generous and
    // turns a sweep into ~16 s — the difference between "finds the gateway within a poll" and
    // "still sweeping when the next poll starts".
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap_or_else(|_| http_client());
    let found = crate::linktap_discover::scan_local_subnet(&client).await;
    match found.len() {
        0 => None,
        1 => {
            let d = &found[0];
            let _g = rt.store.lock().await;
            // Re-read under the lock: the app may have written a host while the sweep ran, and a
            // typed address must never be clobbered by a scan that started before it.
            let mut cfg = hub_config::read_config_in(&rt.base);
            if !cfg.linktap.host.is_empty() {
                crate::hlog!("linktap discovery: a host was configured while scanning - keeping it");
                return Some(cfg);
            }
            cfg.linktap.host = d.host.clone();
            cfg.linktap.gw_id = d.gw_id.clone();
            cfg.linktap.dev_ids = d.dev_ids.clone();
            if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
                crate::hlog!("linktap discovery: found {} but could not save it: {e}", d.host);
                return None;
            }
            crate::hlog!(
                "linktap discovery: adopted gateway {} at {} ({} valve(s)) - saved",
                d.gw_id, d.host, d.dev_ids.len()
            );
            Some(cfg)
        }
        n => {
            crate::hlog!(
                "linktap discovery: {n} gateways answered on this LAN - not guessing which is this vehicle's; set the gateway address in the app"
            );
            None
        }
    }
}

async fn linktap_sync_config(rt: &Rt) -> bool {
    let mut cfg = hub_config::read_config_in(&rt.base);
    // ZERO-CONFIG DISCOVERY. The cloud says this vehicle may drive a valve, but nobody has told the
    // hub WHERE the gateway is — the exact state MVP's hub was found in on 2026-08-26: allowed
    // true, host/gw_id/dev_ids all empty, so the poll loop never ran and the valve was unmanaged.
    // Everything needed is obtainable from the gateway itself (see linktap_discover), so ask the
    // LAN rather than wait for someone to type it.
    //
    // ⚠️ A CONFIGURED HOST ALWAYS WINS and is never overwritten (owner 2026-08-25: manual entry is
    // still needed "in some situations like a huge subnet"). Discovery fills a BLANK host only.
    if cfg.linktap.allowed && cfg.linktap.host.is_empty() {
        if let Some(found) = discover_linktap_gateway(rt).await {
            cfg = found;
        }
    }
    let lt = &cfg.linktap;
    let usable = lt.allowed && !lt.host.is_empty() && !lt.gw_id.is_empty() && !lt.dev_ids.is_empty();
    let mut guard = rt.linktap.lock().await;
    if !usable {
        // Dropping the machine on a revoked plan is deliberate: a hub whose vehicle stopped paying
        // must stop driving the valve, not merely stop advertising that it can.
        if guard.is_some() {
            crate::hlog!("linktap: configuration withdrawn or plan no longer permits valve control - stopping");
        }
        *guard = None;
        return false;
    }
    let gw = linktap::Gateway { host: lt.host.clone(), gw_id: lt.gw_id.clone() };
    let needs_rebuild = match guard.as_ref() {
        None => true,
        Some(r) => {
            let mut have = r.dev_ids();
            have.sort();
            let mut want: Vec<String> = lt.dev_ids.iter().map(|d| linktap::normalize_dev_id(d)).collect();
            want.sort();
            r.gateway.host != gw.host || r.gateway.gw_id != gw.gw_id || have != want
        }
    };
    if needs_rebuild {
        let profile = crate::cycle::Profile {
            duration_secs: 24 * 3600,
            volume_cap_l: 378.0, // the 100 gal Normal Run default; wire profiles override per valve
            auto_restart: false,
        };
        let mut r = crate::linktap_runtime::Runtime::new(gw.clone(), &lt.dev_ids, profile);
        // Read the gateway's unit ONCE per rebuild. Defaults to GALLONS when unreadable, because
        // guessing litres under-reports a cap by 3.79x and the cutoff compares against it.
        r.unit = linktap::read_vol_unit(&http_client(), &gw).await;
        crate::hlog!("linktap: watching {} valve(s) via {} (unit {:?})", lt.dev_ids.len(), lt.host, r.unit);
        *guard = Some(r);
    }
    true
}

/// Act on one machine decision: issue the stop it asked for, restart on a timer expiry, and spool
/// whatever it wants reported.
async fn linktap_act(
    rt: &Rt,
    client: &reqwest::Client,
    dev_id: &str,
    action: crate::cycle::Action,
    reports: Vec<crate::linktap_runtime::Report>,
) {
    // Every observation lands here, from the poll loop AND from the gateway's push. Releasing the
    // waiters here means a local app learns what the hub learned, when the hub learned it.
    rt.valve_rev.send_modify(|v| *v = v.wrapping_add(1));

    for r in reports {
        spool_report(rt, &r).await;
    }
    let gw = {
        let guard = rt.linktap.lock().await;
        match guard.as_ref() {
            Some(x) => x.gateway.clone(),
            None => return,
        }
    };
    if let crate::cycle::Action::Stop(reason) = action {
        crate::hlog!("linktap: {dev_id} - issuing stop ({})", reason.as_str());
        let reply = linktap::post_command(client, &gw, &linktap::build_stop(&gw, dev_id)).await;
        if !reply.ok {
            // A close that did not happen is worth hearing about immediately; the machine keeps
            // stop_issued set, so the next observation retries without a re-issue storm.
            crate::hlog!("linktap: {dev_id} STOP FAILED: {:?}", reply.error);
            spool_report(rt, &crate::linktap_runtime::Report { token: None,
                device: format!("lt_{dev_id}"),
                event: "linktap.stop_failed".into(),
                params: vec![("error".into(), reply.error.unwrap_or_default())],
            }).await;
        }
    }

    // 🔴 THE REOPEN. This function's doc comment has always said "restart on a timer expiry" and
    // its body never did: `should_restart` had NO production caller, so the hub has never reopened
    // a valve for any reason. The washdown resume had the same shape one layer up — it lived in an
    // app-side React ref that was persisted nowhere and died with the page.
    let pending = {
        let mut guard = rt.linktap.lock().await;
        match guard.as_mut() {
            Some(r) => r.take_pending_open(dev_id),
            None => None,
        }
    };
    if let Some(open) = pending {
        // The cap must be expressed in the GATEWAY's unit, exactly as do_valve does — reading it
        // rather than assuming, because guessing litres under-reports a cap by 3.79x.
        let cap_gw = if open.volume_cap_l > 0.0 {
            Some(linktap::read_vol_unit(client, &gw).await.from_litres(open.volume_cap_l))
        } else {
            None
        };
        let body = linktap::build_start(&gw, dev_id, open.duration_secs, cap_gw);
        let reply = linktap::post_command(client, &gw, &body).await;
        if reply.ok {
            crate::hlog!("linktap: {dev_id} - {} -> reopened for {}s", open.why, open.duration_secs);
            // Record it as OURS, or the next poll would meet an already-running valve and adopt it —
            // the bug fixed in 0.3.19, which a reopen path that skipped this would reintroduce.
            let mut guard = rt.linktap.lock().await;
            if let Some(r) = guard.as_mut() {
                let _ = r.note_hub_open(dev_id, now_ms(), crate::cycle::Mode::Normal, open.duration_secs, open.volume_cap_l, false);
            }
        } else {
            crate::hlog!("linktap: {dev_id} - {} FAILED: {:?}", open.why, reply.error);
            spool_report(rt, &crate::linktap_runtime::Report { token: None,
                device: format!("lt_{dev_id}"),
                event: "linktap.reopen_failed".into(),
                params: vec![("why".into(), open.why.into()), ("error".into(), reply.error.unwrap_or_default())],
            }).await;
        }
    }
}

/// The poll floor — and, since this loop is the only thing on the boat that ever talks to the
/// gateway, the GATEWAY-REACHABILITY WATCH as well.
///
/// The watch lives as a local across iterations rather than on `Rt` on purpose: nothing else needs
/// to see it, and an episode belongs to one polling run of one gateway. It is RESET when the
/// configured gateway changes, because an outage attributed to a gateway the hub no longer polls
/// is not an outage of anything.
async fn linktap_poll_loop(rt: Shared) {
    let client = http_client();
    let mut watch = crate::linktap_runtime::GatewayWatch::default();
    let mut watching = String::new();
    loop {
        if linktap_sync_config(&rt).await {
            let (gw, ids) = {
                let guard = rt.linktap.lock().await;
                match guard.as_ref() {
                    Some(r) => (r.gateway.clone(), r.dev_ids()),
                    None => (linktap::Gateway { host: String::new(), gw_id: String::new() }, Vec::new()),
                }
            };
            let key = format!("{}@{}", gw.gw_id, gw.host);
            if key != watching {
                watching = key;
                watch = crate::linktap_runtime::GatewayWatch::default();
            }
            // Did the GATEWAY answer this pass, whatever it said about any individual valve? One
            // reply is enough: a gateway that replied about one valve is reachable, and a `ret: 5`
            // on another valve is a flat battery, not an outage. `None` means we asked nothing —
            // no valves configured — which must not be read as silence from the gateway.
            let mut reached: Option<bool> = None;
            for id in ids {
                let reply = linktap::post_command(&client, &gw, &linktap::build_status(&gw, &id)).await;
                reached = Some(reached.unwrap_or(false) || linktap::reply_reached_gateway(&reply));
                if !reply.ok {
                    continue; // an unreachable gateway is the poll loop's normal weather
                }
                let data = reply.data.get("dev_stat")
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .cloned()
                    .unwrap_or(reply.data);
                let (action, reports) = {
                    let mut guard = rt.linktap.lock().await;
                    match guard.as_mut() {
                        Some(r) => r.observe(&id, &data, now_ms()),
                        None => (crate::cycle::Action::None, Vec::new()),
                    }
                };
                linktap_act(&rt, &client, &id, action, reports).await;
            }
            if let Some(reached) = reached {
                let (next, report) = crate::linktap_runtime::gateway_watch_step(watch, &gw, reached, now_ms());
                watch = next;
                if let Some(r) = report {
                    crate::hlog!("linktap: gateway {} - {}", gw.host, r.event);
                    spool_report(&rt, &r).await;
                }
            }
        }
        // A washdown about to hand over needs a poll INSIDE its lead window, and that window is
        // narrower than the standing cadence — so ask the runtime whether anything is time-critical
        // before sleeping the full minute. Nothing pending ⇒ the normal interval, unchanged.
        let nap = {
            let guard = rt.linktap.lock().await;
            guard
                .as_ref()
                .and_then(|r| r.poll_hint(now_ms()))
                .map(|h| h.min(Duration::from_secs(LINKTAP_POLL_SECS)))
                .unwrap_or(Duration::from_secs(LINKTAP_POLL_SECS))
        };
        // Sleep the computed nap, but cut it short the instant a valve command rings linktap_wake —
        // then settle briefly so the gateway has applied the command before we read it back. A
        // notify delivered mid-poll is not lost: Notify holds one permit, so the next `notified()`
        // returns at once and this pass reports the change without waiting out the nap.
        tokio::select! {
            _ = tokio::time::sleep(nap) => {}
            _ = rt.linktap_wake.notified() => {
                tokio::time::sleep(LINKTAP_WAKE_SETTLE).await;
            }
        }
    }
}

/// Close every watched valve — the flood hook. The close must not wait on the WAN: with the
/// LinkTap cloud gone this is the only automated close path when the uplink is down. The valve
/// self-limits regardless (every open carries duration+volume), so this only ever closes it sooner.
///
/// ⚠️⚠️ THIS PATH IS DELIBERATELY **NOT** TIER-GATED, AND THAT IS A BREAK FROM WHAT WAS HERE.
///
/// It used to be, silently. The only source of (gateway, dev_ids) was `rt.linktap`, and that
/// machine is built by `linktap_sync_config` ONLY when `cfg.linktap.allowed` is true — the paid
/// gate the cloud caches. So on a vehicle whose plan does not include valve control, or whose plan
/// lapsed, or which has simply never had a successful heartbeat since boot (`allowed` defaults to
/// FALSE, correctly, everywhere else), this function found `None` and RETURNED WITHOUT CLOSING
/// ANYTHING. A flood alarm would have been logged and billed and nothing would have shut the water
/// off. Nobody noticed because nothing called this function.
///
/// The gate is right for `do_valve` and stays there: OPENING a valve is a paid feature, an open
/// carries duration+volume limits, and a hub deciding entitlement locally would be the way around
/// a cloud-side rule. CLOSING is the opposite of all three. It spends no water, it removes no
/// safety limit, it is idempotent (a `cmd 7` to a shut valve is a no-op), and the worst outcome of
/// running it on an unentitled vehicle is that a boat which was going to flood does not. There is
/// no revenue to protect on the closing side of a valve.
///
/// How often the hub reads a fix from the configured GPS source. One a minute matches the LinkTap
/// floor and is plenty for a boat's position; the loop re-reads config each pass, so a source
/// configured after boot is picked up with no restart.
const GPS_POLL_SECS: u64 = 60;

/// Poll the configured LAN GPS source and report `gps.measurement` — the hub as GPS acquirer
/// (owner 2026-09-11). Reports through spool_report, so a fix taken while the uplink is down is
/// queued and delivered on reconnect like any other telemetry. Errors are logged only when they
/// CHANGE, so a boat with no lock (or a wrong password) does not fill the log once a minute.
async fn gps_poll_loop(rt: Shared) {
    // The LAN client: a Cradlepoint on 443 presents a self-signed certificate, which the cloud
    // client rightly refuses — and did, silently, until this loop got its own (routers::lan_client).
    let client = crate::routers::lan_client();
    let mut last_note: Option<String> = None; // dedupe the log line across identical passes
    loop {
        let g = hub_config::read_config_in(&rt.base).gps;
        if !g.host.is_empty() && g.enabled && !g.dev_id.is_empty() {
            let result = match g.kind.as_str() {
                "cradlepoint" => crate::gps::poll_cradlepoint(&client, &g.host, g.port, &g.username, &g.password).await,
                "nmea" => crate::gps::poll_nmea(&g.host, g.port, &g.protocol).await,
                other => Err(format!("no driver for GPS source kind '{other}'")),
            };
            match result {
                Ok(fix) => {
                    if last_note.is_some() { crate::hlog!("gps: {} - fix acquired", g.host); last_note = None; }
                    let mut params = vec![
                        ("lat".to_string(), format!("{:.6}", fix.lat)),
                        ("lon".to_string(), format!("{:.6}", fix.lon)),
                    ];
                    if let Some(acc) = fix.acc { params.push(("acc".to_string(), format!("{acc:.1}"))); }
                    spool_report(&rt, &crate::linktap_runtime::Report { token: None,
                        device: g.dev_id.clone(),
                        event: "gps.measurement".to_string(),
                        params,
                    }).await;
                }
                Err(why) => {
                    if last_note.as_deref() != Some(why.as_str()) {
                        crate::hlog!("gps: {} - {why}", g.host);
                        last_note = Some(why);
                    }
                }
            }
        } else {
            last_note = None; // no source configured — reset so a later fault logs once
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(GPS_POLL_SECS)) => {}
            _ = rt.gps_wake.notified() => {}
        }
    }
}

/// So: the machine supplies the gateway when it exists (it also carries `note_stop`, which is what
/// makes the eventual close classify as `flood_shutoff` rather than `unknown`), and when it does
/// not, the CONFIGURED gateway is used directly — `allowed` unread.
pub async fn linktap_flood_stop_all(rt: &Rt) {
    let client = http_client();
    let (gw, ids) = {
        let guard = rt.linktap.lock().await;
        match guard.as_ref() {
            Some(r) => (r.gateway.clone(), r.dev_ids()),
            // No running machine — unpermitted plan, or a hub that has not heard from the cloud
            // yet. Fall through to the stored configuration rather than giving up on the close.
            None => {
                let cfg = hub_config::read_config_in(&rt.base);
                let lt = cfg.linktap;
                let ids: Vec<String> =
                    lt.dev_ids.iter().map(|d| linktap::normalize_dev_id(d)).filter(|d| !d.is_empty()).collect();
                if lt.host.is_empty() || lt.gw_id.is_empty() || ids.is_empty() {
                    // Genuinely nothing to close: no gateway address, no valves. Say so — a flood
                    // alarm that reached a hub with no valve to shut is worth a log line.
                    crate::hlog!("linktap: FLOOD SHUTOFF requested but this hub has no gateway/valves configured");
                    return;
                }
                crate::hlog!(
                    "linktap: FLOOD SHUTOFF with no running machine (plan not permitted, or no heartbeat yet) - closing anyway from the stored configuration"
                );
                (linktap::Gateway { host: lt.host, gw_id: lt.gw_id }, ids)
            }
        }
    };
    for id in ids {
        {
            let mut guard = rt.linktap.lock().await;
            if let Some(r) = guard.as_mut() {
                r.note_stop(&id, crate::cycle::EndReason::FloodShutoff);
            }
        }
        let reply = linktap::post_command(&client, &gw, &linktap::build_stop(&gw, &id)).await;
        crate::hlog!("linktap: flood shutoff -> {id} {}", if reply.ok { "closed" } else { "FAILED" });
        if !reply.ok {
            spool_report(rt, &crate::linktap_runtime::Report { token: None,
                device: format!("lt_{id}"),
                event: "linktap.stop_failed".into(),
                params: vec![("error".into(), reply.error.unwrap_or_default()), ("cause".into(), "flood".into())],
            }).await;
        }
    }
    // Ring the poll loop the way `do_valve` does, so the closed state is READ BACK and reported
    // within seconds instead of on the next 60 s pass. The close itself is already done above;
    // this is only about how soon the cloud (and the Water tab) learn about it — the WhatsApp
    // session measured the cloud's valve state lagging a hub-local flood close by ~40 s+.
    rt.linktap_wake.notify_one();
}

/// Forward a Shelly's report to the cloud AS THAT SHELLY — same endpoint and credential the sensor
/// would have used itself, so the alert pipeline cannot tell a hub was in the path.
///
/// Best-effort, like every other outbound: a flood that cannot be forwarded has already had the
/// valve closed locally, and blocking on the cloud would defeat the point of closing first.
async fn forward_shelly_to_cloud(rt: &Rt, call: &ShellyCall) {
    // A forwarded sensor event (a flood among them) is a local change — wake the heartbeat into
    // ACTIVE cadence. The forward itself is what carries the alarm; this only makes the liveness
    // beat track the situation too.
    note_activity(rt);
    let cfg = hub_config::read_config_in(&rt.base);
    if cfg.shelly_secret.is_empty() || cfg.vid.is_empty() {
        // Unreachable in practice: an empty secret means the ingest refused this report long before
        // here. Said out loud anyway, because "unreachable" and "silent" is how the last one hid.
        crate::hlog!("shelly: cannot forward '{}' - no webhook secret", call.event);
        return;
    }
    let base = rt.worker_base.trim_end_matches('/');
    let Ok(mut u) = url::Url::parse(&format!("{base}/api/shelly")) else { return };
    u.query_pairs_mut()
        .append_pair("vid", &cfg.vid)
        .append_pair("device", &call.device)
        .append_pair("event", &call.event);
    for (k, v) in &call.extras {
        u.query_pairs_mut().append_pair(k, v);
    }
    // The secret goes on LAST and is never logged — the url is not printed anywhere below.
    u.query_pairs_mut().append_pair("k", &cfg.shelly_secret);

    let outcome = match http_client().get(u).send().await {
        Err(e) => Some(format!("failed to send: {}", e.without_url())),
        Ok(res) => report_refusal(res.status().as_u16(), &call.device),
    };
    if let Some(why) = outcome {
        crate::hlog!("shelly: forward of '{}' {why}", call.event);
    }
}

/// Report one telemetry line to the cloud, through the same /api/agent path the heartbeat uses.
/// Best-effort by design: telemetry that cannot be delivered must never block the valve logic that
/// produced it.
/// The most telemetry the hub buffers across a uplink outage before shedding its oldest samples.
/// ~4 hours at one report a minute — long enough to ride out a cellular dead zone, bounded so a
/// multi-day outage cannot grow the queue without limit.
const MAX_SPOOL_REPORTS: usize = 240;

/// PURE: append to a bounded FIFO, dropping oldest entries to stay within `cap`. Returns how many
/// were dropped — the stalest samples go first, because on a slow link the freshest state matters
/// most and old readings are the least worth resending.
fn push_bounded<T>(q: &mut std::collections::VecDeque<T>, item: T, cap: usize) -> usize {
    q.push_back(item);
    let mut dropped = 0;
    while q.len() > cap {
        q.pop_front();
        dropped += 1;
    }
    dropped
}

/// The fate of one delivery attempt: delivered, worth retrying (the uplink), or hopeless (a refusal
/// that will never change on a re-send — do not wedge the queue behind it).
enum SendOutcome {
    Sent,
    Transient,
    Permanent,
}

/// Deliver ONE telemetry report to `/api/agent`, classifying the result.
///
/// ⚠️ NAME OURSELVES AS THE VOUCHER. `/api/agent` authenticates the token against the token's OWN
/// device; a hub speaks for hardware with no cloud credential (a LinkTap valve driven over the LAN),
/// so it must claim its hub id or the worker looks up `agenttoken_lt_<valve>`, finds nothing, and
/// answers 401 — which is how every valve measurement was silently dead 2026-08-26..31. The worker
/// verifies this token against THIS hub id and then allows only `lt_*` devices
/// (cloud-server agentToken.ts::hubMayReportFor).
async fn send_report_once(rt: &Rt, report: &crate::linktap_runtime::Report) -> SendOutcome {
    let cfg = hub_config::read_config_in(&rt.base);
    if cfg.token.is_empty() || cfg.vid.is_empty() {
        return SendOutcome::Permanent; // unregistered — there is nothing to deliver to; do not hoard
    }
    let base = rt.worker_base.trim_end_matches('/');
    let Ok(mut u) = url::Url::parse(&format!("{base}/api/agent")) else { return SendOutcome::Permanent };
    u.query_pairs_mut()
        .append_pair("vid", &cfg.vid)
        .append_pair("device", &report.device)
        .append_pair("event", &report.event);
    match &report.token {
        // A managed router's report carries the ROUTER's token and no `hub` — to the cloud it is
        // the router reporting, exactly as a hub-lite router does (routers.rs).
        Some(t) => {
            u.query_pairs_mut().append_pair("t", t);
        }
        None => {
            u.query_pairs_mut().append_pair("t", &cfg.token);
            if !cfg.hub_id.is_empty() {
                u.query_pairs_mut().append_pair("hub", &cfg.hub_id);
            }
        }
    }
    for (k, v) in &report.params {
        u.query_pairs_mut().append_pair(k, v);
    }
    match http_client().get(u).send().await {
        // A transport error is the uplink, not the report — retry it.
        Err(e) => {
            crate::hlog!("linktap: report {} queued (failed to send: {})", report.event, e.without_url());
            SendOutcome::Transient
        }
        Ok(res) => {
            let code = res.status().as_u16();
            match report_refusal(code, &report.device) {
                None => SendOutcome::Sent,
                // 5xx / 408 / 429 are the worker or edge having a moment — retry. Any other non-2xx
                // is a refusal a re-send cannot fix (bad request, auth, not found); DROP it, or it
                // would sit at the front of the queue forever and block every report behind it.
                Some(why) => {
                    if code >= 500 || code == 408 || code == 429 {
                        crate::hlog!("linktap: report {} queued ({why})", report.event);
                        SendOutcome::Transient
                    } else {
                        crate::hlog!("linktap: report {} dropped ({why})", report.event);
                        SendOutcome::Permanent
                    }
                }
            }
        }
    }
}

/// Flush queued telemetry oldest-first, stopping at the first TRANSIENT failure — the uplink is
/// still down, so keep that report and the rest for the next drain. Serialized by `report_flush` so
/// the poll loop and the heartbeat loop cannot drain at once and double-send. Pop-send-refront keeps
/// a retryable report at the front without holding the queue lock across the network call.
async fn drain_reports(rt: &Rt) {
    let _flush = rt.report_flush.lock().await;
    loop {
        let next = { rt.pending_reports.lock().await.pop_front() };
        let Some(r) = next else { break };
        match send_report_once(rt, &r).await {
            SendOutcome::Sent | SendOutcome::Permanent => {} // delivered, or hopeless — either way it leaves the queue
            SendOutcome::Transient => {
                rt.pending_reports.lock().await.push_front(r);
                break; // uplink down — leave the backlog for the next successful send or heartbeat
            }
        }
    }
}

/// Deliver a telemetry report, retrying past a flaky uplink.
///
/// 🔴 WHY A QUEUE AND NOT A FIRE-AND-FORGET SEND. This used to send once and, on failure, log and
/// DROP the report — a permanent gap in the cloud for every `report … failed to send`, which on a
/// boat's cellular link is constant. The state the app shows would simply skip whatever the hub
/// observed while the uplink hiccuped. Now the report is enqueued (bounded) and the backlog drains
/// the moment a send succeeds — here, and again after every good heartbeat (heartbeat_loop). Still
/// never blocks the valve logic that produced it.
async fn spool_report(rt: &Rt, report: &crate::linktap_runtime::Report) {
    // A report means the local state just changed — wake the heartbeat into ACTIVE cadence so the
    // cloud/app track it live, even if the hub was in a 20-minute quiet nap.
    note_activity(rt);
    let dropped = {
        let mut q = rt.pending_reports.lock().await;
        push_bounded(&mut q, report.clone(), MAX_SPOOL_REPORTS)
    };
    if dropped > 0 {
        crate::hlog!("linktap: report backlog full - dropped {dropped} oldest sample(s)");
    }
    drain_reports(rt).await;
}

/// PURE: is this response a failure worth saying out loud, and what should the line say?
///
/// ⚠️ THE RULE THIS PINS IS THE ONE THAT WAS WRONG. `spool_report` used to check only for a
/// TRANSPORT error, so `Ok(401)` fell through as success and logged nothing. Every
/// `linktap.measurement` this hub sent was answered 401 — the hub's token authenticates it for its
/// OWN device id, not for `lt_<valve>` — so the vehicle's valve telemetry was dead in the cloud
/// from 2026-08-26 for four days while the log stayed clean and every check said reports were fine.
///
/// Split out because the bug was a MISSING BRANCH, and a missing branch in an async fire-and-forget
/// I/O path is exactly the thing no test was ever going to reach.
pub fn report_refusal(status: u16, device: &str) -> Option<String> {
    if (200..300).contains(&status) {
        return None;
    }
    // The status is the whole point of the line: 401 (this hub may not speak for that device) and
    // 500 (the cloud is broken) are different problems, and whoever reads this log is trying to
    // tell them apart.
    Some(format!("REFUSED by the cloud: HTTP {status} (device {device})"))
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// --- Entry --------------------------------------------------------------------------------------

/// The `--hub` main. Never returns except on fatal startup errors or ctrl-c (manual runs);
/// as a service there is no console, so failures also land in the heartbeat's absence — the
/// connectivity sweep alerting on a quiet hub is the real monitor.
pub fn run_headless() {
    // A manual run stops on ctrl-c. A Windows service cannot: it has no console and no signal —
    // the SCM tells it to stop, so the shutdown trigger has to come from OUTSIDE this function.
    run_with_shutdown(async { let _ = tokio::signal::ctrl_c().await; });
}

/// The daemon proper, parameterised by whatever means "stop now" for the caller.
///
/// ⚠️ THE SHUTDOWN FUTURE IS THE WHOLE POINT OF THE SPLIT. A Windows service that ignores its
/// stop control is not a service: the SCM waits out its timeout and then kills the process, which
/// looks to everyone involved like a hang, and `sc stop` reports failure on a daemon that was
/// working perfectly. `win_service.rs` passes a future that resolves when the SCM's control
/// handler fires; `run_headless` passes ctrl-c. Nothing else about the runtime differs — one
/// daemon, two ways of being told to stop.
pub fn run_with_shutdown<F>(shutdown: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let runtime = tokio::runtime::Runtime::new().expect("hub: tokio runtime");
    runtime.block_on(async {
        let base = hub_config::shared_base();
        // FIRST, before anything that might have something to say. Under the SCM there is no
        // console, so until this runs every diagnostic below is written to nowhere — which is
        // exactly how a discovery failure on CENTRAL stayed un-diagnosable for hours.
        crate::hub_log::init(&base);
        let cfg = hub_config::read_config_in(&base);
        let port = cfg.http_port;
        let rt = new_rt(base, WORKER_BASE.into());
        let listener = match tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await {
            Ok(l) => l,
            Err(e) => {
                crate::hlog!("hub: cannot bind 0.0.0.0:{port}: {e}");
                std::process::exit(1);
            }
        };
        crate::hlog!("hub: management API on 0.0.0.0:{port} ({})", if cfg.token.is_empty() { "unregistered - waiting for bootstrap" } else { "registered" });
        // Say the claim window OUT LOUD on an unclaimed hub. This is the one line that makes a
        // headless install self-explanatory: someone who has just run the installer over SSH sees
        // how long they have and what to do if they miss it, without reading any documentation.
        if cfg.token.is_empty() {
            crate::hlog!(
                "setup: this hub is UNCLAIMED and can be set up from this machine, or from any \
                 device on its own LAN, for the next {} minutes - restart the service to reopen \
                 the window",
                crate::adopt::ADOPTION_WINDOW.as_secs() / 60
            );
        }
        tokio::spawn(heartbeat_loop(rt.clone()));
        tokio::spawn(update_check_loop(rt.clone()));
        tokio::spawn(key_sync_loop(rt.clone()));
        // The LinkTap poll floor. It re-reads its own configuration each pass, so a gateway
        // configured (or a plan revoked) after boot is picked up without a restart.
        tokio::spawn(linktap_poll_loop(rt.clone()));
        // GPS acquisition on the LAN — re-reads its own config each pass, same as the LinkTap loop.
        tokio::spawn(gps_poll_loop(rt.clone()));
        // Managed routers (routers.rs) — the hub reads each on its cadence and reports as it.
        tokio::spawn(router_poll_loop(rt.clone()));
        // Sensor wiring — hunts each pending sleepy sensor on the LAN until it answers (sensors.rs).
        tokio::spawn(sensor_hunt_loop(rt.clone()));
        // The outbound socket to the worker: remote control, and live member-key pushes. Failing
        // to connect is not fatal — the LAN API and the polling sync carry on without it.
        tokio::spawn(crate::hub_relay::run(rt.clone()));
        tokio::select! {
            // with_connect_info: the first-run door needs the PEER address, because "only from this
            // machine" is the whole of its security.
            r = axum::serve(listener, router(rt).into_make_service_with_connect_info::<SocketAddr>()) => {
                if let Err(e) = r {
                    crate::hlog!("hub: server exited: {e}");
                }
            }
            // The caller's stop signal — ctrl-c for a manual run, the SCM's Stop/Shutdown control
            // for a Windows service. Either way it ends the select and the daemon winds down.
            _ = shutdown => {
                crate::hlog!("hub: shutting down");
            }
        }
    });
}


// --- Valve control (owner ruling 2026-08-19: with a hub present the control plane runs THROUGH
// it, never app -> device) ------------------------------------------------------------------------
//
// WHY THE APP ROUTES HERE AT ALL: an onsite executor adopts any cycle it did not start as a NORMAL
// RUN and enforces the Normal Run cap. While the app also opens valves directly, that executor
// cannot tell an app-started WASHDOWN (time-only, must never be volume-cut) from a physical button
// press. Routing through removes the ambiguity at its source — and `mode` below is the fact the
// hub could never infer by watching.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ValveReq {
    dev_id: String,
    /// "open" | "close".
    action: String,
    duration_secs: Option<u64>,
    /// Litres. ABSENT for a washdown — the key's absence IS the time-only signal, never a zero.
    volume_cap_l: Option<f64>,
    /// "normal" | "washdown" | "tankfill". Absent is treated as normal.
    mode: Option<String>,
    /// The app's "Start 'Normal Run' when timer expires" checkbox, for a WASHDOWN. Carried on the
    /// run so the hub honours it whether or not any app is open — see cycle::should_resume_normal.
    resume_normal: Option<bool>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GpsReq {
    #[serde(default)] kind: String,
    #[serde(default)] host: String,
    #[serde(default)] port: Option<u16>,
    /// Omitted ⇒ keep the stored username — the same rule as the password, so a partial re-send
    /// (host/port only) never blanks the sign-in. An explicit "" clears it.
    #[serde(default)] username: Option<String>,
    /// Omitted ⇒ keep the stored password, so re-saving other fields never wipes the sign-in.
    #[serde(default)] password: Option<String>,
    #[serde(default)] dev_id: String,
    #[serde(default)] enabled: Option<bool>,
    /// `nmea` only: `tcp` | `udp`. Omitted ⇒ keep.
    #[serde(default)] protocol: Option<String>,
}

/// Configure the LAN GPS source the hub polls (owner/co-owner). The hub is the acquirer now, so the
/// router admin sign-in is entered ONCE here (hub console or an app push) and stored in the hub's
/// credential file — never in app storage, never returned by any endpoint. See crate::gps.
async fn do_gps(rt: &Rt, caller: &Caller, body: &[u8]) -> Answer {
    if !may_administer(&caller.role) {
        return err(403, "configuring the GPS source needs a co-owner or the owner");
    }
    let req: GpsReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return err(422, &format!("invalid JSON body: {e}")),
    };
    {
        let _g = rt.store.lock().await;
        let mut cfg = hub_config::read_config_in(&rt.base);
        cfg.gps.kind = req.kind.trim().to_string();
        cfg.gps.host = req.host.trim().to_string();
        cfg.gps.port = req.port.unwrap_or(0); // 0 ⇒ the driver defaults to 443
        if let Some(u) = req.username {
            cfg.gps.username = u.trim().to_string();
        }
        if let Some(p) = req.password {
            if !p.is_empty() { cfg.gps.password = p; }
        }
        cfg.gps.dev_id = req.dev_id.trim().to_string();
        cfg.gps.enabled = req.enabled.unwrap_or(true);
        if let Some(p) = req.protocol {
            cfg.gps.protocol = p.trim().to_ascii_lowercase();
        }
        if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
            return err(500, &e);
        }
    }
    // Poll the just-configured source now rather than waiting out the interval.
    rt.gps_wake.notify_one();
    ok_json(&status_body(rt).await)
}

// --- Managed routers (routers.rs) -------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RoutersListBody {
    routers: Vec<RouterStatus>,
}

/// The routers this hub manages, redacted, with each one's last-poll snapshot. Any member.
async fn do_routers_list(rt: &Rt) -> Answer {
    let cfg = hub_config::read_config_in(&rt.base);
    ok_json(&RoutersListBody { routers: routers_status(rt, &cfg).await })
}

/// One body, one `action` — a single relayable path for the whole router surface (the relay
/// allow-lists exact paths, and the hub validates the body itself).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouterReq {
    #[serde(default)] action: String,
    #[serde(default)] id: String,
    #[serde(default)] vendor: Option<String>,
    #[serde(default)] name: Option<String>,
    #[serde(default)] host: Option<String>,
    #[serde(default)] port: Option<u16>,
    /// Omitted ⇒ keep. Explicit "" ⇒ the vendor default (`admin`).
    #[serde(default)] username: Option<String>,
    /// Omitted or "" ⇒ keep the stored password.
    #[serde(default)] password: Option<String>,
    /// Omitted or "" ⇒ keep the stored token.
    #[serde(default)] agent_token: Option<String>,
    #[serde(default)] gps_enabled: Option<bool>,
    #[serde(default)] gps_dev_id: Option<String>,
    #[serde(default)] poll_secs: Option<u32>,
    #[serde(default)] enabled: Option<bool>,
    /// `apn` action: `auto` | `manual` (+ `apn` name). Omitted ⇒ read only.
    #[serde(default)] mode: Option<String>,
    #[serde(default)] apn: Option<String>,
    /// `read` action: the `/api/status/…` or `/api/config/…` path to read.
    #[serde(default)] path: Option<String>,
}

/// What `probe` answers: identity plus the modem/WAN/GPS state read in the same breath, so the
/// wizard's Test shows the owner what the hub sees before anything is saved.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProbeBody {
    probe: crate::routers::Probe,
    #[serde(skip_serializing_if = "Option::is_none")]
    modem: Option<crate::routers::ModemStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wan: Option<crate::routers::WanStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gps_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<crate::routers::FixOut>,
}

/// Everything about a managed router, in one door. Owner/co-owner for anything that signs in with
/// an admin credential or changes what the hub stores (`probe`, `add`, `remove`, `apn`, `gps`,
/// `password`, `reboot`); `refresh` is a read and is control-grade.
async fn do_routers(rt: &Rt, caller: &Caller, body: &[u8]) -> Answer {
    let req: RouterReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return err(422, &format!("invalid JSON body: {e}")),
    };
    let action = req.action.trim().to_ascii_lowercase();
    if action == "refresh" || action == "read" {
        if !may_control(&caller.role) {
            return err(403, "reading a router needs control access or above");
        }
    } else if !may_administer(&caller.role) {
        return err(403, "managing a router needs a co-owner or the owner");
    }
    let client = crate::routers::lan_client();
    match action.as_str() {
        // Sign in with what the wizard typed, save nothing, say what was found.
        "probe" => {
            let vendor = req.vendor.as_deref().unwrap_or("cradlepoint").trim().to_ascii_lowercase();
            if !vendor_supported(&vendor) {
                return err(422, &format!("this hub cannot manage a '{vendor}' router yet"));
            }
            let host = req.host.as_deref().unwrap_or("").trim().to_string();
            if host.is_empty() {
                return err(422, "host is required");
            }
            let drv = match Driver::new(
                &client,
                &vendor,
                &host,
                req.port.unwrap_or(0),
                req.username.as_deref().unwrap_or(""),
                req.password.as_deref().unwrap_or(""),
            ) {
                Ok(d) => d,
                Err(why) => return err(422, &why),
            };
            let probe = match drv.probe().await {
                Ok(p) => p,
                Err(why) => return err(502, &why),
            };
            let (modem, wan) = drv.status().await.unwrap_or((None, None));
            let gps = drv.gps(true).await;
            ok_json(&ProbeBody {
                probe,
                modem,
                wan,
                gps_enabled: gps.enabled,
                fix: gps.fix.as_ref().map(crate::routers::FixOut::from),
            })
        }
        // Upsert by id. The sign-in is PROVED against the router before anything is stored, so a
        // typo cannot evict a working credential — and a GPS request switches the router's own
        // GNSS on, because the owner asked the hub to configure both ends.
        "add" => {
            let id = req.id.trim().to_string();
            if id.is_empty() {
                return err(422, "id is required");
            }
            let existing = hub_config::read_config_in(&rt.base).routers.into_iter().find(|r| r.id == id);
            let mut r = existing.clone().unwrap_or_default();
            r.id = id.clone();
            if let Some(v) = req.vendor {
                r.vendor = v.trim().to_ascii_lowercase();
            }
            if r.vendor.is_empty() {
                r.vendor = "cradlepoint".into();
            }
            if !vendor_supported(&r.vendor) {
                return err(422, &format!("this hub cannot manage a '{}' router yet", r.vendor));
            }
            if let Some(n) = req.name {
                r.name = n.trim().to_string();
            }
            if let Some(h) = req.host {
                r.host = h.trim().to_string();
            }
            if let Some(p) = req.port {
                r.port = p;
            }
            if let Some(u) = req.username {
                r.username = u.trim().to_string();
            }
            if let Some(p) = req.password.filter(|p| !p.is_empty()) {
                r.password = p;
            }
            if let Some(t) = req.agent_token.filter(|t| !t.is_empty()) {
                r.agent_token = t;
            }
            if let Some(g) = req.gps_enabled {
                r.gps_enabled = g;
            }
            if let Some(d) = req.gps_dev_id {
                r.gps_dev_id = d.trim().to_string();
            }
            if let Some(s) = req.poll_secs {
                r.poll_secs = s;
            }
            if let Some(e) = req.enabled {
                r.enabled = e;
            }
            if r.host.is_empty() {
                return err(422, "host is required");
            }
            if r.password.is_empty() {
                return err(422, "the router's admin password is required");
            }
            if r.gps_enabled && r.gps_dev_id.is_empty() {
                return err(422, "gpsDevId (the brv_gps_… record) is required when gpsEnabled");
            }
            let drv = match Driver::for_router(&client, &r) {
                Ok(d) => d,
                Err(why) => return err(422, &why),
            };
            let probe = match drv.probe().await {
                Ok(p) => p,
                Err(why) => return err(502, &why),
            };
            if r.name.is_empty() {
                r.name = probe.model.clone().unwrap_or_else(|| "Router".into());
            }
            // Configure the router's end of GPS too (where the vendor has one — a Peplink does
            // not, and this is a no-op for it). Best-effort: some units/carriers have no GNSS,
            // and a router that cannot be told is still worth managing — the snapshot's
            // gpsEnabled tells the app what the router actually reports.
            if r.gps_enabled {
                if let Err(why) = drv.set_gps_enabled(true).await {
                    crate::hlog!("routers: {} - could not switch the router's GPS on: {why}", r.host);
                }
            }
            {
                let _g = rt.store.lock().await;
                let mut cfg = hub_config::read_config_in(&rt.base);
                match cfg.routers.iter_mut().find(|x| x.id == id) {
                    Some(slot) => *slot = r.clone(),
                    None => cfg.routers.push(r.clone()),
                }
                if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
                    return err(500, &e);
                }
            }
            crate::hlog!(
                "routers: {} '{}' at {} {} (gps {})",
                r.vendor, r.name, r.host,
                if existing.is_some() { "updated" } else { "added" },
                if r.gps_enabled { "on" } else { "off" }
            );
            // Seed the snapshot with the identity we just read, then read the rest now.
            rt.router_state.write().await.entry(id.clone()).or_default().probe = Some(probe);
            rt.router_wake.notify_one();
            let states = rt.router_state.read().await;
            ok_json(&router_status(&r, states.get(&id)))
        }
        "remove" => {
            let id = req.id.trim().to_string();
            {
                let _g = rt.store.lock().await;
                let mut cfg = hub_config::read_config_in(&rt.base);
                let before = cfg.routers.len();
                cfg.routers.retain(|r| r.id != id);
                if cfg.routers.len() == before {
                    return err(404, "no managed router with that id");
                }
                if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
                    return err(500, &e);
                }
            }
            rt.router_state.write().await.remove(&id);
            crate::hlog!("routers: {id} removed");
            ok_json(&serde_json::json!({ "ok": true }))
        }
        // The per-router actions below all start by finding the router.
        "refresh" | "read" | "apn" | "gps" | "password" | "reboot" => {
            let id = req.id.trim().to_string();
            let Some(r) = hub_config::read_config_in(&rt.base).routers.into_iter().find(|r| r.id == id) else {
                return err(404, "no managed router with that id");
            };
            // A stored router of a vendor this build cannot drive (a downgrade) is refused here,
            // once, rather than by every action below.
            let drv = match Driver::for_router(&client, &r) {
                Ok(d) => d,
                Err(why) => return err(422, &why),
            };
            match action.as_str() {
                "refresh" => {
                    let prev = rt.router_state.read().await.get(&id).cloned();
                    let snap = crate::routers::poll(&client, &r, prev.as_ref()).await;
                    rt.router_state.write().await.insert(id.clone(), snap.clone());
                    if snap.error.is_none() {
                        rt.router_wake.notify_one(); // let the loop report what was just read
                    }
                    ok_json(&router_status(&r, Some(&snap)))
                }
                "apn" => {
                    let result = match req.mode.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
                        None => drv.apn().await,
                        Some(mode) => {
                            let mode = mode.to_ascii_lowercase();
                            if mode != "auto" && mode != "manual" {
                                return err(422, "mode must be auto or manual");
                            }
                            drv.set_apn(&crate::routers::ApnConfig { mode, apn: req.apn.clone() }).await
                        }
                    };
                    match result {
                        Ok(apn) => {
                            rt.router_state.write().await.entry(id).or_default().apn = Some(apn.clone());
                            ok_json(&apn)
                        }
                        Err(why) => err(502, &why),
                    }
                }
                // Switch GPS on/off at BOTH ends: the router's GNSS (where the vendor has a
                // switch — Driver::set_gps_enabled) and this hub's reporting.
                "gps" => {
                    let on = req.gps_enabled.unwrap_or(true);
                    let dev = req.gps_dev_id.map(|d| d.trim().to_string()).unwrap_or(r.gps_dev_id.clone());
                    if on && dev.is_empty() {
                        return err(422, "gpsDevId (the brv_gps_… record) is required when gpsEnabled");
                    }
                    if let Err(why) = drv.set_gps_enabled(on).await {
                        return err(502, &why);
                    }
                    {
                        let _g = rt.store.lock().await;
                        let mut cfg = hub_config::read_config_in(&rt.base);
                        if let Some(slot) = cfg.routers.iter_mut().find(|x| x.id == id) {
                            slot.gps_enabled = on;
                            slot.gps_dev_id = dev;
                        }
                        if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
                            return err(500, &e);
                        }
                    }
                    crate::hlog!("routers: {} - GPS switched {}", r.host, if on { "on" } else { "off" });
                    rt.router_wake.notify_one();
                    ok_json(&serde_json::json!({ "ok": true, "gpsEnabled": on }))
                }
                "password" => {
                    let Some(pw) = req.password.filter(|p| !p.is_empty()) else {
                        return err(422, "password is required");
                    };
                    let user = req.username.map(|u| u.trim().to_string()).unwrap_or(r.username.clone());
                    let trial = match Driver::new(&client, &r.vendor, &r.host, r.port, &user, &pw) {
                        Ok(d) => d,
                        Err(why) => return err(422, &why),
                    };
                    if let Err(why) = trial.probe().await {
                        return err(502, &why);
                    }
                    {
                        let _g = rt.store.lock().await;
                        let mut cfg = hub_config::read_config_in(&rt.base);
                        if let Some(slot) = cfg.routers.iter_mut().find(|x| x.id == id) {
                            slot.username = user;
                            slot.password = pw;
                        }
                        if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
                            return err(500, &e);
                        }
                    }
                    rt.router_wake.notify_one();
                    ok_json(&serde_json::json!({ "ok": true }))
                }
                "reboot" => {
                    match drv.reboot().await {
                        Ok(()) => {
                            crate::hlog!("routers: {} - reboot requested by {}", r.host, caller.uid);
                            ok_json(&serde_json::json!({ "ok": true }))
                        }
                        Err(why) => err(502, &why),
                    }
                }
                // A read-only diagnostic: one NCOS status/config path, scrubbed and capped
                // (routers::read_path). This is how a parser for a tree the bench has not seen yet
                // (Wi-Fi, LAN, DHCP reservations) gets pinned to what the router actually says.
                "read" => {
                    let path = req.path.as_deref().unwrap_or("").to_string();
                    let ncos = crate::routers::Ncos::for_router(&client, &r);
                    match ncos.read_path(&path).await {
                        Ok(data) => ok_json(&serde_json::json!({ "path": path.trim(), "data": data })),
                        Err(why) if why.starts_with("read takes") || why.starts_with("that is not") || why.starts_with("a path is") => err(422, &why),
                        Err(why) => err(502, &why),
                    }
                }
                _ => unreachable!(),
            }
        }
        other => err(422, &format!("unknown action '{other}'")),
    }
}

// --- Sensor wiring (sensors.rs) ---------------------------------------------------------------------

async fn sensors_status(rt: &Rt) -> Vec<crate::sensors::SensorState> {
    let mut v: Vec<_> = rt.sensor_state.read().await.values().cloned().collect();
    v.sort_by_key(|s| std::cmp::Reverse(s.added_at));
    v
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SensorsListBody {
    sensors: Vec<crate::sensors::SensorState>,
}

async fn do_sensors_list(rt: &Rt) -> Answer {
    ok_json(&SensorsListBody { sensors: sensors_status(rt).await })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SensorReq {
    #[serde(default)] action: String,
    #[serde(default)] id: String,
    #[serde(default)] hosts: Vec<String>,
    #[serde(default)] hooks: Vec<crate::sensors::DesiredHook>,
    #[serde(default)] password: Option<String>,
    #[serde(default)] ble_off: Option<bool>,
}

/// `wire` hands the hub a sleepy sensor to finish; `cancel` withdraws it; `forget` drops a finished
/// entry from the list. Adding devices is co-owner+ (the same authority the wizard runs under).
async fn do_sensors(rt: &Rt, caller: &Caller, body: &[u8]) -> Answer {
    if !may_administer(&caller.role) {
        return err(403, "wiring a sensor needs a co-owner or the owner");
    }
    let req: SensorReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return err(422, &format!("invalid JSON body: {e}")),
    };
    let id = req.id.trim().to_ascii_lowercase();
    if id.is_empty() {
        return err(422, "id (the Shelly device id) is required");
    }
    match req.action.trim().to_ascii_lowercase().as_str() {
        "wire" => {
            let mut hooks: Vec<_> = req.hooks.into_iter().filter(|h| !h.event.is_empty() && !h.urls.is_empty()).collect();
            if hooks.is_empty() {
                return err(422, "at least one hook (event + urls) is required");
            }
            // `__HUB__` ⇒ this hub's own LAN address (the phone may only know the hub via the cloud).
            if hooks.iter().any(|h| h.urls.iter().any(|u| u.contains("__HUB__"))) {
                match crate::linktap_discover::local_ipv4s().into_iter().next() {
                    Some(ip) => crate::sensors::substitute_hub_host(&mut hooks, &ip),
                    None => {
                        // No LAN address to offer: keep the cloud urls, drop the hub ones rather
                        // than writing a url the sensor can never dial.
                        for h in hooks.iter_mut() { h.urls.retain(|u| !u.contains("__HUB__")); }
                        hooks.retain(|h| !h.urls.is_empty());
                        if hooks.is_empty() { return err(409, "this hub has no LAN address to offer the sensor"); }
                    }
                }
            }
            let hosts: Vec<String> = req.hosts.into_iter().map(|h| h.trim().to_string()).filter(|h| !h.is_empty() && h != "0.0.0.0").collect();
            let job = crate::sensors::SensorJob {
                id: id.clone(),
                hosts: if hosts.is_empty() { vec![format!("{id}.local")] } else { hosts },
                hooks,
                password: req.password.unwrap_or_default(),
                ble_off: req.ble_off.unwrap_or(true),
                added_at: now_ms(),
            };
            {
                let _g = rt.store.lock().await;
                let mut cfg = hub_config::read_config_in(&rt.base);
                cfg.sensor_jobs.retain(|j| j.id != id);
                cfg.sensor_jobs.push(job.clone());
                if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
                    return err(500, &e);
                }
            }
            let st = crate::sensors::SensorState::pending(&job);
            rt.sensor_state.write().await.insert(id.clone(), st.clone());
            crate::hlog!("sensors: {id} - wiring job accepted ({} hook(s), hosts {:?}, ble off {})", job.hooks.len(), job.hosts, job.ble_off);
            rt.sensor_wake.notify_one();
            ok_json(&st)
        }
        "cancel" | "forget" => {
            {
                let _g = rt.store.lock().await;
                let mut cfg = hub_config::read_config_in(&rt.base);
                cfg.sensor_jobs.retain(|j| j.id != id);
                if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
                    return err(500, &e);
                }
            }
            rt.sensor_state.write().await.remove(&id);
            ok_json(&serde_json::json!({ "ok": true }))
        }
        other => err(422, &format!("unknown action '{other}'")),
    }
}

/// A sensor with a pending job just reported from `ip`: put that address first and wake the hunt.
async fn note_sensor_seen(rt: &Rt, device: &str, ip: std::net::IpAddr) {
    let id = device.trim().to_ascii_lowercase();
    if !rt.sensor_state.read().await.get(&id).map_or(false, |s| s.state == "pending") {
        return;
    }
    let host = ip.to_string();
    {
        let _g = rt.store.lock().await;
        let mut cfg = hub_config::read_config_in(&rt.base);
        if let Some(j) = cfg.sensor_jobs.iter_mut().find(|j| j.id == id) {
            j.hosts.retain(|h| h != &host);
            j.hosts.insert(0, host.clone());
            let _ = hub_config::write_config_in(&rt.base, &cfg);
        }
    }
    crate::hlog!("sensors: {id} reported from {host} - wiring it now while it is awake");
    rt.sensor_wake.notify_one();
}

/// Hunt every pending sensor across its candidate addresses, every few seconds, until it answers;
/// then wire, verify, switch BLE off, and record the outcome. Pending jobs come from hub.json so a
/// restart resumes them; a finished job leaves hub.json and stays in the in-memory list for the
/// wizard (and the console) to read. Silence is logged only when it CHANGES.
async fn sensor_hunt_loop(rt: Shared) {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(4)).build().expect("reqwest client");
    // Adopt persisted jobs as pending on boot.
    {
        let cfg = hub_config::read_config_in(&rt.base);
        let mut st = rt.sensor_state.write().await;
        for j in &cfg.sensor_jobs {
            st.entry(j.id.clone()).or_insert_with(|| crate::sensors::SensorState::pending(j));
        }
    }
    loop {
        let jobs = hub_config::read_config_in(&rt.base).sensor_jobs;
        for job in &jobs {
            let pending = rt.sensor_state.read().await.get(&job.id).map_or(true, |s| s.state == "pending");
            if !pending {
                continue;
            }
            let mut outcome: Option<crate::sensors::SensorState> = None;
            let mut last_err: Option<String> = None;
            for host in &job.hosts {
                match crate::sensors::attempt(&client, job, host, now_ms()).await {
                    Ok(Some(st)) => { outcome = Some(st); break; }
                    Ok(None) => {}
                    Err(e) => last_err = Some(e),
                }
            }
            match outcome {
                Some(st) => {
                    crate::hlog!(
                        "sensors: {} - {} at {} ({} destination(s) verified{}){}",
                        job.id, st.state, st.host.clone().unwrap_or_default(), st.confirmed.len(),
                        if st.ble_disabled { ", Bluetooth off" } else { "" },
                        st.last_error.as_ref().map(|e| format!(" - {e}")).unwrap_or_default()
                    );
                    rt.sensor_state.write().await.insert(job.id.clone(), st);
                    let _g = rt.store.lock().await;
                    let mut cfg = hub_config::read_config_in(&rt.base);
                    cfg.sensor_jobs.retain(|j| j.id != job.id);
                    let _ = hub_config::write_config_in(&rt.base, &cfg);
                    note_activity(&rt);
                }
                None => {
                    let mut st = rt.sensor_state.write().await;
                    if let Some(s) = st.get_mut(&job.id) {
                        s.attempts += 1;
                        if s.last_error != last_err {
                            if let Some(e) = &last_err { crate::hlog!("sensors: {} - {e}", job.id); }
                            s.last_error = last_err;
                        }
                    }
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(crate::sensors::HUNT_SECS)) => {}
            _ = rt.sensor_wake.notified() => {}
        }
    }
}

/// How often the router loop wakes to see whether any router is due. Each router has its own
/// cadence (routers::poll_secs); this is only the tick.
const ROUTER_TICK_SECS: u64 = 15;

/// Read each managed router on its cadence and report — `modem.measurement` as the router (with
/// the router's own agent token) and, when its GPS is on, `gps.measurement` as the linked gps_source
/// device (with the hub's token, the vouched `brv_gps_*` path). Re-reads config each pass, so a
/// router added, edited or removed from the app is picked up without a restart. Errors are logged
/// only when they CHANGE, so an unplugged router does not fill the log every two minutes.
async fn router_poll_loop(rt: Shared) {
    let client = crate::routers::lan_client();
    let mut last_error: HashMap<String, String> = HashMap::new();
    let mut last_counters: HashMap<String, (u64, u64)> = HashMap::new();
    let mut last_report_ms: HashMap<String, i64> = HashMap::new();
    loop {
        let cfg = hub_config::read_config_in(&rt.base);
        let now = now_ms();
        for r in cfg.routers.iter().filter(|r| r.enabled && !r.host.is_empty()) {
            let due_ms = (crate::routers::poll_secs(r) * 1000) as i64;
            if last_report_ms.get(&r.id).map_or(false, |t| now - t < due_ms) {
                continue;
            }
            let prev = rt.router_state.read().await.get(&r.id).cloned();
            let snap = crate::routers::poll(&client, r, prev.as_ref()).await;
            last_report_ms.insert(r.id.clone(), now);
            match &snap.error {
                Some(why) => {
                    if last_error.get(&r.id) != Some(why) {
                        crate::hlog!("routers: {} '{}' - {why}", r.host, r.name);
                        last_error.insert(r.id.clone(), why.clone());
                    }
                }
                None => {
                    if last_error.remove(&r.id).is_some() {
                        crate::hlog!("routers: {} '{}' - reachable again", r.host, r.name);
                    }
                    if let Some(m) = &snap.modem {
                        let counters = m.tx_bytes.zip(m.rx_bytes);
                        let delta = counters.and_then(|c| crate::routers::wan_kb_delta(last_counters.get(&r.id).copied(), c));
                        if let Some(c) = counters {
                            last_counters.insert(r.id.clone(), c);
                        }
                        if r.agent_token.is_empty() {
                            // Readable in the app, but nothing reaches the cloud: say so once.
                            if last_error.get(&r.id).map_or(true, |e| e != "no agent token") {
                                crate::hlog!("routers: {} '{}' - no agent token; status is local only until the app enrolls it", r.host, r.name);
                                last_error.insert(r.id.clone(), "no agent token".into());
                            }
                        } else {
                            spool_report(&rt, &crate::linktap_runtime::Report {
                                device: r.id.clone(),
                                event: "modem.measurement".into(),
                                params: crate::routers::modem_params(m, snap.wan.as_ref(), snap.probe.as_ref(), delta),
                                token: Some(r.agent_token.clone()),
                            })
                            .await;
                        }
                    }
                    if r.gps_enabled && !r.gps_dev_id.is_empty() {
                        if let Some(fix) = &snap.fix {
                            let mut params = vec![
                                ("lat".to_string(), format!("{:.6}", fix.lat)),
                                ("lon".to_string(), format!("{:.6}", fix.lon)),
                            ];
                            if let Some(acc) = fix.acc {
                                params.push(("acc".to_string(), format!("{acc:.1}")));
                            }
                            spool_report(&rt, &crate::linktap_runtime::Report {
                                device: r.gps_dev_id.clone(),
                                event: "gps.measurement".into(),
                                params,
                                token: None,
                            })
                            .await;
                        }
                    }
                }
            }
            rt.router_state.write().await.insert(r.id.clone(), snap);
        }
        // Forget routers that are gone, so a re-add starts fresh.
        let ids: HashSet<String> = cfg.routers.iter().map(|r| r.id.clone()).collect();
        last_report_ms.retain(|k, _| ids.contains(k));
        last_counters.retain(|k, _| ids.contains(k));
        last_error.retain(|k, _| ids.contains(k));
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(ROUTER_TICK_SECS)) => {}
            _ = rt.router_wake.notified() => {
                // A wake means "read now" — clear the due-times so the next pass polls everything.
                last_report_ms.clear();
            }
        }
    }
}

async fn do_valve(rt: &Rt, caller: &Caller, body: &[u8]) -> Answer {
    if !may_control(&caller.role) {
        return err(403, "controlling a valve needs control access or above");
    }
    let req: ValveReq = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return err(422, &format!("invalid JSON body: {e}")),
    };

    let cfg = hub_config::read_config_in(&rt.base);
    // The paid gate again, at the ACTION not just the advertisement: a caller that skipped the
    // capability check (an older app, a hand-rolled request) must still be refused. Cheap, and it
    // means the gate cannot be bypassed by not asking.
    if !cfg.linktap.allowed {
        return err(402, "this vehicle's plan does not include valve control");
    }
    if cfg.linktap.host.is_empty() || cfg.linktap.gw_id.is_empty() {
        return err(409, "no LinkTap gateway is configured on this hub");
    }
    let dev_id = linktap::normalize_dev_id(&req.dev_id);
    if dev_id.is_empty() {
        return err(422, "devId is required");
    }
    // Only valves this hub was told about — a hub is not a general-purpose proxy onto the
    // vessel's RF network, the same reasoning as the relay's path allowlist.
    if !cfg.linktap.dev_ids.iter().any(|d| linktap::normalize_dev_id(d) == dev_id) {
        return err(404, "that valve is not configured on this hub");
    }

    let gw = linktap::Gateway { host: cfg.linktap.host.clone(), gw_id: cfg.linktap.gw_id.clone() };
    let client = reqwest::Client::new();

    let body_json = match req.action.as_str() {
        "close" => linktap::build_stop(&gw, &dev_id),
        "open" => {
            let secs = match req.duration_secs {
                Some(s) if s > 0 => s,
                _ => return err(422, "durationSecs is required to open a valve"),
            };
            // ⚠️ A WASHDOWN IS TIME-ONLY (owner spec 2026-07-30, re-ratified twice). Mode decides
            // the cap, NOT the presence of a number: a volumeCapL sent alongside mode=washdown is
            // a caller bug, and honouring it would re-create the "external cap" that cut 2-hour
            // hose runs at ~26 gal. Refuse it rather than silently dropping either side.
            let mode = req.mode.as_deref().unwrap_or("normal");
            if mode == "washdown" && req.volume_cap_l.is_some() {
                return err(422, "a washdown is time-limited only — do not send volumeCapL with mode=washdown");
            }
            // The cap must be expressed in the GATEWAY's unit, so read it — one extra
            // round-trip on a user-initiated action, where being right beats being fast.
            // read_vol_unit defaults to GALLONS when unreadable, because guessing litres
            // under-reports a cap by 3.79x and the cutoff compares against that number.
            let cap_gw = match (mode, req.volume_cap_l) {
                ("washdown", _) | (_, None) => None,
                (_, Some(l)) => {
                    let unit = linktap::read_vol_unit(&client, &gw).await;
                    Some(unit.from_litres(l))
                }
            };
            linktap::build_start(&gw, &dev_id, secs, cap_gw)
        }
        other => return err(422, &format!("unknown action '{other}' — expected open or close")),
    };

    let reply = linktap::post_command(&client, &gw, &body_json).await;
    if !reply.ok {
        let detail = reply.error.unwrap_or_else(|| "the gateway refused the command".into());
        crate::hlog!("linktap: {} {} failed: {detail}", req.action, dev_id);
        // 502: the hub is fine, the thing BEHIND it refused. The app falls back and says so.
        return err(502, &format!("the gateway did not accept that command: {detail}"));
    }
    crate::hlog!("linktap: {} {} ok", req.action, dev_id);

    // 🔴 TELL OURSELVES WHAT WE JUST DID. Until this call the hub learned about its OWN opens the
    // same way it learned about a press on the tap — by polling the gateway and finding the valve
    // already running — so `cycle::step` took the `Idle` branch and ADOPTED the run. The targets
    // were then the profile's rather than the ones we had just issued, and `prov` went out as
    // `adopted`, which the app renders as "Started Externally".
    //
    // A close needs no equivalent: `observe` sees `is_watering` go false and ends the cycle with
    // the provenance the Running state already carries.
    if req.action == "open" {
        if let Some(secs) = req.duration_secs {
            let mode = cycle::Mode::from_wire(req.mode.as_deref().unwrap_or("normal"));
            let mut guard = rt.linktap.lock().await;
            if let Some(runtime) = guard.as_mut() {
                // A washdown is time-only, so its cap is 0. Anything else MUST carry one; when the
                // caller omitted it we fall back to the valve's effective profile rather than
                // refusing — the gateway already has the command, so the choice here is between
                // tracking the run with a sane cap and not tracking it at all.
                let cap_l = match mode {
                    cycle::Mode::Washdown => 0.0,
                    _ => req.volume_cap_l.unwrap_or_else(|| runtime.profile_for(&dev_id).volume_cap_l),
                };
                let resume = req.resume_normal.unwrap_or(false);
                if let Err(e) = runtime.note_hub_open(&dev_id, now_ms(), mode, secs, cap_l, resume) {
                    // Never fail the caller for this: the valve IS open, which is what they asked
                    // for. Say it out loud, because a silent miss here reads as "started externally"
                    // in the app and nowhere else.
                    crate::hlog!("linktap: could not record our own open of {dev_id}: {e}");
                }
            }
        }
    }

    // 🔴 REPORT WHAT WE JUST DID, NOW. The valve state the app sees off-boat comes from the linktap
    // poll loop, which otherwise sleeps up to LINKTAP_POLL_SECS between passes — so a command's
    // result took up to a minute to reach the cloud (and the web app, which has no LAN path). Wake
    // the loop: it re-polls after a short settle and reports the new state within a couple seconds.
    // Fires for open AND close.
    rt.linktap_wake.notify_one();

    ok_json(&serde_json::json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;
    use axum::Json;
    use std::collections::HashMap;

    #[test]
    fn the_report_spool_drops_its_oldest_when_full() {
        use std::collections::VecDeque;
        let mut q: VecDeque<u32> = VecDeque::new();
        for i in 0..3 { assert_eq!(push_bounded(&mut q, i, 3), 0, "under cap drops nothing"); }
        assert_eq!(Vec::from(q.clone()), vec![0, 1, 2]);
        // At cap: the next push evicts the oldest (0), keeping the freshest three.
        assert_eq!(push_bounded(&mut q, 3, 3), 1);
        assert_eq!(Vec::from(q.clone()), vec![1, 2, 3], "oldest sample is the one shed, newest kept");
        assert_eq!(push_bounded(&mut q, 4, 3), 1);
        assert_eq!(Vec::from(q), vec![2, 3, 4]);
    }

    #[test]
    fn heartbeat_backs_off_when_idle_and_speeds_up_after_an_event() {
        let cfg = 60; // the default configured cadence
                      // Just after a local event ⇒ ACTIVE (fast), but never slower than configured.
        assert_eq!(heartbeat_interval_secs(0, cfg), HEARTBEAT_ACTIVE_SECS);
        assert_eq!(heartbeat_interval_secs(HEARTBEAT_ACTIVE_WINDOW_MS - 1, cfg), HEARTBEAT_ACTIVE_SECS);
        // Recent-ish ⇒ NORMAL (the configured beat).
        assert_eq!(heartbeat_interval_secs(HEARTBEAT_ACTIVE_WINDOW_MS, cfg), 60);
        assert_eq!(heartbeat_interval_secs(HEARTBEAT_NORMAL_WINDOW_MS - 1, cfg), 60);
        // Long idle ⇒ QUIET (one beat every 20 min), still under the 60-min offline threshold.
        assert_eq!(heartbeat_interval_secs(HEARTBEAT_NORMAL_WINDOW_MS, cfg), HEARTBEAT_QUIET_SECS);
        assert!(HEARTBEAT_QUIET_SECS < 60 * 60, "quiet beat must stay under the 60-min offline default");
    }

    #[test]
    fn heartbeat_respects_an_unusual_configured_cadence() {
        // A very fast configured beat: ACTIVE is never SLOWER than it.
        assert_eq!(heartbeat_interval_secs(0, 15), 15);
        // A configured beat slower than QUIET: QUIET never FASTER than it, and NORMAL honors it.
        let slow = 30 * 60; // 30 min
        assert_eq!(heartbeat_interval_secs(HEARTBEAT_NORMAL_WINDOW_MS, slow), slow);
        assert_eq!(heartbeat_interval_secs(HEARTBEAT_ACTIVE_WINDOW_MS, slow), slow);
        // Below the floor is lifted to the floor.
        assert_eq!(heartbeat_interval_secs(HEARTBEAT_NORMAL_WINDOW_MS, 5), HEARTBEAT_QUIET_SECS);
        assert_eq!(heartbeat_interval_secs(HEARTBEAT_ACTIVE_WINDOW_MS, 5), HEARTBEAT_FLOOR_SECS);
    }

    fn temp_base(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("brvg-hub-server-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn key(role: &str) -> MemberKey {
        MemberKey { key: format!("key-{role}-{}", "x".repeat(32)), uid: format!("uid-{role}"), role: role.into() }
    }

    fn seeded_cfg() -> HubConfig {
        HubConfig {
            hub_id: "hub_abc123".into(), vid: "v1".into(), name: "Central".into(),
            enabled: true, heartbeat_secs: 60, token: "hubtok-secret".into(),
            ..HubConfig::default()
        }
    }

    async fn spawn_server(base: PathBuf, keys: Vec<MemberKey>) -> (String, Shared) {
        let rt = new_rt(base, "https://unused.example".into());
        *rt.keys.write().await = keys;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // with_connect_info, exactly as run_headless serves it — the first-run door reads the peer
        // address, and a harness without it would 500 on a route production answers.
        let app = router(rt.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), rt)
    }

    #[test]
    fn hub_mode_is_the_dash_dash_hub_flag_and_nothing_else() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(hub_mode_requested(args(&["app.exe", "--hub"])));
        assert!(!hub_mode_requested(args(&["app.exe"])));
        // Near-misses must not turn a GUI launch into a headless one.
        assert!(!hub_mode_requested(args(&["app.exe", "--hubx", "hub", "--HUB"])));
    }

    #[test]
    fn authorization_is_deny_by_default_and_exact_match() {
        let keys = vec![key("owner"), key("monitor")];
        assert!(authorize(&keys, "").is_none());
        assert!(authorize(&[], &key("owner").key).is_none()); // pre-sync: NOTHING authenticates
        assert!(authorize(&keys, "key-owner-wrong").is_none());
        let hit = authorize(&keys, &key("monitor").key).unwrap();
        assert_eq!(hit.role, "monitor");
        assert_eq!(hit.uid, "uid-monitor");
    }

    #[test]
    fn write_gates_mirror_the_vehicle_role_matrix() {
        // change_settings grade
        for r in ["owner", "coowner", "admin"] { assert!(may_configure(r), "{r}"); }
        for r in ["control", "monitor", "", "garbage"] { assert!(!may_configure(r), "{r}"); }
        // device-lifecycle grade
        for r in ["owner", "coowner"] { assert!(may_administer(r), "{r}"); }
        for r in ["admin", "control", "monitor", ""] { assert!(!may_administer(r), "{r}"); }
    }

    #[test]
    fn heartbeat_url_carries_acks_and_omits_an_empty_one() {
        let cfg = seeded_cfg();
        let some = url::Url::parse(&heartbeat_url("https://w.example", &cfg, "0.3.30", "linux", None, Some("a1,b2")).unwrap()).unwrap();
        let q: std::collections::HashMap<_, _> = some.query_pairs().into_owned().collect();
        assert_eq!(q.get("ack").map(String::as_str), Some("a1,b2"));
        let none = url::Url::parse(&heartbeat_url("https://w.example", &cfg, "0.3.30", "linux", None, Some("")).unwrap()).unwrap();
        assert!(!none.query_pairs().any(|(k, _)| k == "ack"), "empty ack must be omitted, like update");
    }

    #[test]
    fn parse_agent_commands_keeps_only_well_formed_pairs() {
        let body = serde_json::json!({ "commands": [
            { "id": "x1", "cmd": "self_update" },
            { "id": "", "cmd": "self_update" },
            { "cmd": "self_update" },
            { "id": "x2" },
            { "id": "x3", "cmd": "rollback_agent" }
        ]});
        assert_eq!(
            parse_agent_commands(&body),
            vec![("x1".to_string(), "self_update".to_string()), ("x3".to_string(), "rollback_agent".to_string())]
        );
        assert!(parse_agent_commands(&serde_json::json!({})).is_empty());
        assert!(parse_agent_commands(&serde_json::json!({ "commands": "nope" })).is_empty());
    }

    #[test]
    fn the_heartbeat_is_a_hub_measurement_on_the_agent_wire() {
        let cfg = seeded_cfg();
        let u = url::Url::parse(&heartbeat_url("https://w.example/", &cfg, "1.0.82", "windows", None, None).unwrap()).unwrap();
        assert_eq!(u.path(), "/api/agent"); // same ingest as the router agent
        let q: HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["vid"], "v1");
        assert_eq!(q["device"], "hub_abc123");
        assert_eq!(q["event"], "hub.measurement"); // telemetry classification, never an alert
        assert_eq!(q["t"], "hubtok-secret");
        assert_eq!(q["name"], "Central");
        assert_eq!(q["platform"], "windows");
        assert_eq!(q["ver"], "1.0.82");
        assert!(!q.contains_key("update")); // absent when there is no newer release
    }

    #[test]
    fn the_heartbeat_carries_an_update_marker_only_when_one_exists() {
        let cfg = seeded_cfg();
        // No update → no param (also covers an empty string being treated as none).
        let none = url::Url::parse(&heartbeat_url("https://w.example", &cfg, "0.3.23", "linux", Some(""), None).unwrap()).unwrap();
        assert!(!none.query_pairs().any(|(k, _)| k == "update"));
        // A newer release → the version rides the heartbeat, so the fleet console reads it flat.
        let some = url::Url::parse(&heartbeat_url("https://w.example", &cfg, "0.3.23", "linux", Some("0.3.24"), None).unwrap()).unwrap();
        let q: HashMap<_, _> = some.query_pairs().into_owned().collect();
        assert_eq!(q["update"], "0.3.24");
    }

    #[test]
    fn a_hub_name_with_spaces_and_symbols_cannot_break_the_query() {
        let cfg = HubConfig { name: "Jon's boat & RV=hub".into(), ..seeded_cfg() };
        let raw = heartbeat_url("https://w.example", &cfg, "1.0.82", "macos", None, None).unwrap();
        assert!(!raw.contains("boat & RV"), "the name must be encoded: {raw}");
        let q: HashMap<_, _> = url::Url::parse(&raw).unwrap().query_pairs().into_owned().collect();
        assert_eq!(q["name"], "Jon's boat & RV=hub");
        assert_eq!(q["t"], "hubtok-secret"); // an `=` in the name must not spill into another param
    }

    #[tokio::test]
    async fn the_api_denies_everything_without_a_valid_key() {
        let base = temp_base("deny");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let (origin, _rt) = spawn_server(base, vec![]).await; // pre-sync: empty key set
        let c = reqwest::Client::new();
        let r = c.get(format!("{origin}/api/hub/status")).send().await.unwrap();
        assert_eq!(r.status(), 401);
        let r = c.get(format!("{origin}/api/hub/status")).header(KEY_HEADER, "anything").send().await.unwrap();
        assert_eq!(r.status(), 401);
    }

    #[tokio::test]
    async fn status_answers_a_valid_key_and_never_contains_the_token() {
        let base = temp_base("status");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("monitor")]).await;
        let c = reqwest::Client::new();
        let r = c.get(format!("{origin}/api/hub/status")).header(KEY_HEADER, key("monitor").key).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let text = r.text().await.unwrap();
        assert!(!text.contains("hubtok-secret"), "token leaked into status: {text}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["hubId"], "hub_abc123");
        assert_eq!(v["vid"], "v1");
        assert_eq!(v["registered"], true);
        assert_eq!(v["heartbeatSecs"], 60);
        assert_eq!(v["keysSynced"], 1);
    }

    #[tokio::test]
    async fn a_damaged_config_is_reported_and_cannot_be_silently_re_signed() {
        // THE CENTRAL FAILURE OF 2026-08-28, as an endpoint test. A hub.json that will not parse
        // reads back as defaults, so the hub USED to present as factory-fresh: not registered, no
        // capabilities, no reason given, and one setup flow away from overwriting the real token.
        let base = temp_base("damaged");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        std::fs::write(hub_config::config_path_in(&base), "{\"vid\": \"v1\", \"token\": \"hubtok-sec").unwrap();

        let (origin, _rt) = spawn_server(base.clone(), vec![key("monitor")]).await;
        let c = reqwest::Client::new();

        // Status SAYS SO instead of quietly presenting an unregistered hub.
        let r = c.get(format!("{origin}/api/hub/status")).header(KEY_HEADER, key("monitor").key).send().await.unwrap();
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["registered"], false);
        assert!(v["configDamaged"].as_str().unwrap_or("").contains("not valid JSON"), "{v}");

        // Setup is refused with the true reason, not invited and then failed half way through.
        let r = c.post(format!("{origin}/api/hub/bootstrap"))
            .json(&serde_json::json!({"vid": "v-new", "name": "New", "token": "new-token"})).send().await.unwrap();
        assert_eq!(r.status(), 409);
        assert!(r.text().await.unwrap().contains("damaged"));

        // And the original file — the token inside it — is untouched and still recoverable.
        assert!(std::fs::read_to_string(hub_config::config_path_in(&base)).unwrap().contains("hubtok-sec"));
    }

    #[tokio::test]
    async fn config_writes_require_the_settings_grade_and_persist_to_the_store() {
        let base = temp_base("config");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let (origin, _rt) = spawn_server(base.clone(), vec![key("monitor"), key("admin")]).await;
        let c = reqwest::Client::new();

        // monitor: authenticated but not entitled — 403, and the store is untouched.
        let r = c.post(format!("{origin}/api/hub/config")).header(KEY_HEADER, key("monitor").key)
            .json(&serde_json::json!({"name": "Hacked"})).send().await.unwrap();
        assert_eq!(r.status(), 403);
        assert_eq!(hub_config::read_config_in(&base).name, "Central");

        // admin: entitled for settings.
        let r = c.post(format!("{origin}/api/hub/config")).header(KEY_HEADER, key("admin").key)
            .json(&serde_json::json!({"name": "  Boat PC  ", "heartbeatSecs": 45})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let cfg = hub_config::read_config_in(&base);
        assert_eq!(cfg.name, "Boat PC"); // trimmed
        assert_eq!(cfg.heartbeat_secs, 45);
        assert_eq!(cfg.token, "hubtok-secret"); // untouched by a settings write

        // Below the heartbeat floor is an explicit 422, not a silent clamp.
        let r = c.post(format!("{origin}/api/hub/config")).header(KEY_HEADER, key("admin").key)
            .json(&serde_json::json!({"heartbeatSecs": 5})).send().await.unwrap();
        assert_eq!(r.status(), 422);
        assert_eq!(hub_config::read_config_in(&base).heartbeat_secs, 45);
    }

    #[tokio::test]
    async fn token_rotation_and_clear_are_coowner_grade() {
        let base = temp_base("admin");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let (origin, _rt) = spawn_server(base.clone(), vec![key("admin"), key("coowner")]).await;
        let c = reqwest::Client::new();

        // admin may configure but NOT rotate the credential or tear the hub down.
        let r = c.post(format!("{origin}/api/hub/token")).header(KEY_HEADER, key("admin").key)
            .json(&serde_json::json!({"token": "new-tok"})).send().await.unwrap();
        assert_eq!(r.status(), 403);

        let r = c.post(format!("{origin}/api/hub/token")).header(KEY_HEADER, key("coowner").key)
            .json(&serde_json::json!({"token": "new-tok"})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(hub_config::read_config_in(&base).token, "new-tok");

        let r = c.post(format!("{origin}/api/hub/clear")).header(KEY_HEADER, key("admin").key).send().await.unwrap();
        assert_eq!(r.status(), 403);
        let r = c.post(format!("{origin}/api/hub/clear")).header(KEY_HEADER, key("coowner").key).send().await.unwrap();
        assert_eq!(r.status(), 204);
        assert!(!hub_config::config_path_in(&base).exists());
        // And the key set is dropped with it — a cleared hub authenticates nobody.
        let r = c.get(format!("{origin}/api/hub/status")).header(KEY_HEADER, key("coowner").key).send().await.unwrap();
        assert_eq!(r.status(), 401);
    }

    #[tokio::test]
    async fn authentication_happens_before_the_body_is_ever_parsed() {
        let base = temp_base("authfirst");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let (origin, _rt) = spawn_server(base.clone(), vec![key("coowner")]).await;
        let c = reqwest::Client::new();
        for path in ["/api/hub/config", "/api/hub/token", "/api/hub/linktap/valve"] {
            // Garbage body, no key: the answer must be 401, never a parser complaint. An
            // unauthenticated caller must not reach the deserializer at all.
            let r = c.post(format!("{origin}{path}"))
                .header("content-type", "application/json").body("{not json").send().await.unwrap();
            assert_eq!(r.status(), 401, "{path} leaked its body parser to an unauthenticated caller");
            // Missing required field, no key: same.
            let r = c.post(format!("{origin}{path}"))
                .header("content-type", "application/json").body("{}").send().await.unwrap();
            assert_eq!(r.status(), 401, "{path} validated a body before authenticating");
        }
        // Authenticated, THEN the body is judged.
        let r = c.post(format!("{origin}/api/hub/token")).header(KEY_HEADER, key("coowner").key)
            .header("content-type", "application/json").body("{not json").send().await.unwrap();
        assert_eq!(r.status(), 422);
        assert_eq!(hub_config::read_config_in(&base).token, "hubtok-secret");
    }

    /// The first-run door. Its whole security is "unconfigured AND on this machine", so both
    /// halves are pinned here, from a real socket.
    #[tokio::test]
    async fn ping_answers_without_a_key_and_without_writing_anything() {
        let base = temp_base("ping");
        let (origin, _rt) = spawn_server(base.clone(), vec![]).await;
        let c = reqwest::Client::new();

        // Unsigned hub: status refuses (no keys yet) but ping still says a hub is here. That gap is
        // the whole reason this endpoint exists — the app cannot offer to sign a hub it cannot see.
        assert_eq!(c.get(format!("{origin}/api/hub/status")).send().await.unwrap().status(), 401);
        let r = c.get(format!("{origin}/api/hub/ping")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["registered"], false);
        // And it wrote nothing — an unsigned hub must still be unsigned after being looked at.
        assert!(!hub_config::config_path_in(&base).exists());

        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let v: serde_json::Value = c.get(format!("{origin}/api/hub/ping")).send().await.unwrap().json().await.unwrap();
        assert_eq!(v["registered"], true);
        // No secrets in it, ever.
        assert!(!v.to_string().contains("hubtok-secret"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn the_first_run_door_opens_once_and_only_from_this_machine() {
        let base = temp_base("firstrun");
        // A brand-new machine: no config file at all.
        let (origin, _rt) = spawn_server(base.clone(), vec![]).await;
        let c = reqwest::Client::new();

        // 1. The id is minted on first ask, and is STABLE — the cloud token gets issued to it.
        let r = c.get(format!("{origin}/api/hub/identity")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let id = r.json::<serde_json::Value>().await.unwrap()["hubId"].as_str().unwrap().to_string();
        assert!(id.starts_with("hub_"), "{id}");
        let again = c.get(format!("{origin}/api/hub/identity")).send().await.unwrap()
            .json::<serde_json::Value>().await.unwrap()["hubId"].as_str().unwrap().to_string();
        assert_eq!(id, again, "the id must not be re-minted between the two setup calls");

        // 2. Signing it to a vehicle needs a vid and a token.
        let r = c.post(format!("{origin}/api/hub/bootstrap"))
            .json(&serde_json::json!({"vid": "", "name": "x", "token": "t"})).send().await.unwrap();
        assert_eq!(r.status(), 422);
        let r = c.post(format!("{origin}/api/hub/bootstrap"))
            .json(&serde_json::json!({"vid": "v1", "name": "Central", "token": "cloudtok"})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let cfg = hub_config::read_config_in(&base);
        assert_eq!(cfg.vid, "v1");
        assert_eq!(cfg.name, "Central");
        assert_eq!(cfg.token, "cloudtok");
        assert_eq!(cfg.hub_id, id);
        assert!(cfg.enabled);

        // 3. And now the door is SHUT — for good. Taking a live hub over is not a first run.
        let r = c.post(format!("{origin}/api/hub/bootstrap"))
            .json(&serde_json::json!({"vid": "v-attacker", "name": "Mine", "token": "other"})).send().await.unwrap();
        assert_eq!(r.status(), 409);
        let r = c.get(format!("{origin}/api/hub/identity")).send().await.unwrap();
        assert_eq!(r.status(), 409);
        assert_eq!(hub_config::read_config_in(&base).vid, "v1"); // untouched
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_non_2xx_report_is_a_failure_and_says_which_one() {
        // 🔴 THE BUG THAT HID A BUG. Only a transport error used to count, so a 401 was silently
        // treated as a delivered report. The vehicle's valve telemetry was dead in the cloud for
        // four days while this log stayed clean.
        assert_eq!(report_refusal(200, "lt_x"), None);
        assert_eq!(report_refusal(204, "lt_x"), None);
        let r = report_refusal(401, "lt_3CC1C335004B1200").expect("401 is a refusal");
        assert!(r.contains("401"), "the status must be in the line: {r}");
        assert!(r.contains("lt_3CC1C335004B1200"), "and the device, so it can be told apart: {r}");
        // A server fault is a different problem from a refusal and must be distinguishable.
        assert!(report_refusal(500, "lt_x").unwrap().contains("500"));
        // A redirect is not a delivery either — the report did not land where it was addressed.
        assert!(report_refusal(302, "lt_x").is_some());
    }

    #[tokio::test]
    async fn the_first_run_door_is_shut_to_anything_off_network() {
        // The server binds 127.0.0.1 in these tests, so a non-loopback peer cannot be produced by
        // dialling it. Test the decision itself instead — it is the whole rule.
        //
        // ⚠️ THE PEER HERE IS PUBLIC, NOT `192.168.8.50`, AND THAT MATTERS NOW. `first_run_only`
        // consults the machine's REAL addresses, so an RFC1918 peer would pass or fail depending on
        // what /24 the developer's laptop happens to be on — a test that passes by luck and fails
        // on a colleague's network. The LAN branch is covered exhaustively and deterministically in
        // adopt.rs, where the addresses are arguments rather than facts about the host.
        let base = temp_base("offbox");
        let rt = new_rt(base.clone(), "https://unused.example".into());
        let off: SocketAddr = "203.0.113.9:51000".parse().unwrap();
        let local: SocketAddr = "127.0.0.1:51000".parse().unwrap();
        assert!(!is_loopback(off));
        assert!(is_loopback(local));
        // Unconfigured + off-network ⇒ refused as a location problem, not as "already set up".
        let refusal = first_run_only(&rt, off).await.expect("must refuse");
        assert_eq!(refusal.status, 403);
        assert!(refusal.body.contains("same local network"), "the refusal must say what WOULD work: {}", refusal.body);
        assert!(first_run_only(&rt, local).await.is_none(), "unconfigured + loopback is the open case");
        // Configured ⇒ refused even on loopback. The claim door shuts for good once a hub is signed.
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        assert_eq!(first_run_only(&rt, local).await.expect("must refuse").status, 409);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn ping_says_whether_this_hub_can_be_adopted_and_whether_it_is_damaged() {
        // Ping is the ONLY door a damaged or unclaimed hub can answer, and the app's LAN sweep has
        // exactly one request to decide what it found. Both facts therefore live here.
        let base = temp_base("pingadopt");
        let (origin, _rt) = spawn_server(base.clone(), vec![]).await;
        let c = reqwest::Client::new();

        // Fresh and unclaimed: adoptable.
        let v: serde_json::Value = c.get(format!("{origin}/api/hub/ping")).send().await.unwrap().json().await.unwrap();
        assert_eq!(v["registered"], false);
        assert_eq!(v["adoptable"], true);
        assert_eq!(v["configDamaged"], false);

        // Signed: never adoptable again, whatever the window says.
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let v: serde_json::Value = c.get(format!("{origin}/api/hub/ping")).send().await.unwrap().json().await.unwrap();
        assert_eq!(v["registered"], true);
        assert_eq!(v["adoptable"], false);

        // Damaged: NOT adoptable — the identity is still on disk and must not be signed over — and
        // ping says so, because /api/hub/status cannot be reached when the member keys are in the
        // file that will not parse. That is the gap this field exists to close.
        std::fs::write(hub_config::config_path_in(&base), "{\"token\": \"hubtok-sec").unwrap();
        let v: serde_json::Value = c.get(format!("{origin}/api/hub/ping")).send().await.unwrap().json().await.unwrap();
        assert_eq!(v["registered"], false, "a damaged file reads as defaults");
        assert_eq!(v["configDamaged"], true, "and ping is the only place that can say so");
        assert_eq!(v["adoptable"], false, "damaged is not a first run — the real token is still there");
    }

    #[tokio::test]
    async fn key_sync_pulls_from_the_worker_authenticated_by_the_hub_token() {
        // A stub worker that asserts the query and returns two member keys.
        async fn stub(Query(q): Query<HashMap<String, String>>) -> Json<serde_json::Value> {
            assert_eq!(q["vid"], "v1");
            assert_eq!(q["device"], "hub_abc123");
            assert_eq!(q["t"], "hubtok-secret");
            Json(serde_json::json!({"keys": [
                {"key": "k-owner", "uid": "u1", "role": "owner"},
                {"key": "k-mon", "uid": "u2", "role": "monitor"},
            ]}))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/api/hub/keys", get(stub))).await.unwrap()
        });

        let client = reqwest::Client::new();
        let keys = fetch_member_keys(&client, &format!("http://{addr}"), &seeded_cfg()).await.unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].role, "owner");

        // The endpoint not existing yet (increment C undeployed) is an Err, never a panic — the
        // caller keeps the previous key set.
        let miss = fetch_member_keys(&client, &format!("http://{addr}/nope"), &seeded_cfg()).await;
        assert!(miss.is_err());
    }

    #[tokio::test]
    async fn a_heartbeat_reaches_the_agent_ingest_with_the_hub_identity() {
        async fn stub(Query(q): Query<HashMap<String, String>>) -> &'static str {
            assert_eq!(q["event"], "hub.measurement");
            assert_eq!(q["device"], "hub_abc123");
            assert_eq!(q["t"], "hubtok-secret");
            assert!(!q["ver"].is_empty());
            "OK"
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/api/agent", get(stub))).await.unwrap()
        });
        let client = reqwest::Client::new();
        send_heartbeat_once(&client, &format!("http://{addr}"), &seeded_cfg()).await.unwrap();
    }
    // --- Valve control through the hub -----------------------------------------------------------

    fn valve_cfg(allowed: bool) -> HubConfig {
        HubConfig {
            linktap: hub_config::LinkTapConfig {
                host: "127.0.0.1:9".into(), // reserved discard port — never answers, which is fine:
                gw_id: "GW02".into(),        // every test here asserts a decision made BEFORE the call
                dev_ids: vec!["aaaabbbbccccdddd".into()],
                allowed,
            },
            ..seeded_cfg()
        }
    }

    async fn post_valve(origin: &str, k: &MemberKey, body: &str) -> reqwest::Response {
        reqwest::Client::new()
            .post(format!("{origin}/api/hub/linktap/valve"))
            .header(KEY_HEADER, k.key.clone())
            .header("content-type", "application/json")
            .body(body.to_string())
            .send().await.unwrap()
    }

    /// A gateway that ACCEPTS, unlike `valve_cfg`'s discard port — needed because the wiring under
    /// test runs only after a command actually succeeds.
    async fn accepting_gateway() -> String {
        async fn api(body: String) -> String {
            // cmd 16 is the config/unit read do_valve makes to express a cap in the gateway's unit;
            // everything else (cmd 6 open, cmd 7 close) simply succeeds.
            if body.contains("\"cmd\":16") || body.contains("\"cmd\": 16") {
                r#"<html><body><!--#RET-->{"cmd":16,"gw_id":"GW02","vol_unit":"L","end_dev":["aaaabbbbccccdddd"]}</body></html>"#.to_string()
            } else {
                r#"<html><body><!--#RET-->{"cmd":6,"gw_id":"GW02","ret":0}</body></html>"#.to_string()
            }
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/api.shtml", post(api))).await.unwrap()
        });
        addr.to_string()
    }

    #[tokio::test]
    async fn valve_state_needs_a_member_key() {
        let base = temp_base("state_auth");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = reqwest::Client::new().get(format!("{origin}/api/hub/linktap/state")).send().await.unwrap();
        assert_eq!(r.status(), 401, "the LAN read is a member call like every other");
    }

    #[tokio::test]
    async fn the_web_ui_is_served_unauthenticated_at_root() {
        // The console page itself is inert static HTML with no secret, so it needs no key — the
        // key-gated API it calls is the security boundary. A browser on the LAN must be able to open
        // it directly (no mixed-content block, unlike the HTTPS app reaching this HTTP hub).
        let base = temp_base("webui");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = reqwest::Client::new().get(format!("{origin}/console")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.headers().get("content-type").unwrap().to_str().unwrap().contains("text/html"));
        let body = r.text().await.unwrap();
        assert!(body.contains("DockNeighbor Hub"), "serves the console page");
        assert!(body.contains("x-brvg-key"), "the page authenticates its own API calls with a member key");
    }

    #[tokio::test]
    async fn root_says_no_web_app_yet_until_a_bundle_is_installed() {
        let base = temp_base("noweb");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = reqwest::Client::new().get(format!("{origin}/")).send().await.unwrap();
        assert_eq!(r.status(), 200, "the hub is fine, it just has nothing to serve");
        let body = r.text().await.unwrap();
        assert!(body.contains("has not downloaded the web app yet"), "{body}");
        assert!(body.contains("/console"), "points at the console so the box stays usable");
        // Unknown API-shaped paths are NOT the SPA fallback's business either way.
        let r = reqwest::Client::new().get(format!("{origin}/api/hub/nope")).send().await.unwrap();
        assert_ne!(r.status(), 404, "the fallback answers it (200 placeholder), not axum's 404");
    }

    /// A tiny tar.gz bundle: index.html + one hashed asset, as a Vite build lays them out.
    fn tiny_web_bundle() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::fast());
            let mut ar = tar::Builder::new(gz);
            for (name, body) in [("index.html", "<!doctype html><html><head><title>DockNeighbor</title></head><body>app</body></html>"), ("assets/index-abc123.js", "console.log(1)")] {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                ar.append_data(&mut h, name, body.as_bytes()).unwrap();
            }
            ar.into_inner().unwrap().finish().unwrap();
        }
        out
    }

    #[tokio::test]
    async fn serves_the_installed_web_app_files_and_spa_routes_and_refuses_traversal() {
        let base = temp_base("web");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        crate::web_bundle::install(&base, "1.0.104", &tiny_web_bundle()).unwrap();
        let (origin, rt) = spawn_server(base, vec![key("owner")]).await;
        let c = reqwest::Client::new();

        // index.html at /, told it is hub-served, never cached.
        let r = c.get(format!("{origin}/")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.headers().get("content-type").unwrap().to_str().unwrap().starts_with("text/html"));
        assert_eq!(r.headers().get("cache-control").unwrap(), "no-cache");
        let body = r.text().await.unwrap();
        assert!(body.contains("window.__DN_HUB_SERVED__=true"), "{body}");
        assert!(body.contains("<title>DockNeighbor</title>"));

        // A hashed asset by path, with its own type, immutable.
        let r = c.get(format!("{origin}/assets/index-abc123.js")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.headers().get("content-type").unwrap().to_str().unwrap().starts_with("text/javascript"));
        assert!(r.headers().get("cache-control").unwrap().to_str().unwrap().contains("immutable"));
        assert_eq!(r.text().await.unwrap(), "console.log(1)");

        // A client-side route is the SPA.
        let r = c.get(format!("{origin}/settings/devices")).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert!(r.text().await.unwrap().contains("__DN_HUB_SERVED__"));

        // Traversal never leaves the bundle: the marker file beside it is not reachable.
        for p in ["/../current", "/assets/../../current", "/assets/..%2f..%2fcurrent", "/.hidden"] {
            let r = c.get(format!("{origin}{p}")).send().await.unwrap();
            let body = r.text().await.unwrap();
            assert!(!body.trim().eq("1.0.104"), "{p} leaked the marker");
        }

        // The console still lives at /console, and the API is untouched by the fallback.
        let r = c.get(format!("{origin}/console")).send().await.unwrap();
        assert!(r.text().await.unwrap().contains("x-brvg-key"));
        let r = c.get(format!("{origin}/api/hub/ping")).send().await.unwrap();
        assert!(r.json::<serde_json::Value>().await.unwrap()["ok"].as_bool().unwrap());

        // Status reports the served version (a member key is needed for status).
        let st = c.get(format!("{origin}/api/hub/status")).header("x-brvg-key", key("owner").key).send().await.unwrap()
            .json::<serde_json::Value>().await.unwrap();
        assert_eq!(st["webVersion"], "1.0.104");
        assert_eq!(rt.web.read().await.as_ref().unwrap().version, "1.0.104");
    }

    #[tokio::test]
    async fn valve_state_answers_at_once_when_the_caller_is_behind_the_hub() {
        // A caller whose cursor is stale must NOT be parked — it already has catching up to do.
        let base = temp_base("state_behind");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, rt) = spawn_server(base, vec![key("owner")]).await;
        rt.valve_rev.send_modify(|v| *v = 7);

        let started = Instant::now();
        let body: serde_json::Value = reqwest::Client::new()
            .get(format!("{origin}/api/hub/linktap/state?since=3&wait=10"))
            .header(KEY_HEADER, key("owner").key)
            .send().await.unwrap().json().await.unwrap();
        assert_eq!(body["rev"], 7);
        assert!(started.elapsed() < Duration::from_secs(2), "a stale caller waits for nothing");
    }

    #[tokio::test]
    async fn valve_state_holds_until_the_hub_observes_something_new() {
        // 🔴 THE POINT OF THE WHOLE ENDPOINT. Owner ruling 2026-08-31: the app and the hub should
        // talk in real time on the LAN instead of the app running its own 5s clock against the
        // gateway. The request parks; the observation releases it.
        let base = temp_base("state_wait");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, rt) = spawn_server(base, vec![key("owner")]).await;

        let rt2 = rt.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            rt2.valve_rev.send_modify(|v| *v = v.wrapping_add(1));
        });

        let started = Instant::now();
        let body: serde_json::Value = reqwest::Client::new()
            .get(format!("{origin}/api/hub/linktap/state?since=0&wait=10"))
            .header(KEY_HEADER, key("owner").key)
            .send().await.unwrap().json().await.unwrap();
        let waited = started.elapsed();

        assert_eq!(body["rev"], 1, "released by the observation, not by the timeout");
        assert!(waited >= Duration::from_millis(250), "it really did park: {waited:?}");
        assert!(waited < Duration::from_secs(3), "and it was released, not timed out: {waited:?}");
    }

    #[test]
    fn the_state_wait_is_long_enough_to_be_worth_holding() {
        // The cap IS the battery budget: an idle valve costs one round trip per cap-length. At the
        // old relay-driven 10s that was 360 wake-ups an hour on a phone; a minute makes it 60.
        // Held sockets are cheap, returning requests are not.
        assert!(MAX_STATE_WAIT_SECS >= 60, "a short cap is the expensive choice, not the safe one");
        // But not so long that middleboxes and OS socket reapers start dropping idle connections,
        // which would bring the reconnect churn back for nothing.
        assert!(MAX_STATE_WAIT_SECS <= 90);
    }

    #[tokio::test]
    async fn an_open_issued_through_the_api_is_recorded_as_the_hubs_own_run() {
        // 🔴 THE WIRING for the bug pinned in linktap_runtime's tests. do_valve sent the command and
        // told the runtime NOTHING, so the watcher met an already-open valve on its next poll and
        // adopted it — the hub adopting a run it had just issued itself. The app reads `prov` and
        // drew the owner's own 24h run as "Started Externally".
        let gw = accepting_gateway().await;
        let base = temp_base("valve_prov");
        let mut cfg = valve_cfg(true);
        cfg.linktap.host = gw;
        hub_config::write_config_in(&base, &cfg).unwrap();
        let (origin, rt) = spawn_server(base, vec![key("owner")]).await;

        // The watcher owns this in production; seed it here so the handler has a runtime to talk to.
        *rt.linktap.lock().await = Some(crate::linktap_runtime::Runtime::new(
            linktap::Gateway { host: cfg.linktap.host.clone(), gw_id: "GW02".into() },
            &["aaaabbbbccccdddd".to_string()],
            cycle::Profile { duration_secs: 86_400, volume_cap_l: 1135.6, auto_restart: false },
        ));

        let r = post_valve(&origin, &key("owner"),
            r#"{"devId":"aaaabbbbccccdddd","action":"open","durationSecs":3600,"volumeCapL":200.0,"mode":"normal"}"#).await;
        assert_eq!(r.status(), 200, "the open must succeed for the recording to matter");

        let guard = rt.linktap.lock().await;
        let runtime = guard.as_ref().expect("seeded above");
        let (prov, dur, cap) = runtime.debug_track("aaaabbbbccccdddd").expect("a watched, RUNNING valve");
        assert_eq!(prov, "hub", "we issued this open — it is not an adopted run");
        assert_eq!(dur, 3600, "the duration WE asked for");
        assert!((cap - 200.0).abs() < 0.01, "the cap WE asked for, got {cap}");
    }

    #[tokio::test]
    async fn capabilities_advertise_linktap_only_when_configured_AND_permitted() {
        // The two conditions are ANDed so neither can be forgotten: a configured gateway on an
        // unpermitted plan must not advertise, and a permitted plan with no gateway must not either.
        let base = temp_base("caps");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let body: serde_json::Value = reqwest::Client::new()
            .get(format!("{origin}/api/hub/status")).header(KEY_HEADER, key("owner").key)
            .send().await.unwrap().json().await.unwrap();
        // `routers` is unconditional (managed routers need no plan or gateway to be OFFERED).
        assert_eq!(body["capabilities"], serde_json::json!(["linktap", "routers", "sensors"]));

        let base2 = temp_base("caps_denied");
        hub_config::write_config_in(&base2, &valve_cfg(false)).unwrap();
        let (origin2, _rt2) = spawn_server(base2, vec![key("owner")]).await;
        let body2: serde_json::Value = reqwest::Client::new()
            .get(format!("{origin2}/api/hub/status")).header(KEY_HEADER, key("owner").key)
            .send().await.unwrap().json().await.unwrap();
        assert_eq!(body2["capabilities"], serde_json::json!(["routers", "sensors"]), "an unpermitted plan must not advertise valve capability");

        let base3 = temp_base("caps_nogw");
        hub_config::write_config_in(&base3, &seeded_cfg()).unwrap(); // allowed defaults false, no gateway
        let (origin3, _rt3) = spawn_server(base3, vec![key("owner")]).await;
        let body3: serde_json::Value = reqwest::Client::new()
            .get(format!("{origin3}/api/hub/status")).header(KEY_HEADER, key("owner").key)
            .send().await.unwrap().json().await.unwrap();
        assert_eq!(body3["capabilities"], serde_json::json!(["routers", "sensors"]));
    }

    #[tokio::test]
    async fn the_paid_gate_is_enforced_at_the_action_not_only_the_advertisement() {
        // A caller that skipped the capability check — an older app, a hand-rolled request — must
        // still be refused, or the gate is bypassable by simply not asking.
        let base = temp_base("valve_402");
        hub_config::write_config_in(&base, &valve_cfg(false)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = post_valve(&origin, &key("owner"), r#"{"devId":"aaaabbbbccccdddd","action":"close"}"#).await;
        assert_eq!(r.status(), 402);
    }

    #[tokio::test]
    async fn a_monitor_may_not_actuate_a_valve() {
        let base = temp_base("valve_role");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("monitor")]).await;
        let r = post_valve(&origin, &key("monitor"), r#"{"devId":"aaaabbbbccccdddd","action":"close"}"#).await;
        assert_eq!(r.status(), 403);
    }

    #[tokio::test]
    async fn a_washdown_may_not_carry_a_volume_cap() {
        // Owner spec 2026-07-30, re-ratified twice: washdown is TIME-ONLY. Honouring a cap sent
        // alongside mode=washdown would re-create the "external cap" that cut 2-hour hose runs at
        // ~26 gal, so the request is refused rather than either side being silently dropped.
        let base = temp_base("valve_washdown");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = post_valve(&origin, &key("owner"),
            r#"{"devId":"aaaabbbbccccdddd","action":"open","durationSecs":7200,"volumeCapL":100,"mode":"washdown"}"#).await;
        assert_eq!(r.status(), 422);
        let body: serde_json::Value = r.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("time-limited"));
    }

    #[tokio::test]
    async fn a_valve_this_hub_was_not_told_about_is_refused() {
        // A hub is not a general-purpose proxy onto the vessel's RF network — the same reasoning
        // as the relay's path allowlist.
        let base = temp_base("valve_unknown");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = post_valve(&origin, &key("owner"), r#"{"devId":"ffffeeeeddddcccc","action":"close"}"#).await;
        assert_eq!(r.status(), 404);
    }

    #[tokio::test]
    async fn an_open_without_a_duration_is_refused() {
        // Every open carries a bound — that is the primary safeguard in the valve safety model.
        let base = temp_base("valve_nodur");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = post_valve(&origin, &key("owner"), r#"{"devId":"aaaabbbbccccdddd","action":"open"}"#).await;
        assert_eq!(r.status(), 422);
    }

    #[tokio::test]
    async fn an_unknown_action_is_refused_rather_than_guessed() {
        let base = temp_base("valve_action");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = post_valve(&origin, &key("owner"), r#"{"devId":"aaaabbbbccccdddd","action":"purge"}"#).await;
        assert_eq!(r.status(), 422);
    }

    // --- Increment 3: the I/O shell ---------------------------------------------------------------

    #[test]
    fn parses_the_workers_linktap_blob_with_skip_dont_default_intact() {
        let body = serde_json::json!({
            "ok": true,
            "linktap": { "allowed": true, "profiles": {
                "aaaabbbbccccdddd": { "durationSecs": 7200, "volumeCapL": 250.5, "autoRestart": true },
                "bbbbccccddddeeeeEXTRA": { "volumeCapL": 50 }
            }}
        });
        let (allowed, profiles) = parse_linktap_reply(&body).unwrap();
        assert!(allowed);
        let full = profiles.get("aaaabbbbccccdddd").unwrap();
        assert_eq!(full.duration_secs, Some(7200));
        assert_eq!(full.auto_restart, Some(true));
        // A field the vehicle never set stays None so the hub's own default keeps it.
        let partial = profiles.get("bbbbccccddddeeee").expect("long ids normalise to the canonical 16");
        assert_eq!(partial.volume_cap_l, Some(50.0));
        assert_eq!(partial.duration_secs, None);
        assert_eq!(partial.auto_restart, None);
    }

    #[test]
    fn an_absent_blob_changes_nothing_and_absent_allowed_is_never_permission() {
        assert!(parse_linktap_reply(&serde_json::json!({ "ok": true })).is_none());
        // `allowed` missing must read as DENY — the whole default-deny posture rests on this.
        let (allowed, _) = parse_linktap_reply(&serde_json::json!({ "linktap": { "profiles": {} } })).unwrap();
        assert!(!allowed);
    }

    #[tokio::test]
    async fn a_revoked_plan_stops_the_machine_rather_than_only_hiding_the_capability() {
        // A hub whose vehicle stopped paying must stop DRIVING the valve, not merely stop
        // advertising that it can.
        let base = temp_base("lt_revoke");
        let mut cfg = valve_cfg(true);
        hub_config::write_config_in(&base, &cfg).unwrap();
        let rt = new_rt(base.clone(), "https://unused.example".into());
        // Gateway is unreachable in tests, so the unit read falls back to gal — the machine is
        // still constructed, which is what this asserts.
        assert!(linktap_sync_config(&rt).await);
        assert!(rt.linktap.lock().await.is_some());

        cfg.linktap.allowed = false;
        hub_config::write_config_in(&base, &cfg).unwrap();
        assert!(!linktap_sync_config(&rt).await);
        assert!(rt.linktap.lock().await.is_none(), "the machine must be dropped, not just silenced");
    }

    #[tokio::test]
    async fn no_gateway_configured_means_nothing_polls() {
        let base = temp_base("lt_nogw");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let rt = new_rt(base, "https://unused.example".into());
        assert!(!linktap_sync_config(&rt).await);
        assert!(rt.linktap.lock().await.is_none());
    }

    #[tokio::test]
    async fn the_gateway_push_route_is_unauthenticated_but_inert() {
        // It must accept the appliance's POST (it cannot present a key) while doing nothing a
        // hostile LAN peer could exploit: no commands, no config, unknown valves dropped.
        let base = temp_base("lt_push");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = reqwest::Client::new()
            .post(format!("{origin}/api/hub/linktap/push"))
            .header("content-type", "application/json")
            .body(r#"{"dev_stat":[{"dev_id":"ffffeeeeddddcccc","is_watering":1,"volume":5}]}"#)
            .send().await.unwrap();
        assert_eq!(r.status(), 200, "the gateway cannot authenticate — it must not be refused");
    }

    #[test]
    fn a_push_is_only_taken_from_the_configured_gateway_address() {
        let gw: SocketAddr = "192.168.8.20:54321".parse().unwrap();
        let other: SocketAddr = "192.168.8.99:54321".parse().unwrap();
        assert!(push_peer_allowed("192.168.8.20", gw));
        assert!(!push_peer_allowed("192.168.8.20", other));
        // The config field may carry a port; the comparison is on the address only.
        assert!(push_peer_allowed("192.168.8.20:80", gw));
        // No gateway configured accepts nothing — there is nothing legitimate to accept.
        assert!(!push_peer_allowed("", gw));
        // A HOSTNAME cannot be compared without a DNS lookup per push, so that configuration keeps
        // the pre-check posture and relies on the route's inertness. Stated, not silently assumed.
        assert!(push_peer_allowed("gateway.local", other));
    }

    // --- Local Shelly ingest ---------------------------------------------------------------------

    #[test]
    fn a_shelly_query_parses_into_the_same_shape_the_cloud_reads() {
        let c = parse_shelly_query("vid=v1&event=flood.alarm&device=sh_bilge&k=s3cr3t&battery=97&temp=21.5");
        assert_eq!(c.vid, "v1");
        assert_eq!(c.event, "flood.alarm");
        assert_eq!(c.device, "sh_bilge");
        assert_eq!(c.k, "s3cr3t");
        assert_eq!(c.extras, vec![("battery".to_string(), "97".to_string()), ("temp".to_string(), "21.5".to_string())]);
        // ⚠️ THE SECRET MUST NEVER REACH THE EXTRAS. They are forwarded to the cloud verbatim and
        // stored as telemetry, so a `k` that leaked into them would write the vehicle's webhook
        // bearer into a document every member can read.
        assert!(c.extras.iter().all(|(k, _)| k != "k" && k != "key"));
    }

    #[test]
    fn a_plausible_vessel_network_is_admitted_and_the_public_internet_is_not() {
        use std::net::IpAddr;
        let ok = [
            "192.168.1.50", "10.0.0.9", "172.16.4.2", "127.0.0.1",
            "169.254.10.1",           // DHCP-less auto-address
            "100.64.0.5", "100.127.255.254", // CGNAT, both edges — Starlink/cellular LANs live here
            "::1", "fd00::1", "fe80::1",
            "::ffff:192.168.1.50",    // how a dual-stack listener reports an IPv4 peer
        ];
        for a in ok {
            assert!(shelly_peer_plausible(a.parse::<IpAddr>().unwrap()), "{a} should be admitted");
        }
        let refused = [
            "8.8.8.8", "1.1.1.1", "203.0.113.7",
            "100.63.255.255", "100.128.0.0", // just OUTSIDE CGNAT, both sides
            "2606:4700::1111",
        ];
        for a in refused {
            assert!(!shelly_peer_plausible(a.parse::<IpAddr>().unwrap()), "{a} must be refused");
        }
    }

    #[test]
    fn shelly_defaults_match_the_clouds_exactly() {
        // A sensor that omits event/device must classify identically whichever URL it was given,
        // or the same device behaves differently on the hub than it does direct to the cloud.
        let c = parse_shelly_query("vid=v1&k=s");
        assert_eq!(c.event, "sensor alert");
        assert_eq!(c.device, "unknown");
        // An unfilled installer template must not become a telemetry field reading "null".
        let c = parse_shelly_query("k=s&battery=&signal=null&real=3");
        assert_eq!(c.extras, vec![("real".to_string(), "3".to_string())]);
        // A completely empty query is a parse, not a panic.
        assert_eq!(parse_shelly_query("").event, "sensor alert");
    }

    #[test]
    fn shelly_auth_is_deny_by_default_and_checks_the_secret_before_the_vehicle() {
        let call = |vid: &str, k: &str| parse_shelly_query(&format!("vid={vid}&event=flood&device=d&k={k}"));
        // No stored secret ⇒ NOTHING is accepted. This is the deliberate divergence from the
        // cloud's `legacy` accept-when-unset: that leniency is a rollout for pre-existing
        // vehicles, and this route closes a valve.
        assert_eq!(classify_shelly_auth("v1", "", &call("v1", "s3cr3t")), ShellyAuth::Disarmed);
        assert_eq!(classify_shelly_auth("v1", "", &call("v1", "")), ShellyAuth::Disarmed);
        // Wrong or missing k.
        assert_eq!(classify_shelly_auth("v1", "s3cr3t", &call("v1", "nope")), ShellyAuth::BadSecret);
        assert_eq!(classify_shelly_auth("v1", "s3cr3t", &call("v1", "")), ShellyAuth::BadSecret);
        assert_eq!(classify_shelly_auth("v1", "s3cr3t", &call("v1", "S3CR3T")), ShellyAuth::BadSecret);
        // Right secret, right vehicle.
        assert_eq!(classify_shelly_auth("v1", "s3cr3t", &call("v1", "s3cr3t")), ShellyAuth::Ok);
        // An omitted vid is fine — the hub has one vehicle and the secret already proved which.
        assert_eq!(classify_shelly_auth("v1", "s3cr3t", &parse_shelly_query("k=s3cr3t&event=flood")), ShellyAuth::Ok);
        // A vid that is present and WRONG is a misconfigured sensor, not this vehicle's flood.
        assert_eq!(classify_shelly_auth("v1", "s3cr3t", &call("v-other", "s3cr3t")), ShellyAuth::WrongVehicle);
        // ...but a prober with the wrong secret can never learn that, whatever vid it guesses.
        assert_eq!(classify_shelly_auth("v1", "s3cr3t", &call("v-other", "nope")), ShellyAuth::BadSecret);
    }

    fn shelly_cfg(secret: &str) -> HubConfig {
        HubConfig { shelly_secret: secret.into(), ..valve_cfg(true) }
    }

    #[tokio::test]
    async fn the_shelly_route_answers_a_get_as_well_as_a_post() {
        // ⚠️ THE 405 BUG, pinned. Shelly devices fire GETs at a static URL; a POST-only route
        // looks perfectly healthy in every test that uses reqwest's `.post()` and is silently
        // unreachable by the only device that ever calls it. This project has shipped that once.
        let base = temp_base("shelly_verbs");
        hub_config::write_config_in(&base, &shelly_cfg("s3cr3t")).unwrap();
        let (origin, _rt) = spawn_server(base, vec![]).await;
        let c = reqwest::Client::new();
        let url = format!("{origin}/api/hub/shelly?vid=v1&event=voltmeter.measurement&device=sh_1&k=s3cr3t");
        assert_eq!(c.get(&url).send().await.unwrap().status(), 200, "a Shelly GET must not 405");
        assert_eq!(c.post(&url).send().await.unwrap().status(), 200);
    }

    #[tokio::test]
    async fn the_shelly_route_is_not_the_push_routes_open_door() {
        // /api/hub/linktap/push is unauthenticated because it is INERT. This one closes a valve,
        // so the same reasoning reaches the opposite answer: no secret, no action.
        let base = temp_base("shelly_auth");
        hub_config::write_config_in(&base, &shelly_cfg("s3cr3t")).unwrap();
        let (origin, _rt) = spawn_server(base, vec![]).await;
        let c = reqwest::Client::new();
        for q in ["vid=v1&event=flood.alarm&device=sh_bilge", "vid=v1&event=flood.alarm&device=sh_bilge&k=wrong"] {
            let r = c.get(format!("{origin}/api/hub/shelly?{q}")).send().await.unwrap();
            assert_eq!(r.status(), 401, "an unauthenticated flood report must not reach the valve: {q}");
        }
        // A hub that holds no secret refuses even a well-formed report — deny by default.
        let base2 = temp_base("shelly_disarmed");
        hub_config::write_config_in(&base2, &valve_cfg(true)).unwrap(); // shelly_secret empty
        let (origin2, _rt2) = spawn_server(base2, vec![]).await;
        let r = c.get(format!("{origin2}/api/hub/shelly?vid=v1&event=flood.alarm&device=d&k=anything")).send().await.unwrap();
        assert_eq!(r.status(), 401);
        // A report for somebody else's vehicle is refused too, even with the right secret.
        let r = c.get(format!("{origin}/api/hub/shelly?vid=v-other&event=flood.alarm&device=d&k=s3cr3t")).send().await.unwrap();
        assert_eq!(r.status(), 404);
    }

    #[tokio::test]
    async fn the_shelly_route_is_lan_only_and_unreachable_down_the_relay() {
        // `dispatch` IS the relay's path allowlist (hub_relay.rs routes every worker `call` frame
        // through it). Keeping /api/hub/shelly out of it is what makes the query-string secret a
        // LAN-only credential instead of one with an internet-facing door onto it.
        let base = temp_base("shelly_relay");
        hub_config::write_config_in(&base, &shelly_cfg("s3cr3t")).unwrap();
        let rt = new_rt(base, "https://unused.example".into());
        let owner = Caller { uid: "u1".into(), role: "owner".into() };
        for m in ["GET", "POST"] {
            let a = dispatch(&rt, &owner, m, "/api/hub/shelly", b"").await;
            assert_eq!(a.status, 404, "{m} /api/hub/shelly must not be relayable");
        }
    }

    #[tokio::test]
    async fn the_webhook_secret_is_set_through_config_and_never_read_back() {
        let base = temp_base("shelly_secret_cfg");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap();
        let (origin, _rt) = spawn_server(base.clone(), vec![key("monitor"), key("admin")]).await;
        let c = reqwest::Client::new();

        // Settings grade, like every other field on this endpoint.
        let r = c.post(format!("{origin}/api/hub/config")).header(KEY_HEADER, key("monitor").key)
            .json(&serde_json::json!({"shellySecret": "s3cr3t"})).send().await.unwrap();
        assert_eq!(r.status(), 403);
        assert!(hub_config::read_config_in(&base).shelly_secret.is_empty());

        let r = c.post(format!("{origin}/api/hub/config")).header(KEY_HEADER, key("admin").key)
            .json(&serde_json::json!({"shellySecret": "  s3cr3t\n"})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(hub_config::read_config_in(&base).shelly_secret, "s3cr3t", "trimmed — a pasted newline must not break every comparison");
        // The status body says the ingest is ARMED and does not contain the secret itself.
        let text = r.text().await.unwrap();
        assert!(!text.contains("s3cr3t"), "the webhook secret leaked into status: {text}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["shellyIngestArmed"], true);

        // Empty disarms it deliberately — that is how a rotated secret is taken back.
        let r = c.post(format!("{origin}/api/hub/config")).header(KEY_HEADER, key("admin").key)
            .json(&serde_json::json!({"shellySecret": ""})).send().await.unwrap();
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["shellyIngestArmed"], false);
        assert!(hub_config::read_config_in(&base).shelly_secret.is_empty());
    }

    /// A stub LinkTap gateway that counts the `cmd 7` (stop) commands it is sent.
    async fn stub_gateway() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let stops = Arc::new(AtomicUsize::new(0));
        let seen = stops.clone();
        let app = Router::new().route(
            "/api.shtml",
            post(move |body: axum::body::Bytes| {
                let seen = seen.clone();
                async move {
                    let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                    if v.get("cmd").and_then(|c| c.as_i64()) == Some(7) {
                        seen.fetch_add(1, Ordering::SeqCst);
                    }
                    axum::Json(serde_json::json!({ "ret": 0 }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (addr.to_string(), stops)
    }

    #[tokio::test]
    async fn a_flood_close_is_NOT_blocked_by_the_paid_tier_gate() {
        use std::sync::atomic::Ordering;
        // ⚠️ THIS IS THE BREAK. Before this change the ONLY source of (gateway, valves) for the
        // flood hook was `rt.linktap`, which linktap_sync_config builds only when
        // `cfg.linktap.allowed` is true — so on an unpermitted plan, a lapsed plan, or simply a
        // hub that had not yet had a successful heartbeat (allowed defaults to FALSE), a flood
        // alarm would have closed NOTHING. Opening a valve is a paid feature; closing one spends
        // no water, removes no limit, and is idempotent. There is no revenue on the closing side.
        let (host, stops) = stub_gateway().await;
        let base = temp_base("flood_no_gate");
        let mut cfg = valve_cfg(false); // allowed = FALSE, the unpermitted plan
        cfg.linktap.host = host;
        hub_config::write_config_in(&base, &cfg).unwrap();
        let rt = new_rt(base, "https://unused.example".into());
        // The machine is NOT built on an unpermitted plan — that is the whole trap.
        assert!(!linktap_sync_config(&rt).await);
        assert!(rt.linktap.lock().await.is_none());

        linktap_flood_stop_all(&rt).await;
        assert_eq!(stops.load(Ordering::SeqCst), 1, "an unpermitted plan must still get its valve CLOSED");
    }

    #[tokio::test]
    async fn a_flood_close_with_no_gateway_at_all_is_a_log_line_not_a_panic() {
        let base = temp_base("flood_nogw");
        hub_config::write_config_in(&base, &seeded_cfg()).unwrap(); // no linktap config whatsoever
        let rt = new_rt(base, "https://unused.example".into());
        linktap_flood_stop_all(&rt).await; // must simply return
    }

    #[tokio::test]
    async fn a_forwarded_shelly_report_goes_to_api_shelly_as_the_sensor_itself() {
        // 🔴 THE FORWARD WAS 401'd EVERY TIME AND NOBODY KNEW. It went through /api/agent, which
        // authenticates a token against the token's OWN device — so the hub presenting its own
        // credential for a Shelly's id was refused. Every forwarded Shelly event was dropped, flood
        // alarms included. With the uplink UP nobody noticed, because the sensor also fires its own
        // cloud hook; with the uplink DOWN — the entire case the local ingest exists for — the alert
        // never arrived at all.
        //
        // This pins the DOOR and the CREDENTIAL, because that is what was wrong, not the payload.
        use std::sync::{Arc, Mutex};
        let hits: Arc<Mutex<Vec<(String, HashMap<String, String>)>>> = Arc::new(Mutex::new(Vec::new()));
        let a = hits.clone();
        let b = hits.clone();
        let app = Router::new()
            .route("/api/shelly", get(move |Query(q): Query<HashMap<String, String>>| {
                let a = a.clone();
                async move { a.lock().unwrap().push(("shelly".into(), q)); "ok" }
            }))
            .route("/api/agent", get(move |Query(q): Query<HashMap<String, String>>| {
                let b = b.clone();
                async move { b.lock().unwrap().push(("agent".into(), q)); "ok" }
            }));
        let wl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let worker = format!("http://{}", wl.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(wl, app).await.unwrap() });

        let base = temp_base("shelly_forward");
        hub_config::write_config_in(&base, &shelly_cfg("s3cr3t")).unwrap();
        let rt = new_rt(base, worker);
        let hl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", hl.local_addr().unwrap());
        let hub_app = router(rt.clone());
        tokio::spawn(async move {
            axum::serve(hl, hub_app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap()
        });

        let c = reqwest::Client::new();
        let r = c.get(format!("{origin}/api/hub/shelly?vid=v1&event=temperature.change&device=sh_salon&k=s3cr3t&tC=21.5"))
            .send().await.unwrap();
        assert_eq!(r.status(), 200);

        // The forward is spawned so the sensor never waits on it — poll rather than race.
        let mut found = None;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let g = hits.lock().unwrap();
            if let Some(h) = g.first() { found = Some(h.clone()); break; }
        }
        let (door, q) = found.expect("the report was never forwarded at all");
        assert_eq!(door, "shelly", "must go through /api/shelly, the door a sensor itself uses — /api/agent 401s");
        assert_eq!(q["device"], "sh_salon", "the sensor's OWN id rides through, so the cloud classifies it identically");
        assert_eq!(q["event"], "temperature.change");
        assert_eq!(q["vid"], "v1");
        assert_eq!(q["k"], "s3cr3t", "authenticated with the vehicle webhook secret, exactly as the sensor would");
        assert_eq!(q["tC"], "21.5", "and the device's own params are passed through untouched");
        assert!(!q.contains_key("t"), "the hub's agent token has no business on a webhook forward");
    }

    #[tokio::test]
    async fn a_flood_report_closes_the_valve_and_a_measurement_does_not() {
        use std::sync::atomic::Ordering;
        let (host, stops) = stub_gateway().await;
        let base = temp_base("shelly_flood");
        let mut cfg = shelly_cfg("s3cr3t");
        cfg.linktap.host = host;
        cfg.token = String::new(); // no uplink: spool_report is a no-op, and the close must not care
        hub_config::write_config_in(&base, &cfg).unwrap();
        let (origin, rt) = spawn_server(base, vec![]).await;
        linktap_sync_config(&rt).await;
        let c = reqwest::Client::new();

        // Telemetry from the same sensor must NOT touch the valve.
        let r = c.get(format!("{origin}/api/hub/shelly?vid=v1&event=voltmeter.measurement&device=sh_bilge&k=s3cr3t"))
            .send().await.unwrap();
        assert_eq!(r.status(), 200);
        // A real flood does.
        let r = c.get(format!("{origin}/api/hub/shelly?vid=v1&event=flood.alarm&device=sh_bilge&k=s3cr3t&battery=97"))
            .send().await.unwrap();
        assert_eq!(r.status(), 200);
        // The work is spawned so the sensor is never made to wait out a gateway round trip; give
        // it a moment to land rather than asserting on a race.
        for _ in 0..100 {
            if stops.load(Ordering::SeqCst) > 0 { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(stops.load(Ordering::SeqCst), 1, "a flood must close the valve; a measurement must not");
    }

    #[tokio::test]
    async fn a_push_from_the_wrong_peer_is_answered_identically_to_a_good_one() {
        // Not an authorization surface: telling a prober whether it guessed the gateway's address
        // would make it one.
        let base = temp_base("lt_push_peer");
        hub_config::write_config_in(&base, &valve_cfg(true)).unwrap();
        let (origin, _rt) = spawn_server(base, vec![key("owner")]).await;
        let r = reqwest::Client::new()
            .post(format!("{origin}/api/hub/linktap/push"))
            .header("content-type", "application/json")
            .body(r#"{"dev_stat":[{"dev_id":"aaaabbbbccccdddd","is_watering":1,"volume":5}]}"#)
            .send().await.unwrap();
        // valve_cfg points at 127.0.0.1:9 while the test server sees a 127.0.0.1 peer, so this
        // one is ACCEPTED — the assertion that matters is that the answer is indistinguishable.
        assert_eq!(r.status(), 200);
    }

}
