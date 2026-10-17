//! Streamable HTTP stream resumption (`transports/streamable-http.ts` `consumeResponseStream`, `runGetStream`,
//! `openSseStream`), ported from `packages/mcp/test/streamable-http.test.ts`: a response stream that ends or breaks
//! before its answer is reopened with GET and `Last-Event-ID` (the server's `retry`, else backoff, up to
//! `maxRetries`); the GET notification stream resends the last event id it saw and honours `retry`. A loopback
//! server in the test answers each request as the TypeScript test's server does.

use mcp::http::{HttpOptions, HttpTransport};
use mcp::{Client, ClientOptions, RequestOptions, Transport};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// Only a hang guard: every awaited event is due at once.
const HANG_GUARD: Duration = Duration::from_secs(120);

/// One request the server saw.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    last_event_id: Option<String>,
    message: Value,
}

type Handler = dyn Fn(&Seen, usize) -> String + Send + Sync;

/// A loopback server: `handler` gets each request (and how many came before it) and returns the raw response.
fn server(handler: Arc<Handler>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let shared = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let handler = handler.clone();
            let shared = shared.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                let method = line.split_whitespace().next().unwrap_or_default().to_string();
                let (mut length, mut last_event_id) = (0, None);
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = header.split_once(':') {
                        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim().to_string());
                        if k == "content-length" {
                            length = v.parse().unwrap();
                        } else if k == "last-event-id" {
                            last_event_id = Some(v);
                        }
                    }
                }
                let mut body = vec![0; length];
                let _ = reader.read_exact(&mut body);
                let request = Seen {
                    method,
                    last_event_id,
                    message: serde_json::from_slice(&body).unwrap_or(Value::Null),
                };
                let count = {
                    let mut seen = shared.lock().unwrap();
                    seen.push(request.clone());
                    seen.len() - 1
                };
                let _ = stream.write_all(handler(&request, count).as_bytes());
            });
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), seen)
}

fn reply(status: &str, content_type: &str, extra: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// `protocolHandler`: initialize (with a session id), notifications (202), DELETE, and 405 for GET; `None` for a
/// request the test answers itself.
fn protocol(request: &Seen) -> Option<String> {
    match request.method.as_str() {
        "GET" => Some(reply("405 Method Not Allowed", "text/plain", "", "")),
        "DELETE" => Some(reply("200 OK", "text/plain", "", "")),
        _ if request.message.get("id").is_none() => Some(reply("202 Accepted", "text/plain", "", "")),
        _ if request.message["method"] == "initialize" => Some(reply(
            "200 OK",
            "application/json",
            "mcp-session-id: session-1\r\n",
            &json!({"jsonrpc": "2.0", "id": request.message["id"], "result": {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "http-fixture", "version": "1.0.0"}}}).to_string(),
        )),
        _ => None,
    }
}

fn connect(url: &str, options: HttpOptions) -> (Client, Arc<Mutex<Vec<String>>>) {
    let transport = Arc::new(HttpTransport::new(HttpOptions {
        url: url.into(),
        ..options
    }));
    let client = Client::new(ClientOptions {
        name: "http-test".into(),
        version: "1.0.0".into(),
        ..ClientOptions::default()
    });
    let errors = Arc::new(Mutex::new(Vec::new()));
    let sink = errors.clone();
    client.on_error(move |e| sink.lock().unwrap().push(e.to_string()));
    client.connect(transport as Arc<dyn Transport>).unwrap();
    (client, errors)
}

fn call(client: &Client) -> Result<Value, mcp::Error> {
    client.call_tool(
        "echo",
        Some(json!({})),
        RequestOptions {
            timeout: Some(HANG_GUARD),
            ..RequestOptions::default()
        },
    )
}

/// "resumes a response stream the server closed before answering": a priming event (id, no data) and a retry
/// hint, then the stream ends; the GET resuming after id 1 carries the answer.
#[test]
fn mcp_http_resumes_a_response_stream_closed_before_answering() {
    let (url, seen) = server(Arc::new(|request: &Seen, _| {
        if request.method == "GET" && request.last_event_id.is_some() {
            let answer =
                json!({"jsonrpc": "2.0", "id": 2, "result": {"content": [{"type": "text", "text": "resumed"}]}});
            return reply("200 OK", "text/event-stream", "", &format!("id: 2\ndata: {answer}\n\n"));
        }
        if let Some(response) = protocol(request) {
            return response;
        }
        reply("200 OK", "text/event-stream", "", "id: 1\nretry: 5\ndata:\n\n")
    }));
    // A server `retry` of 5 ms replaces the (here hour-long) backoff: ignoring it would trip the hang guard.
    let (client, errors) = connect(
        &url,
        HttpOptions {
            open_get_stream: Some(false),
            reconnect_initial_delay: Some(Duration::from_secs(3600)),
            reconnect_max_delay: Some(Duration::from_secs(3600)),
            ..HttpOptions::default()
        },
    );
    assert_eq!(
        call(&client).unwrap(),
        json!({"content": [{"type": "text", "text": "resumed"}]})
    );
    let resumes: Vec<Option<String>> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.method == "GET")
        .map(|s| s.last_event_id.clone())
        .collect();
    assert_eq!(resumes, [Some("1".to_string())]);
    assert_eq!(*errors.lock().unwrap(), Vec::<String>::new());
    client.close();
}

/// "fails a request whose response stream ends without an answer": no event id, so no resumption.
#[test]
fn mcp_http_response_stream_without_ids_fails_without_resuming() {
    let (url, seen) = server(Arc::new(|request: &Seen, _| {
        protocol(request).unwrap_or_else(|| reply("200 OK", "text/event-stream", "", ": nothing here\n\n"))
    }));
    let (client, _) = connect(
        &url,
        HttpOptions {
            open_get_stream: Some(false),
            ..HttpOptions::default()
        },
    );
    let error = call(&client).unwrap_err();
    assert_eq!(
        error.to_string(),
        "MCP response stream failed: stream ended without a response"
    );
    assert!(seen.lock().unwrap().iter().all(|s| s.method != "GET"));
    client.close();
}

/// Each resume that delivers nothing counts against `maxRetries`; a transient status keeps retrying until then, and
/// the request fails with the last failure.
#[test]
fn mcp_http_resumption_gives_up_after_max_retries() {
    let (url, seen) = server(Arc::new(|request: &Seen, _| {
        if request.method == "GET" {
            return reply("503 Service Unavailable", "text/plain", "", "busy");
        }
        protocol(request).unwrap_or_else(|| reply("200 OK", "text/event-stream", "", "id: 9\ndata:\n\n"))
    }));
    let (client, _) = connect(
        &url,
        HttpOptions {
            open_get_stream: Some(false),
            reconnect_initial_delay: Some(Duration::from_millis(1)),
            reconnect_max_retries: Some(3),
            ..HttpOptions::default()
        },
    );
    let error = call(&client).unwrap_err();
    assert_eq!(
        error.to_string(),
        "MCP response stream failed: MCP HTTP request failed with status 503: busy"
    );
    let resumes: Vec<Option<String>> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.method == "GET")
        .map(|s| s.last_event_id.clone())
        .collect();
    assert_eq!(resumes, vec![Some("9".to_string()); 3]);
    client.close();
}

/// A non-transient failure while resuming (404 after the session) ends the request at once.
#[test]
fn mcp_http_resumption_stops_at_a_non_transient_failure() {
    let (url, seen) = server(Arc::new(|request: &Seen, _| {
        if request.method == "GET" {
            return reply("400 Bad Request", "text/plain", "", "nope");
        }
        protocol(request).unwrap_or_else(|| reply("200 OK", "text/event-stream", "", "id: 3\ndata:\n\n"))
    }));
    let (client, _) = connect(
        &url,
        HttpOptions {
            open_get_stream: Some(false),
            reconnect_initial_delay: Some(Duration::from_millis(1)),
            ..HttpOptions::default()
        },
    );
    let error = call(&client).unwrap_err();
    assert_eq!(
        error.to_string(),
        "MCP response stream failed: MCP HTTP request failed with status 400: nope"
    );
    assert_eq!(seen.lock().unwrap().iter().filter(|s| s.method == "GET").count(), 1);
    client.close();
}

/// "reconnects the GET stream after it drops": the second GET resumes after the first stream's last id, and the
/// server's `retry` (5 ms) replaces the hour-long backoff.
#[test]
fn mcp_http_get_stream_reconnects_with_last_event_id_and_retry() {
    let (url, seen) = server(Arc::new(|request: &Seen, _| {
        if request.method != "GET" {
            return protocol(request).unwrap();
        }
        let notification = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
        match request.last_event_id {
            None => reply(
                "200 OK",
                "text/event-stream",
                "",
                &format!("retry: 5\nid: g1\ndata: {notification}\n\n"),
            ),
            Some(_) => reply(
                "200 OK",
                "text/event-stream",
                "",
                &format!("id: g2\ndata: {notification}\n\n"),
            ),
        }
    }));
    let client = Client::new(ClientOptions {
        name: "http-test".into(),
        version: "1.0.0".into(),
        ..ClientOptions::default()
    });
    let (tx, rx) = mpsc::channel();
    client.on_notification("notifications/tools/list_changed", move |_| {
        let _ = tx.send(());
    });
    let transport = Arc::new(HttpTransport::new(HttpOptions {
        url,
        reconnect_initial_delay: Some(Duration::from_secs(3600)),
        reconnect_max_delay: Some(Duration::from_secs(3600)),
        ..HttpOptions::default()
    }));
    client.connect(transport as Arc<dyn Transport>).unwrap();
    rx.recv_timeout(HANG_GUARD).unwrap();
    rx.recv_timeout(HANG_GUARD).unwrap();
    let ids: Vec<Option<String>> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.method == "GET")
        .map(|s| s.last_event_id.clone())
        .take(2)
        .collect();
    assert_eq!(ids, [None, Some("g1".to_string())]);
    client.close();
}
