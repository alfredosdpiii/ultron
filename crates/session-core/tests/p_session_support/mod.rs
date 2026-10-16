//! A session actor (`session_core::core`) in a private home against a scripted Anthropic server on loopback: the
//! replies are SSE bodies served in order, every request body is kept. Nothing reads the real environment or home.
#![allow(dead_code)]

use serde_json::{Value, json};
use session_core::core::{Input, Op};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A hang guard, never a pass condition.
pub const HANG: Duration = Duration::from_secs(120);

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn start(id: &str) -> Value {
    json!({"type": "message_start", "message": {"id": id, "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}})
}

/// A text answer.
pub fn text(text: &str) -> String {
    sse(&[
        start("msg_1"),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ])
}

/// One `rlm` call running `code`.
pub fn tool(id: &str, code: &str) -> String {
    sse(&[
        start("msg_2"),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": id, "name": "rlm", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": json!({"code": code}).to_string()}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
        json!({"type": "message_stop"}),
    ])
}

/// A transient provider error (retryable).
pub fn overloaded() -> String {
    let error = json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}});
    format!("event: error\ndata: {error}\n\n")
}

/// A provider error that is not retryable.
pub fn invalid() -> String {
    let error = json!({"type": "error", "error": {"type": "invalid_request_error", "message": "Bad request"}});
    format!("event: error\ndata: {error}\n\n")
}

/// How the server picks a reply for a request: in order, or by what the request says.
pub type Script = Box<dyn FnMut(&Value, usize) -> String + Send>;

/// Replies in order; past the end, "(no more replies)".
pub fn in_order(replies: Vec<String>) -> Script {
    Box::new(move |_, index| replies.get(index).cloned().unwrap_or_else(|| text("(no more replies)")))
}

/// Serve `script` on loopback; the request bodies are kept.
pub fn server(mut script: Script) -> (u16, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut length = 0;
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
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
                continue;
            }
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let index = {
                let mut seen = seen.lock().unwrap();
                seen.push(body.clone());
                seen.len() - 1
            };
            let reply = script(&body, index);
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    (port, requests)
}

/// What a session starts with.
#[derive(Default)]
pub struct Setup {
    pub settings: Option<Value>,
    pub vars: Vec<(&'static str, String)>,
    pub files: Vec<(&'static str, Vec<u8>)>,
}

pub struct Actor {
    pub tx: Sender<Input>,
    actor: Option<std::thread::JoinHandle<()>>,
    pub events: Arc<Mutex<Vec<Value>>>,
    pub requests: Arc<Mutex<Vec<Value>>>,
    pub root: PathBuf,
    pub cwd: PathBuf,
    _tmp: tempfile::TempDir,
}

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

/// A session on `anthropic/claude-sonnet-4-5` against `script`.
pub fn actor(script: Script, setup: Setup) -> Actor {
    let (port, requests) = server(script);
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (agent_dir, cwd) = (root.join("agent"), root.join("cwd"));
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        agent_dir.join("models.json"),
        json!({"providers": {"anthropic": {"baseUrl": format!("http://127.0.0.1:{port}")}}}).to_string(),
    )
    .unwrap();
    if let Some(settings) = &setup.settings {
        std::fs::write(agent_dir.join("settings.json"), settings.to_string()).unwrap();
    }
    for (name, bytes) in &setup.files {
        std::fs::write(cwd.join(name), bytes).unwrap();
    }
    let mut vars: Vec<(String, String)> = vec![
        ("HOME".into(), root.display().to_string()),
        ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent_dir.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ANTHROPIC_API_KEY".into(), "test-key".into()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
        ("ULTRON_PYTHON".into(), python()),
        ("PI_OFFLINE".into(), "1".into()),
        ("TZ".into(), "UTC".into()),
    ];
    vars.extend(setup.vars.iter().map(|(k, v)| (k.to_string(), v.clone())));
    let env = session_core::RunEnv {
        cwd: cwd.clone(),
        home: root.clone(),
        var: Arc::new(move |key| vars.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let sessions = agent_dir.join("experimental").join("sessions");
    let repo = session_journal::SessionRepo::new(&sessions, session_journal::SystemClock::shared());
    let created = repo.create(&cwd.display().to_string(), None, None).unwrap();
    let open = session_core::core::OpenOptions {
        env,
        flags: session_core::ModelFlags {
            model: Some("anthropic/claude-sonnet-4-5".into()),
            ..Default::default()
        },
        session_root: sessions,
        metadata: created.metadata().clone(),
        mode: "rpc".into(),
        services: session_core::compose::Services::default(),
        auto_name: false,
    };
    drop(created);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let (tx, actor, _) = session_core::core::spawn(open, Box::new(move |e| sink.lock().unwrap().push(e))).unwrap();
    Actor {
        tx,
        actor: Some(actor),
        events,
        requests,
        root,
        cwd,
        _tmp: tmp,
    }
}

impl Actor {
    pub fn call(&self, op: Op) -> Result<Option<Value>, String> {
        session_core::core::call(&self.tx, op)
    }

    /// Prompt and wait for the run's end: `{accepted, operationId, error}`.
    pub fn prompt(&self, text: &str) -> Value {
        self.call(Op::PromptAndWait {
            message: text.into(),
            images: Vec::new(),
        })
        .unwrap()
        .unwrap()
    }

    pub fn events(&self, kind: &str) -> Vec<Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == kind)
            .cloned()
            .collect()
    }

    /// Waits until an event of `kind` was emitted (a hang guard, never a pass condition).
    pub fn wait_event(&self, kind: &str) -> Value {
        let end = Instant::now() + HANG;
        loop {
            if let Some(event) = self.events(kind).pop() {
                return event;
            }
            assert!(Instant::now() < end, "timed out waiting for {kind}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    /// The committed writes of the session's journal, in order.
    pub fn writes(&self) -> Vec<Value> {
        let dir = self.root.join("agent").join("experimental").join("sessions");
        let mut files = Vec::new();
        let mut stack = vec![dir];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "jsonl") {
                    files.push(path);
                }
            }
        }
        assert_eq!(files.len(), 1, "{files:?}");
        let text = std::fs::read_to_string(&files[0]).unwrap();
        text.lines()
            .skip(1)
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .flat_map(|t| t.as_array().cloned().unwrap_or_else(|| vec![t]))
            .collect()
    }

    /// Every operation state written, in order.
    pub fn op_states(&self) -> Vec<Value> {
        self.writes()
            .into_iter()
            .filter(|w| w["namespace"] == session_journal::run::OP_STATE && w["op"] == "set")
            .map(|w| w["value"].clone())
            .collect()
    }

    /// The last assistant message's text.
    pub fn last_text(&self) -> Value {
        self.call(Op::LastAssistantText).unwrap().unwrap()
    }
}

impl Drop for Actor {
    fn drop(&mut self) {
        let (done, closed) = channel();
        if self.tx.send(Input::Close(done)).is_ok()
            && closed.recv_timeout(HANG).is_ok()
            && let Some(actor) = self.actor.take()
        {
            let _ = actor.join();
        }
    }
}

/// The tool results a request carries, in order (`tool_result.content`).
pub fn tool_results(request: &Value) -> Vec<Value> {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["type"] == "tool_result")
        .map(|b| b["content"].clone())
        .collect()
}

/// The user texts a request carries (clock lines scrubbed).
pub fn user_texts(request: &Value) -> Vec<Value> {
    request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .flat_map(|m| match &m["content"] {
            Value::String(text) => vec![json!({"type": "text", "text": text})],
            Value::Array(blocks) => blocks.clone(),
            _ => Vec::new(),
        })
        .filter(|b| b["type"] == "text")
        .map(|b| {
            let text = b["text"].as_str().unwrap_or_default();
            if text.starts_with("[clock] ") {
                json!("[clock] <CLOCK>")
            } else {
                json!(text)
            }
        })
        .collect()
}

/// A solid-color RGB PNG, as the TypeScript view_image test builds it (stored deflate blocks).
pub fn png(width: u32, height: u32) -> Vec<u8> {
    let mut row = vec![0u8];
    row.extend((0..width).flat_map(|_| [220u8, 30, 30]));
    let raw = row.repeat(height as usize);
    let mut out = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    let mut chunk = |kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    };
    let mut header = Vec::new();
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(b"IHDR", &header);
    chunk(b"IDAT", &zlib_stored(&raw));
    chunk(b"IEND", &[]);
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(65535).collect()
    };
    for (i, block) in blocks.iter().enumerate() {
        out.push(u8::from(i + 1 == blocks.len()));
        out.extend_from_slice(&(block.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        out.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + u32::from(*byte)) % 65521;
        b = (b + a) % 65521;
    }
    out.extend_from_slice(&((b << 16) | a).to_be_bytes());
    out
}
