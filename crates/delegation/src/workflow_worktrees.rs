//! `rlm-child@1` workflow nodes with `worktree=True` / `"auto"` (native-host.ts `workflowTask` with `newWorktree`):
//! each runs as a subagent of the workflow's caller in the subagent host ([`Delegation`]), which creates its
//! worktree when the node starts, runs its setup, commits what it changed when it ends (the result's `worktree`),
//! and lets the caller merge it with `rlm.merge` like any other worktree child.

use crate::agents::WorktreeChildren;
use crate::host::Delegation;
use kernel::Signal;
use serde_json::{Map, Value, json};

impl WorktreeChildren for Delegation {
    fn spawn(
        &self,
        lane: &str,
        name: &str,
        prompt: &str,
        model: Option<&str>,
        timeout_ms: u64,
        worktree: &Value,
        extra: &Map<String, Value>,
    ) -> Result<String, String> {
        let mut kwargs = json!({ "name": name, "timeout_ms": timeout_ms, "worktree": worktree });
        for (key, value) in extra {
            kwargs[key] = value.clone();
        }
        if let Some(model) = model {
            kwargs["model"] = json!(model);
        }
        let reply = self.handle("rlm.spawn", json!({ "prompt": prompt, "kwargs": kwargs }), lane)?;
        reply["rlm_child_id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "rlm.spawn returned no child id".to_string())
    }

    fn result(&self, id: &str, cell: Option<&Signal>) -> Value {
        let ids = [id.to_string()];
        if let Some((_, outcome)) = self.wait_until(&ids, None, cell).pop() {
            return outcome
                .unwrap_or_else(|error| json!({ "status": "failed", "error": error, "verification": "unverified" }));
        }
        // The requesting cell was cancelled: so is the node's subagent (TS: the request context's abort).
        let reason = cell
            .and_then(Signal::reason)
            .unwrap_or_else(|| "the calling cell was aborted".into());
        match self.cancel(id, &reason) {
            Ok(result) => result,
            Err(error) => json!({ "status": "cancelled", "error": error, "verification": "unverified" }),
        }
    }

    fn agents_request(&self, kind: &str, id: &str, lane: &str, cell: Option<&Signal>) -> Option<Result<Value, String>> {
        Delegation::agents_request(self, kind, id, lane, cell)
    }
}
