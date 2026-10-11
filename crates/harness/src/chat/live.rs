//! `pi.live` operations (spec §8.2): run control, the generation partial, tool slots and compaction statuses, and the
//! Harness cleanup for outcomes the scheduler writes itself. Port of upstream `harness/live.ts`.

use chord_delta::{Draft, Seg};
use durable::{ConversationId, EntryDraft, EntryRecord, SubmissionId, SubmissionSettlement, TaskId, TaskRecord, Tx};
use serde_json::{Value, json};

use super::docs::LIVE;
use super::entries;
use super::usage::{Bucket, record_usage};
use super::util::{d, delete, get, owner, set};
use crate::scheduler::{Result, SchedulerOutcome};

pub(crate) const GENERATION_KIND: &str = "pi.generation";
pub(crate) const CELL_KIND: &str = "pi.tool";
pub(crate) const COMPACTION_KIND: &str = "pi.compaction";

/// The live document of a conversation.
pub(crate) fn live(tx: &mut Tx<'_>, conversation_id: ConversationId) -> Result<Draft> {
    Ok(tx.doc(&LIVE, owner(conversation_id))?)
}

/// The task that owns the run, if the conversation is busy.
pub(crate) fn run_task(live: &Draft) -> Result<Option<TaskId>> {
    Ok(get(live, &["run", "taskId"])?.and_then(|id| id.as_u64()).map(TaskId))
}

/// End the run owned by `task_id`: settle each of its inputs and remove `run`. Always removes `generation`, `tools`
/// and `nestedTools`, whose presentation belongs to the ending run.
pub(crate) fn end_run(tx: &mut Tx<'_>, live: &Draft, task_id: TaskId, settlement: SubmissionSettlement) -> Result<()> {
    if run_task(live)? == Some(task_id) {
        let inputs = get(live, &["run", "inputs"])?.unwrap_or(json!([]));
        for input in inputs.as_array().into_iter().flatten() {
            tx.settle_submission(SubmissionId(input.as_u64().unwrap_or_default()), settlement.clone());
        }
        delete(live, &["run"])?;
    }
    delete(live, &["generation"])?;
    delete(live, &["tools"])?;
    delete(live, &["nestedTools"])
}

/// Hand run control from `from` to `to`; the run's inputs move with it.
pub(crate) fn hand_over(live: &Draft, from: TaskId, to: TaskId) -> Result<()> {
    if run_task(live)? == Some(from) {
        set(live, &["run", "taskId"], json!(to.0))?;
    }
    Ok(())
}

/// Append a provider result and add its usage to `pi.usage` in the same commit.
pub(crate) fn append_assistant(
    tx: &mut Tx<'_>,
    conversation_id: ConversationId,
    message: &Value,
) -> Result<EntryRecord> {
    let key = format!(
        "{}/{}",
        message["provider"].as_str().unwrap_or_default(),
        message["model"].as_str().unwrap_or_default()
    );
    record_usage(tx, conversation_id, Bucket::Models, &key, &message["usage"])?;
    Ok(tx.append_entry(
        conversation_id,
        EntryDraft {
            model: Some(vec![message.clone()]),
            ..EntryDraft::new(entries::ASSISTANT)
        },
    )?)
}

/// Append a committed partial left by an interrupted, aborted, faulted, or orphaned attempt as an aborted assistant
/// entry; the caller replaces or removes `generation`.
pub(crate) fn convert_partial(tx: &mut Tx<'_>, live: &Draft, conversation_id: ConversationId) -> Result<()> {
    let Some(mut partial) = get(live, &["generation", "message"])? else {
        return Ok(());
    };
    partial["stopReason"] = json!("aborted");
    append_assistant(tx, conversation_id, &partial)?;
    Ok(())
}

/// Index of the slot of cell task `task_id` in `pi.live.tools`.
pub(crate) fn slot_index(live: &Draft, task_id: TaskId) -> Result<Option<usize>> {
    let tools = get(live, &["tools"])?.unwrap_or(json!([]));
    Ok(tools
        .as_array()
        .and_then(|slots| slots.iter().position(|slot| slot["taskId"].as_u64() == Some(task_id.0))))
}

pub(crate) fn slot_path(index: usize, field: Option<&str>) -> Vec<Seg> {
    let mut path = vec![Seg::from("tools"), Seg::from(index)];
    if let Some(field) = field {
        path.push(Seg::from(field));
    }
    path
}

/// The slot value of cell task `task_id`, if `pi.live` still lists it.
pub(crate) fn slot(live: &Draft, task_id: TaskId) -> Result<Option<Value>> {
    match slot_index(live, task_id)? {
        Some(index) => d(live.get(&slot_path(index, None))),
        None => Ok(None),
    }
}

/// Remove what a cell published while running; its result entry or a rerun replaces it.
pub(crate) fn clear_progress(live: &Draft, index: usize) -> Result<()> {
    for field in ["output", "droppedBytes", "droppedLines", "details", "diagnostics"] {
        d(live.delete(&slot_path(index, Some(field))))?;
    }
    Ok(())
}

/// Mark a slot done and clear its running output.
pub(crate) fn finish_slot(live: &Draft, index: usize) -> Result<()> {
    d(live.set(&slot_path(index, Some("status")), json!("done")))?;
    clear_progress(live, index)
}

/// Add the status of a compaction task created in this commit; statuses stay in task ID order.
pub(crate) fn add_compaction_status(live: &Draft, status: Value) -> Result<()> {
    if get(live, &["compactions"])?.is_none() {
        set(live, &["compactions"], json!([]))?;
    }
    d(live.push(&[Seg::from("compactions")], vec![status]))?;
    Ok(())
}

pub(crate) fn compaction_status_index(live: &Draft, task_id: TaskId) -> Result<Option<usize>> {
    Ok(get(live, &["compactions"])?.and_then(|statuses| {
        statuses.as_array().and_then(|statuses| {
            statuses
                .iter()
                .position(|status| status["taskId"].as_u64() == Some(task_id.0))
        })
    }))
}

/// Remove the status of compaction task `task_id`, and the list once empty.
pub(crate) fn remove_compaction_status(live: &Draft, task_id: TaskId) -> Result<()> {
    let Some(index) = compaction_status_index(live, task_id)? else {
        return Ok(());
    };
    d(live.splice(&[Seg::from("compactions")], index, 1, Vec::new()))?;
    if get(live, &["compactions"])?.is_some_and(|statuses| statuses.as_array().is_some_and(Vec::is_empty)) {
        delete(live, &["compactions"])?;
    }
    Ok(())
}

/// Harness cleanup for a terminal outcome the scheduler writes itself (`faulted` or `orphaned`). A run task ends its
/// run, converting a committed partial; a cell task's slot is marked done without an entry, and context derivation
/// synthesizes the missing result; a compaction task's status is removed. Other kinds are ignored.
pub fn settle_scheduler_outcome(tx: &mut Tx<'_>, record: &TaskRecord, outcome: &SchedulerOutcome) -> Result<()> {
    match record.kind.as_str() {
        CELL_KIND => {
            let live = live(tx, record.conversation_id)?;
            if let Some(index) = slot_index(&live, record.id)? {
                finish_slot(&live, index)?;
            }
            Ok(())
        }
        COMPACTION_KIND => {
            let live = live(tx, record.conversation_id)?;
            remove_compaction_status(&live, record.id)
        }
        GENERATION_KIND => {
            let live = live(tx, record.conversation_id)?;
            if run_task(&live)? != Some(record.id) {
                return Ok(());
            }
            convert_partial(tx, &live, record.conversation_id)?;
            let settlement = match outcome {
                SchedulerOutcome::Faulted { message } => SubmissionSettlement::Unanswered {
                    reason: "faulted".into(),
                    detail: Some(json!(message)),
                },
                SchedulerOutcome::Orphaned { reason } => SubmissionSettlement::Unanswered {
                    reason: reason.clone(),
                    detail: None,
                },
            };
            end_run(tx, &live, record.id, settlement)
        }
        _ => Ok(()),
    }
}
