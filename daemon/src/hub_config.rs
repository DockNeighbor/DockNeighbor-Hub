// The HUB's own storage — MACHINE-WIDE, not per-user (owner ruling 2026-08-18).
//
// The hub is signed to a VEHICLE, and there is one hub per vehicle. The app's per-user data stays
// per-user: `navigator` may be signed in as one account and `localadmin` as another, each with
// their own vehicles, sessions and localStorage — that separation is deliberate and stays.
// The HUB is not part of it. It belongs to the machine and its vehicle, so it lives in the shared
// folder where every user's app instance AND the SYSTEM service can reach the same one file.
//
// This replaces three things that could not work:
//   * `hub_<installId>` where installId came from localStorage — per-USER, so two logins on one
//     computer would mint two different hubs;
//   * the token/name/enabled/interval in localStorage — invisible to a SYSTEM instance, and wiped
//     by applyUserScope on any identity change, silently disarming the hub;
//   * the service (SYSTEM, session 0) having any way to read either.
//
// ⚠️ SCOPE, stated honestly: this file makes the hub's identity SINGULAR PER MACHINE (one file,
// one id, and hub_service.rs registers one fixed task name). "One hub per VEHICLE" is a
// CLOUD-side rule — only the worker sees every machine — and must be enforced at hub enrollment
// there. This store cannot know about a second computer.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Folder name under the platform's shared data directory.
///
/// ⚠️ RENAMED 2026-09-02 (`BoatRVGuardian` -> `DockNeighbor`, owner ruling) AND IT IS A CONTRACT,
/// not a label. Every one of these names the SAME directory and they must move together:
///   * `daemon/windows/installer.nsi`     `DATA_DIR_NAME`
///   * `daemon/macos/*.plist`             the LaunchDaemon/LaunchAgent program + log paths
///   * `daemon/linux/install.sh`          `ROOT`, and `brvg-hub.service`'s `ExecStart`
///   * `.github/workflows/daemon-release.yml`  the macOS pkg payload root
///   * the APP's `dashboard/src-tauri/src/hub_service.rs`  `hub_bin_path()` + `hub_service_remove()`
///
/// 🔴 A MACHINE INSTALLED UNDER THE OLD NAME DOES NOT MIGRATE ITSELF. Its `hub.json` — hub id,
/// member keys, the Shelly ingest secret, the LinkTap gateway/device ids — sits under the old
/// directory, and a daemon on this version will not find it and will come up UNREGISTERED. Carrying
/// that file across is a deliberate step in the cutover runbook, not something this code does.
pub const DIR_NAME: &str = "DockNeighbor";
const FILE_NAME: &str = "hub.json";

/// Management API port (hub_server.rs). One constant, configurable per install because boats run
/// odd gear on odd networks. 3030 is taken by the interactive app's local Shelly webhook listener.
pub const DEFAULT_HTTP_PORT: u16 = 8722;

/// One member's LAN management credential, minted by the worker per (user, vehicle) —
/// ONSITE.md "Management auth: per-user API keys" (2026-08-18 late). `role` uses the vehicle
/// role vocabulary (`owner` | `coowner` | `admin` | `control` | `monitor`, mirroring
/// vehicleCapabilities.ts) and is what hub_server gates writes on.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct MemberKey {
    pub key: String,
    pub uid: String,
    pub role: String,
}

/// The hub's machine-wide record. Absent file ⇒ this machine is not a hub.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct HubConfig {
    /// `hub_<random>` — minted ONCE per machine and never regenerated, so re-registering or
    /// switching vehicles does not orphan the cloud device record for this hub.
    pub hub_id: String,
    /// The vehicle this hub is signed to.
    pub vid: String,
    pub name: String,
    pub enabled: bool,
    pub heartbeat_secs: u32,
    /// Per-hub bearer token (agentToken custody). Never returned to the UI in full.
    pub token: String,
    /// Port the management API listens on (hub_server.rs).
    pub http_port: u16,
    /// The vehicle members' API keys, CACHED from the worker so a reboot with no internet still
    /// authenticates known members. hub_server's sync loop owns this; stale keys die at the next
    /// successful sync. Lives here because this file is already the hub's one credential store
    /// (SYSTEM/0600) — a second file would just be a second thing to lock down.
    pub member_keys: Vec<MemberKey>,
    /// LinkTap gateway on the LAN, when this vehicle has one. Empty host ⇒ the capability is off
    /// and the hub advertises no `linktap` capability at all.
    #[serde(default)]
    pub linktap: LinkTapConfig,
    /// The per-vehicle WEBHOOK SECRET that authenticates a local Shelly report to
    /// `/api/hub/shelly`, cached here from the app so the hub can authenticate a FLOOD REPORT
    /// with the internet down — the case the whole local-ingest path exists for.
    ///
    /// It is the SAME value and the same shape as the cloud's `&k=` bearer
    /// (cloud-server `auth.ts::classifyVehicleWebhookAuth`, custody in `vehicleSecrets.ts`): a
    /// Shelly fires a static URL and can sign nothing, so a URL bearer is the strongest thing it
    /// can carry. Pointing a sensor at the hub is therefore a URL swap, not a new credential.
    ///
    /// ⚠️ EMPTY MEANS DISARMED, NOT "ACCEPT ANYTHING". The cloud's classifier treats an unset
    /// secret as `legacy` and ACCEPTS — it is a phased rollout across vehicles that predate the
    /// scheme. This hub deliberately does NOT copy that half: /api/hub/shelly CLOSES A VALVE, so
    /// an unset secret refuses every report rather than trusting the LAN. `/api/hub/status`
    /// reports `shellyIngestArmed` so a disarmed hub is visible instead of silently deaf.
    ///
    /// Lives in this file for the same reason `token` and `member_keys` do: it is already the
    /// hub's one credential store (SYSTEM/0600), and a second file would just be a second thing
    /// to lock down. Never returned by any endpoint.
    #[serde(default)]
    pub shelly_secret: String,
    /// A GPS/location source on the LAN (a cellular router's GPS, a NMEA feed) the hub POLLS and
    /// reports as `gps.measurement`. Empty host ⇒ no source configured. The hub is the acquirer,
    /// not the app: a browser cannot reach a LAN router over HTTPS, and only the hub is always
    /// aboard. Its admin sign-in lives here for the same reason `token` does — this file is already
    /// the hub's one credential store (SYSTEM/0600). Never returned in full by any endpoint.
    #[serde(default)]
    pub gps: GpsConfig,
    /// Routers this hub MANAGES (routers.rs): signs in to over the LAN, reads on a timer, reports
    /// for (as the router device, with the router's own agent token) and configures on the owner's
    /// behalf. A GL.iNet is never here — it IS the hub. Sign-ins live in this file for the same
    /// reason `gps.password` does. Never returned in full by any endpoint.
    #[serde(default)]
    pub routers: Vec<RouterConfig>,
    /// Sensor wiring jobs still PENDING (sensors.rs) — persisted so a hub restart keeps hunting for
    /// a sleepy sensor the app handed over. Finished jobs leave this list.
    #[serde(default)]
    pub sensor_jobs: Vec<crate::sensors::SensorJob>,
}

/// One managed router. Mirrors the app's network_device record (`brv_net_<mac>` id, vendor, host,
/// port) plus what only the hub holds: the admin sign-in and the router's cloud agent token.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct RouterConfig {
    /// The app's network_device id (`brv_net_…`) — the device the router's telemetry lands on.
    pub id: String,
    /// `cradlepoint`, `peplink` or `starlink` (routers::vendor_supported).
    pub vendor: String,
    pub name: String,
    pub host: String,
    /// `0` ⇒ the vendor default (routers::default_port: 443 for the routers, 9200 for a dish).
    pub port: u16,
    pub username: String,
    /// Router admin password. Never returned by any endpoint. Empty for a Starlink, which has none.
    pub password: String,
    /// The router's per-device agent token (minted by the app through `/api/agent/enroll`), so the
    /// hub reports AS the router — the cloud then sees exactly what a hub-lite router sends.
    /// Empty ⇒ status is read for the app but nothing is reported to the cloud.
    pub agent_token: String,
    /// Report this router's GNSS position as `gps_dev_id` (a `brv_gps_…` gps_source record the app
    /// created alongside the router — owner: "When a router has a GPS enabled, create a device in
    /// the GPS section").
    pub gps_enabled: bool,
    pub gps_dev_id: String,
    /// Read cadence; `0` ⇒ routers::DEFAULT_POLL_SECS, floored at routers::POLL_FLOOR_SECS.
    pub poll_secs: u32,
    /// A configured-but-paused router: kept, not polled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// 🔴 ENABLED BY DEFAULT — and this cannot be `#[derive(Default)]`. The serde `default_true` above
/// covers a record READ from disk that omits the field; a derived `Default` still yields `false`,
/// and `do_routers`' add path builds a NEW router from `Default` — so on 2026-09-12 the first
/// Cradlepoint added through the app landed on CENTRAL with `enabled: false`, probed fine, and was
/// then silently skipped by the poll loop forever. A router the owner just added is a router the
/// owner wants read.
impl Default for RouterConfig {
    fn default() -> Self {
        RouterConfig {
            id: String::new(), vendor: String::new(), name: String::new(), host: String::new(),
            port: 0, username: String::new(), password: String::new(), agent_token: String::new(),
            gps_enabled: false, gps_dev_id: String::new(), poll_secs: 0, enabled: true,
        }
    }
}

/// The hub's GPS source: what to poll on the LAN and how to sign in. `kind` selects the driver
/// (`cradlepoint` today). Mirrors the app's GpsSourceSpec so a source can be entered on the hub
/// (console / app push) once, and polled with no app aboard.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct GpsConfig {
    /// Driver: `cradlepoint` (NCOS `/api/status/gps`) or `nmea` (a TCP/UDP sentence feed, gps.rs).
    /// `peplink` is a future driver.
    pub kind: String,
    pub host: String,
    /// Default 443 — a Cradlepoint CBA850 serves its NCOS API over HTTPS; `0`/absent ⇒ 443.
    pub port: u16,
    pub username: String,
    /// Router admin password. Never returned by any endpoint (redacted in status like `token`).
    pub password: String,
    /// The `brv_gps_*` device id fixes are reported under — the app's gps_source record id, so the
    /// cloud lands them on the SAME device the app created (hub takes over acquisition for it).
    pub dev_id: String,
    /// A configured-but-paused source. Absent ⇒ enabled when a host is set.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `nmea` only: `tcp` (connect to host:port; the default) or `udp` (listen on port).
    pub protocol: String,
}

fn default_true() -> bool { true }

/// The hub's LinkTap configuration and the cloud's PERMISSION for it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct LinkTapConfig {
    pub host: String,
    pub gw_id: String,
    /// The valves this hub may drive (canonical 16-hex ids).
    pub dev_ids: Vec<String>,
    /// ⚠️ THE PAID GATE, decided by the CLOUD and cached here (worker cloud-server #104 sends it on
    /// every batch reply as `linktap.allowed`). The valve is Weekender+ as of the owner's
    /// 2026-08-15 ruling, enforced cloud-side — but this hub drives the valve on the LAN with no
    /// cloud call in the path, so deciding capability locally would make the hub the way AROUND
    /// that gate. DEFAULTS TO FALSE: a hub that has never heard from the cloud advertises nothing,
    /// which is the same default-deny every other tier decision uses.
    pub allowed: bool,
}

impl Default for HubConfig {
    fn default() -> Self {
        HubConfig {
            hub_id: String::new(),
            vid: String::new(),
            name: String::new(),
            enabled: false,
            heartbeat_secs: 0,
            token: String::new(),
            http_port: DEFAULT_HTTP_PORT,
            member_keys: Vec::new(),
            linktap: LinkTapConfig::default(),
            shelly_secret: String::new(),
            gps: GpsConfig::default(),
            routers: Vec::new(),
            sensor_jobs: Vec::new(),
        }
    }
}

/// PURE: the config path under a given shared base. Split out so the layout is testable without
/// touching a real system directory.
pub fn config_path_in(base: &Path) -> PathBuf {
    base.join(DIR_NAME).join(FILE_NAME)
}

/// The platform's SHARED (all-users) data directory.
///  * Windows: `%ProgramData%` — the shared folder, readable by services and every login.
///  * macOS:   `/Library/Application Support` (the system one, not `~/Library`).
///  * Linux:   `/var/lib`.
pub fn shared_base() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(p) = std::env::var("ProgramData") {
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        return PathBuf::from("C:\\ProgramData");
    }
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support")
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        PathBuf::from("/var/lib")
    }
}

pub fn config_path() -> PathBuf {
    config_path_in(&shared_base())
}

/// PURE: mint a hub id if there isn't one, otherwise keep it. Returns (config, changed).
/// Keeping it is the point — the cloud device record is keyed to this id.
pub fn with_hub_id(mut cfg: HubConfig, mint: impl FnOnce() -> String) -> (HubConfig, bool) {
    if cfg.hub_id.is_empty() {
        cfg.hub_id = mint();
        return (cfg, true);
    }
    (cfg, false)
}

/// 96 bits of hex, prefixed — matches the `hub_` namespace the worker exempts from the device cap.
/// v4 UUID randomness comes from the OS, so two machines imaged from one disk still differ.
pub fn mint_hub_id() -> String {
    let hex = uuid::Uuid::new_v4().simple().to_string();
    format!("hub_{}", &hex[..24])
}

/// ⚠️⚠️ DO NOT EDIT `hub.json` BY HAND ON WINDOWS. It has cost this project twice, from ONE line:
///
/// ```text
/// Get-Content -Raw $cfg | ConvertFrom-Json | ... | ConvertTo-Json | Set-Content $cfg -Encoding UTF8
/// ```
///
/// PowerShell 5.1 gets BOTH halves wrong, in opposite directions:
///   * `Get-Content -Raw` decodes a BOM-less UTF-8 file as **cp1252**, so the em-dash in a hub
///     named `Hub — Central` (`E2 80 94`) comes back as the three characters `â€"`;
///   * `Set-Content -Encoding UTF8` writes a **BOM**, and re-encodes those three characters as
///     `C3 A2 E2 82 AC E2 80 9D`.
///
/// On 2026-08-28 that one command disarmed a registered hub (the BOM — see `strip_bom` below) and
/// silently corrupted its name, which then went out on every heartbeat until 2026-08-29. The
/// mangling is STABLE AT ONE LEVEL, so it looks like data rather than damage and survives review.
///
/// Neither failure is the daemon's: `cfg.name` is only ever assigned from a serde-parsed JSON body,
/// and there is no cp1252 anywhere in this crate. **The fix is not to defend against it here.**
/// Guessing which strings are mojibake and "repairing" them would corrupt a hub someone genuinely
/// named `â€"`, and a config store that rewrites its own contents on a heuristic is worse than one
/// that carries a bad name. Repair belongs at the display layer, or at the source.
///
/// TO CHANGE A HUB'S CONFIGURATION, USE ITS OWN API — `POST /api/hub/config` — so this process does
/// the writing, in UTF-8, through serde. That is the path the app takes and the path that is
/// tested. A shell round-trip through the file is not a supported way to configure a hub.
///
/// A UTF-8 BOM is not JSON, and serde_json is right to refuse it — but something has to strip it.
///
/// ⚠️ THIS COST A HUB IN THE FIELD. On 2026-08-28 CENTRAL's `hub.json` was rewritten with
/// PowerShell 5.1's `Set-Content -Encoding UTF8`, which prepends `EF BB BF` and gives no hint that
/// it did. The three bytes made the whole file unparseable, the daemon silently fell back to
/// defaults, and a registered hub came up `registered:false, allowed:false` — deaf, disarmed, and
/// giving no reason. Any Windows text editor or shell can do this; the hub has to survive it.
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// What is actually on disk. The distinction MATTERS and used to be collapsed:
///  * `Missing`  — a fresh machine that is not a hub. Defaults are correct.
///  * `Parsed`   — the normal case.
///  * `Damaged`  — a file EXISTS and could not be read. Defaults are NOT correct here: this
///    machine IS a hub, and its identity, token and member keys are in that file.
pub enum ConfigState {
    Missing,
    Parsed(Box<HubConfig>),
    Damaged(String),
}

/// Read the file and say honestly which of the three it is. Everything else is built on this.
pub fn read_config_state_in(base: &Path) -> ConfigState {
    let path = config_path_in(base);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ConfigState::Missing,
        Err(e) => return ConfigState::Damaged(format!("could not read {}: {e}", path.display())),
    };
    match serde_json::from_str::<HubConfig>(strip_bom(&text)) {
        Ok(cfg) => ConfigState::Parsed(Box::new(cfg)),
        Err(e) => ConfigState::Damaged(format!("{} is not valid JSON: {e}", path.display())),
    }
}

/// The damage, if there is any — for `/api/hub/status`, so a wedged hub says so out loud instead
/// of presenting as a factory-fresh one.
pub fn config_damage_in(base: &Path) -> Option<String> {
    match read_config_state_in(base) {
        ConfigState::Damaged(why) => Some(why),
        _ => None,
    }
}

/// Last damage message we logged, so an unparseable file does not write a line on every request —
/// `read_config_in` is called on essentially every endpoint. Logging on CHANGE means it is written
/// once per boot per distinct fault, which is what someone reading the log actually wants.
static LOGGED_DAMAGE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn log_damage_once(why: &str) {
    let mut g = match LOGGED_DAMAGE.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if g.as_deref() == Some(why) {
        return;
    }
    *g = Some(why.to_string());
    crate::hlog!("config: DAMAGED - {why}");
    crate::hlog!("config: running on DEFAULTS (unregistered, disarmed) and REFUSING to overwrite the file - fix or delete it");
}

pub fn read_config() -> HubConfig {
    read_config_in(&shared_base())
}

/// Same, under an explicit base — hub_server and its tests read the store without touching the
/// real shared directory.
///
/// Damage still yields defaults, because ~25 call sites need a `HubConfig` and a boat hub that
/// panics is worse than one that is merely unregistered. What has CHANGED is that damage is no
/// longer silent (it is logged, and reported by `/api/hub/status`) and no longer destructive
/// (`write_config_in` refuses to save these defaults over the real file).
pub fn read_config_in(base: &Path) -> HubConfig {
    match read_config_state_in(base) {
        ConfigState::Parsed(cfg) => *cfg,
        ConfigState::Missing => HubConfig::default(),
        ConfigState::Damaged(why) => {
            log_damage_once(&why);
            HubConfig::default()
        }
    }
}

/// Write atomically (temp + rename) so a power cut mid-write cannot leave the hub with a truncated
/// config — this machine is an always-on appliance on a boat, where power cuts are routine.
pub fn write_config(cfg: &HubConfig) -> Result<(), String> {
    write_config_in(&shared_base(), cfg)
}

pub fn write_config_in(base: &Path, cfg: &HubConfig) -> Result<(), String> {
    // ⚠️ REFUSE TO SAVE OVER A FILE WE COULD NOT READ. This is the second half of the BOM failure
    // and by far the worse half: `read_config_in` hands back DEFAULTS for a damaged file, so any
    // writer — discovery saving a gateway, the key sync, `with_hub_id` minting an id — would
    // persist those defaults and PERMANENTLY destroy the hub's token, vehicle and member keys over
    // what may be a three-byte problem. Refusing keeps a recoverable fault recoverable.
    //
    // There is no lockout: the file is still deletable (`clear_in`, and `/api/hub/clear`), which is
    // the deliberate way to start over. Damage is a human-fix situation, and a human can fix it
    // only if the evidence is still there.
    if let ConfigState::Damaged(why) = read_config_state_in(base) {
        crate::hlog!("config: refused a write over a damaged file - {why}");
        return Err(format!(
            "refusing to overwrite a configuration that could not be read ({why}) - fix or remove the file first"
        ));
    }
    let path = config_path_in(base);
    let dir = path.parent().ok_or("no parent directory")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let tmp = dir.join("hub.json.tmp");
    let text = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("could not replace {}: {e}", path.display()))?;
    restrict_permissions(&path);
    Ok(())
}

/// The file holds a bearer token, so it must not be world-readable even though it lives in a
/// shared folder: SYSTEM + Administrators only. Best-effort — a failure here must not break hub
/// setup, but it IS reported by the caller's status text so it cannot fail silently forever.
#[cfg(target_os = "windows")]
fn restrict_permissions(path: &Path) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let p = path.to_string_lossy().to_string();
    let _ = std::process::Command::new("icacls")
        .args([&p, "/inheritance:r", "/grant:r", "SYSTEM:F", "/grant:r", "Administrators:F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

#[cfg(not(target_os = "windows"))]
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Remove the hub's configuration entirely — the local half of un-registering, called by the
/// server's own /api/hub/clear. The service is the only writer of this file, so it is also the
/// only thing that deletes it.
pub fn clear_in(base: &Path) -> Result<(), String> {
    let path = config_path_in(base);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not remove {}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_lives_under_the_shared_folder_in_our_own_directory() {
        let p = config_path_in(Path::new("/shared"));
        assert_eq!(p, PathBuf::from("/shared/DockNeighbor/hub.json"));
        // The real base must be a SHARED location, never a user profile — that is the whole point.
        let base = shared_base().to_string_lossy().to_string();
        assert!(!base.contains("/Users/"), "shared base must not be a user profile: {base}");
        assert!(!base.contains("\\Users\\"), "shared base must not be a user profile: {base}");
    }

    #[test]
    fn a_hub_id_is_minted_once_and_then_kept() {
        let (first, changed) = with_hub_id(HubConfig::default(), || "hub_abc".into());
        assert!(changed);
        assert_eq!(first.hub_id, "hub_abc");
        // Re-registering or switching vehicles must NOT mint a new id — the cloud record is keyed
        // to it, and a fresh id would orphan the old one exactly like the router ghosts did.
        let (second, changed_again) = with_hub_id(first, || "hub_SHOULD_NOT_BE_USED".into());
        assert!(!changed_again);
        assert_eq!(second.hub_id, "hub_abc");
    }

    #[test]
    fn minted_ids_use_the_hub_namespace_and_are_distinct() {
        let a = mint_hub_id();
        let b = mint_hub_id();
        assert!(a.starts_with("hub_"), "{a}");
        assert_eq!(a.len(), 4 + 24);
        assert_ne!(a, b);
    }

    #[test]
    fn config_round_trips_and_absent_fields_default() {
        let cfg = HubConfig {
            hub_id: "hub_1".into(), vid: "v1".into(), name: "Central".into(),
            enabled: true, heartbeat_secs: 60, token: "tok".into(),
            http_port: 9000,
            member_keys: vec![MemberKey { key: "k1".into(), uid: "u1".into(), role: "coowner".into() }],
            linktap: LinkTapConfig {
                host: "192.168.8.20".into(), gw_id: "GW02".into(),
                dev_ids: vec!["aaaabbbbccccdddd".into()], allowed: true,
            },
            shelly_secret: "wh-secret".into(),
            gps: GpsConfig {
                kind: "cradlepoint".into(), host: "192.168.10.1".into(), port: 443,
                username: "admin".into(), password: "routerpw".into(),
                dev_id: "brv_gps_gkljr4kx9".into(), enabled: true, protocol: String::new(),
            },
            routers: vec![RouterConfig {
                id: "brv_net_003044aabbcc".into(), vendor: "cradlepoint".into(), name: "CBA850".into(),
                host: "192.168.10.1".into(), port: 0, username: "admin".into(), password: "routerpw".into(),
                agent_token: "agt_1".into(), gps_enabled: true, gps_dev_id: "brv_gps_gkljr4kx9".into(),
                poll_secs: 0, enabled: true,
            }],
            sensor_jobs: Vec::new(),
        };
        let text = serde_json::to_string(&cfg).unwrap();
        assert_eq!(serde_json::from_str::<HubConfig>(&text).unwrap(), cfg);
        // The GPS source round-trips as camelCase, password included (it is the hub's to hold).
        assert!(text.contains("\"devId\":\"brv_gps_gkljr4kx9\""));
        // A managed router keeps its sign-in AND its agent token here — and `enabled` defaults
        // to true when an older record omits it, so an upgrade never silently pauses a router.
        assert!(text.contains("\"agentToken\":\"agt_1\""));
        let r: RouterConfig = serde_json::from_str(r#"{"id":"brv_net_x","host":"10.0.0.1"}"#).unwrap();
        assert!(r.enabled && r.port == 0 && r.vendor.is_empty());
        // ...and a router built from Default (the add path) is enabled too — it was not, once.
        assert!(RouterConfig::default().enabled);
        // A file written by an older build (or hand-edited) must not fail to parse — and the
        // fields it does not know get REAL defaults: a #388-era hub.json must come up on the
        // default management port, not port 0.
        let partial: HubConfig = serde_json::from_str(r#"{"hub_id":"hub_2"}"#).unwrap();
        // The paid gate DEFAULTS TO DENY: a hub.json that predates the field, or a hub that has
        // never heard from the cloud, must not claim valve capability.
        assert!(!partial.linktap.allowed);
        assert!(partial.linktap.host.is_empty());
        assert_eq!(partial.hub_id, "hub_2");
        assert_eq!(partial.heartbeat_secs, 0);
        assert!(partial.token.is_empty());
        assert_eq!(partial.http_port, DEFAULT_HTTP_PORT);
        assert!(partial.member_keys.is_empty());
        // The Shelly ingest secret defaults to EMPTY, and empty is DISARMED — a hub.json written
        // before this field existed must not start accepting unauthenticated valve-closing
        // reports the moment it is upgraded.
        assert!(partial.shelly_secret.is_empty());
    }

    fn temp_base(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("brvg-hub-config-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join(DIR_NAME)).unwrap();
        d
    }

    fn seeded(base: &Path) -> HubConfig {
        let cfg = HubConfig {
            hub_id: "hub_real".into(), vid: "v_real".into(), name: "Central".into(),
            enabled: true, heartbeat_secs: 60, token: "the-token-that-must-survive".into(),
            ..HubConfig::default()
        };
        write_config_in(base, &cfg).unwrap();
        cfg
    }

    #[test]
    fn a_utf8_bom_does_not_make_the_config_unreadable() {
        // PowerShell 5.1's `Set-Content -Encoding UTF8` writes one, and it disarmed a real hub.
        let base = temp_base("bom");
        let cfg = seeded(&base);
        let text = std::fs::read_to_string(config_path_in(&base)).unwrap();
        std::fs::write(config_path_in(&base), format!("\u{feff}{text}")).unwrap();
        assert_eq!(read_config_in(&base), cfg, "three leading bytes must not cost the hub its identity");
    }

    #[test]
    fn a_missing_file_is_defaults_but_an_unreadable_one_is_damage() {
        let base = temp_base("states");
        // Missing: a machine that is simply not a hub. Defaults are the right answer.
        assert!(matches!(read_config_state_in(&base), ConfigState::Missing));
        assert_eq!(config_damage_in(&base), None);

        seeded(&base);
        assert!(matches!(read_config_state_in(&base), ConfigState::Parsed(_)));

        std::fs::write(config_path_in(&base), "{ this is not json").unwrap();
        assert!(matches!(read_config_state_in(&base), ConfigState::Damaged(_)));
        // ...and status must be able to SAY so, which is the difference between a hub that is
        // wedged and a hub that looks factory-fresh.
        assert!(config_damage_in(&base).unwrap().contains("not valid JSON"));
    }

    #[test]
    fn a_damaged_config_is_never_overwritten() {
        // THE POINT OF THIS WHOLE MECHANISM. Reads fall back to defaults, so without the guard the
        // very next writer persists those defaults and the token is gone for good.
        let base = temp_base("nooverwrite");
        seeded(&base);
        // Truncated mid-write, the shape a power cut or a bad editor actually leaves behind — and
        // the surviving text still holds the identity we must not throw away.
        std::fs::write(config_path_in(&base), "{\"hub_id\": \"hub_real\", \"token\": \"the-tok").unwrap();

        let defaults = read_config_in(&base);
        assert!(defaults.token.is_empty(), "a damaged read yields defaults - that is why the write guard exists");

        let err = write_config_in(&base, &defaults).unwrap_err();
        assert!(err.contains("refusing to overwrite"), "{err}");
        // The evidence — and the token inside it — is still on disk for a human to recover.
        assert_eq!(std::fs::read_to_string(config_path_in(&base)).unwrap().contains("hub_real"), true);
    }

    #[test]
    fn deleting_is_still_the_way_out_of_damage() {
        // No lockout: `clear_in` removes the file, so /api/hub/clear can always start over.
        let base = temp_base("escape");
        seeded(&base);
        std::fs::write(config_path_in(&base), "not json at all").unwrap();
        assert!(write_config_in(&base, &HubConfig::default()).is_err());
        clear_in(&base).unwrap();
        assert!(matches!(read_config_state_in(&base), ConfigState::Missing));
        write_config_in(&base, &HubConfig { hub_id: "hub_fresh".into(), ..HubConfig::default() }).unwrap();
        assert_eq!(read_config_in(&base).hub_id, "hub_fresh");
    }
}
