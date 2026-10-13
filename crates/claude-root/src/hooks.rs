//! `ultron hook <event>` (hooks.ts): Claude Code hook commands for an `ultron mcp` session. Claude Code runs them
//! with the hook input as JSON on stdin; each forwards it to its session's server over the control socket and
//! prints the hook output the server returns (JSON on stdout, exit 0). Failures are quiet: the hook prints nothing
//! and exits 0, so a missing server never blocks a prompt.

use crate::control::{ControlClient, ServerQuery, control_dir, find_server};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Hook commands and the Claude Code events they serve.
pub const HOOK_EVENTS: [(&str, &str); 3] = [
    ("session-start", "SessionStart"),
    ("user-prompt", "UserPromptSubmit"),
    ("stop", "Stop"),
];

/// A SessionStart hook can run while Claude Code is still starting the server; later hooks find it running.
const SESSION_START_WAIT: Duration = Duration::from_secs(10);
const CONNECT_WAIT: Duration = Duration::from_millis(1_500);
/// Recall can take up to its own 20 s timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(50);

pub fn is_hook_event(event: &str) -> bool {
    HOOK_EVENTS.iter().any(|(name, _)| *name == event)
}

/// Run one hook; returns the text to print (may be empty). Never fails.
pub fn run_hook(event: &str, socket: Option<PathBuf>, input: &str, var: &dyn Fn(&str) -> Option<String>) -> String {
    if !is_hook_event(event) {
        return String::new();
    }
    run_hook_inner(event, socket, input, var).unwrap_or_default()
}

fn run_hook_inner(
    event: &str,
    mut socket: Option<PathBuf>,
    raw: &str,
    var: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let input: Map<String, Value> = if raw.trim().is_empty() {
        Map::new()
    } else {
        match serde_json::from_str::<Value>(raw).ok()? {
            Value::Object(input) => input,
            _ => Map::new(),
        }
    };
    let claude_session_id = input
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| var("CLAUDE_CODE_SESSION_ID"));
    let deadline = Instant::now()
        + if event == "session-start" {
            SESSION_START_WAIT
        } else {
            CONNECT_WAIT
        };
    if socket.is_none() {
        let dir = control_dir(var).ok()?;
        let query = ServerQuery {
            claude_session_id,
            cwd: input.get("cwd").and_then(Value::as_str).map(str::to_string),
            name: None,
        };
        loop {
            socket = find_server(&query, &dir).map(|s| PathBuf::from(s.socket));
            if socket.is_some() || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    let socket = socket?;
    let mut client = ControlClient::connect(&socket, deadline.saturating_duration_since(Instant::now())).ok()?;
    let mut fields = Map::new();
    fields.insert("event".into(), event.into());
    fields.insert("input".into(), Value::Object(input));
    let result = client.request("hook", fields, REQUEST_TIMEOUT).ok()?;
    match &result {
        Value::Object(map) if !map.is_empty() => Some(result.to_string()),
        _ => None,
    }
}

/// `ultron hook <event> [--socket path]`; returns the exit code.
pub fn run(
    args: &[String],
    stdin: &mut dyn std::io::Read,
    stdin_is_tty: bool,
    var: &dyn Fn(&str) -> Option<String>,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> i32 {
    let event = args.first().map(String::as_str).unwrap_or("");
    let mut socket = None;
    let mut index = 1;
    while index < args.len() {
        if args[index] == "--socket" && index + 1 < args.len() {
            socket = Some(PathBuf::from(&args[index + 1]));
            index += 2;
        } else {
            let _ = writeln!(err, "ultron hook: unknown option {}", args[index]);
            return 2;
        }
    }
    if !is_hook_event(event) {
        let names: Vec<&str> = HOOK_EVENTS.iter().map(|(name, _)| *name).collect();
        let shown = if event.is_empty() { "(none)" } else { event };
        let _ = writeln!(err, "ultron hook: unknown event {shown}; one of {}", names.join(", "));
        return 2;
    }
    let mut raw = String::new();
    if !stdin_is_tty {
        let mut bytes = Vec::new();
        let _ = stdin.read_to_end(&mut bytes);
        raw = String::from_utf8_lossy(&bytes).into_owned();
    }
    let output = run_hook(event, socket, &raw, var);
    if !output.is_empty() {
        let _ = writeln!(out, "{output}");
    }
    0
}
