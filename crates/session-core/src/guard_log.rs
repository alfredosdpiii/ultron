//! `ULTRON_LOKI_LOG` (loki.ts `lokiLog`): every file-hook guard outcome appended to a file as a JSON line
//! `{"at": <ms since the epoch>, "guard", "phase", "outcome", "ms", "paths", "lane", "detail"?}`, and the session's
//! Loki setup (`setupLoki`'s record) as `{"at", "phase": "setup", "outcome": "on", "mode", "engine", "reports",
//! "projectPython", "ms"}`. Unset or blank writes nothing; a relative path is resolved against the process directory;
//! write errors are ignored.

use host::file_hooks::{GuardRecord, OnRecord};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// One log line: `JSON.stringify({ at, ...record })`.
pub fn log_line(at: u64, record: &GuardRecord) -> String {
    let mut line = json!({ "at": at });
    if let (Value::Object(line), Value::Object(fields)) = (&mut line, record.to_json()) {
        line.extend(fields);
    }
    format!("{line}\n")
}

/// Where `value` (the variable's text) points; None when unset or blank.
fn target(value: Option<&str>) -> Option<PathBuf> {
    let path = value.map(str::trim).filter(|path| !path.is_empty())?;
    Some(std::path::absolute(Path::new(path)).unwrap_or_else(|_| PathBuf::from(path)))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Lines written by one process go out one at a time.
static APPENDING: Mutex<()> = Mutex::new(());

fn append(target: &Path, line: &str) {
    let _held = APPENDING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(target) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// The appender for `value` (the variable's text); None when unset or blank.
pub fn loki_log(value: Option<&str>) -> Option<OnRecord> {
    let target = target(value)?;
    Some(Arc::new(move |record: &GuardRecord| {
        append(&target, &log_line(now_ms(), record))
    }))
}

/// `setupLoki`'s record of a session whose Loki guard is on: the in-process engine (`bundled`: a repository's own
/// `.loki/loki.py` is never run, its `.loki/` policy is read), structured reports, the project interpreter named for
/// it, and how long setting it up took.
pub fn setup_record(mode: &str, project_python: Option<&str>, ms: u64) -> Value {
    json!({
        "phase": "setup",
        "outcome": "on",
        "mode": mode,
        "engine": "bundled",
        "reports": "json",
        "projectPython": project_python,
        "ms": ms,
    })
}

/// `findProjectPython` (loki::project): ULTRON_LOKI_PROJECT_PYTHON, else nothing when LOKI_PYTHON is set (left to
/// the engine), else the session PATH's `python`/`python3` when it is a virtual or conda environment's, else a
/// `.venv` or `venv` interpreter in `cwd`.
pub fn project_python(var: &dyn Fn(&str) -> Option<String>, cwd: &Path) -> Option<String> {
    loki::project::find_project_python(cwd, var)
}

/// The session's Loki setup, written to `ULTRON_LOKI_LOG` when the guard is on (loki.ts `options.record?.({phase:
/// "setup", ...})`).
pub fn loki_setup(config: &crate::compose::Config) {
    let started = std::time::Instant::now();
    let mode = match config.loki {
        host::loki_guard::LokiMode::Off => return,
        host::loki_guard::LokiMode::Advise => "advise",
        host::loki_guard::LokiMode::On => "on",
    };
    let Some(target) = target((config.var)("ULTRON_LOKI_LOG").as_deref()) else {
        return;
    };
    let python = project_python(&|key| (config.var)(key), &config.cwd);
    let record = setup_record(mode, python.as_deref(), started.elapsed().as_millis() as u64);
    let mut line = json!({ "at": now_ms() });
    if let (Value::Object(line), Value::Object(fields)) = (&mut line, record) {
        line.extend(fields);
    }
    append(&target, &format!("{line}\n"));
}
