//! The delegation host's view of the one task journal. The journal itself (`task-store.ts`: validation, bounded
//! persistence, recovery that interrupts unfinished tasks, the error texts) lives once, in `harness::task_store`;
//! this module keeps the JSON-valued API `host.rs` uses and the boxed [`TaskStore`] it is configured with. The
//! durable runtime imports these documents with `harness::chat::migrate::import_task_journal`.

pub use harness::task_store::{MAX_PERSISTED_FINISHED_TASKS, task_fingerprint};
use harness::task_store::{TaskDocumentStore, TaskState};
use serde_json::Value;

/// Atomic document replacement (`NativeHostStore`).
pub trait TaskStore: Send {
    fn read(&mut self) -> Result<Option<Value>, String>;
    fn write(&mut self, document: &Value) -> Result<(), String>;
}

/// An in-memory store (the session value in tests).
#[derive(Default)]
pub struct MemoryStore {
    pub value: Option<Value>,
}

impl TaskStore for MemoryStore {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(self.value.clone())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        self.value = Some(document.clone());
        Ok(())
    }
}

pub fn is_terminal(state: &str) -> bool {
    TaskState::parse(state).is_some_and(TaskState::is_terminal)
}

/// The tasks to persist: every unfinished task, and the newest finished ones up to the bound (the rule of
/// `harness::task_store::persisted_tasks`, on raw records).
pub fn persisted_tasks(records: &[Value]) -> Vec<Value> {
    let terminal = |record: &Value| is_terminal(record["state"].as_str().unwrap_or_default());
    let finished = records.iter().filter(|record| terminal(record)).count();
    let mut excess = finished.saturating_sub(MAX_PERSISTED_FINISHED_TASKS);
    records
        .iter()
        .filter(|record| {
            if excess > 0 && terminal(record) {
                excess -= 1;
                return false;
            }
            true
        })
        .cloned()
        .collect()
}

struct Store(Box<dyn TaskStore>);

impl TaskDocumentStore for Store {
    fn read(&mut self) -> Result<Option<Value>, String> {
        self.0.read()
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        self.0.write(document)
    }
}

/// `NativeTaskJournal`, synchronous: callers serialize access (the host keeps it behind a mutex).
pub struct TaskJournal(harness::task_store::TaskJournal<Store>);

impl TaskJournal {
    pub fn new(store: Box<dyn TaskStore>) -> Self {
        TaskJournal(harness::task_store::TaskJournal::new(Store(store)))
    }

    pub fn list(&mut self) -> Result<Vec<Value>, String> {
        let tasks = self.0.list().map_err(|error| error.to_string())?;
        Ok(tasks.iter().map(|task| self.0.stored_json(task)).collect())
    }

    pub fn find(&mut self, id: &str) -> Option<Value> {
        self.list().ok()?.into_iter().find(|task| task["id"] == id)
    }

    /// Admit a task; returns `(task, created)`. A known key with the same fingerprint returns the earlier task.
    pub fn admit(
        &mut self,
        definition: &str,
        fingerprint: &str,
        key: &str,
        parent_id: Option<&str>,
    ) -> Result<(Value, bool), String> {
        let (task, created) = self
            .0
            .admit(definition, fingerprint, key, parent_id)
            .map_err(|error| error.to_string())?;
        Ok((self.0.stored_json(&task), created))
    }

    pub fn transition(&mut self, id: &str, state: &str, result: Option<Value>) -> Result<Value, String> {
        self.0.ready().map_err(|error| error.to_string())?;
        if id.is_empty() {
            return Err("task ID must be nonempty".into());
        }
        let state = TaskState::parse(state).ok_or("Invalid task state")?;
        let task = self
            .0
            .transition(id, state, result)
            .map_err(|error| error.to_string())?;
        Ok(self.0.stored_json(&task))
    }
}

/// A random version-4 UUID (`randomUUID`).
pub fn uuid_v4() -> String {
    harness::task_store::random_uuid()
}
