//! Native session discovery and selectors from packages/coding-agent/src/native-command.ts.

use crate::Env;
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug, Clone, PartialEq)]
pub struct NativeSession {
    pub id: String,
    pub created_at: f64,
    pub modified_at: f64,
    pub cwd: String,
    pub path: PathBuf,
}

/// `path.resolve(base, input)` without symlink resolution.
fn resolve(base: &Path, input: &str) -> PathBuf {
    config::paths::normalize(&base.join(input))
}

/// `resolveSessionDirectory`: `--session-dir`, else `ULTRON_CODING_AGENT_SESSION_DIR`, else the instance's store
/// (a project instance keeps its own; otherwise `<agent dir>/experimental/sessions`), resolved against the cwd.
pub fn session_directory(session_dir: Option<&str>, env: &Env) -> PathBuf {
    session_directory_of(session_dir, env, &crate::instance(env, false))
}

pub fn session_directory_of(session_dir: Option<&str>, env: &Env, instance: &daemon::instance::Instance) -> PathBuf {
    let dir = session_dir
        .map(str::to_string)
        .or_else(|| (env.var)("ULTRON_CODING_AGENT_SESSION_DIR"))
        .unwrap_or_else(|| instance.default_session_dir(&env.agent_dir()).display().to_string());
    resolve(&env.cwd, &dir)
}

/// The first line of a file (without its newline), with Node's error text when it cannot be opened.
pub fn read_first_line(path: &Path) -> Result<String, String> {
    let file = fs::File::open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("ENOENT: no such file or directory, open '{}'", path.display()),
        _ => e.to_string(),
    })?;
    let mut line = Vec::new();
    BufReader::new(file)
        .read_until(b'\n', &mut line)
        .map_err(|e| e.to_string())?;
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

fn header(path: &Path) -> Option<NativeSession> {
    let value: Value = serde_json::from_str(&read_first_line(path).ok()?).ok()?;
    if value.get("kind")?.as_str()? != "header" {
        return None;
    }
    let modified = fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?;
    Some(NativeSession {
        id: value.get("id")?.as_str()?.to_string(),
        created_at: value.get("createdAt")?.as_f64()?,
        modified_at: modified.as_secs_f64() * 1000.0,
        cwd: value.get("cwd")?.as_str()?.to_string(),
        path: path.to_path_buf(),
    })
}

/// `listNativeSessions`: `<dir>/<any>/<file>.jsonl` headers of sessions started in `cwd`, newest first.
pub fn list_native_sessions(session_dir: &Path, cwd: &Path) -> Result<Vec<NativeSession>, String> {
    list_sessions_of(session_dir, Some(cwd))
}

/// [`list_native_sessions`] of every directory: the sessions of an instance whose sessions are not per directory
/// (the durable instance's, crate::durable_client).
pub fn list_all_native_sessions(session_dir: &Path) -> Result<Vec<NativeSession>, String> {
    list_sessions_of(session_dir, None)
}

fn list_sessions_of(session_dir: &Path, cwd: Option<&Path>) -> Result<Vec<NativeSession>, String> {
    let entries = match fs::read_dir(session_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut sessions = Vec::new();
    for dir in entries.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())) {
        for file in fs::read_dir(dir.path()).map_err(|e| e.to_string())?.flatten() {
            let path = file.path();
            if file.file_type().is_ok_and(|t| t.is_file()) && file.file_name().to_string_lossy().ends_with(".jsonl") {
                sessions.extend(header(&path));
            }
        }
    }
    if let Some(cwd) = cwd {
        let cwd = cwd.display().to_string();
        sessions.retain(|s| s.cwd == cwd);
    }
    sessions.sort_by(|l, r| {
        r.modified_at
            .total_cmp(&l.modified_at)
            .then(r.created_at.total_cmp(&l.created_at))
            .then_with(|| crate::list_models::locale_compare(&l.id, &r.id))
    });
    Ok(sessions)
}

/// `sessionIdFromSelector`: a path (contains a separator or ends in `.jsonl`) names a session file whose header id is
/// taken; otherwise an exact id, else a unique id prefix, among `sessions`.
pub fn session_id_from_selector(selector: &str, sessions: &[NativeSession], cwd: &Path) -> Result<String, String> {
    if selector.contains('/') || selector.contains('\\') || selector.ends_with(".jsonl") {
        let path = resolve(cwd, selector);
        let line = read_first_line(&path)?;
        let invalid = || format!("Session file is not a valid ultron session: {}", path.display());
        let value: Value = serde_json::from_str(&line).map_err(|_| invalid())?;
        if value.get("kind").and_then(Value::as_str) != Some("header") {
            return Err(invalid());
        }
        return value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(invalid);
    }
    if let Some(exact) = sessions.iter().find(|s| s.id == selector) {
        return Ok(exact.id.clone());
    }
    match sessions
        .iter()
        .filter(|s| s.id.starts_with(selector))
        .collect::<Vec<_>>()[..]
    {
        [only] => Ok(only.id.clone()),
        _ => Err(format!("No native session found matching '{selector}'")),
    }
}
