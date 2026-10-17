//! `timeoutMs` on OpenRouter images requests (api/openrouter-images.ts: the OpenAI SDK request `timeout`), replayed
//! from `crates/ai/fixtures/images-timeout.json` (`generate-images-timeout.mjs`): a server that never answers fails
//! the attempt with "Request timed out.", which `maxRetries` retries like a connection failure. The runtime's clock
//! is paused, so the timeouts and the retry backoff pass at once.

use ai::http::{Body, BoxFuture, HeaderMap, HttpRequest, HttpResponse, Transport};
use ai::images::{ImagesContext, ImagesModel, ImagesOptions, generate_images};
use ai::json::normalize_numbers;
use serde_json::{Value, json};
use std::sync::Mutex;

struct OnceBody(Option<Vec<u8>>);

impl Body for OnceBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

/// Per request: "hang" never answers, "ok" answers at once.
struct Scripted {
    script: Mutex<Vec<String>>,
    requests: Mutex<usize>,
}

impl Transport for Scripted {
    fn send(&self, _request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        *self.requests.lock().unwrap() += 1;
        let step = {
            let mut script = self.script.lock().unwrap();
            if script.is_empty() {
                "hang".to_string()
            } else {
                script.remove(0)
            }
        };
        Box::pin(async move {
            // A server that answers only after an hour (the paused clock gets there at once if nothing times out).
            if step == "hang" {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
            let mut headers = HeaderMap::new();
            headers.insert("content-type".into(), "application/json".into());
            let body = json!({"id": "img-1", "choices": [{"message": {"content": "done"}}]}).to_string();
            Ok(HttpResponse {
                status: 200,
                headers,
                body: Box::new(OnceBody(Some(body.into_bytes()))),
            })
        })
    }
}

#[tokio::test(start_paused = true)]
async fn ai_images_timeout_ms_matches_typescript() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/images-timeout.json")).unwrap();
    let model: ImagesModel = serde_json::from_value(fixture["model"].clone()).unwrap();
    let context: ImagesContext =
        serde_json::from_value(json!({"input": [{"type": "text", "text": "Generate a dog"}]})).unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 4);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let transport = Scripted {
            script: Mutex::new(
                case["script"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s.as_str().unwrap().to_string())
                    .collect(),
            ),
            requests: Mutex::new(0),
        };
        let o = &case["options"];
        let options = ImagesOptions {
            api_key: o["apiKey"].as_str().map(str::to_owned),
            max_retries: o["maxRetries"].as_u64().map(|n| n as u32),
            timeout_ms: o["timeoutMs"].as_u64(),
            now_ms: Some(1767225600000),
            ..Default::default()
        };
        let result = generate_images(&model, &context, &options, &transport).await.unwrap();
        assert_eq!(
            normalize_numbers(serde_json::to_value(result).unwrap()),
            case["result"],
            "{name}"
        );
        assert_eq!(json!(*transport.requests.lock().unwrap()), case["requests"], "{name}");
    }
}
