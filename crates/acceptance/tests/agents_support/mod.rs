//! The TS `hostFixture` (packages/coding-agent/test/ultron-host-fixtures.ts) over the Rust typed-agent host
//! (`delegation::agents::Agents`): its task journal, definition registry, usage ledger, strategies and workflows are
//! the real ones; what the TS tests fake, this fakes the same way: a lane is a script from (lane, prompt, the task's
//! abort signal) to the assistant text at the completed tip, and every lane opened, prompt, model and abort is
//! recorded.
#![allow(dead_code)]

use delegation::agents::{Adapter, AdapterRequest, Agents, AgentsOptions, LaneReply, LaneRun, TaskEnd};
use delegation::task_store::TaskStore;
use delegation::usage::{Measurement, UsageLedger};
use kernel::Signal;
use kernel_apis::TaskHost;
use serde_json::{Value, json};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Every durable state each task passed through, by task id.
pub type States = Vec<(String, Vec<String>)>;
/// Usage a lane run reports, by (lane, prompt).
pub type Metered = Arc<dyn Fn(&str, &str) -> Option<Measurement> + Send + Sync>;

/// A session value (`memoryStore()`); clones share it. `states` records every durable state each task passed.
#[derive(Clone, Default)]
pub struct Memory {
    pub value: Arc<Mutex<Option<Value>>>,
    pub states: Arc<Mutex<States>>,
}

impl Memory {
    pub fn document(&self) -> Option<Value> {
        lock(&self.value).clone()
    }
    pub fn states_of(&self, id: &str) -> Vec<String> {
        lock(&self.states)
            .iter()
            .find(|(task, _)| task == id)
            .map(|(_, states)| states.clone())
            .unwrap_or_default()
    }
}

impl TaskStore for Memory {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(lock(&self.value).clone())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        let mut states = lock(&self.states);
        for task in document["tasks"].as_array().into_iter().flatten() {
            let (id, state) = (
                task["id"].as_str().unwrap_or_default(),
                task["state"].as_str().unwrap_or_default(),
            );
            match states.iter_mut().find(|(task, _)| task == id) {
                Some((_, seen)) if seen.last().map(String::as_str) != Some(state) => seen.push(state.into()),
                Some(_) => {}
                None => states.push((id.into(), vec![state.into()])),
            }
        }
        *lock(&self.value) = Some(document.clone());
        Ok(())
    }
}

/// A lane script: (lane, prompt, the task's abort signal, the host) to the reply text, or an error.
pub type Script = Arc<dyn Fn(&str, &str, &Signal, &Agents) -> Result<String, String> + Send + Sync>;

/// Blocks until `signal` aborts; the reply of a hanging lane (`aborted(laneContext)`).
pub fn aborted(signal: &Signal) -> Result<String, String> {
    while !signal.wait(Duration::from_millis(10)) {}
    Err(signal.reason().unwrap_or_else(|| "aborted".into()))
}

#[derive(Default)]
pub struct Options {
    pub script: Option<Script>,
    pub store: Option<Memory>,
    pub definitions: Option<Memory>,
    pub usage: Option<UsageLedger>,
    pub deterministic: Option<Adapter>,
    pub predict: Option<Adapter>,
    /// Usage measured for each lane run (by lane name).
    pub metered: Option<Metered>,
}

pub struct Fixture {
    pub agents: Agents,
    pub events: Receiver<TaskEnd>,
    pub store: Memory,
    pub definitions: Memory,
    /// Lanes opened (`harness.lane(name)`), prompts `(lane, prompt)`, models set `(lane, model)`, lane aborts.
    pub lane_calls: Arc<Mutex<Vec<String>>>,
    pub prompts: Arc<Mutex<Vec<(String, String)>>>,
    pub models: Arc<Mutex<Vec<(String, String)>>>,
    pub aborts: Arc<Mutex<Vec<String>>>,
}

pub fn fixture(mut options: Options) -> Fixture {
    let store = options.store.take().unwrap_or_default();
    let definitions = options.definitions.take().unwrap_or_default();
    let mut fixture = fixture_with_stores(options, Box::new(store.clone()), Box::new(definitions.clone()));
    fixture.store = store;
    fixture.definitions = definitions;
    fixture
}

/// A fixture over the given journal and definition stores (its `store` / `definitions` are unused then).
pub fn fixture_with_stores(options: Options, tasks: Box<dyn TaskStore>, definitions: Box<dyn TaskStore>) -> Fixture {
    let (store, definition_store) = (Memory::default(), Memory::default());
    let lane_calls: Arc<Mutex<Vec<String>>> = Arc::default();
    let prompts: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let models: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let aborts: Arc<Mutex<Vec<String>>> = Arc::default();
    let host: Arc<OnceLock<Agents>> = Arc::default();
    let mut agent_options = AgentsOptions::new(tasks);
    agent_options.definition_store = Some(definitions);
    agent_options.usage = options.usage;
    agent_options.deterministic = options.deterministic;
    agent_options.predict = options.predict;
    let script = options
        .script
        .unwrap_or_else(|| Arc::new(|_, _, _, _| Ok("unused".into())));
    let metered = options.metered;
    let (calls, said, chosen, me) = (lane_calls.clone(), prompts.clone(), models.clone(), host.clone());
    agent_options.lanes = Some(Arc::new(move |run: LaneRun| {
        lock(&calls).push(run.lane.clone());
        if let Some(model) = &run.model {
            lock(&chosen).push((run.lane.clone(), model.clone()));
        }
        lock(&said).push((run.lane.clone(), run.prompt.clone()));
        let agents = me.get().expect("host").clone();
        let text = script(&run.lane, &run.prompt, &run.signal, &agents)?;
        let usage = metered.as_ref().and_then(|m| m(&run.lane, &run.prompt));
        Ok(LaneReply {
            text,
            usage,
            turns: 1,
            tool_calls: 0,
        })
    }));
    let stopped = aborts.clone();
    agent_options.abort_lane = Some(Arc::new(move |lane: &str| lock(&stopped).push(lane.into())));
    let (agents, events) = Agents::new(agent_options);
    let _ = host.set(agents.clone());
    Fixture {
        agents,
        events,
        store,
        definitions: definition_store,
        lane_calls,
        prompts,
        models,
        aborts,
    }
}

impl Fixture {
    /// A request from the root lane.
    pub fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        self.agents.handle(kind, payload, "main", None)
    }

    /// A request from `lane` (a task's lane), as its kernel would make it.
    pub fn call_as(&self, lane: &str, kind: &str, payload: Value) -> Result<Value, String> {
        self.agents.handle(kind, payload, lane, None)
    }

    /// The durable journal, read the way a reopened owner would (`journal(fixture)`).
    pub fn journal(&self) -> Vec<Value> {
        self.call("ping", json!({})).unwrap();
        self.agents.tasks().unwrap()
    }

    pub fn prompt_count(&self) -> usize {
        lock(&self.prompts).len()
    }
}

/// `waitFor(condition)`: polls until it holds, failing after `timeout`.
pub fn wait_for(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "waitFor timed out: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// `definition(id, strategy, overrides)` of the TS fixtures.
pub fn definition(id: &str, strategy: &str, overrides: Value) -> Value {
    let mut out = json!({
        "id": id, "version": "1", "strategy": strategy, "instructions": format!("Run {id}."),
        "inputSchema": { "type": "object", "properties": { "n": { "type": "integer" } }, "required": ["n"],
                         "additionalProperties": false },
        "outputSchema": { "type": "object", "properties": { "doubled": { "type": "integer" } }, "required": ["doubled"],
                          "additionalProperties": false },
        "maxRepairs": 0, "inputDescription": "{n:integer}", "outputDescription": "{doubled:integer}",
    });
    for (key, value) in overrides.as_object().cloned().unwrap_or_default() {
        out[&key] = value;
    }
    out
}

/// The JSON input of a definition prompt (`/Input data:\n(.*)\n/`).
pub fn input_of(prompt: &str) -> Value {
    let at = prompt.find("Input data:\n").expect("a definition prompt");
    serde_json::from_str(prompt[at + 12..].lines().next().unwrap_or_default()).unwrap()
}

/// An adapter from a closure over (definition id, input, repair).
pub fn adapter(
    f: impl Fn(&str, &Value, Option<&Value>, &Signal) -> Result<Value, String> + Send + Sync + 'static,
) -> Adapter {
    Arc::new(move |request: AdapterRequest<'_>| {
        f(
            request.definition["id"].as_str().unwrap_or_default(),
            request.input,
            request.repair.as_ref(),
            request.signal,
        )
    })
}

/// Runs `f` on `n` threads at once and collects the outcomes (`Promise.allSettled` of a burst).
pub fn burst<T: Send + 'static>(n: usize, f: impl Fn(usize) -> T + Send + Sync + 'static) -> Vec<T> {
    let f = Arc::new(f);
    let start = Arc::new(std::sync::Barrier::new(n));
    let threads: Vec<_> = (0..n)
        .map(|i| {
            let (f, start) = (f.clone(), start.clone());
            std::thread::spawn(move || {
                start.wait();
                f(i)
            })
        })
        .collect();
    threads.into_iter().map(|t| t.join().unwrap()).collect()
}
