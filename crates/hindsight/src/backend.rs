//! `createHindsightBackend` of `ultron/memory.ts`: the native Hindsight 0.9.2 document endpoints of one bank.
//!
//! Every call is one request under `<base>/v1/default/banks/<bank>`, with `content-type: application/json` (also on
//! GET and DELETE, as TS sends it) and the configured headers; no retries. Errors are memory error codes only, never
//! a body, URL or cause: a non-2xx answer, a refused redirect or no answer is HTTP_ERROR, a body that is not JSON
//! (or empty) INVALID_RESPONSE, a body over the limit RESPONSE_TOO_LARGE, the per-request timeout TIMEOUT and the
//! caller's signal ABORTED. With `ensure_bank` the bank is created (idempotent PUT `{}`) before the first call; a
//! failed creation is retried on the next call rather than remembered.

use crate::transport::{HttpRequest, ReqwestTransport, Transport, TransportError};
use kernel::Signal;
use kernel_apis::MemoryBackend;
use kernel_apis::MemoryError;
use kernel_apis::util::{js_trim, lock, stringify, utf16_len};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `HindsightBackendOptions` of memory.ts. Numbers are JavaScript numbers: they must be safe integers in range.
#[derive(Clone)]
pub struct HindsightOptions {
    pub base_url: String,
    pub bank_id: String,
    /// Scope ("session", "project", "global") → identity tags.
    pub scope_tags: Map<String, Value>,
    /// Extra request headers, as `new Headers(record)` takes them.
    pub headers: Vec<(String, String)>,
    /// Default 10 000; 1..=120 000.
    pub timeout_ms: Option<f64>,
    /// Default 1 MiB; 1..=16 MiB.
    pub max_response_bytes: Option<f64>,
    /// Create the bank before the first call.
    pub ensure_bank: bool,
    /// The HTTP stack; `None`: [`ReqwestTransport`].
    pub transport: Option<Arc<dyn Transport>>,
}

impl HindsightOptions {
    pub fn new(base_url: impl Into<String>, bank_id: impl Into<String>, scope_tags: Map<String, Value>) -> Self {
        Self {
            base_url: base_url.into(),
            bank_id: bank_id.into(),
            scope_tags,
            headers: Vec::new(),
            timeout_ms: None,
            max_response_bytes: None,
            ensure_bank: false,
            transport: None,
        }
    }
}

pub struct HindsightBackend {
    namespace: String,
    filters: Map<String, Value>,
    headers: Vec<(String, String)>,
    timeout: Duration,
    max_bytes: usize,
    ensure_bank: bool,
    bank_ready: Mutex<bool>,
    transport: Arc<dyn Transport>,
}

const fn code(code: &'static str) -> MemoryError {
    MemoryError::code(code)
}

fn safe_integer_in(value: f64, min: f64, max: f64) -> bool {
    value.is_finite() && value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0 && value >= min && value <= max
}

/// `encodeURIComponent`.
pub fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// A path segment: a non-blank id other than "." and "..", at most 2048 UTF-16 units, URI-component encoded.
fn segment(value: &str) -> Result<String, MemoryError> {
    if js_trim(value).is_empty() || value == "." || value == ".." || utf16_len(value) > 2048 {
        return Err(code("INVALID_CONFIG"));
    }
    Ok(encode_uri_component(value))
}

/// JavaScript's `\s`.
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `/(?:^|:)<scope>:[^\s:][^\s]*$/`.
fn names_scope(tag: &str, scope: &str) -> bool {
    let prefix = format!("{scope}:");
    tag.char_indices().any(|(at, _)| {
        (at == 0 || tag[..at].ends_with(':'))
            && tag[at..].strip_prefix(&prefix).is_some_and(|rest| {
                let mut chars = rest.chars();
                chars.next().is_some_and(|first| !js_space(first) && first != ':') && !chars.any(js_space)
            })
    })
}

/// `scopeTags(value, scope)`: unique non-blank strings, one of which names the scope.
fn scope_tags(value: &Value, scope: &str) -> Result<Value, MemoryError> {
    let tags: Option<Vec<&str>> = value
        .as_array()
        .and_then(|items| items.iter().map(Value::as_str).collect());
    let valid = tags.as_ref().is_some_and(|tags| {
        !tags.is_empty()
            && tags.iter().all(|tag| !js_trim(tag).is_empty())
            && tags.iter().enumerate().all(|(i, tag)| !tags[..i].contains(tag))
            && tags.iter().any(|tag| names_scope(tag, scope))
    });
    if valid {
        Ok(value.clone())
    } else {
        Err(code("UNSUPPORTED_SCOPE"))
    }
}

/// HTTP whitespace fetch strips from both ends of a header value.
fn http_trim(value: &str) -> &str {
    value.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r'))
}

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

/// `new Headers(record)` then `set("content-type", "application/json")`: names lower-cased and valid tokens, values
/// trimmed, Latin-1 and free of NUL/CR/LF, repeated names joined with ", ", sorted by name.
fn build_headers(extra: &[(String, String)]) -> Result<Vec<(String, String)>, MemoryError> {
    let mut headers: Vec<(String, String)> = Vec::new();
    for (name, value) in extra {
        if name.is_empty() || !name.chars().all(token_char) {
            return Err(code("INVALID_CONFIG"));
        }
        let value = http_trim(value);
        if value
            .chars()
            .any(|c| matches!(c, '\0' | '\r' | '\n') || c as u32 > 0xff)
        {
            return Err(code("INVALID_CONFIG"));
        }
        let name = name.to_ascii_lowercase();
        match headers.iter_mut().find(|(have, _)| *have == name) {
            Some((_, joined)) => {
                joined.push_str(", ");
                joined.push_str(value);
            }
            None => headers.push((name, value.to_string())),
        }
    }
    headers.retain(|(name, _)| name != "content-type");
    headers.push(("content-type".into(), "application/json".into()));
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(headers)
}

impl HindsightBackend {
    /// Validates the options like TS: INVALID_CONFIG for limits, URL, bank or headers; UNSUPPORTED_SCOPE for tags.
    pub fn new(options: HindsightOptions) -> Result<Self, MemoryError> {
        let timeout_ms = options.timeout_ms.unwrap_or(10_000.0);
        let max_bytes = options.max_response_bytes.unwrap_or(1_048_576.0);
        if !safe_integer_in(timeout_ms, 1.0, 120_000.0) || !safe_integer_in(max_bytes, 1.0, 16_777_216.0) {
            return Err(code("INVALID_CONFIG"));
        }
        let base = url::Url::parse(&options.base_url).map_err(|_| code("INVALID_CONFIG"))?;
        if !matches!(base.scheme(), "http" | "https")
            || !base.username().is_empty()
            || base.password().is_some_and(|p| !p.is_empty())
            || base.query().is_some_and(|q| !q.is_empty())
            || base.fragment().is_some_and(|f| !f.is_empty())
            || utf16_len(base.as_str()) > 8192
        {
            return Err(code("INVALID_CONFIG"));
        }
        let href = base.as_str();
        let namespace = format!(
            "{}/v1/default/banks/{}",
            href.strip_suffix('/').unwrap_or(href),
            segment(&options.bank_id)?
        );
        if utf16_len(&namespace) > 8192 {
            return Err(code("INVALID_CONFIG"));
        }
        let mut filters = Map::new();
        for (scope, tags) in &options.scope_tags {
            if !matches!(scope.as_str(), "session" | "project" | "global") {
                return Err(code("UNSUPPORTED_SCOPE"));
            }
            filters.insert(scope.clone(), scope_tags(tags, scope)?);
        }
        let headers = build_headers(&options.headers)?;
        let transport = match options.transport {
            Some(transport) => transport,
            None => Arc::new(ReqwestTransport::new().map_err(|_| code("INVALID_CONFIG"))?),
        };
        Ok(Self {
            namespace,
            filters,
            headers,
            timeout: Duration::from_millis(timeout_ms as u64),
            max_bytes: max_bytes as usize,
            ensure_bank: options.ensure_bank,
            bank_ready: Mutex::new(false),
            transport,
        })
    }

    /// One request: `<namespace><path>`, the body as `JSON.stringify` writes it, the answer parsed as JSON.
    fn request(&self, path: &str, method: &str, body: Option<&Value>, signal: &Signal) -> Result<Value, MemoryError> {
        let url = format!("{}{path}", self.namespace);
        if utf16_len(&url) > 8192 {
            return Err(code("INVALID_CONFIG"));
        }
        if signal.is_aborted() {
            return Err(code("ABORTED"));
        }
        let request = HttpRequest {
            method: method.into(),
            url,
            headers: self.headers.clone(),
            body: body.map(|body| stringify(body).into_bytes()),
        };
        let response = self
            .transport
            .send(request, self.max_bytes, self.timeout, signal)
            .map_err(|error| {
                code(match error {
                    TransportError::Timeout => "TIMEOUT",
                    TransportError::Aborted => "ABORTED",
                    TransportError::TooLarge => "RESPONSE_TOO_LARGE",
                    TransportError::Failed => "HTTP_ERROR",
                })
            })?;
        if signal.is_aborted() {
            return Err(code("ABORTED"));
        }
        if !(200..300).contains(&response.status) {
            return Err(code("HTTP_ERROR"));
        }
        serde_json::from_str(&String::from_utf8_lossy(&response.body)).map_err(|_| code("INVALID_RESPONSE"))
    }

    fn call(&self, path: &str, method: &str, body: Option<&Value>, signal: &Signal) -> Result<Value, MemoryError> {
        if self.ensure_bank {
            // Callers arriving during the creation wait for it, as they share its promise in TS.
            let mut ready = lock(&self.bank_ready);
            if !*ready {
                self.request("", "PUT", Some(&json!({})), signal)?;
                *ready = true;
            }
        }
        self.request(path, method, body, signal)
    }

    /// `retain` with the error as a [`MemoryError`].
    pub(crate) fn retain_items(&self, request: &Value, signal: &Signal) -> Result<Value, MemoryError> {
        self.call("/memories", "POST", Some(request), signal)
    }

    fn document(&self, prefix: &str, id: &str, method: &str, signal: &Signal) -> Result<Value, String> {
        let path = format!("/{prefix}/{}", segment(id).map_err(|e| e.code.to_string())?);
        self.call(&path, method, None, signal).map_err(|e| e.code.to_string())
    }
}

impl MemoryBackend for HindsightBackend {
    fn namespace(&self) -> String {
        self.namespace.clone()
    }

    fn scope_tags(&self) -> Map<String, Value> {
        self.filters.clone()
    }

    fn recall(&self, request: &Value, signal: &Signal) -> Result<Value, String> {
        self.call("/memories/recall", "POST", Some(request), signal)
            .map_err(|e| e.code.to_string())
    }

    fn retain(&self, request: &Value, signal: &Signal) -> Result<Value, String> {
        self.retain_items(request, signal).map_err(|e| e.code.to_string())
    }

    fn get(&self, document_id: &str, signal: &Signal) -> Result<Value, String> {
        self.document("documents", document_id, "GET", signal)
    }

    fn delete(&self, document_id: &str, signal: &Signal) -> Result<Value, String> {
        self.document("documents", document_id, "DELETE", signal)
    }

    fn operation(&self, operation_id: &str, signal: &Signal) -> Result<Value, String> {
        self.document("operations", operation_id, "GET", signal)
    }
}
