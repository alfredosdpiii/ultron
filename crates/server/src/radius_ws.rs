//! The Radius relay's WebSocket client, as the TypeScript relay uses undici's `WebSocket`: an HTTP/1.1 upgrade
//! (`ws:`/`wss:` over reqwest, no proxy, no extensions) offering one subprotocol and an `authorization` header, then
//! RFC 6455 messages ([`protocol::websocket`]). Every handshake failure (refused, a status other than 101, a wrong
//! `Sec-WebSocket-Accept`, a missing or different subprotocol) is undici's detail-less failure, which the relay
//! reports as `Radius WebSocket connection failed`. A connection lost without a close frame is an error event
//! (undici's), then a 1006 closure. Pings are answered; sends go through one ordered writer with the relay's
//! pending-byte limit (`OrderedWebSocketWriter`).

use protocol::websocket::{Message, MessageDecoder, accept_key, base64, encode_message};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::mpsc;

/// `webSocketError` for an undici error event without details.
pub const CONNECTION_FAILED: &str = "Radius WebSocket connection failed";
/// The largest message the relay reads (a protocol frame plus the data envelope, generously).
const MAX_MESSAGE: usize = protocol::DEFAULT_MAX_FRAME_LENGTH * 4;
/// How long the writer has to put a close frame on the wire after the socket is closed.
const CLOSE_FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// What a socket delivers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsEvent {
    Text(String),
    Binary(Vec<u8>),
    /// The `close` event: the peer's code (1005 without one, 1006 without a close frame) and reason.
    Closed {
        code: u16,
        reason: String,
    },
    /// The `error` event.
    Error(String),
}

trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

enum Out {
    Message(Message, usize),
    Close(u16, String),
}

/// The ordered writer (`OrderedWebSocketWriter`): sends are queued in order and rejected once the socket is closed
/// or `max_pending` bytes are waiting.
#[derive(Clone)]
pub struct WsWriter {
    tx: mpsc::UnboundedSender<Out>,
    closed: Arc<AtomicBool>,
    pending: Arc<AtomicUsize>,
    max_pending: usize,
}

impl WsWriter {
    pub fn send(&self, message: Message) -> Result<(), String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("Radius relay WebSocket is closed".into());
        }
        let bytes = match &message {
            Message::Text(text) => text.len(),
            Message::Binary(bytes) => bytes.len(),
            _ => 0,
        };
        if self.pending.load(Ordering::SeqCst) + bytes > self.max_pending {
            return Err("Radius relay exceeded its pending byte limit".into());
        }
        self.pending.fetch_add(bytes, Ordering::SeqCst);
        self.tx.send(Out::Message(message, bytes)).map_err(|_| {
            self.pending.fetch_sub(bytes, Ordering::SeqCst);
            "Radius relay WebSocket is closed".to_string()
        })
    }

    /// Stop accepting sends (`writer.close()`); the socket stays open.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// `socket.close(code, reason)`: no more sends; the close frame follows what is queued.
    pub fn close_socket(&self, code: u16, reason: &str) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self.tx.send(Out::Close(code, reason.to_string()));
    }

    fn control(&self, message: Message) {
        let _ = self.tx.send(Out::Message(message, 0));
    }
}

/// An open socket: its writer, its event reader, and the writer task to wait for on shutdown.
pub struct WsConnection {
    pub writer: WsWriter,
    pub reader: WsReader,
    writer_task: tokio::task::JoinHandle<()>,
}

impl WsConnection {
    /// Wait (bounded) for the writer to put everything queued, a close frame included, on the wire.
    pub async fn finish(self) {
        drop(self.reader);
        let WsConnection {
            writer, writer_task, ..
        } = self;
        drop(writer);
        let _ = tokio::time::timeout(CLOSE_FLUSH_TIMEOUT, writer_task).await;
    }
}

pub struct WsReader {
    read: ReadHalf<Box<dyn Io>>,
    decoder: MessageDecoder,
    queued: std::collections::VecDeque<Message>,
    writer: WsWriter,
    /// The close event was delivered; nothing follows it.
    done: bool,
    /// The error event was delivered; the close event (1006) follows.
    failed: bool,
}

impl WsReader {
    /// The next event; after the close event, it repeats.
    pub async fn next(&mut self) -> WsEvent {
        if self.failed {
            self.done = true;
            self.failed = false;
            return WsEvent::Closed {
                code: 1006,
                reason: String::new(),
            };
        }
        if self.done {
            return WsEvent::Closed {
                code: 1006,
                reason: String::new(),
            };
        }
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            while let Some(message) = self.queued.pop_front() {
                match message {
                    Message::Text(text) => return WsEvent::Text(text),
                    Message::Binary(bytes) => return WsEvent::Binary(bytes),
                    Message::Ping(payload) => self.writer.control(Message::Pong(payload)),
                    Message::Pong(_) => {}
                    Message::Close(code, reason) => {
                        // Echo the close (undici does) unless this side already sent one.
                        if !self.writer.closed.load(Ordering::SeqCst) {
                            self.writer.close_socket(code.unwrap_or(1000), "");
                        }
                        self.done = true;
                        return WsEvent::Closed {
                            code: code.unwrap_or(1005),
                            reason,
                        };
                    }
                }
            }
            let read = match self.read.read(&mut buffer).await {
                Ok(0) | Err(_) => return self.fail(),
                Ok(n) => n,
            };
            match self.decoder.push(&buffer[..read]) {
                Ok(messages) => self.queued.extend(messages),
                Err(_) => {
                    self.writer.close_socket(1002, "");
                    return self.fail();
                }
            }
        }
    }

    fn fail(&mut self) -> WsEvent {
        self.failed = true;
        self.writer.close();
        WsEvent::Error(CONNECTION_FAILED.into())
    }
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    let _ = getrandom::fill(&mut bytes);
    bytes
}

/// Open `url` (`ws:` or `wss:`) offering `protocol`, with `authorization`; every failure is [`CONNECTION_FAILED`].
pub async fn connect(
    url: &str,
    protocol: &str,
    authorization: &str,
    max_pending: usize,
) -> Result<WsConnection, String> {
    let fail = |_| CONNECTION_FAILED.to_string();
    let mut http = reqwest::Url::parse(url).map_err(fail)?;
    let scheme = match http.scheme() {
        "ws" => "http",
        "wss" => "https",
        _ => return Err(CONNECTION_FAILED.into()),
    };
    http.set_scheme(scheme).map_err(|_| CONNECTION_FAILED.to_string())?;
    let key = base64(&random::<16>());
    let client = reqwest::Client::builder()
        .http1_only()
        .no_proxy()
        .build()
        .map_err(|_| CONNECTION_FAILED.to_string())?;
    let response = client
        .get(http)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("authorization", authorization)
        .header("sec-websocket-key", &key)
        .header("sec-websocket-version", "13")
        .header("sec-websocket-protocol", protocol)
        .send()
        .await
        .map_err(|_| CONNECTION_FAILED.to_string())?;
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    if response.status().as_u16() != 101
        || header("sec-websocket-accept").as_deref() != Some(accept_key(&key).as_str())
        || header("sec-websocket-protocol").as_deref() != Some(protocol)
        || header("sec-websocket-extensions").is_some()
    {
        return Err(CONNECTION_FAILED.into());
    }
    let upgraded = response.upgrade().await.map_err(|_| CONNECTION_FAILED.to_string())?;
    let io: Box<dyn Io> = Box::new(upgraded);
    let (read, write) = tokio::io::split(io);
    let (tx, rx) = mpsc::unbounded_channel();
    let writer = WsWriter {
        tx,
        closed: Arc::new(AtomicBool::new(false)),
        pending: Arc::new(AtomicUsize::new(0)),
        max_pending,
    };
    let writer_task = tokio::spawn(write_loop(write, rx, writer.pending.clone()));
    Ok(WsConnection {
        reader: WsReader {
            read,
            decoder: MessageDecoder::client(MAX_MESSAGE),
            queued: Default::default(),
            writer: writer.clone(),
            done: false,
            failed: false,
        },
        writer,
        writer_task,
    })
}

async fn write_loop(
    mut write: WriteHalf<Box<dyn Io>>,
    mut rx: mpsc::UnboundedReceiver<Out>,
    pending: Arc<AtomicUsize>,
) {
    while let Some(out) = rx.recv().await {
        match out {
            Out::Message(message, bytes) => {
                let frame = encode_message(&message, Some(random::<4>()));
                let written = write.write_all(&frame).await;
                pending.fetch_sub(bytes, Ordering::SeqCst);
                if written.is_err() {
                    break;
                }
            }
            Out::Close(code, reason) => {
                let frame = encode_message(&Message::Close(Some(code), reason), Some(random::<4>()));
                let _ = write.write_all(&frame).await;
                let _ = write.flush().await;
                break;
            }
        }
    }
    let _ = write.shutdown().await;
}
