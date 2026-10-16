//! A fake Radius gateway on 127.0.0.1 for the relay tests: it accepts one WebSocket upgrade at a time (or refuses it
//! with 403), records the request's path, offered subprotocol and authorization, and every message the relay sends
//! on it (and the close frame the relay sends before the gateway closed), and lets a test send text, binary and close
//! frames or drop the connection. Waits are on channels with a 120 s hang guard; nothing sleeps.

#![allow(dead_code)]

use protocol::websocket::{Message, MessageDecoder, accept_key, encode_message};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const HANG_GUARD: Duration = Duration::from_secs(120);
pub const PONG: &str = r#"{"version":1,"type":"pong"}"#;
pub const PING: &str = r#"{"version":1,"type":"ping"}"#;

#[derive(Default)]
pub struct SocketRecord {
    pub path: String,
    pub protocol: String,
    pub authorization: String,
    pub sent: Vec<Value>,
    pub closed: Option<Value>,
}

pub struct Gateway {
    pub port: u16,
    streams: Receiver<TcpStream>,
}

pub struct GatewaySocket {
    stream: TcpStream,
    pub record: Arc<Mutex<SocketRecord>>,
    gateway_closed: Arc<AtomicBool>,
    pongs: Receiver<()>,
    /// Pongs to drop from the record (the replay's own synchronization pings).
    hidden: Arc<Mutex<usize>>,
    done: Receiver<()>,
}

impl Gateway {
    pub fn start() -> Gateway {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let (tx, streams) = channel();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                if tx.send(stream).is_err() {
                    break;
                }
            }
        });
        Gateway { port, streams }
    }

    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The next upgrade request: answered with 101 (selecting the offered subprotocol) or 403 when `refuse`.
    pub fn accept(&self, refuse: bool) -> GatewaySocket {
        let mut stream = self
            .streams
            .recv_timeout(HANG_GUARD)
            .expect("the relay opened a connection");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).expect("request head");
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let mut lines = head.split("\r\n");
        let path = lines
            .next()
            .unwrap_or_default()
            .split(' ')
            .nth(1)
            .unwrap_or_default()
            .to_string();
        let header = |name: &str| {
            head.split("\r\n")
                .filter_map(|line| line.split_once(':'))
                .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
                .map(|(_, v)| v.trim().to_string())
                .unwrap_or_default()
        };
        let record = Arc::new(Mutex::new(SocketRecord {
            path,
            protocol: header("sec-websocket-protocol"),
            authorization: header("authorization"),
            ..Default::default()
        }));
        let gateway_closed = Arc::new(AtomicBool::new(false));
        let hidden = Arc::new(Mutex::new(0));
        let (pong_tx, pongs) = channel();
        let (done_tx, done) = channel();
        if refuse {
            let _ = stream.write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            let _ = stream.shutdown(Shutdown::Both);
            gateway_closed.store(true, Ordering::SeqCst);
            let _ = done_tx.send(());
        } else {
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {}\r\nsec-websocket-protocol: {}\r\n\r\n",
                accept_key(&header("sec-websocket-key")),
                header("sec-websocket-protocol")
            );
            stream.write_all(response.as_bytes()).expect("upgrade response");
            let reader = stream.try_clone().unwrap();
            spawn_reader(
                reader,
                record.clone(),
                gateway_closed.clone(),
                hidden.clone(),
                pong_tx,
                done_tx,
            );
        }
        GatewaySocket {
            stream,
            record,
            gateway_closed,
            pongs,
            hidden,
            done,
        }
    }
}

fn spawn_reader(
    mut stream: TcpStream,
    record: Arc<Mutex<SocketRecord>>,
    gateway_closed: Arc<AtomicBool>,
    hidden: Arc<Mutex<usize>>,
    pongs: Sender<()>,
    done: Sender<()>,
) {
    std::thread::spawn(move || {
        let mut decoder = MessageDecoder::server(64 * 1024 * 1024);
        let mut buffer = vec![0u8; 64 * 1024];
        'read: loop {
            let n = match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let Ok(messages) = decoder.push(&buffer[..n]) else {
                break;
            };
            for message in messages {
                let mut record = record.lock().unwrap();
                match message {
                    Message::Text(text) => {
                        let is_pong = text == PONG;
                        let mut hidden = hidden.lock().unwrap();
                        if is_pong && *hidden > 0 {
                            *hidden -= 1;
                        } else {
                            record.sent.push(json!({ "text": text }));
                        }
                        if is_pong {
                            let _ = pongs.send(());
                        }
                    }
                    Message::Binary(bytes) => record.sent.push(json!({ "binary": hex(&bytes) })),
                    Message::Close(code, reason) => {
                        if !gateway_closed.swap(true, Ordering::SeqCst) {
                            record.closed = Some(json!({ "code": code.unwrap_or(1005), "reason": reason }));
                            // Answer the close, as a gateway does.
                            let _ = stream.write_all(&encode_message(&Message::Close(code, String::new()), None));
                        }
                        drop(record);
                        let _ = stream.shutdown(Shutdown::Both);
                        break 'read;
                    }
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
        }
        let _ = done.send(());
    });
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

impl GatewaySocket {
    pub fn send(&mut self, message: Message) {
        // A write to a socket the relay already closed fails; the relay is gone, which the record shows.
        let _ = self.stream.write_all(&encode_message(&message, None));
    }

    pub fn close(&mut self, code: u16, reason: &str) {
        self.gateway_closed.store(true, Ordering::SeqCst);
        self.send(Message::Close(Some(code), reason.to_string()));
    }

    pub fn drop_connection(&mut self) {
        self.gateway_closed.store(true, Ordering::SeqCst);
        let _ = self.stream.shutdown(Shutdown::Both);
    }

    /// Ping (the relay's control ping) and wait for the pong, which proves everything sent before was handled.
    pub fn sync(&mut self, recorded: bool) {
        if !recorded {
            *self.hidden.lock().unwrap() += 1;
        }
        self.send(Message::Text(PING.into()));
        self.pongs
            .recv_timeout(HANG_GUARD)
            .expect("the relay answered the ping");
    }

    /// Wait until the relay's side of the connection ended (its close frame, or the connection dropped).
    pub fn wait_done(&self) {
        self.done
            .recv_timeout(HANG_GUARD)
            .expect("the relay closed the connection");
    }

    pub fn is_gateway_closed(&self) -> bool {
        self.gateway_closed.load(Ordering::SeqCst)
    }

    pub fn snapshot(&self) -> Value {
        let record = self.record.lock().unwrap();
        json!({
            "path": record.path,
            "protocol": record.protocol,
            "authorization": record.authorization,
            "sent": record.sent,
            "closed": record.closed,
        })
    }
}
