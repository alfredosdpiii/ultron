//! `ULTRON_LOKI_LOG` (loki.ts `lokiLog`): every file-hook guard outcome appended to a file as a JSON line
//! `{"at": <ms since the epoch>, "guard", "phase", "outcome", "ms", "paths", "lane", "detail"?}`. Unset or blank
//! writes nothing; a relative path is resolved against the process directory; write errors are ignored.

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

/// The appender for `value` (the variable's text); None when unset or blank.
pub fn loki_log(value: Option<&str>) -> Option<OnRecord> {
    let path = value.map(str::trim).filter(|path| !path.is_empty())?;
    let target: PathBuf = std::path::absolute(Path::new(path)).unwrap_or_else(|_| PathBuf::from(path));
    let lock = Arc::new(Mutex::new(()));
    Some(Arc::new(move |record: &GuardRecord| {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let line = log_line(at, record);
        let _held = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&target) {
            let _ = file.write_all(line.as_bytes());
        }
    }))
}
