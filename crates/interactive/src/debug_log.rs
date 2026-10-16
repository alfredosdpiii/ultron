//! Pi's `/debug` (client-tui-pi-commands.ts `writeDebugLog`): the rendered TUI lines and the session's messages,
//! plus the client, server and session worker state (pids, directories, kernel pool, versions), as the text written
//! to `<agentDir>/ultron-debug.log` (never credentials), and the notice that says where it went. The app captures
//! what it shows ([`DebugCapture`]); the client adds the rest and writes the file off the UI thread.

use std::rc::Rc;

use serde_json::Value;
use tui::components::{Component, Text};
use tui::theme::Theme;
use tui::utils::visible_width;

use crate::components::fg;

/// The log's file name in the agent directory (`basename(getDebugLogPath())`).
pub const DEBUG_LOG_NAME: &str = "ultron-debug.log";

/// What the TUI shows when `/debug` runs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DebugCapture {
    pub width: usize,
    pub height: usize,
    /// The rendered lines at the terminal width.
    pub lines: Vec<String>,
    /// The transcript's messages (Pi JSON), oldest first.
    pub messages: Vec<Value>,
    pub session_id: Option<String>,
}

/// The client's side of the log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientFacts {
    /// `new Date().toISOString()`.
    pub time: String,
    /// `<app> <version> · <runtime> · <platform> · pid <pid>`.
    pub process: String,
    pub cwd: String,
    pub server_id: Option<String>,
    /// `unix` or `radius`.
    pub transport: Option<String>,
    pub socket_path: Option<String>,
}

fn or<'a>(value: Option<&'a str>, fallback: &'a str) -> &'a str {
    value.unwrap_or(fallback)
}

/// `dirname` of a socket path.
fn directory(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => path[..i].into(),
        None => ".".into(),
    }
}

/// The log text and the notice's summary lines; `worker` is the worker's `debugInfo`, or why it is missing.
pub fn debug_log(
    capture: &DebugCapture,
    client: &ClientFacts,
    worker: &Result<Value, String>,
) -> (String, Vec<String>) {
    let server_dir = client.socket_path.as_deref().map(directory);
    let worker_value = match worker {
        Ok(info) => info.clone(),
        Err(error) => serde_json::json!({"error": error}),
    };
    let session = or(capture.session_id.as_deref(), "(none)");
    let summary = vec![
        format!("Session: {session}"),
        format!(
            "Server: {} ({}){}",
            or(client.server_id.as_deref(), "(none)"),
            or(client.transport.as_deref(), "unknown"),
            server_dir.as_deref().map(|d| format!(" in {d}")).unwrap_or_default()
        ),
        match worker {
            Err(error) => format!("Worker: {error}"),
            Ok(info) => format!(
                "Worker: pid {} · {} · kernels {}",
                info["pid"],
                info["model"].as_str().unwrap_or("no model"),
                info["kernelPool"]
            ),
        },
    ];
    let mut data = vec![
        format!("Debug output at {}", client.time),
        format!("Terminal: {}x{}", capture.width, capture.height),
        format!("Total lines: {}", capture.lines.len()),
        String::new(),
        "=== Client ===".into(),
        client.process.clone(),
        format!("cwd: {}", client.cwd),
        format!("Session: {session}"),
        format!("Server id: {}", or(client.server_id.as_deref(), "(none)")),
        format!("Server transport: {}", or(client.transport.as_deref(), "unknown")),
        format!("Server socket: {}", or(client.socket_path.as_deref(), "(n/a)")),
        format!("Server directory: {}", or(server_dir.as_deref(), "(n/a)")),
        String::new(),
        "=== Session worker ===".into(),
        serde_json::to_string_pretty(&worker_value).unwrap_or_default(),
        String::new(),
        "=== All rendered lines with visible widths ===".into(),
    ];
    data.extend(capture.lines.iter().enumerate().map(|(i, line)| {
        format!(
            "[{i}] (w={}) {}",
            visible_width(line),
            serde_json::to_string(line).unwrap_or_default()
        )
    }));
    data.push(String::new());
    data.push("=== Agent messages (JSONL) ===".into());
    data.extend(capture.messages.iter().map(|m| m.to_string()));
    data.push(String::new());
    (data.join("\n"), summary)
}

/// The notice after the log was written: "✓ Debug log written", its path, and the summary.
pub fn notice(theme: &Rc<Theme>, path: &str, summary: &[String], width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    let written = format!(
        "{}\n{}",
        fg(theme, "accent", "✓ Debug log written"),
        fg(theme, "muted", path)
    );
    lines.extend(Text::new(&written, 1, 0, None).render(width));
    lines.extend(Text::new(&fg(theme, "dim", &summary.join("\n")), 1, 0, None).render(width));
    lines
}

/// [`notice`] as a transcript component.
pub struct DebugNotice {
    pub theme: Rc<Theme>,
    pub path: String,
    pub summary: Vec<String>,
}

impl Component for DebugNotice {
    fn render(&mut self, width: usize) -> Vec<String> {
        notice(&self.theme, &self.path, &self.summary, width)
    }
}
