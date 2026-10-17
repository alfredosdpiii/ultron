//! The task runtime a phase or abort handler receives: gated commits, first-writer-wins memos, durable sleeps,
//! committed reads, waits, and owned-task aborts, all bound to one invocation (upstream `TaskScheduler.#runtime`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use durable::{Doc, JoinPolicy, Owner, TaskId, TaskOutcome, TaskRecord, TaskState, TaskStatus, Tx, TxScope};
use serde_json::Value;

use super::cancel::{Cancel, Ctx, lock};
use super::registry::RegistrySnapshot;
use super::{HarnessError, Invocation, Mode, PhaseState, Result, Scheduler, closed_error, commit_on};

/// Longest single timer wait; longer sleeps wait in several steps.
const MAX_TIMER_DELAY_MS: u64 = 2_147_483_647;

/// The next state a runtime commit gives its running task.
#[derive(Clone, Debug, PartialEq)]
pub enum NextState {
    Running {
        checkpoint: Value,
    },
    Waiting {
        checkpoint: Value,
        on: Vec<TaskId>,
        policy: JoinPolicy,
    },
    Terminal {
        outcome: TaskOutcome,
    },
}

/// Next state that completes a task with `result`.
pub fn completed(result: Value) -> NextState {
    NextState::Terminal {
        outcome: TaskOutcome::Completed { result },
    }
}

/// Next state that aborts a task with `reason`.
pub fn aborted_with(reason: &str) -> NextState {
    NextState::Terminal {
        outcome: TaskOutcome::Aborted {
            reason: Some(reason.into()),
            result: None,
        },
    }
}

/// Runtime of one invocation; clones share it. Every operation fails once the invocation has ended.
#[derive(Clone)]
pub struct TaskRuntime {
    scheduler: Arc<Scheduler>,
    invocation: Arc<Invocation>,
    phase: Arc<Mutex<PhaseState>>,
}

impl TaskRuntime {
    pub(crate) fn new(
        scheduler: Arc<Scheduler>,
        invocation: Arc<Invocation>,
        phase: Arc<Mutex<PhaseState>>,
    ) -> TaskRuntime {
        TaskRuntime {
            scheduler,
            invocation,
            phase,
        }
    }

    pub fn task_id(&self) -> TaskId {
        self.invocation.task_id
    }

    pub fn conversation_id(&self) -> durable::ConversationId {
        self.invocation.conversation_id
    }

    /// The invocation's signal: aborted by an abort mark (run mode), by close, and when the invocation ends.
    pub fn signal(&self) -> &Cancel {
        &self.invocation.controller
    }

    /// The handler's context: cancelled by the invocation's signal.
    pub fn ctx(&self) -> Ctx {
        Ctx::with_signal(&self.invocation.controller)
    }

    /// The registry snapshot of the current phase; refreshed at each phase boundary.
    pub fn registry(&self) -> Arc<RegistrySnapshot> {
        lock(&self.phase).snapshot.clone()
    }

    /// Commit after rereading the task on the line and gating the invocation. `change` may stage any Session writes
    /// and returns the task's next state, if it changes.
    pub fn commit(
        &self,
        ctx: &Ctx,
        change: impl FnOnce(&mut Tx<'_>, &TaskRecord) -> Result<Option<NextState>>,
    ) -> Result<()> {
        self.gated(ctx, |tx, current| {
            if let Some(next) = change(tx, current)? {
                self.scheduler.commit_state(tx, &self.invocation, current, next)?;
            }
            Ok(())
        })
    }

    /// The memo `name` of the live task, if one was stored.
    pub fn memo(&self, name: &str) -> Result<Option<Value>> {
        self.invocation.check()?;
        Ok(self
            .scheduler
            .live_record(self.invocation.task_id)
            .and_then(|record| record.memos.and_then(|memos| memos.get(name).cloned())))
    }

    /// Store `candidate` as memo `name` unless one is stored; returns the winner.
    pub fn memo_set(&self, name: &str, candidate: Value, ctx: &Ctx) -> Result<Value> {
        self.gated(ctx, |tx, current| {
            if let Some(winner) = current.memos.as_ref().and_then(|memos| memos.get(name)) {
                return Ok(winner.clone());
            }
            let mut memos = current.memos.clone().unwrap_or_default();
            memos.insert(name.to_owned(), candidate.clone());
            tx.set_task(TaskRecord {
                memos: Some(memos),
                ..current.clone()
            })?;
            Ok(candidate)
        })
    }

    /// Wait until the Harness clock reaches `until`, rechecking it after every timer.
    pub fn sleep(&self, until: u64, ctx: &Ctx) -> Result<()> {
        self.invocation.check()?;
        let mut signals = vec![&self.invocation.controller];
        if let Some(signal) = ctx.signal() {
            signals.push(signal);
        }
        let linked = Cancel::any(&signals);
        loop {
            linked.signal().check()?;
            let now = (self.scheduler.now)();
            if now >= until {
                return Ok(());
            }
            let remaining = (until - now).min(MAX_TIMER_DELAY_MS);
            linked.signal().wait_timeout(Duration::from_millis(remaining));
        }
    }

    /// The committed value of a document.
    pub fn snapshot(&self, doc: &Doc, owner: Owner) -> Result<Option<Arc<Value>>> {
        self.invocation.check()?;
        Ok(self.scheduler.session.snapshot(doc, owner)?)
    }

    /// A transaction on the Session line that writes nothing the task depends on: committed reads (entries,
    /// context, documents) for the phase handler, which fail once the invocation has ended.
    pub fn read<T>(&self, read: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        self.invocation.check()?;
        commit_on(&self.scheduler.session, TxScope::default(), read)
    }

    pub fn get_task(&self, id: TaskId) -> Result<Option<TaskRecord>> {
        self.invocation.check()?;
        commit_on(&self.scheduler.session, TxScope::default(), |tx| Ok(tx.task(id)?))
    }

    /// Wait for a task's terminal record; the wait also ends when the invocation does.
    pub fn wait_for_task(&self, id: TaskId, ctx: &Ctx) -> Result<TaskRecord> {
        self.invocation.check()?;
        let bound = self.bind(ctx);
        self.scheduler.wait_for_task(id, &Ctx::with_signal(bound.signal()))
    }

    /// The outcomes of terminal tasks, in order.
    pub fn outcomes(&self, ids: &[TaskId]) -> Result<Vec<TaskOutcome>> {
        self.invocation.check()?;
        commit_on(&self.scheduler.session, TxScope::default(), |tx| {
            let mut outcomes = Vec::new();
            for id in ids {
                match tx.task(*id)?.map(|record| record.state) {
                    Some(TaskState::Terminal { outcome }) => outcomes.push(outcome),
                    _ => return Err(HarnessError::Error(format!("Task {} is not terminal", id.0))),
                }
            }
            Ok(outcomes)
        })
    }

    /// The Harness clock.
    pub fn now(&self) -> Result<u64> {
        self.invocation.check()?;
        Ok((self.scheduler.now)())
    }

    /// Report an error to the Harness's `on_report`.
    pub fn report(&self, error: &HarnessError) -> Result<()> {
        self.invocation.check()?;
        (self.scheduler.report)(error);
        Ok(())
    }

    /// `withAbortSignal(invocation.signal, ctx)`.
    fn bind(&self, ctx: &Ctx) -> super::cancel::Linked {
        let mut signals = vec![&self.invocation.controller];
        if let Some(signal) = ctx.signal() {
            signals.push(signal);
        }
        Cancel::any(&signals)
    }

    /// Commit after rereading the task on the line and gating the invocation.
    fn gated<T>(&self, ctx: &Ctx, change: impl FnOnce(&mut Tx<'_>, &TaskRecord) -> Result<T>) -> Result<T> {
        let invocation = &self.invocation;
        invocation.check()?;
        let scope = TxScope {
            conversation_id: Some(invocation.conversation_id),
            task_id: Some(invocation.task_id),
        };
        commit_on(&self.scheduler.session, scope, |tx| {
            ctx.check()?;
            invocation.check()?;
            if self.scheduler.is_closing() {
                return Err(closed_error(&self.scheduler.session));
            }
            let Some(found) = self.scheduler.live_record(invocation.task_id) else {
                return Err(HarnessError::Error(format!(
                    "Task {} is terminal",
                    invocation.task_id.0
                )));
            };
            let status = found.state.status();
            if status != TaskStatus::Running {
                return Err(HarnessError::Error(format!(
                    "Task {} is {}",
                    invocation.task_id.0,
                    super::status_name(status)
                )));
            }
            if invocation.mode == Mode::Run && found.abort_requested {
                return Err(HarnessError::Error(format!(
                    "Task {} has a durable abort mark",
                    invocation.task_id.0
                )));
            }
            change(tx, &found)
        })
    }

    /// Whether the invocation has ended.
    pub fn ended(&self) -> bool {
        self.invocation.is_ended()
    }
}
