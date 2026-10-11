//! The session JSONL file: parsing, loading with repair, version migrations, header discovery,
//! and listing sessions in a directory.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::context::{entry_type, field};
use crate::env::Env;
use crate::js;
use crate::paths::{join, normalize_path, resolve_path};

/// The session format version this crate writes.
pub const CURRENT_SESSION_VERSION: u64 = 3;
/// Header discovery reads at most this many bytes before giving up on a file.
pub const MAX_SESSION_HEADER_SCAN_BYTES: u64 = 1024 * 1024;

/// One parsed line, as `JSON.parse` would accept it; falsy values (`null`, `0`, `false`, `""`) are skipped.
fn parse_line(line: &str) -> Option<Value> {
    if js::trim(line).is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(line).ok().filter(|v| js::truthy(Some(v)))
}

/// `parseSessionEntries`: every non-blank line that parses as JSON (malformed lines are skipped).
pub fn parse_session_entries(content: &str) -> Vec<Value> {
    js::trim(content)
        .split('\n')
        .filter(|line| !js::trim(line).is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn is_header(entry: &Value) -> bool {
    entry_type(entry) == Some("session") && entry.get("id").is_some_and(Value::is_string)
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `loadEntriesFromFile`: all parsed lines of a session file, or nothing when the first parsed line
/// is not a session header. A missing file is empty. When the file is a session whose last line
/// lacks a newline, a newline is appended so the next append starts a new line.
pub fn load_entries_from_file(path: &str) -> io::Result<Vec<Value>> {
    let path = normalize_path(path);
    let file = match File::open(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        other => other?,
    };
    let mut reader = BufReader::with_capacity(1024 * 1024, file);
    let mut entries = Vec::new();
    let mut line = Vec::new();
    let mut pending = false;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let complete = line.last() == Some(&b'\n');
        let text = lossy(if complete { &line[..line.len() - 1] } else { &line });
        pending = !complete;
        entries.extend(parse_line(&text));
    }
    if entries.first().is_none_or(|header| !is_header(header)) {
        return Ok(Vec::new());
    }
    if pending {
        OpenOptions::new().append(true).open(&path)?.write_all(b"\n")?;
    }
    Ok(entries)
}

/// Why a bounded header scan stopped without an answer.
#[derive(Debug)]
pub enum HeaderScanError {
    Io(io::Error),
    /// The first parsed line did not end within [`MAX_SESSION_HEADER_SCAN_BYTES`].
    ScanLimit,
}

/// `undefined` (keep scanning), `null` (not a session), or the header.
fn header_candidate(line: &[u8]) -> Option<Option<Value>> {
    let entry = parse_line(&lossy(line))?;
    Some(is_header(&entry).then_some(entry))
}

/// `readSessionHeader`: the header from the first parsed line, reading at most
/// [`MAX_SESSION_HEADER_SCAN_BYTES`] (plus one byte to tell a file that ends there from a longer one).
pub fn read_session_header(path: &str) -> Result<Option<Value>, HeaderScanError> {
    let mut file = File::open(path).map_err(HeaderScanError::Io)?;
    let mut reader = BufReader::new((&mut file).take(MAX_SESSION_HEADER_SCAN_BYTES));
    let mut line = Vec::new();
    loop {
        line.clear();
        reader.read_until(b'\n', &mut line).map_err(HeaderScanError::Io)?;
        if line.last() == Some(&b'\n') {
            if let Some(header) = header_candidate(&line[..line.len() - 1]) {
                return Ok(header);
            }
            continue;
        }
        // End of the bounded read: a real end of file, or the scan limit.
        let mut probe = [0u8; 1];
        let at_limit = reader.into_inner().limit() == 0;
        if at_limit && file.read(&mut probe).map_err(HeaderScanError::Io)? != 0 {
            return Err(HeaderScanError::ScanLimit);
        }
        return Ok(header_candidate(&line).flatten());
    }
}

/// Discovery is best-effort: unreadable or oversized files are not sessions.
pub(crate) fn discovery_header(path: &str) -> Option<Value> {
    read_session_header(path).ok().flatten()
}

pub(crate) fn header_cwd(header: &Value) -> Option<&str> {
    header.get("cwd").and_then(Value::as_str)
}

pub(crate) fn cwd_matches(cwd: Option<&str>, resolved_cwd: &str) -> bool {
    cwd.is_some_and(|cwd| !cwd.is_empty() && resolve_path(cwd) == resolved_cwd)
}

/// File names in a directory in byte order, as Node's `readdir` (libuv scandir) returns them.
pub(crate) fn read_dir_sorted(dir: &str) -> io::Result<Vec<String>> {
    let mut names: Vec<String> = fs::read_dir(dir)?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<io::Result<_>>()?;
    names.sort();
    Ok(names)
}

fn mtime_ms(meta: &fs::Metadata) -> io::Result<f64> {
    let modified = meta.modified()?;
    Ok(match modified.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs_f64() * 1000.0,
        Err(e) => -(e.duration().as_secs_f64() * 1000.0),
    })
}

/// `findMostRecentSession`: the most recently modified `.jsonl` session in `dir`, optionally only
/// sessions started in `cwd`. Any directory or stat error makes the lookup unavailable (`None`).
pub fn find_most_recent_session(dir: &str, cwd: Option<&str>) -> Option<String> {
    let dir = normalize_path(dir);
    let resolved_cwd = cwd.filter(|c| !c.is_empty()).map(resolve_path);
    let mut files = Vec::new();
    for name in read_dir_sorted(&dir).ok()? {
        if name.ends_with(".jsonl") {
            let path = join(&[&dir, &name]);
            let mtime = mtime_ms(&fs::metadata(&path).ok()?).ok()?;
            files.push((path, mtime));
        }
    }
    js::sort_by(&mut files, |a, b| b.1 - a.1);
    files.into_iter().map(|(path, _)| path).find(|path| {
        discovery_header(path).is_some_and(|header| {
            resolved_cwd
                .as_deref()
                .is_none_or(|resolved| cwd_matches(header_cwd(&header), resolved))
        })
    })
}

/// Generates an 8-character entry id not in use (`generateId`); after 100 collisions, a full UUID.
pub(crate) fn generate_id(env: &mut dyn Env, in_use: impl Fn(&str) -> bool) -> String {
    for _ in 0..100 {
        let id: String = env.random_uuid().chars().take(8).collect();
        if !in_use(&id) {
            return id;
        }
    }
    env.random_uuid()
}

/// v1 → v2: give every entry an id and chain `parentId`s in file order; compaction
/// `firstKeptEntryIndex` (an index into the file) becomes `firstKeptEntryId`.
fn migrate_v1_to_v2(entries: &mut [Value], env: &mut dyn Env) {
    let mut prev: Option<Value> = None;
    for i in 0..entries.len() {
        let Value::Object(entry) = &mut entries[i] else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) == Some("session") {
            entry.insert("version".into(), 2.into());
            continue;
        }
        // Upstream checks new ids against a set it never fills, so the first candidate always wins.
        let id = Value::from(generate_id(env, |_| false));
        entry.insert("id".into(), id.clone());
        entry.insert("parentId".into(), prev.replace(id).unwrap_or(Value::Null));
        if entry.get("type").and_then(Value::as_str) != Some("compaction") {
            continue;
        }
        let Some(index) = entry.get("firstKeptEntryIndex").and_then(Value::as_f64) else {
            continue;
        };
        let target = (index >= 0.0 && index.fract() == 0.0)
            .then(|| entries.get(index as usize))
            .flatten()
            .filter(|t| entry_type(t) != Some("session"))
            .map(|t| t.get("id").cloned());
        let Value::Object(entry) = &mut entries[i] else {
            continue;
        };
        match target {
            // The target's id is `undefined` until it is migrated, which drops the field.
            Some(Some(id)) => {
                entry.insert("firstKeptEntryId".into(), id);
            }
            Some(None) => {
                entry.shift_remove("firstKeptEntryId");
            }
            None => {}
        }
        entry.shift_remove("firstKeptEntryIndex");
    }
}

/// v2 → v3: the `hookMessage` role is now `custom`.
fn migrate_v2_to_v3(entries: &mut [Value]) {
    for entry in entries.iter_mut() {
        match entry_type(entry) {
            Some("session") => entry["version"] = 3.into(),
            Some("message") => {
                if let Some(Value::Object(message)) = entry.get_mut("message")
                    && message.get("role").and_then(Value::as_str) == Some("hookMessage")
                {
                    message.insert("role".into(), "custom".into());
                }
            }
            _ => {}
        }
    }
}

/// `migrateToCurrentVersion`: brings entries to [`CURRENT_SESSION_VERSION`] in place. The version
/// comes from the first header (absent: 1). Returns whether anything ran.
pub fn migrate_session_entries(entries: &mut [Value], env: &mut dyn Env) -> bool {
    let header = entries.iter().find(|e| entry_type(e) == Some("session"));
    let version = match header.and_then(|h| h.get("version")) {
        None | Some(Value::Null) => 1.0,
        Some(v) => js::to_number(v),
    };
    // NaN compares false everywhere: such a header reports a migration that changes nothing.
    if version >= CURRENT_SESSION_VERSION as f64 {
        return false;
    }
    if version < 2.0 {
        migrate_v1_to_v2(entries, env);
    }
    if version < 3.0 {
        migrate_v2_to_v3(entries);
    }
    true
}

/// `SessionInfo`: one listed session.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionInfo {
    pub path: String,
    pub id: String,
    /// Working directory the session started in; empty for old sessions.
    pub cwd: String,
    pub name: Option<String>,
    pub parent_session_path: Option<String>,
    /// Header timestamp in ms; `None` for an invalid date.
    pub created: Option<i64>,
    /// Latest user/assistant activity, else the header time, else the file mtime (ms).
    pub modified: Option<i64>,
    pub message_count: usize,
    pub first_message: String,
    pub all_messages_text: String,
}

fn text_content(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(blocks) => {
            let mut texts = Vec::new();
            for block in blocks {
                let Value::Object(block) = block else { return None };
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    texts.push(js::join_string(block.get("text")));
                }
            }
            Some(texts.join(" "))
        }
        _ => None,
    }
}

/// `buildSessionInfo`. `None` when the file is not a session or holds data upstream would throw on.
fn build_session_info(path: &str, mtime: Option<f64>) -> Option<SessionInfo> {
    let mtime = match mtime {
        Some(m) => m,
        None => mtime_ms(&fs::metadata(path).ok()?).ok()?,
    };
    let mut reader = BufReader::new(File::open(path).ok()?);
    let mut header: Option<Value> = None;
    let (mut message_count, mut first_message, mut all_messages) = (0, String::new(), Vec::new());
    let mut name = None;
    let mut last_activity: Option<f64> = None;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf).ok()? == 0 {
            break;
        }
        let chunk = buf.strip_suffix(b"\n").unwrap_or(&buf);
        // readline with crlfDelay: Infinity ends lines at \n, \r\n and a lone \r.
        for raw in chunk.split(|b| *b == b'\r') {
            let Some(entry) = parse_line(&lossy(raw)) else { continue };
            if header.is_none() {
                if entry_type(&entry) != Some("session") {
                    return None;
                }
                header = Some(entry);
                continue;
            }
            match entry_type(&entry) {
                Some("session_info") => {
                    name = match entry.get("name") {
                        None | Some(Value::Null) => None,
                        Some(Value::String(n)) => Some(js::trim(n).to_string()).filter(|n| !n.is_empty()),
                        Some(_) => return None,
                    };
                    continue;
                }
                Some("message") => {}
                _ => continue,
            }
            message_count += 1;
            let message = match entry.get("message") {
                // Upstream throws reading `role` of a missing message, which drops the file.
                None | Some(Value::Null) => return None,
                Some(Value::Object(message)) => message,
                Some(_) => continue,
            };
            if !(message.get("role").is_some_and(Value::is_string) && message.contains_key("content")) {
                continue;
            }
            let role = message.get("role").and_then(Value::as_str);
            if !matches!(role, Some("user" | "assistant")) {
                continue;
            }
            let activity = match message.get("timestamp") {
                Some(Value::Number(n)) => n.as_f64(),
                _ => js::date_value(entry.get("timestamp")).map(|t| t as f64),
            };
            if let Some(t) = activity {
                last_activity = Some(last_activity.unwrap_or(0.0).max(t));
            }
            let text = text_content(message.get("content"))?;
            if text.is_empty() {
                continue;
            }
            if first_message.is_empty() && role == Some("user") {
                first_message.clone_from(&text);
            }
            all_messages.push(text);
        }
    }
    let header = header?;
    let header_time = match header.get("timestamp") {
        Some(Value::String(s)) => js::parse_date(s),
        _ => None,
    };
    let modified = match last_activity {
        Some(t) if t > 0.0 => js::time_clip(t),
        _ => header_time.or_else(|| js::time_clip(mtime)),
    };
    Some(SessionInfo {
        path: path.to_string(),
        // ponytail: a non-string header id is listed by upstream; skip such files here.
        id: header.get("id")?.as_str()?.to_string(),
        cwd: header_cwd(&header).unwrap_or_default().to_string(),
        name,
        parent_session_path: header.get("parentSession").and_then(Value::as_str).map(str::to_string),
        created: js::date_value(header.get("timestamp")),
        modified,
        message_count,
        first_message: if first_message.is_empty() {
            "(no messages)".to_string()
        } else {
            first_message
        },
        all_messages_text: all_messages.join(" "),
    })
}

fn sort_session_infos(sessions: &mut [SessionInfo]) {
    let ms = |t: Option<i64>| t.map_or(f64::NAN, |t| t as f64);
    js::sort_by(sessions, |a, b| ms(b.modified) - ms(a.modified));
}

/// `listSessionsFromDir`: every session in `dir`, in reverse file-name order (unsorted by activity).
fn list_dir(dir: &str) -> Vec<SessionInfo> {
    let Ok(mut names) = read_dir_sorted(dir) else {
        return Vec::new();
    };
    names.retain(|n| n.ends_with(".jsonl"));
    // ponytail: upstream orders by localeCompare (ICU collation) descending; byte order agrees on generated names.
    names.reverse();
    names
        .iter()
        .filter_map(|name| build_session_info(&join(&[dir, name]), None))
        .collect()
}

/// A cwd as a session directory name: `--<cwd without its leading slash, / \\ : as ->--`.
fn encode_cwd(cwd: &str) -> String {
    let trimmed = cwd.strip_prefix(['/', '\\']).unwrap_or(cwd);
    format!("--{}--", trimmed.replace(['/', '\\', ':'], "-"))
}

/// `getDefaultSessionDir` without creating it: `<agent>/sessions/<encoded resolved cwd>`.
pub fn default_session_dir_path(cwd: &str, agent_dir: &str) -> String {
    join(&[&resolve_path(agent_dir), "sessions", &encode_cwd(&resolve_path(cwd))])
}

/// `migrateSessionsFromAgentRoot` (src/migrations.ts): v0.30.0 wrote sessions straight into the
/// agent dir; move each `<agent>/*.jsonl` whose first line is a header with a cwd into that cwd's
/// session dir. Files that do not qualify, or whose target exists, stay where they are.
pub fn migrate_sessions_from_agent_root(agent_dir: &str) {
    let Ok(names) = read_dir_sorted(agent_dir) else { return };
    for name in names.into_iter().filter(|n| n.ends_with(".jsonl")) {
        let _ = move_root_session(agent_dir, &name);
    }
}

fn move_root_session(agent_dir: &str, name: &str) -> Option<()> {
    let file = join(&[agent_dir, name]);
    let content = lossy(&fs::read(&file).ok()?);
    let first_line = content.split('\n').next().filter(|l| !js::trim(l).is_empty())?;
    let header: Value = serde_json::from_str(first_line).ok()?;
    if entry_type(&header) != Some("session") {
        return None;
    }
    // The header cwd is used as written (not resolved); a falsy or non-string cwd is skipped.
    let cwd = header.get("cwd")?.as_str().filter(|c| !c.is_empty())?;
    let dir = join(&[agent_dir, "sessions", &encode_cwd(cwd)]);
    fs::create_dir_all(&dir).ok()?;
    let target = join(&[&dir, name]);
    if fs::exists(&target).unwrap_or(false) {
        return None;
    }
    fs::rename(&file, &target).ok()
}

/// `getDefaultSessionDir`: the default session directory for `cwd`, created if missing.
pub fn default_session_dir(cwd: &str, agent_dir: &str) -> io::Result<String> {
    let dir = default_session_dir_path(cwd, agent_dir);
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The directory a static lookup uses, and whether it must filter sessions by cwd: a custom
/// directory other than the cwd's default holds sessions from many projects.
pub(crate) fn lookup_dir(env: &dyn Env, cwd: &str, session_dir: Option<&str>) -> io::Result<(String, bool)> {
    let agent_dir = env.agent_dir();
    Ok(match session_dir {
        Some(dir) => {
            let dir = normalize_path(dir);
            let filter = dir != default_session_dir_path(cwd, &agent_dir);
            (dir, filter)
        }
        None => (default_session_dir(cwd, &agent_dir)?, false),
    })
}

/// `SessionManager.list`: sessions for `cwd`, newest activity first.
pub fn list_sessions(env: &dyn Env, cwd: &str, session_dir: Option<&str>) -> io::Result<Vec<SessionInfo>> {
    let (dir, filter) = lookup_dir(env, cwd, session_dir)?;
    let resolved_cwd = resolve_path(cwd);
    let mut sessions = list_dir(&dir);
    sessions.retain(|s| !filter || cwd_matches(Some(&s.cwd), &resolved_cwd));
    sort_session_infos(&mut sessions);
    Ok(sessions)
}

/// `SessionManager.listAll`: every session in a flat `session_dir`, or with `None` in every
/// project directory under `<agent>/sessions`; newest activity first.
pub fn list_all_sessions(env: &dyn Env, session_dir: Option<&str>) -> Vec<SessionInfo> {
    if let Some(dir) = session_dir {
        let mut sessions = list_dir(&normalize_path(dir));
        sort_session_infos(&mut sessions);
        return sessions;
    }
    let sessions_dir = join(&[&env.agent_dir(), "sessions"]);
    let Ok(dirs) = read_dir_sorted(&sessions_dir) else {
        return Vec::new();
    };
    let mut candidates: Vec<(String, Option<f64>)> = Vec::new();
    for name in dirs {
        let dir = join(&[&sessions_dir, &name]);
        let is_dir = fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir() || m.file_type().is_symlink());
        if !is_dir {
            continue;
        }
        for file in read_dir_sorted(&dir)
            .unwrap_or_default()
            .into_iter()
            .filter(|f| f.ends_with(".jsonl"))
        {
            let path = join(&[&dir, &file]);
            let mtime = fs::metadata(&path).ok().and_then(|m| mtime_ms(&m).ok());
            candidates.push((path, mtime));
        }
    }
    let base = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    js::sort_by(&mut candidates, |a, b| {
        let by_mtime = b.1.unwrap_or(f64::NEG_INFINITY) - a.1.unwrap_or(f64::NEG_INFINITY);
        if by_mtime != 0.0 && !by_mtime.is_nan() {
            return by_mtime;
        }
        // ponytail: upstream uses localeCompare (ICU collation); byte order agrees on generated names.
        match base(&b.0).cmp(&base(&a.0)) {
            std::cmp::Ordering::Less => -1.0,
            std::cmp::Ordering::Equal => 0.0,
            std::cmp::Ordering::Greater => 1.0,
        }
    });
    let mut sessions: Vec<SessionInfo> = candidates
        .into_iter()
        .filter_map(|(path, mtime)| build_session_info(&path, mtime))
        .collect();
    sort_session_infos(&mut sessions);
    sessions
}

/// `SessionManager.findById`: the file whose header has exactly `id`, without reading bodies.
pub fn find_session_by_id(env: &dyn Env, cwd: &str, id: &str, session_dir: Option<&str>) -> Option<String> {
    let (dir, filter) = lookup_dir(env, cwd, session_dir).ok()?;
    let resolved_cwd = resolve_path(cwd);
    read_dir_sorted(&dir)
        .ok()?
        .into_iter()
        .filter(|f| f.ends_with(".jsonl"))
        .find_map(|file| {
            let path = join(&[&dir, &file]);
            let header = discovery_header(&path)?;
            let matches = field(&header, "id").and_then(Value::as_str) == Some(id)
                && (!filter || cwd_matches(header_cwd(&header), &resolved_cwd));
            matches.then_some(path)
        })
}
