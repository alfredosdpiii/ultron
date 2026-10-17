//! Replays `fixtures/validation.json` (recorded from the TypeScript `validateToolArguments` and `runAgentLoop` by
//! `fixtures/validation.mjs`): the validated arguments or error text for a matrix of arguments over TypeBox tool
//! schemas, and agent-loop runs whose tool calls have bad arguments, a `prepareArguments` tool, and
//! `beforeToolCall` / `afterToolCall` hooks. The Rust loop must emit the same events and call the hooks the same way.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use agent::{
    AfterToolCall, AfterToolCallResult, AgentContext, AgentEvent, AgentLoopConfig, AgentTool, AgentToolResult,
    BeforeToolCall, BeforeToolCallResult, StreamFn, ToolExecutionMode, ToolUpdate, agent_loop,
};
use ai::http::BoxFuture;
use ai::{AbortSignal, AssistantMessage, AssistantMessageEvent, EventSink, Message, Model, SimpleStreamOptions};
use ai::{TranscriptContext, UserContent};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/validation.json")).unwrap()
}

fn normalize(value: Value) -> Value {
    fn zero(value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, v) in map.iter_mut() {
                    if key == "timestamp" {
                        *v = json!(0);
                    } else {
                        zero(v);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(zero),
            _ => {}
        }
    }
    let mut value = ai::json::normalize_numbers(value);
    zero(&mut value);
    if let Value::Array(events) = &mut value {
        order_concurrent_completions(events);
    }
    value
}

/// Concurrent tools finish in completion order, which differs between JS microtasks and Rust polling: sort each run
/// of consecutive update/end events by tool call id (stable), as `tests/loop.rs` does.
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

#[test]
fn agent_validate_tool_arguments_matches_typescript() {
    let fixture = fixture();
    let schemas = &fixture["schemas"];
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 53);
    for case in cases {
        let name = case["schema"].as_str().unwrap();
        let actual = mcp::validation::validate_tool_arguments(name, &schemas[name], &case["args"]);
        let label = format!("{name} {}", case["args"]);
        match (&actual, case.get("ok")) {
            (Ok(args), Some(expected)) => {
                assert_eq!(normalize(args.clone()), normalize(expected.clone()), "{label}")
            }
            (Err(error), None) => assert_eq!(error, case["error"].as_str().unwrap(), "{label}"),
            _ => panic!("{label}: got {actual:?}, TypeScript {case}"),
        }
    }
}

struct Scripted(Mutex<VecDeque<AssistantMessage>>);

impl StreamFn for Scripted {
    fn stream<'a>(
        &'a self,
        _model: &'a Model,
        _context: &'a TranscriptContext,
        _options: &'a SimpleStreamOptions,
        sink: &'a mut EventSink<'_>,
    ) -> BoxFuture<'a, AssistantMessage> {
        Box::pin(async move {
            let message = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("more provider calls than recorded");
            sink(&AssistantMessageEvent::Done {
                reason: message.stop_reason,
                message: message.clone(),
            });
            message
        })
    }
}

struct FixtureTool {
    tool: ai::Tool,
}

fn text(t: &str) -> Vec<UserContent> {
    serde_json::from_value(json!([{ "type": "text", "text": t }])).unwrap()
}

fn result(content: Vec<UserContent>, details: Value) -> AgentToolResult {
    AgentToolResult {
        content,
        details,
        usage: None,
        terminate: None,
    }
}

impl AgentTool for FixtureTool {
    fn tool(&self) -> &ai::Tool {
        &self.tool
    }
    fn prepare_arguments(&self, args: &Value) -> Result<Value, String> {
        if self.tool.name != "legacy" {
            return Ok(args.clone());
        }
        if args.get("boom").is_some_and(|b| b != false && !b.is_null()) {
            return Err("cannot prepare".into());
        }
        Ok(match args.get("old") {
            Some(old) => json!({ "text": old }),
            None => args.clone(),
        })
    }
    fn execute<'a>(
        &'a self,
        _id: &'a str,
        args: Value,
        _signal: &'a AbortSignal,
        on_update: &'a ToolUpdate<'a>,
    ) -> BoxFuture<'a, Result<AgentToolResult, String>> {
        Box::pin(async move {
            if self.tool.name == "legacy" {
                return Ok(result(text(&format!("legacy: {args}")), json!({})));
            }
            on_update(result(text("working"), json!({ "step": 1 })));
            let arg = args["text"].as_str().unwrap_or_default();
            Ok(result(text(&format!("echo: {arg}")), json!({ "text": arg })))
        })
    }
}

fn tool(name: &str, schema: &Value) -> Arc<dyn AgentTool> {
    let description = match name {
        "echo" => "Echo the text back",
        "legacy" => "Accepts the old argument name",
        other => panic!("unknown fixture tool {other}"),
    };
    Arc::new(FixtureTool {
        tool: serde_json::from_value(json!({
            "name": name,
            "description": description,
            "parameters": schema,
        }))
        .unwrap(),
    })
}

type Calls = Arc<Mutex<Vec<Value>>>;

/// The fixture's `policy` hooks.
fn policy(calls: &Calls) -> (BeforeToolCall, AfterToolCall) {
    let before_calls = calls.clone();
    let before: BeforeToolCall = Arc::new(move |ctx, _signal| {
        before_calls.lock().unwrap().push(json!({
            "hook": "before", "id": ctx.tool_call.id, "toolCallArgs": ctx.tool_call.arguments, "args": ctx.args,
        }));
        let decision = match ctx.args["text"].as_str() {
            Some("block") => Ok(Some(BeforeToolCallResult {
                block: true,
                reason: Some("blocked by policy".into()),
                terminate: None,
            })),
            Some("quiet") => Ok(Some(BeforeToolCallResult {
                block: true,
                ..Default::default()
            })),
            Some("halt") => Ok(Some(BeforeToolCallResult {
                block: true,
                reason: Some("halted".into()),
                terminate: Some(true),
            })),
            Some("explode") => Err("hook exploded".to_string()),
            Some("pass") => Ok(Some(BeforeToolCallResult::default())),
            _ => Ok(None),
        };
        Box::pin(async move { decision })
    });
    let after_calls = calls.clone();
    let after: AfterToolCall = Arc::new(move |ctx, _signal| {
        after_calls.lock().unwrap().push(json!({
            "hook": "after", "id": ctx.tool_call.id, "args": ctx.args, "result": ctx.result, "isError": ctx.is_error,
        }));
        let replacement = match ctx.args["text"].as_str() {
            Some("swap") => Ok(Some(AfterToolCallResult {
                content: Some(text("replaced")),
                is_error: Some(true),
                details: Some(json!({ "swapped": true })),
                ..Default::default()
            })),
            Some("stop") => Ok(Some(AfterToolCallResult {
                terminate: Some(true),
                ..Default::default()
            })),
            Some("boom") => Err("after exploded".to_string()),
            _ => Ok(None),
        };
        Box::pin(async move { replacement })
    });
    (before, after)
}

async fn replay(name: &str) {
    let fixture = fixture();
    let scenario = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .expect(name);
    let responses: VecDeque<AssistantMessage> = serde_json::from_value(scenario["responses"].clone()).unwrap();
    let stream = Scripted(Mutex::new(responses));
    let mut config = AgentLoopConfig::new(serde_json::from_value(fixture["model"].clone()).unwrap());
    if scenario["toolExecution"] == "sequential" {
        config.tool_execution = ToolExecutionMode::Sequential;
    }
    let calls: Calls = Arc::default();
    match scenario["hooks"].as_str() {
        Some("policy") => {
            let (before, after) = policy(&calls);
            config.before_tool_call = Some(before);
            config.after_tool_call = Some(after);
        }
        Some("abortBlock") => {
            let before_calls = calls.clone();
            config.before_tool_call = Some(Arc::new(move |ctx, signal| {
                before_calls.lock().unwrap().push(json!({
                    "hook": "before", "id": ctx.tool_call.id, "toolCallArgs": ctx.tool_call.arguments, "args": ctx.args,
                }));
                signal.abort();
                Box::pin(async {
                    Ok(Some(BeforeToolCallResult {
                        block: true,
                        reason: Some("blocked after the abort".into()),
                        terminate: None,
                    }))
                })
            }));
        }
        Some("abort") => {
            let before_calls = calls.clone();
            config.before_tool_call = Some(Arc::new(move |ctx, signal| {
                before_calls.lock().unwrap().push(json!({
                    "hook": "before", "id": ctx.tool_call.id, "toolCallArgs": ctx.tool_call.arguments, "args": ctx.args,
                }));
                signal.abort();
                Box::pin(async { Ok(None) })
            }));
        }
        _ => {}
    }
    let context = AgentContext {
        messages: Vec::new(),
        tools: scenario["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| tool(n.as_str().unwrap(), &fixture["toolSchema"]))
            .collect(),
    };
    let prompt: Message = serde_json::from_value(
        json!({ "role": "user", "content": [{ "type": "text", "text": "go" }], "timestamp": 1 }),
    )
    .unwrap();
    let mut events = Vec::new();
    let mut emit = |event: AgentEvent| events.push(serde_json::to_value(&event).unwrap());
    let signal = AbortSignal::new();
    let messages = agent_loop(vec![prompt], context, config, &mut emit, &signal, &stream).await;

    let expected = normalize(scenario["events"].clone());
    let actual = normalize(Value::Array(events));
    let (expected, actual) = (expected.as_array().unwrap(), actual.as_array().unwrap());
    for (i, (e, a)) in expected.iter().zip(actual).enumerate() {
        assert_eq!(a, e, "{name}: event {i}");
    }
    assert_eq!(actual.len(), expected.len(), "{name}: event count");
    assert_eq!(
        normalize(Value::Array(calls.lock().unwrap().clone())),
        normalize(scenario["hookCalls"].clone()),
        "{name}: hook calls"
    );
    assert_eq!(
        normalize(serde_json::to_value(&messages).unwrap()),
        normalize(scenario["messages"].clone()),
        "{name}: messages"
    );
    let used = scenario["responses"].as_array().unwrap().len() - stream.0.lock().unwrap().len();
    assert_eq!(
        used as u64,
        scenario["calls"].as_u64().unwrap(),
        "{name}: provider calls"
    );
}

#[tokio::test]
async fn agent_loop_invalid_arguments_fail_like_typescript_in_parallel() {
    replay("invalid_parallel").await;
}

#[tokio::test]
async fn agent_loop_invalid_arguments_fail_like_typescript_in_sequence() {
    replay("invalid_sequential").await;
}

#[tokio::test]
async fn agent_loop_prepare_arguments_runs_before_validation_like_typescript() {
    replay("prepare_arguments").await;
}

#[tokio::test]
async fn agent_loop_before_tool_call_blocks_and_throws_like_typescript() {
    replay("before_hooks").await;
}

#[tokio::test]
async fn agent_loop_before_tool_call_in_sequence_like_typescript() {
    replay("before_hooks_sequential").await;
}

#[tokio::test]
async fn agent_loop_before_tool_call_terminate_ends_the_run_like_typescript() {
    replay("before_terminate").await;
}

#[tokio::test]
async fn agent_loop_after_tool_call_replaces_and_throws_like_typescript() {
    replay("after_hooks").await;
}

#[tokio::test]
async fn agent_loop_after_tool_call_terminate_ends_the_run_like_typescript() {
    replay("after_terminate").await;
}

#[tokio::test]
async fn agent_loop_abort_during_a_blocking_before_tool_call_reports_the_abort_like_typescript() {
    replay("abort_and_block_in_before").await;
}

#[tokio::test]
async fn agent_loop_abort_during_before_tool_call_like_typescript() {
    replay("abort_in_before").await;
}

#[test]
fn agent_validation_fixture_covers_every_scenario() {
    let names: Vec<String> = fixture()["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        [
            "invalid_parallel",
            "invalid_sequential",
            "prepare_arguments",
            "before_hooks",
            "before_hooks_sequential",
            "before_terminate",
            "after_hooks",
            "after_terminate",
            "abort_and_block_in_before",
            "abort_in_before"
        ]
    );
}
