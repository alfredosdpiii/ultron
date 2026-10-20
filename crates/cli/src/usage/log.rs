//! A native session file read for the session report without opening the session (`ultron/session-log.ts`): a
//! digest of each entry (who wrote it and what it used, never its text), the latest values of the namespaces the
//! report reads, and every operation's lane and result.

use super::stats::{cell_apis, classify_nudge};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogUsage {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total_tokens: f64,
    /// Provider-reported cost; None when the response carried none.
    pub cost: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogToolCall {
    pub name: String,
    /// For an `rlm` cell: the REPL APIs its code names.
    pub apis: Option<Vec<&'static str>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    User,
    Assistant,
    ToolResult,
    Custom,
    Compaction,
    BranchSummary,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: f64,
    pub kind: EntryKind,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub usage: Option<LogUsage>,
    pub tool_calls: Vec<LogToolCall>,
    pub tool_name: Option<String>,
    pub is_error: bool,
    pub redactions: f64,
    pub nudge: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogUsageRow {
    pub usage: LogUsage,
    pub entry_id: Option<String>,
    pub adjustment: bool,
}

/// One model response of a durable runtime conversation other than the root's (an `rlm.spawn` child, a typed agent,
/// another root conversation): its `pi.assistant` entry in the session's durable records (`ultron.module/durable`).
/// The root conversation's responses are transcript entries already, so they are not listed.
#[derive(Clone, Debug, PartialEq)]
pub struct DurableResponse {
    /// The lane the conversation runs: its owning child task's name (`ultron.rlm-child.<task>`); none for another
    /// root conversation.
    pub lane: Option<String>,
    /// Whether a task (a subagent) owns the conversation.
    pub owned: bool,
    /// `provider/model` of the response.
    pub model: String,
    pub usage: LogUsage,
}

/// What the durable records say, gathered while reading (resolved into [`DurableResponse`]s at the end).
#[derive(Default)]
struct DurableDigest {
    /// Conversation id -> its owning task id (none for a root conversation).
    owners: std::collections::HashMap<u64, Option<u64>>,
    /// Task id -> its `input.name` (a child task's lane).
    task_names: std::collections::HashMap<u64, String>,
    /// (conversation, `provider/model`, usage), in commit order.
    responses: Vec<(u64, String, LogUsage)>,
}

/// The root conversation of the durable runtime (`durable::ROOT_CONVERSATION_ID`).
const ROOT_CONVERSATION: u64 = 1;
/// A durable commit line: `{"kind":"list","op":"append","seq":N,"namespace":"ultron.module","key":"durable",...}`.
const DURABLE_MARK: &str = r#""namespace":"ultron.module","key":"durable""#;

/// Whether a list-append line is a durable commit: its namespace and key come within its first bytes.
fn is_durable_commit(line: &str) -> bool {
    let head = &line.as_bytes()[..line.len().min(160)];
    head.windows(DURABLE_MARK.len()).any(|w| w == DURABLE_MARK.as_bytes())
}

impl DurableDigest {
    /// One durable commit (`{seq, writes}`).
    fn commit(&mut self, record: &Value) {
        for write in record.get("writes").and_then(Value::as_array).into_iter().flatten() {
            let value = &write["value"];
            match write.get("type").and_then(Value::as_str) {
                Some("conversation") => {
                    if let Some(id) = value["id"].as_u64() {
                        self.owners.insert(id, value["owner"]["taskId"].as_u64());
                    }
                }
                Some("task") => {
                    if let (Some(id), Some(name)) = (value["id"].as_u64(), value["input"]["name"].as_str()) {
                        self.task_names.entry(id).or_insert_with(|| name.to_string());
                    }
                }
                Some("entry") if value["kind"] == "pi.assistant" => {
                    let Some(conversation) = value["conversationId"].as_u64() else {
                        continue;
                    };
                    if conversation == ROOT_CONVERSATION {
                        continue;
                    }
                    let message = &value["model"][0];
                    let Some(usage) = log_usage(message.get("usage")) else {
                        continue;
                    };
                    let model = format!(
                        "{}/{}",
                        message["provider"].as_str().unwrap_or("unknown"),
                        message["model"].as_str().unwrap_or("unknown")
                    );
                    self.responses.push((conversation, model, usage));
                }
                _ => {}
            }
        }
    }

    fn resolve(self) -> Vec<DurableResponse> {
        self.responses
            .into_iter()
            .map(|(conversation, model, usage)| {
                let owner = self.owners.get(&conversation).copied().flatten();
                DurableResponse {
                    lane: owner.and_then(|task| self.task_names.get(&task).cloned()),
                    owned: owner.is_some(),
                    model,
                    usage,
                }
            })
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogOperation {
    pub id: String,
    pub lane: String,
    pub kind: String,
    pub started_at: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LogResult {
    pub operation_id: String,
    pub kind: String,
    pub status: String,
    pub from_tip_id: Option<String>,
    pub tip_id: Option<String>,
    pub started_at: f64,
    pub ended_at: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Header {
    pub id: String,
    pub cwd: String,
    pub created_at: f64,
    pub parent_session_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionLog {
    pub path: PathBuf,
    pub header: Header,
    pub modified_at: f64,
    pub bytes: u64,
    pub entries: Vec<LogEntry>,
    pub usage_rows: Vec<LogUsageRow>,
    /// Latest value by `namespace/key`, for the namespaces in [`KEPT_NAMESPACES`].
    pub values: IndexMap<String, Value>,
    pub operations: IndexMap<String, LogOperation>,
    pub results: Vec<LogResult>,
    /// Responses of the durable runtime's non-root conversations (sub-agents), from its records.
    pub durable_responses: Vec<DurableResponse>,
    pub unreadable_lines: u64,
}

/// A read failure: `Session` for a file that is not a native session (`SessionLogError`), `Io` otherwise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogError {
    Session(String),
    Io(String),
}

impl LogError {
    pub fn message(&self) -> &str {
        match self {
            LogError::Session(m) | LogError::Io(m) => m,
        }
    }
}

const KEPT_NAMESPACES: [&str; 9] = [
    "pi.session.name",
    "pi.lane.config",
    "pi.branch.tip",
    "ultron.tasks",
    "ultron.usage",
    "ultron.rlm.frames",
    "ultron.memory.state",
    "ultron.module",
    "ultron.claude-code.lanes",
];

pub fn record(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

/// A finite number above zero, else 0.
fn amount(value: Option<&Value>) -> f64 {
    match value.and_then(Value::as_f64) {
        Some(n) if n.is_finite() && n > 0.0 => n,
        _ => 0.0,
    }
}

pub fn log_usage(value: Option<&Value>) -> Option<LogUsage> {
    let usage = record(value)?;
    let cost = record(usage.get("cost"))
        .and_then(|c| c.get("total"))
        .and_then(Value::as_f64);
    let input = amount(usage.get("input"));
    let output = amount(usage.get("output"));
    let cache_read = amount(usage.get("cacheRead"));
    let cache_write = amount(usage.get("cacheWrite"));
    let total = amount(usage.get("totalTokens"));
    Some(LogUsage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: if total != 0.0 {
            total
        } else {
            input + output + cache_read + cache_write
        },
        cost: cost.filter(|c| c.is_finite() && *c >= 0.0),
    })
}

fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(Value::as_object)
            .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect(),
        _ => String::new(),
    }
}

fn timestamp_of(value: Option<&Value>) -> f64 {
    value.and_then(Value::as_f64).filter(|n| n.is_finite()).unwrap_or(0.0)
}

fn string_of(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn digest_entry(write: &Map<String, Value>) -> Option<LogEntry> {
    let id = write.get("id")?.as_str()?.to_string();
    let mut entry = LogEntry {
        id,
        parent_id: string_of(write.get("parentId")),
        timestamp: timestamp_of(write.get("timestamp")),
        kind: EntryKind::Other,
        provider: None,
        model: None,
        usage: None,
        tool_calls: Vec::new(),
        tool_name: None,
        is_error: false,
        redactions: 0.0,
        nudge: None,
    };
    let kind = write.get("type").and_then(Value::as_str);
    if let Some(k) = kind.and_then(|k| match k {
        "compaction" => Some(EntryKind::Compaction),
        "branch_summary" => Some(EntryKind::BranchSummary),
        "custom" => Some(EntryKind::Custom),
        _ => None,
    }) {
        entry.kind = k;
        entry.usage = log_usage(write.get("usage"));
        return Some(entry);
    }
    let Some(message) = record(write.get("message")).filter(|_| kind == Some("message")) else {
        return Some(entry);
    };
    match message.get("role").and_then(Value::as_str) {
        Some("assistant") => {
            entry.kind = EntryKind::Assistant;
            entry.provider = string_of(message.get("provider"));
            entry.model = string_of(message.get("model"));
            entry.usage = log_usage(message.get("usage"));
            for part in message.get("content").and_then(Value::as_array).into_iter().flatten() {
                let Some(item) = part.as_object() else { continue };
                if item.get("type").and_then(Value::as_str) != Some("toolCall") {
                    continue;
                }
                let Some(name) = item.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let code = record(item.get("arguments"))
                    .and_then(|a| a.get("code"))
                    .and_then(Value::as_str);
                entry.tool_calls.push(LogToolCall {
                    name: name.to_string(),
                    apis: code.filter(|_| name == "rlm").map(cell_apis),
                });
            }
        }
        Some("toolResult") => {
            entry.kind = EntryKind::ToolResult;
            entry.tool_name = string_of(message.get("toolName"));
            entry.is_error = message.get("isError") == Some(&Value::Bool(true));
            entry.redactions = (text_of(message.get("content")).matches("[REDACTED:").count()) as f64;
        }
        Some("user") => {
            entry.kind = EntryKind::User;
            entry.nudge = classify_nudge(&text_of(message.get("content")));
        }
        Some("custom") => entry.kind = EntryKind::Custom,
        _ => {}
    }
    Some(entry)
}

fn parse_header(line: &str, path: &Path) -> Result<Header, LogError> {
    let shown = path.display();
    let value: Value = serde_json::from_str(line)
        .map_err(|_| LogError::Session(format!("{shown} is not a session file (its first line is not JSON)")))?;
    let header = value.as_object();
    if let Some(h) = header
        && h.get("type").and_then(Value::as_str) == Some("session")
        && let Some(version) = h.get("version")
    {
        return Err(LogError::Session(format!(
            "{shown} is a Pi-format session (version {}); only native Ultron sessions are reported",
            crate::js::string(version)
        )));
    }
    let header = header
        .filter(|h| h.get("kind").and_then(Value::as_str) == Some("header"))
        .and_then(|h| Some((h, h.get("id")?.as_str()?, h.get("cwd")?.as_str()?)));
    let Some((h, id, cwd)) = header else {
        return Err(LogError::Session(format!(
            "{shown} is not a native Ultron session file"
        )));
    };
    Ok(Header {
        id: id.to_string(),
        cwd: cwd.to_string(),
        created_at: timestamp_of(h.get("createdAt")),
        parent_session_id: string_of(h.get("parentSessionId")),
    })
}

/// Lines as `readline` yields them: split on `\n` (a `\r` before it dropped), the last one even without a newline.
fn for_each_line(path: &Path, mut each: impl FnMut(&str) -> Result<bool, LogError>) -> Result<(), LogError> {
    let file = std::fs::File::open(path).map_err(|e| LogError::Io(io_message(&e, "open", path)))?;
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read = reader
            .read_until(b'\n', &mut buffer)
            .map_err(|e| LogError::Io(e.to_string()))?;
        if read == 0 {
            return Ok(());
        }
        if buffer.last() == Some(&b'\n') {
            buffer.pop();
            if buffer.last() == Some(&b'\r') {
                buffer.pop();
            }
        }
        if !each(&String::from_utf8_lossy(&buffer))? {
            return Ok(());
        }
    }
}

/// Node's error text for a failed file operation.
pub fn io_message(error: &std::io::Error, call: &str, path: &Path) -> String {
    match error.kind() {
        std::io::ErrorKind::NotFound => {
            format!("ENOENT: no such file or directory, {call} '{}'", path.display())
        }
        std::io::ErrorKind::PermissionDenied => format!("EACCES: permission denied, {call} '{}'", path.display()),
        _ => error.to_string(),
    }
}

/// Read only a session file's header (its first line).
pub fn read_session_header(path: &Path) -> Result<Header, LogError> {
    let mut header = None;
    for_each_line(path, |line| {
        header = Some(parse_header(line, path)?);
        Ok(false)
    })?;
    header.ok_or_else(|| LogError::Session(format!("{} is empty", path.display())))
}

/// File modification time in milliseconds (`Math.round(stat.mtimeMs)`) and size.
pub fn file_times(path: &Path) -> Result<(f64, u64), LogError> {
    let meta = std::fs::metadata(path).map_err(|e| LogError::Io(io_message(&e, "stat", path)))?;
    Ok((mtime_ms(&meta).round(), meta.len()))
}

pub fn mtime_ms(meta: &std::fs::Metadata) -> f64 {
    match meta.modified().ok().map(|m| m.duration_since(std::time::UNIX_EPOCH)) {
        Some(Ok(d)) => d.as_secs() as f64 * 1000.0 + f64::from(d.subsec_nanos()) / 1e6,
        Some(Err(e)) => -(e.duration().as_secs() as f64 * 1000.0 + f64::from(e.duration().subsec_nanos()) / 1e6),
        None => 0.0,
    }
}

/// Read a native session file. Fails with [`LogError::Session`] for a file that is not one.
pub fn read_session_log(path: &Path) -> Result<SessionLog, LogError> {
    let (modified_at, bytes) = file_times(path)?;
    let mut header: Option<Header> = None;
    let mut log = SessionLog {
        path: path.to_path_buf(),
        header: Header {
            id: String::new(),
            cwd: String::new(),
            created_at: 0.0,
            parent_session_id: None,
        },
        modified_at,
        bytes,
        entries: Vec::new(),
        usage_rows: Vec::new(),
        values: IndexMap::new(),
        operations: IndexMap::new(),
        results: Vec::new(),
        durable_responses: Vec::new(),
        unreadable_lines: 0,
    };
    let mut durable = DurableDigest::default();
    for_each_line(path, |line| {
        if header.is_none() {
            header = Some(parse_header(line, path)?);
            return Ok(true);
        }
        // List appends are skipped unread (stream frames are most of a file), except the durable runtime's commits.
        if line.is_empty() || (line.starts_with(r#"{"kind":"list""#) && !is_durable_commit(line)) {
            return Ok(true);
        }
        let Ok(parsed) = serde_json::from_str::<Value>(line) else {
            log.unreadable_lines += 1;
            return Ok(true);
        };
        let writes = match parsed {
            Value::Array(items) => items,
            other => vec![other],
        };
        for write in &writes {
            if let Some(item) = write.as_object() {
                apply(&mut log, &mut durable, item);
            }
        }
        Ok(true)
    })?;
    log.header = header.ok_or_else(|| LogError::Session(format!("{} is empty", path.display())))?;
    log.durable_responses = durable.resolve();
    Ok(log)
}

fn apply(log: &mut SessionLog, durable: &mut DurableDigest, write: &Map<String, Value>) {
    let kind = write.get("kind").and_then(Value::as_str);
    if kind == Some("list") {
        if write.get("op").and_then(Value::as_str) == Some("append")
            && write.get("namespace").and_then(Value::as_str) == Some("ultron.module")
            && write.get("key").and_then(Value::as_str) == Some("durable")
            && let Some(record) = write.get("value")
        {
            durable.commit(record);
        }
        return;
    }
    if kind == Some("entry") {
        if let Some(entry) = digest_entry(write) {
            log.entries.push(entry);
        }
        return;
    }
    if kind == Some("usage") {
        if let Some(usage) = log_usage(write.get("usage")) {
            log.usage_rows.push(LogUsageRow {
                usage,
                entry_id: string_of(write.get("entryId")),
                adjustment: write.get("adjustment") == Some(&Value::Bool(true)),
            });
        }
        return;
    }
    let (Some("value"), Some(namespace), Some(key)) = (
        kind,
        write.get("namespace").and_then(Value::as_str),
        write.get("key").and_then(Value::as_str),
    ) else {
        return;
    };
    let op = write.get("op").and_then(Value::as_str);
    let value = record(write.get("value"));
    if namespace == "pi.op.meta" {
        if op == Some("set")
            && let Some(v) = value
            && let Some(lane) = v.get("lane").and_then(Value::as_str)
        {
            let intent = record(v.get("intent"))
                .and_then(|i| i.get("kind"))
                .and_then(Value::as_str);
            log.operations.insert(
                key.to_string(),
                LogOperation {
                    id: key.to_string(),
                    lane: lane.to_string(),
                    kind: intent.unwrap_or("run").to_string(),
                    started_at: timestamp_of(v.get("startedAt")),
                },
            );
        }
        return;
    }
    if namespace == "pi.result" {
        if op == Some("set")
            && let Some(v) = value
        {
            let text = |k: &str| v.get(k).and_then(Value::as_str);
            log.results.push(LogResult {
                operation_id: text("operationId").unwrap_or(key).to_string(),
                kind: text("kind").unwrap_or("run").to_string(),
                status: text("status").unwrap_or("unknown").to_string(),
                from_tip_id: text("fromTipId").map(str::to_string),
                tip_id: text("tipId").map(str::to_string),
                started_at: timestamp_of(v.get("startedAt")),
                ended_at: timestamp_of(v.get("endedAt")),
            });
        }
        return;
    }
    if !KEPT_NAMESPACES.contains(&namespace) {
        return;
    }
    let address = format!("{namespace}/{key}");
    match op {
        Some("set") => {
            let v = write.get("value").cloned().unwrap_or(Value::Null);
            log.values.insert(address, v);
        }
        Some("delete") => {
            log.values.shift_remove(&address);
        }
        _ => {}
    }
}

impl SessionLog {
    /// The latest value at `namespace/key`.
    pub fn value(&self, namespace: &str, key: &str) -> Option<&Value> {
        self.values.get(&format!("{namespace}/{key}"))
    }

    /// The latest values of a namespace, by key, in the order they were first written.
    pub fn values_of(&self, namespace: &str) -> Vec<(&str, &Value)> {
        let prefix = format!("{namespace}/");
        self.values
            .iter()
            .filter_map(|(address, value)| address.strip_prefix(&prefix).map(|key| (key, value)))
            .collect()
    }
}
