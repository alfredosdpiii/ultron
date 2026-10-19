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
    /// The subscriptions made on the session attachment (dropped with it).
    session_subscriptions: Mutex<HashSet<String>>,
    services: Mutex<Option<Arc<Endpoint>>>,
    closed: AtomicBool,
    /// Requests not answered yet, by request id (server.ts `activeRequests`): a `cancel` finds them here.
    active: Mutex<HashMap<String, Active>>,
    next_request: AtomicU64,
    /// The plugin package paths `pi.presentation-plugins.prepareSession` selected for this client (`reload` rebuilds
    /// them); `None` until it prepared a session.
    prepared_plugins: Mutex<Option<Vec<String>>>,
}

/// One request in flight on a connection.
struct Active {
    target: RpcTarget,
    /// Tells this request apart from a later one reusing its id.
    generation: u64,
    /// The worker operation a session call became ([`Manager::cancel_operation`]).
    operation: Option<String>,
    cancelled: bool,
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

    fn active(&self) -> MutexGuard<'_, HashMap<String, Active>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
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
    /// Connections whose session's worker is restarting, by session: they are re-attached when it is back.
    reconnecting: HashMap<u64, String>,
}

/// The code of a session call made while its worker restarts.
pub const RECONNECTING_CODE: &str = "session_reconnecting";
/// How long clients wait for a crashed worker to come back before they are detached.
const RESTART_WAIT: Duration = Duration::from_secs(120);

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
        let (id, path, terminated) = (meta.id.clone(), meta.path.clone(), handle.terminated.clone());
        terminated.on_resolve(Box::new(move |error| {
            if let Some(hub) = hub.upgrade() {
                // Off the reporting thread: releasing talks to the worker manager.
                std::thread::spawn(move || hub.invalidate(&id, &path, error));
            }
        }));
        Ok(handle)
    }

    /// One session service call made by the server itself (a fired schedule, crate::prompt_schedules): the session's
    /// worker is started or adopted as for a client, attached for the call and released after it. Clients attached
    /// to the session see what the call starts live.
    pub fn session_call(&self, session_id: &str, call: Value) -> ServiceResult {
        if self.closing.load(Ordering::SeqCst) {
            return Err(coded("server_draining", "Server is draining"));
        }
        let mut handle = self.serialized(|| self.acquire(session_id))?;
        let attachment = match self.manager.attach_client(&handle) {
            Ok(attachment) => attachment,
            // The worker is retiring: once it is gone, a new one is started for the call.
            Err(_) => {
                let (gone, ended) = std::sync::mpsc::channel();
                handle.terminated.on_resolve(Box::new(move |_| {
                    let _ = gone.send(());
                }));
                let _ = ended.recv_timeout(Duration::from_secs(30));
                // The hub drops the ended worker's entry on its own thread (`invalidate`).
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                while std::time::Instant::now() < deadline
                    && self
                        .router()
                        .hosted
                        .get(session_id)
                        .is_some_and(|h| Arc::ptr_eq(&h.handle.terminated, &handle.terminated))
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                handle = self.serialized(|| self.acquire(session_id))?;
                self.manager.attach_client(&handle)?
            }
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let publish: Publish = Arc::new(|_: &str, _: Value| {});
        self.manager.invoke(
            &handle,
            &attachment,
            call,
            publish,
            Box::new(move |result| {
                let _ = tx.send(result);
            }),
        );
        // A worker that dies under the call rejects it; the bound only guards a hung coordinator.
        let result = rx
            .recv_timeout(Duration::from_secs(600))
            .unwrap_or_else(|_| Err(ServiceError::internal("Session worker did not answer")));
        self.manager.release_attachment(&handle, &attachment);
        result
    }

    /// The worker ended: drop the hosted session and its attachments (clients see `attachment: null`). When the
    /// worker manager restarts it, the clients are re-attached to the new worker (a new attachment ID; session
    /// calls in between fail with `session_reconnecting`).
    fn invalidate(&self, session_id: &str, path: &PathBuf, error: Option<String>) {
        let restarting = error.is_some() && self.manager.restart_pending(path);
        let attachments: Vec<(u64, Attachment)> = {
            let mut router = self.router();
            let Some(hosted) = router.hosted.remove(session_id) else {
                return;
            };
            let attachments: Vec<(u64, Attachment)> = hosted
                .attachments
                .iter()
                .filter_map(|c| router.by_conn.remove(c).map(|a| (*c, a)))
                .collect();
            if restarting {
                for (conn, _) in &attachments {
                    router.reconnecting.insert(*conn, session_id.to_string());
                }
            }
            attachments
        };
        let conns: Vec<u64> = attachments.iter().map(|(c, _)| *c).collect();
        for (conn, attachment) in attachments {
            self.release(conn, attachment, true);
        }
        if let Some(error) = error {
            self.log("[ultron server error]", &error);
        }
        if restarting {
            self.reattach(session_id, path, conns);
        }
    }

    /// Re-attach `conns` to `session_id` once its restarted worker is back (or detach them if it is not).
    fn reattach(&self, session_id: &str, path: &PathBuf, conns: Vec<u64>) {
        let back = self.manager.wait_restart(path, RESTART_WAIT).is_some();
        for id in conns {
            let still = self.router().reconnecting.get(&id).is_some_and(|s| s == session_id);
            let conn = self.conns.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
            let Some(conn) = conn.filter(|_| still) else {
                self.router().reconnecting.remove(&id);
                continue;
            };
            if back {
                let _ = self.serialized(|| self.attach(&conn, session_id));
            }
            self.router().reconnecting.remove(&id);
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
        if let Some(conn) = conn {
            // The attachment's subscriptions end with it.
            let ended: Vec<String> = conn
                .session_subscriptions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .drain()
                .collect();
            let mut subscriptions = conn.subscriptions();
            for subscription in ended {
                subscriptions.remove(&subscription);
            }
            drop(subscriptions);
            if publish {
                conn.send(&ServerMessage::Attachment { attachment: None });
            }
        }
    }

    fn detach(&self, conn: &Arc<Conn>) {
        self.router().reconnecting.remove(&conn.id);
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
                let details = catalog::details(&m);
                let mut listing = m.summary(&self.server_id);
                listing["cwd"] = json!(m.cwd);
                listing["sessionFile"] = json!(m.path.display().to_string());
                listing["name"] = json!(details.name);
                listing["firstMessage"] = json!(details.first_message);
                listing["allMessagesText"] = json!(details.all_messages_text);
                listing["messageCount"] = json!(details.message_count);
                listing["parentSessionId"] = json!(m.parent_session_id);
                listing
            })
            .collect();
        Ok(Some(Value::Array(listings)))
    }

    /// `importPi`: the client sends the Pi file's text (it may live on another machine); it is imported into this
    /// server's session store. A session imported before is answered as `alreadyImported`, nothing written.
    fn import_pi(&self, content: &str) -> ServiceResult {
        let directory = private_temp_dir("ultron-pi-import-")
            .map_err(|e| ServiceError::internal(format!("Pi import directory: {e}")))?;
        let source = directory.join("session.jsonl");
        let written = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&source)
                .and_then(|mut file| file.write_all(content.as_bytes()))
        };
        let imported = written
            .map_err(|e| e.to_string())
            .map(|()| session_core::migration::import_pi_session(&source, &self.session_dir));
        let _ = std::fs::remove_dir_all(&directory);
        let (session_id, already, count) = match imported.map_err(ServiceError::internal)? {
            Ok(result) => (result.session_id, false, result.imported),
            Err(session_core::migration::ImportError::AlreadyImported { session_id, .. }) => (session_id, true, 0),
            Err(error) => return Err(ServiceError::internal(error.to_string())),
        };
        self.refresh();
        let meta = self.resolve(&session_id)?;
        Ok(Some(json!({
            "session": meta.summary(&self.server_id),
            "alreadyImported": already,
            "imported": count,
        })))
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
        let (c1, c2, c3, c4) = (conn_of.clone(), conn_of.clone(), conn_of.clone(), conn_of);
        let (h8, h9) = (hub.clone(), hub.clone());
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
            .method("importPi", move |args| {
                let hub = h8()?;
                let content = args
                    .first()
                    .and_then(|s| s["content"].as_str())
                    .unwrap_or_default()
                    .to_string();
                hub.serialized(|| hub.import_pi(&content))
            })
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
                let (hub, conn) = (h7()?, c3()?);
                let request = args.first().unwrap_or(&Value::Null);
                let id = request["sessionId"].as_str().unwrap_or_default();
                hub.serialized(|| {
                    hub.resolve(id)?;
                    // The selection the client asked for (none: the session's own). Plugin packages are not built
                    // by the Rust server: every selection has no presentation bundles.
                    let paths: Vec<String> = request["packagePaths"]
                        .as_array()
                        .map(|p| p.iter().filter_map(Value::as_str).map(str::to_string).collect())
                        .unwrap_or_default();
                    *conn.prepared_plugins.lock().unwrap_or_else(|e| e.into_inner()) = Some(paths);
                    Ok(Some(presentation_facet_data()))
                })
            })
            .method("reload", move |_| {
                let (hub, conn) = (h9()?, c4()?);
                hub.serialized(|| {
                    if conn
                        .prepared_plugins
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_none()
                    {
                        return Err(ServiceError::internal("No Session plugin selection is prepared"));
                    }
                    Ok(Some(presentation_facet_data()))
                })
            });
        Provider {
            services: vec![
                directory,
                management,
                plugins,
                crate::prompt_schedules::server_service(self.me.clone()),
            ],
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
            session_subscriptions: Mutex::new(HashSet::new()),
            services: Mutex::new(None),
            closed: AtomicBool::new(false),
            active: Mutex::new(HashMap::new()),
            next_request: AtomicU64::new(1),
            prepared_plugins: Mutex::new(None),
        });
        self.conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(conn.id, conn.clone());
        self.connection_count_changed();
        self.read_loop(&conn, stream);
        // Disconnect: release the attachment (without publishing) and the server services.
        conn.close();
        self.router().reconnecting.remove(&conn.id);
        let attachment = self.router().by_conn.remove(&conn.id);
        if let Some(attachment) = attachment {
            let hub = self.clone();
            let id = conn.id;
            std::thread::spawn(move || hub.release(id, attachment, false));
        }
        if let Some(endpoint) = conn.services.lock().unwrap_or_else(|e| e.into_inner()).take() {
            endpoint.dispose();
        }
        // Calls still running stop waiting (server.ts `disconnect` aborts them); their workers are told.
        let operations: Vec<String> = conn.active().drain().filter_map(|(_, a)| a.operation).collect();
        for operation in operations {
            self.manager.cancel_operation(&operation);
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
                    (true, ClientMessage::Cancel { id, target }) => self.cancel(conn, &id, &target),
                    (true, ClientMessage::Request { id, target, call }) => self.request(conn, id, target, call),
                }
            }
        }
    }

    /// `handleCancel`: abort an active request with the same target. A session call stops waiting for its worker
    /// and answers `cancelled`; a server call runs to completion (server services answer before the next message
    /// is read, so one is never active when a cancel arrives).
    fn cancel(&self, conn: &Arc<Conn>, id: &str, target: &RpcTarget) {
        if target.server_id() != self.server_id {
            return;
        }
        let operation = match conn.active().get_mut(id) {
            Some(active) if &active.target == target => {
                active.cancelled = true;
                active.operation.clone()
            }
            _ => return,
        };
        if let Some(operation) = operation {
            self.manager.cancel_operation(&operation);
        }
    }

    fn request(self: &Arc<Self>, conn: &Arc<Conn>, id: String, target: RpcTarget, call: protocol::Value) {
        if conn.active().contains_key(&id) {
            return respond(
                conn,
                &id,
                None,
                Err(coded("invalid_request", "Request ID is already active")),
                self,
            );
        }
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
        let generation = conn.next_request.fetch_add(1, Ordering::SeqCst);
        conn.active().insert(
            id.clone(),
            Active {
                target: target.clone(),
                generation,
                operation: None,
                cancelled: false,
            },
        );
        let finish = {
            let (conn, hub, id) = (conn.clone(), self.clone(), id.clone());
            let subscription = subscription.clone();
            move |result: ServiceResult| {
                let cancelled = {
                    let mut active = conn.active();
                    match active.get(&id) {
                        Some(a) if a.generation == generation => active.remove(&id).is_some_and(|a| a.cancelled),
                        _ => false,
                    }
                };
                // A request aborted by its client answers `cancelled` unless it completed anyway.
                let result = match result {
                    Err(_) if cancelled => {
                        Err(coded(crate::workers::CANCELLED_CODE, crate::workers::CANCELLED_MESSAGE))
                    }
                    other => other,
                };
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
                    if self.router().reconnecting.get(&conn.id) == Some(&session.session_id) {
                        return finish(Err(coded(
                            RECONNECTING_CODE,
                            "The session worker is restarting; this client is re-attached when it is back",
                        )));
                    }
                    return finish(Err(coded(
                        "session_not_attached",
                        "Session is not attached to this client",
                    )));
                };
                if let Some(sub) = &subscription {
                    conn.session_subscriptions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(sub.clone());
                }
                let operation =
                    self.manager
                        .invoke(&handle, &attachment, call_json, conn.publisher(), Box::new(finish));
                // A cancel that came while the call was being sent applies now.
                let cancel_now = operation.and_then(|operation| {
                    let mut active = conn.active();
                    let entry = active.get_mut(&id).filter(|a| a.generation == generation)?;
                    entry.operation = Some(operation.clone());
                    entry.cancelled.then_some(operation)
                });
                if let Some(operation) = cancel_now {
                    self.manager.cancel_operation(&operation);
                }
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

/// `createPresentationFacetData` of the selection's bundles (none in the Rust build).
fn presentation_facet_data() -> Value {
    json!({"presentationFacetBundles": []})
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

/// A new private (0700) directory under the system temp directory.
fn private_temp_dir(prefix: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    loop {
        let mut bytes = [0u8; 6];
        getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
        let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("{prefix}{suffix}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
}
