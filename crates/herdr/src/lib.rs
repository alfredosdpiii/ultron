//! Ultron in herdr's agents panel (herdr 0.9.5's socket API; herdr-gpui is a client of the same daemon, so this one
//! integration shows in both). Inside a herdr pane the environment has `HERDR_ENV=1`, `HERDR_SOCKET_PATH` (a unix
//! socket) and `HERDR_PANE_ID`; a client writes one JSON line `{"id", "method", "params"}` per connection and reads
//! one reply line, whose content it ignores.
//!
//! [`detect`] reads the injected environment (never the process's own) and the `herdr` setting. A [`Reporter`] owns
//! one background thread: callers only update a small queue (never blocking on the socket, so nothing on a paint
//! path waits for herdr), where the latest state wins, metadata is sent at most once a [`METADATA_INTERVAL`] and only
//! when it changed, and an idle or blocked state is held for [`SETTLE`] so a run that continues at once (an
//! auto-nudge, a keep-going continuation) stays `working`. Every error is silent (logged once to stderr under
//! `ULTRON_DEBUG_INTERNAL=1`); every request has an [`IO_TIMEOUT`] connect, write and read timeout and a monotonic
//! `seq`.
//! Dropping the reporter (or [`Reporter::release`], or [`release_active`] from a signal handler) sends the pending
//! state at once and then `pane.release_agent`, waiting at most [`RELEASE_WAIT`].

use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The `source` of every report.
pub const SOURCE: &str = "ultron";
/// The `agent` of every report.
pub const AGENT: &str = "ultron";
/// Write and read timeout of one request.
pub const IO_TIMEOUT: Duration = Duration::from_millis(500);
/// How long an idle or blocked state waits for a newer one before it is sent.
pub const SETTLE: Duration = Duration::from_millis(300);
/// Metadata is sent at most this often.
pub const METADATA_INTERVAL: Duration = Duration::from_secs(1);
/// How long a release waits for the reporter thread to send it.
pub const RELEASE_WAIT: Duration = Duration::from_millis(1_500);
/// herdr's limit on metadata tokens.
pub const MAX_TOKENS: usize = 16;

/// The herdr pane this process runs in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pane {
    pub socket: PathBuf,
    pub pane_id: String,
    /// `ULTRON_DEBUG_INTERNAL=1`: the first error is written to stderr.
    pub debug: bool,
}

/// `ULTRON_HERDR` turns the integration off with `0`, `false`, `off` or `no`.
pub fn disabled_by_env(value: Option<&str>) -> bool {
    value.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"))
}

/// The pane to report to, from the injected environment and the `herdr` setting (`false` turns it off): `None`
/// outside herdr, when turned off, and inside an Ultron kernel (a cell's `ultron -p` must not take over the pane of
/// the session that runs it; every kernel has `ULTRON_RLM_MAX_MEMORY_MB`).
pub fn detect(var: &dyn Fn(&str) -> Option<String>, setting: Option<&Value>) -> Option<Pane> {
    if setting == Some(&Value::Bool(false)) || disabled_by_env(var("ULTRON_HERDR").as_deref()) {
        return None;
    }
    if var("ULTRON_RLM_MAX_MEMORY_MB").is_some() {
        return None;
    }
    if var("HERDR_ENV").as_deref().map(str::trim) != Some("1") {
        return None;
    }
    let socket = var("HERDR_SOCKET_PATH").filter(|v| !v.trim().is_empty())?;
    let pane_id = var("HERDR_PANE_ID").filter(|v| !v.trim().is_empty())?;
    Some(Pane {
        socket: PathBuf::from(socket),
        pane_id,
        debug: var("ULTRON_DEBUG_INTERNAL").as_deref() == Some("1"),
    })
}

/// `PaneAgentState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Blocked,
    Unknown,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Blocked => "blocked",
            State::Unknown => "unknown",
        }
    }
}

/// The agent session a pane shows (and resumes after a herdr restart).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub path: Option<String>,
    /// The first element is a plain command name.
    pub resume_argv: Option<Vec<String>>,
    /// `session_start_source` (`startup`, `resume`, `clear`, ...).
    pub start_source: Option<String>,
}

/// `pane.report_metadata` fields Ultron sets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Metadata {
    pub display_agent: Option<String>,
    pub title: Option<String>,
    /// At most [`MAX_TOKENS`], keys `^[A-Za-z0-9_-]{1,32}$` (others are dropped).
    pub tokens: BTreeMap<String, String>,
}

/// Whether herdr accepts a metadata token key.
pub fn valid_token_key(key: &str) -> bool {
    (1..=32).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl Metadata {
    fn sanitized(mut self) -> Metadata {
        self.tokens.retain(|k, _| valid_token_key(k));
        while self.tokens.len() > MAX_TOKENS {
            let last = self.tokens.keys().next_back().cloned().expect("non-empty");
            self.tokens.remove(&last);
        }
        self
    }
}

/// Whether a run's final answer asks the user something: its last paragraph ends with a question mark (after
/// closing emphasis, quotes and brackets).
pub fn ends_with_question(text: &str) -> bool {
    let trimmed = text.trim();
    let last = trimmed.rsplit("\n\n").next().unwrap_or(trimmed);
    let last = last.trim_end_matches(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '*' | '_' | '`' | ')' | ']' | '"' | '\'' | '\u{201d}' | '\u{2019}' | '\u{bb}'
            )
    });
    last.ends_with('?') || last.ends_with('\u{ff1f}')
}

/// Whether a failed run needs the user: an auth, model or usage-limit error.
pub fn needs_user(error: &str) -> bool {
    const MARKERS: [&str; 24] = [
        "api key",
        "api_key",
        "apikey",
        "unauthorized",
        "401",
        "403",
        "forbidden",
        "authentication",
        "not logged in",
        "log in",
        "login",
        "credential",
        "oauth",
        "no model",
        "model not found",
        "unknown model",
        "not a model",
        "usage limit",
        "rate limit",
        "limit reached",
        "quota",
        "insufficient",
        "billing",
        "429",
    ];
    let lower = error.to_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}

/// Where requests go.
pub trait Transport: Send + 'static {
    /// Send one request line (without its newline); `Err` when it could not be delivered.
    fn send(&mut self, line: &str) -> Result<(), String>;
}

/// herdr's unix socket: one connection per request, [`IO_TIMEOUT`] to write it and to read the reply line.
pub struct SocketTransport {
    pub path: PathBuf,
}

/// A connection to a unix socket whose connect, writes and reads each give up after [`IO_TIMEOUT`]. The send timeout
/// is set before `connect`: a listener whose backlog is full (a herdr that is stuck) makes a unix `connect` wait for
/// up to that timeout instead of forever.
pub fn connect(path: &std::path::Path) -> std::io::Result<UnixStream> {
    use rustix::net::sockopt::{Timeout, set_socket_timeout};
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let fd = rustix::net::socket_with(AddressFamily::UNIX, SocketType::STREAM, SocketFlags::CLOEXEC, None)?;
    set_socket_timeout(&fd, Timeout::Send, Some(IO_TIMEOUT))?;
    set_socket_timeout(&fd, Timeout::Recv, Some(IO_TIMEOUT))?;
    rustix::net::connect(&fd, &SocketAddrUnix::new(path)?)?;
    Ok(UnixStream::from(fd))
}

impl Transport for SocketTransport {
    fn send(&mut self, line: &str) -> Result<(), String> {
        let mut stream = connect(&self.path).map_err(|e| format!("{}: {e}", self.path.display()))?;
        stream
            .write_all(format!("{line}\n").as_bytes())
            .map_err(|e| e.to_string())?;
        // The reply is read (so herdr is not cut off mid-answer) and ignored.
        let mut reply = [0u8; 4096];
        let mut read = 0;
        while read < reply.len() {
            match stream.read(&mut reply[read..]) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    read += n;
                    if reply[..read].contains(&b'\n') {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct Queue {
    session: Option<Session>,
    session_dirty: bool,
    /// The pending state, its message, and when it was set.
    state: Option<(State, Option<String>, Instant)>,
    metadata: Option<Metadata>,
    sent_metadata: Option<Metadata>,
    metadata_at: Option<Instant>,
    releasing: bool,
    released: bool,
    seq: u64,
    logged: bool,
}

struct Shared {
    pane_id: String,
    agent: String,
    debug: bool,
    queue: Mutex<Queue>,
    wake: Condvar,
    done: Mutex<bool>,
    done_wake: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
}

/// `<source>:<ms>:<6 digits>`.
fn request_id(seq: u64) -> String {
    let ms = now_ns() / 1_000_000;
    let mix = seq ^ u64::from(std::process::id()).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    format!("{SOURCE}:{ms}:{:06}", mix % 1_000_000)
}

enum Next {
    Send(String),
    Wait(Option<Duration>),
    Exit,
}

impl Shared {
    fn request(&self, q: &mut Queue, method: &str, mut params: Map<String, Value>) -> String {
        q.seq = (q.seq + 1).max(now_ns());
        params.insert("seq".into(), q.seq.into());
        let mut head = Map::new();
        head.insert("pane_id".into(), self.pane_id.clone().into());
        head.insert("source".into(), SOURCE.into());
        head.insert("agent".into(), self.agent.clone().into());
        head.extend(params);
        json!({"id": request_id(q.seq), "method": method, "params": head}).to_string()
    }

    fn session_fields(q: &Queue, params: &mut Map<String, Value>) {
        if let Some(session) = &q.session {
            params.insert("agent_session_id".into(), session.id.clone().into());
            if let Some(path) = &session.path {
                params.insert("agent_session_path".into(), path.clone().into());
            }
            if let Some(argv) = &session.resume_argv {
                params.insert("resume_argv".into(), json!(argv));
            }
        }
    }

    fn state_request(&self, q: &mut Queue) -> String {
        let (state, message, _) = q.state.take().expect("pending state");
        let mut params = Map::new();
        params.insert("state".into(), state.as_str().into());
        if let Some(message) = message {
            params.insert("message".into(), message.into());
        }
        Self::session_fields(q, &mut params);
        self.request(q, "pane.report_agent", params)
    }

    fn next(&self, q: &mut Queue, now: Instant) -> Next {
        if q.releasing {
            // The last state is sent at once (a `-p` run's end), then the release.
            if q.state.is_some() {
                return Next::Send(self.state_request(q));
            }
            if q.released {
                return Next::Exit;
            }
            q.released = true;
            return Next::Send(self.request(q, "pane.release_agent", Map::new()));
        }
        let mut wait: Option<Duration> = None;
        if q.session_dirty {
            q.session_dirty = false;
            let mut params = Map::new();
            Self::session_fields(q, &mut params);
            if let Some(source) = q.session.as_ref().and_then(|s| s.start_source.clone()) {
                params.insert("session_start_source".into(), source.into());
            }
            return Next::Send(self.request(q, "pane.report_agent_session", params));
        }
        if let Some((state, _, at)) = &q.state {
            let ready_at = if *state == State::Working { *at } else { *at + SETTLE };
            if now >= ready_at {
                return Next::Send(self.state_request(q));
            }
            wait = Some(ready_at - now);
        }
        if q.metadata.is_some() {
            let ready_at = q.metadata_at.map_or(now, |at| at + METADATA_INTERVAL);
            if now >= ready_at {
                let metadata = q.metadata.take().expect("pending metadata");
                if let Some(params) = metadata_params(q.sent_metadata.as_ref(), &metadata) {
                    q.metadata_at = Some(now);
                    q.sent_metadata = Some(metadata);
                    return Next::Send(self.request(q, "pane.report_metadata", params));
                }
            } else {
                let left = ready_at - now;
                wait = Some(wait.map_or(left, |w| w.min(left)));
            }
        }
        Next::Wait(wait)
    }
}

/// The fields of a metadata report from what was sent last; `None` when nothing changed.
fn metadata_params(sent: Option<&Metadata>, now: &Metadata) -> Option<Map<String, Value>> {
    let empty = Metadata::default();
    let sent = sent.unwrap_or(&empty);
    if sent == now {
        return None;
    }
    let mut params = Map::new();
    match &now.display_agent {
        Some(name) => {
            params.insert("display_agent".into(), name.clone().into());
        }
        None if sent.display_agent.is_some() => {
            params.insert("clear_display_agent".into(), true.into());
        }
        None => {}
    }
    match &now.title {
        Some(title) => {
            params.insert("title".into(), title.clone().into());
        }
        None if sent.title.is_some() => {
            params.insert("clear_title".into(), true.into());
        }
        None => {}
    }
    let mut tokens = Map::new();
    for (key, value) in &now.tokens {
        tokens.insert(key.clone(), value.clone().into());
    }
    // A token no longer known is cleared.
    for key in sent.tokens.keys().filter(|k| !now.tokens.contains_key(*k)) {
        if tokens.len() < MAX_TOKENS {
            tokens.insert(key.clone(), Value::Null);
        }
    }
    if !tokens.is_empty() {
        params.insert("tokens".into(), Value::Object(tokens));
    }
    Some(params)
}

fn run(shared: Arc<Shared>, mut transport: Box<dyn Transport>) {
    loop {
        let line = {
            let mut q = lock(&shared.queue);
            loop {
                match shared.next(&mut q, Instant::now()) {
                    Next::Send(line) => break Some(line),
                    Next::Exit => break None,
                    Next::Wait(None) => q = shared.wake.wait(q).unwrap_or_else(PoisonError::into_inner),
                    Next::Wait(Some(left)) => {
                        q = shared
                            .wake
                            .wait_timeout(q, left)
                            .unwrap_or_else(PoisonError::into_inner)
                            .0
                    }
                }
            }
        };
        let Some(line) = line else { break };
        if let Err(error) = transport.send(&line) {
            let mut q = lock(&shared.queue);
            if shared.debug && !q.logged {
                eprintln!("ultron: herdr report failed: {error}");
            }
            q.logged = true;
        }
    }
    *lock(&shared.done) = true;
    shared.done_wake.notify_all();
}

/// The reporter of one process's agent in one pane; dropping it releases the agent.
pub struct Reporter {
    shared: Arc<Shared>,
}

static ACTIVE: Mutex<Option<Weak<Shared>>> = Mutex::new(None);

impl Reporter {
    /// A reporter to the pane's socket.
    pub fn start(pane: &Pane, agent: &str) -> Reporter {
        Reporter::with_transport(
            &pane.pane_id,
            agent,
            pane.debug,
            Box::new(SocketTransport {
                path: pane.socket.clone(),
            }),
        )
    }

    /// A reporter over any transport (tests).
    pub fn with_transport(pane_id: &str, agent: &str, debug: bool, transport: Box<dyn Transport>) -> Reporter {
        let shared = Arc::new(Shared {
            pane_id: pane_id.to_string(),
            agent: agent.to_string(),
            debug,
            queue: Mutex::default(),
            wake: Condvar::new(),
            done: Mutex::new(false),
            done_wake: Condvar::new(),
        });
        let worker = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("herdr-reporter".into())
            .spawn(move || run(worker, transport));
        if spawned.is_err() {
            *lock(&shared.done) = true;
        }
        *lock(&ACTIVE) = Some(Arc::downgrade(&shared));
        Reporter { shared }
    }

    fn update(&self, change: impl FnOnce(&mut Queue)) {
        let mut q = lock(&self.shared.queue);
        if q.releasing {
            return;
        }
        change(&mut q);
        drop(q);
        self.shared.wake.notify_all();
    }

    /// The pane's session (sent as `pane.report_agent_session` when it changed; later state reports carry it).
    pub fn session(&self, session: Session) {
        self.update(|q| {
            if q.session.as_ref() != Some(&session) {
                q.session = Some(session);
                q.session_dirty = true;
            }
        });
    }

    /// The agent's state; replaces a state not sent yet.
    pub fn state(&self, state: State, message: Option<String>) {
        self.update(|q| q.state = Some((state, message, Instant::now())));
    }

    /// The pane's metadata; sent when it differs from what was sent last, at most once a [`METADATA_INTERVAL`].
    pub fn metadata(&self, metadata: Metadata) {
        let metadata = metadata.sanitized();
        self.update(|q| {
            q.metadata = (q.sent_metadata.as_ref() != Some(&metadata)).then_some(metadata);
        });
    }

    /// Send the pending state at once, then `pane.release_agent` (dropping pending metadata), and wait for it at most [`RELEASE_WAIT`]. Later
    /// calls and updates do nothing.
    pub fn release(&self) {
        release(&self.shared);
    }
}

fn release(shared: &Shared) {
    {
        let mut q = lock(&shared.queue);
        if !q.releasing {
            q.releasing = true;
            q.metadata = None;
            q.session_dirty = false;
        }
    }
    shared.wake.notify_all();
    let deadline = Instant::now() + RELEASE_WAIT;
    let mut done = lock(&shared.done);
    while !*done {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        done = shared
            .done_wake
            .wait_timeout(done, deadline - now)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        release(&self.shared);
    }
}

/// Release the most recently started reporter that is still alive (for signal handlers that exit the process
/// without running destructors).
pub fn release_active() {
    let active = lock(&ACTIVE).as_ref().and_then(Weak::upgrade);
    if let Some(shared) = active {
        release(&shared);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        Shared {
            pane_id: "p1".into(),
            agent: AGENT.into(),
            debug: false,
            queue: Mutex::default(),
            wake: Condvar::new(),
            done: Mutex::new(false),
            done_wake: Condvar::new(),
        }
    }

    fn sent(next: Next) -> Value {
        match next {
            Next::Send(line) => serde_json::from_str(&line).unwrap(),
            Next::Wait(w) => panic!("waits {w:?}"),
            Next::Exit => panic!("exits"),
        }
    }

    fn waits(next: Next) -> Option<Duration> {
        match next {
            Next::Wait(w) => w,
            Next::Send(line) => panic!("sends {line}"),
            Next::Exit => panic!("exits"),
        }
    }

    #[test]
    fn herdr_idle_is_held_for_the_settle_time_and_working_is_not() {
        let s = shared();
        let mut q = Queue::default();
        let t0 = Instant::now();
        q.state = Some((State::Idle, None, t0));
        assert_eq!(
            waits(s.next(&mut q, t0 + Duration::from_millis(100))),
            Some(SETTLE - Duration::from_millis(100))
        );
        // A continuation that starts within the settle time replaces the idle state: only `working` is sent.
        q.state = Some((State::Working, None, t0 + Duration::from_millis(150)));
        let line = sent(s.next(&mut q, t0 + Duration::from_millis(150)));
        assert_eq!(line["params"]["state"], "working");
        assert_eq!(waits(s.next(&mut q, t0 + Duration::from_secs(9))), None);
        q.state = Some((State::Blocked, Some("Which file?".into()), t0));
        let line = sent(s.next(&mut q, t0 + SETTLE));
        assert_eq!(line["params"]["state"], "blocked");
        assert_eq!(line["params"]["message"], "Which file?");
    }

    #[test]
    fn herdr_metadata_is_sent_at_most_once_a_second_and_only_when_changed() {
        let s = shared();
        let mut q = Queue::default();
        let t0 = Instant::now();
        let meta = |cost: &str| Metadata {
            display_agent: Some("Ultron".into()),
            title: None,
            tokens: [("cost".to_string(), cost.to_string())].into(),
        };
        q.metadata = Some(meta("$0.01"));
        let line = sent(s.next(&mut q, t0));
        assert_eq!(line["method"], "pane.report_metadata");
        assert_eq!(line["params"]["tokens"], json!({"cost": "$0.01"}));
        q.metadata = Some(meta("$0.02"));
        assert_eq!(
            waits(s.next(&mut q, t0 + Duration::from_millis(400))),
            Some(Duration::from_millis(600))
        );
        let line = sent(s.next(&mut q, t0 + METADATA_INTERVAL));
        assert_eq!(line["params"]["tokens"], json!({"cost": "$0.02"}));
        // The same metadata again is not sent.
        q.metadata = Some(meta("$0.02"));
        assert_eq!(waits(s.next(&mut q, t0 + Duration::from_secs(5))), None);
        assert!(q.metadata.is_none());
    }

    #[test]
    fn herdr_metadata_clears_what_is_no_longer_known() {
        let before = Metadata {
            display_agent: Some("Ultron".into()),
            title: Some("Fix the parser".into()),
            tokens: [("ctx", "12%"), ("model", "m")]
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .into(),
        };
        let after = Metadata {
            display_agent: Some("Ultron".into()),
            title: None,
            tokens: [("model".to_string(), "m".to_string())].into(),
        };
        assert_eq!(
            Value::Object(metadata_params(Some(&before), &after).unwrap()),
            json!({"display_agent": "Ultron", "clear_title": true, "tokens": {"model": "m", "ctx": null}})
        );
        assert_eq!(metadata_params(Some(&after), &after), None);
    }

    #[test]
    fn herdr_release_flushes_the_pending_state_and_is_sent_once() {
        let s = shared();
        let t0 = Instant::now();
        let mut q = Queue {
            state: Some((State::Idle, None, t0)),
            metadata: Some(Metadata::default()),
            ..Queue::default()
        };
        q.releasing = true;
        q.metadata = None;
        // Within the settle time, a release sends the idle state at once.
        assert_eq!(sent(s.next(&mut q, t0))["params"]["state"], "idle");
        let line = sent(s.next(&mut q, t0));
        assert_eq!(line["method"], "pane.release_agent");
        assert_eq!(
            line["params"].as_object().unwrap().keys().collect::<Vec<_>>(),
            ["pane_id", "source", "agent", "seq"]
        );
        assert!(matches!(s.next(&mut q, Instant::now()), Next::Exit));
    }

    #[test]
    fn herdr_seq_is_strictly_increasing() {
        let s = shared();
        let mut q = Queue {
            seq: u64::MAX / 2,
            ..Queue::default()
        };
        let a = s.request(&mut q, "m", Map::new());
        let b = s.request(&mut q, "m", Map::new());
        let seq = |l: &str| {
            serde_json::from_str::<Value>(l).unwrap()["params"]["seq"]
                .as_u64()
                .unwrap()
        };
        assert_eq!(seq(&a), u64::MAX / 2 + 1);
        assert_eq!(seq(&b), u64::MAX / 2 + 2);
    }
}
