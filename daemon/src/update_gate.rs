//! WHEN a hub may update itself, and what to do about one that has just tried — the owner's rules of
//! 2026-09-25, as pure decisions.
//!
//! 🔴 THE OWNER ASKED FOR AUTOMATIC UPDATES WITH HUB-LITE'S SAFETY PROPERTIES. Until now the daemon
//! only made the gap VISIBLE: `update_check_loop` polled every 6 hours, set a flag, and installed
//! nothing. Installing was command-driven (the app's button, or a queued cloud command), so a released
//! daemon reached a vessel only when a person pressed something.
//!
//! His four rulings, and where each lives:
//!   1. an owner may turn it OFF, for daemon AND hub-lite together  -> `UpdatePolicy::enabled`
//!   2. only in a QUIET window                                     -> `quiet_window`, `in_quiet_window`
//!   3. never while an anchor watch or security zone is ARMED       -> `UpdateConditions::armed`
//!   4. the restart's offline/online pushes are SUPPRESSED          -> not here; the worker is told
//!
//! ⚠️ THIS MODULE IS PURE, like `cycle` and `close_watch`: no clock, no I/O, no files. The caller
//! supplies `now_ms` and what it can see. That is what makes the boundary conditions — 02:00 exactly,
//! a window crossing midnight, a deadline that has just passed — testable in microseconds instead of
//! at 2am on a boat.
//!
//! 🔴 AND IT IS WRITTEN SO THAT EVERY UNKNOWN BLOCKS THE UPDATE. A hub that cannot tell whether an
//! anchor watch is armed, or does not know its own local time, does not update. The cost of waiting is
//! six hours; the cost of a wrong guess is a vessel whose watchdog restarted itself mid-alarm.

/// Why an update was refused. Carried rather than discarded so the hub log says which rule held, and
/// the owner's "why has my hub not updated" has an answer that is not "it just does that".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Blocked {
    /// The owner turned automatic updates off (ruling 1). Covers hub-lite too.
    OwnerDisabled,
    /// Outside the quiet window (ruling 2).
    NotQuietYet,
    /// An anchor watch or security zone is armed (ruling 3).
    WatchArmed,
    /// A flood/leak alarm is latched. The hub is the thing acting on it; it does not restart now.
    AlarmActive,
    /// A close is in flight — the hub has told a valve to shut and has not seen it shut.
    ValveClosing,
    /// A watering cycle is running, so a restart could lose the cutoff that bounds it.
    CycleRunning,
    /// This exact version was rolled back on this machine before; installing it again would loop.
    VersionRolledBack,
    /// The hub cannot tell its own local time, so it cannot know whether the window is open.
    LocalTimeUnknown,
}

impl Blocked {
    pub fn as_str(self) -> &'static str {
        match self {
            Blocked::OwnerDisabled => "automatic updates are off for this vessel",
            Blocked::NotQuietYet => "outside the quiet window",
            Blocked::WatchArmed => "an anchor watch or security zone is armed",
            Blocked::AlarmActive => "an alarm is active",
            Blocked::ValveClosing => "a valve close is still unconfirmed",
            Blocked::CycleRunning => "a watering cycle is running",
            Blocked::VersionRolledBack => "this version was rolled back here before",
            Blocked::LocalTimeUnknown => "the hub does not know its local time",
        }
    }
}

/// The owner's policy, as the hub receives it. Defaults are what a vessel that has never been
/// configured gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdatePolicy {
    /// Ruling 1. False ⇒ neither the daemon nor hub-lite installs anything by itself.
    pub enabled: bool,
    /// Ruling 2, inclusive start hour in LOCAL time, 0..=23.
    pub quiet_from_hour: u8,
    /// Exclusive end hour, 0..=23. Equal to `quiet_from_hour` means a ZERO-length window, not
    /// "always" — see `in_quiet_window`, where that distinction is the difference between "never
    /// updates" and "updates at any hour", and the safe reading of an ambiguous config is the former.
    pub quiet_to_hour: u8,
}

impl Default for UpdatePolicy {
    /// ⚠️ ON by default, 02:00–05:00 local. The hours are an implementation choice, NOT an owner
    /// ruling: he said "quiet" and left the numbers to us. Three hours is wide enough that a hub
    /// checking every 6 hours will land inside it within a day, and 02:00 local is the least likely
    /// time for someone to be aboard using the water system.
    fn default() -> Self {
        UpdatePolicy { enabled: true, quiet_from_hour: 2, quiet_to_hour: 5 }
    }
}

/// Is `hour` (local, 0..=23) inside the window? Handles a window that crosses midnight.
///
/// ⚠️ A ZERO-LENGTH WINDOW IS CLOSED, NOT OPEN. `from == to` could mean "no window" or "all day", and
/// the two readings are 24 hours apart in permissiveness. It reads as closed, because a config that
/// accidentally collapses must not become "update whenever you like".
pub fn in_quiet_window(p: &UpdatePolicy, hour: u8) -> bool {
    if hour > 23 || p.quiet_from_hour > 23 || p.quiet_to_hour > 23 {
        return false;
    }
    if p.quiet_from_hour == p.quiet_to_hour {
        return false;
    }
    if p.quiet_from_hour < p.quiet_to_hour {
        hour >= p.quiet_from_hour && hour < p.quiet_to_hour
    } else {
        // Crosses midnight: 22..=23 or 0..<to.
        hour >= p.quiet_from_hour || hour < p.quiet_to_hour
    }
}

/// Everything the hub can see that bears on "is now a safe moment".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct UpdateConditions {
    /// An anchor watch or security zone is armed (ruling 3). The daemon reads this off the batch
    /// reply's `anchor` object, which it already receives.
    pub armed: bool,
    /// A flood/leak alarm is latched on any device.
    pub alarm_active: bool,
    /// A close is in flight (`rt.valve_closes` is non-empty).
    pub valve_closing: bool,
    /// A watering cycle is running.
    pub cycle_running: bool,
    /// Local hour, 0..=23, or None when the hub cannot resolve its timezone.
    pub local_hour: Option<u8>,
    /// True when the version on offer is one this machine already rolled back.
    pub offer_was_rolled_back: bool,
}

/// PURE: may this hub install an update right now?
///
/// ORDER IS DELIBERATE — the cheapest and most owner-visible reasons first, so the hub log says
/// "automatic updates are off" rather than "outside the quiet window" for a vessel that opted out.
/// After that, the physical-safety rules, which are the ones that must never be skipped.
pub fn may_update(p: &UpdatePolicy, c: &UpdateConditions) -> Result<(), Blocked> {
    if !p.enabled {
        return Err(Blocked::OwnerDisabled);
    }
    if c.offer_was_rolled_back {
        return Err(Blocked::VersionRolledBack);
    }
    // 🔴 THE THREE PHYSICAL RULES, CHECKED BEFORE THE CLOCK. A hub mid-alarm must not update even at
    // 03:00 — and putting them first means a future change to the window cannot reorder them away.
    if c.alarm_active {
        return Err(Blocked::AlarmActive);
    }
    if c.valve_closing {
        return Err(Blocked::ValveClosing);
    }
    if c.cycle_running {
        return Err(Blocked::CycleRunning);
    }
    if c.armed {
        return Err(Blocked::WatchArmed);
    }
    match c.local_hour {
        None => Err(Blocked::LocalTimeUnknown),
        Some(h) if in_quiet_window(p, h) => Ok(()),
        Some(_) => Err(Blocked::NotQuietYet),
    }
}

// ── probation: what to do about an update that has already been applied ──────────────────────────

/// How long a freshly installed daemon has to prove itself before it is rolled back.
///
/// ⚠️ IT MUST EXCEED A REAL START PLUS A REAL CLOUD ROUND TRIP ON A BAD LINK. A hub on a Starlink
/// afternoon or an LTE modem re-registering can take minutes to get its first report through, and a
/// deadline shorter than that would roll back a perfectly good version — the failure mode that makes
/// an auto-updater worse than no auto-updater, because it would do it on every release.
pub const PROBATION_MS: i64 = 10 * 60 * 1000;

/// A recorded probation: the version we came FROM, the version we are trying, and when patience ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probation {
    pub from: String,
    pub to: String,
    pub deadline_ms: i64,
}

/// What the caller should do about a recorded probation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbationStep {
    /// The new version is running and has reported to the cloud. Clear the marker; it worked.
    Confirm,
    /// Still inside the window and not yet confirmed. Leave it alone and look again.
    KeepWatching,
    /// The deadline passed without a confirmed report: put the previous binary back.
    RollBack,
    /// The running version is neither side of this probation — a stale marker from an unrelated
    /// install, or one already rolled back. Clear it rather than acting on it.
    Stale,
}

/// PURE: the decision for one look at one probation.
///
/// `reported_ok` is "this process has successfully reported to the cloud since it started" — the same
/// evidence hub-lite's guard uses. It is the only definition of healthy worth having: a daemon that
/// starts, binds its port and cannot reach the cloud is exactly the failure an owner would call broken,
/// and a liveness check that only asked "is the process up" would call it fine.
pub fn probation_step(p: &Probation, running_version: &str, reported_ok: bool, now_ms: i64) -> ProbationStep {
    if running_version == p.to {
        if reported_ok {
            return ProbationStep::Confirm;
        }
        return if now_ms >= p.deadline_ms { ProbationStep::RollBack } else { ProbationStep::KeepWatching };
    }
    // Running the version we came from: either the swap never took effect, or a rollback already
    // happened. Either way there is nothing left to watch and nothing to undo.
    ProbationStep::Stale
}

/// Serialize a probation marker. Deliberately the same shape hub-lite writes (`KEY=value` lines), so
/// one person reading either tier's file sees the same thing.
pub fn write_marker(p: &Probation) -> String {
    format!("FROM={}\nTO={}\nDEADLINE={}\n", p.from, p.to, p.deadline_ms)
}

/// Parse a probation marker. None when it is absent-shaped, truncated or not ours.
///
/// ⚠️ A HALF-WRITTEN MARKER READS AS NO MARKER, and that is the safe direction: the worst outcome of
/// ignoring one is that a bad version keeps running until someone notices, which is exactly today's
/// behaviour. The worst outcome of acting on a garbled one is rolling back a healthy hub to a version
/// the marker misnamed.
pub fn parse_marker(text: &str) -> Option<Probation> {
    let mut from = None;
    let mut to = None;
    let mut deadline = None;
    for line in text.lines() {
        let (k, v) = line.split_once('=')?;
        match k.trim() {
            "FROM" => from = Some(v.trim().to_string()),
            "TO" => to = Some(v.trim().to_string()),
            "DEADLINE" => deadline = v.trim().parse::<i64>().ok(),
            _ => {}
        }
    }
    let (from, to, deadline_ms) = (from?, to?, deadline?);
    if from.is_empty() || to.is_empty() || from == to {
        return None;
    }
    Some(Probation { from, to, deadline_ms })
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_790_000_000_000;

    fn ok_conditions() -> UpdateConditions {
        UpdateConditions { local_hour: Some(3), ..Default::default() }
    }

    #[test]
    fn the_defaults_are_on_and_quiet_overnight() {
        let p = UpdatePolicy::default();
        assert!(p.enabled);
        assert_eq!((p.quiet_from_hour, p.quiet_to_hour), (2, 5));
    }

    #[test]
    fn a_hub_with_nothing_happening_at_3am_updates() {
        assert_eq!(may_update(&UpdatePolicy::default(), &ok_conditions()), Ok(()));
    }

    #[test]
    fn each_of_the_owners_rules_blocks_on_its_own() {
        let p = UpdatePolicy::default();
        // Ruling 1 — and it outranks everything, so an opted-out vessel's log says so plainly.
        let off = UpdatePolicy { enabled: false, ..p };
        assert_eq!(may_update(&off, &ok_conditions()), Err(Blocked::OwnerDisabled));
        // Ruling 3.
        assert_eq!(
            may_update(&p, &UpdateConditions { armed: true, ..ok_conditions() }),
            Err(Blocked::WatchArmed)
        );
        // Ruling 2.
        assert_eq!(
            may_update(&p, &UpdateConditions { local_hour: Some(14), ..ok_conditions() }),
            Err(Blocked::NotQuietYet)
        );
    }

    #[test]
    fn the_physical_rules_hold_even_inside_the_quiet_window() {
        // 🔴 THE POINT OF ORDERING THEM FIRST. 03:00 is no excuse to restart mid-alarm.
        let p = UpdatePolicy::default();
        for (c, want) in [
            (UpdateConditions { alarm_active: true, ..ok_conditions() }, Blocked::AlarmActive),
            (UpdateConditions { valve_closing: true, ..ok_conditions() }, Blocked::ValveClosing),
            (UpdateConditions { cycle_running: true, ..ok_conditions() }, Blocked::CycleRunning),
        ] {
            assert_eq!(may_update(&p, &c), Err(want));
        }
    }

    #[test]
    fn a_physical_reason_is_reported_in_preference_to_the_clock() {
        // Both true at once: mid-alarm AND the middle of the afternoon. The hub log should say the
        // thing that matters ("an alarm is active"), not the thing that happens to be checked first in
        // some other ordering. This is what pins the order in `may_update` — without it, moving the
        // clock check to the top would keep every other test green while making every blocked update
        // report "outside the quiet window", and a real reason would be invisible in the field.
        let c = UpdateConditions { alarm_active: true, local_hour: Some(14), ..Default::default() };
        assert_eq!(may_update(&UpdatePolicy::default(), &c), Err(Blocked::AlarmActive));
        // And the owner's opt-out outranks even that, because "off" is not a fault to investigate.
        let off = UpdatePolicy { enabled: false, ..UpdatePolicy::default() };
        assert_eq!(may_update(&off, &c), Err(Blocked::OwnerDisabled));
    }

    #[test]
    fn an_unknown_local_time_blocks_rather_than_assuming() {
        // Waiting costs six hours. Guessing costs a restart at the wrong moment.
        let c = UpdateConditions { local_hour: None, ..Default::default() };
        assert_eq!(may_update(&UpdatePolicy::default(), &c), Err(Blocked::LocalTimeUnknown));
    }

    #[test]
    fn a_version_this_machine_rolled_back_is_never_offered_again() {
        let c = UpdateConditions { offer_was_rolled_back: true, ..ok_conditions() };
        assert_eq!(may_update(&UpdatePolicy::default(), &c), Err(Blocked::VersionRolledBack));
    }

    #[test]
    fn the_quiet_window_boundaries_are_inclusive_start_exclusive_end() {
        let p = UpdatePolicy::default(); // 02..05
        assert!(!in_quiet_window(&p, 1));
        assert!(in_quiet_window(&p, 2));
        assert!(in_quiet_window(&p, 4));
        assert!(!in_quiet_window(&p, 5));
    }

    #[test]
    fn a_window_that_crosses_midnight_works() {
        let p = UpdatePolicy { enabled: true, quiet_from_hour: 22, quiet_to_hour: 3 };
        for h in [22, 23, 0, 1, 2] {
            assert!(in_quiet_window(&p, h), "{h} should be inside 22..03");
        }
        for h in [3, 4, 12, 21] {
            assert!(!in_quiet_window(&p, h), "{h} should be outside 22..03");
        }
    }

    #[test]
    fn a_collapsed_window_is_closed_not_always_open() {
        // ⚠️ The two readings of from == to are 24 hours apart. A config that collapses must not
        // silently become "update at any hour".
        let p = UpdatePolicy { enabled: true, quiet_from_hour: 3, quiet_to_hour: 3 };
        for h in 0..24u8 {
            assert!(!in_quiet_window(&p, h), "{h} must not be inside a zero-length window");
        }
    }

    #[test]
    fn an_out_of_range_hour_is_never_inside_the_window() {
        assert!(!in_quiet_window(&UpdatePolicy::default(), 24));
        assert!(!in_quiet_window(&UpdatePolicy { enabled: true, quiet_from_hour: 99, quiet_to_hour: 5 }, 3));
    }

    fn pro() -> Probation {
        Probation { from: "0.3.56".into(), to: "0.3.57".into(), deadline_ms: T0 + PROBATION_MS }
    }

    #[test]
    fn a_new_version_that_reports_is_confirmed_immediately() {
        assert_eq!(probation_step(&pro(), "0.3.57", true, T0 + 1000), ProbationStep::Confirm);
    }

    #[test]
    fn a_new_version_that_has_not_reported_yet_is_given_its_window() {
        assert_eq!(probation_step(&pro(), "0.3.57", false, T0 + 1000), ProbationStep::KeepWatching);
        // One millisecond before the deadline it is still being given a chance.
        assert_eq!(probation_step(&pro(), "0.3.57", false, T0 + PROBATION_MS - 1), ProbationStep::KeepWatching);
    }

    #[test]
    fn a_new_version_that_never_reports_is_rolled_back_at_the_deadline() {
        assert_eq!(probation_step(&pro(), "0.3.57", false, T0 + PROBATION_MS), ProbationStep::RollBack);
        assert_eq!(probation_step(&pro(), "0.3.57", false, T0 + 86_400_000), ProbationStep::RollBack);
    }

    #[test]
    fn reporting_beats_the_deadline_however_late() {
        // A hub that finally gets a report through at minute 30 is healthy, not a rollback candidate.
        // Rolling back something that has demonstrably worked would be the worst of both behaviours.
        assert_eq!(probation_step(&pro(), "0.3.57", true, T0 + 86_400_000), ProbationStep::Confirm);
    }

    #[test]
    fn running_the_old_version_means_there_is_nothing_to_watch() {
        // Either the swap never took, or a rollback already happened.
        assert_eq!(probation_step(&pro(), "0.3.56", false, T0 + 86_400_000), ProbationStep::Stale);
        assert_eq!(probation_step(&pro(), "0.3.99", true, T0), ProbationStep::Stale);
    }

    #[test]
    fn a_marker_round_trips() {
        let p = pro();
        assert_eq!(parse_marker(&write_marker(&p)).as_ref(), Some(&p));
    }

    #[test]
    fn a_garbled_marker_reads_as_no_marker() {
        // The safe direction: ignoring one leaves today's behaviour; acting on one could roll back a
        // healthy hub to a version the marker misnamed.
        for bad in [
            "",
            "FROM=0.3.56\n",                                  // no TO, no DEADLINE
            "FROM=0.3.56\nTO=0.3.57\n",                       // truncated before the deadline
            "FROM=\nTO=0.3.57\nDEADLINE=1\n",                 // empty side
            "FROM=0.3.57\nTO=0.3.57\nDEADLINE=1\n",           // same both sides
            "FROM=0.3.56\nTO=0.3.57\nDEADLINE=soon\n",        // unparseable deadline
            "garbage",
        ] {
            assert!(parse_marker(bad).is_none(), "should not parse: {bad:?}");
        }
    }
}
