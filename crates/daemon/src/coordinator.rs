//! The coordinator process (experimental/coordinator.ts): a transport shim that owns a logical server's public
//! socket `<serverId>.sock` and its control socket. Server generations register on the control socket (protocol 4,
//! several side by side, newest receives new public connections); session workers register as peers. It routes
//! opaque `send`/`broadcast` payloads, proxies public connections to the newest server's endpoint, and exits once
//! nothing has been connected for a grace period.

use crate::control::{LineReader, LineWriter};
use crate::sockets;
use serde_json::{Value, json};
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const COORDINATOR_PROTOCOL_VERSION: i64 = 4;
const EMPTY_STARTUP_GRACE: Duration = Duration::from_millis(30_000);
const EMPTY_SHUTDOWN_GRACE: Duration = Duration::from_millis(250);

struct ServerPeer {
    id: String,
    endpoint: String,
    writer: LineWriter,
}

#[derive(Default)]
struct State {
    servers: Vec<ServerPeer>,
    peers: Vec<(String, LineWriter)>,
    control_connections: usize,
    public_connections: usize,
    shutting_down: bool,
    timer_generation: u64,
    timer_armed: bool,
}

struct Coordinator {
    public_path: PathBuf,
    control_path: PathBuf,
    state: Mutex<State>,
}

type Shared = Arc<Coordinator>;

impl Coordinator {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// `runCoordinatorProcess`: never returns once listening; the process exits after its empty shutdown.
pub fn run(args: &[String]) -> io::Result<()> {
    let (Some(public), Some(control)) = (args.first(), args.get(1)) else {
        return Err(io::Error::other("Coordinator requires public and control socket paths"));
    };
    let coordinator = Arc::new(Coordinator {
        public_path: PathBuf::from(public),
        control_path: PathBuf::from(control),
        state: Mutex::new(State::default()),
    });
    sockets::remove_stale_socket(&coordinator.control_path, "Coordinator")?;
    sockets::remove_stale_socket(&coordinator.public_path, "Coordinator")?;
    let listeners = sockets::listen(&coordinator.control_path)
        .and_then(|control| Ok((control, sockets::listen(&coordinator.public_path)?)));
    let (control_listener, public_listener) = match listeners {
        Ok(listeners) => listeners,
        Err(error) => {
            let _ = sockets::cleanup_socket(&coordinator.control_path);
            let _ = sockets::cleanup_socket(&coordinator.public_path);
            return Err(error);
        }
    };
    schedule_empty_shutdown(&coordinator, EMPTY_STARTUP_GRACE);
    // SIGINT/SIGTERM/SIGHUP: shut down, removing both sockets (`shutdownCoordinator`).
    let signalled = coordinator.clone();
    crate::signals::set_handler(Arc::new(move |_| shutdown(&signalled, signalled.lock())));
    let public = coordinator.clone();
    std::thread::spawn(move || {
        for stream in public_listener.incoming().flatten() {
            accept_public(&public, stream);
        }
    });
    for stream in control_listener.incoming().flatten() {
        let coordinator = coordinator.clone();
        std::thread::spawn(move || accept_control(&coordinator, stream));
    }
    Ok(())
}

fn accept_control(coordinator: &Shared, stream: UnixStream) {
    let Ok(read) = stream.try_clone() else { return };
    {
        let mut state = coordinator.lock();
        if state.shutting_down {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }
        state.control_connections += 1;
    }
    let writer = LineWriter::new(stream);
    let mut reader = LineReader::new(read);
    let mut role: Option<(bool, String)> = None;
    loop {
        let message = match reader.read_value() {
            Ok(Some(message)) if message["type"].is_string() => message,
            _ => break,
        };
        let outcome = match &role {
            None => match message["type"].as_str() {
                Some("register_server") => register_server(coordinator, &writer, &message).map(|id| {
                    role = Some((true, id));
                }),
                Some("register_peer") => register_peer(coordinator, &writer, &message).map(|id| {
                    role = Some((false, id));
                }),
                _ => Err("Coordinator connection did not register a role".to_string()),
            },
            Some((true, id)) => {
                let current = coordinator
                    .lock()
                    .servers
                    .iter()
                    .any(|s| &s.id == id && s.writer.same(&writer));
                if current {
                    route(coordinator, id, &message)
                } else {
                    Ok(())
                }
            }
            Some((false, id)) => route(coordinator, id, &message),
        };
        if outcome.is_err() {
            break;
        }
    }
    writer.shutdown();
    let mut state = coordinator.lock();
    state.control_connections -= 1;
    match role {
        Some((true, id)) => {
            let before = state.servers.len();
            state.servers.retain(|s| !(s.id == id && s.writer.same(&writer)));
            if state.servers.len() != before {
                notify(
                    &state.peers,
                    &json!({"type": "server_disconnected", "serverConnectionId": id}),
                );
            }
        }
        Some((false, id)) => {
            let before = state.peers.len();
            state.peers.retain(|(p, w)| !(p == &id && w.same(&writer)));
            if state.peers.len() != before {
                let message = json!({"type": "peer_disconnected", "peerId": id});
                for server in &state.servers {
                    let _ = server.writer.send(&message);
                }
            }
        }
        None => {}
    }
    drop(state);
    check_empty(coordinator);
}

fn notify(peers: &[(String, LineWriter)], message: &Value) {
    for (_, writer) in peers {
        let _ = writer.send(message);
    }
}

fn nonempty(message: &Value, key: &str) -> Option<String> {
    message[key].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn register_server(coordinator: &Shared, writer: &LineWriter, message: &Value) -> Result<String, String> {
    if message["protocol"] != json!(COORDINATOR_PROTOCOL_VERSION) {
        return Err("Unsupported coordinator protocol".into());
    }
    let id = nonempty(message, "serverConnectionId").ok_or("Coordinator serverConnectionId must be a string")?;
    let endpoint = nonempty(message, "endpoint").ok_or("Coordinator endpoint must be a string")?;
    let mut state = coordinator.lock();
    if id == "server" || state.servers.iter().any(|s| s.id == id) || state.peers.iter().any(|(p, _)| *p == id) {
        return Err(format!("Coordinator server is already connected: {id}"));
    }
    state.servers.push(ServerPeer {
        id: id.clone(),
        endpoint,
        writer: writer.clone(),
    });
    cancel_empty_shutdown(&mut state);
    let peers: Vec<&String> = state.peers.iter().map(|(p, _)| p).collect();
    let _ = writer.send(&json!({"type": "server_registered", "serverConnectionId": id, "peers": peers}));
    notify(
        &state.peers,
        &json!({"type": "server_connected", "serverConnectionId": id}),
    );
    Ok(id)
}

fn register_peer(coordinator: &Shared, writer: &LineWriter, message: &Value) -> Result<String, String> {
    if message["protocol"] != json!(COORDINATOR_PROTOCOL_VERSION) {
        return Err("Unsupported coordinator protocol".into());
    }
    let id = nonempty(message, "peerId").ok_or("Coordinator peerId must be a string")?;
    let mut state = coordinator.lock();
    if id == "server" || state.peers.iter().any(|(p, _)| *p == id) || state.servers.iter().any(|s| s.id == id) {
        return Err(format!("Coordinator peer is already connected: {id}"));
    }
    state.peers.push((id.clone(), writer.clone()));
    cancel_empty_shutdown(&mut state);
    let _ = writer.send(&json!({"type": "peer_registered", "peerId": id}));
    let connected = json!({"type": "peer_connected", "peerId": id});
    for server in &state.servers {
        let _ = server.writer.send(&connected);
    }
    Ok(id)
}

fn route(coordinator: &Shared, from: &str, message: &Value) -> Result<(), String> {
    let state = coordinator.lock();
    match message["type"].as_str() {
        Some("send") => {
            let to = nonempty(message, "to").ok_or("Coordinator message target must be a string")?;
            let payload = message.get("payload").cloned();
            let target = if to == "server" {
                state.servers.last().map(|s| &s.writer)
            } else {
                state
                    .servers
                    .iter()
                    .find(|s| s.id == to)
                    .map(|s| &s.writer)
                    .or_else(|| state.peers.iter().find(|(p, _)| *p == to).map(|(_, w)| w))
            };
            if let Some(target) = target {
                let _ = target.send(&routed(from, payload));
            }
            Ok(())
        }
        Some("broadcast") => {
            if !state.servers.iter().any(|s| s.id == from) {
                return Err("Only a server may broadcast".into());
            }
            let payload = message.get("payload").cloned();
            for (_, writer) in &state.peers {
                let _ = writer.send(&routed(from, payload.clone()));
            }
            Ok(())
        }
        other => Err(format!(
            "Unknown coordinator routing message: {}",
            other.unwrap_or_default()
        )),
    }
}

/// `{ type: "message", from, payload }`; an absent payload is omitted, as `JSON.stringify` drops `undefined`.
fn routed(from: &str, payload: Option<Value>) -> Value {
    let mut message = json!({"type": "message", "from": from});
    if let Some(payload) = payload {
        message["payload"] = payload;
    }
    message
}

fn accept_public(coordinator: &Shared, client: UnixStream) {
    let endpoint = {
        let mut state = coordinator.lock();
        match state.servers.last() {
            Some(server) if !state.shutting_down => {
                let endpoint = server.endpoint.clone();
                state.public_connections += 1;
                cancel_empty_shutdown(&mut state);
                endpoint
            }
            _ => {
                let _ = client.shutdown(std::net::Shutdown::Both);
                return;
            }
        }
    };
    let coordinator = coordinator.clone();
    std::thread::spawn(move || {
        if let Ok(upstream) = UnixStream::connect(&endpoint) {
            pipe(client, upstream);
        } else {
            let _ = client.shutdown(std::net::Shutdown::Both);
        }
        coordinator.lock().public_connections -= 1;
        check_empty(&coordinator);
    });
}

/// Copy both directions until either side closes, then close both.
fn pipe(client: UnixStream, upstream: UnixStream) {
    let (Ok(mut client_read), Ok(mut upstream_write)) = (client.try_clone(), upstream.try_clone()) else {
        return;
    };
    let (closer_a, closer_b) = (client.try_clone(), upstream.try_clone());
    let forward = std::thread::spawn(move || {
        let _ = io::copy(&mut client_read, &mut upstream_write);
        if let Ok(stream) = closer_b {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    });
    let (mut upstream_read, mut client_write) = (upstream, client);
    let _ = io::copy(&mut upstream_read, &mut client_write);
    let _ = client_write.shutdown(std::net::Shutdown::Both);
    if let Ok(stream) = closer_a {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    let _ = upstream_read.shutdown(std::net::Shutdown::Both);
    let _ = forward.join();
}

fn is_empty(state: &State) -> bool {
    state.servers.is_empty()
        && state.peers.is_empty()
        && state.public_connections == 0
        && state.control_connections == 0
}

fn check_empty(coordinator: &Shared) {
    let mut state = coordinator.lock();
    if state.shutting_down || !is_empty(&state) {
        cancel_empty_shutdown(&mut state);
        return;
    }
    drop(state);
    schedule_empty_shutdown(coordinator, EMPTY_SHUTDOWN_GRACE);
}

fn cancel_empty_shutdown(state: &mut State) {
    state.timer_generation += 1;
    state.timer_armed = false;
}

fn schedule_empty_shutdown(coordinator: &Shared, delay: Duration) {
    let generation = {
        let mut state = coordinator.lock();
        if state.timer_armed || state.shutting_down {
            return;
        }
        state.timer_armed = true;
        state.timer_generation
    };
    let coordinator = coordinator.clone();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let mut state = coordinator.lock();
        if state.timer_generation != generation || !is_empty(&state) {
            return;
        }
        state.timer_armed = false;
        shutdown(&coordinator, state);
    });
}

fn shutdown(coordinator: &Coordinator, mut state: std::sync::MutexGuard<'_, State>) {
    state.shutting_down = true;
    let _ = sockets::cleanup_socket(&coordinator.public_path);
    let _ = sockets::cleanup_socket(&coordinator.control_path);
    std::process::exit(0);
}
