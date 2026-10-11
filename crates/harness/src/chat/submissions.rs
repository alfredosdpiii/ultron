//! Admission, waits and withdrawal of durable submissions (spec §6). Port of upstream `harness/submissions.ts`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use durable::{
    CommitChange, ConversationId, Session, SessionError, StorageWrite, SubmissionCreate, SubmissionId,
    SubmissionRecord, SubmissionSettlement, SubmissionStatus, SubmissionType, Tx, TxScope,
};
use serde_json::{Value, json};

use super::generation::start_run;
use super::inbox::{At, QueueModes, apply_boundary, entry_draft, is_stale, items, prepare_boundary, remove_inbox_item};
use super::live::live;
use super::util::{d, strict};
use crate::scheduler::cancel::{Ctx, Waiters};
use crate::scheduler::{HarnessError, Result};

/// What a busy conversation does with an input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WhenBusy {
    /// Queue it for the final boundary (the default).
    #[default]
    FollowUp,
    /// Queue it for the next boundary, `postTools` included.
    Steer,
    /// Write nothing and fail with `ConversationBusy`.
    Reject,
}

/// A submission before admission: user input (a string or content blocks) or a passive entry write.
#[derive(Clone, Debug, PartialEq)]
pub enum SubmissionDraft {
    Input {
        content: Value,
        when_busy: WhenBusy,
        request_id: Option<String>,
    },
    /// `entry` is an `EntryDraft` as JSON: `{ kind, model?, data?, head?: id | "self", edits? }`.
    Write { entry: Value, request_id: Option<String> },
}

impl SubmissionDraft {
    pub fn input(content: impl Into<Value>) -> SubmissionDraft {
        SubmissionDraft::Input {
            content: content.into(),
            when_busy: WhenBusy::FollowUp,
            request_id: None,
        }
    }

    pub fn write(entry: Value) -> SubmissionDraft {
        SubmissionDraft::Write {
            entry,
            request_id: None,
        }
    }

    pub fn when_busy(self, mode: WhenBusy) -> SubmissionDraft {
        match self {
            SubmissionDraft::Input {
                content, request_id, ..
            } => SubmissionDraft::Input {
                content,
                when_busy: mode,
                request_id,
            },
            other => other,
        }
    }

    pub fn request_id(self, id: &str) -> SubmissionDraft {
        match self {
            SubmissionDraft::Input { content, when_busy, .. } => SubmissionDraft::Input {
                content,
                when_busy,
                request_id: Some(id.to_owned()),
            },
            SubmissionDraft::Write { entry, .. } => SubmissionDraft::Write {
                entry,
                request_id: Some(id.to_owned()),
            },
        }
    }

    fn kind(&self) -> SubmissionType {
        match self {
            SubmissionDraft::Input { .. } => SubmissionType::Input,
            SubmissionDraft::Write { .. } => SubmissionType::Write,
        }
    }

    fn request(&self) -> Option<&str> {
        match self {
            SubmissionDraft::Input { request_id, .. } | SubmissionDraft::Write { request_id, .. } => {
                request_id.as_deref()
            }
        }
    }
}

fn type_name(kind: SubmissionType) -> &'static str {
    match kind {
        SubmissionType::Input => "input",
        SubmissionType::Write => "write",
    }
}

fn raw(
    conversation_id: ConversationId,
    request_id: Option<&str>,
    kind: SubmissionType,
    status: SubmissionStatus,
) -> SubmissionCreate {
    SubmissionCreate {
        conversation_id,
        request_id: request_id.map(str::to_owned),
        kind,
        status,
        entry: None,
        answer: None,
        reason: None,
        detail: None,
    }
}

/// Admit a submission inside a commit (spec §6). A known request ID returns its existing submission without writing.
/// A busy conversation queues it in `pi.inbox`, or rejects `WhenBusy::Reject` input with `ConversationBusy`. An idle
/// conversation with queued items queues it behind them and runs a final boundary. Otherwise idle input places a user
/// entry and starts a run, and an idle write appends its entry and settles `done`, or `stale` when its head reaches
/// before the active range.
pub fn admit_submission(
    tx: &mut Tx<'_>,
    conversation_id: ConversationId,
    draft: &SubmissionDraft,
    now: u64,
    modes: QueueModes,
) -> Result<SubmissionId> {
    if let Some(request_id) = draft.request()
        && let Some(existing) = tx.submission_by_request(conversation_id, request_id)?
    {
        if existing.kind != draft.kind() {
            return Err(HarnessError::Error(format!(
                "Request {request_id} already identifies a submission of type {}",
                type_name(existing.kind)
            )));
        }
        return Ok(existing.id);
    }
    let live = live(tx, conversation_id)?;
    let busy = super::util::get(&live, &["run"])?.is_some();
    if busy
        && matches!(
            draft,
            SubmissionDraft::Input {
                when_busy: WhenBusy::Reject,
                ..
            }
        )
    {
        return Err(HarnessError::Session(SessionError::Busy(conversation_id)));
    }
    // A boundary reads the table, so it is prepared before the first table write; a busy one needs none.
    let mut boundary = if busy {
        None
    } else {
        Some(prepare_boundary(tx, conversation_id, modes)?)
    };
    let waiting = match &boundary {
        Some(boundary) => !items(&boundary.inbox)?.is_empty(),
        None => true,
    };
    if waiting {
        let record = tx.create_submission(raw(
            conversation_id,
            draft.request(),
            draft.kind(),
            SubmissionStatus::Queued,
        ))?;
        let item = match draft {
            SubmissionDraft::Write { entry, .. } => {
                json!({ "id": record.id.0, "mode": "write", "entry": strict(entry.clone()) })
            }
            SubmissionDraft::Input { content, when_busy, .. } => {
                let mode = if *when_busy == WhenBusy::Steer {
                    "steer"
                } else {
                    "followUp"
                };
                json!({ "id": record.id.0, "mode": mode, "content": strict(content.clone()) })
            }
        };
        let inbox = match &boundary {
            Some(boundary) => boundary.inbox.clone(),
            None => tx.doc(&super::docs::INBOX, super::util::owner(conversation_id))?,
        };
        d(inbox.push(&[chord_delta::Seg::from("items")], vec![item]))?;
        let Some(boundary) = boundary.as_mut() else {
            return Ok(record.id);
        };
        let result = apply_boundary(tx, boundary, At::Final, now)?;
        if !result.users.is_empty() {
            start_run(tx, conversation_id, &live, result.users)?;
        }
        return Ok(record.id);
    }
    let boundary = boundary.expect("idle");
    match draft {
        SubmissionDraft::Write { entry, request_id } => {
            let entry = entry_draft(&strict(entry.clone()))?;
            if is_stale(&boundary, &entry) {
                let mut stale = raw(
                    conversation_id,
                    request_id.as_deref(),
                    SubmissionType::Write,
                    SubmissionStatus::Unanswered,
                );
                stale.reason = Some("stale".into());
                return Ok(tx.create_submission(stale)?.id);
            }
            let appended = tx.append_entry(conversation_id, entry)?;
            let mut done = raw(
                conversation_id,
                request_id.as_deref(),
                SubmissionType::Write,
                SubmissionStatus::Done,
            );
            done.entry = Some(appended.id);
            Ok(tx.create_submission(done)?.id)
        }
        SubmissionDraft::Input {
            content, request_id, ..
        } => {
            let entry = tx.append_entry(conversation_id, super::inbox::user_entry(&strict(content.clone()), now))?;
            let mut placed = raw(
                conversation_id,
                request_id.as_deref(),
                SubmissionType::Input,
                SubmissionStatus::Placed,
            );
            placed.entry = Some(entry.id);
            let id = tx.create_submission(placed)?.id;
            start_run(tx, conversation_id, &live, vec![id])?;
            Ok(id)
        }
    }
}

/// Place input on a conversation created in this transaction (idle, with an empty inbox, so no table read is needed):
/// a user entry, a `placed` input submission, and a run for it.
pub fn place_initial_input(
    tx: &mut Tx<'_>,
    conversation_id: ConversationId,
    content: &Value,
    now: u64,
) -> Result<SubmissionId> {
    let live = live(tx, conversation_id)?;
    let entry = tx.append_entry(conversation_id, super::inbox::user_entry(&strict(content.clone()), now))?;
    let mut placed = raw(conversation_id, None, SubmissionType::Input, SubmissionStatus::Placed);
    placed.entry = Some(entry.id);
    let id = tx.create_submission(placed)?.id;
    start_run(tx, conversation_id, &live, vec![id])?;
    Ok(id)
}

/// What `abort` of a submission did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmissionAbort {
    Aborted,
    AlreadyPlaced,
    Settled,
    NotFound,
}

fn is_settled(record: &SubmissionRecord) -> bool {
    matches!(record.status, SubmissionStatus::Done | SubmissionStatus::Unanswered)
}

/// Waits and withdrawal of the submissions of one Harness.
pub(crate) struct Submissions {
    session: Session,
    waiters: Waiters<SubmissionId, SubmissionRecord>,
    closed: AtomicBool,
}

impl Submissions {
    pub(crate) fn new(session: Session) -> Result<Arc<Submissions>> {
        let submissions = Arc::new(Submissions {
            session: session.clone(),
            waiters: Waiters::new(),
            closed: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&submissions);
        session.subscribe_commits(move |publication| {
            let Some(submissions) = weak.upgrade() else { return };
            for change in &publication.changes {
                if let CommitChange::Table(StorageWrite::Submission { value }) = change
                    && is_settled(value)
                {
                    submissions.waiters.resolve(&value.id, value.clone());
                }
            }
        })?;
        let weak = Arc::downgrade(&submissions);
        session.subscribe_close(move || {
            if let Some(submissions) = weak.upgrade() {
                submissions.closed.store(true, Ordering::SeqCst);
                submissions
                    .waiters
                    .reject_all(crate::scheduler::closed_error(&submissions.session));
            }
        })?;
        Ok(submissions)
    }

    fn commit<T>(&self, change: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        crate::scheduler::commit_on(&self.session, TxScope::default(), change)
    }

    pub(crate) fn status(&self, id: SubmissionId) -> Result<SubmissionRecord> {
        self.commit(|tx| tx.submission(id)?.ok_or_else(|| not_found(id)))
    }

    pub(crate) fn exists(&self, id: SubmissionId) -> Result<bool> {
        self.commit(|tx| Ok(tx.submission(id)?.is_some()))
    }

    /// Wait until the submission is `done` or `unanswered`; cancelling `ctx` cancels only the wait.
    pub(crate) fn wait(&self, id: SubmissionId, ctx: &Ctx) -> Result<SubmissionRecord> {
        // Check and register on the line so no settling publication falls between them.
        let slot = self.commit(|tx| {
            let record = tx.submission(id)?.ok_or_else(|| not_found(id))?;
            if is_settled(&record) {
                return Ok(Err(record));
            }
            if self.closed.load(Ordering::SeqCst) {
                return Err(crate::scheduler::closed_error(&self.session));
            }
            Ok(Ok(self.waiters.add(id, ctx)?))
        })?;
        match slot {
            Err(record) => Ok(record),
            Ok(slot) => slot.wait(ctx),
        }
    }

    /// Withdraw a queued submission and remove its inbox item; placed inputs and settled submissions are reported.
    pub(crate) fn abort(&self, id: SubmissionId, conversation_id: Option<ConversationId>) -> Result<SubmissionAbort> {
        self.commit(|tx| {
            let Some(record) = tx.submission(id)? else {
                return Ok(SubmissionAbort::NotFound);
            };
            if conversation_id.is_some_and(|conversation| conversation != record.conversation_id) {
                return Ok(SubmissionAbort::NotFound);
            }
            match record.status {
                SubmissionStatus::Queued => {
                    tx.settle_submission(
                        id,
                        SubmissionSettlement::Unanswered {
                            reason: "aborted".into(),
                            detail: None,
                        },
                    );
                    remove_inbox_item(tx, record.conversation_id, id)?;
                    Ok(SubmissionAbort::Aborted)
                }
                SubmissionStatus::Placed => Ok(SubmissionAbort::AlreadyPlaced),
                _ => Ok(SubmissionAbort::Settled),
            }
        })
    }
}

fn not_found(id: SubmissionId) -> HarnessError {
    HarnessError::Error(format!("Submission {} does not exist", id.0))
}
