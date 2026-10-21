//! The client runtime print, json and rpc modes (and later the TUI) use: a pipelined connection
//! (packages/client client.ts) whose service subscriptions keep replicas of the server's replicated states
//! (Chord's consumer side), and `run_client` (experimental/client.ts `runClient`): list sessions, attach to one, or
//! create one and prompt it, following its transcript until the run ends.

use chord_delta::{Decoder, Op};
use protocol::{
    ClientMessage, DEFAULT_MAX_FRAME_LENGTH, PROTOCOL_VERSION, ProtocolError, RpcTarget, ServerMessage,
    ServerMessageDecoder, SessionTarget, Value, encode_client_message, is_server_id,
};
use serde_json::{Value as Json, json};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

pub fn to_cbor(json: &Json) -> Value {
    match json {
        Json::Null => Value::Null,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => n
            .as_i64()
            .map_or_else(|| Value::Float(n.as_f64().unwrap_or(f64::NAN)), Value::Int),
        Json::String(s) => Value::Text(s.clone()),
        Json::Array(items) => Value::Array(items.iter().map(to_cbor).collect()),
        Json::Object(map) => Value::Map(map.iter().map(|(k, v)| (k.clone(), to_cbor(v))).collect()),
    }
}

pub fn to_json(value: &Value) -> Json {
    match value {
        Value::Null | Value::Bytes(_) => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(n) => Json::from(*n),
        Value::Float(f) if f.fract() == 0.0 && f.abs() < 9e15 => Json::from(*f as i64),
        Value::Float(f) => serde_json::Number::from_f64(*f).map_or(Json::Null, Json::Number),
        Value::Text(s) => Json::String(s.clone()),
        Value::Array(items) => Json::Array(items.iter().map(to_json).collect()),
        Value::Map(entries) => Json::Object(entries.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
    }
}

/// A failed call: a bounded server error, or a lost connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    Server(ProtocolError),
    Disconnected(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Server(error) => f.write_str(&error.message),
            CallError::Disconnected(message) => f.write_str(message),
        }
    }
}

/// Receives `(member, value)` after each applied state update of one subscription.
pub type UpdateListener = Box<dyn Fn(&str, &Json) + Send>;

/// One state member of a singleton replica (`MemberSlot` over `ReplicatedStateReplica`): `None` until hydrated and
/// after it was cleared (`unavailable`, a sequence gap, an update that does not apply).
struct Slot {
    value: Option<Json>,
    sequence: u64,
}

/// A failure while applying a service update: `Fatal` breaks the operation stream (the wire decoder, `client.ts`
/// fails the connection), `Reported` is the consumer's `reportError` (state.ts/consumer.ts): only that replica's
/// member is cleared and the connection stays up.
enum UpdateError {
    Fatal(String),
    Reported(String),
}

/// The client's replica of one singleton subscription: the wire decoders (`createServiceStateDecoder`) and the
/// consumer facade (`ServiceFacade` of `RemoteServiceBinding`, consumer.ts:579-608).
struct Replica {
    service: String,
    decoders: HashMap<String, Decoder>,
    slots: HashMap<String, Slot>,
    listener: Option<UpdateListener>,
    /// Errors the consumer reported (`onError`), oldest first.
    errors: Vec<String>,
}

fn is_base(ops: &[Op]) -> bool {
    matches!(ops.first(), Some(Op::Replace(_)))
}

fn wire_ops(member: &Json) -> &[Json] {
    member["ops"].as_array().map(Vec::as_slice).unwrap_or_default()
}

impl Replica {
    /// Decode one instance snapshot's state members with fresh decoders (`decodeInstance`).
    fn decode_instance(&mut self, instance: &Json) -> Result<Vec<(String, u64, Vec<Op>)>, String> {
        let mut states = Vec::new();
        for member in instance["members"].as_array().into_iter().flatten() {
            if member["kind"] != "state" {
                continue;
            }
            let name = member["name"].as_str().unwrap_or_default().to_string();
            if self.decoders.contains_key(&name) {
                return Err(format!("Duplicate service state {name}"));
            }
            let decoder = self.decoders.entry(name.clone()).or_default();
            let ops = decoder.decode(wire_ops(member)).map_err(|e| e.to_string())?;
            states.push((name, member["sequence"].as_u64().unwrap_or_default(), ops));
        }
        Ok(states)
    }

    /// `validateMembers`: names are non-empty and unique.
    fn validate_members(instance: &Json) -> Result<(), String> {
        let mut names = std::collections::HashSet::new();
        for member in instance["members"].as_array().into_iter().flatten() {
            let name = member["name"].as_str().unwrap_or_default();
            if name.is_empty() || !names.insert(name) {
                return Err("Remote service has invalid member descriptions".into());
            }
        }
        Ok(())
    }

    /// `ServiceFacade.install`: hydrate every state member from a base batch; a failing member is cleared.
    fn install(&mut self, instance: &Json, states: Vec<(String, u64, Vec<Op>)>) -> Result<(), String> {
        Replica::validate_members(instance)?;
        let names: Vec<&str> = instance["members"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["name"].as_str())
            .collect();
        let mut existing: Vec<&String> = self.slots.keys().collect();
        existing.sort();
        if let Some(missing) = existing.into_iter().find(|name| !names.contains(&name.as_str())) {
            return Err(format!("Unknown remote service member {}.{missing}", self.service));
        }
        let mut states: HashMap<String, (u64, Vec<Op>)> = states
            .into_iter()
            .map(|(name, sequence, ops)| (name, (sequence, ops)))
            .collect();
        for member in instance["members"].as_array().into_iter().flatten() {
            let name = member["name"].as_str().unwrap_or_default().to_string();
            let Some((sequence, ops)) = states.remove(&name) else {
                if self.slots.contains_key(&name) {
                    return Err(format!("Remote service member {}.{name} changed kind", self.service));
                }
                continue;
            };
            let slot = self.slots.entry(name.clone()).or_insert(Slot {
                value: None,
                sequence: 0,
            });
            if !is_base(&ops) {
                slot.value = None;
                return Err("Replicated state snapshot is not a base operation batch".into());
            }
            match chord_delta::apply(Json::Null, ops) {
                Ok(value) => {
                    slot.sequence = sequence;
                    slot.value = Some(value);
                }
                Err(error) => {
                    slot.value = None;
                    return Err(error.to_string());
                }
            }
            if let (Some(listener), Some(value)) = (&self.listener, &slot.value) {
                listener(&name, value);
            }
        }
        Ok(())
    }

    /// The subscribe response: one singleton instance (`#startSingleton`).
    fn hydrate(service: &str, snapshot: &Json) -> Result<Replica, String> {
        let mut replica = Replica {
            service: service.to_string(),
            decoders: HashMap::new(),
            slots: HashMap::new(),
            listener: None,
            errors: Vec::new(),
        };
        let instances = snapshot["instances"].as_array().map(Vec::as_slice).unwrap_or_default();
        let mut decoded = Vec::new();
        for instance in instances {
            decoded.push(replica.decode_instance(instance)?);
        }
        if snapshot["mode"] != "singleton" || snapshot["serviceId"] != service || instances.len() != 1 {
            return Err(format!(
                "Remote service {service} returned an invalid singleton snapshot"
            ));
        }
        let states = decoded.pop().unwrap_or_default();
        replica.install(&instances[0], states)?;
        Ok(replica)
    }

    fn update(&mut self, update: &Json) -> Result<(), UpdateError> {
        match update["type"].as_str() {
            Some("state") => {
                let member = update["member"].as_str().unwrap_or_default();
                let instance = &update["instance"];
                let decoder = match instance.is_null() {
                    true => self.decoders.get_mut(member),
                    false => None,
                };
                let Some(decoder) = decoder else {
                    let name = match instance.is_null() {
                        true => member.to_string(),
                        false => format!("{}@{}.{member}", js(&instance["key"]), js(&instance["generation"])),
                    };
                    return Err(UpdateError::Fatal(format!("Unknown service state {name}")));
                };
                let ops = decoder
                    .decode(wire_ops(update))
                    .map_err(|e| UpdateError::Fatal(e.to_string()))?;
                let sequence = update["sequence"].as_u64().unwrap_or_default();
                let Some(slot) = self.slots.get_mut(member) else {
                    return Err(UpdateError::Reported(format!(
                        "Remote service update targets non-state member {}.{member}",
                        self.service
                    )));
                };
                let Some(value) = slot.value.take() else {
                    return Err(UpdateError::Reported(
                        "Replicated state received an update before hydration".into(),
                    ));
                };
                if sequence != slot.sequence + 1 {
                    return Err(UpdateError::Reported(
                        "Replicated state update sequence has a gap".into(),
                    ));
                }
                let next = chord_delta::apply(value, ops).map_err(|e| UpdateError::Reported(e.to_string()))?;
                slot.sequence = sequence;
                if let Some(listener) = &self.listener {
                    listener(member, &next);
                }
                slot.value = Some(next);
                Ok(())
            }
            Some("replaced") => {
                self.decoders.clear();
                let snapshot = &update["snapshot"];
                let states = self.decode_instance(snapshot).map_err(UpdateError::Fatal)?;
                if !snapshot["instance"].is_null() {
                    return Err(UpdateError::Reported(
                        "Singleton replacement has an instance address".into(),
                    ));
                }
                self.install(snapshot, states).map_err(UpdateError::Reported)
            }
            Some("unavailable") => {
                self.decoders.clear();
                for slot in self.slots.values_mut() {
                    slot.value = None;
                }
                Ok(())
            }
            // Keyed-instance updates: a singleton consumer ignores them.
            Some("spawned" | "closed") => Ok(()),
            _ => Err(UpdateError::Fatal("Invalid service provider update".into())),
        }
    }

    /// Apply one update; a reported error stays with this replica, a fatal one is returned.
    fn receive(&mut self, update: &Json) -> Result<(), String> {
        match self.update(update) {
            Ok(()) => Ok(()),
            Err(UpdateError::Reported(error)) => {
                self.errors.push(error);
                Ok(())
            }
            Err(UpdateError::Fatal(error)) => Err(error),
        }
    }
}

/// JavaScript template interpolation of a JSON scalar.
fn js(value: &Json) -> String {
    match value {
        Json::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[derive(Default)]
struct Shared {
    responses: HashMap<String, Result<Option<Json>, ProtocolError>>,
    replicas: HashMap<String, Replica>,
    /// Updates of a subscription whose snapshot has not been read yet.
    early: HashMap<String, Vec<Json>>,
    /// The service each pending subscription subscribes to.
    services: HashMap<String, String>,
    attachment: Option<SessionTarget>,
    closed: Option<String>,
}

/// The write half of a connection's byte transport: a Unix socket, or the Radius relay
/// ([`crate::radius::RadiusByteWriter`]).
pub trait ConnectionWriter: Write + Send {
    /// End the transport (both directions); the reader then sees it closed.
    fn shutdown(&mut self);
}

impl ConnectionWriter for UnixStream {
    fn shutdown(&mut self) {
        let _ = UnixStream::shutdown(self, std::net::Shutdown::Both);
    }
}

/// A handshaken connection to one server; requests are pipelined and answered by a reader thread.
pub struct Connection {
    pub server_id: String,
    writer: Mutex<Box<dyn ConnectionWriter>>,
    shared: Arc<(Mutex<Shared>, Condvar)>,
    requests: AtomicU64,
    subscriptions: AtomicU64,
}

impl Connection {
    pub fn connect(path: &std::path::Path, server_id: &str) -> Result<Arc<Connection>, CallError> {
        let disconnected = |e: std::io::Error| CallError::Disconnected(e.to_string());
        if !is_server_id(server_id) {
            return Err(CallError::Disconnected(
                "serverId must be a canonical lowercase UUIDv4".into(),
            ));
        }
        let stream = UnixStream::connect(path).map_err(disconnected)?;
        let read = stream.try_clone().map_err(disconnected)?;
        Connection::connect_over(Box::new(read), Box::new(stream), server_id)
    }

    /// The server `server_id` through the Radius gateway (`createRadiusClientTransportFactory`): `auth` is resolved
    /// for this connection (required).
    pub fn connect_radius(
        server_id: &str,
        auth: &dyn server::radius_relay::RelayAuthResolver,
    ) -> Result<Arc<Connection>, CallError> {
        if !is_server_id(server_id) {
            return Err(CallError::Disconnected(
                "serverId must be a canonical lowercase UUIDv4".into(),
            ));
        }
        let (read, write) = crate::radius::RadiusByteStream::connect(server_id, auth)
            .map_err(CallError::Disconnected)?
            .split();
        Connection::connect_over(Box::new(read), Box::new(write), server_id)
    }

    /// Handshake with `server_id` over a byte transport's halves.
    pub fn connect_over(
        mut read: Box<dyn Read + Send>,
        writer: Box<dyn ConnectionWriter>,
        server_id: &str,
    ) -> Result<Arc<Connection>, CallError> {
        let disconnected = |e: std::io::Error| CallError::Disconnected(e.to_string());
        let connection = Arc::new(Connection {
            server_id: server_id.to_string(),
            writer: Mutex::new(writer),
            shared: Arc::new((Mutex::new(Shared::default()), Condvar::new())),
            requests: AtomicU64::new(0),
            subscriptions: AtomicU64::new(0),
        });
        connection.write(&ClientMessage::Hello {
            version: PROTOCOL_VERSION,
        })?;
        let mut decoder = ServerMessageDecoder::default();
        let mut buffer = vec![0u8; 64 * 1024];
        let mut queued = Vec::new();
        let handshake = (|| {
            while queued.is_empty() {
                let n = read.read(&mut buffer).map_err(disconnected)?;
                if n == 0 {
                    return Err(CallError::Disconnected("Byte transport closed".into()));
                }
                queued = decoder.push(&buffer[..n]).map_err(|e| CallError::Disconnected(e.0))?;
            }
            match queued.remove(0) {
                ServerMessage::Hello { server_id: id, .. } if id == server_id => Ok(()),
                ServerMessage::HelloError { error } => Err(CallError::Server(error)),
                _ => Err(CallError::Disconnected("Expected server hello as first message".into())),
            }
        })();
        if let Err(error) = handshake {
            connection.disconnect();
            return Err(error);
        }
        let shared = connection.shared.clone();
        for message in queued {
            dispatch(&shared, message);
        }
        std::thread::spawn(move || {
            let reason = loop {
                let n = match read.read(&mut buffer) {
                    Ok(0) => break "Byte transport closed".to_string(),
                    Ok(n) => n,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => break e.to_string(),
                };
                match decoder.push(&buffer[..n]) {
                    Ok(messages) => {
                        for message in messages {
                            dispatch(&shared, message);
                        }
                    }
                    Err(error) => break error.0,
                }
            };
            let (lock, changed) = &*shared;
            lock.lock()
                .unwrap_or_else(|e| e.into_inner())
                .closed
                .get_or_insert(reason);
            changed.notify_all();
        });
        Ok(connection)
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self, message: &ClientMessage) -> Result<(), CallError> {
        let frame =
            encode_client_message(message, DEFAULT_MAX_FRAME_LENGTH).map_err(|e| CallError::Disconnected(e.0))?;
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(&frame)
            .map_err(|e| CallError::Disconnected(e.to_string()))
    }

    pub fn attachment(&self) -> Option<SessionTarget> {
        self.lock().attachment.clone()
    }

    pub fn server_target(&self) -> RpcTarget {
        RpcTarget::Server {
            server_id: self.server_id.clone(),
        }
    }

    pub fn session_target(&self) -> Result<RpcTarget, CallError> {
        self.attachment()
            .map(RpcTarget::Session)
            .ok_or_else(|| CallError::Disconnected("Session is not attached".into()))
    }

    /// Send one call; its response is awaited with [`Connection::wait`].
    pub fn start(&self, target: RpcTarget, service: &str, member: &str, args: Json) -> Result<String, CallError> {
        let id = format!("request-{}", self.requests.fetch_add(1, Ordering::SeqCst) + 1);
        let call = json!({"serviceId": service, "member": member, "args": args});
        self.write(&ClientMessage::Request {
            id: id.clone(),
            target,
            call: to_cbor(&call),
        })?;
        Ok(id)
    }

    /// Cancel request `id` (sent to `target`): the server aborts it if it is still active, and its response is then
    /// the `cancelled` error (client.ts sends `{type: "cancel", id, target}` when a call's signal aborts).
    pub fn cancel(&self, target: RpcTarget, id: &str) -> Result<(), CallError> {
        self.write(&ClientMessage::Cancel {
            id: id.to_string(),
            target,
        })
    }

    /// Send several calls in one write (as a pipelining client does in one tick); returns their request ids.
    pub fn start_all(&self, calls: Vec<(RpcTarget, &str, &str, Json)>) -> Result<Vec<String>, CallError> {
        let mut frames = Vec::new();
        let mut ids = Vec::new();
        for (target, service, member, args) in calls {
            let id = format!("request-{}", self.requests.fetch_add(1, Ordering::SeqCst) + 1);
            let call = json!({"serviceId": service, "member": member, "args": args});
            let message = ClientMessage::Request {
                id: id.clone(),
                target,
                call: to_cbor(&call),
            };
            frames.extend(
                encode_client_message(&message, DEFAULT_MAX_FRAME_LENGTH).map_err(|e| CallError::Disconnected(e.0))?,
            );
            ids.push(id);
        }
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(&frames)
            .map_err(|e| CallError::Disconnected(e.to_string()))?;
        Ok(ids)
    }

    pub fn wait(&self, id: &str) -> Result<Option<Json>, CallError> {
        let (lock, changed) = &*self.shared;
        let mut shared = lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(result) = shared.responses.remove(id) {
                return result.map_err(CallError::Server);
            }
            if let Some(reason) = &shared.closed {
                return Err(CallError::Disconnected(reason.clone()));
            }
            shared = changed.wait(shared).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// [`Connection::wait`] for at most `timeout`; `None` when the response has not arrived.
    pub fn wait_for(&self, id: &str, timeout: Duration) -> Option<Result<Option<Json>, CallError>> {
        let deadline = Instant::now() + timeout;
        let (lock, changed) = &*self.shared;
        let mut shared = lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(result) = shared.responses.remove(id) {
                return Some(result.map_err(CallError::Server));
            }
            if let Some(reason) = &shared.closed {
                return Some(Err(CallError::Disconnected(reason.clone())));
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            shared = changed
                .wait_timeout(shared, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// The connection ended (the server went away or closed it).
    pub fn is_closed(&self) -> bool {
        self.lock().closed.is_some()
    }

    pub fn call(&self, target: RpcTarget, service: &str, member: &str, args: Json) -> Result<Option<Json>, CallError> {
        let id = self.start(target, service, member, args)?;
        self.wait(&id)
    }

    /// Start a subscription; returns `(subscription id, request id)`.
    pub fn start_subscribe(&self, target: RpcTarget, service: &str) -> Result<(String, String), CallError> {
        let subscription = format!("service-{}", self.subscriptions.fetch_add(1, Ordering::SeqCst) + 1);
        let mut shared = self.lock();
        shared.early.insert(subscription.clone(), Vec::new());
        shared.services.insert(subscription.clone(), service.to_string());
        drop(shared);
        let request = self.start(
            target,
            "$chord.service",
            "subscribe",
            json!([subscription, service, "singleton"]),
        )?;
        Ok((subscription, request))
    }

    /// Finish a subscription: hydrate its replica from the snapshot and apply updates that came before it.
    pub fn finish_subscribe(&self, subscription: &str, request: &str) -> Result<(), CallError> {
        let snapshot = self.wait(request)?.unwrap_or(Json::Null);
        let service = self.lock().services.remove(subscription).unwrap_or_default();
        let mut replica = Replica::hydrate(&service, &snapshot).map_err(CallError::Disconnected)?;
        let mut shared = self.lock();
        for update in shared.early.remove(subscription).unwrap_or_default() {
            replica.receive(&update).map_err(CallError::Disconnected)?;
        }
        shared.replicas.insert(subscription.to_string(), replica);
        Ok(())
    }

    pub fn subscribe(&self, target: RpcTarget, service: &str) -> Result<String, CallError> {
        let (subscription, request) = self.start_subscribe(target, service)?;
        self.finish_subscribe(&subscription, &request)?;
        Ok(subscription)
    }

    pub fn start_unsubscribe(&self, target: RpcTarget, subscription: &str) -> Result<String, CallError> {
        self.start(target, "$chord.service", "unsubscribe", json!([subscription]))
    }

    pub fn forget(&self, subscription: &str) {
        let mut shared = self.lock();
        shared.replicas.remove(subscription);
        shared.early.remove(subscription);
        shared.services.remove(subscription);
    }

    /// The current value of a subscription's state member.
    pub fn state(&self, subscription: &str, member: &str) -> Option<Json> {
        self.lock()
            .replicas
            .get(subscription)
            .and_then(|r| r.slots.get(member))
            .and_then(|slot| slot.value.clone())
    }

    /// Take the errors the subscription's consumer reported (`onError`): a sequence gap, an update before hydration,
    /// a non-base snapshot, an update that does not apply. Each cleared only its own member.
    pub fn take_replica_errors(&self, subscription: &str) -> Vec<String> {
        self.lock()
            .replicas
            .get_mut(subscription)
            .map(|r| std::mem::take(&mut r.errors))
            .unwrap_or_default()
    }

    pub fn listen(&self, subscription: &str, listener: UpdateListener) {
        if let Some(replica) = self.lock().replicas.get_mut(subscription) {
            replica.listener = Some(listener);
        }
    }

    pub fn unlisten(&self, subscription: &str) {
        if let Some(replica) = self.lock().replicas.get_mut(subscription) {
            replica.listener = None;
        }
    }

    /// Wait until the attachment satisfies `done`.
    pub fn wait_attachment(&self, done: impl Fn(Option<&SessionTarget>) -> bool) -> Result<(), CallError> {
        let (lock, changed) = &*self.shared;
        let mut shared = lock.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if done(shared.attachment.as_ref()) {
                return Ok(());
            }
            if let Some(reason) = &shared.closed {
                return Err(CallError::Disconnected(reason.clone()));
            }
            shared = changed.wait(shared).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub fn disconnect(&self) {
        self.writer.lock().unwrap_or_else(|e| e.into_inner()).shutdown();
    }
}

fn dispatch(shared: &Arc<(Mutex<Shared>, Condvar)>, message: ServerMessage) {
    let (lock, changed) = &**shared;
    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
    // A broken operation stream failed the connection (`connection.fail`): nothing after it is delivered.
    if state.closed.is_some() {
        return;
    }
    match message {
        ServerMessage::Response { id, result } => {
            state.responses.insert(id, result.map(|r| r.as_ref().map(to_json)));
        }
        ServerMessage::Attachment { attachment } => state.attachment = attachment,
        ServerMessage::ServiceUpdate {
            subscription_id,
            update,
        } => {
            let update = to_json(&update);
            if let Some(early) = state.early.get_mut(&subscription_id) {
                early.push(update);
            } else if let Some(replica) = state.replicas.get_mut(&subscription_id)
                && let Err(error) = replica.receive(&update)
            {
                state.closed = Some(error);
            }
        }
        ServerMessage::Hello { .. } | ServerMessage::HelloError { .. } => {
            state.closed = Some("Unexpected handshake message".into());
        }
    }
    drop(state);
    changed.notify_all();
}

/// What `runClient` is asked to do (`ClientCommand`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientCommand {
    pub session_id: Option<String>,
    pub name: Option<String>,
    pub continue_: bool,
    pub prompt: Option<String>,
    pub fork_from_session_id: Option<String>,
    pub no_session: bool,
    /// The prompt's images (`@file` images): `{type: "image", mimeType, data}`.
    pub images: Option<Vec<Json>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientResult {
    List(Vec<(String, String)>),
    Attached {
        server_id: String,
        session_id: String,
    },
    Prompted {
        server_id: String,
        session_id: String,
        text: String,
    },
}

impl ClientResult {
    /// `JSON.stringify(result)`, as `--mode json` prints it.
    pub fn to_json(&self) -> Json {
        match self {
            ClientResult::List(sessions) => json!({"kind": "list", "sessions": sessions.iter()
                .map(|(server, session)| json!({"serverId": server, "sessionId": session})).collect::<Vec<_>>()}),
            ClientResult::Attached { server_id, session_id } => {
                json!({"kind": "attached", "serverId": server_id, "sessionId": session_id})
            }
            ClientResult::Prompted {
                server_id,
                session_id,
                text,
            } => json!({"kind": "prompted", "serverId": server_id, "sessionId": session_id, "text": text}),
        }
    }
}

/// A server reached at its socket path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub server_id: String,
    pub path: PathBuf,
}

fn message_text(message: &Json) -> String {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["type"] == "text")
        .filter_map(|c| c["text"].as_str())
        .collect()
}

fn fail(error: CallError) -> String {
    error.to_string()
}

/// Told every transcript event of a prompted session (`onEvent`).
pub type EventListener = Arc<dyn Fn(&Json) + Send + Sync>;

/// How a prompted client survives interruptions.
#[derive(Clone, Default)]
pub struct RunOptions {
    /// Set when the user interrupts (SIGINT/SIGTERM): the prompted turn is aborted, and the client still waits for
    /// it to end, as Pi's print mode does.
    pub interrupt: Option<Arc<AtomicBool>>,
    /// The server went away: reach it again (restarting it), for rejoining the session.
    pub reconnect: Option<Arc<dyn Fn() -> Result<Route, String> + Send + Sync>>,
    /// Every transcript event of the prompted session as it arrives (`onEvent`).
    pub on_event: Option<EventListener>,
    /// A Rust addition: the client's own session options (`ultron durable --model ...`), sent with each attach so
    /// the session's worker starts on them on a server whose workers start on other options.
    pub session_options: Option<Json>,
}

/// The session operations service of the Rust worker (crates/daemon `worker::OPS_SERVICE`).
pub const OPS_SERVICE: &str = "ultron.session-ops";
/// Server error codes that mean the session's worker is gone for now and is being brought back.
pub const LOST_CODES: [&str; 2] = ["session_worker_lost", "session_reconnecting"];
/// How long a client tries to rejoin its session after losing its worker or server.
pub const REJOIN_TIMEOUT: Duration = Duration::from_secs(120);

/// The call failed because the worker or the server went away (the session is durable: rejoin it).
pub fn is_lost(error: &CallError) -> bool {
    match error {
        CallError::Disconnected(_) => true,
        CallError::Server(error) => LOST_CODES.contains(&error.code.as_str()),
    }
}

fn interrupted(options: &RunOptions) -> bool {
    options.interrupt.as_ref().is_some_and(|i| i.load(Ordering::SeqCst))
}

fn ops(connection: &Connection, session: &RpcTarget, op: Json) -> Result<Option<Json>, CallError> {
    connection.call(session.clone(), OPS_SERVICE, "op", json!([op]))
}

/// Rejoin `session_id` after its worker or server went away: attach (the server re-attaches clients of a restarted
/// worker by itself; attaching again is harmless), let the worker recover what was interrupted, follow the
/// session until it is idle with nothing queued, and answer its last assistant text.
pub fn rejoin(connection: &Connection, session_id: &str, options: &RunOptions) -> Result<String, CallError> {
    let deadline = Instant::now() + REJOIN_TIMEOUT;
    let server = connection.server_target();
    let session = loop {
        let attached = connection
            .call(
                server.clone(),
                "pi.session-management",
                "attach",
                match &options.session_options {
                    Some(session_options) => json!([session_id, session_options]),
                    None => json!([session_id]),
                },
            )
            .and_then(|_| {
                connection.wait_attachment(|a| a.is_some_and(|a| a.session_id == session_id))?;
                connection.session_target()
            });
        match attached {
            Ok(session) => break session,
            Err(CallError::Server(error)) if error.code == "session_not_found" => {
                return Err(CallError::Server(error));
            }
            Err(error) if connection.is_closed() || Instant::now() >= deadline => return Err(error),
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    };
    // Recovery is idempotent: whichever of the worker and this client asks first starts the recovered run.
    ops(connection, &session, json!({"op": "recover"}))?;
    let mut abort_sent = false;
    loop {
        let state = ops(connection, &session, json!({"op": "state"}))?.unwrap_or_default();
        let busy = state["isStreaming"] == true || state["pendingMessageCount"].as_u64().unwrap_or(0) > 0;
        if !busy || (abort_sent && state["isStreaming"] != true) {
            break;
        }
        if interrupted(options) && !abort_sent {
            abort_sent = true;
            connection.call(session.clone(), "pi.agent-controller", "requestAbort", json!([]))?;
        }
        if Instant::now() >= deadline {
            return Err(CallError::Disconnected(
                "The rejoined session did not become idle".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let text = ops(connection, &session, json!({"op": "lastAssistantText"}))?;
    Ok(text.and_then(|t| t.as_str().map(str::to_string)).unwrap_or_default())
}

/// `runClient` against one server route.
pub fn run_client(command: &ClientCommand, route: &Route) -> Result<ClientResult, String> {
    run_client_with(command, route, &RunOptions::default())
}

/// [`run_client`] that aborts on interrupt and rejoins its session when the worker or the server goes away.
pub fn run_client_with(command: &ClientCommand, route: &Route, options: &RunOptions) -> Result<ClientResult, String> {
    let connection = Connection::connect(&route.path, &route.server_id).map_err(fail)?;
    run_client_on(command, connection, options)
}

/// [`run_client_with`] over an established connection (a Unix socket's, or the Radius relay's); the connection is
/// disconnected when the run ends.
pub fn run_client_on(
    command: &ClientCommand,
    connection: Arc<Connection>,
    options: &RunOptions,
) -> Result<ClientResult, String> {
    run_client_reopening(command, connection, options, None)
}

/// Opens a new connection to the server with the Session attached again, when the connection dropped mid-prompt
/// (a Radius route's reconnector, [`crate::radius::RadiusReopen`]).
pub type Reopen<'a> = &'a (dyn Fn(&str) -> Result<Arc<Connection>, String> + Send + Sync);

/// [`run_client_on`] whose prompt survives its connection dropping: `reopen` brings the connection back, and the
/// client rejoins the Session on it (follows it until idle and answers its last assistant text).
pub fn run_client_reopening(
    command: &ClientCommand,
    connection: Arc<Connection>,
    options: &RunOptions,
    reopen: Option<Reopen<'_>>,
) -> Result<ClientResult, String> {
    let route_server_id = connection.server_id.clone();
    let result = run_operation(command, &connection, options);
    let mut cleanup = Vec::new();
    if let (Ok(Some(created)), true) = (&result.1, command.no_session) {
        let target = connection.server_target();
        let attached = connection.attachment().is_some_and(|a| &a.session_id == created);
        match connection.call(target, "pi.session-management", "remove", json!([created])) {
            Ok(_) if attached => {
                let _ = connection.wait_attachment(|a| a.is_none());
            }
            Ok(_) => {}
            Err(error) => cleanup.push(error.to_string()),
        }
    }
    let (subscriptions, outcome) = (result.0, result.2);
    let outcome = match outcome {
        Err(Failure::Lost { session_id, error }) => {
            recover_lost(&connection, &route_server_id, &session_id, error, options, reopen)
        }
        Err(Failure::Other(error)) => Err(error),
        Ok(result) => Ok(result),
    };
    // Dispose: unsubscribe every open subscription, server-scoped first, all pipelined.
    let mut calls = Vec::new();
    for (subscription, session) in &subscriptions {
        let target = if *session {
            connection.session_target()
        } else {
            Ok(connection.server_target())
        };
        if let Ok(target) = target {
            calls.push((target, "$chord.service", "unsubscribe", json!([subscription])));
        }
    }
    for id in connection.start_all(calls).unwrap_or_default() {
        let _ = connection.wait(&id);
    }
    connection.disconnect();
    match (outcome, cleanup.first()) {
        (Err(error), _) => Err(error),
        (Ok(_), Some(error)) => Err(error.clone()),
        (Ok(result), None) => Ok(result),
    }
}

/// The prompted session's worker or server went away mid-prompt, or the operation failed.
enum Failure {
    Lost { session_id: String, error: CallError },
    Other(String),
}

impl From<String> for Failure {
    fn from(error: String) -> Self {
        Failure::Other(error)
    }
}

/// Rejoin after a lost prompt: on the same connection when it lives (the worker restarted), else on a new
/// connection (`reopen`ed over the Radius relay, or to the restarted server). Answers the prompted result.
fn recover_lost(
    connection: &Arc<Connection>,
    server_id: &str,
    session_id: &str,
    error: CallError,
    options: &RunOptions,
    reopen: Option<Reopen<'_>>,
) -> Result<ClientResult, String> {
    let rejoined = if !connection.is_closed() {
        rejoin(connection, session_id, options)
    } else if let Some(reopen) = reopen {
        let fresh = reopen(session_id)?;
        let rejoined = rejoin(&fresh, session_id, options);
        fresh.disconnect();
        rejoined
    } else if let Some(reconnect) = &options.reconnect {
        let route = reconnect()?;
        let fresh = Connection::connect(&route.path, &route.server_id).map_err(fail)?;
        let rejoined = rejoin(&fresh, session_id, options);
        fresh.disconnect();
        rejoined
    } else {
        Err(error)
    };
    rejoined
        .map(|text| ClientResult::Prompted {
            server_id: server_id.to_string(),
            session_id: session_id.to_string(),
            text,
        })
        .map_err(fail)
}

type Operation = (
    Vec<(String, bool)>,
    Result<Option<String>, String>,
    Result<ClientResult, Failure>,
);

fn run_operation(command: &ClientCommand, connection: &Arc<Connection>, options: &RunOptions) -> Operation {
    let mut subscriptions = Vec::new();
    let mut created = Ok(None);
    let outcome = operation(command, connection, &mut subscriptions, &mut created, options);
    (subscriptions, created, outcome)
}

/// Wait for a prompt's response; an interrupt asks the worker to abort the run (once) and keeps waiting.
fn await_prompt(
    connection: &Connection,
    session: &RpcTarget,
    request: &str,
    options: &RunOptions,
) -> Result<Option<Json>, CallError> {
    let mut abort_sent = false;
    loop {
        if let Some(result) = connection.wait_for(request, Duration::from_millis(100)) {
            return result;
        }
        if interrupted(options) && !abort_sent {
            abort_sent = true;
            let _ = connection.start(session.clone(), "pi.agent-controller", "requestAbort", json!([]));
        }
    }
}

fn operation(
    command: &ClientCommand,
    connection: &Arc<Connection>,
    subscriptions: &mut Vec<(String, bool)>,
    created: &mut Result<Option<String>, String>,
    options: &RunOptions,
) -> Result<ClientResult, Failure> {
    let server = connection.server_target();
    let server_id = connection.server_id.clone();
    let mut started = Vec::new();
    for service in [
        "pi.session-directory",
        "pi.session-management",
        "pi.presentation-plugins",
    ] {
        started.push(connection.start_subscribe(server.clone(), service).map_err(fail)?);
    }
    for (subscription, request) in &started {
        connection.finish_subscribe(subscription, request).map_err(fail)?;
        subscriptions.push((subscription.clone(), false));
    }
    let directory = &started[0].0;
    let sessions = connection
        .state(directory, "state")
        .map(|s| s["sessions"].as_array().cloned().unwrap_or_default())
        .unwrap_or_default();
    let management = |member: &str, args: Json| connection.call(server.clone(), "pi.session-management", member, args);

    let mut session_id = command.session_id.clone();
    if session_id.is_none() && command.prompt.is_none() {
        let mut list: Vec<(String, String)> = sessions
            .iter()
            .map(|s| {
                (
                    server_id.clone(),
                    s["sessionId"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        list.sort();
        return Ok(ClientResult::List(list));
    }
    if session_id.is_none() && command.continue_ {
        let mut candidates = sessions.clone();
        candidates.sort_by(|a, b| {
            let modified = |s: &Json| s["modifiedAt"].as_f64().unwrap_or(0.0);
            modified(b)
                .partial_cmp(&modified(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a["sessionId"].as_str().cmp(&b["sessionId"].as_str()))
        });
        session_id = candidates
            .first()
            .and_then(|s| s["sessionId"].as_str())
            .map(str::to_string);
    }
    let mut create_options = json!({});
    if let Some(fork) = &command.fork_from_session_id {
        create_options["forkFromSessionId"] = json!(fork);
    }
    if let Some(name) = &command.name {
        create_options["name"] = json!(name);
    }
    let session_id = match session_id {
        None => {
            let summary = management("create", json!([create_options]))
                .map_err(fail)?
                .unwrap_or_default();
            let id = summary["sessionId"].as_str().unwrap_or_default().to_string();
            *created = Ok(Some(id.clone()));
            id
        }
        Some(id) => {
            if sessions.iter().any(|s| s["sessionId"] == json!(id)) {
                if command.fork_from_session_id.is_some() {
                    return Err(format!("Cannot fork into existing Session {id}").into());
                }
                if let Some(name) = &command.name {
                    management("rename", json!([id, name])).map_err(fail)?;
                }
            } else {
                if command.prompt.is_none() {
                    return Err(format!("No discovered server contains session {id}").into());
                }
                create_options["id"] = json!(id);
                let mut ordered = json!({"id": id});
                for (key, value) in create_options.as_object().into_iter().flatten() {
                    ordered[key] = value.clone();
                }
                management("create", json!([ordered])).map_err(fail)?;
                *created = Ok(Some(id.clone()));
            }
            id
        }
    };
    connection
        .call(
            server.clone(),
            "pi.presentation-plugins",
            "prepareSession",
            json!([{"sessionId": session_id, "packagePaths": null}]),
        )
        .map_err(fail)?;
    let attach = match &options.session_options {
        Some(session_options) => json!([session_id, session_options]),
        None => json!([session_id]),
    };
    management("attach", attach).map_err(fail)?;
    connection
        .wait_attachment(|a| a.is_some_and(|a| a.session_id == session_id))
        .map_err(fail)?;
    let session = connection.session_target().map_err(fail)?;
    // The session service namespace checks the catalogue, then subscribes to its facades.
    let catalogue = connection
        .start(session.clone(), "$chord.service", "catalogue", json!([]))
        .map_err(fail)?;
    let mut started = Vec::new();
    for service in ["pi.models", "pi.agent-controller", "pi.transcript"] {
        started.push(connection.start_subscribe(session.clone(), service).map_err(fail)?);
    }
    connection.wait(&catalogue).map_err(fail)?;
    for (subscription, request) in &started {
        connection.finish_subscribe(subscription, request).map_err(fail)?;
        subscriptions.push((subscription.clone(), true));
    }
    let Some(prompt) = &command.prompt else {
        return Ok(ClientResult::Attached { server_id, session_id });
    };
    let transcript = started[2].0.clone();
    if connection
        .state(&transcript, "state")
        .is_none_or(|s| s["snapshot"].is_null())
    {
        return Err(Failure::Other("Transcript has no initialized snapshot".into()));
    }
    let (events, received) = mpsc::channel::<Json>();
    let on_event = options.on_event.clone();
    connection.listen(
        &transcript,
        Box::new(move |_, value| {
            if !value["event"].is_null() {
                if let Some(on_event) = &on_event {
                    on_event(&value["event"]);
                }
                let _ = events.send(value["event"].clone());
            }
        }),
    );
    let images = command.images.clone().map_or(Json::Null, Json::Array);
    let response = connection
        .start(
            session.clone(),
            "pi.agent-controller",
            "prompt",
            json!([{"message": prompt, "images": images}]),
        )
        .and_then(|request| await_prompt(connection, &session, &request, options));
    let response = match response {
        Err(error) if is_lost(&error) => {
            connection.unlisten(&transcript);
            return Err(Failure::Lost { session_id, error });
        }
        other => other.map_err(fail),
    };
    let response = response.and_then(|r| r.ok_or_else(|| "Agent prompt returned no response".to_string()));
    let mut completed: HashMap<String, String> = HashMap::new();
    let mut ended = Vec::new();
    let absorb = |event: &Json, completed: &mut HashMap<String, String>, ended: &mut Vec<String>| {
        let run = event["runId"].as_str().map(str::to_string);
        if event["type"] == "message_end"
            && event["message"]["role"] == "assistant"
            && let Some(run) = &run
        {
            completed.insert(run.clone(), message_text(&event["message"]));
        }
        if matches!(event["type"].as_str(), Some("run_end" | "run_suspend"))
            && let Some(run) = run
        {
            ended.push(run);
        }
    };
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            connection.unlisten(&transcript);
            return Err(Failure::Other(error));
        }
    };
    // The prompt was one of the session's commands (`/goal`, `/persona`, `/keepgoing`): its reply, no run.
    if response["command"].is_string() {
        connection.unlisten(&transcript);
        return Ok(ClientResult::Prompted {
            server_id,
            session_id,
            text: response["text"].as_str().unwrap_or_default().to_string(),
        });
    }
    if response["accepted"] == true {
        let operation = response["operationId"].as_str().unwrap_or_default().to_string();
        // Wait for the operation's boundary in the transcript.
        while !ended.contains(&operation) {
            match received.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(event) => absorb(&event, &mut completed, &mut ended),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(error) = connection.wait_attachment(|_| true) {
                        connection.unlisten(&transcript);
                        return Err(Failure::Lost { session_id, error });
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let mut final_run = operation.clone();
        if response["error"].is_null() {
            follow_event_runs(connection, &session, &received, &mut |e| {
                absorb(e, &mut completed, &mut ended)
            });
            if let Some(index) = ended.iter().position(|r| *r == operation)
                && let Some(last) = ended[index + 1..].last()
            {
                final_run = last.clone();
            }
        }
        connection.unlisten(&transcript);
        while let Ok(event) = received.try_recv() {
            absorb(&event, &mut completed, &mut ended);
        }
        if let Some(message) = response["error"]["message"].as_str() {
            return Err(Failure::Other(message.to_string()));
        }
        return Ok(ClientResult::Prompted {
            server_id,
            session_id,
            text: completed.get(&final_run).cloned().unwrap_or_default(),
        });
    }
    connection.unlisten(&transcript);
    Err(Failure::Other(
        response["error"]["message"]
            .as_str()
            .unwrap_or("Prompt was not accepted")
            .to_string(),
    ))
}

/// `followEventRuns`: while the worker reports root-owned work that can re-invoke the model, keep waiting.
fn follow_event_runs(
    connection: &Arc<Connection>,
    session: &RpcTarget,
    received: &mpsc::Receiver<Json>,
    absorb: &mut dyn FnMut(&Json),
) {
    let Ok(control) = connection.subscribe(session.clone(), "ultron.session-control") else {
        return;
    };
    let mut running: Option<String> = None;
    loop {
        while let Ok(event) = received.try_recv() {
            match event["type"].as_str() {
                Some("run_start") => running = event["runId"].as_str().map(str::to_string),
                Some("run_end" | "run_suspend") => running = None,
                _ => {}
            }
            absorb(&event);
        }
        if running.is_some() {
            if let Ok(event) = received.recv_timeout(std::time::Duration::from_millis(200)) {
                if matches!(event["type"].as_str(), Some("run_end" | "run_suspend")) {
                    running = None;
                }
                absorb(&event);
            }
            continue;
        }
        let status = connection.call(
            session.clone(),
            "ultron.session-control",
            "inspect",
            json!(["async.pending", {}]),
        );
        match status {
            Ok(Some(status)) if status["pending"] == true => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            _ => break,
        }
    }
    if let Ok(id) = connection.start_unsubscribe(session.clone(), &control) {
        let _ = connection.wait(&id);
    }
    connection.forget(&control);
}
