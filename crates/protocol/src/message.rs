//! Validated protocol envelopes (port of `protocol.ts` and `codec.ts`).
//!
//! Strict objects: every envelope must carry exactly its schema's keys. Routed call, result and
//! update payloads are opaque but must be JSON values (no byte strings).

use std::fmt;

use crate::cbor::{CborOptions, Value, decode_cbor, encode_cbor};
use crate::framing::{DEFAULT_MAX_FRAME_LENGTH, FrameDecoder, encode_frame};

pub const PROTOCOL_VERSION: i64 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolValidationError(pub String);

impl fmt::Display for ProtocolValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolValidationError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolError {
    pub code: String,
    pub message: String,
}

impl ProtocolError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTarget {
    pub server_id: String,
    pub session_id: String,
    pub attachment_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcTarget {
    /// A server-wide call, fenced to one logical server.
    Server { server_id: String },
    /// A session call, fenced to one server, durable session and live attachment.
    Session(SessionTarget),
}

impl RpcTarget {
    pub fn server_id(&self) -> &str {
        match self {
            RpcTarget::Server { server_id } | RpcTarget::Session(SessionTarget { server_id, .. }) => server_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    Hello { version: i64 },
    Request { id: String, target: RpcTarget, call: Value },
    Cancel { id: String, target: RpcTarget },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    Hello {
        version: i64,
        server_id: String,
    },
    HelloError {
        error: ProtocolError,
    },
    /// `Ok(None)` is a successful void response without a `result` field.
    Response {
        id: String,
        result: Result<Option<Value>, ProtocolError>,
    },
    ServiceUpdate {
        subscription_id: String,
        update: Value,
    },
    Attachment {
        attachment: Option<SessionTarget>,
    },
}

/// Canonical lowercase UUIDv4.
pub fn is_server_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, &b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            14 => b == b'4',
            19 => matches!(b, b'8' | b'9' | b'a' | b'b'),
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

pub fn is_supported_protocol_version(version: i64) -> bool {
    version == PROTOCOL_VERSION
}

fn text(value: &str) -> Value {
    Value::text(value)
}

fn target_value(target: &RpcTarget) -> Value {
    match target {
        RpcTarget::Server { server_id } => Value::map([("serverId", text(server_id))]),
        RpcTarget::Session(session) => session_value(session),
    }
}

fn session_value(session: &SessionTarget) -> Value {
    Value::map([
        ("serverId", text(&session.server_id)),
        ("sessionId", text(&session.session_id)),
        ("attachmentId", text(&session.attachment_id)),
    ])
}

fn error_value(error: &ProtocolError) -> Value {
    Value::map([("code", text(&error.code)), ("message", text(&error.message))])
}

impl ClientMessage {
    pub fn to_value(&self) -> Value {
        match self {
            ClientMessage::Hello { version } => {
                Value::map([("type", text("hello")), ("version", Value::Int(*version))])
            }
            ClientMessage::Request { id, target, call } => Value::map([
                ("type", text("request")),
                ("id", text(id)),
                ("target", target_value(target)),
                ("call", call.clone()),
            ]),
            ClientMessage::Cancel { id, target } => Value::map([
                ("type", text("cancel")),
                ("id", text(id)),
                ("target", target_value(target)),
            ]),
        }
    }
}

impl ServerMessage {
    pub fn to_value(&self) -> Value {
        match self {
            ServerMessage::Hello { version, server_id } => Value::map([
                ("type", text("hello")),
                ("version", Value::Int(*version)),
                ("serverId", text(server_id)),
            ]),
            ServerMessage::HelloError { error } => {
                Value::map([("type", text("hello_error")), ("error", error_value(error))])
            }
            ServerMessage::Response { id, result: Ok(None) } => {
                Value::map([("type", text("response")), ("id", text(id)), ("ok", Value::Bool(true))])
            }
            ServerMessage::Response {
                id,
                result: Ok(Some(result)),
            } => Value::map([
                ("type", text("response")),
                ("id", text(id)),
                ("ok", Value::Bool(true)),
                ("result", result.clone()),
            ]),
            ServerMessage::Response { id, result: Err(error) } => Value::map([
                ("type", text("response")),
                ("id", text(id)),
                ("ok", Value::Bool(false)),
                ("error", error_value(error)),
            ]),
            ServerMessage::ServiceUpdate {
                subscription_id,
                update,
            } => Value::map([
                ("type", text("service_update")),
                ("subscriptionId", text(subscription_id)),
                ("update", update.clone()),
            ]),
            ServerMessage::Attachment { attachment } => Value::map([
                ("type", text("attachment")),
                ("attachment", attachment.as_ref().map_or(Value::Null, session_value)),
            ]),
        }
    }
}

/// A strict object view: exactly the `required` keys plus any of `optional`.
fn strict<'a>(value: &'a Value, required: &[&str], optional: &[&str]) -> Option<&'a [(String, Value)]> {
    let Value::Map(entries) = value else { return None };
    let known = |k: &str| required.contains(&k) || optional.contains(&k);
    (entries.iter().all(|(k, _)| known(k)) && required.iter().all(|k| value.get(k).is_some())).then_some(entries)
}

fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key)
}

fn id(value: &Value, key: &str) -> Option<String> {
    field(value, key)?.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn server_id(value: &Value) -> Option<String> {
    field(value, "serverId")?
        .as_str()
        .filter(|s| is_server_id(s))
        .map(str::to_owned)
}

fn is_json(value: &Value) -> bool {
    match value {
        Value::Bytes(_) => false,
        Value::Float(f) => f.is_finite(),
        Value::Array(items) => items.iter().all(is_json),
        Value::Map(entries) => entries.iter().all(|(_, v)| is_json(v)),
        _ => true,
    }
}

fn opaque(value: &Value, key: &str) -> Option<Value> {
    field(value, key).filter(|v| is_json(v)).cloned()
}

fn session_target(value: &Value) -> Option<SessionTarget> {
    strict(value, &["serverId", "sessionId", "attachmentId"], &[])?;
    Some(SessionTarget {
        server_id: server_id(value)?,
        session_id: id(value, "sessionId")?,
        attachment_id: id(value, "attachmentId")?,
    })
}

fn target(value: &Value) -> Option<RpcTarget> {
    if strict(value, &["serverId"], &[]).is_some() {
        return Some(RpcTarget::Server {
            server_id: server_id(value)?,
        });
    }
    session_target(value).map(RpcTarget::Session)
}

fn protocol_error(value: &Value) -> Option<ProtocolError> {
    strict(value, &["code", "message"], &[])?;
    Some(ProtocolError {
        code: id(value, "code")?,
        message: field(value, "message")?.as_str()?.to_owned(),
    })
}

fn integer(value: &Value) -> Option<i64> {
    match value {
        Value::Int(n) => Some(*n),
        Value::Float(f) if f.fract() == 0.0 => Some(*f as i64),
        _ => None,
    }
}

fn client_message(value: &Value) -> Option<ClientMessage> {
    match field(value, "type")?.as_str()? {
        "hello" => {
            strict(value, &["type", "version"], &[])?;
            let version = integer(field(value, "version")?).filter(|v| *v >= 0)?;
            Some(ClientMessage::Hello { version })
        }
        "request" => {
            strict(value, &["type", "id", "target", "call"], &[])?;
            Some(ClientMessage::Request {
                id: id(value, "id")?,
                target: target(field(value, "target")?)?,
                call: opaque(value, "call")?,
            })
        }
        "cancel" => {
            strict(value, &["type", "id", "target"], &[])?;
            Some(ClientMessage::Cancel {
                id: id(value, "id")?,
                target: target(field(value, "target")?)?,
            })
        }
        _ => None,
    }
}

fn server_message(value: &Value) -> Option<ServerMessage> {
    match field(value, "type")?.as_str()? {
        "hello" => {
            strict(value, &["type", "version", "serverId"], &[])?;
            let version = integer(field(value, "version")?).filter(|v| *v == PROTOCOL_VERSION)?;
            Some(ServerMessage::Hello {
                version,
                server_id: server_id(value)?,
            })
        }
        "hello_error" => {
            strict(value, &["type", "error"], &[])?;
            Some(ServerMessage::HelloError {
                error: protocol_error(field(value, "error")?)?,
            })
        }
        "response" => {
            let id = id(value, "id")?;
            match field(value, "ok")? {
                Value::Bool(true) => {
                    strict(value, &["type", "id", "ok"], &["result"])?;
                    let result = match field(value, "result") {
                        Some(_) => Some(opaque(value, "result")?),
                        None => None,
                    };
                    Some(ServerMessage::Response { id, result: Ok(result) })
                }
                Value::Bool(false) => {
                    strict(value, &["type", "id", "ok", "error"], &[])?;
                    Some(ServerMessage::Response {
                        id,
                        result: Err(protocol_error(field(value, "error")?)?),
                    })
                }
                _ => None,
            }
        }
        "service_update" => {
            strict(value, &["type", "subscriptionId", "update"], &[])?;
            Some(ServerMessage::ServiceUpdate {
                subscription_id: id(value, "subscriptionId")?,
                update: opaque(value, "update")?,
            })
        }
        "attachment" => {
            strict(value, &["type", "attachment"], &[])?;
            let attachment = match field(value, "attachment")? {
                Value::Null => None,
                other => Some(session_target(other)?),
            };
            Some(ServerMessage::Attachment { attachment })
        }
        _ => None,
    }
}

pub fn parse_client_message(value: &Value) -> Result<ClientMessage, ProtocolValidationError> {
    client_message(value).ok_or_else(|| ProtocolValidationError("Invalid client protocol message".into()))
}

pub fn parse_server_message(value: &Value) -> Result<ServerMessage, ProtocolValidationError> {
    server_message(value).ok_or_else(|| ProtocolValidationError("Invalid server protocol message".into()))
}

fn bounded(message: &str) -> String {
    let units: Vec<u16> = message.encode_utf16().collect();
    if units.len() <= 500 {
        return message.to_owned();
    }
    format!("{}...", String::from_utf16_lossy(&units[..497]))
}

fn options(max_frame_length: usize) -> CborOptions {
    CborOptions {
        max_byte_length: max_frame_length,
        ..CborOptions::default()
    }
}

fn encode(value: &Value, kind: &str, max_frame_length: usize) -> Result<Vec<u8>, ProtocolValidationError> {
    encode_cbor(value, &options(max_frame_length))
        .map_err(|e| e.0)
        .and_then(|payload| encode_frame(&payload).map_err(|e| e.0))
        .map_err(|e| ProtocolValidationError(format!("Unable to encode {kind} protocol message: {}", bounded(&e))))
}

/// Validates and encodes one complete length-prefixed client message.
pub fn encode_client_message(
    message: &ClientMessage,
    max_frame_length: usize,
) -> Result<Vec<u8>, ProtocolValidationError> {
    let value = message.to_value();
    parse_client_message(&value)?;
    encode(&value, "client", max_frame_length)
}

/// Validates and encodes one complete length-prefixed server message.
pub fn encode_server_message(
    message: &ServerMessage,
    max_frame_length: usize,
) -> Result<Vec<u8>, ProtocolValidationError> {
    let value = message.to_value();
    parse_server_message(&value)?;
    encode(&value, "server", max_frame_length)
}

struct Decoder {
    frames: FrameDecoder,
    kind: &'static str,
    max_frame_length: usize,
    failed: bool,
}

impl Decoder {
    fn new(kind: &'static str, max_frame_length: usize) -> Result<Self, ProtocolValidationError> {
        let frames = FrameDecoder::new(max_frame_length).map_err(|e| ProtocolValidationError(e.0))?;
        Ok(Self {
            frames,
            kind,
            max_frame_length,
            failed: false,
        })
    }

    fn push<T>(
        &mut self,
        chunk: &[u8],
        parse: fn(&Value) -> Result<T, ProtocolValidationError>,
    ) -> Result<Vec<T>, ProtocolValidationError> {
        if self.failed {
            return Err(ProtocolValidationError(format!(
                "{} message decoder has failed",
                self.kind
            )));
        }
        let result = self.frames.push(chunk).map_err(|e| e.0).and_then(|frames| {
            let mut messages = Vec::new();
            for frame in frames {
                let value = decode_cbor(&frame, &options(self.max_frame_length)).map_err(|e| e.0)?;
                match parse(&value) {
                    Ok(message) => messages.push(message),
                    Err(error) => return Ok(Err(error)),
                }
            }
            Ok(Ok(messages))
        });
        let outcome = match result {
            Ok(parsed) => parsed,
            Err(message) => Err(ProtocolValidationError(format!(
                "Invalid {} protocol frame: {}",
                self.kind,
                bounded(&message)
            ))),
        };
        self.failed |= outcome.is_err();
        outcome
    }

    fn end(&mut self) -> Result<(), ProtocolValidationError> {
        if self.failed {
            return Err(ProtocolValidationError(format!(
                "{} message decoder has failed",
                self.kind
            )));
        }
        self.frames.end().map_err(|e| {
            self.failed = true;
            ProtocolValidationError(format!("Invalid {} protocol framing: {}", self.kind, bounded(&e.0)))
        })
    }
}

/// Incrementally decodes and validates framed client messages.
pub struct ClientMessageDecoder(Decoder);

impl ClientMessageDecoder {
    pub fn new(max_frame_length: usize) -> Result<Self, ProtocolValidationError> {
        Decoder::new("client", max_frame_length).map(Self)
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ClientMessage>, ProtocolValidationError> {
        self.0.push(chunk, parse_client_message)
    }

    pub fn end(&mut self) -> Result<(), ProtocolValidationError> {
        self.0.end()
    }
}

impl Default for ClientMessageDecoder {
    fn default() -> Self {
        Self(Decoder {
            frames: FrameDecoder::default(),
            kind: "client",
            max_frame_length: DEFAULT_MAX_FRAME_LENGTH,
            failed: false,
        })
    }
}

/// Incrementally decodes and validates framed server messages.
pub struct ServerMessageDecoder(Decoder);

impl ServerMessageDecoder {
    pub fn new(max_frame_length: usize) -> Result<Self, ProtocolValidationError> {
        Decoder::new("server", max_frame_length).map(Self)
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<ServerMessage>, ProtocolValidationError> {
        self.0.push(chunk, parse_server_message)
    }

    pub fn end(&mut self) -> Result<(), ProtocolValidationError> {
        self.0.end()
    }
}

impl Default for ServerMessageDecoder {
    fn default() -> Self {
        Self(Decoder {
            frames: FrameDecoder::default(),
            kind: "server",
            max_frame_length: DEFAULT_MAX_FRAME_LENGTH,
            failed: false,
        })
    }
}
