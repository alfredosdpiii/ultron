//! Recovery of a session whose worker died: the lane's open operation (a run whose terminal transaction never
//! committed) is settled from what the journal committed, without another provider call and without replaying any
//! cell, and the queued messages that were waiting in the lane's inbox are handed back to the queues.
//!
//! The settlement follows the TypeScript harness (`drive/recovery.ts`): an assistant request that was in flight
//! becomes an interrupted error message built from its committed frame prefix; a tool batch whose calls never
//! reported becomes error results whose outcome is unknown (cells are never replayed, `replay: "never"`). The
//! operation then ends `failed` with code `interrupted` (a cancel request that was already recorded ends it
//! `aborted`). Whether the work continues is the caller's choice ([`Recovered::resume`]).

use serde_json::{Map, Value, json};
use session_journal::Session;
use session_journal::Write;
use session_journal::run::{OP_META, OP_RESULT, OP_STATE, OP_TOOL_ARGS, PENDING_ENTRY, PENDING_FRAMES};
use session_journal::session::{BRANCH_TIP, LANE_STATE};
use std::collections::BTreeMap;

const LANE: &str = "main";

/// `interruptedAssistantMessage`'s warning.
pub const INTERRUPTED_ASSISTANT: &str = "Assistant request was interrupted. The preceding content is the latest committed partial; newer live output may be missing and the external outcome is unknown.";

/// The result of a cell that was running when the worker died.
pub const INTERRUPTED_CELL: &str = "Interrupted: the session worker stopped while this cell ran. Its outcome is unknown and it was not replayed; the kernel was restored from its last checkpoint.";

/// The prompt of a run that continues interrupted work.
pub const CONTINUE_PROMPT: &str = "[Ultron] The session worker stopped during the previous turn and was restarted. Committed work is intact; the step that was running when it stopped was interrupted (see above). Continue the task from where it stopped.";

/// The user message field that counts consecutive recoveries (a continuation prompt carries its depth).
pub const RECOVERY_DEPTH: &str = "ultronRecovery";

/// How many interrupted continuations in a row are resumed before recovery stops resuming.
pub const MAX_RECOVERY_DEPTH: u64 = 2;

/// A queued message found in the lane's inbox.
#[derive(Clone, Debug, PartialEq)]
pub struct Inboxed {
    pub entry_id: String,
    /// `steer` or `followUp`.
    pub kind: String,
    pub message: Value,
}

/// What [`settle`] found and did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recovered {
    /// The interrupted operation it settled.
    pub operation_id: Option<String>,
    /// The terminal status it recorded (`failed`, `aborted`, or `completed` for a run whose last answer had
    /// already committed).
    pub status: Option<String>,
    /// The interrupted work should continue: the run had more to do and was not cancelled.
    pub resume: bool,
    /// The continuation depth of the interrupted run (0: the user's own prompt).
    pub depth: u64,
    /// Calls that were running and got interrupted results.
    pub interrupted_calls: usize,
    /// Messages still waiting in the lane's inbox, in order.
    pub queued: Vec<Inboxed>,
}

fn entry(id: &str, parent: Option<&str>, message: Value) -> Write {
    let mut map = Map::new();
    map.insert("id".into(), id.into());
    map.insert("parentId".into(), parent.map_or(Value::Null, Value::from));
    map.insert("type".into(), "message".into());
    map.insert("message".into(), message);
    Write::Entry(map)
}

fn zero_usage() -> Value {
    json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}})
}

/// The committed frame prefix of a response reduced to its text and thinking (`reduceAssistantMessageFrames`
/// without tool calls: a call that never ran has no result, so it is not kept).
pub fn reduce_frames(frames: &[Value]) -> Option<Value> {
    let mut message: Option<Value> = None;
    let mut blocks: BTreeMap<u64, Value> = BTreeMap::new();
    for frame in frames {
        let index = frame["contentIndex"].as_u64().unwrap_or(0);
        match frame["type"].as_str() {
            Some("start") => message = Some(frame["partial"].clone()),
            Some("text_start" | "thinking_start") => {
                blocks.insert(index, frame["content"].clone());
            }
            Some(kind @ ("text_delta" | "thinking_delta")) => {
                let key = if kind == "text_delta" { "text" } else { "thinking" };
                if let Some(block) = blocks.get_mut(&index) {
                    let text = format!(
                        "{}{}",
                        block[key].as_str().unwrap_or(""),
                        frame["delta"].as_str().unwrap_or("")
                    );
                    block[key] = json!(text);
                }
            }
            Some(kind @ ("text_end" | "thinking_end")) => {
                let key = if kind == "text_end" { "text" } else { "thinking" };
                if let Some(block) = blocks.get_mut(&index)
                    && let Some(content) = frame["content"].as_str()
                {
                    block[key] = json!(content);
                }
            }
            _ => {}
        }
    }
    let mut message = message?;
    message["content"] = Value::Array(blocks.into_values().filter(|b| b["type"] != "toolCall").collect());
    Some(message)
}

/// `interruptedAssistantMessage`: the partial (or an empty message of `model`) as a settled error.
pub fn interrupted_assistant(model: &Value, partial: Option<Value>) -> Value {
    let mut message = partial.unwrap_or_else(|| {
        json!({"role": "assistant", "content": [], "api": "unknown",
            "provider": model["provider"], "model": model["modelId"]})
    });
    message["usage"] = zero_usage();
    message["stopReason"] = json!("error");
    message["errorMessage"] = json!(INTERRUPTED_ASSISTANT);
    message["timestamp"] = json!(crate::lane::now_ms());
    message
}

/// The lane's queued messages: inbox items whose pending entries are still stored.
pub fn inbox(session: &Session) -> Vec<Inboxed> {
    let state = session.value(LANE_STATE, LANE).cloned().unwrap_or(Value::Null);
    state["inbox"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let entry_id = item["entryId"].as_str()?.to_string();
            let pending = session.value(PENDING_ENTRY, &entry_id)?;
            Some(Inboxed {
                kind: item["kind"].as_str().unwrap_or("followUp").to_string(),
                message: pending["payload"].clone(),
                entry_id,
            })
        })
        .collect()
}

/// The continuation depth of the run whose operation is `meta`: its prompt's [`RECOVERY_DEPTH`], else 0.
fn depth(session: &Session, meta: &Value) -> u64 {
    meta["intent"]["promptEntryIds"][0]
        .as_str()
        .and_then(|id| session.state().entry(id))
        .and_then(|entry| entry["message"][RECOVERY_DEPTH].as_u64())
        .unwrap_or(0)
}

/// Settle the lane's open operation, if any, in one transaction; report it and the inbox.
pub fn settle(session: &mut Session) -> Result<Recovered, String> {
    let lane_state = session.value(LANE_STATE, LANE).cloned().unwrap_or(Value::Null);
    let Some(operation) = lane_state["currentOperationId"].as_str().map(str::to_string) else {
        return Ok(Recovered {
            queued: inbox(session),
            ..Recovered::default()
        });
    };
    let state = session.value(OP_STATE, &operation).cloned().unwrap_or(Value::Null);
    let meta = session.value(OP_META, &operation).cloned().unwrap_or(Value::Null);
    let cancelled = state["control"]["status"] == "cancel_requested";
    let mut writes = Vec::new();
    let mut tip = session.branch_tip(LANE).flatten();
    let mut interrupted_calls = 0;
    let at = state["at"].as_str().unwrap_or_default();
    match at {
        "assistant.effect_pending" => {
            let response = state["responseEntryId"].as_str().unwrap_or_default().to_string();
            let key = format!("{operation}:{response}");
            let frames: Vec<Value> = session
                .state()
                .read_list(PENDING_FRAMES, &key)
                .into_iter()
                .map(|e| e.value.clone())
                .collect();
            let model = &state["generationContext"]["configuration"]["model"];
            let message = interrupted_assistant(model, reduce_frames(&frames));
            writes.push(entry(&response, tip.as_deref(), message));
            writes.push(Write::delete_list(PENDING_FRAMES, &key));
            tip = Some(response);
        }
        "tools" => {
            let batch = &state["batch"];
            let assistant = batch["assistantEntryId"].as_str().unwrap_or_default();
            let content = session
                .state()
                .entry(assistant)
                .map(|e| e["message"]["content"].clone())
                .unwrap_or(Value::Null);
            for call in batch["calls"].as_array().into_iter().flatten() {
                let result_id = call["resultEntryId"].as_str().unwrap_or_default().to_string();
                let block = &content[call["sourceIndex"].as_u64().unwrap_or(0) as usize];
                let result = match session.value(PENDING_ENTRY, &result_id) {
                    Some(pending) => {
                        let payload = pending["payload"].clone();
                        writes.push(Write::delete(PENDING_ENTRY, &result_id));
                        payload
                    }
                    None => {
                        interrupted_calls += 1;
                        json!({"role": "toolResult", "toolCallId": block["id"], "toolName": block["name"],
                            "content": [{"type": "text", "text": INTERRUPTED_CELL}], "isError": true,
                            "timestamp": crate::lane::now_ms()})
                    }
                };
                writes.push(entry(&result_id, tip.as_deref(), result));
                let args_key = format!(
                    "{operation}:{}:{}",
                    batch["turnId"].as_str().unwrap_or_default(),
                    call["sourceIndex"]
                );
                if session.value(OP_TOOL_ARGS, &args_key).is_some() {
                    writes.push(Write::delete(OP_TOOL_ARGS, &args_key));
                }
                tip = Some(result_id);
            }
        }
        // A navigation without a summary that was admitted commits (`commitNavigation`: nothing external is
        // repeated), unless it was cancelled.
        "navigation.ready_to_commit" if !cancelled => {
            tip = state["targetId"].as_str().map(str::to_string);
            let labelled = state["labelEntryId"]
                .as_str()
                .map(str::to_string)
                .or_else(|| tip.clone());
            if let (Some(entry), Some(label)) = (labelled, state["label"].as_str()) {
                writes.push(Write::set(crate::compaction_op::ENTRY_LABEL, &entry, json!(label)));
            }
            if tip.is_none() {
                writes.push(Write::set(BRANCH_TIP, LANE, Value::Null));
            }
        }
        _ => {}
    }
    if let Some(tip) = &tip {
        writes.push(Write::set(BRANCH_TIP, LANE, json!(tip)));
    }
    // A run whose last answer committed and may finish had nothing left to do; so had an admitted navigation.
    let finished =
        (at == "checkpoint" && state["continuation"]["kind"] == "may_finish") || at == "navigation.ready_to_commit";
    let status = if cancelled {
        "aborted"
    } else if finished {
        "completed"
    } else {
        "failed"
    };
    let mut result = Map::new();
    result.insert("operationId".into(), operation.clone().into());
    result.insert("kind".into(), meta["intent"]["kind"].as_str().unwrap_or("run").into());
    result.insert("status".into(), status.into());
    if status == "failed" {
        result.insert(
            "error".into(),
            json!({"code": "interrupted", "message": "The session worker stopped during this run"}),
        );
    }
    result.insert("fromTipId".into(), meta["sourceTipId"].clone());
    result.insert("tipId".into(), tip.clone().map_or(Value::Null, Value::from));
    result.insert("startedAt".into(), meta["startedAt"].clone());
    result.insert("endedAt".into(), json!(crate::lane::now_ms()));
    // The inbox keeps the queued messages that are still pending.
    let queued = inbox(session);
    let kept: Vec<Value> = queued
        .iter()
        .map(|q| json!({"entryId": q.entry_id, "kind": q.kind}))
        .collect();
    // A compaction's preparation goes with its operation (`operationCleanupWrites`).
    let preparations: Vec<String> = session
        .state()
        .scan_values(crate::op_journal::OP_PREPARATION, &format!("{operation}:"))
        .into_iter()
        .map(|stored| stored.key.clone())
        .collect();
    writes.extend(
        preparations
            .iter()
            .map(|key| Write::delete(crate::op_journal::OP_PREPARATION, key)),
    );
    writes.extend([
        Write::delete(OP_META, &operation),
        Write::delete(OP_STATE, &operation),
        Write::set(OP_RESULT, &operation, Value::Object(result)),
        Write::set(
            LANE_STATE,
            LANE,
            json!({"currentOperationId": null, "lastOperationId": operation, "inbox": kept}),
        ),
    ]);
    session.commit(&writes)?;
    let depth = depth(session, &meta);
    let run = meta["intent"]["kind"].as_str().is_none_or(|k| k == "run");
    Ok(Recovered {
        operation_id: Some(operation),
        status: Some(status.to_string()),
        resume: run && status == "failed" && depth < MAX_RECOVERY_DEPTH,
        depth,
        interrupted_calls,
        queued,
    })
}
