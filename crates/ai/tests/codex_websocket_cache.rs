//! The Codex WebSocket session cache's idle timer (`scheduleSessionWebSocketExpiry`: a cached socket nobody used for
//! `SESSION_WEBSOCKET_CACHE_TTL_MS`, five minutes, is closed with `1000 idle_timeout` and forgotten; using it before
//! then clears the timer) on tokio's paused clock, over in-memory sockets. Expected values are upstream's constants
//! and close reasons, literally.

use ai::http::{BoxFuture, HeaderMap};
use ai::openai_codex_responses::{self, OpenAICodexResponsesOptions};
use ai::websocket::{WebSocketConnection, WsEvent};
use ai::{AssistantMessageEvent, HttpRequest, HttpResponse, Model, TranscriptContext, Transport};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One in-memory socket: answers each `response.create` with a completed response, and logs what it was told.
struct Memory {
    log: Arc<Mutex<Vec<String>>>,
    queued: VecDeque<WsEvent>,
    open: bool,
    responses: u32,
}

impl WebSocketConnection for Memory {
    fn send_text<'a>(&'a mut self, text: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let message: Value = serde_json::from_str(text).unwrap();
            self.log
                .lock()
                .unwrap()
                .push(format!("send {}", message["type"].as_str().unwrap()));
            self.responses += 1;
            let id = format!("resp_{}", self.responses);
            for event in [
                json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "m", "role": "assistant", "content": []}}),
                json!({"type": "response.output_text.delta", "output_index": 0, "delta": "ok"}),
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "id": "m", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "ok", "annotations": []}]}}),
                json!({"type": "response.completed", "response": {"id": id, "status": "completed", "output": []}}),
            ] {
                self.queued.push_back(WsEvent::Text(event.to_string()));
            }
        })
    }

    fn next_event(&mut self) -> BoxFuture<'_, WsEvent> {
        Box::pin(async move {
            self.queued.pop_front().unwrap_or(WsEvent::Closed {
                code: 1006,
                reason: String::new(),
                was_clean: false,
            })
        })
    }

    fn close<'a>(&'a mut self, code: u16, reason: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if self.open {
                self.log.lock().unwrap().push(format!("close {code} {reason}"));
            }
            self.open = false;
        })
    }

    fn is_open(&mut self) -> BoxFuture<'_, bool> {
        Box::pin(async move { self.open })
    }
}

#[derive(Default)]
struct Sockets {
    /// One log per socket opened.
    logs: Mutex<Vec<Arc<Mutex<Vec<String>>>>>,
}

impl Transport for Sockets {
    fn send(&self, _request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async { Err("no SSE in this test".to_string()) })
    }

    fn websocket_available(&self) -> bool {
        true
    }

    fn websocket(
        &self,
        _url: String,
        _headers: HeaderMap,
    ) -> BoxFuture<'_, Result<Box<dyn WebSocketConnection>, String>> {
        let log = Arc::new(Mutex::new(Vec::new()));
        self.logs.lock().unwrap().push(log.clone());
        Box::pin(async move {
            let socket: Box<dyn WebSocketConnection> = Box::new(Memory {
                log,
                queued: VecDeque::new(),
                open: true,
                responses: 0,
            });
            Ok(socket)
        })
    }
}

impl Sockets {
    fn log(&self, index: usize) -> Vec<String> {
        self.logs.lock().unwrap()[index].lock().unwrap().clone()
    }

    fn count(&self) -> usize {
        self.logs.lock().unwrap().len()
    }
}

fn model() -> Model {
    serde_json::from_value(json!({
        "id": "gpt-5.5", "name": "m", "api": "openai-codex-responses", "provider": "openai-codex",
        "baseUrl": "http://127.0.0.1:9/backend-api", "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 1000, "maxTokens": 100,
    }))
    .unwrap()
}

async fn call(transport: &Sockets, session: &str) -> Value {
    let b64 = |v: Value| ai::oauth_flow::base64url(v.to_string().as_bytes());
    let token = format!(
        "{}.{}.example-sig",
        b64(json!({"alg": "none"})),
        b64(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acc"}}))
    );
    let options: OpenAICodexResponsesOptions =
        serde_json::from_value(json!({"apiKey": token, "sessionId": session, "transport": "websocket"})).unwrap();
    let context: TranscriptContext =
        serde_json::from_value(json!({"messages": [{"role": "user", "content": "hi", "timestamp": 1}]})).unwrap();
    let mut sink = |_: &AssistantMessageEvent| {};
    let sink: &mut ai::types::EventSink<'_> = &mut sink;
    let message = openai_codex_responses::stream(&model(), &context, &options, transport, sink).await;
    serde_json::to_value(message).unwrap()
}

/// Let the spawned idle timers that are due run.
async fn settle() {
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn ai_codex_websocket_idle_socket_closes_after_five_minutes() {
    let transport = Sockets::default();
    assert_eq!(call(&transport, "ttl-expire").await["stopReason"], "stop");
    assert_eq!(transport.log(0), ["send response.create"]);
    tokio::time::advance(Duration::from_millis(5 * 60 * 1000 - 1)).await;
    settle().await;
    assert_eq!(
        transport.log(0),
        ["send response.create"],
        "still cached a moment before"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    settle().await;
    assert_eq!(transport.log(0), ["send response.create", "close 1000 idle_timeout"]);
    // The next request opens a new socket.
    call(&transport, "ttl-expire").await;
    assert_eq!(transport.count(), 2);
    let stats = openai_codex_responses::get_openai_codex_websocket_debug_stats("ttl-expire").unwrap();
    assert_eq!((stats.connections_created, stats.connections_reused), (2, 0));
}

#[tokio::test(start_paused = true)]
async fn ai_codex_websocket_use_restarts_the_idle_timer() {
    let transport = Sockets::default();
    call(&transport, "ttl-reuse").await;
    tokio::time::advance(Duration::from_secs(4 * 60)).await;
    settle().await;
    call(&transport, "ttl-reuse").await;
    // The first timer would have fired here; it was cleared by the reuse.
    tokio::time::advance(Duration::from_secs(2 * 60)).await;
    settle().await;
    assert_eq!(transport.log(0), ["send response.create", "send response.create"]);
    assert_eq!(transport.count(), 1);
    // Five minutes after the last use it goes.
    tokio::time::advance(Duration::from_secs(3 * 60)).await;
    settle().await;
    assert_eq!(
        transport.log(0),
        [
            "send response.create",
            "send response.create",
            "close 1000 idle_timeout"
        ]
    );
    let stats = openai_codex_responses::get_openai_codex_websocket_debug_stats("ttl-reuse").unwrap();
    assert_eq!((stats.connections_created, stats.connections_reused), (1, 1));
}

#[tokio::test(start_paused = true)]
async fn ai_codex_websocket_closing_sessions_closes_idle_sockets() {
    let transport = Sockets::default();
    call(&transport, "close-one").await;
    call(&transport, "close-other").await;
    openai_codex_responses::close_openai_codex_websocket_sessions(Some("close-one"));
    settle().await;
    assert_eq!(transport.log(0), ["send response.create", "close 1000 debug_close"]);
    assert_eq!(transport.log(1), ["send response.create"]);
    // The closed session opens a new socket; the other one is reused.
    call(&transport, "close-one").await;
    call(&transport, "close-other").await;
    assert_eq!(transport.count(), 3);
    assert_eq!(transport.log(1), ["send response.create", "send response.create"]);
}
