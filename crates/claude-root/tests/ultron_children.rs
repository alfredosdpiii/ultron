//! `ultron mcp --children ultron`: the Claude Code root's subagents are Ultron agent loops in the server (no
//! durable runtime), and get what a child lane of Ultron's own session gets (crate session_core::memory_children):
//!
//! - agent-hub steering: `agent_message.send(..., receiver_role='child', steer=True)` from the root's kernel
//!   reaches the running child, which sees the family envelope before it ends (the answer after the steer is its
//!   result), as packages/coding-agent/test/ultron-a42-intervention-subtree-stop.test.ts expects of a native child;
//! - the context-pressure reminder: a child's cell result ends with the `[context: ...]` line once its loop's
//!   context reaches the trigger.
//!
//! Through the server's product entry (`mcp_server::start` and the MCP loop over pipes), a private temp root, and an
//! OpenAI-compatible fake provider on 127.0.0.1 scripted by what each request says.

use claude_root::mcp_protocol::serve;
use claude_root::mcp_server::{Running, start};
use serde_json::{Value, json};
use session_core::RunEnv;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Hang guard only: every wait below ends on a condition.
const HANG_GUARD: Duration = Duration::from_secs(120);

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn repo() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

struct Root {
    dir: tempfile::TempDir,
}

impl Root {
    fn new() -> Root {
        let dir = tempfile::Builder::new().prefix("ucc-").tempdir_in("/tmp").unwrap();
        for sub in ["h", "a", "s", "r", "w", "tmp"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        Root { dir }
    }

    fn path(&self, sub: &str) -> PathBuf {
        self.dir.path().join(sub)
    }

    fn env(&self, extra: &[(&str, &str)]) -> RunEnv {
        let p = |s: &str| self.path(s).display().to_string();
        let mut vars: Vec<(String, String)> = vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), p("h")),
            ("ULTRON_CODING_AGENT_DIR".into(), p("a")),
            ("ULTRON_SERVER_DIR".into(), p("s")),
            ("XDG_RUNTIME_DIR".into(), p("r")),
            ("TMPDIR".into(), p("tmp")),
            ("ULTRON_SELF_COMMAND".into(), r#"["/opt/ultron/bin/ultron"]"#.into()),
            ("ULTRON_LOKI".into(), "off".into()),
            ("ULTRON_PYTHON".into(), python()),
            ("PI_OFFLINE".into(), "1".into()),
        ];
        for (key, value) in extra {
            vars.retain(|(k, _)| k != key);
            vars.push((key.to_string(), value.to_string()));
        }
        RunEnv {
            cwd: self.path("w"),
            home: self.path("h"),
            var: Arc::new(move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())),
            docs: repo().join("packages/coding-agent/docs"),
        }
    }
}

struct Server {
    running: Arc<Running>,
    stdin: Option<std::io::PipeWriter>,
    stdout: BufReader<std::io::PipeReader>,
    serving: Option<std::thread::JoinHandle<()>>,
    next: u64,
}

impl Server {
    fn start(root: &Root, port: u16, extra: &[(&str, &str)]) -> Server {
        Server::start_with(root, port, extra, &[])
    }

    /// [`Server::start`] with more server arguments (`--wake`).
    fn start_with(root: &Root, port: u16, extra: &[(&str, &str)], args: &[&str]) -> Server {
        std::fs::write(
            root.path("a/models.json"),
            // The child model's provider needs its own key: the root model's key is that provider's only.
            json!({"providers": {"groq": {"baseUrl": format!("http://127.0.0.1:{port}/v1"), "apiKey": "fake-groq"}}})
                .to_string(),
        )
        .unwrap();
        let argv: Vec<String> = [
            "--children",
            "ultron",
            "--ultron-child-model",
            "groq/llama-3.3-70b-versatile",
        ]
        .iter()
        .chain(args)
        .map(|a| a.to_string())
        .collect();
        let (running, handlers) = start(&argv, &claude_root::Ctx { env: root.env(extra) }).unwrap();
        let (input, stdin) = std::io::pipe().unwrap();
        let (stdout, output) = std::io::pipe().unwrap();
        let serving = std::thread::spawn(move || {
            serve(handlers, &mut BufReader::new(input), Box::new(output));
        });
        let mut server = Server {
            running,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            serving: Some(serving),
            next: 0,
        };
        server.call("initialize", json!({"protocolVersion": "2025-06-18"}));
        server
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        loop {
            let mut text = String::new();
            assert!(self.stdout.read_line(&mut text).unwrap() > 0, "the server closed");
            let message: Value = serde_json::from_str(&text).unwrap();
            if message["id"] == id {
                return message;
            }
        }
    }

    fn cell(&mut self, code: &str) -> String {
        let reply = self.call("tools/call", json!({"name": "rlm", "arguments": {"code": code}}));
        reply["result"]["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn close(mut self) {
        drop(self.stdin.take());
        self.serving.take().unwrap().join().unwrap();
        self.running.shutdown();
    }
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_GUARD;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What the fake provider answers: text, or one `rlm` call with this code.
enum Reply {
    Text(String),
    Cell(String),
}

type Requests = Arc<Mutex<Vec<Value>>>;
type Script = Arc<dyn Fn(&Value) -> Reply + Send + Sync>;

/// An OpenAI-compatible chat-completions endpoint on 127.0.0.1 answering by `script`; records the request bodies.
fn fake_provider(script: Script) -> (u16, Requests) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests: Requests = Arc::default();
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let (seen, script) = (seen.clone(), script.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut length = 0;
                let mut first = true;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 {
                        return;
                    }
                    if header.trim().is_empty() && !first {
                        break;
                    }
                    first = false;
                    if let Some((k, v)) = header.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        length = v.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                seen.lock().unwrap().push(body.clone());
                let chunk = |delta: Value, finish: Value| {
                    format!(
                        "data: {}\n\n",
                        json!({"id": "c", "object": "chat.completion.chunk", "created": 1, "model": "m",
                            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
                            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}})
                    )
                };
                let sse = match script(&body) {
                    Reply::Text(text) => {
                        chunk(json!({"role": "assistant", "content": text}), Value::Null)
                            + &chunk(json!({}), json!("stop"))
                    }
                    Reply::Cell(code) => {
                        let call = json!({"index": 0, "id": "call_1", "type": "function",
                            "function": {"name": "rlm", "arguments": json!({"code": code}).to_string()}});
                        chunk(json!({"role": "assistant", "tool_calls": [call]}), Value::Null)
                            + &chunk(json!({}), json!("tool_calls"))
                    }
                } + "data: [DONE]\n\n";
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    sse.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(sse.as_bytes());
            });
        }
    });
    (port, requests)
}

fn has_tool_result(body: &Value) -> bool {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|m| m["role"] == "tool")
}

#[test]
fn claude_root_ultron_children_are_steered_by_the_agent_hub() {
    let root = Root::new();
    let release = root.path("w/release");
    let gate = release.clone();
    let (port, requests) = fake_provider(Arc::new(move |body: &Value| {
        if body.to_string().contains("Untrusted family message") {
            return Reply::Text("child steered".into());
        }
        // The first answer waits until the test has steered.
        wait_until("the test releases the child", || gate.exists());
        Reply::Text("child first".into())
    }));
    let mut server = Server::start(&root, port, &[]);
    let id = server.cell("h = await rlm.spawn('CHILD-BRIEF: audit the tests', name='auditor')\nprint(h.rlm_child_id)");
    wait_until("the child asks its model", || !requests.lock().unwrap().is_empty());
    let sent = server.cell(&format!(
        "r = await agent_message.send('Stop auditing tests; focus on src/', receiver_role='child', receiver_id='{id}', steer=True)\nprint(r['state'], r['steered'])"
    ));
    assert_eq!(sent, "delivered True");
    std::fs::write(&release, "").unwrap();
    // The child takes the steer before it ends: its next request carries the envelope from the root.
    wait_until("the steered child asks again", || requests.lock().unwrap().len() >= 2);
    let steered = requests.lock().unwrap()[1]["messages"].to_string();
    assert!(
        steered.contains("Verified sender: the root agent (your parent)"),
        "{steered}"
    );
    assert!(
        steered.contains(r#"Content (JSON string): \"Stop auditing tests; focus on src/\""#),
        "{steered}"
    );
    // Its answer after the steer is its result.
    let collected = server.cell(&format!("print(await rlm.collect(['{id}'], timeout_ms=60000))"));
    assert!(collected.contains("child steered"), "{collected}");
    server.close();
}

#[test]
fn claude_root_ultron_children_get_the_context_reminder() {
    let root = Root::new();
    let (port, requests) = fake_provider(Arc::new(|body: &Value| {
        if has_tool_result(body) {
            Reply::Text("child done".into())
        } else {
            Reply::Cell("print('child cell')".into())
        }
    }));
    // A margin past the whole window: the child's first cell result is already at the trigger.
    let mut server = Server::start(&root, port, &[("ULTRON_CONTEXT_REMINDER_TOKENS", "100000000")]);
    let id = server.cell("h = await rlm.spawn('CHILD-BRIEF: run one cell', name='celler')\nprint(h.rlm_child_id)");
    wait_until("the child sends its cell result", || {
        requests.lock().unwrap().iter().any(has_tool_result)
    });
    let result = requests.lock().unwrap().iter().find(|b| has_tool_result(b)).unwrap()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap()["content"]
        .to_string();
    assert!(result.contains("child cell"), "{result}");
    assert!(
        result.contains("tokens used before compaction; free space with ctx now"),
        "{result}"
    );
    let _ = server.cell(&format!("await rlm.collect(['{id}'], timeout_ms=60000)"));
    server.close();
}

/// `ultron hook wait-events` against `socket`: its exit code and stderr.
fn wait_events(socket: &str) -> (i32, String) {
    let args: Vec<String> = ["wait-events", "--socket", socket]
        .iter()
        .map(|a| a.to_string())
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = claude_root::hooks::run(&args, &mut std::io::empty(), true, &|_| None, &mut out, &mut err);
    (code, String::from_utf8(err).unwrap())
}

fn hook(socket: &str, event: &str) {
    let _ = claude_root::hooks::run_hook(event, Some(PathBuf::from(socket)), "{}", &|_| None);
}

#[test]
fn claude_root_a_child_ending_while_claude_is_idle_wakes_it_through_the_wait_events_hook() {
    // The cell never waits (the turn is not held); the asyncRewake hook started after the rlm call wakes the idle
    // model with the child's end instead.
    let root = Root::new();
    let release = root.path("w/release");
    let gate = release.clone();
    let (port, requests) = fake_provider(Arc::new(move |_: &Value| {
        wait_until("the test releases the child", || gate.exists());
        Reply::Text("child answer".into())
    }));
    // The launcher passes `--wake` when its hooks wake the root.
    let mut server = Server::start_with(&root, port, &[], &["--wake"]);
    let socket = server.running.socket();
    hook(&socket, "user-prompt");
    let id = server.cell("h = await rlm.spawn('CHILD-BRIEF: answer later', name='waiter')\nprint(h.rlm_child_id)");
    wait_until("the child asks its model", || !requests.lock().unwrap().is_empty());
    let refused = server.cell("try:\n    await rlm.collect([h])\nexcept RuntimeError as e:\n    print(e)");
    assert!(refused.contains("1 subagent(s) still running"), "{refused}");
    // One waiter per rlm call: the newer one takes over and the older returns without waking anyone.
    let first = {
        let socket = socket.clone();
        std::thread::spawn(move || wait_events(&socket))
    };
    wait_until("the first waiter waits", || server.running.event_waiters() == 1);
    let second = {
        let socket = socket.clone();
        std::thread::spawn(move || wait_events(&socket))
    };
    assert_eq!(first.join().unwrap(), (0, String::new()));
    // The model ends its turn; then the child ends and the waiter wakes it with the child_done event.
    hook(&socket, "stop");
    std::fs::write(&release, "").unwrap();
    let (code, woken) = second.join().unwrap();
    assert_eq!(code, 2, "{woken}");
    assert!(woken.contains("child_done") && woken.contains(&id), "{woken}");
    // Nothing runs and nothing waits now: a new waiter returns without waking.
    assert_eq!(wait_events(&socket), (0, String::new()));
    server.close();
}

/// The first user message of a request (what tells the root's requests from a child's and a grandchild's).
fn first_user(body: &Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["role"] == "user"))
        .map(Value::to_string)
        .unwrap_or_default()
}

/// A cell that starts `rlm.collect([h])`, writes `gate` once the request is out, and prints what it returned.
fn collect_then_gate(spawn: &str, gate: &Path) -> String {
    format!(
        "import asyncio\n{spawn}\nc = asyncio.ensure_future(rlm.collect([h]))\nawait asyncio.sleep(0)\nopen({gate:?}, 'w').close()\ntry:\n    r = await c\n    print('got', r[0]['result']['value'])\nexcept RuntimeError as e:\n    print('raised', e)",
        gate = gate.display().to_string()
    )
}

#[test]
fn claude_root_without_wake_collect_waits_in_the_cell_and_honours_timeout_ms() {
    // A root nothing wakes (no `--wake`: `-p`, `--no-hooks`, a Claude child's server) waits for its subagents in the
    // cell; `timeout_ms` bounds the wait.
    let root = Root::new();
    let release = root.path("w/release");
    let gate = release.clone();
    let (port, _requests) = fake_provider(Arc::new(move |_: &Value| {
        wait_until("the test releases the child", || gate.exists());
        Reply::Text("child answer".into())
    }));
    let mut server = Server::start(&root, port, &[]);
    let id = server.cell("h = await rlm.spawn('CHILD-BRIEF: answer later', name='slow')\nprint(h.rlm_child_id)");
    let timed_out =
        server.cell("try:\n    await rlm.collect([h], timeout_ms=300)\nexcept RuntimeError as e:\n    print(e)");
    assert_eq!(
        timed_out,
        format!(
            "rlm.collect: timed out after 300 ms with 1 subagent(s) still running ({id}); they keep running. `await rlm.results()` returns the results of those that ended and the ids still running; collect again to wait more."
        )
    );
    let collected = server.cell(&collect_then_gate("pass", &release));
    assert_eq!(collected, "got child answer");
    server.close();
}

#[test]
fn claude_root_ultron_child_collect_waits_for_a_grandchild_and_its_guide_has_no_root_apis() {
    let root = Root::new();
    let gate = root.path("w/collecting");
    let child_cell = collect_then_gate("h = await rlm.spawn('GRAND-BRIEF: answer', name='grand')", &gate);
    let waits = gate.clone();
    let (port, requests) = fake_provider(Arc::new(move |body: &Value| {
        let first = first_user(body);
        if first.contains("GRAND-BRIEF") {
            wait_until("the child collects", || waits.exists());
            return Reply::Text("grand answer".into());
        }
        if has_tool_result(body) {
            Reply::Text("child done".into())
        } else {
            Reply::Cell(child_cell.clone())
        }
    }));
    let mut server = Server::start(&root, port, &[]);
    let collected = server.cell(
        "h = await rlm.spawn('CHILD-BRIEF: delegate', name='kid', depth=1)\nr = await rlm.collect([h])\nprint(r[0]['result']['value'])",
    );
    assert_eq!(collected, "child done");
    let requests = requests.lock().unwrap().clone();
    let child: Vec<&Value> = requests
        .iter()
        .filter(|b| first_user(b).contains("CHILD-BRIEF"))
        .collect();
    let result = child.iter().find(|b| has_tool_result(b)).unwrap()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap()["content"]
        .to_string();
    // The child's lane waited in its cell for the grandchild.
    assert!(result.contains("got grand answer"), "{result}");
    // An in-memory child is told to await before replying, without the APIs it cannot use here.
    let system = child[0]["messages"][0].to_string();
    assert!(
        system.contains("Nothing needs polling, and nothing wakes you: your reply ends your run."),
        "{system}"
    );
    assert!(!system.contains("schedule.every"), "{system}");
    assert!(!system.contains("swarm.route"), "{system}");
    assert!(!system.contains("keep=True"), "{system}");
    server.close();
}
