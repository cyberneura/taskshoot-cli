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
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{bail, Context, Result};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tungstenite::http::Request;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Error as WsError, Message, WebSocket};

use crate::api::Api;
use crate::config::Config;

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

/// `~/.config/taskshoot/state/listen-<host>-<user id>.json`
///
/// Keyed by both host and user because the cursor is meaningless across either:
/// pointing a local development server at a production cursor would ask it for
/// ids it has never seen.
fn default_state_path(api_origin: &str, user_id: &str) -> Result<PathBuf> {
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
            "listen-{}-{}.json",
            sanitize_path_segment(host),
            sanitize_path_segment(user_id)
        )))
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
            None => default_state_path(&config.api_origin, &user_id)?,
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

    let mut socket = match tungstenite::connect(request) {
        Ok((socket, _response)) => socket,
        Err(WsError::Http(response)) => return handshake_rejected(response.status().as_u16()),
        Err(e) => return Ok(Disposition::Retry(format!("cannot connect: {e}"))),
    };

    if let Err(e) = set_read_timeout(&mut socket, READ_POLL) {
        // Without the timeout the loop would block in read() forever and never
        // send a ping, so a silently dead peer would hang the process.
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

/// Host (with port) for the handshake's Host header.
fn host_header(url: &str) -> Result<String> {
    let after_scheme = url
        .split("://")
        .nth(1)
        .with_context(|| format!("malformed WebSocket URL {url}"))?;
    let host = after_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or(after_scheme);
    if host.is_empty() {
        bail!("malformed WebSocket URL {url}");
    }
    Ok(host.to_string())
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
        let prod = default_state_path("https://taskshoot-api.cyberneura.com", "019f-user").unwrap();
        let local = default_state_path("http://127.0.0.1:8008", "019f-user").unwrap();
        assert_ne!(prod, local);
        assert_eq!(
            prod.file_name().unwrap(),
            "listen-taskshoot-api.cyberneura.com-019f-user.json"
        );
        // The port's ':' would otherwise be a path separator on Windows
        assert_eq!(
            local.file_name().unwrap(),
            "listen-127.0.0.1_8008-019f-user.json"
        );
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
