//! The web app, served by the hub — "like Plex" (owner, 2026-09-12).
//!
//! The hub does not COMPILE the web app in. It fetches the signed bundle the App release publishes
//! (`dockneighbor-web.tar.gz` + `.sig`, App release.yml `release-web-bundle`), verifies it against
//! the desktop updater's public key embedded below, unpacks it under `<data>/web/<version>/`, and
//! serves it at `/` (hub_server `h_web`). Fetched rather than embedded so a web change never needs a
//! daemon release — every daemon self-update restarts the hub and pushes offline/online alerts.
//!
//! TRUST: the `.sig` is a minisign signature made with `TAURI_SIGNING_PRIVATE_KEY`, the key the
//! desktop updater already refuses to update without. A bundle that does not verify is never
//! unpacked; the hub keeps serving what it has and says why in the log. The public key is a
//! compile-time constant on purpose — a config-settable key would let whoever can write the config
//! choose what code every browser on the boat runs.
//!
//! LAYOUT: `<data>/web/<version>/…` per bundle, `<data>/web/current` a one-line marker naming the
//! version in service. Both writes are tmp-then-rename, so a crash mid-install leaves the previous
//! bundle in service and a `.tmp` directory to clean, never a half-written one being served.
//!
//! WHAT THE SPA IS TOLD: `index.html` is served with one line injected before `</head>` —
//! `window.__DN_HUB_SERVED__ = true` — the only way the app learns it is running from a hub, so its
//! hub calls can take the same-origin LAN door instead of the cloud relay (App: hubClient.ts).

use std::path::{Path, PathBuf};

/// The App repo's `releases/latest` — followed for its redirect, which names the version. The same
/// no-token, no-rate-limit trick update_check.rs uses for the daemon's own releases.
pub const APP_LATEST_RELEASE_URL: &str =
    "https://github.com/DockNeighbor/DockNeighbor-App/releases/latest";
pub const APP_LATEST_DOWNLOAD_BASE: &str =
    "https://github.com/DockNeighbor/DockNeighbor-App/releases/latest/download";
pub const BUNDLE_ASSET: &str = "dockneighbor-web.tar.gz";
pub const SIG_ASSET: &str = "dockneighbor-web.tar.gz.sig";

/// The desktop updater's minisign public key — `plugins.updater.pubkey` in the App's
/// tauri.conf.json, base64-decoded to its second line (key id 731FC192A3DDC5E9). A rotated updater
/// key means a new daemon release, by design: see the module doc.
pub const WEB_PUBKEY_B64: &str = "RWTpxd2jksEfc3YhDIFkKlrsKG473aKYq27gkZfU+yzUfxgeUI3D7hBn";

/// A web bundle is a few MB; a "bundle" of 64 MB is not one. Refused before it is held in memory.
pub const MAX_BUNDLE_BYTES: usize = 64 * 1024 * 1024;

/// Injected into `index.html` before `</head>` when the hub serves it.
pub const HUB_SERVED_SCRIPT: &str = "<script>window.__DN_HUB_SERVED__=true</script>";

/// The bundle in service: where its files are and its `index.html` as served (already injected).
#[derive(Clone, Debug)]
pub struct WebBundle {
    pub version: String,
    pub dir: PathBuf,
    pub index: String,
}

pub fn web_root(base: &Path) -> PathBuf {
    base.join("web")
}

fn marker_path(base: &Path) -> PathBuf {
    web_root(base).join("current")
}

/// `…/releases/tag/v1.0.104` → `1.0.104`. Only the plain `v` family: the App repo tags releases
/// that way, and anything else (a `daemon-v` tag would be the wrong repo entirely) returns None.
pub fn app_version_from_release_url(url: &str) -> Option<String> {
    let tag = url.rsplit("/tag/").next().filter(|t| *t != url)?;
    let ver = tag.strip_prefix('v')?;
    crate::update_check::parse_version(ver).map(|_| ver.to_string())
}

/// `tauri signer sign` writes the minisign signature file BASE64-ENCODED (that is what the updater
/// manifest carries); a hand-made `.sig` is the plain text. Accept both: if the content decodes to
/// something that starts like a minisign file, that is the signature.
pub fn normalize_sig(text: &str) -> String {
    use base64::Engine as _;
    let trimmed = text.trim();
    if trimmed.starts_with("untrusted comment:") {
        return trimmed.to_string();
    }
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(trimmed) {
        if let Ok(s) = String::from_utf8(bytes) {
            if s.trim_start().starts_with("untrusted comment:") {
                return s.trim().to_string();
            }
        }
    }
    trimmed.to_string()
}

/// Does `bytes` carry a signature made with the embedded key? Prehashed (the `ED` algorithm Tauri
/// uses) and legacy signatures are both accepted; the library decides from the signature itself.
pub fn verify(bytes: &[u8], sig_text: &str) -> Result<(), String> {
    let pk = minisign_verify::PublicKey::from_base64(WEB_PUBKEY_B64)
        .map_err(|e| format!("embedded public key is unreadable: {e}"))?;
    let sig = minisign_verify::Signature::decode(&normalize_sig(sig_text))
        .map_err(|e| format!("signature is unreadable: {e}"))?;
    pk.verify(bytes, &sig, true)
        .map_err(|e| format!("signature does not verify against the updater key: {e}"))
}

/// `index.html` as the hub serves it: told that it is hub-served. Idempotent.
pub fn inject_hub_served(index: &str) -> String {
    if index.contains("__DN_HUB_SERVED__") {
        return index.to_string();
    }
    match index.find("</head>") {
        Some(i) => format!("{}{}{}", &index[..i], HUB_SERVED_SCRIPT, &index[i..]),
        None => format!("{HUB_SERVED_SCRIPT}{index}"),
    }
}

/// The bundle the marker names, if its files are there. None ⇒ the hub has nothing to serve yet.
pub fn load_current(base: &Path) -> Option<WebBundle> {
    let version = std::fs::read_to_string(marker_path(base))
        .ok()?
        .trim()
        .to_string();
    if version.is_empty() || version.contains('/') || version.contains("..") {
        return None;
    }
    let dir = web_root(base).join(&version);
    let index = std::fs::read_to_string(dir.join("index.html")).ok()?;
    Some(WebBundle {
        version,
        dir,
        index: inject_hub_served(&index),
    })
}

fn write_marker(base: &Path, version: &str) -> Result<(), String> {
    let root = web_root(base);
    let tmp = root.join("current.tmp");
    std::fs::write(&tmp, version).map_err(|e| format!("write marker: {e}"))?;
    std::fs::rename(&tmp, marker_path(base)).map_err(|e| format!("commit marker: {e}"))
}

/// Unpack a VERIFIED `.tar.gz` into `<web>/<version>` and make it current. Tmp-then-rename for the
/// directory and the marker both. The tar crate refuses entries that escape the target directory.
pub fn install(base: &Path, version: &str, targz: &[u8]) -> Result<WebBundle, String> {
    if version.is_empty() || version.contains('/') || version.contains("..") {
        return Err(format!("refusing bundle version {version:?}"));
    }
    let root = web_root(base);
    std::fs::create_dir_all(&root).map_err(|e| format!("create {}: {e}", root.display()))?;
    let tmp = root.join(format!("{version}.tmp"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
    let gz = flate2::read::GzDecoder::new(targz);
    let mut archive = tar::Archive::new(gz);
    if let Err(e) = archive.unpack(&tmp) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("unpack: {e}"));
    }
    if !tmp.join("index.html").is_file() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err("the archive has no index.html at its root — not a web bundle".into());
    }
    let dest = root.join(version);
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::rename(&tmp, &dest).map_err(|e| format!("commit {}: {e}", dest.display()))?;
    write_marker(base, version)?;
    load_current(base).ok_or_else(|| "the installed bundle did not load back".to_string())
}

/// MIME type for a served file, by extension. Anything unknown is a download, never sniffed.
pub fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") | Some("map") => "application/json; charset=utf-8",
        Some("webmanifest") => "application/manifest+json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        _ => "application/octet-stream",
    }
}

/// The file a request path names inside the bundle, or None (⇒ the SPA's `index.html`). PURE apart
/// from the final `is_file`. Refuses anything that could step outside the bundle directory: `..`,
/// empty segments, backslashes, and dot-files (nothing in a Vite build starts with a dot).
pub fn resolve(dir: &Path, req_path: &str) -> Option<PathBuf> {
    let rel = req_path.trim_start_matches('/');
    if rel.is_empty() {
        return None;
    }
    if rel
        .split('/')
        .any(|seg| seg.is_empty() || seg == ".." || seg.starts_with('.') || seg.contains('\\'))
    {
        return None;
    }
    let p = dir.join(rel);
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// Bring the served bundle up to the latest App release. `Ok(None)` = nothing changed (already
/// current, or no newer release); `Ok(Some)` = a new bundle is in service; `Err` = why not (the
/// caller logs it and keeps serving what it has).
pub async fn refresh(
    client: &reqwest::Client,
    base: &Path,
    current: Option<&str>,
) -> Result<Option<WebBundle>, String> {
    let res = client
        .get(APP_LATEST_RELEASE_URL)
        .send()
        .await
        .map_err(|e| format!("releases/latest: {e}"))?;
    let latest = app_version_from_release_url(res.url().as_str()).ok_or_else(|| {
        format!(
            "releases/latest did not resolve to a v* tag ({})",
            res.url()
        )
    })?;
    if current == Some(latest.as_str()) {
        return Ok(None);
    }
    // Already unpacked from an earlier run whose marker did not commit? Just point at it.
    if web_root(base).join(&latest).join("index.html").is_file() {
        write_marker(base, &latest)?;
        return Ok(load_current(base));
    }
    let sig_res = client
        .get(format!("{APP_LATEST_DOWNLOAD_BASE}/{SIG_ASSET}"))
        .send()
        .await
        .map_err(|e| format!("{SIG_ASSET}: {e}"))?;
    if !sig_res.status().is_success() {
        return Err(format!(
            "the latest app release ({latest}) has no signed web bundle ({SIG_ASSET} -> HTTP {})",
            sig_res.status()
        ));
    }
    let sig = sig_res
        .text()
        .await
        .map_err(|e| format!("{SIG_ASSET}: {e}"))?;
    let bundle_res = client
        .get(format!("{APP_LATEST_DOWNLOAD_BASE}/{BUNDLE_ASSET}"))
        .send()
        .await
        .map_err(|e| format!("{BUNDLE_ASSET}: {e}"))?;
    if !bundle_res.status().is_success() {
        return Err(format!("{BUNDLE_ASSET} -> HTTP {}", bundle_res.status()));
    }
    if let Some(len) = bundle_res.content_length() {
        if len as usize > MAX_BUNDLE_BYTES {
            return Err(format!(
                "{BUNDLE_ASSET} is {len} bytes — refusing anything over {MAX_BUNDLE_BYTES}"
            ));
        }
    }
    let bytes = bundle_res
        .bytes()
        .await
        .map_err(|e| format!("{BUNDLE_ASSET}: {e}"))?;
    if bytes.len() > MAX_BUNDLE_BYTES {
        return Err(format!(
            "{BUNDLE_ASSET} is {} bytes — refusing anything over {MAX_BUNDLE_BYTES}",
            bytes.len()
        ));
    }
    verify(&bytes, &sig)?;
    install(base, &latest, &bytes).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_base(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("brvg-web-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A tiny tar.gz with `index.html` and `assets/app.js` at the root, built in memory.
    fn bundle(files: &[(&str, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let gz = flate2::write::GzEncoder::new(&mut out, flate2::Compression::fast());
            let mut ar = tar::Builder::new(gz);
            for (name, body) in files {
                let mut h = tar::Header::new_gnu();
                h.set_size(body.len() as u64);
                h.set_mode(0o644);
                h.set_cksum();
                ar.append_data(&mut h, name, body.as_bytes()).unwrap();
            }
            ar.into_inner().unwrap().finish().unwrap();
        }
        out
    }

    #[test]
    fn app_release_url_names_the_version_only_for_v_tags() {
        assert_eq!(
            app_version_from_release_url(
                "https://github.com/DockNeighbor/DockNeighbor-App/releases/tag/v1.0.104"
            )
            .as_deref(),
            Some("1.0.104")
        );
        assert_eq!(
            app_version_from_release_url("https://github.com/x/y/releases/tag/daemon-v0.3.37"),
            None
        );
        assert_eq!(
            app_version_from_release_url("https://github.com/x/y/releases/latest"),
            None
        );
        assert_eq!(
            app_version_from_release_url("https://github.com/x/y/releases/tag/vnext"),
            None
        );
    }

    #[test]
    fn a_signature_by_someone_else_is_refused_and_the_key_is_readable() {
        // A syntactically valid minisign signature made with a DIFFERENT key must fail to verify —
        // and fail at verification, not at decoding, which proves the embedded key parses.
        let other = "untrusted comment: signature from minisign secret key\n\
RUQ4+IyBbFuG3Vx4M2FvGGl8Z6cP8p0k7wCkgv9oYbmLgW5bmmpvV+qSOs+eNc7GUB43a0wSMWfEGAB6mPs0+VDo0eE7rZhcXAw=\n\
trusted comment: timestamp:1700000000\tfile:x\n\
aM7v2gFAeh0k7wCkgv9oYbmLgW5bmmpvV+qSOs+eNc7GUB43a0wSMWfEGAB6mPs0+VDo0eE7rZhcXAw+IyBbFuG3Vx4M2FvGGl8Z6cA=\n";
        let err = verify(b"hello", other).unwrap_err();
        assert!(
            err.contains("does not verify") || err.contains("unreadable"),
            "{err}"
        );
        assert!(
            !err.contains("embedded public key"),
            "the embedded key itself must parse: {err}"
        );
        assert!(minisign_verify::PublicKey::from_base64(WEB_PUBKEY_B64).is_ok());
    }

    #[test]
    fn normalize_sig_accepts_tauris_base64_wrapping_and_plain_text() {
        use base64::Engine as _;
        let plain = "untrusted comment: x\nAAAA\ntrusted comment: y\nBBBB";
        assert_eq!(normalize_sig(plain), plain);
        let wrapped = base64::engine::general_purpose::STANDARD.encode(plain);
        assert_eq!(normalize_sig(&wrapped), plain);
        assert_eq!(normalize_sig("garbage"), "garbage");
    }

    #[test]
    fn inject_tells_the_spa_it_is_hub_served_once() {
        let once = inject_hub_served("<html><head><title>x</title></head><body></body></html>");
        assert!(once.contains(&format!("{HUB_SERVED_SCRIPT}</head>")));
        assert_eq!(inject_hub_served(&once), once, "idempotent");
        assert!(inject_hub_served("no head here").starts_with(HUB_SERVED_SCRIPT));
    }

    #[test]
    fn install_unpacks_marks_current_and_loads_back() {
        let base = temp_base("install");
        let b = install(
            &base,
            "1.0.104",
            &bundle(&[("index.html", "<head></head>"), ("assets/app.js", "1")]),
        )
        .unwrap();
        assert_eq!(b.version, "1.0.104");
        assert!(b.index.contains("__DN_HUB_SERVED__"));
        assert!(b.dir.join("assets/app.js").is_file());
        assert_eq!(
            std::fs::read_to_string(web_root(&base).join("current")).unwrap(),
            "1.0.104"
        );
        assert_eq!(load_current(&base).unwrap().version, "1.0.104");
        // A second install replaces the current pointer; the old bundle's files stay on disk.
        install(
            &base,
            "1.0.105",
            &bundle(&[("index.html", "<head></head>")]),
        )
        .unwrap();
        assert_eq!(load_current(&base).unwrap().version, "1.0.105");
        assert!(web_root(&base).join("1.0.104/index.html").is_file());
    }

    #[test]
    fn install_refuses_an_archive_that_is_not_a_web_bundle() {
        let base = temp_base("notweb");
        let err = install(&base, "1.0.1", &bundle(&[("readme.txt", "hi")])).unwrap_err();
        assert!(err.contains("no index.html"), "{err}");
        assert!(load_current(&base).is_none(), "nothing was made current");
        assert!(
            !web_root(&base).join("1.0.1.tmp").exists(),
            "the tmp directory is cleaned up"
        );
    }

    #[test]
    fn resolve_serves_files_and_refuses_traversal() {
        let base = temp_base("resolve");
        let b = install(
            &base,
            "1.0.0",
            &bundle(&[("index.html", "<head></head>"), ("assets/a.js", "1")]),
        )
        .unwrap();
        assert!(resolve(&b.dir, "/assets/a.js").is_some());
        assert!(resolve(&b.dir, "/").is_none(), "root is the SPA");
        assert!(
            resolve(&b.dir, "/settings").is_none(),
            "an app route is the SPA"
        );
        assert!(resolve(&b.dir, "/assets/missing.js").is_none());
        assert!(resolve(&b.dir, "/../current").is_none());
        assert!(resolve(&b.dir, "/assets/../../current").is_none());
        assert!(resolve(&b.dir, "/assets//a.js").is_none());
        assert!(resolve(&b.dir, "/.hidden").is_none());
        assert!(resolve(&b.dir, "/assets\\a.js").is_none());
    }

    #[test]
    fn content_types_cover_a_vite_build() {
        assert_eq!(
            content_type(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("assets/index-abc.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("assets/index-abc.css")),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("manifest.webmanifest")),
            "application/manifest+json"
        );
        assert_eq!(
            content_type(Path::new("sw.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("x.unknown")),
            "application/octet-stream"
        );
    }
}
