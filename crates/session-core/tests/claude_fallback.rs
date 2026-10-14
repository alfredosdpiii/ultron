//! The session lane's provider routes `claude-subscription` calls through the same router as `ai::dispatch`
//! (`ai::claude_subscription::fallback`): an out-of-usage subscription continues on API credits, holds there until
//! the reset, and `claude.apiFallback: false` turns it off. A fake `claude` (the ai crate's
//! `fixtures/claude-subscription/claude.py`) and an in-memory Anthropic API; nothing reads the real environment,
//! ~/.ultron or ~/.claude, and nothing reaches the network.

#[path = "../../ai/tests/claude_subscription_support/isolate.rs"]
mod isolate;

use agent::StreamFn;
use ai::claude_subscription::Config;
use ai::claude_subscription::fallback::{ROUTE_DIAGNOSTIC, Router, RouterOptions};
use ai::http::{BoxFuture, HeaderMap};
use ai::types::{
    AssistantContent, AssistantMessageEvent, Message, SystemContent, SystemMessage, Tool, TranscriptContext,
    UserMessage, UserMessageContent,
};
use ai::{AssistantMessage, Body, HttpRequest, HttpResponse, SimpleStreamOptions, StopReason, Transport};
use isolate::seal;
use serde_json::{Value, json};
use session_core::lane::Provider;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const KEY: &str = "sk-ant-api03-lane-fixture";
const NOW: i64 = 1_790_000_000_000;

struct Chunks(std::vec::IntoIter<Vec<u8>>);

impl Body for Chunks {
    fn next_chunk(&mut self) -> BoxFuture<'_, Result<Option<Vec<u8>>, String>> {
        let next = self.0.next();
        Box::pin(async move { Ok(next) })
    }
}

/// The Messages API in memory: every request streams "from api".
#[derive(Clone, Default)]
struct Api {
    requests: Arc<Mutex<Vec<HttpRequest>>>,
}

impl Transport for Api {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.requests.lock().unwrap().push(request);
        let events = [
            json!({"type": "message_start", "message": {"id": "msg_api", "type": "message", "role": "assistant", "model": "claude-opus-5-5", "content": [], "stop_reason": null, "usage": {"input_tokens": 3, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "from api"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
            json!({"type": "message_stop"}),
        ];
        let chunks: Vec<Vec<u8>> = events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()).into_bytes())
            .collect();
        let mut headers = HeaderMap::new();
        headers.insert("content-type".into(), "text/event-stream".into());
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers,
                body: Box::new(Chunks(chunks.into_iter())),
            })
        })
    }
}

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

/// The fake `claude` answering with native's recorded usage-limit error; its runs are logged.
fn fake_claude(dir: &Path) -> (Config, PathBuf) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../ai/fixtures/claude-subscription/claude.py");
    // The interpreter runs the fixture: an executable written here could be exec'd while another test thread's
    // fork still holds its write descriptor (ETXTBSY under parallel load).
    std::fs::create_dir_all(dir.join("tmp")).unwrap();
    let log = dir.join("log.jsonl");
    let env = [
        ("PATH", "/usr/bin:/bin".to_string()),
        ("HOME", dir.display().to_string()),
        ("FAKE_LOG", log.display().to_string()),
        (
            "FAKE_NATIVE_ERROR",
            "rate_limit:You've hit your session limit · resets 3:45pm".into(),
        ),
    ];
    let config = Config {
        command: Some(vec![python(), "-I".into(), fixture.display().to_string()]),
        env: Some(env.into_iter().map(|(k, v)| (k.to_string(), v)).collect()),
        // Never started: the fake answers before any MCP handshake.
        inert_mcp: Some(vec!["/bin/false".into()]),
        timeout: Duration::from_secs(120),
        idle: Duration::from_secs(120),
        telemetry: None,
        temp_root: Some(dir.join("tmp")),
        upstream: "http://127.0.0.1:9".into(),
    };
    (config, log)
}

fn runs(log: &Path) -> usize {
    std::fs::read_to_string(log).unwrap_or_default().lines().count()
}

fn context() -> TranscriptContext {
    let tool = Tool {
        name: "rlm".into(),
        description: "Run Python in the persistent REPL.".into(),
        parameters: json!({"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"]}),
        constrained_sampling: None,
    };
    TranscriptContext {
        messages: vec![
            Message::System(SystemMessage {
                content: SystemContent::Text("You are Ultron.".into()),
                sections: None,
                tools_added: Some(vec![tool]),
                tools_removed: None,
                timestamp: 0,
            }),
            Message::User(UserMessage {
                content: UserMessageContent::Text("go".into()),
                timestamp: 0,
            }),
        ],
    }
}

fn text_of(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|c| match c {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

fn call(provider: &Provider) -> (AssistantMessage, usize) {
    let model = ai::claude_subscription::catalog::model("claude-opus-5-5").unwrap();
    let events = Mutex::new(0usize);
    let mut sink = |event: &AssistantMessageEvent| {
        if matches!(event, AssistantMessageEvent::Start { .. }) {
            *events.lock().unwrap() += 1;
        }
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let message = runtime.block_on(provider.stream(&model, &context(), &SimpleStreamOptions::default(), &mut sink));
    (message, events.into_inner().unwrap())
}

struct Lane {
    dir: tempfile::TempDir,
    log: PathBuf,
    api: Api,
    clock: Arc<AtomicI64>,
    provider: Provider,
}

fn lane() -> Lane {
    let dir = tempfile::tempdir().unwrap();
    let (config, log) = fake_claude(dir.path());
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let clock = Arc::new(AtomicI64::new(NOW));
    let router = Router::new(RouterOptions {
        clock: {
            let clock = clock.clone();
            Arc::new(move || clock.load(Ordering::SeqCst))
        },
        env: Arc::new(|name| (name == "ANTHROPIC_API_KEY").then(|| KEY.to_string())),
        agent_dir: Some(agent),
        subscription: Some(config),
        api_base_url: None,
    });
    let api = Api::default();
    let provider = Provider::with_router(Arc::new(router), Arc::new(api.clone()));
    Lane {
        dir,
        log,
        api,
        clock,
        provider,
    }
}

#[test]
fn session_core_lane_claude_subscription_falls_back_to_api_credits_and_holds() {
    seal();
    let lane = lane();
    let (first, starts) = call(&lane.provider);
    assert_eq!(first.stop_reason, StopReason::Stop, "{:?}", first.error_message);
    assert_eq!(text_of(&first), "from api");
    assert_eq!(first.provider, "anthropic");
    assert_eq!(starts, 1);
    let route = first
        .diagnostics
        .iter()
        .flatten()
        .find(|d| d.kind == ROUTE_DIAGNOSTIC)
        .and_then(|d| d.details.clone())
        .unwrap();
    assert_eq!(route["route"], "api");
    assert_eq!(route["until"], Value::from(NOW + 3_600_000));
    {
        let requests = lane.api.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "https://api.anthropic.com/v1/messages?beta=true");
        assert_eq!(
            requests[0]
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("x-api-key"))
                .map(|(_, v)| v.as_str()),
            Some(KEY)
        );
        let body: Value = serde_json::from_str(&requests[0].body).unwrap();
        assert_eq!(body["model"], "claude-opus-5-5");
        let loaded: Vec<&Value> = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["defer_loading"] != true)
            .collect();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0]["name"], "rlm");
    }
    assert_eq!(runs(&lane.log), 1);

    // Held: the next call skips the CLI.
    lane.clock.store(NOW + 60_000, Ordering::SeqCst);
    let (second, _) = call(&lane.provider);
    assert_eq!(text_of(&second), "from api");
    assert_eq!(runs(&lane.log), 1);
    assert_eq!(lane.api.requests.lock().unwrap().len(), 2);

    // After the hold the subscription is tried again (still out of usage here, so it falls back again).
    lane.clock.store(NOW + 3_600_000, Ordering::SeqCst);
    let (third, _) = call(&lane.provider);
    assert_eq!(text_of(&third), "from api");
    assert_eq!(runs(&lane.log), 2);
}

#[test]
fn session_core_lane_claude_subscription_fallback_setting_off() {
    seal();
    let lane = lane();
    std::fs::write(
        lane.dir.path().join("agent/settings.json"),
        json!({"claude": {"apiFallback": false}}).to_string(),
    )
    .unwrap();
    let (message, _) = call(&lane.provider);
    assert_eq!(message.stop_reason, StopReason::Error);
    assert_eq!(
        message.error_message.as_deref(),
        Some("Native API error: You've hit your session limit · resets 3:45pm")
    );
    assert!(lane.api.requests.lock().unwrap().is_empty());
    assert_eq!(runs(&lane.log), 1);
}
