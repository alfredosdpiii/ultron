//! Ultron Durable subagents at process level: the real server and session worker, killed with SIGKILL while a
//! child conversation's model request is in flight, against a scripted Anthropic provider that answers by content.
//!
//! - a worktree child (`rlm.spawn(..., worktree=True, thinking="high")`) is reattached after the kill: its pinned
//!   request (with its thinking) is resent, its next cell runs in its worktree with the worktree's environment and
//!   system prompt, and deleting it then reads `cancelled`;
//! - a typed agent's run (`agents.invoke`) is not reattached: as in TypeScript its task reads `interrupted`, and its
//!   child conversation is aborted, never resent;
//! - a subagent's model turns are charged to the usage root of the turn that started it (the ledger counts them).

#[path = "daemon_common/mod.rs"]
mod common;

use client::runtime::{CallError, Connection};
use common::{HANG_GUARD, Home, assert_no_processes, processes_mentioning, text_reply};
use protocol::RpcTarget;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A reply and the file it waits for (`None`: at once).
type Answer = (String, Option<PathBuf>);
type Script = Arc<dyn Fn(&Value, &[Value]) -> Answer + Send + Sync>;

/// A scripted provider that answers each request by its content (and the requests before it); bodies are recorded.
struct Provider {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
}

fn provider(script: Script) -> Provider {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (seen, script) = (seen.clone(), script.clone());
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
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let before = {
                    let mut seen = seen.lock().unwrap();
                    let before = seen.clone();
                    seen.push(body.clone());
                    before
                };
                let (reply, gate) = script(&body, &before);
                if let Some(gate) = gate {
                    let deadline = Instant::now() + HANG_GUARD;
                    while !gate.exists() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
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

impl Provider {
    fn bodies(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !done() {
        assert!(Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// An Anthropic SSE reply with one `rlm` call.
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

/// The user texts of a request, in order (without the host's `[clock]` lines).
fn user_texts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .flat_map(|m| match &m["content"] {
            Value::String(text) => vec![text.clone()],
            blocks => blocks
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| c["text"].as_str().map(str::to_string))
                .collect(),
        })
        .filter(|t| !t.starts_with("[clock]"))
        .collect()
}

/// The texts of the request's tool results, in order.
fn tool_results(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|c| c["type"] == "tool_result")
        .map(|c| match &c["content"] {
            Value::String(text) => text.clone(),
            blocks => blocks
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|b| b["text"].as_str())
                .collect(),
        })
        .collect()
}

fn system_text(body: &Value) -> String {
    match &body["system"] {
        Value::String(text) => text.clone(),
        blocks => blocks
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn first_user(body: &Value) -> String {
    user_texts(body).into_iter().next().unwrap_or_default()
}

/// A line of the request's tool results that starts with `prefix`.
fn result_line(body: &Value, prefix: &str) -> Option<String> {
    tool_results(body)
        .iter()
        .flat_map(|t| t.lines().map(str::to_string).collect::<Vec<_>>())
        .find(|l| l.starts_with(prefix))
}

struct Server {
    child: Child,
    id: String,
    socket: PathBuf,
}

impl Server {
    fn start(home: &Home, env: &[(&str, &str)]) -> Server {
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
        for (k, v) in env {
            command.env(k, v);
        }
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

/// A new session, attached.
fn open(connection: &Connection) -> String {
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
    id
}

fn prompt(connection: &Connection, message: &str) {
    let session = connection.session_target().unwrap();
    connection
        .start(
            session,
            "pi.agent-controller",
            "prompt",
            json!([{"message": message, "images": null}]),
        )
        .unwrap();
}

/// The (restarted) worker's session is idle with nothing queued.
fn wait_idle(connection: &Connection, id: &str) {
    wait_until("the session is idle", || {
        connection.attachment().is_some_and(|a| a.session_id == id)
            && connection.session_target().ok().is_some_and(|session| {
                op(connection, &session, json!({"op": "state"}))
                    .ok()
                    .flatten()
                    .is_some_and(|s| s["isStreaming"] == false && s["pendingMessageCount"] == 0)
            })
    });
}

fn worker_pid(home: &Home, session_id: &str) -> Option<u32> {
    processes_mentioning(&home.root).into_iter().find(|pid| {
        std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| {
            let text = String::from_utf8_lossy(&c).into_owned();
            text.contains("session-worker") && text.contains(session_id)
        })
    })
}

/// Whether process `pid` runs (exists and is not a zombie).
fn running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, rest)| !rest.trim_start().starts_with('Z'))
    })
}

fn kill(pid: u32) {
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid as i32).unwrap(),
        rustix::process::Signal::KILL,
    )
    .unwrap();
}

/// The committed writes of the session's format-4 file.
fn writes(home: &Home, id: &str) -> Vec<Value> {
    let file = std::fs::read_dir(home.sessions())
        .unwrap()
        .flatten()
        .flat_map(|d| std::fs::read_dir(d.path()).unwrap().flatten())
        .map(|f| f.path())
        .find(|p| std::fs::read_to_string(p).is_ok_and(|t| t.lines().next().is_some_and(|l| l.contains(id))))
        .expect("the session file exists");
    let text = std::fs::read_to_string(file).unwrap();
    let complete = &text[..text.rfind('\n').map_or(0, |i| i + 1)];
    complete
        .lines()
        .skip(1)
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .flat_map(|t| t.as_array().cloned().unwrap_or_else(|| vec![t]))
        .collect()
}

fn git(cwd: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(args)
        .current_dir(cwd)
        .env("HOME", cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn cli_durable_a_reattached_worktree_child_keeps_its_worktree_and_thinking_and_its_cancel_reads_cancelled() {
    let gates = tempfile::tempdir().unwrap();
    let (killed, never) = (gates.path().join("killed"), gates.path().join("never"));
    let (k, n) = (killed.clone(), never.clone());
    let spawn = "import json\nh = await rlm.spawn('CHILD: report where you work', name='wt', worktree=True, thinking='high')\nprint('JOB ' + json.dumps({'id': h.rlm_child_id, 'cwd': h.worktree['cwd']}))";
    let script: Script = Arc::new(move |body, before| {
        let users = user_texts(body);
        if first_user(body).starts_with("CHILD:") {
            if !tool_results(body).is_empty() {
                // The child's cell ran: this request is held until the test is done with it.
                return (text_reply("child done"), Some(n.clone()));
            }
            let earlier = before.iter().filter(|b| first_user(b).starts_with("CHILD:")).count();
            if earlier == 0 {
                // In flight when the worker is killed.
                return (text_reply("never seen"), Some(k.clone()));
            }
            return (
                tool_reply(
                    "toolu_cwd",
                    "import os\nprint('CWD ' + os.getcwd())\nprint('WT ' + os.environ.get('ULTRON_WORKTREE', ''))",
                ),
                None,
            );
        }
        let last = users.last().cloned().unwrap_or_default();
        if last.contains("<runtime_event") {
            return (text_reply("noted"), None);
        }
        if let Some(id) = last.strip_prefix("cancel ") {
            if result_line(body, "DELETED").is_some() {
                return (text_reply("cancelled it"), None);
            }
            let code = format!(
                "import json\nr = await rlm.delete_subagent('{id}')\nprint('DELETED ' + json.dumps(r))\nprint('LIST ' + json.dumps(await rlm.list_subagents(), default=str))"
            );
            return (tool_reply("toolu_delete", &code), None);
        }
        if result_line(body, "JOB ").is_some() {
            return (text_reply("spawned"), None);
        }
        (tool_reply("toolu_spawn", spawn), None)
    });
    let provider = provider(script);
    let home = Home::new(provider.port);
    git(&home.cwd(), &["init", "-q"]);
    std::fs::write(home.cwd().join("README"), "repo\n").unwrap();
    git(&home.cwd(), &["add", "README"]);
    git(&home.cwd(), &["commit", "-q", "-m", "init"]);
    let server = Server::start(&home, &[]);
    let connection = server.connect();
    let id = open(&connection);
    prompt(&connection, "start the worktree child");
    let child_requests = |provider: &Provider| -> Vec<Value> {
        provider
            .bodies()
            .into_iter()
            .filter(|b| first_user(b).starts_with("CHILD:"))
            .collect()
    };
    wait_until("the child's first request", || child_requests(&provider).len() == 1);
    wait_until("the root saw the spawn", || {
        provider.bodies().iter().any(|b| result_line(b, "JOB ").is_some())
    });
    let job: Value = provider
        .bodies()
        .iter()
        .find_map(|b| result_line(b, "JOB "))
        .map(|l| serde_json::from_str(&l[4..]).unwrap())
        .unwrap();
    let (child, worktree) = (
        job["id"].as_str().unwrap().to_string(),
        job["cwd"].as_str().unwrap().to_string(),
    );
    assert!(!worktree.is_empty() && worktree != home.cwd().display().to_string());
    // `thinking="high"` reached the child's conversation: its request asks for extended thinking.
    let first = child_requests(&provider)[0].clone();
    assert!(
        first["thinking"].is_object() && first["thinking"]["type"] != "disabled",
        "{}",
        first["thinking"]
    );
    wait_idle(&connection, &id);
    let worker = worker_pid(&home, &id).expect("the worker runs");
    kill(worker);
    std::fs::write(&killed, "").unwrap();
    // The reattached child resends the request it had pinned, thinking included.
    wait_until("the child's resent request", || child_requests(&provider).len() >= 2);
    assert_eq!(
        child_requests(&provider)[1],
        first,
        "the resent request is the pinned one"
    );
    // Its next cell runs in its worktree, with the worktree's environment, and its prompt names the worktree.
    wait_until("the child's cell result", || child_requests(&provider).len() >= 3);
    let third = child_requests(&provider)[2].clone();
    let canonical = |p: &str| std::fs::canonicalize(p).unwrap();
    let cwd_line = result_line(&third, "CWD ").unwrap();
    assert_eq!(canonical(&cwd_line[4..]), canonical(&worktree), "{cwd_line}");
    let wt_line = result_line(&third, "WT ").unwrap();
    assert!(!wt_line[3..].is_empty(), "{wt_line}");
    assert!(
        system_text(&third).contains(&worktree),
        "the child's system prompt names its worktree"
    );
    // Deleting the reattached child cancels it: it reads cancelled, not interrupted.
    wait_idle(&connection, &id);
    prompt(&connection, &format!("cancel {child}"));
    wait_until("the delete ran", || {
        provider.bodies().iter().any(|b| result_line(b, "DELETED ").is_some())
    });
    let body = provider
        .bodies()
        .into_iter()
        .find(|b| result_line(b, "DELETED ").is_some())
        .unwrap();
    assert_eq!(
        result_line(&body, "DELETED ").unwrap(),
        "DELETED {\"deleted\": true}",
        "{:?}",
        result_line(&body, "LIST ")
    );
    let listed: Value = serde_json::from_str(&result_line(&body, "LIST ").unwrap()[5..]).unwrap();
    let entry = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == child.as_str())
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(entry["result"]["status"], "cancelled", "{entry}");
    wait_idle(&connection, &id);
    std::fs::write(&never, "").unwrap();
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
}

#[test]
fn cli_durable_a_typed_agent_run_is_interrupted_by_a_restart_not_resent() {
    let gates = tempfile::tempdir().unwrap();
    let killed = gates.path().join("killed");
    let k = killed.clone();
    let script: Script = Arc::new(move |body, _| {
        if first_user(body).starts_with("AGENT:") {
            return (text_reply("agent answer"), Some(k.clone()));
        }
        let last = user_texts(body).last().cloned().unwrap_or_default();
        if last.contains("<runtime_event") {
            return (text_reply("noted"), None);
        }
        if last == "list the tasks" {
            if result_line(body, "TASKS ").is_some() {
                return (text_reply("listed"), None);
            }
            return (
                tool_reply(
                    "toolu_tasks",
                    "import json\nprint('TASKS ' + json.dumps([[t['definition'], t['state']] for t in (await agents.tasks())['tasks']]))",
                ),
                None,
            );
        }
        if !tool_results(body).is_empty() {
            return (text_reply("saw the interruption"), None);
        }
        (
            tool_reply(
                "toolu_agent",
                "r = await agents.invoke('rlm-child@1', {'prompt': 'AGENT: count the files'})\nprint('R', r)",
            ),
            None,
        )
    });
    let provider = provider(script);
    let home = Home::new(provider.port);
    let server = Server::start(&home, &[]);
    let connection = server.connect();
    let id = open(&connection);
    prompt(&connection, "start the agent");
    let agent_requests = |provider: &Provider| {
        provider
            .bodies()
            .iter()
            .filter(|b| first_user(b).starts_with("AGENT:"))
            .count()
    };
    wait_until("the agent's request", || agent_requests(&provider) == 1);
    let worker = worker_pid(&home, &id).expect("the worker runs");
    kill(worker);
    std::fs::write(&killed, "").unwrap();
    // The root's cell is interrupted and its turn continues; the agent's run is not resumed.
    wait_until("the root continued", || {
        provider
            .bodies()
            .iter()
            .any(|b| tool_results(b).iter().any(|t| t.contains("interrupted")))
    });
    wait_idle(&connection, &id);
    prompt(&connection, "list the tasks");
    wait_until("the tasks were listed", || {
        provider.bodies().iter().any(|b| result_line(b, "TASKS ").is_some())
    });
    wait_idle(&connection, &id);
    let line = provider.bodies().iter().find_map(|b| result_line(b, "TASKS ")).unwrap();
    let tasks: Value = serde_json::from_str(&line[6..]).unwrap();
    assert_eq!(tasks, json!([["rlm-child@1", "interrupted"]]), "{line}");
    assert_eq!(agent_requests(&provider), 1, "the agent's request was never resent");
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
}

#[test]
fn cli_durable_a_subagents_turns_are_charged_to_the_root_that_started_it() {
    let script: Script = Arc::new(|body, _| {
        if first_user(body).starts_with("CHILD:") {
            return (text_reply("hi"), None);
        }
        let last = user_texts(body).last().cloned().unwrap_or_default();
        if last.contains("<runtime_event") {
            return (text_reply("noted"), None);
        }
        if !tool_results(body).is_empty() {
            return (text_reply("spawned"), None);
        }
        (
            tool_reply("toolu_spawn", "h = await rlm.spawn('CHILD: say hi', name='greeter')"),
            None,
        )
    });
    let provider = provider(script);
    let home = Home::new(provider.port);
    // A turn limit makes the ledger keep its counts in the session (`ultron.usage`).
    let server = Server::start(&home, &[("ULTRON_MAX_TOTAL_TURNS", "100")]);
    let connection = server.connect();
    let id = open(&connection);
    prompt(&connection, "spawn a greeter");
    // The root's two responses, the child's one, and the run its completion event started.
    wait_until("four responses", || provider.bodies().len() == 4);
    wait_idle(&connection, &id);
    let ledger = writes(&home, &id)
        .into_iter()
        .rfind(|w| w["namespace"] == "ultron.usage" && w["key"] == "root" && w["op"] == "set")
        .expect("the ledger was written")["value"]
        .clone();
    let counts: Vec<(String, u64)> = ledger["roots"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(root, value)| (root.clone(), value["turns"]["count"].as_u64().unwrap_or(0)))
        .filter(|(_, count)| *count > 0)
        .collect();
    assert_eq!(counts.len(), 1, "{ledger}");
    assert!(counts[0].0.starts_with("turn:"), "{counts:?}");
    assert_eq!(
        counts[0].1, 4,
        "every response of the tree is charged to its root: {ledger}"
    );
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
}

/// A worker stopped by a signal (SIGTERM: a successor takes over) does not cancel its running subagent: the child's
/// task stays unfinished, the restarted worker reattaches it (its pinned request resent), and its result arrives as
/// a `child_done` event and stays listed as succeeded.
#[test]
fn cli_durable_a_signalled_worker_leaves_its_running_child_to_its_successor() {
    let gates = tempfile::tempdir().unwrap();
    let signalled = gates.path().join("signalled");
    let s = signalled.clone();
    let script: Script = Arc::new(move |body, before| {
        if first_user(body).starts_with("CHILD:") {
            let earlier = before.iter().filter(|b| first_user(b).starts_with("CHILD:")).count();
            return (text_reply("child result"), (earlier == 0).then(|| s.clone()));
        }
        let last = user_texts(body).last().cloned().unwrap_or_default();
        if last.contains("<runtime_event") {
            return (text_reply("noted"), None);
        }
        if last == "list them" {
            if result_line(body, "LIST ").is_some() {
                return (text_reply("listed"), None);
            }
            return (
                tool_reply(
                    "toolu_list",
                    "import json\nprint('LIST ' + json.dumps(await rlm.list_subagents(), default=str))",
                ),
                None,
            );
        }
        if !tool_results(body).is_empty() {
            return (text_reply("spawned"), None);
        }
        (
            tool_reply("toolu_spawn", "h = await rlm.spawn('CHILD: work', name='c')"),
            None,
        )
    });
    let provider = provider(script);
    let home = Home::new(provider.port);
    let server = Server::start(&home, &[]);
    let connection = server.connect();
    let id = open(&connection);
    prompt(&connection, "spawn a child");
    let children = |provider: &Provider| {
        provider
            .bodies()
            .iter()
            .filter(|b| first_user(b).starts_with("CHILD:"))
            .count()
    };
    wait_until("the child's request", || children(&provider) == 1);
    wait_idle(&connection, &id);
    let worker = worker_pid(&home, &id).expect("the worker runs");
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(worker as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    // The child's answer is released only once the signalled worker is gone: until it exits it may still commit an
    // answer that arrives (a worker finishing work during its hand-over is correct), which would leave the
    // successor nothing to resend.
    wait_until("the signalled worker exited", || !running(worker));
    std::fs::write(&signalled, "").unwrap();
    // The successor resends the child's pinned request, and the child's end re-invokes the root.
    wait_until("the child's resent request", || children(&provider) == 2);
    wait_until("the child_done event was answered", || {
        provider
            .bodies()
            .iter()
            .any(|b| user_texts(b).last().is_some_and(|u| u.contains("child_done")))
    });
    wait_idle(&connection, &id);
    prompt(&connection, "list them");
    wait_until("the list", || {
        provider.bodies().iter().any(|b| result_line(b, "LIST ").is_some())
    });
    wait_idle(&connection, &id);
    let line = provider.bodies().iter().find_map(|b| result_line(b, "LIST ")).unwrap();
    let listed: Value = serde_json::from_str(&line[5..]).unwrap();
    assert_eq!(listed[0]["result"]["status"], "succeeded", "{line}");
    assert_eq!(listed[0]["result"]["value"], "child result", "{line}");
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
}
