//! Host-checked delegation contracts for `rlm.spawn` subagents (decision row 210, from Sakana MASS): a spawn names
//! the child's `role`, a `contract` the host verifies itself when the child ends, and a `hop`: what happens when the
//! contract fails. New in Rust; no TypeScript counterpart.
//!
//! `contract={"checks": [...], "returns": <JSON schema>}`. Checks, each an object with one kind:
//!
//! - `{"run": "<shell command>", "expect": "exit 0" | {"exit": N} | {"stdout_contains": s}, "timeout_ms": N}`: the
//!   host runs the command in the child's working directory (its worktree with `worktree=True`, else the session's)
//!   under the session's sandbox; `expect` defaults to exit 0, `timeout_ms` to 120 000 (at most 600 000);
//! - `{"changed_files_within": [globs]}`: every file that changed in the child's working directory while it ran
//!   matches one of the globs (`*`, `**`, `?`, `[...]`, `{a,b}`, as memory `path_globs`);
//! - `{"no_new_dependencies": true}`: no dependency manifest or lockfile ([`DEPENDENCY_FILES`]) changed;
//! - `{"files_exist": [paths]}`: each path (relative to the working directory) exists.
//!
//! `returns` is a JSON schema `rlm.finish(outputs=...)` must satisfy: a finish whose outputs miss it is rejected
//! (the child may fix it, as any rejected verdict), and a child that ends without a matching verdict fails it.
//!
//! The host never trusts the child: the checks run after its run ended, on the files as they are then. All pass:
//! the result's `check.contract.verdict` is "verified", with each check's output as evidence. Any fails: "contract_failed"
//! naming each failed check with bounded output, the result becomes a failure (`rlm.merge` then skips it unless
//! `include_failed=True`), and the hop runs: "report" (the default) stops there; `{"retry": N}` (1..=3) re-runs the
//! child up to N more times with the failures appended to its brief; `{"review": brief}` spawns a reviewer child
//! (named `<name>-review`) with the brief, the result and the failures, its handle in `check.contract.hop`.

use crate::schema::{errors as schema_errors, unchecked_keyword, validate_json_schema};
use crate::workspace::WorkspaceDiff;
use kernel::Signal;
use kernel_apis::memory_records::Glob;
use serde_json::{Map, Value, json};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Most checks one contract may list.
pub const MAX_CHECKS: usize = 16;
/// Most extra runs a `{"retry": N}` hop may ask for.
pub const MAX_RETRIES: u32 = 3;
/// A `run` check's default deadline, as the host's other checks (code skill tests, review commands).
pub const DEFAULT_CHECK_TIMEOUT_MS: u64 = 120_000;
pub const MAX_CHECK_TIMEOUT_MS: u64 = 600_000;
/// Characters of a command's stdout (and of its stderr) kept as evidence: the end, where the verdict usually is.
pub const OUTPUT_CHARS: usize = 2_000;
const MAX_TEXT: usize = 4_000;
/// Paths a `changed_files_within` / `no_new_dependencies` failure lists.
const LISTED: usize = 20;

/// File names that declare or lock dependencies, at any depth (`no_new_dependencies`).
pub const DEPENDENCY_FILES: [&str; 22] = [
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "pyproject.toml",
    "poetry.lock",
    "uv.lock",
    "Pipfile",
    "Pipfile.lock",
    "setup.py",
    "setup.cfg",
    "go.mod",
    "go.sum",
    "Gemfile",
    "Gemfile.lock",
    "composer.json",
    "composer.lock",
];

/// Whether `path` (relative, with `/`) is a dependency manifest or lockfile; `requirements*.txt` counts too.
pub fn is_dependency_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    DEPENDENCY_FILES.contains(&name) || (name.starts_with("requirements") && name.ends_with(".txt"))
}

/// What a `run` check expects of its command.
#[derive(Clone, Debug, PartialEq)]
pub enum Expect {
    Exit(i32),
    StdoutContains(String),
}

/// One host-run check.
#[derive(Clone, Debug, PartialEq)]
pub enum HostCheck {
    Run {
        command: String,
        expect: Expect,
        timeout_ms: u64,
    },
    ChangedFilesWithin(Vec<String>),
    NoNewDependencies,
    FilesExist(Vec<String>),
}

/// What happens when the contract fails.
#[derive(Clone, Debug, PartialEq)]
pub enum Hop {
    Report,
    Retry(u32),
    Review(String),
}

/// A spawn's role, contract and hop.
#[derive(Clone, Debug, PartialEq)]
pub struct SpawnContract {
    pub role: Option<String>,
    pub checks: Vec<HostCheck>,
    pub returns: Option<Value>,
    pub hop: Hop,
}

/// The keys of `rlm.spawn` (and of `rlm-child@1` workflow nodes) this module reads.
pub const SPAWN_KEYS: [&str; 3] = ["role", "contract", "hop"];

fn only(map: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<(), String> {
    match map.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("Unknown {what} field: {key}")),
        None => Ok(()),
    }
}

fn text(value: &Value, what: &str) -> Result<String, String> {
    match value {
        Value::String(text) if !text.trim().is_empty() && text.chars().count() <= MAX_TEXT => Ok(text.clone()),
        _ => Err(format!(
            "{what} must be a nonempty string of at most {MAX_TEXT} characters"
        )),
    }
}

fn strings(value: &Value, what: &str) -> Result<Vec<String>, String> {
    match value.as_array() {
        Some(items) if !items.is_empty() && items.len() <= 100 => items
            .iter()
            .map(|item| match item.as_str().map(str::trim) {
                Some(text) if !text.is_empty() => Ok(text.to_string()),
                _ => Err(format!("{what} must be a nonempty list of nonempty strings")),
            })
            .collect(),
        _ => Err(format!("{what} must be a nonempty list of at most 100 strings")),
    }
}

fn integer(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().filter(|v| v.fract() == 0.0).map(|v| v as i64))
}

fn parse_expect(value: Option<&Value>, at: &str) -> Result<Expect, String> {
    let wrong = || {
        format!("{at}.expect must be \"exit 0\" (or \"exit N\"), {{\"exit\": N}} or {{\"stdout_contains\": \"text\"}}")
    };
    match value {
        None | Some(Value::Null) => Ok(Expect::Exit(0)),
        Some(Value::String(text)) => {
            let code = text.trim().strip_prefix("exit").map(str::trim).ok_or_else(wrong)?;
            code.parse::<i32>().map(Expect::Exit).map_err(|_| wrong())
        }
        Some(Value::Object(map)) if map.len() == 1 => match (map.get("exit"), map.get("stdout_contains")) {
            (Some(code), None) => integer(code)
                .and_then(|code| i32::try_from(code).ok())
                .map(Expect::Exit)
                .ok_or_else(wrong),
            (None, Some(Value::String(part))) if !part.is_empty() => Ok(Expect::StdoutContains(part.clone())),
            _ => Err(wrong()),
        },
        Some(_) => Err(wrong()),
    }
}

impl HostCheck {
    /// One check as written; `index` names it in errors.
    pub fn parse(value: &Value, index: usize) -> Result<HostCheck, String> {
        let at = format!("contract.checks[{index}]");
        let map = value.as_object().ok_or_else(|| format!("{at} must be an object"))?;
        if map.contains_key("run") {
            only(map, &["run", "expect", "timeout_ms"], "contract check")?;
            let timeout_ms = match map.get("timeout_ms") {
                None | Some(Value::Null) => DEFAULT_CHECK_TIMEOUT_MS,
                Some(value) => integer(value)
                    .filter(|ms| (1..=MAX_CHECK_TIMEOUT_MS as i64).contains(ms))
                    .ok_or_else(|| format!("{at}.timeout_ms must be an integer from 1 to {MAX_CHECK_TIMEOUT_MS}"))?
                    as u64,
            };
            return Ok(HostCheck::Run {
                command: text(&map["run"], &format!("{at}.run"))?,
                expect: parse_expect(map.get("expect"), &at)?,
                timeout_ms,
            });
        }
        if let Some(globs) = map.get("changed_files_within") {
            only(map, &["changed_files_within"], "contract check")?;
            let globs = strings(globs, &format!("{at}.changed_files_within"))?;
            if let Some(bad) = globs.iter().find(|glob| Glob::new(glob).is_err()) {
                return Err(format!("{at}.changed_files_within has an invalid glob: {bad}"));
            }
            return Ok(HostCheck::ChangedFilesWithin(globs));
        }
        if let Some(flag) = map.get("no_new_dependencies") {
            only(map, &["no_new_dependencies"], "contract check")?;
            if flag != &Value::Bool(true) {
                return Err(format!("{at}.no_new_dependencies must be true"));
            }
            return Ok(HostCheck::NoNewDependencies);
        }
        if let Some(paths) = map.get("files_exist") {
            only(map, &["files_exist"], "contract check")?;
            let paths = strings(paths, &format!("{at}.files_exist"))?;
            if let Some(bad) = paths
                .iter()
                .find(|p| p.starts_with('/') || p.split('/').any(|s| s == ".."))
            {
                return Err(format!(
                    "{at}.files_exist paths are relative to the child's working directory, not {bad}"
                ));
            }
            return Ok(HostCheck::FilesExist(paths));
        }
        Err(format!(
            "{at} must be one of {{\"run\": cmd, \"expect\": ...}}, {{\"changed_files_within\": [globs]}}, {{\"no_new_dependencies\": true}}, {{\"files_exist\": [paths]}}"
        ))
    }

    /// The check as written (normalized).
    pub fn describe(&self) -> Value {
        match self {
            HostCheck::Run {
                command,
                expect,
                timeout_ms,
            } => {
                let expect = match expect {
                    Expect::Exit(code) => json!({ "exit": code }),
                    Expect::StdoutContains(part) => json!({ "stdout_contains": part }),
                };
                json!({ "run": command, "expect": expect, "timeout_ms": timeout_ms })
            }
            HostCheck::ChangedFilesWithin(globs) => json!({ "changed_files_within": globs }),
            HostCheck::NoNewDependencies => json!({ "no_new_dependencies": true }),
            HostCheck::FilesExist(paths) => json!({ "files_exist": paths }),
        }
    }

    /// A short name for failure lists.
    pub fn label(&self) -> String {
        match self {
            HostCheck::Run { command, .. } => format!("run {}", Value::from(command.as_str())),
            HostCheck::ChangedFilesWithin(globs) => format!("changed_files_within {}", json!(globs)),
            HostCheck::NoNewDependencies => "no_new_dependencies".into(),
            HostCheck::FilesExist(paths) => format!("files_exist {}", json!(paths)),
        }
    }
}

impl Hop {
    pub fn parse(value: Option<&Value>) -> Result<Hop, String> {
        let wrong = || {
            format!("hop must be \"report\", {{\"retry\": N}} (N from 1 to {MAX_RETRIES}) or {{\"review\": \"brief\"}}")
        };
        match value {
            None | Some(Value::Null) => Ok(Hop::Report),
            Some(Value::String(name)) if name == "report" => Ok(Hop::Report),
            Some(Value::Object(map)) if map.len() == 1 => match (map.get("retry"), map.get("review")) {
                (Some(count), None) => integer(count)
                    .filter(|n| (1..=i64::from(MAX_RETRIES)).contains(n))
                    .map(|n| Hop::Retry(n as u32))
                    .ok_or_else(wrong),
                (None, Some(brief)) => text(brief, "hop.review").map(Hop::Review),
                _ => Err(wrong()),
            },
            Some(_) => Err(wrong()),
        }
    }

    pub fn describe(&self) -> Value {
        match self {
            Hop::Report => json!("report"),
            Hop::Retry(count) => json!({ "retry": count }),
            Hop::Review(brief) => json!({ "review": brief }),
        }
    }
}

impl SpawnContract {
    /// The contract of a spawn's options (`role`, `contract`, `hop`); `None` when it names none of them.
    pub fn parse(kwargs: &Map<String, Value>) -> Result<Option<SpawnContract>, String> {
        if !SPAWN_KEYS
            .iter()
            .any(|key| kwargs.get(*key).is_some_and(|v| !v.is_null()))
        {
            return Ok(None);
        }
        let role = match kwargs.get("role") {
            None | Some(Value::Null) => None,
            Some(role) => Some(text(role, "role")?),
        };
        let (checks, returns) = match kwargs.get("contract") {
            None | Some(Value::Null) => (Vec::new(), None),
            Some(Value::Object(map)) => {
                only(map, &["checks", "returns"], "contract")?;
                let checks = match map.get("checks") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(items)) if items.len() <= MAX_CHECKS => items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| HostCheck::parse(item, index))
                        .collect::<Result<_, _>>()?,
                    Some(_) => return Err(format!("contract.checks must be a list of at most {MAX_CHECKS} checks")),
                };
                let returns = match map.get("returns") {
                    None | Some(Value::Null) => None,
                    Some(schema @ Value::Object(_)) => {
                        validate_json_schema(schema, "contract.returns")?;
                        if let Some(keyword) = unchecked_keyword(schema, "contract.returns") {
                            return Err(format!("{keyword} is not supported"));
                        }
                        Some(schema.clone())
                    }
                    Some(_) => return Err("contract.returns must be a JSON schema object".into()),
                };
                (checks, returns)
            }
            Some(_) => {
                return Err(
                    "contract must be a dict: {\"checks\": [...], \"returns\": <JSON schema of rlm.finish outputs>}"
                        .into(),
                );
            }
        };
        let hop = Hop::parse(kwargs.get("hop"))?;
        if hop != Hop::Report && checks.is_empty() && returns.is_none() {
            return Err("hop needs a contract to fail: give contract={\"checks\": [...]}".into());
        }
        Ok(Some(SpawnContract {
            role,
            checks,
            returns,
            hop,
        }))
    }

    /// Whether anything is checked when the child ends.
    pub fn checks_anything(&self) -> bool {
        !self.checks.is_empty() || self.returns.is_some()
    }

    /// The contract as written (normalized), for the fingerprint and the result.
    pub fn describe(&self) -> Value {
        json!({
            "role": self.role,
            "checks": self.checks.iter().map(HostCheck::describe).collect::<Vec<_>>(),
            "returns": self.returns,
            "hop": self.hop.describe(),
        })
    }

    /// The child's brief: its role and what the host will check ahead of the task text.
    pub fn prompt(&self, brief: &str) -> String {
        let mut lines = Vec::new();
        if let Some(role) = &self.role {
            lines.push(format!("Role: {role}"));
        }
        if !self.checks.is_empty() {
            let checks: Vec<String> = self.checks.iter().map(|c| c.describe().to_string()).collect();
            lines.push(format!(
                "Contract: when you finish, the host itself runs these checks in your working directory: {}",
                checks.join("; ")
            ));
        }
        if let Some(schema) = &self.returns {
            lines.push(format!(
                "Contract: rlm.finish(outputs=...) must match this JSON schema: {schema}"
            ));
        }
        if lines.is_empty() {
            return brief.to_string();
        }
        format!("{}\n\n{brief}", lines.join("\n"))
    }

    /// Problems of a finish payload's `outputs` against `returns` (empty when it matches or none is set).
    pub fn returns_problems(&self, outputs: &Value) -> Vec<String> {
        let Some(schema) = &self.returns else {
            return Vec::new();
        };
        schema_errors(schema, outputs)
            .into_iter()
            .map(|error| {
                let at = if error.instance_path.is_empty() {
                    "outputs".to_string()
                } else {
                    format!("outputs{}", error.instance_path)
                };
                format!("{at}: {} (contract.returns)", error.message)
            })
            .collect()
    }

    /// Run every check on a child that ended (`verdict`: its recorded `rlm.finish` payload, if any). Returns the
    /// report: `{"verdict": "verified" | "contract_failed", "checks": [...], "failed": [...]}`.
    pub fn evaluate(&self, verdict: Option<&Value>, place: &CheckPlace<'_>, runner: &CheckRunner) -> Value {
        let mut entries = Vec::new();
        let mut failed = Vec::new();
        if self.returns.is_some() {
            let (ok, evidence) = match verdict {
                None => (
                    false,
                    "the child ended without rlm.finish, so its outputs were never given".to_string(),
                ),
                Some(verdict) => {
                    let problems = self.returns_problems(&verdict["outputs"]);
                    match problems.is_empty() {
                        true => (true, "rlm.finish outputs match contract.returns".to_string()),
                        false => (false, problems.join("; ")),
                    }
                }
            };
            if !ok {
                failed.push(format!("returns: {evidence}"));
            }
            entries.push(json!({ "check": {"returns": self.returns}, "ok": ok, "evidence": evidence }));
        }
        for check in &self.checks {
            let mut entry = run_check(check, place, runner);
            if entry["ok"] != true {
                failed.push(format!(
                    "{}: {}",
                    check.label(),
                    entry["evidence"].as_str().unwrap_or_default()
                ));
            }
            entry["check"] = check.describe();
            entries.push(entry);
        }
        json!({
            "verdict": if failed.is_empty() { "verified" } else { "contract_failed" },
            "checks": entries,
            "failed": failed,
        })
    }
}

/// Where a child's checks run: its working directory and environment, and what changed there while it ran.
pub struct CheckPlace<'a> {
    pub cwd: Option<&'a Path>,
    pub env: &'a [(String, String)],
    /// `None`: no snapshot to compare (no working directory).
    pub diff: Option<&'a WorkspaceDiff>,
    /// Aborted when the task is cancelled: a running command is stopped.
    pub signal: &'a Signal,
}

/// A `run` check's command, for a [`CheckRunner`].
pub struct CommandCheck {
    pub command: String,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
    pub signal: Signal,
}

/// How a command ended.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CommandOutcome {
    /// `None` when it was stopped (timeout, cancel) or never started.
    pub exit: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// Why it could not start (or was refused).
    pub error: Option<String>,
}

/// Runs a `run` check's command: the session's runs it under its sandbox ([`run_command`] on a launcher's command).
pub type CheckRunner = Arc<dyn Fn(&CommandCheck) -> CommandOutcome + Send + Sync>;

/// The runner of a host without a sandbox: `/bin/sh -c` in the working directory with the child's environment.
pub fn plain_check_runner() -> CheckRunner {
    Arc::new(|check: &CommandCheck| {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", &check.command])
            .current_dir(&check.cwd)
            .envs(check.env.iter().map(|(k, v)| (k, v)));
        run_command(command, check.timeout, &check.signal)
    })
}

/// The last `limit` characters of `text`, with a marker when cut.
fn tail(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().skip(count - limit).collect();
    format!("[... {} characters cut]{kept}", count - limit)
}

fn drain(source: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let Some(mut source) = source else {
            return String::new();
        };
        let mut kept: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(n) = source.read(&mut chunk) {
            if n == 0 {
                break;
            }
            kept.extend_from_slice(&chunk[..n]);
            // Bounded: only the end is evidence.
            if kept.len() > 64 * OUTPUT_CHARS {
                kept.drain(..kept.len() - 16 * OUTPUT_CHARS);
            }
        }
        String::from_utf8_lossy(&kept).into_owned()
    })
}

/// Run `command` to its end in a process group of its own, stdin closed, stdout and stderr kept (bounded). Past
/// `timeout`, or once `signal` is aborted, the group this call started is stopped (TERM, then KILL after 5 s).
pub fn run_command(mut command: Command, timeout: Duration, signal: &Signal) -> CommandOutcome {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return CommandOutcome {
                error: Some(error.to_string()),
                ..CommandOutcome::default()
            };
        }
    };
    let (stdout, stderr) = (drain(child.stdout.take()), drain(child.stderr.take()));
    // The command's own group (process_group(0) above), through the kernel's guard: never pid <= 1, this process
    // or its own group.
    let group = i32::try_from(child.id()).ok().and_then(|pgid| {
        kernel::command_group_target(
            pgid,
            rustix::process::getpid().as_raw_nonzero().get(),
            rustix::process::getpgrp().as_raw_nonzero().get(),
        )
    });
    let started = Instant::now();
    let mut stopped_at: Option<Instant> = None;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if stopped_at.is_none() && (started.elapsed() >= timeout || signal.is_aborted()) {
            timed_out = !signal.is_aborted();
            if let Some(group) = group {
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::TERM);
            }
            stopped_at = Some(Instant::now());
        }
        if let (Some(at), Some(group)) = (stopped_at, group)
            && at.elapsed() >= Duration::from_secs(5)
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
        // The check thread waits on the task's signal, so a cancel stops the command at once.
        signal.wait(Duration::from_millis(10));
    };
    let (stdout, stderr) = (stdout.join().unwrap_or_default(), stderr.join().unwrap_or_default());
    CommandOutcome {
        exit: if stopped_at.is_some() {
            None
        } else {
            status.and_then(|s| s.code())
        },
        stdout,
        stderr,
        timed_out,
        error: None,
    }
}

fn listed(paths: &[String]) -> String {
    let shown: Vec<&str> = paths.iter().take(LISTED).map(String::as_str).collect();
    let more = paths.len().saturating_sub(LISTED);
    match more {
        0 => shown.join(", "),
        _ => format!("{} (and {more} more)", shown.join(", ")),
    }
}

/// One check's outcome: `{"ok", "evidence", ...}` (a `run` check adds `exit_code`, `stdout`, `stderr`).
fn run_check(check: &HostCheck, place: &CheckPlace<'_>, runner: &CheckRunner) -> Value {
    let no_diff = || json!({ "ok": false, "evidence": "no working directory was snapshotted when the child started, so its changes are unknown" });
    match check {
        HostCheck::Run {
            command,
            expect,
            timeout_ms,
        } => {
            let cwd = match place.cwd {
                Some(cwd) => cwd.to_path_buf(),
                None => match std::env::current_dir() {
                    Ok(cwd) => cwd,
                    Err(error) => return json!({ "ok": false, "evidence": format!("no working directory: {error}") }),
                },
            };
            let outcome = runner(&CommandCheck {
                command: command.clone(),
                cwd,
                env: place.env.to_vec(),
                timeout: Duration::from_millis(*timeout_ms),
                signal: place.signal.clone(),
            });
            let (stdout, stderr) = (tail(&outcome.stdout, OUTPUT_CHARS), tail(&outcome.stderr, OUTPUT_CHARS));
            let ended = match (&outcome.error, outcome.timed_out, outcome.exit) {
                (Some(error), _, _) => format!("could not run: {error}"),
                (None, true, _) => format!("timed out after {timeout_ms} ms"),
                (None, false, Some(code)) => format!("exit {code}"),
                (None, false, None) => "stopped before it ended".to_string(),
            };
            let (ok, evidence) = match expect {
                Expect::Exit(code) => (outcome.error.is_none() && outcome.exit == Some(*code), ended.clone()),
                Expect::StdoutContains(part) => {
                    let found = outcome.error.is_none() && outcome.exit.is_some() && outcome.stdout.contains(part);
                    let said = if found { "contains" } else { "does not contain" };
                    (found, format!("{ended}; stdout {said} {}", Value::from(part.as_str())))
                }
            };
            let mut entry = json!({ "ok": ok, "evidence": evidence, "exit_code": outcome.exit, "stdout": stdout,
                                    "stderr": stderr });
            if !ok {
                // A failure names the deciding output: the end of stderr, else of stdout.
                let shown = if outcome.stderr.trim().is_empty() {
                    &outcome.stdout
                } else {
                    &outcome.stderr
                };
                let shown = tail(shown.trim(), 500);
                if !shown.is_empty() {
                    entry["evidence"] = json!(format!("{evidence}: {shown}"));
                }
            }
            entry
        }
        HostCheck::ChangedFilesWithin(globs) => {
            let Some(diff) = place.diff else {
                return no_diff();
            };
            let compiled: Vec<Glob> = globs.iter().filter_map(|g| Glob::new(g).ok()).collect();
            let outside: Vec<String> = diff
                .changed
                .iter()
                .filter(|path| !compiled.iter().any(|glob| glob.matches(path)))
                .cloned()
                .collect();
            match outside.is_empty() {
                true => {
                    json!({ "ok": true, "evidence": format!("{} changed file(s), all within {}", diff.changed.len(), json!(globs)), "changed": diff.changed })
                }
                false => {
                    json!({ "ok": false, "evidence": format!("changed outside {}: {}", json!(globs), listed(&outside)), "outside": outside })
                }
            }
        }
        HostCheck::NoNewDependencies => {
            let Some(diff) = place.diff else {
                return no_diff();
            };
            let touched: Vec<String> = diff.changed.iter().filter(|p| is_dependency_file(p)).cloned().collect();
            match touched.is_empty() {
                true => json!({ "ok": true, "evidence": "no dependency manifest or lockfile changed" }),
                false => {
                    json!({ "ok": false, "evidence": format!("dependency files changed: {}", listed(&touched)), "files": touched })
                }
            }
        }
        HostCheck::FilesExist(paths) => {
            let root = place
                .cwd
                .map(Path::to_path_buf)
                .or_else(|| std::env::current_dir().ok());
            let missing: Vec<String> = paths
                .iter()
                .filter(|path| !root.as_ref().is_some_and(|root| root.join(path).exists()))
                .cloned()
                .collect();
            match missing.is_empty() {
                true => json!({ "ok": true, "evidence": format!("{} file(s) exist", paths.len()) }),
                false => {
                    json!({ "ok": false, "evidence": format!("missing: {}", listed(&missing)), "missing": missing })
                }
            }
        }
    }
}

/// What a retried child's brief gains: the failed checks of its previous attempt.
pub fn retry_note(report: &Value, attempt: u32) -> String {
    let failed: Vec<String> = report["failed"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|f| format!("- {f}"))
                .collect()
        })
        .unwrap_or_default();
    format!(
        "[Contract] Attempt {attempt} did not pass the host's contract checks:\n{}\nFix these in your working directory, then call rlm.finish again; the host re-runs every check when you end.",
        failed.join("\n")
    )
}

/// A reviewer child's brief: the review brief, the child's result and its failed checks.
pub fn review_brief(brief: &str, name: &str, id: &str, result: &Value, report: &Value) -> String {
    let summary = result["verdict"]["summary"]
        .as_str()
        .map(String::from)
        .or_else(|| result["value"].as_str().map(String::from))
        .unwrap_or_default();
    let failed: Vec<String> = report["failed"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|f| format!("- {f}"))
                .collect()
        })
        .unwrap_or_default();
    let changed = result["verdict"]["changed_files"]
        .as_array()
        .map(|files| {
            files
                .iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    format!(
        "{brief}\n\n[Contract review] Subagent \"{name}\" ({id}) ended; its contract failed the host's checks:\n{}\nIts summary: {}\nFiles it declared changed: {}",
        failed.join("\n"),
        tail(&summary, OUTPUT_CHARS),
        if changed.is_empty() {
            "none".to_string()
        } else {
            listed(&changed)
        }
    )
}
