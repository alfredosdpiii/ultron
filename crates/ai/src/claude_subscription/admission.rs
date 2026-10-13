//! The request-scoped loopback admission relay (`admission.py`). Claude Code can attempt further generations
//! despite `--max-turns 1` and disabled retries; the relay forwards exactly the first Messages request upstream and
//! answers every later attempt locally, so Ultron receives the first completed upstream response, its real usage and
//! stop reason. It listens on an ephemeral loopback port behind a random per-request route; native authorization
//! headers pass through memory to the upstream and are never logged or persisted. The streamed text, signed
//! thinking, tool arguments, usage and stop reason are captured before native recovery can replace them.

use super::rewrite::{pin_message_breakpoint, restore_queried_turn};
use crate::http::AbortSignal;
use reqwest::Url;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

/// Longest byte silence after the response headers before the upstream counts as dead (native's own watchdog).
pub const UPSTREAM_IDLE: Duration = Duration::from_secs(180);
/// The body of every refused attempt after the first.
pub const CONSUMED: &str =
    r#"{"type":"error","error":{"type":"invalid_request_error","message":"ULTRON_MODEL_ADMISSION_CONSUMED"}}"#;
const ERROR_BODY_LIMIT: usize = 65_536;
const HEAD_LIMIT: usize = 65_536;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ------------------------------------------------------------------------------------------------------- capture

/// The Messages SSE stream assembled into the assistant message native would build (`Capture`).
#[derive(Debug, Default)]
pub struct Capture {
    pub message: Option<Value>,
    pub complete: bool,
    pending: String,
    undecoded: Vec<u8>,
    arguments: BTreeMap<u64, String>,
}

impl Capture {
    /// Feed raw bytes; `Err` names the failure kind (malformed event or stream).
    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), &'static str> {
        self.undecoded.extend_from_slice(chunk);
        let valid = match std::str::from_utf8(&self.undecoded) {
            Ok(text) => text.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => return Err("UnicodeDecodeError"),
        };
        let rest = self.undecoded.split_off(valid);
        let decoded =
            String::from_utf8(std::mem::replace(&mut self.undecoded, rest)).map_err(|_| "UnicodeDecodeError")?;
        self.pending.push_str(&decoded);
        while let Some((start, end)) = frame_break(&self.pending) {
            let frame = self.pending[..start].to_string();
            self.pending.drain(..end);
            let data: Vec<&str> = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(|line| line.trim_start_matches(' '))
                .collect();
            if !data.is_empty() {
                let event: Value = serde_json::from_str(&data.join("\n")).map_err(|_| "ValueError")?;
                self.event(&event)?;
            }
        }
        Ok(())
    }

    pub fn event(&mut self, event: &Value) -> Result<(), &'static str> {
        match event.get("type").and_then(Value::as_str).ok_or("KeyError")? {
            "message_start" => {
                let mut message = event.get("message").cloned().ok_or("KeyError")?;
                if !message.is_object() {
                    return Err("TypeError");
                }
                if !message.get("content").is_some_and(Value::is_array) {
                    message["content"] = json!([]);
                }
                self.message = Some(message);
            }
            "content_block_start" => {
                let block = event.get("content_block").cloned().ok_or("KeyError")?;
                self.content()?.push(block);
            }
            "content_block_delta" => {
                let delta = event.get("delta").ok_or("KeyError")?;
                let index = event.get("index").and_then(Value::as_u64).ok_or("KeyError")?;
                let kind = delta.get("type").and_then(Value::as_str).ok_or("KeyError")?;
                let field = match kind {
                    "text_delta" => Some("text"),
                    "thinking_delta" => Some("thinking"),
                    "signature_delta" => Some("signature"),
                    _ => None,
                };
                if let Some(field) = field {
                    let piece = delta.get(field).and_then(Value::as_str).ok_or("KeyError")?;
                    let block = self.block(index)?;
                    let joined = format!(
                        "{}{piece}",
                        block.get(field).and_then(Value::as_str).unwrap_or_default()
                    );
                    block[field] = Value::String(joined);
                } else if kind == "input_json_delta" {
                    let piece = delta.get("partial_json").and_then(Value::as_str).ok_or("KeyError")?;
                    self.block(index)?;
                    self.arguments.entry(index).or_default().push_str(piece);
                } else if kind == "citations_delta" {
                    let citation = delta.get("citation").cloned().ok_or("KeyError")?;
                    let block = self.block(index)?.as_object_mut().ok_or("TypeError")?;
                    let citations = block.entry("citations").or_insert_with(|| json!([]));
                    citations.as_array_mut().ok_or("AttributeError")?.push(citation);
                }
            }
            "content_block_stop" => {
                let index = event.get("index").and_then(Value::as_u64).ok_or("KeyError")?;
                if let Some(raw) = self.arguments.remove(&index) {
                    // A no-argument tool call streams one input_json_delta with an empty partial_json.
                    let input = if raw.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&raw).map_err(|_| "ValueError")?
                    };
                    self.block(index)?["input"] = input;
                }
            }
            "message_delta" => {
                let message = self.message.as_mut().ok_or("TypeError")?;
                let map = message.as_object_mut().ok_or("TypeError")?;
                if let Some(delta) = event.get("delta").and_then(Value::as_object) {
                    for (k, v) in delta {
                        map.insert(k.clone(), v.clone());
                    }
                }
                let usage = map.entry("usage").or_insert_with(|| json!({}));
                let usage = usage.as_object_mut().ok_or("AttributeError")?;
                if let Some(delta) = event.get("usage").and_then(Value::as_object) {
                    for (k, v) in delta {
                        usage.insert(k.clone(), v.clone());
                    }
                }
            }
            "message_stop" => {
                self.complete =
                    self.message.as_ref().is_some_and(|m| truthy(m.get("stop_reason"))) && self.arguments.is_empty();
            }
            _ => {}
        }
        Ok(())
    }

    fn content(&mut self) -> Result<&mut Vec<Value>, &'static str> {
        self.message
            .as_mut()
            .ok_or("TypeError")?
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .ok_or("KeyError")
    }

    fn block(&mut self, index: u64) -> Result<&mut Value, &'static str> {
        self.content()?.get_mut(index as usize).ok_or("IndexError")
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(s)) => !s.is_empty(),
        _ => true,
    }
}

/// The first `\r?\n\r?\n` in `text`: (start, end).
fn frame_break(text: &str) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let mut at = i;
        if bytes[at] == b'\r' {
            at += 1;
        }
        if bytes.get(at) == Some(&b'\n') {
            let mut end = at + 1;
            if bytes.get(end) == Some(&b'\r') {
                end += 1;
            }
            if bytes.get(end) == Some(&b'\n') {
                return Some((i, end + 1));
            }
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------------------------------------- proxy

/// The HTTP CONNECT proxy for the upstream hop: host, port and the `Proxy-Authorization` value from the URL's
/// credentials. TLS still ends at the upstream host, so the subscription bearer never reaches the proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyRoute {
    pub host: String,
    pub port: u16,
    pub authorization: Option<String>,
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = text.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `getproxies_environment`: every `*_proxy` variable by scheme; a lowercase name wins over its uppercase form.
fn proxies(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut found = BTreeMap::new();
    for pass in [false, true] {
        for (name, value) in env {
            let lower = name.to_ascii_lowercase();
            if value.is_empty() || !lower.ends_with("_proxy") || (pass && *name != lower) {
                continue;
            }
            found.insert(lower[..lower.len() - 6].to_string(), value.clone());
        }
    }
    if env.contains_key("REQUEST_METHOD") {
        found.remove("http");
    }
    found
}

/// `proxy_bypass_environment`: `NO_PROXY` entries (comma or space separated) match the host or `host:port`.
fn bypassed(host: &str, port: u16, no_proxy: &str) -> bool {
    if no_proxy == "*" {
        return true;
    }
    let hostonly = host.to_ascii_lowercase();
    let full = format!("{hostonly}:{port}");
    no_proxy.replace(' ', ",").split(',').any(|name| {
        let name = name.trim().trim_start_matches('.').to_ascii_lowercase();
        if name.is_empty() {
            return false;
        }
        let dotted = format!(".{name}");
        hostonly == name || full == name || hostonly.ends_with(&dotted) || full.ends_with(&dotted)
    })
}

/// The CONNECT proxy for `upstream` from `env` (`upstream_proxy`): `https_proxy`/`HTTPS_PROXY`, then
/// `http_proxy`/`HTTP_PROXY`, with `NO_PROXY` exceptions; `ALL_PROXY` is ignored as native ignores it. An unusable
/// proxy (SOCKS, TLS-to-proxy, a bad port) is ignored with a warning that never quotes the URL.
pub fn upstream_proxy(
    upstream: &Url,
    env: &BTreeMap<String, String>,
    warnings: &mut Vec<String>,
) -> Option<ProxyRoute> {
    if upstream.scheme() != "https" {
        return None;
    }
    let found = proxies(env);
    let host = upstream.host_str()?;
    if let Some(no) = found.get("no")
        && bypassed(host, upstream.port_or_known_default().unwrap_or(443), no)
    {
        return None;
    }
    let raw = found.get("https").or_else(|| found.get("http"))?;
    let raw = if raw.contains("://") {
        raw.clone()
    } else {
        format!("http://{raw}")
    };
    let parsed = Url::parse(&raw).ok();
    let usable = parsed
        .as_ref()
        .filter(|url| url.scheme() == "http" && url.host_str().is_some_and(|h| !h.is_empty()));
    let Some(url) = usable else {
        warnings.push(
            "ignoring HTTPS_PROXY for the relay: only an http://host[:port] CONNECT proxy is supported (SOCKS and \
             TLS-to-proxy are not); connecting directly"
                .into(),
        );
        return None;
    };
    let authorization = (!url.username().is_empty() || url.password().is_some()).then(|| {
        let credentials = format!(
            "{}:{}",
            percent_decode(url.username()),
            percent_decode(url.password().unwrap_or_default())
        );
        format!("Basic {}", base64(credentials.as_bytes()))
    });
    Some(ProxyRoute {
        host: url.host_str().unwrap_or_default().trim_matches(['[', ']']).to_string(),
        port: url.port().unwrap_or(80),
        authorization,
    })
}

// -------------------------------------------------------------------------------------------------------- the gate

/// What the one admitted request produced, read after native exits.
#[derive(Debug, Default)]
pub struct Outcome {
    pub used: bool,
    pub denied: u64,
    pub request_id: Option<String>,
    pub status: Option<u16>,
    pub failure: Option<String>,
    pub unrestored: Option<String>,
    pub capture: Capture,
    pub error_body: Vec<u8>,
    /// `anthropic-ratelimit-unified-status` / `-reset` of the answer (the plan's usage window; [`super::limit`]).
    pub unified_status: Option<String>,
    pub unified_reset: Option<String>,
}

impl Outcome {
    /// The upstream's own message for a non-200 answer, `""` when none was captured.
    pub fn error_text(&self) -> String {
        let text = String::from_utf8_lossy(&self.error_body).into_owned();
        serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .unwrap_or(text)
    }
}

struct Gate {
    upstream: Url,
    queried: Option<Vec<Value>>,
    timeout: Duration,
    idle: Duration,
    proxy: Option<ProxyRoute>,
    prefix: String,
    cancelled: AtomicBool,
    abort: AbortSignal,
    outcome: Mutex<Outcome>,
    sockets: Mutex<Vec<TcpStream>>,
}

/// One request's relay. Dropping it closes the listener and aborts any upstream exchange.
pub struct Admission {
    gate: Arc<Gate>,
    /// `http://127.0.0.1:<port>/admit/<random>`: native's `ANTHROPIC_BASE_URL`.
    pub url: String,
    pub prefix: String,
    pub port: u16,
    pub warnings: Vec<String>,
    stop: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
    handlers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

pub fn random_token(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    getrandom::fill(&mut raw).expect("the OS random source works");
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

fn loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        let host = host.trim_matches(['[', ']']);
        host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
    })
}

/// Whether `upstream` may receive the subscription's request: HTTPS, or a loopback HTTP fixture, with no
/// credentials, query or fragment.
pub fn check_upstream(upstream: &str) -> Result<Url, String> {
    let url =
        Url::parse(upstream).map_err(|_| "Native upstream must be HTTPS or a loopback HTTP fixture".to_string())?;
    let scheme_ok = url.scheme() == "https" || (url.scheme() == "http" && loopback(&url));
    if !scheme_ok
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Native upstream must be HTTPS or a loopback HTTP fixture".into());
    }
    Ok(url)
}

impl Admission {
    /// Bind the relay. `queried` is the frame Ultron asked native to answer (restoration and breakpoint anchor);
    /// `env` supplies the proxy variables.
    pub fn new(
        upstream: &str,
        timeout: Duration,
        queried: Option<Vec<Value>>,
        idle: Duration,
        env: &BTreeMap<String, String>,
    ) -> Result<Admission, String> {
        let upstream = check_upstream(upstream)?;
        let mut warnings = Vec::new();
        let proxy = upstream_proxy(&upstream, env, &mut warnings);
        let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let prefix = format!("/admit/{}", random_token(32));
        let gate = Arc::new(Gate {
            upstream,
            queried,
            timeout,
            idle,
            proxy,
            prefix: prefix.clone(),
            cancelled: AtomicBool::new(false),
            abort: AbortSignal::new(),
            outcome: Mutex::new(Outcome::default()),
            sockets: Mutex::new(Vec::new()),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let handlers: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
        let listener = {
            let (gate, stop, handlers) = (gate.clone(), stop.clone(), handlers.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let gate = gate.clone();
                            lock(&handlers).push(std::thread::spawn(move || handle(&gate, stream)));
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            })
        };
        Ok(Admission {
            url: format!("http://127.0.0.1:{port}{prefix}"),
            gate,
            prefix,
            port,
            warnings,
            stop,
            listener: Some(listener),
            handlers,
        })
    }

    pub fn proxy(&self) -> Option<&ProxyRoute> {
        self.gate.proxy.as_ref()
    }

    /// Refuse further attempts and cut the active upstream exchange and native's connection.
    pub fn abort(&self) {
        self.gate.abort_all();
    }

    /// A handle another thread can abort this relay with.
    pub fn abort_handle(&self) -> AbortHandle {
        AbortHandle(self.gate.clone())
    }

    /// The outcome so far (complete once native exited and [`Admission::close`] ran).
    pub fn outcome(&self) -> MutexGuard<'_, Outcome> {
        lock(&self.gate.outcome)
    }

    /// Stop listening and wait for handlers to unwind. Does not abort a finished exchange's outcome.
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        self.abort();
        let handlers: Vec<JoinHandle<()>> = lock(&self.handlers).drain(..).collect();
        for handler in handlers {
            let _ = handler.join();
        }
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.close();
    }
}

impl Gate {
    fn abort_all(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.abort.abort();
        for socket in lock(&self.sockets).iter() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

/// Aborts one relay from any thread.
#[derive(Clone)]
pub struct AbortHandle(Arc<Gate>);

impl AbortHandle {
    pub fn abort(&self) {
        self.0.abort_all();
    }
}

struct Head {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

fn read_head(reader: &mut BufReader<TcpStream>) -> Option<Head> {
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next()?.to_string(), parts.next()?.to_string());
    let mut headers = Vec::new();
    let mut size = line.len();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            return None;
        }
        size += header.len();
        if size > HEAD_LIMIT {
            return None;
        }
        let header = header.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        let (name, value) = header.split_once(':')?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    Some(Head {
        method,
        target,
        headers,
    })
}

fn header<'h>(head: &'h Head, name: &str) -> Option<&'h str> {
    head.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn read_body(reader: &mut BufReader<TcpStream>, head: &Head) -> Option<Vec<u8>> {
    if header(head, "transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        let mut body = Vec::new();
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).ok()?;
            let size = usize::from_str_radix(size.trim().split(';').next()?, 16).ok()?;
            if size == 0 {
                let mut trailer = String::new();
                while reader.read_line(&mut trailer).ok()? > 2 {
                    trailer.clear();
                }
                return Some(body);
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..]).ok()?;
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf).ok()?;
        }
    }
    let length: usize = header(head, "content-length")?.parse().ok()?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(body)
}

fn respond(stream: &mut TcpStream, status: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

fn handle(gate: &Gate, stream: TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(gate.timeout));
    let Ok(read_half) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read_half);
    let mut writer = stream;
    let Some(head) = read_head(&mut reader) else { return };
    let (path, query) = match head.target.split_once('?') {
        Some((path, query)) => (path.to_string(), Some(query.to_string())),
        None => (head.target.clone(), None),
    };
    if head.method != "POST" || path != format!("{}/v1/messages", gate.prefix) || header(&head, "origin").is_some() {
        respond(&mut writer, "404 Not Found", b"");
        return;
    }
    {
        let mut outcome = lock(&gate.outcome);
        if gate.cancelled.load(Ordering::SeqCst) || outcome.used {
            outcome.denied += 1;
            drop(outcome);
            respond(&mut writer, "400 Bad Request", CONSUMED.as_bytes());
            return;
        }
        outcome.used = true;
    }
    if let Ok(clone) = writer.try_clone() {
        lock(&gate.sockets).push(clone);
    }
    let failure = forward(gate, &head, query.as_deref(), &mut reader, &mut writer).err();
    if let Some(failure) = failure {
        lock(&gate.outcome).failure = Some(failure);
    }
    let _ = writer.shutdown(Shutdown::Both);
}

/// Forward the admitted request and stream the answer back to native; `Err` names the failure kind.
fn forward(
    gate: &Gate,
    head: &Head,
    query: Option<&str>,
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
) -> Result<(), String> {
    let payload = read_body(reader, head).ok_or("ValueError")?;
    let queried = gate.queried.as_deref();
    let payload = match restore_queried_turn(&payload, queried) {
        Ok(restored) => restored,
        Err(mismatch) => {
            // Restoration is hygiene, not a gate: a frame it cannot prove lossless forwards as native built it.
            lock(&gate.outcome).unrestored = Some(mismatch.to_string());
            payload
        }
    };
    let payload = pin_message_breakpoint(&payload, queried);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "OSError")?;
    runtime.block_on(async {
        tokio::select! {
            result = exchange(gate, head, query, payload, writer) => result,
            () = gate.abort.cancelled() => Err("Cancelled".into()),
        }
    })
}

async fn exchange(
    gate: &Gate,
    head: &Head,
    query: Option<&str>,
    payload: Vec<u8>,
    writer: &mut TcpStream,
) -> Result<(), String> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(gate.timeout)
        .tcp_keepalive(Duration::from_secs(15))
        .tcp_keepalive_interval(Duration::from_secs(5))
        .tcp_keepalive_retries(3u32)
        .pool_max_idle_per_host(0);
    if let Some(proxy) = &gate.proxy {
        let mut route =
            reqwest::Proxy::all(format!("http://{}:{}", proxy.host, proxy.port)).map_err(|_| "ValueError")?;
        if let Some(auth) = &proxy.authorization {
            route = route.custom_http_auth(reqwest::header::HeaderValue::from_str(auth).map_err(|_| "ValueError")?);
        }
        builder = builder.proxy(route);
    }
    let client = builder.build().map_err(|_| "OSError")?;
    let mut url = gate.upstream.clone();
    url.set_path(&format!("{}/v1/messages", gate.upstream.path().trim_end_matches('/')));
    url.set_query(query);
    let mut request = client.post(url).body(payload);
    // Request identity stays native's; only the HTTP transfer encoding changes.
    const DROPPED: &[&str] = &[
        "host",
        "connection",
        "content-length",
        "transfer-encoding",
        "proxy-authorization",
        "proxy-connection",
        "accept-encoding",
    ];
    for (name, value) in &head.headers {
        if !DROPPED.contains(&name.to_ascii_lowercase().as_str()) {
            request = request.header(name.as_str(), value.as_str());
        }
    }
    request = request.header("Accept-Encoding", "identity");
    let sent = tokio::time::timeout(gate.timeout, request.send()).await;
    let mut response = match sent {
        Err(_) => return Err("TimeoutError".into()),
        Ok(Err(error)) => {
            return Err(if error.is_connect() {
                "ConnectionError"
            } else {
                "OSError"
            }
            .into());
        }
        Ok(Ok(response)) => response,
    };
    let status = response.status();
    {
        let mut outcome = lock(&gate.outcome);
        let id = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        outcome.request_id = id("request-id").or_else(|| id("x-request-id"));
        outcome.unified_status = id("anthropic-ratelimit-unified-status");
        outcome.unified_reset = id("anthropic-ratelimit-unified-reset");
        outcome.status = Some(status.as_u16());
    }
    let mut out = format!(
        "HTTP/1.1 {} {}\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("Unknown")
    );
    for (name, value) in response.headers() {
        if !matches!(name.as_str(), "connection" | "transfer-encoding" | "server" | "date") {
            out.push_str(&format!("{}: {}\r\n", name, String::from_utf8_lossy(value.as_bytes())));
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    writer.write_all(out.as_bytes()).map_err(|_| "BrokenPipeError")?;
    let idle = gate.timeout.min(gate.idle);
    loop {
        let chunk = match tokio::time::timeout(idle, response.chunk()).await {
            Err(_) => return Err("UpstreamIdle".into()),
            Ok(Err(_)) => return Err("IncompleteRead".into()),
            Ok(Ok(None)) => return Ok(()),
            Ok(Ok(Some(chunk))) => chunk,
        };
        {
            let mut outcome = lock(&gate.outcome);
            if status.as_u16() == 200 {
                outcome.capture.feed(&chunk)?;
            } else if outcome.error_body.len() < ERROR_BODY_LIMIT {
                outcome.error_body.extend_from_slice(&chunk);
            }
        }
        writer.write_all(&chunk).map_err(|_| "BrokenPipeError")?;
        writer.flush().map_err(|_| "BrokenPipeError")?;
    }
}
