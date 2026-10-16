//! The experimental server's Radius relay host (`experimental/radius-relay.ts` `RadiusRelayHost`, and the credential
//! resolution of `radius-auth.ts`): one authenticated WebSocket to the Radius gateway at
//! `<gateway>/v1/session-relays/<serverId>/connect` (subprotocol `pi-session-relay.host.v1`), over which the gateway
//! multiplexes remote clients. Text messages are JSON control messages (`ping`/`pong`, `connection_open`,
//! `connection_close`); binary messages are data frames (version 1, type 1, the 16-byte connection UUID, payload).
//! Each opened connection is handed to the server's `accept` as an independent byte connection. The host reconnects
//! with exponential backoff (1 s doubling to 30 s), waits 30 s between checks while no Radius credential is available,
//! and reports its status (`not_authenticated`, `connecting`, `connected`, `retrying`).
//!
//! The host runs on a thread of its own with a single-threaded tokio runtime; connection sends may come from any
//! thread and are queued, in order, on the socket's writer.

use crate::radius_ws::{self, WsConnection, WsEvent, WsWriter};
use protocol::websocket::Message;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use tokio::sync::Notify;

pub const RADIUS_RELAY_HOST_SUBPROTOCOL: &str = "pi-session-relay.host.v1";
pub const RADIUS_RELAY_CLIENT_SUBPROTOCOL: &str = "pi-session-relay.client.v1";
/// `DEFAULT_RADIUS_GATEWAY`.
pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";
/// `ENV_RADIUS_GATEWAY`.
pub const ENV_RADIUS_GATEWAY: &str = "PI_RADIUS_GATEWAY";

const RELAY_DATA_HEADER_BYTES: usize = 18;
const RELAY_DATA_FRAME_VERSION: u8 = 1;
const RELAY_DATA_FRAME_TYPE: u8 = 1;
pub const MAX_PENDING_BYTES: usize = protocol::DEFAULT_MAX_FRAME_LENGTH * 4;
const LOCAL_PROTOCOL_ERROR_CLOSE_CODE: u16 = 4000;
const LOCAL_TRANSPORT_ERROR_CLOSE_CODE: u16 = 4001;

/// The host's retry delays in milliseconds (`HOST_RETRY_INITIAL_MS`, `HOST_RETRY_MAX_MS`, `MISSING_AUTH_RETRY_MS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryDelays {
    pub initial_ms: u64,
    pub max_ms: u64,
    pub missing_auth_ms: u64,
}

impl Default for RetryDelays {
    fn default() -> Self {
        RetryDelays {
            initial_ms: 1_000,
            max_ms: 30_000,
            missing_auth_ms: 30_000,
        }
    }
}

// ------------------------------------------------------------------------------------------------- the envelope

/// The connection ID pattern: a canonical lowercase UUIDv4.
pub fn is_connection_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    bytes.iter().enumerate().all(|(i, &b)| match i {
        8 | 13 | 18 | 23 => b == b'-',
        14 => b == b'4',
        19 => matches!(b, b'8' | b'9' | b'a' | b'b'),
        _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
    })
}

/// `encodeRelayDataFrame`.
pub fn encode_relay_data_frame(connection_id: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
    if !is_connection_id(connection_id) {
        return Err("Invalid Radius relay connection ID".into());
    }
    let hex = connection_id.replace('-', "");
    let mut frame = Vec::with_capacity(RELAY_DATA_HEADER_BYTES + payload.len());
    frame.push(RELAY_DATA_FRAME_VERSION);
    frame.push(RELAY_DATA_FRAME_TYPE);
    for index in 0..16 {
        frame.push(u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap_or_default());
    }
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// `parseRelayDataFrame`: the connection ID and payload, or `None` for anything that is not a version-1 data frame
/// of a valid connection ID.
pub fn parse_relay_data_frame(frame: &[u8]) -> Option<(String, Vec<u8>)> {
    if frame.len() < RELAY_DATA_HEADER_BYTES
        || frame[0] != RELAY_DATA_FRAME_VERSION
        || frame[1] != RELAY_DATA_FRAME_TYPE
    {
        return None;
    }
    let hex: String = frame[2..RELAY_DATA_HEADER_BYTES]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let id = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    is_connection_id(&id).then(|| (id, frame[RELAY_DATA_HEADER_BYTES..].to_vec()))
}

/// A control message from the gateway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostControl {
    Ping,
    Pong,
    ConnectionOpen(String),
    ConnectionClose(String, Option<u16>),
}

const INVALID_CONTROL: &str = "Invalid Radius relay control message";

/// `parseHostControlMessage`.
pub fn parse_host_control_message(text: &str) -> Result<HostControl, String> {
    let Ok(Value::Object(message)) = serde_json::from_str::<Value>(text) else {
        return Err(INVALID_CONTROL.into());
    };
    if message.get("version").and_then(Value::as_f64) != Some(1.0) {
        return Err("Unsupported Radius relay control version".into());
    }
    let kind = message.get("type").and_then(Value::as_str);
    match kind {
        Some("ping") => return Ok(HostControl::Ping),
        Some("pong") => return Ok(HostControl::Pong),
        _ => {}
    }
    let id = message.get("connection_id").and_then(Value::as_str);
    let code = match message.get("code") {
        None => Some(None),
        Some(value) => value
            .as_f64()
            .filter(|c| c.fract() == 0.0 && (1000.0..=4999.0).contains(c))
            .map(|c| Some(c as u16)),
    };
    match (kind, id, code) {
        (Some("connection_open"), Some(id), Some(_)) if is_connection_id(id) => {
            Ok(HostControl::ConnectionOpen(id.to_string()))
        }
        (Some("connection_close"), Some(id), Some(code)) if is_connection_id(id) => {
            Ok(HostControl::ConnectionClose(id.to_string(), code))
        }
        _ => Err(INVALID_CONTROL.into()),
    }
}

fn close_control(connection_id: &str, code: u16) -> String {
    json!({"version": 1, "type": "connection_close", "connection_id": connection_id, "code": code}).to_string()
}

/// `relayWebSocketUrl`.
pub fn relay_web_socket_url(gateway: &str, server_id: &str) -> Result<String, String> {
    let base = reqwest::Url::parse(gateway).map_err(|_| "Invalid URL".to_string())?;
    let mut url = base
        .join(&format!("/v1/session-relays/{server_id}/connect"))
        .map_err(|_| "Invalid URL".to_string())?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => return Err(format!("Unsupported Radius gateway protocol: {other}:")),
    };
    url.set_scheme(scheme).map_err(|_| "Invalid URL".to_string())?;
    Ok(url.to_string())
}

// ------------------------------------------------------------------------------------------------ credentials

/// A gateway and the bearer token for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayAuth {
    pub gateway: String,
    pub token: String,
}

/// Resolves Radius credentials anew for every connection attempt.
pub trait RelayAuthResolver: Send + Sync {
    /// The credential; `None` when there is none and none is `required`.
    fn resolve(&self, required: bool) -> Result<Option<RelayAuth>, String>;
}

/// An explicit credential (`--auth-token`, `--auth-token-file`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthInput {
    Token(String),
    File(PathBuf),
}

/// The stored Radius credential (`getAuthCredential(await runtime.getAuth("radius", { minOAuthValidityMs: 5 min }))`).
pub type StoredToken = Arc<dyn Fn() -> Result<Option<String>, String> + Send + Sync>;

/// `normalizeRadiusGatewayUrl`.
pub fn normalize_radius_gateway_url(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let with_scheme = if lower.starts_with("http://") || lower.starts_with("https://") {
        value.to_string()
    } else {
        format!("https://{value}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// `RadiusRelayAuthResolver`: offline mode (`PI_OFFLINE` set) has no relay; an explicit token (trimmed, never
/// empty) wins; else the stored credential.
pub struct RadiusRelayAuthResolver {
    input: Option<AuthInput>,
    gateway: String,
    offline: bool,
    stored: StoredToken,
}

impl RadiusRelayAuthResolver {
    pub fn new(input: Option<AuthInput>, gateway: &str, offline: bool, stored: StoredToken) -> Self {
        RadiusRelayAuthResolver {
            input,
            gateway: normalize_radius_gateway_url(gateway),
            offline,
            stored,
        }
    }

    pub fn gateway(&self) -> &str {
        &self.gateway
    }

    fn explicit_token(&self) -> Result<Option<String>, String> {
        let value = match &self.input {
            None => return Ok(None),
            Some(AuthInput::Token(token)) => token.clone(),
            Some(AuthInput::File(path)) => std::fs::read_to_string(path).map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    format!("ENOENT: no such file or directory, open '{}'", path.display())
                }
                std::io::ErrorKind::IsADirectory => "EISDIR: illegal operation on a directory, read".into(),
                _ => e.to_string(),
            })?,
        };
        let token = value.trim();
        if token.is_empty() {
            return Err("Radius authentication token must not be empty".into());
        }
        Ok(Some(token.to_string()))
    }
}

impl RelayAuthResolver for RadiusRelayAuthResolver {
    fn resolve(&self, required: bool) -> Result<Option<RelayAuth>, String> {
        if self.offline {
            if required {
                return Err("Radius relay connections are unavailable in offline mode".into());
            }
            return Ok(None);
        }
        let auth = |token| {
            Ok(Some(RelayAuth {
                gateway: self.gateway.clone(),
                token,
            }))
        };
        if let Some(token) = self.explicit_token()? {
            return auth(token);
        }
        if let Some(token) = (self.stored)()?.filter(|t| !t.is_empty()) {
            return auth(token);
        }
        if required {
            return Err("Radius authentication is required; start Pi and run /login radius, then retry".into());
        }
        Ok(None)
    }
}

// ------------------------------------------------------------------------------------------------------ status

/// `RadiusRelayHostStatus`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayHostStatus {
    NotAuthenticated,
    Connecting,
    Connected,
    Retrying(String),
}

impl RelayHostStatus {
    pub fn name(&self) -> &'static str {
        match self {
            RelayHostStatus::NotAuthenticated => "not_authenticated",
            RelayHostStatus::Connecting => "connecting",
            RelayHostStatus::Connected => "connected",
            RelayHostStatus::Retrying(_) => "retrying",
        }
    }
}

/// `reportRelayStatus` of the server command: the `Radius: …` line for a status, once per change; `connecting` is
/// never reported. `previous` is the last reported description.
pub fn relay_status_line(previous: &mut String, status: &RelayHostStatus) -> Option<String> {
    let description = match status {
        RelayHostStatus::Connected => "connected".to_string(),
        RelayHostStatus::NotAuthenticated => "not connected; local only".to_string(),
        RelayHostStatus::Retrying(error) => format!("reconnecting: {error}"),
        RelayHostStatus::Connecting => "connecting".to_string(),
    };
    if description == *previous || *status == RelayHostStatus::Connecting {
        return None;
    }
    *previous = description.clone();
    Some(format!("Radius: {description}"))
}

// ---------------------------------------------------------------------------------------------- connections

/// What the server does with one relayed connection's events (`server.accept`'s handler).
pub trait RelayConnectionHandler: Send {
    fn on_data(&mut self, chunk: &[u8]);
    fn on_close(&mut self);
    fn on_error(&mut self, error: &str);
}

/// `server.accept`: take a relayed connection, answer its handler.
pub type RelayAccept = Arc<dyn Fn(RelayConnection) -> Box<dyn RelayConnectionHandler> + Send + Sync>;

/// One relayed client connection (`RelayServerByteConnection`).
#[derive(Clone)]
pub struct RelayConnection {
    id: String,
    closed: Arc<AtomicBool>,
    shared: Weak<Shared>,
}

impl RelayConnection {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Queue `chunk` for the client.
    pub fn send(&self, chunk: &[u8]) -> Result<(), String> {
        if self.is_closed() {
            return Err("Radius relay connection is closed".into());
        }
        let shared = self.shared.upgrade().ok_or("Radius relay host is disconnected")?;
        if !shared.connections().contains_key(&self.id) {
            return Err("Radius relay connection is closed".into());
        }
        shared.send(Message::Binary(encode_relay_data_frame(&self.id, chunk)?))
    }

    /// Close from the server side, after an optional final chunk.
    pub fn close(&self, final_chunk: Option<&[u8]>) -> Result<(), String> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let Some(shared) = self.shared.upgrade() else {
            return Ok(());
        };
        if shared.connections().remove(&self.id).is_none() {
            return Ok(());
        }
        if let Some(chunk) = final_chunk {
            shared.send(Message::Binary(encode_relay_data_frame(&self.id, chunk)?))?;
        }
        shared.send(Message::Text(close_control(&self.id, 1000)))
    }
}

struct Active {
    connection: RelayConnection,
    handler: Arc<Mutex<Box<dyn RelayConnectionHandler>>>,
}

#[derive(Default)]
struct Shared {
    connections: Mutex<HashMap<String, Active>>,
    writer: Mutex<Option<WsWriter>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn connections(&self) -> MutexGuard<'_, HashMap<String, Active>> {
        lock(&self.connections)
    }

    fn send(&self, message: Message) -> Result<(), String> {
        match &*lock(&self.writer) {
            Some(writer) => writer.send(message),
            None => Err("Radius relay host is disconnected".into()),
        }
    }

    fn drop_connections(&self, error: Option<&str>) {
        let connections: Vec<Active> = self.connections().drain().map(|(_, active)| active).collect();
        for active in connections {
            active.connection.closed.store(true, Ordering::SeqCst);
            let mut handler = lock(&active.handler);
            match error {
                Some(error) => handler.on_error(error),
                None => handler.on_close(),
            }
        }
    }
}

// --------------------------------------------------------------------------------------------------- the host

/// A sleep the host awaits between attempts (tests make it instant).
pub type Sleep = Arc<dyn Fn(u64) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub struct RelayHostOptions {
    pub server_id: String,
    pub accept: RelayAccept,
    pub auth: Arc<dyn RelayAuthResolver>,
    pub on_status: Option<StatusSink>,
    pub delays: RetryDelays,
    /// Instead of a timer, for the retry delays.
    pub sleep: Option<Sleep>,
}

/// Where a relay host's statuses go.
pub type StatusSink = Arc<dyn Fn(&RelayHostStatus) + Send + Sync>;

/// What a server generation needs to run its relay host: the credential lookup and where statuses go.
#[derive(Clone)]
pub struct RelayConfig {
    pub auth: Arc<dyn RelayAuthResolver>,
    pub on_status: Option<StatusSink>,
}

impl std::fmt::Debug for RelayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayConfig").finish_non_exhaustive()
    }
}

impl RelayConfig {
    /// The relay host of the server `server_id`, whose connections go to its Unix socket `endpoint`.
    pub fn start(&self, server_id: &str, endpoint: PathBuf) -> RadiusRelayHost {
        RadiusRelayHost::start(RelayHostOptions {
            server_id: server_id.to_string(),
            accept: unix_accept(endpoint),
            auth: self.auth.clone(),
            on_status: self.on_status.clone(),
            delays: RetryDelays::default(),
            sleep: None,
        })
    }
}

/// `RadiusRelayHost`: started at once; [`RadiusRelayHost::close`] stops it.
pub struct RadiusRelayHost {
    closed: Arc<AtomicBool>,
    stop: Arc<Notify>,
    shared: Arc<Shared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl RadiusRelayHost {
    pub fn start(options: RelayHostOptions) -> RadiusRelayHost {
        let closed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(Notify::new());
        let shared = Arc::new(Shared::default());
        let run = Run {
            options,
            closed: closed.clone(),
            stop: stop.clone(),
            shared: shared.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("radius-relay-host".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
                    return;
                };
                runtime.block_on(run.run());
                // A credential lookup still running after close finishes on its own.
                runtime.shutdown_background();
            })
            .ok();
        RadiusRelayHost {
            closed,
            stop,
            shared,
            thread: Mutex::new(thread),
        }
    }

    /// `close()`: stop reconnecting, close the socket (`1000 Pi server stopped`), close every relayed connection,
    /// and wait for the host loop to end.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(writer) = lock(&self.shared.writer).as_ref() {
            writer.close_socket(1000, "Pi server stopped");
        }
        self.stop.notify_one();
        self.shared.drop_connections(None);
        if let Some(thread) = lock(&self.thread).take() {
            let _ = thread.join();
        }
    }
}

impl Drop for RadiusRelayHost {
    fn drop(&mut self) {
        self.close();
    }
}

struct Run {
    options: RelayHostOptions,
    closed: Arc<AtomicBool>,
    stop: Arc<Notify>,
    shared: Arc<Shared>,
}

impl Run {
    fn status(&self, status: RelayHostStatus) {
        if let Some(on_status) = &self.options.on_status {
            on_status(&status);
        }
    }

    fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Wait `ms` unless the host closes first (then `false`).
    async fn delay(&self, ms: u64) -> bool {
        let sleep = match &self.options.sleep {
            Some(sleep) => sleep(ms),
            None => Box::pin(tokio::time::sleep(std::time::Duration::from_millis(ms))),
        };
        if self.closed() {
            return false;
        }
        tokio::select! {
            () = sleep => !self.closed(),
            () = self.stop.notified() => false,
        }
    }

    async fn run(self) {
        let mut retry_ms = self.options.delays.initial_ms;
        while !self.closed() {
            let error = match self.attempt(&mut retry_ms).await {
                Attempt::Again => continue,
                Attempt::Stop => break,
                Attempt::Failed(error) => error,
            };
            if self.closed() {
                break;
            }
            self.status(RelayHostStatus::Retrying(error));
            if !self.delay(retry_ms).await {
                break;
            }
            retry_ms = (retry_ms * 2).min(self.options.delays.max_ms);
        }
    }

    async fn attempt(&self, retry_ms: &mut u64) -> Attempt {
        let auth = self.options.auth.clone();
        // A credential lookup may block (a token refresh); closing the host does not wait for it.
        let resolving = tokio::task::spawn_blocking(move || auth.resolve(false));
        let resolved = tokio::select! {
            resolved = resolving => match resolved {
                Ok(resolved) => resolved,
                Err(error) => Err(error.to_string()),
            },
            () = self.stop.notified() => return Attempt::Stop,
        };
        let auth = match resolved {
            Err(error) => return Attempt::Failed(error),
            Ok(None) => {
                self.status(RelayHostStatus::NotAuthenticated);
                return if self.delay(self.options.delays.missing_auth_ms).await {
                    Attempt::Again
                } else {
                    Attempt::Stop
                };
            }
            Ok(Some(auth)) => auth,
        };
        self.status(RelayHostStatus::Connecting);
        let url = match relay_web_socket_url(&auth.gateway, &self.options.server_id) {
            Ok(url) => url,
            Err(error) => return Attempt::Failed(error),
        };
        let authorization = format!("Bearer {}", auth.token);
        let connecting = radius_ws::connect(&url, RADIUS_RELAY_HOST_SUBPROTOCOL, &authorization, MAX_PENDING_BYTES);
        let socket = tokio::select! {
            socket = connecting => socket,
            () = self.stop.notified() => return Attempt::Stop,
        };
        let socket = match socket {
            Ok(socket) => socket,
            Err(error) => return Attempt::Failed(error),
        };
        if self.closed() {
            socket.writer.close_socket(1000, "Pi server stopped");
            socket.finish().await;
            return Attempt::Stop;
        }
        *retry_ms = self.options.delays.initial_ms;
        self.status(RelayHostStatus::Connected);
        match self.serve(socket).await {
            Err(error) => Attempt::Failed(error),
            Ok(()) if self.closed() => Attempt::Stop,
            Ok(()) => Attempt::Failed("Radius relay host disconnected".into()),
        }
    }

    /// `#serve`: handle the gateway's messages until the socket closes; `Ok` for a normal (1000) closure.
    async fn serve(&self, mut socket: WsConnection) -> Result<(), String> {
        *lock(&self.shared.writer) = Some(socket.writer.clone());
        let result = loop {
            let event = tokio::select! {
                event = socket.reader.next() => event,
                () = self.stop.notified() => break Ok(()),
            };
            match event {
                WsEvent::Text(text) => {
                    if let Err(error) = self.handle_text(&text) {
                        socket
                            .writer
                            .close_socket(LOCAL_PROTOCOL_ERROR_CLOSE_CODE, "Radius relay protocol error");
                        break Err(error);
                    }
                }
                WsEvent::Binary(bytes) => {
                    if let Err(error) = self.handle_data(&bytes) {
                        socket
                            .writer
                            .close_socket(LOCAL_PROTOCOL_ERROR_CLOSE_CODE, "Radius relay protocol error");
                        break Err(error);
                    }
                }
                WsEvent::Closed { code: 1000, .. } => break Ok(()),
                WsEvent::Closed { code, reason } => {
                    break Err(if reason.is_empty() {
                        format!("Radius relay host closed ({code})")
                    } else {
                        format!("Radius relay host closed ({code}: {reason})")
                    });
                }
                WsEvent::Error(error) => break Err(error),
            }
        };
        socket.writer.close();
        *lock(&self.shared.writer) = None;
        self.shared.drop_connections(result.as_ref().err().map(String::as_str));
        socket.finish().await;
        result
    }

    fn handle_text(&self, text: &str) -> Result<(), String> {
        match parse_host_control_message(text)? {
            HostControl::Ping => {
                let pong = json!({"version": 1, "type": "pong"}).to_string();
                if self.shared.send(Message::Text(pong)).is_err()
                    && let Some(writer) = lock(&self.shared.writer).as_ref()
                {
                    writer.close_socket(LOCAL_TRANSPORT_ERROR_CLOSE_CODE, "Radius relay send failed");
                }
                Ok(())
            }
            HostControl::Pong => Ok(()),
            HostControl::ConnectionOpen(id) => self.open_connection(id),
            HostControl::ConnectionClose(id, _) => {
                let active = self.shared.connections().remove(&id);
                if let Some(active) = active {
                    active.connection.closed.store(true, Ordering::SeqCst);
                    lock(&active.handler).on_close();
                }
                Ok(())
            }
        }
    }

    fn handle_data(&self, bytes: &[u8]) -> Result<(), String> {
        let (id, payload) = parse_relay_data_frame(bytes).ok_or("Invalid Radius relay data frame")?;
        let handler = self.shared.connections().get(&id).map(|a| a.handler.clone());
        match handler {
            Some(handler) => lock(&handler).on_data(&payload),
            None => {
                let _ = self.shared.send(Message::Text(close_control(&id, 1000)));
            }
        }
        Ok(())
    }

    fn open_connection(&self, id: String) -> Result<(), String> {
        if self.shared.connections().contains_key(&id) {
            return Err("Radius relay reused a connection ID".into());
        }
        let connection = RelayConnection {
            id: id.clone(),
            closed: Arc::new(AtomicBool::new(false)),
            shared: Arc::downgrade(&self.shared),
        };
        let handler = (self.options.accept)(connection.clone());
        if connection.is_closed() {
            let _ = self.shared.send(Message::Text(close_control(&id, 1012)));
        } else {
            self.shared.connections().insert(
                id,
                Active {
                    connection,
                    handler: Arc::new(Mutex::new(handler)),
                },
            );
        }
        Ok(())
    }
}

enum Attempt {
    Again,
    Stop,
    Failed(String),
}

// ------------------------------------------------------------------------------------------ the local bridge

struct UnixBridge {
    stream: std::os::unix::net::UnixStream,
    tx: Option<std::sync::mpsc::Sender<Vec<u8>>>,
}

impl RelayConnectionHandler for UnixBridge {
    fn on_data(&mut self, chunk: &[u8]) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(chunk.to_vec());
        }
    }

    fn on_close(&mut self) {
        self.tx = None;
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    fn on_error(&mut self, _error: &str) {
        self.on_close();
    }
}

struct Refused;

impl RelayConnectionHandler for Refused {
    fn on_data(&mut self, _chunk: &[u8]) {}
    fn on_close(&mut self) {}
    fn on_error(&mut self, _error: &str) {}
}

/// `server.accept` for a server listening on a Unix socket: each relayed connection becomes a local connection to
/// `endpoint`, bytes copied both ways; a connection the server refuses is closed (the gateway hears code 1012).
pub fn unix_accept(endpoint: PathBuf) -> RelayAccept {
    Arc::new(move |connection: RelayConnection| -> Box<dyn RelayConnectionHandler> {
        let streams = std::os::unix::net::UnixStream::connect(&endpoint).and_then(|s| {
            let reader = s.try_clone()?;
            let writer = s.try_clone()?;
            Ok((s, reader, writer))
        });
        let Ok((stream, mut reader, mut writer)) = streams else {
            let _ = connection.close(None);
            return Box::new(Refused);
        };
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            use std::io::Write;
            for chunk in rx {
                if writer.write_all(&chunk).is_err() {
                    break;
                }
            }
        });
        let outbound = connection.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        if outbound.send(&buffer[..n]).is_err() {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let _ = outbound.close(None);
        });
        Box::new(UnixBridge { stream, tx: Some(tx) })
    })
}
