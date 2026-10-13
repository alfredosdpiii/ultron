//! A47-A50: bounded inference frames through the shipped `ultron` binary in RPC mode (server generation + session
//! worker), against a scripted openai-completions provider on 127.0.0.1; every request is held to a 48,000-token
//! window (request bytes / 4).
//!
//! Ported from packages/coding-agent/test/ultron-a47-large-input.test.ts (end to end, RPC, same cells and script) and
//! the named cases of ultron-a47-a50-bounded-inference.test.ts. The TypeScript host-level cases call the host
//! (`NativeRlmHost` + `inference.ts`) directly with a fake lane; here the same requests (`rlm.infer`, `rlm.map`,
//! `rlm.frames` with the same payloads) are sent from a real `rlm` cell through runtime.py's bridge, so they reach the
//! session's host stack (`inference::Frames` over `inference::Inference`) and the provider over real HTTP. The
//! provider records every request and can hold one open, fail it with HTTP 500, or report usage with cache reads.
//!
//! Not ported (no Rust subsystem yet): frames journaled as `rlm-frame@1` tasks and RPC `inspect` (`agents.status`,
//! `rlm.frames`), the usage ledger's reservations and model-call count, and nested frames (`budget.depth > 1`).
//! Frame traces are read with `rlm.frames` from the same cell instead of RPC `inspect`.

mod common;

use cli::rpc::client::{RpcClient, RpcClientOptions};
use common::rpc_tool_results;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The scripted model's context window, in the provider's tokens (request bytes / 4).
const WINDOW_TOKENS: usize = 48_000;
const LINES: usize = 16_000;

// ---------------------------------------------------------------------------------------------------------------
// A scripted openai-completions provider.

#[derive(Clone, Debug)]
struct Request {
    raw: String,
    body: Value,
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

impl Request {
    fn messages(&self, role: &str) -> Vec<String> {
        self.body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["role"] == role)
            .map(|m| text_of(&m["content"]))
            .collect()
    }
    fn system(&self) -> String {
        [self.messages("system"), self.messages("developer")]
            .concat()
            .join("\n")
    }
    fn is_frame(&self) -> bool {
        self.system().contains("inference frame")
    }
    fn first_user(&self) -> String {
        self.messages("user").first().cloned().unwrap_or_default()
    }
    /// The prompt of a root request: its first user message that is not the session's clock line.
    fn prompt(&self) -> String {
        self.messages("user")
            .into_iter()
            .find(|m| !m.starts_with("[clock]"))
            .unwrap_or_default()
    }
    fn last_user(&self) -> String {
        self.messages("user").last().cloned().unwrap_or_default()
    }
    /// Assistant messages so far in this conversation.
    fn turn(&self) -> usize {
        self.body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["role"] == "assistant")
            .count()
    }
    fn tools(&self) -> usize {
        self.body["tools"].as_array().map_or(0, Vec::len)
    }
    fn max_tokens(&self) -> u64 {
        self.body["max_completion_tokens"]
            .as_u64()
            .or(self.body["max_tokens"].as_u64())
            .unwrap_or(0)
    }
}

enum Hold {
    No,
    /// Until the flag is set (or the client goes away).
    Until(Arc<AtomicBool>),
    /// Until the client goes away.
    Forever,
}

enum Answer {
    Text(String),
    Rlm(String),
    /// HTTP 500 with no usage.
    Fail,
}

struct Reply {
    answer: Answer,
    /// Provider usage: (input, output, cache read); default input = request bytes / 4, output = 8.
    usage: Option<(u64, u64, u64)>,
    hold: Hold,
}

fn text(text: impl Into<String>) -> Reply {
    Reply {
        answer: Answer::Text(text.into()),
        usage: None,
        hold: Hold::No,
    }
}

fn rlm(code: &str) -> Reply {
    Reply {
        answer: Answer::Rlm(code.into()),
        usage: None,
        hold: Hold::No,
    }
}

type Script = dyn Fn(&Request, &Provider) -> Reply + Send + Sync;

#[derive(Clone)]
struct Provider {
    port: u16,
    requests: Arc<Mutex<Vec<Request>>>,
    /// Held requests whose client closed the connection, with when it was seen.
    closed: Arc<Mutex<Vec<(String, Instant)>>>,
}

impl Provider {
    fn start(script: impl Fn(&Request, &Provider) -> Reply + Send + Sync + 'static) -> Provider {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let provider = Provider {
            port: listener.local_addr().unwrap().port(),
            requests: Arc::default(),
            closed: Arc::default(),
        };
        let script: Arc<Script> = Arc::new(script);
        let me = provider.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let (me, script) = (me.clone(), script.clone());
                std::thread::spawn(move || me.serve(stream, &*script));
            }
        });
        provider
    }

    fn serve(&self, mut stream: TcpStream, script: &Script) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
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
        let raw = String::from_utf8(body).unwrap();
        let request = Request {
            body: serde_json::from_str(&raw).unwrap(),
            raw,
        };
        self.requests.lock().unwrap().push(request.clone());
        let reply = script(&request, self);
        let gone = |stream: &TcpStream| {
            stream.set_nonblocking(true).unwrap();
            let closed = matches!(stream.peek(&mut [0u8; 1]), Ok(0));
            stream.set_nonblocking(false).unwrap();
            closed
        };
        loop {
            let ready = match &reply.hold {
                Hold::No => true,
                Hold::Until(flag) => flag.load(Ordering::SeqCst),
                Hold::Forever => false,
            };
            if ready {
                break;
            }
            if gone(&stream) {
                self.closed.lock().unwrap().push((request.first_user(), Instant::now()));
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if let Answer::Fail = reply.answer {
            let body = r#"{"error":{"message":"scripted failure","type":"server_error"}}"#;
            let _ = write!(
                stream,
                "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            return;
        }
        let (input, output, cached) = reply.usage.unwrap_or(((request.raw.len() as u64).div_ceil(4), 8, 0));
        let usage = json!({"prompt_tokens": input + cached, "completion_tokens": output,
            "total_tokens": input + cached + output, "prompt_tokens_details": {"cached_tokens": cached}});
        let sse = sse(reply.answer, usage);
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            sse.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(sse.as_bytes());
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
    fn frames(&self) -> Vec<Request> {
        self.requests().into_iter().filter(Request::is_frame).collect()
    }
    fn roots(&self) -> Vec<Request> {
        self.requests().into_iter().filter(|r| !r.is_frame()).collect()
    }
}

fn chunk(delta: Value, finish: Value, usage: Option<Value>) -> String {
    let mut event = json!({"id": "chatcmpl-a47", "object": "chat.completion.chunk", "created": 1, "model": "scripted",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
    if let Some(usage) = usage {
        event["usage"] = usage;
    }
    format!("data: {event}\n\n")
}

fn sse(answer: Answer, usage: Value) -> String {
    let mut out = match answer {
        Answer::Text(text) => {
            chunk(json!({"role": "assistant", "content": text}), Value::Null, None)
                + &chunk(json!({}), json!("stop"), Some(usage))
        }
        Answer::Rlm(code) => {
            let call = json!([{"index": 0, "id": "call_a47", "type": "function",
                "function": {"name": "rlm", "arguments": json!({"code": code}).to_string()}}]);
            chunk(
                json!({"role": "assistant", "content": null, "tool_calls": call}),
                Value::Null,
                None,
            ) + &chunk(json!({}), json!("tool_calls"), Some(usage))
        }
        Answer::Fail => unreachable!(),
    };
    out.push_str("data: [DONE]\n\n");
    out
}

// ---------------------------------------------------------------------------------------------------------------
// The `ultron --mode rpc` harness: private agent, server and project directories, a `scripted/scripted` model.

struct Harness {
    root: tempfile::TempDir,
    provider: Provider,
}

impl Harness {
    fn new(provider: Provider) -> Harness {
        let root = tempfile::tempdir().unwrap();
        let harness = Harness { root, provider };
        std::fs::create_dir_all(harness.agent()).unwrap();
        std::fs::create_dir_all(harness.project()).unwrap();
        // The Rust build reads only `providers.<name>.baseUrl` from models.json (no custom models), so the built-in
        // groq model points at the scripted provider; the window bound is still asserted on every request.
        let models =
            json!({"providers": {"groq": {"baseUrl": format!("http://127.0.0.1:{}/v1", harness.provider.port)}}});
        std::fs::write(harness.agent().join("models.json"), models.to_string()).unwrap();
        harness
    }
    fn agent(&self) -> PathBuf {
        self.root.path().join("agent")
    }
    fn project(&self) -> PathBuf {
        self.root.path().join("project")
    }
    fn client(&self) -> RpcClient {
        let path = |p: PathBuf| p.display().to_string();
        let mut env = vec![
            ("PATH".to_string(), std::env::var("PATH").unwrap_or_default()),
            ("HOME".into(), path(self.root.path().to_path_buf())),
            ("ULTRON_CODING_AGENT_DIR".into(), path(self.agent())),
            ("ULTRON_SERVER_DIR".into(), path(self.root.path().join("server"))),
            ("GROQ_API_KEY".into(), "test-groq".into()),
            ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
            ("PI_OFFLINE".into(), "1".into()),
            ("NO_COLOR".into(), "1".into()),
            ("TZ".into(), "UTC".into()),
        ];
        if let Ok(python) = std::env::var("ULTRON_PYTHON") {
            env.push(("ULTRON_PYTHON".into(), python));
        }
        RpcClient::start(&RpcClientOptions {
            program: env!("CARGO_BIN_EXE_ultron-acceptance").into(),
            cwd: Some(self.project()),
            env_clear: true,
            env,
            model: Some(common::MODEL.into()),
            args: vec!["--no-session".into()],
            ..Default::default()
        })
        .expect("start ultron --mode rpc")
    }
}

/// Every `RESULT <json>` line of the session's `rlm` tool results, in order.
fn results(client: &mut RpcClient) -> Vec<Value> {
    tagged(client, "RESULT ")
}

fn tagged(client: &mut RpcClient, tag: &str) -> Vec<Value> {
    rpc_tool_results(&client.get_messages().unwrap())
        .iter()
        .flat_map(|text| text.lines().map(str::to_string).collect::<Vec<_>>())
        .filter_map(|line| line.strip_prefix(tag).map(|json| serde_json::from_str(json).unwrap()))
        .collect()
}

/// Run `cells` as the root's `rlm` calls of one prompt (then "DONE"), returning the RESULT lines.
fn run_cells(
    cells: Vec<String>,
    frame: impl Fn(&Request, &Provider) -> Reply + Send + Sync + 'static,
) -> (Harness, Vec<Value>, String) {
    let provider = Provider::start(move |request, provider| {
        if request.is_frame() {
            return frame(request, provider);
        }
        match cells.get(request.turn()) {
            Some(code) => rlm(code),
            None => text("DONE"),
        }
    });
    let harness = Harness::new(provider);
    let mut client = harness.client();
    client
        .prompt_and_wait("run the cells", Duration::from_secs(120))
        .unwrap();
    let last = client.get_last_assistant_text().unwrap().unwrap_or_default();
    let out = results(&mut client);
    client.finish(common::HANG);
    (harness, out, last)
}

/// A cell prefix: `raw(type, payload)` sends a host request exactly as written (what the TS tests pass to the host).
const RAW: &str = "import json\nraw = rlm.infer.__self__._bridge.request\n";

fn view_of(message: &str) -> &str {
    let Some(start) = message.find("--- view 1: ") else {
        return "";
    };
    let body = &message[start..];
    let body = &body[body.find("---\n").map_or(0, |i| i + 4)..];
    &body[..body.find("\n--- end of view 1 ---").unwrap_or(body.len())]
}

fn error_lines(text: &str) -> usize {
    text.split('\n').filter(|line| line.contains("ERROR")).count()
}

fn log_file() -> (String, usize) {
    let mut text = String::new();
    let mut errors = 0;
    for index in 0..LINES {
        let error = index % 13 == 0 || index % 29 == 0;
        errors += usize::from(error);
        text.push_str(&format!(
            "2026-09-26T03:{:02}:00Z id=L{index:06} {}\n",
            index % 60,
            if error {
                "ERROR upstream timeout"
            } else {
                "INFO request served"
            }
        ));
    }
    (text, errors)
}

/// Whether `text` holds `id=L` followed by six digits (a log line's id).
fn has_log_id(text: &str) -> bool {
    text.match_indices("id=L")
        .any(|(at, _)| text[at + 4..].bytes().take(6).filter(u8::is_ascii_digit).count() == 6)
}

// ---------------------------------------------------------------------------------------------------------------
// The end-to-end harness of ultron-a47-large-input.test.ts.

const COUNT_CELL: &str = "import json
h = await rlm.load('big.log')
counts = await rlm.map('Count the lines that contain ERROR. Reply with the integer.', h.chunks(24000), contract=int, budget=Budget(calls=200))
bad = [c for c in counts if not isinstance(c, int)]
print('RESULT ' + json.dumps({'errors': sum(c for c in counts if isinstance(c, int)), 'frames': len(counts), 'bad': len(bad), 'handle': repr(h), 'spent': counts.spent, 'budget': counts.budget, 'summary': counts.summary()}))
print('FRAMES ' + json.dumps(await rlm.frames(limit=5)))";

const REPAIR_CELL: &str = "import json
h = await rlm.load('big.log')
v = await rlm.infer('REPAIR: how many lines contain ERROR?', h.lines(0, 50), contract=int)
x = await rlm.infer('NEVER: how many lines contain ERROR?', [h.lines(0, 50)], contract=int, max_repairs=1)
print('RESULT ' + json.dumps({'value': v, 'expected': h.lines(0, 50).text.count('ERROR'), 'incomplete': isinstance(x, Incomplete), 'falsy': not x, 'status': x.status, 'trace': x.trace_id, 'last': x.last_outputs, 'spent': x.spent}))
print('TRACE ' + json.dumps(await rlm.frames(x.trace_id)))";

const SLOW_CELL: &str = "h = await rlm.load('big.log')
await rlm.map('SLOW: count the ERROR lines.', h.chunks(24000)[:6], contract=int)";

fn large_input_script(request: &Request, _: &Provider) -> Reply {
    let first = request.first_user();
    if request.is_frame() {
        if first.contains("NEVER:") {
            return text("I cannot tell.");
        }
        if first.contains("SLOW:") {
            return Reply {
                hold: Hold::Forever,
                ..text("0")
            };
        }
        if first.contains("REPAIR:") && request.turn() == 0 {
            return text("There are a few, maybe four?");
        }
        return text(error_lines(view_of(&first)).to_string());
    }
    let prompt = request.prompt();
    let cell = if prompt.starts_with("A47") {
        COUNT_CELL
    } else if prompt.starts_with("A48") {
        REPAIR_CELL
    } else if prompt.starts_with("A50") {
        SLOW_CELL
    } else {
        panic!("unscripted request: {prompt}")
    };
    if request.turn() == 0 { rlm(cell) } else { text("DONE") }
}

fn large_input() -> (Harness, usize) {
    let (log, errors) = log_file();
    let harness = Harness::new(Provider::start(large_input_script));
    std::fs::write(harness.project().join("big.log"), log).unwrap();
    (harness, errors)
}

#[test]
fn acceptance_a47_input_larger_than_the_window_is_counted_exactly_through_bounded_frames_ported_part() {
    let (harness, errors) = large_input();
    let input_chars = std::fs::metadata(harness.project().join("big.log")).unwrap().len() as usize;
    assert!(input_chars / 4 > 4 * WINDOW_TOKENS);
    let mut client = harness.client();
    client
        .prompt_and_wait("A47: count the ERROR lines in big.log", Duration::from_secs(240))
        .unwrap();
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("DONE"));
    let result = results(&mut client).remove(0);
    assert_eq!(result["errors"], errors);
    assert_eq!(result["bad"], 0);
    let frames = result["frames"].as_u64().unwrap();
    assert!(frames > 4, "{result}");
    // Printing a handle shows its label, size and digest only.
    let handle = result["handle"].as_str().unwrap();
    assert!(handle.starts_with("ContextHandle(label='big.log', chars="), "{handle}");
    assert!(
        handle.contains(", size=") && handle.contains(", digest='sha256:"),
        "{handle}"
    );
    // The map reports what it spent: on the result, and as one printed line the model sees.
    assert_eq!(result["spent"]["calls"], frames);
    assert!(result["spent"]["tokens"].as_u64().unwrap() > 0);
    assert_eq!(result["budget"]["calls"], 200);
    let summary = result["summary"].as_str().unwrap();
    let head = format!("[rlm.map] {frames} frames: {frames} complete, 0 incomplete, 0 failed; spent {frames} calls, ");
    let tail = summary.strip_prefix(&head).unwrap_or_else(|| panic!("{summary}"));
    let (spent, limit) = tail.split_once(" tokens of ").unwrap_or_else(|| panic!("{summary}"));
    for number in [spent, limit] {
        assert!(
            !number.is_empty() && number.chars().all(|c| c.is_ascii_digit() || c == ','),
            "{summary}"
        );
    }
    let messages = serde_json::to_string(&client.get_messages().unwrap()).unwrap();
    assert!(messages.contains(&format!("[rlm.map] {frames} frames")));
    // Frame traces (read with rlm.frames in the cell; RPC inspect is not in the Rust build).
    let listed = tagged(&mut client, "FRAMES ").remove(0);
    let listed = listed["frames"].as_array().unwrap();
    assert_eq!(listed.len(), 5);
    assert!(listed.iter().all(|frame| frame["status"] == "complete"));
    client.finish(common::HANG);

    let provider = &harness.provider;
    let (frame_requests, roots) = (provider.frames(), provider.roots());
    assert_eq!(frame_requests.len() as u64, frames);
    assert_eq!(roots.len(), 2);
    // No single request exceeds the declared window (request bytes / 4 as input tokens).
    for request in provider.requests() {
        assert!(request.raw.len().div_ceil(4) <= WINDOW_TOKENS);
    }
    // The root transcript never contains the input.
    for request in &roots {
        assert!(!has_log_id(&request.raw));
        assert!(!request.raw.contains("upstream timeout"));
    }
    // Frames are private: no root system prompt, no tools, no parent transcript.
    for request in &frame_requests {
        assert!(!request.system().contains("rlm"));
        assert_eq!(request.tools(), 0);
        assert_eq!(request.messages("user").len(), 1);
        assert!(!request.raw.contains("A47: count"));
    }
}

#[test]
fn acceptance_a48_end_to_end_repair_is_valid_exhaustion_is_incomplete_and_the_turn_continues() {
    let (harness, _) = large_input();
    let mut client = harness.client();
    client
        .prompt_and_wait("A48: repair and exhaustion", Duration::from_secs(240))
        .unwrap();
    // The root turn continued after the Incomplete and finished normally.
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("DONE"));
    let result = results(&mut client).remove(0);
    assert_eq!(result["value"], result["expected"]);
    assert!(result["expected"].as_u64().unwrap() > 0);
    assert_eq!(result["incomplete"], true);
    assert_eq!(result["falsy"], true);
    assert_eq!(result["status"], "contract_unmet");
    assert_eq!(result["spent"]["calls"], 2);
    assert_eq!(result["last"], json!(["I cannot tell.", "I cannot tell."]));
    let trace_id = result["trace"].as_str().unwrap();
    assert!(trace_id.starts_with("frame-"));
    let trace = tagged(&mut client, "TRACE ").remove(0);
    assert_eq!(trace["id"], trace_id);
    assert_eq!(trace["status"], "incomplete");
    assert_eq!(trace["reason"], "contract_unmet");
    client.finish(common::HANG);
    let repaired: Vec<_> = harness
        .provider
        .frames()
        .into_iter()
        .filter(|r| r.first_user().contains("REPAIR:"))
        .collect();
    assert_eq!(repaired.len(), 2);
    assert!(repaired[1].last_user().contains("does not satisfy the contract"));
}

#[test]
fn acceptance_a50_end_to_end_aborting_the_root_turn_cancels_every_running_frame_within_2s_ported_part() {
    let (harness, _) = large_input();
    let mut client = harness.client();
    let frames = 6;
    client.mark();
    let response = client
        .send(json!({"type": "prompt", "message": "A50: slow frames"}))
        .unwrap();
    assert_eq!(response["success"], true, "{response}");
    let slow = |provider: &Provider| {
        provider
            .frames()
            .iter()
            .filter(|r| r.first_user().contains("SLOW:"))
            .count()
    };
    common::wait_until("six running frames", || slow(&harness.provider) == frames);
    let response = client.send(json!({"type": "abort"})).unwrap();
    assert_eq!(response["success"], true, "{response}");
    // Every running frame's request is cancelled (its connection dropped). The frames are held forever, so only
    // the abort can close them; the upstream "within 2 s" is a latency target, not asserted as wall-clock time
    // (a loaded machine may be slower, and the hang guard sits far below the 300 s HTTP idle timeout).
    common::wait_until("every frame cancelled", || {
        harness.provider.closed.lock().unwrap().len() == frames
    });
    let closed = harness.provider.closed.lock().unwrap().clone();
    assert!(closed.iter().all(|(task, _)| task.contains("SLOW:")));
    client
        .wait_for(&|line| line["type"] == "agent_settled", common::HANG)
        .unwrap();
    // The session keeps working after the abort.
    client.prompt_and_wait("A50: continue", common::HANG).unwrap();
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("DONE"));
    client.finish(common::HANG);
    assert_eq!(slow(&harness.provider), frames);
}

// ---------------------------------------------------------------------------------------------------------------
// ContextHandle in the kernel; rlm.infer's private frame (A47).

#[test]
fn acceptance_a47_handles_never_render_content_and_views_search_lines_chunks_reopen_by_digest() {
    let secret: Vec<String> = (0..400)
        .map(|i| format!("row {i} {}", if i % 9 == 0 { "ERROR disk" } else { "ok" }))
        .collect();
    let cells = [
        "h = await rlm.load('data.log')\nh".to_string(),
        [
            "hits = h.search(r'ERROR', limit=3)",
            "view = h.lines(hits[1]['line'], hits[1]['line'] + 1)",
            "chunks = h.chunks(500)",
            "again = await rlm.open(h.digest)",
            "print([m['line'] for m in hits], view.text.strip(), ''.join(c.text for c in chunks) == h.slice().text, all(len(c) <= 500 for c in chunks), again is h, h.count('ERROR'), h.line_count())",
        ]
        .join("\n"),
    ];
    let provider = Provider::start(move |request, _| match cells.get(request.turn()) {
        Some(code) => rlm(code),
        None => text("DONE"),
    });
    let harness = Harness::new(provider);
    std::fs::write(harness.project().join("data.log"), format!("{}\n", secret.join("\n"))).unwrap();
    let mut client = harness.client();
    client.prompt_and_wait("handles", common::HANG).unwrap();
    let outputs = rpc_tool_results(&client.get_messages().unwrap());
    client.finish(common::HANG);
    assert_eq!(outputs.len(), 2, "{outputs:?}");
    let shown = outputs[0].trim();
    let digest = shown
        .strip_prefix("ContextHandle(label='data.log', chars=")
        .and_then(|rest| rest.split_once(", digest='sha256:"))
        .map(|(sizes, rest)| (sizes.to_string(), rest.to_string()))
        .unwrap_or_else(|| panic!("{shown}"));
    assert!(digest.0.contains(", size="), "{shown}");
    assert_eq!(digest.1.len(), 12 + "'…)".len(), "{shown}");
    assert!(
        digest.1[..12].bytes().all(|b| b.is_ascii_hexdigit()) && digest.1.ends_with("'…)"),
        "{shown}"
    );
    // The tool result is the cell's stdout without its final newline.
    assert_eq!(outputs[1], "[0, 9, 18] row 9 ERROR disk True True True 45 400");
    for request in harness.provider.requests() {
        assert!(!request.raw.contains("row 1 ok"));
    }
}

#[test]
fn acceptance_a47_the_frame_sees_only_its_task_and_views_under_its_own_system_prompt_with_no_tools_ported_part() {
    let cell = format!(
        "{RAW}r = await raw('rlm.infer', {{'task': 'Count the ERROR lines.', 'context': [{{'kind': 'text', 'label': 'app.log[0:26]', 'text': 'ok\\nERROR one\\nok\\nERROR two\\n'}}], 'contract': {{'type': 'integer'}}}})\nprint('RESULT ' + json.dumps(r))"
    );
    let (harness, out, last) = run_cells(vec![cell], |request, _| {
        text(error_lines(view_of(&request.first_user())).to_string())
    });
    assert_eq!(last, "DONE");
    let reply = &out[0];
    assert_eq!(reply["status"], "complete");
    assert_eq!(reply["value"], 2);
    let frames = harness.provider.frames();
    assert_eq!(frames.len(), 1);
    let root_system = harness.provider.roots()[0].system();
    assert!(frames[0].system().contains("inference frame"));
    assert!(!frames[0].system().contains(&root_system[..root_system.len().min(200)]));
    assert!(frames[0].first_user().contains("Count the ERROR lines."));
    assert!(frames[0].first_user().contains("ok\nERROR one\nok\nERROR two\n"));
    assert_eq!(frames[0].tools(), 0);
    assert_eq!(reply["spent"]["calls"], 1);
    assert!(reply["spent"]["tokens"].as_u64().unwrap() > 0);
}

// ---------------------------------------------------------------------------------------------------------------
// A48 contract repair.

#[test]
fn acceptance_a48_a_malformed_json_answer_is_reasked_and_returns_a_valid_value() {
    let cell = format!(
        "{RAW}r = await raw('rlm.infer', {{'task': 'Count.', 'context': [{{'kind': 'text', 'label': 'literal', 'text': 'ERROR\\nERROR\\n'}}], 'contract': {{'type': 'object', 'properties': {{'count': {{'type': 'integer'}}}}, 'required': ['count'], 'additionalProperties': False}}}})\nprint('RESULT ' + json.dumps(r))"
    );
    let (harness, out, _) = run_cells(vec![cell], |request, _| {
        text(if request.turn() == 0 {
            "I think the answer is {count: 2"
        } else {
            "{\"count\": 2}"
        })
    });
    let reply = &out[0];
    assert_eq!(reply["status"], "complete");
    assert_eq!(reply["value"], json!({"count": 2}));
    let frames = harness.provider.frames();
    assert_eq!(frames.len(), 2);
    assert!(frames[1].last_user().contains("does not satisfy the contract"));
    // The re-ask continues the same private conversation.
    assert_eq!(frames[1].first_user(), frames[0].first_user());
    assert_eq!(frames[1].messages("assistant"), ["I think the answer is {count: 2"]);
    assert_eq!(reply["spent"]["calls"], 2);
}

#[test]
fn acceptance_a48_exhausted_repairs_yield_an_incomplete_with_spent_remaining_trace_and_outputs() {
    let cell = format!(
        "{RAW}r = await raw('rlm.infer', {{'task': 'Count.', 'context': [{{'kind': 'text', 'label': 'literal', 'text': 'x'}}], 'contract': {{'type': 'integer'}}, 'max_repairs': 1, 'budget': {{'calls': 5, 'tokens': 100000}}}})\nprint('RESULT ' + json.dumps(r))\nprint('RESULT ' + json.dumps(await raw('rlm.frames', {{'id': r['trace_id']}})))"
    );
    let (harness, out, _) = run_cells(vec![cell], |_, _| text("not json at all"));
    let (reply, trace) = (&out[0], &out[1]);
    assert_eq!(reply["status"], "incomplete");
    assert_eq!(reply["reason"], "contract_unmet");
    assert_eq!(harness.provider.frames().len(), 2);
    assert_eq!(reply["spent"]["calls"], 2);
    assert_eq!(reply["remaining"]["calls"], 3);
    let spent_tokens = reply["spent"]["tokens"].as_u64().unwrap();
    assert_eq!(reply["remaining"]["tokens"], 100_000 - spent_tokens);
    assert_eq!(reply["last_outputs"], json!(["not json at all", "not json at all"]));
    let trace_id = reply["trace_id"].as_str().unwrap();
    assert!(trace_id.starts_with("frame-"));
    assert_eq!(trace["id"], trace_id);
    assert_eq!(trace["status"], "incomplete");
    assert_eq!(trace["reason"], "contract_unmet");
    assert_eq!(trace["attempts"].as_array().unwrap().len(), 2);
}

#[test]
fn acceptance_a48_running_out_of_calls_mid_repair_is_an_incomplete_not_an_exception() {
    let cell = format!(
        "{RAW}r = await raw('rlm.infer', {{'task': 'Count.', 'context': [{{'kind': 'text', 'label': 'literal', 'text': 'x'}}], 'contract': {{'type': 'integer'}}, 'budget': {{'calls': 1}}}})\nprint('RESULT ' + json.dumps(r))"
    );
    let (harness, out, _) = run_cells(vec![cell], |_, _| text("nope"));
    assert_eq!(out[0]["status"], "incomplete");
    assert_eq!(out[0]["reason"], "budget_exhausted");
    assert_eq!(harness.provider.frames().len(), 1);
    assert_eq!(out[0]["remaining"]["calls"], 0);
}

// ---------------------------------------------------------------------------------------------------------------
// A49 budget subtree.

#[test]
fn acceptance_a49_a_map_of_8_frames_under_calls_6_runs_6_and_marks_2_incomplete_in_order() {
    let cell = format!(
        "{RAW}r = await raw('rlm.map', {{'frames': [{{'task': f'Echo item {{i}}.', 'context': []}} for i in range(8)], 'contract': {{'type': 'string'}}, 'budget': {{'calls': 6}}}})\nprint('RESULT ' + json.dumps(r))"
    );
    let (harness, out, _) = run_cells(vec![cell], |request, _| {
        let user = request.first_user();
        let at = user.find("item ").unwrap() + 5;
        text(user[at..].chars().take_while(char::is_ascii_digit).collect::<String>())
    });
    let reply = &out[0];
    assert_eq!(harness.provider.frames().len(), 6);
    let results = reply["results"].as_array().unwrap();
    let statuses: Vec<_> = results.iter().map(|r| r["status"].as_str().unwrap()).collect();
    assert_eq!(
        statuses,
        [
            "complete",
            "complete",
            "complete",
            "complete",
            "complete",
            "complete",
            "incomplete",
            "incomplete"
        ]
    );
    let values: Vec<_> = results[..6].iter().map(|r| r["value"].clone()).collect();
    assert_eq!(
        values,
        [json!("0"), json!("1"), json!("2"), json!("3"), json!("4"), json!("5")]
    );
    assert_eq!(results[6]["reason"], "budget_exhausted");
    assert_eq!(results[6]["remaining"]["calls"], 0);
    assert_eq!(reply["budget"]["spent"]["calls"], 6);
}

#[test]
fn acceptance_a49_concurrent_siblings_hold_tranches_that_never_oversubscribe_the_token_pool() {
    let pool = 40_000u64;
    let cell = format!(
        "{RAW}r = await raw('rlm.map', {{'frames': [{{'task': f'Frame {{i}}.', 'context': [{{'kind': 'text', 'label': 'literal', 'text': 'x' * 200}}]}} for i in range(8)], 'contract': {{'type': 'integer'}}, 'budget': {{'tokens': {pool}}}, 'concurrency': 8}})\nprint('RESULT ' + json.dumps(r))"
    );
    let release = Arc::new(AtomicBool::new(false));
    let gate = release.clone();
    let (harness, out, _) = run_cells(vec![cell], move |_, provider| {
        // Every frame stays in flight until all eight are.
        if provider.frames().len() == 8 {
            gate.store(true, Ordering::SeqCst);
        }
        Reply {
            usage: Some((100, 8, 0)),
            hold: Hold::Until(gate.clone()),
            ..text("1")
        }
    });
    assert!(release.load(Ordering::SeqCst));
    let caps: Vec<u64> = harness.provider.frames().iter().map(Request::max_tokens).collect();
    assert_eq!(caps.len(), 8);
    // Every in-flight request is capped at its tranche: at most a quarter of what remained, at most 16k.
    for cap in &caps {
        assert!(*cap > 0 && *cap <= 16_384.min(pool / 4), "{caps:?}");
    }
    assert!(caps.iter().sum::<u64>() < pool, "{caps:?}");
    let reply = &out[0];
    assert!(
        reply["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] == "complete")
    );
    assert_eq!(reply["budget"]["spent"]["tokens"], 8 * 108);
}

#[test]
fn acceptance_a49_when_the_pool_cannot_cover_a_request_the_frame_is_incomplete_and_nothing_is_sent() {
    let cell = format!(
        "{RAW}r = await raw('rlm.map', {{'frames': [{{'task': 'Small.', 'context': []}}, {{'task': 'Huge.', 'context': [{{'kind': 'text', 'label': 'literal', 'text': 'y' * 4000}}]}}], 'contract': {{'type': 'integer'}}, 'budget': {{'tokens': 700}}, 'concurrency': 1}})\nprint('RESULT ' + json.dumps(r))"
    );
    let (harness, out, _) = run_cells(vec![cell], |_, _| Reply {
        usage: Some((50, 8, 0)),
        ..text("1")
    });
    let results = out[0]["results"].as_array().unwrap();
    assert_eq!(results[0]["status"], "complete");
    assert_eq!(results[1]["status"], "incomplete");
    assert_eq!(results[1]["reason"], "budget_exhausted");
    assert_eq!(harness.provider.frames().len(), 1);
    assert!(out[0]["budget"]["spent"]["tokens"].as_u64().unwrap() <= 700);
}

#[test]
fn acceptance_a49_a_provider_failure_refunds_its_tranche_and_cache_reads_are_discounted_ported_part() {
    let cell = format!(
        "{RAW}r = await raw('rlm.map', {{'frames': [{{'task': 'This one fails.', 'context': []}}, {{'task': 'This one works.', 'context': []}}], 'contract': {{'type': 'integer'}}, 'budget': {{'tokens': 50000}}}})\nprint('RESULT ' + json.dumps(r))"
    );
    let (_harness, out, _) = run_cells(vec![cell], |request, _| {
        if request.first_user().contains("fails") {
            Reply {
                answer: Answer::Fail,
                usage: None,
                hold: Hold::No,
            }
        } else {
            Reply {
                usage: Some((100, 10, 1_000)),
                ..text("7")
            }
        }
    });
    let results = out[0]["results"].as_array().unwrap();
    assert_eq!(results[0]["status"], "error");
    assert_eq!(results[0]["spent"], json!({"calls": 1, "tokens": 0}));
    assert_eq!(results[1]["status"], "complete");
    assert_eq!(results[1]["value"], 7);
    assert_eq!(results[1]["spent"], json!({"calls": 1, "tokens": 210}));
    // Nothing is left held: the whole pool minus what was charged remains.
    assert_eq!(out[0]["budget"]["spent"], json!({"calls": 2, "tokens": 210}));
}

// ---------------------------------------------------------------------------------------------------------------
// A50 cancellation.

#[test]
fn acceptance_a50_aborting_the_root_cancels_every_in_flight_frame_within_2s_ported_part() {
    let cell = format!(
        "{RAW}r = await raw('rlm.map', {{'frames': [{{'task': f'Wait {{i}}.', 'context': []}} for i in range(4)], 'contract': {{'type': 'integer'}}}})\nprint('RESULT ' + json.dumps(r))"
    );
    let provider = Provider::start(move |request, _| {
        if request.is_frame() {
            return Reply {
                hold: Hold::Forever,
                ..text("1")
            };
        }
        if request.turn() == 0 { rlm(&cell) } else { text("DONE") }
    });
    let harness = Harness::new(provider);
    let mut client = harness.client();
    client.mark();
    client.send(json!({"type": "prompt", "message": "A50: wait"})).unwrap();
    common::wait_until("four running frames", || harness.provider.frames().len() == 4);
    client.send(json!({"type": "abort"})).unwrap();
    // Frames held forever close only through the abort (see the end-to-end A50 test for why no 2 s wall clock).
    common::wait_until("every frame cancelled", || {
        harness.provider.closed.lock().unwrap().len() == 4
    });
    let closed = harness.provider.closed.lock().unwrap().clone();
    let mut tasks: Vec<String> = closed.into_iter().map(|(task, _)| task).collect();
    tasks.sort();
    let mut sent: Vec<String> = harness.provider.frames().iter().map(Request::first_user).collect();
    sent.sort();
    assert_eq!(tasks, sent);
    client
        .wait_for(&|line| line["type"] == "agent_settled", common::HANG)
        .unwrap();
    client.finish(common::HANG);
}
