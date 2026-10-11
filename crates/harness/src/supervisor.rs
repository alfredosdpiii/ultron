//! Task supervision of `packages/coding-agent/src/ultron/rlm/native-host.ts` (`NativeRlmHost`): request parsing,
//! idempotent admission, the running transition, first-terminal-wins finishing, subtree cancellation with abort
//! propagation to each task's work, host close, child-lane visibility, and restart recovery (through the journal).
//!
//! What a task *does* (a lane prompt, a deterministic adapter, a predict call) is the caller's: it starts the work
//! after `spawn`, watches the task's [`CancelSignal`], and reports the outcome with [`TaskSupervisor::complete`].
//! The TypeScript host runs the journal writes on promise chains; here every call is one synchronous step, so a
//! task is never observable between admission and its running transition.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

use crate::task_store::{
    Task, TaskDocumentStore, TaskJournal, TaskState, TaskStoreError, is_definition, random_uuid, task_fingerprint,
};

/// Default task timeout when a request names none (30 minutes).
pub const DEFAULT_TIMEOUT_MS: u64 = 30 * 60 * 1000;
/// Largest accepted `timeout_ms` (one hour).
pub const MAX_TIMEOUT_MS: u64 = 60 * 60 * 1000;

const CANCELLED: &str = "Ultron task cancelled";
const HOST_CLOSED: &str = "Ultron task host closed";

/// JavaScript's `\s` and `String.prototype.trim` set: Unicode White_Space without U+0085, plus U+FEFF.
fn is_js_space(c: char) -> bool {
    c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}')
}

/// A validated `agents.spawn` / `agents.invoke` payload.
#[derive(Clone, Debug, PartialEq)]
pub struct SpawnRequest {
    pub definition: String,
    pub input: Value,
    pub model: Option<String>,
    pub key: Option<String>,
    pub timeout_ms: u64,
}

impl SpawnRequest {
    /// `request(payload)` without the definition registry's input schema check.
    pub fn from_payload(payload: &Map<String, Value>) -> Result<SpawnRequest, String> {
        for key in payload.keys() {
            if !["definition", "input", "model", "key", "timeout_ms"].contains(&key.as_str()) {
                return Err(format!("Unknown payload field: {key}"));
            }
        }
        let definition = payload
            .get("definition")
            .and_then(Value::as_str)
            .filter(|d| is_definition(d))
            .ok_or("definition must be id@version")?;
        let input = payload.get("input").ok_or("Agent input is not JSON")?;
        let model = match payload.get("model") {
            None | Some(Value::Null) => None,
            Some(Value::String(model)) if is_model(model) => Some(model.clone()),
            Some(_) => return Err("model must be provider/model".into()),
        };
        let key = match payload.get("key") {
            None | Some(Value::Null) => None,
            Some(Value::String(key)) if !key.trim_matches(is_js_space).is_empty() => Some(key.clone()),
            Some(_) => return Err("key must be a nonempty string".into()),
        };
        let timeout_ms = match payload.get("timeout_ms") {
            None => DEFAULT_TIMEOUT_MS,
            Some(value) => value
                .as_f64()
                .filter(|t| t.fract() == 0.0 && (1.0..=MAX_TIMEOUT_MS as f64).contains(t))
                .map(|t| t as u64)
                .ok_or("timeout_ms must be an integer between 1 and 3600000")?,
        };
        Ok(SpawnRequest {
            definition: definition.into(),
            input: input.clone(),
            model,
            key,
            timeout_ms,
        })
    }

    /// The admission fingerprint: definition, input, model (or null) and timeout.
    pub fn fingerprint(&self) -> String {
        task_fingerprint(&json!({
            "definition": self.definition,
            "input": self.input,
            "model": self.model,
            "timeout_ms": self.timeout_ms,
        }))
    }
}

/// `provider/model[/more]`: at least two nonempty segments without whitespace.
fn is_model(text: &str) -> bool {
    let mut segments = 0;
    for segment in text.split('/') {
        if segment.is_empty() || segment.chars().any(is_js_space) {
            return false;
        }
        segments += 1;
    }
    segments >= 2
}

/// Cancellation of one task's work, shared between the supervisor and whatever runs the task.
#[derive(Clone, Debug, Default)]
pub struct CancelSignal(Arc<Mutex<Option<String>>>);

impl CancelSignal {
    pub fn is_cancelled(&self) -> bool {
        self.reason().is_some()
    }

    /// The first cancellation reason, once cancelled.
    pub fn reason(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn cancel(&self, reason: &str) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(reason.into());
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SupervisorError {
    Closed,
    UnknownTask,
    /// The terminal transition committed but carried no result.
    NoDurableResult,
    Store(TaskStoreError),
}

impl fmt::Display for SupervisorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SupervisorError::Closed => f.write_str("Ultron task host is closed"),
            SupervisorError::UnknownTask => f.write_str("Unknown Ultron task"),
            SupervisorError::NoDurableResult => f.write_str("Terminal task has no durable result"),
            SupervisorError::Store(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for SupervisorError {}

impl From<TaskStoreError> for SupervisorError {
    fn from(error: TaskStoreError) -> Self {
        SupervisorError::Store(error)
    }
}

/// `agents.inspect` / `agents.status` view of a task.
pub fn public_task(task: &Task) -> Value {
    let mut map = Map::new();
    map.insert("id".into(), task.id.clone().into());
    map.insert("definition".into(), task.definition.clone().into());
    map.insert("state".into(), task.state.as_str().into());
    if let Some(parent) = &task.parent_id {
        map.insert("parentId".into(), parent.clone().into());
    }
    if let Some(result) = &task.result {
        map.insert("result".into(), result.clone());
    }
    Value::Object(map)
}

type AbortHook = Box<dyn FnMut(&str) + Send>;

/// One owner's live task tree over a [`TaskJournal`].
pub struct TaskSupervisor<S> {
    journal: TaskJournal<S>,
    /// Signals of tasks this owner admitted; tasks loaded from an earlier owner have none (they are terminal).
    signals: HashMap<String, CancelSignal>,
    closed: bool,
    used: bool,
    mint_key: Box<dyn FnMut() -> String + Send>,
    on_abort: AbortHook,
}

impl<S: TaskDocumentStore> TaskSupervisor<S> {
    pub fn new(store: S) -> Self {
        Self::with_journal(TaskJournal::new(store), Box::new(random_uuid))
    }

    /// A supervisor over `journal` whose generated idempotency keys come from `mint_key`.
    pub fn with_journal(journal: TaskJournal<S>, mint_key: Box<dyn FnMut() -> String + Send>) -> Self {
        TaskSupervisor {
            journal,
            signals: HashMap::new(),
            closed: false,
            used: false,
            mint_key,
            on_abort: Box::new(|_| {}),
        }
    }

    /// Called once for each task whose work is aborted (the lane abort of the TypeScript host).
    pub fn on_abort(&mut self, hook: AbortHook) {
        self.on_abort = hook;
    }

    pub fn journal(&self) -> &TaskJournal<S> {
        &self.journal
    }

    /// The cancellation signal of a task this owner admitted.
    pub fn signal(&self, id: &str) -> Option<CancelSignal> {
        self.signals.get(id).cloned()
    }

    /// Opens a request: refused once closed, else the journal is loaded (recovering an earlier owner's tasks).
    fn enter(&mut self) -> Result<Vec<Task>, SupervisorError> {
        if self.closed {
            return Err(SupervisorError::Closed);
        }
        self.used = true;
        Ok(self.journal.list()?)
    }

    /// The tasks `caller` may see: its own task and descendants; the root lane (`None`) sees everything.
    fn visible(tasks: &[Task], caller: Option<&str>, id: &str) -> bool {
        let Some(caller) = caller else { return true };
        let mut current = tasks.iter().find(|t| t.id == id);
        while let Some(task) = current {
            if task.id == caller {
                return true;
            }
            current = task
                .parent_id
                .as_deref()
                .and_then(|parent| tasks.iter().find(|t| t.id == parent));
        }
        false
    }

    fn lookup(&mut self, caller: Option<&str>, id: &str) -> Result<Task, SupervisorError> {
        let tasks = self.enter()?;
        tasks
            .iter()
            .find(|t| t.id == id)
            .filter(|_| Self::visible(&tasks, caller, id))
            .cloned()
            .ok_or(SupervisorError::UnknownTask)
    }

    /// `agents.status`: every task the caller may see, oldest first.
    pub fn status(&mut self, caller: Option<&str>) -> Result<Vec<Value>, SupervisorError> {
        let tasks = self.enter()?;
        Ok(tasks
            .iter()
            .filter(|t| Self::visible(&tasks, caller, &t.id))
            .map(public_task)
            .collect())
    }

    /// `agents.inspect`.
    pub fn inspect(&mut self, caller: Option<&str>, id: &str) -> Result<Value, SupervisorError> {
        Ok(public_task(&self.lookup(caller, id)?))
    }

    /// `agents.result` once the task is terminal; `None` while it runs (the TypeScript call waits).
    pub fn result(&mut self, caller: Option<&str>, id: &str) -> Result<Option<Value>, SupervisorError> {
        Ok(self.lookup(caller, id)?.result)
    }

    /// Admits a task as a child of `parent` (the calling task, `None` for the root lane) and starts it. A repeated
    /// idempotency key returns the existing task, in whatever state it is. Returns `{id, state}` as admitted.
    pub fn spawn(&mut self, parent: Option<&str>, request: &SpawnRequest) -> Result<Value, SupervisorError> {
        self.enter()?;
        let key = match &request.key {
            Some(key) => key.clone(),
            None => (self.mint_key)(),
        };
        let (task, created) = self
            .journal
            .admit(&request.definition, &request.fingerprint(), &key, parent)?;
        let admitted = json!({ "id": task.id, "state": task.state.as_str() });
        if !created {
            return Ok(admitted);
        }
        self.signals.insert(task.id.clone(), CancelSignal::default());
        if let Err(error) = self.journal.transition(&task.id, TaskState::Running, None) {
            // The task could not start: its work is stopped; the poisoned journal refuses every later request.
            self.abort_work(&task.id, &error.to_string());
        }
        Ok(admitted)
    }

    fn abort_work(&mut self, id: &str, reason: &str) {
        if let Some(signal) = self.signals.get(id) {
            signal.cancel(reason);
            (self.on_abort)(id);
        }
    }

    /// The task's work ended with `result`. The first terminal result wins: a task already ended keeps its result.
    pub fn complete(&mut self, id: &str, result: Value) -> Result<Value, SupervisorError> {
        self.enter()?;
        self.finish(id, result)
    }

    fn finish(&mut self, id: &str, result: Value) -> Result<Value, SupervisorError> {
        let state = match result.get("status").and_then(Value::as_str) {
            Some("succeeded") => TaskState::Completed,
            Some(other) => TaskState::parse(other).ok_or(TaskStoreError::InvalidResult)?,
            None => return Err(TaskStoreError::InvalidResult.into()),
        };
        let committed = match self.journal.transition(id, state, Some(result)) {
            Ok(committed) => committed,
            Err(error) => {
                self.abort_work(id, &error.to_string());
                return Err(error.into());
            }
        };
        committed.result.ok_or(SupervisorError::NoDurableResult)
    }

    /// `agents.cancel`: cancels the task and every unfinished descendant, even when the task itself already ended.
    /// Returns whether the task's own result is a cancellation.
    pub fn cancel(&mut self, caller: Option<&str>, id: &str) -> Result<bool, SupervisorError> {
        self.lookup(caller, id)?;
        let result = self.cancel_tree(id, CANCELLED)?;
        Ok(result["status"] == "cancelled")
    }

    /// Cancel one task, abort its work if this call ended it, then its unfinished children (depth first).
    fn cancel_tree(&mut self, id: &str, reason: &str) -> Result<Value, SupervisorError> {
        let tasks = self.journal.list()?;
        let task = tasks.iter().find(|t| t.id == id).ok_or(SupervisorError::UnknownTask)?;
        let own = match &task.result {
            Some(result) => Ok(result.clone()),
            None => {
                // Only the party that ends the task aborts its work, before the cancellation commits.
                self.abort_work(id, reason);
                self.finish(
                    id,
                    json!({ "status": "cancelled", "error": reason, "verification": "unverified" }),
                )
            }
        };
        let children: Vec<String> = self
            .journal
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|child| child.parent_id.as_deref() == Some(id) && child.result.is_none())
            .map(|child| child.id)
            .collect();
        let cascade = format!("Ancestor {id} cancelled: {reason}");
        for child in children {
            let _ = self.cancel_tree(&child, &cascade);
        }
        own
    }

    /// Closes the owner: every unfinished task is cancelled and later requests are refused. Reports the first
    /// cancellation that failed.
    pub fn close(&mut self) -> Result<(), SupervisorError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        if !self.used {
            return Ok(());
        }
        let unfinished: Vec<String> = match self.journal.list() {
            Ok(tasks) => tasks.into_iter().filter(|t| t.result.is_none()).map(|t| t.id).collect(),
            Err(_) => Vec::new(),
        };
        let mut first = None;
        for id in unfinished {
            if let Err(error) = self.cancel_tree(&id, HOST_CLOSED) {
                first.get_or_insert(error);
            }
        }
        first.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fails every write after the first `ok` writes.
    #[derive(Clone, Default)]
    struct Store {
        value: Arc<Mutex<Option<Value>>>,
        writes: Arc<Mutex<usize>>,
        ok: usize,
    }

    impl TaskDocumentStore for Store {
        fn read(&mut self) -> Result<Option<Value>, String> {
            Ok(self.value.lock().unwrap().clone())
        }

        fn write(&mut self, document: &Value) -> Result<(), String> {
            let mut writes = self.writes.lock().unwrap();
            *writes += 1;
            if *writes > self.ok {
                return Err("write failed".into());
            }
            *self.value.lock().unwrap() = Some(document.clone());
            Ok(())
        }
    }

    fn request() -> SpawnRequest {
        let payload = json!({ "definition": "rlm-child@1", "input": { "prompt": "x" } });
        SpawnRequest::from_payload(payload.as_object().unwrap()).unwrap()
    }

    // NativeRlmHost.close() before any request: its task table was never loaded, so nothing is written.
    #[test]
    fn harness_supervisor_close_before_use_writes_nothing() {
        let seeded = json!({ "version": 1, "tasks": [{
            "id": "t", "key": "k", "fingerprint": "a".repeat(64), "definition": "rlm-child@1", "state": "running"
        }] });
        let store = Store {
            ok: usize::MAX,
            ..Store::default()
        };
        *store.value.lock().unwrap() = Some(seeded.clone());
        let mut host = TaskSupervisor::new(store.clone());
        host.close().unwrap();
        host.close().unwrap();
        assert_eq!(*store.value.lock().unwrap(), Some(seeded));
        assert_eq!(*store.writes.lock().unwrap(), 0);
        assert_eq!(host.status(None), Err(SupervisorError::Closed));
    }

    // NativeRlmHost.run(): a running transition that fails to commit aborts the task's work, and the poisoned
    // journal refuses the owner's later requests.
    #[test]
    fn harness_supervisor_failed_start_aborts_work_and_poisons() {
        let store = Store {
            ok: 1,
            ..Store::default()
        };
        let mut host = TaskSupervisor::new(store.clone());
        let aborted = Arc::new(Mutex::new(Vec::new()));
        let sink = aborted.clone();
        host.on_abort(Box::new(move |id| sink.lock().unwrap().push(id.to_owned())));
        let spawned = host.spawn(None, &request()).unwrap();
        assert_eq!(spawned["state"], "admitted");
        let id = spawned["id"].as_str().unwrap();
        assert_eq!(*aborted.lock().unwrap(), vec![id.to_owned()]);
        assert_eq!(
            host.signal(id).unwrap().reason().as_deref(),
            Some("Task store durability is uncertain; reopen the owner")
        );
        assert_eq!(
            host.status(None),
            Err(SupervisorError::Store(TaskStoreError::Durability))
        );
        assert_eq!(
            host.status(None).unwrap_err().to_string(),
            "Task store durability is uncertain; reopen the owner"
        );
        let stored = store.value.lock().unwrap().clone().unwrap();
        assert_eq!(stored["tasks"][0]["state"], "admitted");
    }

    // A cancellation whose commit fails: the work is aborted first (the canceller ends it) and the failure is
    // reported; the finish failure aborts it again, as NativeRlmHost.finish's catch does.
    #[test]
    fn harness_supervisor_failed_cancel_reports_durability() {
        let store = Store {
            ok: 2,
            ..Store::default()
        };
        let mut host = TaskSupervisor::new(store);
        let count = Arc::new(Mutex::new(0));
        let sink = count.clone();
        host.on_abort(Box::new(move |_| *sink.lock().unwrap() += 1));
        let id = host.spawn(None, &request()).unwrap()["id"].as_str().unwrap().to_owned();
        assert_eq!(
            host.cancel(None, &id),
            Err(SupervisorError::Store(TaskStoreError::Durability))
        );
        assert_eq!(*count.lock().unwrap(), 2);
        assert_eq!(
            host.signal(&id).unwrap().reason().as_deref(),
            Some("Ultron task cancelled")
        );
    }

    #[test]
    fn harness_supervisor_default_request_fingerprint() {
        let request = request();
        assert_eq!(request.timeout_ms, 1_800_000);
        assert_eq!(
            request.fingerprint(),
            task_fingerprint(&json!({
                "definition": "rlm-child@1", "input": { "prompt": "x" }, "model": null, "timeout_ms": 1800000
            }))
        );
    }
}
