//! Where `rlm.collect` waits (session_core::wake), through `ultron -p` over the real stack and Python kernel against a
//! scripted provider on 127.0.0.1: only the root lane of a session whose host wakes it (completion events on) leaves
//! the waiting to the host; a subagent's lane, and a root with `ULTRON_ASYNC_EVENTS=off`, wait in the cell. A print
//! run also stays until a typed task it started has ended and re-invoked it. Each guide says what its lane does.
//!
//! Every run uses a private HOME, agent and server dir under the system temp dir. Held replies wait on a gate file
//! the cell under test writes once its `rlm.collect` request is out, so a collect that did not wait has failed
//! before the reply it would wait for is sent.

mod common;

use common::{Sandbox, anthropic_text, anthropic_tool, assert_private_home, tool_result_text};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MODEL: &str = "anthropic/claude-sonnet-4-5";
/// Hang guard only: every wait ends on a condition.
const HANG_GUARD: Duration = Duration::from_secs(120);

/// One scripted reply.
#[derive(Clone)]
enum Reply {
    Now(String),
    /// Sent once `gate` exists.
    After(PathBuf, String),
    /// Sent, then `gate` is created.
    Then(PathBuf, String),
}

/// The requests of each conversation, told apart by a marker in their first user message.
struct Route {
    marker: Option<&'static str>,
    replies: Vec<Reply>,
}

/// A scripted Anthropic endpoint: each request goes to the first route whose marker its first user message contains
/// (a route without a marker takes the rest) and gets that route's next reply. Requests are recorded as
/// `{route, body}`.
fn scripted(routes: Vec<Route>) -> (u16, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let shared = seen.clone();
    let routes = Arc::new(routes);
    let next: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(vec![0; routes.len()]));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (seen, routes, next) = (shared.clone(), routes.clone(), next.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 {
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
                reader.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let first_user = body["messages"]
                    .as_array()
                    .and_then(|m| {
                        m.iter()
                            .find(|m| m["role"] == "user" && !m.to_string().contains("[clock]"))
                    })
                    .map(Value::to_string)
                    .unwrap_or_default();
                let route = routes
                    .iter()
                    .position(|r| r.marker.is_none_or(|m| first_user.contains(m)))
                    .unwrap();
                let reply = {
                    let mut next = next.lock().unwrap();
                    next[route] += 1;
                    routes[route].replies.get(next[route] - 1).cloned()
                }
                .unwrap_or_else(|| Reply::Now(anthropic_text("(no more replies)")));
                seen.lock().unwrap().push(json!({"route": route, "body": body}));
                let (text, then) = match reply {
                    Reply::Now(text) => (text, None),
                    Reply::After(gate, text) => {
                        wait_for(&gate);
                        (text, None)
                    }
                    Reply::Then(gate, text) => (text, Some(gate)),
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    text.len()
                );
                let mut stream = stream;
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(text.as_bytes());
                let _ = stream.flush();
                if let Some(gate) = then {
                    std::fs::write(gate, "").unwrap();
                }
            });
        }
    });
    (port, seen)
}

fn wait_for(gate: &Path) {
    let deadline = Instant::now() + HANG_GUARD;
    while !gate.exists() {
        assert!(Instant::now() < deadline, "timed out waiting for {}", gate.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn sandbox(port: u16) -> Sandbox {
    let sandbox = Sandbox::new(port);
    assert_private_home(&sandbox.root);
    std::fs::write(
        sandbox.agent_dir.join("settings.json"),
        r#"{"sessionNaming":{"enabled":false}}"#,
    )
    .unwrap();
    sandbox
}

/// `ultron -p` with the Rust runtime guide (not TypeScript's) and `env` set.
fn run(sandbox: &Sandbox, env: &[(&str, &str)], args: &[&str]) -> Output {
    let mut command = sandbox.ultron();
    command.env_remove("ULTRON_GUIDE");
    for (key, value) in env {
        command.env(key, value);
    }
    command.args(args).stdin(std::process::Stdio::null()).output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The requests of route `route`, in arrival order.
fn of(seen: &Arc<Mutex<Vec<Value>>>, route: usize) -> Vec<Value> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|r| r["route"] == route)
        .map(|r| r["body"].clone())
        .collect()
}

/// The cell that starts `rlm.collect(selector)`, then writes `gate` (the collect request is already out), then
/// prints what the collect returned (or the error it raised).
fn collect_then_gate(spawn: &str, gate: &Path) -> String {
    format!(
        "import asyncio\n{spawn}\nc = asyncio.ensure_future(rlm.collect([h]))\nawait asyncio.sleep(0)\nopen({gate:?}, 'w').close()\ntry:\n    r = await c\n    print('got', r[0]['result']['value'])\nexcept RuntimeError as e:\n    print('raised', e)",
        gate = gate.display().to_string()
    )
}

#[test]
fn cli_collect_waits_a_native_child_lane_waits_in_its_cell_for_a_grandchild() {
    let gates = tempfile::tempdir().unwrap();
    let (gate, turn_ended) = (gates.path().join("collecting"), gates.path().join("turn-ended"));
    let child_cell = collect_then_gate("h = await rlm.spawn('GRAND-BRIEF: answer', name='grand')", &gate);
    let (port, seen) = scripted(vec![
        Route {
            marker: Some("GRAND-BRIEF"),
            replies: vec![Reply::After(gate.clone(), anthropic_text("grand answer"))],
        },
        Route {
            marker: Some("CHILD-BRIEF"),
            // The child starts once the root has ended its turn: its end then wakes the idle root.
            replies: vec![
                Reply::After(turn_ended.clone(), anthropic_tool("toolu_k", &child_cell)),
                Reply::Now(anthropic_text("child done")),
            ],
        },
        Route {
            marker: None,
            replies: vec![
                Reply::Now(anthropic_tool(
                    "toolu_s",
                    "h = await rlm.spawn('CHILD-BRIEF: delegate', name='kid', depth=1)\nprint('spawned')",
                )),
                Reply::Then(turn_ended.clone(), anthropic_text("waiting for the child")),
                Reply::Now(anthropic_tool(
                    "toolu_c",
                    "r = await rlm.collect([h])\nprint(r[0]['result']['value'])",
                )),
                Reply::Now(anthropic_text("parent done")),
            ],
        },
    ]);
    let sandbox = sandbox(port);
    let output = run(&sandbox, &[], &["-p", "--model", MODEL, "delegate"]);
    assert_eq!(text(&output.stdout), "parent done\n", "{}", text(&output.stderr));
    // The child's collect waited in its cell for the grandchild (its lane is never woken).
    let child = of(&seen, 1);
    assert_eq!(child.len(), 2);
    assert_eq!(tool_result_text(&child[1], "toolu_k").unwrap(), "got grand answer");
    // The root collected the child after its end woke it.
    let parent = of(&seen, 2);
    assert_eq!(tool_result_text(&parent[3], "toolu_c").unwrap(), "child done");
    // Each guide says what its lane does: the woken root ends its turn (its collect never waits), the subagent awaits
    // before replying and has no schedule.
    let root_system = parent[0]["system"].to_string();
    assert!(
        root_system.contains("end your turn: each end arrives as a `child_done` event, and `await rlm.collect(hs)` then returns the results (it does not wait: while a selected child runs it raises)"),
        "{root_system}"
    );
    assert!(root_system.contains("schedule.every"), "{root_system}");
    let child_system = child[0]["system"].to_string();
    assert!(
        child_system.contains("Nothing needs polling, and nothing wakes you: your reply ends your run. Before replying, await what your answer still needs"),
        "{child_system}"
    );
    assert!(
        child_system.contains("`await out.job.result()` waits for it"),
        "{child_system}"
    );
    assert!(
        !child_system.contains("starts a new turn if yours ended"),
        "{child_system}"
    );
    assert!(
        !child_system.contains("or end your turn: each end arrives"),
        "{child_system}"
    );
    assert!(!child_system.contains("schedule.every"), "{child_system}");
    // A durable child still has the swarm APIs.
    assert!(child_system.contains("swarm.route"), "{child_system}");
}

#[test]
fn cli_collect_waits_a_root_with_events_off_waits_in_its_cell() {
    let gates = tempfile::tempdir().unwrap();
    let gate = gates.path().join("collecting");
    let cell = collect_then_gate("h = await rlm.spawn('CHILD-BRIEF: answer', name='kid')", &gate);
    let (port, seen) = scripted(vec![
        Route {
            marker: Some("CHILD-BRIEF"),
            replies: vec![Reply::After(gate.clone(), anthropic_text("child answer"))],
        },
        Route {
            marker: None,
            replies: vec![
                Reply::Now(anthropic_tool("toolu_s", &cell)),
                Reply::Now(anthropic_text("parent done")),
            ],
        },
    ]);
    let sandbox = sandbox(port);
    let output = run(
        &sandbox,
        &[("ULTRON_ASYNC_EVENTS", "off"), ("ULTRON_HINTS", "on")],
        &["-p", "--model", MODEL, "delegate"],
    );
    assert_eq!(text(&output.stdout), "parent done\n", "{}", text(&output.stderr));
    let parent = of(&seen, 1);
    assert_eq!(tool_result_text(&parent[1], "toolu_s").unwrap(), "got child answer");
    // Its guide tells it to wait (nothing announces the end), and names the hint lines its results may end with.
    let system = parent[0]["system"].to_string();
    assert!(
        system.contains("Completions are not announced in this session (ULTRON_ASYNC_EVENTS=off)"),
        "{system}"
    );
    assert!(system.contains("Act on a trailing `[hint:<tag>]` line"), "{system}");
    assert!(!system.contains("starts a new turn if yours ended"), "{system}");
}

#[test]
fn cli_collect_waits_print_mode_waits_for_a_typed_task_it_started() {
    let gates = tempfile::tempdir().unwrap();
    let gate = gates.path().join("turn-ended");
    let (port, seen) = scripted(vec![
        Route {
            marker: Some("TASK-BRIEF"),
            replies: vec![Reply::After(gate.clone(), anthropic_text("task answer"))],
        },
        Route {
            marker: None,
            replies: vec![
                Reply::Now(anthropic_tool(
                    "toolu_t",
                    "j = await background.start('TASK-BRIEF: work')\nprint('started')",
                )),
                // The root ends its turn with the task running; only then may the task answer.
                Reply::Then(gate.clone(), anthropic_text("started it")),
                Reply::Now(anthropic_text("saw the task")),
            ],
        },
    ]);
    let sandbox = sandbox(port);
    let output = run(&sandbox, &[], &["-p", "--model", MODEL, "start a task"]);
    // The run stayed until the task's end re-invoked the root.
    assert_eq!(text(&output.stdout), "saw the task\n", "{}", text(&output.stderr));
    let parent = of(&seen, 1);
    assert_eq!(parent.len(), 3);
    let event = parent[2]["messages"].as_array().unwrap().last().unwrap().to_string();
    assert!(event.contains("<runtime_event kind=\\\"task_done\\\""), "{event}");
}
