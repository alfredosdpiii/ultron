//! Migration of the pre-durable task journals into the one journal, the durable task table.
//!
//! Both earlier journals (`crates/harness/src/task_store.rs` and `crates/delegation/src/task_store.rs`, ports of
//! `task-store.ts`) persist one document, the session value `ultron.tasks/root`: `{ version: 1, tasks: [{ id, key,
//! fingerprint, definition, state, result?, parentId? }] }`. Importing it writes one terminal `ultron.child` task per
//! record, keeping the legacy fields in its input. A record that was still `admitted` or `running` imports
//! `interrupted`, exactly as reopening the old journal recovered it: unfinished work is never replayed.

use durable::{
    ConversationId, TaskId, TaskOptions, TaskOutcome, TaskOutcomeError, TaskOwnership, TaskQuery, TaskRecord,
    TaskState as RecordState, Tx,
};
use serde_json::{Value, json};

use super::children::{CHILD_KIND, ChildInput};
use super::util::to_value;
use crate::scheduler::{HarnessError, Result};
use crate::task_store::{Task, TaskState, parse_document};

const INTERRUPTED: &str = "Owner ended; automatic replay is disabled";

/// The durable outcome of one legacy record.
fn outcome(task: &Task) -> TaskOutcome {
    let interrupted = json!({ "status": "interrupted", "error": INTERRUPTED, "verification": "unverified" });
    let result = task.result.clone().unwrap_or(interrupted);
    let receipt = json!({ "result": result });
    let error = |fallback: &str| TaskOutcomeError {
        message: result["error"].as_str().unwrap_or(fallback).to_owned(),
        detail: None,
    };
    match task.state {
        TaskState::Completed => TaskOutcome::Completed { result: receipt },
        TaskState::Cancelled => TaskOutcome::Aborted {
            reason: None,
            result: Some(receipt),
        },
        TaskState::Failed => TaskOutcome::Failed {
            error: error("failed"),
            result: Some(receipt),
        },
        TaskState::Interrupted | TaskState::Admitted | TaskState::Running => TaskOutcome::Failed {
            error: error(INTERRUPTED),
            result: Some(receipt),
        },
    }
}

/// Import a legacy journal document into `conversation_id`. Records imported before (by legacy ID) are skipped;
/// returns the tasks written, in record order. An invalid document fails with the old journal's error.
pub fn import_task_journal(tx: &mut Tx<'_>, conversation_id: ConversationId, document: &Value) -> Result<Vec<TaskId>> {
    let tasks = parse_document(document).map_err(|error| HarnessError::msg(error.to_string()))?;
    // Every table read before the first write.
    let mut imported: Vec<String> = Vec::new();
    let query = TaskQuery {
        conversation_id: Some(conversation_id),
        kind: Some(CHILD_KIND.into()),
        ..TaskQuery::default()
    };
    let mut cursor = None;
    loop {
        let page = tx.scan_tasks(&query, 256, cursor.as_ref())?;
        imported.extend(
            page.items
                .iter()
                .filter_map(|record| record.input["legacy"]["id"].as_str().map(str::to_owned)),
        );
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    let definition = durable::TaskDefinition::new(CHILD_KIND, 1, |_| json!({ "phase": "start" }));
    let mut written = Vec::new();
    for task in tasks.iter().filter(|task| !imported.contains(&task.id)) {
        let mut input = to_value(&ChildInput {
            name: task.key.clone(),
            prompt: String::new(),
            model: None,
            timeout_ms: None,
            keep: false,
            ..ChildInput::default()
        });
        let mut legacy = task.to_json();
        if let Some(legacy) = legacy.as_object_mut() {
            legacy.remove("result");
        }
        input["legacy"] = legacy;
        let options = TaskOptions {
            ownership: TaskOwnership::Conversation,
            conversation_id: Some(conversation_id),
            background: true,
            ..TaskOptions::default()
        };
        let id = tx.create_task(&definition, input.clone(), options)?;
        tx.set_task(TaskRecord {
            id,
            conversation_id,
            kind: CHILD_KIND.into(),
            version: 1,
            input,
            owner: None,
            background: true,
            abort_requested: false,
            abort_reason: None,
            abandon_on_restart: false,
            started_at: None,
            ended_at: None,
            state: RecordState::Terminal { outcome: outcome(task) },
            memos: None,
        })?;
        written.push(id);
    }
    Ok(written)
}
