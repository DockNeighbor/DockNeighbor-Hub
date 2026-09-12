//! The LinkTap RUNTIME — where the protocol client meets the cycle machine and the hub's report
//! path. This is what makes a hub autonomous rather than a remote control: it polls the gateway,
//! enforces the software volume cut, classifies cycle ends, restarts on a timer expiry, and closes
//! every valve on a flood alarm — all with no internet in the path.
//!
//! PORTED from `hub/src/linktapRuntime.ts` (frozen on convergence) against its fixtures.
//!
//! Telemetry model, matching the vendor doc: the gateway's HTTP-push (full status on every change
//! plus a 2-minute heartbeat) is the PRIMARY when configured, and the cmd-3 poll is the FLOOR so a
//! gateway nobody configured for push still works and a missed push cannot strand stale state.
//! Both funnel into ONE `observe()` — the machine cannot tell them apart, which is the point.

use std::collections::HashMap;
use std::time::Duration;

use crate::cycle::{self, Action, EndReason, Ledger, Profile, State};
use crate::linktap::{self, Gateway, VolUnit};

/// One valve's live state on this hub.
pub struct Track {
    pub state: State,
    pub ledger: Option<Ledger>,
    /// Per-valve profile from the worker (config-as-state); missing pieces fall to the default.
    pub profile: Option<WireProfile>,
    /// Last `is_flm_plugin` seen — a non-metering valve is bounded by TIME only, said once.
    pub meters: Option<bool>,
    /// A reopen this valve is owed, decided in `observe` and PERFORMED by the caller — the same
    /// split `Action::Stop` already uses, so all gateway I/O stays in one place.
    pending_open: Option<PendingOpen>,
    /// The params of the last `linktap.measurement` this valve produced.
    ///
    /// 🔴 THE APP READS THIS, NOT THE GATEWAY. Owner ruling 2026-08-31: *"The app should not be
    /// talking to the linktap gateway at all, it should only be talking to a hub... except during
    /// provisioning."* Keeping the MEASUREMENT rather than a private shape means the app on the LAN
    /// and the app off it are looking at the same fields, mapped the same way — the LAN path stops
    /// being a second source of truth with its own bugs.
    last_measurement: Option<Vec<(String, String)>>,
}

/// A reopen the hub owes a valve: auto-restart of a Normal Run, or the Normal Run a washdown was
/// told to resume. Mode is always Normal — both paths return the valve to its profile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PendingOpen {
    pub duration_secs: u64,
    pub volume_cap_l: f64,
    /// Why we are reopening, for the log line. "auto-restart" or "washdown resume".
    pub why: &'static str,
}

/// A per-valve profile as the worker sends it. Every field OPTIONAL: the worker omits what the
/// vehicle never set (skip-don't-default), and the hub keeps its own default for those.
#[derive(Clone, Copy, Debug, Default)]
pub struct WireProfile {
    pub duration_secs: Option<u64>,
    pub volume_cap_l: Option<f64>,
    pub auto_restart: Option<bool>,
}

/// A telemetry line the runtime wants reported. The hub's existing batch path spools these.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub device: String,
    pub event: String,
    pub params: Vec<(String, String)>,
    /// The bearer to send this report with. `None` ⇒ the hub's own token, vouching for a device
    /// the cloud lets a hub speak for (`lt_*`, `brv_gps_*`). A managed router (routers.rs) reports
    /// with ITS OWN agent token instead, so the cloud sees the router itself reporting.
    pub token: Option<String>,
}

pub struct Runtime {
    pub gateway: Gateway,
    pub unit: VolUnit,
    pub default_profile: Profile,
    tracks: HashMap<String, Track>,
}

impl Runtime {
    pub fn new(gateway: Gateway, dev_ids: &[String], default_profile: Profile) -> Self {
        let mut tracks = HashMap::new();
        for id in dev_ids {
            let id = linktap::normalize_dev_id(id);
            if !id.is_empty() {
                tracks.insert(id, Track { state: State::Idle, ledger: None, profile: None, meters: None, pending_open: None, last_measurement: None });
            }
        }
        Runtime { gateway, unit: VolUnit::Gal, default_profile, tracks }
    }

    pub fn dev_ids(&self) -> Vec<String> {
        self.tracks.keys().cloned().collect()
    }

    /// The effective profile for one valve: the wire profile's fields over the default's, FIELD BY
    /// FIELD — the worker omits what nobody set, so a wrong guess upstream cannot re-cap a valve.
    pub fn profile_for(&self, dev_id: &str) -> Profile {
        let wire = self.tracks.get(dev_id).and_then(|t| t.profile);
        Profile {
            duration_secs: wire.and_then(|w| w.duration_secs).unwrap_or(self.default_profile.duration_secs),
            volume_cap_l: wire.and_then(|w| w.volume_cap_l).unwrap_or(self.default_profile.volume_cap_l),
            auto_restart: wire.and_then(|w| w.auto_restart).unwrap_or(self.default_profile.auto_restart),
        }
    }

    /// Record that THIS HUB just opened a valve.
    ///
    /// 🔴 WITHOUT THIS, THE HUB ADOPTS ITS OWN RUNS. `cycle::start_hub_cycle` existed from the
    /// start and was called by NOTHING outside its own unit tests: the only way a track ever left
    /// `Idle` was `observe` seeing `is_watering` and running `adopt_cycle`. So a run the hub had
    /// just programmed came back a poll later stamped `Provenance::Adopted`, and the app — reading
    /// `prov` — drew the owner's own 24h run as "Started Externally / Time Remaining Unknown".
    ///
    /// Observed on MVP 2026-08-31: `linktap: open 3CC1C335004B1200 ok` at 14:43:45, and the very
    /// next cloud doc carried `runProvenance: "adopted"`. No restart involved — the hub simply
    /// never told itself what it had done.
    ///
    /// Seeded at the moment of the successful open so the NEXT `observe` takes the `State::Running`
    /// branch and continues this cycle, rather than the `Idle` branch that adopts one.
    pub fn note_hub_open(
        &mut self,
        dev_id: &str,
        at: i64,
        mode: cycle::Mode,
        duration_secs: u64,
        volume_cap_l: f64,
        resume_normal: bool,
    ) -> Result<(), String> {
        let id = linktap::normalize_dev_id(dev_id);
        let state = cycle::start_hub_cycle(at, mode, duration_secs, volume_cap_l, resume_normal)?;
        match self.tracks.get_mut(&id) {
            Some(track) => {
                track.state = state;
                Ok(())
            }
            // Not a valve this hub watches. do_valve already refuses those, so this is a guard
            // against a future caller rather than a path we expect to take.
            None => Err(format!("valve {id} is not watched by this hub")),
        }
    }

    /// How long the poll loop may sleep before it MUST look again.
    ///
    /// A washdown that is about to hand over needs a poll inside its lead window, and the standing
    /// cadence (60s) is wider than the window — so a plain fixed interval could step straight over
    /// it and drop us back on the slow close-then-reopen path the handover exists to avoid.
    /// Returns `None` when nothing is time-critical, and the caller keeps its normal cadence.
    pub fn poll_hint(&self, now_ms: i64) -> Option<Duration> {
        let mut soonest: Option<u64> = None;
        for (id, track) in &self.tracks {
            let State::Running(run) = &track.state else { continue };
            if run.handover_issued || run.stop_issued.is_some() { continue; }
            if run.mode != cycle::Mode::Washdown || !run.resume_normal { continue; }
            if run.provenance != cycle::Provenance::Hub { continue; }
            let _ = id;
            let elapsed = ((now_ms - run.started_at) / 1000).max(0) as u64;
            let left = run.duration_secs.saturating_sub(elapsed);
            // Aim to be looking a little before the lead window opens, so the first poll inside it
            // is early rather than exactly on the boundary.
            let until = left.saturating_sub(cycle::HANDOVER_LEAD_SECS);
            soonest = Some(soonest.map_or(until, |s: u64| s.min(until)));
        }
        // Floor at 5s: a valve seconds from handover must not spin the gateway.
        soonest.map(|s| Duration::from_secs(s.max(5)))
    }

    /// The last measurement this valve produced, as (key, value) pairs — the SAME params the cloud
    /// receives. `None` before the first observation.
    pub fn last_measurement(&self, dev_id: &str) -> Option<&Vec<(String, String)>> {
        self.tracks.get(&linktap::normalize_dev_id(dev_id))?.last_measurement.as_ref()
    }

    /// Every valve this hub watches that has produced a measurement, for a whole-hub read.
    pub fn measurements(&self) -> Vec<(String, Vec<(String, String)>)> {
        let mut out: Vec<(String, Vec<(String, String)>)> = self
            .tracks
            .iter()
            .filter_map(|(id, t)| t.last_measurement.as_ref().map(|m| (id.clone(), m.clone())))
            .collect();
        // Stable order: a map iteration order that shuffles between reads would make the response
        // look changed when nothing did.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Take the reopen this valve is owed, if any. Taking CLEARS it, so a caller that fails to
    /// perform the open does not retry it on every poll for the rest of the day — a valve that
    /// silently reopens minutes later is worse than one that stayed shut and said so.
    pub fn take_pending_open(&mut self, dev_id: &str) -> Option<PendingOpen> {
        self.tracks.get_mut(&linktap::normalize_dev_id(dev_id))?.pending_open.take()
    }

    /// Read one track's RUNNING cycle for assertions: (provenance, duration_secs, cap_l). `None`
    /// when the valve is not watched or is idle, so `Some` already means running. Test-facing, but
    /// not `#[cfg(test)]` — the wiring test lives in hub_server, a different module in this crate.
    #[doc(hidden)]
    pub fn debug_track(&self, dev_id: &str) -> Option<(&'static str, u64, f64)> {
        let id = linktap::normalize_dev_id(dev_id);
        match &self.tracks.get(&id)?.state {
            cycle::State::Running(r) => Some((
                match r.provenance { cycle::Provenance::Hub => "hub", cycle::Provenance::Adopted => "adopted" },
                r.duration_secs,
                r.volume_cap_l,
            )),
            cycle::State::Idle => None,
        }
    }

    /// Apply per-valve profiles from a worker reply. Valves it does not name keep the default —
    /// what arrives IS the current truth for the valves it names.
    pub fn apply_profiles(&mut self, profiles: &HashMap<String, WireProfile>) {
        for (raw, prof) in profiles {
            let id = linktap::normalize_dev_id(raw);
            if let Some(t) = self.tracks.get_mut(&id) {
                t.profile = Some(*prof);
            }
        }
    }

    /// Feed one status payload for one valve — from a poll reply or a gateway push,
    /// indistinguishably. Returns what to DO and what to report; the caller performs the I/O, which
    /// is what keeps this testable without a gateway.
    pub fn observe(&mut self, dev_id: &str, data: &serde_json::Value, now_ms: i64) -> (Action, Vec<Report>) {
        let id = linktap::normalize_dev_id(dev_id);
        let profile = self.profile_for(&id);
        let unit = self.unit;
        let Some(track) = self.tracks.get_mut(&id) else {
            return (Action::None, Vec::new()); // not a valve this hub watches
        };

        let meters = linktap::reports_volume(data);
        let first_time_not_metering = track.meters.is_none() && !meters;
        track.meters = Some(meters);

        let watering = data.get("is_watering").map(linktap::coerce_watering).unwrap_or(false);
        let volume_l = linktap::cycle_volume_litres(data, unit);
        let remain = data.get("remain_duration").and_then(|v| v.as_f64()).filter(|r| *r > 0.0).map(|r| r as u64);

        let speed_lpm = linktap::flow_rate_litres_per_min(data, unit);
        let obs = cycle::Observation { at: now_ms, watering, volume_l, remain_secs: remain, speed_lpm };
        let r = cycle::step(&track.state, obs, &profile);
        track.state = r.state;

        // 🔴 THE SEAMLESS HANDOVER, decided while the valve is STILL OPEN. Owner, MVP 2026-08-31:
        // "it did resume, but it took forever... would be nice if it reprogrammed it right before
        // it was going to stop, so it never stops the water flow during the changing of the plans."
        //
        // The resume-after-close path below cannot be quick — we learn a cycle ended by polling,
        // and the poll is a minute apart. Measured: 30 seconds of dry pipe. So we do not wait for
        // the close; we reprogram the valve into its Normal Run just before its timer expires, and
        // it never shuts. `handover_issued` makes it exactly once.
        if let State::Running(run) = &mut track.state {
            if cycle::should_hand_over(run, remain, now_ms, cycle::HANDOVER_LEAD_SECS) {
                run.handover_issued = true;
                track.pending_open = Some(PendingOpen {
                    duration_secs: profile.duration_secs,
                    volume_cap_l: profile.volume_cap_l,
                    why: "washdown handover",
                });
            }
        }

        let mut reports = Vec::new();
        if let Some(ended) = &r.ended {
            let ledger = cycle::apply_to_ledger(track.ledger.as_ref(), ended);
            track.ledger = Some(ledger);
            // 🔴 THE REOPEN, DECIDED HERE AND PERFORMED BY THE CALLER. Both of these decisions
            // already existed as tested pure functions and NEITHER was ever called: `should_restart`
            // had no production caller at all, and the washdown resume lived only in the app, in an
            // unpersisted React ref that died with the page. `linktap_act`'s own doc comment claimed
            // it restarted on a timer expiry while its body only ever issued stops.
            //
            // Resume is checked FIRST: a washdown that was told to resume is answering an explicit
            // instruction attached to that run, where auto-restart is a standing profile switch.
            if cycle::should_resume_normal(ended) {
                track.pending_open = Some(PendingOpen {
                    duration_secs: profile.duration_secs,
                    volume_cap_l: profile.volume_cap_l,
                    why: "washdown resume",
                });
            } else if cycle::should_auto_restart(ended, profile.auto_restart) {
                track.pending_open = Some(PendingOpen {
                    duration_secs: profile.duration_secs,
                    volume_cap_l: profile.volume_cap_l,
                    why: "auto-restart",
                });
            }
            reports.push(Report { token: None,
                device: format!("lt_{id}"),
                // `.change` so it batches — a cycle end is history, not an alarm, and the worker
                // classifies it as telemetry by the same rule every other component uses.
                event: "linktap.cycle.change".into(),
                params: vec![
                    ("mode".into(), ended.mode.as_str().into()),
                    ("reason".into(), ended.reason.as_str().into()),
                    ("vol_l".into(), format!("{:.2}", ended.volume_l)),
                ],
            });
        }

        let mut params = vec![
            ("watering".into(), if watering { "1".to_string() } else { "0".to_string() }),
            ("vol_l".into(), format!("{volume_l:.2}")),
            ("meters".into(), if meters { "1".to_string() } else { "0".to_string() }),
        ];
        // THE VALVE'S OWN HEALTH, carried on the measurement so the app never needs the gateway.
        //
        // 🔴 Owner ruling 2026-08-31: "The app should not be talking to the linktap gateway at all,
        // it should only be talking to a hub." The app's valve view shows battery, RF signal and
        // link state, and it read all three straight off its own cmd-3 poll. Without them here the
        // app cannot stop polling — it would have to keep a second reader alive for three fields.
        // The hub already HAS them: they arrive in the same payload it is parsing.
        if let Some(b) = data.get("battery").and_then(|v| v.as_f64()) {
            params.push(("battery".into(), format!("{}", b.round() as i64)));
        }
        if let Some(sig) = data.get("signal").and_then(|v| v.as_f64()) {
            params.push(("signal".into(), format!("{}", sig.round() as i64)));
        }
        if let Some(rf) = data.get("is_rf_linked").map(linktap::coerce_watering) {
            params.push(("rf".into(), if rf { "1".into() } else { "0".to_string() }));
        }
        // ⚠️ THE VALVE'S FAULT FLAGS, and they are the reason this list is not "nice to have".
        // The app raises broken / leak / clog from these, and it read them off its own gateway poll.
        // Moving the app onto the hub without carrying them would have quietly deleted valve fault
        // detection while every screen still looked right — the worst shape a regression can take.
        for (src, out) in [("is_broken", "broken"), ("is_leak", "leak"), ("is_clog", "clog"), ("is_cutoff", "cutoff")] {
            if let Some(v) = data.get(src).map(linktap::coerce_watering) {
                params.push((out.into(), if v { "1".into() } else { "0".to_string() }));
            }
        }
        // THE LIVE FLOW RATE, in LITRES PER MINUTE. `speed_lpm` has always been computed here —
        // it drives the cutoff's stop-latency lead (cycle::cutoff_trigger_l) — but it was never
        // reported, so an app that is OFF the LAN (relay or cloud) had volume with no rate and
        // drew a flat 0 L/min through an entire watering cycle. CROSS-REPO CONTRACT: the worker
        // maps `flow_lpm` onto the app's `flow` field.
        //
        // WHILE WATERING ONLY, and reported even when it is 0.00: a closed valve has no rate to
        // report, but a valve that IS open and reading zero (a non-metering G1, or a metering
        // valve between pulses) genuinely is at zero, and omitting the param would leave the app
        // showing the last non-zero rate forever. Zero is a fact; silence is a stale number.
        if watering {
            params.push(("flow_lpm".into(), format!("{speed_lpm:.2}")));
        }
        if let Some(l) = &track.ledger {
            params.push(("day".into(), l.day.clone()));
            params.push(("day_vol_l".into(), format!("{:.2}", l.volume_l)));
        }

        // 🔴 THE RUN'S TARGETS, because the hub is the only thing that knows them and it was
        // telling nobody. Owner, 2026-08-31, looking at a run the hub had itself programmed:
        // *"why does it say this? it should know these from the hub..."* — the app showed
        // "Active Run Progress (Started Externally) / Time Remaining Unknown / Infinite".
        //
        // It said that because the app reads its targets from its OWN localStorage
        // (`lt_target_dur_<deviceId>`), written only when THAT app instance issued the open. So a
        // run started from anywhere else — the hub's API, an automation, a phone across the boat,
        // or the same app after its storage was cleared — renders as an unbounded mystery. Under
        // the hub-required architecture ("the hub is a SERVER, apps are clients") a client holding
        // the authoritative copy of a run's targets is exactly backwards.
        //
        // The hub has had all of it in `cycle::Running` the whole time. Reported here rather than
        // on a new endpoint so it reaches EVERY app the same way `flow_lpm` does: through the
        // cloud, off-LAN included, with no new door to open.
        //
        // WHILE RUNNING ONLY. An idle valve has no targets, and emitting stale ones would leave the
        // app drawing a finished run's numbers forever — the same trap the flow-rate comment below
        // describes, in the opposite direction.
        if let cycle::State::Running(run) = &track.state {
            params.push(("mode".into(), run.mode.as_str().into()));
            params.push(("dur_s".into(), run.duration_secs.to_string()));
            // 0 = uncapped, which is the truth for a washdown rather than a missing value.
            params.push(("cap_l".into(), format!("{:.2}", run.volume_cap_l)));
            if let Some(r) = remain {
                params.push(("remain_s".into(), r.to_string()));
            }
            // ⚠️ WHO STARTED IT. `Adopted` means the valve was already running when the hub first
            // saw it — a button press on the tap, or a schedule inside the gateway. `Hub` means we
            // programmed it and therefore vouch for the targets above. Without this the app cannot
            // tell "started externally, targets unknown" from "started by the hub, targets known",
            // and it currently guesses the first whenever its own localStorage is empty.
            params.push(("prov".into(), match run.provenance {
                cycle::Provenance::Hub => "hub".into(),
                cycle::Provenance::Adopted => "adopted".to_string(),
            }));
        }

        // Kept for the LAN read path as well as spooled to the cloud — one shape, one truth.
        if let Some(t) = self.tracks.get_mut(&id) {
            t.last_measurement = Some(params.clone());
        }
        reports.push(Report { token: None, device: format!("lt_{id}"), event: "linktap.measurement".into(), params });

        if first_time_not_metering {
            crate::hlog!("linktap: {id} does not meter flow - cycles are bounded by TIME only");
        }
        (r.action, reports)
    }

    /// Should this valve restart after the end this step produced? Kept separate from `observe` so
    /// the caller owns every gateway write.
    pub fn should_restart(&self, dev_id: &str, ended: &cycle::Ended) -> bool {
        cycle::should_auto_restart(ended, self.profile_for(&linktap::normalize_dev_id(dev_id)).auto_restart)
    }

    /// Mark a stop the hub is issuing for a reason the machine cannot infer (manual / flood), so
    /// the eventual close classifies correctly instead of reading as "unknown".
    pub fn note_stop(&mut self, dev_id: &str, reason: EndReason) {
        if let Some(t) = self.tracks.get_mut(&linktap::normalize_dev_id(dev_id)) {
            if let State::Running(r) = &mut t.state {
                r.stop_issued = Some(reason);
            }
        }
    }

    pub fn ledger(&self, dev_id: &str) -> Option<&Ledger> {
        self.tracks.get(&linktap::normalize_dev_id(dev_id)).and_then(|t| t.ledger.as_ref())
    }
}

/// Parse a gateway HTTP-push body (§4.1: the same JSON a cmd 3 reply carries).
pub fn parse_gateway_push(body: &str) -> Vec<(String, serde_json::Value)> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(linktap::extract_json(body)) else {
        return Vec::new();
    };
    let stats: Vec<serde_json::Value> = match parsed.get("dev_stat") {
        Some(serde_json::Value::Array(a)) => a.clone(),
        _ if parsed.get("dev_id").is_some() => vec![parsed.clone()],
        _ => Vec::new(),
    };
    stats
        .into_iter()
        .filter_map(|d| {
            let id = d.get("dev_id")?.as_str().map(linktap::normalize_dev_id)?;
            if id.is_empty() { None } else { Some((id, d)) }
        })
        .collect()
}

/// The worker's flood-shutoff classification, ported VERBATIM from cloud-server events.ts so the
/// hub closes the valve on exactly the events the cloud would have: flood/leak/alarm, minus
/// clears, minus telemetry.
/// Words that describe the SENSOR'S OWN CONDITION, not water.
///
/// 🔴 WHY THIS LIST EXISTS (owner, 2026-08-31): *"why can't the hub parse that its a cable unplugged
/// notice, vs flood?"* It could not, and the reason was embarrassing — the rule below was a
/// substring match, so `flood.cable_unplugged` counted as a flood purely because the string contains
/// "flood". A probe cable coming loose would have slammed the vehicle's water shut.
///
/// The workaround that had been applied was to point `flood.cable_unplugged` at the CLOUD ONLY, so
/// it could not reach the hub. The owner rejected that too, and was right: *"if its cloud only and
/// the firewall blocks access outside through the proxy, it would block that function"*. A fault
/// notice that only travels over the WAN is useless in exactly the situation this hub exists for —
/// it made the safety path robust and left the fault path depending on the thing that fails. The
/// answer is to CLASSIFY correctly and let every event reach the hub, not to withhold events from
/// it.
const SENSOR_FAULT_WORDS: &[&str] = &[
    "unplugged", "disconnected", "cable", "fault", "error",
    "low_battery", "battery_low", "mute", "unmute", "offline",
];

/// Is this event WATER, and therefore a reason to shut the valve?
///
/// Deliberately conservative in BOTH directions, because the two mistakes are not symmetric:
/// failing to close on a real flood is the product not working, and closing on a sensor fault is the
/// product turning off a boat's water for no reason. Neither is acceptable, so the classification is
/// explicit rather than a substring guess.
pub fn is_flood_shutoff(event: &str) -> bool {
    let e = event.to_ascii_lowercase();
    if e.ends_with(".measurement") || e.ends_with(".change") {
        return false;
    }
    if e.ends_with("_off") || e.ends_with(".off") {
        return false;
    }
    // A fault is a fault even when its name starts with "flood." — the component reporting it is the
    // flood sensor, which is why the prefix is there and why the substring match was fooled.
    if SENSOR_FAULT_WORDS.iter().any(|w| e.contains(w)) {
        return false;
    }
    e.contains("flood") || e.contains("leak") || e.contains("alarm")
}

// --- Gateway reachability -------------------------------------------------------------------------
//
// The hub polls the gateway every minute, so it is the only thing in the system that KNOWS when the
// gateway stops answering — and until now it told nobody. With the LinkTap cloud removed (owner
// ruling 2026-08-27, option (a): the cloud is gone and a hub is REQUIRED for valve control) there is
// no other source for this fact at all: nothing else on the boat or in the cloud can tell an
// unreachable gateway from a quiet one.

/// The grace window before an unreachable gateway is worth telling anyone about.
///
/// ⚠️ THE WINDOW LIVES HERE, ON THE HUB — not in the cloud. LinkTap gateways FLAP: a brief Wi-Fi
/// hiccup produces an offline and then an online seconds apart, and the first version of this (in
/// the cloud, off the LinkTap webhook) pushed on every single blip, so the owner got a stream of
/// "gateway disconnected" notices about a gateway that was fine. The owner set the window at
/// "30 min plus" — the same number the cloud's `linktapConnectivity.ts::GATEWAY_OFFLINE_GRACE_MS`
/// carries, deliberately, so the two debounces cannot disagree about what counts as an outage.
///
/// Putting it on the hub rather than the cloud is what makes it work at all now: the hub is the
/// only observer, and an outage report that has to survive a WAN round trip to be debounced is one
/// the cloud can only debounce for outages it was told about.
pub const GATEWAY_OFFLINE_GRACE_SECS: i64 = 30 * 60;

/// ⚠️ CROSS-REPO CONTRACT — THE WORKER IS BEING BUILT AGAINST THESE EXACT STRINGS. Do not rename.
///
/// ⚠️ AND NEVER PUT "flood", "leak" OR "alarm" IN THEM. The cloud classifies any event name
/// matching `FLOOD_EVENT_RE = /flood|leak|alarm/i` as a valve-closing flood (events.ts
/// `isFloodShutoff`, ported verbatim into `is_flood_shutoff` above). A gateway-offline notice
/// spelled "gateway.alarm" would therefore CLOSE THE VALVE — an event whose entire meaning is
/// "we cannot reach the gateway" would trigger the one action that needs the gateway. Both names
/// below are pinned against that classifier by test.
pub const GATEWAY_OFFLINE_EVENT: &str = "linktap.gateway.offline";
pub const GATEWAY_ONLINE_EVENT: &str = "linktap.gateway.online";

/// One gateway's reachability episode, as seen by the poll loop. PURE state — the loop owns the
/// clock and the I/O, exactly like `Runtime` owns no timers.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GatewayWatch {
    /// ms epoch of the last poll the gateway ANSWERED, or of the first poll it failed when it has
    /// never answered. `None` only before the very first observation.
    pub last_seen_ms: Option<i64>,
    /// Has `linktap.gateway.offline` already been emitted for THIS episode? Emitting once per
    /// episode is the difference between a notification and a minute-by-minute alarm clock.
    pub offline_reported: bool,
}

/// PURE: fold one poll's outcome into the watch and say what (if anything) to report.
///
/// `reached` is "the gateway answered our HTTP POST", NOT "the command succeeded" — a `ret: 5`
/// (end device not found: valve unpowered, out of RF range) is a gateway that is alive and talking,
/// and calling that an outage would alert on a flat valve battery.
///
/// The two rules that make this quiet enough to be useful:
///   * OFFLINE is emitted ONCE, and only after the gateway has been silent for the whole grace
///     window. A flap inside the window produces nothing at all.
///   * ONLINE is emitted ONLY if an offline was emitted for this episode. A recovery notice for an
///     outage nobody was ever told about is pure noise — it would announce the flaps the grace
///     window exists to swallow.
pub fn gateway_watch_step(
    watch: GatewayWatch,
    gw: &crate::linktap::Gateway,
    reached: bool,
    now_ms: i64,
) -> (GatewayWatch, Option<Report>) {
    let device = format!("lt_gw_{}", gw.gw_id);
    let mut next = watch;
    if reached {
        let was = next.offline_reported;
        let mins = outage_mins(watch.last_seen_ms, now_ms);
        next.last_seen_ms = Some(now_ms);
        next.offline_reported = false;
        if !was {
            return (next, None);
        }
        return (
            next,
            Some(Report { token: None,
                device,
                event: GATEWAY_ONLINE_EVENT.into(),
                params: vec![("host".into(), gw.host.clone()), ("mins".into(), mins.to_string())],
            }),
        );
    }
    // Unreachable. A hub that has NEVER seen this gateway answer starts its clock now rather than
    // claiming an outage of unknown length: the grace window then measures "silent since we began
    // watching", which is the only honest reading on a hub that just booted next to a dead gateway.
    let since = match next.last_seen_ms {
        Some(t) => t,
        None => {
            next.last_seen_ms = Some(now_ms);
            return (next, None);
        }
    };
    if next.offline_reported {
        return (next, None); // already said so for this episode
    }
    if now_ms - since < GATEWAY_OFFLINE_GRACE_SECS * 1000 {
        return (next, None); // inside the grace window — this is a flap until proven otherwise
    }
    next.offline_reported = true;
    (
        next,
        Some(Report { token: None,
            device,
            event: GATEWAY_OFFLINE_EVENT.into(),
            params: vec![
                ("host".into(), gw.host.clone()),
                ("mins".into(), outage_mins(Some(since), now_ms).to_string()),
            ],
        }),
    )
}

/// Whole minutes of outage, floored at zero. Reported as `mins` on both events so the notification
/// can say how long rather than only that something happened.
fn outage_mins(since_ms: Option<i64>, now_ms: i64) -> i64 {
    match since_ms {
        Some(t) => ((now_ms - t) / 60_000).max(0),
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T0: i64 = 1_787_140_800_000;
    const DEV: &str = "aaaabbbbccccdddd";

    fn rt(unit: VolUnit) -> Runtime {
        let mut r = Runtime::new(
            Gateway { host: "h".into(), gw_id: "GW02".into() },
            &[DEV.to_string()],
            Profile { duration_secs: 24 * 3600, volume_cap_l: 100.0, auto_restart: false },
        );
        r.unit = unit;
        r
    }

    #[test]
    fn a_gal_gateway_converts_before_the_cap_comparison() {
        // 27 gal = 102.2 L must cut a 100 L cap even though 27 < 100. The conversion IS the safety.
        let mut r = rt(VolUnit::Gal);
        let (a, _) = r.observe(DEV, &json!({"is_watering":1,"volume":26}), T0);
        assert_eq!(a, Action::None);
        let (a2, _) = r.observe(DEV, &json!({"is_watering":1,"volume":27}), T0 + 60_000);
        assert_eq!(a2, Action::Stop(EndReason::VolumeCap));
    }

    #[test]
    fn the_idle_garbage_latch_never_reaches_the_cap_comparison() {
        let mut r = rt(VolUnit::Gal);
        let (a, _) = r.observe(DEV, &json!({"is_watering":1,"volume":15_886_307.0}), T0);
        assert_eq!(a, Action::None, "a closed valve's latch must not read as water");
    }

    #[test]
    fn the_ledger_rides_the_measurement_params_after_a_cycle_ends() {
        let mut r = rt(VolUnit::Litre);
        r.observe(DEV, &json!({"is_watering":1,"volume":40}), T0);
        let (_, reports) = r.observe(DEV, &json!({"is_watering":0,"volume":42}), T0 + 60_000);
        let m = reports.iter().find(|x| x.event == "linktap.measurement").unwrap();
        let day_vol = m.params.iter().find(|(k, _)| k == "day_vol_l").unwrap();
        assert_eq!(day_vol.1, "42.00");
        assert_eq!(m.device, format!("lt_{DEV}"));
        let end = reports.iter().find(|x| x.event == "linktap.cycle.change").unwrap();
        assert!(end.params.iter().any(|(k, v)| k == "reason" && v == "unknown"));
    }

    #[test]
    fn wire_profiles_override_the_default_field_by_field() {
        let mut r = rt(VolUnit::Litre);
        let mut p = HashMap::new();
        p.insert(DEV.to_string(), WireProfile { volume_cap_l: Some(250.0), ..Default::default() });
        r.apply_profiles(&p);
        let eff = r.profile_for(DEV);
        assert_eq!(eff.volume_cap_l, 250.0);
        assert_eq!(eff.duration_secs, 24 * 3600, "unset fields keep the default");
    }

    #[test]
    fn the_wire_cap_drives_the_cutoff_the_moment_it_applies() {
        let mut r = rt(VolUnit::Litre);
        let mut p = HashMap::new();
        p.insert(DEV.to_string(), WireProfile { volume_cap_l: Some(30.0), ..Default::default() });
        r.apply_profiles(&p);
        r.observe(DEV, &json!({"is_watering":1,"volume":10}), T0);
        let (a, _) = r.observe(DEV, &json!({"is_watering":1,"volume":31}), T0 + 60_000);
        assert_eq!(a, Action::Stop(EndReason::VolumeCap), "under the default 100 but over the wire 30");
    }

    #[test]
    fn a_valve_this_hub_does_not_watch_is_ignored_entirely() {
        let mut r = rt(VolUnit::Litre);
        let (a, reports) = r.observe("ffffeeeeddddcccc", &json!({"is_watering":1,"volume":5}), T0);
        assert_eq!(a, Action::None);
        assert!(reports.is_empty());
    }

    #[test]
    fn a_flood_stop_classifies_as_flood_shutoff() {
        let mut r = rt(VolUnit::Litre);
        r.observe(DEV, &json!({"is_watering":1,"volume":3}), T0);
        r.note_stop(DEV, EndReason::FloodShutoff);
        let (_, reports) = r.observe(DEV, &json!({"is_watering":0,"volume":3}), T0 + 20_000);
        let end = reports.iter().find(|x| x.event == "linktap.cycle.change").unwrap();
        assert!(end.params.iter().any(|(k, v)| k == "reason" && v == "flood_shutoff"));
    }

    #[test]
    fn flood_classification_matches_the_workers_line_verbatim() {
        for e in ["flood.alarm", "leak.detected", "alarm", "Flood.Alarm"] {
            assert!(is_flood_shutoff(e), "{e} should close the valve");
        }
        for e in ["flood.alarm_off", "alarm.off", "flood.measurement", "flood.change", "voltmeter.measurement", "button.push"] {
            assert!(!is_flood_shutoff(e), "{e} must NOT close the valve");
        }
        // 🔴 SENSOR FAULTS ARE NOT FLOODS, however their component names them. `flood.cable_unplugged`
        // is the real Shelly Flood G4 event for a probe cable coming loose, and the old substring
        // rule closed the valve on it because the string contains "flood" — a loose cable would have
        // shut a vehicle's water off. Owner, 2026-08-31: "why can't the hub parse that its a cable
        // unplugged notice, vs flood?"
        for e in [
            "flood.cable_unplugged", "flood.cable_disconnected", "flood.fault", "flood.error",
            "flood.low_battery", "flood.mute", "flood.unmute", "leak.sensor_offline",
        ] {
            assert!(!is_flood_shutoff(e), "{e} is a sensor fault and must NOT close the valve");
        }
        // The real Shelly names that arrive on /api/hub/shelly and MUST close the valve. These are
        // the whole reason the hub has a local ingest at all — with the internet down this
        // classification is the only thing standing between a burst hose and a flooded bilge.
        for e in ["flood", "Flood Detected", "shelly.flood", "water_leak", "smoke.alarm"] {
            assert!(is_flood_shutoff(e), "{e} should close the valve");
        }
        // ⚠️ AND THE TWO GATEWAY-CONNECTIVITY NAMES, pinned here forever. If either ever contains
        // "flood"/"leak"/"alarm" it becomes a valve-closing flood in this classifier AND in the
        // cloud's — so "the gateway is unreachable" would fire the one action that needs the
        // gateway. This assertion is the guard on that rename.
        for e in [GATEWAY_OFFLINE_EVENT, GATEWAY_ONLINE_EVENT] {
            assert!(!is_flood_shutoff(e), "{e} must NOT close the valve");
        }
    }

    #[test]
    fn the_measurement_carries_the_live_flow_rate_while_watering() {
        // The gateway is on GALLONS, so 5.83 gal/min — the rate measured on MVP GW-02 during the
        // 2026-08-22 stop-latency run — must be reported as 22.07 L/min, not as 5.83.
        let mut r = rt(VolUnit::Gal);
        let (_, reports) = r.observe(DEV, &json!({"is_watering":1,"volume":2.0,"speed":5.83}), T0);
        let m = reports.iter().find(|x| x.event == "linktap.measurement").unwrap();
        let flow = m.params.iter().find(|(k, _)| k == "flow_lpm").expect("flow_lpm must be reported");
        assert_eq!(flow.1, "22.07");
    }

    #[test]
    fn a_watering_valve_with_no_flow_reading_reports_zero_rather_than_nothing() {
        // A non-metering G1 (or a metering valve between pulses) genuinely reads zero. Omitting
        // the param would leave an off-LAN app drawing the LAST non-zero rate for the rest of the
        // cycle; zero is a fact, silence is a stale number.
        let mut r = rt(VolUnit::Litre);
        let (_, reports) = r.observe(DEV, &json!({"is_watering":1,"volume":2.0}), T0);
        let m = reports.iter().find(|x| x.event == "linktap.measurement").unwrap();
        assert_eq!(m.params.iter().find(|(k, _)| k == "flow_lpm").unwrap().1, "0.00");
        // ...and a CLOSED valve reports no rate at all — there is nothing flowing to describe.
        let (_, closed) = r.observe(DEV, &json!({"is_watering":0,"volume":2.0}), T0 + 60_000);
        let m2 = closed.iter().find(|x| x.event == "linktap.measurement").unwrap();
        assert!(m2.params.iter().all(|(k, _)| k != "flow_lpm"));
    }

    // --- Gateway reachability -------------------------------------------------------------------

    fn gw() -> crate::linktap::Gateway {
        crate::linktap::Gateway { host: "192.168.8.20".into(), gw_id: "GW02".into() }
    }
    const MIN: i64 = 60_000;

    #[test]
    fn a_flap_inside_the_grace_window_says_absolutely_nothing() {
        // THE BUG THIS EXISTS TO PREVENT: the cloud's first version alerted on every blip, so a
        // 20-second Wi-Fi hiccup pushed "gateway disconnected" about a gateway that was fine.
        let (w, r) = gateway_watch_step(GatewayWatch::default(), &gw(), true, 0);
        assert!(r.is_none(), "the first successful poll is not news");
        let (w, r) = gateway_watch_step(w, &gw(), false, 20_000);
        assert!(r.is_none());
        let (w, r) = gateway_watch_step(w, &gw(), true, 40_000);
        assert!(r.is_none(), "a recovery nobody was told to worry about must stay silent");
        assert!(!w.offline_reported);
    }

    #[test]
    fn a_sustained_outage_reports_once_and_then_recovers_once() {
        let (mut w, _) = gateway_watch_step(GatewayWatch::default(), &gw(), true, 0);
        // Still inside 30 minutes: nothing.
        for t in [5 * MIN, 15 * MIN, 29 * MIN] {
            let (nw, r) = gateway_watch_step(w, &gw(), false, t);
            w = nw;
            assert!(r.is_none(), "reported at {} minutes, inside the grace window", t / MIN);
        }
        // Past it: exactly one offline, carrying the host and the duration.
        let (w2, r) = gateway_watch_step(w, &gw(), false, 31 * MIN);
        let rep = r.expect("a 31-minute outage must be reported");
        assert_eq!(rep.event, "linktap.gateway.offline");
        assert_eq!(rep.device, "lt_gw_GW02");
        assert!(rep.params.contains(&("host".to_string(), "192.168.8.20".to_string())));
        assert!(rep.params.contains(&("mins".to_string(), "31".to_string())));
        // And NEVER again for the same episode — a minute-by-minute repeat is an alarm clock.
        let (w3, again) = gateway_watch_step(w2, &gw(), false, 45 * MIN);
        assert!(again.is_none());
        let (w4, back) = gateway_watch_step(w3, &gw(), true, 60 * MIN);
        let rep = back.expect("an outage that WAS reported must report its recovery");
        assert_eq!(rep.event, "linktap.gateway.online");
        assert_eq!(rep.device, "lt_gw_GW02");
        assert!(rep.params.contains(&("mins".to_string(), "60".to_string())));
        assert!(!w4.offline_reported, "the episode is closed");
        // A second good poll after the recovery is not news either.
        assert!(gateway_watch_step(w4, &gw(), true, 61 * MIN).1.is_none());
    }

    #[test]
    fn a_hub_that_boots_next_to_a_dead_gateway_starts_its_clock_at_the_first_poll() {
        // No prior sighting: claiming an outage of unknown length would be a guess, so the window
        // measures "silent since we began watching" instead. It still fires — 30 minutes later.
        let (w, r) = gateway_watch_step(GatewayWatch::default(), &gw(), false, 1_000);
        assert!(r.is_none());
        assert_eq!(w.last_seen_ms, Some(1_000));
        let (w, r) = gateway_watch_step(w, &gw(), false, 1_000 + 29 * MIN);
        assert!(r.is_none());
        let (_, r) = gateway_watch_step(w, &gw(), false, 1_000 + 31 * MIN);
        assert_eq!(r.expect("must eventually report").event, "linktap.gateway.offline");
    }

    #[test]
    fn the_grace_window_is_the_owners_thirty_minutes() {
        // Provenance, pinned: owner "30 min plus", the same number the cloud's
        // linktapConnectivity.ts carries. Changing it here silently diverges the two debounces.
        assert_eq!(GATEWAY_OFFLINE_GRACE_SECS, 1_800);
    }

    #[test]
    fn parses_both_push_shapes_and_survives_junk() {
        let a = parse_gateway_push(&json!({"gw_id":"G","dev_stat":[{"dev_id":DEV,"is_watering":1}]}).to_string());
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].0, DEV);
        let b = parse_gateway_push(&json!({"dev_id": format!("{DEV}0042"), "is_watering":0}).to_string());
        assert_eq!(b[0].0, DEV, "long ids normalise to the canonical 16");
        assert!(parse_gateway_push("not json").is_empty());
        assert!(parse_gateway_push("{}").is_empty());
    }

    #[test]
    fn the_measurement_carries_everything_the_app_reads_off_the_gateway() {
        // 🔴 THE CONTRACT THAT LETS THE APP STOP POLLING. Owner ruling 2026-08-31: "The app should
        // not be talking to the linktap gateway at all." The app's valve view shows battery, RF
        // signal and link state, and read all three off its OWN cmd-3 poll. If the hub does not
        // carry them, the app cannot stop — it would keep a second reader alive for three fields.
        let mut r = rt(VolUnit::Litre);
        let (_, reports) = r.observe(
            DEV,
            &json!({"is_watering":1,"vol":3.2,"speed":5.5,"battery":93,"signal":69,"is_rf_linked":true}),
            T0,
        );
        let m = reports.iter().find(|x| x.event == "linktap.measurement").expect("a measurement");
        let p = |k: &str| m.params.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        assert_eq!(p("battery"), Some("93".into()));
        assert_eq!(p("signal"), Some("69".into()));
        assert_eq!(p("rf"), Some("1".into()));

        // The fault flags the app raises alarms from.
        let (_, faults) = r.observe(
            DEV,
            &json!({"is_watering":0,"is_broken":true,"is_leak":false,"is_clog":true,"is_cutoff":false}),
            T0 + 1000,
        );
        let mf = faults.iter().find(|x| x.event == "linktap.measurement").unwrap();
        let pf = |k: &str| mf.params.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        assert_eq!(pf("broken"), Some("1".into()));
        assert_eq!(pf("leak"), Some("0".into()));
        assert_eq!(pf("clog"), Some("1".into()));
        assert_eq!(pf("cutoff"), Some("0".into()));

        // A gateway that omits them must not invent zeros — a missing reading is not a flat battery.
        let mut r2 = rt(VolUnit::Litre);
        let (_, reports2) = r2.observe(DEV, &json!({"is_watering":0}), T0);
        let m2 = reports2.iter().find(|x| x.event == "linktap.measurement").unwrap();
        for k in ["battery", "signal", "rf"] {
            assert!(m2.params.iter().all(|(a, _)| a != k), "{k} must be absent, not zero");
        }
    }

    #[test]
    fn a_washdown_hands_over_before_it_expires_so_the_water_never_stops() {
        // 🔴 OWNER, MVP 2026-08-31: "it did resume, but it took forever... would be nice if it
        // reprogrammed it right before it was going to stop, so it never stops the water flow
        // during the changing of the plans."
        //
        // Measured on the boat: closed 14:57:04, watering again 14:57:34 — 30s of dry pipe, because
        // the hub only learns a cycle ended by polling and the poll is 60s apart. The handover
        // reprograms the OPEN valve instead, so it never shuts.
        let mut r = rt(VolUnit::Litre);
        r.note_hub_open(DEV, T0, cycle::Mode::Washdown, 300, 0.0, true).expect("our valve");

        r.observe(DEV, &json!({"is_watering":1,"vol":1.0,"remain_duration":200}), T0 + 100_000);
        assert!(r.take_pending_open(DEV).is_none(), "200s left is not the time to hand over");

        // Inside the lead window, valve STILL OPEN.
        r.observe(DEV, &json!({"is_watering":1,"vol":8.0,"remain_duration":15}), T0 + 285_000);
        let open = r.take_pending_open(DEV).expect("a handover");
        assert_eq!(open.why, "washdown handover");
        assert_eq!(open.duration_secs, 24 * 3600, "the valve's Normal Run profile");
        assert!((open.volume_cap_l - 100.0).abs() < 0.01);

        // Exactly once — a second poll inside the window must not re-issue.
        r.observe(DEV, &json!({"is_watering":1,"vol":8.5,"remain_duration":8}), T0 + 292_000);
        assert!(r.take_pending_open(DEV).is_none(), "handover is issued once");
    }

    #[test]
    fn a_washdown_being_stopped_never_hands_over() {
        // The dangerous case: a flood closes the valve inside the lead window. Handing over there
        // would reopen the water on a vessel that is flooding.
        let mut r = rt(VolUnit::Litre);
        r.note_hub_open(DEV, T0, cycle::Mode::Washdown, 300, 0.0, true).expect("our valve");
        r.observe(DEV, &json!({"is_watering":1,"vol":1.0,"remain_duration":200}), T0 + 100_000);
        r.note_stop(DEV, EndReason::FloodShutoff);
        r.observe(DEV, &json!({"is_watering":1,"vol":8.0,"remain_duration":15}), T0 + 285_000);
        assert!(r.take_pending_open(DEV).is_none(), "a valve on its way shut must stay shut");
    }

    #[test]
    fn the_poll_loop_is_told_to_look_again_before_a_handover_window() {
        let mut r = rt(VolUnit::Litre);
        assert!(r.poll_hint(T0).is_none(), "an idle valve asks for nothing");

        r.note_hub_open(DEV, T0, cycle::Mode::Washdown, 300, 0.0, true).expect("our valve");
        // 300s run, 100s elapsed, 200s left, 20s lead -> look again in ~180s (the caller still caps
        // this at its own 60s cadence).
        assert_eq!(r.poll_hint(T0 + 100_000).expect("a washdown that will hand over").as_secs(), 180);

        // Never busier than every 5s, however close the handover is.
        assert_eq!(r.poll_hint(T0 + 299_000).expect("seconds from handover").as_secs(), 5);

        // A washdown with no resume asked for is not time-critical.
        let mut plain = rt(VolUnit::Litre);
        plain.note_hub_open(DEV, T0, cycle::Mode::Washdown, 300, 0.0, false).unwrap();
        assert!(plain.poll_hint(T0 + 100_000).is_none());
    }

    #[test]
    fn a_finished_washdown_queues_the_normal_run_it_was_told_to_resume() {
        // End to end through the runtime: the reopen must be QUEUED for the caller, because all
        // gateway I/O lives in linktap_act. Before this, `should_restart` had no production caller
        // at all and the washdown resume lived only in the app's memory.
        let mut r = rt(VolUnit::Litre);
        r.note_hub_open(DEV, T0, cycle::Mode::Washdown, 300, 0.0, true).expect("our valve");
        // Watering, then the timer expires and the gateway reports it closed.
        r.observe(DEV, &json!({"is_watering":1,"vol":2.0}), T0 + 1000);
        r.observe(DEV, &json!({"is_watering":0,"vol":9.0}), T0 + 300_000);

        let open = r.take_pending_open(DEV).expect("a washdown that was told to resume");
        assert_eq!(open.why, "washdown resume");
        // The valve's PROFILE decides the run, not the washdown's own numbers.
        assert_eq!(open.duration_secs, 24 * 3600);
        assert!((open.volume_cap_l - 100.0).abs() < 0.01);
        // Taking clears it: a caller that fails must not silently reopen the valve on a later poll.
        assert!(r.take_pending_open(DEV).is_none(), "taking clears the pending open");
    }

    #[test]
    fn a_washdown_stopped_by_a_flood_never_reopens_the_valve() {
        // The one that would matter most. A flood closes the valve mid-washdown; the resume
        // instruction attached to that run must die with it.
        let mut r = rt(VolUnit::Litre);
        r.note_hub_open(DEV, T0, cycle::Mode::Washdown, 300, 0.0, true).expect("our valve");
        r.observe(DEV, &json!({"is_watering":1,"vol":2.0}), T0 + 1000);
        r.note_stop(DEV, EndReason::FloodShutoff);
        r.observe(DEV, &json!({"is_watering":0,"vol":3.0}), T0 + 60_000);
        assert!(r.take_pending_open(DEV).is_none(), "a flood-stopped washdown must stay shut");
    }

    #[test]
    fn a_run_the_hub_opened_is_its_own_not_an_adopted_one() {
        // 🔴 THE BUG THIS PINS. `cycle::start_hub_cycle` was dead code — nothing outside its own
        // unit tests called it — so the ONLY way a track left Idle was `adopt_cycle`. The hub
        // therefore adopted RUNS IT HAD JUST ISSUED, and the app drew the owner's own 24h run as
        // "Started Externally / Time Remaining Unknown / Infinite".
        //
        // Observed on MVP 2026-08-31: `linktap: open 3CC1C335004B1200 ok` at 14:43:45, next cloud
        // doc `runProvenance: "adopted"`. No restart, no external press — the hub just never told
        // itself what it had done.
        let mut r = rt(VolUnit::Litre);
        r.note_hub_open(DEV, T0, cycle::Mode::Normal, 86_400, 1135.6, false).expect("our own valve");

        // The gateway now reports it watering, exactly as it would for an external open. The
        // difference must come from what we recorded, not from what the hardware says.
        let (_, reports) = r.observe(DEV, &json!({"is_watering":1,"vol":3.2,"remain_duration":212,"speed":5.5}), T0 + 1000);
        let m = reports.iter().find(|x| x.event == "linktap.measurement").expect("a measurement");
        let p = |k: &str| m.params.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());

        assert_eq!(p("prov"), Some("hub".into()), "we opened it — this is not an adopted run");
        // AND the targets are the ones WE issued, not the hardware's remaining time. `remain_duration`
        // above says 212s; adopting would have taken it and reported a 212-second run.
        assert_eq!(p("dur_s"), Some("86400".into()), "our duration, not the gateway's remainder");
        assert_eq!(p("cap_l"), Some("1135.60".into()), "our cap, not the profile default");
        assert_eq!(p("mode"), Some("normal".into()));
    }

    #[test]
    fn a_washdown_the_hub_opened_is_time_only() {
        // The outlawed shape guarded in start_hub_cycle, reached through the new door: a washdown
        // carries NO volume cap, and seeding one must not be able to smuggle one in.
        let mut r = rt(VolUnit::Litre);
        r.note_hub_open(DEV, T0, cycle::Mode::Washdown, 7200, 0.0, false).expect("a washdown");
        let (_, reports) = r.observe(DEV, &json!({"is_watering":1,"vol":1.0,"speed":5.5}), T0 + 1000);
        let m = reports.iter().find(|x| x.event == "linktap.measurement").expect("a measurement");
        let p = |k: &str| m.params.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        assert_eq!(p("prov"), Some("hub".into()));
        assert_eq!(p("mode"), Some("washdown".into()));
        assert_eq!(p("cap_l"), Some("0.00".into()), "a washdown is bounded by TIME only");

        // And the outlawed combination is still refused at the door.
        assert!(r.note_hub_open(DEV, T0, cycle::Mode::Washdown, 7200, 50.0, false).is_err(),
            "a washdown with a volume cap is the shape the owner outlawed");
    }

    #[test]
    fn seeding_a_valve_this_hub_does_not_watch_is_refused() {
        let mut r = rt(VolUnit::Litre);
        assert!(r.note_hub_open("0000000000000000", T0, cycle::Mode::Normal, 60, 10.0, false).is_err());
    }

    #[test]
    fn a_running_cycle_reports_its_targets_and_who_started_it() {
        // 🔴 OWNER, 2026-08-31, on a run the hub had itself programmed: "why does it say this? it
        // should know these from the hub..." — the app showed "(Started Externally) / Unknown /
        // Infinite". It did, because the hub had every one of these in cycle::Running and published
        // none of them; the app was left reading its own localStorage.
        let mut r = rt(VolUnit::Litre);
        let (_, reports) = r.observe(DEV, &json!({"is_watering":1,"vol":3.2,"remain_duration":212,"speed":5.5}), T0);
        let m = reports.iter().find(|x| x.event == "linktap.measurement").expect("a measurement");
        let p = |k: &str| m.params.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        // This scenario is a valve ALREADY WATERING the first time the hub looks — a press on the
        // tap, or a schedule inside the gateway — so the cycle is ADOPTED and its duration comes
        // from the hardware rather than from our profile. Asserting the profile's 24h here was my
        // first guess and it was wrong; the code was right.
        assert_eq!(p("prov"), Some("adopted".into()), "already-running means adopted");
        assert_eq!(p("remain_s"), Some("212".into()));
        assert_eq!(p("dur_s"), Some("212".into()), "an adopted run is bounded by what the gateway says");
        assert_eq!(p("cap_l"), Some("100.00".into()), "the cap is ours, from the profile");
        assert!(p("mode").is_some(), "the app needs to tell a washdown from a normal run");
        // ⚠️ AND THIS IS THE DISTINCTION THE OWNER ACTUALLY ASKED FOR. `adopted` is the ONLY case
        // that deserves the app's "Started Externally" label. Today the app shows it whenever its
        // own localStorage is empty, which is why a run the hub itself programmed was described as
        // external.
    }

    #[test]
    fn an_idle_valve_reports_no_targets_at_all() {
        // Emitting a finished run's numbers would leave the app drawing them forever — the same
        // trap the flow rate avoids by being reported only while watering.
        let mut r = rt(VolUnit::Litre);
        let (_, reports) = r.observe(DEV, &json!({"is_watering":0,"vol":0.0}), T0);
        let m = reports.iter().find(|x| x.event == "linktap.measurement").expect("a measurement");
        for k in ["dur_s", "cap_l", "remain_s", "prov", "mode"] {
            assert!(m.params.iter().all(|(a, _)| a != k), "{k} must not be reported for an idle valve");
        }
    }
}
