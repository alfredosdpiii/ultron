//! The Codex SSE request body goes out zstd-compressed with `Content-Encoding: zstd` (openai-codex-responses.ts
//! `compressRequestBodyZstd` and `sseHeaders.set("content-encoding", "zstd")`; the header is in the recorded
//! TypeScript requests of fixtures/codex-websocket.json, replayed by tests/codex_websocket.rs). Here the real
//! transport sends to a 127.0.0.1 fake that reads the raw body: it is one zstd frame that both ruzstd and the
//! system's libzstd (CPython's `compression.zstd`, via ULTRON_PYTHON) decode to exactly the JSON the provider built.

use ai::http::{Body, BoxFuture, HttpRequest, HttpResponse, Transport};
use ai::openai_codex_responses::{self, OpenAICodexResponsesOptions};
use ai::{AssistantMessageEvent, Model, ReqwestTransport, TranscriptContext};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const SSE: &str = "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\",\"id\":\"msg_9\",\"role\":\"assistant\",\"content\":[]}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"via sse\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_sse\",\"object\":\"response\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":20,\"output_tokens\":5,\"total_tokens\":25}}}\n\n";

/// One request as it arrived: lower-cased headers and the raw body.
type Seen = Arc<Mutex<Option<(Vec<(String, String)>, Vec<u8>)>>>;

fn fake_codex() -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let record = seen.clone();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else { return };
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            if header.trim().is_empty() {
                break;
            }
            let (name, value) = header.split_once(':').unwrap();
            headers.push((name.trim().to_lowercase(), value.trim().to_string()));
        }
        let length: usize = headers
            .iter()
            .find(|(k, _)| k == "content-length")
            .map(|(_, v)| v.parse().unwrap())
            .expect("a content-length");
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        *record.lock().unwrap() = Some((headers, body));
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            SSE.len()
        );
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(SSE.as_bytes());
    });
    (port, seen)
}

/// Records the request (as the provider built it) and answers the same stream.
#[derive(Default)]
struct Recorder(Mutex<Vec<HttpRequest>>);

struct Once(Option<Vec<u8>>);

impl Body for Once {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.take();
        Box::pin(async move { Ok(next) })
    }
}

impl Transport for Recorder {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.0.lock().unwrap().push(request);
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: [("content-type".to_string(), "text/event-stream".to_string())].into(),
                body: Box::new(Once(Some(SSE.as_bytes().to_vec()))),
            })
        })
    }
}

fn model(port: u16) -> Model {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/codex-websocket.json")).unwrap();
    let mut model = fixture["model"].clone();
    model["baseUrl"] = json!(model["baseUrl"].as_str().unwrap().replace("<port>", &port.to_string()));
    serde_json::from_value(model).unwrap()
}

fn options() -> OpenAICodexResponsesOptions {
    serde_json::from_value(json!({
        "apiKey": "eyJhbGciOiJub25lIn0.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjLTEyMyJ9fQ.example-sig",
        "sessionId": "s-zstd",
        "transport": "sse",
        "maxRetries": 0,
    }))
    .unwrap()
}

fn context() -> TranscriptContext {
    serde_json::from_value(json!({"messages": [
        {"role": "system", "content": "You are terse.", "timestamp": 1},
        {"role": "user", "content": "hi ".repeat(200), "timestamp": 2},
    ]}))
    .unwrap()
}

/// `compression.zstd.decompress` of the system's libzstd.
fn libzstd_decode(bytes: &[u8]) -> Vec<u8> {
    let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    let mut child = std::process::Command::new(python)
        .args([
            "-I",
            "-c",
            "import sys, compression.zstd as z; sys.stdout.buffer.write(z.decompress(sys.stdin.buffer.read()))",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    output.stdout
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ai_codex_sse_body_goes_out_zstd_compressed_like_typescript() {
    // The JSON the provider builds (what a recording transport sees).
    let recorder = Recorder::default();
    let mut sink = |_: &AssistantMessageEvent| {};
    let built = openai_codex_responses::stream(&model(1), &context(), &options(), &recorder, &mut sink).await;
    assert!(built.error_message.is_none(), "{:?}", built.error_message);
    let recorded = recorder.0.lock().unwrap()[0].clone();
    assert_eq!(
        recorded.headers.get("content-encoding").map(String::as_str),
        Some("zstd")
    );

    let (port, seen) = fake_codex();
    let transport = ReqwestTransport::new(None);
    let message = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        openai_codex_responses::stream(&model(port), &context(), &options(), &transport, &mut sink),
    )
    .await
    .expect("hang guard");
    assert!(message.error_message.is_none(), "{:?}", message.error_message);
    let (headers, body) = seen.lock().unwrap().clone().expect("the fake received the request");
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    assert_eq!(header("content-encoding"), Some("zstd"));
    assert_eq!(header("content-type"), Some("application/json"));
    assert_eq!(&body[..4], &[0x28, 0xb5, 0x2f, 0xfd], "a zstd frame");
    let mut decoded = Vec::new();
    ruzstd::decoding::StreamingDecoder::new(body.as_slice())
        .unwrap()
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(libzstd_decode(&body), decoded);
    let sent: Value = serde_json::from_slice(&decoded).unwrap();
    let mut expected: Value = serde_json::from_str(&recorded.body).unwrap();
    // The prompt cache key and session headers carry the same session either way; nothing else differs.
    expected["prompt_cache_key"] = sent["prompt_cache_key"].clone();
    assert_eq!(sent, expected);
    assert!(
        body.len() < recorded.body.len(),
        "{} bytes for {}",
        body.len(),
        recorded.body.len()
    );
}

/// A request that names no encoding goes out as its JSON; `zstd` (any case) is encoded.
#[test]
fn ai_wire_body_encodes_only_a_zstd_request() {
    let mut request = HttpRequest {
        method: "POST".into(),
        url: "http://127.0.0.1:1/".into(),
        headers: Default::default(),
        body: "{\"a\":1}".into(),
    };
    assert_eq!(ai::http::wire_body(&request), b"{\"a\":1}");
    request.headers.insert("content-encoding".into(), "ZSTD".into());
    let wire = ai::http::wire_body(&request);
    let mut decoded = Vec::new();
    ruzstd::decoding::StreamingDecoder::new(wire.as_slice())
        .unwrap()
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(decoded, b"{\"a\":1}");
}
