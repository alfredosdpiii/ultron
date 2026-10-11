//! The worker's main lane as presentations see it: the `pi.transcript` state `{ snapshot, event }`
//! (experimental/services/transcript-provider.ts), folded with the harness reducer (crates/harness) from the
//! lane-watch events the agent loop's events translate to.
//!
//! The session actor (crates/session-core `core`) reports the lane-watch events; entries are the format-4 journal's
//! (with their `seq` and numeric `timestamp`).

use crate::chord::State;
use agent::AgentEvent;
use serde_json::{Map, Value, json};
use std::sync::Arc;

pub const LANE: &str = "main";

fn zero_usage() -> Value {
    json!({"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
        "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}})
}

fn add(total: &mut Value, usage: &Value) {
    let (Some(total), Some(usage)) = (total.as_object_mut(), usage.as_object()) else {
        return;
    };
    for (key, value) in usage {
        if key == "cost" {
            add(total.entry("cost").or_insert_with(|| json!({})), value);
            continue;
        }
        if let Some(n) = value.as_f64() {
            let current = total.get(key).and_then(Value::as_f64).unwrap_or(0.0);
            let sum = current + n;
            let next = if sum.fract() == 0.0 && sum.abs() < 9e15 {
                json!(sum as i64)
            } else {
                json!(sum)
            };
            total.insert(key.clone(), next);
        }
    }
}

/// Entries as the transcript shows them: the stored entry plus its branch position as `seq`.
fn transcript_entry(entry: &Value, seq: usize) -> Value {
    let mut entry = entry.clone();
    if let Some(map) = entry.as_object_mut() {
        map.insert("seq".into(), json!(seq));
    }
    entry
}

/// The lane snapshot of a session whose branch holds `entries`.
pub fn initial_snapshot(provider: &str, model_id: &str, thinking: &str, entries: &[Value]) -> Value {
    let mut usage = zero_usage();
    for entry in entries {
        if entry["message"]["role"] == "assistant" {
            add(&mut usage, &entry["message"]["usage"]);
        }
    }
    let transcript: Vec<Value> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| transcript_entry(e, i))
        .collect();
    json!({
        "lane": LANE,
        "transcript": transcript,
        "tipId": entries.last().map_or(Value::Null, |e| e["id"].clone()),
        "configuration": {"model": {"provider": provider, "modelId": model_id}, "thinkingLevel": thinking, "activeToolNames": ["rlm"]},
        "stats": {"messageCount": entries.len(), "usage": usage},
        "operation": null,
        "queues": [],
        "faulted": false,
    })
}

fn event(fields: &[(&str, Value)]) -> Value {
    Value::Object(
        fields
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect::<Map<_, _>>(),
    )
}

/// The transcript service state of one lane and the run in progress.
pub struct Lane {
    pub transcript: Arc<State>,
    run_id: Option<String>,
    from_tip: Value,
}

impl Lane {
    pub fn new(snapshot: Value) -> Lane {
        Lane {
            // The provider starts at `{ snapshot: null }` and publishes the lane snapshot once on activation.
            transcript: State::with_sequence(json!({"snapshot": snapshot, "event": null}), 1),
            run_id: None,
            from_tip: Value::Null,
        }
    }

    /// Replace the snapshot (the session actor's, once it opened).
    pub fn reset(&self, snapshot: Value) {
        self.transcript.change(|state| state["snapshot"] = snapshot);
    }

    pub fn snapshot(&self) -> Value {
        self.transcript.value()["snapshot"].clone()
    }

    /// Fold one harness event into the snapshot and publish it as the current event.
    pub fn publish(&self, harness_event: Value) {
        self.transcript.change(|state| {
            harness::reducer::reduce_lane_snapshot(&mut state["snapshot"], &harness_event);
            state["event"] = harness_event;
        });
    }

    pub fn start_run(&mut self, run_id: &str, started_at: i64) {
        self.run_id = Some(run_id.to_string());
        self.from_tip = self.snapshot()["tipId"].clone();
        self.publish(event(&[
            ("type", json!("run_start")),
            ("runId", json!(run_id)),
            ("startedAt", json!(started_at)),
            ("lane", json!(LANE)),
        ]));
    }

    pub fn end_run(&mut self, ended_at: i64, error: Option<&str>) {
        let Some(run_id) = self.run_id.take() else { return };
        let tip = self.snapshot()["tipId"].clone();
        let mut fields = vec![
            ("type", json!("run_end")),
            ("lane", json!(LANE)),
            ("runId", json!(run_id)),
            ("status", json!(if error.is_some() { "failed" } else { "completed" })),
        ];
        if let Some(message) = error {
            fields.push(("error", json!({"code": "run_failed", "message": message})));
        }
        fields.extend([
            ("fromTipId", self.from_tip.clone()),
            ("tipId", tip),
            ("endedAt", json!(ended_at)),
        ]);
        self.publish(event(&fields));
    }

    /// Translate one agent event (with the stored entry of an ended message) into lane-watch events.
    pub fn agent_event(&mut self, agent_event: &AgentEvent, entry: Option<&Value>) {
        let run = json!(self.run_id.clone().unwrap_or_default());
        let lane = json!(LANE);
        match agent_event {
            AgentEvent::MessageStart { message: m } => self.publish(event(&[
                ("type", json!("message_start")),
                ("runId", run),
                ("lane", lane),
                ("message", serde_json::to_value(m).unwrap_or(Value::Null)),
            ])),
            AgentEvent::MessageUpdate {
                message: m,
                assistant_message_event,
            } => {
                let mut frame = serde_json::to_value(assistant_message_event).unwrap_or(Value::Null);
                if let Some(map) = frame.as_object_mut() {
                    map.remove("partial");
                }
                self.publish(event(&[
                    ("type", json!("message_update")),
                    ("runId", run),
                    ("lane", lane),
                    ("message", serde_json::to_value(m).unwrap_or(Value::Null)),
                    ("frame", frame),
                ]));
            }
            AgentEvent::MessageEnd { message: m } => {
                let entry_id = entry.map_or(Value::Null, |e| e["id"].clone());
                self.publish(event(&[
                    ("type", json!("message_end")),
                    ("runId", run),
                    ("lane", lane.clone()),
                    ("message", serde_json::to_value(m).unwrap_or(Value::Null)),
                    ("entryId", entry_id.clone()),
                ]));
                let Some(entry) = entry else { return };
                let seq = self.snapshot()["transcript"].as_array().map_or(0, Vec::len);
                self.publish(event(&[
                    ("type", json!("entry_added")),
                    ("lane", lane.clone()),
                    ("entry", transcript_entry(entry, seq)),
                ]));
                if entry["message"]["role"] == "assistant" {
                    let mut totals = self.snapshot()["stats"]["usage"].clone();
                    let usage = entry["message"]["usage"].clone();
                    add(&mut totals, &usage);
                    self.publish(event(&[
                        ("type", json!("usage")),
                        ("lane", lane),
                        (
                            "row",
                            json!({"id": crate::paths::uuid_v4(), "usage": usage, "entryId": entry_id, "adjustment": false, "seq": seq}),
                        ),
                        ("totals", totals),
                    ]));
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => self.publish(event(&[
                ("type", json!("tool_start")),
                ("runId", run),
                ("lane", lane),
                ("toolCallId", json!(tool_call_id)),
                ("toolName", json!(tool_name)),
                ("args", args.clone()),
            ])),
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                tool_name,
                args,
                partial_result,
            } => self.publish(event(&[
                ("type", json!("tool_update")),
                ("runId", run),
                ("lane", lane),
                ("toolCallId", json!(tool_call_id)),
                ("toolName", json!(tool_name)),
                ("args", args.clone()),
                (
                    "partialResult",
                    serde_json::to_value(partial_result).unwrap_or(Value::Null),
                ),
            ])),
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
                is_error,
                ..
            } => self.publish(event(&[
                ("type", json!("tool_end")),
                ("runId", run),
                ("lane", lane),
                ("toolCallId", json!(tool_call_id)),
                ("toolName", json!(tool_name)),
                ("result", serde_json::to_value(result).unwrap_or(Value::Null)),
                ("isError", json!(is_error)),
            ])),
            _ => {}
        }
    }
}
