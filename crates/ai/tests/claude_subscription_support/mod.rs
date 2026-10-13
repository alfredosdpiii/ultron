//! Shared fixtures of the claude-subscription tests: the fake `claude` (`fixtures/claude-subscription/claude.py`,
//! run through a private wrapper script), a fake upstream Messages server and a raw HTTP client on loopback. No
//! test runs the real CLI, reads real credentials or reaches api.anthropic.com: every config injects a whole
//! environment and a loopback upstream.
#![allow(dead_code)]

pub mod isolate;
pub use isolate::seal;

use ai::claude_subscription::{Config, Env};
use ai::types::{
    AssistantMessageEvent, Message, Model, SystemContent, SystemMessage, Tool, ToolResultMessage, TranscriptContext,
    UserContent, UserMessage, UserMessageContent,
};
use ai::{AssistantMessage, SimpleStreamOptions, TextContent};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

pub fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/claude-subscription/claude.py")
}

pub fn inert_mcp() -> String {
    env!("CARGO_BIN_EXE_ultron-inert-mcp").to_string()
}

/// A private directory with the fake CLI's wrapper and its log.
pub struct Fake {
    pub dir: tempfile::TempDir,
    pub wrapper: PathBuf,
    pub log: PathBuf,
}

impl Fake {
    pub fn new() -> Fake {
        let dir = tempfile::tempdir().unwrap();
        let wrapper = dir.path().join("bin/claude");
        std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
        std::fs::create_dir_all(dir.path().join("tmp")).unwrap();
        std::fs::write(
            &wrapper,
            format!("#!/bin/sh\nexec {} -I {} \"$@\"\n", python(), fixture().display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let log = dir.path().join("log.jsonl");
        Fake { dir, wrapper, log }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn temp_root(&self) -> PathBuf {
        self.dir.path().join("tmp")
    }

    /// The whole child environment: system PATH, a private HOME, the log, and `vars`.
    pub fn env(&self, vars: &[(&str, &str)]) -> Env {
        let mut env = Env::new();
        env.insert("PATH".into(), "/usr/bin:/bin".into());
        env.insert("HOME".into(), self.root().display().to_string());
        env.insert("FAKE_LOG".into(), self.log.display().to_string());
        for (k, v) in vars {
            env.insert(k.to_string(), v.to_string());
        }
        env
    }

    pub fn config(&self, upstream: &str, vars: &[(&str, &str)]) -> Config {
        Config {
            command: Some(vec![self.wrapper.display().to_string()]),
            env: Some(self.env(vars)),
            inert_mcp: Some(vec![inert_mcp()]),
            // Inputs not under test; long so a loaded machine never trips them.
            timeout: Duration::from_secs(120),
            idle: Duration::from_secs(120),
            telemetry: None,
            temp_root: Some(self.temp_root()),
            upstream: upstream.to_string(),
        }
    }

    /// Every run the fake recorded, in order.
    pub fn records(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    pub fn last(&self) -> Value {
        self.records().pop().expect("the fake ran")
    }
}

/// One request the fake upstream received.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Seen {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub type Handler = Arc<dyn Fn(&Seen, &mut TcpStream) + Send + Sync>;

/// A loopback HTTP server standing in for the Messages API.
pub struct Upstream {
    pub port: u16,
    pub seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
}

fn read_request(stream: &TcpStream) -> Option<Seen> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next()?.to_string(), parts.next()?.to_string());
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        let (k, v) = header.split_once(':')?;
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let find = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };
    let mut body = Vec::new();
    if find("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).ok()?;
            let size = usize::from_str_radix(size.trim(), 16).ok()?;
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).ok()?;
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    } else if let Some(length) = find("content-length") {
        body = vec![0; length.parse().ok()?];
        reader.read_exact(&mut body).ok()?;
    }
    Some(Seen {
        method,
        path,
        headers,
        body,
    })
}

impl Upstream {
    pub fn start(handler: impl Fn(&Seen, &mut TcpStream) + Send + Sync + 'static) -> Upstream {
        let handler: Handler = Arc::new(handler);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (seen, stop) = (seen.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream.set_nonblocking(false).unwrap();
                            let (seen, handler) = (seen.clone(), handler.clone());
                            std::thread::spawn(move || {
                                if let Some(request) = read_request(&stream) {
                                    seen.lock().unwrap().push(request.clone());
                                    handler(&request, &mut stream);
                                }
                            });
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            });
        }
        Upstream { port, seen, stop }
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

pub fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

pub fn reply_sse(stream: &mut TcpStream, events: &[Value]) {
    let body = sse(events);
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nrequest-id: req_fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

pub fn reply_status(stream: &mut TcpStream, status: u16, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// The SSE events of a reply with `blocks` (complete blocks, tool input streamed as JSON) and `stop`.
pub fn reply_events(id: &str, blocks: &[Value], stop: &str, usage: Value) -> Vec<Value> {
    let mut events = vec![
        json!({"type": "message_start", "message": {"id": id, "type": "message", "role": "assistant", "model": "claude-sonnet-5-5", "content": [], "stop_reason": null, "usage": usage}}),
    ];
    for (index, block) in blocks.iter().enumerate() {
        match block["type"].as_str().unwrap() {
            "text" => {
                events.push(json!({"type": "content_block_start", "index": index, "content_block": {"type": "text", "text": ""}}));
                events.push(json!({"type": "content_block_delta", "index": index, "delta": {"type": "text_delta", "text": block["text"]}}));
            }
            "thinking" => {
                events.push(json!({"type": "content_block_start", "index": index, "content_block": {"type": "thinking", "thinking": ""}}));
                events.push(json!({"type": "content_block_delta", "index": index, "delta": {"type": "thinking_delta", "thinking": block["thinking"]}}));
                events.push(json!({"type": "content_block_delta", "index": index, "delta": {"type": "signature_delta", "signature": block["signature"]}}));
            }
            "tool_use" => {
                events.push(json!({"type": "content_block_start", "index": index, "content_block": {"type": "tool_use", "id": block["id"], "name": block["name"], "input": {}}}));
                events.push(json!({"type": "content_block_delta", "index": index, "delta": {"type": "input_json_delta", "partial_json": block["input"].to_string()}}));
            }
            other => panic!("block type {other}"),
        }
        events.push(json!({"type": "content_block_stop", "index": index}));
    }
    events.push(json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": usage}));
    events.push(json!({"type": "message_stop"}));
    events
}

/// POST `body` to `url` (`http://127.0.0.1:port/path`) with raw HTTP/1.1; (status, body).
pub fn post(url: &str, body: &[u8], headers: &[(&str, &str)]) -> (u16, Vec<u8>) {
    let rest = url.strip_prefix("http://").unwrap();
    let (host, path) = rest.split_once('/').unwrap();
    let mut stream = TcpStream::connect(host).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let mut head = format!(
        "POST /{path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let text = String::from_utf8_lossy(&response).into_owned();
    let status = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    let body = match text.split_once("\r\n\r\n") {
        Some((_, body)) => body.as_bytes().to_vec(),
        None => Vec::new(),
    };
    (status, body)
}

// ---------------------------------------------------------------------------------------------------- transcripts

pub fn rlm_tool() -> Tool {
    Tool {
        name: "rlm".into(),
        description: "Run Python in the persistent REPL. TAIL".into(),
        parameters: json!({"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"]}),
        constrained_sampling: None,
    }
}

pub fn system(prompt: &str, tools: Vec<Tool>) -> Message {
    Message::System(SystemMessage {
        content: SystemContent::Text(prompt.into()),
        sections: None,
        tools_added: (!tools.is_empty()).then_some(tools),
        tools_removed: None,
        timestamp: 0,
    })
}

pub fn user(text: &str) -> Message {
    Message::User(UserMessage {
        content: UserMessageContent::Text(text.into()),
        timestamp: 0,
    })
}

pub fn tool_result(id: &str, text: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "rlm".into(),
        content: vec![UserContent::Text(TextContent {
            text: text.into(),
            text_signature: None,
        })],
        details: None,
        usage: None,
        is_error: false,
        timestamp: 0,
    })
}

pub fn context(messages: Vec<Message>) -> TranscriptContext {
    TranscriptContext { messages }
}

/// `claude-subscription/<id>` against `upstream`.
pub fn model(id: &str) -> Model {
    ai::claude_subscription::catalog::model(id).unwrap()
}

pub fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Run one call; the final message and every event.
pub fn call(
    config: Config,
    model: &Model,
    context: &TranscriptContext,
    options: &SimpleStreamOptions,
) -> (AssistantMessage, Vec<AssistantMessageEvent>) {
    let events = Mutex::new(Vec::new());
    let mut sink = |event: &AssistantMessageEvent| events.lock().unwrap().push(event.clone());
    let message = runtime().block_on(ai::claude_subscription::stream_with(
        model, context, options, config, &mut sink,
    ));
    (message, events.into_inner().unwrap())
}

/// Whether a process is gone (or a zombie no one waits for).
pub fn gone(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
    }
}

pub fn wait_gone(pids: &[u32]) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        if pids.iter().all(|p| gone(*p)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

pub fn diagnostic<'m>(message: &'m AssistantMessage, kind: &str) -> Option<&'m ai::types::AssistantMessageDiagnostic> {
    message.diagnostics.iter().flatten().find(|d| d.kind == kind)
}
