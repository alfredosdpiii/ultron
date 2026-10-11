//! A server generation's coordinator connection (`CoordinatorConnection` in experimental/coordinator.ts) and
//! `ensureCoordinator`.

use crate::control::{LineReader, LineWriter};
use crate::coordinator::COORDINATOR_PROTOCOL_VERSION;
use crate::process::{self, Role};
use crate::sockets;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const COORDINATOR_START_TIMEOUT: Duration = Duration::from_millis(10_000);
const COORDINATOR_RETRY: Duration = Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    PeerConnected(String),
    PeerDisconnected(String),
    Message { from: String, payload: Value },
}

/// The coordinator closed the connection before accepting the server: another release owns it, or it is exiting.
#[derive(Debug)]
pub struct RegistrationRejected(pub String);

/// Keeps a coordinator from its empty shutdown while a server starts (`CoordinatorStartupLease`).
pub struct StartupLease(#[allow(dead_code)] UnixStream);

/// `ensureCoordinator`: connect to the control socket, else launch a coordinator and wait for it.
pub fn ensure_coordinator(public: &Path, control: &Path) -> io::Result<StartupLease> {
    if let Some(stream) = sockets::try_connect(control)? {
        return Ok(StartupLease(stream));
    }
    let child = process::spawn(
        Role::Coordinator,
        &[public.display().to_string(), control.display().to_string()],
        &[],
    )?;
    let exited = process::watch(child);
    let deadline = Instant::now() + COORDINATOR_START_TIMEOUT;
    loop {
        if let Some(stream) = sockets::try_connect(control)? {
            return Ok(StartupLease(stream));
        }
        if exited.try_recv().is_ok() {
            return Err(io::Error::other("Coordinator exited during startup"));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::other("Timed out waiting for coordinator startup"));
        }
        std::thread::sleep(COORDINATOR_RETRY);
    }
}

pub struct CoordinatorConnection {
    pub server_connection_id: String,
    pub control_path: PathBuf,
    writer: LineWriter,
    pub peer_ids: Mutex<HashSet<String>>,
    lost: (Mutex<bool>, Condvar),
    closed: Mutex<bool>,
}

impl CoordinatorConnection {
    /// Register as a server with `endpoint`; events go to `on_event` on the connection's reader thread.
    pub fn connect(
        control_path: &Path,
        endpoint: &Path,
        on_event: Box<dyn Fn(Event) + Send>,
    ) -> Result<Arc<CoordinatorConnection>, io::Result<RegistrationRejected>> {
        let stream = UnixStream::connect(control_path).map_err(Err)?;
        let read = stream.try_clone().map_err(Err)?;
        let writer = LineWriter::new(stream);
        let server_connection_id = crate::paths::uuid_v4();
        writer
            .send(&json!({
                "type": "register_server", "protocol": COORDINATOR_PROTOCOL_VERSION,
                "serverConnectionId": server_connection_id, "endpoint": endpoint.display().to_string(),
            }))
            .map_err(Err)?;
        let mut reader = LineReader::new(read);
        let registered = match reader.read_value() {
            Ok(Some(message)) => message,
            _ => return Err(Ok(RegistrationRejected(control_path.display().to_string()))),
        };
        if registered["type"] != "server_registered" || registered["serverConnectionId"] != json!(server_connection_id)
        {
            writer.shutdown();
            return Err(Err(io::Error::other(
                "Coordinator returned an invalid server registration",
            )));
        }
        let peers = registered["peers"]
            .as_array()
            .map(|p| p.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default();
        let connection = Arc::new(CoordinatorConnection {
            server_connection_id,
            control_path: control_path.to_path_buf(),
            writer,
            peer_ids: Mutex::new(peers),
            lost: (Mutex::new(false), Condvar::new()),
            closed: Mutex::new(false),
        });
        let reading = connection.clone();
        std::thread::spawn(move || {
            while let Ok(Some(message)) = reader.read_value() {
                let event = match message["type"].as_str() {
                    Some("peer_connected") => message["peerId"].as_str().map(|p| {
                        reading.peers().insert(p.to_string());
                        Event::PeerConnected(p.to_string())
                    }),
                    Some("peer_disconnected") => message["peerId"].as_str().map(|p| {
                        reading.peers().remove(p);
                        Event::PeerDisconnected(p.to_string())
                    }),
                    Some("message") => message["from"].as_str().map(|from| Event::Message {
                        from: from.to_string(),
                        payload: message.get("payload").cloned().unwrap_or(Value::Null),
                    }),
                    _ => None,
                };
                match event {
                    Some(event) => on_event(event),
                    None => break,
                }
            }
            reading.mark_lost();
        });
        Ok(connection)
    }

    fn peers(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.peer_ids.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn peer_snapshot(&self) -> HashSet<String> {
        self.peers().clone()
    }

    fn mark_lost(&self) {
        if *self.closed.lock().unwrap_or_else(|e| e.into_inner()) {
            return;
        }
        let (lost, changed) = &self.lost;
        *lost.lock().unwrap_or_else(|e| e.into_inner()) = true;
        changed.notify_all();
    }

    pub fn was_lost(&self) -> bool {
        *self.lost.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Block until the coordinator connection is lost (not merely closed by this server).
    pub fn wait_lost(&self) {
        let (lost, changed) = &self.lost;
        let mut guard = lost.lock().unwrap_or_else(|e| e.into_inner());
        while !*guard {
            guard = changed.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub fn send(&self, to: &str, payload: Value) -> io::Result<()> {
        self.writer.send(&json!({"type": "send", "to": to, "payload": payload}))
    }

    pub fn broadcast(&self, payload: Value) -> io::Result<()> {
        self.writer.send(&json!({"type": "broadcast", "payload": payload}))
    }

    pub fn close(&self) {
        *self.closed.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.writer.shutdown();
    }
}
