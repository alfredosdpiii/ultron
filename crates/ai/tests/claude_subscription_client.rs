//! One model call through the fake `claude`: ports of the Hermes plugin's `tests/test_directsdk.py`,
//! `test_directsdk_errors.py`, `test_directsdk_replay.py` (process cases), `test_directsdk_telemetry.py` and
//! `test_directsdk_workspace.py` on Ultron's message model, plus the one-tool inventory (MCP `tools/list` and the
//! upstream request's `tools` are exactly `[rlm]`; any other tool is refused).

mod claude_subscription_support;

use ai::claude_subscription::{Config, history::CARRIER_KEY};
use ai::types::{AssistantContent, AssistantMessageEvent, Message, StopReason, TranscriptContext};
use ai::{AbortSignal, AssistantMessage, SimpleStreamOptions};
use claude_subscription_support::*;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const NO_UPSTREAM: &str = "http://127.0.0.1:9";

fn blocks(tool_name: &str) -> String {
    json!([
        {"type": "thinking", "thinking": "private", "signature": "signed-test"},
        {"type": "text", "text": "hello\n"},
        {"type": "tool_use", "id": "toolu_test", "name": tool_name, "input": {"code": "x"}},
    ])
    .to_string()
}

const USAGE: &str = r#"{"input_tokens":3,"output_tokens":5,"cache_read_input_tokens":7,"cache_creation_input_tokens":11,"output_tokens_details":{"thinking_tokens":4}}"#;

fn first_turn() -> TranscriptContext {
    context(vec![system("You are Ultron.", vec![rlm_tool()]), user("go")])
}

fn text_of(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|b| match b {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

fn flag<'a>(argv: &'a Value, name: &str) -> &'a str {
    let argv = argv.as_array().unwrap();
    let at = argv
        .iter()
        .position(|a| a == name)
        .unwrap_or_else(|| panic!("{name} missing"));
    argv[at + 1].as_str().unwrap()
}

fn options() -> SimpleStreamOptions {
    SimpleStreamOptions::default()
}

#[test]
fn ai_claude_subscription_canonical_request_lifecycle() {
    seal();
    let fake = Fake::new();
    let model = model("sonnet");
    let first = fake.config(
        NO_UPSTREAM,
        &[
            ("FAKE_OFFLINE_BLOCKS", &blocks("mcp__ultron__rlm")),
            ("FAKE_OFFLINE_USAGE", USAGE),
        ],
    );
    let (message, events) = call(first, &model, &first_turn(), &options());
    assert_eq!(message.stop_reason, StopReason::ToolUse, "{:?}", message.error_message);
    let streamed = |thinking: bool| -> String {
        events
            .iter()
            .filter_map(|e| match e {
                AssistantMessageEvent::TextDelta { delta, .. } if !thinking => Some(delta.as_str()),
                AssistantMessageEvent::ThinkingDelta { delta, .. } if thinking => Some(delta.as_str()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(streamed(true), "private");
    assert_eq!(streamed(false), "hello\n");
    assert!(matches!(
        events.last(),
        Some(AssistantMessageEvent::Done {
            reason: StopReason::ToolUse,
            ..
        })
    ));
    assert!(events.iter().any(|e| matches!(e, AssistantMessageEvent::ToolCallEnd { tool_call, .. } if tool_call.name == "rlm" && tool_call.arguments == json!({"code": "x"}))));
    let AssistantContent::ToolCall(call_block) = &message.content[2] else {
        panic!("{:?}", message.content)
    };
    assert_eq!(
        (call_block.id.as_str(), call_block.name.as_str()),
        ("toolu_test", "rlm")
    );
    let AssistantContent::Thinking(thinking) = &message.content[0] else {
        panic!()
    };
    assert_eq!(
        (thinking.thinking.as_str(), thinking.thinking_signature.as_deref()),
        ("private", Some("signed-test"))
    );
    // Prompt tokens are uncached + cache read + cache write (21); thinking tokens are reported.
    assert_eq!(
        message.usage.input + message.usage.cache_read + message.usage.cache_write,
        21
    );
    assert_eq!(
        (
            message.usage.output,
            message.usage.reasoning,
            message.usage.total_tokens
        ),
        (5, Some(4), 26)
    );
    assert_eq!(message.usage.cost.total, 0.0);
    let usage = diagnostic(&message, "claude_subscription_usage")
        .unwrap()
        .details
        .clone()
        .unwrap();
    assert_eq!(
        usage["native_cost"],
        json!({"total_cost_usd": 0.012345, "modelUsage": {"claude-sonnet-5-5[1m]": {"costBasis": "list"}}})
    );
    assert_eq!(
        usage["native_admission"],
        json!({"upstream_requests": 0, "blocked_requests": 0, "request_id": null})
    );
    let record = fake.last();
    let settings: Value = serde_json::from_str(record["settings"].as_str().unwrap()).unwrap();
    let wire: Value = serde_json::from_str(settings["env"]["CLAUDE_CODE_EXTRA_BODY"].as_str().unwrap()).unwrap();
    assert!(wire["tools"][0]["description"].as_str().unwrap().ends_with("TAIL"));
    assert!(wire.get("metadata").is_none());
    assert_eq!(flag(&record["argv"], "--max-turns"), "1");
    assert_eq!(flag(&record["argv"], "--permission-mode"), "dontAsk");
    assert_eq!(flag(&record["argv"], "--tools"), "");
    assert_eq!(record["system"], "You are Ultron.");
    assert_eq!(
        record["frames"],
        json!([{"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "go"}]}}])
    );

    // The tool round: the unchanged assistant message replays its signed native blocks.
    let mut history = first_turn().messages;
    history.push(Message::Assistant(message.clone()));
    history.push(tool_result("toolu_test", " host\n result"));
    let done = || {
        fake.config(
            NO_UPSTREAM,
            &[("FAKE_OFFLINE_BLOCKS", r#"[{"type":"text","text":"done"}]"#)],
        )
    };
    let (second, _) = call(done(), &model, &context(history.clone()), &options());
    assert_eq!(text_of(&second), "done");
    assert_eq!(second.stop_reason, StopReason::Stop);
    let frames = fake.last()["frames"].clone();
    let native: Value = serde_json::from_str(&blocks("mcp__ultron__rlm")).unwrap();
    assert_eq!(
        frames[1],
        json!({"type": "assistant", "message": {"id": "msg_native", "role": "assistant", "model": "claude-sonnet-5-5[1m]", "content": native, "stop_reason": "tool_use", "usage": serde_json::from_str::<Value>(USAGE).unwrap()}})
    );
    assert_eq!(frames[0]["shouldQuery"], false);
    assert_eq!(
        frames[2],
        json!({"type": "user", "message": {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_test", "content": " host\n result"}]}})
    );
    assert!(frames[2].get("shouldQuery").is_none(), "the last frame queries");

    // A middleware edit of visible content replays canonical blocks instead of stale signatures.
    let Message::Assistant(edited) = &mut history[2] else {
        panic!()
    };
    edited.content[1] = AssistantContent::Text(ai::TextContent {
        text: "middleware changed".into(),
        text_signature: None,
    });
    let (third, _) = call(done(), &model, &context(history), &options());
    assert_eq!(text_of(&third), "done");
    assert_eq!(
        fake.last()["frames"][1]["message"]["content"][0],
        json!({"type": "text", "text": "middleware changed"})
    );
}

#[test]
fn ai_claude_subscription_advertised_inventory_is_exactly_rlm() {
    seal();
    let fake = Fake::new();
    let upstream = Upstream::start(|_, stream| {
        reply_sse(
            stream,
            &reply_events(
                "m1",
                &[json!({"type": "text", "text": "hi"})],
                "end_turn",
                json!({"input_tokens": 1, "output_tokens": 1}),
            ),
        )
    });
    let (message, _) = call(
        fake.config(&upstream.url(), &[]),
        &model("sonnet"),
        &first_turn(),
        &options(),
    );
    assert_eq!(message.stop_reason, StopReason::Stop, "{:?}", message.error_message);
    let record = fake.last();
    // MCP tools/list served by the Rust inert server: exactly rlm, and tools/call is always denied.
    let mcp = &record["mcp"]["ultron"];
    let listed: Vec<&str> = mcp["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(listed, ["rlm"]);
    assert_eq!(mcp["initialize"]["serverInfo"]["name"], "ultron-inert-inventory");
    assert_eq!(
        mcp["call"],
        json!({"isError": true, "content": [{"type": "text", "text": "Denied: native tools are inert; only Ultron executes tools."}]})
    );
    assert_eq!(record["native_tools"], json!(["mcp__ultron__rlm"]));
    assert_eq!(record["mcp"].as_object().unwrap().len(), 1, "no other MCP server");
    // The upstream request the relay captured advertises exactly one tool.
    let seen = upstream.requests();
    assert_eq!(seen.len(), 1);
    let body = seen[0].json();
    let tools: Vec<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["mcp__ultron__rlm"]);
    // Native's own tools, settings sources, slash commands, MCP config and hooks are off.
    let argv = &record["argv"];
    assert_eq!(flag(argv, "--tools"), "");
    assert_eq!(flag(argv, "--setting-sources"), "");
    assert!(argv.as_array().unwrap().iter().any(|a| a == "--strict-mcp-config"));
    assert!(argv.as_array().unwrap().iter().any(|a| a == "--disable-slash-commands"));
    assert!(argv.as_array().unwrap().iter().any(|a| a == "--no-session-persistence"));

    // A request declaring any other tool is refused before anything starts.
    let runs = fake.records().len();
    let bash = ai::Tool {
        name: "bash".into(),
        ..rlm_tool()
    };
    let (refused, events) = call(
        fake.config(&upstream.url(), &[]),
        &model("sonnet"),
        &context(vec![system("s", vec![rlm_tool(), bash]), user("go")]),
        &options(),
    );
    assert_eq!(refused.stop_reason, StopReason::Error);
    assert_eq!(
        refused.error_message.as_deref(),
        Some("claude-subscription advertises exactly one tool, `rlm`; refusing to advertise `bash`")
    );
    assert!(matches!(events.last(), Some(AssistantMessageEvent::Error { .. })));
    assert_eq!(fake.records().len(), runs);
    assert_eq!(upstream.requests().len(), 1);
}

#[test]
fn ai_claude_subscription_a_tool_outside_the_inventory_is_rejected() {
    seal();
    let fake = Fake::new();
    for name in ["mcp__fastmail__draft_email", "mcp__ultron__unlisted", "Bash"] {
        let config = fake.config(NO_UPSTREAM, &[("FAKE_OFFLINE_BLOCKS", &blocks(name))]);
        let (message, _) = call(config, &model("sonnet"), &first_turn(), &options());
        assert_eq!(message.stop_reason, StopReason::Error, "{name}");
        let expected = format!(
            "The model called `{}`, which is not an Ultron tool: claude-subscription advertises exactly one tool, `rlm`",
            name.strip_prefix("mcp__ultron__").unwrap_or(name)
        );
        assert_eq!(message.error_message.as_deref(), Some(expected.as_str()));
        assert!(
            !message
                .content
                .iter()
                .any(|b| matches!(b, AssistantContent::ToolCall(_))),
            "nothing to run"
        );
    }
}

#[test]
fn ai_claude_subscription_bare_rlm_maps_to_the_tool_and_replays_with_the_offered_name() {
    seal();
    let fake = Fake::new();
    let config = fake.config(
        NO_UPSTREAM,
        &[("FAKE_OFFLINE_BLOCKS", &blocks("x")), ("FAKE_TOOL_NAME", "rlm")],
    );
    let (message, _) = call(config, &model("sonnet"), &first_turn(), &options());
    assert_eq!(message.stop_reason, StopReason::ToolUse, "{:?}", message.error_message);
    let AssistantContent::ToolCall(tool) = &message.content[2] else {
        panic!()
    };
    assert_eq!(tool.name, "rlm");
    let carrier = &message.provider_metadata.as_ref().unwrap()[CARRIER_KEY];
    assert_eq!(carrier["messages"][0]["content"][2]["name"], "mcp__ultron__rlm");
    let mut history = first_turn().messages;
    history.push(Message::Assistant(message));
    history.push(tool_result("toolu_test", "ok"));
    let (second, _) = call(
        fake.config(
            NO_UPSTREAM,
            &[("FAKE_OFFLINE_BLOCKS", r#"[{"type":"text","text":"done"}]"#)],
        ),
        &model("sonnet"),
        &context(history),
        &options(),
    );
    assert_eq!(text_of(&second), "done");
    assert_eq!(
        fake.last()["frames"][1]["message"]["content"][2]["name"],
        "mcp__ultron__rlm"
    );
}

#[test]
fn ai_claude_subscription_native_errors_carry_the_status_for_their_code() {
    seal();
    let fake = Fake::new();
    let cases = [
        ("rate_limit", "You've hit your session limit · resets 3:45pm", Some(429)),
        ("billing_error", "Credit balance is too low", Some(402)),
        ("overloaded", "API Error: Overloaded", Some(529)),
        (
            "server_error",
            "API Error: 500 Internal server error. This is a server-side issue, usually temporary",
            Some(503),
        ),
        ("unknown", "API Error: something else", None),
    ];
    for (code, text, status) in cases {
        let error = format!("{code}:{text}");
        let (message, _) = call(
            fake.config(NO_UPSTREAM, &[("FAKE_NATIVE_ERROR", &error)]),
            &model("sonnet"),
            &first_turn(),
            &options(),
        );
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some(format!("Native API error: {text}").as_str())
        );
        let details = diagnostic(&message, "claude_subscription_error")
            .unwrap()
            .details
            .clone()
            .unwrap();
        assert_eq!(details["status"], status.map_or(Value::Null, Value::from), "{code}");
        assert_eq!(
            diagnostic(&message, "provider_usage_limit").is_some(),
            status == Some(429)
        );
    }
}

#[test]
fn ai_claude_subscription_logged_out_native_raises_the_login_hint() {
    seal();
    let fake = Fake::new();
    let config = fake.config(
        NO_UPSTREAM,
        &[(
            "FAKE_NATIVE_ERROR",
            "authentication_failed:Not logged in · Please run /login",
        )],
    );
    let (message, _) = call(config, &model("sonnet"), &first_turn(), &options());
    let error = message.error_message.unwrap();
    assert!(error.starts_with(ai::claude_subscription::LOGGED_OUT_HINT), "{error}");
    assert!(
        error.ends_with("(native: Not logged in · Please run /login)"),
        "{error}"
    );
    let details = diagnostic(&message_status(&fake), "claude_subscription_error")
        .unwrap()
        .details
        .clone()
        .unwrap();
    assert_eq!(details["status"], 401);
}

fn message_status(fake: &Fake) -> AssistantMessage {
    let config = fake.config(
        NO_UPSTREAM,
        &[("FAKE_NATIVE_ERROR", "authentication_failed:Not logged in")],
    );
    call(config, &model("sonnet"), &first_turn(), &options()).0
}

#[test]
fn ai_claude_subscription_missing_claude_carries_503_and_the_install_hint() {
    seal();
    let fake = Fake::new();
    let mut config = fake.config(NO_UPSTREAM, &[]);
    config.command = Some(vec![fake.root().join("no-such-claude").display().to_string()]);
    let (message, _) = call(config, &model("sonnet"), &first_turn(), &options());
    let error = message.error_message.clone().unwrap();
    assert!(
        error.contains("not installed") && error.contains("npm install -g @anthropic-ai/claude-code"),
        "{error}"
    );
    let details = diagnostic(&message, "claude_subscription_error")
        .unwrap()
        .details
        .clone()
        .unwrap();
    assert_eq!(details["status"], 503);
}

#[test]
fn ai_claude_subscription_api_credentials_never_reach_the_child() {
    seal();
    // The subscription stays primary: an API key, gateway token or Foundry key in the environment is removed from
    // the CLI's environment (it would bill the API instead of the plan) and the call runs on the login.
    let fake = Fake::new();
    let config = fake.config(
        NO_UPSTREAM,
        &[
            ("ANTHROPIC_API_KEY", "sk-ant-api03-fixture"),
            ("ANTHROPIC_AUTH_TOKEN", "fake-token-value"),
            ("ANTHROPIC_FOUNDRY_API_KEY", "fake-foundry-value"),
            ("FAKE_OFFLINE_BLOCKS", r#"[{"type":"text","text":"ok"}]"#),
        ],
    );
    let (message, _) = call(config, &model("sonnet"), &first_turn(), &options());
    assert_eq!(text_of(&message), "ok", "{:?}", message.error_message);
    let env = fake.last()["env"].clone();
    for name in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_FOUNDRY_API_KEY"] {
        assert!(env.get(name).is_none(), "{name} reached the child");
    }
    assert!(!env.to_string().contains("sk-ant-api03-fixture"));
}

#[test]
fn ai_claude_subscription_conflicting_auth_overrides_are_refused_by_name() {
    seal();
    let fake = Fake::new();
    for (name, value) in [
        ("ANTHROPIC_BASE_URL", "https://gateway.invalid"),
        ("CLAUDE_CODE_USE_BEDROCK", "1"),
        ("CLAUDE_CODE_USE_VERTEX", "true"),
    ] {
        let (message, _) = call(
            fake.config(NO_UPSTREAM, &[(name, value)]),
            &model("sonnet"),
            &first_turn(),
            &options(),
        );
        let error = message.error_message.unwrap();
        assert!(
            error.starts_with("OAuth provider refuses conflicting native auth/backend overrides: "),
            "{error}"
        );
        assert!(error.contains(name), "{error}");
        if value.len() > 4 {
            assert!(!error.contains(value), "the value leaked: {error}");
        }
    }
    assert!(fake.records().is_empty(), "nothing started");
    // Off values and empty values are not overrides; empty ones never reach the child.
    let config = fake.config(
        NO_UPSTREAM,
        &[
            ("CLAUDE_CODE_USE_BEDROCK", "0"),
            ("ANTHROPIC_API_KEY", ""),
            ("FAKE_OFFLINE_BLOCKS", r#"[{"type":"text","text":"ok"}]"#),
        ],
    );
    let (message, _) = call(config, &model("sonnet"), &first_turn(), &options());
    assert_eq!(text_of(&message), "ok", "{:?}", message.error_message);
    let env = &fake.last()["env"];
    assert!(env.get("ANTHROPIC_API_KEY").is_none() && env.get("CLAUDE_CODE_USE_BEDROCK").is_none());
}

#[test]
fn ai_claude_subscription_child_environment_and_argv_are_isolated() {
    seal();
    let fake = Fake::new();
    let config = fake.config(
        NO_UPSTREAM,
        &[
            ("FAKE_OFFLINE_BLOCKS", r#"[{"type":"text","text":"ok"}]"#),
            ("CLAUDE_CONFIG_DIR", "/private/claude-config"),
            ("CLAUDE_CODE_EXTRA_BODY", r#"{"metadata":{}}"#),
            ("CLAUDE_CODE_EFFORT_LEVEL", "max"),
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "7"),
        ],
    );
    let mut opts = options();
    opts.base.max_tokens = Some(4096);
    let (message, _) = call(config, &model("opus"), &first_turn(), &opts);
    assert_eq!(text_of(&message), "ok", "{:?}", message.error_message);
    let record = fake.last();
    let env = &record["env"];
    assert_eq!(
        env["CLAUDE_CONFIG_DIR"], "/private/claude-config",
        "passed through untouched"
    );
    for (k, v) in [
        ("ENABLE_TOOL_SEARCH", "false"),
        ("CLAUDE_CODE_MAX_RETRIES", "0"),
        ("DISABLE_AUTO_COMPACT", "1"),
        ("DISABLE_COMPACT", "1"),
        ("CLAUDE_CODE_TOTAL_TOKENS_REMINDER", "off"),
        ("DISABLE_AUTOUPDATER", "1"),
        ("DISABLE_FEEDBACK_COMMAND", "1"),
        ("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "4096"),
    ] {
        assert_eq!(env[k], v, "{k}");
    }
    for gone in ["CLAUDE_CODE_EXTRA_BODY", "CLAUDE_CODE_EFFORT_LEVEL", "CLAUDECODE"] {
        assert!(env.get(gone).is_none(), "{gone}");
    }
    let base = env["ANTHROPIC_BASE_URL"].as_str().unwrap();
    assert!(
        base.starts_with("http://127.0.0.1:") && base.contains("/admit/"),
        "{base}"
    );
    let argv: Vec<&str> = record["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    let system = argv[argv.iter().position(|a| *a == "--system-prompt-file").unwrap() + 1];
    let settings = argv[argv.iter().position(|a| *a == "--settings").unwrap() + 1];
    let mcp = argv[argv.iter().position(|a| *a == "--mcp-config").unwrap() + 1];
    assert_eq!(
        argv,
        [
            "-p",
            "--model",
            "claude-opus-5-5[1m]",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--tools",
            "",
            "--system-prompt-file",
            system,
            "--settings",
            settings,
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--disable-slash-commands",
            "--max-turns",
            "1",
            "--permission-mode",
            "dontAsk",
            "--no-session-persistence",
            "--mcp-config",
            mcp,
        ]
    );
    let mcp: Value = serde_json::from_str(mcp).unwrap();
    assert_eq!(mcp["mcpServers"]["ultron"]["command"], inert_mcp());
    let private = std::path::Path::new(system).parent().unwrap();
    assert_eq!(std::path::Path::new(settings).parent().unwrap(), private);
    assert!(!private.exists(), "the per-request directory is removed");
    assert!(private.starts_with(fake.temp_root()));
    let settings: Value = serde_json::from_str(record["settings"].as_str().unwrap()).unwrap();
    let wire: Value = serde_json::from_str(settings["env"]["CLAUDE_CODE_EXTRA_BODY"].as_str().unwrap()).unwrap();
    assert_eq!(wire["max_tokens"], 4096);
    assert!(wire.get("metadata").is_none());
}

#[test]
fn ai_claude_subscription_large_requests_use_files_and_effort_follows_reasoning() {
    seal();
    let fake = Fake::new();
    let mut tool = rlm_tool();
    tool.description = "d".repeat(150_000);
    let context = context(vec![system(&"s".repeat(150_000), vec![tool]), user("go")]);
    let mut opts = options();
    opts.reasoning = Some("minimal".into());
    opts.base.temperature = Some(0.3);
    // Eight deltas 0.3 s apart (2.4 s in all) outlive a 2 s read-idle budget: output resets it. Under load the
    // fake can stall past the budget (Python startup alone can); the timeout is then right, and the call is retried
    // until the fake kept its pace.
    let mut done = None;
    for _attempt in 0..20 {
        let mut config = fake.config(
            NO_UPSTREAM,
            &[
                ("FAKE_OFFLINE_BLOCKS", r#"[{"type":"text","text":"xxxxxxxx"}]"#),
                ("FAKE_DELTA_DELAY", "0.3"),
                ("CLAUDE_CODE_EFFORT_LEVEL", "max"),
            ],
        );
        config.timeout = Duration::from_secs(2);
        let (message, events) = call(config, &model("sonnet"), &context, &opts);
        if message.error_message.as_deref() == Some("Claude request timed out") {
            continue;
        }
        done = Some((message, events));
        break;
    }
    let (message, events) = done.expect("the fake never kept a 0.3 s pace in 20 attempts");
    assert_eq!(text_of(&message), "xxxxxxxx", "{:?}", message.error_message);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, AssistantMessageEvent::TextDelta { .. }))
            .count(),
        8
    );
    let record = fake.last();
    assert_eq!(record["system"].as_str().unwrap().len(), 150_000);
    let settings: Value = serde_json::from_str(record["settings"].as_str().unwrap()).unwrap();
    let wire: Value = serde_json::from_str(settings["env"]["CLAUDE_CODE_EXTRA_BODY"].as_str().unwrap()).unwrap();
    assert_eq!(wire["tools"][0]["description"].as_str().unwrap().len(), 150_000);
    assert!(wire.get("temperature").is_none() && wire.get("top_p").is_none());
    assert_eq!(wire["output_config"]["effort"], "low");
    assert_eq!(flag(&record["argv"], "--effort"), "low");
    assert!(record["env"].get("CLAUDE_CODE_EFFORT_LEVEL").is_none());
    for arg in record["argv"].as_array().unwrap() {
        assert!(arg.as_str().unwrap().len() < 10_000, "no large argument");
    }
}

#[test]
fn ai_claude_subscription_a_silent_native_times_out() {
    seal();
    let fake = Fake::new();
    let mut config = fake.config(
        NO_UPSTREAM,
        &[
            ("FAKE_OFFLINE_BLOCKS", r#"[{"type":"text","text":"late"}]"#),
            ("FAKE_SLEEP", "3600"),
        ],
    );
    config.timeout = Duration::from_millis(500);
    let started = Instant::now();
    let (message, _) = call(config, &model("sonnet"), &first_turn(), &options());
    assert_eq!(message.error_message.as_deref(), Some("Claude request timed out"));
    assert_eq!(message.stop_reason, StopReason::Error);
    // The 0.5 s budget ended it, not the fake's hour-long sleep (a hang guard, loose for load).
    assert!(started.elapsed() < Duration::from_secs(120));
}

#[test]
fn ai_claude_subscription_invalid_stream_json_names_the_offending_line() {
    seal();
    let fake = Fake::new();
    let (message, _) = call(
        fake.config(NO_UPSTREAM, &[("FAKE_GARBAGE", "1")]),
        &model("sonnet"),
        &first_turn(),
        &options(),
    );
    assert_eq!(
        message.error_message.as_deref(),
        Some("Invalid native stream-json output: 'mise WARN tool not activated'")
    );
}

#[test]
fn ai_claude_subscription_replay_needs_zero_turn_acknowledgments() {
    seal();
    let fake = Fake::new();
    let history = context(vec![
        system("s", vec![rlm_tool()]),
        user("one"),
        Message::Assistant(plain_assistant("ack")),
        user("two"),
    ]);
    let (message, _) = call(
        fake.config(NO_UPSTREAM, &[("FAKE_BAD_ACK", "1"), ("FAKE_OFFLINE_BLOCKS", "[]")]),
        &model("sonnet"),
        &history,
        &options(),
    );
    assert_eq!(
        message.error_message.as_deref(),
        Some("Native history replay not supported: expected zero-turn acknowledgment")
    );
}

fn plain_assistant(text: &str) -> AssistantMessage {
    let mut message: AssistantMessage = serde_json::from_value(json!({
        "role": "assistant", "content": [{"type": "text", "text": text}], "api": "claude-subscription",
        "provider": "claude-subscription", "model": "sonnet", "usage": ai::Usage::default(), "stopReason": "stop", "timestamp": 0,
    }))
    .unwrap();
    message.stop_reason = StopReason::Stop;
    message
}

#[test]
fn ai_claude_subscription_cancellation_kills_native_and_its_grandchild() {
    seal();
    let fake = Fake::new();
    let pid_file = fake.root().join("pid");
    let pid_path = pid_file.display().to_string();
    let config = fake.config(NO_UPSTREAM, &[("FAKE_HANG", "1"), ("FAKE_PID_FILE", &pid_path)]);
    let signal = AbortSignal::new();
    let mut opts = options();
    opts.base.signal = Some(signal.clone());
    let child_file = format!("{pid_path}.child");
    // Abort once the fake has started its grandchild and streamed its first delta (it writes both pids first).
    let events = Mutex::new(Vec::new());
    let mut sink = |event: &AssistantMessageEvent| {
        if matches!(event, AssistantMessageEvent::TextDelta { delta, .. } if delta == "started") {
            signal.abort();
        }
        events.lock().unwrap().push(event.clone());
    };
    let message = runtime().block_on(ai::claude_subscription::stream_with(
        &model("sonnet"),
        &first_turn(),
        &opts,
        config,
        &mut sink,
    ));
    let events = events.into_inner().unwrap();
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(message.error_message.as_deref(), Some("Claude request cancelled"));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AssistantMessageEvent::TextDelta { delta, .. } if delta == "started"))
    );
    let read = |p: &str| std::fs::read_to_string(p).unwrap().trim().parse::<u32>().unwrap();
    assert!(
        wait_gone(&[read(&pid_path), read(&child_file)]),
        "cancel left native or its grandchild running"
    );
    // Already aborted: nothing starts.
    let runs = fake.records().len();
    let (message, _) = call(
        fake.config(NO_UPSTREAM, &[("FAKE_OFFLINE_BLOCKS", "[]")]),
        &model("sonnet"),
        &first_turn(),
        &opts,
    );
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(fake.records().len(), runs);
}

// --------------------------------------------------------------------------------------------------- telemetry

const QUIET: [&str; 3] = [
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "DISABLE_TELEMETRY",
    "DISABLE_ERROR_REPORTING",
];

fn turn_env(fake: &Fake, vars: &[(&str, &str)]) -> Value {
    let mut all = vec![("FAKE_OFFLINE_BLOCKS", r#"[{"type":"text","text":"ok"}]"#)];
    all.extend_from_slice(vars);
    let (message, _) = call(
        fake.config(NO_UPSTREAM, &all),
        &model("sonnet"),
        &first_turn(),
        &options(),
    );
    assert_eq!(text_of(&message), "ok", "{:?}", message.error_message);
    fake.last()["env"].clone()
}

#[test]
fn ai_claude_subscription_telemetry_stays_on_by_default_and_the_cli_never_updates_itself() {
    seal();
    let fake = Fake::new();
    for env in [
        turn_env(&fake, &[]),
        turn_env(&fake, &[("ULTRON_CLAUDE_TELEMETRY", "true")]),
    ] {
        for flag in QUIET {
            assert!(env.get(flag).is_none(), "{flag}");
        }
        assert_eq!(env["DISABLE_AUTOUPDATER"], "1");
        assert_eq!(env["DISABLE_FEEDBACK_COMMAND"], "1");
    }
    for value in ["false", "off", "0", "no"] {
        let env = turn_env(&fake, &[("ULTRON_CLAUDE_TELEMETRY", value)]);
        for flag in QUIET {
            assert_eq!(env[flag], "1", "{value} {flag}");
        }
        assert_eq!(env["DISABLE_AUTOUPDATER"], "1");
    }
    // A user's own opt-out is never stripped.
    let env = turn_env(&fake, &[("DISABLE_TELEMETRY", "1"), ("DO_NOT_TRACK", "1")]);
    assert_eq!(
        (env["DISABLE_TELEMETRY"].as_str(), env["DO_NOT_TRACK"].as_str()),
        (Some("1"), Some("1"))
    );
    // The config setting wins over the environment.
    let mut config = fake.config(
        NO_UPSTREAM,
        &[("FAKE_OFFLINE_BLOCKS", r#"[{"type":"text","text":"ok"}]"#)],
    );
    config.telemetry = Some(false);
    call(config, &model("sonnet"), &first_turn(), &options());
    assert_eq!(fake.last()["env"]["DISABLE_TELEMETRY"], "1");
}

// ---------------------------------------------------------------------------------------------------- workspace

fn cwd_of(fake: &Fake, config: Config) -> String {
    let mut config = config;
    config
        .env
        .as_mut()
        .unwrap()
        .insert("FAKE_OFFLINE_BLOCKS".into(), r#"[{"type":"text","text":"ok"}]"#.into());
    let (message, _) = call(config, &model("sonnet"), &first_turn(), &options());
    assert_eq!(text_of(&message), "ok", "{:?}", message.error_message);
    fake.last()["cwd"].as_str().unwrap().to_string()
}

fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(std::env::temp_dir()).map(|_| ()).unwrap();
    let probe = tempfile::tempfile().unwrap();
    probe.metadata().unwrap().uid()
}

#[test]
fn ai_claude_subscription_requests_share_one_workspace_that_survives_prune() {
    seal();
    let fake = Fake::new();
    let shared = fake.temp_root().join(format!("ultron-claude-cwd-{}", uid()));
    let first = cwd_of(&fake, fake.config(NO_UPSTREAM, &[]));
    let second = cwd_of(&fake, fake.config(NO_UPSTREAM, &[]));
    assert_eq!(
        (first.as_str(), second.as_str()),
        (shared.to_str().unwrap(), shared.to_str().unwrap())
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&shared).unwrap().permissions().mode() & 0o777, 0o700);
    std::fs::remove_dir_all(&shared).unwrap();
    let file = std::fs::File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(fake.root().join("old"))
        .unwrap();
    drop(file);
    assert_eq!(cwd_of(&fake, fake.config(NO_UPSTREAM, &[])), shared.to_str().unwrap());
    let before = std::fs::metadata(&shared).unwrap().modified().unwrap();
    std::fs::File::open(&shared)
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH)
        .unwrap();
    cwd_of(&fake, fake.config(NO_UPSTREAM, &[]));
    assert!(
        std::fs::metadata(&shared).unwrap().modified().unwrap() >= before,
        "every request refreshes the mtime"
    );
}

#[test]
fn ai_claude_subscription_an_unsafe_shared_workspace_falls_back_to_one_private_workspace() {
    seal();
    use std::os::unix::fs::PermissionsExt;
    for kind in ["file", "symlink", "open_mode", "open_parent"] {
        let fake = Fake::new();
        let root = fake.temp_root();
        let shared = root.join(format!("ultron-claude-cwd-{}", uid()));
        match kind {
            "file" => std::fs::write(&shared, "not a directory").unwrap(),
            "symlink" => {
                std::fs::create_dir(fake.root().join("elsewhere")).unwrap();
                std::os::unix::fs::symlink(fake.root().join("elsewhere"), &shared).unwrap();
            }
            "open_mode" => {
                std::fs::create_dir(&shared).unwrap();
                std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
            }
            _ => std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap(),
        }
        let private = cwd_of(&fake, fake.config(NO_UPSTREAM, &[]));
        assert_ne!(
            private,
            shared.to_str().unwrap(),
            "{kind}: an unsafe shared path is never used"
        );
        assert!(private.contains("ultron-claude-cwd-"), "{private}");
        assert_eq!(
            cwd_of(&fake, fake.config(NO_UPSTREAM, &[])),
            private,
            "{kind}: one private cwd per process"
        );
        std::fs::remove_dir_all(&private).unwrap();
        assert_eq!(
            cwd_of(&fake, fake.config(NO_UPSTREAM, &[])),
            private,
            "{kind}: recreated after a prune"
        );
        std::fs::remove_dir_all(&private).unwrap();
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o777)).unwrap();
        let next = cwd_of(&fake, fake.config(NO_UPSTREAM, &[]));
        assert_ne!(next, private, "{kind}: a path someone else recreated is never adopted");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}
