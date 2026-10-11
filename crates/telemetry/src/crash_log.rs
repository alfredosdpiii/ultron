//! The crash log ported from `packages/coding-agent/src/core/crash-log.ts`: the newest five crashes in
//! `<agent dir>/crashes.json`, announced once within seven days, and attribution of a stack trace to loaded extensions.
//! Every operation is best effort: a process that is already crashing never fails because of its crash log.

use crate::bug_report::pretty;
use crate::js;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_CRASH_RECORDS: usize = 5;
const MAX_AGE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// One crash record. Fields are kept as read (unknown ones included) so rewrites preserve them.
#[derive(Debug, Clone, PartialEq)]
pub struct CrashRecord(pub Map<String, Value>);

impl CrashRecord {
    pub fn timestamp(&self) -> &str {
        self.0.get("timestamp").and_then(Value::as_str).unwrap_or_default()
    }

    pub fn message(&self) -> &str {
        self.0.get("message").and_then(Value::as_str).unwrap_or_default()
    }

    pub fn notified(&self) -> bool {
        js::truthy(self.0.get("notified"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashKind {
    UncaughtException,
    FatalError,
}

impl CrashKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::UncaughtException => "uncaught_exception",
            Self::FatalError => "fatal_error",
        }
    }
}

/// What was thrown: an `Error` or any other value, already converted with `String(value)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrashError {
    Error {
        name: String,
        message: String,
        stack: Option<String>,
    },
    Value(String),
}

#[derive(Debug, Clone)]
pub struct Crash<'a> {
    pub kind: CrashKind,
    pub error: &'a CrashError,
    pub session_file: Option<&'a str>,
    pub cwd: &'a str,
}

/// `<agent dir>/crashes.json`.
pub fn crash_log_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("crashes.json")
}

/// `readCrashLog`: records with a string `timestamp` and `message`; a missing or unreadable file is empty.
pub fn read_crash_log(path: &Path) -> Vec<CrashRecord> {
    let Ok(bytes) = fs::read(path) else { return Vec::new() };
    let Ok(Value::Array(records)) = serde_json::from_str::<Value>(&String::from_utf8_lossy(&bytes)) else {
        return Vec::new();
    };
    records
        .into_iter()
        .filter_map(|record| match record {
            Value::Object(fields)
                if fields.get("timestamp").is_some_and(Value::is_string)
                    && fields.get("message").is_some_and(Value::is_string) =>
            {
                Some(CrashRecord(fields))
            }
            _ => None,
        })
        .collect()
}

fn write_crash_log(records: &[CrashRecord], path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let values: Vec<&Map<String, Value>> = records.iter().map(|r| &r.0).collect();
    fs::write(path, format!("{}\n", pretty(&values)))
}

/// `recordCrash`: appends a record (keeping the newest five) and returns it, or `None` if it could not be written.
pub fn record_crash(crash: &Crash<'_>, version: &str, now_ms: i64, path: &Path) -> Option<CrashRecord> {
    let (message, stack) = match crash.error {
        CrashError::Error { name, message, stack } => (
            if message.is_empty() {
                name.clone()
            } else {
                message.clone()
            },
            stack.clone().filter(|s| !s.is_empty()),
        ),
        CrashError::Value(value) => (value.clone(), None),
    };
    let Value::Object(fields) = json!({
        "timestamp": js::iso_timestamp(now_ms),
        "version": version,
        "kind": crash.kind.as_str(),
        "message": message,
        "stack": stack,
        "sessionFile": crash.session_file,
        "cwd": crash.cwd,
    }) else {
        return None;
    };
    let record = CrashRecord(fields);
    let mut records = read_crash_log(path);
    records.push(record.clone());
    let keep = records.len().saturating_sub(MAX_CRASH_RECORDS);
    write_crash_log(&records[keep..], path).ok()?;
    Some(record)
}

/// `takeUnnotifiedCrash`: the newest unannounced crash from the last seven days; marks every record announced.
pub fn take_unnotified_crash(path: &Path, now_ms: i64) -> Option<CrashRecord> {
    let records = read_crash_log(path);
    let crash = records
        .iter()
        .rev()
        .find(|record| {
            !record.notified() && js::parse_iso_millis(record.timestamp()).is_some_and(|at| now_ms - at <= MAX_AGE_MS)
        })?
        .clone();
    let marked: Vec<CrashRecord> = records
        .into_iter()
        .map(|mut record| {
            if !record.notified() {
                record.0.insert("notified".into(), Value::Bool(true));
            }
            record
        })
        .collect();
    // Showing the notice again is harmless.
    let _ = write_crash_log(&marked, path);
    Some(crash)
}

/// `clearCrashLog`.
pub fn clear_crash_log(path: &Path) {
    let _ = fs::remove_file(path);
}

/// The parts of a loaded extension the crash log and bug report read.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Extension {
    pub path: String,
    pub resolved_path: String,
    pub source_info: SourceInfo,
    #[serde(default)]
    pub hidden: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub source: String,
    pub scope: String,
    pub origin: String,
    #[serde(default)]
    pub base_dir: Option<String>,
}

fn normalize_stack_path(value: &str) -> String {
    value.replace('\\', "/").trim_end_matches('/').to_owned()
}

/// `/^[a-z]:\//iu`; with the `u` flag, `ſ` and the Kelvin sign fold into the class.
fn is_drive_path(target: &str) -> bool {
    let mut chars = target.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '\u{17F}' || c == '\u{212A}')
        && chars.next() == Some(':')
        && chars.next() == Some('/')
}

fn stack_contains_path(stack: &str, target_path: &str, include_descendants: bool) -> bool {
    let target = normalize_stack_path(target_path);
    if target.is_empty() || target.starts_with('<') {
        return false;
    }
    let case_insensitive = is_drive_path(&target);
    let (haystack, needle) = if case_insensitive {
        (stack.to_lowercase(), target.to_lowercase())
    } else {
        (stack.to_owned(), target)
    };
    if include_descendants {
        return haystack.contains(&format!("{needle}/"));
    }
    let mut from = 0;
    while let Some(found) = haystack[from..].find(&needle) {
        let end = from + found + needle.len();
        match haystack[end..].chars().next() {
            None | Some(':' | ')') => return true,
            Some(c) if js::is_space(c) => return true,
            _ => {}
        }
        from = end;
    }
    false
}

fn is_stack_frame(line: &str) -> bool {
    let rest = line.trim_start_matches(js::is_space);
    rest.len() < line.len()
        && rest
            .strip_prefix("at")
            .and_then(|r| r.chars().next())
            .is_some_and(js::is_space)
}

fn has_script_extension(path: &str) -> bool {
    let Some(dot) = path.rfind('.') else { return false };
    matches!(&path[dot + 1..], "js" | "ts" | "cjs" | "cts" | "mjs" | "mts")
}

/// `findExtensionStackMatches`: labels of loaded extensions with source files in the stack's frames (the first line,
/// the error message, is ignored). Package extensions match anything below their root and are labelled by source.
pub fn find_extension_stack_matches(stack: Option<&str>, extensions: &[Extension]) -> Vec<String> {
    let Some(stack) = stack.filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    let normalized = stack
        .split('\n')
        .skip(1)
        .filter(|line| is_stack_frame(line))
        .map(|line| js::decode_uri(line).unwrap_or_else(|| line.to_owned()))
        .collect::<Vec<_>>()
        .join("\n")
        .replace('\\', "/");
    let mut matches = Vec::new();
    let mut seen = HashSet::new();
    for extension in extensions {
        let info = &extension.source_info;
        let resolved = normalize_stack_path(&extension.resolved_path);
        let is_package = info.origin == "package";
        let remote = ["npm:", "git:", "http://", "https://", "ssh://"]
            .iter()
            .any(|prefix| info.source.starts_with(prefix));
        let single_file_package = is_package && !remote && has_script_extension(&info.source);
        let package_root = info
            .base_dir
            .as_deref()
            .filter(|dir| is_package && !single_file_package && !dir.is_empty());
        let slash = resolved.rfind('/');
        let directory_entry = slash.is_some_and(|at| {
            resolved[at + 1..]
                .strip_prefix("index.")
                .is_some_and(|ext| matches!(ext, "js" | "ts" | "cjs" | "cts" | "mjs" | "mts"))
        });
        let matched = match (package_root, slash) {
            (Some(root), _) => stack_contains_path(&normalized, root, true),
            (None, Some(at)) if directory_entry => stack_contains_path(&normalized, &resolved[..at], true),
            _ => stack_contains_path(&normalized, &resolved, false),
        };
        if !matched {
            continue;
        }
        let label = if is_package && !info.source.is_empty() {
            info.source.clone()
        } else {
            extension.path.clone()
        };
        if seen.insert(label.clone()) {
            matches.push(label);
        }
    }
    matches
}
