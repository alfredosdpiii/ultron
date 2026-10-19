//! Internal processes (experimental/process.ts): the `ultron` executable re-launched detached in a role.
//!
//! TypeScript names the role in the `__PI_INTERNAL_SPAWN` environment variable and deletes it at startup so
//! descendants never inherit it. Rust cannot remove a variable from its own environment without `unsafe`, so the
//! role is the first two arguments instead (`ultron __internal <role> ...`), which no descendant inherits.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;

pub use rustix::process::Signal;

/// The argument that marks an internal process invocation: `ultron __internal <role> ...`.
pub const INTERNAL_ARGUMENT: &str = "__internal";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Coordinator,
    Server,
    SessionWorker,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Role::Coordinator => "coordinator",
            Role::Server => "server",
            Role::SessionWorker => "session-worker",
        }
    }
}

/// The role an argument vector (without the program name) starts, and the role's own arguments.
pub fn role(argv: &[String]) -> Result<Option<(Role, &[String])>, String> {
    if argv.first().map(String::as_str) != Some(INTERNAL_ARGUMENT) {
        return Ok(None);
    }
    let role = parse_role(argv.get(1).map(String::as_str))?.ok_or("Internal process requires a role")?;
    Ok(Some((role, &argv[2..])))
}

fn parse_role(value: Option<&str>) -> Result<Option<Role>, String> {
    match value {
        None => Ok(None),
        Some("coordinator") => Ok(Some(Role::Coordinator)),
        Some("server") => Ok(Some(Role::Server)),
        Some("session-worker") => Ok(Some(Role::SessionWorker)),
        Some(other) => Err(format!("Unsupported internal process role: {other}")),
    }
}

static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();

/// The executable internal processes run; defaults to this process's own (`process.execPath`).
pub fn set_executable(path: PathBuf) {
    let _ = EXECUTABLE.set(path);
}

pub fn executable() -> PathBuf {
    EXECUTABLE
        .get()
        .cloned()
        .or_else(|| std::env::var_os("ULTRON_INTERNAL_EXE").map(PathBuf::from))
        .or_else(|| std::env::current_exe().ok())
        .unwrap_or_else(|| PathBuf::from("ultron"))
}

/// What cli/setup.ts `setupCli` puts in the CLI's environment (`PI_CODING_AGENT=true`, `AI_AGENT=pi`), so every
/// process the session starts (its bash commands, the Python kernel, MCP servers) inherits it. Rust
/// cannot set its own environment without `unsafe`, so the internal processes, which run every session, get it.
pub const AGENT_MARKERS: [(&str, &str); 2] = [("PI_CODING_AGENT", "true"), ("AI_AGENT", "pi")];

/// Spawn a detached internal process in its own process group; output is discarded unless
/// `ULTRON_DEBUG_INTERNAL=1`. It runs with [`AGENT_MARKERS`] (under `env`, which wins as `options.env` does).
pub fn spawn(role: Role, args: &[String], env: &[(String, String)]) -> std::io::Result<Child> {
    use std::os::unix::process::CommandExt;
    let debug = std::env::var("ULTRON_DEBUG_INTERNAL").as_deref() == Ok("1");
    let mut command = Command::new(executable());
    command
        .arg(INTERNAL_ARGUMENT)
        .arg(role.name())
        .args(args)
        .envs(AGENT_MARKERS)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::null())
        .stdout(if debug { Stdio::inherit() } else { Stdio::null() })
        .stderr(if debug { Stdio::inherit() } else { Stdio::null() })
        .process_group(0);
    let child = command.spawn()?;
    Ok(child)
}

/// Reap a child in the background so it never lingers as a zombie; returns its exit observer.
pub fn watch(mut child: Child) -> std::sync::mpsc::Receiver<Option<std::process::ExitStatus>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait().ok());
    });
    rx
}

pub fn kill(pid: u32, signal: rustix::process::Signal) -> bool {
    rustix::process::Pid::from_raw(pid as i32).is_some_and(|pid| rustix::process::kill_process(pid, signal).is_ok())
}

pub fn alive(pid: u32) -> bool {
    rustix::process::Pid::from_raw(pid as i32).is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
}

/// `traceStartup(label)` from this process (`ULTRON_STARTUP_TRACE`; nothing when unset).
pub fn trace_startup(label: &str) {
    session_core::startup_trace::trace_startup(std::env::var("ULTRON_STARTUP_TRACE").ok().as_deref(), label);
}
