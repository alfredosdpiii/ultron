//! The members of `ultron.session-control` that act on the worker's settings and session state
//! (experimental/services/session-control-provider.ts): `getSettings`, the queue modes, auto compaction and retry,
//! labels, `beforeFork` (the extensions' `session_before_fork`), `readSettings` / `setSetting` (crate::worker_settings), `/trust`'s `setProjectTrust`,
//! `reloadAuth` and `debugInfo`. They reach the session through [`CoreCall`] (session-core operations), so a test
//! drives them with a recording core.

use crate::chord::{Service, ServiceError, ServiceResult};
use crate::worker_settings::{self, Change, Live};
use serde_json::{Value, json};
use session_core::core::{Op, OpResult, QueueKind};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// One session operation, answered with the actor's result.
pub type CoreCall = Arc<dyn Fn(Op) -> OpResult + Send + Sync>;

/// What the worker knows about itself for these members.
pub struct ControlContext {
    pub cwd: PathBuf,
    /// The profile directory (`settings.json`, `auth.json`, `trust.json`).
    pub agent_dir: PathBuf,
    /// The session file.
    pub session_file: Option<String>,
    /// Whether this worker loaded the project's settings (`workerProjectTrusted` when it started).
    pub project_trusted: bool,
    /// The worker's environment.
    pub var: session_core::VarLookup,
    /// Every environment variable (for `/debug`'s `ULTRON_*`/`PI_*` listing).
    pub vars: Arc<dyn Fn() -> Vec<(String, String)> + Send + Sync>,
    /// The models the providers with credentials make available now (`auth.json` re-read).
    pub available_models: Arc<dyn Fn() -> usize + Send + Sync>,
    pub started: Instant,
    /// The session's auto-retry setting as last set through this worker (the session starts with retries on).
    pub auto_retry: AtomicBool,
}

impl ControlContext {
    fn settings(&self) -> config::settings::SettingsManager {
        config::settings::SettingsManager::create(&self.cwd, &self.agent_dir, self.project_trusted)
    }
}

fn internal(message: impl Into<String>) -> ServiceError {
    ServiceError::internal(message)
}

fn state(core: &CoreCall) -> Result<Value, ServiceError> {
    Ok(core(Op::State).map_err(internal)?.unwrap_or(Value::Null))
}

fn mode_of(value: &Value) -> String {
    value.as_str().unwrap_or("one-at-a-time").to_string()
}

/// `{name, steeringMode, followUpMode, autoCompaction, autoRetry}`.
fn get_settings(core: &CoreCall, context: &ControlContext) -> ServiceResult {
    let state = state(core)?;
    Ok(Some(json!({
        "name": state.get("sessionName").cloned().unwrap_or(Value::Null),
        "steeringMode": mode_of(&state["steeringMode"]),
        "followUpMode": mode_of(&state["followUpMode"]),
        "autoCompaction": state["autoCompactionEnabled"] != false,
        "autoRetry": context.auto_retry.load(Ordering::SeqCst),
    })))
}

fn live(core: &CoreCall, context: &ControlContext) -> Result<Live, ServiceError> {
    let state = state(core)?;
    Ok(Live {
        compaction: state["autoCompactionEnabled"] != false,
        retry: context.auto_retry.load(Ordering::SeqCst),
        steering_mode: mode_of(&state["steeringMode"]),
        follow_up_mode: mode_of(&state["followUpMode"]),
    })
}

fn set_retry(core: &CoreCall, context: &ControlContext, enabled: bool) -> Result<(), ServiceError> {
    core(Op::SetAutoRetry(enabled)).map_err(internal)?;
    context.auto_retry.store(enabled, Ordering::SeqCst);
    Ok(())
}

/// Apply what the running session takes now. Transport and the HTTP idle timeout are read when the session opens
/// in the Rust build, so `setSetting` reports them as applying on restart (see [`set_setting`]).
fn apply_change(core: &CoreCall, context: &ControlContext, change: Change) -> Result<(), ServiceError> {
    let op = match change {
        Change::Compaction(enabled) => Op::SetAutoCompaction(enabled),
        Change::Retry(enabled) => return set_retry(core, context, enabled),
        Change::SteeringMode(mode) => Op::SetQueueMode {
            kind: QueueKind::Steer,
            mode,
        },
        Change::FollowUpMode(mode) => Op::SetQueueMode {
            kind: QueueKind::FollowUp,
            mode,
        },
        Change::Thinking(level) => Op::SetThinking { level },
        Change::Transport(_) | Change::HttpIdleTimeout(_) | Change::StreamTimeout(_) => return Ok(()),
    };
    core(op).map_err(internal).map(drop)
}

/// `setSetting`: persist one setting and apply it where Pi applies it live; `{applied}`.
fn set_setting(core: &CoreCall, context: &ControlContext, key: &str, value: &Value) -> ServiceResult {
    let model = state(core)?["model"].clone();
    let current = model["provider"].as_str().zip(model["id"].as_str());
    let mut settings = context.settings();
    let (mut applied, changes) = worker_settings::apply(&mut settings, key, value, current).map_err(internal)?;
    if matches!(key, "transport" | "httpIdleTimeoutMs") {
        applied = worker_settings::Applied::Restart;
    }
    for change in changes {
        apply_change(core, context, change)?;
    }
    Ok(Some(json!({"applied": applied.name()})))
}

/// `setProjectTrust`: save `/trust` decisions to the profile's `trust.json` (`{path, decision | null}` each).
fn set_project_trust(context: &ControlContext, updates: &Value) -> ServiceResult {
    let updates: Vec<config::trust::TrustUpdate> = updates
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|u| config::trust::TrustUpdate {
            path: PathBuf::from(u["path"].as_str().unwrap_or_default()),
            decision: u["decision"].as_bool(),
        })
        .collect();
    config::trust::ProjectTrustStore::new(&context.agent_dir)
        .set_many(&updates)
        .map_err(internal)?;
    Ok(None)
}

/// `process.platform-process.arch` as Node names them.
pub fn platform() -> String {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    format!("{}-{arch}", std::env::consts::OS)
}

/// This process's resident set size (`process.memoryUsage().rss`), from `/proc/self/status`.
fn rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|l| l.strip_prefix("VmRSS:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        })
        .map_or(0, |kb| kb * 1024)
}

/// The product version (`VERSION`): the coding agent package's.
pub fn version() -> String {
    let package: Value = serde_json::from_str(include_str!("../../../packages/coding-agent/package.json"))
        .expect("packages/coding-agent/package.json parses");
    package["version"].as_str().unwrap_or_default().to_string()
}

/// `debugInfo`: non-secret facts about the worker for `/debug`. The Rust worker runs no Node: `nodeVersion` names
/// the build instead.
fn debug_info(core: &CoreCall, context: &ControlContext) -> ServiceResult {
    let state = state(core)?;
    let model = &state["model"];
    let model = match (model["provider"].as_str(), model["id"].as_str()) {
        (Some(provider), Some(id)) => json!(format!("{provider}/{id}")),
        _ => Value::Null,
    };
    let kernel_pool = match core(Op::Inspect {
        request: "rlm.pool".into(),
        payload: json!({}),
    }) {
        Ok(pool) => pool.unwrap_or(Value::Null),
        Err(error) => json!({"error": error}),
    };
    Ok(Some(json!({
        "pid": std::process::id(),
        "parentPid": std::os::unix::process::parent_id(),
        "nodeVersion": "none (Rust build)",
        "version": version(),
        "platform": platform(),
        "cwd": context.cwd.display().to_string(),
        "agentDir": context.agent_dir.display().to_string(),
        "sessionFile": context.session_file,
        "uptimeMs": context.started.elapsed().as_millis() as u64,
        "rssBytes": rss_bytes(),
        "model": model,
        "thinkingLevel": state["thinkingLevel"].as_str().unwrap_or("off"),
        "kernelPool": kernel_pool,
        "environment": worker_settings::debug_environment((context.vars)()),
    })))
}

fn text(args: &[Value], index: usize) -> String {
    args.get(index).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn flag(args: &[Value]) -> bool {
    args.first() == Some(&Value::Bool(true))
}

/// Add the settings members to the `ultron.session-control` service.
pub fn members(service: Service, core: CoreCall, context: Arc<ControlContext>) -> Service {
    let with = |f: fn(&CoreCall, &ControlContext, Vec<Value>) -> ServiceResult| {
        let (core, context) = (core.clone(), context.clone());
        move |args: Vec<Value>| f(&core, &context, args)
    };
    service
        .method("getSettings", with(|core, context, _| get_settings(core, context)))
        .method(
            "setSteeringMode",
            with(|core, _, args| {
                let mode = text(&args, 0);
                core(Op::SetQueueMode {
                    kind: QueueKind::Steer,
                    mode,
                })
                .map_err(internal)
                .map(|_| None)
            }),
        )
        .method(
            "setFollowUpMode",
            with(|core, _, args| {
                let mode = text(&args, 0);
                core(Op::SetQueueMode {
                    kind: QueueKind::FollowUp,
                    mode,
                })
                .map_err(internal)
                .map(|_| None)
            }),
        )
        .method(
            "setAutoCompaction",
            with(|core, _, args| core(Op::SetAutoCompaction(flag(&args))).map_err(internal).map(|_| None)),
        )
        .method(
            "setAutoRetry",
            with(|core, context, args| set_retry(core, context, flag(&args)).map(|()| None)),
        )
        .method(
            "setLabel",
            with(|core, _, args| {
                // `null` or an empty label clears it.
                let label = args
                    .get(1)
                    .and_then(Value::as_str)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string);
                core(Op::SetLabel {
                    entry_id: text(&args, 0),
                    label,
                })
                .map_err(internal)
                .map(|_| None)
            }),
        )
        // The session's extensions answer `session_before_fork`; one of them may cancel the fork.
        .method(
            "beforeFork",
            with(|core, _, args| {
                let position = args.get(1).and_then(Value::as_str).unwrap_or("before").to_string();
                // No extension answered: the fork goes ahead (`{cancelled: false}`).
                core(Op::BeforeFork {
                    entry_id: text(&args, 0),
                    position,
                })
                .map_err(internal)
                .map(|answer| answer.or_else(|| Some(json!({"cancelled": false}))))
            }),
        )
        .method(
            "readSettings",
            with(|core, context, _| {
                let live = live(core, context)?;
                let settings = context.settings();
                worker_settings::read(&settings, &live, &context.cwd, &context.agent_dir, &|k| {
                    (context.var)(k)
                })
                .map(Some)
                .map_err(internal)
            }),
        )
        .method(
            "setSetting",
            with(|core, context, args| {
                set_setting(core, context, &text(&args, 0), args.get(1).unwrap_or(&Value::Null))
            }),
        )
        .method(
            "setProjectTrust",
            with(|_, context, args| set_project_trust(context, args.first().unwrap_or(&Value::Null))),
        )
        .method(
            "reloadAuth",
            // Credentials are read from `auth.json` per request; availability is computed again now.
            with(|_, context, _| Ok(Some(json!({"availableModels": (context.available_models)()})))),
        )
        .method("debugInfo", with(|core, context, _| debug_info(core, context)))
}

/// `AgentController.resume` over the session's recovery (`Op::Recover`'s `{resumed, runId, reason}`) and the run in
/// progress: the resumed (or running) run's `{accepted, operationId, error}`, else `nothing_to_resume` with the
/// lane's message.
pub fn resume_response(recovered: &Value, running: Option<String>) -> Value {
    let run = recovered["runId"].as_str().map(str::to_string).or_else(|| {
        // A run already in progress is the active operation `resume` drives.
        (recovered["reason"] == "a run is in progress")
            .then_some(running)
            .flatten()
    });
    match run {
        Some(run) if recovered["resumed"] == true || recovered["reason"] == "a run is in progress" => {
            json!({"accepted": true, "operationId": run, "error": null})
        }
        _ => json!({"accepted": false, "operationId": null, "error": {
            "code": "nothing_to_resume",
            "message": format!("Lane {} has no active operation to resume", json!(session_core::core::LANE)),
        }}),
    }
}

/// The models service's catalogue entry for a model (`{provider, modelId, name, reasoning, model}`).
fn catalog_entry(model: &Value) -> Value {
    json!({"provider": model["provider"], "modelId": model["id"], "name": model["name"],
        "reasoning": model["reasoning"], "model": model})
}

/// `Models.refresh` (models-provider.ts) on the models state: `refresh.status` is `refreshing` while `refresh`
/// runs (the catalogues), then the catalogue (the available models, plus the selected one when it is not among
/// them) under the next revision, the configuration, and `done` or `warning` with each failed provider's error.
/// A refresh that fails leaves the status `refreshing` and fails the call, as the TypeScript `await` throws.
pub fn refresh_models(
    models: &crate::chord::State,
    refresh: impl FnOnce() -> Result<Vec<(String, String)>, String>,
    available: impl FnOnce() -> Vec<Value>,
    session: &Value,
) -> ServiceResult {
    models.change(|state| state["refresh"] = json!({"status": "refreshing"}));
    let errors = refresh().map_err(internal)?;
    let selected = &session["model"];
    let mut catalog = available();
    if selected.is_object()
        && !catalog
            .iter()
            .any(|m| m["provider"] == selected["provider"] && m["id"] == selected["id"])
    {
        catalog.push(selected.clone());
    }
    let configuration = json!({
        "model": if selected.is_object() { json!({"provider": selected["provider"], "modelId": selected["id"]}) } else { Value::Null },
        "thinkingLevel": session["thinkingLevel"].as_str().unwrap_or("off"),
    });
    models.change(|state| {
        let revision = state["catalog"]["revision"].as_u64().unwrap_or(0) + 1;
        state["catalog"] =
            json!({"revision": revision, "availableModels": catalog.iter().map(catalog_entry).collect::<Vec<_>>()});
        state["configuration"] = configuration;
        state["refresh"] = if errors.is_empty() {
            json!({"status": "done"})
        } else {
            let errors: serde_json::Map<String, Value> =
                errors.into_iter().map(|(p, e)| (p, Value::String(e))).collect();
            json!({"status": "warning", "errors": errors})
        };
    });
    Ok(None)
}

/// The worker's [`ControlContext`] for the session file `path` in `env`.
pub fn context(env: &session_core::RunEnv, path: &std::path::Path) -> Arc<ControlContext> {
    let agent_dir = env.agent_dir();
    let models_env = env.clone();
    Arc::new(ControlContext {
        project_trusted: session_core::extension_commands::worker_project_trusted(&env.cwd, &agent_dir, &env.home),
        cwd: env.cwd.clone(),
        agent_dir,
        session_file: Some(path.display().to_string()),
        var: env.var.clone(),
        vars: Arc::new(|| std::env::vars().collect()),
        available_models: Arc::new(move || session_core::lane::available_models(&models_env).len()),
        started: Instant::now(),
        auto_retry: AtomicBool::new(true),
    })
}
