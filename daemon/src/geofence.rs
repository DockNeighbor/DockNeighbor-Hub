// THE GEOFENCE — the daemon port of hub-lite's GPS gate and drag detection (brvg-hub-lite.sh
// `gps_should_send` / `check_anchor` / `anchor_ring`), extended with the security zone, underway
// detection and the fix-quality gate the telemetry design adds.
//
// Design of record: sc4-internal docs/TELEMETRY-ARCHITECTURE-2026-09-13.md §A7.2/§A7.3 (APPROVED
// 2026-09-13) and the hub cadence audit (2026-09-15, owner ruling: "100–200 updates from the hub to
// the cloud a day — one every ~15 min when not underway, GPS not moving and nothing alarming").
//
// hub-lite stays the REFERENCE implementation for the anchor rule: two consecutive samples outside
// the radius by more than each sample's own accuracy, a separate warn ring that only counts while the
// alarm ring holds, one event per episode, recovery inside clears the latch. Everything here is PURE —
// no clock, no I/O — so the streak, underway and deadband rules are unit-tested rather than trusted.
//
// What decides a SEND (the cloud position), per GPS device:
//   * leased (a member is watching)      → every sample (the quality gate flags, never suppresses)
//   * a final fix owed after a disarm    → that one sample
//   * underway                           → one position every UNDERWAY_SEND_SECS (5 min — owner ruling
//                                          2026-09-15), plus one on entry and one on stopping; still
//                                          SAMPLED at 30 s so exit detection keeps its resolution
//   * ANCHOR WATCH armed                 → only while a drag is confirmed (every 30 s tick outside);
//                                          liveness is the 60 s `gps.heartbeat`
//   * unarmed, or only a SECURITY ZONE   → moved ≥ 50 m (floor 25 m) AND > 2 × acc from the last SENT
//                                          position; the keep-alive rides the 15-min keyframe
//
// 🔴 A SECURITY ZONE IS NOT AN ANCHOR WATCH (owner ruling, Jonathan 2026-09-15: "Security zone is 15 min
// checkin, not faster like the anchorwatch"). Only an armed ANCHOR WATCH buys the 30 s sampling and the
// 60 s heartbeat. With just a zone armed the hub stays on the idle cadence; the zone breach itself is
// still detected here (streak 3 at the idle sample rate) and sent the moment it confirms.
//
// The local ALARM never waits on a send: `anchor.motion`, `anchor.warn.motion` and `zone.motion` are
// emitted the moment the streak confirms, whatever the network is doing.

use serde_json::Value;

/// Unarmed deadband from the last SENT position (§A7.2, projects-08 measurement on MVP).
pub const UNARMED_DEADBAND_M: f64 = 50.0;
/// The deadband can never be configured below this — 10–25 m chatters on modem GNSS wander alone.
pub const DEADBAND_FLOOR_M: f64 = 25.0;
/// Sample cadence while an ANCHOR WATCH is armed or the boat is underway (never for a zone alone).
pub const ARMED_SAMPLE_SECS: u64 = 30;
/// The anchor watch's "checks in OK" heartbeat (G1) — anchor only, never for a zone alone. The
/// cloud's lost-device alarm for an anchor watch fires at 10 minutes, so this has ample margin.
pub const HEARTBEAT_SECS: u64 = 60;
/// Consecutive outside samples that confirm an anchor drag (hub-lite's rule, ported unchanged).
pub const ANCHOR_STREAK: u32 = 2;
/// A zone's streak when the reply does not say (`zoneStreak`).
pub const DEFAULT_ZONE_STREAK: u32 = 3;
/// Underway entry: SOG at or above this on consecutive reliable samples…
pub const UNDERWAY_ENTER_SOG_KN: f64 = 1.5;
/// …or at least this far from the last SENT position on consecutive reliable samples.
pub const UNDERWAY_ENTER_MOVE_M: f64 = 50.0;
pub const UNDERWAY_ENTER_STREAK: u32 = 2;
/// Underway exit: this long below the exit speed and inside the net-movement circle.
pub const UNDERWAY_EXIT_SOG_KN: f64 = 0.5;
pub const UNDERWAY_EXIT_NET_M: f64 = 25.0;
pub const UNDERWAY_EXIT_MS: i64 = 5 * 60_000;
/// Underway: how often the voyage track sends a `gps.measurement` (owner ruling, Jonathan 2026-09-15:
/// "Underway: GPS goes every 5 minutes."). Entry/exit detection and the 30 s sampling are unchanged;
/// a lease (someone has the app open) still sends every sample.
pub const UNDERWAY_SEND_SECS: u64 = 300;
/// The quality gate (cloud gpsFeed.ts UNRELIABLE_* — same numbers on both sides).
pub const UNRELIABLE_HDOP: f64 = 5.0;
pub const UNRELIABLE_MIN_SATS: u32 = 4;

/// PURE: great-circle distance in metres (haversine — the same formula as hub-lite's awk).
pub fn distance_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let r = std::f64::consts::PI / 180.0;
    let dla = (lat2 - lat1) * r;
    let dlo = (lon2 - lon1) * r;
    let sa = (dla / 2.0).sin();
    let sb = (dlo / 2.0).sin();
    let a = (sa * sa + (lat1 * r).cos() * (lat2 * r).cos() * sb * sb).min(1.0);
    2.0 * 6_371_000.0 * a.sqrt().atan2((1.0 - a).sqrt())
}

/// PURE: a configured deadband, never below the floor.
pub fn deadband_m(configured: f64) -> f64 {
    if configured.is_finite() {
        configured.max(DEADBAND_FLOOR_M)
    } else {
        UNARMED_DEADBAND_M
    }
}

// ── The flat v2 `anchor` reply ─────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct Circle {
    pub lat: f64,
    pub lon: f64,
    pub radius_m: f64,
    /// 0 = no warn ring.
    pub warn_m: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Zone {
    pub lat: f64,
    pub lon: f64,
    pub radius_m: f64,
    pub streak: u32,
}

/// The watch the vehicle has armed, as the worker states it. `sig` is what the hub echoes as
/// `anchorsig`; a changed sig is a NEW EPISODE by construction.
#[derive(Clone, Debug, PartialEq)]
pub struct Watch {
    pub sig: u64,
    pub anchor: Option<Circle>,
    pub zone: Option<Zone>,
    pub hb_secs: u64,
    pub sample_secs: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AnchorReply {
    /// `{"sig":0}` — stand down.
    Disarm,
    Arm(Watch),
}

fn num(o: &Value, k: &str) -> Option<f64> {
    match o.get(k)? {
        Value::Number(n) => n.as_f64().filter(|v| v.is_finite()),
        // The contract says numbers, but a tolerant reader costs nothing and a stringly worker
        // must not silently unarm a boat.
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|v| v.is_finite()),
        _ => None,
    }
}

/// PURE: read `body.anchor` off a worker reply (`/api/agent` or `/api/agent/batch`). `None` when the
/// reply carries no anchor object (the common case: the hub already runs the vehicle's signature) or
/// when the object is malformed — a garbled config must never be read as a disarm.
pub fn parse_anchor_reply(body: &Value) -> Option<AnchorReply> {
    parse_anchor_object(body.get("anchor")?)
}

/// PURE: one flat anchor object (fixtures/anchor-reply.v2.json). Reads the v1 base keys
/// (`sig lat lon radiusM warnM`) and the v2 extras (`zoneCy zoneCx zoneR zoneStreak hbSec sampleSec`).
pub fn parse_anchor_object(o: &Value) -> Option<AnchorReply> {
    if !o.is_object() {
        return None;
    }
    let sig = num(o, "sig")?;
    if sig < 0.0 {
        return None;
    }
    let sig = sig as u64;
    if sig == 0 {
        return Some(AnchorReply::Disarm);
    }
    let anchor = match (num(o, "lat"), num(o, "lon"), num(o, "radiusM")) {
        (Some(lat), Some(lon), Some(r)) if r > 0.0 => {
            Some(Circle { lat, lon, radius_m: r, warn_m: num(o, "warnM").unwrap_or(0.0).max(0.0) })
        }
        _ => None,
    };
    let zone = match (num(o, "zoneCy"), num(o, "zoneCx"), num(o, "zoneR")) {
        (Some(lat), Some(lon), Some(r)) if r > 0.0 => {
            Some(Zone { lat, lon, radius_m: r, streak: num(o, "zoneStreak").map(|s| s.max(1.0) as u32).unwrap_or(DEFAULT_ZONE_STREAK) })
        }
        _ => None,
    };
    if anchor.is_none() && zone.is_none() {
        return None;
    }
    Some(AnchorReply::Arm(Watch {
        sig,
        anchor,
        zone,
        hb_secs: num(o, "hbSec").map(|s| s.max(10.0) as u64).unwrap_or(HEARTBEAT_SECS),
        sample_secs: num(o, "sampleSec").map(|s| s.max(5.0) as u64).unwrap_or(ARMED_SAMPLE_SECS),
    }))
}

// ── Samples and the quality gate ───────────────────────────────────────────────────────────────────

/// One GPS sample as the geofence sees it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    pub lat: f64,
    pub lon: f64,
    pub acc: Option<f64>,
    pub hdop: Option<f64>,
    pub sats: Option<u32>,
    pub sog_kn: Option<f64>,
    /// Seconds since the source produced this fix (0 for a fix read this instant).
    pub fix_age_s: f64,
}

/// PURE: "fix unreliable" — hdop > 5, sats < 4, or a fix older than 3 × the sample interval. A THIRD
/// state: it never advances (or starts, or ends) anything; it is reported in the heartbeat.
pub fn unreliable(s: &Sample, sample_secs: u64) -> bool {
    s.hdop.is_some_and(|h| h > UNRELIABLE_HDOP) || s.sats.is_some_and(|n| n < UNRELIABLE_MIN_SATS) || s.fix_age_s > 3.0 * sample_secs as f64
}

/// What the last observed sample looked like — the heartbeat and the LAN live feed read it.
#[derive(Clone, Debug, PartialEq)]
pub struct Observed {
    pub sample: Sample,
    pub at_ms: i64,
    pub inside: Option<bool>,
    pub dist_from_center_m: Option<f64>,
    pub unreliable: bool,
}

/// One observation's outcome.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Decision {
    /// Send this sample to the cloud as `gps.measurement` now.
    pub send_position: bool,
    /// Local alarms to send immediately: (event, params).
    pub events: Vec<(String, Vec<(String, String)>)>,
    /// `Some(true)` on entering underway, `Some(false)` on leaving it.
    pub underway_changed: Option<bool>,
}

/// The per-GPS-device state machine.
#[derive(Clone, Debug, Default)]
pub struct Geofence {
    sig: u64,
    /// An anchor watch (not merely a zone) is part of the armed signature.
    anchor_armed: bool,
    last_sent: Option<(f64, f64)>,
    anchor_streak: u32,
    warn_streak: u32,
    zone_streak: u32,
    anchor_alerted: bool,
    warn_alerted: bool,
    zone_alerted: bool,
    final_fix_owed: bool,
    underway: bool,
    enter_streak: u32,
    slow_since: Option<(i64, f64, f64)>,
    /// When the underway track last sent a position.
    underway_sent_ms: Option<i64>,
    last: Option<Observed>,
}

impl Geofence {
    /// The signature this device is running ("0" when unarmed) — echoed as `anchorsig`.
    pub fn sig(&self) -> u64 {
        self.sig
    }
    /// Anything armed (anchor watch and/or security zone).
    pub fn armed(&self) -> bool {
        self.sig != 0
    }
    /// An ANCHOR WATCH is armed — the only state that buys 30 s sampling, the 60 s heartbeat and
    /// breach-only position sends (owner ruling 2026-09-15).
    pub fn anchor_armed(&self) -> bool {
        self.anchor_armed
    }
    pub fn underway(&self) -> bool {
        self.underway
    }
    pub fn last(&self) -> Option<&Observed> {
        self.last.as_ref()
    }
    pub fn last_sent(&self) -> Option<(f64, f64)> {
        self.last_sent
    }

    /// Adopt the vehicle's watch. A changed signature resets every streak and latch (hub-lite
    /// `apply_anchor`); going from armed to disarmed owes the cloud one final fix.
    pub fn sync_watch(&mut self, watch: Option<&Watch>) {
        let new_sig = watch.map_or(0, |w| w.sig);
        self.anchor_armed = watch.is_some_and(|w| w.anchor.is_some());
        if new_sig == self.sig {
            return;
        }
        if self.sig != 0 && new_sig == 0 {
            self.final_fix_owed = true;
        }
        self.sig = new_sig;
        self.anchor_streak = 0;
        self.warn_streak = 0;
        self.zone_streak = 0;
        self.anchor_alerted = false;
        self.warn_alerted = false;
        self.zone_alerted = false;
    }

    /// Record that this position reached the cloud (a single send, or the keyframe) — the unarmed
    /// deadband and underway entry measure from here.
    pub fn mark_sent(&mut self, lat: f64, lon: f64) {
        self.last_sent = Some((lat, lon));
    }

    /// Does this device want the 30 s sample rate (before the lease / LAN-live overrides)? An anchor
    /// watch or a trip — never a security zone alone.
    pub fn wants_fast_sampling(&self) -> bool {
        self.anchor_armed || self.underway
    }

    /// Evaluate one sample. `leased` = a member is watching (every sample goes out).
    pub fn observe(&mut self, watch: Option<&Watch>, s: &Sample, now_ms: i64, leased: bool) -> Decision {
        self.sync_watch(watch);
        let mut d = Decision::default();
        let sample_secs = if self.wants_fast_sampling() { watch.map_or(ARMED_SAMPLE_SECS, |w| w.sample_secs) } else { 60 };
        let bad = unreliable(s, sample_secs);
        let acc = s.acc.unwrap_or(0.0).max(0.0);

        let mut inside: Option<bool> = None;
        let mut dist_center: Option<f64> = None;
        let mut anchor_breach = false;

        if let Some(w) = watch {
            if let Some(a) = &w.anchor {
                let dist = distance_m(a.lat, a.lon, s.lat, s.lon);
                let limit = a.radius_m + acc;
                inside = Some(dist <= limit);
                dist_center = Some(dist);
                if !bad {
                    if dist > limit {
                        self.anchor_streak += 1;
                        if self.anchor_streak >= ANCHOR_STREAK && !self.anchor_alerted {
                            self.anchor_alerted = true;
                            d.events.push(("anchor.motion".into(), ring_params(dist, a.radius_m, s)));
                        }
                    } else {
                        self.anchor_streak = 0;
                        self.anchor_alerted = false;
                    }
                    // The warn ring counts only while the alarm ring holds — the drag alarm says
                    // everything the warning would.
                    if a.warn_m > 0.0 && dist <= limit {
                        if dist > a.warn_m + acc {
                            self.warn_streak += 1;
                            if self.warn_streak >= ANCHOR_STREAK && !self.warn_alerted {
                                self.warn_alerted = true;
                                d.events.push(("anchor.warn.motion".into(), ring_params(dist, a.warn_m, s)));
                            }
                        } else {
                            self.warn_streak = 0;
                            self.warn_alerted = false;
                        }
                    }
                }
                anchor_breach = self.anchor_streak >= ANCHOR_STREAK;
            }
            if let Some(z) = &w.zone {
                let dist = distance_m(z.lat, z.lon, s.lat, s.lon);
                let limit = z.radius_m + acc;
                if inside.is_none() {
                    inside = Some(dist <= limit);
                    dist_center = Some(dist);
                }
                if !bad {
                    if dist > limit {
                        self.zone_streak += 1;
                        if self.zone_streak >= z.streak && !self.zone_alerted {
                            self.zone_alerted = true;
                            d.events.push(("zone.motion".into(), ring_params(dist, z.radius_m, s)));
                        }
                    } else {
                        self.zone_streak = 0;
                        self.zone_alerted = false;
                    }
                }
            }
        }

        // Underway: an unreliable stretch never starts or ends a trip.
        if bad {
            self.enter_streak = 0;
            self.slow_since = None;
        } else if !self.underway {
            let fast = s.sog_kn.is_some_and(|v| v >= UNDERWAY_ENTER_SOG_KN);
            let moved = self.last_sent.is_some_and(|(la, lo)| distance_m(la, lo, s.lat, s.lon) >= UNDERWAY_ENTER_MOVE_M);
            self.enter_streak = if fast || moved { self.enter_streak + 1 } else { 0 };
            if self.enter_streak >= UNDERWAY_ENTER_STREAK {
                self.underway = true;
                self.enter_streak = 0;
                self.slow_since = None;
                d.underway_changed = Some(true);
            }
        } else {
            let slow = s.sog_kn.map_or(true, |v| v < UNDERWAY_EXIT_SOG_KN);
            if !slow {
                self.slow_since = None;
            } else {
                match self.slow_since {
                    Some((t, la, lo)) if distance_m(la, lo, s.lat, s.lon) < UNDERWAY_EXIT_NET_M => {
                        if now_ms - t >= UNDERWAY_EXIT_MS {
                            self.underway = false;
                            self.slow_since = None;
                            d.underway_changed = Some(false);
                        }
                    }
                    _ => self.slow_since = Some((now_ms, s.lat, s.lon)),
                }
            }
        }

        d.send_position = if leased {
            true
        } else if self.final_fix_owed {
            self.final_fix_owed = false;
            true
        } else if d.underway_changed.is_some() {
            // Entering (the track's first point) or stopping (where the boat came to rest).
            self.underway_sent_ms = if self.underway { Some(now_ms) } else { None };
            true
        } else if self.underway {
            let due = match self.underway_sent_ms {
                None => true,
                Some(t) => now_ms - t >= UNDERWAY_SEND_SECS as i64 * 1000,
            };
            if due {
                self.underway_sent_ms = Some(now_ms);
            }
            due || (self.anchor_armed && anchor_breach)
        } else if self.anchor_armed {
            anchor_breach
        } else {
            match self.last_sent {
                None => true,
                Some((la, lo)) => {
                    let moved = distance_m(la, lo, s.lat, s.lon);
                    moved >= UNARMED_DEADBAND_M && moved > 2.0 * acc
                }
            }
        };

        self.last = Some(Observed { sample: s.clone(), at_ms: now_ms, inside, dist_from_center_m: dist_center, unreliable: bad });
        d
    }

    /// The `gps.heartbeat` params (§A7.3) — sent only while an anchor watch is armed: no position, ever.
    /// `fixValid sats hdop fixAgeS inside distFromCenterM streak unreliable anchorsig`.
    pub fn heartbeat_params(&self, now_ms: i64) -> Vec<(String, String)> {
        let mut p: Vec<(String, String)> = Vec::new();
        match &self.last {
            None => {
                p.push(("fixValid".into(), "0".into()));
                p.push(("unreliable".into(), "1".into()));
            }
            Some(o) => {
                let age_s = ((now_ms - o.at_ms).max(0) as f64 / 1000.0) + o.sample.fix_age_s;
                let stale = age_s > 3.0 * ARMED_SAMPLE_SECS as f64;
                p.push(("fixValid".into(), if stale { "0" } else { "1" }.into()));
                if let Some(n) = o.sample.sats {
                    p.push(("sats".into(), n.to_string()));
                }
                if let Some(h) = o.sample.hdop {
                    p.push(("hdop".into(), format!("{h:.1}")));
                }
                p.push(("fixAgeS".into(), format!("{}", age_s.round() as i64)));
                if let Some(i) = o.inside {
                    p.push(("inside".into(), if i { "1" } else { "0" }.into()));
                }
                if let Some(dc) = o.dist_from_center_m {
                    p.push(("distFromCenterM".into(), format!("{}", dc.round() as i64)));
                }
                p.push(("unreliable".into(), if o.unreliable || stale { "1" } else { "0" }.into()));
            }
        }
        p.push(("streak".into(), self.anchor_streak.max(self.zone_streak).to_string()));
        p.push(("anchorsig".into(), self.sig.to_string()));
        p
    }
}

fn ring_params(dist: f64, limit: f64, s: &Sample) -> Vec<(String, String)> {
    vec![
        ("dist".into(), format!("{}", dist.round() as i64)),
        ("limit".into(), format!("{}", limit.round() as i64)),
        ("lat".into(), format!("{:.6}", s.lat)),
        ("lon".into(), format!("{:.6}", s.lon)),
    ]
}

/// PURE: how often a GPS source should be sampled now.
///   * a LAN long-poll open (`/api/hub/gps/live`) → the source's full rate (NMEA 1 s, router 10 s)
///   * leased → NMEA 10 s, router 30 s
///   * armed or underway → 30 s
///   * otherwise → `idle_secs` (60 s for the hub's own source, the router's poll cadence)
pub fn sample_secs(fast: bool, leased: bool, lan_live: bool, nmea: bool, idle_secs: u64) -> u64 {
    if lan_live {
        return if nmea { 1 } else { 10 };
    }
    if leased {
        return if nmea { 10 } else { 30 };
    }
    if fast {
        return ARMED_SAMPLE_SECS.min(idle_secs.max(1));
    }
    idle_secs
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FIXTURE: &str = include_str!("../fixtures/anchor-reply.v2.json");

    /// Metres north of a point, in degrees of latitude.
    fn north(m: f64) -> f64 {
        m / 111_195.0
    }

    fn at(lat: f64, lon: f64) -> Sample {
        Sample { lat, lon, acc: Some(3.0), hdop: Some(0.9), sats: Some(10), sog_kn: Some(0.0), fix_age_s: 0.0 }
    }

    fn anchor_watch(radius: f64, warn: f64) -> Watch {
        Watch {
            sig: 1757750400000,
            anchor: Some(Circle { lat: 41.0, lon: -81.0, radius_m: radius, warn_m: warn }),
            zone: None,
            hb_secs: 60,
            sample_secs: 30,
        }
    }

    #[test]
    fn parses_every_fixture_case_byte_matched_to_sc4_internal() {
        let fx: Value = serde_json::from_str(FIXTURE).unwrap();
        let cases = &fx["cases"];
        match parse_anchor_object(&cases["armed"]["reply"]).unwrap() {
            AnchorReply::Arm(w) => {
                assert_eq!(w.sig, 1757750400000);
                let a = w.anchor.unwrap();
                assert_eq!((a.lat, a.lon, a.radius_m, a.warn_m), (41.492907, -81.694361, 60.0, 45.0));
                assert!(w.zone.is_none());
                assert_eq!((w.hb_secs, w.sample_secs), (60, 30));
            }
            other => panic!("armed parsed as {other:?}"),
        }
        assert_eq!(parse_anchor_object(&cases["disarm"]["reply"]), Some(AnchorReply::Disarm));
        match parse_anchor_object(&cases["v2-flat-extras"]["reply"]).unwrap() {
            AnchorReply::Arm(w) => {
                assert_eq!(w.sig, 3515501400000);
                let z = w.zone.unwrap();
                assert_eq!((z.lat, z.lon, z.radius_m, z.streak), (41.4929, -81.6944, 40.0, 3));
                assert_eq!(w.anchor.unwrap().radius_m, 60.0);
            }
            other => panic!("v2 parsed as {other:?}"),
        }
    }

    #[test]
    fn a_reply_without_an_anchor_or_with_a_garbled_one_changes_nothing() {
        assert_eq!(parse_anchor_reply(&json!({"status": "ok"})), None);
        assert_eq!(parse_anchor_reply(&json!({"anchor": {"lat": 1.5, "sig": 7}})), None, "armed sig with no geometry is not a disarm");
        assert_eq!(parse_anchor_reply(&json!({"anchor": "nope"})), None);
        // v1 (flag off) shape still arms.
        assert!(matches!(
            parse_anchor_reply(&json!({"anchor": {"lat": 41.4086, "lon": -81.7494, "radiusM": 50, "warnM": 30, "sig": 1234}})),
            Some(AnchorReply::Arm(Watch { sig: 1234, .. }))
        ));
    }

    #[test]
    fn distance_matches_hub_lite_reference_points() {
        assert!(distance_m(41.4086, -81.7494, 41.4086, -81.7494) < 0.01);
        let d = distance_m(41.4086, -81.7494, 41.4095, -81.7494);
        assert!((95.0..105.0).contains(&d), "0.0009 deg lat is about 100 m, got {d}");
        assert!(distance_m(0.0, 179.9995, 0.0, -179.9995) < 200.0, "date-line crossing stays short");
    }

    #[test]
    fn anchor_drag_needs_two_consecutive_samples_outside_by_more_than_accuracy() {
        let w = anchor_watch(60.0, 0.0);
        let mut g = Geofence::default();
        // Inside: nothing.
        let d = g.observe(Some(&w), &at(41.0 + north(30.0), -81.0), 0, false);
        assert!(d.events.is_empty() && !d.send_position, "a swinging boat inside its circle sends nothing");
        // Borderline: outside the radius but within its own error bar — never counts.
        let mut s = at(41.0 + north(62.0), -81.0);
        s.acc = Some(5.0);
        assert!(g.observe(Some(&w), &s, 30_000, false).events.is_empty());
        // One outside: streak 1, no alarm yet.
        let d = g.observe(Some(&w), &at(41.0 + north(80.0), -81.0), 60_000, false);
        assert!(d.events.is_empty() && !d.send_position);
        // Second outside: DRAG.
        let d = g.observe(Some(&w), &at(41.0 + north(85.0), -81.0), 90_000, false);
        assert_eq!(d.events.len(), 1);
        assert_eq!(d.events[0].0, "anchor.motion");
        assert!(d.send_position, "positions every tick while the breach holds");
        // Third outside: position again, but the alarm fired once per episode.
        let d = g.observe(Some(&w), &at(41.0 + north(90.0), -81.0), 120_000, false);
        assert!(d.events.is_empty() && d.send_position);
        // Back inside: episode over, positions stop; a new drift alarms again.
        assert!(!g.observe(Some(&w), &at(41.0 + north(10.0), -81.0), 150_000, false).send_position);
        g.observe(Some(&w), &at(41.0 + north(80.0), -81.0), 180_000, false);
        assert_eq!(g.observe(Some(&w), &at(41.0 + north(80.0), -81.0), 210_000, false).events.len(), 1);
    }

    #[test]
    fn an_unreliable_sample_never_advances_a_breach_streak() {
        let w = anchor_watch(60.0, 0.0);
        let mut g = Geofence::default();
        g.observe(Some(&w), &at(41.0 + north(80.0), -81.0), 0, false);
        let mut bad = at(41.0 + north(200.0), -81.0);
        bad.hdop = Some(9.0);
        assert!(g.observe(Some(&w), &bad, 30_000, false).events.is_empty(), "hdop 9 cannot confirm a drag");
        let mut few = at(41.0 + north(200.0), -81.0);
        few.sats = Some(3);
        assert!(g.observe(Some(&w), &few, 60_000, false).events.is_empty(), "3 satellites cannot confirm a drag");
        assert!(g.last().unwrap().unreliable);
    }

    #[test]
    fn the_warn_ring_has_its_own_streak_and_only_counts_inside_the_alarm_ring() {
        let w = anchor_watch(60.0, 40.0);
        let mut g = Geofence::default();
        assert!(g.observe(Some(&w), &at(41.0 + north(50.0), -81.0), 0, false).events.is_empty());
        let d = g.observe(Some(&w), &at(41.0 + north(52.0), -81.0), 30_000, false);
        assert_eq!(d.events.iter().map(|e| e.0.as_str()).collect::<Vec<_>>(), vec!["anchor.warn.motion"]);
        assert!(!d.send_position, "a warning is an event, not a breach");
    }

    #[test]
    fn a_zone_breach_needs_its_streak_of_three_and_is_sent_at_once() {
        let w = Watch {
            sig: 9,
            anchor: None,
            zone: Some(Zone { lat: 41.0, lon: -81.0, radius_m: 40.0, streak: 3 }),
            hb_secs: 60,
            sample_secs: 30,
        };
        let mut g = Geofence::default();
        g.mark_sent(41.0 + north(70.0), -81.0);
        for (i, expect) in [(0, 0usize), (1, 0), (2, 1), (3, 0)] {
            let d = g.observe(Some(&w), &at(41.0 + north(70.0), -81.0), i * 60_000, false);
            assert_eq!(d.events.len(), expect, "sample {i}");
            if expect == 1 {
                assert_eq!(d.events[0].0, "zone.motion");
                let p: std::collections::HashMap<_, _> = d.events[0].1.iter().cloned().collect();
                assert!(p.contains_key("lat") && p.contains_key("dist"), "the breach event carries where the boat is");
            }
            assert!(!d.send_position, "a zone breach is an EVENT, not a stream of positions (owner: zone is the 15-min cadence)");
        }
    }

    #[test]
    fn a_security_zone_alone_stays_on_the_idle_cadence() {
        // Owner ruling 2026-09-15: "Security zone is 15 min checkin, not faster like the anchorwatch."
        let zone = Watch {
            sig: 9,
            anchor: None,
            zone: Some(Zone { lat: 41.0, lon: -81.0, radius_m: 40.0, streak: 3 }),
            hb_secs: 60,
            sample_secs: 30,
        };
        let mut g = Geofence::default();
        g.sync_watch(Some(&zone));
        assert!(g.armed() && !g.anchor_armed());
        assert!(!g.wants_fast_sampling(), "no 30 s sampling for a zone");
        // Inside the zone it behaves like an unarmed boat: the deadband, not silence-until-breach.
        g.mark_sent(41.0, -81.0);
        assert!(!g.observe(Some(&zone), &at(41.0 + north(10.0), -81.0), 0, false).send_position);
        let mut both = anchor_watch(60.0, 0.0);
        both.zone = zone.zone.clone();
        g.sync_watch(Some(&both));
        assert!(g.anchor_armed() && g.wants_fast_sampling(), "an anchor watch does buy the fast cadence");
    }

    #[test]
    fn rearming_resets_streaks_and_a_disarm_owes_one_final_fix() {
        let w = anchor_watch(60.0, 0.0);
        let mut g = Geofence::default();
        g.observe(Some(&w), &at(41.0 + north(80.0), -81.0), 0, false);
        let mut w2 = w.clone();
        w2.sig += 1;
        assert!(g.observe(Some(&w2), &at(41.0 + north(80.0), -81.0), 30_000, false).events.is_empty(), "new sig = new episode");
        // Disarm.
        g.mark_sent(41.0, -81.0);
        assert!(g.observe(None, &at(41.0 + north(5.0), -81.0), 60_000, false).send_position, "one final fix on disarm");
        assert!(!g.observe(None, &at(41.0 + north(6.0), -81.0), 120_000, false).send_position);
        assert_eq!(g.sig(), 0);
    }

    #[test]
    fn unarmed_sends_only_past_the_deadband_and_twice_the_accuracy() {
        let mut g = Geofence::default();
        assert!(g.observe(None, &at(41.0, -81.0), 0, false).send_position, "first fix establishes the baseline");
        g.mark_sent(41.0, -81.0);
        assert!(!g.observe(None, &at(41.0 + north(30.0), -81.0), 60_000, false).send_position, "30 m is wander");
        let mut wide = at(41.0 + north(55.0), -81.0);
        wide.acc = Some(30.0);
        assert!(!g.observe(None, &wide, 120_000, false).send_position, "55 m inside 2 x acc(30) is not a move");
        assert!(g.observe(None, &at(41.0 + north(55.0), -81.0), 180_000, false).send_position);
        assert_eq!(deadband_m(10.0), DEADBAND_FLOOR_M);
        assert_eq!(deadband_m(80.0), 80.0);
    }

    #[test]
    fn leased_sends_every_sample_even_an_unreliable_one() {
        let mut g = Geofence::default();
        g.mark_sent(41.0, -81.0);
        let mut s = at(41.0, -81.0);
        s.hdop = Some(12.0);
        assert!(g.observe(None, &s, 0, true).send_position);
    }

    #[test]
    fn underway_enters_on_two_fast_samples_and_exits_after_five_slow_minutes() {
        let mut g = Geofence::default();
        g.mark_sent(41.0, -81.0);
        let mut s = at(41.0, -81.0);
        s.sog_kn = Some(2.0);
        assert_eq!(g.observe(None, &s, 0, false).underway_changed, None, "one fast sample is not a trip");
        let d = g.observe(None, &s, 30_000, false);
        assert_eq!(d.underway_changed, Some(true));
        assert!(d.send_position && g.underway() && g.wants_fast_sampling());
        // Still moving: sampled at 30 s, but the next position is not due for 5 minutes.
        assert!(!g.observe(None, &s, 60_000, false).send_position);
        assert!(g.observe(None, &s, 60_000, true).send_position, "a lease still sends every sample");
        // Stopped: 4 min 30 s is not enough.
        let mut stop = at(41.0 + north(3.0), -81.0);
        stop.sog_kn = Some(0.1);
        let t0 = 90_000;
        for k in 0..=9 {
            let d = g.observe(None, &stop, t0 + k * 30_000, false);
            assert_eq!(d.underway_changed, None, "tick {k}");
        }
        let d = g.observe(None, &stop, t0 + UNDERWAY_EXIT_MS, false);
        assert_eq!(d.underway_changed, Some(false));
        assert!(d.send_position, "the stop position goes out");
        assert!(!g.underway());
    }

    #[test]
    fn an_underway_hour_sends_about_twelve_positions() {
        // Owner ruling 2026-09-15: "Underway: GPS goes every 5 minutes." One hour at 6 kn, sampled
        // every 30 s (120 samples), nobody watching.
        let mut g = Geofence::default();
        let (mut lat, lon) = (41.0, -81.0);
        g.mark_sent(lat, lon);
        let mut sends = 0;
        for k in 0..120i64 {
            lat += north(92.6); // 6 kn for 30 s
            let mut s = at(lat, lon);
            s.sog_kn = Some(6.0);
            let d = g.observe(None, &s, k * 30_000, false);
            if d.send_position {
                sends += 1;
                g.mark_sent(s.lat, s.lon);
            }
        }
        assert!(g.underway());
        assert!((12..=13).contains(&sends), "about 12 positions an hour underway, got {sends}");
        // The same hour with the app open: every sample.
        let mut w = Geofence::default();
        let mut leased_sends = 0;
        for k in 0..120i64 {
            let mut s = at(41.0 + north(92.6 * k as f64), lon);
            s.sog_kn = Some(6.0);
            if w.observe(None, &s, k * 30_000, true).send_position {
                leased_sends += 1;
            }
        }
        assert_eq!(leased_sends, 120);
    }

    #[test]
    fn underway_enters_on_fifty_metres_twice_without_speed() {
        let mut g = Geofence::default();
        g.mark_sent(41.0, -81.0);
        let mut s = at(41.0 + north(60.0), -81.0);
        s.sog_kn = None;
        let d = g.observe(None, &s, 0, false);
        assert!(d.send_position && d.underway_changed.is_none());
        g.mark_sent(s.lat, s.lon);
        s.lat += north(60.0);
        assert_eq!(g.observe(None, &s, 60_000, false).underway_changed, Some(true));
    }

    #[test]
    fn a_drifting_stop_restarts_the_exit_clock_and_bad_quality_blocks_exit() {
        let mut g = Geofence::default();
        let mut s = at(41.0, -81.0);
        s.sog_kn = Some(3.0);
        g.observe(None, &s, 0, false);
        g.observe(None, &s, 30_000, false);
        assert!(g.underway());
        s.sog_kn = Some(0.2);
        g.observe(None, &s, 60_000, false);
        // 30 m net movement restarts the clock.
        let mut moved = s.clone();
        moved.lat += north(30.0);
        g.observe(None, &moved, 60_000 + 4 * 60_000, false);
        assert_eq!(g.observe(None, &moved, 60_000 + 6 * 60_000, false).underway_changed, None);
        // An unreliable sample clears the stretch, so exit needs a fresh five minutes.
        let mut bad = moved.clone();
        bad.sats = Some(2);
        g.observe(None, &bad, 60_000 + 8 * 60_000, false);
        assert_eq!(g.observe(None, &moved, 60_000 + 9 * 60_000, false).underway_changed, None);
        assert_eq!(g.observe(None, &moved, 60_000 + 14 * 60_000, false).underway_changed, Some(false));
    }

    #[test]
    fn heartbeat_carries_quality_and_distance_but_never_a_position() {
        let w = anchor_watch(60.0, 0.0);
        let mut g = Geofence::default();
        g.observe(Some(&w), &at(41.0 + north(34.0), -81.0), 1_000, false);
        let p: std::collections::HashMap<String, String> = g.heartbeat_params(13_000).into_iter().collect();
        assert_eq!(p["fixValid"], "1");
        assert_eq!(p["sats"], "10");
        assert_eq!(p["hdop"], "0.9");
        assert_eq!(p["fixAgeS"], "12");
        assert_eq!(p["inside"], "1");
        assert_eq!(p["distFromCenterM"], "34");
        assert_eq!(p["streak"], "0");
        assert_eq!(p["unreliable"], "0");
        assert_eq!(p["anchorsig"], "1757750400000");
        assert!(!p.contains_key("lat") && !p.contains_key("lon"));
        // A source that went quiet: fixValid 0, unreliable 1.
        let q: std::collections::HashMap<String, String> = g.heartbeat_params(200_000).into_iter().collect();
        assert_eq!((q["fixValid"].as_str(), q["unreliable"].as_str()), ("0", "1"));
        assert_eq!(Geofence::default().heartbeat_params(0).iter().find(|(k, _)| k == "fixValid").unwrap().1, "0");
    }

    #[test]
    fn sampling_speeds_up_for_a_live_viewer_a_lease_and_a_watch() {
        assert_eq!(sample_secs(false, false, false, true, 60), 60);
        assert_eq!(sample_secs(true, false, false, true, 60), 30);
        assert_eq!(sample_secs(false, true, false, true, 60), 10);
        assert_eq!(sample_secs(false, true, false, false, 120), 30);
        assert_eq!(sample_secs(true, true, true, true, 60), 1);
        assert_eq!(sample_secs(false, false, true, false, 120), 10);
    }
}
