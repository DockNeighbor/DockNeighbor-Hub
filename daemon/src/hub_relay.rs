// The hub's OUTBOUND WebSocket to the worker (owner, 2026-08-18: "it will also have a websocket to
// BRVG so a remote app can also control it").
//
// The LAN management API in hub_server.rs only answers people who are aboard. A boat sits behind
// marina NAT with no inbound route and no stable address, so the hub DIALS OUT and holds a socket
// to the worker; a remote app's authenticated call is relayed down it. Nothing is forwarded, nothing
// new listens on the public internet, and the connection is only ever established by the hub.
//
// Two kinds of frame come down:
//   * `keys` — the vehicle's member-key set, pushed by the worker. The hub applies it immediately,
//     so a member who was just added (or a hub that just reconnected) does not wait out the
//     five-minute HTTP poll. The poll stays as the backstop when the socket is down.
//   * `call` — one relayed management call, carrying the uid and role the WORKER authenticated.
//     It goes through the same `dispatch` as a LAN call, so both doors obey identical rules; the
//     hub re-applies its own role gates rather than trusting that the worker checked.
//
// The socket is authenticated by the hub's own device token, in the query string — the same
// credential its telemetry uses. That means the URL is a secret, so nothing here ever logs it, and
// anything that might carry it is redacted before it reaches a log line.

use std::future::Future;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio_tungstenite::tungstenite::Message;

use crate::hub_config::{self, HubConfig, MemberKey};
use crate::hub_server::{dispatch, Answer, Caller, Shared};

/// Longest wait between reconnection attempts. A hub that cannot reach the cloud has nothing better
/// to do than keep trying, but a boat on a metered cellular link should not retry in a tight loop.
const MAX_BACKOFF_SECS: u64 = 60;

/// 🔴 EVERY NETWORK AWAIT IN THIS FILE HAS A DEADLINE, because one without cost a real vehicle its
/// remote control for ~12 hours, 2026-09-16. CENTRAL's hub.log: `relay socket failed: … (os error
/// 10054)` at 13:06, then NOTHING — no further `relay socket failed`, no `relay connected` — until the
/// service was restarted that night. The reconnect in `run` had called `connect_async` with no timeout,
/// and on the boat's flaky WAN a TCP/TLS/WebSocket handshake stalled and never returned, so the loop
/// was parked inside one attempt forever: no log, no retry, and every remote valve command answered
/// "No hub took that command" while the 15-minute telemetry (a separate HTTP path) kept working. The
/// 35s silence check could not help — it only runs once a connection has reached the select loop.
///
/// A timeout is an ordinary `Err`, so `run` logs it and backs off exactly as for any other failure.
///
/// Connect covers DNS, TCP, TLS and the WebSocket upgrade together. 30s is far beyond a healthy
/// handshake even on cellular, and short enough that a wedged attempt costs one backoff step, not a day.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// One frame write (send + flush). A write that cannot complete in this long is into a dead or wedged
/// peer; ending the connection is what gets us a fresh one.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// The longest the socket loop waits on one relayed call before answering 504 and going back to
/// serving the socket. The worker gives up on a call after 15s (hubRelay.ts RELAY_TIMEOUT_MS), so a
/// later answer is read by nobody — and while a call is awaited, no ping is sent and no frame is read.
/// Kept under SILENCE_LIMIT so a slow call cannot, by itself, make a healthy socket look silent.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Await `fut`, but give up after `limit`: its own error, or `"<what> timed out after Ns"`.
pub async fn within<T, E: ToString>(
    limit: Duration,
    what: &str,
    fut: impl Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r.map_err(|e| e.to_string()),
        Err(_) => Err(format!("{what} timed out after {}s", limit.as_secs_f32())),
    }
}

type RelaySocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Open the socket — DNS, TCP, TLS and the WebSocket handshake — within `limit`.
async fn connect(url: &str, limit: Duration) -> Result<RelaySocket, String> {
    let (socket, _resp) = within(limit, "connect", tokio_tungstenite::connect_async(url)).await?;
    Ok(socket)
}

/// Run one relayed call without letting it hold the socket loop hostage. The call is SPAWNED rather
/// than awaited in place so that, on timeout, it is detached and left to finish — not cancelled
/// halfway through a gateway command or a config write. Its late answer is dropped; the worker has
/// long since answered the caller.
async fn answer_within<F>(limit: Duration, call: F) -> Answer
where
    F: Future<Output = Answer> + Send + 'static,
{
    match tokio::time::timeout(limit, tokio::spawn(call)).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(join)) => Answer {
            status: 500,
            body: serde_json::json!({ "error": format!("the hub call failed: {join}") }).to_string(),
        },
        Err(_) => {
            crate::hlog!("hub: relayed call still running after {}s; answering 504", limit.as_secs());
            Answer {
                status: 504,
                body: serde_json::json!({ "error": "the hub did not finish that call in time" }).to_string(),
            }
        }
    }
}

/// PURE: the socket URL for this hub. `https` → `wss` so one pinned base serves both.
pub fn relay_socket_url(worker_base: &str, cfg: &HubConfig) -> Result<String, String> {
    let base = worker_base.trim_end_matches('/');
    let mut u = url::Url::parse(&format!("{base}/api/hub/ws")).map_err(|e| e.to_string())?;
    let scheme = match u.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => return Err(format!("unsupported worker scheme {other}")),
    };
    u.set_scheme(scheme).map_err(|_| "could not set the websocket scheme".to_string())?;
    u.query_pairs_mut()
        .append_pair("vid", &cfg.vid)
        .append_pair("device", &cfg.hub_id)
        .append_pair("t", &cfg.token);
    Ok(u.to_string())
}

/// PURE: exponential backoff, capped. Attempt 0 is the first retry.
pub fn backoff_secs(attempt: u32) -> u64 {
    let secs = 1u64.checked_shl(attempt.min(16)).unwrap_or(MAX_BACKOFF_SECS);
    secs.min(MAX_BACKOFF_SECS)
}

/// A connection that lived at least this long WAS a connection, not a failing handshake.
pub const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(60);

/// PURE: the attempt counter for the NEXT reconnect, from how the last one ended and how long the
/// connection it served lived.
///
/// 🔴 THE COUNTER NEVER RESET ON A FAILURE (D9, owner ruling 2026-09-24). `run` zeroed `attempt`
/// only on a CLEAN close, so after the first few failures the hub sat permanently at the 60 s cap:
/// a socket that had been up for two hours and was then reset by the edge cost ~61 s of "no hub"
/// before the hub even tried again. CENTRAL logged 78 resets between 2026-09-19 and 2026-09-24 —
/// over an hour of windows in which a valve command answered "no hub took that command", on a
/// vessel whose web app has NO LAN path to the hub.
///
/// Backoff exists to stop a hub hammering a cloud that will not have it. A connection that lived a
/// full minute is the opposite of that evidence, so the next failure starts the schedule over.
pub fn next_attempt(attempt: u32, lived: Duration, clean_close: bool) -> u32 {
    if clean_close || lived >= BACKOFF_RESET_AFTER {
        0
    } else {
        attempt.saturating_add(1)
    }
}

/// PURE: strip a secret out of anything on its way to a log. The socket URL carries the hub token,
/// and a transport error that quotes the URL would otherwise write the credential to disk.
pub fn redact(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        return text.to_string();
    }
    text.replace(secret, "…redacted…")
}

/// What the worker can say. Anything else is IGNORED rather than guessed at — this end is talking
/// to a service we trust, but a bug there must not become undefined behaviour here.
#[derive(Debug, PartialEq)]
pub enum WorkerMessage {
    Keys(Vec<MemberKey>),
    /// The worker's answer to our application ping. A pong with NO `n` is the auto-response's, or an
    /// older worker's: its arrival is TEXT, which still counts for the any-TEXT liveness rule, but it
    /// proves only that the edge is there. A pong that ECHOES the nonce we sent could only have come
    /// from the object's own `webSocketMessage` — that is the one that proves the object is running.
    /// See `nonce_ping_frame` and the heartbeat in `serve_once`.
    Pong { n: Option<String> },
    Call {
        id: String,
        uid: String,
        role: String,
        method: String,
        path: String,
        body: String,
    },
}

#[derive(Deserialize)]
struct RawFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    keys: Option<Vec<MemberKey>>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    uid: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    body: Option<String>,
    /// The nonce echoed back on a `pong` (`nonce_ping_frame`). Absent on the auto-response's pong
    /// and on every worker older than Cloud #—wan-state-log.
    #[serde(default)]
    n: Option<String>,
}

/// PURE: read one frame from the worker.
pub fn parse_worker_message(raw: &str) -> Option<WorkerMessage> {
    let f: RawFrame = serde_json::from_str(raw).ok()?;
    match f.kind.as_str() {
        "keys" => Some(WorkerMessage::Keys(f.keys.unwrap_or_default())),
        "pong" => Some(WorkerMessage::Pong { n: f.n.filter(|s| !s.is_empty()) }),
        "call" => {
            let id = f.id.filter(|s| !s.is_empty())?;
            // A call with no caller is not answerable: every action here is role-gated, and a
            // blank role would fall through to "denied" anyway. Refuse it at the parse instead,
            // so it can never be mistaken for an anonymous-but-valid request.
            let uid = f.uid.filter(|s| !s.is_empty())?;
            let role = f.role.filter(|s| !s.is_empty())?;
            Some(WorkerMessage::Call {
                id,
                uid,
                role,
                method: f.method.unwrap_or_default(),
                path: f.path.unwrap_or_default(),
                body: f.body.unwrap_or_default(),
            })
        }
        _ => None,
    }
}

/// PURE: the frame that answers one relayed call.
pub fn result_frame(id: &str, answer: &Answer) -> String {
    serde_json::json!({
        "type": "result",
        "id": id,
        "status": answer.status,
        "body": answer.body,
    })
    .to_string()
}

/// PURE: the application heartbeat. A TEXT frame, deliberately — a control-frame PING is answered by
/// the Cloudflare edge for a hibernating object, so it would prove the edge is reachable, not that
/// the object still holds this socket.
///
/// ⚠️ THESE BYTES ARE PINNED. The worker hands exactly this string to `setWebSocketAutoResponse`
/// (DockNeighbor-Cloud hubRelay.ts `APP_PING_FRAME`), and a drift check compares the two, so the
/// 10 s heartbeat must stay byte-for-byte `{"type":"ping"}`. It is NOT, on its own, proof the
/// object is running any more — see `nonce_ping_frame`.
fn app_ping_frame() -> String {
    r#"{"type":"ping"}"#.to_string()
}

/// PURE: the NONCE heartbeat — the one frame the runtime's auto-response cannot answer.
///
/// 🔴 D15, the third turn of the half-open screw (owner ruling 2026-09-24). Cloud #476 installed
/// `setWebSocketAutoResponse` with a request/response pair BYTE-IDENTICAL to `app_ping_frame()`
/// and its pong. That is a real cost saving, and it also means the Cloudflare RUNTIME answers the
/// 10 s ping WITHOUT the Durable Object running — while `serve_once` below counts any TEXT frame
/// as proof of life. So the plain ping has quietly become, again, a test of the edge rather than
/// of the object: the very thing the 2026-08-31 and 2026-09-09 outages were about.
///
/// A ping carrying a random `n` cannot equal the auto-response's request string, so it falls
/// through to the object's `webSocketMessage`, which echoes `{"type":"pong","n":…}`. ONLY an echo
/// whose `n` matches the nonce we sent counts for the nonce silence limit; the edge cannot forge
/// one, because it does not know the nonce and has no rule that produces it.
pub fn nonce_ping_frame(nonce: &str) -> String {
    format!(r#"{{"type":"ping","n":"{nonce}"}}"#)
}

/// PURE: a nonce. Unguessable is not the point — UNMATCHABLE by a static auto-response pair is, so
/// any value that varies per ping does the job. Base-36 of the clock plus a per-connection counter,
/// so two pings in the same millisecond still differ.
///
/// ⚠️ THE SHAPE IS THE WORKER'S, NOT OURS. `hubLink.ts` echoes `n` back only when it matches
/// `^[A-Za-z0-9]{1,64}$`; anything else is refused and the worker falls back to a BARE
/// `{"type":"pong"}`. That degrades silently — the socket keeps answering, `nonce_echoed` never
/// becomes true, and the liveness proof quietly stops proving anything. So: plain ASCII
/// alphanumerics, comfortably inside the cap. `NONCE_MAX_CHARS` and the test below pin it.
pub const NONCE_MAX_CHARS: usize = 32;

pub fn next_nonce(counter: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut v = now ^ counter.rotate_left(29) ^ 0x9E37_79B9_7F4A_7C15;
    let mut out = String::with_capacity(13);
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    for _ in 0..12 {
        out.push(ALPHABET[(v % 36) as usize] as char);
        v /= 36;
    }
    out
}

/// PURE. `lanIps` + `webVersion` + `httpPort` are how the cloud learns WHERE this hub is and
/// whether it hosts the web app — the link-out to the local copy (App banner) is built from these.
fn hello_frame(cfg: &HubConfig, lan_ips: &[String], web_version: Option<&str>) -> String {
    serde_json::json!({
        "type": "hello",
        "hubId": cfg.hub_id,
        "version": env!("CARGO_PKG_VERSION"),
        "lanIps": lan_ips,
        "webVersion": web_version,
        "httpPort": cfg.http_port,
    })
    .to_string()
}

/// Apply a pushed key set: live for the running server, and persisted so a reboot with no internet
/// still authenticates known members.
async fn apply_keys(rt: &Shared, keys: Vec<MemberKey>) {
    // Sign the pushed set ourselves (key_sync::member_set_sig is the cloud's own algorithm), so the
    // NEXT payload reply's `keysSig` agrees with what we hold and costs no fetch. A push is also
    // proof of freshness: it restarts the daily safety clock.
    let sig = crate::key_sync::member_set_sig(&keys);
    rt.key_sync.lock().await.note_pushed(sig.clone(), crate::hub_server::now_ms());
    *rt.keys.write().await = keys.clone();
    let _g = rt.store.lock().await;
    let mut cfg = hub_config::read_config_in(&rt.base);
    cfg.member_keys = keys;
    cfg.member_keys_sig = sig;
    if let Err(e) = hub_config::write_config_in(&rt.base, &cfg) {
        crate::hlog!("hub: could not persist pushed member keys: {e}");
    }
}

/// Connect, serve, reconnect — forever. Returns only if the hub is not registered, which cannot
/// happen while the caller holds it in a loop.
pub async fn run(rt: Shared) {
    let mut attempt: u32 = 0;
    loop {
        let cfg = hub_config::read_config_in(&rt.base);
        if cfg.token.is_empty() || cfg.vid.is_empty() || cfg.hub_id.is_empty() {
            // Not registered yet. The bootstrap seed may land at any moment, so wait and re-read
            // rather than exiting — the same reasoning as the heartbeat loop.
            tokio::time::sleep(Duration::from_secs(30)).await;
            continue;
        }
        let opened = tokio::time::Instant::now();
        let outcome = serve_once(&rt, &cfg).await;
        let lived = opened.elapsed();
        match &outcome {
            Ok(()) => crate::hlog!("hub: relay socket closed; reconnecting"),
            Err(e) => crate::hlog!("hub: relay socket failed after {}s: {}", lived.as_secs(), redact(e, &cfg.token)),
        }
        attempt = next_attempt(attempt, lived, outcome.is_ok());
        tokio::time::sleep(Duration::from_secs(backoff_secs(attempt))).await;
    }
}

/// How often to ping the worker, and how long silence may last before we call the socket dead.
///
/// 🔴 A HALF-OPEN RELAY SOCKET COST A REAL VEHICLE ITS REMOTE CONTROL, 2026-08-31. The hub logged
/// `relay connected` and then sat in `socket.next().await` for five hours while the worker's side
/// had no socket at all — every relayed call answered 503 "no hub is connected". The hub had no way
/// to notice: it ANSWERED pings and never SENT one, so nothing ever asked the connection a question
/// it could fail to answer. A TCP connection that nobody writes to can stay "open" indefinitely.
///
/// 🔴 THE FIRST FIX WAS INCOMPLETE, and the same failure recurred 2026-09-09. Sending a ping is not
/// enough if it is a CONTROL-frame PING: the Durable Object hibernates between messages, and while it
/// sleeps the Cloudflare EDGE answers control PINGs with PONGs itself — the object is never woken and
/// never consulted. So the hub kept "seeing pongs" and reading `connected` while the object had again
/// lost the socket, and a 1:00 PM valve command got "no hub took that command". The real question a
/// heartbeat must ask is "is the OBJECT there", not "is the edge there". So the ping is now an
/// APPLICATION message (TEXT `{"type":"ping"}`) that the object answers with a TEXT `pong`, and ONLY
/// a TEXT frame counts as proof of life — a control PONG no longer does. TEXT wakes the object; the
/// edge cannot forge the reply. See app_ping_frame, the `Message::Text` liveness rule, and
/// brvg-cloud-server/src/hubLink.ts::webSocketMessage.
///
/// Three missed pings before giving up: one lost frame on a marina's Wi-Fi is not a dead socket, and
/// reconnecting on every hiccup would be its own outage.
///
/// 🔴 TIGHTENED 2026-09-09 (30s/95s -> 10s/35s) because 95s of detection latency was a real product
/// failure: the cloud relay edge resets this socket every 20 min-2 hr (Cloudflare recycling the
/// hibernated connection — see brvg-cloud-server/src/hubLink.ts), the hub reconnected fine, but a
/// valve command routed through the relay in that up-to-95s window got "no hub took that command."
/// The web app has NO LAN path to the hub (browser mixed-content blocks http://hub from an https
/// page), so it depends entirely on this socket. Faster pings catch a reset on the next ping WRITE
/// (the os-error-10054 case) within ~10s instead of ~30, and shorten the silent-drop ceiling to
/// ~35s. Still three intervals of grace, so a single lost frame is not mistaken for a dead peer —
/// the pairing the test below enforces. The worker also now waits briefly for a reconnect before it
/// answers "no hub", so a command arriving mid-reconnect rides over the gap instead of failing.
const PING_EVERY: Duration = Duration::from_secs(10);
const SILENCE_LIMIT: Duration = Duration::from_secs(35);

/// HOW OFTEN THE NONCE PING GOES OUT, and how many echoes may be missed before the socket is dead.
///
/// ⚠️ THE CADENCE IS THE CLOUD SESSION'S CALL — these two constants are the whole knob, deliberately
/// named and deliberately adjacent, so changing the number is a one-line edit with no other
/// consequence. The cloud session proposed every 5 minutes (≈11 minutes to detect a half-open
/// socket). This ships 60 s with 3 missed echoes (≈3 minutes) because the path being kept alive is
/// a VALVE COMMAND path and 11 minutes of "no hub took that command" is a product failure, not a
/// cost question: a nonce ping is one inbound WebSocket message, which Cloudflare bills at 1/20th
/// of a request, so 60 s costs on the order of a rounding error per hub per day.
///
/// Three missed echoes, not one, for the same reason as SILENCE_LIMIT: a single lost frame on a
/// marina's Wi-Fi is not a dead socket, and reconnecting on every hiccup would be its own outage.
pub const NONCE_PING_EVERY: Duration = Duration::from_secs(60);
pub const NONCE_MISSES_ALLOWED: u32 = 3;
/// The silence the nonce heartbeat allows: NONCE_MISSES_ALLOWED intervals.
pub const NONCE_SILENCE_LIMIT: Duration = Duration::from_secs(NONCE_PING_EVERY.as_secs() * NONCE_MISSES_ALLOWED as u64);

/// PURE: has the peer gone silent long enough to call the socket dead?
///
/// Split out because the rule lives inside a `select!` arm, which no test can reach — and a
/// liveness check nothing verifies is how the original "answer pings, never send one" survived.
pub fn relay_is_silent(silent_for: Duration, limit: Duration) -> bool {
    silent_for > limit
}

/// PURE: has the OBJECT gone silent, as opposed to the edge? (D15.)
///
/// BACKWARD COMPATIBLE IN BOTH DIRECTIONS, and this predicate is where that lives:
///   * Against a worker that NEVER echoes a nonce (any worker before the wan-state-log release),
///     `echoed` stays false for the whole connection and this rule never fires. The hub falls back
///     to exactly today's behaviour — the 10 s ping and the 35 s any-TEXT silence limit — so an old
///     worker costs nothing but the stronger check.
///   * Against the new worker, the first echo proves it speaks nonces, and from then on the
///     connection is held to it: a socket whose pings are being answered only by the runtime stops
///     producing echoes, and ~3 minutes later the hub reconnects. A 0.3.53 daemon, which never
///     sends `n`, is unaffected: the new worker answers its plain ping exactly as before.
pub fn nonce_is_silent(echoed: bool, since_last_echo: Duration, limit: Duration) -> bool {
    echoed && since_last_echo > limit
}

/// One connection's lifetime. `Ok` means a clean close; `Err` carries a reason worth backing off for.
async fn serve_once(rt: &Shared, cfg: &HubConfig) -> Result<(), String> {
    let url = relay_socket_url(&rt.worker_base, cfg)?;
    let socket = connect(&url, CONNECT_TIMEOUT).await?;
    crate::hlog!("hub: relay connected");
    // Split so the ping timer can write while the read half is parked on `next()`. Without this the
    // two borrows collide and the whole liveness check is impossible to express.
    let (mut write, mut read) = socket.split();
    let lan_ips = crate::linktap_discover::local_ipv4s();
    let web_version = rt.web.read().await.as_ref().map(|w| w.version.clone());
    within(WRITE_TIMEOUT, "hello write", write.send(Message::Text(hello_frame(cfg, &lan_ips, web_version.as_deref()))))
        .await?;

    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // the first tick fires immediately; we want a full interval of grace
    let mut last_seen = tokio::time::Instant::now();

    // The nonce heartbeat (D15) — a second, slower timer beside the 10 s one. `nonce_echoed` is the
    // back-compat switch: until this worker has echoed once, the nonce limit is not enforced.
    let mut nonce_ping = tokio::time::interval(NONCE_PING_EVERY);
    nonce_ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    nonce_ping.tick().await;
    let mut nonce_seq: u64 = 0;
    let mut pending_nonce: Option<String> = None;
    let mut nonce_echoed = false;
    let mut last_nonce_echo = tokio::time::Instant::now();

    loop {
        tokio::select! {
            frame = read.next() => {
                let Some(frame) = frame else { return Ok(()) }; // the stream ended cleanly
                let frame = frame.map_err(|e| e.to_string())?;
                let text = match frame {
                    // Only a TEXT frame is proof of life. A control-frame PONG is NOT — the Cloudflare
                    // edge answers our control PINGs itself while the object hibernates, so counting
                    // one would recreate the exact half-open outage this heartbeat exists to prevent.
                    // TEXT (keys / call / pong) can only come from the live object on the far end.
                    Message::Text(t) => { last_seen = tokio::time::Instant::now(); t }
                    Message::Ping(p) => {
                        // Answer for transport hygiene, but do NOT treat it as liveness.
                        within(WRITE_TIMEOUT, "pong write", write.send(Message::Pong(p))).await?;
                        continue;
                    }
                    Message::Close(_) => return Ok(()),
                    // Binary / control-frame Pong / raw frames carry nothing this protocol reads and,
                    // deliberately, do not count as liveness.
                    _ => continue,
                };
                let Some(msg) = parse_worker_message(&text) else { continue };
                match msg {
                    WorkerMessage::Pong { n } => {
                        // Any TEXT already counted for `last_seen` above. A pong that echoes the
                        // nonce we last sent is the stronger fact: the OBJECT ran. From the first
                        // such echo this connection is held to the nonce limit as well.
                        if n.is_some() && n == pending_nonce {
                            nonce_echoed = true;
                            last_nonce_echo = tokio::time::Instant::now();
                            pending_nonce = None;
                        }
                    }
                    WorkerMessage::Keys(keys) => {
                        crate::hlog!("hub: member keys pushed ({})", keys.len());
                        apply_keys(rt, keys).await;
                    }
                    WorkerMessage::Call { id, uid, role, method, path, body } => {
                        let caller = Caller { uid, role };
                        let call_rt = rt.clone();
                        let answer = answer_within(CALL_TIMEOUT, async move {
                            dispatch(&call_rt, &caller, &method, &path, body.as_bytes()).await
                        })
                        .await;
                        within(WRITE_TIMEOUT, "result write", write.send(Message::Text(result_frame(&id, &answer))))
                            .await?;
                    }
                }
            }
            _ = ping.tick() => {
                // ⚠️ CHECK BEFORE SENDING. If the peer has gone silent, another ping into the void
                // proves nothing; returning Err drops us into the reconnect backoff, which is the
                // only thing that actually restores remote control.
                if last_seen.elapsed() > SILENCE_LIMIT {
                    return Err(format!(
                        "relay went silent for {}s - treating the socket as dead and reconnecting",
                        last_seen.elapsed().as_secs()
                    ));
                }
                // A TEXT ping, not a control-frame one: the object must answer it with a `pong`, and
                // a hibernating object's control PINGs are answered by the edge instead — see
                // app_ping_frame and the `Message::Text` liveness rule above.
                within(WRITE_TIMEOUT, "ping write", write.send(Message::Text(app_ping_frame()))).await?;
            }
            _ = nonce_ping.tick() => {
                // ⚠️ CHECK BEFORE SENDING, for the same reason as above. This is the check the plain
                // ping can no longer make: the runtime auto-answers `{"type":"ping"}` without the
                // object, so only a matching nonce echo says the object is still there.
                if nonce_is_silent(nonce_echoed, last_nonce_echo.elapsed(), NONCE_SILENCE_LIMIT) {
                    return Err(format!(
                        "the relay object stopped echoing nonce pings for {}s - treating the socket as dead and reconnecting",
                        last_nonce_echo.elapsed().as_secs()
                    ));
                }
                nonce_seq = nonce_seq.wrapping_add(1);
                let n = next_nonce(nonce_seq);
                within(WRITE_TIMEOUT, "nonce ping write", write.send(Message::Text(nonce_ping_frame(&n)))).await?;
                pending_nonce = Some(n);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> HubConfig {
        HubConfig {
            hub_id: "hub_abc".into(), vid: "v1".into(), name: "Central".into(),
            enabled: true, heartbeat_secs: 60, token: "hubtok-secret".into(),
            ..HubConfig::default()
        }
    }

    #[test]
    fn the_socket_url_upgrades_the_scheme_and_carries_the_hub_identity() {
        let raw = relay_socket_url("https://api.example/", &cfg()).unwrap();
        assert!(raw.starts_with("wss://"), "{raw}");
        let u = url::Url::parse(&raw).unwrap();
        assert_eq!(u.path(), "/api/hub/ws");
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["vid"], "v1");
        assert_eq!(q["device"], "hub_abc");
        assert_eq!(q["t"], "hubtok-secret");
        // Plain http (a local worker in development) maps to ws, not wss.
        assert!(relay_socket_url("http://127.0.0.1:8787", &cfg()).unwrap().starts_with("ws://"));
        assert!(relay_socket_url("ftp://nope", &cfg()).is_err());
    }

    #[test]
    fn the_token_never_reaches_a_log_line() {
        let leaked = format!("connect failed: {}", relay_socket_url("https://api.example", &cfg()).unwrap());
        let safe = redact(&leaked, &cfg().token);
        assert!(!safe.contains("hubtok-secret"), "{safe}");
        assert!(safe.contains("…redacted…"));
        // An empty secret must not turn every log line into redaction soup.
        assert_eq!(redact("plain", ""), "plain");
    }

    #[test]
    fn backoff_climbs_and_then_stops_climbing() {
        assert_eq!(backoff_secs(0), 1);
        assert_eq!(backoff_secs(1), 2);
        assert_eq!(backoff_secs(4), 16);
        assert_eq!(backoff_secs(6), MAX_BACKOFF_SECS); // 64 → capped
        assert_eq!(backoff_secs(u32::MAX), MAX_BACKOFF_SECS); // no overflow, no zero-length wait
    }

    /// D9 — THE COUNTER NEVER RESET ON A FAILURE, so a hub that had been up for hours paid the 60 s
    /// cap on the reset that followed. CENTRAL: 78 resets in five days, each one a ~61 s NO_HUB window.
    #[test]
    fn a_connection_that_really_lived_starts_the_backoff_over() {
        // A run of failing handshakes still climbs — that is what backoff is for.
        let mut attempt = 0;
        for expected in [1, 2, 3, 4] {
            attempt = next_attempt(attempt, Duration::from_secs(2), false);
            assert_eq!(attempt, expected);
        }
        assert_eq!(backoff_secs(attempt), 16, "four failed handshakes in a row: back off");

        // 🔴 And then a connection that LIVED. The next failure starts at one second, not sixty.
        let after = next_attempt(attempt, BACKOFF_RESET_AFTER, false);
        assert_eq!(after, 0);
        assert_eq!(backoff_secs(after), 1, "a reset after a real connection costs a second, not a minute");
        assert_eq!(next_attempt(9, Duration::from_secs(7200), false), 0, "two hours, then an edge reset");

        // The boundary, and the case it must not swallow: a handshake that wedges just under it.
        assert_eq!(next_attempt(5, BACKOFF_RESET_AFTER - Duration::from_millis(1), false), 6, "59.999 s is not a connection");
        assert_eq!(next_attempt(5, Duration::ZERO, false), 6);
        // A clean close has always reset, and still does, however short it was.
        assert_eq!(next_attempt(5, Duration::ZERO, true), 0);
    }

    /// D15 — the nonce heartbeat. The plain ping is auto-answered by the Cloudflare runtime, so it
    /// no longer proves the object is running; only an echoed nonce does.
    #[test]
    fn a_nonce_ping_cannot_be_answered_by_the_auto_response_pair() {
        let n = next_nonce(1);
        let frame = nonce_ping_frame(&n);
        // 🔴 The auto-response compares the request string BYTE FOR BYTE (hubRelay.ts APP_PING_FRAME).
        assert_ne!(frame, app_ping_frame(), "a nonce ping must never equal the auto-answered frame");
        assert_eq!(app_ping_frame(), r#"{"type":"ping"}"#, "and the plain ping's bytes are pinned by a drift check");
        // It is still a ping the worker's own parser reads, and it carries the nonce.
        let parsed: serde_json::Value = serde_json::from_str(&frame).expect("valid JSON");
        assert_eq!(parsed["type"], "ping");
        assert_eq!(parsed["n"], n.as_str());
        // 🔴 The worker echoes `n` ONLY when it matches ^[A-Za-z0-9]{1,64}$ and otherwise answers a
        // BARE pong — which would leave `nonce_echoed` false forever and silently un-prove liveness.
        for c in [1u64, 2, 7, 4096, u64::MAX] {
            let n = next_nonce(c);
            assert!(!n.is_empty(), "an empty nonce is no nonce");
            assert!(n.chars().count() <= NONCE_MAX_CHARS, "inside the worker's 64-char cap: {n}");
            assert!(n.chars().all(|c| c.is_ascii_alphanumeric()), "the worker's accepted shape: {n}");
        }
        assert_ne!(next_nonce(2), next_nonce(3), "a repeated nonce would be a static frame again");
    }

    #[test]
    fn only_an_echo_of_the_nonce_we_sent_counts_as_the_objects_proof_of_life() {
        assert_eq!(parse_worker_message(r#"{"type":"pong","n":"abc123"}"#), Some(WorkerMessage::Pong { n: Some("abc123".into()) }));
        assert_eq!(parse_worker_message(r#"{"type":"pong","n":""}"#), Some(WorkerMessage::Pong { n: None }), "an empty echo is no echo");
        assert_eq!(parse_worker_message(r#"{"type":"pong"}"#), Some(WorkerMessage::Pong { n: None }), "the auto-response's pong");

        // The rule the select! arm applies, and the back-compat switch inside it.
        let limit = NONCE_SILENCE_LIMIT;
        assert!(!nonce_is_silent(false, Duration::from_secs(86_400), limit), "a worker that never echoed is never held to it");
        assert!(!nonce_is_silent(true, limit, limit), "exactly at the limit is still alive");
        assert!(nonce_is_silent(true, limit + Duration::from_secs(1), limit));
        // The cadence pair, stated: three missed echoes, ~3 minutes to notice a half-open socket.
        assert_eq!(NONCE_PING_EVERY, Duration::from_secs(60));
        assert_eq!(NONCE_MISSES_ALLOWED, 3);
        assert_eq!(NONCE_SILENCE_LIMIT, NONCE_PING_EVERY * NONCE_MISSES_ALLOWED, "the limit IS the misses, not a second number");
        assert!(NONCE_SILENCE_LIMIT > NONCE_PING_EVERY, "one lost frame is not a dead socket");
    }

    #[test]
    fn a_pushed_key_set_is_read_including_an_empty_one() {
        assert_eq!(
            parse_worker_message(r#"{"type":"keys","keys":[{"key":"k","uid":"u","role":"owner"}]}"#),
            Some(WorkerMessage::Keys(vec![MemberKey { key: "k".into(), uid: "u".into(), role: "owner".into() }])),
        );
        // "trust nobody" is a legitimate instruction — it must not read as "no message".
        assert_eq!(parse_worker_message(r#"{"type":"keys","keys":[]}"#), Some(WorkerMessage::Keys(vec![])));
        assert_eq!(parse_worker_message(r#"{"type":"keys"}"#), Some(WorkerMessage::Keys(vec![])));
    }

    #[test]
    fn the_worker_pong_is_read_as_proof_of_life() {
        // The frame whose ARRIVAL is the whole point: only the live object sends it, so it, not a
        // control-frame PONG, is what the liveness check trusts. Losing this parse would silently
        // reopen the half-open outage — a pong would fall through to "unknown frame" and be ignored.
        assert_eq!(parse_worker_message(r#"{"type":"pong"}"#), Some(WorkerMessage::Pong { n: None }));
    }

    #[test]
    fn a_relayed_call_carries_the_caller_the_worker_authenticated() {
        let msg = parse_worker_message(
            r#"{"type":"call","id":"r1","uid":"u1","role":"coowner","method":"POST","path":"/api/hub/config","body":"{\"name\":\"x\"}"}"#,
        );
        assert_eq!(msg, Some(WorkerMessage::Call {
            id: "r1".into(), uid: "u1".into(), role: "coowner".into(),
            method: "POST".into(), path: "/api/hub/config".into(), body: "{\"name\":\"x\"}".into(),
        }));
    }

    #[test]
    fn a_call_with_no_caller_is_refused_rather_than_treated_as_anonymous() {
        for raw in [
            r#"{"type":"call","id":"r1","role":"owner","method":"GET","path":"/api/hub/status"}"#,
            r#"{"type":"call","id":"r1","uid":"","role":"owner","method":"GET","path":"/api/hub/status"}"#,
            r#"{"type":"call","id":"r1","uid":"u1","method":"GET","path":"/api/hub/status"}"#,
            r#"{"type":"call","id":"r1","uid":"u1","role":"","method":"GET","path":"/api/hub/status"}"#,
            r#"{"type":"call","uid":"u1","role":"owner","method":"GET","path":"/api/hub/status"}"#,
        ] {
            assert_eq!(parse_worker_message(raw), None, "{raw}");
        }
    }

    #[test]
    fn frames_we_do_not_understand_are_ignored() {
        for raw in ["not json", "", "[]", "null", r#"{"type":"result","id":"r"}"#, r#"{"type":"whatever"}"#] {
            assert_eq!(parse_worker_message(raw), None, "{raw}");
        }
    }

    /// END TO END over a real socket: connect → hello → a pushed key set is applied and persisted
    /// → relayed calls are dispatched through the SAME core the LAN door uses, role gates included.
    /// A stub worker stands in for the real one; everything on the hub side is production code.
    #[tokio::test]
    async fn a_relayed_call_runs_through_the_same_gates_as_a_lan_call() {
        use crate::hub_server::new_rt;
        use tokio_tungstenite::tungstenite::Message as M;

        // --- the stub worker -------------------------------------------------------------------
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(frame)) = ws.next().await {
                let M::Text(t) = frame else { continue };
                let v: serde_json::Value = serde_json::from_str(&t).unwrap();
                if v["type"] == "hello" {
                    tx.send(v).unwrap();
                    // Push a key set, then three calls: one a monitor may not make, one they may,
                    // and one path that is not the hub's at all.
                    ws.send(M::Text(r#"{"type":"keys","keys":[{"key":"k-mon","uid":"u-mon","role":"monitor"}]}"#.into())).await.unwrap();
                    ws.send(M::Text(r#"{"type":"call","id":"c1","uid":"u-mon","role":"monitor","method":"POST","path":"/api/hub/config","body":"{\"name\":\"Hacked\"}"}"#.into())).await.unwrap();
                    ws.send(M::Text(r#"{"type":"call","id":"c2","uid":"u-own","role":"owner","method":"POST","path":"/api/hub/config","body":"{\"name\":\"Boat PC\"}"}"#.into())).await.unwrap();
                    ws.send(M::Text(r#"{"type":"call","id":"c3","uid":"u-own","role":"owner","method":"GET","path":"/etc/passwd"}"#.into())).await.unwrap();
                } else {
                    tx.send(v).unwrap();
                }
            }
        });

        // --- the hub ---------------------------------------------------------------------------
        let base = temp_base("relay");
        hub_config::write_config_in(&base, &cfg()).unwrap();
        let rt = new_rt(base.clone(), format!("http://{addr}"));
        let serving = tokio::spawn({
            let rt = rt.clone();
            async move { let _ = serve_once(&rt, &cfg()).await; }
        });

        async fn next(
            rx: &mut tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
        ) -> serde_json::Value {
            tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("timed out").unwrap()
        }

        let hello = next(&mut rx).await;
        assert_eq!(hello["hubId"], "hub_abc");

        let c1 = next(&mut rx).await;
        assert_eq!(c1["id"], "c1");
        assert_eq!(c1["status"], 403, "a monitor must not be able to reconfigure the hub");
        assert!(c1["body"].as_str().unwrap().contains("admin"));

        let c2 = next(&mut rx).await;
        assert_eq!(c2["id"], "c2");
        assert_eq!(c2["status"], 200);
        // The write really happened, through the same store the LAN door writes.
        assert_eq!(hub_config::read_config_in(&base).name, "Boat PC");

        let c3 = next(&mut rx).await;
        assert_eq!(c3["status"], 404, "the relay reaches only the hub's own endpoints");

        // The pushed keys are live AND persisted, so a reboot with no internet still knows them.
        assert_eq!(rt.keys.read().await.len(), 1);
        assert_eq!(hub_config::read_config_in(&base).member_keys[0].uid, "u-mon");

        serving.abort();
        let _ = std::fs::remove_dir_all(&base);
    }

    fn temp_base(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("brvg-hub-relay-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_result_frame_is_what_the_worker_correlates_on() {
        let f = result_frame("r1", &Answer { status: 403, body: r#"{"error":"nope"}"#.into() });
        let v: serde_json::Value = serde_json::from_str(&f).unwrap();
        assert_eq!(v["type"], "result");
        assert_eq!(v["id"], "r1");
        assert_eq!(v["status"], 403);
        assert_eq!(v["body"], r#"{"error":"nope"}"#);
        // The worker refuses a non-numeric status, so this must never be stringified.
        assert!(v["status"].is_number());
    }

    #[test]
    fn a_silent_relay_is_treated_as_dead_but_a_hiccup_is_not() {
        // 🔴 The hub answered pings and never sent one, so `socket.next()` parked forever on a
        // half-open connection — five hours of "relay connected" while the worker had no socket at
        // all and every remote call answered 503.
        assert!(!relay_is_silent(Duration::from_secs(0), SILENCE_LIMIT));
        assert!(!relay_is_silent(SILENCE_LIMIT, SILENCE_LIMIT), "exactly at the limit is still alive");
        assert!(relay_is_silent(SILENCE_LIMIT + Duration::from_secs(1), SILENCE_LIMIT));
    }

    /// 🔴 THE 2026-09-16 OUTAGE, REPRODUCED. A listener that accepts TCP and then says nothing is
    /// exactly a handshake stalled on a flaky WAN. Before the fix `connect_async` awaited it forever,
    /// and `run` never logged or retried for ~12 hours. The outer guard turns a regression into a
    /// FAILURE instead of a hung test run.
    #[tokio::test]
    async fn a_connect_whose_handshake_never_answers_fails_instead_of_hanging() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let holder = tokio::spawn(async move {
            // Accept, then hold the stream open and silent: TCP is up, the upgrade never comes back.
            let (stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(stream);
        });

        let bound = Duration::from_millis(300);
        let started = tokio::time::Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(5), connect(&format!("ws://{addr}/api/hub/ws"), bound))
            .await
            .expect("connect hung past the guard: the handshake has no deadline");
        let Err(e) = outcome else { panic!("a silent handshake must not count as connected") };
        assert!(e.starts_with("connect timed out"), "{e}");
        assert!(started.elapsed() >= bound, "gave up before the bound: {:?}", started.elapsed());
        holder.abort();
    }

    #[tokio::test]
    async fn a_write_that_never_completes_ends_the_connection_with_an_error() {
        let stuck = std::future::pending::<Result<(), String>>();
        let e = tokio::time::timeout(Duration::from_secs(5), within(Duration::from_millis(50), "ping write", stuck))
            .await
            .expect("within must not hang")
            .unwrap_err();
        assert_eq!(e, "ping write timed out after 0.05s");
        // A future that finishes in time passes its own result and error straight through.
        assert_eq!(within(Duration::from_secs(1), "x", async { Ok::<_, String>(7) }).await, Ok(7));
        assert_eq!(within::<(), _>(Duration::from_secs(1), "x", async { Err("reset") }).await, Err("reset".into()));
    }

    #[tokio::test]
    async fn a_relayed_call_that_never_returns_is_answered_504_and_does_not_wedge_the_socket() {
        let a = tokio::time::timeout(
            Duration::from_secs(5),
            answer_within(Duration::from_millis(50), std::future::pending::<Answer>()),
        )
        .await
        .expect("a hung call must not hold the socket loop");
        assert_eq!(a.status, 504);
        let quick = answer_within(Duration::from_secs(1), async { Answer { status: 200, body: "{}".into() } }).await;
        assert_eq!(quick.status, 200);
    }

    #[test]
    fn a_slow_call_cannot_by_itself_make_a_healthy_socket_look_silent() {
        // While a call is awaited no ping goes out, so the call bound must end before the silence
        // limit would; otherwise a slow-but-finite call reconnects a perfectly good socket.
        assert!(CALL_TIMEOUT < SILENCE_LIMIT);
        assert!(CONNECT_TIMEOUT > Duration::ZERO && WRITE_TIMEOUT > Duration::ZERO);
    }

    #[test]
    fn the_silence_limit_leaves_room_for_more_than_one_lost_ping() {
        // ⚠️ THE CONSTANTS ARE A PAIR. A limit shorter than the ping interval would declare the
        // socket dead before the first ping could possibly be answered, turning the fix into a
        // reconnect loop — an outage of its own. Three intervals of grace means one lost frame on a
        // marina's Wi-Fi is not mistaken for a dead peer.
        assert!(SILENCE_LIMIT > PING_EVERY, "the limit must outlast a single ping interval");
        assert!(SILENCE_LIMIT >= PING_EVERY * 3, "at least three pings before giving up");
        assert!(SILENCE_LIMIT < PING_EVERY * 6, "but not so long that remote control stays dead for minutes");
    }
}
