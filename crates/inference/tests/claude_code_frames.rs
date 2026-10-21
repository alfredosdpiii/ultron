//! A frame with `budget.depth > 1` on a `claude-code/<id>` model: its conversation declares the `rlm` tool and goes
//! straight through `ai::dispatch` (no session lane), which runs it on the Claude subscription instead of failing
//! with "does not support tool calling here". The subscription is the ai crate's fake `claude`
//! (`fixtures/claude-subscription/claude`, named by ULTRON_CLAUDE_CODE_BIN in the frame's provider env), answering
//! with the plan's usage limit; an API key in that env is never used. The task host is a fake that runs each
//! `rlm-frame@1` task on its own thread. Sealed environment; nothing reaches the network.

#[path = "../../ai/tests/claude_subscription_support/isolate.rs"]
mod isolate;

use agent::{AgentTool, AgentToolResult, ToolUpdate};
use ai::http::BoxFuture;
use ai::{AbortSignal, HttpRequest, HttpResponse, Transport};
use inference::{FrameExecution, FrameSession, FrameTasks, Inference, MemoryTraceStore};
use kernel::{EchoHandler, HostHandler, Signal};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const HIT_LIMIT: &str = "rate_limit:You've hit your session limit · resets 3:45pm";

/// Never called: the fake `claude` answers before any cell.
struct Rlm(ai::Tool);

impl AgentTool for Rlm {
    fn tool(&self) -> &ai::Tool {
        &self.0
    }
    fn execute<'a>(
        &'a self,
        _: &'a str,
        _: Value,
        _: &'a AbortSignal,
        _: &'a ToolUpdate<'a>,
    ) -> BoxFuture<'a, Result<AgentToolResult, String>> {
        Box::pin(async { Err("no cells in this test".to_string()) })
    }
}

/// Runs each frame task on its own thread through the session's frames, as the typed-agent host does.
#[derive(Default)]
struct Tasks {
    session: OnceLock<Arc<FrameSession>>,
    results: Mutex<HashMap<String, Receiver<Result<Value, String>>>>,
    spawned: Mutex<u64>,
}

impl FrameTasks for Tasks {
    fn caller_task(&self, _: &str) -> Option<String> {
        None
    }
    fn spawn(&self, _: &str, input: Value, _: Option<&str>, timeout_ms: u64) -> Result<String, String> {
        let id = {
            let mut spawned = self.spawned.lock().unwrap();
            *spawned += 1;
            format!("task-{spawned}")
        };
        let session = self.session.get().unwrap().clone();
        let (send, receive) = channel();
        let run = FrameExecution {
            task_id: id.clone(),
            input,
            lane: format!("frame-{id}"),
            usage_root: None,
            deadline_at: None,
            timeout_ms,
            signal: Signal::new(),
        };
        std::thread::spawn(move || {
            let _ = send.send(session.execute(run));
        });
        self.results.lock().unwrap().insert(id.clone(), receive);
        Ok(id)
    }
    fn result(&self, _: &str, task_id: &str, _: &Signal) -> Value {
        let receive = self.results.lock().unwrap().remove(task_id).unwrap();
        match receive
            .recv_timeout(Duration::from_secs(120))
            .expect("the frame task ends")
        {
            Ok(value) => json!({"status": "succeeded", "value": value}),
            Err(error) => json!({"status": "failed", "error": error}),
        }
    }
    fn reserve(&self, _: &Value) -> Result<Option<Value>, String> {
        Ok(None)
    }
    fn settle(&self, _: &Value, _: &str, _: Option<&Value>) {}
    fn code_tool(&self, _: &str) -> Result<Arc<dyn AgentTool>, String> {
        Ok(Arc::new(Rlm(ai::Tool {
            name: "rlm".into(),
            description: "Run Python in the persistent REPL.".into(),
            parameters: json!({"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"]}),
            constrained_sampling: None,
        })))
    }
    fn close_lane(&self, _: &str) {}
}

/// Records every HTTP request and answers none.
#[derive(Default)]
struct Recording(Mutex<Vec<String>>);

impl Transport for Recording {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.0.lock().unwrap().push(request.url);
        Box::pin(async { Err("no network in tests".to_string()) })
    }
}

fn claude_code(id: &str) -> ai::Model {
    let mut model = ai::claude_subscription::catalog::model(id).unwrap();
    model.api = "claude-code-cli".into();
    model.provider = "claude-code".into();
    model
}

#[test]
fn inference_depth_frame_on_claude_code_runs_on_the_subscription_without_api_credits() {
    isolate::seal();
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("claude.jsonl");
    let wrapper = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ai/fixtures/claude-subscription/claude");
    let env: ai::env::ProviderEnv = [
        ("ULTRON_CLAUDE_CODE_BIN", wrapper.display().to_string()),
        ("FAKE_LOG", log.display().to_string()),
        ("FAKE_NATIVE_ERROR", HIT_LIMIT.to_string()),
        ("ANTHROPIC_API_KEY", ["sk-ant-api03", "frame", "fixture"].join("-")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect();
    let transport = Arc::new(Recording::default());
    let session = FrameSession::new(Arc::new(MemoryTraceStore::default()));
    let tasks = Arc::new(Tasks::default());
    let _ = tasks.session.set(session.clone());
    session.set_tasks(tasks.clone());
    let handler = Inference::new(
        Arc::new(EchoHandler),
        claude_code("claude-haiku-5-5"),
        None,
        transport.clone(),
        dir.path().join("context"),
    )
    .with_session(session, "main")
    .with_provider_env(env);
    let Value::Object(payload) = json!({"task": "count", "budget": {"depth": 2, "calls": 2}}) else {
        unreachable!()
    };
    let observed = handler.handle("rlm.infer", payload, &Signal::new());
    let text = format!("{observed:?}");
    assert!(text.contains("You've hit your session limit"), "{text}");
    assert!(!text.contains("does not support tool calling"), "{text}");
    // One subscription model call; no request on API credits.
    let records: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .filter(|r: &Value| r["argv"].as_array().is_some_and(|a| a.iter().any(|v| v == "--model")))
        .collect();
    assert_eq!(records.len(), 1, "{records:?}");
    let argv = records[0]["argv"].as_array().unwrap();
    let at = argv.iter().position(|v| v == "--model").unwrap();
    assert_eq!(
        argv[at + 1],
        json!(ai::claude_subscription::catalog::native_model("claude-haiku-5-5").unwrap())
    );
    assert!(transport.0.lock().unwrap().is_empty());
}
