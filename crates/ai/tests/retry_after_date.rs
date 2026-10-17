//! `retry-after` as an HTTP date (`utils/provider-retry.ts` `getRetryDelayMs`: `Date.parse(retryAfter) - Date.now()`,
//! checked against `maxRetryDelayMs`; `openai-codex-responses.ts` `getRetryAfterDelayMs`: the same, at least 0).
//! Ported from `packages/ai/test/openai-codex-stream.test.ts` "uses retry-after HTTP date for SSE retries" (45 s) and
//! `provider-retry.test.ts` "rejects a provider-requested retry delay above the limit". The runtime's clock is
//! paused, so a retry sleep advances it by exactly the delay; the date is a whole second, so the delay is just under
//! the named seconds.

use ai::http::{BoxFuture, HeaderMap};
use ai::openai_codex_responses::{self, OpenAICodexResponsesOptions};
use ai::openai_completions::{self, OpenAICompletionsOptions};
use ai::{AssistantMessage, Body, HttpRequest, HttpResponse, Model, StopReason, TranscriptContext, Transport};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

struct Chunks(std::vec::IntoIter<Vec<u8>>);

impl Body for Chunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

/// Answers the first request with 429 and `retry-after: <date>`, every later one with `ok`; keeps when each request
/// arrived on the runtime's (paused) clock.
#[derive(Clone)]
struct RateLimited {
    retry_after: String,
    ok: Vec<u8>,
    seen: Arc<Mutex<Vec<tokio::time::Instant>>>,
}

impl Transport for RateLimited {
    fn send(&self, _request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        let first = {
            let mut seen = self.seen.lock().unwrap();
            seen.push(tokio::time::Instant::now());
            seen.len() == 1
        };
        let mut headers = HeaderMap::new();
        let (status, body) = if first {
            headers.insert("content-type".into(), "application/json".into());
            headers.insert("retry-after".into(), self.retry_after.clone());
            (
                429,
                json!({"error": {"code": "rate_limit_exceeded", "message": "rate limited"}})
                    .to_string()
                    .into_bytes(),
            )
        } else {
            headers.insert("content-type".into(), "text/event-stream".into());
            (200, self.ok.clone())
        };
        Box::pin(async move {
            Ok(HttpResponse {
                status,
                headers,
                body: Box::new(Chunks(vec![body].into_iter())),
            })
        })
    }
}

/// `new Date(Date.now() + seconds * 1000).toUTCString()`, at a whole second.
fn http_date_in(seconds: i64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let t = now + seconds;
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    // civil_from_days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let weekday = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{weekday}, {day:02} {} {year} {:02}:{:02}:{:02} GMT",
        months[(month - 1) as usize],
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

fn context() -> TranscriptContext {
    serde_json::from_value(json!({"messages": [{"role": "user", "content": "hi", "timestamp": 1}]})).unwrap()
}

fn paused() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap()
}

const CODEX_TOKEN: &str = "eyJhbGciOiJub25lIn0.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjLTEyMyJ9fQ.example-sig";

fn codex_model() -> Model {
    serde_json::from_value(json!({"id": "gpt-5.5", "name": "Test Model", "api": "openai-codex-responses", "provider": "openai-codex", "baseUrl": "https://chatgpt.com/backend-api", "reasoning": true, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 200000, "maxTokens": 32000})).unwrap()
}

const CODEX_OK: &str = "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_3\",\"role\":\"assistant\",\"content\":[]}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"retried\"}\n\ndata: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_3\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"retried\",\"annotations\":[]}]}}\n\ndata: {\"type\":\"response.done\",\"response\":{\"id\":\"resp_1\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":2,\"output_tokens\":1,\"total_tokens\":3}}}\n\n";

fn codex(retry_after: String, options: Value) -> (AssistantMessage, Vec<tokio::time::Instant>) {
    let transport = RateLimited {
        retry_after,
        ok: CODEX_OK.as_bytes().to_vec(),
        seen: Arc::default(),
    };
    let mut merged = json!({"apiKey": CODEX_TOKEN, "transport": "sse", "maxRetries": 1});
    merged
        .as_object_mut()
        .unwrap()
        .extend(options.as_object().unwrap().clone());
    let options: OpenAICodexResponsesOptions = serde_json::from_value(merged).unwrap();
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let message = paused().block_on(openai_codex_responses::stream(
        &codex_model(),
        &context(),
        &options,
        &transport,
        &mut sink,
    ));
    let seen = transport.seen.lock().unwrap().clone();
    (message, seen)
}

/// The Codex SSE retry waits until the `retry-after` date (45 s ahead), then succeeds.
#[test]
fn ai_codex_retry_after_http_date_waits_until_the_date() {
    let (message, seen) = codex(http_date_in(45), json!({}));
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(seen.len(), 2);
    let waited = seen[1] - seen[0];
    assert!(
        waited > Duration::from_secs(43) && waited <= Duration::from_secs(45),
        "{waited:?}"
    );
}

/// A Codex `retry-after` date past `maxRetryDelayMs` (default 60 s) fails at once with the delay it asked for.
#[test]
fn ai_codex_retry_after_http_date_over_the_cap_fails() {
    let (message, seen) = codex(http_date_in(120), json!({}));
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(seen.len(), 1);
    let error = message.error_message.unwrap();
    assert!(
        error.contains("Server requested 120s retry delay (max: 60s)"),
        "{error}"
    );
}

/// A Codex `retry-after` date in the past retries at once.
#[test]
fn ai_codex_retry_after_past_http_date_retries_at_once() {
    let (message, seen) = codex(http_date_in(-30), json!({}));
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(seen[1] - seen[0], Duration::ZERO);
}

fn completions_model() -> Model {
    serde_json::from_value(json!({"id": "m", "name": "M", "api": "openai-completions", "provider": "stub", "baseUrl": "http://127.0.0.1:9/v1", "reasoning": false, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 1000, "maxTokens": 100})).unwrap()
}

const COMPLETIONS_OK: &str = "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

fn completions(retry_after: String, options: Value) -> (AssistantMessage, Vec<tokio::time::Instant>) {
    let transport = RateLimited {
        retry_after,
        ok: COMPLETIONS_OK.as_bytes().to_vec(),
        seen: Arc::default(),
    };
    let mut merged = json!({"apiKey": "sk-fake", "maxRetries": 1});
    merged
        .as_object_mut()
        .unwrap()
        .extend(options.as_object().unwrap().clone());
    let options: OpenAICompletionsOptions = serde_json::from_value(merged).unwrap();
    let mut sink = |_: &ai::AssistantMessageEvent| {};
    let message = paused().block_on(openai_completions::stream(
        &completions_model(),
        &context(),
        &options,
        &transport,
        &mut sink,
    ));
    let seen = transport.seen.lock().unwrap().clone();
    (message, seen)
}

/// `retryProviderRequest` (the OpenAI and Anthropic SDK retry): a `retry-after` date 30 s ahead is waited out.
#[test]
fn ai_provider_retry_after_http_date_waits_until_the_date() {
    let (message, seen) = completions(http_date_in(30), json!({}));
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(seen.len(), 2);
    let waited = seen[1] - seen[0];
    assert!(
        waited > Duration::from_secs(28) && waited <= Duration::from_secs(30),
        "{waited:?}"
    );
}

/// A date beyond `maxRetryDelayMs` fails without retrying; with the cap disabled (0) it is waited out.
#[test]
fn ai_provider_retry_after_http_date_is_checked_against_the_cap() {
    let (message, seen) = completions(http_date_in(90), json!({"maxRetryDelayMs": 5000}));
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(seen.len(), 1);
    let error = message.error_message.unwrap();
    assert!(
        error.starts_with("Server requested 90s retry delay (max: 5s)."),
        "{error}"
    );
    let (message, seen) = completions(http_date_in(90), json!({"maxRetryDelayMs": 0}));
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let waited = seen[1] - seen[0];
    assert!(
        waited > Duration::from_secs(88) && waited <= Duration::from_secs(90),
        "{waited:?}"
    );
}
