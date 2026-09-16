// The member-key set's REFRESH CADENCE — the pure half (this module decides, hub_server performs).
//
// WHAT THIS REPLACES, AND WHY. `key_sync_loop` slept 300 s and called `/api/hub/keys`
// UNCONDITIONALLY: 288 fetches a day from every daemon hub, each one about 22 Firestore reads on the
// worker (the vehicle document, the private key collection, the digest reconcile), for a set that
// changes when someone is invited or removed — a few times a year. Nothing gated it, so the whole
// fleet paid the poll to learn "no change" 287 times out of 288.
//
// The owner's approved cadence design is that KEY SYNC RIDES THE PAYLOAD REPLY, exactly as the watch
// config and the valve permission already do. The cloud signs the member-key set and puts that
// signature on the reply as flat `keysSig` (64 lowercase hex — the same value `/api/agent/member-keys`
// sends as its ETag; cloud-server routerMemberKeys.ts `memberKeySetSig`). A hub that already holds
// that signature knows its set is current and fetches NOTHING. So the fetch happens only when:
//
//   * BOOT — one sync, as before (a hub that was off while a member was removed must converge).
//   * SIGNATURE MISMATCH — a reply advertised a signature that is not the one we applied.
//   * THE LAN DOOR MET A KEY IT DOES NOT KNOW — a member whose key was just minted or rotated is
//     standing at the door; ask once, rate-limited (DOOR_MISS_GAP_MS), never per request.
//   * A SAFETY REFRESH at most once every SAFETY_REFRESH_SECS (24 h), so a hub whose cloud never
//     signs (an older worker, or a vehicle whose digest map has never been written) still converges
//     within a day instead of never.
//
// 🔴 A REPLY WITH NO `keysSig` MEANS "NO CHANGE", NOT "GO AND ASK". Falling back to a periodic
// conditional poll would hand an older worker — which is every worker until the hub half of the
// signature ships (see the contract note in hub_server.rs `fetch_member_keys_conditional`) — the old
// 288-a-day pattern under a new name. The safety refresh is the whole fallback, and it is daily.
//
// 🔴 A FAILED FETCH KEEPS THE LAST KNOWN SET. That is the property the loop exists to protect: the
// key set is every member's LAN management access, and a network drop, a 401 or a 500 must never
// lock the owner out of a hub that is otherwise fine. A failure also BACKS OFF (BACKOFF_START_SECS
// doubling to BACKOFF_MAX_SECS, reset on success) instead of retrying on the loop's own tick, so a
// worker outage costs a handful of attempts an hour rather than one every tick.
//
// The live push is unaffected and remains the fast path for revocation: the worker pushes the new
// set down the hub's socket the moment a key is minted, rotated or a member removed (hub_relay.rs
// `apply_keys`, cloud-server `pushKeys`). This module only paces the BACKSTOP behind that push.

use crate::hub_config::MemberKey;

/// The backstop refresh for a hub whose cloud never signs the set. Daily, and daily only.
pub const SAFETY_REFRESH_SECS: i64 = 24 * 60 * 60;
/// First wait after a failed fetch; it doubles per consecutive failure.
pub const BACKOFF_START_SECS: i64 = 60;
/// Ceiling on that doubling — a worker outage settles at two attempts an hour.
pub const BACKOFF_MAX_SECS: i64 = 30 * 60;
/// How often the LAN door may ring the sync after meeting a key it does not know. An unknown key is
/// also what a scan of the door looks like, so this is a rate limit first and a courtesy second.
pub const DOOR_MISS_GAP_MS: i64 = 60_000;
/// The loop's own tick. Nothing is fetched on a tick that is not due — it is the clock the safety
/// refresh and the failure backoff are read against, and every real trigger rings the loop awake.
pub const TICK_SECS: u64 = 60;

/// The roles the signature covers, in the cloud's own vocabulary (routerMemberKeys.ts `ROLES`).
/// A role outside it is dropped from the signed set there, so it must be dropped here too or the two
/// sides would sign the same membership differently.
const SIGNED_ROLES: [&str; 6] = ["owner", "coowner", "admin", "control", "monitor", "monitor_quiet"];

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in d {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// PURE: is this a member-key signature — 64 lowercase hex, the only shape the cloud sends?
/// Anything else is not compared against and not stored: a malformed value must never be adopted as
/// "the set we hold", because that would suppress the very fetch it should have caused.
pub fn valid_sig(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// PURE: the signature of a member-key set, the Rust twin of cloud-server routerMemberKeys.ts
/// (`routerMemberKeys` → `routerMemberSet` → `memberKeySetSig`): each member as `sha256(key)` plus
/// their role, keys without a signed role dropped, sorted, joined as `"<digest> <role>\n"`, and the
/// SHA-256 of that. The same membership therefore signs identically on both sides.
///
/// It is used for the sets that arrive WITHOUT a signature — the socket push, and a 200 from a
/// worker that sends no ETag — so that the next reply's `keysSig` has something to agree with. A
/// signature the cloud actually advertised always outranks it (see `applied_set`), which is what
/// keeps a disagreement between the two computations from looping: the cloud's word is adopted once
/// and the mismatch ends.
pub fn member_set_sig(keys: &[MemberKey]) -> String {
    let mut set: Vec<(String, &str)> = keys
        .iter()
        .filter(|k| !k.key.is_empty() && SIGNED_ROLES.contains(&k.role.as_str()))
        .map(|k| (sha256_hex(k.key.as_bytes()), k.role.as_str()))
        .collect();
    set.sort();
    let mut line_form = String::new();
    for (h, role) in &set {
        line_form.push_str(h);
        line_form.push(' ');
        line_form.push_str(role);
        line_form.push('\n');
    }
    sha256_hex(line_form.as_bytes())
}

/// PURE: the flat `keysSig` a batch / check-in reply carried, when it carried a well-formed one.
pub fn parse_keys_sig(body: &serde_json::Value) -> Option<String> {
    let s = body.get("keysSig")?.as_str()?;
    valid_sig(s).then(|| s.to_string())
}

/// Why a fetch is due. Log-facing, and what the tests assert on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// First pass after start-up.
    Boot,
    /// A reply advertised a signature that is not the one we hold.
    Signature,
    /// The LAN door met a key the set does not contain.
    DoorMiss,
    /// Nothing has confirmed the set for SAFETY_REFRESH_SECS.
    Safety,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Boot => "boot",
            Reason::Signature => "signature changed",
            Reason::DoorMiss => "an unknown key at the LAN door",
            Reason::Safety => "the daily safety refresh",
        }
    }
}

/// The gate itself: everything the loop needs to decide whether to spend a request. Pure — every
/// method takes the clock, so the whole cadence is testable without a timer or a socket.
#[derive(Debug)]
pub struct KeySync {
    /// The signature of the set we currently hold, when we know it. Persisted next to the key set
    /// (hub.json `member_keys_sig`) so a restart does not re-fetch what it already has.
    applied: Option<String>,
    /// Set when something makes a fetch due, cleared when one lands.
    want: Option<Reason>,
    /// The signature that made `want` Signature — adopted verbatim once the fetch succeeds.
    want_sig: Option<String>,
    /// When the set was last CONFIRMED current: a 200, a 304, a push, or a reply whose signature
    /// agreed with ours. The safety refresh is measured from here.
    last_ok_ms: i64,
    /// Consecutive failed fetches, for the backoff.
    fails: u32,
    /// No fetch before this instant (the backoff). 0 when not backing off.
    retry_at_ms: i64,
    /// When the LAN door last rang, for DOOR_MISS_GAP_MS.
    last_door_ms: i64,
}

impl KeySync {
    /// At start-up: whatever signature was persisted with the key set, and one due sync (4).
    pub fn boot(applied: Option<String>, now_ms: i64) -> Self {
        Self {
            applied: applied.filter(|s| valid_sig(s)),
            want: Some(Reason::Boot),
            want_sig: None,
            last_ok_ms: now_ms,
            fails: 0,
            retry_at_ms: 0,
            last_door_ms: 0,
        }
    }

    /// What to send as `If-None-Match` — the signature of the set we hold.
    pub fn if_none_match(&self) -> Option<&str> {
        self.applied.as_deref()
    }

    /// Is a fetch due right now, and why? None while backing off from a failure.
    pub fn due(&self, now_ms: i64) -> Option<Reason> {
        if now_ms < self.retry_at_ms {
            return None;
        }
        if let Some(r) = self.want {
            return Some(r);
        }
        (now_ms - self.last_ok_ms >= SAFETY_REFRESH_SECS * 1000).then_some(Reason::Safety)
    }

    /// A payload reply arrived. `sig` is its flat `keysSig`, absent on an older worker.
    ///
    /// A signature EQUAL to ours is positive proof the set is current — it restarts the safety
    /// clock and retires a safety refresh that was only due because nothing had confirmed anything.
    /// A DIFFERENT one makes a fetch due. NO signature says nothing at all, and so changes nothing:
    /// see the module header for why that is not a fallback poll.
    pub fn note_reply_sig(&mut self, sig: Option<&str>, now_ms: i64) {
        let Some(sig) = sig.filter(|s| valid_sig(s)) else { return };
        if self.applied.as_deref() == Some(sig) {
            self.last_ok_ms = now_ms;
            if self.want == Some(Reason::Safety) {
                self.want = None;
            }
            return;
        }
        self.want_sig = Some(sig.to_string());
        if self.want.is_none() {
            self.want = Some(Reason::Signature);
        }
    }

    /// The LAN door met a key that is not in the set. Returns whether this one actually rang (false
    /// while inside DOOR_MISS_GAP_MS), so the caller only wakes the loop when there is a point.
    pub fn note_door_miss(&mut self, now_ms: i64) -> bool {
        if self.last_door_ms != 0 && now_ms - self.last_door_ms < DOOR_MISS_GAP_MS {
            return false;
        }
        self.last_door_ms = now_ms;
        if self.want.is_none() {
            self.want = Some(Reason::DoorMiss);
        }
        true
    }

    /// The worker pushed a set down the socket (hub_relay.rs). It IS the current set, so it both
    /// confirms freshness and gives the next reply's `keysSig` something to agree with.
    pub fn note_pushed(&mut self, sig: String, now_ms: i64) -> String {
        self.applied = Some(sig.clone());
        self.want = None;
        self.want_sig = None;
        self.last_ok_ms = now_ms;
        self.fails = 0;
        self.retry_at_ms = 0;
        sig
    }

    /// A fetch returned a set. The signature adopted is, in order: the one the worker sent as an
    /// ETag, the one the reply advertised (so the cloud's own word ends any disagreement about how
    /// the set signs), then our own computation of it.
    pub fn applied_set(&mut self, etag: Option<String>, local: String, now_ms: i64) -> String {
        let sig = etag.filter(|s| valid_sig(s)).or_else(|| self.want_sig.take()).unwrap_or(local);
        self.applied = Some(sig.clone());
        self.want = None;
        self.want_sig = None;
        self.last_ok_ms = now_ms;
        self.fails = 0;
        self.retry_at_ms = 0;
        sig
    }

    /// A conditional fetch answered 304: the set we hold is the current one. Nothing to apply,
    /// nothing to write, and the clock restarts.
    pub fn not_modified(&mut self, now_ms: i64) {
        self.want = None;
        self.want_sig = None;
        self.last_ok_ms = now_ms;
        self.fails = 0;
        self.retry_at_ms = 0;
    }

    /// A fetch failed (no network, an HTTP error, a 401). The last known set is untouched — the
    /// caller never had a new one to apply — and the next attempt is pushed out. Returns the wait,
    /// in seconds, for the log line.
    pub fn failed(&mut self, now_ms: i64) -> i64 {
        self.fails = self.fails.saturating_add(1);
        let wait = BACKOFF_START_SECS
            .saturating_mul(1i64 << (self.fails - 1).min(20))
            .min(BACKOFF_MAX_SECS);
        self.retry_at_ms = now_ms + wait * 1000;
        wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(key: &str, role: &str) -> MemberKey {
        MemberKey { key: key.into(), uid: format!("uid-{key}"), role: role.into() }
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn a_signature_is_sixty_four_lowercase_hex_and_nothing_else() {
        assert!(valid_sig(A));
        assert!(!valid_sig(&A.to_uppercase()), "uppercase is not the shape the cloud sends");
        assert!(!valid_sig(""));
        assert!(!valid_sig(&A[..63]));
        assert!(!valid_sig(&format!("{}g", &A[..63])));
    }

    #[test]
    fn member_set_sig_matches_the_clouds_line_form_and_ignores_order_and_unsigned_roles() {
        // The cloud's canonical form: sorted `"<sha256(key)> <role>\n"` lines, hashed.
        let one = member_set_sig(&[mk("k1", "owner"), mk("k2", "monitor")]);
        assert!(valid_sig(&one));
        assert_eq!(one, member_set_sig(&[mk("k2", "monitor"), mk("k1", "owner")]), "order cannot change it");
        assert_ne!(one, member_set_sig(&[mk("k1", "owner"), mk("k2", "control")]), "a role change must");
        assert_ne!(one, member_set_sig(&[mk("k1", "owner")]), "a removal must");
        assert_eq!(
            member_set_sig(&[mk("k1", "owner"), mk("k2", "stowaway"), mk("", "control")]),
            member_set_sig(&[mk("k1", "owner")]),
            "an unsigned role and an empty key are dropped, exactly as routerMemberSet drops them",
        );
        // Recomputed by hand the way routerMemberKeys.ts does it, so a drift in either is caught.
        let expect = {
            let mut lines: Vec<String> =
                vec![format!("{} owner\n", sha256_hex(b"k1")), format!("{} monitor\n", sha256_hex(b"k2"))];
            lines.sort();
            sha256_hex(lines.concat().as_bytes())
        };
        assert_eq!(one, expect);
    }

    #[test]
    fn parse_keys_sig_takes_the_flat_field_and_refuses_anything_malformed() {
        assert_eq!(parse_keys_sig(&serde_json::json!({ "keysSig": A })).as_deref(), Some(A));
        assert_eq!(parse_keys_sig(&serde_json::json!({ "status": "ok" })), None);
        assert_eq!(parse_keys_sig(&serde_json::json!({ "keysSig": "nope" })), None);
        assert_eq!(parse_keys_sig(&serde_json::json!({ "keysSig": 42 })), None);
    }

    #[test]
    fn boot_syncs_once_then_an_agreeing_signature_never_fetches() {
        let mut k = KeySync::boot(None, 0);
        assert_eq!(k.due(0), Some(Reason::Boot));
        k.applied_set(Some(A.into()), "local".into(), 0);
        assert_eq!(k.due(1), None, "the boot sync is the only one");
        // Every reply for the next 24 h agrees: nothing is due, ever.
        let mut t = 0;
        while t < SAFETY_REFRESH_SECS * 1000 {
            t += 15 * 60 * 1000;
            k.note_reply_sig(Some(A), t);
            assert_eq!(k.due(t), None, "an agreeing signature is proof, not a reason to ask");
        }
    }

    #[test]
    fn a_changed_signature_makes_exactly_one_fetch_due_and_the_new_signature_is_adopted() {
        let mut k = KeySync::boot(Some(A.into()), 0);
        k.applied_set(Some(A.into()), "local".into(), 0);
        k.note_reply_sig(Some(B), 1_000);
        assert_eq!(k.due(1_000), Some(Reason::Signature));
        assert_eq!(k.if_none_match(), Some(A), "the conditional GET offers what we hold");
        // The worker sent no ETag: the signature the REPLY advertised is adopted, not our own
        // computation — so a disagreement about how the set signs cannot loop.
        assert_eq!(k.applied_set(None, "locally-computed".into(), 1_100), B);
        assert_eq!(k.due(2_000), None);
        k.note_reply_sig(Some(B), 3_000);
        assert_eq!(k.due(3_000), None, "and the next reply agrees");
    }

    #[test]
    fn an_etag_outranks_the_advertised_signature_and_a_local_computation_is_the_last_resort() {
        let mut k = KeySync::boot(None, 0);
        k.note_reply_sig(Some(B), 0);
        assert_eq!(k.applied_set(Some(A.into()), "local".into(), 0), A, "the ETag wins");
        let mut k = KeySync::boot(None, 0);
        assert_eq!(k.applied_set(None, A.into(), 0), A, "nothing advertised: our own computation");
        let mut k = KeySync::boot(None, 0);
        assert_eq!(k.applied_set(Some("bogus".into()), A.into(), 0), A, "a malformed ETag is not adopted");
    }

    #[test]
    fn a_reply_with_no_signature_changes_nothing() {
        let mut k = KeySync::boot(None, 0);
        k.applied_set(Some(A.into()), "local".into(), 0);
        // A day's worth of 15-minute check-ins, one short of the daily refresh (which is tested on
        // its own): a worker that never signs costs this hub NOTHING between refreshes.
        for i in 1..96 {
            k.note_reply_sig(None, i * 15 * 60 * 1000);
            assert_eq!(k.due(i * 15 * 60 * 1000), None, "an older worker's silence is not a reason to poll");
        }
    }

    #[test]
    fn twenty_four_idle_hours_cost_one_request_and_the_daily_refresh_is_the_only_fallback() {
        // The fleet-cost test. A day of 15-minute check-ins whose replies carry NO signature (the
        // worst case — every worker until the hub half of `keysSig` ships).
        for signed in [false, true] {
            let mut k = KeySync::boot(None, 0);
            let mut fetches = 0;
            let mut t: i64 = 0;
            while t <= SAFETY_REFRESH_SECS * 1000 {
                if k.due(t).is_some() {
                    fetches += 1;
                    k.applied_set(Some(A.into()), "local".into(), t);
                }
                k.note_reply_sig(signed.then_some(A), t);
                t += 60 * 1000; // the loop's own tick, faster than the check-in
            }
            assert!(fetches <= 24, "a day must never cost more than an hourly poll would");
            assert_eq!(fetches, if signed { 1 } else { 2 }, "boot, plus the daily refresh only when nothing signs");
        }
    }

    #[test]
    fn the_lan_door_may_ring_early_but_is_rate_limited() {
        let mut k = KeySync::boot(None, 0);
        k.applied_set(Some(A.into()), "local".into(), 0);
        assert!(k.note_door_miss(1_000), "an unknown key asks once");
        assert_eq!(k.due(1_000), Some(Reason::DoorMiss));
        k.applied_set(Some(A.into()), "local".into(), 1_100);
        for i in 1..50 {
            assert!(!k.note_door_miss(1_100 + i * 1_000), "and a scan of the door does not ask again");
        }
        assert_eq!(k.due(2_000), None);
        assert!(k.note_door_miss(1_000 + DOOR_MISS_GAP_MS + 1), "past the gap, a real one may ask again");
        assert_eq!(k.due(1_000 + DOOR_MISS_GAP_MS + 1), Some(Reason::DoorMiss));
    }

    #[test]
    fn a_failure_backs_off_doubling_to_the_cap_and_keeps_wanting_what_it_wanted() {
        let mut k = KeySync::boot(None, 0);
        assert_eq!(k.due(0), Some(Reason::Boot));
        assert_eq!(k.failed(0), BACKOFF_START_SECS);
        assert_eq!(k.due(0), None, "not straight away");
        assert_eq!(k.due(BACKOFF_START_SECS * 1000), Some(Reason::Boot), "and still for the same reason");
        let waits: Vec<i64> = (0..8).map(|_| k.failed(0)).collect();
        assert_eq!(waits, vec![120, 240, 480, 960, 1_800, 1_800, 1_800, 1_800], "doubling, capped");
        assert_eq!(*waits.last().unwrap(), BACKOFF_MAX_SECS);
        // An hour of a dead worker costs a handful of attempts, not one per tick.
        let mut k = KeySync::boot(None, 0);
        let (mut attempts, mut t) = (0, 0i64);
        while t <= 3_600_000 {
            if k.due(t).is_some() {
                attempts += 1;
                k.failed(t);
            }
            t += TICK_SECS as i64 * 1000;
        }
        assert!(attempts <= 8, "an hour of failure made {attempts} attempts");
        // Success clears the backoff outright.
        k.applied_set(Some(A.into()), "local".into(), t);
        k.note_reply_sig(Some(B), t);
        assert_eq!(k.due(t), Some(Reason::Signature), "no residual backoff after a success");
    }

    #[test]
    fn a_pushed_set_confirms_freshness_so_the_reply_that_follows_agrees() {
        let keys = vec![mk("k1", "owner"), mk("k2", "control")];
        let mut k = KeySync::boot(None, 0);
        k.applied_set(Some(A.into()), "local".into(), 0);
        let sig = k.note_pushed(member_set_sig(&keys), 1_000);
        assert_eq!(sig, member_set_sig(&keys));
        assert_eq!(k.due(1_000), None);
        k.note_reply_sig(Some(&member_set_sig(&keys)), 2_000);
        assert_eq!(k.due(2_000), None, "the cloud's signature of the pushed set agrees with ours");
    }

    #[test]
    fn the_safety_refresh_is_daily_and_an_agreeing_signature_retires_it() {
        let mut k = KeySync::boot(None, 0);
        k.applied_set(Some(A.into()), "local".into(), 0);
        let day = SAFETY_REFRESH_SECS * 1000;
        assert_eq!(k.due(day - 1), None);
        assert_eq!(k.due(day), Some(Reason::Safety));
        k.note_reply_sig(Some(A), day);
        assert_eq!(k.due(day), None, "proof of freshness is as good as the refresh it would have spent");
        assert_eq!(k.due(day + day), Some(Reason::Safety), "and the clock restarts from that proof");
    }
}
