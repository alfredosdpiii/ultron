//! Ultron Durable at process level: the `ultron` binary's server, coordinator and session worker processes, killed
//! and signalled for real, each test in private ULTRON_SERVER_DIR / ULTRON_CODING_AGENT_DIR / HOME temp dirs and
//! checking that no process it started is left running.
//!
//! - a worker killed mid-turn with a queued follow-up restarts by itself (no client attach), its interrupted run
//!   continues and the follow-up runs, and the transcript keeps everything committed;
//! - a worker that stops answering heartbeats is killed and restarted;
//! - two servers racing for one session: one worker owns it, the other is refused (`SessionInUseError`);
//! - a stuck run is reported (health, server log) and aborted through the session operations service;
//! - print and RPC clients whose coordinator is killed reconnect to a restarted server and finish the session;
//! - SIGINT aborts a print run (exit 130); SIGTERM shuts the coordinator and a server down with socket cleanup.

#[path = "daemon_common/mod.rs"]
mod common;

use client::runtime::{CallError, Connection};
use common::{Home, assert_no_processes, processes_mentioning, text_reply};
use protocol::RpcTarget;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MODEL: &str = "anthropic/claude-sonnet-4-5";
/// Every wait is on a condition; this only guards against a hang.
const HANG_GUARD: Duration = Duration::from_secs(120);
/// A provider reply that never comes while the test runs (the turn stays in flight until it is killed or aborted).
const STALL: Duration = Duration::from_secs(3600);

/// A scripted provider whose n-th reply waits its own delay; request bodies are recorded as they arrive.
struct Provider {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
}

fn provider(replies: Vec<(String, Duration)>) -> Provider {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { continue };
            let (reply, delay) = replies
                .get(i)
                .cloned()
                .unwrap_or_else(|| (text_reply("(no more replies)"), Duration::ZERO));
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() {
                        return;
                    }
                    if header.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = header.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        length = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                seen.lock()
                    .unwrap()
                    .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
                std::thread::sleep(delay);
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(reply.as_bytes());
            });
        }
    });
    Provider { port, requests }
}

impl Provider {
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn wait_for(&self, count: usize, what: &str) {
        wait_until(HANG_GUARD, what, || self.count() >= count);
    }
}

fn wait_until(timeout: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The user texts of a provider request body, in order.
fn user_texts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["text"].as_str().map(str::to_string))
        .filter(|t| !t.starts_with("[clock]"))
        .collect()
}

/// A detached server process (`runServerProcess`, held open) with extra environment.
struct Server {
    child: Child,
    id: String,
    socket: PathBuf,
}

impl Server {
    fn start(home: &Home, id: &str, env: &[(&str, &str)]) -> Server {
        let mut command = home.command(&[
            "__internal",
            "server",
            home.server_dir().to_str().unwrap(),
            id,
            home.sessions().to_str().unwrap(),
            r#"{"model":"anthropic/claude-sonnet-4-5"}"#,
        ]);
        command
            .env("__ULTRON_SERVER_KEEP_ALIVE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        for (k, v) in env {
            command.env(k, v);
        }
        let child = command.spawn().unwrap();
        let socket = home.server_dir().join(format!("{id}.sock"));
        wait_until(HANG_GUARD, "server start", || Connection::connect(&socket, id).is_ok());
        Server {
            child,
            id: id.into(),
            socket,
        }
    }

    fn connect(&self) -> Arc<Connection> {
        Connection::connect(&self.socket, &self.id).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn create(connection: &Connection) -> String {
    let created = connection
        .call(
            connection.server_target(),
            "pi.session-management",
            "create",
            json!([{}]),
        )
        .unwrap()
        .unwrap();
    created["sessionId"].as_str().unwrap().to_string()
}

fn attach(connection: &Connection, id: &str) -> Result<RpcTarget, CallError> {
    connection.call(
        connection.server_target(),
        "pi.session-management",
        "attach",
        json!([id]),
    )?;
    connection.wait_attachment(|a| a.is_some_and(|a| a.session_id == id))?;
    connection.session_target()
}

fn op(connection: &Connection, session: &RpcTarget, op: Value) -> Result<Option<Value>, CallError> {
    connection.call(session.clone(), "ultron.session-ops", "op", json!([op]))
}

/// The pid of the internal process of `role` whose command line mentions `needle` (and the test's root).
fn internal_pid(home: &Home, role: &str, needle: &str) -> Option<u32> {
    processes_mentioning(&home.root).into_iter().find(|pid| {
        std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| {
            let text = String::from_utf8_lossy(&c).into_owned();
            text.contains(role) && text.contains(needle)
        })
    })
}

fn worker_pid(home: &Home, session_id: &str) -> Option<u32> {
    internal_pid(home, "session-worker", session_id)
}

fn signal(pid: u32, signal: rustix::process::Signal) {
    rustix::process::kill_process(rustix::process::Pid::from_raw(pid as i32).unwrap(), signal).unwrap();
}

/// The session file whose header names `id`.
fn session_file(home: &Home, id: &str) -> PathBuf {
    std::fs::read_dir(home.sessions())
        .unwrap()
        .flatten()
        .flat_map(|d| std::fs::read_dir(d.path()).unwrap().flatten())
        .map(|f| f.path())
        .find(|p| {
            p.extension().is_some_and(|e| e == "jsonl")
                && std::fs::read_to_string(p).is_ok_and(|t| t.lines().next().is_some_and(|l| l.contains(id)))
        })
        .expect("the session file exists")
}

/// The committed writes of a format-4 session file, in order (read only: a live worker may be appending).
fn writes(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap();
    let complete = &text[..text.rfind('\n').map_or(0, |i| i + 1)];
    complete
        .lines()
        .skip(1)
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .flat_map(|t| t.as_array().cloned().unwrap_or_else(|| vec![t]))
        .collect()
}

/// The session's messages as `role: text [error]` lines, in commit order (its branch, for these linear sessions).
fn transcript(path: &Path) -> Vec<String> {
    writes(path)
        .iter()
        .filter(|w| w["kind"] == "entry" && w["type"] == "message")
        .map(|e| {
            let message = &e["message"];
            let text: String = message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["text"].as_str())
                .collect();
            let error = message["errorMessage"]
                .as_str()
                .map_or(String::new(), |e| format!(" [{e}]"));
            format!("{}: {text}{error}", message["role"].as_str().unwrap_or_default())
        })
        .collect()
}

/// Results of the session's settled operations, oldest first: `(status, error code)`.
fn results(path: &Path) -> Vec<(String, Option<String>)> {
    writes(path)
        .iter()
        .filter(|w| w["namespace"] == session_journal::run::OP_RESULT && w["op"] == "set")
        .map(|w| {
            (
                w["value"]["status"].as_str().unwrap_or_default().to_string(),
                w["value"]["error"]["code"].as_str().map(str::to_string),
            )
        })
        .collect()
}

#[test]
fn cli_durable_killed_worker_restarts_by_itself_and_runs_the_queued_follow_up() {
    let provider = provider(vec![
        (text_reply("Never seen."), STALL),
        (text_reply("Continued."), Duration::ZERO),
        (text_reply("Second."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    let server = Server::start(&home, common::SERVER_ID, &[]);
    let connection = server.connect();
    let id = create(&connection);
    let session = attach(&connection, &id).unwrap();
    let first_attachment = connection.attachment().unwrap().attachment_id;
    let prompting = connection
        .start(
            session.clone(),
            "pi.agent-controller",
            "prompt",
            json!([{"message": "first", "images": null}]),
        )
        .unwrap();
    provider.wait_for(1, "the first turn reached the provider");
    op(
        &connection,
        &session,
        json!({"op": "enqueue", "kind": "followUp", "message": "second"}),
    )
    .unwrap();
    let worker = worker_pid(&home, &id).expect("the worker runs");
    signal(worker, rustix::process::Signal::KILL);
    // The prompt in flight fails as lost; nothing else is asked of the client.
    match connection.wait(&prompting) {
        Err(CallError::Server(error)) => {
            assert_eq!(error.code, "session_worker_lost", "{}", error.message);
            // Whichever the server notices first: the process exit or the lost peer.
            assert!(
                error.message.contains("exited unexpectedly") || error.message.contains("disconnected unexpectedly"),
                "{}",
                error.message
            );
            assert!(daemon::lost::is_lost(&error.message), "{}", error.message);
        }
        other => panic!("the prompt must fail as lost, got {other:?}"),
    }
    // The server restarts the worker and re-attaches this client under a new attachment by itself.
    wait_until(HANG_GUARD, "re-attachment", || {
        connection
            .attachment()
            .is_some_and(|a| a.session_id == id && a.attachment_id != first_attachment)
    });
    let restarted = worker_pid(&home, &id).expect("a new worker runs");
    assert_ne!(restarted, worker);
    provider.wait_for(3, "the continuation and the follow-up ran");
    let session = connection.session_target().unwrap();
    wait_until(HANG_GUARD, "the session is idle", || {
        op(&connection, &session, json!({"op": "state"}))
            .ok()
            .flatten()
            .is_some_and(|s| s["isStreaming"] == false && s["pendingMessageCount"] == 0)
    });
    let requests = provider.requests.lock().unwrap().clone();
    // The durable generation resends the request it had pinned, byte for byte; the follow-up comes after.
    assert_eq!(requests[1], requests[0], "the resent request is the pinned one");
    assert_eq!(user_texts(&requests[2]), ["first", "second"]);
    let file = session_file(&home, &id);
    assert_eq!(
        transcript(&file),
        [
            "user: first",
            "assistant: Continued.",
            "user: second",
            "assistant: Second.",
        ]
    );
    assert_eq!(results(&file), [("completed".to_string(), None)]);
    let log = std::fs::read_to_string(home.server_dir().join(format!("server-{}.log", common::SERVER_ID))).unwrap();
    assert!(log.contains("restarting it in 250 ms (attempt 1 of 5)"), "{log}");
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
}

#[test]
fn cli_durable_busy_worker_without_clients_is_restarted_and_finishes() {
    let provider = provider(vec![
        (text_reply("Never seen."), STALL),
        (text_reply("Done after restart."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    let server = Server::start(&home, common::SERVER_ID, &[]);
    let connection = server.connect();
    let id = create(&connection);
    let session = attach(&connection, &id).unwrap();
    connection
        .start(
            session,
            "pi.agent-controller",
            "prompt",
            json!([{"message": "solo", "images": null}]),
        )
        .unwrap();
    provider.wait_for(1, "the turn reached the provider");
    // The client detaches (answered once the worker applied the withdrawn demand) and leaves; the busy worker keeps
    // its run.
    connection
        .call(connection.server_target(), "pi.session-management", "detach", json!([]))
        .unwrap();
    connection.disconnect();
    let worker = worker_pid(&home, &id).expect("the busy worker keeps running");
    signal(worker, rustix::process::Signal::KILL);
    provider.wait_for(2, "the restarted worker continued the run");
    let file = session_file(&home, &id);
    wait_until(HANG_GUARD, "the run committed", || {
        transcript(&file).last().map(String::as_str) == Some("assistant: Done after restart.")
    });
    // With nothing left to do and nobody attached, the restarted worker retires.
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
}

#[test]
fn cli_durable_worker_that_stops_answering_heartbeats_is_killed_and_restarted() {
    let provider = provider(vec![
        (text_reply("Never seen."), STALL),
        (text_reply("Back."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    let server = Server::start(&home, common::SERVER_ID, &[("ULTRON_WORKER_HEARTBEAT_MS", "200")]);
    let connection = server.connect();
    let id = create(&connection);
    let session = attach(&connection, &id).unwrap();
    connection
        .start(
            session,
            "pi.agent-controller",
            "prompt",
            json!([{"message": "hang", "images": null}]),
        )
        .unwrap();
    provider.wait_for(1, "the turn reached the provider");
    let worker = worker_pid(&home, &id).unwrap();
    // A stopped process is alive but answers nothing.
    signal(worker, rustix::process::Signal::STOP);
    wait_until(HANG_GUARD, "a new worker", || {
        worker_pid(&home, &id).is_some_and(|pid| pid != worker)
    });
    provider.wait_for(2, "the restarted worker continued the run");
    let log = std::fs::read_to_string(home.server_dir().join(format!("server-{}.log", common::SERVER_ID))).unwrap();
    assert!(
        log.contains(&format!("(pid {worker}) did not answer heartbeats")),
        "{log}"
    );
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
}

#[test]
fn cli_durable_two_servers_racing_for_one_session_one_is_refused() {
    let provider = provider(Vec::new());
    let home = Home::new(provider.port);
    let a = Server::start(&home, common::SERVER_ID, &[]);
    let b = Server::start(&home, "00000000-0000-4000-8000-0000000000bb", &[]);
    let (ca, cb) = (a.connect(), b.connect());
    let id = create(&ca);
    // Both servers see the session file; their attaches race to start a worker for it.
    let racers: Vec<_> = [ca.clone(), cb.clone()]
        .into_iter()
        .map(|c| {
            let id = id.clone();
            std::thread::spawn(move || attach(&c, &id).map(drop))
        })
        .collect();
    let outcomes: Vec<Result<(), CallError>> = racers.into_iter().map(|t| t.join().unwrap()).collect();
    let refused: Vec<&CallError> = outcomes.iter().filter_map(|o| o.as_ref().err()).collect();
    assert_eq!(refused.len(), 1, "exactly one attach is refused: {outcomes:?}");
    // The refused worker reports its refusal and then exits: the refusal can reach this client first, so the
    // loser may still be alive here (on a loaded machine, for a while). Wait for it to be gone before naming the
    // owner, or `worker_pid` may pick the loser.
    let workers = || -> Vec<u32> {
        processes_mentioning(&home.root)
            .into_iter()
            .filter(|pid| {
                std::fs::read(format!("/proc/{pid}/cmdline"))
                    .is_ok_and(|c| String::from_utf8_lossy(&c).contains("session-worker"))
            })
            .collect()
    };
    wait_until(HANG_GUARD, "the refused worker exits", || workers().len() == 1);
    let owner = worker_pid(&home, &id).expect("the winner's worker runs");
    match refused[0] {
        CallError::Server(error) => {
            assert_eq!(error.code, "service_invalid_value");
            assert_eq!(
                error.message,
                format!(
                    "Session {id} is open in another Ultron process (worker pid {owner}); close it there, or wait for its work to finish, then resume it"
                )
            );
        }
        other => panic!("refused with {other:?}"),
    }
    assert_eq!(workers(), [owner], "one worker owns the session");
    ca.disconnect();
    cb.disconnect();
    drop(a);
    drop(b);
    assert_no_processes(&home.root, HANG_GUARD);
}

#[test]
fn cli_durable_stuck_run_is_reported_and_aborted_through_session_ops() {
    let provider = provider(vec![(text_reply("Too late."), STALL)]);
    let home = Home::new(provider.port);
    let server = Server::start(&home, common::SERVER_ID, &[("ULTRON_STUCK_AFTER_MS", "500")]);
    let connection = server.connect();
    let id = create(&connection);
    let session = attach(&connection, &id).unwrap();
    let prompting = connection
        .start(
            session.clone(),
            "pi.agent-controller",
            "prompt",
            json!([{"message": "slow", "images": null}]),
        )
        .unwrap();
    provider.wait_for(1, "the turn reached the provider");
    let health = |connection: &Connection| {
        connection
            .call(session.clone(), "ultron.session-ops", "checkHealth", json!([]))
            .unwrap()
            .unwrap()
    };
    wait_until(HANG_GUARD, "the run is reported stuck", || {
        health(&connection)["stuck"] == true
    });
    let reported = health(&connection);
    assert_eq!(reported["running"], true);
    assert_eq!(reported["stuckAfterMs"], 500);
    assert!(reported["idleMs"].as_u64().unwrap() >= 500);
    let log_path = home.server_dir().join(format!("server-{}.log", common::SERVER_ID));
    wait_until(HANG_GUARD, "the server logs the stuck run", || {
        std::fs::read_to_string(&log_path).is_ok_and(|l| l.contains("made no progress"))
    });
    let aborted = connection
        .call(session.clone(), "ultron.session-ops", "abortStuck", json!([]))
        .unwrap()
        .unwrap();
    assert_eq!(aborted["aborted"], true);
    assert_eq!(aborted["restarted"], false);
    assert_eq!(aborted["runId"], reported["runId"]);
    let response = connection.wait(&prompting).unwrap().unwrap();
    assert_eq!(response["accepted"], true);
    assert_eq!(health(&connection)["running"], false);
    assert_eq!(results(&session_file(&home, &id)), [("aborted".to_string(), None)]);
    let nothing = connection
        .call(session, "ultron.session-ops", "abortStuck", json!([]))
        .unwrap()
        .unwrap();
    assert_eq!(nothing, json!({"aborted": false, "reason": "no run in progress"}));
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
}

fn coordinator_pid(home: &Home) -> Option<u32> {
    internal_pid(home, "coordinator", &home.server_dir().display().to_string())
}

#[test]
fn cli_durable_print_client_rejoins_after_its_coordinator_is_killed() {
    let provider = provider(vec![
        (text_reply("Never seen."), STALL),
        (text_reply("Recovered."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    let client = home
        .command(&["--mode", "json", "--model", MODEL, "first"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    provider.wait_for(1, "the turn reached the provider");
    let coordinator = coordinator_pid(&home).expect("the coordinator runs");
    signal(coordinator, rustix::process::Signal::KILL);
    let output = client.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["kind"], "prompted");
    assert_eq!(result["text"], "Recovered.");
    let replaced = coordinator_pid(&home);
    assert_ne!(replaced, Some(coordinator));
    let id = result["sessionId"].as_str().unwrap();
    let file = session_file(&home, id);
    assert_eq!(transcript(&file), ["user: first", "assistant: Recovered."]);
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests[1], requests[0], "the resent request is the pinned one");
    assert_no_processes(&home.root, HANG_GUARD);
}

/// Read RPC stdout lines until one satisfies `done`; returns every line read.
fn read_until(lines: &mut impl Iterator<Item = std::io::Result<String>>, done: impl Fn(&Value) -> bool) -> Vec<Value> {
    let mut seen = Vec::new();
    for line in lines.by_ref() {
        let value: Value = serde_json::from_str(&line.unwrap()).unwrap_or(Value::Null);
        let finished = done(&value);
        seen.push(value);
        if finished {
            return seen;
        }
    }
    panic!("RPC output ended: {seen:?}");
}

#[test]
fn cli_durable_rpc_client_rejoins_after_its_coordinator_is_killed() {
    let provider = provider(vec![
        (text_reply("One."), Duration::ZERO),
        (text_reply("Two."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    let mut child = home
        .command(&["--mode", "rpc", "--model", MODEL])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    let mut lines = std::iter::from_fn(|| rx.recv_timeout(HANG_GUARD).ok());
    writeln!(stdin, r#"{{"id":"1","type":"prompt","message":"one"}}"#).unwrap();
    read_until(&mut lines, |v| v["type"] == "agent_end");
    let coordinator = coordinator_pid(&home).expect("the coordinator runs");
    signal(coordinator, rustix::process::Signal::KILL);
    wait_until(HANG_GUARD, "a new coordinator", || {
        coordinator_pid(&home).is_some_and(|pid| pid != coordinator)
    });
    // The link rejoined the session on a new server: the next prompt runs with the whole transcript.
    writeln!(stdin, r#"{{"id":"2","type":"prompt","message":"two"}}"#).unwrap();
    let seen = read_until(&mut lines, |v| v["type"] == "agent_end");
    assert_eq!(
        seen.iter().filter(|v| v["type"] == "agent_end").count(),
        1,
        "only the new run ends: {seen:?}"
    );
    let response = seen.iter().find(|v| v["id"] == "2").expect("the prompt is answered");
    assert_eq!(response["success"], true, "{response}");
    assert!(
        seen.iter()
            .any(|v| v["type"] == "message_end" && v["message"]["content"][0]["text"] == "Two."),
        "{seen:?}"
    );
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(user_texts(&requests[1]), ["one", "two"]);
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success());
    assert_no_processes(&home.root, HANG_GUARD);
}

#[test]
fn cli_durable_sigint_aborts_the_print_turn() {
    let provider = provider(vec![(text_reply("Never seen."), STALL)]);
    let home = Home::new(provider.port);
    let client = home
        .command(&["--mode", "json", "--model", MODEL, "stop me"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    provider.wait_for(1, "the turn reached the provider");
    signal(client.id(), rustix::process::Signal::INT);
    let output = client.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    let sessions = std::fs::read_dir(home.sessions())
        .unwrap()
        .flatten()
        .flat_map(|d| std::fs::read_dir(d.path()).unwrap().flatten())
        .map(|f| f.path())
        .find(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .unwrap();
    assert_eq!(results(&sessions), [("aborted".to_string(), None)]);
    assert_no_processes(&home.root, HANG_GUARD);
}

#[test]
fn cli_durable_sigterm_shuts_the_coordinator_and_a_server_down_with_socket_cleanup() {
    let provider = provider(Vec::new());
    let home = Home::new(provider.port);
    let mut server = Server::start(&home, common::SERVER_ID, &[]);
    let sockets = || -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(home.server_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".sock"))
            .collect();
        names.sort();
        names
    };
    let id = common::SERVER_ID;
    assert!(
        sockets().iter().any(|n| n.starts_with(&format!("server-{id}-"))),
        "{:?}",
        sockets()
    );
    // The server generation: its endpoint goes, it exits 0; the coordinator stays (it has a grace period).
    signal(server.child.id(), rustix::process::Signal::TERM);
    let status = server.child.wait().unwrap();
    assert_eq!(status.code(), Some(0));
    assert!(!sockets().iter().any(|n| n.starts_with("server-")), "{:?}", sockets());
    let coordinator = coordinator_pid(&home).expect("the coordinator runs");
    let mut expected = vec![format!("{id}.sock"), format!("control-{id}.sock")];
    expected.sort();
    assert_eq!(sockets(), expected);
    signal(coordinator, rustix::process::Signal::TERM);
    wait_until(HANG_GUARD, "the coordinator exits", || coordinator_pid(&home).is_none());
    assert!(sockets().is_empty(), "{:?}", sockets());
    assert_no_processes(&home.root, HANG_GUARD);
}

/// An Anthropic SSE reply with one `rlm` call (`toolReply` of the fixture generators).
fn tool_reply(id: &str, code: &str) -> String {
    common::sse(&[
        json!({"type": "message_start", "message": {"id": "msg_2", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": id, "name": "rlm", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": json!({"code": code}).to_string()}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
        json!({"type": "message_stop"}),
    ])
}

/// A server, a new session attached, and its prompt `message` started; returns what the kill tests need.
struct Prompted {
    server: Server,
    connection: Arc<Connection>,
    id: String,
    prompting: String,
}

fn prompted(home: &Home, env: &[(&str, &str)], message: &str) -> Prompted {
    let server = Server::start(home, common::SERVER_ID, env);
    let connection = server.connect();
    let id = create(&connection);
    let session = attach(&connection, &id).unwrap();
    let prompting = connection
        .start(
            session,
            "pi.agent-controller",
            "prompt",
            json!([{"message": message, "images": null}]),
        )
        .unwrap();
    Prompted {
        server,
        connection,
        id,
        prompting,
    }
}

/// Wait until the (restarted) worker's session is idle with nothing queued.
fn wait_idle(connection: &Connection, id: &str) {
    wait_until(HANG_GUARD, "the session is idle", || {
        connection.attachment().is_some_and(|a| a.session_id == id)
            && connection.session_target().ok().is_some_and(|session| {
                op(connection, &session, json!({"op": "state"}))
                    .ok()
                    .flatten()
                    .is_some_and(|s| s["isStreaming"] == false && s["pendingMessageCount"] == 0)
            })
    });
}

/// The last tool result message of a request body (Anthropic `tool_result` blocks), as text.
fn tool_result_text(body: &Value) -> String {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .rfind(|c| c["type"] == "tool_result")
        .map(|c| match &c["content"] {
            Value::String(text) => text.clone(),
            blocks => blocks
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| b["text"].as_str())
                .collect(),
        })
        .unwrap_or_default()
}

/// Kill point: during the model call. The restarted worker's generation resends the pinned request (the same body)
/// and commits one answer: no interrupted message, no continuation prompt, no duplicate.
#[test]
fn cli_durable_kill_during_model_call_resends_the_pinned_request_once() {
    let provider = provider(vec![
        (text_reply("Never seen."), STALL),
        (text_reply("Answered once."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    let run = prompted(&home, &[], "first");
    provider.wait_for(1, "the turn reached the provider");
    let worker = worker_pid(&home, &run.id).expect("the worker runs");
    signal(worker, rustix::process::Signal::KILL);
    assert!(
        run.connection.wait(&run.prompting).is_err(),
        "the prompt in flight is lost"
    );
    provider.wait_for(2, "the restarted worker resent the request");
    wait_idle(&run.connection, &run.id);
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1], requests[0], "the resent request is the pinned one");
    let file = session_file(&home, &run.id);
    assert_eq!(transcript(&file), ["user: first", "assistant: Answered once."]);
    assert_eq!(results(&file), [("completed".to_string(), None)]);
    run.connection.disconnect();
    drop(run.server);
    assert_no_processes(&home.root, HANG_GUARD);
}

/// A cell that records its run in the working directory, prints computed shell output (not in its source), then
/// waits forever.
fn code_placeholder() -> String {
    "open('runs', 'a').write('x')\nout = await bash('echo partial-$((40 + 2))')\nimport time\ntime.sleep(3600)".into()
}

/// Kill point: during a cell. The cell is not run again: the reopened runtime settles it `interrupted`, carrying
/// the output it had durably published (its shell output), and the turn continues from that result.
#[test]
fn cli_durable_kill_during_a_cell_reports_it_interrupted_with_its_partial_output() {
    let provider = provider(vec![
        (tool_reply("toolu_cell", &code_placeholder()), Duration::ZERO),
        (text_reply("Saw the interruption."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    let runs = home.cwd().join("runs");
    let run = prompted(&home, &[], "start the cell");
    provider.wait_for(1, "the turn reached the provider");
    let file_of = || {
        std::fs::read_dir(home.sessions())
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|d| std::fs::read_dir(d.path()).into_iter().flatten().flatten())
            .map(|f| f.path())
            .find(|p| std::fs::read_to_string(p).is_ok_and(|t| t.lines().next().is_some_and(|l| l.contains(&run.id))))
    };
    // The cell's shell output is durable before the worker dies.
    wait_until(HANG_GUARD, "the cell's partial output is committed", || {
        file_of().is_some_and(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains("partial-42")))
    });
    let worker = worker_pid(&home, &run.id).expect("the worker runs");
    signal(worker, rustix::process::Signal::KILL);
    provider.wait_for(2, "the turn continued after the restart");
    wait_idle(&run.connection, &run.id);
    let requests = provider.requests.lock().unwrap().clone();
    let result = tool_result_text(&requests[1]);
    assert!(result.contains("partial-42"), "{result}");
    assert!(
        result.contains("[error] Tool rlm was interrupted and may have partially run"),
        "{result}"
    );
    assert_eq!(std::fs::read_to_string(&runs).unwrap(), "x", "the cell ran once");
    let file = session_file(&home, &run.id);
    let lines = transcript(&file);
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(lines[0], "user: start the cell");
    assert_eq!(lines[1], "assistant: ");
    assert!(lines[2].starts_with("toolResult: partial-42"), "{lines:?}");
    assert_eq!(lines[3], "assistant: Saw the interruption.");
    assert_eq!(results(&file), [("completed".to_string(), None)]);
    run.connection.disconnect();
    drop(run.server);
    assert_no_processes(&home.root, HANG_GUARD);
}

/// Kill point: during a cell that printed (Python `print`, no shell). runtime.py streams what a running cell prints
/// to the host, so the reopened runtime's `interrupted` result carries the printed line, not only shell output.
#[test]
fn cli_durable_kill_during_a_cell_keeps_its_printed_output() {
    let code = "open('runs', 'a').write('x')\nprint('printed-' + str(40 + 2))\nimport sys\nprint('err-' + str(6 * 7), file=sys.stderr)\nimport time\ntime.sleep(3600)";
    let provider = provider(vec![
        (tool_reply("toolu_print", code), Duration::ZERO),
        (text_reply("Saw the printed output."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    let run = prompted(&home, &[], "start the printing cell");
    provider.wait_for(1, "the turn reached the provider");
    wait_until(HANG_GUARD, "the cell's printed output is committed", || {
        let text = std::fs::read_dir(home.sessions())
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|d| std::fs::read_dir(d.path()).into_iter().flatten().flatten())
            .filter_map(|f| std::fs::read_to_string(f.path()).ok())
            .find(|t| t.lines().next().is_some_and(|l| l.contains(&run.id)))
            .unwrap_or_default();
        text.contains("printed-42") && text.contains("err-42")
    });
    let worker = worker_pid(&home, &run.id).expect("the worker runs");
    signal(worker, rustix::process::Signal::KILL);
    provider.wait_for(2, "the turn continued after the restart");
    wait_idle(&run.connection, &run.id);
    let requests = provider.requests.lock().unwrap().clone();
    let result = tool_result_text(&requests[1]);
    assert!(result.starts_with("printed-42\nerr-42"), "{result}");
    assert!(
        result.contains("[error] Tool rlm was interrupted and may have partially run"),
        "{result}"
    );
    assert_eq!(
        std::fs::read_to_string(home.cwd().join("runs")).unwrap(),
        "x",
        "the cell ran once"
    );
    let lines = transcript(&session_file(&home, &run.id));
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert!(lines[2].starts_with("toolResult: printed-42"), "{lines:?}");
    assert_eq!(lines[3], "assistant: Saw the printed output.");
    run.connection.disconnect();
    drop(run.server);
    assert_no_processes(&home.root, HANG_GUARD);
}

/// A signalled worker (SIGTERM) leaves its session to a successor: `DurableWork::hand_over` keeps its running shell
/// job running and journaled, so the restarted worker follows it to its real end (not `cancelled` by the old
/// worker's shutdown). Shell sandboxing is off: a sandboxed job dies with the thread that launched it.
#[test]
fn cli_durable_a_signalled_worker_hands_its_running_shell_job_to_its_successor() {
    let start = "job = await bash('while [ ! -e release ]; do sleep 0.05; done; echo job-done-$((40+2))', yield_after=0)\nprint('JOB ' + job.status)";
    let check = "j = (await rlm.jobs())[0]\nawait j.result()\nprint('STATUS ' + j.status + ' ' + j.text.strip())";
    let provider = provider(vec![
        (tool_reply("toolu_start", start), Duration::ZERO),
        (text_reply("Started."), Duration::ZERO),
        (tool_reply("toolu_check", check), Duration::ZERO),
        (text_reply("Checked."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    std::fs::write(
        home.agent().join("settings.json"),
        r#"{"sessionNaming":{"enabled":false},"sandbox":{"mode":"off"}}"#,
    )
    .unwrap();
    // Completion events would start a run of their own when the job ends; this test reads the job itself.
    let run = prompted(&home, &[("ULTRON_ASYNC_EVENTS", "off")], "start the job");
    provider.wait_for(2, "the job was started");
    wait_idle(&run.connection, &run.id);
    assert!(tool_result_text(&provider.requests.lock().unwrap()[1]).contains("JOB running"));
    let worker = worker_pid(&home, &run.id).expect("the worker runs");
    signal(worker, rustix::process::Signal::TERM);
    wait_until(HANG_GUARD, "a successor worker runs", || {
        worker_pid(&home, &run.id).is_some_and(|pid| pid != worker)
    });
    wait_idle(&run.connection, &run.id);
    // The job still runs: it ends only now, followed by the successor.
    std::fs::write(home.cwd().join("release"), "").unwrap();
    let session = run.connection.session_target().unwrap();
    let checking = run
        .connection
        .start(
            session,
            "pi.agent-controller",
            "prompt",
            json!([{"message": "check the job", "images": null}]),
        )
        .unwrap();
    let _ = run.connection.wait(&checking);
    provider.wait_for(4, "the job was checked");
    wait_idle(&run.connection, &run.id);
    let result = tool_result_text(&provider.requests.lock().unwrap()[3]);
    // (The restarted kernel's continuity note follows the cell's output.)
    assert!(result.starts_with("STATUS completed job-done-42\n"), "{result}");
    run.connection.disconnect();
    drop(run.server);
    assert_no_processes(&home.root, HANG_GUARD);
}

/// Kill point: after the answer is committed, before anything acknowledged it (the transcript, the client). The
/// reopened session projects the committed answer once and asks the model nothing more.
#[test]
fn cli_durable_kill_after_the_answer_commits_keeps_it_once_without_asking_again() {
    let marker = tempfile::tempdir().unwrap();
    let crash = format!("answer:{}", marker.path().join("crashed").display());
    let provider = provider(vec![(text_reply("Committed once."), Duration::ZERO)]);
    let home = Home::new(provider.port);
    let run = prompted(&home, &[("__ULTRON_DURABLE_CRASH", &crash)], "first");
    assert!(
        run.connection.wait(&run.prompting).is_err(),
        "the worker died before answering"
    );
    assert!(marker.path().join("crashed").exists());
    wait_idle(&run.connection, &run.id);
    let file = session_file(&home, &run.id);
    wait_until(HANG_GUARD, "the run settled", || !results(&file).is_empty());
    assert_eq!(provider.count(), 1, "the committed answer is never requested again");
    assert_eq!(transcript(&file), ["user: first", "assistant: Committed once."]);
    assert_eq!(results(&file), [("completed".to_string(), None)]);
    run.connection.disconnect();
    drop(run.server);
    assert_no_processes(&home.root, HANG_GUARD);
}

/// Kill point: after a cell's result commits, before the transcript has it. The cell is not run again, its result
/// is projected once, and the turn continues with one more request.
#[test]
fn cli_durable_kill_after_a_cell_result_commits_keeps_it_once() {
    let marker = tempfile::tempdir().unwrap();
    let crash = format!("result:{}", marker.path().join("crashed").display());
    let code = "open('runs', 'a').write('x')\n6 * 7";
    let provider = provider(vec![
        (tool_reply("toolu_once", code), Duration::ZERO),
        (text_reply("Forty-two."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    let runs = home.cwd().join("runs");
    let run = prompted(&home, &[("__ULTRON_DURABLE_CRASH", &crash)], "compute");
    assert!(
        run.connection.wait(&run.prompting).is_err(),
        "the worker died after the result"
    );
    provider.wait_for(2, "the turn continued after the restart");
    wait_idle(&run.connection, &run.id);
    assert_eq!(provider.count(), 2);
    assert_eq!(std::fs::read_to_string(&runs).unwrap(), "x", "the cell ran once");
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(tool_result_text(&requests[1]), "42");
    let file = session_file(&home, &run.id);
    assert_eq!(
        transcript(&file),
        [
            "user: compute",
            "assistant: ",
            "toolResult: 42",
            "assistant: Forty-two."
        ]
    );
    assert_eq!(results(&file), [("completed".to_string(), None)]);
    run.connection.disconnect();
    drop(run.server);
    assert_no_processes(&home.root, HANG_GUARD);
}

/// An Anthropic SSE answer that reports `input` prompt tokens (claude-sonnet-4-5's window is 1,000,000).
fn text_reply_using(text: &str, input: u64) -> String {
    common::sse(&[
        json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": input, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ])
}

/// Anthropic's stream error for a request over the window.
fn overflow_reply() -> String {
    common::sse(&[json!({"type": "error", "error": {"type": "invalid_request_error",
        "message": "prompt is too long: 1213462 tokens > 1000000 maximum"}})])
}

/// The summaries of the session's compaction entries, in commit order.
fn compactions(path: &Path) -> Vec<String> {
    writes(path)
        .iter()
        .filter(|w| w["kind"] == "entry" && w["type"] == "compaction")
        .map(|e| e["summary"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Compaction preparations (`pi.op.preparation`) written and not removed again.
fn open_preparations(path: &Path) -> usize {
    let ops: Vec<Value> = writes(path)
        .iter()
        .filter(|w| w["namespace"] == "pi.op.preparation")
        .map(|w| w["op"].clone())
        .collect();
    ops.iter().filter(|op| *op == "set").count() - ops.iter().filter(|op| *op == "delete").count()
}

/// Whether a request is a compaction's summary request.
fn summary_request(body: &Value) -> bool {
    body["system"].to_string().contains("context summarization assistant")
}

/// Kill point: during a threshold auto-compaction (its summary request in flight). The run's operation was inside
/// the compaction; the restarted worker reopens it at the checkpoint it started from and compacts again there: one
/// compaction entry (the second summary), the answer kept once and never asked for again, the run completed and no
/// preparation left behind.
#[test]
fn cli_durable_kill_during_auto_compaction_compacts_again_after_the_restart() {
    let provider = provider(vec![
        (text_reply_using("Over the threshold.", 990_000), Duration::ZERO),
        (text_reply("## Goal\nLost summary."), STALL),
        (text_reply("## Goal\nSummary after the restart."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    let run = prompted(&home, &[], "first");
    provider.wait_for(2, "the compaction's summary request");
    assert!(summary_request(&provider.requests.lock().unwrap()[1]));
    let file = session_file(&home, &run.id);
    assert!(compactions(&file).is_empty(), "killed before the compaction committed");
    let worker = worker_pid(&home, &run.id).expect("the worker runs");
    signal(worker, rustix::process::Signal::KILL);
    assert!(
        run.connection.wait(&run.prompting).is_err(),
        "the prompt in flight is lost"
    );
    provider.wait_for(3, "the restarted worker summarized again");
    wait_idle(&run.connection, &run.id);
    wait_until(HANG_GUARD, "the run settled", || !results(&file).is_empty());
    assert_eq!(provider.count(), 3, "the answer is never requested again");
    assert!(summary_request(&provider.requests.lock().unwrap()[2]));
    assert_eq!(transcript(&file), ["user: first", "assistant: Over the threshold."]);
    assert_eq!(compactions(&file), ["## Goal\nSummary after the restart."]);
    assert_eq!(results(&file), [("completed".to_string(), None)]);
    assert_eq!(open_preparations(&file), 0);
    run.connection.disconnect();
    drop(run.server);
    assert_no_processes(&home.root, HANG_GUARD);
}

/// Kill point: during an overflow compaction (the overflowed answer committed, its summary request in flight). The
/// restarted worker's generation resumes the compaction before asking again: one compaction after the error, then
/// the retried request (the summary and the prompt, not the error), answered once; the run completes.
#[test]
fn cli_durable_kill_during_overflow_compaction_compacts_and_retries_after_the_restart() {
    let provider = provider(vec![
        (overflow_reply(), Duration::ZERO),
        (text_reply("## Goal\nLost summary."), STALL),
        (text_reply("## Goal\nSummary after the restart."), Duration::ZERO),
        (text_reply("Retried."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    let run = prompted(&home, &[], "first");
    provider.wait_for(2, "the overflow compaction's summary request");
    assert!(summary_request(&provider.requests.lock().unwrap()[1]));
    let worker = worker_pid(&home, &run.id).expect("the worker runs");
    signal(worker, rustix::process::Signal::KILL);
    assert!(
        run.connection.wait(&run.prompting).is_err(),
        "the prompt in flight is lost"
    );
    provider.wait_for(4, "the restarted worker compacted and retried");
    wait_idle(&run.connection, &run.id);
    let file = session_file(&home, &run.id);
    wait_until(HANG_GUARD, "the run settled", || !results(&file).is_empty());
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    assert!(summary_request(&requests[2]));
    assert!(!summary_request(&requests[3]));
    let retried = requests[3]["messages"].to_string();
    assert!(retried.contains("Summary after the restart."), "{retried}");
    assert_eq!(user_texts(&requests[3]).last().map(String::as_str), Some("first"));
    assert!(!retried.contains("prompt is too long"), "{retried}");
    let lines = transcript(&file);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[0], "user: first");
    assert!(
        lines[1].starts_with("assistant:  [") && lines[1].contains("prompt is too long"),
        "{lines:?}"
    );
    assert_eq!(lines[2], "assistant: Retried.");
    assert_eq!(compactions(&file), ["## Goal\nSummary after the restart."]);
    // It keeps what the compaction the overflow started would have: the branch before the overflowed answer.
    let kept: Vec<Value> = writes(&file)
        .iter()
        .filter(|w| w["kind"] == "entry" && w["type"] == "compaction")
        .flat_map(|e| e["retainedTail"].as_array().cloned().unwrap_or_default())
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0]["role"], "user");
    assert_eq!(results(&file), [("completed".to_string(), None)]);
    assert_eq!(open_preparations(&file), 0);
    run.connection.disconnect();
    drop(run.server);
    assert_no_processes(&home.root, HANG_GUARD);
}

/// Kill point: during the model call that took a steering message. The message was placed once by the runtime's
/// inbox and committed with that generation; the resent (pinned) request carries it, and the transcript holds it
/// once.
#[test]
fn cli_durable_kill_after_a_steer_is_taken_delivers_it_exactly_once() {
    let code = "import os, time\nopen('started', 'w').write('1')\nwhile not os.path.exists('go'):\n    time.sleep(0.01)\n'went'";
    let provider = provider(vec![
        (tool_reply("toolu_wait", code), Duration::ZERO),
        (text_reply("Never seen."), STALL),
        (text_reply("Steered."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    let run = prompted(&home, &[], "first");
    wait_until(HANG_GUARD, "the cell runs", || home.cwd().join("started").exists());
    let session = run.connection.session_target().unwrap();
    op(
        &run.connection,
        &session,
        json!({"op": "enqueue", "kind": "steer", "message": "steer me"}),
    )
    .unwrap();
    std::fs::write(home.cwd().join("go"), "").unwrap();
    provider.wait_for(2, "the generation that takes the steer reached the provider");
    let worker = worker_pid(&home, &run.id).expect("the worker runs");
    signal(worker, rustix::process::Signal::KILL);
    provider.wait_for(3, "the restarted worker resent the request");
    wait_idle(&run.connection, &run.id);
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2], requests[1], "the resent request is the pinned one");
    assert_eq!(user_texts(&requests[2]), ["first", "steer me"]);
    let file = session_file(&home, &run.id);
    assert_eq!(
        transcript(&file),
        [
            "user: first",
            "assistant: ",
            "toolResult: 'went'",
            "user: steer me",
            "assistant: Steered."
        ]
    );
    assert_eq!(results(&file), [("completed".to_string(), None)]);
    run.connection.disconnect();
    drop(run.server);
    assert_no_processes(&home.root, HANG_GUARD);
}

/// Many conversations under one durable runtime: two root conversations of one session run at once (both model
/// requests are in flight together), the worker is killed, and the restarted worker resumes both: each resends its
/// own pinned request and gets one answer.
#[test]
fn cli_durable_two_conversations_progress_together_and_both_resume_after_a_kill() {
    let provider = provider(vec![
        (text_reply("Never seen."), STALL),
        (text_reply("Never seen."), STALL),
        (text_reply("First answer."), Duration::ZERO),
        (text_reply("Second answer."), Duration::ZERO),
    ]);
    let home = Home::new(provider.port);
    let server = Server::start(&home, common::SERVER_ID, &[]);
    let connection = server.connect();
    let id = create(&connection);
    let session = attach(&connection, &id).unwrap();
    let mut conversations = Vec::new();
    for message in ["alpha work", "beta work"] {
        let started = op(
            &connection,
            &session,
            json!({"op": "startConversation", "message": message}),
        )
        .unwrap()
        .unwrap();
        conversations.push(started["conversationId"].as_u64().unwrap());
    }
    assert_ne!(conversations[0], conversations[1]);
    // Both conversations are at their model call at the same time.
    provider.wait_for(2, "both conversations reached the provider");
    let worker = worker_pid(&home, &id).expect("the worker runs");
    signal(worker, rustix::process::Signal::KILL);
    provider.wait_for(4, "both conversations resent their requests");
    let views = || -> Option<Vec<Value>> {
        let session = connection.session_target().ok()?;
        let all = op(&connection, &session, json!({"op": "conversations"})).ok()??;
        Some(all["conversations"].as_array()?.clone())
    };
    wait_until(HANG_GUARD, "both conversations answered", || {
        views().is_some_and(|all| {
            all.len() == 2
                && all
                    .iter()
                    .all(|c| c["busy"] == false && c["messages"].as_array().is_some_and(|m| m.len() == 2))
        })
    });
    let all = views().unwrap();
    let mut answers: Vec<String> = all
        .iter()
        .map(|c| {
            let messages = c["messages"].as_array().unwrap();
            assert_eq!(messages[0]["role"], "user");
            assert_eq!(messages[1]["role"], "assistant");
            messages[1]["text"].as_str().unwrap().to_string()
        })
        .collect();
    answers.sort();
    assert_eq!(answers, ["First answer.", "Second answer."]);
    // Each conversation resent exactly the request it had pinned.
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    for text in ["alpha work", "beta work"] {
        let bodies: Vec<&Value> = requests.iter().filter(|r| user_texts(r) == [text]).collect();
        assert_eq!(bodies.len(), 2, "{text}");
        assert_eq!(bodies[0], bodies[1], "the resent request of {text} is the pinned one");
    }
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
}
