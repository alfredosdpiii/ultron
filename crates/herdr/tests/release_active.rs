//! [`herdr::release_active`] (what signal handlers call before they exit the process): its own test binary, since
//! it acts on the process's most recent reporter.

use herdr::{Pane, Reporter, State, release_active};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::mpsc;
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
fn herdr_release_active_releases_the_live_reporter() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("herdr.sock");
    let rx = fake_herdr(&socket);
    let reporter = Reporter::start(&pane(&socket), "ultron");
    reporter.state(State::Working, None);
    assert_eq!(next(&rx)["params"]["state"], "working");
    release_active();
    assert_eq!(next(&rx)["method"], "pane.release_agent");
    // Dropping it afterwards sends nothing more.
    drop(reporter);
    assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
}
