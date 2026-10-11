//! Host operations behind the thin Python kernel APIs, served as one [`kernel::HostHandler`]:
//!
//! - `skills.*`: the version-pinned skill catalog (refresh, list, select, load, why; invoke is validated but needs
//!   task spawning) and code skills (propose_code / propose_policy with test gating in a fresh kernel, rollback,
//!   code_list, code_history), ported from `ultron/skills.ts` and `ultron/code-skills.ts`;
//! - `memory.*`: prepare / propose / correct / get / forget / why / list over a Hindsight-shaped backend, ported from
//!   `ultron/memory.ts` and the dispatch of `ultron/worker-services.ts`;
//! - `grants.*`: scoped approvals (`ultron/grants.ts`);
//! - `hints.*`: per-lane hint mutes (`ultron/rlm/hints.ts`).
//!
//! Modules are [`ApiModule`]s, matched by request-type prefix as TS host modules are; every other request goes to
//! the inner handler. Modules start once, on the first request of any type. Requests carry the calling lane, the
//! task that owns it (from the [`TaskHost`]) and the host's task operations.

pub mod code_skills;
pub mod context_control;
pub mod family;
pub mod goal;
pub mod grants;
pub mod hints;
pub mod instances;
pub mod memory;
pub mod progress;
pub mod schedules;
pub mod skills;
pub mod store;
pub mod tasks;
pub mod util;

use kernel::{HostHandler, Signal};
use serde_json::{Map, Value};
use std::sync::{Arc, Mutex};

pub use code_skills::{CodeSkillKind, CodeSkills, CodeSkillsOptions};
pub use context_control::{ContextBranches, ContextControl};
pub use family::{FamilyModule, FamilyOptions};
pub use goal::{GoalDriver, GoalJobs};
pub use grants::GrantsModule;
pub use hints::HintsModule;
pub use instances::InstancesModule;
pub use memory::{MemoryApi, MemoryBackend, MemoryError, MemoryService, worker_gate};
pub use progress::ProgressModule;
pub use schedules::SchedulesModule;
pub use skills::{SkillLoader, SkillSource, SkillsModule};
pub use store::{DocumentStore, FileDocumentStore, MemoryDocumentStore};
pub use tasks::{RootOnly, TaskHost};

/// One host request as a module sees it (TS `HostModuleRequest` plus the `NativeHostApi`).
pub struct Request<'a> {
    pub kind: &'a str,
    pub payload: &'a Map<String, Value>,
    /// Lane whose kernel sent the request; the root agent is "main".
    pub lane: &'a str,
    /// Task that owns the lane, `None` for the root lane.
    pub caller_task: Option<&'a str>,
    pub host: &'a dyn TaskHost,
    /// Aborted when the requesting cell ends.
    pub signal: &'a Signal,
}

/// A host module: owns every request type that starts with one of its prefixes.
pub trait ApiModule: Send + Sync {
    fn prefixes(&self) -> &[&str];
    /// Called once before the first request is served.
    fn start(&self, _host: &dyn TaskHost) -> Result<(), String> {
        Ok(())
    }
    fn handle(&self, request: &Request<'_>) -> Result<Value, String>;
}

pub struct KernelApis {
    inner: Arc<dyn HostHandler>,
    host: Arc<dyn TaskHost>,
    lane: String,
    modules: Vec<Arc<dyn ApiModule>>,
    /// The outcome of starting the modules, kept like TS keeps its start promise.
    started: Mutex<Option<Result<(), String>>>,
}

impl KernelApis {
    /// Serves the root lane ("main") of a host without a task runtime; see [`KernelApis::with_host`].
    pub fn new(inner: Arc<dyn HostHandler>) -> Self {
        Self {
            inner,
            host: Arc::new(RootOnly {
                now: util::system_clock(),
            }),
            lane: "main".into(),
            modules: Vec::new(),
            started: Mutex::new(None),
        }
    }

    /// The task operations modules use, and the lane whose kernel this handler serves.
    pub fn with_host(mut self, host: Arc<dyn TaskHost>, lane: impl Into<String>) -> Self {
        self.host = host;
        self.lane = lane.into();
        self
    }

    pub fn with(mut self, module: Arc<dyn ApiModule>) -> Self {
        self.modules.push(module);
        self
    }

    fn start(&self) -> Result<(), String> {
        let mut started = util::lock(&self.started);
        started
            .get_or_insert_with(|| {
                self.modules
                    .iter()
                    .try_for_each(|module| module.start(self.host.as_ref()))
            })
            .clone()
    }
}

impl HostHandler for KernelApis {
    fn handle(&self, kind: &str, payload: Map<String, Value>, signal: &Signal) -> Result<Value, String> {
        self.start()?;
        let module = self
            .modules
            .iter()
            .find(|module| module.prefixes().iter().any(|prefix| kind.starts_with(prefix)));
        let Some(module) = module else {
            return self.inner.handle(kind, payload, signal);
        };
        let caller = self.host.caller_task_id(&self.lane);
        module.handle(&Request {
            kind,
            payload: &payload,
            lane: &self.lane,
            caller_task: caller.as_deref(),
            host: self.host.as_ref(),
            signal,
        })
    }
}
