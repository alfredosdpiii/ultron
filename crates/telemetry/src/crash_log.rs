//! The crash log ported from `packages/coding-agent/src/core/crash-log.ts`, as the bug report uses it: reading and
//! clearing `<agent dir>/crashes.json` (written by the TypeScript interactive mode; the native worker path records
//! no crashes), and the loaded-extension record it describes.

use serde::Deserialize;
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

/// One crash record. Fields are kept as read (unknown ones included) so rewrites preserve them.
#[derive(Debug, Clone, PartialEq)]
pub struct CrashRecord(pub Map<String, Value>);

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
