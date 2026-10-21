//! Claude Code CLI transport (`api/claude-code-cli.ts`): one model call is one headless `claude -p` process speaking
//! stream-json. The CLI is feature-detected (`--version`, `--help`, `auth status --json`) once per binary and config
//! dir; every call runs isolated (no tools, MCP servers, settings sources, slash commands or session persistence, an
//! empty working directory, a replaced system prompt); active calls are capped; after a call, one idle process with
//! the same arguments is started ahead of time (bounded, expiring) for the next call of that shape.
//!
//! A request whose transcript declares tools fails before any process starts (no tool runner is registered here).

use crate::options::{SimpleStreamOptions, StreamOptions, now_ms};
use crate::transcript::{current_system_message, current_tools, system_message_text};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageDiagnostic, AssistantMessageEvent, DiagnosticErrorInfo,
    EventSink, Mapped, Message, Model, StopReason, TextContent, ThinkingContent, TranscriptContext, Usage, UserContent,
    UserMessageContent,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

pub const CLAUDE_CODE_BIN_ENV: &str = "ULTRON_CLAUDE_CODE_BIN";
pub const CLAUDE_CODE_CONCURRENCY_ENV: &str = "ULTRON_CLAUDE_CODE_CONCURRENCY";
pub const CLAUDE_CODE_TIMEOUT_ENV: &str = "ULTRON_CLAUDE_CODE_TIMEOUT_MS";
pub const CLAUDE_CODE_WARM_ENV: &str = "ULTRON_CLAUDE_CODE_WARM";
pub const CLAUDE_CODE_WARM_TTL_ENV: &str = "ULTRON_CLAUDE_CODE_WARM_TTL_MS";
pub const CLAUDE_CODE_ALLOW_THIRD_PARTY_ENV: &str = "ULTRON_CLAUDE_CODE_ALLOW_THIRD_PARTY";
pub const DEFAULT_CONCURRENCY: u64 = 4;
pub const DEFAULT_TIMEOUT_MS: u64 = 10 * 60 * 1000;
pub const DEFAULT_WARM_SPARES: u64 = 2;
pub const DEFAULT_WARM_TTL_MS: u64 = 30_000;
pub const USAGE_LIMIT_DIAGNOSTIC: &str = "provider_usage_limit";
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_FAILURE_TTL: Duration = Duration::from_secs(10);
const EXIT_GRACE: Duration = Duration::from_secs(5);
const KILL_GRACE: Duration = Duration::from_secs(2);
const STDERR_TAIL_CHARS: usize = 4_000;
const MAX_SYSTEM_PROMPT_ARG_CHARS: usize = 100_000;
const NEUTRAL_SYSTEM_PROMPT: &str = "You are a helpful assistant. Follow the user's instructions exactly.";
/// Why a request with tools fails on `claude -p`. Through `dispatch` (session lanes, frames with depth) such a request
/// runs on the Claude subscription instead (`claude_subscription::claude_code_lane`), so this is the text of an id
/// the subscription does not serve, or of a direct call. TypeScript's text recommended frames; this one does not.
pub const TOOLS_UNSUPPORTED: &str = "does not support tool calling here: `claude -p` returns completions, not raw tool calls, and lanes with tools run Claude Code models on the Claude subscription (claude-subscription), which does not serve this model id. Use a Claude model id the subscription serves (claude-subscription/<id>), or run Claude Code as the root agent with `ultron --claude` or `ultron claude`.";

/// Flags every request relies on; each must appear in `claude --help`.
pub const REQUIRED_FLAGS: &[&str] = &[
    "--print",
    "--output-format",
    "--input-format",
    "--verbose",
    "--include-partial-messages",
    "--tools",
    "--strict-mcp-config",
    "--mcp-config",
    "--setting-sources",
    "--permission-prompts",
    "--disable-slash-commands",
    "--no-session-persistence",
    "--model",
    "--system-prompt",
    "--settings",
];

/// Session variables a parent Claude Code sets for its children; a frame is not one of them.
const PARENT_SESSION_ENV: &[&str] = &[
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
];

pub type Env = BTreeMap<String, String>;

fn positive_int(value: Option<&String>, fallback: u64, allow_zero: bool) -> u64 {
    let Some(text) = value.map(|v| v.trim()).filter(|v| !v.is_empty()) else {
        return fallback;
    };
    match text.parse::<f64>() {
        Ok(n) if n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 && n >= if allow_zero { 0.0 } else { 1.0 } => {
            n as u64
        }
        _ => fallback,
    }
}

#[derive(Clone, Copy, Debug)]
struct RuntimeConfig {
    concurrency: u64,
    timeout_ms: u64,
    warm_spares: u64,
    warm_ttl_ms: u64,
}

fn runtime_config(env: &Env, options: &StreamOptions) -> RuntimeConfig {
    RuntimeConfig {
        concurrency: positive_int(env.get(CLAUDE_CODE_CONCURRENCY_ENV), DEFAULT_CONCURRENCY, false),
        timeout_ms: options
            .timeout_ms
            .unwrap_or_else(|| positive_int(env.get(CLAUDE_CODE_TIMEOUT_ENV), DEFAULT_TIMEOUT_MS, false)),
        warm_spares: positive_int(env.get(CLAUDE_CODE_WARM_ENV), DEFAULT_WARM_SPARES, true),
        warm_ttl_ms: positive_int(env.get(CLAUDE_CODE_WARM_TTL_ENV), DEFAULT_WARM_TTL_MS, false),
    }
}

/// `childEnv`: the process env plus the provider env, without the parent Claude Code session variables.
pub fn child_env(options: &StreamOptions) -> Env {
    let mut env: Env = std::env::vars().collect();
    for (name, value) in options.env.iter().flatten() {
        env.insert(name.clone(), value.clone());
    }
    for name in PARENT_SESSION_ENV {
        env.remove(*name);
    }
    env
}

// --------------------------------------------------------------------------------------------- locating the CLI

static RESOLVED_CLI: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(Mutex::default);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn is_executable(file: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(file).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// `resolveClaudeCli`: `ULTRON_CLAUDE_CODE_BIN`, else the first `claude` on PATH. Only a found CLI is remembered.
pub fn resolve_claude_cli(env: &Env) -> Result<String, String> {
    let configured = env
        .get(CLAUDE_CODE_BIN_ENV)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let key = format!(
        "{}\0{}",
        configured.as_deref().unwrap_or_default(),
        env.get("PATH").map(String::as_str).unwrap_or_default()
    );
    if let Some(found) = lock(&RESOLVED_CLI).get(&key) {
        return Ok(found.clone());
    }
    let found = match &configured {
        Some(path) => is_executable(Path::new(path)).then(|| path.clone()),
        None => env
            .get("PATH")
            .map(String::as_str)
            .unwrap_or_default()
            .split(':')
            .filter(|dir| !dir.is_empty())
            .map(|dir| Path::new(dir).join("claude"))
            .find(|candidate| is_executable(candidate))
            .map(|p| p.to_string_lossy().into_owned()),
    };
    match found {
        Some(found) => {
            lock(&RESOLVED_CLI).insert(key, found.clone());
            Ok(found)
        }
        None => Err(match configured {
            Some(path) => format!("claude-code provider: {CLAUDE_CODE_BIN_ENV}={path} is not an executable file."),
            None => "claude-code provider: the Claude Code CLI (`claude`) was not found on PATH. Install Claude Code and run `claude auth login`, or set ULTRON_CLAUDE_CODE_BIN.".into(),
        }),
    }
}

// ------------------------------------------------------------------------------------- feature detection / login

#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeCliCapabilities {
    pub version: String,
    pub flags: HashSet<String>,
    pub warnings: Vec<String>,
}

struct ProbeEntry {
    result: Result<ClaudeCliCapabilities, String>,
    at: Instant,
}

static PROBES: LazyLock<Mutex<HashMap<String, ProbeEntry>>> = LazyLock::new(Mutex::default);

struct ExecResult {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn exec_cli(bin: &str, args: &[&str], env: &Env, cwd: &Path) -> Result<ExecResult, String> {
    let mut child = Command::new(bin)
        .args(args)
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("claude-code provider: could not run {bin} {}: {e}", args.join(" ")))?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(stdout) = stdout.as_mut() {
            let _ = stdout.read_to_string(&mut text);
        }
        text
    });
    let err_thread = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(stderr) = stderr.as_mut() {
            let _ = stderr.read_to_string(&mut text);
        }
        text
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                break child.wait().map_err(|e| e.to_string())?;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => return Err(e.to_string()),
        }
    };
    Ok(ExecResult {
        code: status.code(),
        stdout: out_thread.join().unwrap_or_default(),
        stderr: err_thread.join().unwrap_or_default(),
    })
}

/// `parseHelpFlags`: the long flags a help text lists, lower-cased.
pub fn parse_help_flags(help: &str) -> Vec<String> {
    let mut flags: Vec<String> = Vec::new();
    let chars: Vec<char> = help.chars().collect();
    let mut i = 0;
    while i + 2 < chars.len() + 1 {
        let boundary = i == 0 || chars[i - 1].is_whitespace() || chars[i - 1] == ',';
        if boundary
            && chars.get(i) == Some(&'-')
            && chars.get(i + 1) == Some(&'-')
            && chars.get(i + 2).is_some_and(|c| c.is_ascii_alphabetic())
        {
            let mut j = i + 3;
            while chars.get(j).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '-') {
                j += 1;
            }
            let flag: String = chars[i..j].iter().collect::<String>().to_lowercase();
            if !flags.contains(&flag) {
                flags.push(flag);
            }
            i = j;
            continue;
        }
        i += 1;
    }
    flags
}

fn tail(text: &str, limit: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() > limit {
        String::from_utf16_lossy(&units[units.len() - limit..])
    } else {
        text.to_string()
    }
}

fn probe_cli(bin: &str, env: &Env, cwd: &Path) -> Result<ClaudeCliCapabilities, String> {
    let version_run = exec_cli(bin, &["--version"], env, cwd)?;
    let help_run = exec_cli(bin, &["--help"], env, cwd)?;
    let auth_run = exec_cli(bin, &["auth", "status", "--json"], env, cwd)?;
    let version = version_run
        .stdout
        .split_whitespace()
        .next()
        .filter(|v| !v.is_empty())
        .unwrap_or("unknown")
        .to_string();
    let flags: HashSet<String> = parse_help_flags(&help_run.stdout).into_iter().collect();
    let missing: Vec<&str> = REQUIRED_FLAGS.iter().copied().filter(|f| !flags.contains(*f)).collect();
    if !missing.is_empty() {
        return Err(format!(
            "claude-code provider: the installed Claude Code CLI ({bin}, version {version}) does not support {} (not listed in `claude --help`). Update Claude Code (`claude update`).",
            missing.join(", ")
        ));
    }
    let auth = serde_json::from_str::<Value>(auth_run.stdout.trim())
        .ok()
        .filter(|v| v.get("loggedIn").is_some_and(Value::is_boolean));
    let Some(auth) = auth else {
        let text = if auth_run.stderr.is_empty() {
            &auth_run.stdout
        } else {
            &auth_run.stderr
        };
        let code = auth_run.code.map_or("null".to_string(), |c| c.to_string());
        return Err(format!(
            "claude-code provider: `claude auth status --json` did not report a login state (exit {code}): {}",
            tail(text, 500)
        ));
    };
    if auth["loggedIn"] != true {
        return Err("claude-code provider: the Claude Code CLI is not logged in. Run `claude auth login` (or `claude` and /login) and try again.".into());
    }
    let mut warnings = Vec::new();
    if let Some(provider) = auth
        .get("apiProvider")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty() && *p != "firstParty")
    {
        let message = format!("the Claude Code CLI is configured for {provider}, not a first-party Anthropic login");
        if env.get(CLAUDE_CODE_ALLOW_THIRD_PARTY_ENV).map(String::as_str) != Some("1") {
            return Err(format!(
                "claude-code provider: {message}. Set {CLAUDE_CODE_ALLOW_THIRD_PARTY_ENV}=1 to use it anyway."
            ));
        }
        warnings.push(message);
    }
    if let Some(method) = auth
        .get("authMethod")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty() && *m != "claude.ai")
    {
        warnings.push(format!(
            "the Claude Code CLI authenticates with {method}, not a claude.ai subscription login; calls may be billed to that account"
        ));
    }
    Ok(ClaudeCliCapabilities {
        version,
        flags,
        warnings,
    })
}

/// `ensureClaudeCli`: probe once per CLI and config dir; a failure is retried after a short delay.
pub async fn ensure_claude_cli(bin: &str, env: &Env, cwd: &Path) -> Result<ClaudeCliCapabilities, String> {
    let key = format!(
        "{bin}\0{}",
        env.get("CLAUDE_CONFIG_DIR").map(String::as_str).unwrap_or_default()
    );
    if let Some(entry) = lock(&PROBES).get(&key)
        && (entry.result.is_ok() || entry.at.elapsed() < PROBE_FAILURE_TTL)
    {
        return entry.result.clone();
    }
    let (bin_owned, env_owned, cwd_owned) = (bin.to_string(), env.clone(), cwd.to_path_buf());
    let result = tokio::task::spawn_blocking(move || probe_cli(&bin_owned, &env_owned, &cwd_owned))
        .await
        .map_err(|e| e.to_string())?;
    lock(&PROBES).insert(
        key,
        ProbeEntry {
            result: result.clone(),
            at: Instant::now(),
        },
    );
    result
}

// ------------------------------------------------------------------------------------------------ working directory

static WORK_DIR: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(Mutex::default);

/// One empty directory per user, so no project CLAUDE.md or settings are near the call.
pub fn empty_work_dir() -> Result<PathBuf, String> {
    let mut guard = lock(&WORK_DIR);
    if let Some(dir) = guard.as_ref() {
        return Ok(dir.clone());
    }
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let user = std::fs::metadata("/proc/self")
        .map(|m| m.uid().to_string())
        .unwrap_or_else(|_| "user".into());
    let dir = std::env::temp_dir().join(format!("ultron-claude-code-{user}"));
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| e.to_string())?;
    *guard = Some(dir.clone());
    Ok(dir)
}

// ---------------------------------------------------------------------------------------------- concurrency cap

#[derive(Default)]
struct Limiter {
    active: Mutex<u64>,
    notify: Notify,
}

static LIMITER: LazyLock<Arc<Limiter>> = LazyLock::new(Arc::default);

struct Permit(Arc<Limiter>);

impl Drop for Permit {
    fn drop(&mut self) {
        *lock(&self.0.active) -= 1;
        self.0.notify.notify_one();
    }
}

async fn acquire(limit: u64, signal: Option<&crate::http::AbortSignal>) -> Result<Permit, String> {
    let limiter = LIMITER.clone();
    loop {
        let notified = limiter.notify.notified();
        {
            let mut active = lock(&limiter.active);
            if *active < limit {
                *active += 1;
                return Ok(Permit(limiter.clone()));
            }
        }
        tokio::select! {
            () = notified => {}
            () = crate::http::cancelled(signal) => return Err("This operation was aborted".into()),
        }
    }
}

// --------------------------------------------------------------------------------------------------- processes

/// Exit code and signal of a finished process.
type ExitStatus = (Option<i32>, Option<i32>);

enum ChildEvent {
    Line(Value),
    Exited(Option<i32>, Option<i32>),
}

/// One `claude` process: stdout lines that are JSON objects become events; stderr keeps a tail.
pub struct CliProcess {
    child: Arc<Mutex<Child>>,
    stdin: Option<ChildStdin>,
    events: tokio::sync::mpsc::UnboundedReceiver<ChildEvent>,
    stderr: Arc<Mutex<String>>,
    exited: Arc<Mutex<Option<ExitStatus>>>,
    spawn_error: Option<String>,
    pid: u32,
}

impl CliProcess {
    fn spawn(bin: &str, args: &[String], env: &Env, cwd: &Path) -> Self {
        use std::os::unix::process::CommandExt;
        let (sender, events) = tokio::sync::mpsc::unbounded_channel();
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let exited = Arc::new(Mutex::new(None));
        let spawned = Command::new(bin)
            .args(args)
            .env_clear()
            .envs(env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                *lock(&exited) = Some((None, None));
                let _ = sender.send(ChildEvent::Exited(None, None));
                return CliProcess {
                    child: Arc::new(Mutex::new(Command::new("true").spawn().expect("`true` runs"))),
                    stdin: None,
                    events,
                    stderr: stderr_tail,
                    exited,
                    spawn_error: Some(error.to_string()),
                    pid: 0,
                };
            }
        };
        let pid = child.id();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let child = Arc::new(Mutex::new(child));
        if let Some(stderr) = stderr {
            let tail_buffer = stderr_tail.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut chunk = String::new();
                while reader.read_line(&mut chunk).is_ok_and(|n| n > 0) {
                    let mut buffer = lock(&tail_buffer);
                    let joined = format!("{buffer}{chunk}");
                    *buffer = tail(&joined, STDERR_TAIL_CHARS);
                    chunk.clear();
                }
            });
        }
        {
            let child = child.clone();
            let exited = exited.clone();
            std::thread::spawn(move || {
                if let Some(stdout) = stdout {
                    for line in BufReader::new(stdout).lines() {
                        let Ok(line) = line else { break };
                        let line = line.trim();
                        if line.starts_with('{')
                            && let Ok(value) = serde_json::from_str::<Value>(line)
                        {
                            let _ = sender.send(ChildEvent::Line(value));
                        }
                    }
                }
                let status = loop {
                    let polled = lock(&child).try_wait();
                    match polled {
                        Ok(Some(status)) => break Some(status),
                        Ok(None) => std::thread::sleep(Duration::from_millis(2)),
                        Err(_) => break None,
                    }
                };
                use std::os::unix::process::ExitStatusExt;
                let code = status.and_then(|s| s.code());
                let signal = status.and_then(|s| s.signal());
                *lock(&exited) = Some((code, signal));
                let _ = sender.send(ChildEvent::Exited(code, signal));
            });
        }
        CliProcess {
            child,
            stdin,
            events,
            stderr: stderr_tail,
            exited,
            spawn_error: None,
            pid,
        }
    }

    fn alive(&self) -> bool {
        self.spawn_error.is_none() && lock(&self.exited).is_none()
    }

    fn send(&mut self, message: &Value) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "{message}");
            let _ = stdin.flush();
        }
    }

    /// Close stdin so the CLI exits by itself; stop it if it lingers.
    fn finish(mut self) {
        self.stdin.take();
        if !self.alive() {
            return;
        }
        std::thread::spawn(move || {
            let deadline = Instant::now() + EXIT_GRACE;
            while Instant::now() < deadline {
                if !self.alive() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            self.kill();
        });
    }

    /// SIGTERM to the process group, SIGKILL after a grace period.
    fn kill(&self) {
        if !self.alive() || self.pid == 0 {
            return;
        }
        let group = format!("-{}", self.pid);
        let signal = |sig: &str| {
            let sent = Command::new("kill")
                .args([sig, "--", &group])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if !sent {
                let _ = lock(&self.child).kill();
            }
        };
        signal("-TERM");
        let child = self.child.clone();
        let exited = self.exited.clone();
        let pid = self.pid;
        std::thread::spawn(move || {
            std::thread::sleep(KILL_GRACE);
            if lock(&exited).is_none() {
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{pid}")])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = lock(&child).kill();
            }
        });
    }
}

// --------------------------------------------------------------------------------------------------- warm spares

struct Spare {
    key: String,
    process: CliProcess,
    expires: Instant,
}

static SPARES: LazyLock<Mutex<Vec<Spare>>> = LazyLock::new(Mutex::default);
static PROVEN_KEYS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

fn take_spare(key: &str) -> Option<CliProcess> {
    let mut spares = lock(&SPARES);
    let mut index = 0;
    while index < spares.len() {
        if spares[index].key != key {
            index += 1;
            continue;
        }
        let spare = spares.remove(index);
        if spare.process.alive() && spare.expires > Instant::now() {
            return Some(spare.process);
        }
        spare.process.kill();
    }
    None
}

fn start_spare(key: &str, bin: &str, args: &[String], env: &Env, cwd: &Path, config: RuntimeConfig) {
    if config.warm_spares == 0 {
        return;
    }
    let mut spares = lock(&SPARES);
    let now = Instant::now();
    spares.retain(|spare| {
        let keep = spare.expires > now && spare.process.alive();
        if !keep {
            spare.process.kill();
        }
        keep
    });
    if spares.iter().any(|s| s.key == key) {
        return;
    }
    while spares.len() as u64 >= config.warm_spares {
        let oldest = spares.remove(0);
        oldest.process.kill();
    }
    spares.push(Spare {
        key: key.to_string(),
        process: CliProcess::spawn(bin, args, env, cwd),
        expires: now + Duration::from_millis(config.warm_ttl_ms),
    });
}

/// Test hook: forget cached CLI paths, probes and warm spares.
pub fn reset_claude_code_cli_state() {
    lock(&RESOLVED_CLI).clear();
    lock(&PROBES).clear();
    lock(&PROVEN_KEYS).clear();
    for spare in lock(&SPARES).drain(..) {
        spare.process.kill();
    }
    *lock(&WORK_DIR) = None;
}

/// `claudeCodeCliStats`: requests in flight and live spares.
pub fn claude_code_cli_stats() -> (u64, usize) {
    let in_flight = *lock(&LIMITER.active);
    let spares = lock(&SPARES).iter().filter(|s| s.process.alive()).count();
    (in_flight, spares)
}

// ------------------------------------------------------------------------------------------- request construction

fn push_text(blocks: &mut Vec<Value>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = blocks.last_mut()
        && last["type"] == "text"
    {
        let joined = format!("{}{text}", last["text"].as_str().unwrap_or_default());
        last["text"] = json!(joined);
        return;
    }
    blocks.push(json!({"type": "text", "text": text}));
}

fn push_content(blocks: &mut Vec<Value>, content: &[UserContent], images: bool) {
    for part in content {
        match part {
            UserContent::Text(text) => push_text(blocks, &text.text),
            UserContent::Image(image) if images => blocks.push(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": image.mime_type, "data": image.data},
            })),
            UserContent::Image(_) => push_text(blocks, "[image omitted: this model takes text only]"),
        }
    }
}

fn push_user(blocks: &mut Vec<Value>, content: &UserMessageContent, images: bool) {
    match content {
        UserMessageContent::Text(text) => push_text(blocks, text),
        UserMessageContent::Blocks(parts) => push_content(blocks, parts, images),
    }
}

/// `renderClaudeCodePrompt`: one user turn as is, a longer conversation as a text transcript.
pub fn render_claude_code_prompt(messages: &[Message], images: bool) -> Vec<Value> {
    let turns: Vec<&Message> = messages.iter().filter(|m| !matches!(m, Message::System(_))).collect();
    let mut blocks = Vec::new();
    if let [Message::User(user)] = turns.as_slice() {
        push_user(&mut blocks, &user.content, images);
        if blocks.is_empty() {
            push_text(&mut blocks, "(empty message)");
        }
        return blocks;
    }
    push_text(&mut blocks, "<conversation>\n");
    for message in turns {
        match message {
            Message::User(user) => {
                push_text(&mut blocks, "<user>\n");
                push_user(&mut blocks, &user.content, images);
                push_text(&mut blocks, "\n</user>\n");
            }
            Message::Assistant(assistant) => {
                let parts: Vec<String> = assistant
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        AssistantContent::Text(text) => Some(text.text.clone()),
                        AssistantContent::ToolCall(call) => Some(format!(
                            "[tool call {} (id {}): {}]",
                            call.name,
                            call.id,
                            crate::json::stringify(&call.arguments)
                        )),
                        AssistantContent::Thinking(_) => None,
                    })
                    .collect();
                push_text(
                    &mut blocks,
                    &format!("<assistant>\n{}\n</assistant>\n", parts.join("\n")),
                );
            }
            Message::ToolResult(result) => {
                push_text(
                    &mut blocks,
                    &format!(
                        "<tool_result name=\"{}\" id=\"{}\"{}>\n",
                        result.tool_name,
                        result.tool_call_id,
                        if result.is_error { " error=\"true\"" } else { "" }
                    ),
                );
                push_content(&mut blocks, &result.content, images);
                push_text(&mut blocks, "\n</tool_result>\n");
            }
            Message::System(_) => {}
        }
    }
    push_text(
        &mut blocks,
        "</conversation>\n\nYou are the assistant in the conversation above. Write the assistant's next reply to the latest user message; reply with that text only.",
    );
    blocks
}

/// What the provider sends (`ClaudeCodePayload`), exposed to `onPayload`.
fn payload(model: &Model, context: &TranscriptContext, options: &ClaudeCodeOptions) -> Value {
    let system = current_system_message(&context.messages)
        .map(|m| system_message_text(&m))
        .unwrap_or_default();
    let mut payload = Map::new();
    payload.insert("model".into(), json!(model.id));
    payload.insert("system".into(), json!(system));
    payload.insert(
        "content".into(),
        Value::Array(render_claude_code_prompt(&context.messages, model.supports_images())),
    );
    payload.insert(
        "max_tokens".into(),
        json!(
            options
                .base
                .base
                .max_tokens
                .unwrap_or(model.max_tokens)
                .min(model.max_tokens)
        ),
    );
    if let Some(schema) = options.json_schema.as_ref().filter(|s| s.is_object()) {
        payload.insert("json_schema".into(), schema.clone());
    }
    if let Some(effort) = effort_for(model, options.base.reasoning.as_deref()) {
        payload.insert("effort".into(), json!(effort));
    }
    Value::Object(payload)
}

fn effort_for(model: &Model, level: Option<&str>) -> Option<String> {
    let level = level?;
    if !model.reasoning {
        return None;
    }
    match model.thinking_level(level) {
        Mapped::Unsupported => None,
        Mapped::Value(value) => Some(value.to_string()),
        Mapped::Missing => Some(level.to_string()),
    }
}

struct PlannedRequest {
    args: Vec<String>,
    env: Env,
    message: Value,
    structured: bool,
}

fn plan_request(
    model: &Model,
    payload: &Value,
    base_env: &Env,
    default_max_tokens: u64,
    capabilities: &ClaudeCliCapabilities,
    use_schema_flag: bool,
) -> Result<PlannedRequest, String> {
    let mut content: Vec<Value> = payload["content"].as_array().cloned().unwrap_or_default();
    let mut system = payload["system"].as_str().unwrap_or_default().trim().to_string();
    if system.is_empty() {
        system = NEUTRAL_SYSTEM_PROMPT.into();
    }
    if system.encode_utf16().count() > MAX_SYSTEM_PROMPT_ARG_CHARS {
        content.insert(
            0,
            json!({"type": "text", "text": format!("<instructions>\n{system}\n</instructions>\n\n")}),
        );
        system = NEUTRAL_SYSTEM_PROMPT.into();
    }
    let schema = payload.get("json_schema").filter(|s| !s.is_null());
    let structured = schema.is_some_and(|s| s.is_object() && s["type"] == "object") && use_schema_flag;
    if structured && !capabilities.flags.contains("--json-schema") {
        return Err(format!(
            "claude-code provider: this request needs --json-schema, which the installed Claude Code CLI ({}) does not list in `claude --help`.",
            capabilities.version
        ));
    }
    if let Some(schema) = schema
        && !structured
    {
        push_text(
            &mut content,
            &format!(
                "\n\nReply with only one JSON value (no prose, no code fences) that satisfies this JSON schema: {}",
                crate::json::stringify(schema)
            ),
        );
    }
    let effort = payload.get("effort").and_then(Value::as_str).filter(|e| !e.is_empty());
    if effort.is_some() && !capabilities.flags.contains("--effort") {
        return Err(format!(
            "claude-code provider: a thinking level needs --effort, which the installed Claude Code CLI ({}) does not list in `claude --help`.",
            capabilities.version
        ));
    }
    let model_id = payload["model"].as_str().filter(|m| !m.is_empty()).unwrap_or(&model.id);
    let mut args: Vec<String> = [
        "--print",
        "--output-format=stream-json",
        "--input-format=stream-json",
        "--verbose",
        "--include-partial-messages",
        "--tools=",
        "--strict-mcp-config",
        "--mcp-config={\"mcpServers\":{}}",
        "--setting-sources=",
        "--permission-prompts=none",
        "--disable-slash-commands",
        "--no-session-persistence",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(format!("--model={model_id}"));
    args.push(format!("--system-prompt={system}"));
    args.push(match effort {
        Some(effort) => format!("--effort={effort}"),
        None => "--settings={\"alwaysThinkingEnabled\":false}".into(),
    });
    if structured && let Some(schema) = schema {
        args.push(format!("--json-schema={}", crate::json::stringify(schema)));
    }
    let mut env = base_env.clone();
    if let Some(max_tokens) = payload["max_tokens"].as_u64()
        && max_tokens > 0
        && max_tokens != default_max_tokens
    {
        env.insert("CLAUDE_CODE_MAX_OUTPUT_TOKENS".into(), max_tokens.to_string());
    }
    Ok(PlannedRequest {
        args,
        env,
        message: json!({"type": "user", "message": {"role": "user", "content": content}}),
        structured,
    })
}

// ----------------------------------------------------------------------------------------------- result mapping

fn num(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|n| n.is_finite() && *n >= 0.0)
}

/// `usageFromResult`: token counts from the result event; the cost is the CLI's `total_cost_usd`, or unknown (NaN).
pub fn usage_from_result(result: &Value) -> Usage {
    let usage = result
        .get("usage")
        .filter(|u| u.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let count = |key: &str| num(usage.get(key)).unwrap_or(0.0) as u64;
    let mut out = Usage {
        input: count("input_tokens"),
        output: count("output_tokens"),
        cache_read: count("cache_read_input_tokens"),
        cache_write: count("cache_creation_input_tokens"),
        ..Usage::default()
    };
    if let Some(long) = num(usage.pointer("/cache_creation/ephemeral_1h_input_tokens")).filter(|n| *n > 0.0) {
        out.cache_write_1h = Some(long as u64);
    }
    if let Some(thinking) = num(usage.pointer("/output_tokens_details/thinking_tokens")) {
        out.reasoning = Some(thinking as u64);
    }
    out.total_tokens = out.input + out.output + out.cache_read + out.cache_write;
    out.cost.total = num(result.get("total_cost_usd")).unwrap_or(f64::NAN);
    out
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RateLimitInfo {
    pub status: Option<String>,
    pub rate_limit_type: Option<String>,
    pub resets_at: Option<f64>,
}

pub fn rate_limit_info(event: &Value) -> Option<RateLimitInfo> {
    let info = event.get("rate_limit_info").filter(|i| i.is_object())?;
    Some(RateLimitInfo {
        status: info.get("status").and_then(Value::as_str).map(str::to_string),
        rate_limit_type: info.get("rateLimitType").and_then(Value::as_str).map(str::to_string),
        resets_at: num(info.get("resetsAt")),
    })
}

/// `new Date(ms).toISOString()`.
pub fn iso_string(ms: f64) -> String {
    let ms = ms as i64;
    let date = crate::aws::amz_date(ms);
    format!(
        "{}-{}-{}T{}:{}:{}.{:03}Z",
        &date[0..4],
        &date[4..6],
        &date[6..8],
        &date[9..11],
        &date[11..13],
        &date[13..15],
        ms.rem_euclid(1000)
    )
}

/// `resetText`.
pub fn reset_text(info: Option<&RateLimitInfo>) -> String {
    match info.and_then(|i| i.resets_at).filter(|r| *r != 0.0) {
        Some(resets_at) => {
            let ms = if resets_at < 1e12 {
                resets_at * 1000.0
            } else {
                resets_at
            };
            format!("; resets at {}", iso_string(ms))
        }
        None => String::new(),
    }
}

enum Failure {
    Length,
    SchemaRejected(String),
    Error { message: String, usage_limit: bool },
}

fn contains_ci(text: &str, needle: &str) -> bool {
    text.to_lowercase().contains(needle)
}

/// `classifyFailure`.
fn classify_failure(result: &Value, rate_limit: Option<&RateLimitInfo>) -> Failure {
    let text = result
        .get("result")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .unwrap_or("unknown error")
        .to_string();
    let status = num(result.get("api_error_status")).map(|s| s as u64);
    let lower = text.to_lowercase();
    if output_limit_exceeded(&lower) {
        return Failure::Length;
    }
    if status == Some(400) && lower.contains("input_schema") {
        return Failure::SchemaRejected(text);
    }
    if rate_limit.and_then(|r| r.status.as_deref()) == Some("rejected")
        || lower.contains("usage limit")
        || lower.contains("limit reached")
        || lower.contains("out of usage")
        || lower.contains("out of extra usage")
    {
        let window = match rate_limit.and_then(|r| r.rate_limit_type.as_deref()) {
            Some(kind) => format!(" ({kind} window{})", reset_text(rate_limit)),
            None => String::new(),
        };
        return Failure::Error {
            message: format!(
                "Claude Code usage limit reached{window}: {text}. The subscription's capacity is exhausted for now; this is not retried."
            ),
            usage_limit: true,
        };
    }
    if status == Some(429) || crate::jsutil::loose_match(&text, "rate", "limit") {
        return Failure::Error {
            message: format!("Claude Code rate limited (429): {text}"),
            usage_limit: false,
        };
    }
    if status == Some(529) || lower.contains("overloaded") {
        return Failure::Error {
            message: format!("Claude Code overloaded (529): {text}"),
            usage_limit: false,
        };
    }
    if [
        "not logged in",
        "please run /login",
        "invalid api key",
        "authentication",
    ]
    .iter()
    .any(|n| contains_ci(&text, n))
    {
        return Failure::Error {
            message: format!("Claude Code CLI authentication failed: {text}. Run `claude auth login` and try again."),
            usage_limit: false,
        };
    }
    Failure::Error {
        message: format!(
            "Claude Code CLI error{}: {text}",
            status
                .filter(|s| *s != 0)
                .map(|s| format!(" ({s})"))
                .unwrap_or_default()
        ),
        usage_limit: false,
    }
}

/// `/exceeded the \d+ output token maximum/i`.
fn output_limit_exceeded(lower: &str) -> bool {
    let mut rest = lower;
    while let Some(index) = rest.find("exceeded the ") {
        let after = &rest[index + "exceeded the ".len()..];
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && after[digits..].starts_with(" output token maximum") {
            return true;
        }
        rest = &rest[index + 1..];
    }
    false
}

fn diagnostic(kind: &str, message: &str) -> AssistantMessageDiagnostic {
    AssistantMessageDiagnostic {
        kind: kind.into(),
        timestamp: now_ms(),
        error: Some(DiagnosticErrorInfo {
            name: None,
            message: message.into(),
            stack: None,
            code: None,
        }),
        details: None,
    }
}

// ------------------------------------------------------------------------------------------------------ one request

/// Options of a Claude Code request: the simple options plus an optional JSON schema for structured output.
#[derive(Clone, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeOptions {
    #[serde(flatten)]
    pub base: SimpleStreamOptions,
    pub json_schema: Option<Value>,
}

struct RequestRun<'s, 'e> {
    output: AssistantMessage,
    sink: &'s mut EventSink<'e>,
    blocks: HashMap<i64, usize>,
    streamed: HashSet<String>,
    rate_limit: Option<RateLimitInfo>,
    usage_known: bool,
    warnings: Vec<String>,
}

enum RunError {
    Aborted(String),
    Failed { message: String, usage_limit: bool },
}

impl RequestRun<'_, '_> {
    fn emit(&mut self, event: impl FnOnce(AssistantMessage) -> AssistantMessageEvent) {
        let event = event(self.output.clone());
        (self.sink)(&event);
    }

    fn warn(&mut self, message: String) {
        if !self.warnings.contains(&message) {
            self.warnings.push(message);
        }
    }

    fn add_text(&mut self, text: &str) {
        self.output.content.push(AssistantContent::Text(TextContent {
            text: String::new(),
            text_signature: None,
        }));
        let index = self.output.content.len() - 1;
        self.emit(|partial| AssistantMessageEvent::TextStart {
            content_index: index,
            partial,
        });
        if let AssistantContent::Text(block) = &mut self.output.content[index] {
            block.text = text.to_string();
        }
        let delta = text.to_string();
        self.emit(|partial| AssistantMessageEvent::TextDelta {
            content_index: index,
            delta,
            partial,
        });
        let content = text.to_string();
        self.emit(|partial| AssistantMessageEvent::TextEnd {
            content_index: index,
            content,
            partial,
        });
    }

    fn on_stream_event(&mut self, event: &Value, structured: bool) {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or_default();
        if kind == "message_start" {
            self.blocks.clear();
            if let Some(id) = event.pointer("/message/id").and_then(Value::as_str) {
                self.streamed.insert(id.to_string());
                self.output.response_id = Some(id.to_string());
            }
            if let Some(model) = event.pointer("/message/model").and_then(Value::as_str)
                && model != self.output.model
            {
                self.output.response_model = Some(model.to_string());
            }
            return;
        }
        let cli_index = event.get("index").and_then(Value::as_i64).unwrap_or(-1);
        if kind == "content_block_start" {
            match event.pointer("/content_block/type").and_then(Value::as_str) {
                Some("text") if !structured => {
                    self.output.content.push(AssistantContent::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    }));
                    let index = self.output.content.len() - 1;
                    self.blocks.insert(cli_index, index);
                    self.emit(|partial| AssistantMessageEvent::TextStart {
                        content_index: index,
                        partial,
                    });
                }
                Some("thinking") => {
                    self.output.content.push(AssistantContent::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    }));
                    let index = self.output.content.len() - 1;
                    self.blocks.insert(cli_index, index);
                    self.emit(|partial| AssistantMessageEvent::ThinkingStart {
                        content_index: index,
                        partial,
                    });
                }
                _ => {}
            }
            return;
        }
        let Some(&index) = self.blocks.get(&cli_index) else {
            return;
        };
        if kind == "content_block_delta" {
            let delta = event.get("delta").cloned().unwrap_or(Value::Null);
            match (
                delta.get("type").and_then(Value::as_str),
                &mut self.output.content[index],
            ) {
                (Some("text_delta"), AssistantContent::Text(block)) => {
                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                        block.text.push_str(text);
                        let delta = text.to_string();
                        self.emit(|partial| AssistantMessageEvent::TextDelta {
                            content_index: index,
                            delta,
                            partial,
                        });
                    }
                }
                (Some("thinking_delta"), AssistantContent::Thinking(block)) => {
                    if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                        block.thinking.push_str(text);
                        let delta = text.to_string();
                        self.emit(|partial| AssistantMessageEvent::ThinkingDelta {
                            content_index: index,
                            delta,
                            partial,
                        });
                    }
                }
                _ => {}
            }
        } else if kind == "content_block_stop" {
            self.blocks.remove(&cli_index);
            match &self.output.content[index] {
                AssistantContent::Text(text) => {
                    let content = text.text.clone();
                    self.emit(|partial| AssistantMessageEvent::TextEnd {
                        content_index: index,
                        content,
                        partial,
                    });
                }
                AssistantContent::Thinking(thinking) => {
                    let content = thinking.thinking.clone();
                    self.emit(|partial| AssistantMessageEvent::ThinkingEnd {
                        content_index: index,
                        content,
                        partial,
                    });
                }
                AssistantContent::ToolCall(_) => {}
            }
        }
    }

    fn on_assistant(&mut self, event: &Value, structured: bool) {
        let Some(message) = event.get("message").filter(|m| m.is_object()) else {
            return;
        };
        if structured {
            return;
        }
        if let Some(id) = message.get("id").and_then(Value::as_str)
            && self.streamed.contains(id)
        {
            return;
        }
        if message.get("model").and_then(Value::as_str) == Some("<synthetic>") {
            return;
        }
        for part in message.get("content").and_then(Value::as_array).into_iter().flatten() {
            if part.get("type").and_then(Value::as_str) == Some("text")
                && let Some(text) = part.get("text").and_then(Value::as_str).filter(|t| !t.is_empty())
            {
                self.add_text(text);
            }
        }
    }

    fn finish(&mut self, result: &Value, plan: &PlannedRequest) -> Option<Failure> {
        self.output.usage = usage_from_result(result);
        self.usage_known = true;
        if result.get("is_error") == Some(&json!(true)) {
            return match classify_failure(result, self.rate_limit.as_ref()) {
                Failure::Length => {
                    self.output.stop_reason = StopReason::Length;
                    self.output.raw_stop_reason = Some("max_output_tokens".into());
                    None
                }
                other => Some(other),
            };
        }
        if plan.structured {
            let text = match result.get("structured_output").filter(|v| !v.is_null()) {
                Some(value) => crate::json::stringify(value),
                None => result
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            };
            self.add_text(&text);
        } else if !self
            .output
            .content
            .iter()
            .any(|p| matches!(p, AssistantContent::Text(_)))
            && let Some(text) = result.get("result").and_then(Value::as_str).filter(|t| !t.is_empty())
        {
            self.add_text(text);
        }
        let stop = result.get("stop_reason").and_then(Value::as_str);
        self.output.stop_reason = if stop == Some("max_tokens") {
            StopReason::Length
        } else {
            StopReason::Stop
        };
        if let Some(stop) = stop {
            self.output.raw_stop_reason = Some(stop.to_string());
        }
        self.output.end_turn = Some(stop == Some("end_turn"));
        None
    }

    async fn drive(
        &mut self,
        child: &mut CliProcess,
        plan: &PlannedRequest,
        signal: Option<&crate::http::AbortSignal>,
        deadline: Instant,
        timeout_ms: u64,
    ) -> Result<Option<Failure>, RunError> {
        if crate::http::is_aborted(signal) {
            child.kill();
            return Err(RunError::Aborted("Request was aborted".into()));
        }
        child.send(&plan.message);
        loop {
            let next = tokio::select! {
                event = child.events.recv() => event,
                () = crate::http::cancelled(signal) => {
                    child.kill();
                    return Err(RunError::Aborted("Request was aborted".into()));
                }
                () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    child.kill();
                    return Err(RunError::Failed {
                        message: format!("Claude Code CLI request timed out after {}s", (timeout_ms as f64 / 1000.0).round()),
                        usage_limit: false,
                    });
                }
            };
            let event = match next {
                Some(ChildEvent::Line(event)) => event,
                Some(ChildEvent::Exited(code, signal)) => {
                    // Lines the reader sent before the exit are already delivered; there was no result.
                    return Err(self.exit_error(child, code, signal));
                }
                None => return Err(self.exit_error(child, None, None)),
            };
            match (
                event.get("type").and_then(Value::as_str),
                event.get("subtype").and_then(Value::as_str),
            ) {
                (Some("system"), Some("init")) => {
                    if let Some(source) = event
                        .get("apiKeySource")
                        .and_then(Value::as_str)
                        .filter(|s| *s != "none")
                    {
                        self.warn(format!(
                            "the Claude Code CLI is using an API key (apiKeySource: {source}) instead of the subscription login; calls may be billed to that key"
                        ));
                    }
                }
                (Some("stream_event"), _) => {
                    if let Some(inner) = event.get("event").filter(|e| e.is_object()) {
                        self.on_stream_event(inner, plan.structured);
                    }
                }
                (Some("assistant"), _) => self.on_assistant(&event, plan.structured),
                (Some("rate_limit_event"), _) => {
                    if let Some(info) = rate_limit_info(&event) {
                        self.rate_limit = Some(info);
                    }
                    if let Some(info) = self.rate_limit.clone()
                        && info.status.as_deref() == Some("allowed_warning")
                    {
                        self.warn(format!(
                            "Claude Code subscription usage is near its {} limit{}",
                            info.rate_limit_type.as_deref().unwrap_or_default(),
                            reset_text(Some(&info))
                        ));
                    }
                }
                (Some("result"), _) => return Ok(self.finish(&event, plan)),
                _ => {}
            }
        }
    }

    fn exit_error(&self, child: &CliProcess, code: Option<i32>, signal: Option<i32>) -> RunError {
        if let Some(error) = &child.spawn_error {
            return RunError::Failed {
                message: format!("Claude Code CLI failed to start: {error}"),
                usage_limit: false,
            };
        }
        // Give the stderr reader a moment to collect the last lines.
        std::thread::sleep(Duration::from_millis(20));
        let stderr = lock(&child.stderr).clone();
        if let Some(start) = stderr.find("unknown option '") {
            let rest = &stderr[start + "unknown option '".len()..];
            if let Some(end) = rest.find('\'') {
                return RunError::Failed {
                    message: format!(
                        "claude-code provider: the installed Claude Code CLI rejected {}. Update Claude Code (`claude update`).",
                        &rest[..end]
                    ),
                    usage_limit: false,
                };
            }
        }
        let how = match (signal, code) {
            (Some(signal), _) => signal_name(signal),
            (None, code) => format!("code {}", code.map_or("null".into(), |c| c.to_string())),
        };
        let trimmed = stderr.trim();
        RunError::Failed {
            message: format!(
                "Claude Code CLI exited ({how}) before a result{}",
                if trimmed.is_empty() {
                    String::new()
                } else {
                    format!(": {}", tail(trimmed, 1_000))
                }
            ),
            usage_limit: false,
        }
    }
}

fn signal_name(signal: i32) -> String {
    match signal {
        1 => "SIGHUP",
        2 => "SIGINT",
        6 => "SIGABRT",
        9 => "SIGKILL",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        _ => return format!("signal {signal}"),
    }
    .into()
}

/// One request against the CLI as an assistant-message stream.
pub async fn stream_simple(
    model: &Model,
    context: &TranscriptContext,
    options: &ClaudeCodeOptions,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    let mut output = AssistantMessage::pending(model, now_ms());
    output.stop_reason = StopReason::Stop;
    let mut run = RequestRun {
        output,
        sink,
        blocks: HashMap::new(),
        streamed: HashSet::new(),
        rate_limit: None,
        usage_known: false,
        warnings: Vec::new(),
    };
    let mut spawned = false;
    let result = drive(&mut run, model, context, options, &mut spawned).await;
    match result {
        Ok(()) => {
            if !run.warnings.is_empty() {
                run.output.diagnostics = Some(
                    run.warnings
                        .iter()
                        .map(|w| diagnostic("claude_code_warning", w))
                        .collect(),
                );
            }
            let reason = if run.output.stop_reason == StopReason::Length {
                StopReason::Length
            } else {
                StopReason::Stop
            };
            run.emit(|message| AssistantMessageEvent::Done { reason, message });
        }
        Err(error) => {
            let aborted = crate::http::is_aborted(options.base.base.signal());
            let (message, usage_limit, aborted) = match error {
                RunError::Aborted(message) => (message, false, true),
                RunError::Failed { message, usage_limit } => (message, usage_limit, aborted),
            };
            run.output.stop_reason = if aborted {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            run.output.error_message = Some(message.clone());
            let mut diagnostics: Vec<AssistantMessageDiagnostic> = run
                .warnings
                .iter()
                .map(|w| diagnostic("claude_code_warning", w))
                .collect();
            if usage_limit {
                diagnostics.push(diagnostic(USAGE_LIMIT_DIAGNOSTIC, &message));
            }
            if !diagnostics.is_empty() {
                run.output.diagnostics = Some(diagnostics);
            }
            if spawned && !run.usage_known {
                run.output.usage.cost.total = f64::NAN;
            }
            let reason = run.output.stop_reason;
            run.emit(|error| AssistantMessageEvent::Error { reason, error });
        }
    }
    run.output
}

async fn drive(
    run: &mut RequestRun<'_, '_>,
    model: &Model,
    context: &TranscriptContext,
    options: &ClaudeCodeOptions,
    spawned: &mut bool,
) -> Result<(), RunError> {
    let base = &options.base.base;
    let signal = base.signal();
    let failed = |message: String| RunError::Failed {
        message,
        usage_limit: false,
    };
    if crate::http::is_aborted(signal) {
        return Err(RunError::Aborted("This operation was aborted".into()));
    }
    let tools: Vec<String> = current_tools(&context.messages).into_iter().map(|t| t.name).collect();
    if !tools.is_empty() {
        let listed = tools.iter().take(8).cloned().collect::<Vec<_>>().join(", ");
        let more = if tools.len() > 8 { ", ..." } else { "" };
        return Err(failed(format!(
            "{}/{} {TOOLS_UNSUPPORTED} (this lane declares tools: {listed}{more}).",
            model.provider, model.id
        )));
    }
    let env = child_env(base);
    let config = runtime_config(&env, base);
    let bin = resolve_claude_cli(&env).map_err(failed)?;
    let cwd = empty_work_dir().map_err(failed)?;
    let capabilities = ensure_claude_cli(&bin, &env, &cwd).await.map_err(failed)?;
    for warning in &capabilities.warnings {
        run.warn(warning.clone());
    }
    if crate::http::is_aborted(signal) {
        return Err(RunError::Aborted("This operation was aborted".into()));
    }
    let default_max_tokens = model.max_tokens;
    let mut request = payload(model, context, options);
    if let Some(next) = base.on_payload.as_ref().and_then(|hook| hook(&request, model)) {
        request = next;
    }
    let _permit = acquire(config.concurrency, signal).await.map_err(RunError::Aborted)?;
    let deadline = Instant::now() + Duration::from_millis(config.timeout_ms);
    run.emit(|partial| AssistantMessageEvent::Start { partial });
    let mut use_schema_flag = true;
    loop {
        let plan = plan_request(
            model,
            &request,
            &env,
            default_max_tokens,
            &capabilities,
            use_schema_flag,
        )
        .map_err(failed)?;
        let key = crate::json::stringify(&json!([bin, cwd.to_string_lossy(), plan.args, plan.env]));
        let mut child = take_spare(&key).unwrap_or_else(|| CliProcess::spawn(&bin, &plan.args, &plan.env, &cwd));
        *spawned = true;
        if lock(&PROVEN_KEYS).contains(&key) {
            start_spare(&key, &bin, &plan.args, &plan.env, &cwd, config);
        }
        let attempt = run.drive(&mut child, &plan, signal, deadline, config.timeout_ms).await;
        child.finish();
        match attempt? {
            Some(Failure::SchemaRejected(message)) if use_schema_flag => {
                run.warn(format!(
                    "--json-schema was rejected ({message}); asked for JSON in the prompt instead"
                ));
                use_schema_flag = false;
                continue;
            }
            Some(Failure::SchemaRejected(message)) => {
                return Err(RunError::Failed {
                    message,
                    usage_limit: false,
                });
            }
            Some(Failure::Error { message, usage_limit }) => return Err(RunError::Failed { message, usage_limit }),
            Some(Failure::Length) | None => {}
        }
        if !crate::http::is_aborted(signal) {
            lock(&PROVEN_KEYS).insert(key.clone());
            start_spare(&key, &bin, &plan.args, &plan.env, &cwd, config);
        }
        return Ok(());
    }
}

/// `stream`: the same as [`stream_simple`].
pub async fn stream(
    model: &Model,
    context: &TranscriptContext,
    options: &ClaudeCodeOptions,
    sink: &mut EventSink<'_>,
) -> AssistantMessage {
    stream_simple(model, context, options, sink).await
}
