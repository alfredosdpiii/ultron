//! A client's link to one session over the native architecture, as `rpc-native.ts` and `client-tui.ts` hold it:
//! this process's own server generation for the instance (crates/daemon), a client connection (crates/client), the
//! session selected (created, forked or opened) and attached, its services subscribed, and its transcript's
//! lane-watch events forwarded to the caller. Operations go to the session worker's `ultron.session-control.op`
//! (the session actor's [`Op`]s); switching sessions re-attaches, as TypeScript clients do.
//!
//! The link never gives up on its session: a supervisor watches the connection and the attachment. When the
//! session's worker restarts (the server re-attaches this client under a new attachment) or the server itself goes
//! away (a new server generation is started, which brings up a new coordinator), the link re-attaches, subscribes
//! again and reports `{"type": "connection", "state": "reconnecting" | "rejoined" | "lost"}` to its watcher.

use crate::Env;
use crate::args::Args;
use crate::print_mode::{Target, server_options};
use client::radius::{ConnectionState, RadiusClientReconnect, ReconnectClient};
use client::runtime::{CallError, Connection};
use daemon::server::{RunningServer, ServerOptions, start_foreground_server};
use serde_json::{Value, json};
use server::radius_relay::RelayAuthResolver;
use session_core::core::{Op, OpResult};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub(crate) fn fail(error: CallError) -> String {
    daemon::lost::describe(&error.to_string()).unwrap_or_else(|| error.to_string())
}

/// Where a link forwards the attached session's lane-watch events.
pub type WatchSink = Arc<dyn Fn(Value) + Send + Sync>;

/// How often the supervisor looks at the connection.
const SUPERVISE: Duration = Duration::from_millis(100);
/// How long a link keeps trying to rejoin before it reports the session lost.
const REJOIN_TIMEOUT: Duration = client::runtime::REJOIN_TIMEOUT;

/// The `reconnecting` reason when the attached session's worker restarted under the link.
pub const WORKER_RESTARTED: &str = "The session worker restarted";

/// A `connection` watch event.
pub fn connection_event(state: &str, message: Option<&str>) -> Value {
    json!({"type": "connection", "state": state, "message": message})
}

/// A server this client reaches but does not run (`ClientRuntimeRoute`): a Unix socket, or a server hosted through
/// the Radius gateway.
#[derive(Clone)]
pub enum RemoteRoute {
    Unix(client::runtime::Route),
    Radius {
        server_id: String,
        auth: Arc<dyn RelayAuthResolver>,
    },
}

impl RemoteRoute {
    pub fn server_id(&self) -> &str {
        match self {
            RemoteRoute::Unix(route) => &route.server_id,
            RemoteRoute::Radius { server_id, .. } => server_id,
        }
    }

    /// `unix` or `radius`.
    pub fn transport(&self) -> &'static str {
        match self {
            RemoteRoute::Unix(_) => "unix",
            RemoteRoute::Radius { .. } => "radius",
        }
    }

    pub fn socket_path(&self) -> Option<&std::path::Path> {
        match self {
            RemoteRoute::Unix(route) => Some(&route.path),
            RemoteRoute::Radius { .. } => None,
        }
    }

    /// A new connection over the route's transport.
    pub fn connect(&self) -> Result<Arc<Connection>, CallError> {
        match self {
            RemoteRoute::Unix(route) => Connection::connect(&route.path, &route.server_id),
            RemoteRoute::Radius { server_id, auth } => Connection::connect_radius(server_id, auth.as_ref()),
        }
    }
}

/// Where a link's server is: this process's own generation (restarted when it goes away), or a remote route.
#[derive(Clone)]
enum Origin {
    Foreground(Box<ServerOptions>),
    /// A client of the durable instance (`ultron durable ...`, crate::durable_client): its detached server,
    /// activated when it is not running, never stopped by this client.
    Durable(Box<ServerOptions>),
    Remote(RemoteRoute),
}

/// The session a remote client opens (`prepareClientSession`): the one named, the most recently modified one, a
/// new one (a fork of `fork` when given); `name` names a new one and renames an existing one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteSelection {
    pub session_id: Option<String>,
    pub continue_: bool,
    pub name: Option<String>,
    pub fork: Option<String>,
}

/// What [`select_remote_session`] decided.
#[derive(Debug, Clone, PartialEq)]
pub enum Selected {
    /// Open this existing session (renaming it first when `rename`).
    Existing { session_id: String, rename: Option<String> },
    /// Create a session with these `pi.session-management.create` options.
    Create(Value),
}

/// `prepareClientSession`'s choice among the server's sessions (`describe` summaries).
pub fn select_remote_session(
    selection: &RemoteSelection,
    sessions: &[Value],
    radius: bool,
) -> Result<Selected, String> {
    let existing = |id: &str| Selected::Existing {
        session_id: id.to_string(),
        rename: selection.name.clone(),
    };
    if let Some(id) = &selection.session_id {
        if sessions.iter().any(|s| s["sessionId"] == json!(id)) {
            return Ok(existing(id));
        }
        if radius {
            return Err(format!("Remote server does not contain Session {id}"));
        }
        let mut options = json!({"id": id});
        if let Some(name) = &selection.name {
            options["name"] = json!(name);
        }
        return Ok(Selected::Create(options));
    }
    if selection.continue_ {
        let text = |s: &Value, key: &str| s[key].as_str().unwrap_or_default().to_string();
        let newest = sessions.iter().min_by(|a, b| {
            let modified = |s: &Value| s["modifiedAt"].as_f64().unwrap_or(0.0);
            modified(b)
                .partial_cmp(&modified(a))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| text(a, "serverId").cmp(&text(b, "serverId")))
                .then_with(|| text(a, "sessionId").cmp(&text(b, "sessionId")))
        });
        if let Some(newest) = newest {
            return Ok(existing(&text(newest, "sessionId")));
        }
    }
    let mut options = json!({});
    if let Some(fork) = &selection.fork {
        options["forkFromSessionId"] = json!(fork);
    }
    if let Some(name) = &selection.name {
        options["name"] = json!(name);
    }
    Ok(Selected::Create(options))
}

/// A Radius recovery's watch events (`ServerConnectionState` and `SessionAttachmentState` of client-tui.ts).
pub fn radius_connection_event(state: ConnectionState) -> Value {
    let state = match state {
        ConnectionState::Connecting => "connecting",
        ConnectionState::Connected => "connected",
        ConnectionState::Disconnected => "disconnected",
    };
    json!({"type": "radius_connection", "state": state})
}

pub fn radius_attachment_event(state: &str, session_id: &str) -> Value {
    json!({"type": "radius_attachment", "state": state, "sessionId": session_id})
}

struct State {
    /// This process's own server generation (none for a remote route).
    server: Option<Arc<RunningServer>>,
    connection: Arc<Connection>,
    session_id: String,
    /// The attachment the subscriptions belong to.
    attachment_id: Option<String>,
    subscriptions: Vec<String>,
    transcript: Option<String>,
    /// A reconnection is under way (operations wait for it).
    reconnecting: bool,
}

struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    origin: Origin,
    events: WatchSink,
    /// A Radius route's reconnector (`RadiusClientReconnect`), and whether it is recovering the connection now.
    radius: Mutex<Option<Arc<RadiusClientReconnect>>>,
    radius_recovering: AtomicBool,
    /// A reconnection attempt is under way (`connecting`).
    radius_connecting: AtomicBool,
    /// A remote Unix route's loss was reported (it has no reconnect).
    lost_reported: AtomicBool,
    closing: AtomicBool,
    /// Serializes attaching (the supervisor and the owner's `attach`).
    attaching: Mutex<()>,
    /// The terminal appearance the TUI reported ([`Link::set_terminal_theme`]): told again to every session this
    /// link attaches (a switch, a restarted worker, a new server generation), which starts without it.
    terminal_theme: Mutex<Option<String>>,
}

/// A call of the attached session, sent again once the link rejoined when it was refused before reaching a worker.
fn session_call(shared: &Shared, service: &str, member: &str, args: Value) -> OpResult {
    let deadline = Instant::now() + REJOIN_TIMEOUT;
    loop {
        shared.wait_reconnected();
        let connection = shared.connection();
        if connection.is_closed() && !shared.lost_reported.load(Ordering::SeqCst) && Instant::now() < deadline {
            // Not sent: wait for the supervisor to reconnect.
            std::thread::sleep(SUPERVISE);
            continue;
        }
        let result = connection
            .session_target()
            .and_then(|session| connection.call(session, service, member, args.clone()));
        match result {
            // Refused before it reached a worker: send it again once the link rejoined.
            Err(CallError::Server(error))
                if error.code == daemon::routed::RECONNECTING_CODE && Instant::now() < deadline =>
            {
                std::thread::sleep(SUPERVISE);
            }
            other => return other.map_err(fail),
        }
    }
}

/// Session operations of a link's attached session, from any thread ([`Link::ops`]).
#[derive(Clone)]
pub struct Ops(Arc<Shared>);

impl Ops {
    /// Run a session operation on the attached session's worker.
    pub fn op(&self, op: &Op) -> OpResult {
        session_call(&self.0, daemon::worker::OPS_SERVICE, "op", json!([op.to_json()]))
    }
}

/// The link as another thread follows it ([`Link::handle`]).
#[derive(Clone)]
pub struct LinkHandle {
    shared: Arc<Shared>,
}

impl LinkHandle {
    pub fn connection(&self) -> Arc<Connection> {
        self.shared.connection()
    }

    pub fn session_id(&self) -> String {
        self.shared.lock().session_id.clone()
    }
}

pub struct Link {
    shared: Arc<Shared>,
    /// Sessions this link created (removed on close under `--no-session`).
    pub created: Vec<String>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn connection(&self) -> Arc<Connection> {
        self.lock().connection.clone()
    }

    fn management(&self, member: &str, args: Value) -> Result<Option<Value>, String> {
        let connection = self.connection();
        connection
            .call(connection.server_target(), "pi.session-management", member, args)
            .map_err(fail)
    }

    /// Drop this link's subscriptions (unsubscribing them while the attachment still serves them).
    fn unsubscribe(&self, state: &mut State) {
        let connection = state.connection.clone();
        if let Some(transcript) = state.transcript.take() {
            connection.unlisten(&transcript);
        }
        for subscription in &state.subscriptions {
            connection.unlisten(subscription);
        }
        let current = connection.attachment().map(|a| a.attachment_id);
        let session = connection
            .session_target()
            .ok()
            .filter(|_| current.is_some() && current == state.attachment_id);
        let pending: Vec<String> = state
            .subscriptions
            .drain(..)
            .filter_map(|s| {
                connection.forget(&s);
                session
                    .as_ref()
                    .and_then(|session| connection.start_unsubscribe(session.clone(), &s).ok())
            })
            .collect();
        for id in pending {
            let _ = connection.wait(&id);
        }
        state.attachment_id = None;
    }

    /// Attach to session `id` (replacing the current one): its services subscribed, its transcript followed.
    fn attach(&self, id: &str) -> Result<(), String> {
        let _attaching = self.attaching.lock().unwrap_or_else(|e| e.into_inner());
        let connection = {
            let mut state = self.lock();
            self.unsubscribe(&mut state);
            state.connection.clone()
        };
        let server = connection.server_target();
        connection
            .call(
                server,
                "pi.presentation-plugins",
                "prepareSession",
                json!([{"sessionId": id, "packagePaths": null}]),
            )
            .map_err(fail)?;
        self.management("attach", json!([id]))?;
        connection
            .wait_attachment(|a| a.is_some_and(|a| a.session_id == id))
            .map_err(fail)?;
        self.subscribe(&connection, id)?;
        // The session's worker starts without the terminal's appearance: tell it (best effort; the next report
        // tells it again).
        let theme = self.terminal_theme.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(theme) = theme {
            let op = Op::SetTerminalTheme { theme };
            let _ = connection
                .session_target()
                .and_then(|session| connection.call(session, daemon::worker::OPS_SERVICE, "op", json!([op.to_json()])));
        }
        let reconnector = self.radius.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(reconnector) = reconnector {
            reconnector.attachment_changed(Some(id));
        }
        Ok(())
    }

    /// Subscribe the attached session's services and follow its transcript.
    fn subscribe(&self, connection: &Arc<Connection>, id: &str) -> Result<(), String> {
        let attachment = connection.attachment().map(|a| a.attachment_id);
        let session = connection.session_target().map_err(fail)?;
        let mut started = Vec::new();
        for service in ["pi.models", "pi.agent-controller", "pi.transcript"] {
            started.push(connection.start_subscribe(session.clone(), service).map_err(fail)?);
        }
        let mut subscriptions = Vec::new();
        for (subscription, request) in &started {
            connection.finish_subscribe(subscription, request).map_err(fail)?;
            subscriptions.push(subscription.clone());
        }
        // The session's name and run health, as they change (the Rust-only operations service).
        let ops = connection
            .subscribe(session, daemon::worker::OPS_SERVICE)
            .map_err(fail)?;
        let ops_events = self.events.clone();
        connection.listen(
            &ops,
            Box::new(move |member, value| match member {
                "name" => ops_events(json!({"type": "session_name", "name": value["name"]})),
                "health" if value["stuck"] == true => ops_events(json!({"type": "run_stuck", "health": value})),
                _ => {}
            }),
        );
        subscriptions.push(ops);
        let transcript = started[2].0.clone();
        let events = self.events.clone();
        connection.listen(
            &transcript,
            Box::new(move |_, value| {
                if !value["event"].is_null() {
                    events(value["event"].clone());
                }
            }),
        );
        let mut state = self.lock();
        state.subscriptions = subscriptions;
        state.transcript = Some(transcript);
        state.session_id = id.to_string();
        state.attachment_id = attachment;
        Ok(())
    }

    fn set_reconnecting(&self, reconnecting: bool) {
        self.lock().reconnecting = reconnecting;
        self.changed.notify_all();
    }

    /// Wait (bounded) while a reconnection is under way or about to start (the supervisor has yet to notice a
    /// lost server or a moved attachment).
    fn wait_reconnected(&self) {
        let deadline = Instant::now() + REJOIN_TIMEOUT;
        loop {
            let reconnecting = self.lock().reconnecting;
            if self.closing.load(Ordering::SeqCst)
                || self.lost_reported.load(Ordering::SeqCst)
                || Instant::now() >= deadline
                || (!reconnecting && self.needs_rejoin().is_none())
            {
                return;
            }
            let state = self.lock();
            drop(
                self.changed
                    .wait_timeout(state, SUPERVISE)
                    .unwrap_or_else(|e| e.into_inner()),
            );
        }
    }

    /// What the supervisor sees: whether the link lost its server, or its attachment moved or ended.
    fn needs_rejoin(&self) -> Option<&'static str> {
        let state = self.lock();
        if state.session_id.is_empty() || state.attachment_id.is_none() && !state.reconnecting {
            return None;
        }
        if state.connection.is_closed() {
            return Some("The Ultron server went away");
        }
        let current = state.connection.attachment();
        let moved = current.as_ref().map(|a| (&a.session_id, &a.attachment_id))
            != Some((&state.session_id, state.attachment_id.as_ref()?));
        moved.then_some(WORKER_RESTARTED)
    }

    /// Rejoin the session: a new server generation (and connection) if the server is gone, then attach and
    /// subscribe again. Retries until it works or [`REJOIN_TIMEOUT`] passes.
    fn rejoin(&self, reason: &str) -> Result<(), String> {
        self.set_reconnecting(true);
        (self.events)(connection_event("reconnecting", Some(reason)));
        let deadline = Instant::now() + REJOIN_TIMEOUT;
        let mut delay = Duration::from_millis(100);
        let result = loop {
            if self.closing.load(Ordering::SeqCst) {
                break Err("The link is closing".to_string());
            }
            match self.try_rejoin() {
                Ok(()) => break Ok(()),
                Err(error) if Instant::now() >= deadline => break Err(error),
                Err(_) => {
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
            }
        };
        self.set_reconnecting(false);
        match &result {
            Ok(()) => (self.events)(connection_event("rejoined", None)),
            Err(error) => (self.events)(connection_event(
                "lost",
                Some(&daemon::lost::lost_server_message(Some(error))),
            )),
        }
        result
    }

    fn try_rejoin(&self) -> Result<(), String> {
        let (closed, id) = {
            let state = self.lock();
            (state.connection.is_closed(), state.session_id.clone())
        };
        if closed {
            if let Origin::Durable(options) = &self.origin {
                // The durable server again (activated when it is gone).
                let route = crate::durable_client::activate(options)?;
                let connection = Connection::connect(&route.path, &route.server_id).map_err(fail)?;
                self.replace_connection(connection, None);
                return self.attach(&id);
            }
            let Origin::Foreground(options) = &self.origin else {
                return Err("The server connection closed".into());
            };
            let server = start_foreground_server(options).map_err(|e| e.to_string())?;
            let connection = Connection::connect(&server.endpoint_path, &server.server_id).map_err(fail)?;
            let old = self.replace_connection(connection, Some(server));
            // The old generation already closed when it lost its coordinator; this only waits for that.
            if let Some(old) = old {
                std::thread::spawn(move || old.close());
            }
        }
        self.attach(&id)
    }

    /// Use `connection` from now on (and `server`, when this process started a new generation): the old
    /// connection's subscriptions are gone with it. Returns the replaced generation.
    fn replace_connection(
        &self,
        connection: Arc<Connection>,
        server: Option<Arc<RunningServer>>,
    ) -> Option<Arc<RunningServer>> {
        let mut state = self.lock();
        state.connection = connection;
        state.subscriptions.clear();
        state.transcript = None;
        state.attachment_id = None;
        match server {
            Some(server) => state.server.replace(server),
            None => None,
        }
    }

    fn radius_route(&self) -> Option<&RemoteRoute> {
        match &self.origin {
            Origin::Remote(route @ RemoteRoute::Radius { .. }) => Some(route),
            _ => None,
        }
    }

    /// The Radius connection closed: report it and let the reconnector bring it back (`RadiusClientReconnect`:
    /// after 1 s, doubling to 30 s per failure), then re-attach the session. Operations wait meanwhile.
    fn recover_radius(self: &Arc<Self>) {
        if self.radius_recovering.swap(true, Ordering::SeqCst) {
            return;
        }
        self.set_reconnecting(true);
        (self.events)(radius_connection_event(ConnectionState::Disconnected));
        let reconnector = self.radius.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(reconnector) = reconnector {
            reconnector.connection_state_changed(ConnectionState::Disconnected);
        }
    }

    /// A Radius route's reconnector over this link.
    fn start_radius_reconnector(self: &Arc<Self>) {
        if self.radius_route().is_none() {
            return;
        }
        let client: Arc<dyn ReconnectClient> = Arc::new(RadiusLink(Arc::downgrade(self)));
        let shared = Arc::downgrade(self);
        let reattach: client::radius::Reattach = Arc::new(move |session_id: &str| {
            let shared = shared.upgrade().ok_or("The link is closed")?;
            (shared.events)(radius_attachment_event("attaching", session_id));
            shared.attach(session_id)?;
            shared.radius_recovering.store(false, Ordering::SeqCst);
            shared.set_reconnecting(false);
            (shared.events)(radius_attachment_event("attached", session_id));
            Ok(())
        });
        let session = Some(self.lock().session_id.clone()).filter(|id| !id.is_empty());
        let reconnector = RadiusClientReconnect::new(client, session, reattach, None);
        *self.radius.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(reconnector));
    }

    /// Stop a Radius route's reconnector (`dispose()`).
    fn stop_radius_reconnector(&self) {
        let reconnector = self.radius.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(reconnector) = reconnector {
            reconnector.dispose();
        }
    }
}

/// The link as the Radius reconnector's client (`RadiusReconnectClient`): reconnecting opens a new relay connection
/// for the link; the connection's states are reported to the link's watcher.
struct RadiusLink(std::sync::Weak<Shared>);

impl ReconnectClient for RadiusLink {
    fn connected(&self) -> bool {
        self.0
            .upgrade()
            .is_some_and(|shared| !shared.radius_connecting.load(Ordering::SeqCst) && !shared.connection().is_closed())
    }

    fn connection_state(&self) -> ConnectionState {
        match self.0.upgrade() {
            Some(shared) if shared.radius_connecting.load(Ordering::SeqCst) => ConnectionState::Connecting,
            Some(shared) if !shared.connection().is_closed() => ConnectionState::Connected,
            _ => ConnectionState::Disconnected,
        }
    }

    fn disconnect(&self, _reason: &str) {
        if let Some(shared) = self.0.upgrade() {
            shared.connection().disconnect();
            if !shared.closing.load(Ordering::SeqCst) {
                (shared.events)(radius_connection_event(ConnectionState::Disconnected));
            }
        }
    }

    fn reconnect(&self) -> Result<(), String> {
        let shared = self.0.upgrade().ok_or("The link is closed")?;
        let route = shared.radius_route().cloned().ok_or("Not a Radius route")?;
        shared.radius_connecting.store(true, Ordering::SeqCst);
        (shared.events)(radius_connection_event(ConnectionState::Connecting));
        let connected = route.connect();
        shared.radius_connecting.store(false, Ordering::SeqCst);
        match connected {
            Ok(connection) => {
                shared.replace_connection(connection, None);
                (shared.events)(radius_connection_event(ConnectionState::Connected));
                Ok(())
            }
            Err(error) => {
                (shared.events)(radius_connection_event(ConnectionState::Disconnected));
                Err(fail(error))
            }
        }
    }
}

impl Link {
    /// Start the server (a client of the durable instance uses its detached server instead, crate::durable_client),
    /// select `target`'s session and attach to it; transcript events go to `events`. Closing the link never stops a
    /// server it did not start.
    pub fn open(parsed: &Args, env: &Env, target: &Target, mode: &str, events: WatchSink) -> Result<Link, String> {
        let options = server_options(parsed, env, mode)?;
        let durable = crate::durable_client::is_client(env);
        let (server, endpoint, server_id) = match durable {
            true => {
                let route = crate::durable_client::activate(&options)?;
                crate::print_mode::trace_startup(env, "cli.server-attached");
                (None, route.path, route.server_id)
            }
            false => {
                let server = start_foreground_server(&options).map_err(|e| e.to_string())?;
                crate::print_mode::trace_startup(env, "cli.server-started");
                let (endpoint, id) = (server.endpoint_path.clone(), server.server_id.clone());
                (Some(server), endpoint, id)
            }
        };
        // rpc-native.ts's milestones: the client runtime opens after the server started, its services are ready once
        // connected, then the session is selected and attached.
        let rpc_trace = |label: &str| {
            if mode == "rpc" {
                crate::print_mode::trace_startup(env, label);
            }
        };
        rpc_trace("rpc.open");
        let connection = match Connection::connect(&endpoint, &server_id) {
            Ok(connection) => connection,
            Err(error) => {
                if let Some(server) = server {
                    server.close();
                }
                return Err(fail(error));
            }
        };
        let origin = if durable {
            Origin::Durable(Box::new(options))
        } else {
            Origin::Foreground(Box::new(options))
        };
        let mut link = Link::new(server, connection, origin, events);
        rpc_trace("rpc.services-ready");
        let name = parsed.name.as_deref();
        let selected = match target {
            Target::Open(metadata) => {
                if let Some(name) = name {
                    link.management("rename", json!([metadata.id, name]))?;
                }
                Ok(metadata.id.clone())
            }
            Target::New { id } => {
                let mut options = json!({});
                if let Some(id) = id {
                    options["id"] = json!(id);
                }
                if let Some(name) = name {
                    options["name"] = json!(name);
                }
                link.create(options)
            }
            Target::Fork(metadata) => {
                let mut options = json!({"forkFromSessionId": metadata.id});
                if let Some(name) = name {
                    options["name"] = json!(name);
                }
                link.create(options)
            }
            // `--no-session --name x`: the session is removed when the run ends, but named while it lasts.
            Target::Ephemeral => link.create(name.map_or_else(|| json!({}), |name| json!({"name": name}))),
        };
        let attached = selected.and_then(|id| {
            rpc_trace("rpc.session-selected");
            link.attach(&id)
        });
        if let Err(error) = attached {
            link.close(false);
            return Err(error);
        }
        rpc_trace("rpc.attached");
        link.supervise();
        Ok(link)
    }

    fn new(server: Option<Arc<RunningServer>>, connection: Arc<Connection>, origin: Origin, events: WatchSink) -> Link {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                server,
                connection,
                session_id: String::new(),
                attachment_id: None,
                subscriptions: Vec::new(),
                transcript: None,
                reconnecting: false,
            }),
            changed: Condvar::new(),
            origin,
            events,
            closing: AtomicBool::new(false),
            attaching: Mutex::new(()),
            terminal_theme: Mutex::new(None),
            radius: Mutex::new(None),
            radius_recovering: AtomicBool::new(false),
            radius_connecting: AtomicBool::new(false),
            lost_reported: AtomicBool::new(false),
        });
        Link {
            shared,
            created: Vec::new(),
        }
    }

    /// Connect to a server this client does not run (`--connect`), select `selection`'s session on it
    /// (`prepareClientSession`) and attach; transcript events (and a Radius route's recovery events) go to `events`.
    pub fn open_remote(route: RemoteRoute, selection: &RemoteSelection, events: WatchSink) -> Result<Link, String> {
        let connection = route.connect().map_err(fail)?;
        let radius = matches!(route, RemoteRoute::Radius { .. });
        let mut link = Link::new(None, connection, Origin::Remote(route), events);
        let selected = link
            .management("describe", json!([{}]))
            .map(|listed| listed.and_then(|l| l.as_array().cloned()).unwrap_or_default())
            .and_then(|sessions| select_remote_session(selection, &sessions, radius))
            .and_then(|selected| match selected {
                Selected::Existing { session_id, rename } => {
                    if let Some(name) = rename {
                        link.management("rename", json!([session_id, name]))?;
                    }
                    Ok(session_id)
                }
                Selected::Create(options) => link.create(options),
            });
        if let Err(error) = selected.and_then(|id| link.attach(&id)) {
            link.close(false);
            return Err(error);
        }
        link.shared.start_radius_reconnector();
        link.supervise();
        Ok(link)
    }

    /// The route of a remote link (`None` for this process's own server).
    pub fn remote_route(&self) -> Option<RemoteRoute> {
        match &self.shared.origin {
            Origin::Remote(route) => Some(route.clone()),
            Origin::Foreground(_) | Origin::Durable(_) => None,
        }
    }

    /// Watch the connection and the attachment; rejoin when either goes away. A Radius route's lost connection is
    /// recovered by its reconnector; a remote Unix route's is reported once (it has no reconnect).
    fn supervise(&self) {
        let shared = Arc::downgrade(&self.shared);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(SUPERVISE);
                let Some(shared) = shared.upgrade() else { return };
                if shared.closing.load(Ordering::SeqCst) {
                    return;
                }
                if shared.radius_recovering.load(Ordering::SeqCst) {
                    continue;
                }
                let Some(reason) = shared.needs_rejoin() else {
                    continue;
                };
                let closed = shared.connection().is_closed();
                match &shared.origin {
                    Origin::Remote(RemoteRoute::Radius { .. }) if closed => shared.recover_radius(),
                    Origin::Remote(RemoteRoute::Unix(_)) if closed => {
                        if !shared.lost_reported.swap(true, Ordering::SeqCst) {
                            (shared.events)(connection_event(
                                "lost",
                                Some(&daemon::lost::lost_server_message(Some(reason))),
                            ));
                        }
                    }
                    _ => {
                        let _ = shared.rejoin(reason);
                    }
                }
            }
        });
    }

    /// The current connection (a new one after the server was restarted).
    pub fn connection(&self) -> Arc<Connection> {
        self.shared.connection()
    }

    /// A handle another thread can follow the link with (its current connection and session).
    pub fn handle(&self) -> LinkHandle {
        LinkHandle {
            shared: self.shared.clone(),
        }
    }

    /// The attached session's id.
    pub fn session_id(&self) -> String {
        self.shared.lock().session_id.clone()
    }

    fn management(&self, member: &str, args: Value) -> Result<Option<Value>, String> {
        self.shared.management(member, args)
    }

    /// Create a session (`pi.session-management.create` options); returns its id.
    pub fn create(&mut self, options: Value) -> Result<String, String> {
        let summary = self.management("create", json!([options]))?.unwrap_or_default();
        let id = summary["sessionId"].as_str().unwrap_or_default().to_string();
        self.created.push(id.clone());
        Ok(id)
    }

    /// Remove a stored session through the server (`pi.session-management.remove`).
    pub fn remove(&self, id: &str) -> Result<(), String> {
        self.management("remove", json!([id])).map(|_| ())
    }

    /// Import a Pi session's text through the server (`pi.session-management.importPi`): `{session, alreadyImported,
    /// imported}`.
    pub fn import_pi(&self, source_path: &str, content: &str) -> Result<Value, String> {
        let source = json!({"sourcePath": source_path, "content": content});
        Ok(self.management("importPi", json!([source]))?.unwrap_or_default())
    }

    /// Rename a stored session through the server (`pi.session-management.rename`).
    pub fn rename(&self, id: &str, name: &str) -> Result<(), String> {
        self.management("rename", json!([id, name])).map(|_| ())
    }

    /// The file of a session the server lists.
    pub fn session_file(&self, id: &str) -> Result<Option<String>, String> {
        let listed = self.management("describe", json!([{}]))?.unwrap_or_default();
        Ok(listed
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["sessionId"] == id)
            .and_then(|s| s["sessionFile"].as_str().map(str::to_string)))
    }

    /// Attach to session `id` (replacing the current one): its services subscribed, its transcript followed.
    pub fn attach(&mut self, id: &str) -> Result<(), String> {
        self.shared.wait_reconnected();
        self.shared.attach(id)
    }

    /// The attached session's transcript snapshot (`LaneSnapshot`).
    pub fn snapshot(&self) -> Value {
        let state = self.shared.lock();
        state
            .transcript
            .as_ref()
            .and_then(|t| state.connection.state(t, "state"))
            .map_or(Value::Null, |s| s["snapshot"].clone())
    }

    /// A call on the attached session, after any reconnection under way; a call refused because the worker was
    /// restarting never reached it and is sent again once the link rejoined.
    fn session_call(&self, service: &str, member: &str, args: Value) -> OpResult {
        session_call(&self.shared, service, member, args)
    }

    /// A handle that runs session operations from another thread, beside this link's own calls.
    pub fn ops(&self) -> Ops {
        Ops(self.shared.clone())
    }

    /// Run a session operation on the attached session's worker.
    pub fn op(&self, op: &Op) -> OpResult {
        self.session_call(daemon::worker::OPS_SERVICE, "op", json!([op.to_json()]))
    }

    /// The terminal's appearance (`"light"` / `"dark"`, as the TUI detected it): told to the attached session now and
    /// to every session attached later ([`Op::SetTerminalTheme`]).
    pub fn set_terminal_theme(&self, theme: &str) -> OpResult {
        *self.shared.terminal_theme.lock().unwrap_or_else(|e| e.into_inner()) = Some(theme.to_string());
        self.op(&Op::SetTerminalTheme {
            theme: theme.to_string(),
        })
    }

    /// A blocking call of the attached session (`bash`, `compact`, the agent controller's `prompt`).
    pub fn call(&self, service: &str, member: &str, args: Value) -> OpResult {
        self.session_call(service, member, args)
    }

    /// Detach, remove the sessions this link created when `remove_created`, and stop the server.
    pub fn close(mut self, remove_created: bool) {
        self.shared.closing.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
        // A recovery under way is abandoned (the reconnector stops and disconnects).
        if self.shared.radius_recovering.load(Ordering::SeqCst) {
            self.shared.stop_radius_reconnector();
        }
        {
            let mut state = self.shared.lock();
            self.shared.unsubscribe(&mut state);
        }
        let connection = self.shared.connection();
        if remove_created {
            for id in std::mem::take(&mut self.created) {
                let attached = connection.attachment().is_some_and(|a| a.session_id == id);
                if self.management("remove", json!([id])).is_ok() && attached {
                    let _ = connection.wait_attachment(|a| a.is_none());
                }
            }
        }
        self.shared.stop_radius_reconnector();
        connection.disconnect();
        let server = self.shared.lock().server.clone();
        if let Some(server) = server {
            server.close();
        }
    }
}
