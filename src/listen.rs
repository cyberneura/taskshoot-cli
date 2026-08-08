//! `taskshoot listen` -- stream notifications from the user notification
//! WebSocket as JSON Lines.
//!
//! This exists so a bot can react to a mention in seconds instead of waiting
//! for the next polling cycle. The server side (`/ws/user/notifications/`)
//! accepts an API key, filters by notification type and replays what was
//! missed while disconnected.
//!
//! **The socket is not the source of truth.** channels' `group_send` is
//! fire-and-forget: no ACK, no replay. Notifications created while this
//! process is disconnected only come back through the `since` catchup, and
//! that catchup is capped server-side (50 events). Whatever consumes this
//! stream must keep its periodic polling as a backstop.
//!
//! Output contract:
//!
//! - **stdout** carries one compact JSON object per line, carrying the server's
//!   message unchanged (`{"type":"notification_created","notification":{...}}`).
//!   It is re-serialized rather than echoed, so the bytes (key order, spacing)
//!   are not guaranteed -- the fields are. Nothing else is written there, so
//!   `taskshoot listen | while read -r line` works without filtering.
//! - **stderr** carries connection lifecycle logs (connect, catchup, retry).

use std::fs;
use std::io::Write;
use std::net::{Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tungstenite::handshake::HandshakeError;
use tungstenite::http::Request;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Error as WsError, Message, WebSocket};

use crate::api::Api;
use crate::config::Config;
use crate::proxy::{self, Proxy};

const WS_PATH: &str = "/ws/user/notifications/";

// Application close codes defined by the server (notification/ws.py).
const CLOSE_UNAUTHENTICATED: u16 = 4401;
const CLOSE_BAD_REQUEST: u16 = 4400;

// How often an application level ping goes out, and how long its pong may take.
// The ping is the JSON one the consumer implements (not a protocol ping) so a
// reply proves the ASGI application is alive, not just the proxy in front of it.
const PING_INTERVAL: Duration = Duration::from_secs(30);
const PONG_TIMEOUT: Duration = Duration::from_secs(30);
// Socket read timeout. It only sets how often the loop wakes up to send a ping
// or notice a dead peer; it is not an idle timeout for the connection itself.
const READ_POLL: Duration = Duration::from_secs(5);
// Bounds on getting a connection up. Without them a proxy that completes the
// TCP handshake and then goes quiet leaves the blocking upgrade parked on a
// stream with no timeout, and the reconnect loop below never gets to run.
//
// CONNECT_DEADLINE is the one that actually bounds the attempt: it covers name
// resolution, every address tried, and the upgrade together. The other two are
// per-operation, so on their own they would bound neither a multi-address
// connect nor a peer answering slowly but steadily.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_IO_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_DEADLINE: Duration = Duration::from_secs(30);
// A worker that missed the deadline is cancelled (see `establish`), but the one
// step that cannot be cancelled is the name lookup: it is the OS resolver's to
// end. This caps how many such workers may be outstanding at once, so a
// resolver that never answers costs a fixed number of threads for the life of
// the process instead of one more per reconnect.
const MAX_CONNECT_WORKERS: usize = 4;

/// Connect workers that have been started and have not returned yet.
static CONNECT_WORKERS: AtomicUsize = AtomicUsize::new(0);

const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
// A connection that lasted this long counts as healthy, so the next drop starts
// backing off from the beginning again. Without it a process that stays up for
// days reconnects at the 60s ceiling after a single unrelated blip.
const STABLE_CONNECTION: Duration = Duration::from_secs(60);

/// How many delivered notification ids are remembered for deduplication.
///
/// It has to exceed the server's catchup limit (50): after a reconnect the
/// catchup deliberately re-sends a 30 second overlap around the cursor, and
/// those repeats have to be recognisable. The set is persisted with the cursor
/// so a restart does not re-emit them either.
const SEEN_CAPACITY: usize = 256;

/// Everything `listen` takes from the command line.
pub struct ListenArgs {
    /// Notification types to receive (empty = every type).
    pub types: Vec<String>,
    /// Cursor override. Without it the cursor comes from the state file.
    pub since: Option<String>,
    /// State file path override.
    pub state: Option<PathBuf>,
    /// Do not read or write a state file (cursor comes from --since only).
    pub no_state: bool,
    /// Exit successfully after this many notifications (unlimited when None).
    /// The command line rejects 0, so the limit is always reachable.
    pub max_events: Option<u64>,
}

/// Cursor and recent ids, persisted between runs.
///
/// The cursor is what lets a restart pick up what happened while the process
/// was down; `seen` is what keeps that catchup from re-emitting events the
/// consumer already acted on.
#[derive(Default, Serialize, Deserialize)]
pub struct ListenState {
    #[serde(default)]
    pub cursor: Option<String>,
    /// Delivered notification ids, oldest first.
    #[serde(default)]
    pub seen: Vec<String>,
}

impl ListenState {
    /// Whether this id has already been handed to the consumer.
    ///
    /// Kept separate from [`record`](Self::record) so that an event is only
    /// marked delivered once it actually has been: the catchup overlap makes
    /// repeats normal, but a repeat and a failed write must not look alike.
    pub fn is_delivered(&self, id: &str) -> bool {
        self.seen.iter().any(|seen| seen == id)
    }

    /// Mark an id delivered and move the cursor to it. A repeat is a no-op.
    ///
    /// The cursor only ever moves forward: notification ids are UUID7, whose
    /// canonical text form sorts in creation order, and the catchup overlap
    /// hands back ids *older* than the cursor on purpose. Taking the max keeps
    /// one of those from rewinding the cursor and replaying the same window
    /// forever.
    pub fn record(&mut self, id: &str) {
        if self.is_delivered(id) {
            return;
        }
        self.seen.push(id.to_string());
        if self.seen.len() > SEEN_CAPACITY {
            let excess = self.seen.len() - SEEN_CAPACITY;
            self.seen.drain(..excess);
        }
        let newer = match self.cursor.as_deref() {
            Some(cursor) => id > cursor,
            None => true,
        };
        if newer {
            self.cursor = Some(id.to_string());
        }
    }

    fn load(path: &Path) -> Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        // A truncated or hand-edited file must not stop the listener: losing the
        // cursor costs a replay window, refusing to start costs every event.
        match serde_json::from_str(&text) {
            Ok(state) => Ok(state),
            Err(e) => {
                eprintln!(
                    "taskshoot: ignoring unreadable state file {} ({e})",
                    path.display()
                );
                Ok(Self::default())
            }
        }
    }

    /// Write via a temporary file and rename, so a crash mid-write cannot leave
    /// a truncated cursor behind (which would replay or skip on restart).
    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let text = serde_json::to_string(self).context("cannot serialize the listen state")?;
        // The temporary name carries the pid: two listeners pointed at one state
        // file are already a mistake, but sharing a scratch name would turn it
        // into a corrupt or half-written cursor rather than a loud one.
        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        fs::write(&temp, text).with_context(|| format!("cannot write {}", temp.display()))?;
        fs::rename(&temp, path).with_context(|| format!("cannot write {}", path.display()))?;
        Ok(())
    }
}

/// `wss://host/ws/user/notifications/?types=...&since=...`
///
/// The WebSocket goes straight to the API origin. The frontend's Cloudflare
/// Worker proxies its own WS path, but bots hold an API key rather than a
/// session cookie and have no reason to go through it.
pub fn ws_url(api_origin: &str, types: &[String], since: Option<&str>) -> Result<String> {
    let origin = api_origin.trim_end_matches('/');
    let ws_origin = if let Some(rest) = origin.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = origin.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        bail!("the API origin must start with http:// or https:// (got {api_origin})");
    };
    let mut url = format!("{ws_origin}{WS_PATH}");
    let mut separator = '?';
    let joined = types.join(",");
    if !joined.is_empty() {
        url.push(separator);
        url.push_str(&format!("types={}", query_enc(&joined)));
        separator = '&';
    }
    if let Some(since) = since {
        url.push(separator);
        url.push_str(&format!("since={}", query_enc(since)));
    }
    Ok(url)
}

/// Percent-encode a query value. NON_ALPHANUMERIC is deliberately blunt: the
/// values here are ids and type names, so over-encoding costs nothing and
/// leaves no delimiter (`,` `&` `=`) able to change the query's shape.
fn query_enc(value: &str) -> String {
    const QUERY: &AsciiSet = &NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    utf8_percent_encode(value, QUERY).to_string()
}

/// `~/.config/taskshoot/state/listen-<host>-<user id>-<types>.json`
///
/// Keyed by host and user because the cursor is meaningless across either:
/// pointing a local development server at a production cursor would ask it for
/// ids it has never seen.
///
/// Keyed by the subscription as well because the cursor only summarises what
/// *this* filter has seen. A run with `--types task_mentioned` advances the
/// cursor past the assignments it was never sent; a later run subscribing to
/// more types would hand that cursor back and the catchup would skip them
/// silently. Separate files keep each subscription's replay window intact.
fn default_state_path(api_origin: &str, user_id: &str, types: &[String]) -> Result<PathBuf> {
    let home = dirs::home_dir().context("cannot determine the home directory")?;
    let host = api_origin
        .split("://")
        .nth(1)
        .unwrap_or(api_origin)
        .trim_end_matches('/');
    Ok(home
        .join(".config")
        .join("taskshoot")
        .join("state")
        .join(format!(
            "listen-{}-{}-{}.json",
            sanitize_path_segment(host),
            sanitize_path_segment(user_id),
            types_key(types)
        )))
}

/// How much of the readable type list survives in the file name before the
/// digest has to carry the rest of it.
const TYPES_KEY_READABLE_MAX: usize = 32;

/// A file name component identifying one subscription.
///
/// Order and repetition do not change what the server sends, so they must not
/// change the key either -- `--types a,b` and `--types b,a,b` share a cursor on
/// purpose. The readable part is what makes the file recognisable; the digest is
/// what keeps two subscriptions apart once sanitizing or truncating has made
/// their readable parts equal, which is the only way they could otherwise end
/// up sharing a cursor.
fn types_key(types: &[String]) -> String {
    if types.is_empty() {
        return "all".to_string();
    }
    let mut normalized: Vec<&str> = types.iter().map(String::as_str).collect();
    normalized.sort_unstable();
    normalized.dedup();
    let joined = normalized.join("+");
    // `sanitize_path_segment` maps every non-ASCII character to a single byte,
    // so the result is pure ASCII and this truncation cannot split a character.
    let mut readable = sanitize_path_segment(&joined);
    readable.truncate(TYPES_KEY_READABLE_MAX);
    format!("{readable}-{:016x}", fnv1a64(&joined))
}

/// FNV-1a over the exact normalized type list.
///
/// The digest has to be stable across runs and distinct for distinct lists; it
/// guards nothing against a chosen input, since the only thing it separates is
/// one local user's own subscriptions. 64 bits leaves an accidental collision
/// far below the odds of anything else in this file going wrong, and keeps the
/// cost at no dependency and no allocation.
fn fnv1a64(value: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Keep a value usable as one file name component (host names carry `:` for a
/// port, and a user id is only expected to be a UUID).
fn sanitize_path_segment(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// What ended a connection attempt.
enum Disposition {
    /// Reconnect after backing off. Carries the reason, for the log.
    Retry(String),
    /// --max-events reached; stop cleanly.
    Done,
}

pub fn listen(api: &Api, config: &Config, args: &ListenArgs) -> Result<()> {
    // Resolve the user before opening the socket: it validates the API key with
    // a readable HTTP error (a bad key would otherwise surface as a bare 4401
    // close) and names the state file.
    let me = api.me()?;
    let user_id = me
        .get("id")
        .and_then(Value::as_str)
        .context("unexpected /api/user/me response: no id")?
        .to_string();

    let state_path = if args.no_state {
        None
    } else {
        Some(match args.state.clone() {
            Some(path) => path,
            None => default_state_path(&config.api_origin, &user_id, &args.types)?,
        })
    };

    let mut state = match state_path.as_deref() {
        Some(path) => ListenState::load(path)?,
        None => ListenState::default(),
    };
    if let Some(since) = args.since.as_deref() {
        // An explicit cursor wins over the stored one; `seen` is kept, since it
        // still describes what this consumer has already acted on.
        state.cursor = Some(since.to_string());
    }

    match state_path.as_deref() {
        Some(path) => eprintln!("taskshoot: state file {}", path.display()),
        None => eprintln!("taskshoot: running without a state file (--no-state)"),
    }

    let mut delivered: u64 = 0;
    let mut backoff = BACKOFF_INITIAL;
    loop {
        let started = Instant::now();
        let disposition = run_connection(
            config,
            args,
            &mut state,
            state_path.as_deref(),
            &mut delivered,
        )?;
        match disposition {
            Disposition::Done => return Ok(()),
            Disposition::Retry(reason) => {
                if started.elapsed() >= STABLE_CONNECTION {
                    backoff = BACKOFF_INITIAL;
                }
                let wait = jittered(backoff);
                eprintln!(
                    "taskshoot: {reason}; reconnecting in {:.1}s",
                    wait.as_secs_f64()
                );
                std::thread::sleep(wait);
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// Spread reconnects by up to 25% so that a fleet of listeners knocked off by
/// one deploy does not come back in lockstep.
fn jittered(base: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let spread = base.as_millis() as u64 / 4;
    if spread == 0 {
        return base;
    }
    base + Duration::from_millis(nanos as u64 % spread)
}

/// Open one connection and pump it until it dies.
///
/// `Ok` means "keep going" (retry or stop); `Err` is reserved for failures that
/// reconnecting cannot fix -- a rejected key or a rejected query -- because
/// retrying those just spins forever against a server that already answered.
fn run_connection(
    config: &Config,
    args: &ListenArgs,
    state: &mut ListenState,
    state_path: Option<&Path>,
    delivered: &mut u64,
) -> Result<Disposition> {
    let url = ws_url(&config.api_origin, &args.types, state.cursor.as_deref())?;
    let request = Request::builder()
        .uri(&url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        // tungstenite fills the handshake headers in for a plain URL, but not
        // when the request is built by hand.
        .header("Host", host_header(&url)?)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .context("cannot build the WebSocket handshake request")?;

    // A malformed authority is not worth retrying: it comes from the configured
    // origin, so the next attempt would build the same URL.
    let (host, port) = connect_target(&url)?;
    // Not retryable either: a proxy variable that does not parse is a mistake in
    // the environment, and the next attempt would read the same value.
    let proxy = proxy::select(url.starts_with("wss:"), &host, port, |name| {
        std::env::var(name).ok()
    })?;
    if let Some(proxy) = &proxy {
        eprintln!(
            "taskshoot: connecting through the proxy in {} ({}:{})",
            proxy.source, proxy.host, proxy.port
        );
    }
    let mut socket = match establish(host, port, proxy, request, CONNECT_DEADLINE) {
        Ok(socket) => socket,
        Err(ConnectFailure::Rejected(status)) => return handshake_rejected(status),
        Err(ConnectFailure::Failed(reason)) => return Ok(Disposition::Retry(reason)),
    };

    // Relax the read timeout from the handshake bound to the polling interval:
    // from here on it only sets how often the loop wakes up. It still must be
    // set -- without it read() would block forever and never send a ping, so a
    // silently dead peer would hang the process.
    if let Err(e) = set_read_timeout(&mut socket, READ_POLL) {
        let _ = socket.close(None);
        return Ok(Disposition::Retry(format!(
            "cannot set the socket read timeout: {e}"
        )));
    }

    eprintln!(
        "taskshoot: connected to {}{WS_PATH}{}",
        config.api_origin,
        match state.cursor.as_deref() {
            Some(cursor) => format!(" (catching up since {cursor})"),
            None => String::new(),
        }
    );

    let mut last_ping: Option<Instant> = None;
    let mut awaiting_pong: Option<Instant> = None;
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                match handle_message(text.as_str(), state, state_path, delivered)? {
                    Handled::Pong => awaiting_pong = None,
                    Handled::Other => {}
                }
                if let Some(max) = args.max_events {
                    if *delivered >= max {
                        let _ = socket.close(None);
                        return Ok(Disposition::Done);
                    }
                }
            }
            // The consumer only speaks text; anything else is protocol level and
            // tungstenite has already answered it (pong for ping).
            Ok(Message::Close(frame)) => {
                let code = frame.as_ref().map(|f| u16::from(f.code));
                let reason = frame
                    .as_ref()
                    .map(|f| f.reason.to_string())
                    .unwrap_or_default();
                match code {
                    Some(CLOSE_UNAUTHENTICATED) => {
                        bail!("the API key was rejected by the server (close {CLOSE_UNAUTHENTICATED})")
                    }
                    Some(CLOSE_BAD_REQUEST) => bail!(
                        "the server rejected the subscription (close {CLOSE_BAD_REQUEST}): \
                         check --types and --since{}",
                        if reason.is_empty() {
                            String::new()
                        } else {
                            format!(" ({reason})")
                        }
                    ),
                    other => {
                        return Ok(Disposition::Retry(format!(
                            "server closed the connection ({})",
                            other.map(|c| c.to_string()).unwrap_or("no code".into())
                        )))
                    }
                }
            }
            Ok(_) => {}
            Err(WsError::Io(e)) if would_block(&e) => {
                // The read timeout fired: nothing arrived, so use the wakeup to
                // check the peer is still answering.
                if let Some(sent) = awaiting_pong {
                    if sent.elapsed() >= PONG_TIMEOUT {
                        return Ok(Disposition::Retry(format!(
                            "no pong within {}s",
                            PONG_TIMEOUT.as_secs()
                        )));
                    }
                }
                let due = last_ping.is_none_or(|sent| sent.elapsed() >= PING_INTERVAL);
                if due {
                    if let Err(e) = socket.send(Message::Text(r#"{"type":"ping"}"#.into())) {
                        return Ok(Disposition::Retry(format!("cannot send a ping: {e}")));
                    }
                    let now = Instant::now();
                    last_ping = Some(now);
                    // Only start the pong deadline when one is not already
                    // running, so a late pong is measured from its own ping.
                    awaiting_pong.get_or_insert(now);
                }
            }
            Err(WsError::ConnectionClosed | WsError::AlreadyClosed) => {
                return Ok(Disposition::Retry("the connection was closed".into()))
            }
            Err(e) => return Ok(Disposition::Retry(format!("read failed: {e}"))),
        }
    }
}

/// Turn a non-101 handshake response into either a fatal error or a retry.
///
/// 403 is deliberately not read as "bad key" alone: a consumer that closes
/// before accepting -- which is what the server does for an unknown `types`
/// value or a malformed `since` -- also reaches the client as a plain 403, with
/// the real reason only in the server log. Both causes are permanent, so this
/// is fatal either way; the message just has to name both.
fn handshake_rejected(status: u16) -> Result<Disposition> {
    match status {
        401 => bail!("the API key was rejected by the WebSocket handshake (HTTP 401)"),
        403 => bail!(
            "the server refused the WebSocket handshake (HTTP 403): the API key was rejected, \
             or --types / --since holds a value the server does not accept"
        ),
        other => Ok(Disposition::Retry(format!(
            "WebSocket handshake failed (HTTP {other})"
        ))),
    }
}

enum Handled {
    Pong,
    Other,
}

/// Emit one server message. Only notifications reach stdout; everything else is
/// logged, so an unknown message type from a newer server cannot corrupt the
/// JSON Lines stream.
fn handle_message(
    text: &str,
    state: &mut ListenState,
    state_path: Option<&Path>,
    delivered: &mut u64,
) -> Result<Handled> {
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("taskshoot: ignoring an unparseable message ({e})");
            return Ok(Handled::Other);
        }
    };
    match value.get("type").and_then(Value::as_str) {
        Some("pong") => return Ok(Handled::Pong),
        Some("catchup_done") => {
            let count = value.get("count").and_then(Value::as_u64).unwrap_or(0);
            eprintln!("taskshoot: catchup complete ({count} event(s))");
            return Ok(Handled::Other);
        }
        Some("notification_created") => {}
        other => {
            eprintln!(
                "taskshoot: ignoring an unknown message type {:?}",
                other.unwrap_or("(none)")
            );
            return Ok(Handled::Other);
        }
    }

    let id = value
        .get("notification")
        .and_then(|n| n.get("id"))
        .and_then(Value::as_str);
    let Some(id) = id else {
        eprintln!("taskshoot: ignoring a notification without an id");
        return Ok(Handled::Other);
    };
    if state.is_delivered(id) {
        // The catchup overlap re-sends events around the cursor by design.
        return Ok(Handled::Other);
    }

    // Print before recording the cursor: a crash in between repeats an event
    // (the consumer must be idempotent by id anyway), whereas recording first
    // would drop it silently.
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{value}").context("cannot write to stdout")?;
    stdout.flush().context("cannot flush stdout")?;
    drop(stdout);
    *delivered += 1;

    state.record(id);
    if let Some(path) = state_path {
        state.save(path)?;
    }
    Ok(Handled::Other)
}

/// Scheme and authority of a WebSocket URL, e.g. `("wss", "example.com:8443")`.
///
/// The authority is returned as written: that is exactly what the Host header
/// needs, and it keeps an IPv6 literal's brackets for [`connect_target`] to
/// strip.
fn split_url(url: &str) -> Result<(&str, &str)> {
    let (scheme, rest) = url
        .split_once("://")
        .with_context(|| format!("malformed WebSocket URL {url}"))?;
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    if authority.is_empty() {
        bail!("malformed WebSocket URL {url}");
    }
    Ok((scheme, authority))
}

/// Host (with port) for the handshake's Host header.
fn host_header(url: &str) -> Result<String> {
    Ok(split_url(url)?.1.to_string())
}

/// Why a connection attempt produced no socket.
enum ConnectFailure {
    /// The server answered the handshake with a non-101 status, so it decided
    /// rather than failed.
    Rejected(u16),
    /// Anything else -- resolution, the TCP connect, TLS, or the deadline.
    Failed(String),
}

/// Resolve, connect and upgrade, under one deadline for the whole phase.
///
/// The connection is built by hand rather than through `tungstenite::connect`
/// for two reasons. The timeouts have to be on the stream *before* the upgrade
/// is written to it, and tungstenite's redirect following is not wanted on a
/// request carrying a bearer token: this path does not redirect, and following
/// one would hand the key to whatever host the `Location` named.
///
/// It runs on its own thread only so that the deadline can cover the phase as a
/// whole. Missing the deadline therefore leaves a worker behind, and that worker
/// has to be stopped rather than merely dropped: a peer that trickles handshake
/// bytes just often enough to keep resetting the per-operation timeouts would
/// otherwise park a thread and a socket for as long as it liked, one more per
/// retry. So the worker publishes a duplicate of its socket in `cancel`, and a
/// deadline miss shuts that duplicate down, which fails the read or write the
/// worker is blocked in.
///
/// What remains uncancellable is the name lookup, which belongs to the OS
/// resolver and ends when it says so. `MAX_CONNECT_WORKERS` bounds that case:
/// once that many attempts are outstanding, this fails immediately instead of
/// starting one more.
fn establish(
    host: String,
    port: u16,
    proxy: Option<Proxy>,
    request: Request<()>,
    deadline: Duration,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, ConnectFailure> {
    let worker =
        ConnectWorker::acquire(&CONNECT_WORKERS, MAX_CONNECT_WORKERS).ok_or_else(|| {
            ConnectFailure::Failed(format!(
                "{MAX_CONNECT_WORKERS} earlier connect attempts are still blocked"
            ))
        })?;

    let (sender, receiver) = mpsc::channel();
    let cancel: CancelSlot = Arc::new(Mutex::new(Cancellation::Pending));
    let worker_cancel = Arc::clone(&cancel);
    std::thread::Builder::new()
        .name("taskshoot-connect".into())
        .spawn(move || {
            // Held for the whole attempt so the slot is only released once this
            // worker really is gone, abandoned or not.
            let _worker = worker;
            let _ = sender.send(connect_and_upgrade(
                &host,
                port,
                proxy.as_ref(),
                request,
                &worker_cancel,
            ));
        })
        .map_err(|e| ConnectFailure::Failed(format!("cannot start the connect thread: {e}")))?;

    match receiver.recv_timeout(deadline) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Nothing worth saving if the worker did get a socket: this attempt
            // is failed either way, so a connection that completed in the same
            // instant is one this caller has already stopped waiting for.
            cancel_attempt(&cancel);
            Err(ConnectFailure::Failed(format!(
                "no connection within {deadline:?}"
            )))
        }
        // The sender was dropped without sending, which only a panic does; its
        // message is already on stderr.
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(ConnectFailure::Failed(
            "the connect attempt panicked".into(),
        )),
    }
}

/// How a connect worker and its deadline hand cancellation to each other.
///
/// A plain "socket or nothing" slot would lose the race where the deadline
/// fires while the worker is between the TCP connect and publishing its
/// duplicate: the deadline would find nothing to shut down, and the socket
/// arriving a moment later would have no one left to shut it down. Recording
/// the cancellation means whichever side is second does the shutting down.
enum Cancellation {
    /// The attempt is still wanted and has no socket yet.
    Pending,
    /// The socket to shut down if the deadline gives up on this attempt.
    Socket(TcpStream),
    /// The deadline already gave up.
    Cancelled,
}

/// Shared between [`establish`] and its connect worker.
type CancelSlot = Arc<Mutex<Cancellation>>;

/// Lock a [`CancelSlot`], reading through a poisoned lock.
///
/// Poisoning means the connect worker panicked mid-attempt, which is exactly
/// when its socket most needs shutting down.
fn lock_cancel(cancel: &CancelSlot) -> std::sync::MutexGuard<'_, Cancellation> {
    cancel
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Give up on the attempt, shutting its socket down if it has published one.
fn cancel_attempt(cancel: &CancelSlot) {
    let taken = std::mem::replace(&mut *lock_cancel(cancel), Cancellation::Cancelled);
    if let Cancellation::Socket(stream) = taken {
        let _ = stream.shutdown(Shutdown::Both);
    }
}

/// Offer the socket up for cancellation.
///
/// `false` means the deadline gave up first, so the socket was shut down here
/// instead of stored and the attempt should stop.
fn publish_socket(cancel: &CancelSlot, stream: TcpStream) -> bool {
    let mut slot = lock_cancel(cancel);
    if matches!(*slot, Cancellation::Cancelled) {
        drop(slot);
        let _ = stream.shutdown(Shutdown::Both);
        return false;
    }
    *slot = Cancellation::Socket(stream);
    true
}

/// A permit to run one connect worker, released when the worker returns.
///
/// Counting live workers rather than abandoned ones keeps the bookkeeping on
/// the worker itself, which is the only side that knows when it is really done.
struct ConnectWorker(&'static AtomicUsize);

impl ConnectWorker {
    /// `None` once `max` workers are already running, which only happens when
    /// earlier attempts are stuck somewhere uncancellable.
    fn acquire(counter: &'static AtomicUsize, max: usize) -> Option<Self> {
        let mut live = counter.load(Ordering::Acquire);
        loop {
            if live >= max {
                return None;
            }
            match counter.compare_exchange_weak(live, live + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(Self(counter)),
                Err(actual) => live = actual,
            }
        }
    }
}

impl Drop for ConnectWorker {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The blocking half of [`establish`], run on its own thread.
fn connect_and_upgrade(
    host: &str,
    port: u16,
    proxy: Option<&Proxy>,
    request: Request<()>,
    cancel: &CancelSlot,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, ConnectFailure> {
    // With a proxy the socket goes to the proxy, and the tunnel to the API is
    // asked for over it. Everything after that -- timeouts, cancellation, the
    // TLS handshake -- is the same either way.
    let target = proxy.map_or((host, port), |proxy| (proxy.host.as_str(), proxy.port));
    let mut stream = connect_with_timeout(target.0, target.1)
        .map_err(|e| ConnectFailure::Failed(format!("cannot connect: {e:#}")))?;
    // Per-operation bounds, not a bound on the phase (that is the deadline in
    // `establish`). What they are needed for is the abandoned case: a socket
    // with no timeout at all would keep its attempt -- and its file descriptor
    // -- alive indefinitely after the deadline gave up on it.
    set_stream_timeouts(&stream, HANDSHAKE_IO_TIMEOUT)
        .map_err(|e| ConnectFailure::Failed(format!("cannot bound the handshake: {e:#}")))?;
    // Publish before the upgrade is written, because from here on every way
    // this can block is a read or a write on this socket -- which is what
    // shutting the duplicate down ends. A duplicate that outlives the attempt
    // costs a descriptor until it is dropped; closing it does not close the
    // connection the caller was handed.
    match stream.try_clone() {
        Ok(handle) => {
            if !publish_socket(cancel, handle) {
                return Err(ConnectFailure::Failed(
                    "the connect attempt was cancelled".into(),
                ));
            }
        }
        // Fail rather than continue uncancellably: running out of descriptors
        // is itself a reason to give this attempt up to the backoff.
        Err(e) => {
            return Err(ConnectFailure::Failed(format!(
                "cannot duplicate the socket: {e}"
            )))
        }
    }

    // After the socket is published, so a proxy that accepts the connection and
    // then stalls on CONNECT is cancelled by the deadline like any other stall.
    if let Some(proxy) = proxy {
        if let Err(e) = proxy::tunnel(&mut stream, proxy, host, port) {
            return Err(ConnectFailure::Failed(format!("{e:#}")));
        }
    }

    match tungstenite::client_tls(request, stream) {
        Ok((socket, _response)) => Ok(socket),
        Err(HandshakeError::Failure(WsError::Http(response))) => {
            Err(ConnectFailure::Rejected(response.status().as_u16()))
        }
        // Interrupted only happens on a non-blocking stream. This one blocks
        // (with timeouts), so report it as a failed attempt rather than trust
        // the invariant enough to panic on it.
        Err(e) => Err(ConnectFailure::Failed(format!(
            "the WebSocket handshake failed: {e}"
        ))),
    }
}

/// Host and port to open the TCP connection to, from the WebSocket URL.
///
/// A malformed URL is an error rather than a retry: reconnecting cannot make it
/// parse.
fn connect_target(url: &str) -> Result<(String, u16)> {
    let (scheme, authority) = split_url(url)?;
    let default_port = match scheme {
        "wss" => 443,
        "ws" => 80,
        other => bail!("unsupported WebSocket scheme {other} in {url}"),
    };
    // An IPv6 literal is bracketed (`[::1]:8008`), so the ':' inside it is not
    // the port separator.
    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let (host, rest) = after_bracket
            .split_once(']')
            .with_context(|| format!("malformed WebSocket URL {url}"))?;
        (host, rest.strip_prefix(':'))
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if host.is_empty() {
        bail!("malformed WebSocket URL {url}");
    }
    let port = match port {
        Some(port) => port
            .parse()
            .with_context(|| format!("malformed port {port:?} in {url}"))?,
        None => default_port,
    };
    Ok((host.to_string(), port))
}

/// Connect to the first address that answers, giving each its own bound.
///
/// Name resolution itself is left to the OS resolver, which applies its own
/// timeout; the unbounded wait this is here to avoid is the connect, not the
/// lookup.
fn connect_with_timeout(host: &str, port: u16) -> Result<TcpStream> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve {host}:{port}"))?
        .collect();
    let mut last: Option<std::io::Error> = None;
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
            Ok(stream) => {
                // Nagle would only delay the small ping/pong frames; there is
                // nothing here worth coalescing.
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(e) => last = Some(e),
        }
    }
    match last {
        Some(e) => Err(e).with_context(|| format!("cannot reach {host}:{port}")),
        None => bail!("{host}:{port} resolved to no addresses"),
    }
}

/// Bound both directions on the raw stream, before it is wrapped for TLS and
/// the upgrade is written to it.
fn set_stream_timeouts(stream: &TcpStream, timeout: Duration) -> Result<()> {
    stream
        .set_read_timeout(Some(timeout))
        .context("set_read_timeout failed")?;
    stream
        .set_write_timeout(Some(timeout))
        .context("set_write_timeout failed")?;
    Ok(())
}

/// A socket read timeout surfaces as WouldBlock on unix and TimedOut on
/// Windows; both mean "nothing to read yet", not a broken connection.
fn would_block(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn set_read_timeout(
    socket: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    timeout: Duration,
) -> Result<()> {
    let stream = match socket.get_ref() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::Rustls(stream) => stream.get_ref(),
        // MaybeTlsStream is non_exhaustive: a TLS backend this build does not
        // use would land here rather than failing to compile.
        _ => bail!("unsupported WebSocket stream type"),
    };
    stream
        .set_read_timeout(Some(timeout))
        .context("set_read_timeout failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_url_switches_scheme_and_encodes_the_query() {
        assert_eq!(
            ws_url("https://taskshoot-api.cyberneura.com", &[], None).unwrap(),
            "wss://taskshoot-api.cyberneura.com/ws/user/notifications/"
        );
        assert_eq!(
            ws_url("http://127.0.0.1:8008", &[], None).unwrap(),
            "ws://127.0.0.1:8008/ws/user/notifications/"
        );
        // A trailing slash on the origin must not double up with the path
        assert_eq!(
            ws_url("https://example.com/", &[], None).unwrap(),
            "wss://example.com/ws/user/notifications/"
        );
        assert_eq!(
            ws_url(
                "https://example.com",
                &["task_mentioned".into(), "task_assigned".into()],
                Some("019f-cursor"),
            )
            .unwrap(),
            "wss://example.com/ws/user/notifications/\
             ?types=task_mentioned%2Ctask_assigned&since=019f-cursor"
        );
        // since alone still opens the query with '?'
        assert_eq!(
            ws_url("https://example.com", &[], Some("abc")).unwrap(),
            "wss://example.com/ws/user/notifications/?since=abc"
        );
    }

    #[test]
    fn ws_url_rejects_an_origin_without_a_known_scheme() {
        assert!(ws_url("taskshoot-api.cyberneura.com", &[], None).is_err());
        assert!(ws_url("ftp://example.com", &[], None).is_err());
    }

    #[test]
    fn host_header_keeps_the_port_and_drops_the_path() {
        assert_eq!(
            host_header("ws://127.0.0.1:8008/ws/x/").unwrap(),
            "127.0.0.1:8008"
        );
        assert_eq!(
            host_header("wss://example.com/ws/user/notifications/?types=a").unwrap(),
            "example.com"
        );
        assert!(host_header("example.com/ws/").is_err());
    }

    #[test]
    fn handshake_rejected_is_fatal_only_for_a_refusal() {
        // 401 / 403 mean the server decided; reconnecting would only repeat it
        assert!(handshake_rejected(401).is_err());
        assert!(handshake_rejected(403).is_err());
        // A gateway hiccup is worth retrying
        assert!(matches!(
            handshake_rejected(502).unwrap(),
            Disposition::Retry(_)
        ));
    }

    #[test]
    fn record_reports_repeats_and_advances_the_cursor() {
        let mut state = ListenState::default();
        state.record("019f0001");
        assert_eq!(state.cursor.as_deref(), Some("019f0001"));
        state.record("019f0002");
        assert_eq!(state.cursor.as_deref(), Some("019f0002"));
        // A repeat (what the catchup overlap sends) is recognised as delivered
        assert!(state.is_delivered("019f0002"));
        assert!(state.is_delivered("019f0001"));
        // and recording it again changes nothing
        state.record("019f0001");
        assert_eq!(state.seen, ["019f0001", "019f0002"]);
        assert_eq!(state.cursor.as_deref(), Some("019f0002"));
    }

    #[test]
    fn record_does_not_rewind_the_cursor_for_an_older_id() {
        // The catchup deliberately returns ids older than the cursor; letting
        // one of them become the cursor would replay the same window forever.
        let mut state = ListenState::default();
        state.record("019f0005");
        state.record("019f0003");
        assert!(state.is_delivered("019f0003"));
        assert_eq!(state.cursor.as_deref(), Some("019f0005"));
    }

    #[test]
    fn record_keeps_the_seen_list_bounded_to_the_newest_ids() {
        let mut state = ListenState::default();
        for i in 0..(SEEN_CAPACITY + 10) {
            state.record(&format!("{i:08}"));
        }
        assert_eq!(state.seen.len(), SEEN_CAPACITY);
        // The oldest ids fell off; the newest are still deduplicated
        assert_eq!(state.seen.first().unwrap(), "00000010");
        assert!(state.is_delivered(&format!("{:08}", SEEN_CAPACITY + 9)));
        assert!(!state.is_delivered("00000000"));
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("taskshoot-listen-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn state_round_trips_through_the_file() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("nested").join("listen.json");
        let mut state = ListenState::default();
        state.record("019f0001");
        state.record("019f0002");
        state.save(&path).unwrap();

        let loaded = ListenState::load(&path).unwrap();
        assert_eq!(loaded.cursor.as_deref(), Some("019f0002"));
        assert_eq!(loaded.seen, ["019f0001", "019f0002"]);
        // No leftover temporary file next to it
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn state_saves_to_a_bare_relative_path() {
        // `--state listen.json` gives an empty parent. create_dir_all("") is a
        // no-op rather than an error, which is what makes the unconditional
        // create above safe -- pin that down.
        let dir = temp_dir("relative");
        let previous = std::env::current_dir().unwrap();
        // set_current_dir is process-wide, so keep the window to this one call
        std::env::set_current_dir(&dir).unwrap();
        let result = ListenState::default().save(Path::new("listen.json"));
        std::env::set_current_dir(previous).unwrap();

        result.unwrap();
        assert!(dir.join("listen.json").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn state_load_tolerates_a_missing_or_corrupt_file() {
        let dir = temp_dir("corrupt");
        let missing = dir.join("absent.json");
        assert!(ListenState::load(&missing).unwrap().cursor.is_none());

        let broken = dir.join("broken.json");
        fs::write(&broken, "{not json").unwrap();
        // Refusing to start would cost every future event, not just the cursor
        assert!(ListenState::load(&broken).unwrap().cursor.is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn default_state_path_separates_hosts_and_users() {
        let prod =
            default_state_path("https://taskshoot-api.cyberneura.com", "019f-user", &[]).unwrap();
        let local = default_state_path("http://127.0.0.1:8008", "019f-user", &[]).unwrap();
        assert_ne!(prod, local);
        assert_eq!(
            prod.file_name().unwrap(),
            "listen-taskshoot-api.cyberneura.com-019f-user-all.json"
        );
        // The port's ':' would otherwise be a path separator on Windows
        assert_eq!(
            local.file_name().unwrap(),
            "listen-127.0.0.1_8008-019f-user-all.json"
        );
    }

    #[test]
    fn default_state_path_separates_subscriptions() {
        let path = |types: &[&str]| {
            let types: Vec<String> = types.iter().map(|t| (*t).to_string()).collect();
            default_state_path("https://example.com", "019f-user", &types).unwrap()
        };
        // A narrower filter advances its cursor past what it never received, so
        // it must not hand that cursor to a wider one
        let mentioned = path(&["task_mentioned"]);
        let both = path(&["task_mentioned", "task_assigned"]);
        assert_ne!(mentioned, both);
        assert_ne!(both, path(&[]));
        // ... but the same subscription written differently is the same one
        assert_eq!(both, path(&["task_assigned", "task_mentioned"]));
        assert_eq!(
            both,
            path(&["task_assigned", "task_mentioned", "task_assigned"])
        );
    }

    #[test]
    fn types_key_stays_a_usable_file_name_component() {
        assert_eq!(types_key(&[]), "all");
        assert!(types_key(&["task_mentioned".into()]).starts_with("task_mentioned-"));
        // A type list has no bound from the command line; the file name does
        let huge: Vec<String> = (0..50).map(|i| format!("type_number_{i}")).collect();
        let key = types_key(&huge);
        assert!(key.len() <= TYPES_KEY_READABLE_MAX + 17, "{key}");
        // Truncation must not merge two subscriptions that share a prefix
        let mut other = huge.clone();
        other.push("type_number_extra".into());
        assert_ne!(key, types_key(&other));
        // Separators are gone, so the key stays one component. `.` survives (it
        // is legal in a name), but the digest suffix keeps the result from ever
        // being bare `.` or `..`.
        let key = types_key(&["a/b".into(), "../c".into()]);
        assert!(!key.contains(std::path::is_separator), "{key}");
        assert_eq!(Path::new(&key).components().count(), 1, "{key}");
    }

    #[test]
    fn establish_gives_up_on_a_peer_that_accepts_and_goes_quiet() {
        // The failure this guards against: a proxy completes the TCP handshake
        // and then never answers the upgrade. With the bound only on the socket
        // after `connect` returned, this attempt never came back and the
        // reconnect loop never ran.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            // Accept and hold: read nothing, write nothing, close nothing.
            let held: Vec<_> = listener.incoming().flatten().collect();
            drop(held);
        });

        let request = Request::builder()
            .uri(format!("ws://127.0.0.1:{port}/ws/"))
            .header("Host", format!("127.0.0.1:{port}"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tungstenite::handshake::client::generate_key(),
            )
            .body(())
            .unwrap();

        let started = Instant::now();
        let deadline = Duration::from_millis(500);
        match establish("127.0.0.1".to_string(), port, None, request, deadline) {
            Ok(_) => panic!("a peer that never answers must not produce a socket"),
            Err(ConnectFailure::Failed(reason)) => assert!(
                reason.contains("no connection"),
                "expected a deadline miss, got {reason}"
            ),
            Err(ConnectFailure::Rejected(status)) => {
                panic!("the peer answered nothing, yet it was read as a refusal ({status})")
            }
        }
        // The point is that it returns at all; allow generous slack for a loaded
        // CI machine while still failing if the deadline was ignored.
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn establish_closes_the_connection_it_abandons() {
        // Returning on time is not enough on its own: the worker left behind
        // keeps a thread and a socket, and a peer that answers slowly but
        // steadily can keep resetting the per-operation timeouts, so one such
        // worker would accumulate per retry for as long as the peer wanted.
        use std::io::Read as _;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (report, closed) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            // The upgrade request, which is answered with silence.
            let _ = peer.read(&mut buf);
            peer.set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            // Ends at EOF (or a reset) once the abandoned attempt is shut down;
            // without that it blocks until the read timeout above.
            let ended = match peer.read(&mut buf) {
                Ok(0) => true,
                Ok(_) => false,
                Err(e) => !would_block(&e),
            };
            let _ = report.send(ended);
        });

        let request = Request::builder()
            .uri(format!("ws://127.0.0.1:{port}/ws/"))
            .header("Host", format!("127.0.0.1:{port}"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tungstenite::handshake::client::generate_key(),
            )
            .body(())
            .unwrap();

        assert!(establish(
            "127.0.0.1".to_string(),
            port,
            None,
            request,
            Duration::from_millis(500)
        )
        .is_err());
        assert_eq!(closed.recv_timeout(Duration::from_secs(5)), Ok(true));
    }

    #[test]
    fn a_socket_published_after_the_deadline_is_shut_down_by_its_worker() {
        // The race the Cancellation states exist for: the deadline fires while
        // the worker is between the TCP connect and publishing its duplicate.
        // Whoever is second has to do the shutting down, or the socket is left
        // with no one holding a handle to stop it.
        use std::io::Read as _;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = std::thread::spawn(move || listener.accept().unwrap().0);
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut peer = accepted.join().unwrap();

        let cancel: CancelSlot = Arc::new(Mutex::new(Cancellation::Pending));
        cancel_attempt(&cancel);
        assert!(
            !publish_socket(&cancel, stream),
            "publishing after the deadline must report the attempt cancelled"
        );

        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 16];
        assert!(
            matches!(peer.read(&mut buf), Ok(0)),
            "the late socket must have been shut down rather than stored"
        );
    }

    #[test]
    fn connect_workers_are_capped_and_released() {
        // Its own counter rather than CONNECT_WORKERS: filling the real one
        // would starve any connect test running beside this one.
        static COUNTER: AtomicUsize = AtomicUsize::new(0);

        let first = ConnectWorker::acquire(&COUNTER, 2).expect("the first worker fits");
        let second = ConnectWorker::acquire(&COUNTER, 2).expect("the second worker fits");
        assert!(
            ConnectWorker::acquire(&COUNTER, 2).is_none(),
            "a third worker must be refused rather than started"
        );

        drop(second);
        assert!(
            ConnectWorker::acquire(&COUNTER, 2).is_some(),
            "a worker that returned must give its slot back"
        );
        drop(first);
    }

    #[test]
    fn connect_target_reads_the_host_and_port() {
        assert_eq!(
            connect_target("wss://example.com/ws/user/notifications/").unwrap(),
            ("example.com".to_string(), 443)
        );
        assert_eq!(
            connect_target("ws://example.com/ws/").unwrap(),
            ("example.com".to_string(), 80)
        );
        assert_eq!(
            connect_target("ws://127.0.0.1:8008/ws/?types=a").unwrap(),
            ("127.0.0.1".to_string(), 8008)
        );
        // The ':' inside an IPv6 literal is not the port separator
        assert_eq!(
            connect_target("ws://[::1]/ws/").unwrap(),
            ("::1".to_string(), 80)
        );
        assert_eq!(
            connect_target("wss://[::1]:8443/ws/").unwrap(),
            ("::1".to_string(), 8443)
        );
        assert!(connect_target("example.com/ws/").is_err());
        assert!(connect_target("https://example.com/ws/").is_err());
        assert!(connect_target("ws://example.com:not-a-port/ws/").is_err());
    }

    #[test]
    fn handle_message_writes_notifications_once_and_tracks_the_cursor() {
        let mut state = ListenState::default();
        let mut delivered = 0;
        let message = r#"{"type":"notification_created","notification":{"id":"019f0001","notification_type":"task_mentioned"}}"#;
        handle_message(message, &mut state, None, &mut delivered).unwrap();
        assert_eq!(delivered, 1);
        assert_eq!(state.cursor.as_deref(), Some("019f0001"));
        // The same event arriving again (catchup overlap) is dropped
        handle_message(message, &mut state, None, &mut delivered).unwrap();
        assert_eq!(delivered, 1);
    }

    #[test]
    fn handle_message_ignores_everything_that_is_not_a_notification() {
        let mut state = ListenState::default();
        let mut delivered = 0;
        assert!(matches!(
            handle_message(r#"{"type":"pong"}"#, &mut state, None, &mut delivered).unwrap(),
            Handled::Pong
        ));
        for message in [
            r#"{"type":"catchup_done","count":3}"#,
            // A type this build does not know about must not reach stdout
            r#"{"type":"something_new","notification":{"id":"019f0009"}}"#,
            r#"{"type":"notification_created","notification":{}}"#,
            "not json at all",
        ] {
            handle_message(message, &mut state, None, &mut delivered).unwrap();
        }
        assert_eq!(delivered, 0);
        assert!(state.cursor.is_none());
    }
}
