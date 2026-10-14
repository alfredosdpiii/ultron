//! Replays `fixtures/stream-errors.json` (recorded from the TypeScript providers against a real local HTTP server by
//! `fixtures/generate-stream-errors.mjs`): the same error responses, served over real HTTP to the Rust providers
//! (reqwest transport), must end the stream with the same `stopReason` and `errorMessage`. Covers Anthropic's SSE
//! `error` event and its HTTP 529 / 429 / 500 bodies, and the stream and HTTP errors of OpenAI completions,
//! OpenAI responses, Google and Mistral.

use ai::anthropic::{self, AnthropicOptions};
use ai::google::{self, GoogleOptions};
use ai::http::ReqwestTransport;
use ai::mistral::{self, MistralOptions};
use ai::openai_completions::{self, OpenAICompletionsOptions};
use ai::openai_responses::{self, OpenAIResponsesOptions};
use ai::{AssistantMessage, AssistantMessageEvent, Model, TranscriptContext};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const FIXTURE: &str = include_str!("../fixtures/stream-errors.json");

/// A local HTTP server answering each request with the next canned response (`{status, headers, body}`).
fn serve(responses: Vec<Value>) -> (String, Arc<Mutex<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let served = Arc::new(Mutex::new(0));
    let count = served.clone();
    std::thread::spawn(move || {
        let mut responses = responses.into_iter();
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = header.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let Some(canned) = responses.next() else { return };
            *count.lock().unwrap() += 1;
            let text = canned["body"].as_str().unwrap();
            let mut head = format!("HTTP/1.1 {} X\r\n", canned["status"]);
            for (name, value) in canned["headers"].as_object().unwrap() {
                head.push_str(&format!("{name}: {}\r\n", value.as_str().unwrap()));
            }
            head.push_str(&format!("content-length: {}\r\nconnection: close\r\n\r\n", text.len()));
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(text.as_bytes());
        }
    });
    (base, served)
}

fn run(scenario: &Value) -> (AssistantMessage, usize) {
    let (base, served) = serve(scenario["responses"].as_array().unwrap().clone());
    let mut model = scenario["model"].clone();
    model["baseUrl"] = json!(model["baseUrl"].as_str().unwrap().replace("{base}", &base));
    let model: Model = serde_json::from_value(model).unwrap();
    let context: TranscriptContext = serde_json::from_value(scenario["context"].clone()).unwrap();
    let options = scenario["options"].clone();
    let transport = ReqwestTransport::new(None);
    let mut sink = |_: &AssistantMessageEvent| {};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let message = runtime.block_on(async {
        match model.api.as_str() {
            "anthropic-messages" => {
                let options: AnthropicOptions = serde_json::from_value(options.clone()).unwrap();
                anthropic::stream(&model, &context, &options, &transport, &mut sink).await
            }
            "openai-completions" => {
                let options: OpenAICompletionsOptions = serde_json::from_value(options.clone()).unwrap();
                openai_completions::stream(&model, &context, &options, &transport, &mut sink).await
            }
            "openai-responses" => {
                let options: OpenAIResponsesOptions = serde_json::from_value(options.clone()).unwrap();
                openai_responses::stream(&model, &context, &options, &transport, &mut sink).await
            }
            "google-generative-ai" => {
                let options: GoogleOptions = serde_json::from_value(options.clone()).unwrap();
                google::stream(&model, &context, &options, &transport, &mut sink).await
            }
            "mistral-conversations" => {
                let options: MistralOptions = serde_json::from_value(options.clone()).unwrap();
                mistral::stream(&model, &context, &options, &transport, &mut sink).await
            }
            api => panic!("unknown api {api}"),
        }
    });
    let count = *served.lock().unwrap();
    (message, count)
}

/// Every recorded error scenario: the Rust provider's final `stopReason` and `errorMessage` equal TypeScript's,
/// after the same number of requests (no retry).
#[test]
fn ai_stream_errors_match_typescript_over_http() {
    let fixture: Value = serde_json::from_str(FIXTURE).unwrap();
    let mut mismatches = Vec::new();
    for scenario in fixture["scenarios"].as_array().unwrap() {
        assert!(scenario["thrown"].is_null(), "{}", scenario["name"]);
        let (message, served) = run(scenario);
        let got = json!({
            "stopReason": serde_json::to_value(message.stop_reason).unwrap(),
            "errorMessage": message.error_message,
        });
        let expected_served =
            scenario["responses"].as_array().unwrap().len() - scenario["unanswered"].as_u64().unwrap() as usize;
        if got != scenario["result"] || served != expected_served {
            mismatches.push(format!(
                "{}: got {got} after {served} requests, TypeScript {} after {expected_served}",
                scenario["name"], scenario["result"]
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
    assert_eq!(fixture["scenarios"].as_array().unwrap().len(), 14);
}
