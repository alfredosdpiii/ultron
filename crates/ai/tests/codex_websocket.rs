//! The OpenAI Codex WebSocket transport replayed against `fixtures/codex-websocket.json`
//! (`fixtures/generate-codex-websocket.mjs`: the TypeScript provider with Node's own WebSocket). The same scripted
//! fake Codex WebSocket server runs here on 127.0.0.1 (RFC 6455 over std sockets), reached through the real
//! `ReqwestTransport` WebSocket client; SSE fallback requests get the same canned responses. Every call must emit the
//! same events and end with the same message; every connection must see the same handshake headers, the same
//! `response.create` messages (deltas included) and the same close frames; the debug stats must agree.

use ai::http::{BoxFuture, HeaderMap};
use ai::json::normalize_numbers;
use ai::openai_codex_responses::{self, OpenAICodexResponsesOptions};
use ai::options::pi_user_agent;
use ai::websocket::WebSocketConnection;
use ai::{AbortSignal, AssistantMessageEvent, Body, HttpRequest, HttpResponse, Model, TranscriptContext, Transport};
use protocol::websocket::{Message, MessageDecoder, OPCODE_PING, accept_key, close_payload, encode_message};
use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::{Duration, Instant};

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/codex-websocket.json")).unwrap());

const UNRECORDED: [&str; 13] = [
    "host",
    "connection",
    "upgrade",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-extensions",
    "accept",
    "accept-language",
    "accept-encoding",
    "sec-fetch-mode",
    "pragma",
    "cache-control",
    "content-length",
];

fn is_uuid_v7(text: &str) -> bool {
    text.len() == 36
        && text.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            14 => c == '7',
            19 => "89ab".contains(c),
            _ => c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
        })
}

/// What the fake server saw, as the generator records it, plus a condition variable for waiting on it.
#[derive(Default)]
struct Log {
    connections: Mutex<Vec<Value>>,
    changed: Condvar,
}

impl Log {
    fn update(&self, index: usize, change: impl FnOnce(&mut Value)) {
        let mut connections = self.connections.lock().unwrap();
        change(&mut connections[index]);
        self.changed.notify_all();
    }

    /// Wait (hang guard 120 s) until the connections are `want`; then they are returned for the comparison.
    fn wait_for(&self, want: &Value) -> Value {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut connections = self.connections.lock().unwrap();
        loop {
            let got = Value::Array(connections.clone());
            if &got == want || Instant::now() >= deadline {
                return got;
            }
            connections = self
                .changed
                .wait_timeout(connections, Duration::from_secs(1))
                .unwrap()
                .0;
        }
    }
}

struct FakeCodex {
    port: u16,
    log: Arc<Log>,
    scripts: Arc<Mutex<Vec<Value>>>,
}

fn read_head(stream: &TcpStream) -> Option<(String, Map<String, Value>)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let path = line.split_whitespace().nth(1)?.to_string();
    let mut headers = Map::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), json!(value.trim()));
        }
    }
    Some((path, headers))
}

fn send(stream: &mut TcpStream, message: &Message) {
    let _ = stream.write_all(&encode_message(message, None));
}

fn serve(mut stream: TcpStream, index: usize, script: Value, log: Arc<Log>) {
    let Some((path, headers)) = read_head(&stream) else {
        return;
    };
    let key = headers
        .get("sec-websocket-key")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let user_agent = pi_user_agent();
    let mut recorded: Vec<(String, Value)> = headers
        .iter()
        .filter(|(name, _)| !UNRECORDED.contains(&name.as_str()))
        .map(|(name, value)| {
            let value = value.as_str().unwrap();
            let value = if is_uuid_v7(value) {
                "<uuidv7>".to_string()
            } else {
                value.replace(&user_agent, "<pi-user-agent>")
            };
            (name.clone(), json!(value))
        })
        .collect();
    recorded.sort_by(|a, b| a.0.cmp(&b.0));
    log.update(index, |conn| {
        conn["path"] = json!(path);
        conn["headers"] = Value::Object(recorded.into_iter().collect());
    });
    match script["handshake"].as_str() {
        Some("reject") | None => {
            let _ = stream
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            return;
        }
        Some("destroy") => return,
        Some("hang") => {
            // Held open without an answer until the client gives up.
            let mut sink = Vec::new();
            let _ = stream.read_to_end(&mut sink);
            return;
        }
        _ => {}
    }
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            accept_key(&key)
        )
        .as_bytes(),
    );
    let mut decoder = MessageDecoder::server(64 * 1024 * 1024);
    let mut buffer = vec![0u8; 64 * 1024];
    let mut received = 0usize;
    let mut closed = false;
    loop {
        let n = match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let Ok(messages) = decoder.push(&buffer[..n]) else {
            return;
        };
        for message in messages {
            match message {
                Message::Text(text) => {
                    let parsed: Value = serde_json::from_str(&text).unwrap();
                    log.update(index, |conn| conn["received"].as_array_mut().unwrap().push(parsed));
                    received += 1;
                    let turn = &script["turns"][received - 1];
                    if turn.is_null() {
                        continue;
                    }
                    if turn["ping"] == true {
                        let _ = stream.write_all(&protocol::websocket::encode_frame(OPCODE_PING, b"hb", None));
                    }
                    for event in turn["events"].as_array().into_iter().flatten() {
                        let text = match event {
                            Value::String(text) => text.clone(),
                            other => other.to_string(),
                        };
                        if turn["binary"] == true {
                            send(&mut stream, &Message::Binary(text.into_bytes()));
                        } else {
                            send(&mut stream, &Message::Text(text));
                        }
                    }
                    match &turn["then"] {
                        Value::String(action) if action == "drop" => {
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                            return;
                        }
                        Value::Array(close) => {
                            closed = true;
                            let code = close[0].as_u64().map(|c| c as u16);
                            let reason = close.get(1).and_then(Value::as_str).unwrap_or_default();
                            let _ = stream.write_all(&protocol::websocket::encode_frame(
                                protocol::websocket::OPCODE_CLOSE,
                                &close_payload(code, reason),
                                None,
                            ));
                        }
                        _ => {}
                    }
                }
                Message::Close(code, reason) => {
                    log.update(index, |conn| {
                        conn["closes"]
                            .as_array_mut()
                            .unwrap()
                            .push(json!({"code": code, "reason": reason}))
                    });
                    if !closed {
                        send(&mut stream, &Message::Close(code, String::new()));
                    }
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    return;
                }
                Message::Pong(_) => log.update(index, |conn| {
                    let pongs = conn["pongs"].as_u64().unwrap();
                    conn["pongs"] = json!(pongs + 1);
                }),
                Message::Binary(_) | Message::Ping(_) => {}
            }
        }
    }
}

impl FakeCodex {
    fn start() -> FakeCodex {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Log::default());
        let scripts: Arc<Mutex<Vec<Value>>> = Arc::default();
        let (shared_log, shared_scripts) = (log.clone(), scripts.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let index = {
                    let mut connections = shared_log.connections.lock().unwrap();
                    connections.push(json!({"path": null, "headers": {}, "received": [], "closes": [], "pongs": 0}));
                    connections.len() - 1
                };
                let script = shared_scripts
                    .lock()
                    .unwrap()
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| json!({"handshake": "reject"}));
                let log = shared_log.clone();
                std::thread::spawn(move || serve(stream, index, script, log));
            }
        });
        FakeCodex { port, log, scripts }
    }
}

struct ListBody(std::vec::IntoIter<Vec<u8>>);

impl Body for ListBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

/// SSE from the canned responses; WebSockets through the real client.
struct Replay {
    sse: Mutex<Vec<Value>>,
    requests: Mutex<Vec<HttpRequest>>,
    real: ai::ReqwestTransport,
}

impl Transport for Replay {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        let canned = {
            let mut queue = self.sse.lock().unwrap();
            (!queue.is_empty()).then(|| queue.remove(0))
        };
        Box::pin(async move {
            let canned = canned.ok_or("unexpected request")?;
            let chunks: Vec<Vec<u8>> = canned["chunks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c.as_str().unwrap().as_bytes().to_vec())
                .collect();
            Ok(HttpResponse {
                status: 200,
                headers: [("content-type".to_string(), "text/event-stream".to_string())].into(),
                body: Box::new(ListBody(chunks.into_iter())),
            })
        })
    }

    fn websocket_available(&self) -> bool {
        true
    }

    fn websocket(
        &self,
        url: String,
        headers: HeaderMap,
    ) -> BoxFuture<'_, Result<Box<dyn WebSocketConnection>, String>> {
        self.real.websocket(url, headers)
    }
}

const SCRATCH_KEYS: [&str; 6] = [
    "index",
    "partialJson",
    "partialArgs",
    "customInput",
    "streamIndex",
    "redactedChunks",
];

/// Timestamps and scratch fields: the generator pins `Date.now()`; Rust reads the clock.
fn pin(message: &mut Value, now: &Value) {
    let Some(object) = message.as_object_mut() else {
        return;
    };
    object.insert("timestamp".into(), now.clone());
    if let Some(Value::Array(diagnostics)) = object.get_mut("diagnostics") {
        for diagnostic in diagnostics {
            diagnostic["timestamp"] = now.clone();
        }
    }
    if let Some(Value::Array(content)) = object.get_mut("content") {
        for block in content {
            if let Some(block) = block.as_object_mut() {
                for key in SCRATCH_KEYS {
                    block.shift_remove(key);
                }
            }
        }
    }
}

fn pin_event(event: &mut Value, now: &Value) {
    for key in ["partial", "message", "error"] {
        if let Some(message) = event.get_mut(key) {
            pin(message, now);
        }
    }
}

fn normalize_port(value: &Value, port: u16) -> Value {
    serde_json::from_str(&value.to_string().replace(&port.to_string(), "<port>")).unwrap()
}

fn same(what: &str, expected: &Value, actual: &Value, diffs: &mut Vec<String>) {
    let (e, a) = (normalize_numbers(expected.clone()), normalize_numbers(actual.clone()));
    if e != a {
        diffs.push(format!("{what}\n  expected {e}\n  actual   {a}"));
    }
}

async fn replay(scenario: &Value, server: &FakeCodex) -> Vec<String> {
    let name = scenario["name"].as_str().unwrap();
    let now = FIXTURE["now"].clone();
    let port = server.port;
    server.log.connections.lock().unwrap().clear();
    *server.scripts.lock().unwrap() = scenario["scripts"].as_array().unwrap().clone();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let transport = Replay {
        sse: Mutex::new(scenario["sse"].as_array().unwrap().clone()),
        requests: Mutex::default(),
        real: ai::ReqwestTransport::with_client(client, None),
    };
    let model: Model = serde_json::from_value(
        serde_json::from_str(&FIXTURE["model"].to_string().replace("<port>", &port.to_string())).unwrap(),
    )
    .unwrap();
    let mut diffs = Vec::new();
    for (index, call) in scenario["calls"].as_array().unwrap().iter().enumerate() {
        let what = |part: &str| format!("{name} call {index} {part}");
        if let Some(ms) = call["advanceMs"].as_i64().filter(|ms| *ms > 0) {
            openai_codex_responses::advance_openai_codex_websocket_clock(ms);
        }
        let context: TranscriptContext = serde_json::from_value(call["context"].clone()).unwrap();
        let mut options: OpenAICodexResponsesOptions = serde_json::from_value(call["options"].clone()).unwrap();
        let signal = AbortSignal::new();
        options.base.signal = Some(signal.clone());
        let abort_on = call["abortOn"]
            .as_object()
            .map(|a| (a["type"].as_str().unwrap().to_string(), a["nth"].as_u64().unwrap()));
        let mut events = Vec::new();
        let mut seen = 0;
        let mut sink = |event: &AssistantMessageEvent| {
            let value = serde_json::to_value(event).unwrap();
            if let Some((kind, nth)) = &abort_on
                && value["type"] == kind.as_str()
            {
                seen += 1;
                if seen == *nth {
                    signal.abort();
                }
            }
            events.push(value);
        };
        transport.requests.lock().unwrap().clear();
        let sink: &mut ai::types::EventSink<'_> = &mut sink;
        // Hang guard only: a call that waits on a socket the script never answers fails here.
        let result = tokio::time::timeout(
            Duration::from_secs(120),
            openai_codex_responses::stream(&model, &context, &options, &transport, sink),
        )
        .await
        .unwrap_or_else(|_| panic!("{name} call {index} hung"));
        let mut result = normalize_port(&serde_json::to_value(result).unwrap(), port);
        pin(&mut result, &now);
        let mut expected_result = call["result"].clone();
        pin(&mut expected_result, &now);
        same(&what("result"), &expected_result, &result, &mut diffs);
        let events: Vec<Value> = events
            .into_iter()
            .map(|mut e| {
                pin_event(&mut e, &now);
                normalize_port(&e, port)
            })
            .collect();
        let expected_events: Vec<Value> = call["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                let mut e = e.clone();
                pin_event(&mut e, &now);
                e
            })
            .collect();
        same(&what("events"), &json!(expected_events), &json!(events), &mut diffs);
        let user_agent = pi_user_agent();
        let requests: Vec<Value> = transport
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| {
                let headers: Map<String, Value> = r
                    .headers
                    .iter()
                    .map(|(k, v)| (k.clone(), json!(v.replace(&user_agent, "<pi-user-agent>"))))
                    .collect();
                normalize_port(
                    &json!({"url": r.url, "method": r.method, "headers": headers, "body": serde_json::from_str::<Value>(&r.body).unwrap()}),
                    port,
                )
            })
            .collect();
        same(
            &what("SSE requests"),
            &call["sseRequests"],
            &json!(requests),
            &mut diffs,
        );
    }
    for (session, expected) in scenario["stats"].as_object().unwrap() {
        let actual =
            serde_json::to_value(openai_codex_responses::get_openai_codex_websocket_debug_stats(session)).unwrap();
        same(&format!("{name} stats {session}"), expected, &actual, &mut diffs);
    }
    openai_codex_responses::close_openai_codex_websocket_sessions(None);
    let expected = &scenario["connections"];
    let got = normalize_port(&server.log.wait_for(expected), port);
    same(&format!("{name} connections"), expected, &got, &mut diffs);
    openai_codex_responses::reset_openai_codex_websocket_debug_stats(None);
    diffs
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ai_codex_websocket_replays_recorded_typescript_scenarios() {
    let server = FakeCodex::start();
    openai_codex_responses::reset_openai_codex_websocket_debug_stats(None);
    let mut diffs = Vec::new();
    let scenarios = FIXTURE["scenarios"].as_array().unwrap();
    assert_eq!(scenarios.len(), 21);
    // Stop at the first scenario that differs: later ones start from its sockets and stats.
    for scenario in scenarios {
        diffs.extend(replay(scenario, &server).await);
        assert!(diffs.is_empty(), "{} differences:\n{}", diffs.len(), diffs.join("\n"));
    }
}
