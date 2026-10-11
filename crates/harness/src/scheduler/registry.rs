//! Task definitions and the extension registry the scheduler resolves them from.
//!
//! Port of upstream `harness/define.ts` (`defineTask`), the task part of `harness/registry.ts`, and `TaskDefinition`
//! of `types.ts`. Tools are kept only as names (the tool runtime is not ported); built-in tasks (generation, tool,
//! compaction) are not registered because their runtimes are not ported.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use durable::{TaskRecord, TaskState};
use serde_json::Value;

use super::cancel::lock;
use super::{HarnessError, TaskRuntime};

/// A phase or abort handler: runs off the Session line with the running task and its invocation's runtime. Its
/// context is `runtime.ctx()`, cancelled by the invocation's signal.
pub type Handler = Arc<dyn Fn(&TaskRecord, &TaskRuntime) -> Result<(), HarnessError> + Send + Sync>;
/// `migrate(input, checkpoint, fromVersion) -> (input, checkpoint)`, run at reservation.
pub type Migrate = Arc<dyn Fn(&Value, &Value, u64) -> Result<(Value, Value), HarnessError> + Send + Sync>;

struct TaskInner {
    definition: durable::TaskDefinition,
    phases: HashMap<String, Handler>,
    abort: Handler,
    migrate: Option<Migrate>,
}

/// An executable task definition. Identity matters: a registry snapshot that resolves a kind to another `Task`
/// (by pointer) is a replacement definition.
#[derive(Clone)]
pub struct Task(Arc<TaskInner>);

impl std::fmt::Debug for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Task({} v{})", self.name(), self.version())
    }
}

impl Task {
    /// `defineTask({ name, version, initial, phases, abort })`; add phases with [`TaskBuilder::phase`].
    pub fn define(name: &str, version: u64, initial: impl Fn(&Value) -> Value + Send + Sync + 'static) -> TaskBuilder {
        TaskBuilder {
            name: name.to_owned(),
            version,
            initial: Arc::new(initial),
            phases: HashMap::new(),
            abort: None,
            migrate: None,
        }
    }

    pub fn name(&self) -> &str {
        &self.0.definition.name
    }

    pub fn version(&self) -> u64 {
        self.0.definition.version
    }

    /// The Session-facing definition, for `Tx::create_task`.
    pub fn definition(&self) -> &durable::TaskDefinition {
        &self.0.definition
    }

    pub fn same(&self, other: &Task) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub(crate) fn phase(&self, name: &str) -> Option<&Handler> {
        self.0.phases.get(name)
    }

    pub(crate) fn abort_handler(&self) -> &Handler {
        &self.0.abort
    }

    pub(crate) fn migration(&self) -> Option<&Migrate> {
        self.0.migrate.as_ref()
    }
}

pub struct TaskBuilder {
    name: String,
    version: u64,
    initial: Arc<dyn Fn(&Value) -> Value + Send + Sync>,
    phases: HashMap<String, Handler>,
    abort: Option<Handler>,
    migrate: Option<Migrate>,
}

impl TaskBuilder {
    pub fn phase(
        mut self,
        name: &str,
        handler: impl Fn(&TaskRecord, &TaskRuntime) -> Result<(), HarnessError> + Send + Sync + 'static,
    ) -> Self {
        self.phases.insert(name.to_owned(), Arc::new(handler));
        self
    }

    /// Runs in a fresh invocation after an abort mark and must commit a terminal outcome.
    pub fn abort(
        mut self,
        handler: impl Fn(&TaskRecord, &TaskRuntime) -> Result<(), HarnessError> + Send + Sync + 'static,
    ) -> Self {
        self.abort = Some(Arc::new(handler));
        self
    }

    pub fn migrate(
        mut self,
        migrate: impl Fn(&Value, &Value, u64) -> Result<(Value, Value), HarnessError> + Send + Sync + 'static,
    ) -> Self {
        self.migrate = Some(Arc::new(migrate));
        self
    }

    pub fn build(self) -> Task {
        let initial = self.initial.clone();
        Task(Arc::new(TaskInner {
            definition: durable::TaskDefinition::new(&self.name, self.version, move |input| initial(input)),
            phases: self.phases,
            abort: self.abort.unwrap_or_else(|| Arc::new(|_, _| Ok(()))),
            migrate: self.migrate,
        }))
    }
}

/// An installable unit: tasks and (as names only) tools.
#[derive(Clone, Debug, Default)]
pub struct Extension {
    pub name: String,
    pub tasks: Vec<Task>,
    pub tools: Vec<String>,
}

impl Extension {
    pub fn tasks(name: &str, tasks: Vec<Task>) -> Extension {
        Extension {
            name: name.to_owned(),
            tasks,
            tools: Vec::new(),
        }
    }
}

/// Immutable published registry state.
#[derive(Debug, Default)]
pub struct RegistrySnapshot {
    extensions: Vec<Extension>,
    tasks: HashMap<String, Task>,
}

impl RegistrySnapshot {
    fn new(extensions: Vec<Extension>) -> Result<RegistrySnapshot, HarnessError> {
        let mut tasks = HashMap::new();
        for extension in &extensions {
            for task in &extension.tasks {
                if tasks.insert(task.name().to_owned(), task.clone()).is_some() {
                    return Err(HarnessError::Error(format!(
                        "Task {} of extension {} is already installed",
                        task.name(),
                        extension.name
                    )));
                }
            }
        }
        Ok(RegistrySnapshot { extensions, tasks })
    }

    pub fn task(&self, name: &str) -> Option<&Task> {
        self.tasks.get(name)
    }

    pub fn installed(&self) -> &[Extension] {
        &self.extensions
    }

    /// Tool names in extension order.
    pub fn tools(&self) -> Vec<String> {
        self.extensions
            .iter()
            .flat_map(|extension| extension.tools.iter().cloned())
            .collect()
    }
}

/// What the scheduler reads: the current snapshot, and a change notification.
pub trait RegistryReader: Send + Sync {
    fn snapshot(&self) -> Arc<RegistrySnapshot>;
    /// Register a change listener; the returned function unsubscribes.
    fn subscribe(&self, listener: Box<dyn Fn() + Send + Sync>) -> Box<dyn FnOnce() + Send>;
}

type Listener = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct RegistryState {
    current: Arc<RegistrySnapshot>,
    next: u64,
    listeners: Vec<(u64, Listener)>,
}

/// Application-owned registry (upstream `createRegistry()`); clones share it.
#[derive(Clone, Default)]
pub struct Registry(Arc<Mutex<RegistryState>>);

impl Registry {
    pub fn new() -> Registry {
        Registry::default()
    }

    /// Install an extension; one with the same name is replaced in place. Listeners run synchronously.
    pub fn install(&self, extension: Extension) -> Result<(), HarnessError> {
        let listeners = {
            let mut state = lock(&self.0);
            let mut next = state.current.extensions.clone();
            match next.iter().position(|installed| installed.name == extension.name) {
                Some(index) => next[index] = extension,
                None => next.push(extension),
            }
            state.current = Arc::new(RegistrySnapshot::new(next)?);
            state
                .listeners
                .iter()
                .map(|(_, listener)| listener.clone())
                .collect::<Vec<_>>()
        };
        for listener in listeners {
            listener();
        }
        Ok(())
    }

    pub fn uninstall(&self, name: &str) {
        let listeners = {
            let mut state = lock(&self.0);
            if !state.current.extensions.iter().any(|installed| installed.name == name) {
                return;
            }
            let next = state
                .current
                .extensions
                .iter()
                .filter(|installed| installed.name != name)
                .cloned()
                .collect();
            state.current = Arc::new(RegistrySnapshot::new(next).expect("a subset installs"));
            state
                .listeners
                .iter()
                .map(|(_, listener)| listener.clone())
                .collect::<Vec<_>>()
        };
        for listener in listeners {
            listener();
        }
    }
}

impl RegistryReader for Registry {
    fn snapshot(&self) -> Arc<RegistrySnapshot> {
        lock(&self.0).current.clone()
    }

    fn subscribe(&self, listener: Box<dyn Fn() + Send + Sync>) -> Box<dyn FnOnce() + Send> {
        let mut state = lock(&self.0);
        state.next += 1;
        let id = state.next;
        state.listeners.push((id, Arc::from(listener)));
        let registry = self.0.clone();
        Box::new(move || lock(&registry).listeners.retain(|(each, _)| *each != id))
    }
}

/// The phase name of a pending, running, or waiting record's checkpoint.
pub(crate) fn phase_of(record: &TaskRecord) -> Option<&str> {
    match &record.state {
        TaskState::Pending { checkpoint }
        | TaskState::Running { checkpoint }
        | TaskState::Waiting { checkpoint, .. } => checkpoint.get("phase").and_then(Value::as_str),
        _ => None,
    }
}
