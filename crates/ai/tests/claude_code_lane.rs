//! `claude-code/<id>` requests with tools run on the Claude subscription (`claude_subscription::claude_code_lane`):
//! through `dispatch` (every caller: session lanes, inference frames with depth) and through a router of its own.
//! They never fall back to API credits, a backend override refuses them with the `ultron claude` hint, and
//! tool-free requests stay on `claude -p`. A fake `claude` (`fixtures/claude-subscription/claude.py`, and the
//! `claude` wrapper beside it for ULTRON_CLAUDE_CODE_BIN) in the sealed environment; nothing reaches the network.

mod claude_subscription_support;

use ai::claude_code_cli::TOOLS_UNSUPPORTED;
use ai::claude_subscription::claude_code_lane::{Route, backend_error, route};
use ai::claude_subscription::fallback::{KEY_HINT, Router, RouterOptions};
use ai::claude_subscription::{Env, catalog, process};
use ai::http::BoxFuture;
use ai::types::{AssistantContent, AssistantMessageEvent, TranscriptContext};
use ai::{AssistantMessage, HttpRequest, HttpResponse, Model, ReqwestTransport, SimpleStreamOptions, StopReason};
use claude_subscription_support::*;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const NO_UPSTREAM: &str = "http://127.0.0.1:9";
const HIT_LIMIT: &str = "rate_limit:You've hit your session limit · resets 3:45pm";
/// Built from parts so the secret scan stays clean; obviously fake.
fn key() -> String {
    ["sk-ant-api03", "claude-code-lane", "fixture"].join("-")
}

/// `claude-code/<id>` as the registry lists it.
fn claude_code(id: &str) -> Model {
    let mut model = catalog::model(id).unwrap_or_else(|| model("claude-haiku-5-5"));
    model.id = id.into();
    model.api = "claude-code-cli".into();
    model.provider = "claude-code".into();
    model
}

fn with_tools() -> TranscriptContext {
    context(vec![system("You are Ultron.", vec![rlm_tool()]), user("go")])
}

fn tool_free() -> TranscriptContext {
    context(vec![system("You are a frame.", vec![]), user("go")])
}

fn wrapper() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/claude-subscription/claude")
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

fn kinds(events: &[AssistantMessageEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|e| match e {
            AssistantMessageEvent::Start { .. } => "start",
            AssistantMessageEvent::Done { .. } => "done",
            AssistantMessageEvent::Error { .. } => "error",
            _ => "other",
        })
        .collect()
}

/// The `--model` of every model call the fake ran (not its `--version` / `auth status` probes).
fn model_calls(fake: &Fake) -> Vec<String> {
    fake.records()
        .iter()
        .filter_map(|r| {
            let argv: Vec<&str> = r["argv"].as_array()?.iter().filter_map(Value::as_str).collect();
            let at = argv.iter().position(|a| *a == "--model")?;
            argv.get(at + 1).map(|m| m.to_string())
        })
        .collect()
}

/// A router over the fake `claude` (`vars`) whose API fallback would go to `api` with a configured key.
fn router(fake: &Fake, vars: &[(&str, &str)], api: &Upstream, agent: &tempfile::TempDir) -> Router {
    let env: HashMap<String, String> = [("ANTHROPIC_API_KEY".to_string(), key())].into();
    Router::new(RouterOptions {
        env: Arc::new(move |name| env.get(name).cloned()),
        agent_dir: Some(agent.path().to_path_buf()),
        subscription: Some(fake.config(NO_UPSTREAM, vars)),
        api_base_url: Some(api.url()),
        ..RouterOptions::default()
    })
}

fn api() -> Upstream {
    Upstream::start(|_, stream| reply_status(stream, 500, "{}"))
}

fn on_router(
    router: &Router,
    model: &Model,
    context: &TranscriptContext,
) -> Option<(AssistantMessage, Vec<AssistantMessageEvent>)> {
    let events = Mutex::new(Vec::new());
    let mut sink = |event: &AssistantMessageEvent| events.lock().unwrap().push(event.clone());
    let options = SimpleStreamOptions::default();
    let message = runtime().block_on(router.stream_claude_code(model, context, &options, &mut sink))?;
    Some((message, events.into_inner().unwrap()))
}

/// Records every HTTP request and answers none (no network in tests).
#[derive(Default)]
struct Recording(Mutex<Vec<String>>);

impl ai::Transport for Recording {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, String>> {
        self.0.lock().unwrap().push(request.url.clone());
        Box::pin(async { Err("no network in tests".to_string()) })
    }
}

fn dispatch(
    model: &Model,
    context: &TranscriptContext,
    env: &[(&str, String)],
    transport: &Recording,
) -> (AssistantMessage, Vec<AssistantMessageEvent>) {
    let mut options = SimpleStreamOptions::default();
    options.base.env = Some(env.iter().map(|(k, v)| (k.to_string(), v.clone())).collect());
    let events = Mutex::new(Vec::new());
    let mut sink = |event: &AssistantMessageEvent| events.lock().unwrap().push(event.clone());
    let message = runtime()
        .block_on(ai::dispatch::stream_simple(
            model, context, &options, transport, &mut sink,
        ))
        .unwrap();
    (message, events.into_inner().unwrap())
}

#[test]
fn ai_claude_code_lane_with_tools_runs_on_the_subscription() {
    seal();
    let (fake, api, agent) = (Fake::new(), api(), tempfile::tempdir().unwrap());
    let router = router(
        &fake,
        &[("FAKE_OFFLINE_BLOCKS", r#"[{"type":"text","text":"from the plan"}]"#)],
        &api,
        &agent,
    );
    let (message, events) = on_router(&router, &claude_code("claude-haiku-5-5"), &with_tools()).expect("rerouted");
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    assert_eq!(text_of(&message), "from the plan");
    assert_eq!(
        (message.provider.as_str(), message.model.as_str()),
        ("claude-subscription", "claude-haiku-5-5")
    );
    assert_eq!(kinds(&events).first(), Some(&"start"));
    assert_eq!(kinds(&events).last(), Some(&"done"));
    assert_eq!(model_calls(&fake), [catalog::native_model("claude-haiku-5-5").unwrap()]);
    // The lane's `rlm` tool reached the model through the inert MCP inventory.
    let tools = fake.last()["mcp"].to_string();
    assert!(tools.contains("\"rlm\""), "{tools}");
    assert!(api.requests().is_empty());
}

#[test]
fn ai_claude_code_lane_out_of_usage_never_spends_api_credits() {
    seal();
    let (fake, api, agent) = (Fake::new(), api(), tempfile::tempdir().unwrap());
    let router = router(&fake, &[("FAKE_NATIVE_ERROR", HIT_LIMIT)], &api, &agent);
    let (message, events) = on_router(&router, &claude_code("claude-haiku-5-5"), &with_tools()).expect("rerouted");
    assert_eq!(message.stop_reason, StopReason::Error);
    let error = message.error_message.clone().unwrap_or_default();
    assert!(error.contains("You've hit your session limit"), "{error}");
    assert!(!error.contains(KEY_HINT), "{error}");
    assert_eq!(kinds(&events), ["start", "error"]);
    assert_eq!(
        router.held_until(),
        None,
        "a claude-code call never switches the router to the API"
    );
    assert!(api.requests().is_empty(), "no request on API credits");
    assert_eq!(model_calls(&fake).len(), 1);

    // An explicit claude-subscription model on the same router keeps its fallback.
    let mut sink = |_: &AssistantMessageEvent| {};
    let transport = ReqwestTransport::with_client(reqwest::Client::builder().no_proxy().build().unwrap(), None);
    let explicit = runtime().block_on(router.stream(
        &model("claude-haiku-5-5"),
        &with_tools(),
        &SimpleStreamOptions::default(),
        &transport,
        &mut sink,
    ));
    assert_eq!(api.requests().len(), 1, "{:?}", explicit.error_message);
    assert!(router.held_until().is_some());
}

#[test]
fn ai_claude_code_lane_backend_override_is_refused_with_the_root_hint() {
    seal();
    let (fake, api, agent) = (Fake::new(), api(), tempfile::tempdir().unwrap());
    let model = claude_code("claude-opus-5-5");
    let bedrock = router(&fake, &[("CLAUDE_CODE_USE_BEDROCK", "1")], &api, &agent);
    let (message, events) = on_router(&bedrock, &model, &with_tools()).expect("refused");
    assert_eq!(
        message.error_message.as_deref(),
        Some(
            "claude-code/claude-opus-5-5 cannot run a lane with tools while CLAUDE_CODE_USE_BEDROCK is set: tool lanes on Claude Code run on the Claude subscription (claude-subscription), which uses only the Claude Code login, not that backend. With this backend, run Claude Code as the root agent with `ultron claude`, or use a model that is not claude-code for this lane."
        )
    );
    assert_eq!(
        (message.provider.as_str(), message.stop_reason),
        ("claude-code", StopReason::Error)
    );
    assert_eq!(kinds(&events), ["start", "error"]);
    assert!(fake.records().is_empty(), "nothing spawned");

    let both = router(
        &fake,
        &[
            ("ANTHROPIC_BASE_URL", "http://gateway.invalid"),
            ("CLAUDE_CODE_USE_VERTEX", "true"),
        ],
        &api,
        &agent,
    );
    let (message, _) = on_router(&both, &model, &with_tools()).expect("refused");
    assert_eq!(
        message.error_message,
        Some(backend_error(&model, &["ANTHROPIC_BASE_URL", "CLAUDE_CODE_USE_VERTEX"]))
    );
    assert!(
        message
            .error_message
            .unwrap()
            .contains("ANTHROPIC_BASE_URL, CLAUDE_CODE_USE_VERTEX are set")
    );
    // A flag turned off is no override.
    let off = router(
        &fake,
        &[("CLAUDE_CODE_USE_FOUNDRY", "0"), ("FAKE_NATIVE_ERROR", HIT_LIMIT)],
        &api,
        &agent,
    );
    let (message, _) = on_router(&off, &model, &with_tools()).expect("rerouted");
    assert_eq!(message.provider, "claude-subscription");
}

#[test]
fn ai_claude_code_lane_routes_only_tool_requests_the_subscription_serves() {
    seal();
    let none = Env::new;
    let haiku = claude_code("claude-haiku-5-5");
    assert_eq!(
        route(&haiku, &tool_free(), none),
        None,
        "tool-free requests stay on claude -p"
    );
    assert_eq!(
        route(&model("claude-haiku-5-5"), &with_tools(), none),
        None,
        "another API"
    );
    assert_eq!(
        route(&haiku, &with_tools(), none),
        Some(Route::Subscription(Box::new(model("claude-haiku-5-5"))))
    );
    assert_eq!(
        route(&claude_code("haiku"), &with_tools(), none),
        Some(Route::Subscription(Box::new(model("haiku"))))
    );
    assert_eq!(route(&claude_code("Not A Model"), &with_tools(), none), None);
    // The environment is read only for a request that would reroute.
    let read = Mutex::new(false);
    route(&haiku, &tool_free(), || {
        *read.lock().unwrap() = true;
        Env::new()
    });
    assert!(!*read.lock().unwrap());

    // Through dispatch an id the subscription does not serve keeps claude -p's tools error, in this build's text.
    let transport = Recording::default();
    let (message, _) = dispatch(&claude_code("Not A Model"), &with_tools(), &[], &transport);
    assert_eq!(
        message.error_message,
        Some(format!(
            "claude-code/Not A Model {TOOLS_UNSUPPORTED} (this lane declares tools: rlm)."
        ))
    );
    assert!(!TOOLS_UNSUPPORTED.contains("frames"), "no longer recommends frames");
}

/// Every caller of `dispatch` (inference frames with depth call it directly) gets the reroute; the subscription
/// finds Claude Code by ULTRON_CLAUDE_CODE_BIN, as the claude-code provider does, past the sealed PATH's sentinel;
/// and with an API key in the request's environment the out-of-usage call still spends nothing.
#[test]
fn ai_claude_code_lane_dispatch_reroutes_every_caller() {
    seal();
    let fake = Fake::new();
    let env = [
        ("ULTRON_CLAUDE_CODE_BIN", wrapper().display().to_string()),
        ("FAKE_LOG", fake.log.display().to_string()),
        ("FAKE_NATIVE_ERROR", HIT_LIMIT.to_string()),
        ("ANTHROPIC_API_KEY", key()),
        // The subscription's own setting names another binary: the reroute runs the claude-code provider's CLI.
        ("ULTRON_CLAUDE_COMMAND", "/bin/false".to_string()),
    ];
    let transport = Recording::default();
    let (message, events) = dispatch(&claude_code("claude-haiku-5-5"), &with_tools(), &env, &transport);
    let error = message.error_message.clone().unwrap_or_default();
    assert!(error.contains("You've hit your session limit"), "{error}");
    assert_eq!(message.provider, "claude-subscription");
    assert_eq!(kinds(&events), ["start", "error"]);
    assert_eq!(model_calls(&fake), [catalog::native_model("claude-haiku-5-5").unwrap()]);
    assert!(transport.0.lock().unwrap().is_empty(), "no API request");
}

#[test]
fn ai_claude_subscription_resolve_claude_falls_back_to_claude_code_bin() {
    seal();
    let bin = wrapper().display().to_string();
    let env = |pairs: &[(&str, &str)]| -> Env {
        let mut env: Env = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        env.insert("PATH".into(), "/nonexistent".into());
        env
    };
    assert_eq!(
        process::resolve_claude(None, &env(&[("ULTRON_CLAUDE_CODE_BIN", &format!(" {bin} "))])),
        Some(vec![bin.clone()])
    );
    let python = python();
    assert_eq!(
        process::resolve_claude(
            None,
            &env(&[("ULTRON_CLAUDE_CODE_BIN", &bin), ("ULTRON_CLAUDE_COMMAND", &python)])
        ),
        Some(vec![python.clone()]),
        "ULTRON_CLAUDE_COMMAND wins"
    );
}

/// The reroute runs the `claude` the claude-code provider would (ULTRON_CLAUDE_CODE_BIN, else PATH), never one the
/// subscription would probe elsewhere: without one it fails with the provider's own message, spawning nothing.
#[test]
fn ai_claude_code_lane_dispatch_runs_only_the_claude_code_providers_cli() {
    seal();
    let empty = tempfile::tempdir().unwrap();
    let env = [("PATH", empty.path().display().to_string())];
    let transport = Recording::default();
    let (message, events) = dispatch(&claude_code("claude-haiku-5-5"), &with_tools(), &env, &transport);
    assert_eq!(
        message.error_message.as_deref(),
        Some(
            "claude-code provider: the Claude Code CLI (`claude`) was not found on PATH. Install Claude Code and run `claude auth login`, or set ULTRON_CLAUDE_CODE_BIN."
        )
    );
    assert_eq!(message.provider, "claude-code");
    assert_eq!(kinds(&events), ["start", "error"]);
}
