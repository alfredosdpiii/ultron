//! Shared by the tests: a scripted chat-completions transport and the fixture's fake model.
#![allow(dead_code)]

use ai::{Body, HttpRequest, HttpResponse, Model, Transport};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

struct Chunks(std::vec::IntoIter<Vec<u8>>);

impl Body for Chunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

/// Answers each chat-completions request with the next scripted reply as an SSE stream; records the bodies.
#[derive(Default)]
pub struct Scripted {
    pub replies: Mutex<Vec<Value>>,
    pub bodies: Mutex<Vec<Value>>,
}

impl Transport for Scripted {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.bodies
            .lock()
            .unwrap()
            .push(serde_json::from_str(&request.body).unwrap());
        let reply = {
            let mut replies = self.replies.lock().unwrap();
            (!replies.is_empty()).then(|| replies.remove(0))
        };
        // A provider slow to answer: blocks the sending thread, so no timer can cut the request short.
        if let Some(ms) = reply.as_ref().and_then(|r| r["block_ms"].as_u64()) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        Box::pin(async move {
            let reply = reply.ok_or("unexpected request")?;
            if let Some(count) = reply["gather"].as_u64() {
                // Concurrent frames: answer only once `count` requests are in flight (or after 2 s, so a host that
                // runs them one at a time fails the comparison instead of hanging).
                let since = std::time::Instant::now();
                while (self.bodies.lock().unwrap().len() as u64) < count && since.elapsed().as_secs() < 2 {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            }
            if reply.get("hang").is_some() {
                // A provider that never answers: only the frame's timeout ends the request.
                std::future::pending::<()>().await;
            }
            if let Some(status) = reply["status"].as_u64() {
                let body = br#"{"error":{"message":"scripted failure"}}"#.to_vec();
                let headers = [("content-type".to_string(), "application/json".to_string())].into();
                return Ok(HttpResponse {
                    status: status as u16,
                    headers,
                    body: Box::new(Chunks(vec![body].into_iter())),
                });
            }
            let usage = &reply["usage"];
            let (input, output, cached) = (
                usage["input"].as_u64().unwrap(),
                usage["output"].as_u64().unwrap(),
                usage["cacheRead"].as_u64().unwrap(),
            );
            let events = [
                json!({"id": "c", "object": "chat.completion.chunk", "model": "frame-model", "choices": [{"index": 0, "delta": {"role": "assistant", "content": reply["text"]}, "finish_reason": null}]}),
                json!({"id": "c", "object": "chat.completion.chunk", "model": "frame-model", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": input + cached, "completion_tokens": output, "prompt_tokens_details": {"cached_tokens": cached}}}),
            ];
            let mut chunks: Vec<Vec<u8>> = events.iter().map(|e| format!("data: {e}\n\n").into_bytes()).collect();
            chunks.push(b"data: [DONE]\n\n".to_vec());
            let headers = [("content-type".to_string(), "text/event-stream".to_string())].into();
            Ok(HttpResponse {
                status: 200,
                headers,
                body: Box::new(Chunks(chunks.into_iter())),
            })
        })
    }
}

pub fn model(recorded: &Value) -> Model {
    serde_json::from_value(json!({
        "id": recorded["id"], "name": "Frame model", "api": recorded["api"], "provider": recorded["provider"],
        "baseUrl": "http://fake.invalid/v1", "reasoning": false, "input": ["text"],
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": recorded["contextWindow"], "maxTokens": recorded["maxTokens"],
    }))
    .unwrap()
}
