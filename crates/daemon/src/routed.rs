//! One server generation's protocol endpoint (packages/server server.ts + session-router.ts, with the server
//! services of experimental/services/server.ts): connections over the generation's Unix socket, the handshake,
//! server-scoped services (session directory, management, presentation plugins), session attachments routed to
//! session workers, and per-subscription state encoding.

use crate::catalog::{self, Meta};
use crate::chord::{Call, Endpoint, Provider, Publish, Service, ServiceError, ServiceResult, State, WireEncoder};
use crate::chord::{subscribe_id, unsubscribe_id};
use crate::wire::{to_cbor, to_json};
use crate::workers::{Handle, Manager};
use protocol::{
    ClientMessage, ClientMessageDecoder, DEFAULT_MAX_FRAME_LENGTH, PROTOCOL_VERSION, ProtocolError, RpcTarget,
    ServerMessage, SessionTarget, encode_server_message, is_supported_protocol_version,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

pub const INTERNAL_SERVER_ERROR_MESSAGE: &str = "Internal server error";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(5_000);

enum Subscription {
    /// Updates published before the subscribe response was sent.
    Pending(Vec<Value>),
    Ready(WireEncoder),
}

pub struct Conn {
    id: u64,
    stream: Mutex<UnixStream>,
    subscriptions: Mutex<HashMap<String, Subscription>>,
    services: Mutex<Option<Arc<Endpoint>>>,
    closed: AtomicBool,
}

impl Conn {
    fn send(&self, message: &ServerMessage) -> bool {
        if self.closed.load(Ordering::SeqCst) {
            return false;
        }
        let Ok(frame) = encode_server_message(message, DEFAULT_MAX_FRAME_LENGTH) else {
            self.close();
            return false;
        };
        let written = self.stream.lock().unwrap_or_else(|e| e.into_inner()).write_all(&frame);
        if written.is_err() {
            self.close();
            return false;
        }
        true
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let _ = self
            .stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown(std::net::Shutdown::Both);
    }

    fn subscriptions(&self) -> MutexGuard<'_, HashMap<String, Subscription>> {
        self.subscriptions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Deliver one plain update to a subscription: buffered until its response, encoded after.
    fn publish(&self, subscription: &str, update: Value) {
        let mut subscriptions = self.subscriptions();
        match subscriptions.get_mut(subscription) {
            Some(Subscription::Pending(buffer)) => buffer.push(update),
            Some(Subscription::Ready(encoder)) => {
                if let Ok(update) = encoder.encode_update(&update) {
                    // Sent under the subscriptions lock so updates leave in publication order.
                    self.send(&ServerMessage::ServiceUpdate {
                        subscription_id: subscription.to_string(),
                        update: to_cbor(&update),
                    });
                }
            }
            None => {}
        }
    }

    fn publisher(self: &Arc<Self>) -> Publish {
        let conn = Arc::downgrade(self);
        Arc::new(move |subscription: &str, update: Value| {
            if let Some(conn) = conn.upgrade() {
                conn.publish(subscription, update);
            }
        })
    }
}

struct Hosted {
    handle: Handle,
    attachments: HashSet<u64>,
}

struct Attachment {
    id: String,
    session_id: String,
    handle: Handle,
}

#[derive(Default)]
struct Router {
    hosted: HashMap<String, Hosted>,
    by_conn: HashMap<u64, Attachment>,
}

/// The shared state of one generation's endpoint.
pub struct Hub {
    me: std::sync::Weak<Hub>,
    pub server_id: String,
    pub session_dir: PathBuf,
    pub cwd: String,
    pub manager: Arc<Manager>,
    conns: Mutex<HashMap<u64, Arc<Conn>>>,
    router: Mutex<Router>,
    directory: Arc<State>,
    revision: Mutex<i64>,
    /// Serializes session management (`mutationTail`).
    mutations: Mutex<()>,
    next_conn: AtomicU64,
    closing: AtomicBool,
    on_connections: Box<dyn Fn(usize) + Send + Sync>,
    pub log_path: PathBuf,
}

fn coded(code: &str, message: &str) -> ServiceError {
    ServiceError::coded(code, message)
}

impl Hub {
    pub fn new(
        server_id: String,
        session_dir: PathBuf,
        manager: Arc<Manager>,
        log_path: PathBuf,
        on_connections: Box<dyn Fn(usize) + Send + Sync>,
    ) -> Arc<Hub> {
        let cwd = std::env::current_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        let mut sessions = catalog::list(&session_dir);
        sessions.sort_by(|a, b| a.id.cmp(&b.id).then(a.created_at.cmp(&b.created_at)));
        let sessions: Vec<Value> = sessions.iter().map(|m| m.summary(&server_id)).collect();
        Arc::new_cyclic(|me| Hub {
            me: me.clone(),
            server_id,
            session_dir,
            cwd,
            manager,
            conns: Mutex::new(HashMap::new()),
            router: Mutex::new(Router::default()),
            directory: State::new(json!({"revision": 1, "sessions": sessions})),
            revision: Mutex::new(1),
            mutations: Mutex::new(()),
            next_conn: AtomicU64::new(1),
            closing: AtomicBool::new(false),
            on_connections,
            log_path,
        })
    }

    fn router(&self) -> MutexGuard<'_, Router> {
        self.router.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn list(&self) -> Vec<Meta> {
        let mut sessions = catalog::list(&self.session_dir);
        for meta in self.manager.tracked_sessions() {
            if !sessions.iter().any(|m| m.path == meta.path) {
                sessions.push(meta);
            }
        }
        sessions
    }

    fn summaries(&self) -> Vec<Value> {
        let mut sessions = self.list();
        sessions.sort_by(|a, b| a.id.cmp(&b.id).then(a.created_at.cmp(&b.created_at)));
        sessions.iter().map(|m| m.summary(&self.server_id)).collect()
    }

    /// `refreshNow`: republish the session directory under the next revision.
    pub fn refresh(&self) {
        let sessions = self.summaries();
        let revision = {
            let mut revision = self.revision.lock().unwrap_or_else(|e| e.into_inner());
            *revision += 1;
            *revision
        };
        self.directory.change(|state| {
            state["revision"] = json!(revision);
            state["sessions"] = Value::Array(sessions);
        });
    }

    fn resolve(&self, session_id: &str) -> Result<Meta, ServiceError> {
        catalog::resolve(&self.session_dir, session_id, &self.manager.tracked_sessions())
    }

    pub fn log(&self, label: &str, message: &str) {
        crate::server::record_error(&self.log_path, label, message);
    }

    // ── Session routing ──────────────────────────────────────────────────────

    fn attach(&self, conn: &Arc<Conn>, session_id: &str) -> Result<(), ServiceError> {
        if self.closing.load(Ordering::SeqCst) || conn.closed.load(Ordering::SeqCst) {
            return Err(coded("server_draining", "Server is draining"));
        }
        if self
            .router()
            .by_conn
            .get(&conn.id)
            .is_some_and(|a| a.session_id == session_id)
        {
            return Ok(());
        }
        let handle = self.acquire(session_id)?;
        let previous = self.router().by_conn.remove(&conn.id);
        if let Some(previous) = previous {
            self.release(conn.id, previous, false);
        }
        let attachment = self.manager.attach_client(&handle)?;
        let mut router = self.router();
        match router.hosted.get_mut(session_id) {
            Some(hosted) if !conn.closed.load(Ordering::SeqCst) => {
                hosted.attachments.insert(conn.id);
            }
            _ => {
                drop(router);
                self.manager.release_attachment(&handle, &attachment);
                return Err(coded("server_draining", "Server is draining"));
            }
        }
        router.by_conn.insert(
            conn.id,
            Attachment {
                id: attachment.clone(),
                session_id: session_id.to_string(),
                handle,
            },
        );
        drop(router);
        conn.send(&ServerMessage::Attachment {
            attachment: Some(SessionTarget {
                server_id: self.server_id.clone(),
                session_id: session_id.to_string(),
                attachment_id: attachment,
            }),
        });
        Ok(())
    }

    fn acquire(&self, session_id: &str) -> Result<Handle, ServiceError> {
        if let Some(hosted) = self.router().hosted.get(session_id) {
            return Ok(hosted.handle.clone());
        }
        let meta = self.resolve(session_id)?;
        let handle = self.manager.open_session(&meta)?;
        let mut router = self.router();
        if let Some(hosted) = router.hosted.get(session_id) {
            return Ok(hosted.handle.clone());
        }
        router.hosted.insert(
            meta.id.clone(),
            Hosted {
                handle: handle.clone(),
                attachments: HashSet::new(),
            },
        );
        drop(router);
        let hub = self.me.clone();
        let (id, terminated) = (meta.id.clone(), handle.terminated.clone());
        terminated.on_resolve(Box::new(move |error| {
            if let Some(hub) = hub.upgrade() {
                // Off the reporting thread: releasing talks to the worker manager.
                std::thread::spawn(move || hub.invalidate(&id, error));
            }
        }));
        Ok(handle)
    }

    /// The worker ended: drop the hosted session and its attachments (clients see `attachment: null`).
    fn invalidate(&self, session_id: &str, error: Option<String>) {
        let attachments: Vec<(u64, Attachment)> = {
            let mut router = self.router();
            let Some(hosted) = router.hosted.remove(session_id) else {
                return;
            };
            hosted
                .attachments
                .iter()
                .filter_map(|c| router.by_conn.remove(c).map(|a| (*c, a)))
                .collect()
        };
        for (conn, attachment) in attachments {
            self.release(conn, attachment, true);
        }
        if let Some(error) = error {
            self.log("[ultron server error]", &error);
        }
    }

    fn release(&self, conn_id: u64, attachment: Attachment, publish: bool) {
        self.manager.release_attachment(&attachment.handle, &attachment.id);
        if let Some(hosted) = self.router().hosted.get_mut(&attachment.session_id) {
            hosted.attachments.remove(&conn_id);
        }
        let conn = self
            .conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&conn_id)
            .cloned();
        if let Some(conn) = conn
            && publish
        {
            conn.send(&ServerMessage::Attachment { attachment: None });
        }
    }

    fn detach(&self, conn: &Arc<Conn>) {
        let attachment = self.router().by_conn.remove(&conn.id);
        if let Some(attachment) = attachment {
            self.release(conn.id, attachment, true);
        }
    }

    /// Release every attachment and handle of a session before its file is deleted.
    fn remove_session(&self, session_id: &str) {
        let (hosted, attachments) = {
            let mut router = self.router();
            let Some(hosted) = router.hosted.remove(session_id) else {
                return;
            };
            let attachments: Vec<(u64, Attachment)> = hosted
                .attachments
                .iter()
                .filter_map(|c| router.by_conn.remove(c).map(|a| (*c, a)))
                .collect();
            (hosted, attachments)
        };
        for (conn, attachment) in attachments {
            self.release(conn, attachment, true);
        }
        self.manager.close_session(&hosted.handle);
    }

    // ── Server services ──────────────────────────────────────────────────────

    fn serialized<T>(&self, operation: impl FnOnce() -> T) -> T {
        let _guard = self.mutations.lock().unwrap_or_else(|e| e.into_inner());
        operation()
    }

    fn create(&self, options: &Value) -> ServiceResult {
        let id = options["id"].as_str().map(str::to_string);
        let name = options["name"].as_str();
        let created = match options["forkFromSessionId"].as_str() {
            Some(source) => {
                let source = self.resolve(source)?;
                let path = catalog::ForkPath::from_json(&options["forkPath"]);
                catalog::fork(&self.session_dir, &source, id, name, &path)?
            }
            None => catalog::create(&self.session_dir, &self.cwd, id, name)?,
        };
        self.refresh();
        let mut summary = created.summary(&self.server_id);
        summary["sessionFile"] = json!(created.path.display().to_string());
        Ok(Some(summary))
    }

    fn describe(&self, cwd: Option<&str>) -> ServiceResult {
        let wanted = cwd.map(|c| config::paths::normalize(std::path::Path::new(c)));
        let listings: Vec<Value> = self
            .list()
            .into_iter()
            .filter(|m| {
                wanted
                    .as_ref()
                    .is_none_or(|w| config::paths::normalize(std::path::Path::new(&m.cwd)) == *w)
            })
            .map(|m| {
                let (name, first, count) = catalog::details(&m);
                let mut listing = m.summary(&self.server_id);
                listing["cwd"] = json!(m.cwd);
                listing["sessionFile"] = json!(m.path.display().to_string());
                listing["name"] = json!(name);
                listing["firstMessage"] = json!(first);
                listing["messageCount"] = json!(count);
                listing["parentSessionId"] = json!(m.parent_session_id);
                listing
            })
            .collect();
        Ok(Some(Value::Array(listings)))
    }

    fn server_services(&self, conn: &Arc<Conn>) -> Provider {
        let directory = Service::new("pi.session-directory").state("state", self.directory.clone());
        let weak = self.me.clone();
        let hub = move || weak.upgrade().ok_or_else(|| ServiceError::internal("Server is closed"));
        let conn_weak = Arc::downgrade(conn);
        let conn_of = move || {
            conn_weak
                .upgrade()
                .ok_or_else(|| ServiceError::internal("Connection is closed"))
        };
        let (h1, h2, h3, h4, h5, h6, h7) = (
            hub.clone(),
            hub.clone(),
            hub.clone(),
            hub.clone(),
            hub.clone(),
            hub.clone(),
            hub.clone(),
        );
        let (c1, c2) = (conn_of.clone(), conn_of);
        let management = Service::new("pi.session-management")
            .method("attach", move |args| {
                let (hub, conn) = (h1()?, c1()?);
                let id = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
                hub.serialized(|| hub.attach(&conn, &id)).map(|()| None)
            })
            .method("create", move |args| {
                let hub = h2()?;
                hub.serialized(|| hub.create(args.first().unwrap_or(&Value::Null)))
            })
            .method("describe", move |args| {
                let hub = h3()?;
                hub.describe(args.first().and_then(|o| o["cwd"].as_str()))
            })
            .method("detach", move |_| {
                let (hub, conn) = (h4()?, c2()?);
                hub.serialized(|| hub.detach(&conn));
                Ok(None)
            })
            .unported("importPi")
            .method("remove", move |args| {
                let hub = h5()?;
                let id = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
                hub.serialized(|| {
                    hub.remove_session(&id);
                    let meta = hub.resolve(&id)?;
                    hub.manager.stop_session(&meta);
                    catalog::remove(&meta)?;
                    hub.refresh();
                    Ok(None)
                })
            })
            .method("rename", move |args| {
                let hub = h6()?;
                let id = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
                let name = args.get(1).and_then(Value::as_str).unwrap_or_default().to_string();
                hub.serialized(|| {
                    let meta = hub.resolve(&id)?;
                    // A running worker owns the session file; a second writer would interleave entries.
                    if hub.manager.worker_pids().contains_key(&meta.id) {
                        return Err(ServiceError::coded(
                            "service_invalid_value",
                            format!("Session {id} is open in a worker"),
                        ));
                    }
                    catalog::rename(&meta.path, &name)?;
                    hub.refresh();
                    Ok(None)
                })
            });
        let plugins = Service::new("pi.presentation-plugins")
            .method("prepareSession", move |args| {
                let hub = h7()?;
                let id = args.first().and_then(|r| r["sessionId"].as_str()).unwrap_or_default();
                hub.resolve(id)?;
                // Plugin packages are not ported: every session has the empty selection.
                Ok(Some(json!({"presentationFacetBundles": []})))
            })
            .method("reload", |_| {
                Err(ServiceError::internal("No Session plugin selection is prepared"))
            });
        Provider {
            services: vec![directory, management, plugins],
        }
    }

    // ── Connections ──────────────────────────────────────────────────────────

    fn connection_count_changed(&self) {
        let count = self.conns.lock().unwrap_or_else(|e| e.into_inner()).len();
        (self.on_connections)(count);
    }

    /// Serve connections from `listener` until the hub closes.
    pub fn serve(self: &Arc<Self>, listener: UnixListener) {
        let hub = self.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if hub.closing.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let hub = hub.clone();
                std::thread::spawn(move || hub.connection(stream));
            }
        });
    }

    fn connection(self: Arc<Self>, stream: UnixStream) {
        let Ok(write) = stream.try_clone() else { return };
        let conn = Arc::new(Conn {
            id: self.next_conn.fetch_add(1, Ordering::SeqCst),
            stream: Mutex::new(write),
            subscriptions: Mutex::new(HashMap::new()),
            services: Mutex::new(None),
            closed: AtomicBool::new(false),
        });
        self.conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(conn.id, conn.clone());
        self.connection_count_changed();
        self.read_loop(&conn, stream);
        // Disconnect: release the attachment (without publishing) and the server services.
        conn.close();
        let attachment = self.router().by_conn.remove(&conn.id);
        if let Some(attachment) = attachment {
            let hub = self.clone();
            let id = conn.id;
            std::thread::spawn(move || hub.release(id, attachment, false));
        }
        if let Some(endpoint) = conn.services.lock().unwrap_or_else(|e| e.into_inner()).take() {
            endpoint.dispose();
        }
        self.conns.lock().unwrap_or_else(|e| e.into_inner()).remove(&conn.id);
        self.connection_count_changed();
    }

    fn fail(&self, conn: &Conn, code: &str, message: String) {
        let error = ServerMessage::HelloError {
            error: ProtocolError::new(code, message),
        };
        conn.send(&error);
        conn.close();
    }

    fn read_loop(self: &Arc<Self>, conn: &Arc<Conn>, mut stream: UnixStream) {
        let mut decoder = ClientMessageDecoder::default();
        let mut ready = false;
        let _ = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT));
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = match stream.read(&mut buffer) {
                Ok(0) => {
                    if let Err(error) = decoder.end() {
                        self.log("[ultron server error]", &error.0);
                    }
                    return;
                }
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) if !ready && matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return self.fail(conn, "invalid_request", "Handshake timeout".into());
                }
                Err(_) => return,
            };
            let messages = match decoder.push(&buffer[..read]) {
                Ok(messages) => messages,
                Err(error) => return self.fail(conn, "invalid_request", error.0),
            };
            for message in messages {
                if conn.closed.load(Ordering::SeqCst) {
                    return;
                }
                match (ready, message) {
                    (false, ClientMessage::Hello { version }) => {
                        if !is_supported_protocol_version(version) {
                            let message =
                                format!("Unsupported protocol version {version}; expected {PROTOCOL_VERSION}");
                            return self.fail(conn, "version", message);
                        }
                        let provider = Arc::new(self.server_services(conn));
                        *conn.services.lock().unwrap_or_else(|e| e.into_inner()) =
                            Some(Arc::new(Endpoint::new(provider)));
                        if !conn.send(&ServerMessage::Hello {
                            version: PROTOCOL_VERSION,
                            server_id: self.server_id.clone(),
                        }) {
                            return;
                        }
                        ready = true;
                        let _ = stream.set_read_timeout(None);
                    }
                    (false, _) => {
                        return self.fail(conn, "invalid_request", "The first client message must be hello".into());
                    }
                    (true, ClientMessage::Hello { .. }) => {
                        return self.fail(
                            conn,
                            "invalid_request",
                            "hello may only be sent as the first message".into(),
                        );
                    }
                    // ponytail: cancellation is not ported; calls run to completion.
                    (true, ClientMessage::Cancel { .. }) => {}
                    (true, ClientMessage::Request { id, target, call }) => self.request(conn, id, target, call),
                }
            }
        }
    }

    fn request(self: &Arc<Self>, conn: &Arc<Conn>, id: String, target: RpcTarget, call: protocol::Value) {
        let call_json = to_json(&call).unwrap_or(Value::Null);
        let Some(parsed) = Call::parse(&call_json) else {
            return respond(
                conn,
                &id,
                None,
                Err(coded("invalid_request", "Invalid service call")),
                self,
            );
        };
        let subscription = subscribe_id(&call_json);
        let unsubscribe = unsubscribe_id(&call_json);
        let error = if target.server_id() != self.server_id {
            Some(coded("wrong_server", "Request was addressed to another server"))
        } else if let Some(sub) = &subscription
            && conn.subscriptions().contains_key(sub)
        {
            Some(coded(
                "invalid_request",
                &format!("Duplicate service subscription {sub}"),
            ))
        } else {
            None
        };
        if let Some(error) = error {
            return respond(conn, &id, None, Err(error), self);
        }
        if let Some(sub) = &subscription {
            conn.subscriptions()
                .insert(sub.clone(), Subscription::Pending(Vec::new()));
        }
        let finish = {
            let (conn, hub, id) = (conn.clone(), self.clone(), id.clone());
            let subscription = subscription.clone();
            move |result: ServiceResult| {
                if result.is_ok()
                    && let Some(sub) = &unsubscribe
                {
                    conn.subscriptions().remove(sub);
                }
                respond(&conn, &id, subscription.as_deref(), result, &hub)
            }
        };
        match target {
            RpcTarget::Server { .. } => {
                let endpoint = conn.services.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let result = match endpoint {
                    Some(endpoint) => endpoint.invoke(&parsed, conn.publisher()),
                    None => Err(ServiceError::internal("Server services are not attached")),
                };
                finish(result);
            }
            RpcTarget::Session(session) => {
                let attachment = {
                    let router = self.router();
                    router
                        .by_conn
                        .get(&conn.id)
                        .filter(|a| a.session_id == session.session_id && a.id == session.attachment_id)
                        .map(|a| (a.handle.clone(), a.id.clone()))
                };
                let Some((handle, attachment)) = attachment else {
                    return finish(Err(coded(
                        "session_not_attached",
                        "Session is not attached to this client",
                    )));
                };
                self.manager
                    .invoke(&handle, &attachment, call_json, conn.publisher(), Box::new(finish));
            }
        }
    }

    /// Stop accepting, drop every connection and attachment, and release (or stop) every worker.
    pub fn close(&self) {
        if self.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        self.manager.prepare_shutdown();
        let conns: Vec<Arc<Conn>> = self
            .conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for conn in &conns {
            conn.close();
        }
        let (attachments, hosted): (Vec<(u64, Attachment)>, Vec<Hosted>) = {
            let mut router = self.router();
            (
                router.by_conn.drain().collect(),
                router.hosted.drain().map(|(_, h)| h).collect(),
            )
        };
        for (conn, attachment) in attachments {
            self.release(conn, attachment, false);
        }
        let threads: Vec<_> = hosted
            .into_iter()
            .map(|hosted| {
                let manager = self.manager.clone();
                std::thread::spawn(move || manager.close_session(&hosted.handle))
            })
            .collect();
        for thread in threads {
            let _ = thread.join();
        }
    }
}

fn respond(conn: &Arc<Conn>, id: &str, subscription: Option<&str>, result: ServiceResult, hub: &Hub) {
    let result = result.map_err(|error| match error.code {
        Some(code) => ProtocolError::new(&code, error.message),
        None => {
            hub.log("[ultron server error]", &error.message);
            ProtocolError::new("internal_error", INTERNAL_SERVER_ERROR_MESSAGE)
        }
    });
    let Some(subscription) = subscription else {
        conn.send(&ServerMessage::Response {
            id: id.to_string(),
            result: result.map(|r| r.map(|r| to_cbor(&r))),
        });
        return;
    };
    let mut subscriptions = conn.subscriptions();
    let snapshot = match result {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) | Err(_) => {
            subscriptions.remove(subscription);
            let error = result.err().unwrap_or_else(|| {
                ProtocolError::new("invalid_request", "Service subscription did not return a snapshot")
            });
            conn.send(&ServerMessage::Response {
                id: id.to_string(),
                result: Err(error),
            });
            return;
        }
    };
    let mut encoder = WireEncoder::default();
    let encoded = encoder.encode_snapshot(&snapshot);
    let buffered = match subscriptions.remove(subscription) {
        Some(Subscription::Pending(buffer)) => buffer,
        _ => Vec::new(),
    };
    let Ok(encoded) = encoded else {
        conn.send(&ServerMessage::Response {
            id: id.to_string(),
            result: Err(ProtocolError::new(
                "invalid_request",
                "Invalid service subscription snapshot",
            )),
        });
        return;
    };
    conn.send(&ServerMessage::Response {
        id: id.to_string(),
        result: Ok(Some(to_cbor(&encoded))),
    });
    for update in buffered {
        if let Ok(update) = encoder.encode_update(&update) {
            conn.send(&ServerMessage::ServiceUpdate {
                subscription_id: subscription.to_string(),
                update: to_cbor(&update),
            });
        }
    }
    subscriptions.insert(subscription.to_string(), Subscription::Ready(encoder));
}
