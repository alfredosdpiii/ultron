//! One `claude -p` process per model call (`directsdk.py` `Client._run`, `directsdk_setup.py`): locating the CLI,
//! the child environment (auth overrides refused, isolation and quiet flags set, credentials never touched), the
//! stable working directory, the private per-request files, the stream-json exchange, process-group cancellation
//! and the reconciliation of native's output with what the admission relay captured.

use super::admission::{AbortHandle, Admission};
use super::catalog::native_model;
use super::history::{Body, PREFIX, TOOL};
use super::{ClaudeError, Config, Env, INSTALL_HINT, LOGGED_OUT_HINT};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// Settings variable naming the `claude` executable (an absolute path, or a name looked up on PATH).
pub const COMMAND_ENV: &str = "ULTRON_CLAUDE_COMMAND";
/// `0`/`false`/`no`/`off` turns Claude Code's telemetry and feature-flag fetch off (default on).
pub const TELEMETRY_ENV: &str = "ULTRON_CLAUDE_TELEMETRY";

/// API credentials Claude Code would bill (the API, a gateway or Foundry) instead of the plan. They are removed from
/// the child's environment, never refused: the subscription stays primary, and `ANTHROPIC_API_KEY` is the API
/// credits fallback's key (`fallback`).
pub const API_CREDENTIALS: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_FOUNDRY_API_KEY"];
/// Conflicting endpoint overrides: refused (the relay owns `ANTHROPIC_BASE_URL`).
pub const AUTH_OVERRIDES: &[&str] = &["ANTHROPIC_BASE_URL"];
pub const BACKEND_OVERRIDES: &[&str] = &[
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// Set whatever the telemetry setting: no self-update of the user's CLI, no feedback upload path.
pub const ALWAYS_QUIET: &[(&str, &str)] = &[("DISABLE_AUTOUPDATER", "1"), ("DISABLE_FEEDBACK_COMMAND", "1")];
/// Set only with telemetry off; any one of them turns the CLI's feature-flag fetch off.
pub const QUIET_TRAFFIC: &[(&str, &str)] = &[
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("DISABLE_TELEMETRY", "1"),
    ("DISABLE_ERROR_REPORTING", "1"),
];
const OFF: &[&str] = &["false", "0", "no", "off"];

/// Session variables a parent Claude Code sets for its children, and inherited request knobs this provider owns.
const DROPPED_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SSE_PORT",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDE_CODE_MAX_OUTPUT_TOKENS",
    "CLAUDE_CODE_EXTRA_BODY",
    "CLAUDE_CODE_EFFORT_LEVEL",
];

/// Install prefixes probed after PATH (a service or desktop launch inherits a PATH without them).
const HOME_PREFIXES: &[&str] = &[
    ".local/bin",
    ".claude/local",
    "bin",
    ".npm-global/bin",
    ".bun/bin",
    ".volta/bin",
];
const SYSTEM_PREFIXES: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];
const DEFAULT_PATH: &str = "/bin:/usr/bin";

/// Native's own error codes for errors it answers without a relayed upstream status.
pub fn native_error_status(code: &str) -> Option<u16> {
    match code {
        "rate_limit" => Some(429),
        "billing_error" => Some(402),
        "authentication_failed" => Some(401),
        "overloaded" => Some(529),
        "server_error" => Some(503),
        _ => None,
    }
}

fn executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn which(name: &str, path: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return executable(Path::new(name)).then(|| PathBuf::from(name));
    }
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| executable(candidate))
}

/// The argv prefix that runs Claude Code (`_resolve`): `command`, else `ULTRON_CLAUDE_COMMAND`, else
/// `ULTRON_CLAUDE_CODE_BIN` (the binary the `claude-code` provider runs and is listed by), else `claude` on the
/// env's PATH, then in the usual install directories. Only the bare default name is probed outside PATH.
pub fn resolve_claude(command: Option<&[String]>, env: &Env) -> Option<Vec<String>> {
    let named = |name: &str| env.get(name).filter(|v| !v.trim().is_empty()).cloned();
    let command: Vec<String> = match command {
        Some(command) if !command.is_empty() => command.to_vec(),
        _ => vec![
            named(COMMAND_ENV)
                .or_else(|| named(crate::claude_code_cli::CLAUDE_CODE_BIN_ENV).map(|v| v.trim().to_string()))
                .unwrap_or_else(|| "claude".into()),
        ],
    };
    let head = &command[0];
    let found = if Path::new(head).is_absolute() && executable(Path::new(head)) {
        Some(PathBuf::from(head))
    } else {
        which(head, env.get("PATH").map_or(DEFAULT_PATH, String::as_str))
    };
    let found = found.or_else(|| {
        if head != "claude" {
            return None;
        }
        let home = env.get("HOME").filter(|h| !h.is_empty());
        let prefixes: Vec<String> = home
            .into_iter()
            .flat_map(|home| HOME_PREFIXES.iter().map(move |p| format!("{home}/{p}")))
            .chain(SYSTEM_PREFIXES.iter().map(|p| p.to_string()))
            .collect();
        which(head, &prefixes.join(":"))
    })?;
    let mut resolved = vec![found.to_string_lossy().into_owned()];
    resolved.extend(command[1..].iter().cloned());
    Some(resolved)
}

fn truthy_flag(value: Option<&String>) -> bool {
    value.is_some_and(|v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        )
    })
}

/// The names of conflicting overrides set in `env` (values are never read beyond emptiness).
pub fn conflicts(env: &Env) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = AUTH_OVERRIDES
        .iter()
        .copied()
        .filter(|k| env.get(*k).is_some_and(|v| !v.is_empty()))
        .collect();
    found.extend(BACKEND_OVERRIDES.iter().copied().filter(|k| truthy_flag(env.get(*k))));
    found
}

/// Whether telemetry and feature flags stay on: the config, else `ULTRON_CLAUDE_TELEMETRY`, default on.
pub fn telemetry_enabled(config: &Config, env: &Env) -> bool {
    config.telemetry.unwrap_or_else(|| {
        env.get(TELEMETRY_ENV)
            .is_none_or(|v| !OFF.contains(&v.trim().to_ascii_lowercase().as_str()))
    })
}

/// Set the traffic flags on a child env. Never removes anything: an opt-out the user exported still applies.
pub fn apply_traffic_policy(env: &mut Env, telemetry: bool) {
    for (k, v) in ALWAYS_QUIET {
        env.insert(k.to_string(), v.to_string());
    }
    if !telemetry {
        for (k, v) in QUIET_TRAFFIC {
            env.insert(k.to_string(), v.to_string());
        }
    }
}

/// The env a request starts from: the config's env, else the process env; `overlay` on top (an empty value
/// removes a variable).
pub fn base_env(config: &Config, overlay: Option<&crate::env::ProviderEnv>) -> Env {
    let mut env: Env = config.env.clone().unwrap_or_else(|| std::env::vars().collect());
    for (k, v) in overlay.into_iter().flatten() {
        if v.is_empty() {
            env.remove(k);
        } else {
            env.insert(k.clone(), v.clone());
        }
    }
    env
}

/// Refuse conflicting overrides before anything starts; the error names variables, never values.
pub fn refuse_overrides(env: &Env) -> Result<(), ClaudeError> {
    let found = conflicts(env);
    if found.is_empty() {
        return Ok(());
    }
    Err(ClaudeError::Invalid(format!(
        "OAuth provider refuses conflicting native auth/backend overrides: {}. claude-subscription runs on the Claude Code login; unset them for this provider.",
        found.join(", ")
    )))
}

// ------------------------------------------------------------------------------------------------- working dirs

fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

/// Our own real directory, closed to others.
fn private_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && m.uid() == uid() && m.mode() & 0o077 == 0)
}

/// Close a directory this process just created, through a descriptor opened without following links (someone
/// who can write the parent could swap a symlink in after the mkdir). Best effort; `private_dir` decides.
fn tighten(path: &Path) {
    use rustix::fs::{Mode, OFlags};
    if let Ok(fd) = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        let _ = rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700));
    }
}

/// Create `path` closed to others; `false` when something already sits there.
fn create_private(path: &Path) -> std::io::Result<bool> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {
            tighten(path);
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

fn touch(path: &Path) {
    if let Ok(dir) = std::fs::File::open(path) {
        let _ = dir.set_modified(SystemTime::now());
    }
}

/// One native cwd for every request of this OS user (`shared_workdir`): native writes its cwd into every request's
/// environment block, so a fresh directory per call would move the prompt-cache prefix. It must be our own real
/// directory, closed to others, in a parent nobody else can rename it out of; `None` falls back to a private one.
pub fn shared_workdir(temp_root: &Path) -> Option<PathBuf> {
    let path = temp_root.join(format!("ultron-claude-cwd-{}", uid()));
    let parent = std::fs::metadata(temp_root).ok()?.mode();
    if parent & 0o022 != 0 && parent & 0o1000 == 0 {
        return None;
    }
    create_private(&path).ok()?;
    if !private_dir(&path) {
        return None;
    }
    touch(&path);
    Some(path)
}

static PROCESS_WORKDIRS: LazyLock<Mutex<std::collections::HashMap<PathBuf, PathBuf>>> = LazyLock::new(Mutex::default);

fn private_temp(temp_root: &Path, prefix: &str) -> std::io::Result<PathBuf> {
    loop {
        let path = temp_root.join(format!("{prefix}{}", super::admission::random_token(6)));
        if create_private(&path)? {
            return Ok(path);
        }
    }
}

/// One private cwd for every request of this process when the shared one is refused (`process_workdir`):
/// recreated at the same path after a prune, never adopted when someone else recreated it.
pub fn process_workdir(temp_root: &Path) -> std::io::Result<PathBuf> {
    let mut table = PROCESS_WORKDIRS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = table.get(temp_root) {
        let _ = create_private(existing);
        if private_dir(existing) {
            touch(existing);
            return Ok(existing.clone());
        }
    }
    let fresh = private_temp(temp_root, "ultron-claude-cwd-")?;
    table.insert(temp_root.to_path_buf(), fresh.clone());
    Ok(fresh)
}

/// The shared cwd, else this process's private one.
pub fn workdir(temp_root: &Path) -> std::io::Result<PathBuf> {
    match shared_workdir(temp_root) {
        Some(path) => Ok(path),
        None => process_workdir(temp_root),
    }
}

// --------------------------------------------------------------------------------------------------- the request

/// A model call ready to run: the system prompt, the stream-json frames and the extra body.
#[derive(Clone, Debug)]
pub struct Prepared {
    /// The model as selected (`sonnet`, `claude-opus-5-5[1m]`, ...).
    pub model: String,
    pub system: String,
    pub frames: Vec<Value>,
    pub body: Body,
}

/// Incremental output while native streams.
#[derive(Clone, Debug, PartialEq)]
pub enum Delta {
    Text(Option<u64>, String),
    Thinking(Option<u64>, String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NativeCall {
    pub id: String,
    /// The host tool name (`rlm`).
    pub name: String,
    pub input: Value,
}

/// The completed native turn.
#[derive(Clone, Debug)]
pub struct NativeResponse {
    /// Native assistant messages as they replay (the carrier), tool names in the offered form.
    pub assistants: Vec<Value>,
    pub blocks: Vec<Value>,
    pub calls: Vec<NativeCall>,
    pub refusal: Option<String>,
    pub finish: Finish,
    pub usage: Value,
    pub cost: Value,
    pub admission: Value,
    /// Text the final message holds beyond what streamed.
    pub tail: String,
    pub warnings: Vec<String>,
}

/// Cancels one request from any thread: the relay first, then native's whole process group.
#[derive(Default)]
pub struct Handle {
    cancelled: AtomicBool,
    child: Mutex<Option<Child>>,
    admission: Mutex<Option<AbortHandle>>,
    /// The request's last `rate_limit_event` (the footer's subscription usage).
    rate_limit_event: Mutex<Option<Value>>,
}

impl Handle {
    pub fn new() -> Arc<Handle> {
        Arc::new(Handle::default())
    }

    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(admission) = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            admission.abort();
        }
        self.kill();
    }

    /// SIGKILL native's process group while its leader is not reaped (a shim's node child dies with it).
    fn kill(&self) {
        let mut child = self.child.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(child) = child.as_mut()
            && matches!(child.try_wait(), Ok(None))
            && let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32)
        {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }

    /// The request's last `rate_limit_event`, whether the call succeeded or not.
    pub fn rate_limit_event(&self) -> Option<Value> {
        self.rate_limit_event
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The exit code once native exited; `None` while it runs.
    fn exit_code(&self) -> Option<Option<i32>> {
        let mut child = self.child.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match child.as_mut().map(Child::try_wait) {
            Some(Ok(Some(status))) => Some(status.code()),
            Some(Ok(None)) => None,
            _ => Some(None),
        }
    }
}

enum Event {
    Line(Value),
    Invalid(String),
    Eof,
}

fn remove_tree(path: &Path) {
    let _ = std::fs::remove_dir_all(path);
}

/// Removes the per-request directory and stops the relay and native, whatever happened.
struct Cleanup<'a> {
    handle: &'a Handle,
    dir: Option<PathBuf>,
    admission: Option<Admission>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        self.handle.cancel();
        if let Some(mut admission) = self.admission.take() {
            admission.close();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.handle.exit_code().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(dir) = self.dir.take() {
            remove_tree(&dir);
        }
    }
}

fn text_of(blocks: &[Value], kind: &str, field: &str) -> String {
    blocks
        .iter()
        .filter(|b| b["type"] == kind)
        .filter_map(|b| b[field].as_str())
        .collect()
}

fn python_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        _ => true,
    }
}

/// Run one model call against native (blocking). `upstream` is where the admitted request goes.
pub fn run(
    config: &Config,
    env: Env,
    prepared: &Prepared,
    handle: &Arc<Handle>,
    on_delta: &mut dyn FnMut(Delta),
) -> Result<NativeResponse, ClaudeError> {
    let timeout = config.timeout;
    if timeout.is_zero() {
        return Err(ClaudeError::Invalid("timeout must be positive seconds".into()));
    }
    let mut env = env;
    refuse_overrides(&env)?;
    for key in AUTH_OVERRIDES.iter().chain(BACKEND_OVERRIDES).chain(API_CREDENTIALS) {
        env.remove(*key);
    }
    // Fail with the install hint, not a spawn error, when Claude Code is absent.
    let resolved =
        resolve_claude(config.command.as_deref(), &env).ok_or_else(|| ClaudeError::Missing(INSTALL_HINT.into()))?;
    let model = native_model(&prepared.model).map_err(ClaudeError::Invalid)?;
    let temp_root = config.temp_root.clone().unwrap_or_else(std::env::temp_dir);
    let mut cleanup = Cleanup {
        handle,
        dir: None,
        admission: None,
        reader: None,
    };
    let root = private_temp(&temp_root, "ultron-claude-").map_err(|e| ClaudeError::Failed(e.to_string()))?;
    cleanup.dir = Some(root.clone());
    let write = |name: &str, text: &str| -> Result<PathBuf, ClaudeError> {
        let path = root.join(name);
        std::fs::write(&path, text).map_err(|e| ClaudeError::Failed(e.to_string()))?;
        Ok(path)
    };
    let manifest = write("tools.json", &Value::Array(prepared.body.manifest.clone()).to_string())?;
    let mut inert = match &config.inert_mcp {
        Some(command) => command.clone(),
        None => {
            let exe = std::env::current_exe().map_err(|e| ClaudeError::Failed(e.to_string()))?;
            vec![
                exe.to_string_lossy().into_owned(),
                "__internal".into(),
                "inert-mcp".into(),
            ]
        }
    };
    inert.push(manifest.to_string_lossy().into_owned());
    let mcp = json!({"mcpServers": {"ultron": {"command": inert[0], "args": inert[1..]}}});
    for key in DROPPED_ENV {
        env.remove(*key);
    }
    for (k, v) in [
        ("ENABLE_TOOL_SEARCH", "false"),
        ("CLAUDE_CODE_MAX_RETRIES", "0"),
        ("DISABLE_AUTO_COMPACT", "1"),
        ("DISABLE_COMPACT", "1"),
        // Ultron owns budgets; native's replayed reminder invalidates cached history.
        ("CLAUDE_CODE_TOTAL_TOKENS_REMINDER", "off"),
    ] {
        env.insert(k.into(), v.into());
    }
    let telemetry = telemetry_enabled(config, &env);
    apply_traffic_policy(&mut env, telemetry);
    let queried = prepared
        .frames
        .last()
        .and_then(|f| f["message"]["content"].as_array())
        .cloned();
    let admission =
        Admission::new(&config.upstream, timeout, queried, config.idle, &env).map_err(ClaudeError::Invalid)?;
    env.insert("ANTHROPIC_BASE_URL".into(), admission.url.clone());
    let warnings = admission.warnings.clone();
    *handle
        .admission
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(admission.abort_handle());
    cleanup.admission = Some(admission);
    // Settings apply the extra body inside the process, avoiding execve's per-string limit for large schemas.
    let settings = write(
        "settings.json",
        &json!({"env": {"CLAUDE_CODE_EXTRA_BODY": prepared.body.encoded}}).to_string(),
    )?;
    let system = write("system.md", &prepared.system)?;
    let parsed = prepared.body.value();
    if let Some(max) = parsed.get("max_tokens") {
        env.insert("CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(), max.to_string());
    }
    let mut argv: Vec<String> = resolved;
    argv.extend(
        [
            "-p",
            "--model",
            &model,
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--tools",
            "",
            "--system-prompt-file",
            &system.to_string_lossy(),
            "--settings",
            &settings.to_string_lossy(),
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--max-turns",
            "1",
            "--permission-mode",
            "dontAsk",
            "--no-session-persistence",
            "--mcp-config",
            &mcp.to_string(),
        ]
        .map(str::to_string),
    );
    // Native appends a per-turn effort message at its own level unless --effort names one.
    if let Some(effort) = parsed["output_config"]["effort"].as_str() {
        argv.extend(["--effort".to_string(), effort.to_string()]);
    }
    let cwd = workdir(&temp_root).map_err(|e| ClaudeError::Failed(e.to_string()))?;
    let (sender, events) = std::sync::mpsc::channel::<Event>();
    let mut stdin = {
        use std::os::unix::process::CommandExt;
        let mut child_slot = handle.child.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if handle.cancelled() {
            return Err(ClaudeError::Cancelled);
        }
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .env_clear()
            .envs(&env)
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(|e| ClaudeError::Failed(format!("Could not start Claude Code ({}): {e}", argv[0])))?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stdin = child.stdin.take().expect("stdin is piped");
        *child_slot = Some(child);
        cleanup.reader = Some(std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                let event = match serde_json::from_str::<Value>(&line) {
                    Ok(value) => Event::Line(value),
                    Err(_) => Event::Invalid(line.chars().take(300).collect()),
                };
                if sender.send(event).is_err() {
                    return;
                }
            }
            let _ = sender.send(Event::Eof);
        }));
        stdin
    };
    let mut deadline = Instant::now() + timeout;
    let mut receive = |events: &Receiver<Event>| -> Result<Option<Value>, ClaudeError> {
        loop {
            if handle.cancelled() {
                return Err(ClaudeError::Cancelled);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ClaudeError::Timeout);
            }
            match events.recv_timeout(remaining.min(Duration::from_millis(200))) {
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) | Ok(Event::Eof) => return Ok(None),
                Ok(Event::Invalid(line)) => {
                    return Err(ClaudeError::Failed(format!(
                        "Invalid native stream-json output: '{line}'"
                    )));
                }
                Ok(Event::Line(value)) => {
                    deadline = Instant::now() + timeout;
                    return Ok(Some(value));
                }
            }
        }
    };
    let last = prepared.frames.len().saturating_sub(1);
    for (index, frame) in prepared.frames.iter().enumerate() {
        let mut frame = frame.clone();
        let parked = frame["type"] == "user" && index < last;
        if parked {
            frame["shouldQuery"] = json!(false);
        }
        let written = writeln!(stdin, "{frame}").and_then(|()| stdin.flush());
        if written.is_err() && !parked {
            break;
        }
        if parked {
            loop {
                let Some(ack) = receive(&events)? else {
                    return Err(ClaudeError::Failed("Native exited before replay acknowledgment".into()));
                };
                if ack["type"] == "result" {
                    if ack["num_turns"] != json!(0) || truthy(ack.get("is_error")) {
                        return Err(ClaudeError::Failed(
                            "Native history replay not supported: expected zero-turn acknowledgment".into(),
                        ));
                    }
                    break;
                }
            }
        }
    }
    drop(stdin);
    let mut assistants: Vec<Value> = Vec::new();
    let mut results: Vec<Value> = Vec::new();
    let mut stopped = false;
    let mut emitted = String::new();
    let mut native_error: Option<(String, Option<String>)> = None;
    let mut signals = super::limit::Signals::default();
    while let Some(event) = receive(&events)? {
        match event["type"].as_str() {
            Some("rate_limit_event") => {
                if let Some(info) = event.get("rate_limit_info").filter(|i| i.is_object()) {
                    signals.event = Some(info.clone());
                    *handle
                        .rate_limit_event
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(event.clone());
                }
            }
            Some("assistant") => {
                let failed = truthy(event.get("error")) || truthy(event["message"].get("error"));
                if failed {
                    let detail = event["message"]["content"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    native_error = Some((detail, event["error"].as_str().map(str::to_string)));
                } else {
                    assistants.push(event["message"].clone());
                }
            }
            Some("result") => results.push(event),
            Some("stream_event") => {
                let native = &event["event"];
                if native["type"] == "message_stop" {
                    stopped = true;
                }
                let index = native["index"].as_u64();
                let delta = &native["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let text = delta["text"].as_str().unwrap_or_default().to_string();
                        emitted.push_str(&text);
                        on_delta(Delta::Text(index, text));
                    }
                    Some("thinking_delta") => {
                        on_delta(Delta::Thinking(
                            index,
                            delta["thinking"].as_str().unwrap_or_default().to_string(),
                        ));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let code = loop {
        if let Some(code) = handle.exit_code() {
            break code;
        }
        if handle.cancelled() {
            return Err(ClaudeError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(ClaudeError::Timeout);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if let Some(reader) = cleanup.reader.take() {
        let _ = reader.join();
    }
    if handle.cancelled() {
        return Err(ClaudeError::Cancelled);
    }
    let admission = cleanup.admission.as_mut().expect("the relay is open");
    admission.close();
    let outcome = admission.outcome();
    signals.header_status = outcome.unified_status.clone();
    signals.header_reset = outcome.unified_reset.clone();
    signals.native_code = native_error.as_ref().and_then(|(_, code)| code.clone());
    if outcome.used {
        let complete = outcome.capture.complete;
        if outcome.status != Some(200) || !complete {
            let mut first = format!(
                "first upstream attempt: status {}, capture {}",
                outcome.status.map_or("None".into(), |s| s.to_string()),
                if complete { "complete" } else { "incomplete" }
            );
            if let Some(failure) = &outcome.failure {
                first.push_str(&format!(", relay failure {failure}"));
            }
            first.push_str(&format!(", native retries denied: {}", outcome.denied));
            let said = outcome.error_text();
            if !said.is_empty() {
                first.push_str(&format!(
                    ", upstream said: {}",
                    said.chars().take(500).collect::<String>()
                ));
            }
            let mut message = format!("Incomplete upstream response ({first})");
            if let Some((detail, _)) = &native_error {
                message.push_str(&format!(": {detail}"));
            }
            let status = outcome.status.filter(|s| *s >= 400);
            return Err(super::limit::classify(ClaudeError::Api { message, status }, &signals));
        }
        assistants = vec![outcome.capture.message.clone().unwrap_or(Value::Null)];
        stopped = true;
    }
    let handled = outcome.denied > 0 || (outcome.used && assistants[0]["stop_reason"] == "refusal");
    if let Some((detail, code)) = &native_error
        && !handled
    {
        if code.as_deref() == Some("authentication_failed") && !outcome.used {
            // No usable login where Ultron runs native: it refuses before any upstream request.
            return Err(ClaudeError::LoggedOut(format!("{LOGGED_OUT_HINT} (native: {detail})")));
        }
        let error = ClaudeError::Api {
            message: format!("Native API error: {detail}"),
            status: code.as_deref().and_then(native_error_status),
        };
        return Err(super::limit::classify(error, &signals));
    }
    if results.len() != 1 || assistants.is_empty() || !stopped {
        return Err(ClaudeError::Failed(
            "Incomplete native response: assistant, message_stop and one result required".into(),
        ));
    }
    let last_result = &results[0];
    let names: std::collections::BTreeSet<&str> = prepared.body.names.iter().map(String::as_str).collect();
    let mut calls = Vec::new();
    for assistant in &mut assistants {
        for block in assistant["content"].as_array_mut().into_iter().flatten() {
            if block["type"] != "tool_use" {
                continue;
            }
            let name = block["name"].as_str().unwrap_or_default().to_string();
            // Native sometimes drops the prefix on a tool it was offered; carry the offered name so replayed
            // history does not teach the slip back to the model.
            if names.contains(name.as_str()) {
                block["name"] = json!(format!("{PREFIX}{name}"));
            }
            let host = block["name"].as_str().unwrap_or_default();
            calls.push(NativeCall {
                id: block["id"].as_str().unwrap_or_default().to_string(),
                name: host.strip_prefix(PREFIX).unwrap_or(host).to_string(),
                input: block.get("input").cloned().unwrap_or(json!({})),
            });
        }
    }
    let subtype = last_result.get("subtype");
    let boundary = !calls.is_empty() && subtype == Some(&json!("error_max_turns")) && code == Some(1);
    if !boundary
        && !handled
        && (code != Some(0) || truthy(last_result.get("is_error")) || subtype != Some(&json!("success")))
    {
        return Err(ClaudeError::Failed(format!(
            "Native request failed: {}",
            python_str(subtype)
        )));
    }
    let usage = if outcome.used {
        assistants[0]["usage"].clone()
    } else {
        last_result["usage"].clone()
    };
    if !usage["input_tokens"].is_number() || !usage["output_tokens"].is_number() {
        return Err(ClaudeError::Failed("Native result missing complete token usage".into()));
    }
    let blocks: Vec<Value> = assistants
        .iter()
        .flat_map(|a| a["content"].as_array().cloned().unwrap_or_default())
        .collect();
    let text = text_of(&blocks, "text", "text");
    let tail = if emitted == text {
        String::new()
    } else if let Some(rest) = text.strip_prefix(&emitted) {
        rest.to_string()
    } else {
        return Err(ClaudeError::Failed(
            "Native final text differs from incremental stream".into(),
        ));
    };
    let refused = assistants.iter().any(|a| a["stop_reason"] == "refusal");
    let mut refusal = None;
    if refused {
        let details = assistants
            .iter()
            .find_map(|a| a.get("stop_details").filter(|d| d.is_object()))
            .cloned()
            .unwrap_or(json!({}));
        let explanation = details["explanation"].as_str().map(str::trim).filter(|e| !e.is_empty());
        let category = details["category"].as_str().filter(|c| !c.is_empty());
        refusal = explanation
            .map(str::to_string)
            .or_else(|| category.map(|c| format!("provider refusal category: {c}")));
        // The cut-off call must not run; the carrier keeps native's turn as sent.
        calls.clear();
    } else if let Some(other) = calls.iter().find(|c| c.name != TOOL) {
        return Err(ClaudeError::Failed(format!(
            "The model called `{}`, which is not an Ultron tool: claude-subscription advertises exactly one tool, `{TOOL}`",
            other.name
        )));
    }
    let finish = if refused {
        Finish::ContentFilter
    } else if !calls.is_empty() {
        Finish::ToolCalls
    } else if assistants.iter().any(|a| {
        matches!(
            a["stop_reason"].as_str(),
            Some("max_tokens" | "model_context_window_exceeded")
        )
    }) {
        Finish::Length
    } else {
        Finish::Stop
    };
    let mut admission_report = json!({
        "upstream_requests": u64::from(outcome.used),
        "blocked_requests": outcome.denied,
        "request_id": outcome.request_id,
    });
    if let Some(unrestored) = &outcome.unrestored {
        admission_report["unrestored"] = json!(unrestored);
    }
    let cost = json!({"total_cost_usd": last_result.get("total_cost_usd").cloned().unwrap_or(Value::Null), "modelUsage": last_result.get("modelUsage").cloned().unwrap_or(Value::Null)});
    drop(outcome);
    Ok(NativeResponse {
        assistants,
        blocks,
        calls,
        refusal,
        finish,
        usage,
        cost,
        admission: admission_report,
        tail,
        warnings,
    })
}
