//! Replays `fixtures/loop.json` (recorded from the TypeScript `Agent` + faux provider by `fixtures/generate.mjs`): the
//! scripted stream function serves each recorded provider call's events, and the Rust agent must emit the same
//! events, send the same transcripts, and end in the same state. Timestamps are wall-clock in both and compared as 0.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use agent::{
    Agent, AgentControl, AgentEvent, AgentOptions, AgentTool, AgentToolResult, QueueMode, StreamFn, ToolExecutionMode,
    ToolUpdate,
};
use ai::http::BoxFuture;
use ai::{
    AbortSignal, AssistantMessage, AssistantMessageEvent, EventSink, Message, Model, SimpleStreamOptions, StopReason,
    TranscriptContext,
};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/loop.json")).unwrap()
}

fn zero_timestamps(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if key == "timestamp" {
                    *v = json!(0);
                } else {
                    zero_timestamps(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(zero_timestamps),
        _ => {}
    }
}

/// Concurrent tools finish in completion order, which differs between JS microtasks and Rust polling: sort each run
/// of consecutive update/end events by tool call id (stable).
fn order_concurrent_completions(events: &mut [Value]) {
    let is_completion = |e: &Value| matches!(e["type"].as_str(), Some("tool_execution_update" | "tool_execution_end"));
    let mut start = 0;
    while start < events.len() {
        let mut end = start;
        while end < events.len() && is_completion(&events[end]) {
            end += 1;
        }
        if end > start {
            events[start..end].sort_by(|a, b| a["toolCallId"].as_str().cmp(&b["toolCallId"].as_str()));
            start = end;
        } else {
            start += 1;
        }
    }
}

struct Scripted {
    calls: Mutex<VecDeque<Vec<AssistantMessageEvent>>>,
    contexts: Arc<Mutex<Vec<Value>>>,
}

impl StreamFn for Scripted {
    fn stream<'a>(
        &'a self,
        _model: &'a Model,
        context: &'a TranscriptContext,
        options: &'a SimpleStreamOptions,
        sink: &'a mut EventSink<'_>,
    ) -> BoxFuture<'a, AssistantMessage> {
        Box::pin(async move {
            self.contexts
                .lock()
                .unwrap()
                .push(serde_json::to_value(context).unwrap());
            let events = self
                .calls
                .lock()
                .unwrap()
                .pop_front()
                .expect("more provider calls than recorded");
            let signal = options.base.signal.clone().expect("loop passes its abort signal");
            for event in events {
                sink(&event);
                match event {
                    AssistantMessageEvent::Done { message, .. } => return message,
                    AssistantMessageEvent::Error { error, .. } => {
                        // The faux provider ends with `aborted` only after seeing the signal: the Rust abort must
                        // have reached the stream by now too.
                        assert_eq!(
                            error.stop_reason == StopReason::Aborted,
                            signal.aborted(),
                            "abort reached the stream"
                        );
                        return error;
                    }
                    _ => {}
                }
            }
            panic!("recorded call has no terminal event")
        })
    }
}

struct Fixed {
    tool: ai::Tool,
    kind: &'static str,
}

impl AgentTool for Fixed {
    fn tool(&self) -> &ai::Tool {
        &self.tool
    }
    fn execute<'a>(
        &'a self,
        _id: &'a str,
        args: Value,
        _signal: &'a AbortSignal,
        on_update: &'a ToolUpdate<'a>,
    ) -> BoxFuture<'a, Result<AgentToolResult, String>> {
        Box::pin(async move {
            let text = |t: &str| json!([{ "type": "text", "text": t }]);
            let result = |content: Value, details: Value, terminate: Option<bool>| AgentToolResult {
                content: serde_json::from_value(content).unwrap(),
                details,
                usage: None,
                terminate,
            };
            match self.kind {
                "echo" => {
                    on_update(result(text("working"), json!({ "step": 1 }), None));
                    let arg = args["text"].as_str().unwrap_or_default();
                    Ok(result(text(&format!("echo: {arg}")), json!({ "text": arg }), None))
                }
                "fail" => Err("tool failed".into()),
                _ => Ok(result(text("stopping"), json!({}), Some(true))),
            }
        })
    }
}

fn tool(name: &str) -> Arc<dyn AgentTool> {
    let (kind, description) = match name {
        "echo" => ("echo", "Echo the text back"),
        "fail" => ("fail", "Always throws"),
        "stop" => ("stop", "Ends the run"),
        other => panic!("unknown fixture tool {other}"),
    };
    Arc::new(Fixed {
        tool: serde_json::from_value(json!({
            "name": name,
            "description": description,
            "parameters": { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] },
        }))
        .unwrap(),
        kind,
    })
}

fn user(text: &str) -> Message {
    serde_json::from_value(json!({ "role": "user", "content": [{ "type": "text", "text": text }], "timestamp": 1 }))
        .unwrap()
}

fn mode(value: &Value) -> QueueMode {
    if value == "all" {
        QueueMode::All
    } else {
        QueueMode::OneAtATime
    }
}

/// The recorded JS predicates, by the event they fire on (each fires once).
fn triggers(action: &Value, event: &Value) -> bool {
    let on = action["on"].as_str().unwrap();
    let ty = event["type"].as_str().unwrap();
    if on.contains("text_delta") {
        ty == "message_update" && event["assistantMessageEvent"]["type"] == "text_delta"
    } else if on.contains("tool_execution_start") {
        ty == "tool_execution_start"
    } else if on.contains("tool_execution_end") {
        ty == "tool_execution_end"
    } else if on.contains("assistant") {
        ty == "message_end" && event["message"]["role"] == "assistant"
    } else {
        panic!("unknown trigger {on}")
    }
}

fn act(action: &Value, control: &AgentControl) {
    let texts = action["texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| user(t.as_str().unwrap()));
    match action["do"].as_str().unwrap() {
        "abort" => control.abort(),
        "abortFollowUp" => {
            texts.for_each(|m| control.follow_up(m));
            control.abort();
        }
        "steer" | "steers" => texts.for_each(|m| control.steer(m)),
        "followUps" => texts.for_each(|m| control.follow_up(m)),
        other => panic!("unknown action {other}"),
    }
}

async fn replay(scenario: &Value) {
    let name = scenario["name"].as_str().unwrap();
    let names = |key: &str| {
        scenario[key]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| tool(n.as_str().unwrap()))
            .collect::<Vec<_>>()
    };
    let calls = scenario["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| serde_json::from_value(call["events"].clone()).unwrap())
        .collect();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut options = AgentOptions::new(serde_json::from_value(scenario["model"].clone()).unwrap());
    options.system_prompt = scenario["systemPrompt"].as_str().map(String::from);
    options.tools = names("tools");
    options.steering_mode = mode(&scenario["steeringMode"]);
    options.follow_up_mode = mode(&scenario["followUpMode"]);
    if scenario["toolExecution"] == "sequential" {
        options.tool_execution = ToolExecutionMode::Sequential;
    }
    let mut agent = Agent::new(
        options,
        Box::new(Scripted {
            calls: Mutex::new(calls),
            contexts: contexts.clone(),
        }),
    );
    let added = names("addTools");
    if !added.is_empty() {
        agent.tools = added;
    }
    let events = Arc::new(Mutex::new(Vec::new()));
    let control = agent.control();
    let recorded = events.clone();
    let mut pending: Vec<Value> = scenario["actions"].as_array().unwrap().clone();
    agent.subscribe(move |event: &AgentEvent| {
        let value = serde_json::to_value(event).unwrap();
        if let Some(index) = pending.iter().position(|action| triggers(action, &value)) {
            act(&pending.remove(index), &control);
        }
        recorded.lock().unwrap().push(value);
    });

    agent
        .prompt_text(scenario["prompt"].as_str().unwrap(), Vec::new())
        .await;

    // JSON numbers compare as JS numbers do (0.0 is 0).
    let normalize = |value: Value| {
        let mut value = ai::json::normalize_numbers(value);
        zero_timestamps(&mut value);
        if let Value::Array(items) = &mut value {
            order_concurrent_completions(items);
        }
        value
    };
    let expected_events = normalize(scenario["events"].clone());
    let actual_events = normalize(Value::Array(events.lock().unwrap().clone()));
    let (expected, actual) = (expected_events.as_array().unwrap(), actual_events.as_array().unwrap());
    for (i, (e, a)) in expected.iter().zip(actual).enumerate() {
        assert_eq!(a, e, "{name}: event {i}");
    }
    assert_eq!(actual.len(), expected.len(), "{name}: event count");

    let expected_contexts = normalize(Value::Array(
        scenario["calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["context"].clone())
            .collect(),
    ));
    assert_eq!(
        normalize(Value::Array(contexts.lock().unwrap().clone())),
        expected_contexts,
        "{name}: provider transcripts"
    );

    let state = &scenario["state"];
    assert_eq!(
        normalize(serde_json::to_value(&agent.messages).unwrap()),
        normalize(state["messages"].clone()),
        "{name}: messages"
    );
    assert_eq!(
        agent.error_message.as_deref(),
        state["errorMessage"].as_str(),
        "{name}: errorMessage"
    );
    assert_eq!(
        agent.control().has_queued_messages(),
        state["queued"].as_bool().unwrap(),
        "{name}: queued"
    );
}

async fn replay_named(name: &str) {
    let fixture = fixture();
    let scenario = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .expect(name);
    replay(scenario).await;
}

#[tokio::test]
async fn agent_loop_text_turn_matches_typescript() {
    replay_named("text").await;
}

#[tokio::test]
async fn agent_loop_tool_round_trip_matches_typescript() {
    replay_named("tool_round_trip").await;
}

#[tokio::test]
async fn agent_loop_parallel_tools_with_error_and_unknown_tool_match_typescript() {
    replay_named("two_tools_parallel").await;
}

#[tokio::test]
async fn agent_loop_sequential_tools_match_typescript() {
    replay_named("two_tools_sequential").await;
}

#[tokio::test]
async fn agent_loop_abort_mid_stream_matches_typescript() {
    replay_named("abort_mid_stream").await;
}

#[tokio::test]
async fn agent_loop_steering_matches_typescript() {
    replay_named("steering").await;
}

#[tokio::test]
async fn agent_loop_steering_one_at_a_time_matches_typescript() {
    replay_named("steering_one_at_a_time").await;
}

#[tokio::test]
async fn agent_loop_follow_up_all_mode_matches_typescript() {
    replay_named("follow_up").await;
}

#[tokio::test]
async fn agent_loop_provider_error_matches_typescript() {
    replay_named("provider_error").await;
}

#[tokio::test]
async fn agent_loop_exhausted_provider_error_matches_typescript() {
    replay_named("no_response_queued").await;
}

#[tokio::test]
async fn agent_loop_length_stop_fails_tool_calls_like_typescript() {
    replay_named("length_stop").await;
}

#[tokio::test]
async fn agent_loop_terminating_tool_batch_matches_typescript() {
    replay_named("terminate").await;
}

#[tokio::test]
async fn agent_loop_mixed_terminate_batch_continues_like_typescript() {
    replay_named("terminate_mixed").await;
}

#[tokio::test]
async fn agent_loop_abort_ends_run_despite_queued_follow_up_like_typescript() {
    replay_named("abort_with_queued_follow_up").await;
}

#[tokio::test]
async fn agent_loop_abort_stops_sequential_tools_like_typescript() {
    replay_named("abort_during_sequential_tools").await;
}

#[tokio::test]
async fn agent_loop_declares_tool_changes_like_typescript() {
    replay_named("tools_added_later").await;
}

#[test]
fn agent_loop_fixture_covers_every_scenario() {
    let fixture = fixture();
    let names: Vec<&str> = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "text",
            "tool_round_trip",
            "two_tools_parallel",
            "two_tools_sequential",
            "abort_mid_stream",
            "steering",
            "follow_up",
            "steering_one_at_a_time",
            "provider_error",
            "no_response_queued",
            "length_stop",
            "terminate",
            "terminate_mixed",
            "abort_with_queued_follow_up",
            "abort_during_sequential_tools",
            "tools_added_later"
        ]
    );
}
