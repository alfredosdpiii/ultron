//! A fake Radius gateway on 127.0.0.1 that bridges clients to a hosting server, as the real gateway's session relay
//! does (`/v1/session-relays/<server id>/connect`): a server's relay host connects with `pi-session-relay.host.v1`;
//! each client connecting with `pi-session-relay.client.v1` becomes a relayed connection on it (`connection_open`
//! with a new connection ID, binary data frames both ways, `connection_close` when either side ends). Every upgrade
//! is recorded (path, offered subprotocol, authorization) with the close frame the peer sent. A test can hold new
//! client upgrades until released, and drop every client connection (the gateway losing them). Waits are on
//! condition variables with a 120 s hang guard; nothing sleeps.
#![allow(dead_code)]

use protocol::websocket::{Message, MessageDecoder, accept_key, encode_message};
use serde_json::{Value, json};
use server::radius_relay::{encode_relay_data_frame, parse_relay_data_frame};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const HANG_GUARD: Duration = Duration::from_secs(120);
const HOST: &str = "pi-session-relay.host.v1";
const CLIENT: &str = "pi-session-relay.client.v1";

/// One WebSocket upgrade the gateway saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upgrade {
    pub path: String,
    pub protocol: String,
    pub authorization: String,
    /// The close frame the peer sent (`code reason`), once it did.
    pub closed: Option<String>,
}

#[derive(Default)]
struct State {
    upgrades: Vec<Upgrade>,
    /// The relay host of each server id.
    hosts: HashMap<String, Arc<Mutex<TcpStream>>>,
    /// Client connections by connection id: (server id, stream).
    clients: HashMap<String, (String, Arc<Mutex<TcpStream>>)>,
    next_connection: u64,
    /// New client upgrades wait while held.
    hold: bool,
    /// Client upgrades waiting while held.
    waiting: usize,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait_until(&self, what: &str, done: impl Fn(&State) -> bool) {
        let deadline = Instant::now() + HANG_GUARD;
        let mut state = self.lock();
        while !done(&state) {
            let now = Instant::now();
            assert!(now < deadline, "timed out waiting for {what}");
            state = self.changed.wait_timeout(state, deadline - now).unwrap().0;
        }
    }
}

pub struct Bridge {
    pub port: u16,
    shared: Arc<Shared>,
}

fn send(stream: &Mutex<TcpStream>, message: &Message) -> bool {
    stream
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .write_all(&encode_message(message, None))
        .is_ok()
}

impl Bridge {
    pub fn start() -> Bridge {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let accepting = shared.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let shared = accepting.clone();
                std::thread::spawn(move || serve(stream, &shared));
            }
        });
        Bridge { port, shared }
    }

    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn upgrades(&self) -> Vec<Upgrade> {
        self.shared.lock().upgrades.clone()
    }

    /// Wait until `count` upgrades with `protocol` were seen; returns them.
    pub fn wait_upgrades(&self, protocol: &str, count: usize) -> Vec<Upgrade> {
        self.shared.wait_until(&format!("{count} {protocol} upgrades"), |s| {
            s.upgrades.iter().filter(|u| u.protocol == protocol).count() >= count
        });
        self.upgrades().into_iter().filter(|u| u.protocol == protocol).collect()
    }

    /// Wait until the host of `server_id` is connected.
    pub fn wait_host(&self, server_id: &str) {
        self.shared
            .wait_until("the relay host", |s| s.hosts.contains_key(server_id));
    }

    /// Wait until the host upgrade's close frame arrived.
    pub fn wait_host_closed(&self) -> Upgrade {
        self.shared.wait_until("the relay host's close", |s| {
            s.upgrades.iter().any(|u| u.protocol == HOST && u.closed.is_some())
        });
        self.upgrades()
            .into_iter()
            .find(|u| u.protocol == HOST && u.closed.is_some())
            .unwrap()
    }

    /// Hold (or stop holding) new client upgrades.
    pub fn hold_clients(&self, hold: bool) {
        self.shared.lock().hold = hold;
        self.shared.changed.notify_all();
    }

    /// Wait until a client upgrade is held.
    pub fn wait_held(&self) {
        self.shared.wait_until("a held client upgrade", |s| s.waiting > 0);
    }

    /// Drop every client connection (the gateway lost them); their hosts are told the connections closed.
    pub fn drop_clients(&self) {
        let clients: Vec<_> = self.shared.lock().clients.values().map(|(_, s)| s.clone()).collect();
        for client in clients {
            let _ = client
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .shutdown(Shutdown::Both);
        }
    }

    /// The number of open client connections.
    pub fn client_count(&self) -> usize {
        self.shared.lock().clients.len()
    }
}

/// Read one HTTP request head.
fn request_head(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).ok()?;
        head.push(byte[0]);
    }
    Some(String::from_utf8_lossy(&head).into_owned())
}

fn serve(mut stream: TcpStream, shared: &Arc<Shared>) {
    let Some(head) = request_head(&mut stream) else {
        return;
    };
    let header = |name: &str| {
        head.split("\r\n")
            .filter_map(|line| line.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_default()
    };
    let path = head.split(' ').nth(1).unwrap_or_default().to_string();
    let protocol = header("sec-websocket-protocol");
    let server_id = path
        .strip_prefix("/v1/session-relays/")
        .and_then(|rest| rest.strip_suffix("/connect"))
        .unwrap_or_default()
        .to_string();
    let index = {
        let mut state = shared.lock();
        state.upgrades.push(Upgrade {
            path: path.clone(),
            protocol: protocol.clone(),
            authorization: header("authorization"),
            closed: None,
        });
        state.upgrades.len() - 1
    };
    shared.changed.notify_all();
    if protocol == CLIENT {
        let mut state = shared.lock();
        state.waiting += 1;
        shared.changed.notify_all();
        while state.hold {
            state = shared.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.waiting -= 1;
        let refuse = !state.hosts.contains_key(&server_id);
        drop(state);
        if refuse {
            let _ =
                stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            return;
        }
    }
    let response = format!(
        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\nsec-websocket-accept: {}\r\nsec-websocket-protocol: {protocol}\r\n\r\n",
        accept_key(&header("sec-websocket-key")),
    );
    if stream.write_all(response.as_bytes()).is_err() {
        return;
    }
    let writer = Arc::new(Mutex::new(stream.try_clone().unwrap()));
    if protocol == HOST {
        shared.lock().hosts.insert(server_id.clone(), writer.clone());
        shared.changed.notify_all();
        host_loop(stream, &writer, shared, &server_id, index);
    } else {
        let connection_id = {
            let mut state = shared.lock();
            state.next_connection += 1;
            let id = format!("00000000-0000-4000-8000-{:012x}", state.next_connection);
            state.clients.insert(id.clone(), (server_id.clone(), writer.clone()));
            id
        };
        shared.changed.notify_all();
        let host = shared.lock().hosts.get(&server_id).cloned();
        if let Some(host) = &host {
            send(
                host,
                &Message::Text(
                    json!({"version": 1, "type": "connection_open", "connection_id": connection_id}).to_string(),
                ),
            );
        }
        client_loop(stream, shared, &connection_id, host, index);
    }
}

fn record_close(shared: &Shared, index: usize, code: Option<u16>, reason: &str) {
    shared.lock().upgrades[index].closed = Some(format!("{} {reason}", code.unwrap_or(1005)));
    shared.changed.notify_all();
}

/// A relay host's messages: data frames go to their client, `connection_close` ends one.
fn host_loop(
    mut stream: TcpStream,
    writer: &Arc<Mutex<TcpStream>>,
    shared: &Arc<Shared>,
    server_id: &str,
    index: usize,
) {
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
            match message {
                Message::Text(text) => {
                    let control: Value = serde_json::from_str(&text).unwrap_or_default();
                    if control["type"] == "connection_close" {
                        let id = control["connection_id"].as_str().unwrap_or_default();
                        let client = shared.lock().clients.remove(id);
                        if let Some((_, client)) = client {
                            send(&client, &Message::Close(Some(1000), String::new()));
                            let _ = client
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .shutdown(Shutdown::Both);
                        }
                    }
                }
                Message::Binary(frame) => {
                    if let Some((id, payload)) = parse_relay_data_frame(&frame) {
                        let client = shared.lock().clients.get(&id).map(|(_, s)| s.clone());
                        if let Some(client) = client {
                            send(&client, &Message::Binary(payload));
                        }
                    }
                }
                Message::Close(code, reason) => {
                    record_close(shared, index, code, &reason);
                    send(writer, &Message::Close(code, String::new()));
                    break 'read;
                }
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
    let mut state = shared.lock();
    if state.hosts.get(server_id).is_some_and(|h| Arc::ptr_eq(h, writer)) {
        state.hosts.remove(server_id);
    }
    drop(state);
    shared.changed.notify_all();
}

/// A client's messages go to its host as data frames; its end is the host's `connection_close`.
fn client_loop(
    mut stream: TcpStream,
    shared: &Arc<Shared>,
    connection_id: &str,
    host: Option<Arc<Mutex<TcpStream>>>,
    index: usize,
) {
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
            match message {
                Message::Binary(payload) => {
                    if let Some(host) = &host {
                        send(
                            host,
                            &Message::Binary(encode_relay_data_frame(connection_id, &payload).unwrap()),
                        );
                    }
                }
                Message::Close(code, reason) => {
                    record_close(shared, index, code, &reason);
                    let _ = stream.write_all(&encode_message(&Message::Close(code, String::new()), None));
                    break 'read;
                }
                _ => {}
            }
        }
    }
    let _ = stream.shutdown(Shutdown::Both);
    let removed = shared.lock().clients.remove(connection_id).is_some();
    if removed && let Some(host) = &host {
        send(
            host,
            &Message::Text(
                json!({"version": 1, "type": "connection_close", "connection_id": connection_id, "code": 1000})
                    .to_string(),
            ),
        );
    }
    shared.changed.notify_all();
}
