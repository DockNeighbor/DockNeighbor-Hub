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
    let sign = if d < 0.0 || (d == 0.0 && d.is_sign_negative()) { -1.0 } else { 1.0 };
    Some(sign * (d.abs() + m.abs() / 60.0 + s.abs() / 3600.0))
}

/// PURE: reject an impossible or placeholder fix (the (0,0) several firmwares emit before a lock).
fn valid_fix(lat: Option<f64>, lon: Option<f64>, acc: Option<f64>) -> Option<GpsFix> {
    let (lat, lon) = (lat?, lon?);
    if lat.abs() > 90.0 || lon.abs() > 180.0 { return None; }
    if lat == 0.0 && lon == 0.0 { return None; }
    Some(GpsFix { lat, lon, acc: acc.filter(|a| *a >= 0.0) })
}

/// PURE: parse a Cradlepoint NCOS `/api/status/gps` body into a fix. Accepts every shape the app
/// accepts: the fix under `data.fix`, `fix`, `data`, or the root; lat/lon as decimals OR as DMS
/// objects. Returns None for "reachable but no lock" so the caller reports nothing rather than a lie.
pub fn parse_cradlepoint_gps(body: &serde_json::Value) -> Option<GpsFix> {
    let fix = body.get("data").and_then(|d| d.get("fix"))
        .or_else(|| body.get("fix"))
        .or_else(|| body.get("data"))
        .unwrap_or(body);
    if !fix.is_object() { return None; }
    let lat_v = fix.get("latitude");
    let (lat, lon) = if lat_v.map(|v| v.is_object()).unwrap_or(false) {
        (lat_v.and_then(from_dms), fix.get("longitude").and_then(from_dms))
    } else {
        (
            fix.get("latitude").or_else(|| fix.get("lat")).and_then(as_f64),
            fix.get("longitude").or_else(|| fix.get("lon")).or_else(|| fix.get("lng")).and_then(as_f64),
        )
    };
    valid_fix(lat, lon, fix.get("accuracy").and_then(as_f64))
}

#[derive(Deserialize)]
struct Ignore {}

/// Fetch one fix from a Cradlepoint. `Err` carries a short reason (unreachable, auth, or no lock)
/// suited to a log line — never the credential.
pub async fn poll_cradlepoint(
    client: &reqwest::Client, host: &str, port: u16, user: &str, pass: &str,
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
        return Err("router refused the sign-in (check the GPS device's admin username/password)".into());
    }
    if !res.status().is_success() {
        // Parse-and-ignore keeps the type import honest without leaking a body that may echo the URL.
        let _: Result<Ignore, _> = res.json().await;
        return Err(format!("router returned HTTP {code}"));
    }
    let body: serde_json::Value = res.json().await.map_err(|e| e.without_url().to_string())?;
    parse_cradlepoint_gps(&body).ok_or_else(|| "reachable, but no GPS lock yet (check the antenna / NCOS GPS)".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_base_url_is_https_on_443_and_http_otherwise() {
        assert_eq!(cradlepoint_base("192.168.10.1", 443), "https://192.168.10.1");
        assert_eq!(cradlepoint_base("192.168.10.1", 0), "https://192.168.10.1", "unset defaults to 443");
        assert_eq!(cradlepoint_base("192.168.10.1", 80), "http://192.168.10.1");
        assert_eq!(cradlepoint_base("10.0.0.5", 8080), "http://10.0.0.5:8080");
    }

    #[test]
    fn parses_a_decimal_fix() {
        let b = json!({ "data": { "fix": { "latitude": 41.4086, "longitude": -81.7494, "accuracy": 12.5 } } });
        assert_eq!(parse_cradlepoint_gps(&b), Some(GpsFix { lat: 41.4086, lon: -81.7494, acc: Some(12.5) }));
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
        assert_eq!(parse_cradlepoint_gps(&json!({ "fix": { "latitude": 0, "longitude": 0 } })), None);
        assert_eq!(parse_cradlepoint_gps(&json!({ "fix": { "latitude": 99, "longitude": 10 } })), None);
        assert_eq!(parse_cradlepoint_gps(&json!({ "stat": "fail" })), None);
    }
}
