//! `ultron usage [session-id|path] [--last N] [--json]` (`ultron/usage-cli.ts`): the session report of a session
//! file, or a table of the most recent sessions. It only reads session files: no model call, no server, nothing
//! written.

pub mod log;
pub mod report;
pub mod stats;
pub mod text;

use crate::Env;
use crate::localtime::Zone;
use log::{LogError, mtime_ms, read_session_header, read_session_log};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const SESSION_REPORT_LIST_SCHEMA: &str = "ultron.session-report-list/1";

fn usage() -> String {
    [
        "Usage: ultron usage [session-id | session.jsonl] [options]",
        "",
        "What a session did: turns, cells, depth (frames, sub-agents, workflows), tokens and cost per lane and model,",
        "guardrails and memory decisions. Reads session files only: no model call, no server, nothing written.",
        "",
        "With no session, reports the most recent session of the current directory.",
        "",
        "Options:",
        "  --last <N>              A table of the N most recent sessions, across all directories",
        "  --here                  With --last: only sessions of the current directory",
        "  --all                   With --last: include sessions in which nothing ran",
        "  --json                  The report as JSON (schema ultron.session-report/1; with --last, a list)",
        "  --utc                   Print times in UTC",
        "  --sessions-root <dir>   Where sessions are kept (default: the profile's sessions directory)",
    ]
    .join("\n")
}

#[derive(Debug, Default)]
struct Options {
    target: Option<String>,
    last: Option<u32>,
    here: bool,
    all: bool,
    json: bool,
    utc: bool,
    sessions_root: Option<String>,
}

/// A usage error (exit 2, usage printed) or a failure (exit 1).
enum Failure {
    Usage(String),
    Error(String),
}

/// `Number(text)` for the `--last` value: a safe integer or nothing.
fn js_integer(text: &str) -> Option<f64> {
    let t = crate::js::trim(text);
    if t.is_empty() {
        return Some(0.0);
    }
    let value: f64 = if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()? as f64
    } else if t.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) {
        t.parse().ok()?
    } else {
        return None;
    };
    (value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0).then_some(value)
}

fn parse(args: &[String]) -> Result<Options, Failure> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let mut value = |name: &str| -> Result<String, Failure> {
            let inline = match arg.find('=') {
                Some(at) => Some(arg[at + 1..].to_string()),
                None => {
                    index += 1;
                    args.get(index).cloned()
                }
            };
            match inline {
                Some(v) if !v.is_empty() => Ok(v),
                _ => Err(Failure::Usage(format!("Option {name} requires a value"))),
            }
        };
        if arg == "--json" {
            options.json = true;
        } else if arg == "--here" {
            options.here = true;
        } else if arg == "--all" {
            options.all = true;
        } else if arg == "--utc" {
            options.utc = true;
        } else if arg == "--last" || arg.starts_with("--last=") {
            let count = js_integer(&value("--last")?);
            match count {
                Some(n) if (1.0..=1000.0).contains(&n) => options.last = Some(n as u32),
                _ => return Err(Failure::Usage("--last needs a number between 1 and 1000".into())),
            }
        } else if arg == "--sessions-root" || arg.starts_with("--sessions-root=") {
            options.sessions_root = Some(value("--sessions-root")?);
        } else if arg.starts_with('-') {
            return Err(Failure::Usage(format!("Unknown option {arg}")));
        } else if options.target.is_some() {
            return Err(Failure::Usage(format!("Unexpected argument: {arg}")));
        } else {
            options.target = Some(arg.clone());
        }
        index += 1;
    }
    if options.target.is_some() && options.last.is_some() {
        return Err(Failure::Usage("Give a session or --last, not both".into()));
    }
    if (options.here || options.all) && options.last.is_none() {
        return Err(Failure::Usage("--here and --all go with --last".into()));
    }
    Ok(options)
}

struct SessionFile {
    path: PathBuf,
    modified_at: f64,
}

/// Every session file under the sessions root (one directory per working directory), newest first.
fn list_session_files(root: &Path) -> Vec<SessionFile> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return files;
    };
    let mut directories: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    directories.sort();
    for directory in directories {
        let Ok(names) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in names.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".jsonl") {
                continue;
            }
            let path = directory.join(&name);
            if let Ok(meta) = std::fs::metadata(&path)
                && meta.is_file()
            {
                files.push(SessionFile {
                    modified_at: mtime_ms(&meta),
                    path,
                });
            }
        }
    }
    files.sort_by(|a, b| {
        b.modified_at
            .partial_cmp(&a.modified_at)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| crate::list_models::locale_compare(&a.path.to_string_lossy(), &b.path.to_string_lossy()))
    });
    files
}

/// The session id in a session file's name (`<created>_<id>.jsonl`).
fn file_session_id(path: &Path) -> String {
    let text = path.to_string_lossy();
    let name = text.rsplit('/').next().unwrap_or(&text);
    let name = name.strip_suffix(".jsonl").unwrap_or(name);
    name.split_once('_').map_or(name, |(_, id)| id).to_string()
}

/// The Ultron session of a Claude Code session (`ultron claude` remembers it by Claude Code's session id).
fn claude_session_path(agent_dir: &Path, id: &str) -> Option<PathBuf> {
    let safe: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = agent_dir
        .join("claude-code")
        .join("sessions")
        .join(format!("{safe}.json"));
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let target = PathBuf::from(saved.get("path")?.as_str()?);
    target.exists().then_some(target)
}

fn resolve_target(target: &str, root: &Path, cwd: &Path, agent_dir: &Path) -> Result<PathBuf, String> {
    let as_path = config::paths::normalize(&cwd.join(target));
    if std::fs::metadata(&as_path).is_ok_and(|m| m.is_file()) {
        return Ok(as_path);
    }
    let files = list_session_files(root);
    let exact: Vec<&SessionFile> = files.iter().filter(|f| file_session_id(&f.path) == target).collect();
    let matches = if exact.is_empty() {
        files
            .iter()
            .filter(|f| file_session_id(&f.path).starts_with(target))
            .collect()
    } else {
        exact
    };
    if matches.len() == 1 {
        return Ok(matches[0].path.clone());
    }
    if matches.len() > 1 {
        let shown: Vec<String> = matches.iter().take(5).map(|f| file_session_id(&f.path)).collect();
        return Err(format!(
            "{} sessions match \"{target}\": {}{}",
            matches.len(),
            shown.join(", "),
            if matches.len() > 5 { ", ..." } else { "" }
        ));
    }
    if let Some(path) = claude_session_path(agent_dir, target) {
        return Ok(path);
    }
    Err(format!(
        "No session file or session id \"{target}\" (looked in {})",
        root.display()
    ))
}

/// A session's report, with the costs of `subscription` providers (a subscription login) as notional.
fn report_of(path: &Path, subscription: &[String]) -> Result<(Value, bool), LogError> {
    let log = read_session_log(path)?;
    Ok((
        report::build_session_report_with(&log, subscription),
        report::session_is_empty(&log),
    ))
}

/// Handle `usage ...`; returns None when `args` is not a usage command, else the exit code.
pub fn run(args: &[String], env: &Env, out: &mut dyn std::io::Write, err: &mut dyn std::io::Write) -> Option<i32> {
    if args.first().map(String::as_str) != Some("usage") {
        return None;
    }
    let rest = &args[1..];
    let mut lines: Vec<String> = Vec::new();
    let code = if rest.iter().any(|a| a == "--help" || a == "-h") {
        lines.push(usage());
        0
    } else {
        match command(rest, env, &mut lines) {
            Ok(()) => 0,
            Err(Failure::Usage(message)) => {
                let _ = writeln!(err, "Error: {message}\n\n{}", usage());
                2
            }
            Err(Failure::Error(message)) => {
                let _ = writeln!(err, "Error: {message}");
                1
            }
        }
    };
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
    Some(code)
}

fn command(rest: &[String], env: &Env, lines: &mut Vec<String>) -> Result<(), Failure> {
    let options = parse(rest)?;
    let agent_dir = env.agent_dir();
    let default_root = agent_dir.join("experimental").join("sessions").display().to_string();
    let root_input = options
        .sessions_root
        .clone()
        .or_else(|| (env.var)(config::paths::ENV_SESSION_DIR))
        .unwrap_or(default_root);
    let root = config::paths::resolve(&root_input, &env.cwd, &env.home);
    let zone = Zone::from_env((env.var)("TZ").as_deref());
    let home = env.home.display().to_string();
    let text = text::TextOptions {
        utc: options.utc,
        home: &home,
        zone: &zone,
    };
    let cwd = env.cwd.display().to_string();
    let error = |e: LogError| Failure::Error(e.message().to_string());
    // Costs of providers this runtime reaches through a subscription login are notional, as in `/usage`.
    let subscription = session_core::lane::subscription_providers(env);
    if let Some(last) = options.last {
        let mut reports = Vec::new();
        let mut skipped = 0;
        for file in list_session_files(&root) {
            if reports.len() >= last as usize {
                break;
            }
            let outcome = (|| -> Result<Option<(Value, bool)>, LogError> {
                if options.here && read_session_header(&file.path)?.cwd != cwd {
                    return Ok(None);
                }
                report_of(&file.path, &subscription).map(Some)
            })();
            match outcome {
                Ok(None) | Err(LogError::Session(_)) => {}
                Ok(Some((report, empty))) => {
                    if empty && !options.all {
                        skipped += 1;
                    } else {
                        reports.push(report);
                    }
                }
                Err(e) => return Err(error(e)),
            }
        }
        if options.json {
            lines.push(config::json::stringify_pretty(&json!({
                "schema": SESSION_REPORT_LIST_SCHEMA,
                "sessions": reports,
            })));
        } else if reports.is_empty() {
            lines.push(format!(
                "No sessions{} under {}{}",
                if options.here {
                    format!(" of {cwd}")
                } else {
                    String::new()
                },
                root.display(),
                if skipped > 0 {
                    format!(" ({skipped} with nothing run; --all lists them)")
                } else {
                    String::new()
                }
            ));
        } else {
            lines.extend(text::render_session_table(&reports, &text));
            if skipped > 0 {
                lines.push(format!(
                    "({skipped} newer session{} in which nothing ran not listed; --all lists them)",
                    if skipped == 1 { "" } else { "s" }
                ));
            }
        }
        return Ok(());
    }
    let path = match &options.target {
        Some(target) => resolve_target(target, &root, &env.cwd, &agent_dir).map_err(Failure::Error)?,
        None => {
            let mut found = None;
            let mut fallback = None;
            for file in list_session_files(&root) {
                match read_session_header(&file.path) {
                    Ok(header) if header.cwd == cwd => {}
                    _ => continue,
                }
                fallback.get_or_insert_with(|| file.path.clone());
                if !report::session_is_empty(&read_session_log(&file.path).map_err(error)?) {
                    found = Some(file.path);
                    break;
                }
            }
            found.or(fallback).ok_or_else(|| {
                Failure::Error(format!(
                    "No session of {cwd} under {}; name one, or see `ultron usage --last 10`",
                    root.display()
                ))
            })?
        }
    };
    let (report, _) = report_of(&path, &subscription).map_err(error)?;
    if options.json {
        lines.push(config::json::stringify_pretty(&report));
    } else {
        lines.extend(text::render_session_report(&report, &text));
    }
    Ok(())
}
