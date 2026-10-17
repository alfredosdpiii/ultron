//! A WebSocket client with the behavior Node's `WebSocket` (undici) shows the providers: an HTTP/1.1 upgrade
//! (`ws:`/`wss:`) carrying the request's headers, then RFC 6455 messages ([`protocol::websocket`]). Every handshake
//! failure (refused, a status other than 101, a wrong `Sec-WebSocket-Accept`, an extension nobody offered) is an
//! `error` event without details, as undici reports it; a connection lost without a close frame is an `error`, then a
//! 1006 closure; a close frame is echoed. Pings are answered. No extension (permessage-deflate) is offered.

use crate::http::{BoxFuture, HeaderMap};
use protocol::websocket::{Message, MessageDecoder, accept_key, base64, encode_message};
use std::collections::VecDeque;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};

/// What `new WebSocket(...)` throws where the runtime has none.
pub const NO_WEBSOCKET: &str = "WebSocket transport is not available in this runtime";
/// The message of undici's detail-less `error` event, as the providers report it (`extractWebSocketError`).
pub const WEBSOCKET_ERROR: &str = "WebSocket error";
/// The largest message read (a response event is far smaller).
const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// What an open socket delivers (`message`, `close` and `error` events).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsEvent {
    Text(String),
    Binary(Vec<u8>),
    /// The `close` event: the peer's code (1005 without one, 1006 without a close frame), reason and `wasClean`.
    Closed {
        code: u16,
        reason: String,
        was_clean: bool,
    },
    Error(String),
}

/// An open WebSocket.
pub trait WebSocketConnection: Send {
    /// `socket.send(text)`; on a socket that is no longer open the message is dropped, as the standard says.
    fn send_text<'a>(&'a mut self, text: &'a str) -> BoxFuture<'a, ()>;
    /// The next event; after the close event, it repeats.
    fn next_event(&mut self) -> BoxFuture<'_, WsEvent>;
    /// `socket.close(code, reason)`: a close frame, then the connection ends. Never fails.
    fn close<'a>(&'a mut self, code: u16, reason: &'a str) -> BoxFuture<'a, ()>;
    /// `readyState === OPEN`, looking at what already arrived without waiting: a close frame or a lost connection
    /// makes it closed; messages that arrived while nobody listened are dropped, pings answered.
    fn is_open(&mut self) -> BoxFuture<'_, bool>;
}

trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Open,
    /// The connection failed: the next event is the 1006 closure.
    Failed,
    Closed,
}

struct Socket {
    read: ReadHalf<Box<dyn Io>>,
    write: WriteHalf<Box<dyn Io>>,
    decoder: MessageDecoder,
    queued: VecDeque<Message>,
    state: State,
    /// This side sent its close frame.
    close_sent: bool,
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    let _ = getrandom::fill(&mut bytes);
    bytes
}

impl Socket {
    async fn write_message(&mut self, message: &Message) -> bool {
        let frame = encode_message(message, Some(random::<4>()));
        self.write.write_all(&frame).await.is_ok() && self.write.flush().await.is_ok()
    }

    async fn send_close(&mut self, code: u16, reason: &str) {
        if !self.close_sent {
            self.close_sent = true;
            let _ = self
                .write_message(&Message::Close(Some(code), reason.to_string()))
                .await;
            let _ = self.write.shutdown().await;
        }
    }

    fn fail(&mut self) -> WsEvent {
        self.state = State::Failed;
        WsEvent::Error(WEBSOCKET_ERROR.into())
    }

    /// One queued message as an event; control frames are handled here.
    async fn take_queued(&mut self) -> Option<WsEvent> {
        while let Some(message) = self.queued.pop_front() {
            match message {
                Message::Text(text) => return Some(WsEvent::Text(text)),
                Message::Binary(bytes) => return Some(WsEvent::Binary(bytes)),
                Message::Ping(payload) => {
                    let _ = self.write_message(&Message::Pong(payload)).await;
                }
                Message::Pong(_) => {}
                Message::Close(code, reason) => {
                    self.send_close(code.unwrap_or(1000), "").await;
                    self.state = State::Closed;
                    return Some(WsEvent::Closed {
                        code: code.unwrap_or(1005),
                        reason,
                        was_clean: true,
                    });
                }
            }
        }
        None
    }

    fn push(&mut self, bytes: &[u8]) -> bool {
        match self.decoder.push(bytes) {
            Ok(messages) => {
                self.queued.extend(messages);
                true
            }
            Err(_) => false,
        }
    }

    async fn event(&mut self) -> WsEvent {
        match self.state {
            State::Open => {}
            State::Failed => {
                self.state = State::Closed;
                return WsEvent::Closed {
                    code: 1006,
                    reason: String::new(),
                    was_clean: false,
                };
            }
            State::Closed => {
                return WsEvent::Closed {
                    code: 1006,
                    reason: String::new(),
                    was_clean: false,
                };
            }
        }
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            if let Some(event) = self.take_queued().await {
                return event;
            }
            let read = match self.read.read(&mut buffer).await {
                Ok(0) | Err(_) => return self.fail(),
                Ok(n) => n,
            };
            if !self.push(&buffer[..read]) {
                self.send_close(1002, "").await;
                return self.fail();
            }
        }
    }

    async fn poll_open(&mut self) -> bool {
        let mut buffer = vec![0u8; 64 * 1024];
        while self.state == State::Open {
            // Nobody listens between requests: what arrived is dropped (control frames are still handled).
            match self.take_queued().await {
                Some(WsEvent::Closed { .. }) => return false,
                Some(_) => continue,
                None => {}
            }
            // One look at the connection without waiting (`timeout` polls the read once before its deadline).
            match tokio::time::timeout(Duration::ZERO, self.read.read(&mut buffer)).await {
                Err(_) => return true,
                Ok(Ok(0)) | Ok(Err(_)) => {
                    self.state = State::Closed;
                    return false;
                }
                Ok(Ok(n)) => {
                    if !self.push(&buffer[..n]) {
                        self.send_close(1002, "").await;
                        self.state = State::Closed;
                        return false;
                    }
                }
            }
        }
        false
    }
}

impl WebSocketConnection for Socket {
    fn send_text<'a>(&'a mut self, text: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if self.state == State::Open && !self.close_sent && !self.write_message(&Message::Text(text.into())).await {
                self.state = State::Failed;
            }
        })
    }

    fn next_event(&mut self) -> BoxFuture<'_, WsEvent> {
        Box::pin(self.event())
    }

    fn close<'a>(&'a mut self, code: u16, reason: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if self.state == State::Open {
                self.send_close(code, reason).await;
            }
            self.state = State::Closed;
        })
    }

    fn is_open(&mut self) -> BoxFuture<'_, bool> {
        Box::pin(self.poll_open())
    }
}

/// Open `url` (`ws:` or `wss:`) through `client` (an HTTP/1.1 client) with `headers`; any handshake failure is
/// [`WEBSOCKET_ERROR`].
pub async fn connect(
    client: &reqwest::Client,
    url: &str,
    headers: &HeaderMap,
) -> Result<Box<dyn WebSocketConnection>, String> {
    let fail = || WEBSOCKET_ERROR.to_string();
    let mut http = reqwest::Url::parse(url).map_err(|e| format!("Failed to construct 'WebSocket': {e}"))?;
    let scheme = match http.scheme() {
        "ws" | "http" => "http",
        "wss" | "https" => "https",
        other => {
            return Err(format!(
                "Failed to construct 'WebSocket': The URL's scheme must be either 'http', 'https', 'ws', or 'wss'. '{other}:' is not allowed."
            ));
        }
    };
    http.set_scheme(scheme).map_err(|_| fail())?;
    let key = base64(&random::<16>());
    let mut request = client.get(http);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-key", &key)
        .header("sec-websocket-version", "13")
        .send()
        .await
        .map_err(|_| fail())?;
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    if response.status().as_u16() != 101
        || header("sec-websocket-accept").as_deref() != Some(accept_key(&key).as_str())
        || header("sec-websocket-extensions").is_some()
        || header("sec-websocket-protocol").is_some()
    {
        return Err(fail());
    }
    let upgraded = response.upgrade().await.map_err(|_| fail())?;
    let io: Box<dyn Io> = Box::new(upgraded);
    let (read, write) = tokio::io::split(io);
    Ok(Box::new(Socket {
        read,
        write,
        decoder: MessageDecoder::client(MAX_MESSAGE),
        queued: VecDeque::new(),
        state: State::Open,
        close_sent: false,
    }))
}
