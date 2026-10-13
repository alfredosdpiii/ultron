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

impl Delegation {
    /// Load the journal's tasks after a restart. `reattach` is asked about every subagent task the journal held
    /// unfinished before it was reopened (`unfinished`, read from the stored document first) and returns a waiter
    /// for those a durable runtime still runs. Returns the reattached task ids.
    pub fn restore(
        &self,
        unfinished: &[String],
        mut reattach: impl FnMut(&str) -> Option<ChildWaiter>,
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
                let waiter = if child && unfinished.contains(&id) {
                    reattach(&id)
                } else {
                    None
                };
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
                    worktree: None,
                    worktree_baseline: None,
                    outside_before: None,
                    before: None,
                    merge_status: None,
                });
                if let Some(waiter) = waiter {
                    waiters.push((id, signal, waiter));
                }
            }
        }
        let resumed = waiters.iter().map(|(id, _, _)| id.clone()).collect();
        for (id, signal, waiter) in waiters {
            let host = self.clone();
            std::thread::spawn(move || host.run_restored(&id, &signal, waiter));
        }
        Ok(resumed)
    }

    fn run_restored(&self, id: &str, signal: &Signal, waiter: ChildWaiter) {
        let reply = waiter(signal.clone());
        if signal.is_aborted() {
            return;
        }
        let result = match reply {
            Ok(text) => self.child_result(id, &text),
            Err(error) => Err(error),
        };
        let result =
            result.unwrap_or_else(|error| json!({ "status": "failed", "error": error, "verification": "unverified" }));
        self.end_restored(id, result);
    }

    /// End a reattached child: its result becomes the task's outcome and its end is announced (`child_done`).
    fn end_restored(&self, id: &str, result: Value) {
        let event = {
            let mut state = lock(&self.inner.state);
            let closed = state.closed;
            let owner_lane = state
                .task(id)
                .and_then(|t| t.parent_id.clone())
                .and_then(|p| state.task(&p).and_then(|t| t.lane_name.clone()))
                .unwrap_or_else(|| "main".into());
            let now = (self.inner.now)();
            let Some(task) = state.task_mut(id) else { return };
            if task.outcome.is_some() {
                return;
            }
            let terminal = match result["status"].as_str() {
                Some("succeeded") => "completed",
                Some(other) => other,
                None => "failed",
            };
            task.record["state"] = json!(terminal);
            task.record["result"] = result.clone();
            task.ended_at.get_or_insert(now);
            task.signal.abort("ended");
            task.outcome = Some(Ok(result));
            self.inner.changed.notify_all();
            (!closed).then(|| DetachedEnd {
                task: task.record.clone(),
                kind: "child_done",
                owner_lane,
                awaited: task.waiters > 0,
                fetch: format!("await rlm.collect([\"{id}\"])"),
            })
        };
        if let Some(event) = event {
            let _ = self
                .inner
                .events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .send(event);
        }
    }
}
