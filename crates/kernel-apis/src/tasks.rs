//! The task operations host modules use (TS `NativeHostApi` of `ultron/rlm/host-module.ts`). Tasks and results are
//! the TS JSON shapes (`NativeTask`, `NativeResult` of `ultron/rlm/task-store.ts`) so modules port unchanged.
//! Identity comes from the calling lane, never from a payload.

use kernel::Signal;
use serde_json::Value;

pub trait TaskHost: Send + Sync {
    /// Task that owns `lane`, or `None` for the root lane.
    fn caller_task_id(&self, lane: &str) -> Option<String>;
    /// Lane a live task ran on, if any.
    fn task_lane(&self, task_id: &str) -> Option<String>;
    /// Execution strategy of a registered definition ("deterministic" | "predict" | "rlm").
    fn strategy(&self, definition: &str) -> Result<String, String>;
    /// Every task of the session (`NativeTask` objects: id, parentId, definition, state, ...).
    fn tasks(&self) -> Result<Vec<Value>, String>;
    /// Admits `request` (`{definition, input, model?, key?, timeoutMs?, lane?}`) as a child of `parent`.
    fn spawn(&self, request: &Value, parent: Option<&str>, signal: &Signal) -> Result<Value, String>;
    /// Blocks until the task's durable terminal result (`NativeResult`).
    fn result(&self, task_id: &str) -> Result<Value, String>;
    fn cancel(&self, task_id: &str, reason: &str) -> Result<Value, String>;
    /// Queues a steering message into a running task's lane; false when it has no live lane.
    fn steer(&self, task_id: &str, message: &str, signal: &Signal) -> Result<bool, String>;
    /// Usage status of the caller's root.
    fn usage(&self) -> Result<Option<Value>, String> {
        Ok(None)
    }
    fn now(&self) -> i64;
    /// Keeps a lane's Python kernel alive for `holder`; false when the host has no pool or no pin capacity left.
    fn pin_lane(&self, _lane: &str, _holder: &str) -> bool {
        false
    }
    fn unpin_lane(&self, _lane: &str, _holder: &str) {}
}

/// A host with only the root lane and no task runtime: every task operation that needs one is refused.
pub struct RootOnly {
    pub now: crate::util::Clock,
}

const NO_TASKS: &str = "Ultron task runtime is not available in this host";

impl TaskHost for RootOnly {
    fn caller_task_id(&self, _lane: &str) -> Option<String> {
        None
    }
    fn task_lane(&self, _task_id: &str) -> Option<String> {
        None
    }
    fn strategy(&self, definition: &str) -> Result<String, String> {
        Err(format!("Unknown Ultron definition: {definition}"))
    }
    fn tasks(&self) -> Result<Vec<Value>, String> {
        Ok(Vec::new())
    }
    fn spawn(&self, _request: &Value, _parent: Option<&str>, _signal: &Signal) -> Result<Value, String> {
        Err(NO_TASKS.into())
    }
    fn result(&self, _task_id: &str) -> Result<Value, String> {
        Err("Unknown Ultron task".into())
    }
    fn cancel(&self, _task_id: &str, _reason: &str) -> Result<Value, String> {
        Err("Unknown Ultron task".into())
    }
    fn steer(&self, _task_id: &str, _message: &str, _signal: &Signal) -> Result<bool, String> {
        Ok(false)
    }
    fn now(&self) -> i64 {
        (self.now)()
    }
}
