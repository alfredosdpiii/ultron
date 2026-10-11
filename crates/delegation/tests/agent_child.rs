//! Subagents as `crates/agent` loops with a scripted model: the child calls `rlm.finish` from its own lane through a
//! tool (standing in for its REPL), the root's `rlm.collect` waits host-side, the completion event arrives on the
//! channel without polling, and cancelling a child stops its loop.

use agent::{AgentOptions, AgentTool, AgentToolResult, StreamFn, ToolUpdate};
use ai::http::BoxFuture;
use ai::{AbortSignal, AssistantMessage, EventSink, Model, SimpleStreamOptions, TranscriptContext};
use delegation::child::agent_runner;
use delegation::host::{Delegation, DelegationOptions};
use delegation::task_store::MemoryStore;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn model() -> Model {
    serde_json::from_value(
        json!({ "id": "m", "name": "m", "api": "faux", "provider": "faux", "baseUrl": "",
        "reasoning": false, "input": ["text"], "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100 }),
    )
    .unwrap()
}

fn message(content: Value, stop: &str) -> AssistantMessage {
    serde_json::from_value(
        json!({ "content": content, "api": "faux", "provider": "faux", "model": "m",
        "usage": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0,
                   "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0 } },
        "stopReason": stop, "timestamp": 0 }),
    )
    .unwrap()
}

/// Serves scripted replies in order; `None` blocks until the run is aborted.
struct Scripted(Mutex<VecDeque<Option<AssistantMessage>>>);

impl StreamFn for Scripted {
    fn stream<'a>(
        &'a self,
        _model: &'a Model,
        _context: &'a TranscriptContext,
        options: &'a SimpleStreamOptions,
        _sink: &'a mut EventSink<'_>,
    ) -> BoxFuture<'a, AssistantMessage> {
        Box::pin(async move {
            let next = self.0.lock().unwrap().pop_front().flatten();
            match next {
                Some(message) => message,
                None => {
                    let signal = options.base.signal.clone().unwrap();
                    while !signal.aborted() {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                    let mut aborted = message(json!([]), "aborted");
                    aborted.error_message = Some("Request was aborted".into());
                    aborted
                }
            }
        })
    }
}

/// The child's host access: one tool forwarding `{kind, payload}` to its lane's handler.
struct HostTool {
    tool: ai::Tool,
    host: Delegation,
    lane: String,
}

impl AgentTool for HostTool {
    fn tool(&self) -> &ai::Tool {
        &self.tool
    }
    fn execute<'a>(
        &'a self,
        _id: &'a str,
        args: Value,
        _signal: &'a AbortSignal,
        _on_update: &'a ToolUpdate<'a>,
    ) -> BoxFuture<'a, Result<AgentToolResult, String>> {
        Box::pin(async move {
            let reply = self
                .host
                .handle(args["kind"].as_str().unwrap(), args["payload"].clone(), &self.lane)?;
            Ok(serde_json::from_value(
                json!({ "content": [{ "type": "text", "text": reply.to_string() }], "details": {} }),
            )
            .unwrap())
        })
    }
}

fn host_with(
    scripts: Vec<Vec<Option<AssistantMessage>>>,
) -> (Delegation, std::sync::mpsc::Receiver<delegation::host::DetachedEnd>) {
    let scripts = Arc::new(Mutex::new(VecDeque::from(scripts)));
    let runner = agent_runner(Arc::new(move |run| {
        let mut options = AgentOptions::new(model());
        options.tools = vec![Arc::new(HostTool {
            tool: serde_json::from_value(json!({ "name": "rlm", "description": "host", "parameters": {} })).unwrap(),
            host: run.host.clone(),
            lane: run.lane_name.clone(),
        })];
        let script = scripts.lock().unwrap().pop_front().unwrap();
        (
            options,
            Box::new(Scripted(Mutex::new(script.into()))) as Box<dyn StreamFn>,
        )
    }));
    Delegation::new(DelegationOptions {
        store: Box::new(MemoryStore::default()),
        collect: delegation::host::CollectMode::Wait,
        runner,
        spawn_depth: 3,
        child_model: None,
        workspace: None,
        session_id: "session".into(),
        worktree_setup: None,
        keep_worktrees: false,
        now: Box::new(|| 0.0),
    })
}

#[test]
fn delegation_agent_child_finishes_and_root_collects() {
    let finish = json!({ "kind": "rlm.finish", "payload": { "status": "passed", "summary": "added it",
        "evidence": ["cargo test: exit 0, 3 passed"] } });
    let (host, events) = host_with(vec![vec![
        Some(message(
            json!([{ "type": "toolCall", "id": "c1", "name": "rlm", "arguments": finish }]),
            "toolUse",
        )),
        Some(message(
            json!([{ "type": "text", "text": "first" }, { "type": "text", "text": "second" }]),
            "stop",
        )),
    ]]);
    let spawned = host
        .handle(
            "rlm.spawn",
            json!({ "prompt": "do it", "kwargs": { "name": "a" } }),
            "main",
        )
        .unwrap();
    let id = spawned["rlm_child_id"].as_str().unwrap().to_string();
    let collected = host.handle("rlm.collect", json!({}), "main").unwrap();
    assert_eq!(collected["results"][0]["id"], id.as_str());
    let result = &collected["results"][0]["result"];
    assert_eq!(result["value"], "first\nsecond");
    assert_eq!(result["verdict"]["summary"], "added it");
    assert_eq!(result["check"]["outcome"], "unchecked");
    let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(event.task["id"], id.as_str());
    assert_eq!(event.kind, "child_done");
    assert_eq!(event.owner_lane, "main");
    assert_eq!(event.fetch, format!("await rlm.collect([\"{id}\"])"));
}

#[test]
fn delegation_detached_child_end_arrives_as_event_without_a_wait() {
    let (host, events) = host_with(vec![vec![Some(message(
        json!([{ "type": "text", "text": "hi" }]),
        "stop",
    ))]]);
    host.handle("rlm.spawn", json!({ "prompt": "p", "kwargs": { "name": "a" } }), "main")
        .unwrap();
    let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(!event.awaited);
    assert_eq!(event.task["state"], "completed");
    assert_eq!(event.task["result"]["value"], "hi");
    assert_eq!(event.task["result"]["unverified"], true);
}

#[test]
fn delegation_deleting_a_child_aborts_its_agent_loop() {
    let (host, events) = host_with(vec![vec![None]]);
    let spawned = host
        .handle("rlm.spawn", json!({ "prompt": "p", "kwargs": { "name": "a" } }), "main")
        .unwrap();
    let id = spawned["rlm_child_id"].as_str().unwrap();
    // A bounded host-side wait returns nothing while the child still runs.
    assert!(host.wait(&[id.to_string()], Some(Duration::from_millis(20))).is_empty());
    assert_eq!(
        host.handle("rlm.delete_subagent", json!({ "selector": id }), "main")
            .unwrap(),
        json!({ "deleted": true })
    );
    let event = events.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        event.task["result"],
        json!({ "status": "cancelled", "error": "RLM child deleted", "verification": "unverified" })
    );
}

#[test]
fn delegation_failed_child_run_reports_the_model_error() {
    let mut failed = message(json!([]), "error");
    failed.error_message = Some("rate limited".into());
    let (host, _events) = host_with(vec![vec![Some(failed)]]);
    host.handle("rlm.spawn", json!({ "prompt": "p", "kwargs": { "name": "a" } }), "main")
        .unwrap();
    let collected = host.handle("rlm.collect", json!({}), "main").unwrap();
    assert_eq!(
        collected["results"][0]["result"],
        json!({ "status": "failed", "error": "rate limited", "verification": "unverified" })
    );
}
