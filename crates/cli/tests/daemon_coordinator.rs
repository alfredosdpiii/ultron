//! Replays `crates/daemon/fixtures/coordinator.json` (recorded by `generate-coordinator.mjs` from the TypeScript
//! coordinator process): the same scripted control and public socket exchange against `ultron __internal coordinator`
//! must deliver byte-identical lines to every socket, close the same sockets, and end in the same empty shutdown.

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Seen {
    lines: Vec<String>,
    closed: bool,
}

type Shared = Arc<Mutex<BTreeMap<String, Seen>>>;

fn reader(seen: Shared, name: String, mut stream: UnixStream, lines: bool) {
    std::thread::spawn(move || {
        let mut pending = Vec::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut seen = seen.lock().unwrap();
            let entry = seen.get_mut(&name).unwrap();
            if !lines {
                entry.lines.push(String::from_utf8_lossy(&buffer[..n]).into_owned());
                continue;
            }
            pending.extend_from_slice(&buffer[..n]);
            while let Some(i) = pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = pending.drain(..=i).collect();
                entry.lines.push(String::from_utf8(line).unwrap());
            }
        }
        seen.lock().unwrap().get_mut(&name).unwrap().closed = true;
    });
}

/// Waits for `check` (hang guard 120 s).
fn wait_for(check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !check() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn cli_daemon_coordinator_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../../daemon/fixtures/coordinator.json")).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let tmp_text = root.display().to_string();
    let (public, control) = (root.join("public.sock"), root.join("control.sock"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_ultron"))
        .args([
            "__internal",
            "coordinator",
            public.to_str().unwrap(),
            control.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    wait_for(|| public.exists());
    let seen: Shared = Arc::default();
    let mut streams: BTreeMap<String, UnixStream> = BTreeMap::new();
    let mut exit = None;
    for step in fixture["script"].as_array().unwrap() {
        let name = step["name"].as_str().unwrap_or_default().to_string();
        match step["op"].as_str().unwrap() {
            "endpoint" => {
                seen.lock().unwrap().insert(name.clone(), Seen::default());
                let listener = UnixListener::bind(root.join(format!("{name}.sock"))).unwrap();
                let seen = seen.clone();
                std::thread::spawn(move || {
                    for mut stream in listener.incoming().flatten() {
                        let mut buffer = [0u8; 4096];
                        while let Ok(n) = stream.read(&mut buffer) {
                            if n == 0 {
                                break;
                            }
                            let data = String::from_utf8_lossy(&buffer[..n]).into_owned();
                            seen.lock().unwrap().get_mut(&name).unwrap().lines.push(data.clone());
                            let _ = stream.write_all(format!("pong:{data}").as_bytes());
                        }
                    }
                });
            }
            "connect" => {
                seen.lock().unwrap().insert(name.clone(), Seen::default());
                let public_socket = step["to"] == "public";
                let stream = UnixStream::connect(if public_socket { &public } else { &control }).unwrap();
                reader(seen.clone(), name.clone(), stream.try_clone().unwrap(), !public_socket);
                streams.insert(name, stream);
            }
            "send" => {
                let line = format!("{}\n", step["line"]).replace("{TMP}", &tmp_text);
                // The fixture's lines are JSON.stringify output; serde writes the same text for these values.
                let _ = streams.get_mut(&name).unwrap().write_all(line.as_bytes());
            }
            "write" => {
                let data = step["data"].as_str().unwrap();
                streams.get_mut(&name).unwrap().write_all(data.as_bytes()).unwrap();
            }
            "expect" => {
                let count = step["count"].as_u64().unwrap() as usize;
                let must_close = step["closed"] == true;
                wait_for(|| {
                    let seen = seen.lock().unwrap();
                    let entry = &seen[&name];
                    entry.lines.len() >= count && (!must_close || entry.closed)
                });
            }
            "close" => {
                let stream = streams.remove(&name).unwrap();
                stream.shutdown(std::net::Shutdown::Both).unwrap();
            }
            "exit" => {
                let deadline = Instant::now() + Duration::from_secs(120);
                exit = loop {
                    if let Some(status) = child.try_wait().unwrap() {
                        break status.code();
                    }
                    assert!(Instant::now() < deadline, "the coordinator did not exit when empty");
                    std::thread::sleep(Duration::from_millis(10));
                };
            }
            other => panic!("unknown step {other}"),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let want = &fixture["received"];
    let seen = seen.lock().unwrap();
    for (name, entry) in seen.iter() {
        let lines: Vec<String> = entry.lines.iter().map(|l| l.replace(&tmp_text, "<TMP>")).collect();
        let want_lines: Vec<String> = serde_json::from_value(want[name]["lines"].clone()).unwrap();
        assert_eq!(lines, want_lines, "{name} lines");
        assert_eq!(Value::Bool(entry.closed), want[name]["closed"], "{name} closed");
    }
    assert_eq!(
        exit.map(|c| c.to_string()),
        want["exit"]["lines"][0].as_str().map(str::to_string)
    );
    let sockets = [public.exists(), control.exists()].map(|e| e.to_string());
    assert_eq!(serde_json::to_value(sockets).unwrap(), want["sockets"]["lines"]);
    assert!(!Path::new(&public).exists());
}

#[path = "daemon_common/mod.rs"]
mod common;

/// A server generation whose leased coordinator exits before registering it (its empty shutdown fired while the
/// lease sat unaccepted, and it removed its control socket) starts a live coordinator and registers there, instead
/// of failing with the removed socket's `No such file or directory`.
#[test]
fn cli_daemon_server_registers_again_when_its_leased_coordinator_exits() {
    let provider = common::provider(Vec::new(), Duration::ZERO);
    let home = common::Home::new(provider.port);
    let sockets = daemon::instance::socket_dir(&home.server_dir());
    daemon::paths::ensure_private_directory(&sockets).unwrap();
    let control = sockets.join(format!("control-{}.sock", common::SERVER_ID));
    // The exiting coordinator: takes the lease connection, then removes its socket and goes away unanswered.
    let listener = UnixListener::bind(&control).unwrap();
    let exiting = {
        let control = control.clone();
        std::thread::spawn(move || {
            let lease = listener.accept().unwrap();
            std::fs::remove_file(&control).unwrap();
            drop((listener, lease));
        })
    };
    let model = r#"{"model":"anthropic/claude-sonnet-4-5"}"#;
    let mut child = home
        .command(&[
            "__internal",
            "server",
            home.server_dir().to_str().unwrap(),
            common::SERVER_ID,
            home.sessions().to_str().unwrap(),
            model,
        ])
        .env("__ULTRON_SERVER_KEEP_ALIVE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    exiting.join().unwrap();
    let socket = sockets.join(format!("{}.sock", common::SERVER_ID));
    let deadline = Instant::now() + common::HANG_GUARD;
    while client::runtime::Connection::connect(&socket, common::SERVER_ID).is_err() {
        if let Some(status) = child.try_wait().unwrap() {
            let mut stderr = String::new();
            child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
            panic!("the server exited ({status}): {stderr}");
        }
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    common::assert_no_processes(&home.root, common::HANG_GUARD);
}
