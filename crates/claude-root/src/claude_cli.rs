//! The installed Claude Code CLI, driven only through its documented command line (claude-cli.ts): where it is,
//! whether it is logged in (`claude auth status --json`, which reports the login without exposing credentials),
//! and which flags this version has (read from `claude --help`). Ultron never reads, copies or writes Claude Code's
//! credential files or its settings.

use regex::Regex;
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

/// Claude Code model of the root, of claude subagents and (as claude-code/…) of frames.
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5-5";

/// Flags `ultron claude` needs from Claude Code; a CLI without one of them is too old.
pub const REQUIRED_CLAUDE_FLAGS: [&str; 6] = [
    "mcp-config",
    "strict-mcp-config",
    "tools",
    "allowedTools",
    "settings",
    "setting-sources",
];

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// `ULTRON_CLAUDE_BIN`, else `claude` on PATH.
pub fn resolve_claude_binary(var: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let path = var("PATH").unwrap_or_default();
    if let Some(configured) = var("ULTRON_CLAUDE_BIN")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        if Path::new(&configured).is_absolute() {
            return Some(configured);
        }
        return Some(which(&configured, &path).unwrap_or(configured));
    }
    which("claude", &path)
}

/// The first executable `command` in `path` (`accessSync(X_OK)`).
pub fn which(command: &str, path: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    path.split(':').filter(|dir| !dir.is_empty()).find_map(|dir| {
        let candidate = Path::new(dir).join(command);
        let mode = std::fs::metadata(&candidate).ok()?.permissions().mode();
        (mode & 0o111 != 0).then(|| candidate.display().to_string())
    })
}

/// Run `binary args` with no stdin, capturing stdout and stderr, killed after `PROBE_TIMEOUT`.
pub fn run_capture(binary: &str, args: &[&str]) -> std::io::Result<(String, String)> {
    let mut child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let out_reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = out.read_to_end(&mut text);
        text
    });
    let err_reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = err.read_to_end(&mut text);
        text
    });
    let started = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() >= PROBE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let stdout = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
    Ok((stdout, stderr))
}

/// `claude auth status --json`, reduced to the fields Ultron shows (never the account's identity).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaudeAuthStatus {
    pub logged_in: bool,
    pub auth_method: Option<String>,
    pub api_provider: Option<String>,
    pub subscription_type: Option<String>,
}

impl ClaudeAuthStatus {
    /// From the command's stdout; anything but a JSON object is "not logged in".
    pub fn parse(stdout: &str) -> ClaudeAuthStatus {
        let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(stdout) else {
            return ClaudeAuthStatus::default();
        };
        let text = |key: &str| parsed.get(key).and_then(Value::as_str).map(str::to_string);
        ClaudeAuthStatus {
            logged_in: parsed.get("loggedIn") == Some(&Value::Bool(true)),
            auth_method: text("authMethod"),
            api_provider: text("apiProvider"),
            subscription_type: text("subscriptionType"),
        }
    }

    /// Method, provider and subscription joined with ", " (empty parts left out).
    pub fn describe(&self) -> String {
        [&self.auth_method, &self.api_provider, &self.subscription_type]
            .into_iter()
            .filter_map(|v| v.as_deref().filter(|s| !s.is_empty()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub fn claude_auth_status(binary: &str) -> Result<ClaudeAuthStatus, String> {
    let (stdout, _) =
        run_capture(binary, &["auth", "status", "--json"]).map_err(|e| format!("could not run {binary}: {e}"))?;
    Ok(ClaudeAuthStatus::parse(&stdout))
}

/// The long flags `claude --help` lists (without the leading dashes).
pub fn claude_flags(binary: &str) -> BTreeSet<String> {
    let (stdout, stderr) = run_capture(binary, &["--help"]).unwrap_or_default();
    parse_flags(&format!("{stdout}\n{stderr}"))
}

static FLAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[\s,])--([A-Za-z][A-Za-z0-9-]*)").expect("flag pattern"));
static VERSION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+\.\d+\.\d+").expect("version pattern"));

pub fn parse_flags(help: &str) -> BTreeSet<String> {
    FLAG.captures_iter(help).map(|c| c[1].to_string()).collect()
}

/// `claude --version`'s version number, if it prints one.
pub fn claude_version(binary: &str) -> Option<String> {
    let (stdout, _) = run_capture(binary, &["--version"]).ok()?;
    VERSION.find(&stdout).map(|m| m.as_str().to_string())
}

/// `--dangerously-skip-permissions` for the Claude Code processes Ultron starts (`ultron claude`, its subagents,
/// `ultron --claude`): on by default, as in TypeScript. Left out with `ULTRON_CLAUDE_SKIP_PERMISSIONS=off` (or 0,
/// false, no), with the global setting `claudeCode.skipPermissions: false` (Rust only; TypeScript has no setting),
/// and when running as root, where Claude Code refuses the flag unless `IS_SANDBOX=1` says the process is
/// sandboxed (a container).
pub fn skip_permissions_args(
    var: &dyn Fn(&str) -> Option<String>,
    uid: Option<u32>,
    setting: Option<bool>,
) -> Vec<String> {
    let raw = var("ULTRON_CLAUDE_SKIP_PERMISSIONS").map(|v| v.trim().to_lowercase());
    if matches!(raw.as_deref(), Some("off" | "0" | "false" | "no")) || setting == Some(false) {
        return Vec::new();
    }
    if uid == Some(0) && var("IS_SANDBOX").as_deref() != Some("1") {
        return Vec::new();
    }
    vec!["--dangerously-skip-permissions".into()]
}

/// This process's user id.
pub fn current_uid() -> Option<u32> {
    Some(rustix::process::getuid().as_raw())
}
