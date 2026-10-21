//! `SessionWorkerManager` (experimental/session-worker-manager.ts): the session workers one server generation owns.
//! It launches a worker per session, adopts orphans through discovery, applies attachment demand, routes service
//! operations and their updates through the coordinator, and stops, releases or forgets workers.

use crate::catalog::Meta;
use crate::chord::{Publish, ServiceError, ServiceResult, subscribe_id, unsubscribe_id};
use crate::coord_client::{CoordinatorConnection, Event};
use crate::paths::uuid_v4;
use crate::process::{self, Role};
use base64::Engine;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

const WORKER_STARTUP_TIMEOUT: Duration = Duration::from_millis(15_000);
/// The error code and message of a call its client cancelled (packages/server server.ts `handleRequest`).
pub const CANCELLED_CODE: &str = "cancelled";
pub const CANCELLED_MESSAGE: &str = "RPC request cancelled";
const WORKER_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(10_000);
const WORKER_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(5_000);
const WORKER_DEMAND_TIMEOUT: Duration = Duration::from_millis(5_000);

/// The server's heartbeat to its workers (`ULTRON_WORKER_HEARTBEAT_MS`); a worker silent for
/// [`HEARTBEAT_MISSES`] periods is hung and is killed (and restarted when it had work).
pub const HEARTBEAT_ENV: &str = "ULTRON_WORKER_HEARTBEAT_MS";
const DEFAULT_HEARTBEAT: Duration = Duration::from_millis(5_000);
const HEARTBEAT_MISSES: u32 = 6;
/// Restarts of a crashed worker back off from [`RESTART_BASE`] doubling to [`RESTART_MAX_DELAY`]; after
/// [`MAX_RESTARTS`] crashes without [`RESTART_STABLE`] of uptime in between, the session is left for the user.
pub const RESTART_BASE: Duration = Duration::from_millis(250);
pub const RESTART_MAX_DELAY: Duration = Duration::from_secs(8);
pub const MAX_RESTARTS: u32 = 5;
pub const RESTART_STABLE: Duration = Duration::from_secs(60);
/// The code of an operation whose worker died under it (the worker is restarting).
pub const WORKER_LOST_CODE: &str = "session_worker_lost";
/// The code of an attach refused because the session's worker is retiring (it went idle after its last client left)
/// or stopping: once it is gone, a new worker serves the session ([`Manager::attach_client`]).
pub const WORKER_RETIRING_CODE: &str = "session_worker_retiring";
/// What a retiring worker answers a demand (crate::lifecycle `set_demand`).
const RETIRING_REJECTION: &str = "Session worker is retiring";

/// The delay before restart `attempt` (1-based).
pub fn restart_delay(attempt: u32) -> Duration {
    let factor = 1u32 << attempt.saturating_sub(1).min(16);
    (RESTART_BASE * factor).min(RESTART_MAX_DELAY)
}

fn heartbeat_period() -> Duration {
    std::env::var(HEARTBEAT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_HEARTBEAT, Duration::from_millis)
}

/// What new workers are started with (the server's model selection).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkerModel {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub thinking: Option<String>,
    pub extension_mode: Option<String>,
    /// Sessions are not named (`--no-session` runs, whose session is removed when they end).
    pub no_naming: bool,
    /// `--models`: the scoped model patterns a new session's model is chosen from (else `enabledModels`).
    pub models: Option<Vec<String>>,
    /// `--system-prompt`, `--no-skills`, `--no-context-files`.
    pub session: session_core::session_flags::SessionFlags,
    /// Environment the workers start with on top of this process's: what the CLI set for its run (`--offline`'s
    /// `PI_OFFLINE`, the `httpProxy` setting's `HTTP_PROXY`/`HTTPS_PROXY`), as TypeScript sets `process.env`.
    pub env: Vec<(String, String)>,
    /// `--approve` (`Some(true)`) / `--no-approve` (`Some(false)`): the run's project trust override, which the
    /// workers read from [`config::trust::TRUST_OVERRIDE_ENV`].
    pub project_trust: Option<bool>,
}

impl WorkerModel {
    /// A client's own session options (`ultron durable --model ...`), as its `attach` request carries them to a
    /// server whose workers start on other options; `None` when it sets none.
    pub fn session_options(&self) -> Option<Value> {
        let mut options = json!({});
        if let Some(model) = &self.model {
            options["model"] = json!(model);
            if let Some(provider) = &self.provider {
                options["provider"] = json!(provider);
            }
        }
        if let Some(key) = &self.api_key {
            options["apiKey"] = json!(key);
        }
        if let Some(thinking) = &self.thinking {
            options["thinking"] = json!(thinking);
        }
        if let Some(models) = &self.models {
            options["models"] = json!(models);
        }
        if self.no_naming {
            options["noNaming"] = json!(true);
        }
        if let Some(trusted) = self.project_trust {
            options["projectTrust"] = json!(trusted);
        }
        self.session.write_worker_options(&mut options);
        options.as_object().is_some_and(|o| !o.is_empty()).then_some(options)
    }

    /// These options with a client's session options (see [`WorkerModel::session_options`]) over them.
    pub fn with_session_options(&self, options: &Value) -> WorkerModel {
        let text = |key: &str| options[key].as_str().map(str::to_string);
        let mut model = self.clone();
        if options.get("model").is_some() {
            model.provider = text("provider");
            model.model = text("model");
        }
        if let Some(key) = text("apiKey") {
            model.api_key = Some(key);
        }
        if let Some(thinking) = text("thinking") {
            model.thinking = Some(thinking);
        }
        if let Some(models) = options["models"].as_array() {
            model.models = Some(models.iter().filter_map(Value::as_str).map(str::to_string).collect());
        }
        model.no_naming |= options["noNaming"] == true;
        if let Some(trusted) = options["projectTrust"].as_bool() {
            model.project_trust = Some(trusted);
        }
        let flags = session_core::session_flags::SessionFlags::from_worker_options(options);
        if flags.system_prompt.is_some() {
            model.session.system_prompt = flags.system_prompt;
        }
        if flags.append_system_prompt.is_some() {
            model.session.append_system_prompt = flags.append_system_prompt;
        }
        model.session.no_skills |= flags.no_skills;
        model.session.no_context_files |= flags.no_context_files;
        if !flags.skill_paths.is_empty() {
            model.session.skill_paths = flags.skill_paths;
        }
        model
    }
}

type TerminationListener = Box<dyn FnOnce(Option<String>) + Send>;

/// Resolves once with `None` for an expected stop or the unexpected termination's message.
#[derive(Default)]
pub struct Terminated {
    state: Mutex<Option<Option<String>>>,
    listeners: Mutex<Vec<TerminationListener>>,
    changed: Condvar,
}

impl Terminated {
    fn resolve(&self, error: Option<String>) {
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.is_some() {
                return;
            }
            *state = Some(error.clone());
        }
        self.changed.notify_all();
        let listeners = std::mem::take(&mut *self.listeners.lock().unwrap_or_else(|e| e.into_inner()));
        for listener in listeners {
            listener(error.clone());
        }
    }

    /// Run `listener` once it resolves (now, if it already has).
    pub fn on_resolve(&self, listener: Box<dyn FnOnce(Option<String>) + Send>) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match state {
            Some(error) => listener(error),
            None => self.listeners.lock().unwrap_or_else(|e| e.into_inner()).push(listener),
        }
    }

    pub fn is_resolved(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    fn wait(&self, timeout: Duration) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |s| s.is_none())
            .unwrap_or_else(|e| e.into_inner());
        state.is_some()
    }
}

struct Worker {
    peer: String,
    meta: Meta,
    pid: u32,
    token: String,
    attachments: HashSet<String>,
    expected_stop: bool,
    retiring: bool,
    stopping: bool,
    terminated: Arc<Terminated>,
    /// A run is in progress (the worker's last status).
    busy: bool,
    /// The worker reported its run stuck (logged once per episode).
    stuck: bool,
    /// When the worker last answered (any message from it).
    last_seen: Instant,
    started: Instant,
}

struct Pending {
    peer: String,
    token: String,
    pid: u32,
    outcome: Option<Result<String, ServiceError>>,
    /// The process exited before readiness while the coordinator still listed it: its disconnect settles the
    /// launch with this error, unless a `worker_failed` relayed before the disconnect already did.
    exited: Option<ServiceError>,
}

/// A crashed worker's restart: attempts since it last ran stably, and whether one is under way.
struct Restart {
    attempts: u32,
    pending: bool,
}

type Done = Box<dyn FnOnce(ServiceResult) + Send>;

#[derive(Default)]
struct Inner {
    workers: HashMap<String, Worker>,
    by_session: HashMap<PathBuf, String>,
    pending: HashMap<PathBuf, Pending>,
    demands: HashMap<String, (String, Option<Result<(), String>>)>,
    operations: HashMap<String, (String, Value, Done)>,
    subscriptions: HashMap<String, (String, Publish)>,
    releases: HashMap<String, (String, Option<bool>, bool)>,
    foreign: HashMap<PathBuf, (String, u32)>,
    discovery: Option<HashSet<String>>,
    exited: HashMap<String, Worker>,
    restarts: HashMap<PathBuf, Restart>,
    detached: bool,
    shutting_down: bool,
    releasing: bool,
}

/// A routed session's worker, as the router holds it.
#[derive(Clone)]
pub struct Handle {
    peer: String,
    pub session_id: String,
    pub path: PathBuf,
    pub terminated: Arc<Terminated>,
}

type Logger = Box<dyn Fn(&str, &str) + Send + Sync>;
/// Told a worker's report of its session's active schedules (crate::prompt_schedules).
type SchedulesListener = Box<dyn Fn(&Meta, &Value) + Send + Sync>;

pub struct Manager {
    me: std::sync::Weak<Manager>,
    coordinator: OnceLock<Arc<CoordinatorConnection>>,
    session_dir: PathBuf,
    model: WorkerModel,
    /// The options a session's worker starts on when a client gave its own (`ultron durable --model ...`), by
    /// session path; kept for its restarts, replaced when a later client starts it again.
    session_models: Mutex<HashMap<PathBuf, WorkerModel>>,
    inner: Mutex<Inner>,
    changed: Condvar,
    on_count: Box<dyn Fn(usize) + Send + Sync>,
    logger: OnceLock<Logger>,
    schedules: OnceLock<SchedulesListener>,
    /// Settings defaults its workers apply under the user's files (`settingsDefaults`; the durable instance's
    /// `persona: "ultron"`).
    settings_defaults: OnceLock<Value>,
}

fn scoped(scope: &Value, subscription: &str) -> String {
    format!(
        "{}\0{}\0{subscription}",
        scope["serverConnectionId"].as_str().unwrap_or_default(),
        scope["attachmentId"].as_str().unwrap_or_default()
    )
}

impl Manager {
    pub fn new(session_dir: PathBuf, model: WorkerModel, on_count: Box<dyn Fn(usize) + Send + Sync>) -> Arc<Manager> {
        Arc::new_cyclic(|me| Manager {
            me: me.clone(),
            coordinator: OnceLock::new(),
            session_dir,
            model,
            session_models: Mutex::new(HashMap::new()),
            inner: Mutex::new(Inner::default()),
            changed: Condvar::new(),
            on_count,
            logger: OnceLock::new(),
            schedules: OnceLock::new(),
            settings_defaults: OnceLock::new(),
        })
    }

    /// Workers started from now on apply `defaults` under the user's settings files (config::settings
    /// `set_process_defaults`).
    pub fn set_settings_defaults(&self, defaults: Value) {
        let _ = self.settings_defaults.set(defaults);
    }

    pub fn set_coordinator(&self, coordinator: Arc<CoordinatorConnection>) {
        let _ = self.coordinator.set(coordinator);
        self.start_heartbeat();
    }

    /// Where restarts, hung workers and stuck runs are reported (the server log).
    pub fn set_logger(&self, logger: Logger) {
        let _ = self.logger.set(logger);
    }

    /// Where workers' reports of their sessions' active schedules go (the server's schedule timer).
    pub fn on_schedules(&self, listener: SchedulesListener) {
        let _ = self.schedules.set(listener);
    }

    fn log(&self, label: &str, message: &str) {
        if let Some(logger) = self.logger.get() {
            logger(label, message);
        }
    }

    /// Ping every worker each period; one silent for [`HEARTBEAT_MISSES`] periods is hung: killed, and restarted
    /// like any crashed worker that had work.
    fn start_heartbeat(&self) {
        let period = heartbeat_period();
        let this = self.me.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(period);
                let Some(this) = this.upgrade() else { return };
                let (live, hung): (Vec<String>, Vec<(u32, String, Duration)>) = {
                    let inner = this.lock();
                    if inner.detached || inner.shutting_down {
                        return;
                    }
                    let active = inner.workers.values().filter(|w| !w.stopping && !w.expected_stop);
                    let (hung, live): (Vec<&Worker>, Vec<&Worker>) =
                        active.partition(|w| w.last_seen.elapsed() > period * HEARTBEAT_MISSES);
                    (
                        live.iter().map(|w| w.peer.clone()).collect(),
                        hung.iter()
                            .map(|w| (w.pid, w.meta.id.clone(), w.last_seen.elapsed()))
                            .collect(),
                    )
                };
                for (pid, id, silent) in hung {
                    this.log(
                        "[ultron server warning]",
                        &format!(
                            "Session worker {id} (pid {pid}) did not answer heartbeats for {} ms; killing it",
                            silent.as_millis()
                        ),
                    );
                    process::kill(pid, rustix::process::Signal::KILL);
                }
                for peer in live {
                    let _ = this.send(&peer, json!({"type": "ping", "nonce": uuid_v4()}));
                }
            }
        });
    }

    /// A restart of `path`'s crashed worker is under way.
    pub fn restart_pending(&self, path: &PathBuf) -> bool {
        self.lock().restarts.get(path).is_some_and(|r| r.pending)
    }

    /// Wait for `path`'s restart to finish; the restarted worker's handle, if it came back.
    pub fn wait_restart(&self, path: &PathBuf, timeout: Duration) -> Option<Handle> {
        let (inner, _) = self.wait_until(timeout, |i| {
            i.detached || i.shutting_down || !i.restarts.get(path).is_some_and(|r| r.pending)
        });
        let peer = inner.by_session.get(path)?;
        Some(self.handle(&inner.workers[peer]))
    }

    /// Whether a worker that ended unexpectedly is restarted: it had a run in progress or clients attached, and
    /// the server is not going away.
    fn should_restart(inner: &Inner, worker: &Worker) -> bool {
        !inner.detached
            && !inner.shutting_down
            && !inner.releasing
            && !worker.expected_stop
            && !worker.retiring
            && (worker.busy || !worker.attachments.is_empty())
    }

    /// Restart `meta`'s worker after a backoff, retrying launch failures; gives up after [`MAX_RESTARTS`].
    fn schedule_restart(&self, meta: Meta, uptime: Duration, reason: &str) {
        let attempt = {
            let mut inner = self.lock();
            let restart = inner.restarts.entry(meta.path.clone()).or_insert(Restart {
                attempts: 0,
                pending: false,
            });
            if uptime >= RESTART_STABLE {
                restart.attempts = 0;
            }
            if restart.attempts >= MAX_RESTARTS {
                inner.restarts.remove(&meta.path);
                drop(inner);
                self.changed.notify_all();
                self.log(
                    "[ultron server error]",
                    &format!(
                        "Session worker {} crashed {MAX_RESTARTS} times in a row; not restarting it ({reason}). Its session is saved: attach to it again (`ultron --session-id {}`) once the cause is fixed",
                        meta.id, meta.id
                    ),
                );
                return;
            }
            restart.attempts += 1;
            restart.pending = true;
            restart.attempts
        };
        self.log(
            "[ultron server warning]",
            &format!(
                "{reason}; restarting it in {} ms (attempt {attempt} of {MAX_RESTARTS})",
                restart_delay(attempt).as_millis()
            ),
        );
        let this = self.me.clone();
        std::thread::spawn(move || {
            std::thread::sleep(restart_delay(attempt));
            let Some(this) = this.upgrade() else { return };
            let launched = {
                let inner = this.lock();
                if inner.detached || inner.shutting_down || inner.releasing {
                    None
                } else if inner.by_session.contains_key(&meta.path) || inner.pending.contains_key(&meta.path) {
                    // A client's attach launched it already.
                    Some(Ok(()))
                } else {
                    drop(inner);
                    Some(this.launch(&meta).map(drop))
                }
            };
            match launched {
                Some(Err(error)) => {
                    if let Some(restart) = this.lock().restarts.get_mut(&meta.path) {
                        restart.pending = false;
                    }
                    let reason = format!("Session worker {} failed to restart: {}", meta.id, error.message);
                    this.schedule_restart(meta, Duration::ZERO, &reason);
                }
                Some(Ok(())) => {
                    if let Some(restart) = this.lock().restarts.get_mut(&meta.path) {
                        restart.pending = false;
                    }
                    this.log("[ultron server info]", &format!("Session worker {} restarted", meta.id));
                }
                None => {
                    this.lock().restarts.remove(&meta.path);
                }
            }
            this.changed.notify_all();
        });
    }

    fn coordinator(&self) -> &Arc<CoordinatorConnection> {
        self.coordinator
            .get()
            .expect("the coordinator connects before workers are used")
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wait_until(&self, timeout: Duration, mut done: impl FnMut(&mut Inner) -> bool) -> (MutexGuard<'_, Inner>, bool) {
        let deadline = Instant::now() + timeout;
        let mut inner = self.lock();
        loop {
            if done(&mut inner) {
                return (inner, true);
            }
            let now = Instant::now();
            if now >= deadline {
                return (inner, false);
            }
            inner = self
                .changed
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    fn notify_count(&self, inner: &Inner) {
        (self.on_count)(inner.workers.len() + inner.pending.len());
    }

    fn send(&self, peer: &str, payload: Value) -> std::io::Result<()> {
        self.coordinator().send(peer, payload)
    }

    /// The sessions of live workers (listed even before their files are).
    pub fn tracked_sessions(&self) -> Vec<Meta> {
        self.lock().workers.values().map(|w| w.meta.clone()).collect()
    }

    /// Session ID to worker PID.
    pub fn worker_pids(&self) -> HashMap<String, u32> {
        self.lock()
            .workers
            .values()
            .map(|w| (w.meta.id.clone(), w.pid))
            .collect()
    }

    /// Ask the coordinator's other workers who they belong to: orphans are adopted, owned ones answer so.
    pub fn discover(&self) {
        let peers = self.coordinator().peer_snapshot();
        let mut inner = self.lock();
        if inner.detached {
            return;
        }
        let undiscovered: HashSet<String> = peers
            .into_iter()
            .filter(|p| !inner.workers.contains_key(p) && !inner.pending.values().any(|q| &q.peer == p))
            .collect();
        if undiscovered.is_empty() {
            return;
        }
        inner.foreign.retain(|_, (peer, _)| !undiscovered.contains(peer));
        inner.discovery = Some(undiscovered);
        drop(inner);
        let _ = self.coordinator().broadcast(json!({"type": "discover_workers"}));
        let (mut inner, _) = self.wait_until(WORKER_DISCOVERY_TIMEOUT, |i| {
            i.discovery.as_ref().is_none_or(HashSet::is_empty)
        });
        inner.discovery = None;
    }

    fn mark_discovered(inner: &mut Inner, peer: &str) {
        if let Some(discovery) = inner.discovery.as_mut() {
            discovery.remove(peer);
        }
    }

    pub fn open_session(&self, meta: &Meta) -> Result<Handle, ServiceError> {
        self.open_session_with(meta, None)
    }

    /// The options `meta`'s worker starts (or started) on.
    fn model_for(&self, path: &PathBuf) -> WorkerModel {
        self.session_models
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            .cloned()
            .unwrap_or_else(|| self.model.clone())
    }

    /// A client's session options for `meta`'s worker that is already running: refused unless they are the ones it
    /// started on (they cannot change a running worker).
    pub fn check_session_options(
        &self,
        meta_path: &PathBuf,
        session_id: &str,
        options: Option<&Value>,
    ) -> Result<(), ServiceError> {
        match options {
            Some(options) if self.model.with_session_options(options) != self.model_for(meta_path) => {
                Err(ServiceError::coded(
                    "service_invalid_value",
                    format!(
                        "Session {session_id} is already open on this server with other session options (model, \
                         thinking, prompt or skill flags): they apply when its worker starts. Detach its other \
                         clients and let it go idle first, or leave the flags out"
                    ),
                ))
            }
            _ => Ok(()),
        }
    }

    /// [`Manager::open_session`] with a client's session options: a worker started for it runs on them.
    pub fn open_session_with(&self, meta: &Meta, options: Option<&Value>) -> Result<Handle, ServiceError> {
        {
            let inner = self.lock();
            if inner.detached || inner.shutting_down {
                return Err(ServiceError::internal("Experimental server is shutting down"));
            }
        }
        if let Some(handle) = self.existing(meta) {
            self.check_session_options(&meta.path, &meta.id, options)?;
            return handle;
        }
        // Another server may own this session's worker, or have released it as an orphan.
        self.discover();
        if let Some(handle) = self.existing(meta) {
            self.check_session_options(&meta.path, &meta.id, options)?;
            return handle;
        }
        let foreign = self.lock().foreign.get(&meta.path).cloned();
        if let Some((peer, pid)) = foreign
            && self.coordinator().peer_snapshot().contains(&peer)
        {
            return Err(ServiceError::coded(
                "service_invalid_value",
                crate::worker::session_in_use(&meta.id, Some(pid)),
            ));
        }
        {
            let mut models = self.session_models.lock().unwrap_or_else(|e| e.into_inner());
            match options {
                Some(options) => models.insert(meta.path.clone(), self.model.with_session_options(options)),
                None => models.remove(&meta.path),
            };
        }
        self.launch(meta)
    }

    fn existing(&self, meta: &Meta) -> Option<Result<Handle, ServiceError>> {
        let inner = self.lock();
        if let Some(peer) = inner.by_session.get(&meta.path) {
            return Some(Ok(self.handle(&inner.workers[peer])));
        }
        let pending = inner.pending.get(&meta.path)?.peer.clone();
        drop(inner);
        Some(self.await_launch(&meta.path, &pending))
    }

    fn handle(&self, worker: &Worker) -> Handle {
        Handle {
            peer: worker.peer.clone(),
            session_id: worker.meta.id.clone(),
            path: worker.meta.path.clone(),
            terminated: worker.terminated.clone(),
        }
    }

    fn worker_options(&self, meta: &Meta) -> Value {
        let mut options = json!({"sessionDir": self.session_dir.display().to_string(), "metadata": meta.to_json(), "pluginManifestPaths": []});
        let model = &self.model_for(&meta.path);
        if model.provider.is_some() || model.model.is_some() {
            if let Some(provider) = &model.provider {
                options["provider"] = json!(provider);
            }
            if let Some(id) = &model.model {
                options["model"] = json!(id);
            }
        }
        if let Some(thinking) = &model.thinking {
            options["thinking"] = json!(thinking);
        }
        if let Some(mode) = &model.extension_mode {
            options["extensionMode"] = json!(mode);
        }
        if model.no_naming {
            options["noNaming"] = json!(true);
        }
        if let Some(models) = &model.models {
            options["models"] = json!(models);
        }
        model.session.write_worker_options(&mut options);
        if let Some(defaults) = self.settings_defaults.get() {
            options["settingsDefaults"] = defaults.clone();
        }
        options
    }

    fn launch(&self, meta: &Meta) -> Result<Handle, ServiceError> {
        let peer = format!("worker-{}", uuid_v4());
        let token = uuid_v4();
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(meta.path.display().to_string());
        let coordinator = self.coordinator();
        let mut env = vec![
            (
                crate::worker::CONTROL_ADDRESS_ENV.to_string(),
                coordinator.control_path.display().to_string(),
            ),
            (crate::worker::CONTROL_TOKEN_ENV.to_string(), token.clone()),
            (crate::worker::SESSION_KEY_ENV.to_string(), key),
            (crate::worker::PEER_ID_ENV.to_string(), peer.clone()),
            (
                crate::worker::OWNER_ENV.to_string(),
                coordinator.server_connection_id.clone(),
            ),
        ];
        let model = self.model_for(&meta.path);
        if let Some(api_key) = &model.api_key {
            env.push((crate::worker::API_KEY_ENV.to_string(), api_key.clone()));
        }
        env.extend(model.env.iter().cloned());
        if let Some(trusted) = model.project_trust {
            env.push((
                config::trust::TRUST_OVERRIDE_ENV.to_string(),
                if trusted { "1" } else { "0" }.to_string(),
            ));
        }
        session_core::startup_trace::trace_startup(
            std::env::var("ULTRON_STARTUP_TRACE").ok().as_deref(),
            "server.spawn-worker",
        );
        let child = process::spawn(Role::SessionWorker, &[self.worker_options(meta).to_string()], &env)
            .map_err(|e| ServiceError::internal(e.to_string()))?;
        let pid = child.id();
        {
            let mut inner = self.lock();
            inner.pending.insert(
                meta.path.clone(),
                Pending {
                    peer: peer.clone(),
                    token,
                    pid,
                    outcome: None,
                    exited: None,
                },
            );
            self.notify_count(&inner);
        }
        let exited = process::watch(child);
        let (path, watch_peer) = (meta.path.clone(), peer.clone());
        let this = self.self_arc();
        std::thread::spawn(move || {
            let status = exited.recv().ok().flatten();
            if let Some(this) = this.and_then(|w| w.upgrade()) {
                this.child_exited(&path, &watch_peer, status);
            }
        });
        self.await_launch(&meta.path, &peer)
    }

    fn self_arc(&self) -> Option<std::sync::Weak<Manager>> {
        Some(self.me.clone())
    }

    fn await_launch(&self, path: &PathBuf, peer: &str) -> Result<Handle, ServiceError> {
        let (mut inner, ready) = self.wait_until(WORKER_STARTUP_TIMEOUT, |i| {
            i.pending
                .get(path)
                .is_none_or(|p| p.peer != peer || p.outcome.is_some())
        });
        if !ready {
            drop(inner);
            self.fail_pending(path, ServiceError::internal("Session worker startup timed out"));
            inner = self.lock();
        }
        match inner.pending.get(path) {
            Some(p) if p.peer == peer && p.outcome.is_some() => {
                let pending = inner.pending.remove(path).expect("checked above");
                self.notify_count(&inner);
                match pending.outcome.expect("checked above") {
                    Ok(peer) => inner
                        .workers
                        .get(&peer)
                        .map(|w| self.handle(w))
                        .ok_or_else(|| ServiceError::internal("Session worker disconnected during startup")),
                    Err(error) => Err(error),
                }
            }
            _ => match inner.by_session.get(path) {
                Some(peer) => Ok(self.handle(&inner.workers[peer])),
                None => Err(ServiceError::internal("Session worker startup failed")),
            },
        }
    }

    fn fail_pending(&self, path: &PathBuf, error: ServiceError) {
        let mut inner = self.lock();
        if let Some(pending) = inner.pending.get_mut(path)
            && pending.outcome.is_none()
        {
            pending.outcome = Some(Err(error));
            process::kill(pending.pid, rustix::process::Signal::KILL);
        }
        drop(inner);
        self.changed.notify_all();
    }

    pub fn attach_client(&self, handle: &Handle) -> Result<String, ServiceError> {
        {
            let inner = self.lock();
            let worker = inner.workers.get(&handle.peer);
            if inner.detached || inner.shutting_down {
                return Err(ServiceError::internal("Experimental Session worker is stopping"));
            }
            if worker.is_none_or(|w| w.stopping || w.retiring) {
                return Err(ServiceError::coded(
                    WORKER_RETIRING_CODE,
                    "Experimental Session worker is stopping",
                ));
            }
        }
        let attachment = uuid_v4();
        if let Some(worker) = self.lock().workers.get_mut(&handle.peer) {
            worker.attachments.insert(attachment.clone());
        }
        if let Err(error) = self.apply_demand(&handle.peer, &attachment, true) {
            if let Some(worker) = self.lock().workers.get_mut(&handle.peer) {
                worker.attachments.remove(&attachment);
            }
            // The worker went idle and is retiring (its announcement may still be in flight): the caller waits for
            // it to end and starts a new one.
            if error.ends_with(RETIRING_REJECTION) || handle.terminated.is_resolved() {
                return Err(ServiceError::coded(WORKER_RETIRING_CODE, error));
            }
            return Err(ServiceError::internal(error));
        }
        Ok(attachment)
    }

    pub fn release_attachment(&self, handle: &Handle, attachment: &str) {
        let active = {
            let inner = self.lock();
            !inner.detached
                && inner
                    .workers
                    .get(&handle.peer)
                    .is_some_and(|w| w.attachments.contains(attachment))
        };
        if active {
            let _ = self.apply_demand(&handle.peer, attachment, false);
        }
        let server = self.coordinator().server_connection_id.clone();
        let mut inner = self.lock();
        if let Some(worker) = inner.workers.get_mut(&handle.peer) {
            worker.attachments.remove(attachment);
        }
        let prefix = format!("{server}\0{attachment}\0");
        inner.subscriptions.retain(|key, _| !key.starts_with(&prefix));
    }

    fn apply_demand(&self, peer: &str, attachment: &str, attached: bool) -> Result<(), String> {
        let request = uuid_v4();
        {
            let mut inner = self.lock();
            if inner.workers.get(peer).is_none_or(|w| w.stopping) {
                return Err("Experimental Session worker is stopping".into());
            }
            inner.demands.insert(request.clone(), (peer.to_string(), None));
        }
        let server = self.coordinator().server_connection_id.clone();
        let sent = self.send(
            peer,
            json!({"type": "session_demand", "serverConnectionId": server, "requestId": request, "attachmentId": attachment, "attached": attached}),
        );
        if let Err(error) = sent {
            self.lock().demands.remove(&request);
            return Err(error.to_string());
        }
        let (mut inner, done) = self.wait_until(WORKER_DEMAND_TIMEOUT, |i| {
            i.demands.get(&request).is_none_or(|(_, outcome)| outcome.is_some())
        });
        let outcome = inner.demands.remove(&request);
        drop(inner);
        match outcome {
            Some((_, Some(result))) => result,
            None => Err("Session worker disconnected during demand update".into()),
            Some((_, None)) if !done && attached => {
                // A lost attach is compensated by withdrawing it; a lost detach counts as applied.
                let _ = self.apply_demand(peer, attachment, false);
                Err("Session worker demand update timed out".into())
            }
            Some((_, None)) => Ok(()),
        }
    }

    /// Route one service operation on an attachment; `done` runs with its result (on the coordinator reader).
    /// Returns the operation's request id once it was sent ([`Manager::cancel_operation`] cancels it).
    pub fn invoke(
        &self,
        handle: &Handle,
        attachment: &str,
        call: Value,
        publish: Publish,
        done: Done,
    ) -> Option<String> {
        let server = self.coordinator().server_connection_id.clone();
        let scope = json!({"serverConnectionId": server, "attachmentId": attachment});
        let mut inner = self.lock();
        let valid = !inner.detached
            && !inner.shutting_down
            && inner
                .workers
                .get(&handle.peer)
                .is_some_and(|w| !w.stopping && w.attachments.contains(attachment));
        if !valid {
            drop(inner);
            done(Err(ServiceError::internal(
                "Experimental Session worker has no active attachment",
            )));
            return None;
        }
        let subscription = subscribe_id(&call);
        if let Some(id) = &subscription {
            let key = scoped(&scope, id);
            if inner.subscriptions.contains_key(&key) {
                drop(inner);
                done(Err(ServiceError::internal("Service subscription ID is already active")));
                return None;
            }
            inner.subscriptions.insert(key, (handle.peer.clone(), publish));
        }
        let unsubscribe = unsubscribe_id(&call).map(|id| scoped(&scope, &id));
        let request = uuid_v4();
        let (sub_key, inner_scope) = (subscription.map(|id| scoped(&scope, &id)), scope.clone());
        let manager_done: Done = Box::new(move |result: ServiceResult| done(result));
        let wrapped: Done = {
            let this = self.self_arc();
            Box::new(move |result: ServiceResult| {
                if let Some(this) = this.and_then(|w| w.upgrade()) {
                    let mut inner = this.lock();
                    if result.is_err()
                        && let Some(key) = &sub_key
                    {
                        inner.subscriptions.remove(key);
                    }
                    if result.is_ok()
                        && let Some(key) = &unsubscribe
                    {
                        inner.subscriptions.remove(key);
                    }
                }
                manager_done(result);
            })
        };
        inner
            .operations
            .insert(request.clone(), (handle.peer.clone(), inner_scope, wrapped));
        drop(inner);
        let sent = self.send(
            &handle.peer,
            json!({"type": "operation", "requestId": request, "scope": scope, "call": call}),
        );
        if let Err(error) = sent {
            self.reject_operation(&request, error.to_string());
            return None;
        }
        Some(request)
    }

    /// The caller stopped waiting for operation `request` (its client cancelled the call or disconnected): the worker
    /// is told (`operation_cancel`, its call's context is cancelled) and the operation settles now as cancelled.
    /// An operation that already settled is left alone.
    pub fn cancel_operation(&self, request: &str) {
        let removed = self.lock().operations.remove(request);
        let Some((peer, scope, done)) = removed else {
            return;
        };
        let _ = self.send(
            &peer,
            json!({"type": "operation_cancel", "requestId": request, "scope": scope}),
        );
        done(Err(ServiceError::coded(CANCELLED_CODE, CANCELLED_MESSAGE)));
    }

    fn reject_operation(&self, request: &str, message: String) {
        let removed = self.lock().operations.remove(request);
        if let Some((_, _, done)) = removed {
            done(Err(ServiceError::internal(message)));
        }
    }

    /// `closeSession`: stop the worker, or release it while the server shuts down.
    pub fn close_session(&self, handle: &Handle) {
        if self.lock().releasing {
            self.release_worker(&handle.peer);
        } else {
            self.stop_worker(&handle.peer);
        }
    }

    pub fn stop_session(&self, meta: &Meta) {
        let peer = self.lock().by_session.get(&meta.path).cloned();
        if let Some(peer) = peer {
            self.stop_worker(&peer);
        }
    }

    fn stop_worker(&self, peer: &str) {
        let terminated = {
            let mut inner = self.lock();
            if inner.detached {
                return;
            }
            let Some(worker) = inner.workers.get_mut(peer) else {
                return;
            };
            worker.stopping = true;
            worker.expected_stop = true;
            worker.terminated.clone()
        };
        self.reject_worker_operations(peer, "Session worker is stopping");
        let _ = self.send(peer, json!({"type": "shutdown"}));
        if !terminated.wait(WORKER_SHUTDOWN_TIMEOUT) {
            let pid = self.lock().workers.get(peer).map(|w| w.pid);
            if let Some(pid) = pid {
                process::kill(pid, rustix::process::Signal::KILL);
            }
            self.remove_worker(peer, None);
        }
    }

    /// Hand a busy worker off (it keeps running detached); an idle one exits.
    fn release_worker(&self, peer: &str) {
        let terminated = {
            let mut inner = self.lock();
            if inner.detached {
                return;
            }
            let Some(worker) = inner.workers.get_mut(peer) else {
                return;
            };
            worker.expected_stop = true;
            worker.terminated.clone()
        };
        self.reject_worker_operations(peer, "Experimental server is shutting down");
        let request = uuid_v4();
        self.lock()
            .releases
            .insert(request.clone(), (peer.to_string(), None, false));
        let server = self.coordinator().server_connection_id.clone();
        if self
            .send(
                peer,
                json!({"type": "release", "requestId": request, "serverConnectionId": server}),
            )
            .is_err()
            && let Some(release) = self.lock().releases.get_mut(&request)
        {
            release.1 = Some(false);
            release.2 = true;
        }
        let terminated_wait = terminated.clone();
        let (mut inner, _) = self.wait_until(WORKER_SHUTDOWN_TIMEOUT, |i| {
            i.releases.get(&request).is_none_or(|r| r.2) || terminated_wait.is_resolved()
        });
        let retained = inner.releases.remove(&request).and_then(|r| r.1);
        drop(inner);
        let retained = if terminated.is_resolved() {
            Some(false)
        } else {
            retained
        };
        match retained {
            Some(true) => self.forget_and_resolve(peer, None),
            Some(false) => {
                if let Some(worker) = self.lock().workers.get_mut(peer) {
                    worker.stopping = true;
                }
                if !terminated.wait(WORKER_SHUTDOWN_TIMEOUT) {
                    self.stop_worker(peer);
                }
            }
            None => self.stop_worker(peer),
        }
    }

    pub fn prepare_shutdown(&self) {
        self.lock().releasing = true;
    }

    /// Release every worker (busy ones stay running detached) and forget them.
    pub fn shutdown(&self) {
        let peers: Vec<String> = {
            let mut inner = self.lock();
            if inner.detached || inner.shutting_down {
                return;
            }
            inner.releasing = true;
            for pending in inner.pending.values() {
                process::kill(pending.pid, rustix::process::Signal::KILL);
            }
            inner.workers.keys().cloned().collect()
        };
        let threads: Vec<_> = peers
            .into_iter()
            .map(|peer| {
                let this = self.self_arc();
                std::thread::spawn(move || {
                    if let Some(this) = this.and_then(|w| w.upgrade()) {
                        this.release_worker(&peer);
                    }
                })
            })
            .collect();
        for thread in threads {
            let _ = thread.join();
        }
        self.lock().shutting_down = true;
        self.detach_state();
    }

    /// Forget workers without stopping them (the coordinator was lost).
    pub fn detach(&self) {
        let mut inner = self.lock();
        if inner.detached {
            return;
        }
        inner.detached = true;
        for pending in inner.pending.values_mut() {
            pending
                .outcome
                .get_or_insert(Err(ServiceError::internal("Ultron server lost its coordinator")));
        }
        drop(inner);
        self.detach_state();
    }

    fn detach_state(&self) {
        let mut inner = self.lock();
        inner.detached = true;
        let operations: Vec<Done> = inner.operations.drain().map(|(_, (_, _, done))| done).collect();
        for (_, outcome) in inner.demands.values_mut() {
            outcome.get_or_insert(Ok(()));
        }
        for release in inner.releases.values_mut() {
            release.2 = true;
        }
        inner.foreign.clear();
        inner.subscriptions.clear();
        inner.workers.clear();
        inner.by_session.clear();
        inner.discovery = None;
        drop(inner);
        self.changed.notify_all();
        for done in operations {
            done(Err(ServiceError::internal(
                "Experimental server detached during a worker operation",
            )));
        }
    }

    fn reject_worker_operations(&self, peer: &str, message: &str) {
        self.reject_worker_operations_with(peer, ServiceError::internal(message));
    }

    fn reject_worker_operations_with(&self, peer: &str, error: ServiceError) {
        let rejected: Vec<Done> = {
            let mut inner = self.lock();
            let ids: Vec<String> = inner
                .operations
                .iter()
                .filter(|(_, (p, _, _))| p == peer)
                .map(|(k, _)| k.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| inner.operations.remove(&id))
                .map(|(_, _, done)| done)
                .collect()
        };
        for done in rejected {
            done(Err(error.clone()));
        }
    }

    fn remove_worker(&self, peer: &str, error: Option<String>) {
        self.forget_and_resolve(peer, error);
    }

    fn forget_and_resolve(&self, peer: &str, error: Option<String>) {
        if let Some(worker) = self.forget(peer, error.as_deref()) {
            self.resolve_lost(worker, error);
        }
    }

    /// Resolve a forgotten worker's termination; an unexpected end of a worker with work restarts it first (so
    /// the router sees the restart under way and keeps its clients).
    fn resolve_lost(&self, worker: Worker, error: Option<String>) {
        if let Some(reason) = &error
            && Self::should_restart(&self.lock(), &worker)
        {
            self.schedule_restart(worker.meta.clone(), worker.started.elapsed(), reason);
        }
        worker.terminated.resolve(error);
    }

    /// Drop a worker from routing and fail its in-flight work (`lost`: it ended unexpectedly, with this reason).
    fn forget(&self, peer: &str, lost: Option<&str>) -> Option<Worker> {
        let mut inner = self.lock();
        let worker = inner.workers.remove(peer)?;
        inner.by_session.remove(&worker.meta.path);
        inner.subscriptions.retain(|_, (p, _)| p != peer);
        for (owner, outcome) in inner.demands.values_mut() {
            if owner == peer {
                outcome.get_or_insert(Err("Session worker disconnected during demand update".into()));
            }
        }
        self.notify_count(&inner);
        drop(inner);
        self.changed.notify_all();
        match lost {
            Some(reason) => self.reject_worker_operations_with(
                peer,
                ServiceError::coded(WORKER_LOST_CODE, format!("{reason} during an operation")),
            ),
            None => self.reject_worker_operations(peer, "Session worker disconnected during an operation"),
        }
        Some(worker)
    }

    fn child_exited(&self, path: &PathBuf, peer: &str, status: Option<std::process::ExitStatus>) {
        let describe = status.map_or("unknown".to_string(), |s| {
            use std::os::unix::process::ExitStatusExt;
            match (s.code(), s.signal()) {
                (Some(code), _) => code.to_string(),
                (None, Some(signal)) => format!("signal {signal}"),
                _ => "unknown".into(),
            }
        });
        let pending_here = self
            .lock()
            .pending
            .get(path)
            .is_some_and(|p| p.peer == peer && p.outcome.is_none());
        if pending_here {
            let error = ServiceError::internal(format!("Session worker exited before readiness ({describe})"));
            // A worker that refuses its session (`worker_failed`, such as another process owning it) says so
            // through the coordinator and exits; the exit, observed directly, can win that race. While the
            // coordinator still lists the worker, its relayed messages and then its disconnect are still to come,
            // in that order: the disconnect settles the launch with this error, unless the refusal did first.
            if self.coordinator().peer_snapshot().contains(peer) {
                let mut inner = self.lock();
                if let Some(pending) = inner.pending.get_mut(path)
                    && pending.peer == peer
                    && pending.outcome.is_none()
                {
                    // Reaped: its pid may be reused, so nothing may signal it any more.
                    pending.pid = 0;
                    pending.exited = Some(error);
                }
                return;
            }
            self.fail_pending(path, error);
            return;
        }
        let inner = self.lock();
        let Some(worker) = inner.workers.get(peer) else { return };
        let unexpected = format!("Session worker {} exited unexpectedly ({describe})", worker.meta.id);
        let clean = status.is_some_and(|s| s.success());
        let (expected, retiring) = (worker.expected_stop, worker.retiring);
        drop(inner);
        if expected || !clean || retiring {
            let error = (!(expected || (clean && retiring))).then_some(unexpected);
            self.remove_worker(peer, error);
            return;
        }
        // A clean exit may be a retirement whose announcement is still in flight through the coordinator: forget
        // the worker now and judge the exit once its disconnect arrives.
        if let Some(worker) = self.forget(peer, None) {
            let terminated = worker.terminated.clone();
            self.lock().exited.insert(peer.to_string(), worker);
            let this = self.self_arc();
            let peer = peer.to_string();
            std::thread::spawn(move || {
                std::thread::sleep(WORKER_SHUTDOWN_TIMEOUT);
                if let Some(this) = this.and_then(|w| w.upgrade()) {
                    this.settle_exited(&peer);
                }
                let _ = terminated;
            });
        }
    }

    fn settle_exited(&self, peer: &str) {
        let worker = self.lock().exited.remove(peer);
        if let Some(worker) = worker {
            let error = (!(worker.expected_stop || worker.retiring))
                .then(|| format!("Session worker {} exited unexpectedly (0)", worker.meta.id));
            self.resolve_lost(worker, error);
        }
    }

    /// Coordinator events, on the coordinator connection's reader thread.
    pub fn handle_event(&self, event: Event) {
        if self.lock().detached {
            return;
        }
        match event {
            Event::PeerConnected(_) => {}
            Event::PeerDisconnected(peer) => {
                let mut inner = self.lock();
                Self::mark_discovered(&mut inner, &peer);
                inner.foreign.retain(|_, (p, _)| *p != peer);
                let worker = inner
                    .workers
                    .get(&peer)
                    .map(|w| (w.expected_stop || w.retiring, w.meta.id.clone()));
                let pending = inner
                    .pending
                    .iter_mut()
                    .find(|(_, p)| p.peer == peer)
                    .map(|(k, p)| (k.clone(), p.exited.take()));
                drop(inner);
                self.changed.notify_all();
                self.settle_exited(&peer);
                if let Some((expected, id)) = worker {
                    let error = (!expected).then(|| format!("Session worker {id} disconnected unexpectedly"));
                    self.remove_worker(&peer, error);
                }
                if let Some((path, exited)) = pending {
                    let error =
                        exited.unwrap_or_else(|| ServiceError::internal("Session worker disconnected during startup"));
                    self.fail_pending(&path, error);
                }
            }
            Event::Message { from, payload } => self.handle_message(&from, &payload),
        }
    }

    fn handle_message(&self, from: &str, message: &Value) {
        let key = |m: &Value| PathBuf::from(m["sessionKey"].as_str().unwrap_or_default());
        let token = message["token"].as_str().unwrap_or_default();
        let mut inner = self.lock();
        if let Some(worker) = inner.workers.get_mut(from) {
            worker.last_seen = Instant::now();
        }
        let owns = |inner: &Inner, path: &PathBuf| {
            inner
                .workers
                .get(from)
                .is_some_and(|w| w.token == token && w.meta.path == *path)
        };
        match message["type"].as_str() {
            Some("worker_owned") => {
                if !inner.workers.contains_key(from) {
                    let pid = message["pid"].as_u64().unwrap_or(0) as u32;
                    inner.foreign.insert(key(message), (from.to_string(), pid));
                }
                Self::mark_discovered(&mut inner, from);
            }
            Some("worker_failed") => {
                let path = key(message);
                if inner
                    .pending
                    .get(&path)
                    .is_some_and(|p| p.peer == from && p.token == token)
                {
                    drop(inner);
                    let text = message["message"].as_str().unwrap_or_default();
                    let error = match message["code"].as_str() {
                        Some(code) => ServiceError::coded(code, text),
                        None => ServiceError::internal(format!("Session worker failed: {text}")),
                    };
                    self.fail_pending(&path, error);
                    return;
                }
            }
            Some(kind @ ("demand_applied" | "demand_rejected")) => {
                let request = message["requestId"].as_str().unwrap_or_default().to_string();
                let path = key(message);
                if inner.demands.get(&request).is_some_and(|(p, _)| p == from) && owns(&inner, &path) {
                    let outcome = if kind == "demand_applied" {
                        Ok(())
                    } else {
                        Err(format!(
                            "Session worker rejected demand: {}",
                            message["message"].as_str().unwrap_or_default()
                        ))
                    };
                    if let Some(entry) = inner.demands.get_mut(&request) {
                        entry.1 = Some(outcome);
                    }
                }
            }
            Some("worker_retiring") => {
                let path = key(message);
                for worker in [inner.workers.get_mut(from)].into_iter().flatten() {
                    if worker.token == token && worker.meta.path == path {
                        worker.retiring = true;
                    }
                }
                if let Some(worker) = inner.exited.get_mut(from)
                    && worker.token == token
                {
                    worker.retiring = true;
                }
            }
            Some("worker_released") => {
                let request = message["requestId"].as_str().unwrap_or_default().to_string();
                let path = key(message);
                let valid = owns(&inner, &path);
                if let Some(release) = inner.releases.get_mut(&request)
                    && release.0 == from
                    && valid
                {
                    release.1 = message["retained"].as_bool();
                    release.2 = true;
                }
            }
            Some("operation_response") => {
                let response = &message["response"];
                let request = response["requestId"].as_str().unwrap_or_default().to_string();
                let path = key(message);
                let valid = owns(&inner, &path);
                let Some((peer, scope, done)) = inner.operations.remove(&request) else {
                    return;
                };
                drop(inner);
                let result = if peer != from || !valid || response["scope"] != scope {
                    Err(ServiceError::internal(
                        "Session worker returned a mismatched operation response",
                    ))
                } else if response["type"] == "operation_error" {
                    let text = response["message"].as_str().unwrap_or_default();
                    Err(match response["code"].as_str() {
                        Some(code) => ServiceError::coded(code, text),
                        None => ServiceError::internal(format!("Session worker operation failed: {text}")),
                    })
                } else {
                    Ok(response.get("result").cloned())
                };
                done(result);
                return;
            }
            Some("service_update") => {
                let subscription = message["subscriptionId"].as_str().unwrap_or_default();
                let path = key(message);
                let valid = owns(&inner, &path);
                let entry = inner
                    .subscriptions
                    .get(&scoped(&message["scope"], subscription))
                    .cloned();
                drop(inner);
                if let Some((peer, publish)) = entry
                    && peer == from
                    && valid
                {
                    publish(subscription, message["update"].clone());
                }
                return;
            }
            Some("worker_schedules") => {
                let path = key(message);
                let meta = inner
                    .workers
                    .get(from)
                    .filter(|_| owns(&inner, &path))
                    .map(|w| w.meta.clone());
                drop(inner);
                if let (Some(meta), Some(listener)) = (meta, self.schedules.get()) {
                    listener(&meta, &message["schedules"]);
                }
                return;
            }
            Some("worker_ready") => {
                drop(inner);
                self.record_ready(from, message);
                return;
            }
            Some("worker_status") => {
                let path = key(message);
                if !owns(&inner, &path) {
                    return;
                }
                let health = &message["health"];
                let stuck = health["stuck"] == true;
                let Some(worker) = inner.workers.get_mut(from) else {
                    return;
                };
                worker.busy = message["busy"] == true;
                let newly_stuck = stuck && !worker.stuck;
                worker.stuck = stuck;
                let id = worker.meta.id.clone();
                drop(inner);
                if newly_stuck {
                    self.log(
                        "[ultron server warning]",
                        &format!(
                            "Session {id}: run {} made no progress for {} ms (stuck); abort it with ultron.session-ops abortStuck",
                            health["runId"].as_str().unwrap_or_default(),
                            health["idleMs"]
                        ),
                    );
                }
                return;
            }
            _ => {}
        }
        drop(inner);
        self.changed.notify_all();
    }

    fn record_ready(&self, peer: &str, message: &Value) {
        let Some(meta) = Meta::from_json(&message["metadata"]) else {
            return;
        };
        let path = PathBuf::from(message["sessionKey"].as_str().unwrap_or_default());
        if path != meta.path
            || message["sessionId"] != json!(meta.id)
            || !std::path::Path::new(&meta.cwd).is_absolute()
            || !meta.path.is_absolute()
        {
            return;
        }
        let token = message["token"].as_str().unwrap_or_default().to_string();
        let pid = message["pid"].as_u64().unwrap_or(0) as u32;
        let mut inner = self.lock();
        Self::mark_discovered(&mut inner, peer);
        if inner.foreign.get(&path).is_some_and(|(p, _)| p == peer) {
            inner.foreign.remove(&path);
        }
        if let Some(existing) = inner.by_session.get(&path)
            && existing != peer
        {
            drop(inner);
            let _ = self.send(peer, json!({"type": "shutdown"}));
            return;
        }
        if let Some(pending) = inner.pending.get(&path)
            && (pending.peer != peer || pending.token != token || pending.pid != pid)
        {
            return;
        }
        if inner.by_session.contains_key(&path) {
            return;
        }
        inner.workers.insert(
            peer.to_string(),
            Worker {
                peer: peer.to_string(),
                meta,
                pid,
                token,
                attachments: HashSet::new(),
                expected_stop: false,
                retiring: false,
                stopping: false,
                terminated: Arc::new(Terminated::default()),
                busy: message["busy"] == true,
                stuck: false,
                last_seen: Instant::now(),
                started: Instant::now(),
            },
        );
        inner.by_session.insert(path.clone(), peer.to_string());
        if let Some(pending) = inner.pending.get_mut(&path) {
            pending.outcome = Some(Ok(peer.to_string()));
        }
        self.notify_count(&inner);
        drop(inner);
        self.changed.notify_all();
    }
}
