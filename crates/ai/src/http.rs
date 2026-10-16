//! HTTP transport seam, cancellation, SDK-shaped API errors and the provider retry policy
//! (`utils/provider-retry.ts`, `utils/error-body.ts`, and the error construction of the pinned OpenAI/Anthropic SDKs).

use crate::json::{js_len, js_prefix, parse_json, stringify};
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Lower-cased header names, as `fetch` `Headers` presents them.
pub type HeaderMap = BTreeMap<String, String>;

#[derive(Clone, Debug, PartialEq)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: HeaderMap,
    pub body: String,
}

/// A response body read chunk by chunk; `Ok(None)` is the end.
pub trait Body: Send {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>>;
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Box<dyn Body>,
}

/// Sends one request. `Err` is a connection failure (no response).
pub trait Transport: Send + Sync {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>>;

    /// This transport presenting a TLS client certificate (mTLS), as Node's `https.Agent({ cert, key })` does: the
    /// certificate (chain) and private key files' contents, PEM. A transport that cannot fails.
    fn with_client_identity(&self, cert: &[u8], key: &[u8]) -> Result<Arc<dyn Transport>, String> {
        let _ = (cert, key);
        Err("this HTTP transport cannot present a TLS client certificate".into())
    }
}

/// Request cancellation shared between the caller and the provider (`AbortSignal`).
#[derive(Clone, Debug, Default)]
pub struct AbortSignal(Arc<SignalState>);

#[derive(Debug, Default)]
struct SignalState {
    aborted: AtomicBool,
    notify: Notify,
}

impl AbortSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn abort(&self) {
        self.0.aborted.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn aborted(&self) -> bool {
        self.0.aborted.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            if self.aborted() {
                return;
            }
            notified.await;
        }
    }
}

pub(crate) fn is_aborted(signal: Option<&AbortSignal>) -> bool {
    signal.is_some_and(AbortSignal::aborted)
}

/// Resolves when `signal` aborts; never without one.
pub(crate) async fn cancelled(signal: Option<&AbortSignal>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Which SDK's error construction to mirror.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sdk {
    Anthropic,
    OpenAi,
}

/// A failed provider request or stream, carrying what upstream's error objects carry.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Failure {
    pub message: String,
    pub status: Option<u16>,
    pub headers: Option<Box<HeaderMap>>,
    /// The SDK error's `error` field (OpenAI: the body's `error` object; Anthropic: the whole body).
    pub error: Option<Box<Value>>,
    /// An SDK `APIError` (statusless only for connection failures); plain `Error`s are never retried.
    pub api: bool,
    /// Raw `error.metadata.raw` from an OpenAI error body.
    pub raw_metadata: Option<String>,
}

impl Failure {
    pub fn plain(message: impl Into<String>) -> Self {
        Failure {
            message: message.into(),
            status: None,
            headers: None,
            error: None,
            api: false,
            raw_metadata: None,
        }
    }

    /// `APIError.makeMessage` and `APIError.generate` of both SDKs.
    pub fn api(
        sdk: Sdk,
        status: Option<u16>,
        headers: Option<HeaderMap>,
        body: Option<Value>,
        text: Option<String>,
    ) -> Self {
        let error = match sdk {
            Sdk::Anthropic => body.clone(),
            Sdk::OpenAi => body.as_ref().and_then(|b| b.get("error").cloned()),
        };
        let message_field = error.as_ref().and_then(|e| e.get("message")).filter(|m| truthy(m));
        let msg = match (message_field, &error) {
            (Some(Value::String(s)), _) => Some(s.clone()),
            (Some(other), _) => Some(stringify(other)),
            (None, Some(e)) if truthy(e) => Some(stringify(e)),
            _ => text.filter(|t| !t.is_empty()),
        };
        let message = match (status, msg) {
            (Some(status), Some(msg)) if !msg.is_empty() => format!("{status} {msg}"),
            (Some(status), _) => format!("{status} status code (no body)"),
            (None, Some(msg)) if !msg.is_empty() => msg,
            _ => "(no status code or body)".to_string(),
        };
        let raw_metadata = error
            .as_ref()
            .and_then(|e| e.pointer("/metadata/raw"))
            .filter(|r| truthy(r))
            .map(js_string);
        Failure {
            message,
            status,
            headers: headers.map(Box::new),
            error: error.map(Box::new),
            api: true,
            raw_metadata,
        }
    }

    fn connection() -> Self {
        Failure {
            api: true,
            ..Failure::plain("Connection error.")
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.as_ref()?.get(name).map(String::as_str)
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64() != Some(0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// `String(value)`.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => stringify(other),
    }
}

pub(crate) const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4000;

/// `formatProviderError(normalizeProviderError(error), prefix)`.
pub(crate) fn format_provider_error(failure: &Failure, prefix: Option<&str>) -> String {
    let body = failure
        .error
        .as_ref()
        .filter(|e| e.as_object().is_some_and(|o| !o.is_empty()))
        .map(|e| stringify(e))
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
        .map(|text| truncate_error_text(&text, MAX_PROVIDER_ERROR_BODY_CHARS));
    let carries_body = body.as_ref().is_none_or(|b| failure.message.contains(b.as_str()));
    match (carries_body, failure.status, body) {
        (false, Some(status), Some(body)) => match prefix {
            Some(prefix) => format!("{prefix} ({status}): {body}"),
            None => format!("{status}: {body}"),
        },
        (_, status, _) => match (prefix, status) {
            (Some(prefix), Some(status)) => format!("{prefix} ({status}): {}", failure.message),
            _ => failure.message.clone(),
        },
    }
}

pub(crate) fn truncate_error_text(text: &str, max: usize) -> String {
    let len = js_len(text);
    if len <= max {
        return text.to_string();
    }
    format!("{}... [truncated {} chars]", js_prefix(text, max), len - max)
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RetryOptions {
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<f64>,
}

const DEFAULT_MAX_RETRY_DELAY_MS: f64 = 60_000.0;

fn is_retryable(failure: &Failure) -> bool {
    match failure.header("x-should-retry") {
        Some("true") => return true,
        Some("false") => return false,
        _ => {}
    }
    match failure.status {
        None => true,
        Some(status) => matches!(status, 408 | 409 | 429) || status >= 500,
    }
}

/// `Number.parseFloat`: the longest numeric prefix after leading whitespace.
fn parse_float(text: &str) -> Option<f64> {
    let text = text.trim_start();
    let mut end = 0;
    let mut seen_digit = false;
    let mut seen_dot = false;
    let mut seen_exp = false;
    let bytes = text.as_bytes();
    while end < bytes.len() {
        let b = bytes[end];
        let ok = match b {
            b'0'..=b'9' => {
                seen_digit = true;
                true
            }
            b'+' | b'-' => end == 0 || matches!(bytes[end - 1], b'e' | b'E'),
            b'.' if !seen_dot && !seen_exp => {
                seen_dot = true;
                true
            }
            b'e' | b'E' if seen_digit && !seen_exp => {
                seen_exp = true;
                true
            }
            _ => false,
        };
        if !ok {
            break;
        }
        end += 1;
    }
    let mut candidate = &text[..end];
    while !candidate.is_empty() {
        if let Ok(value) = candidate.parse::<f64>() {
            return Some(value);
        }
        candidate = &candidate[..candidate.len() - 1];
    }
    if text.starts_with("Infinity") || text.starts_with("+Infinity") {
        return Some(f64::INFINITY);
    }
    None
}

fn validate_delay(delay_ms: f64, max: Option<f64>, message: &str) -> Result<f64, Failure> {
    let max = max.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if max > 0.0 && delay_ms > max {
        return Err(Failure::plain(format!(
            "Server requested {}s retry delay (max: {}s). {message}",
            (delay_ms / 1000.0).ceil(),
            (max / 1000.0).ceil()
        )));
    }
    Ok(delay_ms)
}

/// Delay before retry `retry_index`. A non-numeric `retry-after` (an HTTP date) waits 0 ms here; upstream waits until
/// that date.
fn retry_delay_ms(failure: &Failure, retry_index: u32, max: Option<f64>) -> Result<f64, Failure> {
    if let Some(value) = failure
        .header("retry-after-ms")
        .filter(|v| !v.is_empty())
        .and_then(parse_float)
    {
        return validate_delay(value, max, &failure.message);
    }
    if let Some(retry_after) = failure.header("retry-after").filter(|v| !v.is_empty()) {
        let delay = parse_float(retry_after).map_or(0.0, |seconds| seconds * 1000.0);
        return validate_delay(delay, max, &failure.message);
    }
    let exponential = (0.5 * 2f64.powi(retry_index as i32)).min(8.0) * 1000.0;
    Ok(exponential * (1.0 - jitter() * 0.25))
}

/// A cheap uniform value in [0, 1) for backoff jitter.
fn jitter() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    f64::from(nanos % 1_000_000) / 1_000_000.0
}

fn abort_failure() -> Failure {
    Failure::plain("Request aborted")
}

/// Send `request` and turn a non-2xx response into the SDK's `APIError`, retrying like `retryProviderRequest`.
pub(crate) async fn send_with_retry(
    transport: &dyn Transport,
    request: &HttpRequest,
    sdk: Sdk,
    retry: RetryOptions,
    signal: Option<&AbortSignal>,
) -> Result<HttpResponse, Failure> {
    let max_retries = retry.max_retries.unwrap_or(0);
    let mut remaining = max_retries;
    loop {
        let attempt = send_once(transport, request, sdk, signal).await;
        let failure = match attempt {
            Ok(response) => return Ok(response),
            Err(failure) => failure,
        };
        if is_aborted(signal) {
            return Err(abort_failure());
        }
        if remaining == 0 || !failure.api || !is_retryable(&failure) {
            return Err(failure);
        }
        let retry_index = max_retries - remaining;
        remaining -= 1;
        let delay = retry_delay_ms(&failure, retry_index, retry.max_retry_delay_ms)?;
        let sleep = tokio::time::sleep(Duration::from_secs_f64(delay.max(0.0) / 1000.0));
        tokio::select! {
            () = sleep => {}
            () = cancelled(signal) => return Err(abort_failure()),
        }
    }
}

async fn send_once(
    transport: &dyn Transport,
    request: &HttpRequest,
    sdk: Sdk,
    signal: Option<&AbortSignal>,
) -> Result<HttpResponse, Failure> {
    if is_aborted(signal) {
        return Err(Failure {
            api: true,
            ..Failure::plain("Request was aborted.")
        });
    }
    let sent = tokio::select! {
        sent = transport.send(request.clone()) => sent,
        () = cancelled(signal) => return Err(Failure { api: true, ..Failure::plain("Request was aborted.") }),
    };
    let mut response = sent.map_err(|_| Failure::connection())?;
    if (200..300).contains(&response.status) {
        return Ok(response);
    }
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.body.next_chunk().await {
        bytes.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let body = parse_json(&text).ok().filter(truthy);
    let text = if body.is_some() { None } else { Some(text) };
    Err(Failure::api(
        sdk,
        Some(response.status),
        Some(response.headers),
        body,
        text,
    ))
}

/// Ordered header assignments: names are case-insensitive, later values win, `None` removes.
#[derive(Clone, Debug, Default)]
pub(crate) struct HeaderBuilder(Vec<(String, Option<String>)>);

impl HeaderBuilder {
    pub fn set(&mut self, name: &str, value: impl Into<String>) -> &mut Self {
        self.0.push((name.to_string(), Some(value.into())));
        self
    }

    pub fn extend<'a, I>(&mut self, headers: I) -> &mut Self
    where
        I: IntoIterator<Item = (&'a String, &'a Option<String>)>,
    {
        self.0.extend(headers.into_iter().map(|(k, v)| (k.clone(), v.clone())));
        self
    }

    pub fn extend_set<'a, I>(&mut self, headers: I) -> &mut Self
    where
        I: IntoIterator<Item = (&'a String, &'a String)>,
    {
        self.0
            .extend(headers.into_iter().map(|(k, v)| (k.clone(), Some(v.clone()))));
        self
    }

    pub fn build(&self) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in &self.0 {
            let name = name.to_ascii_lowercase();
            match value {
                Some(value) => map.insert(name, value.clone()),
                None => map.remove(&name),
            };
        }
        map
    }
}

/// `hasHeader`: a non-blank value under `name`, case-insensitively.
pub(crate) fn has_header(headers: Option<&indexmap::IndexMap<String, Option<String>>>, name: &str) -> bool {
    headers.is_some_and(|headers| {
        headers
            .iter()
            .any(|(k, v)| k.eq_ignore_ascii_case(name) && v.as_ref().is_some_and(|v| !v.trim().is_empty()))
    })
}

/// SDK URL join: one duplicate slash between base and path is dropped.
pub(crate) fn join_url(base: &str, path: &str) -> String {
    match (base.ends_with('/'), path.strip_prefix('/')) {
        (true, Some(rest)) => format!("{base}{rest}"),
        _ => format!("{base}{path}"),
    }
}

/// `DEFAULT_HTTP_IDLE_TIMEOUT_MS` of `core/http-dispatcher.ts`: five minutes.
pub const DEFAULT_HTTP_IDLE_TIMEOUT: Duration = Duration::from_millis(300_000);

/// `parseHttpIdleTimeoutMs` of the `httpIdleTimeoutMs` setting: a non-negative number of milliseconds (floored), a
/// numeric string, or `"disabled"` (0). `None` for anything else.
pub fn parse_http_idle_timeout_ms(value: &Value) -> Option<u64> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.eq_ignore_ascii_case("disabled") {
                return Some(0);
            }
            if trimmed.is_empty() {
                return None;
            }
            trimmed
                .parse::<f64>()
                .ok()
                .and_then(|n| parse_http_idle_timeout_ms(&serde_json::json!(n)))
        }
        Value::Number(n) => n
            .as_f64()
            .filter(|n| n.is_finite() && *n >= 0.0)
            .map(|n| n.floor() as u64),
        _ => None,
    }
}

/// The idle timeout a `httpIdleTimeoutMs` setting selects (`None`: disabled; an invalid or missing setting is the
/// default).
pub fn http_idle_timeout(setting: Option<&Value>) -> Option<Duration> {
    match setting.and_then(parse_http_idle_timeout_ms) {
        Some(0) => None,
        Some(ms) => Some(Duration::from_millis(ms)),
        None => Some(DEFAULT_HTTP_IDLE_TIMEOUT),
    }
}

/// Real HTTP over reqwest with rustls.
#[derive(Clone, Debug, Default)]
pub struct ReqwestTransport {
    client: reqwest::Client,
    timeout: Option<Duration>,
    /// The longest wait for the response headers, or between two body chunks (undici's `headersTimeout` and
    /// `bodyTimeout`, which TypeScript sets to `httpIdleTimeoutMs`).
    idle: Option<Duration>,
    /// How `client` was built, so a client presenting a certificate can be built alike (`None`: a caller's client).
    config: Option<crate::net::ClientConfig>,
}

impl ReqwestTransport {
    pub fn new(timeout: Option<Duration>) -> Self {
        let config = crate::net::ClientConfig::default();
        let client = config.client().unwrap_or_default();
        ReqwestTransport {
            config: Some(config),
            ..Self::with_client(client, timeout)
        }
    }

    /// A transport whose clients are built from `config` (an injected environment's proxies).
    pub fn configured(config: crate::net::ClientConfig, timeout: Option<Duration>) -> Result<Self, String> {
        let client = config.client()?;
        Ok(ReqwestTransport {
            config: Some(config),
            ..Self::with_client(client, timeout)
        })
    }

    /// [`ReqwestTransport::configured`] with the proxies of the environment `var` holds (`HTTP_PROXY`,
    /// `HTTPS_PROXY`, `NO_PROXY`, as the TypeScript HTTP stack reads them).
    pub fn for_env(var: &dyn Fn(&str) -> Option<String>, timeout: Option<Duration>) -> Self {
        let config = crate::net::ClientConfig::from_lookup(var);
        Self::configured(config, timeout).unwrap_or_else(|_| Self::new(timeout))
    }

    pub fn with_client(client: reqwest::Client, timeout: Option<Duration>) -> Self {
        ReqwestTransport {
            client,
            timeout,
            idle: None,
            config: None,
        }
    }

    /// Fail a request whose headers, or whose next body chunk, take longer than `idle` (`None`: wait forever).
    /// A stalled body fails as `terminated`, as Node's fetch reports undici's body timeout, which the retry
    /// classifier retries.
    pub fn with_idle_timeout(mut self, idle: Option<Duration>) -> Self {
        self.idle = idle;
        self
    }
}

struct ReqwestBody(reqwest::Response, Option<Duration>);

impl Body for ReqwestBody {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        Box::pin(async move {
            let chunk = self.0.chunk();
            let chunk = match self.1 {
                Some(idle) => tokio::time::timeout(idle, chunk)
                    .await
                    .map_err(|_| "terminated".to_string())?,
                None => chunk.await,
            };
            chunk.map(|c| c.map(|b| b.to_vec())).map_err(|e| e.to_string())
        })
    }
}

impl Transport for ReqwestTransport {
    fn with_client_identity(&self, cert: &[u8], key: &[u8]) -> Result<Arc<dyn Transport>, String> {
        let config = self
            .config
            .as_ref()
            .ok_or("this HTTP transport cannot present a TLS client certificate")?;
        let mut pem = cert.to_vec();
        if !pem.ends_with(b"\n") {
            pem.push(b'\n');
        }
        pem.extend_from_slice(key);
        let identity = reqwest::Identity::from_pem(&pem).map_err(|e| e.to_string())?;
        let client = config
            .builder()?
            .identity(identity)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(ReqwestTransport {
            client,
            timeout: self.timeout,
            idle: self.idle,
            config: self.config.clone(),
        }))
    }

    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|e| e.to_string())?;
            let mut builder = self.client.request(method, &request.url).body(request.body);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            if let Some(timeout) = self.timeout {
                builder = builder.timeout(timeout);
            }
            let response = match self.idle {
                Some(idle) => tokio::time::timeout(idle, builder.send())
                    .await
                    .map_err(|_| "Headers Timeout Error".to_string())?,
                None => builder.send().await,
            }
            .map_err(|e| e.to_string())?;
            let status = response.status().as_u16();
            let mut headers = HeaderMap::new();
            for (name, value) in response.headers() {
                if let Ok(value) = value.to_str() {
                    headers.insert(name.as_str().to_string(), value.to_string());
                }
            }
            Ok(HttpResponse {
                status,
                headers,
                body: Box::new(ReqwestBody(response, self.idle)),
            })
        })
    }
}

/// The SSE body reader both stream loops use: each call yields the next chunk, `None` at the end, or the abort.
pub(crate) enum Read {
    Chunk(Vec<u8>),
    End,
    Aborted,
    Failed(String),
}

pub(crate) async fn read_chunk(body: &mut dyn Body, signal: Option<&AbortSignal>) -> Read {
    tokio::select! {
        chunk = body.next_chunk() => match chunk {
            Ok(Some(bytes)) => Read::Chunk(bytes),
            Ok(None) => Read::End,
            Err(error) => Read::Failed(error),
        },
        () = cancelled(signal) => Read::Aborted,
    }
}
