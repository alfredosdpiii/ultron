//! Streamable HTTP transport (`transports/streamable-http.ts`): JSON-RPC over POST, answered with JSON or an SSE
//! stream; the session id and protocol version travel as headers; a server-to-client GET stream opens after
//! initialization and reconnects with backoff; close ends the session with DELETE.
//!
//! HTTP failures are `Error::Mcp` with code [`HTTP_ERROR_CODE`] and `data` `{kind, status, body}`, so the client's
//! error type stays unchanged; [`http_error`] reads them back. `kind` is the TypeScript class name, and network
//! failures (`TypeError` from fetch) have status 0.

use crate::jsonrpc::{Error, INTERNAL_ERROR, is_request, parse_message};
use crate::stdio::DEFAULT_MAX_MESSAGE_BYTES;
use crate::{Incoming, Sink, Transport};
use reqwest::blocking::{Client, RequestBuilder, Response};
use serde_json::{Value, json};
use std::io::Read;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const HTTP_ERROR_CODE: i64 = -32099;
const MAX_ERROR_BODY_BYTES: usize = 8 * 1024;
const ERROR_MESSAGE_BODY_CHARS: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
    pub id: Option<String>,
}

/// What the SSE parser reports, in stream order.
#[derive(Debug, Clone, PartialEq)]
pub enum SseItem {
    Event(SseEvent),
    Id(String),
    Retry(u64),
}

/// Incremental `text/event-stream` parser (`consumeSseStream`). Feed bytes, then `finish`.
pub struct SseParser {
    max_event_bytes: usize,
    buffered: Vec<u8>,
    event: Option<String>,
    id: Option<String>,
    data: Vec<String>,
    data_bytes: usize,
}

impl SseParser {
    pub fn new(max_event_bytes: usize) -> Self {
        Self {
            max_event_bytes,
            buffered: Vec::new(),
            event: None,
            id: None,
            data: Vec::new(),
            data_bytes: 0,
        }
    }

    fn too_big(&self) -> String {
        format!("MCP SSE event exceeds {} bytes", self.max_event_bytes)
    }

    fn dispatch(&mut self, out: &mut Vec<SseItem>) {
        let event = self.event.take();
        let id = self.id.take();
        if self.data.is_empty() {
            return;
        }
        out.push(SseItem::Event(SseEvent {
            event: event.filter(|e| !e.is_empty()),
            data: self.data.join("\n"),
            id: id.filter(|i| !i.is_empty()),
        }));
        self.data.clear();
        self.data_bytes = 0;
    }

    fn line(&mut self, raw: &str, out: &mut Vec<SseItem>) -> Result<(), String> {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.is_empty() {
            self.dispatch(out);
            return Ok(());
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let (field, value) = match line.find(':') {
            Some(colon) => (&line[..colon], &line[colon + 1..]),
            None => (line, ""),
        };
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                self.data_bytes += value.len() + usize::from(!self.data.is_empty());
                if self.data_bytes > self.max_event_bytes {
                    return Err(self.too_big());
                }
                self.data.push(value.into());
            }
            "event" => self.event = Some(value.into()),
            "id" if !value.contains('\0') => {
                self.id = Some(value.into());
                out.push(SseItem::Id(value.into()));
            }
            "retry" if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) => {
                out.push(SseItem::Retry(value.parse().unwrap_or(u64::MAX)));
            }
            _ => {}
        }
        Ok(())
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<SseItem>, String> {
        let mut out = Vec::new();
        self.buffered.extend_from_slice(bytes);
        while let Some(newline) = self.buffered.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buffered.drain(..=newline).take(newline).collect();
            self.line(&String::from_utf8_lossy(&line), &mut out)?;
        }
        if self.buffered.len() > self.max_event_bytes {
            return Err(self.too_big());
        }
        Ok(out)
    }

    pub fn finish(&mut self) -> Result<Vec<SseItem>, String> {
        let mut out = Vec::new();
        if !self.buffered.is_empty() {
            let rest = std::mem::take(&mut self.buffered);
            self.line(&String::from_utf8_lossy(&rest), &mut out)?;
        }
        self.dispatch(&mut out);
        Ok(out)
    }
}

/// An HTTP failure as the client's error.
pub fn http_failure(kind: &str, status: u16, message: impl Into<String>, body: &str) -> Error {
    Error::Mcp {
        code: HTTP_ERROR_CODE,
        message: message.into(),
        data: Some(json!({ "kind": kind, "status": status, "body": body })),
    }
}

/// `(kind, status)` of an error from [`http_failure`].
pub fn http_error(error: &Error) -> Option<(&str, u16)> {
    match error {
        Error::Mcp {
            code: HTTP_ERROR_CODE,
            data: Some(data),
            ..
        } => Some((data["kind"].as_str()?, u16::try_from(data["status"].as_u64()?).ok()?)),
        _ => None,
    }
}

fn network_error(error: &reqwest::Error) -> Error {
    http_failure("TypeError", 0, format!("fetch failed: {error}"), "")
}

fn describe_failure(status: u16, body: &str) -> String {
    let text = body.trim();
    let snippet = if text.chars().count() > ERROR_MESSAGE_BODY_CHARS {
        format!(
            "{}...",
            text.chars().take(ERROR_MESSAGE_BODY_CHARS - 3).collect::<String>()
        )
    } else {
        text.to_string()
    };
    let sep = if snippet.is_empty() { "" } else { ": " };
    format!("MCP HTTP request failed with status {status}{sep}{snippet}")
}

fn content_type(response: &Response) -> Option<String> {
    let value = response.headers().get("content-type")?.to_str().ok()?;
    Some(value.split(';').next().unwrap_or_default().trim().to_lowercase())
}

#[derive(Clone, Default)]
pub struct HttpOptions {
    pub url: String,
    pub headers: Vec<(String, String)>,
    /// Open the server-to-client GET stream after initialization. Default (None): true.
    pub open_get_stream: Option<bool>,
    pub max_message_bytes: Option<usize>,
    /// Delay before the first GET stream reconnect, unless the server sent `retry`. Default 1000 ms.
    pub reconnect_initial_delay: Option<Duration>,
    /// Upper bound of the exponential backoff. Default 30 s.
    pub reconnect_max_delay: Option<Duration>,
    /// Consecutive failed reconnects before giving up. Default 5.
    pub reconnect_max_retries: Option<u32>,
}

#[derive(Default)]
struct State {
    started: bool,
    closed: bool,
    session_id: Option<String>,
    protocol_version: Option<String>,
    get_started: bool,
    sink: Option<Sink>,
}

struct Shared {
    options: HttpOptions,
    client: Client,
    state: Mutex<State>,
}

pub struct HttpTransport(Arc<Shared>);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl HttpTransport {
    pub fn new(options: HttpOptions) -> Self {
        HttpTransport(Arc::new(Shared {
            options,
            client: Client::new(),
            state: Mutex::new(State::default()),
        }))
    }

    pub fn session_id(&self) -> Option<String> {
        lock(&self.0.state).session_id.clone()
    }
}

impl Shared {
    fn closed(&self) -> bool {
        lock(&self.state).closed
    }

    fn emit(&self, incoming: Incoming) {
        let sink = lock(&self.state).sink.clone();
        if let Some(sink) = sink {
            sink(incoming);
        }
    }

    fn request(&self, method: reqwest::Method) -> RequestBuilder {
        let mut builder = self.client.request(method, &self.options.url);
        for (name, value) in &self.options.headers {
            builder = builder.header(name, value);
        }
        let state = lock(&self.state);
        if let Some(id) = &state.session_id {
            builder = builder.header("Mcp-Session-Id", id);
        }
        if let Some(version) = &state.protocol_version {
            builder = builder.header("MCP-Protocol-Version", version);
        }
        builder
    }

    fn capture_session(&self, response: &Response) {
        if let Some(id) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
        {
            lock(&self.state).session_id = Some(id.into());
        }
    }

    fn check(&self, response: Response) -> Result<Response, Error> {
        let status = response.status().as_u16();
        if response.status().is_success() {
            return Ok(response);
        }
        let mut bytes = Vec::new();
        let _ = response.take(MAX_ERROR_BODY_BYTES as u64).read_to_end(&mut bytes);
        let body = String::from_utf8_lossy(&bytes).into_owned();
        if status == 401 {
            return Err(http_failure(
                "McpAuthRequiredError",
                401,
                "MCP server requires authentication",
                &body,
            ));
        }
        if status == 404 && lock(&self.state).session_id.is_some() {
            return Err(http_failure(
                "McpSessionExpiredError",
                404,
                "MCP session expired",
                &body,
            ));
        }
        Err(http_failure(
            "McpHttpError",
            status,
            describe_failure(status, &body),
            &body,
        ))
    }

    /// Reads an SSE body, emitting JSON-RPC messages. Returns the stream's last event id and retry, and whether
    /// any event arrived.
    fn consume(&self, mut body: Response, on_message: &mut dyn FnMut(&Value)) -> Result<bool, Error> {
        let max = self.options.max_message_bytes.unwrap_or(DEFAULT_MAX_MESSAGE_BYTES);
        let mut parser = SseParser::new(max);
        let mut chunk = vec![0u8; 16 * 1024];
        let mut received = false;
        loop {
            if self.closed() {
                return Ok(received);
            }
            let n = body
                .read(&mut chunk)
                .map_err(|e| http_failure("TypeError", 0, format!("terminated: {e}"), ""))?;
            let items = if n == 0 {
                parser.finish()
            } else {
                parser.feed(&chunk[..n])
            }
            .map_err(Error::Other)?;
            for item in items {
                let SseItem::Event(event) = item else { continue };
                received = true;
                if event.data.trim().is_empty() || event.event.as_deref().is_some_and(|e| e != "message") {
                    continue;
                }
                match serde_json::from_str::<Value>(&event.data)
                    .map_err(|e| Error::Other(e.to_string()))
                    .and_then(parse_message)
                {
                    Ok(message) => {
                        on_message(&message);
                        self.emit(Incoming::Message(message));
                    }
                    Err(error) => self.emit(Incoming::Error(error.to_string())),
                }
            }
            if n == 0 {
                return Ok(received);
            }
        }
    }

    /// Sleeps up to `delay`; false when the transport closed meanwhile.
    fn sleep(&self, delay: Duration) -> bool {
        let end = Instant::now() + delay;
        while Instant::now() < end {
            if self.closed() {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20).min(end.saturating_duration_since(Instant::now())));
        }
        !self.closed()
    }

    /// The server-to-client GET stream, reopened with backoff while it keeps failing transiently.
    fn run_get_stream(self: Arc<Self>) {
        let initial = self.options.reconnect_initial_delay.unwrap_or(Duration::from_secs(1));
        let max_delay = self.options.reconnect_max_delay.unwrap_or(Duration::from_secs(30));
        let max_retries = self.options.reconnect_max_retries.unwrap_or(5);
        let mut attempt: u32 = 0;
        while !self.closed() {
            let opened = self
                .request(reqwest::Method::GET)
                .header("accept", "text/event-stream")
                .send()
                .map_err(|e| network_error(&e));
            let outcome = opened.and_then(|response| {
                if response.status().as_u16() == 405 {
                    return Ok(None);
                }
                let response = self.check(response)?;
                self.capture_session(&response);
                if content_type(&response).as_deref() != Some("text/event-stream") {
                    let kind = content_type(&response).unwrap_or_else(|| "missing".into());
                    return Err(http_failure(
                        "McpHttpError",
                        response.status().as_u16(),
                        format!("Unsupported MCP GET response content type: {kind}"),
                        "",
                    ));
                }
                let opened_at = Instant::now();
                let received = self.consume(response, &mut |_| {})?;
                Ok(Some(received || opened_at.elapsed() > max_delay))
            });
            match outcome {
                Ok(None) => return,
                Ok(Some(healthy)) => {
                    if healthy {
                        attempt = 0;
                    }
                }
                Err(error) => {
                    if self.closed() {
                        return;
                    }
                    if !is_retryable(&error) {
                        self.emit(Incoming::Error(error.to_string()));
                        return;
                    }
                }
            }
            if attempt >= max_retries {
                self.emit(Incoming::Error(
                    "MCP server-to-client stream dropped and could not be reopened".into(),
                ));
                return;
            }
            let delay = initial.saturating_mul(2u32.saturating_pow(attempt)).min(max_delay);
            attempt += 1;
            if !self.sleep(delay) {
                return;
            }
        }
    }
}

/// Network failures and transient statuses (408, 429, 5xx) are worth reopening a stream for.
fn is_retryable(error: &Error) -> bool {
    match http_error(error) {
        Some(("TypeError", _)) => true,
        Some((_, status)) => status == 408 || status == 429 || status >= 500,
        None => false,
    }
}

impl Transport for HttpTransport {
    fn start(&self, sink: Sink) -> Result<(), Error> {
        let mut state = lock(&self.0.state);
        if state.started {
            return Err(Error::Other("MCP Streamable HTTP transport already started".into()));
        }
        if state.closed {
            return Err(Error::closed());
        }
        state.started = true;
        state.sink = Some(sink);
        Ok(())
    }

    fn set_protocol_version(&self, version: &str) {
        lock(&self.0.state).protocol_version = Some(version.into());
    }

    fn send(&self, message: &Value) -> Result<(), Error> {
        let shared = &self.0;
        {
            let state = lock(&shared.state);
            if !state.started || state.closed {
                return Err(Error::closed());
            }
        }
        let response = shared
            .request(reqwest::Method::POST)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .body(serde_json::to_string(message).unwrap_or_default())
            .send()
            .map_err(|e| network_error(&e))?;
        let response = shared.check(response)?;
        shared.capture_session(&response);
        let status = response.status().as_u16();
        if !is_request(message) {
            drop(response);
            let start_get =
                message["method"] == "notifications/initialized" && shared.options.open_get_stream != Some(false) && {
                    let mut state = lock(&shared.state);
                    let first = !state.get_started && !state.closed;
                    state.get_started = true;
                    first
                };
            if start_get {
                let shared = Arc::clone(shared);
                std::thread::spawn(move || shared.run_get_stream());
            }
            return Ok(());
        }
        let method = message["method"].as_str().unwrap_or_default();
        if status == 202 || status == 204 {
            return Err(http_failure(
                "McpHttpError",
                status,
                format!("MCP server accepted request {method} without a response"),
                "",
            ));
        }
        match content_type(&response).as_deref() {
            Some("application/json") => {
                let bytes = response.bytes().map_err(|e| network_error(&e))?;
                let body: Value = serde_json::from_slice(&bytes).map_err(|e| Error::Other(e.to_string()))?;
                let items = match body {
                    Value::Array(items) => items,
                    other => vec![other],
                };
                for item in items {
                    shared.emit(Incoming::Message(parse_message(item)?));
                }
                Ok(())
            }
            Some("text/event-stream") => {
                let shared = Arc::clone(shared);
                let id = message["id"].clone();
                std::thread::spawn(move || {
                    let mut answered = false;
                    let result = shared.consume(response, &mut |m| {
                        if m.get("method").is_none() && m["id"] == id {
                            answered = true;
                        }
                    });
                    if answered || shared.closed() {
                        return;
                    }
                    let reason = match result {
                        Ok(_) => "stream ended without a response".to_string(),
                        Err(error) => error.to_string(),
                    };
                    shared.emit(Incoming::Message(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": INTERNAL_ERROR, "message": format!("MCP response stream failed: {reason}") },
                    })));
                });
                Ok(())
            }
            other => Err(http_failure(
                "McpHttpError",
                status,
                format!("Unsupported MCP response content type: {}", other.unwrap_or("missing")),
                "",
            )),
        }
    }

    fn close(&self) {
        let shared = &self.0;
        let (started, session) = {
            let mut state = lock(&shared.state);
            if state.closed {
                return;
            }
            state.closed = true;
            (state.started, state.session_id.clone())
        };
        if started && session.is_some() {
            let _ = shared
                .request(reqwest::Method::DELETE)
                .timeout(Duration::from_secs(1))
                .send();
        }
        shared.emit(Incoming::Closed);
    }
}
