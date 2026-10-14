//! `experimental/rpc-events.ts`: lane events become the Pi RPC event stream. The lane's own turn boundaries are not
//! forwarded, so turns are reconstructed from message boundaries, and stream events are reshaped into Pi's
//! partial-free `assistantMessageEvent` form.

use ai::AssistantMessageEvent;
use serde_json::{Map, Value, json};

/// A lane watch event (`LaneWatchEvent`), the input of [`RpcEventTranslator`].
#[derive(Clone, Debug, PartialEq)]
pub enum LaneEvent {
    RunStart,
    /// `run_end`; `error` is set when the run failed.
    RunEnd {
        error: Option<String>,
        ended_at: i64,
    },
    MessageStart(Value),
    /// An assistant message changed: its usage now, and the provider event (`frame`).
    MessageUpdate {
        usage: Value,
        event: Value,
    },
    /// `message_end`; `in_run` when it belongs to the current run.
    MessageEnd {
        message: Value,
        in_run: bool,
    },
    ToolStart {
        id: String,
        name: String,
        args: Value,
    },
    ToolUpdate {
        id: String,
        name: String,
        partial: Value,
    },
    ToolEnd {
        id: String,
        name: String,
        result: Value,
        is_error: bool,
    },
    /// The queued messages after a change: `(kind, text)` with kind `steer`, `followUp` or `nextRun`.
    QueueUpdate(Vec<(String, String)>),
    CompactionStart {
        reason: String,
    },
    CompactionEnd {
        reason: String,
        aborted: bool,
        error: Option<String>,
    },
    EntryAdded(Value),
    ThinkingLevel(String),
    /// AgentSession's `summarization_retry_scheduled` / `_attempt_start` / `_finished` (a compaction's or branch
    /// summary's retry), as the RPC stream carries them.
    SummarizationRetry(Value),
}

impl LaneEvent {
    /// The event a lane-watch event (`LaneWatchEvent` JSON, as the transcript state carries it) is; `None` for
    /// events the RPC stream does not show.
    pub fn from_watch(event: &Value) -> Option<LaneEvent> {
        let text = |key: &str| event[key].as_str().unwrap_or_default().to_string();
        Some(match event["type"].as_str()? {
            "run_start" => LaneEvent::RunStart,
            "run_end" => LaneEvent::RunEnd {
                error: (event["status"] == "failed")
                    .then(|| event["error"]["message"].as_str().map(str::to_string))
                    .flatten(),
                ended_at: event["endedAt"].as_i64().unwrap_or(0),
            },
            "message_start" => LaneEvent::MessageStart(event["message"].clone()),
            "message_update" => {
                if event["message"]["role"] != "assistant" || event["frame"].is_null() {
                    return None;
                }
                LaneEvent::MessageUpdate {
                    usage: event["message"]["usage"].clone(),
                    event: event["frame"].clone(),
                }
            }
            "message_end" => LaneEvent::MessageEnd {
                message: event["message"].clone(),
                in_run: event.get("runId").is_some_and(|r| !r.is_null()),
            },
            "tool_start" => LaneEvent::ToolStart {
                id: text("toolCallId"),
                name: text("toolName"),
                args: event["args"].clone(),
            },
            "tool_update" => LaneEvent::ToolUpdate {
                id: text("toolCallId"),
                name: text("toolName"),
                partial: event["partialResult"].clone(),
            },
            "tool_end" => LaneEvent::ToolEnd {
                id: text("toolCallId"),
                name: text("toolName"),
                result: event["result"].clone(),
                is_error: event["isError"] == true,
            },
            "queue_update" => LaneEvent::QueueUpdate(
                event["queues"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|item| item["type"] == "message")
                    .map(|item| (text_of(&item["kind"]), message_text(&item["message"])))
                    .collect(),
            ),
            "compaction_start" => LaneEvent::CompactionStart { reason: text("reason") },
            "compaction_end" => LaneEvent::CompactionEnd {
                reason: text("reason"),
                aborted: event["status"] == "aborted",
                error: (event["status"] == "failed")
                    .then(|| event["error"]["message"].as_str().unwrap_or_default().to_string()),
            },
            "entry_added" => LaneEvent::EntryAdded(event["entry"].clone()),
            "config_update" if event["property"] == "thinkingLevel" => LaneEvent::ThinkingLevel(text("value")),
            "summarization_retry_scheduled" | "summarization_retry_attempt_start" | "summarization_retry_finished" => {
                let mut shown = event.as_object().cloned().unwrap_or_default();
                shown.remove("lane");
                LaneEvent::SummarizationRetry(Value::Object(shown))
            }
            _ => return None,
        })
    }
}

fn text_of(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

/// Translates lane events into Pi RPC wire events.
#[derive(Default)]
pub struct RpcEventTranslator {
    run_messages: Vec<Value>,
    turn_open: bool,
    turn_tool_results: Vec<Value>,
    tool_args: Vec<(String, Value)>,
}

fn role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

impl RpcEventTranslator {
    pub fn translate(&mut self, event: LaneEvent) -> Vec<Value> {
        match event {
            LaneEvent::RunStart => {
                self.run_messages.clear();
                self.turn_open = false;
                self.turn_tool_results.clear();
                vec![json!({"type": "agent_start"})]
            }
            LaneEvent::RunEnd { error, ended_at } => {
                let mut events = self.close_turn();
                // A run that failed before any response has no assistant error message; Pi clients learn of a
                // failed run from one, so it is shown as one.
                let answered_with_error = self
                    .run_messages
                    .last()
                    .is_some_and(|m| role(m) == Some("assistant") && m["stopReason"] == "error");
                if let Some(error) = error
                    && !answered_with_error
                {
                    let message = run_failure_message(&error, ended_at);
                    self.run_messages.push(message.clone());
                    events.push(json!({"type": "message_start", "message": message}));
                    events.push(json!({"type": "message_end", "message": message}));
                }
                let messages = std::mem::take(&mut self.run_messages);
                events.push(json!({"type": "agent_end", "messages": messages, "willRetry": false}));
                events.push(json!({"type": "agent_settled"}));
                events
            }
            LaneEvent::MessageStart(message) => {
                let mut events = Vec::new();
                if role(&message) == Some("assistant") {
                    events.extend(self.close_turn());
                    self.turn_open = true;
                    events.push(json!({"type": "turn_start"}));
                }
                events.push(json!({"type": "message_start", "message": message}));
                events
            }
            LaneEvent::MessageUpdate { usage, event } => match frame_to_assistant_message_event(&event) {
                Some(update) => {
                    vec![json!({"type": "message_update", "usage": usage, "assistantMessageEvent": update})]
                }
                None => Vec::new(),
            },
            LaneEvent::MessageEnd { message, in_run } => {
                if in_run {
                    self.run_messages.push(message.clone());
                }
                if role(&message) == Some("toolResult") {
                    self.turn_tool_results.push(message.clone());
                }
                vec![json!({"type": "message_end", "message": message})]
            }
            LaneEvent::ToolStart { id, name, args } => {
                self.tool_args.push((id.clone(), args.clone()));
                vec![json!({"type": "tool_execution_start", "toolCallId": id, "toolName": name, "args": args})]
            }
            LaneEvent::ToolUpdate { id, name, partial } => {
                let mut event = json!({"type": "tool_execution_update", "toolCallId": id, "toolName": name});
                if let Some((_, args)) = self.tool_args.iter().find(|(known, _)| *known == id) {
                    event["args"] = args.clone();
                }
                event["partialResult"] = partial;
                vec![event]
            }
            LaneEvent::ToolEnd {
                id,
                name,
                result,
                is_error,
            } => {
                self.tool_args.retain(|(known, _)| *known != id);
                vec![json!({
                    "type": "tool_execution_end", "toolCallId": id, "toolName": name, "result": result, "isError": is_error,
                })]
            }
            LaneEvent::QueueUpdate(queues) => vec![queue_update(&queues)],
            LaneEvent::CompactionStart { reason } => vec![json!({"type": "compaction_start", "reason": reason})],
            LaneEvent::CompactionEnd { reason, aborted, error } => {
                let mut event =
                    json!({"type": "compaction_end", "reason": reason, "aborted": aborted, "willRetry": false});
                if let Some(error) = error {
                    event["errorMessage"] = error.into();
                }
                vec![event]
            }
            LaneEvent::EntryAdded(entry) => vec![json!({"type": "entry_appended", "entry": entry})],
            LaneEvent::ThinkingLevel(level) => vec![json!({"type": "thinking_level_changed", "level": level})],
            LaneEvent::SummarizationRetry(event) => vec![event],
        }
    }

    fn close_turn(&mut self) -> Vec<Value> {
        if !self.turn_open {
            return Vec::new();
        }
        self.turn_open = false;
        let last_assistant = self.run_messages.iter().rev().find(|m| role(m) == Some("assistant"));
        let mut event = json!({"type": "turn_end"});
        if let Some(message) = last_assistant {
            event["message"] = message.clone();
        }
        event["toolResults"] = Value::Array(std::mem::take(&mut self.turn_tool_results));
        vec![event]
    }
}

/// `queueUpdate`: steering texts and follow-up (and next-run) texts.
pub fn queue_update(queues: &[(String, String)]) -> Value {
    let pick = |kinds: &[&str]| -> Vec<&str> {
        queues
            .iter()
            .filter(|(kind, _)| kinds.contains(&kind.as_str()))
            .map(|(_, text)| text.as_str())
            .collect()
    };
    json!({"type": "queue_update", "steering": pick(&["steer"]), "followUp": pick(&["followUp", "nextRun"])})
}

/// `frameToAssistantMessageEvent` over the frame a provider event encodes to: the event without its `partial`;
/// start events of a tool call carry its id and name, end events the whole call. Terminal events have no frame.
pub fn frame_to_assistant_message_event(event: &Value) -> Option<Value> {
    let kind = event.get("type")?.as_str()?;
    let index = event.get("contentIndex").cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    out.insert("type".into(), kind.into());
    match kind {
        "start" => {}
        "text_start" | "thinking_start" => {
            out.insert("contentIndex".into(), index);
        }
        "text_delta" | "thinking_delta" | "toolcall_delta" => {
            let delta = event.get("delta")?.clone();
            // The frame encoder drops empty tool-call deltas.
            if kind == "toolcall_delta" && delta.as_str() == Some("") {
                return None;
            }
            out.insert("contentIndex".into(), index);
            out.insert("delta".into(), delta);
        }
        "text_end" | "thinking_end" => {
            out.insert("contentIndex".into(), index);
            out.insert("content".into(), event.get("content")?.clone());
        }
        "toolcall_start" => {
            // A frame names the call; a provider event carries it in its partial message.
            let (id, name) = match (event.get("id"), event.get("toolName")) {
                (Some(id), Some(name)) => (id.clone(), name.clone()),
                _ => {
                    let call = &event["partial"]["content"][index.as_u64()? as usize];
                    (call.get("id")?.clone(), call.get("name")?.clone())
                }
            };
            out.insert("contentIndex".into(), index);
            out.insert("id".into(), id);
            out.insert("toolName".into(), name);
        }
        "toolcall_end" => {
            let call = event.get("toolCall")?;
            let mut tool_call = Map::new();
            tool_call.insert("type".into(), "toolCall".into());
            for key in ["id", "name", "arguments", "thoughtSignature", "namespace"] {
                if let Some(value) = call.get(key) {
                    tool_call.insert(key.into(), value.clone());
                }
            }
            out.insert("contentIndex".into(), index);
            out.insert("toolCall".into(), Value::Object(tool_call));
        }
        _ => return None,
    }
    Some(Value::Object(out))
}

/// The provider event as the JSON a lane watch carries.
pub fn event_value(event: &AssistantMessageEvent) -> Value {
    serde_json::to_value(event).unwrap_or(Value::Null)
}

/// `messageText`: a string content, or the text blocks joined.
pub use session_core::view::message_text;

fn run_failure_message(error: &str, timestamp: i64) -> Value {
    json!({
        "role": "assistant", "content": [], "api": "openai-completions", "provider": "ultron", "model": "ultron",
        "usage": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
        "stopReason": "error", "errorMessage": error, "timestamp": timestamp,
    })
}
