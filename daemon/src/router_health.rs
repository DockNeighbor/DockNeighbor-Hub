// ROUTER UP/DOWN GRACE — when a managed router, modem or Starlink dish is REPORTED down.
//
// ⚠️ 2026-09-24, Jonathan, SECOND RULING: "the way you are measuring up/down on routers is wrong.
// It's too aggressive and not actually logging the state." The grace window below is unchanged; what
// changed is that the hub no longer reports DOWN for a router it could not READ — see the `upSrc`
// block after this one, `unread_params` and `common_mode_failure`. "Down" below now means only what
// a device SAID, never what the hub failed to find out.
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
// - A device that reports, in the same answer, an outage it has itself MEASURED at ≥ 45 s counts at
//   once — it has already watched the window elapse, so a hub restarted mid-outage need not wait
//   another 45 s. A device that reports no duration waits the window like everything else.
//
// ⚠️ A STARLINK HAS ITS OWN, LONGER THRESHOLD, AND FOR A DISH IT IS THE ONE THAT DECIDES.
// Owner ruling (Jonathan, 2026-09-24): an outage is "no internet for over 2 minutes", and every
// cause the dish reports counts ("all of them except connectd"). starlink::OUTAGE_MIN_MS is
// therefore 120 s, and starlink::wan_of only ever produces `up=0` for a dish whose outage is
// ALREADY past it (starlink::carry_outage times the run).
//
// The 45 s window below NEVER ADDS to those two minutes. The first down sample a dish can produce
// carries a measured duration over 120 s, which is ≥ DOWN_GRACE_MS, so the measured-duration
// shortcut above reports it down in that same poll instead of waiting a further 45 s. A dish goes
// down at ~2 min, not at 2 min 45 s, and never at 45 s. Proven in starlink.rs and at the poll loop
// in hub_server.rs.
//
// 45 s still governs everything else, INCLUDING a dish poll that FAILED: a reading the hub could
// not make is not an outage — it is a lost reading, held as `upSrc=unread` — and it waits the same
// 45 s as any other router, whatever the vendor.
//
// PURE: no clock, no I/O. hub_server's router_poll_loop is the shell: it samples, calls `observe`
// with the times it read, and reports what the verdict says.

// WHERE `up` CAME FROM — `upSrc`, the param added 0.3.54 (owner ruling, Jonathan 2026-09-24: "the
// way you are measuring up/down on routers is wrong. It's too aggressive and not actually logging
// the state").
//
// 🔴 AN UNREADABLE ROUTER IS NOT A DOWN ROUTER. Until 0.3.54 a poll that kept failing past the
// grace window reported the last reading with `up` REPLACED BY 0 — so a hub that had lost its own
// LAN, or a router whose API had gone quiet, produced a link-down on CENTRAL's Starlink and wired
// Peplink, which are the links the owner streams video over. The hub now reports what it actually
// knows: the last `up` the router DID say, marked `upSrc=unread`, with a short `reason`.
//
// The vocabulary, and the one compatibility rule: `upSrc` ABSENT means `read`. Every reading an
// older daemon ever sent, and every reading already stored in the cloud, is therefore a `read` —
// which is what it was.
/// The param name.
pub const UP_SRC: &str = "upSrc";
/// The router answered and this is what it said.
pub const UP_SRC_READ: &str = "read";
/// The hub could not read the router. `up` beside this is the last value the router DID say — it
/// is NOT evidence about the link now, and nothing downstream may treat it as a down.
pub const UP_SRC_UNREAD: &str = "unread";
/// The short failure class, or the vendor's own words, beside an `upSrc=unread` reading.
pub const REASON: &str = "reason";

/// PURE: the two params every READ report opens with — `up`, and `upSrc=read` beside it so a
/// reader never has to infer it from an absence.
pub fn up_read(up: bool) -> Vec<(String, String)> {
    vec![
        ("up".into(), if up { "1" } else { "0" }.into()),
        (UP_SRC.into(), UP_SRC_READ.into()),
    ]
}

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

/// PURE: the report for a router the hub COULD NOT READ past the grace window — its last reported
/// reading with `up` LEFT EXACTLY AS THE ROUTER LAST SAID IT, marked `upSrc=unread`, plus a short
/// `reason`. Every other field is what it last said.
///
/// 🔴 THIS USED TO INSERT `up=0` (D1). It does not any more: an unreadable router is not a down
/// router, and the hub has no business asserting a link state it did not read. See the `upSrc`
/// note at the top of this file.
///
/// After a RESTART there is no last reading (`last` is None) — and reporting nothing at all is how
/// a router that was already unreachable when the hub came up stayed invisible to the cloud
/// forever (D8). So None still produces a report: `upSrc=unread` and the reason, WITHOUT an `up`
/// field, because the hub genuinely does not know one and will not invent it. `identity` is
/// whatever the hub can honestly say about the device (its agent version, and a model/firmware it
/// probed before the restart) — never a link state.
pub fn unread_params(
    last: Option<&[(String, String)]>,
    reason: Option<&str>,
    identity: &[(String, String)],
) -> Vec<(String, String)> {
    let mut p: Vec<(String, String)> = match last {
        Some(l) => l.iter().filter(|(k, _)| k != UP_SRC && k != REASON).cloned().collect(),
        None => identity.iter().filter(|(k, _)| k != "up" && k != UP_SRC && k != REASON).cloned().collect(),
    };
    // Right after `up` when there is one, so the pair reads together; first otherwise.
    let at = p.iter().position(|(k, _)| k == "up").map_or(0, |i| i + 1);
    p.insert(at, (UP_SRC.into(), UP_SRC_UNREAD.into()));
    if let Some(r) = reason.map(str::trim).filter(|r| !r.is_empty()) {
        p.push((REASON.into(), r.to_string()));
    }
    p
}

/// PURE: the sample a poll's report makes. `up=0` is a reported-down uplink — but ONLY when the
/// same report says the hub READ it (`upSrc` absent or `read`). A reading carried forward with
/// `upSrc=unread` is the hub's own memory, not a fresh down, and folding it back in as one would
/// let a single unreadable router re-arm the grace machine off its own held report.
pub fn sample_of(params: Option<&[(String, String)]>, measured_ms: Option<i64>) -> Sample {
    let get = |key: &str| params.and_then(|p| p.iter().rev().find(|(k, _)| k == key)).map(|(_, v)| v.as_str());
    if get(UP_SRC).is_some_and(|s| s == UP_SRC_UNREAD) {
        return Sample::Up;
    }
    match get("up") {
        Some("0") => Sample::ReportedDown { measured_ms },
        _ => Sample::Up,
    }
}

/// PURE: did this poll pass fail in a way that means THE HUB lost its own LAN, rather than the
/// routers going down? (D7, owner ruling 2026-09-24.)
///
/// Two or more managed routers OF DIFFERENT VENDORS failing to read in the same pass is not a
/// coincidence. A boat does not lose a Cradlepoint and a Peplink at the same instant; it loses the
/// switch they both hang off, or the hub's own network interface. Two failures of the SAME vendor
/// are not this — one bad firmware or one bad credential explains those — so the rule is
/// deliberately about vendor DIVERSITY, not about a count.
///
/// `failed_vendors` is the vendor of every router whose read failed in one pass, in any order.
pub fn common_mode_failure(failed_vendors: &[&str]) -> bool {
    let mut seen: Vec<&str> = Vec::new();
    for v in failed_vendors {
        let v = v.trim();
        if !v.is_empty() && !seen.contains(&v) {
            seen.push(v);
        }
    }
    seen.len() >= 2
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

    fn p(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// D1 — THE DEFECT ITSELF. An unreadable router keeps the `up` its router last GAVE; the hub
    /// never substitutes a 0 for a reading it did not make.
    #[test]
    fn an_unreadable_router_keeps_its_last_up_and_is_marked_unread() {
        let last = p(&[("up", "1"), ("wan", "wired"), ("model", "Balance One")]);
        let held = unread_params(Some(&last), Some("timeout"), &[]);
        assert_eq!(
            held,
            p(&[("up", "1"), ("upSrc", "unread"), ("wan", "wired"), ("model", "Balance One"), ("reason", "timeout")]),
            "the last GOOD reading, `up` untouched, marked unread with its reason"
        );
        assert!(!held.iter().any(|(k, v)| k == "up" && v == "0"), "a router the hub cannot read is NEVER reported down");

        // A router whose last reading was a genuine, READ down keeps that too — the hub does not
        // flip it back up either. Only the source changes.
        let was_down = p(&[("up", "0"), ("upSrc", "read"), ("wan", "none")]);
        assert_eq!(unread_params(Some(&was_down), None, &[]), p(&[("up", "0"), ("upSrc", "unread"), ("wan", "none")]));

        // `upSrc`/`reason` are replaced, never stacked, however long the router stays unreadable.
        let again = unread_params(Some(&held), Some("unreachable"), &[]);
        assert_eq!(again.iter().filter(|(k, _)| k == "upSrc").count(), 1);
        assert_eq!(again.iter().filter(|(k, _)| k == "reason").count(), 1);
        assert_eq!(again.iter().rev().find(|(k, _)| k == "reason").unwrap().1, "unreachable");
    }

    /// D8 — after a restart there is no last reading, and reporting NOTHING is how a router that
    /// was already unreachable at boot stayed invisible. Identity, `upSrc=unread`, no invented `up`.
    #[test]
    fn a_router_unreachable_since_boot_is_still_reported_without_inventing_an_up() {
        let identity = p(&[("model", "Balance One"), ("fw", "8.5.5"), ("av", "hub-0.3.54")]);
        let first = unread_params(None, Some("unreachable"), &identity);
        assert_eq!(first[0], ("upSrc".to_string(), "unread".to_string()), "the first thing it says is that it did not read");
        assert!(!first.iter().any(|(k, _)| k == "up"), "no last reading means no `up` — the hub does not guess one");
        assert_eq!(first.iter().rev().find(|(k, _)| k == "reason").unwrap().1, "unreachable");
        for (k, v) in &identity {
            assert!(first.contains(&(k.clone(), v.clone())), "identity `{k}` is what the hub CAN honestly say");
        }
        // An `up` smuggled in through `identity` is refused: identity is not a link state.
        let sneaky = p(&[("up", "1"), ("av", "hub-0.3.54")]);
        assert!(!unread_params(None, None, &sneaky).iter().any(|(k, _)| k == "up"));
    }

    #[test]
    fn a_read_down_is_a_sample_but_a_held_unread_reading_is_not() {
        let last = p(&[("up", "1"), ("upSrc", "read"), ("wan", "wired")]);
        assert_eq!(sample_of(Some(&last), None), Sample::Up);
        assert_eq!(
            sample_of(Some(&p(&[("up", "0"), ("upSrc", "read"), ("wan", "starlink")])), Some(3)),
            Sample::ReportedDown { measured_ms: Some(3) }
        );
        assert_eq!(sample_of(Some(&p(&[("up", "0"), ("wan", "starlink")])), Some(3)), Sample::ReportedDown { measured_ms: Some(3) },
            "`upSrc` absent means READ — every reading an older daemon sent");
        // 🔴 The held reading carries `up=0` when the router's last READ said so. Folding that back
        // in as a fresh down would let one unreadable router re-arm the grace machine off its own memory.
        assert_eq!(sample_of(Some(&p(&[("up", "0"), ("upSrc", "unread")])), Some(99_000)), Sample::Up);
        assert_eq!(sample_of(None, None), Sample::Up, "a read with nothing to report is not a down");
    }

    /// D7 — two vendors failing at once is the HUB's LAN, not two routers.
    #[test]
    fn two_vendors_failing_together_is_the_hubs_own_network_and_one_vendor_is_not() {
        assert!(common_mode_failure(&["cradlepoint", "peplink"]));
        assert!(common_mode_failure(&["starlink", "peplink", "peplink"]));
        assert!(!common_mode_failure(&["peplink", "peplink"]), "two of ONE vendor is one bad firmware or one bad credential");
        assert!(!common_mode_failure(&["cradlepoint"]));
        assert!(!common_mode_failure(&[]), "a pass with no failures is not a LAN loss");
        assert!(!common_mode_failure(&["peplink", "", " "]), "an unset vendor is not a second vendor");
    }

    #[test]
    fn a_read_report_says_so_in_up_src() {
        assert_eq!(up_read(true), p(&[("up", "1"), ("upSrc", "read")]));
        assert_eq!(up_read(false), p(&[("up", "0"), ("upSrc", "read")]));
    }
}
