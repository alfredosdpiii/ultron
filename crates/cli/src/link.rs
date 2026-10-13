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
use client::runtime::{CallError, Connection};
use daemon::server::{RunningServer, ServerOptions, start_foreground_server};
use serde_json::{Value, json};
use session_core::core::{Op, OpResult};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

fn fail(error: CallError) -> String {
    daemon::lost::describe(&error.to_string()).unwrap_or_else(|| error.to_string())
}

/// Where a link forwards the attached session's lane-watch events.
pub type WatchSink = Arc<dyn Fn(Value) + Send + Sync>;

/// How often the supervisor looks at the connection.
const SUPERVISE: Duration = Duration::from_millis(100);
/// How long a link keeps trying to rejoin before it reports the session lost.
const REJOIN_TIMEOUT: Duration = client::runtime::REJOIN_TIMEOUT;

/// A `connection` watch event.
pub fn connection_event(state: &str, message: Option<&str>) -> Value {
    json!({"type": "connection", "state": state, "message": message})
}

struct State {
    server: Arc<RunningServer>,
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
    options: ServerOptions,
    events: WatchSink,
    closing: AtomicBool,
    /// Serializes attaching (the supervisor and the owner's `attach`).
    attaching: Mutex<()>,
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
        self.subscribe(&connection, id)
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
        moved.then_some("The session worker restarted")
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
            let server = start_foreground_server(&self.options).map_err(|e| e.to_string())?;
            let connection = Connection::connect(&server.endpoint_path, &server.server_id).map_err(fail)?;
            let mut state = self.lock();
            let old = std::mem::replace(&mut state.server, server);
            state.connection = connection;
            state.subscriptions.clear();
            state.transcript = None;
            state.attachment_id = None;
            drop(state);
            // The old generation already closed when it lost its coordinator; this only waits for that.
            std::thread::spawn(move || old.close());
        }
        self.attach(&id)
    }
}

impl Link {
    /// Start the server, select `target`'s session and attach to it; transcript events go to `events`.
    pub fn open(parsed: &Args, env: &Env, target: &Target, mode: &str, events: WatchSink) -> Result<Link, String> {
        let options = server_options(parsed, env, mode)?;
        let server = start_foreground_server(&options).map_err(|e| e.to_string())?;
        let connection = match Connection::connect(&server.endpoint_path, &server.server_id) {
            Ok(connection) => connection,
            Err(error) => {
                server.close();
                return Err(fail(error));
            }
        };
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
            options,
            events,
            closing: AtomicBool::new(false),
            attaching: Mutex::new(()),
        });
        let mut link = Link {
            shared,
            created: Vec::new(),
        };
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
            Target::Ephemeral => link.create(json!({})),
        };
        let attached = selected.and_then(|id| link.attach(&id));
        if let Err(error) = attached {
            link.close(false);
            return Err(error);
        }
        link.supervise();
        Ok(link)
    }

    /// Watch the connection and the attachment; rejoin when either goes away.
    fn supervise(&self) {
        let shared = Arc::downgrade(&self.shared);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(SUPERVISE);
                let Some(shared) = shared.upgrade() else { return };
                if shared.closing.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(reason) = shared.needs_rejoin() {
                    let _ = shared.rejoin(reason);
                }
            }
        });
    }

    /// The current connection (a new one after the server was restarted).
    pub fn connection(&self) -> Arc<Connection> {
        self.shared.connection()
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
        let deadline = Instant::now() + REJOIN_TIMEOUT;
        loop {
            self.shared.wait_reconnected();
            let connection = self.shared.connection();
            if connection.is_closed() && Instant::now() < deadline {
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

    /// Run a session operation on the attached session's worker.
    pub fn op(&self, op: &Op) -> OpResult {
        self.session_call(daemon::worker::OPS_SERVICE, "op", json!([op.to_json()]))
    }

    /// A blocking call of the attached session (`bash`, `compact`, the agent controller's `prompt`).
    pub fn call(&self, service: &str, member: &str, args: Value) -> OpResult {
        self.session_call(service, member, args)
    }

    /// Detach, remove the sessions this link created when `remove_created`, and stop the server.
    pub fn close(mut self, remove_created: bool) {
        self.shared.closing.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
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
        connection.disconnect();
        let server = self.shared.lock().server.clone();
        server.close();
    }
}
