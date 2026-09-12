// GPS acquisition on the LAN — the hub as device manager (owner 2026-09-11: "the hub should be
// controlling/configuring the routers/GPS devices, not the app"). A browser cannot reach a LAN
// router's GPS API over HTTPS (mixed content) and a phone is not always aboard; the hub is, so it
// polls the source itself and reports `gps.measurement` upward through the same spool_report path as
// LinkTap. Today's driver is Cradlepoint NCOS (`GET /api/status/gps`, HTTP Basic auth); the parser
// mirrors the app's parseCradlepointGps/fromDms/validFix so both agree on every payload shape.

use serde::Deserialize;

/// One position fix. `acc` is a radius in metres when the source reports one.
#[derive(Debug, Clone, PartialEq)]
pub struct GpsFix {
    pub lat: f64,
    pub lon: f64,
    pub acc: Option<f64>,
}

/// PURE: the base URL for a Cradlepoint on `host:port`. NCOS is plain HTTP on the LAN by default,
/// but a unit reachable on 443 serves HTTPS — mirror the app's cradlepointBase exactly. Port 0
/// means "unset" and defaults to 443 (owner: the default should be 443).
pub fn cradlepoint_base(host: &str, port: u16) -> String {
    let p = if port == 0 { 443 } else { port };
    let scheme = if p == 443 { "https" } else { "http" };
    if p == 80 || p == 443 {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}:{p}")
    }
}

fn as_f64(v: &serde_json::Value) -> Option<f64> {
    match v {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// PURE: a DMS object `{degree, minute, second}` → signed decimal degrees. Sign follows `degree`
/// (including -0), matching the app's fromDms.
fn from_dms(v: &serde_json::Value) -> Option<f64> {
    let d = as_f64(v.get("degree")?)?;
    let m = v.get("minute").and_then(as_f64).unwrap_or(0.0);
    let s = v.get("second").and_then(as_f64).unwrap_or(0.0);
    let sign = if d < 0.0 || (d == 0.0 && d.is_sign_negative()) {
        -1.0
    } else {
        1.0
    };
    Some(sign * (d.abs() + m.abs() / 60.0 + s.abs() / 3600.0))
}

/// PURE: reject an impossible or placeholder fix (the (0,0) several firmwares emit before a lock).
fn valid_fix(lat: Option<f64>, lon: Option<f64>, acc: Option<f64>) -> Option<GpsFix> {
    let (lat, lon) = (lat?, lon?);
    if lat.abs() > 90.0 || lon.abs() > 180.0 {
        return None;
    }
    if lat == 0.0 && lon == 0.0 {
        return None;
    }
    Some(GpsFix {
        lat,
        lon,
        acc: acc.filter(|a| *a >= 0.0),
    })
}

/// PURE: parse a Cradlepoint NCOS `/api/status/gps` body into a fix. Accepts every shape the app
/// accepts: the fix under `data.fix`, `fix`, `data`, or the root; lat/lon as decimals OR as DMS
/// objects. Returns None for "reachable but no lock" so the caller reports nothing rather than a lie.
pub fn parse_cradlepoint_gps(body: &serde_json::Value) -> Option<GpsFix> {
    let fix = body
        .get("data")
        .and_then(|d| d.get("fix"))
        .or_else(|| body.get("fix"))
        .or_else(|| body.get("data"))
        .unwrap_or(body);
    if !fix.is_object() {
        return None;
    }
    let lat_v = fix.get("latitude");
    let (lat, lon) = if lat_v.map(|v| v.is_object()).unwrap_or(false) {
        (
            lat_v.and_then(from_dms),
            fix.get("longitude").and_then(from_dms),
        )
    } else {
        (
            fix.get("latitude")
                .or_else(|| fix.get("lat"))
                .and_then(as_f64),
            fix.get("longitude")
                .or_else(|| fix.get("lon"))
                .or_else(|| fix.get("lng"))
                .and_then(as_f64),
        )
    };
    valid_fix(lat, lon, fix.get("accuracy").and_then(as_f64))
}

#[derive(Deserialize)]
struct Ignore {}

/// Fetch one fix from a Cradlepoint. `Err` carries a short reason (unreachable, auth, or no lock)
/// suited to a log line — never the credential.
pub async fn poll_cradlepoint(
    client: &reqwest::Client,
    host: &str,
    port: u16,
    user: &str,
    pass: &str,
) -> Result<GpsFix, String> {
    let url = format!("{}/api/status/gps", cradlepoint_base(host, port));
    let res = client
        .get(&url)
        .basic_auth(user, Some(pass))
        .send()
        .await
        .map_err(|e| e.without_url().to_string())?;
    let code = res.status().as_u16();
    if code == 401 || code == 403 {
        return Err(
            "router refused the sign-in (check the GPS device's admin username/password)".into(),
        );
    }
    if !res.status().is_success() {
        // Parse-and-ignore keeps the type import honest without leaking a body that may echo the URL.
        let _: Result<Ignore, _> = res.json().await;
        return Err(format!("router returned HTTP {code}"));
    }
    let body: serde_json::Value = res.json().await.map_err(|e| e.without_url().to_string())?;
    parse_cradlepoint_gps(&body)
        .ok_or_else(|| "reachable, but no GPS lock yet (check the antenna / NCOS GPS)".into())
}

// ---- NMEA 0183 over the network -----------------------------------------------------------------
// A chartplotter, AIS receiver, gpsd, or a router forwarding its receiver's sentences on TCP/UDP
// (owner, 2026-09-12: "it's whether the hub can reach the device, not the browser"). The parser is
// a port of the app's gpsSources.ts one, so a feed that worked from the desktop app works from the
// hub; the transports are poll-shaped like the Cradlepoint driver — connect (or listen), take the
// first valid fix within a short window, report, close — rather than a held-open stream, because
// the poll loop's cadence is the product's cadence and a stream would just be dropped fixes.

/// One poll listens this long for sentences. Receivers emit RMC/GGA at least once a second; eight
/// seconds tolerates a slow plotter and a UDP feed's gaps without holding the loop hostage.
pub const NMEA_WINDOW: std::time::Duration = std::time::Duration::from_secs(8);
/// NMEA's conventional TCP/UDP port (OpenCPN, gpsd forwarders, most plotters default to it).
pub const NMEA_DEFAULT_PORT: u16 = 10110;

/// XOR of every byte between `$` and `*`, against the two hex digits after `*`. A sentence with no
/// checksum is ACCEPTED — it is optional in the wild and some forwarders strip it.
pub fn nmea_checksum_ok(line: &str) -> bool {
    if !line.starts_with('$') {
        return false;
    }
    let Some(star) = line.rfind('*') else {
        return true;
    };
    let sum = line.as_bytes()[1..star].iter().fold(0u8, |a, b| a ^ b);
    match line
        .get(star + 1..star + 3)
        .map(|h| u8::from_str_radix(h, 16))
    {
        Some(Ok(want)) => want == sum,
        _ => false,
    }
}

/// `ddmm.mmmm` (or `dddmm.mmmm`) plus a hemisphere letter → signed decimal degrees.
fn from_nmea_coord(raw: &str, hemi: &str) -> Option<f64> {
    let v: f64 = raw.trim().parse().ok()?;
    if hemi.is_empty() {
        return None;
    }
    let deg = (v / 100.0).floor();
    let min = v - deg * 100.0;
    if min >= 60.0 {
        return None;
    }
    let dd = deg + min / 60.0;
    Some(if hemi == "S" || hemi == "W" { -dd } else { dd })
}

/// The latest valid fix in a blob of sentences: RMC preferred (the LAST valid one — freshest), GGA
/// as the fallback, carrying HDOP × 5 m as a rough accuracy. Talker-agnostic (GP/GN/GL…). Void
/// RMC (`V`), GGA with no fix quality, and sentences that fail their checksum are skipped.
pub fn parse_nmea(text: &str) -> Option<GpsFix> {
    let mut rmc: Option<GpsFix> = None;
    let mut gga: Option<GpsFix> = None;
    for raw in text.split(['\r', '\n']) {
        let line = raw.trim();
        if !line.starts_with('$') || !nmea_checksum_ok(line) {
            continue;
        }
        let body = match line.rfind('*') {
            Some(i) if i > 0 => &line[1..i],
            _ => &line[1..],
        };
        let f: Vec<&str> = body.split(',').collect();
        let g = |i: usize| f.get(i).copied().unwrap_or("");
        let typ = &f[0][f[0].len().saturating_sub(3)..];
        if typ == "RMC" && g(2) == "A" {
            if let Some(fix) = valid_fix(
                from_nmea_coord(g(3), g(4)),
                from_nmea_coord(g(5), g(6)),
                None,
            ) {
                rmc = Some(fix);
            }
        } else if typ == "GGA" {
            let quality: f64 = g(6).trim().parse().unwrap_or(0.0);
            if quality > 0.0 {
                let hdop: Option<f64> = g(8).trim().parse().ok();
                if let Some(fix) = valid_fix(
                    from_nmea_coord(g(2), g(3)),
                    from_nmea_coord(g(4), g(5)),
                    hdop.map(|h| h * 5.0),
                ) {
                    gga = Some(fix);
                }
            }
        }
    }
    rmc.or(gga)
}

/// Only whole lines are parsed: a sentence cut mid-number by a read boundary must never yield a
/// wrong position. Everything up to and including the last line terminator.
fn complete_lines(text: &str) -> &str {
    match text.rfind(['\r', '\n']) {
        Some(i) => &text[..=i],
        None => "",
    }
}

/// Poll an NMEA feed. `port` 0 ⇒ 10110. `protocol` "udp" listens for datagrams on that port (the
/// feed is pointed at this hub); anything else connects to `host:port` as a TCP client.
pub async fn poll_nmea(host: &str, port: u16, protocol: &str) -> Result<GpsFix, String> {
    let port = if port == 0 { NMEA_DEFAULT_PORT } else { port };
    if protocol.eq_ignore_ascii_case("udp") {
        let sock = tokio::net::UdpSocket::bind(("0.0.0.0", port))
            .await
            .map_err(|e| format!("cannot listen on UDP port {port} ({e})"))?;
        poll_nmea_udp_on(sock).await
    } else {
        poll_nmea_tcp(host, port).await
    }
}

async fn poll_nmea_tcp(host: &str, port: u16) -> Result<GpsFix, String> {
    use tokio::io::AsyncReadExt;
    let mut stream = match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::net::TcpStream::connect((host, port)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(format!("{host}:{port} refused the connection ({e})")),
        Err(_) => return Err(format!("{host}:{port} did not answer within 5 s")),
    };
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + NMEA_WINDOW;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, stream.read(&mut chunk)).await {
            Ok(Ok(0)) => break, // the peer closed
            Ok(Ok(n)) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(fix) = parse_nmea(complete_lines(&String::from_utf8_lossy(&buf))) {
                    return Ok(fix);
                }
                if buf.len() > 64 * 1024 {
                    buf.drain(..32 * 1024);
                }
            }
            Ok(Err(e)) => return Err(format!("reading from {host}:{port} failed ({e})")),
            Err(_) => break, // the window elapsed
        }
    }
    if buf.is_empty() {
        Err(format!(
            "connected to {host}:{port} but no NMEA sentences arrived in {} s",
            NMEA_WINDOW.as_secs()
        ))
    } else {
        Err(format!(
            "{host}:{port} sends NMEA but no valid fix yet (receiver still acquiring?)"
        ))
    }
}

/// Split from `poll_nmea` so a test can bind an ephemeral port and know where to send.
async fn poll_nmea_udp_on(sock: tokio::net::UdpSocket) -> Result<GpsFix, String> {
    let port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
    let mut buf = vec![0u8; 4096];
    let mut text = String::new();
    let deadline = tokio::time::Instant::now() + NMEA_WINDOW;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => {
                text.push_str(&String::from_utf8_lossy(&buf[..n]));
                text.push('\n'); // a datagram is a whole sentence (or several); terminate it
                if let Some(fix) = parse_nmea(&text) {
                    return Ok(fix);
                }
                if text.len() > 64 * 1024 {
                    text.drain(..32 * 1024);
                }
            }
            Ok(Err(e)) => return Err(format!("receiving on UDP port {port} failed ({e})")),
            Err(_) => break,
        }
    }
    if text.is_empty() {
        Err(format!("no NMEA datagrams arrived on UDP port {port} in {} s (is the feed pointed at this hub?)", NMEA_WINDOW.as_secs()))
    } else {
        Err(format!("the UDP feed on port {port} sends NMEA but no valid fix yet (receiver still acquiring?)"))
    }
}

#[cfg(test)]
mod nmea_tests {
    use super::*;

    /// `$<body>*HH` with the checksum computed, so every fixture is a legal sentence.
    fn sentence(body: &str) -> String {
        let sum = body.bytes().fold(0u8, |a, b| a ^ b);
        format!("${body}*{sum:02X}")
    }

    const RMC_BODY: &str = "GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W";
    const GGA_BODY: &str = "GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,";

    #[test]
    fn checksum_accepts_a_good_sentence_rejects_a_bad_one_and_tolerates_none() {
        assert!(nmea_checksum_ok(&sentence(RMC_BODY)));
        assert!(!nmea_checksum_ok(&format!("${RMC_BODY}*00")));
        assert!(
            nmea_checksum_ok(&format!("${RMC_BODY}")),
            "checksum is optional in the wild"
        );
        assert!(!nmea_checksum_ok("GPRMC,no,dollar"));
    }

    #[test]
    fn rmc_parses_to_signed_decimal_degrees() {
        let fix = parse_nmea(&sentence(RMC_BODY)).unwrap();
        assert!((fix.lat - 48.1173).abs() < 1e-4, "{}", fix.lat);
        assert!((fix.lon - 11.5167).abs() < 1e-4, "{}", fix.lon);
        assert_eq!(fix.acc, None, "RMC carries no accuracy");
        // Southern / western hemispheres are negative.
        let sw = sentence("GNRMC,123519,A,3352.129,S,15112.558,W,0.0,0.0,230394,,");
        let f = parse_nmea(&sw).unwrap();
        assert!(f.lat < 0.0 && f.lon < 0.0);
    }

    #[test]
    fn gga_is_the_fallback_and_carries_hdop_as_accuracy() {
        let fix = parse_nmea(&sentence(GGA_BODY)).unwrap();
        assert!((fix.lat - 48.1173).abs() < 1e-4);
        assert_eq!(fix.acc, Some(4.5), "hdop 0.9 x 5 m");
        // RMC wins over GGA when both are present, whatever the order.
        let both = format!("{}\r\n{}\r\n", sentence(GGA_BODY), sentence(RMC_BODY));
        assert_eq!(parse_nmea(&both).unwrap().acc, None);
        let both = format!("{}\r\n{}\r\n", sentence(RMC_BODY), sentence(GGA_BODY));
        assert_eq!(parse_nmea(&both).unwrap().acc, None);
    }

    #[test]
    fn void_rmc_no_fix_gga_bad_checksum_and_junk_are_skipped() {
        let void = sentence("GPRMC,123519,V,4807.038,N,01131.000,E,,,230394,,");
        assert_eq!(parse_nmea(&void), None);
        let nofix = sentence("GPGGA,123519,4807.038,N,01131.000,E,0,00,,,M,,M,,");
        assert_eq!(parse_nmea(&nofix), None);
        assert_eq!(parse_nmea(&format!("${RMC_BODY}*00")), None, "bad checksum");
        assert_eq!(parse_nmea("hello\r\n$GPZDA,x\r\n\r\n"), None);
        // The LAST valid RMC is the freshest.
        let older = sentence("GPRMC,123518,A,4807.000,N,01131.000,E,0,0,230394,,");
        let newer = sentence(RMC_BODY);
        let f = parse_nmea(&format!("{older}\n{newer}\n")).unwrap();
        assert!((f.lat - 48.1173).abs() < 1e-4);
    }

    #[test]
    fn only_complete_lines_are_parsed() {
        // A sentence cut mid-number must not produce a position.
        let cut = &sentence(RMC_BODY)[..30];
        assert_eq!(complete_lines(cut), "");
        let whole = format!("{}\r\n{}", sentence(RMC_BODY), cut);
        assert_eq!(
            complete_lines(&whole),
            format!("{}\r\n", sentence(RMC_BODY))
        );
    }

    #[tokio::test]
    async fn tcp_poll_takes_the_first_valid_fix_from_a_live_feed() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            // Junk, then a void sentence, then a real one — split across writes mid-sentence.
            s.write_all(b"garbage\r\n").await.unwrap();
            s.write_all(sentence("GPRMC,1,V,,,,,,,,,").as_bytes())
                .await
                .unwrap();
            s.write_all(b"\r\n").await.unwrap();
            let good = sentence(RMC_BODY);
            let (a, b) = good.split_at(20);
            s.write_all(a.as_bytes()).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            s.write_all(b.as_bytes()).await.unwrap();
            s.write_all(b"\r\n").await.unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(2)).await; // keep it open like a plotter would
        });
        let fix = poll_nmea("127.0.0.1", port, "tcp").await.unwrap();
        assert!((fix.lat - 48.1173).abs() < 1e-4);
    }

    #[tokio::test]
    async fn tcp_poll_reports_a_feed_with_no_fix_and_a_closed_port() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            s.write_all(format!("{}\r\n", sentence("GPRMC,1,V,,,,,,,,,")).as_bytes())
                .await
                .unwrap();
            // then close: the poll must not wait the whole window
        });
        let err = poll_nmea("127.0.0.1", port, "tcp").await.unwrap_err();
        assert!(err.contains("no valid fix yet"), "{err}");
        // Nothing listening: a refusal, quickly.
        let err = poll_nmea("127.0.0.1", 1, "tcp").await.unwrap_err();
        assert!(
            err.contains("refused") || err.contains("did not answer"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn udp_poll_listens_and_takes_a_datagram_fix() {
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = sock.local_addr().unwrap();
        tokio::spawn(async move {
            let tx = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            tx.send_to(b"junk", addr).await.unwrap();
            tx.send_to(sentence(GGA_BODY).as_bytes(), addr)
                .await
                .unwrap();
        });
        let fix = poll_nmea_udp_on(sock).await.unwrap();
        assert_eq!(fix.acc, Some(4.5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_base_url_is_https_on_443_and_http_otherwise() {
        assert_eq!(
            cradlepoint_base("192.168.10.1", 443),
            "https://192.168.10.1"
        );
        assert_eq!(
            cradlepoint_base("192.168.10.1", 0),
            "https://192.168.10.1",
            "unset defaults to 443"
        );
        assert_eq!(cradlepoint_base("192.168.10.1", 80), "http://192.168.10.1");
        assert_eq!(cradlepoint_base("10.0.0.5", 8080), "http://10.0.0.5:8080");
    }

    #[test]
    fn parses_a_decimal_fix() {
        let b = json!({ "data": { "fix": { "latitude": 41.4086, "longitude": -81.7494, "accuracy": 12.5 } } });
        assert_eq!(
            parse_cradlepoint_gps(&b),
            Some(GpsFix {
                lat: 41.4086,
                lon: -81.7494,
                acc: Some(12.5)
            })
        );
    }

    #[test]
    fn parses_a_dms_fix_with_a_negative_degree() {
        // 81°44'57.8"W ⇒ negative longitude, sign carried by the degree.
        let b = json!({ "fix": {
            "latitude":  { "degree": 41, "minute": 24, "second": 30.0 },
            "longitude": { "degree": -81, "minute": 44, "second": 57.8 },
        }});
        let f = parse_cradlepoint_gps(&b).unwrap();
        assert!((f.lat - 41.40833).abs() < 1e-4);
        assert!((f.lon + 81.74939).abs() < 1e-4, "west is negative");
    }

    #[test]
    fn rejects_the_zero_zero_no_lock_placeholder_and_out_of_range() {
        assert_eq!(
            parse_cradlepoint_gps(&json!({ "fix": { "latitude": 0, "longitude": 0 } })),
            None
        );
        assert_eq!(
            parse_cradlepoint_gps(&json!({ "fix": { "latitude": 99, "longitude": 10 } })),
            None
        );
        assert_eq!(parse_cradlepoint_gps(&json!({ "stat": "fail" })), None);
    }
}
