//! Port of `packages/coding-agent/src/ultron/rlm/task-store.ts`: the session-scoped journal of RLM tasks
//! (frames, spawns, agents). One owner per store; every change rewrites one JSON document and is
//! published only after the write commits. A failed write poisons the journal; reopening turns
//! unfinished tasks into `interrupted` and never replays them.

use std::fmt;

use config::json::js_number;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Finished tasks kept in the persisted document: the newest ones. Memory keeps every task.
pub const MAX_PERSISTED_FINISHED_TASKS: usize = 100;

const RESULT_KEYS: [&str; 8] = [
    "status",
    "value",
    "error",
    "verification",
    "verdict",
    "check",
    "unverified",
    "worktree",
];
const TASK_KEYS: [&str; 7] = ["id", "key", "fingerprint", "definition", "state", "result", "parentId"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Admitted,
    Running,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
}

impl TaskState {
    pub fn parse(text: &str) -> Option<TaskState> {
        Some(match text {
            "admitted" => TaskState::Admitted,
            "running" => TaskState::Running,
            "completed" => TaskState::Completed,
            "failed" => TaskState::Failed,
            "cancelled" => TaskState::Cancelled,
            "interrupted" => TaskState::Interrupted,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Admitted => "admitted",
            TaskState::Running => "running",
            TaskState::Completed => "completed",
            TaskState::Failed => "failed",
            TaskState::Cancelled => "cancelled",
            TaskState::Interrupted => "interrupted",
        }
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, TaskState::Admitted | TaskState::Running)
    }

    /// The result status a terminal state must carry (`completed` pairs with `succeeded`).
    fn result_status(self) -> &'static str {
        match self {
            TaskState::Completed => "succeeded",
            other => other.as_str(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Task {
    pub id: String,
    pub key: String,
    pub fingerprint: String,
    pub definition: String,
    pub state: TaskState,
    /// A validated result object; present exactly when the state is terminal.
    pub result: Option<Value>,
    /// Spawning task; absent for tasks started by the root lane.
    pub parent_id: Option<String>,
}

impl Task {
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("id".into(), self.id.clone().into());
        map.insert("definition".into(), self.definition.clone().into());
        map.insert("fingerprint".into(), self.fingerprint.clone().into());
        map.insert("key".into(), self.key.clone().into());
        map.insert("state".into(), self.state.as_str().into());
        if let Some(parent) = &self.parent_id {
            map.insert("parentId".into(), parent.clone().into());
        }
        if let Some(result) = &self.result {
            map.insert("result".into(), result.clone());
        }
        Value::Object(map)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TaskStoreError {
    Durability,
    ReadFailed,
    InvalidDocument,
    UnsupportedVersion(Value),
    InvalidResult,
    InvalidDefinition,
    InvalidFingerprint,
    InvalidKey,
    KeyReused,
    InvalidId,
    UnknownTask,
    InvalidTransition,
    StateResultMismatch,
}

impl fmt::Display for TaskStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaskStoreError::Durability => f.write_str("Task store durability is uncertain; reopen the owner"),
            TaskStoreError::ReadFailed => f.write_str("Task store could not be read"),
            TaskStoreError::InvalidDocument => f.write_str("Invalid task document"),
            TaskStoreError::UnsupportedVersion(version) => write!(
                f,
                "Cannot resume: session value ultron.tasks/root has format version {}, but this Ultron reads only \
                 format version 1. It was left unchanged; open the session with the Ultron version that wrote it, or upgrade.",
                js_stringify(version)
            ),
            TaskStoreError::InvalidResult => f.write_str("Invalid task result"),
            TaskStoreError::InvalidDefinition => f.write_str("definition must be id@version"),
            TaskStoreError::InvalidFingerprint => f.write_str("fingerprint must be a lowercase SHA-256 hex string"),
            TaskStoreError::InvalidKey => f.write_str("idempotency key must be nonempty"),
            TaskStoreError::KeyReused => f.write_str("Idempotency key reused for a different task"),
            TaskStoreError::InvalidId => f.write_str("task ID must be nonempty"),
            TaskStoreError::UnknownTask => f.write_str("Unknown Ultron task"),
            TaskStoreError::InvalidTransition => f.write_str("Invalid task transition"),
            TaskStoreError::StateResultMismatch => f.write_str("Task state and result do not match"),
        }
    }
}

impl std::error::Error for TaskStoreError {}

/// Atomic replacement of the one task document (the session value `ultron.tasks/root`).
pub trait TaskDocumentStore {
    fn read(&mut self) -> Result<Option<Value>, String>;
    fn write(&mut self, document: &Value) -> Result<(), String>;
}

/// `JSON.stringify` for strict JSON values (numbers formatted as JavaScript does).
fn js_stringify(value: &Value) -> String {
    match value {
        Value::Number(n) => js_number(n),
        other => other.to_string(),
    }
}

/// SHA-256 of the canonical JSON (object keys sorted by UTF-16 code units, as JavaScript's sort does).
pub fn task_fingerprint(request: &Value) -> String {
    fn canonical(item: &Value, out: &mut String) {
        match item {
            Value::Array(items) => {
                out.push('[');
                for (n, item) in items.iter().enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    canonical(item, out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
                out.push('{');
                for (n, key) in keys.into_iter().enumerate() {
                    if n > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(key.clone()).to_string());
                    out.push(':');
                    canonical(&map[key], out);
                }
                out.push('}');
            }
            other => out.push_str(&js_stringify(other)),
        }
    }
    let mut text = String::new();
    canonical(request, &mut text);
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `id@version`: a lowercase id (`[a-z][a-z0-9-]*`) and a decimal version.
pub fn is_definition(text: &str) -> bool {
    let Some((name, version)) = text.split_once('@') else {
        return false;
    };
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !version.is_empty()
        && version.bytes().all(|b| b.is_ascii_digit())
}

fn is_fingerprint(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_result(result: &Value) -> bool {
    let Some(map) = result.as_object() else { return false };
    let status_ok = map
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|s| ["succeeded", "failed", "cancelled", "interrupted"].contains(&s));
    map.keys().all(|k| RESULT_KEYS.contains(&k.as_str()))
        && status_ok
        && map.get("verification").and_then(Value::as_str) == Some("unverified")
        && map.get("error").is_none_or(Value::is_string)
        && map.get("unverified").is_none_or(|v| v == &Value::Bool(true))
}

fn nonempty(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn parse_task(value: &Value) -> Option<Task> {
    let map = value.as_object()?;
    if !map.keys().all(|k| TASK_KEYS.contains(&k.as_str())) {
        return None;
    }
    let fingerprint = map.get("fingerprint")?.as_str().filter(|s| is_fingerprint(s))?;
    let definition = map.get("definition")?.as_str().filter(|s| is_definition(s))?;
    let state = TaskState::parse(map.get("state")?.as_str()?)?;
    let result = match map.get("result") {
        None => None,
        Some(result) if is_result(result) => Some(result.clone()),
        Some(_) => return None,
    };
    let parent_id = match map.get("parentId") {
        None => None,
        some => Some(nonempty(some)?),
    };
    let valid_result = match &result {
        None => !state.is_terminal(),
        Some(result) => state.is_terminal() && result["status"] == state.result_status(),
    };
    if !valid_result {
        return None;
    }
    Some(Task {
        id: nonempty(map.get("id"))?,
        key: nonempty(map.get("key"))?,
        fingerprint: fingerprint.into(),
        definition: definition.into(),
        state,
        result,
        parent_id,
    })
}

/// `readVersioned` then the schema and invariant checks of the saved document.
pub fn parse_document(saved: &Value) -> Result<Vec<Task>, TaskStoreError> {
    let map = saved.as_object().ok_or(TaskStoreError::InvalidDocument)?;
    if let Some(version) = map.get("version")
        && version.as_f64() != Some(1.0)
    {
        return Err(TaskStoreError::UnsupportedVersion(version.clone()));
    }
    if !map.keys().all(|k| k == "version" || k == "tasks") {
        return Err(TaskStoreError::InvalidDocument);
    }
    let items = map
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or(TaskStoreError::InvalidDocument)?;
    let mut tasks: Vec<Task> = Vec::with_capacity(items.len());
    for item in items {
        let task = parse_task(item).ok_or(TaskStoreError::InvalidDocument)?;
        if tasks.iter().any(|seen| seen.id == task.id || seen.key == task.key) {
            return Err(TaskStoreError::InvalidDocument);
        }
        tasks.push(task);
    }
    Ok(tasks)
}

/// `json` with the keys of `order` first, in that order, then the rest.
fn ordered(json: Value, order: Option<&Vec<String>>) -> Value {
    let (Some(order), Value::Object(mut map)) = (order, json.clone()) else {
        return json;
    };
    let mut out = Map::new();
    for key in order {
        if let Some(value) = map.shift_remove(key) {
            out.insert(key.clone(), value);
        }
    }
    out.extend(map);
    Value::Object(out)
}

impl<S> TaskJournal<S> {
    /// A record as the journal stores it (its stored key order kept).
    pub fn stored_json(&self, task: &Task) -> Value {
        ordered(task.to_json(), self.orders.get(&task.id))
    }
}

/// The tasks to persist: every unfinished task and the newest finished ones up to the bound.
pub fn persisted_tasks(records: &[Task]) -> Vec<Task> {
    let finished = records.iter().filter(|t| t.state.is_terminal()).count();
    let mut excess = finished.saturating_sub(MAX_PERSISTED_FINISHED_TASKS);
    records
        .iter()
        .filter(|task| {
            if excess > 0 && task.state.is_terminal() {
                excess -= 1;
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

fn random_task_id() -> String {
    format!("ultron-task-{}", random_uuid())
}

/// A random RFC 4122 version 4 UUID (`crypto.randomUUID`).
pub fn random_uuid() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("OS randomness is unavailable");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// Serialized, commit-before-publish task records. `&mut self` serializes changes the way the
/// TypeScript promise chain does.
pub struct TaskJournal<S> {
    store: S,
    records: Vec<Task>,
    /// Key order of each record as it was stored: task-store.ts rewrites a loaded record by spreading it, so its keys
    /// keep their stored order and new ones follow.
    orders: std::collections::HashMap<String, Vec<String>>,
    loaded: Option<Result<(), TaskStoreError>>,
    broken: bool,
    mint: Box<dyn FnMut() -> String + Send>,
}

impl<S: TaskDocumentStore> TaskJournal<S> {
    pub fn new(store: S) -> Self {
        Self::with_id_source(store, Box::new(random_task_id))
    }

    /// A journal whose new task IDs come from `mint` (replay of recorded IDs).
    pub fn with_id_source(store: S, mint: Box<dyn FnMut() -> String + Send>) -> Self {
        TaskJournal {
            store,
            records: Vec::new(),
            orders: std::collections::HashMap::new(),
            loaded: None,
            broken: false,
            mint,
        }
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    fn assert_healthy(&self) -> Result<(), TaskStoreError> {
        if self.broken {
            Err(TaskStoreError::Durability)
        } else {
            Ok(())
        }
    }

    fn load(&mut self) -> Result<(), TaskStoreError> {
        let Some(saved) = self.store.read().map_err(|_| TaskStoreError::ReadFailed)? else {
            return Ok(());
        };
        let parsed = parse_document(&saved)?;
        for task in saved["tasks"].as_array().into_iter().flatten() {
            if let (Some(id), Some(map)) = (task["id"].as_str(), task.as_object()) {
                self.orders.insert(id.to_owned(), map.keys().cloned().collect());
            }
        }
        let unfinished = parsed.iter().any(|t| !t.state.is_terminal());
        let recovered: Vec<Task> = parsed
            .into_iter()
            .map(|task| {
                if task.state.is_terminal() {
                    return task;
                }
                Task {
                    state: TaskState::Interrupted,
                    result: Some(json!({
                        "status": "interrupted",
                        "error": "Owner ended; automatic replay is disabled",
                        "verification": "unverified",
                    })),
                    ..task
                }
            })
            .collect();
        if unfinished {
            self.write(recovered)
        } else {
            self.records = recovered;
            Ok(())
        }
    }

    /// Loads once; a failed load stays failed for this owner.
    fn ensure_loaded(&mut self) -> Result<(), TaskStoreError> {
        if self.loaded.is_none() {
            let outcome = self.load();
            self.loaded = Some(outcome);
        }
        self.loaded.clone().unwrap_or(Ok(()))
    }

    fn write(&mut self, next: Vec<Task>) -> Result<(), TaskStoreError> {
        let tasks: Vec<Value> = persisted_tasks(&next)
            .iter()
            .map(|task| ordered(task.to_json(), self.orders.get(&task.id)))
            .collect();
        if self.store.write(&json!({ "version": 1, "tasks": tasks })).is_err() {
            self.broken = true;
            return Err(TaskStoreError::Durability);
        }
        self.records = next;
        Ok(())
    }

    fn begin(&mut self) -> Result<(), TaskStoreError> {
        self.assert_healthy()?;
        self.ensure_loaded()?;
        self.assert_healthy()
    }

    pub fn ready(&mut self) -> Result<(), TaskStoreError> {
        self.ensure_loaded()?;
        self.assert_healthy()
    }

    pub fn list(&mut self) -> Result<Vec<Task>, TaskStoreError> {
        self.begin()?;
        Ok(self.records.clone())
    }

    /// Admits a task under an idempotency key; a repeated key with the same fingerprint returns the
    /// existing task with `created == false`.
    pub fn admit(
        &mut self,
        definition: &str,
        fingerprint: &str,
        key: &str,
        parent_id: Option<&str>,
    ) -> Result<(Task, bool), TaskStoreError> {
        self.begin()?;
        if !is_definition(definition) {
            return Err(TaskStoreError::InvalidDefinition);
        }
        if !is_fingerprint(fingerprint) {
            return Err(TaskStoreError::InvalidFingerprint);
        }
        if key.is_empty() {
            return Err(TaskStoreError::InvalidKey);
        }
        if let Some(previous) = self.records.iter().find(|t| t.key == key) {
            if previous.fingerprint != fingerprint {
                return Err(TaskStoreError::KeyReused);
            }
            return Ok((previous.clone(), false));
        }
        let task = Task {
            id: (self.mint)(),
            key: key.into(),
            fingerprint: fingerprint.into(),
            definition: definition.into(),
            state: TaskState::Admitted,
            result: None,
            parent_id: parent_id.map(str::to_owned),
        };
        let mut next = self.records.clone();
        next.push(task.clone());
        self.write(next)?;
        Ok((task, true))
    }

    /// Moves a task forward. A task already terminal is returned unchanged, so a completion that
    /// races a committed cancellation observes the cancellation.
    pub fn transition(&mut self, id: &str, state: TaskState, result: Option<Value>) -> Result<Task, TaskStoreError> {
        self.begin()?;
        if id.is_empty() {
            return Err(TaskStoreError::InvalidId);
        }
        let index = self
            .records
            .iter()
            .position(|t| t.id == id)
            .ok_or(TaskStoreError::UnknownTask)?;
        let current = &self.records[index];
        if current.state.is_terminal() {
            return Ok(current.clone());
        }
        if state == TaskState::Running {
            if current.state != TaskState::Admitted || result.is_some() {
                return Err(TaskStoreError::InvalidTransition);
            }
        } else {
            let can_finish = current.state == TaskState::Running
                || (current.state == TaskState::Admitted && state == TaskState::Cancelled);
            let Some(result) = result.as_ref().filter(|_| can_finish && state.is_terminal()) else {
                return Err(TaskStoreError::InvalidTransition);
            };
            if !is_result(result) {
                return Err(TaskStoreError::InvalidResult);
            }
            if result["status"] != state.result_status() {
                return Err(TaskStoreError::StateResultMismatch);
            }
        }
        let next_task = Task {
            state,
            result,
            ..current.clone()
        };
        let mut next = self.records.clone();
        next[index] = next_task.clone();
        self.write(next)?;
        Ok(next_task)
    }
}
