//! What a Claude Code subagent costs, charged as native-host.ts charges an external child (`externalChild`): before
//! it starts, a model reservation on the root its spawn belongs to (refused once that root's turn, token or cost
//! budget is spent); when it ends, the usage `claude -p` reported settles the reservation and its model turns count
//! toward the root's limits (its tokens and cost on the first), and its model is kept in the session's counters.
//! The ledger and the task journal are the session's own values, so `ultron usage` and `usage.report` see them.

use crate::child::ChildResult;
use delegation::usage::{Measurement, ReserveRequest, UsageLedger};
use session_core::session_stats::SessionStats;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// The usage root of each subagent, kept when `rlm.spawn` admits it: the turn the root lane spawned it in, else its
/// parent task's root.
#[derive(Default)]
pub struct ChildRoots(Mutex<HashMap<String, Option<String>>>);

impl ChildRoots {
    /// Task `task` was admitted; `turn_root` is the root of the root's turn now open (if one is).
    pub fn spawned(&self, task: &str, parent: Option<&str>, turn_root: Option<String>) {
        let mut roots = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let root = match parent {
            Some(parent) => roots.get(parent).cloned().flatten(),
            None => turn_root,
        };
        roots.insert(task.to_string(), root);
    }

    /// The root task `task` is charged to (none: the ledger's default root).
    pub fn root_of(&self, task: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(task)
            .cloned()
            .flatten()
    }
}

/// One subagent run as the ledger and the counters see it.
pub struct Charge<'a> {
    pub usage: &'a UsageLedger,
    pub stats: &'a Arc<SessionStats>,
    pub root: Option<String>,
    pub task_id: &'a str,
    pub timeout_ms: u64,
    /// `claude-code/<model>` the child is started on.
    pub model: String,
}

/// Run `child` under `charge`'s root: admitted first (a refused reservation is the child's failure), settled with
/// what it reported, its turns counted. `cancelled` tells a cancelled run from a failed one.
pub fn charged(
    charge: &Charge,
    cancelled: &dyn Fn() -> bool,
    child: impl FnOnce() -> Result<ChildResult, String>,
) -> Result<ChildResult, String> {
    charge.stats.child_model(charge.task_id, &charge.model);
    let reservation = charge.usage.reserve(&ReserveRequest {
        kind: "model".into(),
        root_id: charge.root.clone(),
        task_id: Some(charge.task_id.to_string()),
        parent_task_id: Some(charge.task_id.to_string()),
        request_key: Some(format!("{}:model", charge.task_id)),
        timeout_ms: Some(charge.timeout_ms as f64),
        ..ReserveRequest::default()
    })?;
    let result = child();
    let measured = result.as_ref().ok().map(|done| Measurement {
        input_tokens: Some(done.input_tokens),
        output_tokens: Some(done.output_tokens),
        total_tokens: Some(done.input_tokens + done.output_tokens),
        cost: done.cost,
        wall_ms: done.wall_ms,
    });
    let status = match (&result, cancelled()) {
        (_, true) => "cancelled",
        (Ok(_), false) => "succeeded",
        (Err(_), false) => "failed",
    };
    if let Ok(done) = &result
        && let Some(model) = &done.model
    {
        charge.stats.child_model(charge.task_id, model);
    }
    let _ = charge.usage.settle(&reservation, Some(status), measured.as_ref());
    let turns = result.as_ref().map_or(0, |done| done.turns);
    for index in 0..turns.max(1) {
        let (tokens, cost) = match (index, &measured) {
            (0, Some(measured)) => (measured.total_tokens, measured.cost),
            (0, None) => (None, None),
            _ => (Some(0.0), Some(0.0)),
        };
        let _ = charge.usage.record_turn(charge.root.as_deref(), tokens, cost);
    }
    result
}
