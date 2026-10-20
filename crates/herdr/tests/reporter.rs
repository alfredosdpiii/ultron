//! The herdr reporter against a fake herdr socket in a temp dir (never the real herdr): the exact request lines,
//! latest-state-wins while the socket is busy, updates that never wait for the socket, a release that is bounded
//! when herdr never answers, and env detection from the injected lookup only.

use herdr::{Metadata, Pane, Reporter, Session, State, Transport, detect, ends_with_question, needs_user};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const HANG_GUARD: Duration = Duration::from_secs(120);

/// A fake herdr: every request line, answered with `{"id", "result": {}}`.
fn fake_herdr(path: &Path) -> mpsc::Receiver<Value> {
    let listener = UnixListener::bind(path).unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut line = String::new();
            if BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .is_err()
            {
                continue;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            let _ = writeln!(stream, "{}", json!({"id": request["id"], "result": {}}));
            if tx.send(request).is_err() {
                return;
            }
        }
    });
    rx
}

/// A request without its varying `id` and `seq` (both checked for their shape).
fn stable(request: Value) -> Value {
    let id = request["id"].as_str().unwrap().to_string();
    let parts: Vec<&str> = id.split(':').collect();
    assert_eq!(parts.len(), 3, "{id}");
    assert_eq!(parts[0], "ultron");
    assert!(parts[1].parse::<u64>().is_ok() && parts[2].len() == 6, "{id}");
    let mut request = request;
    let seq = request["params"].as_object_mut().unwrap().remove("seq").unwrap();
    assert!(seq.as_u64().is_some_and(|s| s > 0), "{seq}");
    request.as_object_mut().unwrap().remove("id");
    request
}

fn next(rx: &mpsc::Receiver<Value>) -> Value {
    rx.recv_timeout(HANG_GUARD).expect("a herdr request")
}

fn pane(socket: &Path) -> Pane {
    Pane {
        socket: socket.to_path_buf(),
        pane_id: "w1:t1:p3".into(),
        debug: false,
    }
}

#[test]
fn herdr_reporter_sends_session_state_metadata_and_release_lines() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let rx = fake_herdr(&socket);
    let reporter = Reporter::start(&pane(&socket), "ultron");
    reporter.session(Session {
        id: "s-1".into(),
        path: Some("/tmp/s-1.jsonl".into()),
        resume_argv: Some(vec!["ultron".into(), "--session".into(), "s-1".into()]),
        start_source: Some("startup".into()),
    });
    assert_eq!(
        stable(next(&rx)),
        json!({"method": "pane.report_agent_session", "params": {"pane_id": "w1:t1:p3", "source": "ultron",
            "agent": "ultron", "agent_session_id": "s-1", "agent_session_path": "/tmp/s-1.jsonl",
            "resume_argv": ["ultron", "--session", "s-1"], "session_start_source": "startup"}})
    );
    reporter.state(State::Working, None);
    assert_eq!(
        stable(next(&rx)),
        json!({"method": "pane.report_agent", "params": {"pane_id": "w1:t1:p3", "source": "ultron",
            "agent": "ultron", "state": "working", "agent_session_id": "s-1",
            "agent_session_path": "/tmp/s-1.jsonl", "resume_argv": ["ultron", "--session", "s-1"]}})
    );
    reporter.metadata(Metadata {
        display_agent: Some("Ultron".into()),
        title: Some("Fix the parser".into()),
        tokens: [("model", "claude-sonnet-4-5"), ("bad key!", "x")]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into(),
    });
    assert_eq!(
        stable(next(&rx)),
        json!({"method": "pane.report_metadata", "params": {"pane_id": "w1:t1:p3", "source": "ultron",
            "agent": "ultron", "display_agent": "Ultron", "title": "Fix the parser",
            "tokens": {"model": "claude-sonnet-4-5"}}})
    );
    reporter.state(State::Blocked, Some("Which file?".into()));
    assert_eq!(stable(next(&rx))["params"]["state"], "blocked");
    drop(reporter);
    assert_eq!(
        stable(next(&rx)),
        json!({"method": "pane.release_agent", "params": {"pane_id": "w1:t1:p3", "source": "ultron",
            "agent": "ultron"}})
    );
}

/// The gate is open, sends started, lines sent.
type Gate = Mutex<(bool, usize, Vec<Value>)>;

/// A transport that blocks each send until the test opens its gate; records the lines.
#[derive(Clone, Default)]
struct Gated {
    inner: Arc<(Gate, Condvar)>,
}

impl Gated {
    fn open(&self) {
        self.inner.0.lock().unwrap().0 = true;
        self.inner.1.notify_all();
    }

    /// Wait until `n` sends started.
    fn wait_started(&self, n: usize) {
        let mut state = self.inner.0.lock().unwrap();
        while state.1 < n {
            state = self.inner.1.wait_timeout(state, HANG_GUARD).unwrap().0;
        }
    }

    fn wait_lines(&self, n: usize) -> Vec<Value> {
        let mut state = self.inner.0.lock().unwrap();
        while state.2.len() < n {
            state = self.inner.1.wait_timeout(state, HANG_GUARD).unwrap().0;
        }
        state.2.clone()
    }
}

impl Transport for Gated {
    fn send(&mut self, line: &str) -> Result<(), String> {
        let mut state = self.inner.0.lock().unwrap();
        state.1 += 1;
        self.inner.1.notify_all();
        while !state.0 {
            state = self.inner.1.wait(state).unwrap();
        }
        state.2.push(serde_json::from_str(line).unwrap());
        self.inner.1.notify_all();
        Ok(())
    }
}

#[test]
fn herdr_updates_never_wait_for_the_socket_and_the_latest_state_wins() {
    let gate = Gated::default();
    let reporter = Reporter::with_transport("p", "ultron", false, Box::new(gate.clone()));
    reporter.state(State::Working, Some("first".into()));
    // The reporter thread is now stuck in herdr; every update still returns at once.
    gate.wait_started(1);
    for n in 0..1_000 {
        reporter.state(State::Working, Some(format!("update {n}")));
        reporter.metadata(Metadata {
            title: Some(format!("t{n}")),
            ..Metadata::default()
        });
    }
    reporter.state(State::Working, Some("last".into()));
    gate.open();
    let lines = gate.wait_lines(3);
    assert_eq!(lines[0]["params"]["message"], "first");
    assert_eq!(lines[1]["params"]["message"], "last");
    assert_eq!(lines[2]["params"]["title"], "t999");
    drop(reporter);
    let lines = gate.wait_lines(4);
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[3]["method"], "pane.release_agent");
}

#[test]
fn herdr_release_is_bounded_when_herdr_never_answers() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    // Listening, never accepting: connects queue in the backlog, replies never come.
    let _listener = UnixListener::bind(&socket).unwrap();
    let gate = Gated::default();
    let reporter = Reporter::with_transport("p", "ultron", false, Box::new(gate.clone()));
    reporter.state(State::Working, None);
    gate.wait_started(1);
    // The transport never returns; the release waits at most RELEASE_WAIT, then the process may exit.
    reporter.release();
    reporter.state(State::Idle, None);
    drop(reporter);
    // The socket transport itself gives up after its timeouts.
    let real = Reporter::start(&pane(&socket), "ultron");
    real.state(State::Working, None);
    drop(real);
}

/// A herdr that is stuck: a listener with a backlog of 0 that never accepts, with the one queued connection already
/// taken, so the next unix `connect` to it waits.
fn stuck_herdr(path: &Path) -> (std::os::fd::OwnedFd, std::os::unix::net::UnixStream) {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let fd = rustix::net::socket_with(AddressFamily::UNIX, SocketType::STREAM, SocketFlags::CLOEXEC, None).unwrap();
    rustix::net::bind(&fd, &SocketAddrUnix::new(path).unwrap()).unwrap();
    rustix::net::listen(&fd, 0).unwrap();
    let queued = std::os::unix::net::UnixStream::connect(path).unwrap();
    (fd, queued)
}

#[test]
fn herdr_connect_gives_up_on_a_stuck_herdr_instead_of_waiting_forever() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let _stuck = stuck_herdr(&socket);
    let (tx, rx) = mpsc::channel();
    let path = socket.clone();
    std::thread::spawn(move || {
        let connected = herdr::connect(&path).map(|_| ());
        let sent = herdr::SocketTransport { path }.send("{}");
        let _ = tx.send((connected, sent));
    });
    // Without the connect timeout both wait for an accept that never comes (the hang guard fails the test).
    let (connected, sent) = rx.recv_timeout(HANG_GUARD).expect("connect gave up");
    assert!(connected.is_err());
    assert!(sent.is_err());
}

#[test]
fn herdr_missing_socket_is_silent() {
    let dir = tempfile::tempdir().unwrap();
    let reporter = Reporter::start(&pane(&dir.path().join("absent.sock")), "ultron");
    reporter.state(State::Working, None);
    reporter.release();
}

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
}

#[test]
fn herdr_detect_reads_the_injected_env_and_the_setting() {
    let inside = [
        ("HERDR_ENV", "1"),
        ("HERDR_SOCKET_PATH", "/x/h.sock"),
        ("HERDR_PANE_ID", "p9"),
    ];
    assert_eq!(
        detect(&vars(&inside), None),
        Some(Pane {
            socket: PathBuf::from("/x/h.sock"),
            pane_id: "p9".into(),
            debug: false,
        })
    );
    assert_eq!(
        detect(&vars(&inside), Some(&json!(true))).map(|p| p.pane_id),
        Some("p9".into())
    );
    assert_eq!(detect(&vars(&inside), Some(&json!(false))), None);
    for off in ["0", "false", "OFF", "no"] {
        let mut env = inside.to_vec();
        env.push(("ULTRON_HERDR", off));
        assert_eq!(detect(&vars(&env), None), None, "{off}");
    }
    let mut kernel = inside.to_vec();
    kernel.push(("ULTRON_RLM_MAX_MEMORY_MB", "4096"));
    assert_eq!(detect(&vars(&kernel), None), None);
    assert_eq!(detect(&vars(&inside[1..]), None), None);
    assert_eq!(detect(&vars(&[("HERDR_ENV", "1"), ("HERDR_PANE_ID", "p")]), None), None);
    assert_eq!(
        detect(
            &vars(&[("HERDR_ENV", "1"), ("HERDR_SOCKET_PATH", "/s"), ("HERDR_PANE_ID", "")]),
            None
        ),
        None
    );
    assert_eq!(
        detect(
            &vars(&[("HERDR_ENV", "0"), ("HERDR_SOCKET_PATH", "/s"), ("HERDR_PANE_ID", "p")]),
            None
        ),
        None
    );
}

#[test]
fn herdr_question_and_needs_user_rules() {
    assert!(ends_with_question("Done.\n\nShould I also update the docs?"));
    assert!(ends_with_question("Which one do you want: **A or B?**"));
    assert!(ends_with_question("Proceed? \n"));
    assert!(!ends_with_question("Is it fixed? Yes.\n\nAll tests pass."));
    assert!(!ends_with_question("Why? Because.\n\nDone"));
    assert!(!ends_with_question(""));
    assert!(needs_user("401 Unauthorized: invalid x-api-key"));
    assert!(needs_user("No API key found for anthropic"));
    assert!(needs_user("Claude usage limit reached"));
    assert!(needs_user("Model not found: foo/bar"));
    assert!(!needs_user("connection reset by peer"));
}
