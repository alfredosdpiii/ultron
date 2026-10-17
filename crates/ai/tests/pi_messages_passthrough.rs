//! pi-messages events and stop reasons a newer backend may send (api/pi-messages.ts `createEventConverter`): an
//! unknown event type is passed on upstream (`{ ...event, partial }`) and changes nothing; an unknown `reason`
//! becomes the message's stopReason there. Here the stream carries on past the unknown event, and an unknown reason
//! keeps the outcome (a `done` stops, an `error` errs) instead of failing the stream: see decisions.tsv.

use ai::http::{Body, BoxFuture, HeaderMap, HttpRequest, HttpResponse, Transport};
use ai::{AssistantContent, Model, StopReason, TranscriptContext};
use serde_json::json;

struct OnceBody(Option<Vec<u8>>);

impl Body for OnceBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

struct Canned(String);

impl Transport for Canned {
    fn send(&self, _request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        let body = self.0.clone().into_bytes();
        Box::pin(async move {
            let mut headers = HeaderMap::new();
            headers.insert("content-type".into(), "text/event-stream".into());
            Ok(HttpResponse {
                status: 200,
                headers,
                body: Box::new(OnceBody(Some(body))),
            })
        })
    }
}

fn run(events: &[serde_json::Value]) -> ai::AssistantMessage {
    let model: Model = serde_json::from_value(json!({"id": "deep", "name": "Deep", "api": "pi-messages", "provider": "gw", "baseUrl": "http://127.0.0.1:9/gw", "reasoning": false, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 1000, "maxTokens": 100})).unwrap();
    let context: TranscriptContext =
        serde_json::from_value(json!({"messages": [{"role": "user", "content": "hi", "timestamp": 1}]})).unwrap();
    let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    let mut options = ai::SimpleStreamOptions::default();
    options.base.api_key = Some("k".into());
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(ai::pi_messages::stream_simple(
        &model,
        &context,
        &options,
        &Canned(body),
        &mut sink,
    ))
}

const USAGE: &str = r#"{"input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 2, "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}}"#;

#[test]
fn ai_pi_messages_unknown_event_and_reason_finish_the_stream() {
    let usage: serde_json::Value = serde_json::from_str(USAGE).unwrap();
    let message = run(&[
        json!({"type": "start"}),
        json!({"type": "text_start", "contentIndex": 0}),
        json!({"type": "reasoning_summary_v9", "contentIndex": 0, "summary": "x"}),
        json!({"type": "text_delta", "contentIndex": 0, "delta": "hello"}),
        json!({"type": "text_end", "contentIndex": 0, "content": "hello"}),
        json!({"type": "done", "reason": "refusal", "usage": usage, "responseId": "r1"}),
    ]);
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(message.error_message, None);
    assert_eq!(message.response_id.as_deref(), Some("r1"));
    assert!(matches!(&message.content[..], [AssistantContent::Text(t)] if t.text == "hello"));
}

#[test]
fn ai_pi_messages_error_with_an_unknown_reason_is_an_error() {
    let usage: serde_json::Value = serde_json::from_str(USAGE).unwrap();
    let message = run(&[
        json!({"type": "start"}),
        json!({"type": "error", "reason": "quota_exhausted", "usage": usage, "errorMessage": "out of quota"}),
    ]);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("out of quota"));
}
