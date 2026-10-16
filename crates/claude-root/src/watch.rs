//! `ultron watch` (watch.ts): the RLM view of a running `ultron mcp` session (Claude Code's TUI cannot draw it). It
//! polls the server's control socket for the read-only inspections the native TUI polls (`agents.status {graph:
//! true}`, `instances.list`, `rlm.pool`, `ctx.state`, `rlm.frames`) plus the root cells Claude Code ran, and draws
//! them with the TUI's RLM views: a header, the docked RLM panel, then the RLM pane. `--once` prints one frame.

use crate::control::{ControlClient, ServerQuery, control_dir, find_server, list_servers};
use interactive::rlm::RlmSnapshot;
use interactive::rlm::pane::RlmPane;
use interactive::rlm::visualizer::{Color, RlmClock, RlmStyle};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// `POLL_MS`.
const POLL: Duration = Duration::from_millis(500);
/// `RLM_FRAME_LIMIT`.
const RLM_FRAME_LIMIT: u64 = 20;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchArgs {
    pub socket: Option<String>,
    pub list: bool,
    pub once: bool,
    pub plain: bool,
    pub width: Option<u64>,
}

pub fn parse_watch_args(args: &[String]) -> Result<WatchArgs, String> {
    let mut parsed = WatchArgs::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--socket" => {
                index += 1;
                parsed.socket = Some(args.get(index).cloned().ok_or("--socket needs a path")?);
            }
            "--list" => parsed.list = true,
            "--once" => parsed.once = true,
            "--plain" => parsed.plain = true,
            "--width" => {
                index += 1;
                let value = args.get(index).map_or(f64::NAN, |v| {
                    session_file::js::to_number(&serde_json::Value::String(v.clone()))
                });
                if value.fract() != 0.0 || !(20.0..=9_007_199_254_740_991.0).contains(&value) {
                    return Err("--width must be an integer >= 20".into());
                }
                parsed.width = Some(value as u64);
            }
            other => return Err(format!("unknown option for ultron watch: {other}")),
        }
        index += 1;
    }
    Ok(parsed)
}

/// The `--list` lines: `[  child ]<name>\t<cwd>\tpid <pid>\t<socket>`.
pub fn list_lines(dir: &Path) -> String {
    list_servers(dir)
        .iter()
        .map(|s| {
            format!(
                "{}{}\t{}\tpid {}\t{}\n",
                if s.parent.is_some() { "  child " } else { "" },
                s.name,
                s.cwd,
                s.pid,
                s.socket
            )
        })
        .collect()
}

/// The session to watch: `--socket`, else the newest root server in the cwd, of this Claude Code session, or any.
pub fn resolve_socket(
    args: &WatchArgs,
    cwd: &str,
    var: &dyn Fn(&str) -> Option<String>,
    dir: &Path,
) -> Result<PathBuf, String> {
    if let Some(socket) = &args.socket {
        return Ok(PathBuf::from(socket));
    }
    let by_cwd = find_server(
        &ServerQuery {
            cwd: Some(cwd.to_string()),
            ..ServerQuery::default()
        },
        dir,
    );
    let by_session = || {
        var("CLAUDE_CODE_SESSION_ID").filter(|v| !v.is_empty()).and_then(|id| {
            find_server(
                &ServerQuery {
                    claude_session_id: Some(id),
                    ..ServerQuery::default()
                },
                dir,
            )
        })
    };
    by_cwd
        .or_else(by_session)
        .or_else(|| list_servers(dir).into_iter().find(|s| s.parent.is_none()))
        .map(|s| PathBuf::from(s.socket))
        .ok_or_else(|| {
            "no running `ultron mcp` session found; start one with `ultron claude`, or pass --socket <path>".into()
        })
}

/// `ultron watch ...`; returns the exit code.
pub fn run(
    args: &[String],
    cwd: &str,
    var: &dyn Fn(&str) -> Option<String>,
    out: &mut dyn std::io::Write,
) -> Result<i32, String> {
    let parsed = parse_watch_args(args)?;
    let dir = control_dir(var)?;
    if parsed.list {
        out.write_all(list_lines(&dir).as_bytes()).map_err(|e| e.to_string())?;
        return Ok(0);
    }
    let socket = resolve_socket(&parsed, cwd, var, &dir)?;
    let mut client = ControlClient::connect(&socket, Duration::from_secs(5))?;
    let mut clock = RlmClock::new();
    let keys = tui::keybindings::KeybindingsManager::new(
        tui::qol_keys::native_app_keybindings(tui::keybindings::Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    );
    let plain = parsed.plain || !std::io::IsTerminal::is_terminal(&std::io::stdout());
    let style = if plain {
        RlmStyle::plain()
    } else {
        RlmStyle::themed(std::rc::Rc::new(watch_theme(cwd, var)))
    };
    let (snapshot, status) = poll_snapshot(&mut client, &mut clock, epoch_ms());
    if parsed.once {
        let mut pane = RlmPane::new();
        let width = parsed.width.map_or_else(terminal_width, |w| w as usize);
        let lines = render_watch_frame(&mut pane, &keys, &snapshot, &status, width, 40, &style, false, 30);
        let text = lines.join("\n");
        let text = if plain { strip_ansi(&text) } else { text };
        out.write_all(format!("{text}\n").as_bytes())
            .map_err(|e| e.to_string())?;
        return Ok(0);
    }
    live(client, clock, keys, style, snapshot, status)?;
    Ok(0)
}

fn epoch_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_millis() as f64)
}

fn terminal_width() -> usize {
    match tui::terminal::size() {
        (0, _) => 100,
        (columns, _) => columns as usize,
    }
}

/// The theme the settings name (`initTheme(settings.getTheme())`): a built-in or `<agent dir>/themes/<name>.json`;
/// dark when it does not load.
fn watch_theme(cwd: &str, var: &dyn Fn(&str) -> Option<String>) -> tui::theme::Theme {
    let home = PathBuf::from(var("HOME").unwrap_or_default());
    let agent_dir = config::paths::agent_dir(var, &home);
    let settings = config::settings::SettingsManager::create(Path::new(cwd), &agent_dir, false);
    let mode = tui::theme::ColorMode::from_capabilities(&tui::image::detect_capabilities(&|k| var(k), false, false));
    let mut registry = tui::theme::registry::ThemeRegistry::new(PathBuf::new(), agent_dir.join("themes"), mode);
    registry.init_theme(settings.theme().as_deref(), "dark");
    registry.current().cloned().unwrap_or_else(|| {
        let json = tui::theme::builtin_theme_json("dark").expect("the dark theme");
        tui::theme::create_theme(&json, mode, None).expect("the dark theme resolves")
    })
}

/// `ANSI`: CSI and OSC sequences, removed from plain output.
pub fn strip_ansi(text: &str) -> String {
    let pattern = regex::Regex::new(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b\][^\x07]*\x07").expect("ansi pattern");
    pattern.replace_all(text, "").into_owned()
}

/// `pollSnapshot`: one poll of a session, everything a frame is drawn from (`now`: ms since the epoch). A failed
/// inspection leaves its part empty; a failed `agents.status` (or the server's own error) shows as the error.
pub fn poll_snapshot(client: &mut ControlClient, clock: &mut RlmClock, now: f64) -> (RlmSnapshot, Value) {
    let timeout = Duration::from_secs(10);
    let inspect = |client: &mut ControlClient, request: &str, payload: Value| {
        let mut fields = Map::new();
        fields.insert("request".into(), request.into());
        fields.insert("payload".into(), payload);
        client.request("inspect", fields, timeout)
    };
    let status = client
        .request("status", Map::new(), timeout)
        .unwrap_or_else(|_| json!({}));
    let agents = inspect(client, "agents.status", json!({"graph": true}));
    let instances = inspect(client, "instances.list", json!({}));
    let pool = inspect(client, "rlm.pool", json!({}));
    let context = inspect(client, "ctx.state", json!({}));
    let frames = inspect(client, "rlm.frames", json!({"limit": RLM_FRAME_LIMIT}));
    let records = client.request("cells", Map::new(), timeout).ok();
    let cells: Vec<Value> = records
        .as_ref()
        .and_then(|r| r["cells"].as_array())
        .into_iter()
        .flatten()
        .map(|cell| {
            let mut out = Map::new();
            out.insert("toolCallId".into(), json!(format!("cell-{}", cell["n"])));
            out.insert("code".into(), cell["code"].clone());
            out.insert("status".into(), cell["status"].clone());
            out.insert("startedAt".into(), cell["startedAt"].clone());
            for key in ["endedAt", "output"] {
                if !cell[key].is_null() {
                    out.insert(key.into(), cell[key].clone());
                }
            }
            Value::Object(out)
        })
        .collect();
    let error = match &agents {
        Err(error) => Some(error.clone()),
        Ok(_) => status["error"].as_str().map(str::to_string),
    };
    let wire = json!({
        "now": now,
        "agentsStatus": agents.unwrap_or(Value::Null),
        "instances": instances.unwrap_or(Value::Null),
        "pool": pool.unwrap_or(Value::Null),
        "ctx": context.unwrap_or(Value::Null),
        "frames": frames.unwrap_or(Value::Null),
        "rootCell": cells.last().cloned().unwrap_or(Value::Null),
        "cells": cells[cells.len().saturating_sub(12)..].to_vec(),
        "turn": cells.first().map_or(Value::Null, |c| json!({"startedAt": c["startedAt"]})),
        "error": error,
    });
    let mut snapshot = RlmSnapshot::from_wire(&wire);
    snapshot.timing = clock.timings(&snapshot.tasks, snapshot.usage.as_ref(), now);
    (snapshot, status)
}

/// A JSON number as JavaScript prints it in a template (integers without a fraction).
fn js_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// `headerLine`: "Ultron" and the server's status fields.
fn header_line(status: &Value, style: &RlmStyle) -> String {
    let text = |key: &str| status[key].as_str().filter(|s| !s.is_empty());
    let mut parts: Vec<String> = vec![text("cwd").unwrap_or("").to_string()];
    if let Some(id) = text("claudeSessionId") {
        parts.push(format!("claude {}", id.chars().take(8).collect::<String>()));
    }
    if status["ready"] == false {
        parts.push("starting".into());
    }
    if let Some(model) = text("frameModel") {
        parts.push(format!("frames {model}"));
    }
    if let Some(children) = text("children") {
        parts.push(format!("children {children}"));
    }
    if let Some(cells) = status["cells"].as_f64() {
        parts.push(format!("{} cells", js_number(cells)));
    }
    if let Some(pending) = status["pendingEvents"].as_f64().filter(|n| *n != 0.0) {
        parts.push(format!("{} events waiting", js_number(pending)));
    }
    parts.retain(|p| !p.is_empty());
    format!(
        "{} {}",
        style.bold(&style.fg(Color::Accent, "Ultron")),
        style.fg(Color::Muted, &parts.join(" · "))
    )
}

/// `renderWatchFrame`: header, the docked panel (at most 40% of `height`), a blank line, then the pane (the rest,
/// at least six rows). `pane_height` is the pane's own height, `focused` whether it takes keys.
#[allow(clippy::too_many_arguments)]
pub fn render_watch_frame(
    pane: &mut RlmPane,
    keys: &tui::keybindings::KeybindingsManager,
    snapshot: &RlmSnapshot,
    status: &Value,
    width: usize,
    height: usize,
    style: &RlmStyle,
    focused: bool,
    pane_height: usize,
) -> Vec<String> {
    let header = tui::utils::truncate_to_width(&header_line(status, style), width.max(1), "…", false);
    let options = interactive::rlm::graph::RlmViewOptions {
        style: style.clone(),
        spinner_frame: (snapshot.now / 100.0).floor() as i64,
        ..Default::default()
    };
    let max_rows = 6usize.max((height as f64 * 0.4).floor() as usize);
    let dock: Vec<String> =
        interactive::rlm::render_rlm_dock(snapshot, width.saturating_sub(2).max(1), &options, max_rows)
            .into_iter()
            .map(|line| format!(" {line}"))
            .collect();
    let rest = 6usize.max(height.saturating_sub(dock.len() + 2));
    let pane_lines = pane.render(keys, snapshot, width, pane_height, focused, None, style);
    let mut lines = vec![header];
    lines.extend(dock);
    lines.push(String::new());
    lines.extend(pane_lines.into_iter().take(rest));
    lines
}

/// The live view: the alternate screen, a poll every 500 ms, the pane's keys; `q` or Ctrl+C quits.
fn live(
    mut client: ControlClient,
    mut clock: RlmClock,
    keys: tui::keybindings::KeybindingsManager,
    style: RlmStyle,
    mut snapshot: RlmSnapshot,
    mut status: Value,
) -> Result<(), String> {
    let mut terminal = tui::terminal::Terminal::new(std::io::stdout());
    terminal.start(true).map_err(|e| e.to_string())?;
    let input = tui::terminal::spawn_stdin_pump();
    let mut screen = tui::screen::AltScreen::new();
    let mut pane = RlmPane::new();
    let result = (|| -> Result<(), String> {
        terminal.write(&screen.enter()).map_err(|e| e.to_string())?;
        let mut next_poll = std::time::Instant::now() + POLL;
        loop {
            let (columns, rows) = tui::terminal::size();
            let (width, height) = (columns.max(20) as usize, rows.max(10) as usize);
            let pane_height = 6usize.max(height.saturating_sub((height as f64 * 0.4).floor() as usize + 3));
            let lines = render_watch_frame(
                &mut pane,
                &keys,
                &snapshot,
                &status,
                width,
                height,
                &style,
                true,
                pane_height,
            );
            terminal
                .write(&screen.paint(&lines, width, height))
                .map_err(|e| e.to_string())?;
            let wait = next_poll.saturating_duration_since(std::time::Instant::now());
            match input.recv_timeout(wait) {
                Ok(data) if data == "q" || data == "\u{3}" => return Ok(()),
                Ok(data) => {
                    if pane.handle_input(&keys, &data, &snapshot, pane_height)
                        == interactive::rlm::pane::PaneEvent::Close
                    {
                        return Ok(());
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    (snapshot, status) = poll_snapshot(&mut client, &mut clock, epoch_ms());
                    next_poll = std::time::Instant::now() + POLL;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    })();
    let _ = terminal.write(&screen.exit(&[], 1));
    let _ = terminal.stop();
    result
}
