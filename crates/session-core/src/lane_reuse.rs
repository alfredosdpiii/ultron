//! A typed agent's run on a reused lane (a retained instance, `instances.invoke`) continues that lane's durable
//! conversation instead of starting a new one, as the TS host prompts the same named harness lane again: the earlier
//! child conversation of the lane gets the new prompt as a submission, and its answer is the run's reply. The lane's
//! REPL is the same one (cells of the conversation run on the lane named by its owning child task), with its scratch
//! reset by the host before the run (`before_lane_reuse`) and its declared state kept. Earlier invocations' results
//! are untouched: they are their own tasks' terminal records.

use delegation::agents::{LaneReply, LaneRun};
use durable::{ConversationId, Cursor, SubmissionStatus, TaskQuery};
use harness::chat::children::{CHILD_KIND, child_report};
use harness::chat::{AgentChange, ChatHarness, ModelRef, SubmissionDraft, WhenBusy};
use harness::scheduler::{Cancel, Ctx};
use serde_json::Value;
use std::time::Duration;

/// The conversation a lane's newest child task holds, if the lane ever ran as a child conversation.
pub fn lane_conversation(chat: &ChatHarness, lane: &str) -> Result<Option<ConversationId>, String> {
    chat.commit(|tx| {
        let query = TaskQuery {
            kind: Some(CHILD_KIND.to_string()),
            ..TaskQuery::default()
        };
        let mut newest: Option<(u64, ConversationId)> = None;
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = tx.scan_tasks(&query, 256, cursor.as_ref())?;
            for task in page.items {
                if task.input["name"].as_str() != Some(lane) {
                    continue;
                }
                let conversation = child_report(&task)
                    .and_then(|report| report["conversation"].as_u64())
                    .map(ConversationId);
                if let Some(conversation) = conversation
                    && newest.is_none_or(|(id, _)| task.id.0 > id)
                {
                    newest = Some((task.id.0, conversation));
                }
            }
            match page.next {
                Some(next) => cursor = Some(next),
                None => return Ok(newest.map(|(_, conversation)| conversation)),
            }
        }
    })
    .map_err(|e| e.to_string())
}

/// Run `run`'s prompt as the next turn of `conversation`. Cancelling the task (`run.signal`) aborts the turn.
pub fn continue_lane(chat: &ChatHarness, conversation: ConversationId, run: &LaneRun) -> Result<LaneReply, String> {
    let handle = chat
        .conversation(conversation)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("The conversation of lane {} is gone", run.lane))?;
    if let Some((provider, id)) = run.model.as_deref().and_then(|spec| spec.split_once('/')) {
        handle
            .configure(&AgentChange::model(ModelRef::new(provider, id)))
            .map_err(|e| e.to_string())?;
    }
    let submission = handle
        .submit(SubmissionDraft::input(run.prompt.clone()).when_busy(WhenBusy::Reject))
        .map_err(|e| e.to_string())?;
    let cancel = Cancel::new();
    let done = Cancel::new();
    let (watched, stop, target) = (run.signal.clone(), done.clone(), cancel.clone());
    // The task's signal has no callback: it is watched until the wait ends.
    let watcher = std::thread::spawn(move || {
        while !stop.is_cancelled() {
            if watched.wait(Duration::from_millis(200)) {
                target.abort();
                return;
            }
        }
    });
    let settled = submission.wait(&Ctx::with_signal(&cancel));
    done.abort();
    let _ = watcher.join();
    let settled = match settled {
        Ok(settled) => settled,
        Err(error) => {
            let _ = handle.abort(false, &Ctx::background());
            return Err(run.signal.reason().unwrap_or_else(|| error.to_string()));
        }
    };
    match (settled.status, settled.answer) {
        (SubmissionStatus::Done, Some(answer)) => {
            let text = chat
                .commit(|tx| Ok(tx.entry(answer)?.and_then(|entry| entry.model)))
                .map_err(|e| e.to_string())?
                .and_then(|messages| messages.into_iter().next())
                .map(|message| answer_text(&message))
                .unwrap_or_default();
            Ok(LaneReply {
                text,
                ..LaneReply::default()
            })
        }
        _ => Err(format!(
            "Lane {} ended without an answer: {}",
            run.lane,
            settled
                .detail
                .as_ref()
                .and_then(Value::as_str)
                .or(settled.reason.as_deref())
                .unwrap_or("unanswered")
        )),
    }
}

fn answer_text(message: &Value) -> String {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}
