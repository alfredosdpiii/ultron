//! The session worker process (experimental/session-worker.ts `run`): registers with the coordinator as a peer,
//! holds the session file's lock, runs the session (crate::session) on its own thread, serves the session services
//! to each attachment scope, and follows its owner's demand until it retires, is released or shut down.

use crate::chord::{Call, Endpoint, Provider, Service, ServiceError, ServiceResult};
use crate::control::{LineReader, LineWriter};
use crate::coordinator::COORDINATOR_PROTOCOL_VERSION;
use crate::lane::Lane;
use crate::lifecycle::{Hold, Lifecycle};
use crate::watchdog::{Change, Progress};
use base64::Engine;
use serde_json::{Value, json};
use session_core::compose::Services;
use session_core::core::{Input, Op, OpenOptions, QueueKind};
use session_core::{ModelFlags, RunEnv};
use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};
use std::time::{Duration, Instant};

pub const CONTROL_ADDRESS_ENV: &str = "PI_SESSION_WORKER_CONTROL_ADDRESS";
pub const CONTROL_TOKEN_ENV: &str = "PI_SESSION_WORKER_CONTROL_TOKEN";
pub const SESSION_KEY_ENV: &str = "PI_SESSION_WORKER_SESSION_KEY_BASE64";
pub const PEER_ID_ENV: &str = "PI_SESSION_WORKER_PEER_ID";
pub const API_KEY_ENV: &str = "PI_SESSION_WORKER_API_KEY";
pub const OWNER_ENV: &str = "PI_SESSION_WORKER_OWNER";
pub const INITIAL_DEMAND_GRACE_ENV: &str = "__PI_SESSION_WORKER_INITIAL_DEMAND_GRACE_MS";
pub const ORPHAN_DEMAND_GRACE_ENV: &str = "__PI_SESSION_WORKER_ORPHAN_DEMAND_GRACE_MS";
pub const DISCOVERY_GRACE_ENV: &str = "__PI_SESSION_WORKER_DISCOVERY_GRACE_MS";

const DEFAULT_INITIAL_DEMAND_GRACE_MS: u64 = 10_000;
const DEFAULT_ORPHAN_DEMAND_GRACE_MS: u64 = 1_000;
const DEFAULT_DISCOVERY_GRACE_MS: u64 = 5_000;

/// How long `abortStuck` waits for an aborted run to end before it restarts the worker.
pub const ABORT_GRACE_ENV: &str = "ULTRON_ABORT_GRACE_MS";
const DEFAULT_ABORT_GRACE_MS: u64 = 10_000;
/// The exit code of a worker that restarts itself because an aborted run would not stop.
pub const RESTART_EXIT_CODE: i32 = 75;

/// Operations that wait on a run (or a long poll) execute off the command reader, so it keeps serving the rest.
/// The others run in order on one operation thread; the reader itself only dispatches, so it always answers the
/// server's heartbeat.
const LONG_OPERATIONS: [(&str, &str); 10] = [
    // `/mcp` may wait on the client's dialogs (its menu, a sign-in's redirect prompt).
    ("pi.legacy-extension-commands", "run"),
    ("pi.agent-controller", "prompt"),
    ("pi.agent-controller", "resume"),
    ("pi.agent-controller", "compact"),
    ("pi.agent-controller", "navigate"),
    ("ultron.extension-ui", "poll"),
    ("ultron.session-control", "bash"),
    ("ultron.session-control", "readTree"),
    // A session operation may be a compaction or a bash command; `abortStuck` waits for the run to stop.
    (OPS_SERVICE, "op"),
    (OPS_SERVICE, "abortStuck"),
];

fn lifecycle_delay(name: &str, fallback: u64) -> Result<Duration, String> {
    match std::env::var(name) {
        Err(_) => Ok(Duration::from_millis(fallback)),
        Ok(value) => value
            .parse::<u64>()
            .map(Duration::from_millis)
            .map_err(|_| format!("{name} must be a non-negative safe integer")),
    }
}

/// The parsed `SessionWorkerOptions` argument.
#[derive(Debug, Clone)]
pub struct WorkerOptions {
    pub metadata: Value,
    pub session_dir: String,
    pub plugin_manifest_paths: Vec<String>,
    pub session: ModelFlags,
    /// `print`, `json`, `rpc` or `interactive` (`extensionMode`).
    pub mode: String,
    /// `noNaming`: the session is not named.
    pub no_naming: bool,
    /// `noExtensions` (Pi `--no-extensions`): no extensions, and no MCP servers.
    pub no_extensions: bool,
}

pub fn parse_options(argument: &str) -> Result<WorkerOptions, String> {
    let options: Value = serde_json::from_str(argument).map_err(|_| "Session worker received invalid options")?;
    let absolute = |v: &Value| v.as_str().is_some_and(|p| Path::new(p).is_absolute());
    let metadata = &options["metadata"];
    let text = |key: &str| options[key].as_str().map(str::to_string);
    if !absolute(&options["sessionDir"])
        || !absolute(&metadata["cwd"])
        || !absolute(&metadata["path"])
        || !metadata["id"].is_string()
        || (options.get("provider").is_some() && options.get("model").is_none())
    {
        return Err("Session worker received invalid options".into());
    }
    Ok(WorkerOptions {
        metadata: metadata.clone(),
        plugin_manifest_paths: options["pluginManifestPaths"]
            .as_array()
            .map(|p| p.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        session_dir: options["sessionDir"].as_str().unwrap_or_default().to_string(),
        session: ModelFlags {
            provider: text("provider"),
            model: text("model"),
            api_key: std::env::var(API_KEY_ENV).ok(),
            thinking: text("thinking"),
            // The worker protocol carries no `--models` (as TypeScript's); the settings' `enabledModels` apply.
            models: None,
        },
        mode: text("extensionMode").unwrap_or_else(|| "interactive".into()),
        no_naming: options["noNaming"] == true,
        no_extensions: options["noExtensions"] == true,
    })
}

struct Shared {
    token: String,
    session_key: String,
    session_id: String,
    metadata: Value,
    plugin_manifest_paths: Vec<String>,
    writer: LineWriter,
    lifecycle: Mutex<Option<Arc<Lifecycle>>>,
    endpoints: Mutex<HashMap<String, (Value, Arc<Endpoint>)>>,
    provider: Arc<Provider>,
    core: Sender<Input>,
    closing: Mutex<bool>,
    ready: Mutex<bool>,
    _lock: Mutex<Option<crate::lock::Lock>>,
    _owner: Mutex<Option<durable::owner::OwnerLock>>,
    progress: Arc<Progress>,
    /// The `health` state of the operations service (crate::watchdog).
    health: Arc<crate::chord::State>,
    stuck_after: Option<Duration>,
    /// The ordered operation thread.
    jobs: Mutex<mpsc::Sender<Box<dyn FnOnce() + Send>>>,
}

impl Shared {
    fn lifecycle(&self) -> Arc<Lifecycle> {
        self.lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("the lifecycle is set before commands are read")
    }

    /// Send an event to one server: `to` defaults to the current owner, else the newest server.
    fn send(&self, payload: Value, to: Option<&str>) -> std::io::Result<()> {
        let owner = self
            .lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|l| l.owner());
        let to = to.map(str::to_string).or(owner).unwrap_or_else(|| "server".into());
        self.writer.send(&json!({"type": "send", "to": to, "payload": payload}))
    }

    fn worker_ready(&self) -> Value {
        json!({"type": "worker_ready", "token": self.token, "sessionKey": self.session_key, "sessionId": self.session_id,
            "pid": std::process::id(), "metadata": self.metadata, "pluginManifestPaths": self.plugin_manifest_paths,
            "busy": self.progress.busy()})
    }

    /// `worker_status`: whether a run is in progress and its health (the heartbeat answer, sent on every change).
    fn status(&self, nonce: Value) -> Value {
        let busy = self.progress.busy()
            || self
                .lifecycle
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|l| l.busy());
        json!({"type": "worker_status", "token": self.token, "sessionKey": self.session_key, "nonce": nonce,
            "pid": std::process::id(), "busy": busy, "health": self.progress.health(session_core::lane::now_ms(), self.stuck_after)})
    }

    fn report(&self) {
        self.health
            .replace(self.progress.health(session_core::lane::now_ms(), self.stuck_after));
        let _ = self.send(self.status(Value::Null), None);
    }

    /// A run started or ended (it holds retirement while it runs), or a stuck run moved again.
    /// `holdActivity`: detached work is active (held as an operation) or no longer.
    fn activity(&self, active: bool) {
        let Some(lifecycle) = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return;
        };
        if active {
            lifecycle.operation_started("activity", "main", "detached");
        } else if lifecycle.busy() {
            lifecycle.operation_stopped("activity", "main", "detached");
        }
    }

    fn progress_changed(&self, change: Change) {
        let lifecycle = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match (&change, &lifecycle) {
            (Change::Started(run), Some(lifecycle)) => lifecycle.operation_started("run", "main", run),
            (Change::Ended(run), Some(lifecycle)) => lifecycle.operation_stopped("run", "main", run),
            _ => {}
        }
        self.report();
    }

    fn remove_subscriptions(&self, matches: impl Fn(&Value) -> bool) {
        let removed: Vec<Arc<Endpoint>> = {
            let mut endpoints = self.endpoints.lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<String> = endpoints
                .iter()
                .filter(|(_, (scope, _))| matches(scope))
                .map(|(k, _)| k.clone())
                .collect();
            keys.into_iter()
                .filter_map(|k| endpoints.remove(&k))
                .map(|(_, e)| e)
                .collect()
        };
        for endpoint in removed {
            endpoint.dispose();
        }
    }

    /// `closeAndExit`: stop the session (kernel, session file) and exit.
    fn close_and_exit(&self, code: i32) -> ! {
        self.exit_with(code, false)
    }

    /// Stop and exit leaving the session to a successor worker (a signal, or the lost server: the run stays open
    /// in the journal and the next worker recovers it): running shell jobs keep running for it to follow.
    fn hand_over_and_exit(&self, code: i32) -> ! {
        self.exit_with(code, true)
    }

    fn exit_with(&self, code: i32, hand_over: bool) -> ! {
        let first = !std::mem::replace(&mut *self.closing.lock().unwrap_or_else(|e| e.into_inner()), true);
        if first {
            self.remove_subscriptions(|_| true);
            let (done, wait) = mpsc::channel();
            let input = if hand_over {
                Input::HandOver(done)
            } else {
                Input::Close(done)
            };
            if self.core.send(input).is_ok() {
                let _ = wait.recv_timeout(Duration::from_secs(10));
            }
            self._lock.lock().unwrap_or_else(|e| e.into_inner()).take();
            std::process::exit(code);
        }
        loop {
            std::thread::park();
        }
    }
}

fn scope_key(scope: &Value) -> String {
    format!(
        "{}\0{}",
        scope["serverConnectionId"].as_str().unwrap_or_default(),
        scope["attachmentId"].as_str().unwrap_or_default()
    )
}

/// The Rust session-operations service (not in TypeScript's catalogue order; listed last).
pub const OPS_SERVICE: &str = "ultron.session-ops";

/// One operation of the session actor, answered with the actor's result as a service result.
fn call_core(core: &Sender<Input>, op: Op) -> ServiceResult {
    session_core::core::call(core, op).map_err(|message| ServiceError::coded("session_operation_failed", message))
}

/// The first argument of a call as a request object.
fn request(args: &[Value]) -> &Value {
    args.first().unwrap_or(&Value::Null)
}

/// The session services in their catalogue order, over the session actor `core`.
/// What the operations service's watchdog members use.
struct Watch {
    progress: Arc<Progress>,
    health: Arc<crate::chord::State>,
    stuck_after: Option<Duration>,
    abort_grace: Duration,
    shared: Arc<OnceLock<Weak<Shared>>>,
}

/// The session's extension UI and its MCP gateway (once the session's stack opened it).
struct Extensions {
    ui: Arc<crate::extension_ui::ExtensionUiBridge>,
    mcp: Arc<OnceLock<Arc<mcp::gateway::Gateway>>>,
}

fn services(
    lane: &Lane,
    models_state: Arc<crate::chord::State>,
    name_state: Arc<crate::chord::State>,
    core: Sender<Input>,
    lifecycle: LifecycleRef,
    watch: Watch,
    extensions: Extensions,
) -> Provider {
    let navigate_core = core.clone();
    let (prompt_core, abort_core, steer_core, follow_core, cancel_core, compact_core) = (
        core.clone(),
        core.clone(),
        core.clone(),
        core.clone(),
        core.clone(),
        core.clone(),
    );
    let prompt_lifecycle = lifecycle.clone();
    let agent = Service::new("pi.agent-controller")
        .method("cancelQueued", move |args| {
            let entry_id = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
            call_core(&cancel_core, Op::CancelQueued { entry_id })
        })
        .method("compact", move |args| {
            let instructions = request(&args)["customInstructions"].as_str().map(str::to_string);
            call_core(&compact_core, Op::Compact { instructions })
        })
        .method("followUp", move |args| {
            let message = request(&args)["message"].as_str().unwrap_or_default().to_string();
            call_core(
                &follow_core,
                Op::Enqueue {
                    kind: QueueKind::FollowUp,
                    message,
                    images: Vec::new(),
                    entry_id: None,
                },
            )
        })
        .method("navigate", move |args| {
            // `{targetId, summarize, label, customInstructions}`; a long operation (a summary is a model call).
            let request = request(&args);
            call_core(
                &navigate_core,
                Op::Navigate {
                    target_id: request["targetId"].as_str().map(str::to_string),
                    summarize: request["summarize"] == true,
                    instructions: request["customInstructions"].as_str().map(str::to_string),
                    label: request["label"].as_str().map(str::to_string),
                },
            )
        })
        .unported("nextRun")
        .method("prompt", move |args| {
            let message = request(&args)["message"]
                .as_str()
                .ok_or_else(|| ServiceError::internal("Invalid prompt request"))?
                .to_string();
            let images = request(&args)["images"].as_array().cloned().unwrap_or_default();
            let lifecycle = prompt_lifecycle.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let marker = format!("prompt-{}", crate::paths::uuid_v4());
            if let Some(lifecycle) = &lifecycle {
                lifecycle.operation_started("run", "main", &marker);
            }
            let result = call_core(&prompt_core, Op::PromptAndWait { message, images });
            if let Some(lifecycle) = &lifecycle {
                lifecycle.operation_stopped("run", "main", &marker);
            }
            result
        })
        .method("requestAbort", move |_| call_core(&abort_core, Op::Abort))
        .unported("resume")
        .method("steer", move |args| {
            let message = request(&args)["message"].as_str().unwrap_or_default().to_string();
            call_core(
                &steer_core,
                Op::Enqueue {
                    kind: QueueKind::Steer,
                    message,
                    images: Vec::new(),
                    entry_id: None,
                },
            )
        });
    let plugins = Service::new("pi.session-plugins").method("reload", |_| Ok(None));
    // The extension processes' commands (session_core::extension_commands) and `/mcp`, the built-in MCP extension's
    // command (extensions/mcp): `/mcp` runs here, its dialogs and notices go to the client through the extension UI.
    let (list_core, run_core) = (core.clone(), core.clone());
    let (list_mcp, run_mcp, run_ui) = (extensions.mcp.clone(), extensions.mcp.clone(), extensions.ui.clone());
    let legacy = Service::new("pi.legacy-extension-commands")
        .method("list", move |_| {
            let commands = call_core(&list_core, Op::Commands)?.unwrap_or_default();
            let listed =
                session_core::extension_commands::from_listing(commands.as_array().map_or(&[][..], Vec::as_slice));
            let has_mcp = list_mcp.get().is_some();
            Ok(Some(
                listed
                    .iter()
                    .filter(|c| has_mcp || c.name != "mcp")
                    .map(session_core::extension_commands::command_info)
                    .collect(),
            ))
        })
        .method("run", move |args| {
            let text = |i: usize| args.get(i).and_then(Value::as_str).map(str::to_string);
            let (Some(name), Some(args)) = (text(0), text(1)) else {
                return Err(ServiceError::internal("Extension command service is unavailable"));
            };
            if name == "mcp"
                && let Some(gateway) = run_mcp.get()
            {
                gateway.run_command(&args, &crate::extension_ui::BridgeUi(run_ui.clone()));
                return Ok(Some(json!({"notifications": []})));
            }
            // A command's failure is a plain error of the service, as TypeScript's `runCommand` throws it (it crosses
            // the wire as an internal error).
            session_core::core::call(&run_core, Op::ExtensionCommand { name, args }).map_err(ServiceError::internal)
        });
    let (stuck_core, health_progress) = (core.clone(), watch.progress.clone());
    let (op_core, bash_core, abort_bash_core, name_core, forked_core, tree_core, inspect_core) = (
        core.clone(),
        core.clone(),
        core.clone(),
        core.clone(),
        core.clone(),
        core.clone(),
        core.clone(),
    );
    let commands_core = core.clone();
    let mut control = Service::new("ultron.session-control")
        .method("abortBash", move |_| call_core(&abort_bash_core, Op::AbortBash))
        .method("bash", move |args| {
            let command = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
            let exclude = args.get(1) == Some(&Value::Bool(true));
            call_core(&bash_core, Op::Bash { command, exclude })
        })
        .method("forked", move |args| match args.first().and_then(Value::as_str) {
            Some(target) => call_core(
                &forked_core,
                Op::Forked {
                    target: target.to_string(),
                },
            ),
            None => Ok(None),
        })
        .method("inspect", move |args| match args.first().and_then(Value::as_str) {
            // Root-owned work whose completion may still re-invoke the root: the running run.
            Some("async.pending") => call_core(&inspect_core, Op::AsyncPending),
            // Every other request through the session's inspector (session-core `inspect`), which refuses anything
            // that is not a read-only inspection.
            other => call_core(
                &inspect_core,
                Op::Inspect {
                    request: other.unwrap_or_default().to_string(),
                    payload: args.get(1).cloned().unwrap_or(Value::Null),
                },
            ),
        })
        .method("readTree", move |_| {
            let entries = call_core(&tree_core, Op::Entries { since: None })?.unwrap_or_default();
            Ok(Some(
                json!({"entries": entries["entries"], "leafId": entries["leafId"]}),
            ))
        })
        .method("setName", move |args| {
            let name = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
            call_core(&name_core, Op::SetName { name })
        })
        .method("listCommands", move |_| {
            let commands = call_core(&commands_core, Op::Commands)?.unwrap_or_default();
            let commands = commands.as_array().map_or(&[][..], Vec::as_slice);
            Ok(Some(
                session_core::extension_commands::session_command_infos(commands).into(),
            ))
        });
    for name in [
        "beforeFork",
        "debugInfo",
        "getSettings",
        "readSettings",
        "reloadAuth",
        "setAutoCompaction",
        "setAutoRetry",
        "setFollowUpMode",
        "setLabel",
        "setProjectTrust",
        "setSetting",
        "setSteeringMode",
    ] {
        control = control.unported(name);
    }
    let (poll_ui, respond_ui) = (extensions.ui.clone(), extensions.ui);
    let extension_ui = Service::new("ultron.extension-ui")
        .method("poll", move |args| {
            let after = args.first().and_then(Value::as_u64);
            let wait = args.get(1).and_then(Value::as_u64).unwrap_or(0);
            Ok(Some(poll_ui.poll(after, Duration::from_millis(wait))))
        })
        .method("respond", move |args| {
            if let (Some(id), Some(answer)) = (args.first().and_then(Value::as_str), args.get(1)) {
                respond_ui.respond(id, answer.clone());
            }
            Ok(None)
        });
    let (cycle_core, levels_core, select_core, thinking_core) = (core.clone(), core.clone(), core.clone(), core);
    let (select_state, thinking_state) = (models_state.clone(), models_state.clone());
    let models = Service::new("pi.models")
        .method("cycleThinking", move |_| call_core(&cycle_core, Op::CycleThinking))
        .method("getThinkingLevels", move |_| {
            call_core(&levels_core, Op::ThinkingLevels)
        })
        .unported("refresh")
        .method("select", move |args| {
            let target = request(&args);
            let provider = target["provider"].as_str().unwrap_or_default().to_string();
            let model_id = target["modelId"].as_str().unwrap_or_default().to_string();
            let model = call_core(&select_core, Op::SetModel { provider, model_id })?;
            if let Some(model) = &model {
                select_state.change(|state| {
                    state["configuration"]["model"] = json!({"provider": model["provider"], "modelId": model["id"]});
                });
            }
            Ok(model)
        })
        .method("selectThinking", move |args| {
            let level = args.first().and_then(Value::as_str).unwrap_or_default().to_string();
            call_core(&thinking_core, Op::SetThinking { level: level.clone() })?;
            thinking_state.change(|state| state["configuration"]["thinkingLevel"] = json!(level));
            Ok(None)
        })
        .state("state", models_state);
    let transcript = Service::new("pi.transcript").state("state", lane.transcript.clone());
    // Rust-only: one session operation (session-core `Op` as JSON), the RPC and TUI clients' whole command set.
    let ops = Service::new(OPS_SERVICE)
        .method("op", move |args| {
            let op = Op::from_json(request(&args)).map_err(ServiceError::internal)?;
            call_core(&op_core, op)
        })
        // The run's progress (crate::watchdog), now.
        .method("checkHealth", move |_| {
            Ok(Some(
                health_progress.health(session_core::lane::now_ms(), watch.stuck_after),
            ))
        })
        // Abort the run in progress; one that does not stop within the grace restarts the worker, whose recovery
        // ends it as aborted (its cancel request is journaled first).
        .method("abortStuck", move |_| {
            let Some(run) = watch.progress.run() else {
                return Ok(Some(json!({"aborted": false, "reason": "no run in progress"})));
            };
            call_core(&stuck_core, Op::Abort)?;
            let deadline = Instant::now() + watch.abort_grace;
            while Instant::now() < deadline {
                if watch.progress.run().as_deref() != Some(run.as_str()) {
                    return Ok(Some(json!({"aborted": true, "runId": run, "restarted": false})));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            if let Some(shared) = watch.shared.get().and_then(Weak::upgrade) {
                std::thread::spawn(move || {
                    // Let the answer leave first.
                    std::thread::sleep(Duration::from_millis(100));
                    shared.close_and_exit(RESTART_EXIT_CODE)
                });
            }
            Ok(Some(json!({"aborted": true, "runId": run, "restarted": true})))
        })
        // The session's display name (`{name}`), as it changes.
        .state("name", name_state)
        .state("health", watch.health)
        .unlisted();
    Provider {
        services: vec![agent, plugins, legacy, control, extension_ui, models, transcript, ops],
    }
}

type LifecycleRef = Arc<Mutex<Option<Arc<Lifecycle>>>>;

/// `runSessionWorkerWithHarness`: run until retirement, release or shutdown; never returns on success.
pub fn run(args: &[String], docs: PathBuf) -> Result<(), String> {
    let [argument] = args else {
        return Err("Session worker requires one options argument".into());
    };
    let options = parse_options(argument)?;
    let address = std::env::var(CONTROL_ADDRESS_ENV).ok();
    let token = std::env::var(CONTROL_TOKEN_ENV).ok();
    let encoded_key = std::env::var(SESSION_KEY_ENV).ok();
    let (Some(address), Some(token), Some(encoded_key)) = (address, token, encoded_key) else {
        return Err("Session worker requires a control address".into());
    };
    let peer_id = std::env::var(PEER_ID_ENV).map_err(|_| "Session worker requires a peer ID")?;
    let session_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded_key.trim_end_matches('='))
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or("Session worker received an invalid session key")?;
    let stream = UnixStream::connect(&address).map_err(|e| e.to_string())?;
    let read = stream.try_clone().map_err(|e| e.to_string())?;
    let writer = LineWriter::new(stream);
    let mut reader = LineReader::new(read);
    writer
        .send(&json!({"type": "register_peer", "protocol": COORDINATOR_PROTOCOL_VERSION, "peerId": peer_id}))
        .map_err(|e| e.to_string())?;
    let registered = reader.read_value().ok().flatten();
    if registered.as_ref().map(|r| &r["type"]) != Some(&json!("peer_registered")) {
        return Err("Coordinator rejected the session worker registration".into());
    }
    let owner = std::env::var(OWNER_ENV)
        .ok()
        .filter(|o| !o.is_empty())
        .or_else(|| registered.and_then(|r| r["serverConnectionId"].as_str().map(str::to_string)));
    let fail_coded = |code: Option<&str>, message: String| -> String {
        let to = owner.clone().unwrap_or_else(|| "server".into());
        let mut payload =
            json!({"type": "worker_failed", "token": token, "sessionKey": session_key, "message": message});
        if let Some(code) = code {
            payload["code"] = json!(code);
        }
        let _ = writer.send(&json!({"type": "send", "to": to, "payload": payload}));
        message
    };
    let fail = |message: String| fail_coded(None, message);
    let metadata = options.metadata.clone();
    let path = PathBuf::from(metadata["path"].as_str().unwrap_or_default());
    let session_id = metadata["id"].as_str().unwrap_or_default().to_string();
    // One writer per session file, across every server and instance: a live owner refuses this worker at once.
    let owner_lock = acquire_owner(&path).map_err(|error| match error {
        durable::owner::OwnerError::InUse { pid } => {
            fail_coded(Some("service_invalid_value"), session_in_use(&session_id, pid))
        }
        durable::owner::OwnerError::Io(error) => fail(error.to_string()),
    })?;
    let lock = crate::lock::Lock::acquire(
        &path,
        Duration::from_millis(2_000),
        Duration::from_millis(25),
        Duration::from_millis(8_000),
    )
    .map_err(|e| fail(e.to_string()))?;
    let env = RunEnv::process(PathBuf::from(metadata["cwd"].as_str().unwrap_or_default()), docs);
    let session_metadata = session_journal::repo::read_metadata(&path)
        .ok_or_else(|| fail(format!("Not a session file: {}", path.display())))?;
    let session_root = PathBuf::from(options.session_dir.clone());
    // The actor reports no event while it opens; the transcript state starts at its snapshot.
    let lane_slot: Arc<std::sync::OnceLock<Arc<Mutex<Lane>>>> = Arc::new(std::sync::OnceLock::new());
    let sink_lane = lane_slot.clone();
    // The client this worker is started for serves the extension UI (Pi's `extensionMode` tui or rpc).
    let ui =
        crate::extension_ui::ExtensionUiBridge::new(matches!(options.mode.as_str(), "interactive" | "tui" | "rpc"));
    let gateway_slot: Arc<OnceLock<Arc<mcp::gateway::Gateway>>> = Arc::new(OnceLock::new());
    let (notice_ui, slot) = (ui.clone(), gateway_slot.clone());
    let open = OpenOptions {
        env,
        flags: options.session.clone(),
        session_root,
        metadata: session_metadata,
        mode: options.mode.clone(),
        services: Services {
            on_mcp: Some(Arc::new(move |gateway: &Arc<mcp::gateway::Gateway>| {
                let _ = slot.set(gateway.clone());
            })),
            mcp_notices: Some(Arc::new(move |message: &str, level: &str| {
                mcp::gateway::CommandUi::notify(&crate::extension_ui::BridgeUi(notice_ui.clone()), message, level);
            })),
            no_extensions: options.no_extensions,
            ..Services::default()
        },
        auto_name: !options.no_naming,
    };
    let name_state = crate::chord::State::new(json!({"name": null}));
    let names = name_state.clone();
    let progress = Arc::new(Progress::default());
    let stuck_after = crate::watchdog::stuck_after(&|k| std::env::var(k).ok());
    let health_state = crate::chord::State::new(progress.health(session_core::lane::now_ms(), stuck_after));
    let shared_slot: Arc<OnceLock<Weak<Shared>>> = Arc::new(OnceLock::new());
    let (sink_progress, sink_shared) = (progress.clone(), shared_slot.clone());
    let (core, _actor, summary) = session_core::core::spawn(
        open,
        Box::new(move |event| {
            if event["type"] == "activity" {
                // Detached work (jobs, subagents, pending events) holds retirement as a run does; it is not a run's
                // progress (the stuck-run watchdog never sees it).
                if let Some(shared) = sink_shared.get().and_then(Weak::upgrade) {
                    shared.activity(event["active"] == true);
                }
                return;
            }
            let change = sink_progress.event(&event, session_core::lane::now_ms());
            if change != Change::None
                && let Some(shared) = sink_shared.get().and_then(Weak::upgrade)
            {
                shared.progress_changed(change);
            }
            if event["type"] == "session_name" {
                names.replace(json!({"name": event["name"]}));
            } else if let Some(lane) = sink_lane.get() {
                lane.lock().unwrap_or_else(|e| e.into_inner()).publish(event);
            }
        }),
    )
    .map_err(fail)?;
    let lane = lane_slot
        .get_or_init(|| Arc::new(Mutex::new(Lane::new(summary["snapshot"].clone()))))
        .clone();
    let model = &summary["model"];
    let models_state = crate::chord::State::with_sequence(
        json!({
            "catalog": {"revision": 1, "availableModels": [{"provider": model["provider"], "modelId": model["id"], "name": model["name"], "reasoning": model["reasoning"]}]},
            "configuration": {"model": {"provider": model["provider"], "modelId": model["id"]}, "thinkingLevel": summary["thinkingLevel"]},
            "refresh": {"status": "idle"},
        }),
        1,
    );
    let lifecycle_slot: LifecycleRef = Arc::new(Mutex::new(None));
    if let Ok(Some(described)) = session_core::core::call(&core, Op::Describe) {
        name_state.replace(json!({"name": described["name"]}));
    }
    let watch = Watch {
        progress: progress.clone(),
        health: health_state.clone(),
        stuck_after,
        abort_grace: lifecycle_delay(ABORT_GRACE_ENV, DEFAULT_ABORT_GRACE_MS)?,
        shared: shared_slot.clone(),
    };
    let provider = Arc::new(services(
        &lane.lock().unwrap_or_else(|e| e.into_inner()),
        models_state,
        name_state,
        core.clone(),
        lifecycle_slot.clone(),
        watch,
        Extensions {
            ui: ui.clone(),
            mcp: gateway_slot,
        },
    ));
    let (jobs, queue) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
    std::thread::spawn(move || {
        for job in queue {
            job();
        }
    });
    let shared = Arc::new(Shared {
        token: token.clone(),
        session_key: session_key.clone(),
        session_id: session_id.clone(),
        metadata,
        plugin_manifest_paths: options.plugin_manifest_paths.clone(),
        writer: writer.clone(),
        lifecycle: Mutex::new(None),
        endpoints: Mutex::new(HashMap::new()),
        provider,
        core,
        closing: Mutex::new(false),
        ready: Mutex::new(false),
        _lock: Mutex::new(Some(lock)),
        _owner: Mutex::new(Some(owner_lock)),
        progress,
        health: health_state,
        stuck_after,
        jobs: Mutex::new(jobs),
    });
    let _ = shared_slot.set(Arc::downgrade(&shared));
    let retiring = Arc::downgrade(&shared);
    let lifecycle = Lifecycle::new(
        owner,
        lifecycle_delay(INITIAL_DEMAND_GRACE_ENV, DEFAULT_INITIAL_DEMAND_GRACE_MS)?,
        lifecycle_delay(ORPHAN_DEMAND_GRACE_ENV, DEFAULT_ORPHAN_DEMAND_GRACE_MS)?,
        Box::new(move || {
            // Announce the deliberate retirement before exiting; the coordinator delivers it before the disconnect.
            if let Some(shared) = retiring.upgrade() {
                std::thread::spawn(move || {
                    let event =
                        json!({"type": "worker_retiring", "token": shared.token, "sessionKey": shared.session_key});
                    let _ = shared.send(event, None);
                    shared.close_and_exit(0);
                });
            }
        }),
    );
    *shared.lifecycle.lock().unwrap_or_else(|e| e.into_inner()) = Some(lifecycle.clone());
    *lifecycle_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(lifecycle);
    let discovery_grace = lifecycle_delay(DISCOVERY_GRACE_ENV, DEFAULT_DISCOVERY_GRACE_MS)?;
    *shared.ready.lock().unwrap_or_else(|e| e.into_inner()) = true;
    if shared.send(shared.worker_ready(), None).is_err() {
        shared.close_and_exit(1);
    }
    // A signal stops the session (its run stays open in the journal; the next worker recovers it) and exits.
    let signalled = Arc::downgrade(&shared);
    crate::signals::set_handler(Arc::new(move |_| {
        if let Some(shared) = signalled.upgrade() {
            shared.hand_over_and_exit(0);
        }
        std::process::exit(0);
    }));
    // Continue what a dead worker left (an interrupted run, queued messages), without waiting for a client.
    let recover = shared.core.clone();
    std::thread::spawn(move || {
        let _ = session_core::core::call(&recover, Op::Recover);
    });
    spawn_watchdog(&shared);
    loop {
        let message = match reader.read_value() {
            Ok(Some(message)) => message,
            // The server (or its coordinator) is gone; a new one starts a successor for the session.
            _ => shared.hand_over_and_exit(0),
        };
        handle_input(&shared, &message, discovery_grace);
    }
}

fn handle_input(shared: &Arc<Shared>, message: &Value, discovery_grace: Duration) {
    let lifecycle = shared.lifecycle();
    match message["type"].as_str() {
        Some("server_connected") => {}
        Some("server_disconnected") => {
            let server = message["serverConnectionId"].as_str().unwrap_or_default().to_string();
            shared.remove_subscriptions(|scope| scope["serverConnectionId"] == json!(server));
            lifecycle.server_disconnected(&server);
        }
        Some("message") => {
            let from = message["from"].as_str().unwrap_or_default();
            let command = &message["payload"];
            if command["type"] == "discover_workers" {
                announce(shared, &lifecycle, from, discovery_grace);
                return;
            }
            // Another live server must not steer (or stop) a worker it does not own.
            if lifecycle.owner().as_deref() != Some(from) {
                return;
            }
            match command["type"].as_str() {
                // The server's heartbeat: answered by the reader, which never waits on the session.
                Some("ping") => {
                    let _ = shared.send(shared.status(command["nonce"].clone()), None);
                }
                Some("shutdown") => shared.close_and_exit(0),
                Some("release") => release(shared, &lifecycle, command),
                Some("session_demand") => demand(shared, &lifecycle, command),
                Some("operation") => operation(shared, &lifecycle, command.clone()),
                // Cancelling a call never aborts the run it started (AgentController): nothing to stop here.
                _ => {}
            }
        }
        _ => {}
    }
}

fn announce(shared: &Arc<Shared>, lifecycle: &Arc<Lifecycle>, from: &str, grace: Duration) {
    if !*shared.ready.lock().unwrap_or_else(|e| e.into_inner()) || lifecycle.retiring() {
        return;
    }
    if let Some(owner) = lifecycle.owner()
        && owner != from
    {
        let owned = json!({"type": "worker_owned", "sessionKey": shared.session_key, "sessionId": shared.session_id, "pid": std::process::id()});
        if shared.send(owned, Some(from)).is_err() {
            shared.close_and_exit(1);
        }
        return;
    }
    if lifecycle.owner().is_none() {
        lifecycle.server_connected(from);
    }
    // A detached worker a new server just discovered must not retire before that server attaches.
    let hold = lifecycle.hold_retirement();
    std::thread::spawn(move || {
        std::thread::sleep(grace);
        drop(hold);
    });
    if shared.send(shared.worker_ready(), None).is_err() {
        shared.close_and_exit(1);
    }
}

fn release(shared: &Arc<Shared>, lifecycle: &Arc<Lifecycle>, command: &Value) {
    let server = command["serverConnectionId"].as_str().unwrap_or_default().to_string();
    let retained = lifecycle.busy() && !lifecycle.retiring();
    if retained {
        shared.remove_subscriptions(|scope| scope["serverConnectionId"] == json!(server));
    }
    let event = json!({"type": "worker_released", "token": shared.token, "sessionKey": shared.session_key,
        "requestId": command["requestId"], "retained": retained});
    let _ = shared.send(event, None);
    if retained {
        lifecycle.release_server(&server);
    } else {
        shared.close_and_exit(0);
    }
}

fn demand(shared: &Arc<Shared>, lifecycle: &Arc<Lifecycle>, command: &Value) {
    let _hold: Hold = lifecycle.hold_retirement();
    let server = command["serverConnectionId"].as_str().unwrap_or_default();
    let attachment = command["attachmentId"].as_str().unwrap_or_default();
    let attached = command["attached"] == true;
    if !attached {
        let key = format!("{server}\0{attachment}");
        shared.remove_subscriptions(|scope| scope_key(scope) == key);
    }
    let event = match lifecycle.set_demand(server, attachment, attached) {
        Err(message) => json!({"type": "demand_rejected", "token": shared.token, "sessionKey": shared.session_key,
            "requestId": command["requestId"], "message": message}),
        Ok(()) => json!({"type": "demand_applied", "token": shared.token, "sessionKey": shared.session_key,
            "requestId": command["requestId"], "attachmentId": attachment, "attached": attached}),
    };
    let _ = shared.send(event, None);
}

fn operation(shared: &Arc<Shared>, lifecycle: &Arc<Lifecycle>, request: Value) {
    let scope = request["scope"].clone();
    let hold = lifecycle.begin_request(
        scope["serverConnectionId"].as_str().unwrap_or_default(),
        scope["attachmentId"].as_str().unwrap_or_default(),
    );
    let call = Call::parse(&request["call"]);
    let long = call
        .as_ref()
        .is_some_and(|c| LONG_OPERATIONS.contains(&(c.service_id.as_str(), c.member.as_str())));
    let jobs = shared.jobs.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let shared = shared.clone();
    let execute = move || {
        let result: ServiceResult = match (hold, call) {
            (Err(message), _) => Err(ServiceError::internal(message)),
            (Ok(_), None) => Err(ServiceError::internal("Invalid service call")),
            (Ok(hold), Some(call)) => {
                let endpoint = shared
                    .endpoints
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .entry(scope_key(&scope))
                    .or_insert_with(|| (scope.clone(), Arc::new(Endpoint::new(shared.provider.clone()))))
                    .1
                    .clone();
                let publisher = shared.clone();
                let publish_scope = scope.clone();
                let result = endpoint.invoke(
                    &call,
                    Arc::new(move |subscription: &str, update: Value| {
                        let event = json!({"type": "service_update", "token": publisher.token, "sessionKey": publisher.session_key,
                            "scope": publish_scope, "subscriptionId": subscription, "update": update});
                        let _ = publisher.send(event, None);
                    }),
                );
                drop(hold);
                result
            }
        };
        let mut response = match result {
            Ok(result) => {
                let mut response =
                    json!({"type": "operation_result", "requestId": request["requestId"], "scope": scope});
                if let Some(result) = result {
                    response["result"] = result;
                }
                response
            }
            Err(error) => {
                let mut response =
                    json!({"type": "operation_error", "requestId": request["requestId"], "scope": scope});
                if let Some(code) = error.code {
                    response["code"] = json!(code);
                }
                response["message"] = json!(error.message);
                response
            }
        };
        let event = json!({"type": "operation_response", "token": shared.token, "sessionKey": shared.session_key,
            "response": std::mem::take(&mut response)});
        if shared.send(event, None).is_err() {
            shared.close_and_exit(1);
        }
    };
    if long {
        std::thread::spawn(execute);
    } else {
        let _ = jobs.send(Box::new(execute));
    }
}

/// Own `path`'s storage, or name its live owner. An owner records its pid just after it takes the lock;
/// `OwnerLock::acquire` gives that record a short grace, which a loaded machine can outlast (the refusal then named
/// no pid). Until the owner's record appears, or the owner lets go (then this worker owns the file), ask again,
/// bounded by a hang guard.
fn acquire_owner(path: &std::path::Path) -> Result<durable::owner::OwnerLock, durable::owner::OwnerError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match durable::owner::OwnerLock::acquire(path) {
            Err(durable::owner::OwnerError::InUse { pid: None }) if std::time::Instant::now() < deadline => {
                if let Some(pid) = durable::owner::recorded_owner(path) {
                    return Err(durable::owner::OwnerError::InUse { pid: Some(pid) });
                }
            }
            other => return other,
        }
    }
}

/// `SessionInUseError`'s message.
pub fn session_in_use(session_id: &str, pid: Option<u32>) -> String {
    let pid = pid.map_or_else(|| "unknown".to_string(), |p| p.to_string());
    format!(
        "Session {session_id} is open in another Ultron process (worker pid {pid}); close it there, or wait for its work to finish, then resume it"
    )
}

/// Check the run's progress against the stuck threshold; a run that just became stuck is reported.
fn spawn_watchdog(shared: &Arc<Shared>) {
    let Some(after) = shared.stuck_after else { return };
    let period = (after / 4).clamp(Duration::from_millis(50), Duration::from_secs(1));
    let watched = Arc::downgrade(shared);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(period);
            let Some(shared) = watched.upgrade() else { return };
            if shared.progress.check(session_core::lane::now_ms(), shared.stuck_after) {
                shared.report();
            }
        }
    });
}
