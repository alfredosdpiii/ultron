//! Port of `packages/agent/src/harness/runtime/reducer.ts`: folds one harness event into a lane snapshot, so a
//! watcher keeps a replica of the lane (its open operation, running tools, transcript, queues and stats) from the
//! event stream alone. Navigation completion cannot be folded: it asks for a fresh snapshot.
//!
//! Snapshots and events are the JSON shapes the TypeScript runtime publishes (`LaneSnapshot`, `HarnessEvent`);
//! fields the TypeScript leaves `undefined` are absent here.

use serde_json::{Map, Value, json};

/// The snapshot cannot be advanced from the event; the watcher must take a fresh one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reduction {
    Rebase,
}

/// `{ ...fields }` where an `undefined` field is left out.
fn object(fields: &[(&str, Option<&Value>)]) -> Value {
    let mut map = Map::new();
    for (key, value) in fields {
        if let Some(value) = value {
            map.insert((*key).into(), (*value).clone());
        }
    }
    Value::Object(map)
}

fn open_operation(kind: &str, event: &Value, from_tip: &Value) -> Value {
    object(&[
        ("id", event.get("runId")),
        ("kind", Some(&kind.into())),
        ("startedAt", event.get("startedAt")),
        ("fromTipId", Some(from_tip)),
        ("status", Some(&"open".into())),
        ("runningTools", Some(&json!([]))),
    ])
}

/// The open operation when its ID is `id` (`matchingOperation`).
fn matching<'a>(snapshot: &'a mut Value, id: Option<&Value>) -> Option<&'a mut Map<String, Value>> {
    let operation = snapshot.get_mut("operation")?.as_object_mut()?;
    (id.is_some() && operation.get("id") == id).then_some(operation)
}

fn tools(operation: &mut Map<String, Value>) -> Option<&mut Vec<Value>> {
    operation.get_mut("runningTools")?.as_array_mut()
}

fn position(tools: &[Value], call: Option<&Value>) -> Option<usize> {
    tools.iter().position(|tool| tool.get("toolCallId") == call)
}

/// `result.status === "failed" ? { error } : {}`: the error is kept only for failed operations.
fn failure(event: &Value) -> Option<&Value> {
    (event.get("status").and_then(Value::as_str) == Some("failed"))
        .then(|| event.get("error"))
        .flatten()
}

fn increment(value: &mut Value) {
    *value = match (value.as_u64(), value.as_f64()) {
        (Some(n), _) => json!(n + 1),
        (None, Some(n)) => json!(n + 1.0),
        (None, None) => return,
    };
}

/// Apply one harness event to a lane snapshot (`reduceLaneSnapshot`).
pub fn reduce_lane_snapshot(snapshot: &mut Value, event: &Value) -> Option<Reduction> {
    let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
    if let Some(lane) = event.get("lane")
        && Some(lane) != snapshot.get("lane")
        && kind != "usage"
    {
        return None;
    }
    let run_id = event.get("runId");
    match kind {
        "run_start" | "navigation_start" => {
            let tip = snapshot["tipId"].clone();
            let kind = if kind == "run_start" { "run" } else { "navigation" };
            snapshot["operation"] = open_operation(kind, event, &tip);
        }
        "compaction_start" => {
            if snapshot["operation"].is_null() {
                let tip = snapshot["tipId"].clone();
                snapshot["operation"] = open_operation("compaction", event, &tip);
            }
        }
        "operation_abort" => {
            if let Some(operation) = matching(snapshot, event.get("operationId")) {
                operation.insert("status".into(), "aborting".into());
            }
        }
        "run_resume" => {
            if let Some(operation) = matching(snapshot, run_id) {
                operation.shift_remove("deferred");
            }
        }
        "run_suspend" => {
            if let Some(operation) = matching(snapshot, run_id) {
                operation.shift_remove("streamingMessage");
                let deferred = object(&[("handle", event.get("deferred")), ("poll", event.get("poll"))]);
                operation.insert("deferred".into(), deferred);
            }
        }
        "retry_scheduled" => {
            if let Some(operation) = matching(snapshot, run_id) {
                let retry = object(&[
                    ("attempt", event.get("attempt")),
                    ("maxAttempts", event.get("maxAttempts")),
                    ("nextAttemptAt", event.get("notBefore")),
                ]);
                operation.insert("retry".into(), retry);
            }
        }
        "retry_start" | "retry_end" => {
            if let Some(operation) = matching(snapshot, run_id) {
                operation.shift_remove("retry");
            }
        }
        "message_start" | "message_update" => {
            let message = &event["message"];
            if message["role"] != "assistant" {
                return None;
            }
            if kind == "message_start" && (run_id.is_none() || message["stopReason"] != "pending") {
                return None;
            }
            if let Some(operation) = matching(snapshot, run_id) {
                operation.insert("streamingMessage".into(), message.clone());
            }
        }
        "message_end" => {
            if let Some(operation) = matching(snapshot, run_id) {
                operation.shift_remove("streamingMessage");
            }
        }
        "tool_start" => {
            let tools = matching(snapshot, run_id).and_then(tools)?;
            let tool = object(&[
                ("status", Some(&"running".into())),
                ("toolCallId", event.get("toolCallId")),
                ("toolName", event.get("toolName")),
                ("args", event.get("args")),
            ]);
            match position(tools, event.get("toolCallId")) {
                Some(index) => tools[index] = tool,
                None => tools.push(tool),
            }
        }
        "tool_update" => {
            let tools = matching(snapshot, run_id).and_then(tools)?;
            if let Some(index) = position(tools, event.get("toolCallId"))
                && tools[index]["status"] == "running"
                && let Some(partial) = event.get("partialResult")
            {
                tools[index]["result"] = partial.clone();
            }
        }
        "tool_end" => {
            let tools = matching(snapshot, run_id).and_then(tools)?;
            let index = position(tools, event.get("toolCallId"))?;
            let args = tools[index].get("args").cloned();
            tools[index] = object(&[
                ("status", Some(&"settled".into())),
                ("toolCallId", event.get("toolCallId")),
                ("toolName", event.get("toolName")),
                ("args", args.as_ref()),
                ("result", event.get("result")),
                ("isError", event.get("isError")),
            ]);
        }
        "entry_added" => {
            let entry = &event["entry"];
            let is_message = entry["type"] == "message";
            if is_message
                && entry["message"]["role"] == "toolResult"
                && let Some(tools) = snapshot
                    .get_mut("operation")
                    .and_then(Value::as_object_mut)
                    .and_then(tools)
                && let Some(index) = position(tools, entry["message"].get("toolCallId"))
            {
                tools.remove(index);
            }
            if let Some(transcript) = snapshot.get_mut("transcript").and_then(Value::as_array_mut) {
                if entry["type"] == "compaction" {
                    transcript.clear();
                }
                transcript.push(entry.clone());
            }
            snapshot["tipId"] = entry["id"].clone();
            if is_message {
                increment(&mut snapshot["stats"]["messageCount"]);
            }
        }
        "queue_update" => snapshot["queues"] = event["queues"].clone(),
        "usage" => snapshot["stats"]["usage"] = event["totals"].clone(),
        "config_update" => {
            event.get("lane")?;
            let field = match event["property"].as_str() {
                Some("model") => "model",
                Some("thinkingLevel") => "thinkingLevel",
                Some("activeTools") => "activeToolNames",
                _ => return None,
            };
            snapshot["configuration"][field] = event["value"].clone();
        }
        "run_end" => {
            let tip = event.get("tipId");
            let operation = matching(snapshot, run_id).filter(|op| op.get("kind") == Some(&"run".into()))?;
            let record = object(&[
                ("operationId", run_id),
                ("kind", Some(&"run".into())),
                ("status", event.get("status")),
                ("error", failure(event)),
                ("fromTipId", event.get("fromTipId")),
                ("tipId", tip),
                ("startedAt", operation.get("startedAt")),
                ("endedAt", event.get("endedAt")),
            ]);
            snapshot["lastResult"] = record;
            snapshot["operation"] = Value::Null;
            snapshot["tipId"] = tip.cloned().unwrap_or(Value::Null);
        }
        "compaction_end" => {
            let tip = snapshot["tipId"].clone();
            let operation = matching(snapshot, run_id).filter(|op| op.get("kind") == Some(&"compaction".into()))?;
            let record = object(&[
                ("operationId", run_id),
                ("kind", Some(&"compaction".into())),
                ("status", event.get("status")),
                ("error", failure(event)),
                ("fromTipId", operation.get("fromTipId")),
                ("tipId", Some(&tip)),
                ("startedAt", operation.get("startedAt")),
                ("endedAt", event.get("endedAt")),
            ]);
            snapshot["lastResult"] = record;
            snapshot["operation"] = Value::Null;
        }
        "navigation_end" => return Some(Reduction::Rebase),
        "fault" => snapshot["faulted"] = true.into(),
        _ => {}
    }
    None
}
