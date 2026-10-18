//! `ultron mcp` (mcp-server.ts): Ultron's REPL as an MCP server for Claude Code. One server process is one Claude
//! Code session (or one Claude Code subagent, `--child`), and it runs Ultron's own session runtime on an Ultron
//! session of its own: the same host stack, kernel, file hooks (Loki), secret masking, subagents and frames as
//! Ultron's root lane. Claude Code is the root agent; the server exposes:
//!
//! - tool `rlm` (and nothing else): run a Python cell in the persistent kernel ([`ExternalRoot::run_cell`]);
//! - prompt `ultron-guide` and resource `ultron://guide`: the runtime guide (also the server instructions, unless
//!   `--no-instructions`, which `ultron claude` passes because the guide is its system prompt);
//! - a control socket for hooks (`ultron hook`), the viewer (`ultron watch`) and subagent servers.
//!
//! The Ultron session of a Claude Code session is remembered by Claude Code's session id, so `claude --resume`
//! reopens the same session.

use crate::claude_cli::{DEFAULT_CLAUDE_MODEL, resolve_claude_binary};
use crate::control::{ControlClient, ControlHandler, ServerRecord, control_dir, socket_path_for, start_control_server};
use crate::external_root::ExternalRoot;
use crate::guide::{Audience, GuideOptions, UNLIMITED, claude_runtime_guide, claude_system_prompt, hints_enabled};
use crate::mcp_protocol::{McpHandlers, McpPrompt, McpResource, serve};
use crate::{Ctx, UsageError};
use ai::AbortSignal;
use serde_json::{Map, Value, json};
use session_core::RunEnv;
use session_core::compose::{Runtime, Services, SubagentHost};
use session_core::lane::{Lane, MAIN};
use session_journal::SessionRepo;
use session_journal::repo::Metadata;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

pub fn default_frame_model() -> String {
    format!("claude-code/{DEFAULT_CLAUDE_MODEL}")
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerArgs {
    pub child: bool,
    pub parent_socket: Option<String>,
    pub parent_name: Option<String>,
    pub parent_lane: Option<String>,
    pub level: u64,
    /// [`UNLIMITED`] for a root server.
    pub allowance: u64,
    pub frame_model: Option<String>,
    pub children: Option<String>,
    pub child_model: Option<String>,
    pub ultron_child_model: Option<String>,
    pub socket: Option<String>,
    pub instructions: bool,
    pub no_context_files: bool,
    pub no_skills: bool,
}

/// Parse `ultron mcp` flags; unknown flags are an error.
pub fn parse_mcp_args(args: &[String]) -> Result<McpServerArgs, UsageError> {
    let mut parsed = McpServerArgs {
        child: false,
        parent_socket: None,
        parent_name: None,
        parent_lane: None,
        level: 0,
        allowance: UNLIMITED,
        frame_model: None,
        children: None,
        child_model: None,
        ultron_child_model: None,
        socket: None,
        instructions: true,
        no_context_files: false,
        no_skills: false,
    };
    let count = |text: &str, flag: &str| -> Result<u64, UsageError> {
        let number = session_file::js::to_number(&Value::String(text.to_string()));
        if number.fract() != 0.0 || !(0.0..=9_007_199_254_740_991.0).contains(&number) {
            return Err(UsageError(format!("{flag} must be a non-negative integer")));
        }
        Ok(number as u64)
    };
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        let mut value = || -> Result<String, UsageError> {
            index += 1;
            args.get(index)
                .cloned()
                .ok_or_else(|| UsageError(format!("{arg} needs a value")))
        };
        match arg {
            "--child" => parsed.child = true,
            "--no-instructions" => parsed.instructions = false,
            "--parent-socket" => parsed.parent_socket = Some(value()?),
            "--parent-name" => parsed.parent_name = Some(value()?),
            "--parent-lane" => parsed.parent_lane = Some(value()?),
            "--level" => parsed.level = count(&value()?, arg)?,
            "--allowance" => parsed.allowance = count(&value()?, arg)?,
            "--frame-model" => parsed.frame_model = Some(value()?),
            "--children" => {
                let mode = value()?;
                if mode != "claude" && mode != "ultron" {
                    return Err(UsageError("--children must be claude or ultron".into()));
                }
                parsed.children = Some(mode);
            }
            "--child-model" => parsed.child_model = Some(value()?),
            "--ultron-child-model" => parsed.ultron_child_model = Some(value()?),
            "--socket" => parsed.socket = Some(value()?),
            "--no-context-files" => parsed.no_context_files = true,
            "--no-skills" => parsed.no_skills = true,
            other => return Err(UsageError(format!("unknown option for ultron mcp: {other}"))),
        }
        index += 1;
    }
    if parsed.child && (parsed.parent_socket.is_none() || parsed.parent_lane.is_none()) {
        return Err(UsageError(
            "ultron mcp --child needs --parent-socket and --parent-lane".into(),
        ));
    }
    Ok(parsed)
}

/// `provider/model` split at the first slash.
pub fn split_model(model: &str) -> Option<(String, String)> {
    let slash = model.find('/')?;
    (slash > 0 && slash < model.len() - 1).then(|| (model[..slash].to_string(), model[slash + 1..].to_string()))
}

/// Inspections the viewer may make (none of them changes anything).
pub const READ_ONLY_INSPECTIONS: [&str; 8] = [
    "agents.status",
    "instances.list",
    "rlm.pool",
    "ctx.state",
    "rlm.frames",
    "progress.assess",
    "async.pending",
    "usage.report",
];

/// The models and modes a server runs with (env, settings and flags resolved).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub frame_model: String,
    pub children: String,
    pub child_model: String,
    pub ultron_child_model: Option<String>,
}

/// `runMcpServer`'s model and mode resolution. An environment variable that is set wins even when blank (JS `??`).
pub fn resolve(args: &McpServerArgs, var: &dyn Fn(&str) -> Option<String>, cwd: &Path, agent_dir: &Path) -> Resolved {
    let settings = crate::settings::ClaudeCodeSettings::load(cwd, agent_dir);
    let manager = config::settings::SettingsManager::create(cwd, agent_dir, false);
    let trimmed = |key: &str| var(key).map(|v| v.trim().to_string());
    let frame_model = args
        .frame_model
        .clone()
        .or_else(|| trimmed("ULTRON_CLAUDE_FRAME_MODEL"))
        .or(settings.frame_model.clone())
        .unwrap_or_else(default_frame_model);
    let children = args
        .children
        .clone()
        .or_else(|| var("ULTRON_CLAUDE_CHILDREN").filter(|v| v == "ultron" || v == "claude"))
        .or(settings.children.clone())
        .unwrap_or_else(|| {
            if resolve_claude_binary(var).is_none() {
                "ultron".into()
            } else {
                "claude".into()
            }
        });
    let child_model = args
        .child_model
        .clone()
        .or_else(|| trimmed("ULTRON_CLAUDE_CHILD_MODEL"))
        .or(settings.child_model.clone())
        .unwrap_or_else(|| DEFAULT_CLAUDE_MODEL.into());
    let default = match (manager.default_provider(), manager.default_model()) {
        (Some(Value::String(p)), Some(Value::String(m))) if !p.is_empty() && !m.is_empty() => Some(format!("{p}/{m}")),
        _ => None,
    };
    let ultron_child_model = args
        .ultron_child_model
        .clone()
        .or(settings.ultron_child_model.clone())
        .or(default);
    Resolved {
        frame_model,
        children,
        child_model,
        ultron_child_model,
    }
}

/// Where the Ultron session of each Claude Code session is remembered.
fn session_map_path(agent_dir: &Path, claude_session_id: &str) -> PathBuf {
    let dir = agent_dir.join("claude-code").join("sessions");
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir);
    }
    let safe: String = claude_session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    dir.join(format!("{safe}.json"))
}

fn remembered_session(agent_dir: &Path, claude_session_id: &str) -> Option<Value> {
    let text = std::fs::read_to_string(session_map_path(agent_dir, claude_session_id)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The metadata JSON `JsonlSessionMetadata` remembers (TypeScript's field order).
fn metadata_json(metadata: &Metadata) -> Value {
    let mut value = Map::new();
    value.insert("id".into(), metadata.id.clone().into());
    value.insert("createdAt".into(), metadata.created_at.into());
    value.insert("storageVersion".into(), metadata.storage_version.into());
    value.insert("cwd".into(), metadata.cwd.clone().into());
    value.insert("path".into(), metadata.path.display().to_string().into());
    value.insert("modifiedAt".into(), json!(metadata.modified_at));
    if let Some(parent) = &metadata.parent_session_id {
        value.insert("parentSessionId".into(), parent.clone().into());
    }
    Value::Object(value)
}

fn metadata_from_json(value: &Value) -> Option<Metadata> {
    Some(Metadata {
        id: value["id"].as_str()?.to_string(),
        created_at: value["createdAt"].as_i64()?,
        storage_version: value["storageVersion"].as_i64().unwrap_or(1),
        cwd: value["cwd"].as_str()?.to_string(),
        path: PathBuf::from(value["path"].as_str()?),
        modified_at: value["modifiedAt"].as_f64().unwrap_or(0.0),
        parent_session_id: value["parentSessionId"].as_str().map(str::to_string),
        legacy_parent_session_path: None,
    })
}

/// A held session lock: `proper-lockfile`'s `<file>.lock` directory, its mtime refreshed every second.
struct SessionLock {
    _guard: config::lock::LockGuard,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

fn lock_session(path: &Path) -> Option<SessionLock> {
    let lock_path = config::lock::lock_path_for(&std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    let guard = config::lock::acquire(&lock_path, Duration::from_secs(2), 41).ok()?;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::spawn(move || {
        while !flag.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(1));
            if let Ok(dir) = std::fs::File::open(&lock_path) {
                let _ = dir.set_modified(std::time::SystemTime::now());
            }
        }
    });
    Some(SessionLock { _guard: guard, stop })
}

/// The runtime a server's cells run on, once opened.
pub struct Opened {
    pub lane: Arc<Lane>,
    pub root: ExternalRoot,
    /// The root lane's events on their way to `root` (the dispatcher's coalescing window).
    pub sink: Arc<crate::root_sink::RootSink>,
    pub session_id: String,
    /// The session file (`usage.report` reads it).
    pub session_path: PathBuf,
    pub model: String,
    _lock: Option<SessionLock>,
}

impl Opened {
    pub fn close(&self) {
        self.lane.shutdown();
    }
}

/// Open (or reopen, for a remembered Claude Code session) the Ultron session and its root lane.
fn open_runtime(
    args: &McpServerArgs,
    env: &RunEnv,
    claude_session_id: Option<&str>,
    resolved: &Resolved,
    subagents: Option<SubagentHost>,
    root_finish: Option<delegation::host::RootFinish>,
) -> Result<Opened, String> {
    let agent_dir = env.agent_dir();
    let cwd = env.cwd.display().to_string();
    let session_root = (env.var)(config::paths::ENV_SESSION_DIR)
        .map(PathBuf::from)
        .unwrap_or_else(|| agent_dir.join("experimental").join("sessions"));
    let repo = SessionRepo::new(&session_root, session_journal::SystemClock::shared());
    let mut opened: Option<(Metadata, Option<SessionLock>, session_journal::Session)> = None;
    if !args.child
        && let Some(id) = claude_session_id
        && let Some(metadata) = remembered_session(&agent_dir, id).as_ref().and_then(metadata_from_json)
        && metadata.cwd == cwd
        && let Ok(session) = repo.open(&metadata)
    {
        // Another server (a second client resuming the same Claude Code session) holds it: start a fresh one.
        if let Some(lock) = lock_session(&metadata.path) {
            opened = Some((metadata, Some(lock), session));
        }
    }
    let (metadata, lock, session) = match opened {
        Some(found) => found,
        None => {
            let mut session = repo.create(&cwd, None, None)?;
            let name = if args.child {
                format!("claude subagent ({})", args.parent_lane.as_deref().unwrap_or(""))
            } else {
                "claude code".to_string()
            };
            let _ = session.set_name(Some(&name));
            let metadata = session.metadata().clone();
            if let Some(id) = claude_session_id.filter(|_| !args.child) {
                crate::write_private(
                    &session_map_path(&agent_dir, id),
                    &format!("{}\n", session_file::js::stringify(&metadata_json(&metadata))),
                )
                .map_err(|e| e.to_string())?;
            }
            let lock = lock_session(&metadata.path);
            (metadata, lock, session)
        }
    };
    let model = frame_model(&resolved.frame_model, env)?;
    let api_key = session_core::lane::api_key(&session_core::ModelFlags::default(), env, &model.provider);
    let sessions_dir = session_root.join(session_core::lane::session_directory_name(&metadata.cwd));
    let runtime = Runtime::new(env, "print", sessions_dir, Services::default())?;
    for diagnostic in &runtime.diagnostics {
        eprintln!("Warning: {diagnostic}");
    }
    // Loki's setup (the minimal install where allowed, its commit in the background) before any cell runs.
    let loki_notice = session_core::loki_notice::LokiNotice::start(&runtime.config);
    let lane = Arc::new(Lane::open_with(&runtime, &model, api_key, &metadata.id, subagents)?);
    let delegation = &lane.stack.delegation;
    // The root is itself a subagent (`rootSpawn`, `rootFinish`): nesting counts from its level and allowance, and its
    // `rlm.finish` verdict goes to its parent's server.
    if args.child {
        let allowance =
            (args.allowance != crate::guide::UNLIMITED).then(|| i64::try_from(args.allowance).unwrap_or(i64::MAX));
        delegation.set_root_spawn(u32::try_from(args.level).unwrap_or(u32::MAX), allowance);
    }
    if let Some(finish) = root_finish {
        delegation.set_root_finish(finish);
    }
    // `childModel`: `--children ultron`'s child model, else `rlm.childModel` (/settings -> Models), for spawns that
    // name none.
    let settings_child = runtime.config.settings["rlm"]["childModel"]
        .as_str()
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    let child_model = resolved
        .ultron_child_model
        .clone()
        .filter(|_| resolved.children == "ultron")
        .or(settings_child);
    if child_model.is_some() {
        delegation.set_child_model(child_model);
    }
    let loki_context = runtime
        .config
        .loki_root
        .as_deref()
        .and_then(|root| loki::config::load_config(root).ok())
        .and_then(|config| loki::injected_context(&config, Some("ultron")).ok());
    // The runtime counters (`/usage`, `ultron usage`) are the session value `ultron.module/stats` of a session whose
    // root is Claude Code (counting continues from a resumed one).
    lane.stack.stats.set_root("external");
    let session: session_core::durable_store::SharedSession = Arc::new(Mutex::new(session));
    lane.stack
        .stats
        .attach(Arc::new(session_core::session_stats::SessionStore(session.clone())));
    // What the session actor wires for its own root lane: `ctx.*`, the budget line, shell jobs' roots and the
    // completion events of root work (into the root's inbox).
    let host = Arc::new(crate::external_lane::LaneRootHost::new(lane.stack.clone()));
    let sink = Arc::new(crate::root_sink::RootSink::system());
    crate::external_lane::wire(&lane, session, &host, sink.clone());
    let root =
        ExternalRoot::over_lane(lane.clone(), loki_context, sink.clone(), host, &*env.var).with_loki_setup(loki_notice);
    Ok(Opened {
        lane,
        root,
        sink,
        session_path: metadata.path.clone(),
        session_id: metadata.id,
        model: format!("{}/{}", model.provider, model.id),
        _lock: lock,
    })
}

/// The frames' model: `provider/id` from the catalog (the Claude Code CLI's models included), else the configured
/// default model.
fn frame_model(frame: &str, env: &RunEnv) -> Result<ai::Model, String> {
    if let Some((provider, id)) = split_model(frame) {
        // The catalog includes the Claude Code CLI's models (`ai::claude_code::MODELS`).
        if let Ok(model) = session_core::lane::catalog_model(&provider, &id, env) {
            return Ok(model);
        }
    }
    let manager = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    match (manager.default_provider(), manager.default_model()) {
        (Some(Value::String(provider)), Some(Value::String(id))) => {
            session_core::lane::catalog_model(&provider, &id, env)
        }
        _ => Err(format!("Model \"{frame}\" not found")),
    }
}

/// A root cell as the viewer shows it.
#[derive(Clone, Debug)]
struct CellRecord {
    n: u64,
    started_at: i64,
    ended_at: Option<i64>,
    code: String,
    status: &'static str,
    output: Option<String>,
}

impl CellRecord {
    fn to_json(&self) -> Value {
        let mut cell = Map::new();
        cell.insert("n".into(), self.n.into());
        cell.insert("startedAt".into(), self.started_at.into());
        cell.insert("code".into(), self.code.clone().into());
        cell.insert("status".into(), self.status.into());
        if let Some(ended) = self.ended_at {
            cell.insert("endedAt".into(), ended.into());
        }
        if let Some(output) = &self.output {
            cell.insert("output".into(), output.clone().into());
        }
        Value::Object(cell)
    }
}

/// The first `n` UTF-16 units of `text` (JS `slice(0, n)`), never splitting a character.
fn js_head(text: &str, n: usize) -> String {
    let mut units = 0;
    text.chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= n
        })
        .collect()
}

/// The last `n` UTF-16 units of `text` (JS `slice(-n)`).
fn js_tail(text: &str, n: usize) -> String {
    let mut units = 0;
    let mut tail: Vec<char> = text
        .chars()
        .rev()
        .take_while(|c| {
            units += c.len_utf16();
            units <= n
        })
        .collect();
    tail.reverse();
    tail.into_iter().collect()
}

/// The runtime once it opened, or why it failed.
#[derive(Default)]
struct Startup {
    done: bool,
    opened: Option<Arc<Opened>>,
    error: Option<String>,
}

pub struct Server {
    args: McpServerArgs,
    env: RunEnv,
    name: String,
    record: ServerRecord,
    resolved: Resolved,
    claude_session_id: Option<String>,
    startup: Mutex<Startup>,
    ready: Condvar,
    cells: Mutex<(u64, Vec<CellRecord>)>,
    hints: bool,
    /// Tokens of running Claude Code subagents: a verdict is accepted only from the child holding its token.
    child_tokens: ChildTokens,
}

impl Server {
    /// Wait for the runtime; the startup error when it failed.
    fn runtime(&self) -> Result<Arc<Opened>, String> {
        let mut startup = self.startup.lock().unwrap_or_else(PoisonError::into_inner);
        while !startup.done {
            startup = self.ready.wait(startup).unwrap_or_else(PoisonError::into_inner);
        }
        match (&startup.opened, &startup.error) {
            (Some(opened), _) => Ok(opened.clone()),
            (None, error) => Err(error.clone().unwrap_or_else(|| "the runtime did not start".into())),
        }
    }

    fn opened_now(&self) -> (Option<Arc<Opened>>, Option<String>) {
        let startup = self.startup.lock().unwrap_or_else(PoisonError::into_inner);
        (startup.opened.clone(), startup.error.clone())
    }

    fn status(&self) -> Value {
        let (opened, error) = self.opened_now();
        let mut status = Map::new();
        status.insert("name".into(), self.name.clone().into());
        status.insert("pid".into(), std::process::id().into());
        status.insert("cwd".into(), self.record.cwd.clone().into());
        status.insert("child".into(), self.args.child.into());
        status.insert("startedAt".into(), self.record.started_at.into());
        status.insert("ready".into(), opened.is_some().into());
        if let Some(error) = error {
            status.insert("error".into(), error.into());
        }
        if let Some(id) = &self.claude_session_id {
            status.insert("claudeSessionId".into(), id.clone().into());
        }
        if let Some(opened) = opened {
            status.insert("sessionId".into(), opened.session_id.clone().into());
            status.insert("frameModel".into(), opened.model.clone().into());
            status.insert("children".into(), self.resolved.children.clone().into());
            status.insert("turn".into(), opened.root.turn().map_or(Value::Null, Value::from));
            status.insert("cells".into(), opened.root.cells().into());
            status.insert("pendingEvents".into(), opened.root.pending_events().into());
        }
        Value::Object(status)
    }

    /// One control request.
    pub fn control(&self, request: Map<String, Value>) -> Result<Value, String> {
        let op = request.get("op").and_then(Value::as_str).unwrap_or("");
        match op {
            "status" => Ok(self.status()),
            "cells" => {
                let cells = self.cells.lock().unwrap_or_else(PoisonError::into_inner);
                let start = cells.1.len().saturating_sub(50);
                Ok(json!({"cells": cells.1[start..].iter().map(CellRecord::to_json).collect::<Vec<_>>()}))
            }
            "inspect" => {
                // `String(request.request ?? "")`.
                let inspect = match request.get("request") {
                    None | Some(Value::Null) => String::new(),
                    other => crate::mcp_protocol::js_string(other),
                };
                if !READ_ONLY_INSPECTIONS.contains(&inspect.as_str()) {
                    return Err(format!("not a read-only inspection: {inspect}"));
                }
                let payload = match request.get("payload") {
                    Some(Value::Object(payload)) => payload.clone(),
                    _ => Map::new(),
                };
                let opened = self.runtime()?;
                let stack = &opened.lane.stack;
                match inspect.as_str() {
                    // The worker answers these itself (session-worker.ts `inspect`).
                    "rlm.pool" => Ok(stack.pool_stats()),
                    "async.pending" => Ok(crate::external_lane::async_pending(stack, &opened.sink)),
                    "usage.report" => {
                        let report = SESSION_REPORT.get().ok_or("the session report is not available")?;
                        // The counters still in memory go to the session file first.
                        stack.stats.flush();
                        report(&opened.session_path)
                    }
                    _ => stack
                        .cell_handler(MAIN, None)
                        .handle(&inspect, payload, &kernel::Signal::new()),
                }
            }
            "hook" => {
                let event = match request.get("event") {
                    None | Some(Value::Null) => String::new(),
                    other => crate::mcp_protocol::js_string(other),
                };
                self.hook(&event)
            }
            "child.finish" => {
                let lane = request
                    .get("token")
                    .and_then(Value::as_str)
                    .and_then(|token| {
                        self.child_tokens
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .get(token)
                            .cloned()
                    })
                    .ok_or("rlm.finish: unknown subagent")?;
                let payload = match request.get("payload") {
                    Some(Value::Object(payload)) => payload.clone(),
                    _ => Map::new(),
                };
                let opened = self.runtime()?;
                opened
                    .lane
                    .stack
                    .cell_handler(&lane, None)
                    .handle("rlm.finish", payload, &kernel::Signal::new())
            }
            other => Err(format!("unknown control op: {other}")),
        }
    }

    /// Claude Code hook events: the hook command forwards its input here and prints what this returns.
    fn hook(&self, event: &str) -> Result<Value, String> {
        let opened = self.runtime()?;
        let root = &opened.root;
        let context = |name: &str, context: Option<String>| match context {
            Some(text) => json!({"hookSpecificOutput": {"hookEventName": name, "additionalContext": text}}),
            None => json!({}),
        };
        match event {
            "session-start" => Ok(context("SessionStart", root.session_context())),
            "user-prompt" => Ok(context("UserPromptSubmit", root.begin_turn())),
            "stop" => {
                root.end_turn();
                Ok(json!({}))
            }
            other => Err(format!("unknown hook event: {other}")),
        }
    }

    fn guide_text(&self) -> String {
        if self.args.child {
            claude_system_prompt(
                Audience::ClaudeChild,
                &GuideOptions {
                    allowance: Some(self.args.allowance),
                    ..GuideOptions::default()
                },
                self.hints,
            )
        } else {
            claude_runtime_guide(self.hints)
        }
    }

    fn tool_description(&self) -> String {
        let config = session_core::compose::Config::from_env(&self.env);
        let listing = kernel_apis::CodeSkills::new(kernel_apis::CodeSkillsOptions::new(
            &config.skills_dir,
            &config.runtime_path,
        ))
        .describe();
        let section = if listing.is_empty() {
            String::new()
        } else {
            format!("\n{listing}")
        };
        format!("{}{section}", crate::guide::rlm_tool_description())
    }

    fn run_cell(&self, code: &str, signal: &AbortSignal) -> Value {
        let opened = match self.runtime() {
            Ok(opened) => opened,
            Err(error) => {
                return json!({"content": [{"type": "text", "text": format!("Ultron's runtime failed to start: {error}")}], "isError": true});
            }
        };
        let n = {
            let mut cells = self.cells.lock().unwrap_or_else(PoisonError::into_inner);
            cells.0 += 1;
            let n = cells.0;
            cells.1.push(CellRecord {
                n,
                started_at: session_core::lane::now_ms(),
                ended_at: None,
                code: js_head(code, 400),
                status: "running",
                output: None,
            });
            let excess = cells.1.len().saturating_sub(200);
            cells.1.drain(..excess);
            n
        };
        let result = opened.root.run_cell(code, signal);
        if let Some(cell) = self
            .cells
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .1
            .iter_mut()
            .find(|c| c.n == n)
        {
            cell.ended_at = Some(session_core::lane::now_ms());
            cell.status = if result.is_error { "error" } else { "ok" };
            cell.output = result
                .content
                .first()
                .and_then(|p| p["text"].as_str())
                .map(|t| js_tail(t, 400));
        }
        let mut reply = Map::new();
        reply.insert("content".into(), Value::Array(result.content));
        if result.is_error {
            reply.insert("isError".into(), true.into());
        }
        Value::Object(reply)
    }
}

/// The MCP face of a [`Server`].
pub struct Handlers(Arc<Server>);

impl McpHandlers for Handlers {
    fn name(&self) -> String {
        "ultron".into()
    }
    fn version(&self) -> String {
        crate::cli::version()
    }
    fn instructions(&self) -> Option<String> {
        self.0.args.instructions.then(|| claude_runtime_guide(self.0.hints))
    }
    fn tools(&self) -> Vec<Value> {
        vec![json!({
            "name": "rlm",
            "title": "Ultron REPL",
            "description": self.0.tool_description(),
            "inputSchema": {
                "type": "object",
                "properties": {"code": {"type": "string", "description": "Python code for persistent RLM computation, typed agents, and data processing"}},
                "required": ["code"],
                "additionalProperties": false,
            },
        })]
    }
    fn call_tool(
        &self,
        _name: &str,
        args: Map<String, Value>,
        signal: &AbortSignal,
        _meta: Map<String, Value>,
    ) -> Result<Value, String> {
        Ok(match args.get("code") {
            Some(Value::String(code)) => self.0.run_cell(code, signal),
            _ => json!({"content": [{"type": "text", "text": "rlm needs `code` (a string)"}], "isError": true}),
        })
    }
    fn prompts(&self) -> Option<Vec<McpPrompt>> {
        Some(vec![McpPrompt {
            name: "ultron-guide".into(),
            title: Some("Ultron runtime guide".into()),
            description: "How to work in Ultron's REPL".into(),
            text: self.0.guide_text(),
        }])
    }
    fn resources(&self) -> Option<Vec<McpResource>> {
        Some(vec![McpResource {
            uri: "ultron://guide".into(),
            name: "guide".into(),
            title: Some("Ultron runtime guide".into()),
            description: "How to work in Ultron's REPL (the rlm tool)".into(),
            mime_type: "text/markdown".into(),
            text: self.0.guide_text(),
        }])
    }
}

/// Builds the session report (`usage.report`) from a session file; the CLI, which owns the report, sets it.
pub type SessionReport = fn(&Path) -> Result<Value, String>;

static SESSION_REPORT: std::sync::OnceLock<SessionReport> = std::sync::OnceLock::new();

/// Answer `usage.report` inspections with `report`.
pub fn set_session_report(report: SessionReport) {
    let _ = SESSION_REPORT.set(report);
}

/// What a running server holds: shut down once, by Claude Code disconnecting or by a signal.
pub struct Running {
    pub server: Arc<Server>,
    control: Mutex<Option<crate::control::ControlServer>>,
    parent: Arc<Mutex<Option<ControlClient>>>,
    done: std::sync::atomic::AtomicBool,
}

impl Running {
    /// Shut down like a disconnect (TypeScript's end of `runMcpServer`): stop the control server and unregister,
    /// drop the parent connection, and close the runtime once it opened (kernels are snapshotted, running subagents
    /// and jobs stop). A second call does nothing.
    pub fn shutdown(&self) {
        if self.done.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        if let Some(control) = self.control.lock().unwrap_or_else(PoisonError::into_inner).take() {
            control.close();
        }
        self.parent.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Ok(opened) = self.server.runtime() {
            opened.close();
        }
    }

    /// The control socket's path.
    pub fn socket(&self) -> String {
        self.server.record.socket.clone()
    }
}

/// Run `ultron mcp` on stdio until Claude Code disconnects; returns the exit code. SIGTERM, SIGINT and SIGHUP of
/// this process shut it down the same way and exit 0.
pub fn run(
    argv: &[String],
    ctx: &Ctx,
    stdin: &mut dyn std::io::BufRead,
    stdout: Box<dyn std::io::Write + Send>,
) -> Result<i32, String> {
    let (running, handlers) = start(argv, ctx)?;
    let signalled = running.clone();
    daemon::signals::set_handler(Arc::new(move |_| {
        signalled.shutdown();
        std::process::exit(0);
    }));
    serve(handlers, stdin, stdout);
    // Claude Code closed the connection: shut down (running subagents and jobs stop with the host).
    running.shutdown();
    daemon::signals::clear_handler();
    Ok(0)
}

/// Start a server: its runtime opening in the background, its control socket listening. [`serve`] the returned
/// handlers on the MCP connection, then [`Running::shutdown`].
pub fn start(argv: &[String], ctx: &Ctx) -> Result<(Arc<Running>, Arc<Handlers>), String> {
    let args = parse_mcp_args(argv).map_err(|e| e.0)?;
    let mut env = ctx.env.clone();
    // A subagent server's verdict token (TypeScript then deletes it from the process env, which Rust cannot do
    // without `unsafe`: the kernel's environment drops it as a credential and Claude Code children are started
    // without it).
    let parent_token = (ctx.env.var)("ULTRON_PARENT_TOKEN");
    let var = env.var.clone();
    // A worktree subagent's server works in the worktree its parent gave it.
    if args.child
        && let Some(dir) = var("ULTRON_CHILD_CWD")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    {
        std::env::set_current_dir(&dir).map_err(|e| format!("{dir}: {e}"))?;
        env.cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    }
    let cwd = env.cwd.display().to_string();
    let agent_dir = env.agent_dir();
    let claude_session_id = var("CLAUDE_CODE_SESSION_ID")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let claude_pid = var("CLAUDE_PID")
        .map(|v| session_file::js::to_number(&Value::String(v)))
        .filter(|n| *n != 0.0 && n.is_finite() && n.fract() == 0.0 && *n > 0.0)
        .map(|n| n as u64);
    let resolved = resolve(&args, &*var, &env.cwd, &agent_dir);
    let name = if args.child {
        format!("child-{}-{}", std::process::id(), crate::random_hex(3))
    } else {
        claude_session_id.clone().unwrap_or_else(|| {
            format!(
                "mcp-{}-{}",
                std::process::id(),
                crate::launcher::base36(session_core::lane::now_ms() as u64)
            )
        })
    };
    let dir = control_dir(&*var)?;
    let socket = args
        .socket
        .clone()
        .unwrap_or_else(|| socket_path_for(&name, &dir).display().to_string());
    let record = ServerRecord {
        name: name.clone(),
        pid: std::process::id(),
        socket,
        cwd,
        started_at: session_core::lane::now_ms(),
        claude_session_id: claude_session_id.clone(),
        claude_pid,
        session_id: None,
        parent: args.parent_name.clone().filter(|_| args.child),
    };
    // A subagent server talks to its parent: its `rlm.finish` is relayed as `child.finish` with its token.
    let parent = Arc::new(Mutex::new(if args.child {
        Some(ControlClient::connect(
            Path::new(args.parent_socket.as_deref().unwrap_or_default()),
            Duration::from_secs(10),
        )?)
    } else {
        None
    }));
    let root_finish = args.child.then(|| root_finish(parent.clone(), parent_token));
    let server = Arc::new(Server {
        hints: hints_enabled(var("ULTRON_HINTS").as_deref()),
        args: args.clone(),
        env: env.clone(),
        name,
        record: record.clone(),
        resolved: resolved.clone(),
        claude_session_id: claude_session_id.clone(),
        startup: Mutex::default(),
        ready: Condvar::new(),
        cells: Mutex::default(),
        child_tokens: ChildTokens::default(),
    });
    // `--children claude` (the default when `claude` is installed): each `rlm.spawn` child is a Claude Code process
    // attached to its own `ultron mcp --child` server.
    let claude = resolve_claude_binary(&*var).filter(|_| resolved.children == "claude");
    let starter = server.clone();
    std::thread::spawn(move || {
        let subagents = claude.map(|claude| claude_subagents(&starter, claude));
        let outcome = subagents.transpose().and_then(|subagents| {
            open_runtime(
                &starter.args,
                &starter.env,
                starter.claude_session_id.as_deref(),
                &starter.resolved,
                subagents,
                root_finish,
            )
        });
        let mut startup = starter.startup.lock().unwrap_or_else(PoisonError::into_inner);
        match outcome {
            Ok(opened) => startup.opened = Some(Arc::new(opened)),
            Err(error) => {
                eprintln!("ultron mcp: runtime failed to start: {error}");
                startup.error = Some(error);
            }
        }
        startup.done = true;
        starter.ready.notify_all();
    });
    let handler_server = server.clone();
    let handler: ControlHandler = Arc::new(move |request| handler_server.control(request));
    let control = start_control_server(&record, handler, &dir)?;
    let handlers = Arc::new(Handlers(server.clone()));
    let running = Arc::new(Running {
        server,
        control: Mutex::new(Some(control)),
        parent,
        done: std::sync::atomic::AtomicBool::new(false),
    });
    Ok((running, handlers))
}

/// `rootFinish`: the verdict goes to the parent server as `child.finish` with this server's token.
fn root_finish(parent: Arc<Mutex<Option<ControlClient>>>, token: Option<String>) -> delegation::host::RootFinish {
    Arc::new(move |payload: Map<String, Value>| {
        let mut parent = parent.lock().unwrap_or_else(PoisonError::into_inner);
        let client = parent
            .as_mut()
            .ok_or("rlm.finish: the parent server is not reachable")?;
        let mut fields = Map::new();
        if let Some(token) = &token {
            fields.insert("token".into(), token.clone().into());
        }
        fields.insert("payload".into(), Value::Object(payload));
        client.request("child.finish", fields, Duration::from_secs(30))
    })
}

/// Child tokens of running Claude Code subagents, by token: their lane.
type ChildTokens = Arc<Mutex<std::collections::HashMap<String, String>>>;

/// The subagent host of a `--children claude` server: each child runs as `claude -p` on its own `ultron mcp
/// --child` server (child.ts `runClaudeChild`), its verdict relayed back over this server's control socket
/// (`child.finish`, checked against its one-time token).
fn claude_subagents(server: &Arc<Server>, claude: String) -> Result<SubagentHost, String> {
    let var = server.env.var.clone();
    let self_cmd = crate::self_cmd::self_command(&*var)?;
    let settings = crate::settings::ClaudeCodeSettings::load(&server.env.cwd, &server.env.agent_dir());
    let skip = crate::child::child_skip_permissions(&*var, settings.skip_permissions);
    let flags = crate::guide::ResourceFlags {
        no_context_files: server.args.no_context_files,
        no_skills: server.args.no_skills,
    };
    let ctx = crate::Ctx {
        env: server.env.clone(),
    };
    let bundled = ctx.bundled_skills();
    // Subagents get the context files and skills native Ultron loads here, read once.
    let resources = crate::guide::load_prompt_resources(&ctx.env, bundled.as_deref(), flags);
    let server = server.clone();
    Ok(SubagentHost {
        store: Box::new(delegation::task_store::MemoryStore::default()),
        runner: Box::new(move |_stack| {
            Arc::new(move |run: delegation::host::ChildRun| {
                let cwd = run.cwd.clone().unwrap_or_else(|| server.env.cwd.display().to_string());
                let system_prompt = claude_system_prompt(
                    Audience::ClaudeChild,
                    &GuideOptions {
                        cwd: Some(cwd.clone()),
                        platform: Some(crate::guide::platform().into()),
                        date: Some(crate::guide::utc_date()),
                        allowance: Some(u64::from(run.allowance)),
                        resources: Some(&resources),
                        bundled_skills: bundled.as_deref(),
                    },
                    server.hints,
                );
                let token = crate::random_hex(24);
                server
                    .child_tokens
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(token.clone(), run.lane_name.clone());
                let options = crate::child::ChildOptions {
                    claude: &claude,
                    server: crate::child::ChildServer {
                        self_cmd: &self_cmd,
                        parent_socket: &server.record.socket,
                        parent_name: &server.name,
                        frame_model: Some(&server.resolved.frame_model),
                        no_context_files: flags.no_context_files,
                        no_skills: flags.no_skills,
                    },
                    cwd: &cwd,
                    model: &server.resolved.child_model,
                    system_prompt,
                    skip_permissions: skip.clone(),
                    var: &*var,
                };
                let child = crate::child::ChildRun {
                    lane_name: run.lane_name.clone(),
                    prompt: run.prompt.clone(),
                    level: u64::from(run.level),
                    allowance: u64::from(run.allowance),
                    model: run.model.clone(),
                    cwd: run.cwd.clone(),
                    env: run.env.clone(),
                };
                let result = crate::child::run_claude_child(&options, &child, &token, &run.signal, &|_| {});
                server
                    .child_tokens
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&token);
                result.map(|done| done.text)
            }) as delegation::host::ChildRunner
        }),
        agents: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn env(root: &std::path::Path) -> RunEnv {
        let agent = root.join("agent");
        let vars = [
            ("ULTRON_CODING_AGENT_DIR".to_string(), agent.display().to_string()),
            ("PI_OFFLINE".to_string(), "1".to_string()),
        ];
        RunEnv {
            cwd: root.to_path_buf(),
            home: root.to_path_buf(),
            var: Arc::new(move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())),
            docs: root.join("docs"),
        }
    }

    #[test]
    fn claude_root_frame_model_names_the_claude_code_providers_own_models() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env(tmp.path());
        for id in ["claude-opus-5-5", "claude-haiku-5-5", "opus", "sonnet", "haiku"] {
            let model = frame_model(&format!("claude-code/{id}"), &env).unwrap();
            assert_eq!(model, ai::claude_code::MODELS[id], "{id}");
        }
        let sonnet = frame_model("claude-code/sonnet", &env).unwrap();
        assert_eq!(
            (
                sonnet.name.as_str(),
                sonnet.api.as_str(),
                sonnet.context_window,
                sonnet.max_tokens
            ),
            ("Claude Sonnet (Claude Code CLI)", "claude-code-cli", 200_000, 32_000)
        );
        // An id the provider does not list is no Claude Code model; without a default model nothing resolves.
        assert_eq!(
            frame_model("claude-code/claude-sonnet-9", &env).unwrap_err(),
            "Model \"claude-code/claude-sonnet-9\" not found"
        );
    }
}
