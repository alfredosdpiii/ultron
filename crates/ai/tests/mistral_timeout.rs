//! The Mistral request's whole-request limit (api/mistral-conversations.ts `AbortSignal.timeout(timeoutMs ?? 60000)`
//! over the fetch and the stream), replayed from `fixtures/mistral-timeout.json` (`generate-mistral-timeout.mjs`):
//! a server that never answers, or answers and then stalls, ends the message with TypeScript's timeout error (its
//! text kept); one that answers in time is unaffected. The runtime's clock is paused, so the limits pass at once.

use ai::http::{Body, BoxFuture, HeaderMap, HttpRequest, HttpResponse, Transport};
use ai::mistral::{self, MistralOptions};
use ai::{AssistantMessageEvent, Model, TranscriptContext};
use serde_json::{Value, json};

const FIXTURE: &str = include_str!("../fixtures/mistral-timeout.json");

/// The chunks of a stream, then its end, or (`stall`) nothing more, ever.
struct Chunks {
    chunks: Vec<Vec<u8>>,
    stall: bool,
}

impl Body for Chunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        if !self.chunks.is_empty() {
            let next = self.chunks.remove(0);
            return Box::pin(async move { Ok(Some(next)) });
        }
        let stall = self.stall;
        Box::pin(async move {
            if stall {
                std::future::pending::<()>().await;
            }
            Ok(None)
        })
    }
}

struct Scripted {
    script: String,
    chunks: Vec<String>,
}

impl Transport for Scripted {
    fn send(&self, _request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async move {
            if self.script == "hang" {
                std::future::pending::<()>().await;
            }
            let mut headers = HeaderMap::new();
            headers.insert("content-type".into(), "text/event-stream".into());
            Ok(HttpResponse {
                status: 200,
                headers,
                body: Box::new(Chunks {
                    chunks: self.chunks.iter().map(|c| c.clone().into_bytes()).collect(),
                    stall: self.script == "stall",
                }),
            })
        })
    }
}

async fn run(model: &Model, script: &str, chunks: Vec<String>, timeout_ms: Option<u64>) -> ai::AssistantMessage {
    let context: TranscriptContext =
        serde_json::from_value(json!({"messages": [{"role": "user", "content": "hi", "timestamp": 2}]})).unwrap();
    let mut options = json!({"apiKey": "test-key"});
    if let Some(ms) = timeout_ms {
        options["timeoutMs"] = json!(ms);
    }
    let options: MistralOptions = serde_json::from_value(options).unwrap();
    let transport = Scripted {
        script: script.into(),
        chunks,
    };
    let mut sink = |_: &AssistantMessageEvent| {};
    mistral::stream(model, &context, &options, &transport, &mut sink).await
}

#[tokio::test(start_paused = true)]
async fn ai_mistral_whole_request_timeout_matches_typescript() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let model: Model = serde_json::from_value(fixture["model"].clone()).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 3);
    for case in cases {
        let chunks = case["chunks"]
            .as_array()
            .map(|c| c.iter().map(|s| s.as_str().unwrap().to_string()).collect())
            .unwrap_or_default();
        let started = tokio::time::Instant::now();
        let message = run(
            &model,
            case["script"].as_str().unwrap(),
            chunks,
            case["timeoutMs"].as_u64(),
        )
        .await;
        let got = json!({
            "stopReason": serde_json::to_value(message.stop_reason).unwrap(),
            "errorMessage": message.error_message,
            "content": serde_json::to_value(&message.content).unwrap(),
        });
        assert_eq!(got, case["result"], "{}", case["name"]);
        if case["script"] != "ok" {
            assert_eq!(
                started.elapsed().as_millis(),
                u128::from(case["timeoutMs"].as_u64().unwrap()),
                "{}",
                case["name"]
            );
        }
    }
}

/// Without `timeoutMs` the limit is 60 s (`timeoutMs ?? 60_000`).
#[tokio::test(start_paused = true)]
async fn ai_mistral_request_limit_defaults_to_sixty_seconds() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let model: Model = serde_json::from_value(fixture["model"].clone()).unwrap();
    let started = tokio::time::Instant::now();
    let message = run(&model, "hang", Vec::new(), None).await;
    assert_eq!(started.elapsed(), std::time::Duration::from_secs(60));
    assert_eq!(
        message.error_message.as_deref(),
        Some("The operation was aborted due to timeout")
    );
}
