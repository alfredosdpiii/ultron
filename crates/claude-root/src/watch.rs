//! `ultron watch` (watch.ts): the RLM view of a running `ultron mcp` session. Argument parsing, `--list` and finding
//! the session's socket are ported; drawing the frame needs the RLM dock and pane (experimental/rlm-graph.ts,
//! rlm-pane.ts, rlm-visualizer.ts), which the Rust TUI does not have yet, so after connecting the command says so.

use crate::control::{ControlClient, ServerQuery, control_dir, find_server, list_servers};
use std::path::{Path, PathBuf};
use std::time::Duration;

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
    ControlClient::connect(&socket, Duration::from_secs(5))?;
    Err("`ultron watch` frames (the RLM dock and pane) are not implemented in the Rust build yet".into())
}
