//! Server generations (experimental/server.ts): `startServer` (profile, coordinator, endpoint, worker manager,
//! lifetime), `startForegroundServer` (the `ultron` process's own generation), `activateServer` (start-on-demand of a
//! detached server process) and `runServerProcess`, plus the server error log (server-log.ts).

use crate::coord_client::{self, CoordinatorConnection, RegistrationRejected};
use crate::instance;
use crate::paths;
use crate::process::{self, Role};
use crate::routed::Hub;
use crate::sockets;
use crate::workers::{Manager, WorkerModel};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const AUTO_SERVER_STARTUP_GRACE: Duration = Duration::from_millis(10_000);
const AUTO_SERVER_IDLE_GRACE: Duration = Duration::from_millis(1_000);
const ACTIVATION_TIMEOUT: Duration = Duration::from_millis(10_000);
const ACTIVATION_RETRY: Duration = Duration::from_millis(10);
const MAX_SERVER_LOG_BYTES: u64 = 1024 * 1024;
/// Set on a spawned server process to hold it open without demand (`ultron durable`).
pub const KEEP_ALIVE_ENV: &str = "__ULTRON_SERVER_KEEP_ALIVE";
/// A test knob: the idle grace of an automatically activated server in ms (default 1000). Never set in the product.
pub const IDLE_GRACE_ENV: &str = "__ULTRON_SERVER_IDLE_GRACE_MS";

/// Whether a client with `options` can use a server started on the default worker options: its workers would start
/// exactly as that server's do (the client's mode is not a worker difference that matters).
pub fn attach_compatible(options: &ServerOptions) -> bool {
    WorkerModel {
        extension_mode: None,
        ..options.model.clone()
    } == WorkerModel::default()
}

/// Whether `directory` is the durable instance's (`~/.ultron/server` of this process's HOME).
pub fn is_durable_directory(directory: &Path) -> bool {
    std::env::var_os("HOME").is_some_and(|home| {
        config::paths::normalize(directory)
            == config::paths::normalize(&instance::ultron_home(Path::new(&home)).join("server"))
    })
}

/// A server process of the durable instance moves to the home directory.
fn durable_home(directory: &Path) {
    if is_durable_directory(directory)
        && let Some(home) = std::env::var_os("HOME")
    {
        let _ = std::env::set_current_dir(home);
    }
}

/// The idle grace a server process uses: [`IDLE_GRACE_ENV`] when it holds a number of ms, else the default.
pub fn idle_grace(var: &dyn Fn(&str) -> Option<String>) -> Duration {
    var(IDLE_GRACE_ENV)
        .and_then(|ms| ms.trim().parse::<u64>().ok())
        .map_or(AUTO_SERVER_IDLE_GRACE, Duration::from_millis)
}

/// `serverErrorLogPath`.
pub fn error_log_path(directory: &Path, server_id: &str) -> PathBuf {
    directory.join(format!("server-{server_id}.log"))
}

/// `recordServerError`: append to the server log, rotating it past 1 MiB. Never fails.
pub fn record_error(path: &Path, label: &str, message: &str) {
    if std::env::var("ULTRON_DEBUG_INTERNAL").as_deref() == Ok("1") {
        eprintln!("{label} {message}");
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_SERVER_LOG_BYTES) {
        let _ = std::fs::rename(path, format!("{}.1", path.display()));
    }
    use std::os::unix::fs::OpenOptionsExt;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
    {
        let time = session_file::js::iso_string(paths::now_ms());
        let _ = writeln!(file, "{time} {label} {message}");
    }
}

#[derive(Default)]
struct LifetimeState {
    connections: usize,
    workers: usize,
    /// The schedule timer holds a pending schedule (crate::prompt_schedules).
    schedules: bool,
    startup_held: bool,
    stopped: bool,
    timer: u64,
    armed: bool,
}

/// `ServerLifetime`: retires an automatically activated server once it has had no connection, no worker and no
/// pending scheduled prompt for the idle grace (after the startup grace, or the first connection).
pub struct Lifetime {
    keep_alive: bool,
    startup_grace: Duration,
    idle_grace: Duration,
    state: Mutex<LifetimeState>,
    retire: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    me: std::sync::Weak<Lifetime>,
}

impl Lifetime {
    pub fn new(keep_alive: bool) -> Arc<Lifetime> {
        Self::with_graces(keep_alive, AUTO_SERVER_STARTUP_GRACE, AUTO_SERVER_IDLE_GRACE)
    }

    pub fn with_graces(keep_alive: bool, startup_grace: Duration, idle_grace: Duration) -> Arc<Lifetime> {
        Arc::new_cyclic(|me| Lifetime {
            keep_alive,
            startup_grace,
            idle_grace,
            state: Mutex::new(LifetimeState {
                startup_held: !keep_alive,
                ..Default::default()
            }),
            retire: Mutex::new(None),
            me: me.clone(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LifetimeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn start(&self, retire: Box<dyn Fn() + Send + Sync>) {
        *self.retire.lock().unwrap_or_else(|e| e.into_inner()) = Some(retire);
        if self.lock().startup_held {
            let me = self.me.clone();
            std::thread::spawn(move || {
                let Some(grace) = me.upgrade().map(|m| m.startup_grace) else {
                    return;
                };
                std::thread::sleep(grace);
                if let Some(me) = me.upgrade() {
                    me.lock().startup_held = false;
                    me.reconcile();
                }
            });
        }
        self.reconcile();
    }

    pub fn set_connections(&self, count: usize) {
        {
            let mut state = self.lock();
            state.connections = count;
            if count > 0 {
                state.startup_held = false;
            }
        }
        self.reconcile();
    }

    pub fn set_workers(&self, count: usize) {
        self.lock().workers = count;
        self.reconcile();
    }

    /// Whether the server's schedule timer holds a pending schedule: while it does the server never retires on idle.
    pub fn set_schedules(&self, pending: bool) {
        self.lock().schedules = pending;
        self.reconcile();
    }

    /// Whether an idle retirement is armed (its grace running).
    pub fn retiring(&self) -> bool {
        self.lock().armed
    }

    pub fn stop(&self) {
        let mut state = self.lock();
        state.stopped = true;
        state.timer += 1;
    }

    fn idle(state: &LifetimeState) -> bool {
        !state.stopped && !state.startup_held && state.connections == 0 && state.workers == 0 && !state.schedules
    }

    fn reconcile(&self) {
        let mut state = self.lock();
        if self.keep_alive || !Self::idle(&state) {
            state.timer += 1;
            state.armed = false;
            return;
        }
        if state.armed || self.retire.lock().unwrap_or_else(|e| e.into_inner()).is_none() {
            return;
        }
        state.armed = true;
        let token = state.timer;
        let (me, grace) = (self.me.clone(), self.idle_grace);
        std::thread::spawn(move || {
            std::thread::sleep(grace);
            let Some(me) = me.upgrade() else { return };
            let fire = {
                let mut state = me.lock();
                let fire = state.timer == token && Self::idle(&state);
                state.armed = false;
                fire
            };
            if fire && let Some(retire) = &*me.retire.lock().unwrap_or_else(|e| e.into_inner()) {
                retire();
            }
        });
    }
}

#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// The instance directory: profile, locks, generation records and the server log.
    pub directory: PathBuf,
    pub session_dir: PathBuf,
    pub server_id: Option<String>,
    pub model: WorkerModel,
    /// Hold the server open without client or session demand (foreground servers).
    pub keep_alive: bool,
    /// The project root, recorded for `ultron instances`.
    pub root: Option<PathBuf>,
    /// The Radius relay host each generation runs (`startServerGeneration`'s `RadiusRelayHost`); `None` runs none.
    pub relay: Option<server::radius_relay::RelayConfig>,
}

struct Closed {
    done: Mutex<bool>,
    changed: Condvar,
}

/// A running server generation.
pub struct RunningServer {
    pub server_id: String,
    /// The coordinator's public socket: it reaches the newest generation of this server ID.
    pub socket_path: PathBuf,
    /// This generation's own endpoint.
    pub endpoint_path: PathBuf,
    hub: Arc<Hub>,
    manager: Arc<Manager>,
    coordinator: Arc<CoordinatorConnection>,
    lifetime: Arc<Lifetime>,
    record: PathBuf,
    closed: Arc<Closed>,
    closing: Mutex<bool>,
    relay: Mutex<Option<server::radius_relay::RadiusRelayHost>>,
    /// The timer of the sessions' scheduled prompts (crate::prompt_schedules).
    pub schedules: Arc<crate::prompt_schedules::Scheduler>,
}

impl RunningServer {
    pub fn worker_pids(&self) -> HashMap<String, u32> {
        self.manager.worker_pids()
    }

    /// Close this generation: busy workers are released (they keep running detached), idle ones exit.
    pub fn close(&self) {
        if std::mem::replace(&mut *self.closing.lock().unwrap_or_else(|e| e.into_inner()), true) {
            return self.wait_closed();
        }
        self.lifetime.stop();
        self.schedules.stop();
        let relay = self.relay.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(relay) = relay {
            relay.close();
        }
        let lost = self.coordinator.was_lost();
        if !lost {
            self.manager.prepare_shutdown();
        }
        self.hub.close();
        if lost {
            self.manager.detach();
        } else {
            self.manager.shutdown();
        }
        self.coordinator.close();
        let _ = sockets::cleanup_socket(&self.endpoint_path);
        let _ = std::fs::remove_file(&self.record);
        *self.closed.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.closed.changed.notify_all();
    }

    pub fn wait_closed(&self) {
        let mut done = self.closed.done.lock().unwrap_or_else(|e| e.into_inner());
        while !*done {
            done = self.closed.changed.wait(done).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// `startServer`: a generation of the directory's logical server, beside any others behind its coordinator.
pub fn start_server(options: &ServerOptions) -> io::Result<Arc<RunningServer>> {
    paths::ensure_private_directory(&options.directory)?;
    let profile = paths::acquire_profile(&options.directory, options.server_id.as_deref())?;
    match start_generation(options, &profile.server_id) {
        Err(Ok(RegistrationRejected(_))) => {
            // Another release's coordinator owns this ID (and its live sessions): run beside it under a derived ID.
            start_generation(options, &paths::fallback_server_id(&profile.server_id)).map_err(|e| match e {
                Ok(RegistrationRejected(path)) => {
                    io::Error::other(format!("Coordinator at {path} rejected the server registration"))
                }
                Err(e) => e,
            })
        }
        Err(Err(e)) => Err(e),
        Ok(server) => Ok(server),
    }
}

/// How often a generation registers again when the coordinator it leased exited before accepting it.
const COORDINATOR_REGISTRATION_ATTEMPTS: u32 = 5;

/// A hard-killed server leaves its endpoint socket behind: remove this ID's endpoints that refuse connections.
fn remove_dead_endpoints(directory: &Path, server_id: &str) {
    let prefix = format!("server-{server_id}-");
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(&prefix) && name.ends_with(".sock") && !sockets::is_live(&entry.path()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn start_generation(
    options: &ServerOptions,
    server_id: &str,
) -> Result<Arc<RunningServer>, io::Result<RegistrationRejected>> {
    let sockets_dir = instance::socket_dir(&options.directory);
    paths::ensure_private_directory(&sockets_dir).map_err(Err)?;
    let socket_path = sockets_dir.join(format!("{server_id}.sock"));
    let control_path = sockets_dir.join(format!("control-{server_id}.sock"));
    let nonce: String = paths::uuid_v4().replace('-', "")[..12].to_string();
    let endpoint_path = sockets_dir.join(format!("server-{server_id}-{nonce}.sock"));
    remove_dead_endpoints(&sockets_dir, server_id);
    session_core::startup_trace::trace_startup(
        std::env::var("ULTRON_STARTUP_TRACE").ok().as_deref(),
        "server.coordinator",
    );
    let mut lease = coord_client::ensure_coordinator(&socket_path, &control_path).map_err(Err)?;
    session_core::startup_trace::trace_startup(
        std::env::var("ULTRON_STARTUP_TRACE").ok().as_deref(),
        "server.coordinator-ready",
    );
    let lifetime = Lifetime::with_graces(
        options.keep_alive,
        AUTO_SERVER_STARTUP_GRACE,
        idle_grace(&|key| std::env::var(key).ok()),
    );
    let workers_lifetime = lifetime.clone();
    let manager = Manager::new(
        options.session_dir.clone(),
        options.model.clone(),
        Box::new(move |count| workers_lifetime.set_workers(count)),
    );
    let connections_lifetime = lifetime.clone();
    let hub = Hub::new(
        server_id.to_string(),
        options.session_dir.clone(),
        manager.clone(),
        error_log_path(&options.directory, server_id),
        Box::new(move |count| connections_lifetime.set_connections(count)),
    );
    // Every session of the durable instance has the Ultron persona on by default (a settings default under the
    // user's files, which an explicit `persona` value overrides).
    if is_durable_directory(&options.directory) {
        manager.set_settings_defaults(json!({resources::persona::SETTING: resources::persona::ULTRON}));
    }
    let log_path = error_log_path(&options.directory, server_id);
    manager.set_logger(Box::new(move |label, message| record_error(&log_path, label, message)));
    // Scheduled prompts: one timer per generation over the store's sessions; workers report their changes.
    let schedule_log = error_log_path(&options.directory, server_id);
    let schedules = crate::prompt_schedules::Scheduler::new(
        kernel_apis::prompt_schedule::clock_for(&|key| std::env::var(key).ok()),
        Arc::new(crate::prompt_schedules::HubDeliver(Arc::downgrade(&hub))),
        Some(Box::new(move |message| {
            record_error(&schedule_log, "[ultron server warning]", message)
        })),
    );
    let reports = schedules.clone();
    manager.on_schedules(Box::new(move |meta, active| reports.update(meta, active)));
    // A server holding a pending schedule stays up until it is done or cancelled.
    schedules.record_errors(options.directory.join(crate::prompt_schedules::ERRORS_FILE));
    let held = lifetime.clone();
    schedules.on_hold(Box::new(move |pending| held.set_schedules(pending)));
    let listener = sockets::listen(&endpoint_path).map_err(Err)?;
    hub.serve(listener);
    // The lease connects before the coordinator has accepted (and counted) it, so a coordinator whose empty
    // shutdown fires just then still exits: its control socket is gone (registration finds no socket, or is closed
    // unanswered). That coordinator is not another release's; start or find a live one and register again.
    let mut attempts = 0;
    let coordinator = loop {
        let events = manager.clone();
        let result = CoordinatorConnection::connect(
            &control_path,
            &endpoint_path,
            Box::new(move |event| events.handle_event(event)),
        );
        let exited = match &result {
            Err(Err(error)) => matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused),
            Err(Ok(_)) => !sockets::is_live(&control_path),
            Ok(_) => false,
        };
        attempts += 1;
        if exited && attempts < COORDINATOR_REGISTRATION_ATTEMPTS {
            match coord_client::ensure_coordinator(&socket_path, &control_path) {
                Ok(next) => {
                    lease = next;
                    continue;
                }
                Err(error) => {
                    hub.close();
                    let _ = sockets::cleanup_socket(&endpoint_path);
                    return Err(Err(error));
                }
            }
        }
        match result {
            Ok(coordinator) => break coordinator,
            Err(error) => {
                hub.close();
                let _ = sockets::cleanup_socket(&endpoint_path);
                return Err(error);
            }
        }
    };
    manager.set_coordinator(coordinator.clone());
    drop(lease);
    session_core::startup_trace::trace_startup(
        std::env::var("ULTRON_STARTUP_TRACE").ok().as_deref(),
        "server.connected",
    );
    manager.discover();
    hub.refresh();
    schedules.start(options.session_dir.clone());
    let record = options
        .directory
        .join(format!("generation-{}.json", coordinator.server_connection_id));
    let record_value = json!({
        "pid": std::process::id(), "serverId": server_id, "endpoint": endpoint_path.display().to_string(),
        "socket": socket_path.display().to_string(),
        "root": options.root.as_ref().map(|r| r.display().to_string()),
        "sessionDir": options.session_dir.display().to_string(),
    });
    let _ = std::fs::write(&record, format!("{record_value}\n"));
    let relay = options
        .relay
        .as_ref()
        .map(|relay| relay.start(server_id, endpoint_path.clone()));
    let running = Arc::new(RunningServer {
        server_id: server_id.to_string(),
        socket_path,
        endpoint_path,
        hub,
        manager,
        coordinator: coordinator.clone(),
        lifetime: lifetime.clone(),
        record,
        closed: Arc::new(Closed {
            done: Mutex::new(false),
            changed: Condvar::new(),
        }),
        closing: Mutex::new(false),
        relay: Mutex::new(relay),
        schedules,
    });
    // Only losing the coordinator itself ends a generation early.
    let lost = Arc::downgrade(&running);
    std::thread::spawn(move || {
        coordinator.wait_lost();
        if let Some(running) = lost.upgrade() {
            running.close();
        }
    });
    let retiring = Arc::downgrade(&running);
    lifetime.start(Box::new(move || {
        if let Some(running) = retiring.upgrade() {
            std::thread::spawn(move || running.close());
        }
    }));
    Ok(running)
}

/// `startForegroundServer`: this process's own generation, held open, serialized against cold activation.
pub fn start_foreground_server(options: &ServerOptions) -> io::Result<Arc<RunningServer>> {
    paths::ensure_private_directory(&options.directory)?;
    let server_id = paths::acquire_profile(&options.directory, options.server_id.as_deref())?.server_id;
    let _activation = paths::acquire_activation(&options.directory, &server_id)?;
    start_server(&ServerOptions {
        server_id: Some(server_id),
        keep_alive: true,
        ..options.clone()
    })
}

/// A reachable logical server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub server_id: String,
    pub path: PathBuf,
}

fn reachable(route: &Route) -> bool {
    let Ok(Some(stream)) = sockets::try_connect(&route.path) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(1_000)));
    client::Client::connect(stream, &route.server_id).is_ok()
}

/// `activateServer`: make the directory's logical server reachable, launching a detached server process if needed.
pub fn activate_server(options: &ServerOptions, keep_alive: bool) -> io::Result<Route> {
    paths::ensure_private_directory(&options.directory)?;
    let server_id = paths::acquire_profile(&options.directory, options.server_id.as_deref())?.server_id;
    let route = Route {
        path: instance::socket_dir(&options.directory).join(format!("{server_id}.sock")),
        server_id: server_id.clone(),
    };
    let _activation = paths::acquire_activation(&options.directory, &server_id)?;
    if reachable(&route) {
        return Ok(route);
    }
    let mut args = vec![
        options.directory.display().to_string(),
        server_id,
        options.session_dir.display().to_string(),
    ];
    if let Some(model) = &options.model.model {
        let mut selection = json!({"model": model});
        if let Some(provider) = &options.model.provider {
            selection = json!({"provider": provider, "model": model});
        }
        args.push(selection.to_string());
    }
    let mut env = Vec::new();
    if keep_alive {
        env.push((KEEP_ALIVE_ENV.to_string(), "1".to_string()));
    }
    if let Some(root) = &options.root {
        env.push(("__ULTRON_SERVER_ROOT".to_string(), root.display().to_string()));
    }
    let child = process::spawn(Role::Server, &args, &env)?;
    let pid = child.id();
    let exited = process::watch(child);
    let deadline = Instant::now() + ACTIVATION_TIMEOUT;
    loop {
        if reachable(&route) {
            return Ok(route);
        }
        let failure = if exited.try_recv().is_ok() {
            Some("Automatically activated server exited during startup")
        } else if Instant::now() >= deadline {
            Some("Timed out waiting for automatically activated server")
        } else {
            None
        };
        if let Some(message) = failure {
            process::kill(pid, rustix::process::Signal::KILL);
            return Err(io::Error::other(message));
        }
        std::thread::sleep(ACTIVATION_RETRY);
    }
}

fn parse_model(value: Option<&String>) -> Result<WorkerModel, String> {
    let Some(value) = value else {
        return Ok(WorkerModel::default());
    };
    let invalid = || "Internal server received invalid model options".to_string();
    let parsed: Value = serde_json::from_str(value).map_err(|_| invalid())?;
    let map = parsed.as_object().ok_or_else(invalid)?;
    if map.keys().any(|k| k != "provider" && k != "model") {
        return Err(invalid());
    }
    let model = map
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .ok_or_else(invalid)?;
    let provider = match map.get("provider") {
        None => None,
        Some(p) => Some(p.as_str().filter(|p| !p.is_empty()).ok_or_else(invalid)?.to_string()),
    };
    Ok(WorkerModel {
        provider,
        model: Some(model.to_string()),
        ..Default::default()
    })
}

/// `runServerProcess`: an automatically activated server, until its client and session demand disappears. Its
/// generations run `relay` (`startServerGeneration`'s `RadiusRelayHost` with the stored credential), which the
/// composition root builds from this process's environment.
pub fn run_server_process(args: &[String], relay: Option<server::radius_relay::RelayConfig>) -> Result<(), String> {
    if args.len() > 4 {
        return Err("Internal server received unexpected arguments".into());
    }
    let directory = args.first().filter(|d| Path::new(d).is_absolute());
    let directory = directory.ok_or("Internal server requires an absolute server directory")?;
    let server_id = args.get(1).filter(|id| protocol::is_server_id(id));
    let server_id = server_id.ok_or("Internal server requires a canonical server ID")?;
    let session_dir = args.get(2).filter(|d| Path::new(d).is_absolute());
    let session_dir = session_dir.ok_or("Internal server requires an absolute Session directory")?;
    let model = parse_model(args.get(3))?;
    // The durable instance works in the home directory (its sessions are created there), wherever it was activated.
    durable_home(Path::new(directory));
    let server = start_server(&ServerOptions {
        directory: PathBuf::from(directory),
        session_dir: PathBuf::from(session_dir),
        server_id: Some(server_id.clone()),
        model,
        keep_alive: std::env::var(KEEP_ALIVE_ENV).as_deref() == Ok("1"),
        root: std::env::var_os("__ULTRON_SERVER_ROOT").map(PathBuf::from),
        relay,
    })
    .map_err(|e| e.to_string())?;
    // A signal closes the generation (busy workers keep running detached) and removes its endpoint.
    let signalled = server.clone();
    crate::signals::set_handler(Arc::new(move |_| {
        signalled.close();
        std::process::exit(0);
    }));
    server.wait_closed();
    Ok(())
}

/// `ultron service run`: the durable instance's server in this process, held open (a supervised service), until a
/// signal. Serialized against cold activation as a foreground server is.
pub fn serve(options: &ServerOptions) -> Result<(), String> {
    durable_home(&options.directory);
    let server = start_foreground_server(options).map_err(|e| e.to_string())?;
    let signalled = server.clone();
    crate::signals::set_handler(Arc::new(move |_| {
        signalled.close();
        std::process::exit(0);
    }));
    server.wait_closed();
    Ok(())
}
