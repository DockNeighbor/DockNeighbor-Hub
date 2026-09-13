//! Find NMEA 0183 over IP on the boat LAN without the owner typing an address.
//!
//! WHY (owner, 2026-09-12): "my tz navigator auto finds the NMEA over IP. i would like to do
//! similar." Navigation programs find a feed two ways, and so does this:
//!
//!   1. PASSIVE UDP. Most Wi-Fi NMEA gateways and multiplexers BROADCAST sentences to a well-known
//!      port. Listening on those ports for a few seconds hears every such feed at once, with no
//!      traffic sent. Ports: 10110 (IEC 61162-450 / NMEA's registered port), 2000 (Yacht Devices,
//!      many Wi-Fi gateways), 60001 (Actisense W2K-1, Digital Yacht), 1456, 2002.
//!   2. ACTIVE TCP. Plotters and gateways that serve TCP only answer a client, so the hub tries the
//!      usual TCP ports on every host of each private /24 it is attached to (10110, 2000, 39150,
//!      8375) and keeps a host only if what it sends is real NMEA.
//!
//! The listen SHARES each UDP port (gps::bind_udp_shared) because the hub may run on the very PC
//! that is running TimeZero, which already holds the port. A port that still cannot be bound is
//! skipped and reported, never fatal.
//!
//! The whole call finishes in about 12 s so it fits the cloud relay's 15 s timeout.

use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

/// Common NMEA-over-IP UDP ports, in the order they are most often seen.
pub const UDP_PORTS: [u16; 5] = [10110, 2000, 60001, 1456, 2002];
/// Common NMEA-over-IP TCP server ports.
pub const TCP_PORTS: [u16; 4] = [10110, 2000, 39150, 8375];
/// How long the passive UDP listen runs. Gateways send at least once a second.
pub const UDP_WINDOW: Duration = Duration::from_secs(8);
/// A host that does not accept within this is treated as absent.
pub const TCP_CONNECT_TIMEOUT: Duration = Duration::from_millis(400);
/// How long an open TCP port is read for sentences.
pub const TCP_READ_WINDOW: Duration = Duration::from_millis(2500);
/// The TCP probe's hard stop, measured from the start of the call.
pub const TCP_DEADLINE: Duration = Duration::from_millis(11_500);
/// Parallel TCP connects. ~1000 targets per /24 at 128 wide and 400 ms each is ~3 s worst case.
pub const TCP_CONCURRENCY: usize = 128;
/// Pass 2 (see `discover`): a patient retry of only the hosts the LAN says are alive. 1.5 s covers
/// Windows' behaviour of dropping the first SYN to a host whose ARP entry is still being resolved
/// and resending it about a second later — which a 400 ms pass-1 timeout never waits for.
pub const TCP_RETRY_CONNECT_TIMEOUT: Duration = Duration::from_millis(1500);
pub const TCP_RETRY_CONCURRENCY: usize = 16;
/// Pass 1 is cut off here so pass 2 always has time left inside TCP_DEADLINE.
pub const TCP_SWEEP_DEADLINE: Duration = Duration::from_millis(4_000);
/// Text kept per source; enough for every sentence type a feed sends in a few seconds.
const MAX_TEXT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LatLon {
    pub lat: f64,
    pub lon: f64,
}

/// One feed the hub heard. `host`/`port`/`protocol` are exactly what the GPS source form needs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NmeaSource {
    pub protocol: String,
    pub host: String,
    pub port: u16,
    pub talkers: Vec<String>,
    pub sentences: Vec<String>,
    pub fix: Option<LatLon>,
    pub sample: String,
}

/// A UDP port the hub could not listen on (another program holds it exclusively).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skipped {
    pub protocol: String,
    pub port: u16,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiscoverBody {
    pub sources: Vec<NmeaSource>,
    pub skipped: Vec<Skipped>,
}

// --- Pure helpers ---------------------------------------------------------------------------------

/// PURE: talker and sentence type of one line, or None unless it is a checksum-valid NMEA 0183
/// sentence. Stricter than the poll's parser on purpose: the checksum must be PRESENT, because a
/// discovery listens to whatever arrives on a shared port and a stray text packet must not be
/// offered as a GPS. `$GPRMC` → (GP, RMC); `!AIVDM` → (AI, VDM); proprietary `$PGRME` → (P, GRME).
pub fn classify_sentence(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    let lead = line.chars().next()?;
    if lead != '$' && lead != '!' {
        return None;
    }
    let star = line.rfind('*')?;
    let hex = line.get(star + 1..star + 3)?;
    let want = u8::from_str_radix(hex, 16).ok()?;
    if line.len() > star + 3 {
        return None;
    }
    let sum = line.as_bytes()[1..star].iter().fold(0u8, |a, b| a ^ b);
    if sum != want {
        return None;
    }
    let address = line[1..star].split(',').next()?;
    if !line[1..star].contains(',') || !address.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
        return None;
    }
    if let Some(rest) = address.strip_prefix('P') {
        // Proprietary: `P` + a manufacturer code + the sentence; no 0183 talker starts with P.
        return (2..=8).contains(&rest.len()).then(|| ("P".into(), rest.into()));
    }
    if address.len() != 5 || !address[..2].chars().all(|c| c.is_ascii_uppercase()) {
        return None;
    }
    Some((address[..2].to_string(), address[2..].to_string()))
}

/// PURE: one source from the text a host sent, or None when nothing in it is valid NMEA.
pub fn summarize(protocol: &str, host: &str, port: u16, text: &str) -> Option<NmeaSource> {
    let mut talkers: Vec<String> = Vec::new();
    let mut sentences: Vec<String> = Vec::new();
    let mut sample: Option<String> = None;
    let mut position_sample: Option<String> = None;
    let mut valid = String::new();
    for raw in text.split(['\r', '\n']) {
        let Some((talker, typ)) = classify_sentence(raw) else { continue };
        let line = raw.trim();
        if !talkers.contains(&talker) {
            talkers.push(talker);
        }
        if sample.is_none() {
            sample = Some(line.to_string());
        }
        if position_sample.is_none() && (typ == "RMC" || typ == "GGA") {
            position_sample = Some(line.to_string());
        }
        if !sentences.contains(&typ) {
            sentences.push(typ);
        }
        valid.push_str(line);
        valid.push('\n');
    }
    let sample = position_sample.or(sample)?;
    talkers.sort();
    sentences.sort();
    let fix = crate::gps::parse_nmea(&valid).map(|f| LatLon { lat: f.lat, lon: f.lon });
    Some(NmeaSource { protocol: protocol.into(), host: host.into(), port, talkers, sentences, fix, sample })
}

/// PURE: datagrams `(sender ip, local port, payload)` → one source per (sender, port). A datagram
/// is one or more whole sentences, so each is terminated before it is appended.
pub fn group_datagrams(datagrams: &[(String, u16, String)]) -> Vec<NmeaSource> {
    let mut by: BTreeMap<(String, u16), String> = BTreeMap::new();
    for (ip, port, payload) in datagrams {
        let text = by.entry((ip.clone(), *port)).or_default();
        if text.len() < MAX_TEXT {
            text.push_str(payload);
            text.push('\n');
        }
    }
    by.into_iter().filter_map(|((ip, port), text)| summarize("udp", &ip, port, &text)).collect()
}

/// PURE: sources with a fix first; then UDP before TCP, then host (numerically), then port.
pub fn order_sources(sources: &mut [NmeaSource]) {
    fn ip_key(h: &str) -> (u8, Vec<u32>, String) {
        match h.parse::<std::net::Ipv4Addr>() {
            Ok(ip) => (0, ip.octets().iter().map(|o| u32::from(*o)).collect(), String::new()),
            Err(_) => (1, Vec::new(), h.to_string()),
        }
    }
    sources.sort_by(|a, b| {
        b.fix.is_some().cmp(&a.fix.is_some())
            .then_with(|| (a.protocol != "udp").cmp(&(b.protocol != "udp")))
            .then_with(|| ip_key(&a.host).cmp(&ip_key(&b.host)))
            .then_with(|| a.port.cmp(&b.port))
    });
}

/// PURE: is this an RFC 1918 private IPv4 address (the only ranges the TCP probe sweeps)?
pub fn is_private_v4(ip: &str) -> bool {
    ip.parse::<std::net::Ipv4Addr>().map(|a| a.is_private()).unwrap_or(false)
}

/// PURE: every (host, port) the TCP probe tries for the hub's own addresses: each private /24 once,
/// hosts .1–.254, the hub's own addresses excluded.
pub fn tcp_targets(own: &[String], ports: &[u16]) -> Vec<(String, u16)> {
    let mut prefixes: Vec<String> = Vec::new();
    for ip in own.iter().filter(|ip| is_private_v4(ip)) {
        if let Some(p) = crate::linktap_discover::slash24_prefix(ip) {
            if !prefixes.contains(&p) {
                prefixes.push(p);
            }
        }
    }
    let mut out = Vec::new();
    for prefix in &prefixes {
        for n in 1u16..=254 {
            let host = format!("{prefix}.{n}");
            if own.contains(&host) {
                continue;
            }
            for port in ports {
                out.push((host.clone(), *port));
            }
        }
    }
    out
}

// --- Transports -----------------------------------------------------------------------------------

/// Listen on every port in `ports` for `window`, concurrently. Returns the sources heard and the
/// ports that could not be bound.
pub async fn listen_udp(ports: &[u16], window: Duration) -> (Vec<NmeaSource>, Vec<Skipped>) {
    let mut skipped = Vec::new();
    let mut set = tokio::task::JoinSet::new();
    for &port in ports {
        match crate::gps::bind_udp_shared(port) {
            Ok(sock) => {
                set.spawn(async move { listen_one(sock, port, window).await });
            }
            Err(e) => skipped.push(Skipped { protocol: "udp".into(), port, error: udp_bind_error(&e) }),
        }
    }
    let mut datagrams: Vec<(String, u16, String)> = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(mut d) = res {
            datagrams.append(&mut d);
        }
    }
    (group_datagrams(&datagrams), skipped)
}

/// Say what a refused UDP bind usually MEANS. Windows answers WSAEACCES (10013) when another program
/// holds the port exclusively — on the boat PC that is the chart plotter (TimeZero on CENTRAL,
/// 2026-09-13). The raw text alone ("forbidden by its access permissions") reads like a hub fault.
pub fn udp_bind_error(e: &std::io::Error) -> String {
    let in_use = e.kind() == std::io::ErrorKind::AddrInUse
        || e.kind() == std::io::ErrorKind::PermissionDenied
        || e.raw_os_error() == Some(10013)
        || e.raw_os_error() == Some(10048);
    if in_use {
        format!("another program on this computer holds this port (a chart plotter?) — its TCP feed can still be found ({e})")
    } else {
        e.to_string()
    }
}

async fn listen_one(sock: tokio::net::UdpSocket, port: u16, window: Duration) -> Vec<(String, u16, String)> {
    let mut out = Vec::new();
    let mut kept = 0usize;
    let mut buf = vec![0u8; 4096];
    let deadline = tokio::time::Instant::now() + window;
    loop {
        match tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, from))) => {
                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                // A datagram counts only if it holds at least one valid sentence.
                if kept < 4 * MAX_TEXT && text.split(['\r', '\n']).any(|l| classify_sentence(l).is_some()) {
                    kept += text.len();
                    out.push((from.ip().to_string(), port, text));
                }
            }
            // Windows reports an ICMP port-unreachable as a recv error on UDP; keep listening.
            Ok(Err(_)) => tokio::time::sleep(Duration::from_millis(20)).await,
            Err(_) => break,
        }
    }
    out
}

/// Try every target, `concurrency` at a time, until `deadline`. A target is kept only if it sends
/// valid NMEA within `read_window` of connecting.
pub async fn probe_tcp(
    targets: Vec<(String, u16)>,
    connect_timeout: Duration,
    read_window: Duration,
    deadline: tokio::time::Instant,
    concurrency: usize,
) -> Vec<NmeaSource> {
    probe_tcp_pass(targets, connect_timeout, read_window, deadline, concurrency).await.0
}

/// One pass: the sources heard, and the targets whose connect TIMED OUT (or never got a turn
/// before `deadline`) — the only ones a slower retry could still find something on.
async fn probe_tcp_pass(
    targets: Vec<(String, u16)>,
    connect_timeout: Duration,
    read_window: Duration,
    deadline: tokio::time::Instant,
    concurrency: usize,
) -> (Vec<NmeaSource>, Vec<(String, u16)>) {
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency.max(1)));
    let mut set = tokio::task::JoinSet::new();
    for (host, port) in targets {
        let sem = sem.clone();
        set.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else { return (host, port, Probe::TimedOut) };
            if tokio::time::Instant::now() + connect_timeout > deadline {
                return (host, port, Probe::TimedOut);
            }
            let outcome = probe_one(&host, port, connect_timeout, read_window, deadline).await;
            (host, port, outcome)
        });
    }
    let mut found = Vec::new();
    let mut timed_out = Vec::new();
    loop {
        match tokio::time::timeout_at(deadline, set.join_next()).await {
            Ok(Some(Ok((_, _, Probe::Heard(Some(s)))))) => found.push(s),
            Ok(Some(Ok((host, port, Probe::TimedOut)))) => timed_out.push((host, port)),
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => {
                set.abort_all();
                break;
            }
        }
    }
    (found, timed_out)
}

/// PURE: IPv4 addresses the OS neighbour (ARP) table holds as RESOLVED, from `arp -a` / `arp -an`
/// output (Windows and macOS/BSD) or `/proc/net/arp` (Linux). Incomplete entries, broadcast,
/// multicast and all-zero MACs are dropped — they are not hosts that answered.
pub fn parse_neighbors(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("incomplete") {
            continue;
        }
        let ip = line
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .find(|t| t.parse::<std::net::Ipv4Addr>().is_ok());
        let Some(ip) = ip else { continue };
        let Ok(addr) = ip.parse::<std::net::Ipv4Addr>() else { continue };
        if addr.is_multicast() || addr.is_broadcast() || addr.octets()[3] == 255 || addr.octets()[3] == 0 {
            continue;
        }
        // A real MAC on the line: six hex groups separated by ':' or '-', not all zero / all ff.
        let mac = lower
            .split_whitespace()
            .find(|t| {
                let parts: Vec<&str> = t.split([':', '-']).collect();
                parts.len() == 6 && parts.iter().all(|p| !p.is_empty() && p.len() <= 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
            })
            .map(|t| t.replace('-', ":"));
        let Some(mac) = mac else { continue };
        if mac.split(':').all(|p| p.trim_start_matches('0').is_empty()) || mac.split(':').all(|p| p == "ff") {
            continue;
        }
        let ip = addr.to_string();
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

/// PURE: the pass-2 targets — pass-1 timeouts on hosts the LAN says are alive (neighbour table, or a
/// host we heard any datagram from), known NMEA senders first.
pub fn retry_targets(timed_out: &[(String, u16)], alive: &[String], senders: &[String]) -> Vec<(String, u16)> {
    let mut picked: Vec<(String, u16)> = timed_out
        .iter()
        .filter(|(h, _)| alive.contains(h) || senders.contains(h))
        .cloned()
        .collect();
    picked.sort_by_key(|(h, p)| (!senders.contains(h), h.clone(), *p));
    picked.dedup();
    picked
}

/// The OS neighbour table, read the way each platform exposes it. Best-effort: an empty list only
/// means pass 2 falls back to the hosts heard over UDP.
async fn neighbor_ipv4s() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(t) = tokio::fs::read_to_string("/proc/net/arp").await {
            return parse_neighbors(&t);
        }
    }
    let args: &[&str] = if cfg!(windows) { &["-a"] } else { &["-an"] };
    let mut cmd = tokio::process::Command::new("arp");
    cmd.args(args).kill_on_drop(true);
    #[cfg(windows)]
    {
        // No console window flashing up on the boat PC for a background read.
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    match tokio::time::timeout(Duration::from_millis(1500), cmd.output()).await {
        Ok(Ok(out)) => parse_neighbors(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

/// What one connect attempt learned. Only `TimedOut` is worth a patient second try: a refusal is a
/// live host with nothing on that port, and `Heard` already read what there was.
#[derive(Debug)]
enum Probe {
    Heard(Option<NmeaSource>),
    Refused,
    TimedOut,
}

async fn probe_one(
    host: &str,
    port: u16,
    connect_timeout: Duration,
    read_window: Duration,
    deadline: tokio::time::Instant,
) -> Probe {
    use tokio::io::AsyncReadExt;
    let mut stream = match tokio::time::timeout(connect_timeout, tokio::net::TcpStream::connect((host, port))).await {
        Err(_) => return Probe::TimedOut,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::TimedOut => return Probe::TimedOut,
        Ok(Err(_)) => return Probe::Refused,
        Ok(Ok(s)) => s,
    };
    let stop = std::cmp::min(tokio::time::Instant::now() + read_window, deadline);
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 2048];
    while buf.len() < MAX_TEXT {
        match tokio::time::timeout_at(stop, stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let text = String::from_utf8_lossy(&buf);
    Probe::Heard(summarize("tcp", host, port, crate::gps::complete_lines(&text)))
}

/// The endpoint's work: UDP listen and TCP probe at once, results ordered, all within ~12 s.
pub async fn discover() -> DiscoverBody {
    let start = tokio::time::Instant::now();
    let own = crate::linktap_discover::local_ipv4s();
    let targets = tcp_targets(&own, &TCP_PORTS);
    crate::hlog!(
        "nmea discovery: listening on UDP {:?} for {} s and probing {} TCP targets",
        UDP_PORTS,
        UDP_WINDOW.as_secs(),
        targets.len()
    );
    let tcp_two_pass = async {
        // Pass 1: the fast sweep. Besides finding quick hosts it makes the OS resolve every live
        // neighbour, which is what lets pass 2 be small.
        let (mut found, timed_out) =
            probe_tcp_pass(targets, TCP_CONNECT_TIMEOUT, TCP_READ_WINDOW, start + TCP_SWEEP_DEADLINE, TCP_CONCURRENCY).await;
        // Pass 2: patient retries, only on hosts that exist. CENTRAL (Windows) missed a Digital
        // Yacht Nemo serving NMEA on TCP 10110 on 2026-09-13 with pass 1 alone; this Mac found it.
        let alive = neighbor_ipv4s().await;
        let have: Vec<(String, u16)> = found.iter().map(|s| (s.host.clone(), s.port)).collect();
        let retry: Vec<(String, u16)> = retry_targets(&timed_out, &alive, &[])
            .into_iter()
            .filter(|t| !have.contains(t))
            .collect();
        let retried = retry.len();
        let (more, _) =
            probe_tcp_pass(retry, TCP_RETRY_CONNECT_TIMEOUT, TCP_READ_WINDOW, start + TCP_DEADLINE, TCP_RETRY_CONCURRENCY).await;
        found.extend(more);
        (found, alive.len(), retried)
    };
    let ((udp, skipped), (tcp, alive, retried)) = tokio::join!(listen_udp(&UDP_PORTS, UDP_WINDOW), tcp_two_pass);
    crate::hlog!("nmea discovery: {alive} live neighbour(s), {retried} slow TCP target(s) retried");
    let mut sources = udp;
    // A feed heard on BOTH transports from the same host:port is listed once per transport — the
    // owner picks; TCP is the one that coexists with a chart plotter holding the UDP port.
    sources.extend(tcp);
    order_sources(&mut sources);
    crate::hlog!(
        "nmea discovery: found {} source(s), {} UDP port(s) skipped, in {} ms",
        sources.len(),
        skipped.len(),
        start.elapsed().as_millis()
    );
    DiscoverBody { sources, skipped }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sentence(body: &str) -> String {
        let sum = body.bytes().fold(0u8, |a, b| a ^ b);
        format!("${body}*{sum:02X}")
    }
    const RMC: &str = "GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W";
    const GGA: &str = "GNGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,";
    const MWV: &str = "IIMWV,045,R,12.5,N,A";

    #[test]
    fn classify_takes_talker_and_type_from_checksum_valid_lines_only() {
        assert_eq!(classify_sentence(&sentence(RMC)), Some(("GP".into(), "RMC".into())));
        assert_eq!(classify_sentence(&format!("  {}\r", sentence(GGA))), Some(("GN".into(), "GGA".into())));
        let ais = format!("!{}", &sentence("AIVDM,1,1,,A,13aEOK?P00PD2wVMdLDRhgvL289?,0")[1..]);
        assert_eq!(classify_sentence(&ais), Some(("AI".into(), "VDM".into())));
        assert_eq!(classify_sentence(&sentence("PGRME,15.0,M,45.0,M,25.0,M")), Some(("P".into(), "GRME".into())));
        // Wrong checksum, missing checksum, junk, lowercase address, no fields.
        assert_eq!(classify_sentence(&format!("${RMC}*00")), None);
        assert_eq!(classify_sentence(&format!("${RMC}")), None, "discovery needs the checksum present");
        assert_eq!(classify_sentence("hello world"), None);
        assert_eq!(classify_sentence(&sentence("gprmc,1,2")), None);
        assert_eq!(classify_sentence(&sentence("GPRMC")), None);
        assert_eq!(classify_sentence(&format!("{}trailing", sentence(RMC))), None);
    }

    #[test]
    fn summarize_collects_talkers_types_fix_and_prefers_a_position_sample() {
        let text = format!("{}\r\n{}\r\n{}\r\njunk\r\n{}\r\n", sentence(MWV), sentence(RMC), sentence(GGA), sentence(MWV));
        let s = summarize("tcp", "192.168.1.20", 10110, &text).unwrap();
        assert_eq!(s.talkers, vec!["GN", "GP", "II"]);
        assert_eq!(s.sentences, vec!["GGA", "MWV", "RMC"]);
        assert_eq!(s.sample, sentence(RMC));
        let fix = s.fix.unwrap();
        assert!((fix.lat - 48.1173).abs() < 1e-4 && (fix.lon - 11.5167).abs() < 1e-4);
        // Instruments only: a source, no fix, sample is what it sent.
        let wind = summarize("udp", "10.0.0.5", 2000, &sentence(MWV)).unwrap();
        assert_eq!(wind.fix, None);
        assert_eq!(wind.sample, sentence(MWV));
        // Nothing valid: not a source.
        assert_eq!(summarize("udp", "10.0.0.5", 2000, &format!("${RMC}*00\nhello")), None);
    }

    #[test]
    fn datagrams_group_by_sender_and_port_and_drop_senders_with_nothing_valid() {
        let d = vec![
            ("192.168.1.9".to_string(), 2000, sentence(MWV)),
            ("192.168.1.9".to_string(), 2000, sentence(RMC)),
            ("192.168.1.9".to_string(), 10110, sentence(GGA)),
            ("192.168.1.7".to_string(), 2000, "not nmea".to_string()),
        ];
        let g = group_datagrams(&d);
        assert_eq!(g.len(), 2);
        let a = g.iter().find(|s| s.port == 2000).unwrap();
        assert_eq!(a.host, "192.168.1.9");
        assert_eq!(a.sentences, vec!["MWV", "RMC"]);
        assert!(a.fix.is_some());
        assert!(g.iter().all(|s| s.protocol == "udp" && s.host == "192.168.1.9"));
    }

    #[test]
    fn ordering_puts_fixes_first_then_udp_then_host_numerically_then_port() {
        let mk = |p: &str, h: &str, port: u16, fix: bool| NmeaSource {
            protocol: p.into(), host: h.into(), port, talkers: vec![], sentences: vec![],
            fix: fix.then_some(LatLon { lat: 1.0, lon: 1.0 }), sample: String::new(),
        };
        let mut v = vec![
            mk("udp", "192.168.1.100", 2000, false),
            mk("tcp", "192.168.1.9", 10110, true),
            mk("udp", "192.168.1.20", 10110, true),
            mk("udp", "192.168.1.9", 10110, true),
            mk("udp", "192.168.1.9", 2000, true),
        ];
        order_sources(&mut v);
        let got: Vec<String> = v.iter().map(|s| format!("{} {}:{}", s.protocol, s.host, s.port)).collect();
        assert_eq!(got, vec![
            "udp 192.168.1.9:2000", "udp 192.168.1.9:10110", "udp 192.168.1.20:10110",
            "tcp 192.168.1.9:10110", "udp 192.168.1.100:2000",
        ]);
    }

    #[test]
    fn tcp_targets_cover_each_private_slash24_once_without_the_hub_itself() {
        let own = vec!["192.168.1.50".to_string(), "192.168.1.51".to_string(), "100.64.0.2".to_string(), "8.8.8.8".to_string()];
        let t = tcp_targets(&own, &[10110, 2000]);
        assert_eq!(t.len(), 252 * 2, "one /24, two hub addresses skipped, two ports");
        assert!(!t.iter().any(|(h, _)| h == "192.168.1.50" || h == "192.168.1.51"));
        assert!(t.iter().all(|(h, _)| h.starts_with("192.168.1.")));
        assert!(tcp_targets(&["8.8.8.8".into()], &[10110]).is_empty(), "public ranges are never swept");
        assert!(is_private_v4("10.1.2.3") && is_private_v4("172.20.0.1") && !is_private_v4("172.32.0.1"));
    }

    /// A port nobody holds right now (bound, then released) so the listener can take it.
    fn free_udp_port() -> u16 {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        s.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn udp_listen_finds_a_sender_with_its_fix_and_skips_junk() {
        let port = free_udp_port();
        let listen = tokio::spawn(async move { listen_udp(&[port], Duration::from_millis(1500)).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let tx = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        tx.set_broadcast(true).unwrap();
        for _ in 0..3 {
            tx.send_to(b"junk that is not nmea", ("127.0.0.1", port)).await.unwrap();
            let burst = format!("{}\r\n{}\r\n", sentence(RMC), sentence(MWV));
            tx.send_to(burst.as_bytes(), ("127.0.0.1", port)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let (sources, skipped) = listen.await.unwrap();
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(sources.len(), 1, "{sources:?}");
        let s = &sources[0];
        assert_eq!((s.protocol.as_str(), s.host.as_str(), s.port), ("udp", "127.0.0.1", port));
        assert_eq!(s.sentences, vec!["MWV", "RMC"]);
        assert!(s.fix.is_some());
    }

    /// Broadcast datagrams reach a socket bound to 0.0.0.0. Best effort: a sandbox with no
    /// broadcast-capable interface cannot send to 255.255.255.255 at all, and then there is
    /// nothing to assert.
    #[tokio::test]
    async fn udp_listen_receives_limited_broadcast() {
        let port = free_udp_port();
        let listen = tokio::spawn(async move { listen_udp(&[port], Duration::from_millis(1500)).await });
        tokio::time::sleep(Duration::from_millis(200)).await;
        let tx = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
        tx.set_broadcast(true).unwrap();
        let mut sent = false;
        for _ in 0..3 {
            sent |= tx.send_to(sentence(GGA).as_bytes(), ("255.255.255.255", port)).await.is_ok();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let (sources, _) = listen.await.unwrap();
        if !sent {
            eprintln!("no broadcast-capable interface here; broadcast receipt not exercised");
            return;
        }
        assert_eq!(sources.len(), 1, "a 0.0.0.0 listener must hear a broadcast: {sources:?}");
        assert!(sources[0].fix.is_some());
    }

    #[tokio::test]
    async fn two_sockets_share_one_udp_port() {
        let first = crate::gps::bind_udp_shared(0).unwrap();
        let port = first.local_addr().unwrap().port();
        let second = crate::gps::bind_udp_shared(port);
        assert!(second.is_ok(), "the second bind must share the port: {:?}", second.err());
        // And the discovery listen on a held port binds rather than being skipped.
        let (_, skipped) = listen_udp(&[port], Duration::from_millis(50)).await;
        assert!(skipped.is_empty(), "{skipped:?}");
        drop(first);
    }

    #[tokio::test]
    async fn tcp_probe_keeps_an_nmea_server_and_drops_a_silent_or_non_nmea_one() {
        use tokio::io::AsyncWriteExt;
        let nmea = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let nmea_port = nmea.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = nmea.accept().await.unwrap();
            for _ in 0..5 {
                let _ = s.write_all(format!("{}\r\n{}\r\n", sentence(GGA), sentence(MWV)).as_bytes()).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = http.accept().await.unwrap();
            let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            tokio::time::sleep(Duration::from_secs(3)).await;
        });
        let closed = free_tcp_port();
        let started = tokio::time::Instant::now();
        let found = probe_tcp(
            vec![
                ("127.0.0.1".into(), nmea_port),
                ("127.0.0.1".into(), http_port),
                ("127.0.0.1".into(), closed),
            ],
            TCP_CONNECT_TIMEOUT,
            Duration::from_millis(800),
            started + Duration::from_secs(5),
            8,
        )
        .await;
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!((found[0].protocol.as_str(), found[0].port), ("tcp", nmea_port));
        assert_eq!(found[0].sentences, vec!["GGA", "MWV"]);
        assert!(found[0].fix.is_some());
        assert!(started.elapsed() < Duration::from_secs(2), "the read window bounds each probe");
    }

    #[tokio::test]
    async fn tcp_probe_stops_at_the_deadline() {
        // A listener that accepts and never writes: without the deadline this would wait the window.
        let quiet = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = quiet.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _held = quiet.accept().await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        let started = tokio::time::Instant::now();
        let found = probe_tcp(
            vec![("127.0.0.1".into(), port)],
            TCP_CONNECT_TIMEOUT,
            Duration::from_secs(5),
            started + Duration::from_millis(600),
            4,
        )
        .await;
        assert!(found.is_empty());
        assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
    }

    fn free_tcp_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }
}

#[cfg(test)]
mod two_pass {
    use super::*;

    #[test]
    fn windows_arp_a_output_yields_resolved_hosts_only() {
        let t = "\r\nInterface: 172.31.0.105 --- 0x7\r\n  Internet Address      Physical Address      Type\r\n  172.31.0.1            00-1a-dd-c2-11-02     dynamic   \r\n  172.31.0.112          04-79-b7-f2-18-b8     dynamic   \r\n  172.31.0.255          ff-ff-ff-ff-ff-ff     static    \r\n  224.0.0.22            01-00-5e-00-00-16     static    \r\n  239.255.255.250       01-00-5e-7f-ff-fa     static    \r\n";
        assert_eq!(parse_neighbors(t), vec!["172.31.0.1".to_string(), "172.31.0.112".to_string()]);
    }

    #[test]
    fn macos_arp_an_and_linux_proc_net_arp() {
        let mac = "? (172.31.0.112) at 4:79:b7:f2:18:b8 on en0 ifscope [ethernet]\n? (172.31.0.9) at (incomplete) on en0 ifscope [ethernet]\n? (172.31.0.255) at ff:ff:ff:ff:ff:ff on en0 ifscope [ethernet]\n";
        assert_eq!(parse_neighbors(mac), vec!["172.31.0.112".to_string()]);
        let linux = "IP address       HW type     Flags       HW address            Mask     Device\n192.168.8.20     0x1         0x2         aa:bb:cc:dd:ee:01     *        br-lan\n192.168.8.21     0x1         0x0         00:00:00:00:00:00     *        br-lan\n";
        assert_eq!(parse_neighbors(linux), vec!["192.168.8.20".to_string()]);
    }

    #[test]
    fn retry_takes_only_live_timeouts_senders_first() {
        let timed_out = vec![
            ("172.31.0.50".to_string(), 10110), ("172.31.0.112".to_string(), 10110),
            ("172.31.0.112".to_string(), 2000), ("172.31.0.200".to_string(), 10110),
        ];
        let alive = vec!["172.31.0.112".to_string(), "172.31.0.200".to_string()];
        let senders = vec!["172.31.0.200".to_string()];
        assert_eq!(retry_targets(&timed_out, &alive, &senders), vec![
            ("172.31.0.200".to_string(), 10110),
            ("172.31.0.112".to_string(), 2000), ("172.31.0.112".to_string(), 10110),
        ]);
        assert!(retry_targets(&timed_out, &[], &[]).is_empty(), "a host nothing says is alive is not retried");
    }

    #[test]
    fn a_port_held_by_another_program_says_so() {
        let e = std::io::Error::from_raw_os_error(if cfg!(windows) { 10013 } else { 98 });
        let _ = e; // raw codes differ per OS; the kind path is what unix exercises
        let in_use = std::io::Error::new(std::io::ErrorKind::AddrInUse, "Address already in use");
        assert!(udp_bind_error(&in_use).contains("another program"));
        let other = std::io::Error::new(std::io::ErrorKind::Other, "boom");
        assert_eq!(udp_bind_error(&other), "boom");
    }

    #[tokio::test]
    async fn the_retry_pass_reads_a_feed_on_a_host_the_sweep_timed_out_on() {
        use tokio::io::AsyncWriteExt;
        // Loopback accepts instantly, so a real sweep timeout cannot be staged here; the sweep's
        // verdict is supplied directly and the test proves the retry pass reads the feed.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = {
            let s = "GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W";
            let cs = s.bytes().fold(0u8, |a, b| a ^ b);
            format!("${s}*{cs:02X}\r\n")
        };
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let body = body.clone();
                tokio::spawn(async move {
                    for _ in 0..20 { if sock.write_all(body.as_bytes()).await.is_err() { break; } tokio::time::sleep(Duration::from_millis(100)).await; }
                });
            }
        });
        let timed_out = vec![("127.0.0.1".to_string(), port)];
        let retry = retry_targets(&timed_out, &["127.0.0.1".to_string()], &[]);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let (found, _) = probe_tcp_pass(retry, TCP_RETRY_CONNECT_TIMEOUT, Duration::from_millis(800), deadline, TCP_RETRY_CONCURRENCY).await;
        assert_eq!(found.len(), 1);
        assert!(found[0].fix.is_some());
    }
}

#[cfg(test)]
mod live_lan {
    /// Real-network check, run by hand on a boat LAN: `NMEA_LIVE=1 cargo test --lib live_lan -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn discover_on_this_lan() {
        if std::env::var("NMEA_LIVE").is_err() { return; }
        let own = crate::linktap_discover::local_ipv4s();
        let t0 = std::time::Instant::now();
        let body = super::discover().await;
        eprintln!("own={own:?} elapsed={:?}", t0.elapsed());
        for s in &body.sources { eprintln!("{} {}:{} fix={:?} {:?}", s.protocol, s.host, s.port, s.fix, s.sentences); }
        eprintln!("skipped={:?}", body.skipped.iter().map(|k| (k.port, k.error.clone())).collect::<Vec<_>>());
    }
    /// Same, but the sweep is given a 1 ms connect timeout so every host times out and ONLY the
    /// neighbour-table retry can find anything — the CENTRAL failure, reproduced on purpose.
    #[tokio::test]
    #[ignore]
    async fn retry_alone_finds_the_feed_on_this_lan() {
        if std::env::var("NMEA_LIVE").is_err() { return; }
        let own = crate::linktap_discover::local_ipv4s();
        let targets = super::tcp_targets(&own, &super::TCP_PORTS);
        let start = tokio::time::Instant::now();
        let (found1, timed_out) = super::probe_tcp_pass(targets, std::time::Duration::from_millis(1), super::TCP_READ_WINDOW, start + super::TCP_SWEEP_DEADLINE, super::TCP_CONCURRENCY).await;
        let alive = super::neighbor_ipv4s().await;
        let retry = super::retry_targets(&timed_out, &alive, &[]);
        let (found2, _) = super::probe_tcp_pass(retry.clone(), super::TCP_RETRY_CONNECT_TIMEOUT, super::TCP_READ_WINDOW, start + super::TCP_DEADLINE, super::TCP_RETRY_CONCURRENCY).await;
        eprintln!("sweep found {} | timed out {} | alive {} | retried {} | retry found {:?} in {:?}", found1.len(), timed_out.len(), alive.len(), retry.len(), found2.iter().map(|s| format!("{}:{} fix={}", s.host, s.port, s.fix.is_some())).collect::<Vec<_>>(), start.elapsed());
        assert!(found2.iter().any(|s| s.fix.is_some()));
    }
}
