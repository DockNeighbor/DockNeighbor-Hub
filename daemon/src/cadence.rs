// THE HUB → CLOUD CADENCE — what is sent now, and what waits for the 15-minute keyframe.
//
// Owner ruling (Jonathan, 2026-09-15): "There should be 100–200 updates from the hub to the cloud a day
// — one every ~15 min when not underway, the GPS is not moving, and nothing is alarming." Spec: the hub
// cadence audit §2.1/§2.2 (thresholds proposed there and approved with the ruling) and §4 H1–H9.
//
// Principle: ONE consolidated keyframe per boat every 15 min while idle; an immediate single event on a
// real change; faster only while a watch is armed (geofence.rs), a member is watching (the lease) or
// the boat is underway. THE HUB decides what changed, always against the last value it SENT — so a slow
// drift eventually goes out and an oscillation across a line does not chatter.
//
// Everything in this file is PURE (no clock, no I/O). hub_server.rs is the shell that samples, asks
// these functions, and performs the sends.

use crate::geofence;

/// The consolidated keyframe cadence while idle.
pub const CHECKIN_SECS: u64 = 15 * 60;
/// The first keyframe after start waits this long, so the first router, valve and GPS reads are in it.
pub const FIRST_CHECKIN_SECS: u64 = 45;
/// After a failed keyframe, try again this soon (not the full 15 minutes).
pub const CHECKIN_RETRY_SECS: u64 = 120;
/// Two check-ins are never closer than this, however many wakes arrive (refresh, lease, events).
pub const MIN_CHECKIN_GAP_MS: i64 = 5_000;
/// `POST /api/hub/refresh` calls within this window coalesce into one wake (§A7.2).
pub const REFRESH_COALESCE_MS: i64 = 5_000;

/// The watch lease (G5): default TTL, and the bounds any single call is clamped to (gpsFeed.ts).
pub const LEASE_DEFAULT_SEC: f64 = 120.0;
pub const LEASE_MIN_SEC: f64 = 30.0;
pub const LEASE_MAX_SEC: f64 = 300.0;

// ── Lease and refresh ─────────────────────────────────────────────────────────────────────────────

/// PURE: the new `leaseUntil` after a lease call. The latest expiry wins (several viewers), a call can
/// never shorten a lease someone else holds, and `leaseSec` is clamped to 30..300 (default 120).
pub fn lease_extend(current_until_ms: i64, now_ms: i64, lease_sec: Option<f64>) -> i64 {
    let sec = lease_sec.filter(|s| s.is_finite()).unwrap_or(LEASE_DEFAULT_SEC).clamp(LEASE_MIN_SEC, LEASE_MAX_SEC);
    current_until_ms.max(now_ms + (sec * 1000.0) as i64)
}

pub fn lease_active(until_ms: i64, now_ms: i64) -> bool {
    until_ms > now_ms
}

/// PURE: should this refresh ring the check-in, or coalesce into the one already rung?
pub fn refresh_should_ring(last_ring_ms: i64, now_ms: i64) -> bool {
    last_ring_ms <= 0 || now_ms - last_ring_ms >= REFRESH_COALESCE_MS
}

// ── The anchor-watch heartbeat clock ──────────────────────────────────────────────────────────────

/// First retry after a failed heartbeat post; doubles, capped at HEARTBEAT_RETRY_MAX_SECS.
pub const HEARTBEAT_RETRY_SECS: u64 = 30;
pub const HEARTBEAT_RETRY_MAX_SECS: u64 = 60;

/// PURE: the heartbeat interval for an armed anchor watch — 5 min while every GPS source is inside the
/// circle, 60 s while a drag is in progress (owner ruling 2026-09-15).
pub fn anchor_heartbeat_secs(breaching: bool) -> u64 {
    if breaching {
        geofence::HEARTBEAT_SECS
    } else {
        geofence::HEARTBEAT_INSIDE_SECS
    }
}

/// When the next anchor-watch heartbeat is due.
///
/// ANY delivered post to the cloud (a heartbeat, an immediate event, a keyframe) is a check-in and
/// restarts the interval. A failed heartbeat is retried after 30 s, then every 60 s, until a post lands
/// — so one lost beat can never age the vessel past the cloud's 10-minute lost-device alarm. A new
/// watch signature is due at once, so the cloud sees this arm's first beat without waiting 5 minutes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HeartbeatClock {
    /// The signature the last delivered heartbeat carried.
    sent_sig: u64,
    failures: u32,
    failed_at_ms: i64,
    retry_at_ms: i64,
}

impl HeartbeatClock {
    /// `last_ok_ms` = the last delivered post of any kind.
    pub fn due(&self, sig: u64, last_ok_ms: i64, now_ms: i64, interval_secs: u64) -> bool {
        if sig != self.sent_sig {
            return true;
        }
        if self.failures > 0 && last_ok_ms <= self.failed_at_ms {
            return now_ms >= self.retry_at_ms;
        }
        now_ms - last_ok_ms >= interval_secs as i64 * 1000
    }
    pub fn delivered(&mut self, sig: u64) {
        self.sent_sig = sig;
        self.failures = 0;
    }
    pub fn failed(&mut self, now_ms: i64) {
        self.failures += 1;
        let backoff = (HEARTBEAT_RETRY_SECS << (self.failures - 1).min(4)).min(HEARTBEAT_RETRY_MAX_SECS);
        self.failed_at_ms = now_ms;
        self.retry_at_ms = now_ms + backoff as i64 * 1000;
    }
    /// Disarmed: the next arm starts fresh.
    pub fn reset(&mut self) {
        *self = HeartbeatClock::default();
    }
}

// ── Numeric change gates (§2.2) ───────────────────────────────────────────────────────────────────

/// A change-since-last-SENT gate with direction hysteresis: a move in the same direction as the last
/// sent move needs the threshold; a REVERSAL needs the threshold plus half again. That is the "half
/// the threshold on the way back" rule, and it is what stops a reading oscillating around a value
/// from sending every swing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeltaGate {
    last_sent: Option<f64>,
    /// Direction of the last sent move: -1, 0 (none yet), +1.
    dir: i8,
}

impl DeltaGate {
    pub fn last_sent(&self) -> Option<f64> {
        self.last_sent
    }
    /// Would `v` be a reportable change? Never true for the first value (the keyframe carries it).
    pub fn exceeds(&self, v: f64, threshold: f64) -> bool {
        let Some(last) = self.last_sent else { return false };
        let d = v - last;
        let dir: i8 = if d > 0.0 {
            1
        } else if d < 0.0 {
            -1
        } else {
            0
        };
        let need = if self.dir != 0 && dir != 0 && dir != self.dir { threshold * 1.5 } else { threshold };
        d.abs() + 1e-9 >= need
    }
    pub fn mark_sent(&mut self, v: f64) {
        if let Some(last) = self.last_sent {
            let d = v - last;
            if d != 0.0 {
                self.dir = if d > 0.0 { 1 } else { -1 };
            }
        }
        self.last_sent = Some(v);
    }
}

/// An alarm line with a hysteresis band: `beyond` flips ON the moment the value crosses the line on
/// its bad side, and flips OFF only once it is back by `band` (half the threshold). Returns whether the
/// state changed — a change is always a send.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Line {
    pub at: f64,
    /// true = below the line is the bad side (low battery, freeze); false = above (heat).
    pub below_is_bad: bool,
}

pub fn line_step(beyond: &mut Option<bool>, line: Line, v: f64, band: f64) -> bool {
    let now_beyond = match (*beyond, line.below_is_bad) {
        (Some(true), true) => v < line.at + band,
        (Some(true), false) => v > line.at - band,
        (_, true) => v < line.at,
        (_, false) => v > line.at,
    };
    let changed = beyond.map_or(now_beyond, |b| b != now_beyond);
    *beyond = Some(now_beyond);
    changed
}

/// Which §2.2 rule a reading falls under, from its event name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadingKind {
    /// Shelly Uni `voltmeter.*` — the house bank, param `v`.
    BatteryBank,
    /// PM Mini `pm1.voltage*` — shore power, param `v`: on/off only.
    ShoreAc,
    /// `temperature.*`, param `tC`.
    Temperature,
    /// `humidity.*`, param `rh`.
    Humidity,
    /// Any other periodic value (a device's own battery %, …): the keyframe only.
    Other,
}

/// PURE: classify a Shelly telemetry event and name the param the rule reads. The param names are the
/// app's webhook templates (shellyRpc.ts eventParams): `v`, `tC`, `rh`.
pub fn reading_kind(event: &str) -> (ReadingKind, &'static str) {
    let e = event.to_ascii_lowercase();
    if e.starts_with("voltmeter.") {
        (ReadingKind::BatteryBank, "v")
    } else if e.starts_with("pm1.voltage") {
        (ReadingKind::ShoreAc, "v")
    } else if e.starts_with("temperature.") {
        (ReadingKind::Temperature, "tC")
    } else if e.starts_with("humidity.") {
        (ReadingKind::Humidity, "rh")
    } else {
        (ReadingKind::Other, "")
    }
}

/// PURE: is this event periodic telemetry (the cloud's TELEMETRY_EVENT_RE, `[._](measurement|change)$`)?
/// Anything else — an alarm, its clear, `switch.on/off` — is always sent at once.
pub fn is_telemetry_event(event: &str) -> bool {
    let e = event.to_ascii_lowercase();
    ["measurement", "change"]
        .iter()
        .any(|suffix| e.len() > suffix.len() + 1 && e.ends_with(suffix) && matches!(e.as_bytes()[e.len() - suffix.len() - 1], b'.' | b'_'))
}

/// §2.2 numbers.
pub const BATTERY_DELTA_V_12: f64 = 0.20;
pub const BATTERY_LOW_LINES_12: [f64; 2] = [12.2, 11.8];
/// A bank reading above this is a 24 V bank: every battery number doubles.
pub const BATTERY_24V_ABOVE: f64 = 18.0;
pub const SHORE_LOST_BELOW_V: f64 = 90.0;
pub const SHORE_RESTORED_AT_V: f64 = 100.0;
pub const TEMP_DELTA_C: f64 = 1.0;
pub const TEMP_FREEZE_C: f64 = 2.0;
pub const TEMP_HEAT_C: f64 = 45.0;
pub const RH_DELTA: f64 = 5.0;

/// Per-(device, kind) state for a reading.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReadingGate {
    delta: DeltaGate,
    lines: [Option<bool>; 2],
    /// Shore power: Some(true) = present.
    shore: Option<bool>,
}

impl ReadingGate {
    /// PURE-ish: step the gate with an observed value; true ⇒ send it now. Line and shore state
    /// advance on every observation (they are the hysteresis); the delta compares with the last SENT.
    pub fn observe(&mut self, kind: ReadingKind, v: f64) -> bool {
        match kind {
            ReadingKind::BatteryBank => {
                let scale = if v > BATTERY_24V_ABOVE { 2.0 } else { 1.0 };
                let delta = BATTERY_DELTA_V_12 * scale;
                let mut crossed = false;
                for (i, at) in BATTERY_LOW_LINES_12.iter().enumerate() {
                    crossed |= line_step(&mut self.lines[i], Line { at: at * scale, below_is_bad: true }, v, delta / 2.0);
                }
                // The first observation only sets the baseline — unless the bank is already low.
                let first = self.delta.last_sent().is_none();
                (crossed && !(first && !self.lines.contains(&Some(true)))) || self.delta.exceeds(v, delta)
            }
            ReadingKind::ShoreAc => {
                let next = if v < SHORE_LOST_BELOW_V {
                    Some(false)
                } else if v >= SHORE_RESTORED_AT_V {
                    Some(true)
                } else {
                    self.shore // inside the band: no change
                };
                let changed = match (self.shore, next) {
                    (Some(a), Some(b)) => a != b,
                    // First reading: only a shore that is already LOST is worth an immediate send.
                    (None, Some(b)) => !b,
                    _ => false,
                };
                self.shore = next.or(self.shore);
                changed
            }
            ReadingKind::Temperature => {
                let band = TEMP_DELTA_C / 2.0;
                let first = self.delta.last_sent().is_none();
                let a = line_step(&mut self.lines[0], Line { at: TEMP_FREEZE_C, below_is_bad: true }, v, band);
                let b = line_step(&mut self.lines[1], Line { at: TEMP_HEAT_C, below_is_bad: false }, v, band);
                ((a || b) && !(first && !self.lines.contains(&Some(true)))) || self.delta.exceeds(v, TEMP_DELTA_C)
            }
            ReadingKind::Humidity => self.delta.exceeds(v, RH_DELTA),
            ReadingKind::Other => false,
        }
    }

    pub fn mark_sent(&mut self, v: f64) {
        self.delta.mark_sent(v);
    }
}

// ── Routers ───────────────────────────────────────────────────────────────────────────────────────

/// The router fields that are EVENTS: which uplink carries the boat (`wan`: lte/wired/starlink/…)
/// and whether it is up. RSSI/SINR/RSRP are never events — they ride the keyframe.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RouterSent {
    pub up: Option<String>,
    pub wan: Option<String>,
}

fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

impl RouterSent {
    pub fn from_params(params: &[(String, String)]) -> Self {
        RouterSent { up: param(params, "up").map(str::to_string), wan: param(params, "wan").map(str::to_string) }
    }
}

/// PURE: is this router poll an immediate event? Only once a baseline exists (the first read after
/// start rides the first keyframe), and only when the uplink kind or its up/down state moved.
pub fn router_is_event(last_sent: Option<&RouterSent>, params: &[(String, String)]) -> bool {
    let Some(last) = last_sent else { return false };
    let now = RouterSent::from_params(params);
    (now.up.is_some() && now.up != last.up) || (now.wan.is_some() && now.wan != last.wan)
}

// ── LinkTap valves ────────────────────────────────────────────────────────────────────────────────

/// What the cloud last received for a valve, for transition detection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ValveSent {
    pub watering: Option<String>,
    pub rf: Option<String>,
    pub faults: [Option<String>; 4],
}

const VALVE_FAULTS: [&str; 4] = ["broken", "leak", "clog", "cutoff"];

impl ValveSent {
    pub fn from_params(params: &[(String, String)]) -> Self {
        let mut faults: [Option<String>; 4] = Default::default();
        for (i, f) in VALVE_FAULTS.iter().enumerate() {
            faults[i] = param(params, f).map(str::to_string);
        }
        ValveSent { watering: param(params, "watering").map(str::to_string), rf: param(params, "rf").map(str::to_string), faults }
    }
}

/// PURE: did a valve make a transition the app must see at once — watering on/off, its RF link
/// lost/back, or a fault flag (broken/leak/clog/cutoff) changing? Never before a baseline exists.
pub fn valve_transition(last_sent: Option<&ValveSent>, params: &[(String, String)]) -> bool {
    let Some(last) = last_sent else { return false };
    let now = ValveSent::from_params(params);
    let moved = |a: &Option<String>, b: &Option<String>| a.is_some() && a != b;
    moved(&now.watering, &last.watering) || moved(&now.rf, &last.rf) || now.faults.iter().zip(last.faults.iter()).any(|(a, b)| moved(a, b))
}

/// PURE: should this `linktap.measurement` go out now? While WATERING, every poll (flow and volume
/// progress is what the app shows); otherwise only on a `valve_transition`. An idle, healthy valve
/// rides the keyframe.
pub fn valve_measurement_due(last_sent: Option<&ValveSent>, params: &[(String, String)]) -> bool {
    ValveSent::from_params(params).watering.as_deref() == Some("1") || valve_transition(last_sent, params)
}

// ── GPS in the keyframe ───────────────────────────────────────────────────────────────────────────

/// PURE: does the keyframe carry this GPS device's newest fix? Yes as the keep-alive, unless an ANCHOR
/// WATCH is armed and nobody is looking (§A7.3: the anchor watch sends no position except on a drag and
/// on disarm — its 60 s heartbeat proves liveness), and never a fix older than two check-ins (it would
/// read as fresh). A security zone alone is the idle cadence (owner ruling 2026-09-15), so it keeps the
/// keep-alive fix.
pub fn keyframe_carries_fix(g: &geofence::Geofence, leased: bool, fix_age_ms: i64) -> bool {
    if fix_age_ms > 2 * CHECKIN_SECS as i64 * 1000 {
        return false;
    }
    leased || !g.anchor_armed() || g.underway()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geofence::{Geofence, Sample};

    fn p(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn an_armed_hour_inside_the_circle_checks_in_about_twelve_times() {
        // Owner ruling 2026-09-15: heartbeat every 5 min while inside. Every delivered post counts, so
        // the 4 keyframes in the hour replace the heartbeats they coincide with.
        let sig = 1757750400000;
        let run = |with_keyframes: bool| {
            let mut c = HeartbeatClock::default();
            let (mut last_ok, mut beats, mut keyframes) = (-1_000_000_000i64, 0, 0);
            for t in 0..3600i64 {
                let now = t * 1000;
                if with_keyframes && t % CHECKIN_SECS as i64 == 0 {
                    keyframes += 1;
                    last_ok = now;
                }
                if c.due(sig, last_ok, now, anchor_heartbeat_secs(false)) {
                    beats += 1;
                    c.delivered(sig);
                    last_ok = now;
                }
            }
            (beats, keyframes)
        };
        let (beats, _) = run(false);
        assert_eq!(beats, 12, "one heartbeat every 5 minutes, the first at once for the new arm");
        let (beats, keyframes) = run(true);
        assert_eq!(keyframes, 4);
        assert!((8..=9).contains(&beats), "keyframes count as check-ins, got {beats}");
        assert!((12..=13).contains(&(beats + keyframes)), "about 12 check-ins an armed hour");
        assert_eq!(anchor_heartbeat_secs(true), 60, "a drag in progress beats every 60 s");
    }

    #[test]
    fn a_failed_heartbeat_retries_within_sixty_seconds_until_a_post_lands() {
        let sig = 9;
        let mut c = HeartbeatClock::default();
        assert!(c.due(sig, 0, 0, 300), "a new arm is due at once");
        c.delivered(sig);
        assert!(!c.due(sig, 0, 299_000, 300));
        assert!(c.due(sig, 0, 300_000, 300));
        c.failed(300_000);
        assert!(!c.due(sig, 0, 329_000, 300));
        assert!(c.due(sig, 0, 330_000, 300), "first retry after 30 s");
        c.failed(330_000);
        assert!(!c.due(sig, 0, 389_000, 300));
        assert!(c.due(sig, 0, 390_000, 300), "then every 60 s, never longer");
        c.failed(390_000);
        assert!(c.due(sig, 0, 450_000, 300), "capped at 60 s");
        // An immediate event landing counts as the check-in: the retry stops and the 5 min restarts.
        assert!(!c.due(sig, 400_000, 410_000, 300));
        assert!(c.due(sig, 400_000, 700_000, 300));
        c.delivered(sig);
        // Re-arm: due at once; disarm resets.
        assert!(c.due(sig + 1, 700_000, 700_001, 300));
        c.reset();
        assert!(c.due(sig, 700_000, 700_001, 300));
    }

    /// Live-only fields (batch::MODEM_LIVE_ONLY, LINKTAP_LIVE_ONLY) never drive change detection:
    /// they are not sent without a lease, so an "event" on one would send a reading with the very
    /// field that moved stripped off. Moving every one of them is not an event; `up`/`wan` still are.
    #[test]
    fn live_only_fields_never_make_an_event() {
        let base = crate::batch::fixtures::dish();
        let last = RouterSent::from_params(&base);
        let bump = |p: &[(String, String)], live: &[&str]| -> Vec<(String, String)> {
            p.iter().map(|(k, v)| (k.clone(), if live.contains(&k.as_str()) { format!("{v}9") } else { v.clone() })).collect()
        };
        for p in [base.clone(), crate::batch::fixtures::lte()] {
            let last = RouterSent::from_params(&p);
            assert!(!router_is_event(Some(&last), &bump(&p, crate::batch::MODEM_LIVE_ONLY)), "{p:?}");
        }
        let flipped: Vec<(String, String)> = base.iter().map(|(k, v)| (k.clone(), if k == "up" { if v == "1" { "0".into() } else { "1".into() } } else { v.clone() })).collect();
        assert!(router_is_event(Some(&last), &flipped), "up/down is still an event");
        let rewired: Vec<(String, String)> = base.iter().map(|(k, v)| (k.clone(), if k == "wan" { format!("{v}-other") } else { v.clone() })).collect();
        assert!(router_is_event(Some(&last), &rewired), "a WAN change is still an event");

        let valve: Vec<(String, String)> = [("watering", "0"), ("rf", "1"), ("signal", "69"), ("battery", "93")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let vlast = ValveSent::from_params(&valve);
        assert!(!valve_transition(Some(&vlast), &bump(&valve, crate::batch::LINKTAP_LIVE_ONLY)));
        assert!(!valve_measurement_due(Some(&vlast), &bump(&valve, crate::batch::LINKTAP_LIVE_ONLY)));
    }

    #[test]
    fn a_lease_takes_the_latest_expiry_and_clamps_the_ttl() {
        let now = 1_000_000;
        assert_eq!(lease_extend(0, now, None), now + 120_000, "default 120 s");
        assert_eq!(lease_extend(0, now, Some(5.0)), now + 30_000, "floor 30 s");
        assert_eq!(lease_extend(0, now, Some(9_999.0)), now + 300_000, "cap 300 s");
        assert_eq!(lease_extend(now + 250_000, now, Some(60.0)), now + 250_000, "a short renewal never cuts another viewer's lease");
        assert_eq!(lease_extend(0, now, Some(f64::NAN)), now + 120_000);
    }

    #[test]
    fn a_lease_expires_at_its_ttl() {
        let until = lease_extend(0, 0, Some(120.0));
        assert!(lease_active(until, 119_999));
        assert!(!lease_active(until, 120_000), "expired exactly at leaseUntil");
        assert!(!lease_active(0, 1));
    }

    #[test]
    fn refresh_calls_within_five_seconds_coalesce_into_one_wake() {
        assert!(refresh_should_ring(0, 10_000), "the first call rings");
        assert!(!refresh_should_ring(10_000, 10_001));
        assert!(!refresh_should_ring(10_000, 14_999));
        assert!(refresh_should_ring(10_000, 15_000));
    }

    #[test]
    fn telemetry_classification_matches_the_clouds_regex() {
        for e in ["temperature.change", "temperature.measurement", "pm1.voltage_change", "voltmeter.measurement", "linktap.cycle.change"] {
            assert!(is_telemetry_event(e), "{e}");
        }
        for e in ["flood.alarm", "flood.alarm_off", "switch.on", "switch.off", "sensor alert", "change", ".change"] {
            assert!(!is_telemetry_event(e), "{e}");
        }
    }

    #[test]
    fn battery_sends_on_a_fifth_of_a_volt_and_on_the_low_lines_with_hysteresis() {
        let mut g = ReadingGate::default();
        assert!(!g.observe(ReadingKind::BatteryBank, 12.70), "first value is the baseline");
        g.mark_sent(12.70);
        assert!(!g.observe(ReadingKind::BatteryBank, 12.55), "0.15 V is noise");
        assert!(g.observe(ReadingKind::BatteryBank, 12.50), "0.20 V down from the last SENT value");
        g.mark_sent(12.50);
        // A reversal needs 0.30 V (threshold + half).
        assert!(!g.observe(ReadingKind::BatteryBank, 12.75));
        assert!(g.observe(ReadingKind::BatteryBank, 12.80));
        g.mark_sent(12.80);
        // Crossing 12.2 V is a send even though it is only 0.1 V below the last sent value.
        let mut g = ReadingGate::default();
        g.observe(ReadingKind::BatteryBank, 12.25);
        g.mark_sent(12.25);
        assert!(g.observe(ReadingKind::BatteryBank, 12.18), "crossed the 12.2 V line");
        g.mark_sent(12.18);
        assert!(!g.observe(ReadingKind::BatteryBank, 12.24), "back above by less than half the threshold: still low");
        assert!(g.observe(ReadingKind::BatteryBank, 12.31), "restored past 12.2 + 0.1");
    }

    #[test]
    fn a_24_volt_bank_doubles_every_battery_number() {
        let mut g = ReadingGate::default();
        g.observe(ReadingKind::BatteryBank, 25.4);
        g.mark_sent(25.4);
        assert!(!g.observe(ReadingKind::BatteryBank, 25.1), "0.3 V on 24 V is noise");
        assert!(g.observe(ReadingKind::BatteryBank, 25.0), "0.4 V on 24 V sends");
        let mut low = ReadingGate::default();
        low.observe(ReadingKind::BatteryBank, 24.5);
        low.mark_sent(24.5);
        assert!(low.observe(ReadingKind::BatteryBank, 24.35), "24.4 V is the 24 V low line");
    }

    #[test]
    fn shore_power_is_on_off_only_with_a_ten_volt_band() {
        let mut g = ReadingGate::default();
        assert!(!g.observe(ReadingKind::ShoreAc, 121.0), "first reading, present: baseline");
        assert!(!g.observe(ReadingKind::ShoreAc, 108.0), "sag inside normal range is never an event");
        assert!(g.observe(ReadingKind::ShoreAc, 40.0), "lost below 90 V");
        assert!(!g.observe(ReadingKind::ShoreAc, 95.0), "95 V is inside the band: still lost");
        assert!(!g.observe(ReadingKind::ShoreAc, 0.0));
        assert!(g.observe(ReadingKind::ShoreAc, 100.0), "restored at 100 V");
        assert!(!g.observe(ReadingKind::ShoreAc, 92.0), "92 V is inside the band: still present");
        assert!(ReadingGate::default().observe(ReadingKind::ShoreAc, 0.0), "a first reading that is already lost sends");
    }

    #[test]
    fn temperature_sends_on_a_degree_and_on_the_freeze_and_heat_lines() {
        let mut g = ReadingGate::default();
        assert!(!g.observe(ReadingKind::Temperature, 20.0));
        g.mark_sent(20.0);
        for v in [20.4, 19.7, 20.6, 19.5, 20.9] {
            assert!(!g.observe(ReadingKind::Temperature, v), "{v} is inside a degree of 20.0");
        }
        assert!(g.observe(ReadingKind::Temperature, 21.0));
        g.mark_sent(21.0);
        assert!(!g.observe(ReadingKind::Temperature, 20.0), "an oscillation back needs 1.5 degrees");
        assert!(g.observe(ReadingKind::Temperature, 19.5));
        // Freeze line at 2 °C: crossing it sends even with a small step.
        let mut f = ReadingGate::default();
        f.observe(ReadingKind::Temperature, 2.4);
        f.mark_sent(2.4);
        assert!(f.observe(ReadingKind::Temperature, 1.9));
        f.mark_sent(1.9);
        assert!(!f.observe(ReadingKind::Temperature, 2.3), "not restored until 2.5");
        assert!(f.observe(ReadingKind::Temperature, 2.6));
        let mut h = ReadingGate::default();
        h.observe(ReadingKind::Temperature, 44.8);
        h.mark_sent(44.8);
        assert!(h.observe(ReadingKind::Temperature, 45.2), "heat line at 45");
    }

    #[test]
    fn humidity_sends_on_five_percent_and_other_readings_wait_for_the_keyframe() {
        let mut g = ReadingGate::default();
        g.observe(ReadingKind::Humidity, 60.0);
        g.mark_sent(60.0);
        assert!(!g.observe(ReadingKind::Humidity, 64.0));
        assert!(g.observe(ReadingKind::Humidity, 65.0));
        let mut o = ReadingGate::default();
        assert!(!o.observe(ReadingKind::Other, 50.0));
        o.mark_sent(50.0);
        assert!(!o.observe(ReadingKind::Other, 5.0), "a device battery % is never an event");
        assert_eq!(reading_kind("pm1.voltage_change"), (ReadingKind::ShoreAc, "v"));
        assert_eq!(reading_kind("voltmeter.measurement"), (ReadingKind::BatteryBank, "v"));
        assert_eq!(reading_kind("devicepower.battery_change"), (ReadingKind::Other, ""));
    }

    #[test]
    fn rssi_and_sinr_are_never_router_events_but_the_uplink_is() {
        let base = p(&[("up", "1"), ("wan", "lte"), ("rssi", "-71"), ("sinr", "12")]);
        assert!(!router_is_event(None, &base), "the first poll is the keyframe's baseline");
        let sent = RouterSent::from_params(&base);
        assert!(!router_is_event(Some(&sent), &p(&[("up", "1"), ("wan", "lte"), ("rssi", "-95"), ("sinr", "-3")])));
        assert!(router_is_event(Some(&sent), &p(&[("up", "0"), ("wan", "lte")])), "WAN down");
        assert!(router_is_event(Some(&sent), &p(&[("up", "1"), ("wan", "starlink")])), "uplink source changed");
    }

    #[test]
    fn a_valve_reports_every_poll_while_watering_and_only_transitions_when_idle() {
        let idle = p(&[("watering", "0"), ("vol_l", "0.00"), ("battery", "92"), ("rf", "1"), ("leak", "0")]);
        assert!(!valve_measurement_due(None, &idle), "boot baseline rides the keyframe");
        let sent = ValveSent::from_params(&idle);
        assert!(
            !valve_measurement_due(Some(&sent), &p(&[("watering", "0"), ("vol_l", "0.00"), ("battery", "91"), ("rf", "1"), ("leak", "0")])),
            "battery drift is not a transition"
        );
        let on = p(&[("watering", "1"), ("flow_lpm", "7.50"), ("rf", "1")]);
        assert!(valve_measurement_due(Some(&sent), &on));
        assert!(valve_measurement_due(Some(&ValveSent::from_params(&on)), &on), "every poll while watering");
        assert!(valve_measurement_due(Some(&ValveSent::from_params(&on)), &idle), "watering -> idle is a transition");
        assert!(valve_measurement_due(Some(&sent), &p(&[("watering", "0"), ("rf", "0"), ("leak", "0")])), "RF link lost");
        assert!(valve_measurement_due(Some(&sent), &p(&[("watering", "0"), ("rf", "1"), ("leak", "1")])), "a leak flag");
    }

    /// Deterministic noise for the cadence simulation.
    struct Lcg(u64);
    impl Lcg {
        fn unit(&mut self) -> f64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        }
    }

    /// 🔴 THE OWNER'S NUMBER, PROVEN. A docked, unarmed, unwatched boat with the CENTRAL profile — hub
    /// GPS sampled every 60 s with modem-grade wander, two routers polled every 120 s with noisy
    /// signal and a moving data counter, an idle valve polled every 60 s, and two Shellys reporting
    /// through the hub every 5 minutes with ordinary drift — for 24 hours. Every sample goes through
    /// the same gates the shell uses; what leaves the hub is the keyframe schedule plus the immediate
    /// sends the gates allow. The ruling is 100–200 a day; idle must sit at about 100.
    #[test]
    fn an_idle_docked_boat_sends_about_one_hundred_times_a_day() {
        let day_s: i64 = 24 * 3600;
        let mut rng = Lcg(42);
        let mut sends = 0u32;
        let mut keyframes = 0u32;

        let mut gps = Geofence::default();
        let (lat0, lon0) = (41.492907, -81.694361);
        let mut routers: Vec<Option<RouterSent>> = vec![None, None];
        let mut valve: Option<ValveSent> = None;
        let mut temp = ReadingGate::default();
        let mut bank = ReadingGate::default();
        let mut next_checkin = FIRST_CHECKIN_SECS as i64;

        for t in 0..day_s {
            let now_ms = t * 1000;
            if t % 60 == 0 {
                // 5–12 m of wander, sats/hdop healthy, SOG noise under 0.3 kn.
                let s = Sample {
                    lat: lat0 + rng.unit() * 8.0 / 111_195.0,
                    lon: lon0 + rng.unit() * 8.0 / 83_000.0,
                    acc: Some(6.0),
                    hdop: Some(1.1),
                    sats: Some(9),
                    sog_kn: Some(rng.unit().abs() * 0.3),
                    fix_age_s: 0.0,
                };
                let d = gps.observe(None, &s, now_ms, false);
                assert!(d.events.is_empty() && d.underway_changed.is_none(), "wander must never look like a trip");
                if d.send_position {
                    sends += 1;
                    gps.mark_sent(s.lat, s.lon);
                }
                let v = p(&[("watering", "0"), ("battery", "92"), ("rf", "1"), ("leak", "0")]);
                if valve_measurement_due(valve.as_ref(), &v) {
                    sends += 1;
                    valve = Some(ValveSent::from_params(&v));
                }
            }
            if t % 120 == 0 {
                for r in routers.iter_mut() {
                    let rssi = format!("{}", -70 - (rng.unit().abs() * 20.0) as i64);
                    let params = p(&[("up", "1"), ("wan", "lte"), ("rssi", &rssi)]);
                    if router_is_event(r.as_ref(), &params) {
                        sends += 1;
                        *r = Some(RouterSent::from_params(&params));
                    }
                }
            }
            if t % 300 == 0 {
                let tc = 21.0 + rng.unit() * 0.6 + (t as f64 / day_s as f64 * std::f64::consts::TAU).sin() * 0.4;
                if temp.observe(ReadingKind::Temperature, tc) {
                    sends += 1;
                    temp.mark_sent(tc);
                }
                let v = 12.9 - t as f64 / day_s as f64 * 0.15 + rng.unit() * 0.05;
                if bank.observe(ReadingKind::BatteryBank, v) {
                    sends += 1;
                    bank.mark_sent(v);
                }
                // The keyframe marks these sent, like the shell does.
                if t + 300 > next_checkin {
                    temp.mark_sent(tc);
                    bank.mark_sent(v);
                }
            }
            if t == next_checkin {
                sends += 1;
                keyframes += 1;
                next_checkin += CHECKIN_SECS as i64;
                for r in routers.iter_mut() {
                    if r.is_none() {
                        *r = Some(RouterSent::from_params(&p(&[("up", "1"), ("wan", "lte")])));
                    }
                }
                if valve.is_none() {
                    valve = Some(ValveSent::from_params(&p(&[("watering", "0"), ("battery", "92"), ("rf", "1"), ("leak", "0")])));
                }
                if let Some(o) = gps.last() {
                    let (la, lo) = (o.sample.lat, o.sample.lon);
                    if keyframe_carries_fix(&gps, false, now_ms - o.at_ms) {
                        gps.mark_sent(la, lo);
                    }
                }
            }
        }
        eprintln!("idle docked boat: {sends} sends in 24 h ({keyframes} keyframes)");
        assert_eq!(keyframes, 96, "one keyframe every 15 minutes");
        assert!(sends <= 105, "idle traffic must be about 100/day (ruling 100–200), got {sends}");
        assert!(sends >= 96);
    }

    #[test]
    fn an_anchored_boat_keyframe_carries_no_position_unless_someone_watches() {
        let mut g = Geofence::default();
        let w = crate::geofence::Watch {
            sig: 5,
            anchor: Some(crate::geofence::Circle { lat: 41.0, lon: -81.0, radius_m: 60.0, warn_m: 0.0 }),
            zone: None,
            hb_secs: 60,
            sample_secs: 30,
        };
        g.sync_watch(Some(&w));
        assert!(!keyframe_carries_fix(&g, false, 1_000));
        assert!(keyframe_carries_fix(&g, true, 1_000));
        assert!(keyframe_carries_fix(&Geofence::default(), false, 1_000));
        assert!(!keyframe_carries_fix(&Geofence::default(), false, 31 * 60_000), "a stale fix never rides as fresh");
        let mut z = Geofence::default();
        z.sync_watch(Some(&crate::geofence::Watch {
            sig: 6,
            anchor: None,
            zone: Some(crate::geofence::Zone { lat: 41.0, lon: -81.0, radius_m: 40.0, streak: 3 }),
            hb_secs: 60,
            sample_secs: 30,
        }));
        assert!(keyframe_carries_fix(&z, false, 1_000), "a zone alone keeps the keep-alive fix");
    }
}
