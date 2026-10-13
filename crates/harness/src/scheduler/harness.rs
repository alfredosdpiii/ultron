//! The Harness: a Session with a durable task scheduler (the task half of upstream `harness/harness.ts`).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use durable::{
    ConversationId, ConversationOwnership, Doc, Owner, ROOT_CONVERSATION_ID, Session, SessionEnd, SessionError,
    Storage, SubmissionId, SubmissionQuery, SubmissionRecord, SubmissionStatus, TaskId, TaskRecord, Tx, TxScope,
};
use serde_json::Value;

use super::cancel::Ctx;
use super::registry::RegistryReader;
use super::{
    BlockedReason, Clock, HarnessError, Report, Result, Scheduler, SchedulerOptions, SettleOutcome, WithdrawInputs,
    commit_on,
};
use crate::chat::inbox::withdraw_queued_inputs;

/// What `abort_task` did: marked (or settled `orphaned`), or found the task terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbortResult {
    Marked,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheduling {
    Paused,
    Running,
    Closing,
}

/// A live task's derived scheduling state.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskInspectionState {
    Running,
    Completing,
    Waiting {
        on: Vec<TaskId>,
    },
    Blocked {
        reason: BlockedReason,
        error: Option<HarnessError>,
    },
    Ready {
        migrates: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TaskInspection {
    pub record: TaskRecord,
    pub state: TaskInspectionState,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Inspection {
    pub scheduling: Scheduling,
    pub tasks: Vec<TaskInspection>,
    /// Queued and placed submissions, in ID order.
    pub submissions: Vec<SubmissionRecord>,
}

pub type FallibleClock = Arc<dyn Fn() -> std::result::Result<u64, HarnessError> + Send + Sync>;

/// Harness options. `registry` may keep changing while the Harness runs.
#[derive(Clone)]
pub struct HarnessOptions {
    pub registry: Arc<dyn RegistryReader>,
    /// The Harness clock (milliseconds); a failing clock is reported once and falls back to the wall clock.
    pub now: Option<FallibleClock>,
    /// Where the Session's failure, failed migrations, and listener errors go; it must not wait on the Session line.
    pub on_report: Option<Report>,
    /// Harness cleanup in the commit that makes a `faulted` or `orphaned` outcome terminal. Default: nothing.
    pub settle_outcome: Option<SettleOutcome>,
    /// Withdraw a conversation's queued inputs. Default: the `pi.inbox` withdrawal
    /// ([`crate::chat::inbox::withdraw_queued_inputs`]).
    pub withdraw_inputs: Option<WithdrawInputs>,
}

impl HarnessOptions {
    pub fn new(registry: Arc<dyn RegistryReader>) -> HarnessOptions {
        HarnessOptions {
            registry,
            now: None,
            on_report: None,
            settle_outcome: None,
            withdraw_inputs: None,
        }
    }
}

fn wall_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// `onReport` that cannot fail: it is the last place an error goes, so its own panic is dropped.
fn safe_report(on_report: Option<Report>) -> Report {
    Arc::new(move |error| {
        if let Some(on_report) = &on_report {
            let _ = catch_unwind(AssertUnwindSafe(|| on_report(error)));
        }
    })
}

/// The host clock, falling back to the wall clock when it fails. The first failure is reported; the clock is read
/// in every task change, so later ones are not.
pub fn safe_now(now: Option<FallibleClock>, report: Report) -> Clock {
    let Some(now) = now else {
        return Arc::new(wall_clock);
    };
    let reported = AtomicBool::new(false);
    Arc::new(move || {
        let read =
            catch_unwind(AssertUnwindSafe(|| now())).unwrap_or_else(|_| Err(HarnessError::msg("clock panicked")));
        match read {
            Ok(value) => value,
            Err(error) => {
                // Set first: a report handler that reads the clock again must not report again.
                if !reported.swap(true, Ordering::SeqCst) {
                    report(&error);
                }
                wall_clock()
            }
        }
    })
}

/// Durable task harness over one Session. Clones share it.
#[derive(Clone)]
pub struct Harness {
    session: Session,
    scheduler: Arc<Scheduler>,
}

impl Harness {
    /// Open a Harness over storage: load live tasks, reconcile surviving `running` tasks to `pending`, and derive
    /// what a crash left unapplied. Nothing runs until [`Harness::resume`] (or a wait) enables scheduling.
    pub fn open(storage: impl Storage + Send + 'static, options: HarnessOptions) -> Result<Harness> {
        let report = safe_report(options.on_report);
        let now = safe_now(options.now, report.clone());
        let clock = now.clone();
        let session = Session::with_clock(storage, move || clock());
        let scheduler = Scheduler::new(SchedulerOptions {
            session: session.clone(),
            registry: options.registry,
            now,
            report,
            settle_outcome: options.settle_outcome.unwrap_or_else(|| Arc::new(|_, _, _| Ok(()))),
            withdraw_inputs: options
                .withdraw_inputs
                .unwrap_or_else(|| Arc::new(withdraw_queued_inputs)),
        });
        if let Err(error) = scheduler.open() {
            // A failing Storage close fails the Session, which reports it.
            let _ = session.close();
            scheduler.join();
            return Err(error);
        }
        Ok(Harness { session, scheduler })
    }

    /// Close this Harness, then open a fresh one over `storage` (typically the same database reopened): the
    /// recovery path after a failure.
    pub fn reopen(&self, storage: impl Storage + Send + 'static, options: HarnessOptions) -> Result<Harness> {
        let _ = self.close();
        Harness::open(storage, options)
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    fn assert_usable(&self) -> Result<()> {
        if let Some(failure) = self.session.failure() {
            return Err(HarnessError::Session(SessionError::Failed(Box::new(failure))));
        }
        if self.scheduler.is_closing() {
            return Err(HarnessError::Session(SessionError::Closed));
        }
        Ok(())
    }

    /// Enable scheduling.
    pub fn resume(&self) -> Result<()> {
        self.assert_usable()?;
        self.scheduler.resume();
        Ok(())
    }

    /// One atomic transaction on the Session line.
    pub fn commit<T>(&self, change: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        commit_on(&self.session, TxScope::default(), change)
    }

    /// The root conversation, created at its reserved ID on first use.
    pub fn root(&self) -> Result<Conversation> {
        self.assert_usable()?;
        let id = self.commit(|tx| {
            if tx.conversation(ROOT_CONVERSATION_ID)?.is_some() {
                return Ok(ROOT_CONVERSATION_ID);
            }
            Ok(tx.create_root_conversation()?.id)
        })?;
        Ok(self.handle(id))
    }

    pub fn create_conversation(&self, ownership: ConversationOwnership) -> Result<Conversation> {
        self.assert_usable()?;
        let id = self.commit(|tx| Ok(tx.create_conversation(ownership)?.id))?;
        Ok(self.handle(id))
    }

    pub fn conversation(&self, id: ConversationId) -> Result<Option<Conversation>> {
        self.assert_usable()?;
        let record = self.commit(|tx| Ok(tx.conversation(id)?))?;
        Ok(record.map(|record| self.handle(record.id)))
    }

    fn handle(&self, id: ConversationId) -> Conversation {
        Conversation {
            id,
            harness: self.clone(),
        }
    }

    pub fn get_task(&self, id: TaskId) -> Result<Option<TaskRecord>> {
        self.commit(|tx| Ok(tx.task(id)?))
    }

    pub fn submission(&self, id: SubmissionId) -> Result<Option<SubmissionRecord>> {
        self.commit(|tx| Ok(tx.submission(id)?))
    }

    /// Wait for a task's terminal record; enables scheduling.
    pub fn wait_for_task(&self, id: TaskId, ctx: &Ctx) -> Result<TaskRecord> {
        self.scheduler.resume();
        self.scheduler.wait_for_task(id, ctx)
    }

    /// Mark a task for abort and join its run; a task no definition can take, with nothing it owns live, settles
    /// `orphaned` in the marking commit.
    pub fn abort_task(&self, id: TaskId, ctx: &Ctx) -> Result<AbortResult> {
        self.scheduler.abort(id, ctx, false)
    }

    /// Wait until no live non-background task is reachable from any ownerless conversation; enables scheduling.
    pub fn wait_for_idle(&self, ctx: &Ctx) -> Result<()> {
        self.scheduler.resume();
        self.scheduler.wait_for_idle(None, ctx)
    }

    /// Scheduling state, every live task with its derived state, and the queued and placed submissions.
    pub fn inspect(&self) -> Result<Inspection> {
        let snapshot = self.scheduler.registry().snapshot();
        self.commit(|tx| {
            let (scheduling, tasks) = self.scheduler.inspect(tx, &snapshot)?;
            let mut submissions = Vec::new();
            for status in [SubmissionStatus::Queued, SubmissionStatus::Placed] {
                let query = SubmissionQuery {
                    status: Some(status),
                    ..SubmissionQuery::default()
                };
                let mut cursor = None;
                loop {
                    let page = tx.committed_scan_submissions(&query, super::SCAN_PAGE_SIZE, cursor.as_ref())?;
                    submissions.extend(page.items);
                    match page.next {
                        Some(next) => cursor = Some(next),
                        None => break,
                    }
                }
            }
            submissions.sort_by_key(|submission| submission.id);
            Ok(Inspection {
                scheduling,
                tasks,
                submissions,
            })
        })
    }

    pub fn snapshot(&self, doc: &Doc, owner: Owner) -> Result<Option<Arc<Value>>> {
        Ok(self.session.snapshot(doc, owner)?)
    }

    /// Seal admission, signal and join every invocation (writing no outcome), then close Storage. A Storage that
    /// cannot close fails the Session; the error is reported unless an earlier failure is the cause.
    pub fn close(&self) -> Result<()> {
        let result = self.session.close();
        self.scheduler.join();
        // A Storage that cannot close is a failed one; an earlier failure stays the cause and is what is reported.
        self.scheduler.report_failure();
        result.map_err(HarnessError::Session)
    }

    /// Jobs admitted to the Session line and not yet finished (commits, waits registering, a close), the running one
    /// included: a test's handshake for "this call has queued behind the held commit".
    pub fn line_len(&self) -> u64 {
        self.session.line_len()
    }

    /// Wait until the Harness has closed: Storage closed and every invocation ended.
    pub fn closed(&self) -> SessionEnd {
        let end = self.session.wait_closed();
        self.scheduler.join();
        self.scheduler.report_failure();
        end
    }
}

/// Handle of one conversation.
#[derive(Clone)]
pub struct Conversation {
    pub id: ConversationId,
    harness: Harness,
}

impl Conversation {
    /// Commit with this conversation as the default for `Tx::create_task`.
    pub fn commit<T>(&self, change: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        let scope = TxScope {
            conversation_id: Some(self.id),
            task_id: None,
        };
        commit_on(&self.harness.session, scope, change)
    }

    /// Withdraw queued inputs and abort the live non-background work reachable from this conversation (with
    /// `background`, across background boundaries); returns once the scope is idle.
    pub fn abort(&self, background: bool, ctx: &Ctx) -> Result<()> {
        self.harness.scheduler.resume();
        self.harness.scheduler.abort_conversation(self.id, background, ctx)
    }

    pub fn wait_for_idle(&self, ctx: &Ctx) -> Result<()> {
        self.harness.scheduler.resume();
        self.harness.scheduler.wait_for_idle(Some(self.id), ctx)
    }
}
