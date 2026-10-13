//! The provider HTTP idle timeout (`httpIdleTimeoutMs`, core/http-dispatcher.ts): undici's `headersTimeout` and
//! `bodyTimeout` both take it, so a server that stalls before its headers or between two body chunks fails the
//! request; 0 (`"disabled"`) waits forever; the default is five minutes. Literal cases of `parseHttpIdleTimeoutMs`.

use ai::http::{DEFAULT_HTTP_IDLE_TIMEOUT, HttpRequest, Transport, http_idle_timeout, parse_http_idle_timeout_ms};
use ai::{AssistantMessageEvent, Model, ReqwestTransport, StopReason, TranscriptContext};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

static PROVIDERS: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/providers.json")).expect("providers parse"));

#[test]
fn ai_http_idle_timeout_setting_parses_like_typescript() {
    let cases = [
        (json!(30_000), Some(30_000)),
        (json!(1.9), Some(1)),
        (json!(0), Some(0)),
        (json!(-1), None),
        (json!(" 1500 "), Some(1500)),
        (json!("Disabled"), Some(0)),
        (json!(""), None),
        (json!("soon"), None),
        (json!(true), None),
        (json!(null), None),
    ];
    for (value, expected) in cases {
        assert_eq!(parse_http_idle_timeout_ms(&value), expected, "{value}");
    }
    assert_eq!(DEFAULT_HTTP_IDLE_TIMEOUT, Duration::from_millis(300_000));
    assert_eq!(http_idle_timeout(None), Some(DEFAULT_HTTP_IDLE_TIMEOUT));
    assert_eq!(http_idle_timeout(Some(&json!("x"))), Some(DEFAULT_HTTP_IDLE_TIMEOUT));
    assert_eq!(http_idle_timeout(Some(&json!(0))), None);
    assert_eq!(http_idle_timeout(Some(&json!(250))), Some(Duration::from_millis(250)));
}

/// A server that answers one request with `head` (status line and headers, or nothing), then `chunks` (each
/// flushed after its delay), then holds the connection open for far longer than any test runs.
fn stalling_server(head: Option<&'static str>, chunks: Vec<(Duration, &'static str)>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 8192];
        let _ = socket.read(&mut buffer);
        if let Some(head) = head {
            socket.write_all(head.as_bytes()).unwrap();
            for (delay, chunk) in chunks {
                std::thread::sleep(delay);
                let _ = socket.write_all(format!("{:x}\r\n{chunk}\r\n", chunk.len()).as_bytes());
            }
        }
        std::thread::sleep(Duration::from_secs(600));
    });
    address
}

const CHUNKED: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";

fn transport(idle: Option<Duration>) -> ReqwestTransport {
    ReqwestTransport::with_client(reqwest::Client::builder().no_proxy().build().unwrap(), None).with_idle_timeout(idle)
}

fn request(address: SocketAddr) -> HttpRequest {
    HttpRequest {
        method: "POST".into(),
        url: format!("http://{address}/"),
        headers: Default::default(),
        body: "{}".into(),
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn ai_http_idle_timeout_fails_a_stalled_body_but_not_a_slow_steady_one() {
    let idle = Duration::from_secs(1);
    let gap = Duration::from_millis(600);
    let runtime = runtime();
    let transport = transport(Some(idle));
    // Chunks arrive every 600 ms (under the idle timeout, 1.2 s in all, so the timer must reset per chunk), then
    // the body stalls. Under load the server thread can be starved past the idle timeout between two chunks; the
    // timeout is then right to fire, so that attempt only checks the stall really was that long, and the test
    // tries again until the server kept its pace.
    for _attempt in 0..50 {
        let address = stalling_server(Some(CHUNKED), vec![(Duration::ZERO, "a"), (gap, "b"), (gap, "c")]);
        let (chunks, error, stalled_for) = runtime.block_on(async {
            let mut response = transport.send(request(address)).await.unwrap();
            let mut chunks = Vec::new();
            loop {
                let started = Instant::now();
                match response.body.next_chunk().await {
                    Ok(Some(chunk)) => chunks.push(String::from_utf8(chunk).unwrap()),
                    Ok(None) => return (chunks, None, started.elapsed()),
                    Err(error) => return (chunks, Some(error), started.elapsed()),
                }
            }
        });
        assert_eq!(error.as_deref(), Some("terminated"));
        assert!(stalled_for >= idle, "{stalled_for:?}");
        if chunks.concat() == "abc" {
            return;
        }
        assert!(matches!(chunks.concat().as_str(), "" | "a" | "ab"), "{chunks:?}");
    }
    panic!("the server never kept a 600 ms pace in 50 attempts");
}

#[test]
fn ai_http_idle_timeout_fails_missing_headers_and_zero_disables_it() {
    let runtime = runtime();
    let address = stalling_server(None, Vec::new());
    let started = Instant::now();
    let sent = runtime.block_on(transport(Some(Duration::from_millis(200))).send(request(address)));
    assert_eq!(sent.err().as_deref(), Some("Headers Timeout Error"));
    // The 200 ms setting fired, not the five-minute default (a hang guard, loose for load).
    assert!(started.elapsed() < Duration::from_secs(120));
    // Disabled: the request is still waiting well past the timeout it would have had.
    let address = stalling_server(None, Vec::new());
    let waited = runtime.block_on(async {
        tokio::time::timeout(Duration::from_millis(600), transport(None).send(request(address))).await
    });
    assert!(waited.is_err(), "no idle timeout without the setting");
}

#[test]
fn ai_http_idle_timeout_ends_a_stalled_stream_as_a_retryable_error() {
    let address = stalling_server(
        Some(CHUNKED),
        vec![(
            Duration::ZERO,
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"test-model\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
        )],
    );
    let mut model: Model = serde_json::from_value(PROVIDERS["scenarios"][0]["model"].clone()).unwrap();
    model.base_url = format!("http://{address}");
    let context = TranscriptContext {
        messages: serde_json::from_value(json!([{"role": "user", "content": "hi", "timestamp": 1}])).unwrap(),
    };
    let options = ai::anthropic::AnthropicOptions {
        base: ai::StreamOptions {
            api_key: Some("sk-local".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let transport = transport(Some(Duration::from_millis(300)));
    let message = runtime().block_on(ai::anthropic::stream(
        &model,
        &context,
        &options,
        &transport,
        &mut |_: &AssistantMessageEvent| {},
    ));
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(message.error_message.as_deref(), Some("terminated"));
}
