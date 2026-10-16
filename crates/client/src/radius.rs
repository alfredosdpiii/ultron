//! The client side of the Radius relay (`experimental/radius-relay.ts`): the byte transport to a server reached
//! through the Radius gateway (`createRadiusClientTransportFactory`, `RadiusClientByteTransport`: one authenticated
//! WebSocket offering `pi-session-relay.client.v1`, binary messages carrying the protocol's bytes), a blocking
//! `Read + Write` stream over it for [`crate::Client`], the reconnector that brings an established Radius client back
//! and re-attaches its last Session (`RadiusClientReconnect`), and the statuses the client TUI shows meanwhile
//! (`client-tui.ts` `#handleConnectionState` / `#handleAttachmentState`).

use protocol::websocket::Message;
use server::radius_relay::{
    MAX_PENDING_BYTES, RADIUS_RELAY_CLIENT_SUBPROTOCOL, RelayAuthResolver, relay_web_socket_url,
};
use server::radius_ws::{self, WsEvent, WsWriter};
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

const CLIENT_RETRY_INITIAL_MS: u64 = 1_000;
const CLIENT_RETRY_MAX_MS: u64 = 30_000;
const LOCAL_TRANSPORT_ERROR_CLOSE_CODE: u16 = 4001;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// `ByteTransportHandlers`.
pub trait ByteTransportHandlers: Send {
    fn on_data(&mut self, chunk: &[u8]);
    fn on_close(&mut self);
    fn on_error(&mut self, error: &str);
}

/// `RadiusClientByteTransport`: established by [`connect_radius_client_transport`].
pub struct RadiusClientTransport {
    writer: WsWriter,
    closed: Arc<AtomicBool>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl RadiusClientTransport {
    pub fn send(&self, chunk: &[u8]) -> Result<(), String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("Radius relay client is closed".into());
        }
        self.writer.send(Message::Binary(chunk.to_vec()))
    }

    /// `close()`: `1000 Pi client closed`; no handler is called.
    pub fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.writer.close_socket(1000, "Pi client closed");
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

impl Drop for RadiusClientTransport {
    fn drop(&mut self) {
        self.close();
        if let Some(thread) = lock(&self.thread).take() {
            let _ = thread.join();
        }
    }
}

/// `createRadiusClientTransportFactory(...)(handlers)`: resolve the required credential, open the socket to
/// `server_id` through the gateway, and deliver its events to `handlers` on a thread of the transport's own.
pub fn connect_radius_client_transport(
    server_id: &str,
    auth: &dyn RelayAuthResolver,
    mut handlers: Box<dyn ByteTransportHandlers>,
) -> Result<RadiusClientTransport, String> {
    let auth = auth.resolve(true)?.ok_or("Radius authentication is required")?;
    let url = relay_web_socket_url(&auth.gateway, server_id)?;
    let authorization = format!("Bearer {}", auth.token);
    let (opened_tx, opened_rx) = mpsc::channel();
    let closed = Arc::new(AtomicBool::new(false));
    let thread_closed = closed.clone();
    let thread = std::thread::Builder::new()
        .name("radius-relay-client".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = opened_tx.send(Err(error.to_string()));
                    return;
                }
            };
            runtime.block_on(async move {
                let socket =
                    radius_ws::connect(&url, RADIUS_RELAY_CLIENT_SUBPROTOCOL, &authorization, MAX_PENDING_BYTES).await;
                let mut socket = match socket {
                    Ok(socket) => socket,
                    Err(error) => {
                        let _ = opened_tx.send(Err(error));
                        return;
                    }
                };
                let _ = opened_tx.send(Ok(socket.writer.clone()));
                loop {
                    match socket.reader.next().await {
                        WsEvent::Binary(bytes) => {
                            if !thread_closed.load(Ordering::SeqCst) {
                                handlers.on_data(&bytes);
                            }
                        }
                        WsEvent::Text(_) => {
                            if !thread_closed.swap(true, Ordering::SeqCst) {
                                socket
                                    .writer
                                    .close_socket(LOCAL_TRANSPORT_ERROR_CLOSE_CODE, "Radius relay transport error");
                                handlers.on_error("Radius relay client received a non-binary message");
                            }
                            break;
                        }
                        WsEvent::Closed { .. } => {
                            if !thread_closed.swap(true, Ordering::SeqCst) {
                                handlers.on_close();
                            }
                            break;
                        }
                        WsEvent::Error(error) => {
                            if !thread_closed.swap(true, Ordering::SeqCst) {
                                socket
                                    .writer
                                    .close_socket(LOCAL_TRANSPORT_ERROR_CLOSE_CODE, "Radius relay transport error");
                                handlers.on_error(&error);
                            }
                            break;
                        }
                    }
                }
                socket.writer.close();
                socket.finish().await;
            });
        })
        .map_err(|e| e.to_string())?;
    let writer = match opened_rx.recv() {
        Ok(Ok(writer)) => writer,
        Ok(Err(error)) => {
            let _ = thread.join();
            return Err(error);
        }
        Err(_) => {
            let _ = thread.join();
            return Err(radius_ws::CONNECTION_FAILED.into());
        }
    };
    Ok(RadiusClientTransport {
        writer,
        closed,
        thread: Mutex::new(Some(thread)),
    })
}

// ------------------------------------------------------------------------------------------- a blocking stream

enum Incoming {
    Data(Vec<u8>),
    Closed,
    Error(String),
}

struct Forward(mpsc::Sender<Incoming>);

impl ByteTransportHandlers for Forward {
    fn on_data(&mut self, chunk: &[u8]) {
        let _ = self.0.send(Incoming::Data(chunk.to_vec()));
    }
    fn on_close(&mut self) {
        let _ = self.0.send(Incoming::Closed);
    }
    fn on_error(&mut self, error: &str) {
        let _ = self.0.send(Incoming::Error(error.to_string()));
    }
}

/// The transport as a blocking byte stream: reads end (`Ok(0)`) when the relay closes, fail with the transport's
/// error, and writes queue on the socket. [`crate::Client::connect`] runs the protocol over it.
pub struct RadiusByteStream {
    transport: RadiusClientTransport,
    incoming: mpsc::Receiver<Incoming>,
    leftover: Vec<u8>,
    ended: Option<Result<(), String>>,
}

impl RadiusByteStream {
    pub fn connect(server_id: &str, auth: &dyn RelayAuthResolver) -> Result<RadiusByteStream, String> {
        let (tx, incoming) = mpsc::channel();
        let transport = connect_radius_client_transport(server_id, auth, Box::new(Forward(tx)))?;
        Ok(RadiusByteStream {
            transport,
            incoming,
            leftover: Vec::new(),
            ended: None,
        })
    }

    pub fn transport(&self) -> &RadiusClientTransport {
        &self.transport
    }

    /// The stream's read and write halves, for [`crate::runtime::Connection::connect_over`].
    pub fn split(self) -> (RadiusByteReader, RadiusByteWriter) {
        (
            RadiusByteReader {
                incoming: self.incoming,
                leftover: self.leftover,
                ended: self.ended,
            },
            RadiusByteWriter(self.transport),
        )
    }
}

/// The read half of a [`RadiusByteStream`]: reads end (`Ok(0)`) when the relay closes (or the write half closed
/// the transport), fail with the transport's error.
pub struct RadiusByteReader {
    incoming: mpsc::Receiver<Incoming>,
    leftover: Vec<u8>,
    ended: Option<Result<(), String>>,
}

impl Read for RadiusByteReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_incoming(&self.incoming, &mut self.leftover, &mut self.ended, buf)
    }
}

/// The write half of a [`RadiusByteStream`]; shutting it down closes the transport (`1000 Pi client closed`).
pub struct RadiusByteWriter(RadiusClientTransport);

impl Write for RadiusByteWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.send(buf).map_err(io::Error::other)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl crate::runtime::ConnectionWriter for RadiusByteWriter {
    fn shutdown(&mut self) {
        self.0.close();
    }
}

fn read_incoming(
    incoming: &mpsc::Receiver<Incoming>,
    leftover: &mut Vec<u8>,
    ended: &mut Option<Result<(), String>>,
    buf: &mut [u8],
) -> io::Result<usize> {
    while leftover.is_empty() {
        if let Some(end) = ended {
            return match end {
                Ok(()) => Ok(0),
                Err(error) => Err(io::Error::other(error.clone())),
            };
        }
        match incoming.recv() {
            Ok(Incoming::Data(chunk)) => *leftover = chunk,
            Ok(Incoming::Closed) | Err(_) => *ended = Some(Ok(())),
            Ok(Incoming::Error(error)) => *ended = Some(Err(error)),
        }
    }
    let n = buf.len().min(leftover.len());
    buf[..n].copy_from_slice(&leftover[..n]);
    leftover.drain(..n);
    Ok(n)
}

impl Read for RadiusByteStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_incoming(&self.incoming, &mut self.leftover, &mut self.ended, buf)
    }
}

impl Write for RadiusByteStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.transport.send(buf).map_err(io::Error::other)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ------------------------------------------------------------------------------------------------- reconnecting

/// `ClientConnectionState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Disconnected,
}

/// What [`RadiusClientReconnect`] needs of a client (`RadiusReconnectClient`).
pub trait ReconnectClient: Send + Sync {
    fn connected(&self) -> bool;
    fn connection_state(&self) -> ConnectionState;
    fn disconnect(&self, reason: &str);
    fn reconnect(&self) -> Result<(), String>;
}

/// Stops a reconnector's waits.
#[derive(Default)]
pub struct Cancellation {
    cancelled: Mutex<bool>,
    changed: Condvar,
}

impl Cancellation {
    pub fn cancel(&self) {
        *lock(&self.cancelled) = true;
        self.changed.notify_all();
    }

    pub fn is_cancelled(&self) -> bool {
        *lock(&self.cancelled)
    }

    /// Wait `duration`; `false` when cancelled first.
    pub fn wait(&self, duration: Duration) -> bool {
        let guard = lock(&self.cancelled);
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, duration, |cancelled| !*cancelled)
            .unwrap_or_else(|e| e.into_inner());
        !*guard
    }
}

/// The delay before each reconnection attempt (milliseconds); `false` stops the reconnector.
pub type ReconnectSleep = Arc<dyn Fn(u64, &Cancellation) -> bool + Send + Sync>;
/// Re-attach the Session (`reconnectManagement.attach(sessionId)`).
pub type Reattach = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

struct ReconnectState {
    desired_session: Option<String>,
    reconnecting: Option<std::thread::JoinHandle<()>>,
    running: bool,
}

struct ReconnectInner {
    client: Arc<dyn ReconnectClient>,
    reattach: Reattach,
    sleep: ReconnectSleep,
    state: Mutex<ReconnectState>,
    disposed: AtomicBool,
    cancel: Cancellation,
}

/// `RadiusClientReconnect`: when the established client disconnects, reconnect after 1 s (doubling to 30 s per
/// failure) and re-attach the last selected Session. The owner reports the client's attachment and connection-state
/// changes ([`RadiusClientReconnect::attachment_changed`], [`RadiusClientReconnect::connection_state_changed`]).
pub struct RadiusClientReconnect {
    inner: Arc<ReconnectInner>,
}

impl RadiusClientReconnect {
    pub fn new(
        client: Arc<dyn ReconnectClient>,
        attached_session: Option<String>,
        reattach: Reattach,
        sleep: Option<ReconnectSleep>,
    ) -> RadiusClientReconnect {
        let sleep =
            sleep.unwrap_or_else(|| Arc::new(|ms, cancel: &Cancellation| cancel.wait(Duration::from_millis(ms))));
        RadiusClientReconnect {
            inner: Arc::new(ReconnectInner {
                client,
                reattach,
                sleep,
                state: Mutex::new(ReconnectState {
                    desired_session: attached_session,
                    reconnecting: None,
                    running: false,
                }),
                disposed: AtomicBool::new(false),
                cancel: Cancellation::default(),
            }),
        }
    }

    /// `onAttachmentChange`: remember the Session to restore; a detach while connected forgets it.
    pub fn attachment_changed(&self, session_id: Option<&str>) {
        match session_id {
            Some(id) => lock(&self.inner.state).desired_session = Some(id.to_string()),
            None if self.inner.client.connected() => lock(&self.inner.state).desired_session = None,
            None => {}
        }
    }

    /// `onConnectionStateChange`: a disconnect starts reconnecting (once at a time).
    pub fn connection_state_changed(&self, state: ConnectionState) {
        if state != ConnectionState::Disconnected || self.inner.disposed.load(Ordering::SeqCst) {
            return;
        }
        let mut guard = lock(&self.inner.state);
        if guard.running {
            return;
        }
        guard.running = true;
        if let Some(previous) = guard.reconnecting.take() {
            drop(guard);
            let _ = previous.join();
            guard = lock(&self.inner.state);
        }
        let inner = self.inner.clone();
        guard.reconnecting = Some(std::thread::spawn(move || {
            inner.run();
            lock(&inner.state).running = false;
        }));
    }

    /// Wait for a reconnection under way to end (succeed, or stop).
    pub fn wait_idle(&self) {
        let thread = lock(&self.inner.state).reconnecting.take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }

    /// `dispose()`: stop reconnecting and disconnect the client unless it already is.
    pub fn dispose(&self) {
        if self.inner.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.cancel.cancel();
        if self.inner.client.connection_state() != ConnectionState::Disconnected {
            self.inner.client.disconnect("Radius reconnect stopped");
        }
        let thread = lock(&self.inner.state).reconnecting.take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl ReconnectInner {
    fn disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst) || self.cancel.is_cancelled()
    }

    fn run(&self) {
        let mut retry_ms = CLIENT_RETRY_INITIAL_MS;
        while !self.disposed() && !self.client.connected() {
            if !(self.sleep)(retry_ms, &self.cancel) || self.disposed() {
                return;
            }
            let attempt = self.client.reconnect().and_then(|()| {
                let session = lock(&self.state).desired_session.clone();
                match session {
                    Some(session) => (self.reattach)(&session),
                    None => Ok(()),
                }
            });
            match attempt {
                Ok(()) => return,
                Err(error) => {
                    if self.disposed() {
                        return;
                    }
                    if self.client.connected() {
                        self.client.disconnect(&error);
                    }
                    retry_ms = (retry_ms * 2).min(CLIENT_RETRY_MAX_MS);
                }
            }
        }
    }
}

// ----------------------------------------------------------------------------------------------- TUI statuses

/// What the client TUI shows for its selected Radius server (`#busy`, `#status`, and whether the Session's lane is
/// closed while recovering).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryView {
    pub busy: bool,
    pub status: &'static str,
    pub close_lane: bool,
}

/// `#handleConnectionState` of the selected server; `None` leaves the view as it is.
pub fn connection_state_view(state: ConnectionState, lane_open: bool) -> Option<RecoveryView> {
    match state {
        ConnectionState::Connected if lane_open => None,
        ConnectionState::Connected => Some(RecoveryView {
            busy: true,
            status: "Reattaching Session…",
            close_lane: false,
        }),
        ConnectionState::Connecting => Some(RecoveryView {
            busy: true,
            status: "Reconnecting to Radius…",
            close_lane: true,
        }),
        ConnectionState::Disconnected => Some(RecoveryView {
            busy: true,
            status: "Radius disconnected; retrying…",
            close_lane: true,
        }),
    }
}

/// A Session attachment state of the selected server (`SessionAttachmentState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentState {
    Attaching(String),
    Attached(String),
    Detached,
}

/// `#handleAttachmentState` for the TUI's Session `session_id`: attached again ends the recovery (the lane reopens),
/// attaching shows `Reattaching Session…`; `None` leaves the view as it is.
pub fn attachment_state_view(state: &AttachmentState, session_id: &str) -> Option<RecoveryView> {
    match state {
        AttachmentState::Attached(id) if id == session_id => Some(RecoveryView {
            busy: false,
            status: "",
            close_lane: false,
        }),
        AttachmentState::Attaching(id) if id == session_id => Some(RecoveryView {
            busy: true,
            status: "Reattaching Session…",
            close_lane: false,
        }),
        _ => None,
    }
}
