// The hub, as a library — the binary in main.rs is one caller; the desktop app is (transitionally)
// the other, for its `--hub` compatibility path. See Cargo.toml for why this is its own crate.

pub mod hub_log;
pub mod adopt;
pub mod gateway_http;
pub mod hub_config;
pub mod cycle;
pub mod linktap_runtime;
pub mod linktap;
pub mod gps;
pub mod routers;
pub mod peplink;
pub mod sensors;
pub mod hub_relay;
pub mod hub_server;
pub mod update_check;
pub mod web_bundle;
pub mod self_update;
pub mod linktap_discover;
pub mod tray_state;

// The Windows service host. Windows-only: it links against the SCM (advapi32 via the
// windows-service crate), which does not exist on macOS or the Linux CI leg. `main.rs` chooses
// between this and a plain foreground run at startup.
#[cfg(windows)]
pub mod win_service;
