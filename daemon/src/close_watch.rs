//! CONFIRM-THEN-RETRY for a valve CLOSE — the pure half.
//!
//! 🔴 THE BUG THIS EXISTS FOR: A CLOSE WAS ONE PACKET AND A HOPE. Every `cmd 7` site in this hub
//! judged success from the COMMAND'S REPLY — `reply.ok`, i.e. the gateway accepted the request —
//! and then stopped. The valve was never asked whether it had actually shut:
//!
//!   * the volume cutoff (`cycle::step`) fires `Action::Stop` only while `stop_issued.is_none()`
//!     and sets `stop_issued` in the same step, so the machine CANNOT re-issue it. The comment in
//!     `hub_server::linktap_act` claiming "the machine keeps stop_issued set, so the next
//!     observation retries without a re-issue storm" was exactly backwards: with the mark set the
//!     cutoff branch never fires again, so nothing retried at all.
//!   * the flood shutoff sent one `cmd 7` per valve and spooled `linktap.stop_failed` if the reply
//!     was not ok. One attempt. No confirmation. No alert when the reply WAS ok and the valve
//!     stayed open.
//!
//! And the reply is known to lie: `cycle::STOP_LATENCY_SECS` was measured on MVP's GW-02 precisely
//! because "a single `cmd 7` frequently returns `ret:0` while the valve keeps running". The latency
//! comment already said the real close "costs a confirm-and-retry loop" — this is that loop.
//!
//! WHAT COUNTS AS SUCCESS, AND IT IS THE WHOLE POINT: the VALVE'S OWN REPORTED STATE — `is_watering`
//! going false in the gateway's `cmd 3` status, the same field and the same parse
//! (`linktap::coerce_watering`) the poll loop already feeds to the cycle machine. Never the
//! command's `ret`. A gateway that accepts a command it does not deliver over RF is the failure
//! mode; asking the accepting party whether it succeeded cannot see it.
//!
//! PURE by construction, like `cycle`: no clock, no I/O, no timers. The caller supplies `now_ms`
//! and performs the reads and the writes, which is what makes the give-up boundary testable without
//! waiting five minutes or owning a gateway.

/// Why the hub is closing this valve. Decides nothing about the RETRY schedule — every cause gets
/// the same effort — and everything about what the eventual failure is called: the `cause` param on
/// the alert is how the owner (and the app) tell "the flood shutoff could not close it" from "you
/// pressed Close and it did not close".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseCause {
    /// A flood/leak alarm. The safety path.
    Flood,
    /// The software volume cutoff — the only volume enforcement that exists (`cycle::step`).
    VolumeCap,
    /// A person pressed Close in the app, or through `/api/hub/linktap/valve`.
    Manual,
}

impl CloseCause {
    pub fn as_str(self) -> &'static str {
        match self {
            CloseCause::Flood => "flood",
            CloseCause::VolumeCap => "volume_cap",
            CloseCause::Manual => "manual",
        }
    }

    /// The end reason a close of this cause marks on the run, so the eventual close classifies as
    /// what the hub DID rather than as `unknown` (`cycle::step`'s classification order).
    pub fn end_reason(self) -> crate::cycle::EndReason {
        match self {
            CloseCause::Flood => crate::cycle::EndReason::FloodShutoff,
            CloseCause::VolumeCap => crate::cycle::EndReason::VolumeCap,
            CloseCause::Manual => crate::cycle::EndReason::Manual,
        }
    }

    /// Does a NEW close for `self` override one already in flight for `other`?
    ///
    /// One in-flight close per valve is the rule that keeps two callers from both driving `cmd 7`
    /// at one valve. A flood is the one exception: an alarm arriving while a manual or volume close
    /// is being retried restarts the sequence AS A FLOOD, because the cause is what the owner is
    /// told about and a flood must never be reported as a failed manual press.
    pub fn overrides(self, other: CloseCause) -> bool {
        self == CloseCause::Flood && other != CloseCause::Flood
    }
}

/// The retry schedule, as a value so the give-up boundary can be tested in milliseconds instead of
/// in five real minutes. `PRODUCTION` is the owner's numbers and is pinned by test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloseSchedule {
    /// A confirmation read must land within this long of EVERY issue — it bounds how long the hub
    /// can be holding an unanswered question about a valve it has told to shut.
    pub confirm_within_ms: i64,
    /// Elapsed milliseconds, FROM THE FIRST ATTEMPT, at which re-issues 2..n fall due.
    pub retry_at_ms: &'static [i64],
    /// After `retry_at_ms` is exhausted, one re-issue this often.
    pub then_every_ms: i64,
    /// Milliseconds from the first attempt at which the hub stops trying and tells the owner.
    pub give_up_ms: i64,
}

/// Milliseconds are the unit here ONLY so the five-minute give-up boundary can be exercised by a
/// test in a hundred milliseconds instead of in five real minutes — `PRODUCTION` below is written in
/// seconds × 1000 and pinned to the owner's numbers by `the_production_schedule_is_the_owners_numbers`.
const SEC: i64 = 1000;

impl CloseSchedule {
    /// 🔴 THE OWNER'S NUMBERS (Jonathan, 2026-09-24): "confirm within 10 s of issuing; if
    /// unconfirmed, re-issue at 5 s, 10 s, 20 s, 40 s, then every 60 s; give up at 5 minutes from
    /// the first attempt."
    ///
    /// ⚠️ THE 5 s FIRST RETRY FIRES DURING A HEALTHY CLOSE, AND THAT IS ACCEPTED DELIBERATELY.
    /// `cycle::STOP_LATENCY_SECS` is 8.0 — measured on hardware as the time between issuing a stop
    /// and the valve actually shutting — so a valve that IS closing normally is still reporting
    /// `is_watering: true` at 5 s and takes a second `cmd 7`. That is one extra idempotent command
    /// per close (a `cmd 7` to a shut valve is a no-op), not a storm, and in the case this module
    /// exists for it recovers a lost RF delivery three seconds sooner. Moving the first retry to
    /// 10 s would spend that recovery time to save a command; the owner owns that trade, so his
    /// number is kept and the interaction is written down here rather than quietly adjusted.
    pub const PRODUCTION: CloseSchedule = CloseSchedule {
        confirm_within_ms: 10 * SEC,
        retry_at_ms: &[5 * SEC, 10 * SEC, 20 * SEC, 40 * SEC],
        then_every_ms: 60 * SEC,
        give_up_ms: 300 * SEC,
    };
}

/// One close the hub has issued and not yet seen confirmed. Lives per valve; there is at most one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloseWatch {
    pub cause: CloseCause,
    /// ms epoch of the FIRST `cmd 7` of this sequence — the give-up window starts here.
    pub first_ms: i64,
    /// ms epoch of the most recent `cmd 7` — the confirm deadline runs from here.
    pub last_ms: i64,
    /// How many `cmd 7`s this sequence has sent, including the first. Never zero.
    pub attempts: u32,
}

impl CloseWatch {
    /// Opened at the moment the FIRST `cmd 7` is issued — before the reply is read, deliberately:
    /// the reply is not evidence, and a hub that dies between the send and the record must not
    /// forget it asked.
    pub fn opened(cause: CloseCause, at_ms: i64) -> CloseWatch {
        CloseWatch { cause, first_ms: at_ms, last_ms: at_ms, attempts: 1 }
    }
}

/// What to do about an unconfirmed close, given the valve's own reported state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseStep {
    /// The valve reports itself shut. Done — drop the watch and stop everything.
    Confirmed,
    /// Still open, and the next re-issue is due now.
    Reissue,
    /// Still open, nothing due yet.
    Wait,
    /// Still open and the give-up window has passed: stop trying and tell the owner.
    GaveUp,
}

/// Elapsed milliseconds, from the first attempt, at which attempt number `n` falls due. `n` is 1-based,
/// so the first RE-issue is n = 2. Attempt 1 is due immediately (it is what opened the watch).
pub fn due_at_ms(s: &CloseSchedule, n: u32) -> i64 {
    if n <= 1 {
        return 0;
    }
    let i = (n - 2) as usize;
    match s.retry_at_ms.get(i) {
        Some(ms) => *ms,
        None => {
            // Past the named offsets: one every `then_every_ms`, counted from the last named one.
            let last = s.retry_at_ms.last().copied().unwrap_or(0);
            let extra = (i - s.retry_at_ms.len() + 1) as i64;
            last + extra * s.then_every_ms
        }
    }
}

/// PURE: the decision for one observation of one closing valve.
///
/// ORDER IS LOAD-BEARING, and it is the same doctrine as `cycle::step`'s classification order —
/// what we can SEE outranks what we planned:
///   1. CONFIRMED first. A valve that reports itself shut ends the sequence whatever the clock says,
///      which is the rule "a confirmed-closed valve stops everything".
///   2. GIVE UP second, so a retry that falls due exactly at the boundary does not send one more
///      `cmd 7` after the hub has decided to stop trying.
///   3. Only then a re-issue.
pub fn close_step(s: &CloseSchedule, w: &CloseWatch, watering: bool, now_ms: i64) -> CloseStep {
    if !watering {
        return CloseStep::Confirmed;
    }
    let elapsed = (now_ms - w.first_ms).max(0);
    if elapsed >= s.give_up_ms {
        return CloseStep::GaveUp;
    }
    if elapsed >= due_at_ms(s, w.attempts + 1) {
        CloseStep::Reissue
    } else {
        CloseStep::Wait
    }
}

/// PURE: milliseconds the caller may sleep before it MUST look at this valve again — the smaller of "the
/// next re-issue falls due" and "the confirm deadline of the last issue expires", so every issue is
/// answered inside `confirm_within_ms` even out where the re-issues are a minute apart. Floored at
/// 1 ms so it is never zero; the caller applies its own floor before sleeping.
pub fn next_look_ms(s: &CloseSchedule, w: &CloseWatch, now_ms: i64) -> i64 {
    let due = w.first_ms + due_at_ms(s, w.attempts + 1);
    let confirm_by = w.last_ms + s.confirm_within_ms;
    let give_up_at = w.first_ms + s.give_up_ms;
    // ⚠️ A DEADLINE ALREADY PAST IS NOT A DEADLINE. The confirm window binds only while it is still
    // ahead of us: once this issue HAS been answered (the read that just happened), keeping it in the
    // minimum pins the target at `now` and the caller spins on a 1 ms nap until the next re-issue
    // falls due — a poll storm produced by the very thing meant to bound one. Found by test, not by
    // reading: `every_issue_is_answered_inside_the_confirm_window` asked for 50 s and got 0.
    let target = if confirm_by > now_ms { due.min(confirm_by) } else { due };
    target.min(give_up_at).saturating_sub(now_ms).max(1)
}

/// ⚠️ CROSS-REPO CONTRACT — three implementations answer to this ONE string: this daemon, hub-lite
/// (`LT_CLOSE_UNCONFIRMED_EVENT` in brvg-hub-lite.sh) and the worker
/// (`HUB_VALVE_CLOSE_UNCONFIRMED_EVENT` in DockNeighbor-Cloud `src/hubValveState.ts`, which
/// classifies it and picks its wording). Renaming it on one side silently deletes the alert.
///
/// 🔴 WHY THIS EXACT NAME IS SAFE, and the naming rules it is threading between (all three live in
/// the worker's `events.ts` and are ported into `linktap_runtime::is_flood_shutoff` here):
///   * `FLOOD_EVENT_RE = /flood|leak|alarm/i` is a SUBSTRING match that CLOSES EVERY VALVE and
///     latches `alarmActive`. A name carrying "flood", "leak" or "alarm" would make the hub's
///     "I could not close the valve" report arrive as a fresh flood alarm — and the cloud would
///     answer it by asking the same hub to close the same valve. This name carries none of them.
///   * `ALARM_CLEARED_RE = /(?:_off|\.off)$/i` reads a trailing `_off`/`.off` as an ALL-CLEAR: it
///     resolves the alarm episode and pushes a calm "cleared" note. The single worst outcome
///     available here would be a failed flood close being delivered as good news. This name ends in
///     `unconfirmed` and contains no "off" at all.
///   * `TELEMETRY_EVENT_RE = /[._](measurement|change)$/i` makes anything ending `.change` or
///     `.measurement` cached-and-never-pushed. `linktap.flood_close.change` (hub-lite) is exactly
///     that, on purpose — visibility, not an alert. This name must NOT end that way, and does not.
/// It also avoids `closed`, which `notifyCategories`' security rule claims
/// (`/motion|vibration|smoke|opened|closed|btn|button/`) — `close_unconfirmed` does not contain it,
/// so the name falls through to the water rule (`/water|valve|flow|volume|irrigat|tap/`) like every
/// other LinkTap event. Followed from `linktap.valve.fault` (Cloud #490), which threaded the same
/// needle for the same reasons.
pub const CLOSE_UNCONFIRMED_EVENT: &str = "linktap.valve.close_unconfirmed";

/// PURE: the params of the alert a given-up close raises. `cause` is what distinguishes a failed
/// flood shutoff from a failed manual press; `attempts` and `secs` are how hard the hub tried, which
/// is the first thing anyone debugging a bench failure will ask.
pub fn unconfirmed_params(w: &CloseWatch, now_ms: i64) -> Vec<(String, String)> {
    let secs = (now_ms - w.first_ms).max(0) / SEC;
    vec![
        ("cause".into(), w.cause.as_str().into()),
        ("attempts".into(), w.attempts.to_string()),
        ("secs".into(), secs.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: CloseSchedule = CloseSchedule::PRODUCTION;
    const T0: i64 = 1_787_140_800_000;

    fn at(secs: i64) -> i64 {
        T0 + secs * 1000
    }

    #[test]
    fn the_production_schedule_is_the_owners_numbers() {
        // 🔴 PINNED AGAINST THE OWNER'S WORDS (2026-09-24): "confirm within 10 s of issuing; if
        // unconfirmed, re-issue at 5 s, 10 s, 20 s, 40 s, then every 60 s; give up at 5 minutes".
        // A drift here is a safety change made by accident, so it is asserted as literals.
        assert_eq!(S.confirm_within_ms, 10_000);
        assert_eq!(S.retry_at_ms, &[5_000, 10_000, 20_000, 40_000]);
        assert_eq!(S.then_every_ms, 60_000);
        assert_eq!(S.give_up_ms, 300_000);
        // …and stated once more in the units the owner used, so a unit slip cannot read as green.
        assert_eq!(S.give_up_ms / SEC, 5 * 60);
    }

    #[test]
    fn the_retry_offsets_run_5_10_20_40_then_every_60_and_stop_inside_the_window() {
        // Attempt 1 is the close itself; 2..5 are the named offsets; 6 onward are a minute apart,
        // counted from the last named one (40 + 60 = 100, not 40 + 60·n from zero).
        assert_eq!(due_at_ms(&S, 1) / SEC, 0);
        assert_eq!(due_at_ms(&S, 2) / SEC, 5);
        assert_eq!(due_at_ms(&S, 3) / SEC, 10);
        assert_eq!(due_at_ms(&S, 4) / SEC, 20);
        assert_eq!(due_at_ms(&S, 5) / SEC, 40);
        assert_eq!(due_at_ms(&S, 6) / SEC, 100);
        assert_eq!(due_at_ms(&S, 7) / SEC, 160);
        assert_eq!(due_at_ms(&S, 8) / SEC, 220);
        assert_eq!(due_at_ms(&S, 9) / SEC, 280);
        // The tenth would fall at 340 s, past the 300 s give-up — so nine attempts is the ceiling,
        // which is what bounds the traffic this whole mechanism can produce for one valve.
        assert_eq!(due_at_ms(&S, 10) / SEC, 340);
        assert!(due_at_ms(&S, 10) > S.give_up_ms);
    }

    #[test]
    fn a_close_confirmed_on_the_first_read_issues_no_retry() {
        let w = CloseWatch::opened(CloseCause::VolumeCap, T0);
        // The valve reports shut: nothing else matters, at any point in the window.
        for t in [0, 4, 5, 41, 299, 301, 100_000] {
            assert_eq!(
                close_step(&S, &w, false, at(t)),
                CloseStep::Confirmed,
                "a closed valve at t={t}s must confirm, never retry or give up"
            );
        }
    }

    #[test]
    fn an_unconfirmed_close_retries_on_schedule_and_gives_up_at_five_minutes() {
        // The whole failure this module exists for: the command succeeded, the valve never shut.
        // Walk the real clock and count what the hub would send.
        let mut w = CloseWatch::opened(CloseCause::Flood, T0);
        let mut issued_at: Vec<i64> = vec![0];
        let mut t = 0;
        while t <= 400 {
            match close_step(&S, &w, true, at(t)) {
                CloseStep::Reissue => {
                    w.attempts += 1;
                    w.last_ms = at(t);
                    issued_at.push(t);
                }
                CloseStep::GaveUp => break,
                CloseStep::Wait => {}
                CloseStep::Confirmed => panic!("a watering valve must not read as confirmed"),
            }
            t += 1;
        }
        assert_eq!(issued_at, vec![0, 5, 10, 20, 40, 100, 160, 220, 280]);
        assert_eq!(w.attempts, 9);
        // Gave up at 300 s exactly — not at 299, and not by running out of offsets.
        assert_eq!(close_step(&S, &w, true, at(299)), CloseStep::Wait);
        assert_eq!(close_step(&S, &w, true, at(300)), CloseStep::GaveUp);
        assert_eq!(close_step(&S, &w, true, at(3600)), CloseStep::GaveUp);
    }

    #[test]
    fn the_give_up_boundary_beats_a_retry_that_falls_due_at_the_same_instant() {
        // Order matters: a watch whose next retry is due at 300 s must NOT send a tenth command on
        // its way out. Constructed so due_at_ms(attempts+1) == give_up_ms exactly.
        const TIGHT: CloseSchedule = CloseSchedule { confirm_within_ms: 10 * SEC, retry_at_ms: &[300 * SEC], then_every_ms: 60 * SEC, give_up_ms: 300 * SEC };
        let s = TIGHT;
        let w = CloseWatch::opened(CloseCause::Flood, T0);
        assert_eq!(due_at_ms(&s, 2), s.give_up_ms);
        assert_eq!(close_step(&s, &w, true, at(300)), CloseStep::GaveUp);
    }

    #[test]
    fn a_valve_that_closes_mid_retry_stops_the_sequence() {
        // Three attempts in, then the valve reports shut. The sequence must end there — the next
        // look cannot produce another command, whatever the clock has reached.
        let mut w = CloseWatch::opened(CloseCause::Manual, T0);
        w.attempts = 3;
        w.last_ms = at(10);
        assert_eq!(close_step(&S, &w, true, at(20)), CloseStep::Reissue);
        assert_eq!(close_step(&S, &w, false, at(20)), CloseStep::Confirmed);
        assert_eq!(close_step(&S, &w, false, at(280)), CloseStep::Confirmed);
    }

    #[test]
    fn every_issue_is_answered_inside_the_confirm_window() {
        // Out past the named offsets the re-issues are a minute apart, and a minute is far too long
        // to be holding an unanswered question about a valve told to shut. The look interval is the
        // smaller of "next due" and "this issue's confirm deadline".
        let mut w = CloseWatch::opened(CloseCause::Flood, T0);
        w.attempts = 6; // the 100 s attempt; the next falls at 160 s
        w.last_ms = at(100);
        assert_eq!(next_look_ms(&S, &w, at(100)) / SEC, 10, "10 s after the issue, not 60");
        // And once that read has happened the deadline is SPENT: the next look is the re-issue
        // itself, 50 s away. Keeping a passed deadline in the minimum pins the target at `now` and
        // the caller spins — the bug this line caught.
        assert_eq!(next_look_ms(&S, &w, at(110)) / SEC, 50);
        assert_eq!(next_look_ms(&S, &w, at(120)) / SEC, 40);
        // Early in the sequence the due time is the binding constraint, not the confirm window.
        let w1 = CloseWatch::opened(CloseCause::Flood, T0);
        assert_eq!(next_look_ms(&S, &w1, T0) / SEC, 5);
        // Never zero or negative, however late the caller is.
        let mut late = CloseWatch::opened(CloseCause::Flood, T0);
        late.attempts = 9;
        late.last_ms = at(280);
        assert!(next_look_ms(&S, &late, at(1000)) >= 1);
        // The look never reaches past the give-up: the decision at 300 s must be taken at 300 s.
        let mut w9 = CloseWatch::opened(CloseCause::Flood, T0);
        w9.attempts = 9;
        w9.last_ms = at(280);
        assert_eq!(next_look_ms(&S, &w9, at(290)) / SEC, 10);
    }

    #[test]
    fn a_flood_overrides_a_close_already_in_flight_and_nothing_else_does() {
        // One in-flight close per valve, with one exception: an alarm arriving mid-retry restarts
        // the sequence as a FLOOD, because the cause is what the owner is told and a flood must
        // never be reported as a failed manual press.
        assert!(CloseCause::Flood.overrides(CloseCause::Manual));
        assert!(CloseCause::Flood.overrides(CloseCause::VolumeCap));
        assert!(!CloseCause::Flood.overrides(CloseCause::Flood), "a flood does not restart itself");
        for c in [CloseCause::Manual, CloseCause::VolumeCap] {
            for other in [CloseCause::Flood, CloseCause::Manual, CloseCause::VolumeCap] {
                assert!(!c.overrides(other), "{c:?} must not override {other:?}");
            }
        }
    }

    #[test]
    fn a_cause_marks_the_run_with_the_end_reason_that_names_it() {
        use crate::cycle::EndReason;
        assert_eq!(CloseCause::Flood.end_reason(), EndReason::FloodShutoff);
        assert_eq!(CloseCause::VolumeCap.end_reason(), EndReason::VolumeCap);
        assert_eq!(CloseCause::Manual.end_reason(), EndReason::Manual);
        // The wire spellings the cloud and hub-lite match on.
        assert_eq!(CloseCause::Flood.as_str(), "flood");
        assert_eq!(CloseCause::VolumeCap.as_str(), "volume_cap");
        assert_eq!(CloseCause::Manual.as_str(), "manual");
    }

    #[test]
    fn the_event_name_cannot_be_read_as_a_flood_a_clear_or_telemetry() {
        // The three traps in the worker's events.ts, checked against the name itself rather than
        // against a comment claiming it is safe. `is_flood_shutoff` IS the ported classifier.
        let e = CLOSE_UNCONFIRMED_EVENT;
        assert!(!crate::linktap_runtime::is_flood_shutoff(e), "{e} must not close valves");
        assert!(!e.contains("flood") && !e.contains("leak") && !e.contains("alarm"));
        assert!(!e.ends_with("_off") && !e.ends_with(".off") && !e.contains("off"));
        assert!(!e.ends_with(".change") && !e.ends_with(".measurement"));
        // `closed` is claimed by the security category rule; `close_unconfirmed` is not `closed`.
        assert!(!e.contains("closed"));
        // And it still reads as a LinkTap valve event, so it categorises as water like its siblings.
        assert!(e.starts_with("linktap.valve."));
    }

    #[test]
    fn the_alert_says_which_cause_how_many_tries_and_how_long() {
        let mut w = CloseWatch::opened(CloseCause::Flood, T0);
        w.attempts = 9;
        let p = unconfirmed_params(&w, at(300));
        assert_eq!(p, vec![
            ("cause".to_string(), "flood".to_string()),
            ("attempts".to_string(), "9".to_string()),
            ("secs".to_string(), "300".to_string()),
        ]);
        let m = unconfirmed_params(&CloseWatch::opened(CloseCause::Manual, T0), at(300));
        assert_eq!(m[0], ("cause".to_string(), "manual".to_string()));
    }
}
