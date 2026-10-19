//! Shared test helpers: a scripted provider server and the format-4 journal normalization of
//! `fixtures/print_mode.mjs` (`normalizeJournal`).
#![allow(dead_code)]

use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The system line of a session-naming request.
pub const NAMING_SYSTEM: &str = "Name this coding session in 3-6 words";

/// How the server answers naming requests.
#[derive(Clone, Debug)]
pub enum Naming {
    /// A text reply with this title.
    Title(String),
    /// HTTP 500.
    Fail,
    /// Wait this long, then reply with this title.
    Slow(Duration, String),
}

/// What the scripted server saw.
#[derive(Default)]
pub struct Seen {
    pub requests: Vec<Value>,
    pub naming: Vec<Value>,
}

pub fn anthropic_text(text: &str) -> String {
    let events = [
        json!({"type": "message_start", "message": {"id": "msg_1", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ];
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

pub fn completions_text(text: &str) -> String {
    let chunk = |delta: Value, finish: Value, usage: Option<Value>| {
        let mut c = json!({"id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1, "model": "m", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
        if let Some(u) = usage {
            c["usage"] = u;
        }
        format!("data: {c}\n\n")
    };
    format!(
        "{}{}{}data: [DONE]\n\n",
        chunk(json!({"role": "assistant", "content": ""}), Value::Null, None),
        chunk(json!({"content": text}), Value::Null, None),
        chunk(
            json!({}),
            json!("stop"),
            Some(json!({"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}))
        ),
    )
}

/// Serve each scripted reply once (naming requests are answered by `naming` and recorded apart).
pub fn mock_server(replies: Vec<String>, naming: Naming) -> (u16, Arc<Mutex<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let shared = seen.clone();
    std::thread::spawn(move || {
        let mut next = 0;
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut parts = line.split_whitespace();
            let (method, url) = (parts.next().unwrap().to_string(), parts.next().unwrap().to_string());
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
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
            let body: Value = serde_json::from_slice(&body).unwrap();
            let record = json!({"method": method, "url": url, "body": body});
            let completions = url.ends_with("/chat/completions");
            if body.to_string().contains(NAMING_SYSTEM) {
                shared.lock().unwrap().naming.push(record);
                let naming = naming.clone();
                std::thread::spawn(move || {
                    let (status, reply) = match naming {
                        Naming::Fail => ("500 Internal Server Error", "{\"error\":\"down\"}".to_string()),
                        Naming::Title(t) => (
                            "200 OK",
                            if completions {
                                completions_text(&t)
                            } else {
                                anthropic_text(&t)
                            },
                        ),
                        Naming::Slow(wait, t) => {
                            std::thread::sleep(wait);
                            (
                                "200 OK",
                                if completions {
                                    completions_text(&t)
                                } else {
                                    anthropic_text(&t)
                                },
                            )
                        }
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        reply.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(reply.as_bytes());
                });
                continue;
            }
            shared.lock().unwrap().requests.push(record);
            let reply = replies.get(next).cloned().unwrap_or_default();
            next += 1;
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(reply.as_bytes()).unwrap();
        }
    });
    (port, seen)
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
}

fn uuid_at(bytes: &[u8], i: usize) -> bool {
    const SHAPE: [usize; 5] = [8, 4, 4, 4, 12];
    let mut at = i;
    for (n, len) in SHAPE.iter().enumerate() {
        for _ in 0..*len {
            if at >= bytes.len() || !is_hex(bytes[at]) {
                return false;
            }
            at += 1;
        }
        if n < 4 {
            if bytes.get(at) != Some(&b'-') {
                return false;
            }
            at += 1;
        }
    }
    true
}

/// `renameIds`: every UUID-shaped string as `00000000-0000-7000-8000-<n>` in order of first appearance.
pub fn rename_ids(text: &str, ids: &mut Vec<String>) -> String {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut start = 0;
    while i < bytes.len() {
        if uuid_at(bytes, i) {
            out.push_str(&text[start..i]);
            let id = &text[i..i + 36];
            let n = match ids.iter().position(|x| x == id) {
                Some(n) => n,
                None => {
                    ids.push(id.to_string());
                    ids.len() - 1
                }
            };
            out.push_str(&format!("00000000-0000-7000-8000-{:012}", n + 1));
            i += 36;
            start = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&text[start..]);
    out
}

pub fn zero_times(value: &Value) -> Value {
    const KEYS: [&str; 8] = [
        "timestamp",
        "createdAt",
        "startedAt",
        "endedAt",
        "lastAt",
        "since",
        "updatedAt",
        "requestedAt",
    ];
    match value {
        Value::Array(items) => Value::Array(items.iter().map(zero_times).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let v = if KEYS.contains(&k.as_str()) && v.is_number() {
                        json!(0)
                    } else {
                        zero_times(v)
                    };
                    (k.clone(), v)
                })
                .collect::<Map<_, _>>(),
        ),
        other => other.clone(),
    }
}

/// `normalizeJournal` of print_mode.mjs.
pub fn normalize_journal(text: &str) -> Value {
    let mut lines = text.lines().filter(|l| !l.is_empty());
    let mut header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
    let mut transactions: Vec<Vec<Value>> = lines
        .map(|line| match serde_json::from_str::<Value>(line).unwrap() {
            Value::Array(items) => items,
            one => vec![one],
        })
        .map(|records| {
            let auto = records
                .iter()
                .any(|r| r["namespace"] == "ultron.module" && r["key"] == "session-name");
            records
                .into_iter()
                .filter(|r| {
                    r["namespace"] != "ultron.module"
                        && r["namespace"] != "pi.pending.assistant_frame"
                        && !(auto && r["namespace"] == "pi.session.name")
                        && !(r["kind"] == "usage" && r["details"]["source"] == "session-naming")
                })
                .collect::<Vec<_>>()
        })
        .filter(|records| !records.is_empty())
        .collect();
    let seqs: Vec<i64> = transactions
        .iter()
        .flatten()
        .map(|r| r["seq"].as_i64().unwrap())
        .collect();
    for record in transactions.iter_mut().flatten() {
        let seq = record["seq"].as_i64().unwrap();
        record["seq"] = json!(seqs.iter().position(|s| *s == seq).unwrap() + 1);
    }
    if let Some(next) = header.get("nextSeq").and_then(Value::as_i64) {
        header["nextSeq"] = json!(seqs.iter().filter(|s| **s < next).count() + 1);
    }
    let whole = json!({"header": header, "transactions": transactions});
    let renamed = rename_ids(&session_file::js::stringify(&whole), &mut Vec::new());
    zero_times(&serde_json::from_str(&renamed).unwrap())
}

/// The header's cwd and the message entries of a format-4 journal (one chain from the root), timestamps zeroed.
pub fn transcript(text: &str) -> (String, Vec<Value>) {
    let mut cwd = String::new();
    let mut messages = Vec::new();
    let mut parent = Value::Null;
    for line in text.lines().filter(|l| !l.is_empty()) {
        let records = match serde_json::from_str::<Value>(line).unwrap() {
            Value::Array(records) => records,
            record => vec![record],
        };
        for record in records {
            if record["kind"] == "header" {
                cwd = record["cwd"].as_str().unwrap().to_string();
            }
            if record["kind"] == "entry" && record["type"] == "message" {
                assert_eq!(record["parentId"], parent, "entry chain");
                parent = record["id"].clone();
                let mut message = record["message"].clone();
                message["timestamp"] = Value::from(0);
                messages.push(message);
            }
        }
    }
    (cwd, messages)
}

pub fn session_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(session_files(&path));
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

/// A private agent/server/cwd under one temp dir, with `models.json` pointing providers at `port`.
pub struct Sandbox {
    pub tmp: tempfile::TempDir,
    pub root: std::path::PathBuf,
    pub agent_dir: std::path::PathBuf,
    pub cwd: std::path::PathBuf,
}

impl Sandbox {
    pub fn new(port: u16) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let (agent_dir, cwd) = (root.join("agent"), root.join("cwd"));
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let sandbox = Sandbox {
            tmp,
            root,
            agent_dir,
            cwd,
        };
        sandbox.point_at(port);
        sandbox
    }

    pub fn point_at(&self, port: u16) {
        std::fs::write(
            self.agent_dir.join("models.json"),
            format!(
                r#"{{"providers":{{"anthropic":{{"baseUrl":"http://127.0.0.1:{port}"}},"groq":{{"baseUrl":"http://127.0.0.1:{port}/v1"}}}}}}"#
            ),
        )
        .unwrap();
    }

    pub fn sessions_dir(&self) -> std::path::PathBuf {
        self.agent_dir.join("experimental").join("sessions")
    }

    /// The Rust `ultron` with private dirs.
    pub fn ultron(&self) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ultron"));
        command
            .env_clear()
            .current_dir(&self.cwd)
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.root)
            .env("ULTRON_CODING_AGENT_DIR", &self.agent_dir)
            .env("ULTRON_SERVER_DIR", self.root.join("server"))
            .env("ANTHROPIC_API_KEY", "test-key")
            .env("NO_COLOR", "1")
            .env("TZ", "UTC")
            // Requests are compared with TS recordings: the TS runtime guide, without the Rust-only lines.
            .env("ULTRON_GUIDE", "ts");
        command
    }
}

/// An Anthropic SSE reply that calls `rlm` with `code`.
pub fn anthropic_tool(id: &str, code: &str) -> String {
    let events = [
        json!({"type": "message_start", "message": {"id": "msg_2", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 10, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": id, "name": "rlm", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": json!({"code": code}).to_string()}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
        json!({"type": "message_stop"}),
    ];
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// The text of the tool result `id` in a request body (Anthropic `tool_result` blocks).
pub fn tool_result_text(body: &Value, id: &str) -> Option<String> {
    for message in body["messages"].as_array()? {
        for block in message["content"].as_array().into_iter().flatten() {
            if block["type"] == "tool_result" && block["tool_use_id"] == id {
                return Some(match &block["content"] {
                    Value::String(text) => text.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|p| p["text"].as_str())
                        .collect::<Vec<_>>()
                        .join(""),
                    other => other.to_string(),
                });
            }
        }
    }
    None
}

/// Guard: the sandbox's HOME is inside the system temp dir, never a real home.
pub fn assert_private_home(home: &std::path::Path) {
    let temp = std::env::temp_dir().canonicalize().unwrap();
    assert!(
        home.starts_with(&temp),
        "test HOME {} is not under {}",
        home.display(),
        temp.display()
    );
    if let Some(real) = std::env::var_os("HOME") {
        assert_ne!(home, std::path::Path::new(&real), "test HOME is the real home");
    }
}

/// A scripted provider for a parent and its subagents: a request whose first user message contains `child_marker`
/// is a subagent's and gets the next `child` reply; every other request gets the next `parent` reply. Requests are
/// recorded in arrival order as `{method, url, body, child}`.
pub fn routed_server(parent: Vec<String>, child_marker: &str, child: Vec<String>) -> (u16, Arc<Mutex<Vec<Value>>>) {
    routed_server_held(parent, child_marker, child, 0)
}

/// [`routed_server`] whose subagent replies are held until `parent_requests` parent requests have arrived: a child
/// then cannot finish before the parent reached that point, however the processes are scheduled.
pub fn routed_server_held(
    parent: Vec<String>,
    child_marker: &str,
    child: Vec<String>,
    parent_requests: usize,
) -> (u16, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = seen.clone();
    let marker = child_marker.to_string();
    std::thread::spawn(move || {
        let (mut next_parent, mut next_child) = (0, 0);
        // Child replies not yet sent (a parent request reaching the count releases them).
        let mut held: Vec<(std::net::TcpStream, String)> = Vec::new();
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
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
            let is_child = first_user.contains(&marker);
            let reply = if is_child {
                next_child += 1;
                child.get(next_child - 1).cloned()
            } else {
                next_parent += 1;
                parent.get(next_parent - 1).cloned()
            }
            .unwrap_or_else(|| anthropic_text("(no more replies)"));
            shared.lock().unwrap().push(json!({"body": body, "child": is_child}));
            // A parent's reply goes out at once, after any child reply its arrival releases.
            let mut out = Vec::new();
            if next_parent >= parent_requests {
                out.append(&mut held);
            }
            if is_child && next_parent < parent_requests {
                held.push((stream, reply));
            } else {
                out.push((stream, reply));
            }
            for (mut stream, reply) in out {
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    reply.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(reply.as_bytes());
            }
        }
    });
    (port, seen)
}
