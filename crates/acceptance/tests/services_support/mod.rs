//! The TS `NativeRlmHost` with local services, as the local-service acceptance rows build it, over the Rust stack:
//! the typed-agent host (`delegation::agents::Agents`, its journal, definition registry, optional usage ledger,
//! adapters and the refinement hook of `NativeHostOptions.refinements`) answers `agents.*` / `workflows.run`, and
//! `kernel_apis::KernelApis` routes every other request to the host modules (progress, schedules and goals, grants,
//! release gates, local services, memory) exactly as a session's cell handler does. Lanes are faked as the TS
//! fixtures fake them: a script from the lane and its prompt to the reply, every prompt recorded.
#![allow(dead_code)]

use delegation::agents::{Adapter, Agents, AgentsOptions, LaneReply, LaneRun, RefinementsHook, TaskEnd};
use delegation::task_store::{MemoryStore, TaskStore};
use delegation::usage::UsageLedger;
use kernel::{HostHandler, Signal};
use kernel_apis::artifacts::{KeyedDocuments, MemoryKeyedDocuments};
use kernel_apis::{ApiModule, KernelApis};
use serde_json::{Map, Value};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// A lane script: (lane, prompt, the task's abort signal) to the reply text, or an error.
pub type Script = Arc<dyn Fn(&str, &str, &Signal) -> Result<String, String> + Send + Sync>;

pub struct Setup {
    pub tasks: Box<dyn TaskStore>,
    pub definitions: Box<dyn TaskStore>,
    pub usage: Option<UsageLedger>,
    pub deterministic: Option<Adapter>,
    pub predict: Option<Adapter>,
    pub script: Option<Script>,
    pub refinements: Option<RefinementsHook>,
    pub modules: Vec<Arc<dyn ApiModule>>,
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            tasks: Box::new(MemoryStore::default()),
            definitions: Box::new(MemoryStore::default()),
            usage: None,
            deterministic: None,
            predict: None,
            script: None,
            refinements: None,
            modules: Vec::new(),
        }
    }
}

pub struct Host {
    pub agents: Agents,
    pub apis: Arc<KernelApis>,
    pub events: Receiver<TaskEnd>,
    /// `(lane, prompt)` of every lane run, in order.
    pub prompts: Arc<Mutex<Vec<(String, String)>>>,
}

/// Every request the stack does not serve: the TS host's "not wired" refusal.
pub fn unwired() -> Arc<dyn HostHandler> {
    Arc::new(|kind: &str, _: Map<String, Value>, _: &Signal| {
        Err::<Value, String>(format!("Ultron RLM host request is not wired: {kind}"))
    })
}

pub fn build(setup: Setup) -> Host {
    let prompts: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let mut options = AgentsOptions::new(setup.tasks);
    options.definition_store = Some(setup.definitions);
    options.usage = setup.usage;
    options.deterministic = setup.deterministic;
    options.predict = setup.predict;
    options.refinements = setup.refinements;
    let script = setup.script.unwrap_or_else(|| Arc::new(|_, _, _| Ok("unused".into())));
    let said = prompts.clone();
    options.lanes = Some(Arc::new(move |run: LaneRun| {
        said.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((run.lane.clone(), run.prompt.clone()));
        Ok(LaneReply {
            text: script(&run.lane, &run.prompt, &run.signal)?,
            usage: None,
            turns: 1,
            tool_calls: 0,
        })
    }));
    let (agents, events) = Agents::new(options);
    let mut apis =
        KernelApis::new(Arc::new(agents.handler("main", unwired()))).with_host(Arc::new(agents.clone()), "main");
    for module in setup.modules {
        apis = apis.with(module);
    }
    Host {
        agents,
        apis: Arc::new(apis),
        events,
        prompts,
    }
}

impl Host {
    /// A request from the root lane, as a cell makes it.
    pub fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        let Value::Object(payload) = payload else {
            panic!("payload must be an object")
        };
        self.apis.handle(kind, payload, &Signal::new())
    }

    pub fn last_prompt(&self) -> String {
        self.prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last()
            .map(|(_, prompt)| prompt.clone())
            .unwrap_or_default()
    }

    pub fn close(&self) {
        self.agents.close();
    }
}

/// A JavaScript `Map` of keyed documents shared with the test (`documents()` of the TS tests).
#[derive(Clone, Default)]
pub struct Documents(pub Arc<MemoryKeyedDocuments>);

impl Documents {
    pub fn keys(&self) -> Vec<String> {
        self.0.list("").unwrap().into_iter().map(|(key, _)| key).collect()
    }
}

impl KeyedDocuments for Documents {
    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        self.0.get(key)
    }
    fn set(&self, key: &str, value: &Value) -> Result<(), String> {
        self.0.set(key, value)
    }
    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String> {
        self.0.list(prefix)
    }
}

/// Keyed documents that refuse every access (a session-value namespace that is down).
pub struct Down(pub &'static str);

impl KeyedDocuments for Down {
    fn get(&self, _: &str) -> Result<Option<Value>, String> {
        Err(self.0.into())
    }
    fn set(&self, _: &str, _: &Value) -> Result<(), String> {
        Err(self.0.into())
    }
    fn list(&self, _: &str) -> Result<Vec<(String, Value)>, String> {
        Err(self.0.into())
    }
}

/// A host set once its builder has it (for lane scripts that call back into the host).
pub type Later = Arc<OnceLock<Agents>>;

pub fn rejects(outcome: Result<Value, String>, text: &str) {
    match outcome {
        Err(error) => assert!(error.contains(text), "{error:?} lacks {text:?}"),
        Ok(value) => panic!("expected an error containing {text:?}, got {value}"),
    }
}
