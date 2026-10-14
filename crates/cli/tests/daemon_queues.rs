//! Queue ordering at a final answer, against the TypeScript agent loop (crates/session-core/fixtures/queues.json,
//! recorded by `generate-queues.mjs` from packages/agent): steering and follow-up messages queued while the model
//! produces its final answer. TypeScript continues the run with the steers first and takes the follow-ups only when it
//! would stop, so a steer and a follow-up run as two turns. The real server and session worker run each scenario: the
//! first answer is held until the messages are queued through the session operations, and every request's user
//! texts and the transcript must match the recording.

#[path = "daemon_common/mod.rs"]
mod common;

use client::runtime::{CallError, Connection};
use common::{HANG_GUARD, Home, assert_no_processes, text_reply};
use protocol::RpcTarget;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A scripted provider whose first reply waits until `gate` exists; request bodies are recorded as they arrive.
struct Provider {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
}

fn provider(replies: Vec<String>, gate: PathBuf) -> Provider {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { continue };
            let reply = replies
                .get(i)
                .cloned()
                .unwrap_or_else(|| text_reply("(no more replies)"));
            let (seen, gate) = (seen.clone(), gate.clone());
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
                // The first answer is held until the test has queued its messages (a hang guard bounds it).
                let deadline = Instant::now() + HANG_GUARD;
                while i == 0 && !gate.exists() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
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

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The user texts of a provider request body, in order (without the host's `[clock]` lines).
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

struct Server {
    child: Child,
    id: String,
    socket: PathBuf,
}

impl Server {
    fn start(home: &Home) -> Server {
        let id = common::SERVER_ID;
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
        let child = command.spawn().unwrap();
        let socket = home.server_dir().join(format!("{id}.sock"));
        wait_until("server start", || Connection::connect(&socket, id).is_ok());
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

fn op(connection: &Connection, session: &RpcTarget, op: Value) -> Result<Option<Value>, CallError> {
    connection.call(session.clone(), "ultron.session-ops", "op", json!([op]))
}

/// The session's messages as `role: text` lines, in commit order.
fn transcript(sessions: &Path, id: &str) -> Vec<String> {
    let file = std::fs::read_dir(sessions)
        .unwrap()
        .flatten()
        .flat_map(|d| std::fs::read_dir(d.path()).unwrap().flatten())
        .map(|f| f.path())
        .find(|p| std::fs::read_to_string(p).is_ok_and(|t| t.lines().next().is_some_and(|l| l.contains(id))))
        .expect("the session file exists");
    let text = std::fs::read_to_string(file).unwrap();
    text.lines()
        .skip(1)
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .flat_map(|t| t.as_array().cloned().unwrap_or_else(|| vec![t]))
        .filter(|w| w["kind"] == "entry" && w["type"] == "message")
        .map(|e| {
            let message = &e["message"];
            let text: String = message["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["text"].as_str())
                .collect();
            format!("{}: {text}", message["role"].as_str().unwrap_or_default())
        })
        .collect()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

/// One recorded scenario through the real server: prompt, queue the messages while the first answer is held, then
/// let it finish.
fn run_scenario(scenario: &Value) {
    let name = scenario["name"].as_str().unwrap();
    let answers: Vec<String> = strings(&scenario["answers"]).iter().map(|a| text_reply(a)).collect();
    let gate = tempfile::tempdir().unwrap();
    let release = gate.path().join("release");
    let provider = provider(answers, release.clone());
    let home = Home::new(provider.port);
    let server = Server::start(&home);
    let connection = server.connect();
    let created = connection
        .call(
            connection.server_target(),
            "pi.session-management",
            "create",
            json!([{}]),
        )
        .unwrap()
        .unwrap();
    let id = created["sessionId"].as_str().unwrap().to_string();
    connection
        .call(
            connection.server_target(),
            "pi.session-management",
            "attach",
            json!([id]),
        )
        .unwrap();
    connection
        .wait_attachment(|a| a.is_some_and(|a| a.session_id == id))
        .unwrap();
    let session = connection.session_target().unwrap();
    let _prompting = connection
        .start(
            session.clone(),
            "pi.agent-controller",
            "prompt",
            json!([{"message": scenario["prompt"], "images": null}]),
        )
        .unwrap();
    wait_until("the first request reached the provider", || {
        !provider.requests.lock().unwrap().is_empty()
    });
    let enqueue = |kind: &str, texts: &Value| {
        for text in strings(texts) {
            op(
                &connection,
                &session,
                json!({"op": "enqueue", "kind": kind, "message": text}),
            )
            .unwrap();
        }
    };
    if scenario["followUpFirst"] == true {
        enqueue("followUp", &scenario["followUps"]);
        enqueue("steer", &scenario["steers"]);
    } else {
        enqueue("steer", &scenario["steers"]);
        enqueue("followUp", &scenario["followUps"]);
    }
    std::fs::write(&release, "").unwrap();
    let expected = scenario["requests"].as_array().unwrap().len();
    wait_until("the session is idle with nothing queued", || {
        provider.requests.lock().unwrap().len() >= expected
            && op(&connection, &session, json!({"op": "state"}))
                .ok()
                .flatten()
                .is_some_and(|s| s["isStreaming"] == false && s["pendingMessageCount"] == 0)
    });
    let requests: Vec<Vec<String>> = provider.requests.lock().unwrap().iter().map(user_texts).collect();
    let recorded: Vec<Vec<String>> = scenario["requests"].as_array().unwrap().iter().map(strings).collect();
    assert_eq!(requests, recorded, "{name}: the requests' user messages");
    assert_eq!(
        transcript(&home.sessions(), &id),
        strings(&scenario["transcript"]),
        "{name}: the transcript"
    );
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
}

fn scenarios() -> Vec<Value> {
    let fixture: Value = serde_json::from_str(include_str!("../../session-core/fixtures/queues.json")).unwrap();
    fixture["scenarios"].as_array().unwrap().clone()
}

fn scenario(name: &str) -> Value {
    scenarios()
        .into_iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("no scenario {name}"))
}

#[test]
fn cli_queues_a_steer_and_a_follow_up_at_a_final_answer_run_as_two_turns() {
    run_scenario(&scenario("steer_and_follow_up_at_a_final_answer"));
}

#[test]
fn cli_queues_a_follow_up_queued_before_a_steer_still_waits_for_it() {
    run_scenario(&scenario("follow_up_queued_before_steer"));
}

#[test]
fn cli_queues_all_steers_then_all_follow_ups() {
    run_scenario(&scenario("two_steers_two_follow_ups"));
}

#[test]
fn cli_queues_a_follow_up_alone_runs_after_the_final_answer() {
    run_scenario(&scenario("follow_up_only"));
}

#[test]
fn cli_queues_every_recorded_scenario_is_tested() {
    let names: Vec<String> = scenarios()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        [
            "steer_and_follow_up_at_a_final_answer",
            "follow_up_queued_before_steer",
            "two_steers_two_follow_ups",
            "follow_up_only"
        ]
    );
}
