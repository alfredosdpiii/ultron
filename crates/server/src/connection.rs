//! One client connection, sans I/O: bytes in, frames and close decisions out.
//!
//! Mirrors `Server.accept/receive/dispatchMessage/finishHandshake/handleRequest/failProtocol` for
//! server-scoped services. Calls are synchronous, so a request settles before the next message is
//! read; a `cancel` therefore never finds an active request (TS aborts in-flight async calls).

use protocol::{
    ClientMessage, ClientMessageDecoder, PROTOCOL_VERSION, ProtocolError, RpcTarget, ServerMessage,
    encode_server_message, is_server_id, is_supported_protocol_version,
};

use protocol::{ServiceCall, parse_service_call};

pub const INTERNAL_SERVER_ERROR_MESSAGE: &str = "Internal server error";
const MAX_UINT32: usize = 0xffff_ffff;

/// A failure raised by a service implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostError {
    /// A bounded, protocol-safe error (`ServerError` / `RemoteServiceError`): its code and message cross the wire.
    Coded(ProtocolError),
    /// Anything else: reported locally, sent as `internal_error`.
    Internal(String),
}

/// One connection's server-scoped service endpoint.
pub trait ServerServices {
    fn invoke(&mut self, call: &ServiceCall) -> Result<Option<protocol::Value>, HostError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    Send(Vec<u8>),
    /// Close the transport, optionally after one final frame.
    Close(Option<Vec<u8>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub server_id: String,
    pub max_frame_length: usize,
}

impl ServerConfig {
    pub fn new(server_id: &str, max_frame_length: usize) -> Result<Self, String> {
        if !is_server_id(server_id) {
            return Err("serverId must be a canonical lowercase UUIDv4".into());
        }
        if max_frame_length == 0 || max_frame_length > MAX_UINT32 {
            return Err(format!(
                "Server maxFrameLength must be an integer between 1 and {MAX_UINT32}"
            ));
        }
        Ok(Self {
            server_id: server_id.into(),
            max_frame_length,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    AwaitingHello,
    Ready,
    Closed,
}

pub struct Connection<S> {
    config: ServerConfig,
    decoder: ClientMessageDecoder,
    stage: Stage,
    services: S,
    /// Errors the TS server passes to `onError`.
    pub reported: Vec<String>,
}

impl<S: ServerServices> Connection<S> {
    pub fn new(config: ServerConfig, services: S) -> Self {
        let decoder = ClientMessageDecoder::new(config.max_frame_length).unwrap_or_default();
        Self {
            config,
            decoder,
            stage: Stage::AwaitingHello,
            services,
            reported: Vec::new(),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.stage == Stage::Ready
    }

    pub fn is_closed(&self) -> bool {
        self.stage == Stage::Closed
    }

    pub fn receive(&mut self, chunk: &[u8]) -> Vec<Output> {
        let mut out = Vec::new();
        if self.is_closed() {
            return out;
        }
        let messages = match self.decoder.push(chunk) {
            Ok(messages) => messages,
            Err(error) => {
                self.fail(ProtocolError::new("invalid_request", error.0), &mut out);
                return out;
            }
        };
        for message in messages {
            if self.is_closed() {
                break;
            }
            self.dispatch(message, &mut out);
        }
        out
    }

    /// The handshake timer fired before the connection became ready.
    pub fn handshake_timeout(&mut self) -> Vec<Output> {
        let mut out = Vec::new();
        if self.stage == Stage::AwaitingHello {
            self.fail(ProtocolError::new("invalid_request", "Handshake timeout"), &mut out);
        }
        out
    }

    /// The peer closed its side; a partial trailing frame is reported, not answered.
    pub fn transport_closed(&mut self) {
        if !self.is_closed()
            && let Err(error) = self.decoder.end()
        {
            self.reported.push(error.0);
        }
        self.stage = Stage::Closed;
    }

    fn dispatch(&mut self, message: ClientMessage, out: &mut Vec<Output>) {
        match (self.stage, message) {
            (Stage::AwaitingHello, ClientMessage::Hello { version }) => {
                if !is_supported_protocol_version(version) {
                    let message = format!("Unsupported protocol version {version}; expected {PROTOCOL_VERSION}");
                    return self.fail(ProtocolError::new("version", message), out);
                }
                let hello = ServerMessage::Hello {
                    version: PROTOCOL_VERSION,
                    server_id: self.config.server_id.clone(),
                };
                if self.send(&hello, out) {
                    self.stage = Stage::Ready;
                }
            }
            (Stage::AwaitingHello, _) => self.fail(
                ProtocolError::new("invalid_request", "The first client message must be hello"),
                out,
            ),
            (_, ClientMessage::Hello { .. }) => self.fail(
                ProtocolError::new("invalid_request", "hello may only be sent as the first message"),
                out,
            ),
            (_, ClientMessage::Cancel { .. }) => {}
            (_, ClientMessage::Request { id, target, call }) => {
                let result = match parse_service_call(&call) {
                    None => Err(ProtocolError::new("invalid_request", "Invalid service call")),
                    Some(call) => self.route(&target, &call),
                };
                self.send(&ServerMessage::Response { id, result }, out);
            }
        }
    }

    fn route(&mut self, target: &RpcTarget, call: &ServiceCall) -> Result<Option<protocol::Value>, ProtocolError> {
        if target.server_id() != self.config.server_id {
            return Err(ProtocolError::new(
                "wrong_server",
                "Request was addressed to another server",
            ));
        }
        if let RpcTarget::Session(_) = target {
            // ponytail: no session router yet, so no connection ever holds an attachment.
            return Err(ProtocolError::new(
                "session_not_attached",
                "Session is not attached to this client",
            ));
        }
        self.services.invoke(call).map_err(|error| match error {
            HostError::Coded(error) => error,
            HostError::Internal(message) => {
                self.reported.push(message);
                ProtocolError::new("internal_error", INTERNAL_SERVER_ERROR_MESSAGE)
            }
        })
    }

    fn send(&mut self, message: &ServerMessage, out: &mut Vec<Output>) -> bool {
        match encode_server_message(message, self.config.max_frame_length) {
            Ok(frame) => {
                out.push(Output::Send(frame));
                true
            }
            Err(error) => {
                self.reported.push(error.0);
                out.push(Output::Close(None));
                self.stage = Stage::Closed;
                false
            }
        }
    }

    fn fail(&mut self, error: ProtocolError, out: &mut Vec<Output>) {
        if self.is_closed() {
            return;
        }
        let final_frame =
            match encode_server_message(&ServerMessage::HelloError { error }, self.config.max_frame_length) {
                Ok(frame) => Some(frame),
                Err(error) => {
                    self.reported.push(error.0);
                    None
                }
            };
        out.push(Output::Close(final_frame));
        self.stage = Stage::Closed;
    }
}
