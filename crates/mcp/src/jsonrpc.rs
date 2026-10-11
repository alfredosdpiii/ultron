//! JSON-RPC 2.0 message checks and the client's errors (`protocol/jsonrpc.ts`).

use serde_json::{Map, Value, json};
use std::fmt;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// A JSON-RPC error response, or a malformed result (`McpError`).
    Mcp {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// `McpConnectionClosedError`.
    ConnectionClosed(String),
    /// `McpTimeoutError`, in milliseconds.
    Timeout(u64),
    /// `McpAbortError` (name `AbortError`).
    Aborted,
    /// Any other failure (`Error`).
    Other(String),
}

impl Error {
    pub fn mcp(code: i64, message: impl Into<String>) -> Self {
        Error::Mcp {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Error::mcp(INVALID_REQUEST, message)
    }

    pub fn closed() -> Self {
        Error::ConnectionClosed("MCP connection closed".into())
    }

    /// The TypeScript error class name.
    pub fn name(&self) -> &'static str {
        match self {
            Error::Mcp { .. } => "McpError",
            Error::ConnectionClosed(_) => "McpConnectionClosedError",
            Error::Timeout(_) => "McpTimeoutError",
            Error::Aborted => "AbortError",
            Error::Other(_) => "Error",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Mcp { message, .. } | Error::ConnectionClosed(message) | Error::Other(message) => {
                f.write_str(message)
            }
            Error::Timeout(ms) => write!(f, "MCP request timed out after {ms}ms"),
            Error::Aborted => f.write_str("MCP request aborted"),
        }
    }
}

impl std::error::Error for Error {}

pub fn is_id(value: &Value) -> bool {
    value.is_string() || value.as_f64().is_some_and(f64::is_finite)
}

fn object(message: &Value) -> Option<&Map<String, Value>> {
    message.as_object().filter(|m| m.get("jsonrpc") == Some(&json!("2.0")))
}

pub fn is_request(message: &Value) -> bool {
    object(message).is_some_and(|m| m.get("id").is_some_and(is_id) && m.get("method").is_some_and(Value::is_string))
}

pub fn is_notification(message: &Value) -> bool {
    object(message).is_some_and(|m| !m.contains_key("id") && m.get("method").is_some_and(Value::is_string))
}

pub fn is_response(message: &Value) -> bool {
    let Some(m) = object(message) else { return false };
    if !m.get("id").is_some_and(is_id) {
        return false;
    }
    if m.contains_key("result") {
        return !m.contains_key("error");
    }
    m.get("error")
        .and_then(Value::as_object)
        .is_some_and(|e| e.get("code").is_some_and(Value::is_number) && e.get("message").is_some_and(Value::is_string))
}

/// Accepts a request, notification or response; anything else is an invalid-request `McpError`.
pub fn parse_message(value: Value) -> Result<Value, Error> {
    if is_request(&value) || is_notification(&value) || is_response(&value) {
        Ok(value)
    } else {
        Err(Error::invalid("Invalid JSON-RPC message"))
    }
}

/// `String(id)` as JavaScript prints a JSON-RPC id.
pub fn id_text(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
