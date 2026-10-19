//! A task host reopened after its worker died (Ultron Durable). The journal's tasks become the host's again:
//! finished results stay collectable and listed, and an unfinished subagent that a durable runtime still runs keeps
//! running: its waiter delivers its reply when it ends, and the result is checked and announced as any child's.
//! Unfinished tasks nothing runs read as the journal recovered them (`interrupted`, never replayed).
//!
//! The journal itself follows `task-store.ts`: reopening records every unfinished task as interrupted, which is what
//! the TypeScript product reads. A reattached child's later result is the host's (and the durable runtime's), not a
//! second terminal record.

use super::*;

/// Waits for a reattached child's reply. It is given the task's signal, aborted when the task is cancelled.
pub type ChildWaiter = Box<dyn FnOnce(Signal) -> Result<String, String> + Send>;

/// A child a durable runtime still runs, as its session kept it.
pub struct Reattached {
    pub waiter: ChildWaiter,
    /// Its contract as [`Delegation::contract_of`] gave it when it was spawned (`None`: it had none, or the session
    /// predates kept contracts): the host checks the child's end against it, as before the restart.
    pub contract: Option<Value>,
    /// The run to repeat for a `{"retry": N}` hop (its signal is replaced by the task's); `None`: a failed
    /// contract is reported without a retry.
    pub run: Option<ChildRun>,
}

impl Delegation {
    /// Load the journal's tasks after a restart. `reattach` is asked about every subagent task the journal held
    /// unfinished before it was reopened (`unfinished`, read from the stored document first) and returns a waiter
    /// (with the child's kept contract) for those a durable runtime still runs. Returns the reattached task ids.
    ///
    /// A kept contract that no longer parses fails the child when it ends ("Contract failed: ..."): the host never
    /// lets a contract it cannot read pass as no contract.
    pub fn restore(
        &self,
        unfinished: &[String],
        mut reattach: impl FnMut(&str) -> Option<Reattached>,
    ) -> Result<Vec<String>, String> {
        let records = lock(&self.inner.state).journal.list()?;
        let mut waiters = Vec::new();
        {
            let mut state = lock(&self.inner.state);
            for mut record in records {
                let id = record["id"].as_str().unwrap_or_default().to_string();
                if id.is_empty() || state.task(&id).is_some() {
                    continue;
                }
                let child = record["definition"] == CHILD;
                let reattached = if child && unfinished.contains(&id) {
                    reattach(&id)
                } else {
                    None
                };
                let (waiter, contract, run) = match reattached {
                    Some(r) => (Some(r.waiter), r.contract, r.run),
                    None => (None, None, None),
                };
                // The Git baseline kept beside a contract with workspace checks: their "before" from now on.
                let git_baseline = contract
                    .as_ref()
                    .and_then(|kept| GitBaseline::from_value(&kept["baseline"]));
                let contract = contract.map(|kept| SpawnContract::from_described(&kept).map(Arc::new));
                let signal = Signal::new();
                let outcome = match &waiter {
                    Some(_) => {
                        record["state"] = json!("running");
                        if let Some(fields) = record.as_object_mut() {
                            fields.remove("result");
                        }
                        None
                    }
                    None => {
                        signal.abort("ended");
                        Some(Ok(record["result"].clone()))
                    }
                };
                let lane_name = child.then(|| format!("ultron.rlm-child.{id}"));
                // A reattached worktree child keeps its worktree (its saved record names the task): its work is
                // committed on its branch when it ends, and `rlm.merge` finds it.
                let worktree = waiter
                    .as_ref()
                    .and(self.inner.workspace.as_deref())
                    .and_then(|workspace| {
                        wt::list_records(workspace)
                            .into_iter()
                            .find(|record| record.task_id.as_deref() == Some(id.as_str()))
                    });
                if let (Some(lane), Some(_)) = (&lane_name, &waiter) {
                    state.lane_tasks.insert(lane.clone(), id.clone());
                }
                state.tasks.push(Task {
                    id: id.clone(),
                    parent_id: record["parentId"].as_str().map(String::from),
                    record,
                    outcome,
                    spawn_depth: 0,
                    lane_name,
                    started_at: None,
                    ended_at: None,
                    verdict: None,
                    verdict_rejections: 0,
                    verdict_problems: Vec::new(),
                    verdict_invalid: false,
                    waiters: 0,
                    signal: signal.clone(),
                    finishing: false,
                    worktree,
                    worktree_baseline: None,
                    outside_before: None,
                    before: None,
                    merge_status: None,
                    contract: contract.clone().and_then(Result::ok),
                    git_baseline,
                });
                if let Some(waiter) = waiter {
                    let unreadable = contract.and_then(Result::err);
                    waiters.push((id, signal, waiter, run, unreadable));
                }
            }
        }
        let resumed = waiters.iter().map(|(id, ..)| id.clone()).collect();
        for (id, signal, waiter, run, unreadable) in waiters {
            let host = self.clone();
            std::thread::spawn(move || host.run_restored(&id, &signal, waiter, run, unreadable));
        }
        Ok(resumed)
    }

    fn run_restored(
        &self,
        id: &str,
        signal: &Signal,
        waiter: ChildWaiter,
        run: Option<ChildRun>,
        unreadable: Option<String>,
    ) {
        let reply = waiter(signal.clone());
        if signal.is_aborted() {
            return;
        }
        let mut result = self.reply_result(id, reply);
        let contract = lock(&self.inner.state).task(id).and_then(|t| t.contract.clone());
        if let Some(contract) = contract {
            let run = run.map(|run| ChildRun {
                signal: signal.clone(),
                ..run
            });
            result = match self.enforce_contract(id, &contract, run, result, signal) {
                Some(result) => result,
                None => return,
            };
        } else if let Some(error) = unreadable {
            result["status"] = json!("failed");
            result["error"] = json!(format!(
                "Contract failed: the session's kept contract is unreadable: {error}"
            ));
        }
        // As any child's end (its worktree work committed first), kept by the host: the journal already holds the
        // task as it recovered it.
        let _ = self.end(id, result);
    }
}
