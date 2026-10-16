//! Port of `packages/client`: a blocking client over any ordered byte stream (handshake, request
//! correlation, attachment routing updates) and discovery of local servers by probing
//! `<serverId>.sock` entries in a server directory.

pub mod radius;
pub mod runtime;

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use protocol::{
    ClientMessage, DEFAULT_MAX_FRAME_LENGTH, PROTOCOL_VERSION, ProtocolError, RpcTarget, ServerMessage,
    ServerMessageDecoder, SessionTarget, Value, encode_client_message, is_server_id, parse_service_call,
};

pub const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1_000);

#[derive(Debug)]
pub enum ClientError {
    /// A bounded error sent by the server (`hello_error` or a failed response).
    Server(ProtocolError),
    /// The peer violated the protocol; the connection is closed.
    Validation(String),
    /// The connection is gone.
    Disconnected(String),
    /// The call is not a strict service call; nothing was sent.
    InvalidCall,
    Io(io::Error),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Server(error) => write!(f, "{}: {}", error.code, error.message),
            ClientError::Validation(message) | ClientError::Disconnected(message) => f.write_str(message),
            ClientError::InvalidCall => f.write_str("Invalid service call"),
            ClientError::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ClientError {}

pub struct Client<T> {
    stream: T,
    server_id: String,
    decoder: ServerMessageDecoder,
    queued: VecDeque<ServerMessage>,
    sequence: u64,
    attachment: Option<SessionTarget>,
    connected: bool,
}

impl<T: Read + Write> Client<T> {
    /// Send `hello` and wait for the server's; the server must be `server_id`.
    pub fn connect(stream: T, server_id: &str) -> Result<Self, ClientError> {
        if !is_server_id(server_id) {
            return Err(ClientError::Validation(
                "serverId must be a canonical lowercase UUIDv4".into(),
            ));
        }
        let mut client = Self {
            stream,
            server_id: server_id.into(),
            decoder: ServerMessageDecoder::default(),
            queued: VecDeque::new(),
            sequence: 0,
            attachment: None,
            connected: true,
        };
        client.send(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })?;
        let error = match client.next()? {
            ServerMessage::HelloError { error } => ClientError::Server(error),
            ServerMessage::Hello { server_id, .. } if server_id == client.server_id => return Ok(client),
            ServerMessage::Hello { server_id, .. } => ClientError::Validation(format!(
                "Connected server {server_id:?} does not match {:?}",
                client.server_id
            )),
            _ => ClientError::Validation("Expected server hello as first message".into()),
        };
        client.connected = false;
        Err(error)
    }

    pub fn attachment(&self) -> Option<&SessionTarget> {
        self.attachment.as_ref()
    }

    /// One service call; out-of-band attachment updates seen while waiting are applied.
    pub fn request(&mut self, target: RpcTarget, call: &Value) -> Result<Option<Value>, ClientError> {
        if !self.connected {
            return Err(ClientError::Disconnected("Client is disconnected".into()));
        }
        // The ID is consumed even when the call is rejected locally, like TS.
        self.sequence += 1;
        if parse_service_call(call).is_none() {
            return Err(ClientError::InvalidCall);
        }
        let id = format!("request-{}", self.sequence);
        self.send(&ClientMessage::Request {
            id: id.clone(),
            target,
            call: call.clone(),
        })?;
        loop {
            match self.next()? {
                ServerMessage::Response {
                    id: response_id,
                    result,
                } if response_id == id => {
                    return result.map_err(ClientError::Server);
                }
                ServerMessage::Response { .. } => return Err(self.fail("Response has no matching request")),
                ServerMessage::Hello { .. } | ServerMessage::HelloError { .. } => {
                    return Err(self.fail("Unexpected handshake message"));
                }
                ServerMessage::Attachment { attachment } => {
                    if attachment.as_ref().is_some_and(|a| a.server_id != self.server_id) {
                        return Err(self.fail("Attachment update belongs to another server"));
                    }
                    self.attachment = attachment;
                }
                // ponytail: service subscriptions are not ported; their updates are dropped.
                ServerMessage::ServiceUpdate { .. } => {}
            }
        }
    }

    fn fail(&mut self, message: &str) -> ClientError {
        self.connected = false;
        ClientError::Validation(message.into())
    }

    fn send(&mut self, message: &ClientMessage) -> Result<(), ClientError> {
        let frame =
            encode_client_message(message, DEFAULT_MAX_FRAME_LENGTH).map_err(|e| ClientError::Validation(e.0))?;
        self.stream.write_all(&frame).map_err(|e| {
            self.connected = false;
            ClientError::Disconnected(e.to_string())
        })
    }

    fn next(&mut self) -> Result<ServerMessage, ClientError> {
        let mut buffer = [0u8; 64 * 1024];
        loop {
            if let Some(message) = self.queued.pop_front() {
                return Ok(message);
            }
            let read = match self.stream.read(&mut buffer) {
                Ok(read) => read,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.connected = false;
                    return Err(ClientError::Io(e));
                }
            };
            if read == 0 {
                self.connected = false;
                return Err(match self.decoder.end() {
                    Err(error) => ClientError::Validation(error.0),
                    Ok(()) => ClientError::Disconnected("Byte transport closed".into()),
                });
            }
            match self.decoder.push(&buffer[..read]) {
                Ok(messages) => self.queued.extend(messages),
                Err(error) => return Err(self.fail(&error.0)),
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnixServerRoute {
    pub server_id: String,
    pub path: PathBuf,
}

/// Discover reachable local servers by probing server-addressed Unix sockets, in server ID order.
/// Missing, refused, timed-out, version-mismatched or non-conforming endpoints are omitted, never deleted.
pub fn discover_unix_servers(directory: &Path, timeout: Duration) -> io::Result<Vec<UnixServerRoute>> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut routes = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(server_id) = name.to_str().and_then(|n| n.strip_suffix(".sock")) else {
            continue;
        };
        if !is_server_id(server_id) {
            continue;
        }
        let path = entry.path();
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_socket() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
        if probe(&path, server_id, timeout)? {
            routes.push(UnixServerRoute {
                server_id: server_id.into(),
                path,
            });
        }
    }
    routes.sort_by(|a, b| a.server_id.cmp(&b.server_id));
    Ok(routes)
}

fn probe(path: &Path, server_id: &str, timeout: Duration) -> io::Result<bool> {
    let stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(e) if is_absent(&e) => return Ok(false),
        Err(e) => return Err(e),
    };
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    match Client::connect(stream, server_id) {
        Ok(_) => Ok(true),
        Err(ClientError::Validation(_) | ClientError::Disconnected(_)) => Ok(false),
        Err(ClientError::Server(error)) if error.code == "version" => Ok(false),
        Err(ClientError::Io(e)) if is_absent(&e) || matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            Ok(false)
        }
        Err(ClientError::Io(e)) => Err(e),
        Err(other) => Err(io::Error::other(other.to_string())),
    }
}

fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::NotFound | ErrorKind::ConnectionRefused | ErrorKind::ConnectionReset | ErrorKind::BrokenPipe
    )
}
