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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};

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

struct Replica {
    states: HashMap<String, (Decoder, Json)>,
    listener: Option<UpdateListener>,
}

impl Replica {
    fn hydrate(snapshot: &Json) -> Result<Replica, String> {
        let mut states = HashMap::new();
        for instance in snapshot["instances"].as_array().into_iter().flatten() {
            for member in instance["members"].as_array().into_iter().flatten() {
                if member["kind"] != "state" {
                    continue;
                }
                let mut decoder = Decoder::default();
                let ops = decoder
                    .decode(member["ops"].as_array().map(Vec::as_slice).unwrap_or_default())
                    .map_err(|e| e.to_string())?;
                let value = chord_delta::apply(Json::Null, ops).map_err(|e| e.to_string())?;
                states.insert(
                    member["name"].as_str().unwrap_or_default().to_string(),
                    (decoder, value),
                );
            }
        }
        Ok(Replica { states, listener: None })
    }

    fn update(&mut self, update: &Json) -> Result<(), String> {
        if update["type"] != "state" {
            return Ok(());
        }
        let member = update["member"].as_str().unwrap_or_default();
        let (decoder, value) = self
            .states
            .get_mut(member)
            .ok_or_else(|| format!("Unknown service state {member}"))?;
        let ops: Vec<Op> = decoder
            .decode(update["ops"].as_array().map(Vec::as_slice).unwrap_or_default())
            .map_err(|e| e.to_string())?;
        *value = chord_delta::apply(std::mem::take(value), ops).map_err(|e| e.to_string())?;
        if let Some(listener) = &self.listener {
            listener(member, value);
        }
        Ok(())
    }
}

#[derive(Default)]
struct Shared {
    responses: HashMap<String, Result<Option<Json>, ProtocolError>>,
    replicas: HashMap<String, Replica>,
    /// Updates of a subscription whose snapshot has not been read yet.
    early: HashMap<String, Vec<Json>>,
    attachment: Option<SessionTarget>,
    closed: Option<String>,
}

/// A handshaken connection to one server; requests are pipelined and answered by a reader thread.
pub struct Connection {
    pub server_id: String,
    writer: Mutex<UnixStream>,
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
        let mut read = stream.try_clone().map_err(disconnected)?;
        let connection = Arc::new(Connection {
            server_id: server_id.to_string(),
            writer: Mutex::new(stream),
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
        while queued.is_empty() {
            let n = read.read(&mut buffer).map_err(disconnected)?;
            if n == 0 {
                return Err(CallError::Disconnected("Byte transport closed".into()));
            }
            queued = decoder.push(&buffer[..n]).map_err(|e| CallError::Disconnected(e.0))?;
        }
        match queued.remove(0) {
            ServerMessage::Hello { server_id: id, .. } if id == server_id => {}
            ServerMessage::HelloError { error } => return Err(CallError::Server(error)),
            _ => return Err(CallError::Disconnected("Expected server hello as first message".into())),
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
            lock.lock().unwrap_or_else(|e| e.into_inner()).closed = Some(reason);
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

    pub fn call(&self, target: RpcTarget, service: &str, member: &str, args: Json) -> Result<Option<Json>, CallError> {
        let id = self.start(target, service, member, args)?;
        self.wait(&id)
    }

    /// Start a subscription; returns `(subscription id, request id)`.
    pub fn start_subscribe(&self, target: RpcTarget, service: &str) -> Result<(String, String), CallError> {
        let subscription = format!("service-{}", self.subscriptions.fetch_add(1, Ordering::SeqCst) + 1);
        self.lock().early.insert(subscription.clone(), Vec::new());
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
        let mut replica = Replica::hydrate(&snapshot).map_err(CallError::Disconnected)?;
        let mut shared = self.lock();
        for update in shared.early.remove(subscription).unwrap_or_default() {
            replica.update(&update).map_err(CallError::Disconnected)?;
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
    }

    /// The current value of a subscription's state member.
    pub fn state(&self, subscription: &str, member: &str) -> Option<Json> {
        self.lock()
            .replicas
            .get(subscription)
            .and_then(|r| r.states.get(member))
            .map(|(_, v)| v.clone())
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
        let _ = self
            .writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown(std::net::Shutdown::Both);
    }
}

fn dispatch(shared: &Arc<(Mutex<Shared>, Condvar)>, message: ServerMessage) {
    let (lock, changed) = &**shared;
    let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
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
                && let Err(error) = replica.update(&update)
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

/// `runClient` against one server route.
pub fn run_client(command: &ClientCommand, route: &Route) -> Result<ClientResult, String> {
    let connection = Connection::connect(&route.path, &route.server_id).map_err(fail)?;
    let result = run_operation(command, &connection);
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

type Operation = (
    Vec<(String, bool)>,
    Result<Option<String>, String>,
    Result<ClientResult, String>,
);

fn run_operation(command: &ClientCommand, connection: &Arc<Connection>) -> Operation {
    let mut subscriptions = Vec::new();
    let mut created = Ok(None);
    let outcome = operation(command, connection, &mut subscriptions, &mut created);
    (subscriptions, created, outcome)
}

fn operation(
    command: &ClientCommand,
    connection: &Arc<Connection>,
    subscriptions: &mut Vec<(String, bool)>,
    created: &mut Result<Option<String>, String>,
) -> Result<ClientResult, String> {
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
                    return Err(format!("Cannot fork into existing Session {id}"));
                }
                if let Some(name) = &command.name {
                    management("rename", json!([id, name])).map_err(fail)?;
                }
            } else {
                if command.prompt.is_none() {
                    return Err(format!("No discovered server contains session {id}"));
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
    management("attach", json!([session_id])).map_err(fail)?;
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
        return Err("Transcript has no initialized snapshot".into());
    }
    let (events, received) = mpsc::channel::<Json>();
    connection.listen(
        &transcript,
        Box::new(move |_, value| {
            if !value["event"].is_null() {
                let _ = events.send(value["event"].clone());
            }
        }),
    );
    let response = connection
        .call(
            session.clone(),
            "pi.agent-controller",
            "prompt",
            json!([{"message": prompt, "images": null}]),
        )
        .map_err(fail);
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
            return Err(error);
        }
    };
    if response["accepted"] == true {
        let operation = response["operationId"].as_str().unwrap_or_default().to_string();
        // Wait for the operation's boundary in the transcript.
        while !ended.contains(&operation) {
            match received.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(event) => absorb(&event, &mut completed, &mut ended),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Err(error) = connection.wait_attachment(|_| true) {
                        connection.unlisten(&transcript);
                        return Err(fail(error));
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
            return Err(message.to_string());
        }
        return Ok(ClientResult::Prompted {
            server_id,
            session_id,
            text: completed.get(&final_run).cloned().unwrap_or_default(),
        });
    }
    connection.unlisten(&transcript);
    Err(response["error"]["message"]
        .as_str()
        .unwrap_or("Prompt was not accepted")
        .to_string())
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
