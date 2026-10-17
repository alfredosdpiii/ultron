//! A session's close closes the provider connections cached under its provider session IDs, as AgentSession.dispose
//! calls `cleanupSessionResources(sessionId)`, which runs `closeOpenAICodexWebSocketSessions(sessionId)` (the Codex
//! WebSocket cache is process-wide, so without it a closed session's sockets stay open). The cache is filled through
//! the Codex provider over an in-memory socket under the session's root provider session ID (read from its journal)
//! and under an unrelated ID that must stay cached.

mod p_session_support;

use ai::http::{BoxFuture, HeaderMap};
use ai::openai_codex_responses::{self, OpenAICodexResponsesOptions};
use ai::websocket::{WebSocketConnection, WsEvent};
use ai::{AssistantMessageEvent, HttpRequest, HttpResponse, Model, TranscriptContext, Transport};
use p_session_support::{Setup, actor, in_order, text};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

type Log = Arc<Mutex<Vec<String>>>;

/// One in-memory socket: answers `response.create` with a completed response; logs closes and its drop.
struct Memory {
    log: Log,
    queued: VecDeque<WsEvent>,
}

impl Drop for Memory {
    fn drop(&mut self) {
        self.log.lock().unwrap().push("dropped".into());
    }
}

impl WebSocketConnection for Memory {
    fn send_text<'a>(&'a mut self, _text: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            for event in [
                json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message", "id": "m", "role": "assistant", "content": []}}),
                json!({"type": "response.output_text.delta", "output_index": 0, "delta": "ok"}),
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message", "id": "m", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": "ok", "annotations": []}]}}),
                json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "output": []}}),
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
        Box::pin(async move { self.log.lock().unwrap().push(format!("close {code} {reason}")) })
    }

    fn is_open(&mut self) -> BoxFuture<'_, bool> {
        Box::pin(async { true })
    }
}

struct Sockets(Log);

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
        let log = self.0.clone();
        Box::pin(async move {
            let socket: Box<dyn WebSocketConnection> = Box::new(Memory {
                log,
                queued: VecDeque::new(),
            });
            Ok(socket)
        })
    }
}

/// One Codex request over a new in-memory socket, cached under `session`; the socket's log.
fn cache_socket(session: &str) -> Log {
    let log: Log = Arc::default();
    let transport = Sockets(log.clone());
    let model: Model = serde_json::from_value(json!({
        "id": "gpt-5.5", "name": "m", "api": "openai-codex-responses", "provider": "openai-codex",
        "baseUrl": "http://127.0.0.1:9/backend-api", "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 1000, "maxTokens": 100,
    }))
    .unwrap();
    let b64 = |v: Value| ai::oauth_flow::base64url(v.to_string().as_bytes());
    let token = format!(
        "{}.{}.sig",
        b64(json!({"alg": "none"})),
        b64(json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acc"}}))
    );
    let options: OpenAICodexResponsesOptions =
        serde_json::from_value(json!({"apiKey": token, "sessionId": session, "transport": "websocket"})).unwrap();
    let context: TranscriptContext =
        serde_json::from_value(json!({"messages": [{"role": "user", "content": "hi", "timestamp": 1}]})).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let message = runtime.block_on(async {
        let mut sink = |_: &AssistantMessageEvent| {};
        let sink: &mut ai::types::EventSink<'_> = &mut sink;
        openai_codex_responses::stream(&model, &context, &options, &transport, sink).await
    });
    assert_eq!(serde_json::to_value(message).unwrap()["stopReason"], "stop");
    log
}

/// The provider session IDs (`pi.provider`) the session's journal created.
fn provider_session_ids(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if map.get("record").and_then(|r| r.get("kind")) == Some(&json!("pi.provider"))
                && let Some(id) = map["content"]["value"]["sessionId"].as_str()
            {
                found.push(id.to_string());
            }
            map.values().for_each(|v| provider_session_ids(v, found));
        }
        Value::Array(items) => items.iter().for_each(|v| provider_session_ids(v, found)),
        _ => {}
    }
}

#[test]
fn session_core_close_closes_the_codex_sockets_of_its_provider_sessions() {
    let a = actor(in_order(vec![text("hi")]), Setup::default());
    a.prompt("go");
    let mut ids = Vec::new();
    for write in a.writes() {
        provider_session_ids(&write, &mut ids);
    }
    assert_eq!(ids.len(), 1, "{ids:?}");
    let ours = cache_socket(&ids[0]);
    let other_id = format!("{}-unrelated", ids[0]);
    let other = cache_socket(&other_id);
    // Cached: neither closed nor dropped.
    assert_eq!(*ours.lock().unwrap(), Vec::<String>::new());
    drop(a);
    // The session's socket left the cache and was closed (dropped: the actor thread has no async runtime).
    assert_eq!(*ours.lock().unwrap(), ["dropped"]);
    assert_eq!(*other.lock().unwrap(), Vec::<String>::new());
    openai_codex_responses::close_openai_codex_websocket_sessions(Some(&other_id));
    assert_eq!(*other.lock().unwrap(), ["dropped"]);
}
