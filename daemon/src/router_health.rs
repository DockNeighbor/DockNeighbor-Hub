// ROUTER UP/DOWN GRACE — when a managed router, modem or Starlink dish is REPORTED down.
//
// Owner ruling (Jonathan, 2026-09-17): router polling was too aggressive and router cards flapped. "A
// single failed poll or timeout never marks a router down"; retry after a few seconds, back off while
// failures continue; report DOWN only once failures have been CONTINUOUS for a 30–60 s grace window;
// recover to UP immediately on the first success. "Down" is BOTH a poll that failed (unreachable, API
// error, timeout) AND a poll that answered with the uplink down (a WAN down, a modem not connected, a
// Starlink outage) — a single down sample waits out the same window.
//
// The values, and why:
// - DOWN_GRACE_MS = 45 s — the middle of the owner's 30–60 s. With the poll's 30 s per-request timeout
//   (routers::POLL_HTTP_TIMEOUT) a router that has stopped answering needs at least two attempts
//   (0 s → timeout at 30 s, retry at 35 s) before 45 s is reached, so one hung request can never be
//   the whole window on its own; 30 s could be. Nothing in the code argues for another value.
// - The retry schedule after the Nth consecutive bad sample: 5 s, 10 s, 20 s, 40 s, then 60 s (cap).
//   5 s is the poll loop's tick (hub_server ROUTER_TICK_SECS), so it is the quickest retry the loop
//   can make. 60 s caps how long a down router waits for its recovery to be seen, and it is half the
//   120 s idle cadence, so a down router is still checked more often than a healthy one; the loop
//   never checks a failing router LESS often than its normal cadence either (`due`).
// - Until down is decided, a retry is clamped to land on the grace deadline (first bad sample + 45 s),
//   so down is decided at ~45 s rather than at whichever backoff step comes after it.
// - A Starlink that reports, in the same answer, an outage it has itself measured at ≥ 45 s
//   (DishOutage.duration_ns) counts at once — the dish has already watched the window elapse (a hub
//   restarted mid-outage need not wait another 45 s). A dish that does not report a duration waits
//   the window like everything else.
//
// PURE: no clock, no I/O. hub_server's router_poll_loop is the shell: it samples, calls `observe`
// with the times it read, and reports what the verdict says.

/// Bad samples must be continuous for this long before a router is reported down.
pub const DOWN_GRACE_MS: i64 = 45_000;
/// The first retry after a bad sample.
pub const FIRST_RETRY_MS: i64 = 5_000;
/// The backoff never waits longer than this between attempts.
pub const MAX_RETRY_MS: i64 = 60_000;

/// What one poll of a router found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sample {
    /// The poll answered and the uplink is up (or the read carries no up/down at all).
    Up,
    /// The poll answered and says the uplink is down. `measured_ms` is how long the device itself
    /// says this outage has lasted, when it says (a Starlink's DishOutage duration).
    ReportedDown { measured_ms: Option<i64> },
    /// The poll failed: unreachable, refused, an API error or a timeout.
    Failed,
}

/// What the hub has told the cloud about the router's up/down state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Reported {
    #[default]
    Up,
    Down,
}

/// What to report after a sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A good sample: report this reading as it is. `recovered` = the router had been reported down,
    /// so this is the up transition.
    Report { recovered: bool },
    /// A bad sample inside the grace window: keep reporting the LAST GOOD reading, unchanged.
    Hold,
    /// This bad sample completed the window: report down now (the one down transition).
    WentDown,
    /// Already reported down and still bad: keep reporting down.
    StillDown,
}

/// Per router: the grace state machine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouterHealth {
    /// When the first bad sample of the current run was TAKEN (the attempt's start), None while good.
    pub first_failure_at: Option<i64>,
    /// Bad samples in a row.
    pub consecutive_failures: u32,
    /// When to try again while bad (None = the normal cadence applies).
    pub next_retry_at: Option<i64>,
    pub reported: Reported,
}

/// PURE: the wait after the Nth consecutive bad sample (n ≥ 1): 5 s, 10 s, 20 s, 40 s, 60 s, 60 s, …
pub fn retry_delay_ms(n: u32) -> i64 {
    let doublings = n.saturating_sub(1).min(16);
    (FIRST_RETRY_MS << doublings).min(MAX_RETRY_MS)
}

impl RouterHealth {
    /// Fold one sample in. `started_ms` is when the poll began (a bad run is dated from the start of its
    /// first attempt — the router was already not answering then), `finished_ms` when it came back.
    pub fn observe(&mut self, sample: Sample, started_ms: i64, finished_ms: i64) -> Verdict {
        let measured_ms = match sample {
            Sample::Up => {
                let recovered = self.reported == Reported::Down;
                *self = RouterHealth::default();
                return Verdict::Report { recovered };
            }
            Sample::ReportedDown { measured_ms } => measured_ms,
            Sample::Failed => None,
        };
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let first = *self.first_failure_at.get_or_insert(started_ms);
        let backoff_at = finished_ms + retry_delay_ms(self.consecutive_failures);
        if self.reported == Reported::Down {
            self.next_retry_at = Some(backoff_at);
            return Verdict::StillDown;
        }
        let measured = measured_ms.is_some_and(|m| m >= DOWN_GRACE_MS);
        let waited = self.consecutive_failures >= 2 && finished_ms - first >= DOWN_GRACE_MS;
        if measured || waited {
            self.reported = Reported::Down;
            self.next_retry_at = Some(backoff_at);
            return Verdict::WentDown;
        }
        // Land the next attempt on the deadline if the backoff would overshoot it.
        let deadline = first + DOWN_GRACE_MS;
        self.next_retry_at = Some(backoff_at.min(deadline).max(finished_ms));
        Verdict::Hold
    }

    /// Is a poll due? A bad run polls at its retry time; either way a router is never polled LESS often
    /// than its normal cadence (`last_poll_ms` = when it was last polled, None = never).
    pub fn due(&self, now_ms: i64, last_poll_ms: Option<i64>, cadence_ms: i64) -> bool {
        let cadence_due = last_poll_ms.map_or(true, |t| now_ms - t >= cadence_ms);
        cadence_due || self.next_retry_at.is_some_and(|t| now_ms >= t)
    }
}

/// PURE: the report for a router whose POLL FAILED past the grace window — its last reported reading
/// with `up` set to 0 (every other field is what it last said). None when there was never a reading.
pub fn down_params(last: Option<&[(String, String)]>) -> Option<Vec<(String, String)>> {
    let last = last?;
    let mut p: Vec<(String, String)> = last.iter().filter(|(k, _)| k != "up").cloned().collect();
    p.insert(0, ("up".into(), "0".into()));
    Some(p)
}

/// PURE: the sample a poll's report makes. `up=0` is a reported-down uplink; anything else is up.
pub fn sample_of(params: Option<&[(String, String)]>, measured_ms: Option<i64>) -> Sample {
    match params.and_then(|p| p.iter().rev().find(|(k, _)| k == "up")) {
        Some((_, v)) if v == "0" => Sample::ReportedDown { measured_ms },
        _ => Sample::Up,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000;

    /// Instant polls at the given seconds, all bad; returns the verdicts.
    fn fail_at(h: &mut RouterHealth, secs: &[i64]) -> Vec<Verdict> {
        secs.iter().map(|t| h.observe(Sample::Failed, t * S, t * S)).collect()
    }

    #[test]
    fn a_single_failure_never_marks_down() {
        let mut h = RouterHealth::default();
        assert_eq!(h.observe(Sample::Failed, 0, 30 * S), Verdict::Hold, "one timed-out poll is not down");
        assert_eq!(h.reported, Reported::Up);
        assert_eq!(h.consecutive_failures, 1);
        assert_eq!(h.first_failure_at, Some(0), "the run is dated from the attempt's start");
        // Even a single failure whose attempt took longer than the window is still one failure.
        let mut slow = RouterHealth::default();
        assert_eq!(slow.observe(Sample::Failed, 0, 50 * S), Verdict::Hold);
        assert_eq!(slow.reported, Reported::Up);
    }

    #[test]
    fn failures_for_44_s_do_not_mark_down_and_45_s_do() {
        let mut h = RouterHealth::default();
        assert_eq!(fail_at(&mut h, &[0, 5, 15, 35, 44]), vec![Verdict::Hold; 5]);
        assert_eq!(h.reported, Reported::Up);
        assert_eq!(h.observe(Sample::Failed, 45 * S, 45 * S), Verdict::WentDown);
        assert_eq!(h.reported, Reported::Down);
        assert_eq!(h.observe(Sample::Failed, 60 * S, 60 * S), Verdict::StillDown, "down is sent once");
    }

    #[test]
    fn the_retry_schedule_is_quick_then_exponential_to_the_cap() {
        assert_eq!((1..=8).map(retry_delay_ms).collect::<Vec<_>>(), vec![5 * S, 10 * S, 20 * S, 40 * S, 60 * S, 60 * S, 60 * S, 60 * S]);
        assert_eq!(retry_delay_ms(u32::MAX), MAX_RETRY_MS, "no overflow however long it stays down");

        let mut h = RouterHealth::default();
        h.observe(Sample::Failed, 0, 0);
        assert_eq!(h.next_retry_at, Some(5 * S), "first retry a few seconds after the failure");
        h.observe(Sample::Failed, 5 * S, 5 * S);
        assert_eq!(h.next_retry_at, Some(15 * S));
        h.observe(Sample::Failed, 15 * S, 15 * S);
        assert_eq!(h.next_retry_at, Some(35 * S));
        h.observe(Sample::Failed, 35 * S, 35 * S);
        assert_eq!(h.next_retry_at, Some(45 * S), "40 s would overshoot the window: clamped to its deadline");
        assert_eq!(h.observe(Sample::Failed, 45 * S, 45 * S), Verdict::WentDown);
        assert_eq!(h.next_retry_at, Some(105 * S), "after down the backoff carries on: 5th bad sample waits the 60 s cap");
        h.observe(Sample::Failed, 105 * S, 105 * S);
        assert_eq!(h.next_retry_at, Some(165 * S), "capped");
    }

    #[test]
    fn a_timed_out_poll_is_retried_and_down_needs_a_second_attempt() {
        // 30 s timeouts: 0→30 fails, retry at 35→65 fails; the run is 65 s old → down on the 2nd.
        let mut h = RouterHealth::default();
        assert_eq!(h.observe(Sample::Failed, 0, 30 * S), Verdict::Hold);
        assert_eq!(h.next_retry_at, Some(35 * S));
        assert_eq!(h.observe(Sample::Failed, 35 * S, 65 * S), Verdict::WentDown);
    }

    #[test]
    fn the_first_success_after_down_restores_up_at_once() {
        let mut h = RouterHealth::default();
        fail_at(&mut h, &[0, 5, 15, 35, 45]);
        assert_eq!(h.reported, Reported::Down);
        assert_eq!(h.observe(Sample::Up, 50 * S, 50 * S), Verdict::Report { recovered: true });
        assert_eq!(h, RouterHealth::default(), "normal cadence again: no retry pending, no run");
        assert_eq!(h.observe(Sample::Up, 60 * S, 60 * S), Verdict::Report { recovered: false }, "one up transition, not two");
        // A success inside the window is not a recovery (it was never reported down) and resets the run.
        let mut g = RouterHealth::default();
        fail_at(&mut g, &[0, 5, 15, 35]);
        assert_eq!(g.observe(Sample::Up, 40 * S, 40 * S), Verdict::Report { recovered: false });
        assert_eq!(fail_at(&mut g, &[50, 55, 65, 85]), vec![Verdict::Hold; 4], "a new run starts its own 45 s");
    }

    #[test]
    fn a_single_outage_sample_waits_the_window_but_45_s_continuous_goes_down() {
        let out = Sample::ReportedDown { measured_ms: None };
        let mut h = RouterHealth::default();
        assert_eq!(h.observe(out, 0, 0), Verdict::Hold);
        assert_eq!(h.observe(Sample::Up, 5 * S, 5 * S), Verdict::Report { recovered: false }, "a blip never went out");
        let mut h = RouterHealth::default();
        for t in [0, 5, 15, 35, 44] {
            assert_eq!(h.observe(out, t * S, t * S), Verdict::Hold);
        }
        assert_eq!(h.observe(out, 45 * S, 45 * S), Verdict::WentDown);
        // Failures and down reads are one continuous bad run.
        let mut m = RouterHealth::default();
        assert_eq!(m.observe(Sample::Failed, 0, 0), Verdict::Hold);
        assert_eq!(m.observe(out, 45 * S, 45 * S), Verdict::WentDown);
    }

    #[test]
    fn a_dish_that_measured_its_own_outage_past_the_window_counts_at_once() {
        let mut h = RouterHealth::default();
        assert_eq!(h.observe(Sample::ReportedDown { measured_ms: Some(44 * S) }, 0, 0), Verdict::Hold);
        let mut h = RouterHealth::default();
        assert_eq!(h.observe(Sample::ReportedDown { measured_ms: Some(45 * S) }, 0, 0), Verdict::WentDown);
        // A poll FAILURE has no measured duration: one failure is never down, whatever came before.
        let mut f = RouterHealth::default();
        assert_eq!(f.observe(Sample::Failed, 0, 0), Verdict::Hold);
    }

    #[test]
    fn due_follows_the_retry_but_never_falls_behind_the_cadence() {
        let good = RouterHealth::default();
        assert!(good.due(0, None, 120 * S), "never polled: due");
        assert!(!good.due(119 * S, Some(0), 120 * S));
        assert!(good.due(120 * S, Some(0), 120 * S));
        let mut bad = RouterHealth::default();
        bad.observe(Sample::Failed, 0, 0);
        assert!(!bad.due(4 * S, Some(0), 120 * S));
        assert!(bad.due(5 * S, Some(0), 120 * S), "the quick retry");
        let mut down = RouterHealth::default();
        fail_at(&mut down, &[0, 5, 15, 35, 45]);
        assert_eq!(down.next_retry_at, Some(105 * S));
        assert!(down.due(75 * S, Some(45 * S), 30 * S), "a 30 s leased cadence beats the 60 s backoff");
    }

    #[test]
    fn a_failed_poll_past_the_window_reports_the_last_reading_with_up_0() {
        let p = |v: &[(&str, &str)]| v.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        let last = p(&[("up", "1"), ("wan", "wired"), ("model", "Balance One")]);
        assert_eq!(down_params(Some(&last)), Some(p(&[("up", "0"), ("wan", "wired"), ("model", "Balance One")])));
        assert_eq!(down_params(None), None, "never read since start: nothing invented");
        assert_eq!(sample_of(Some(&last), None), Sample::Up);
        assert_eq!(sample_of(Some(&p(&[("up", "0"), ("wan", "starlink")])), Some(3)), Sample::ReportedDown { measured_ms: Some(3) });
        assert_eq!(sample_of(None, None), Sample::Up, "a read with nothing to report is not a down");
    }
}
