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
        std::fs::write(
            root.path("a/models.json"),
            json!({"providers": {"groq": {"baseUrl": format!("http://127.0.0.1:{port}/v1")}}}).to_string(),
        )
        .unwrap();
        let argv: Vec<String> = [
            "--children",
            "ultron",
            "--ultron-child-model",
            "groq/llama-3.3-70b-versatile",
        ]
        .iter()
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
