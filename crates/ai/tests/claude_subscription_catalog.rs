//! Ports of the Hermes plugin's `tests/test_directsdk_models.py`, the request-body cases of `test_directsdk.py`
//! (`test_fail_closed_and_cancellation`, `test_tool_schemas_are_normalized_for_the_native_validator`),
//! `test_directsdk_headers.py`'s empty-inventory body, and `test_directsdk_replay.py` / `test_directsdk_vision.py`
//! on Ultron's message model: routes, windows, thinking policy, schema normalization, the one-tool inventory and
//! native history replay.

mod claude_subscription_support;

use ai::claude_subscription::ClaudeError;
use ai::claude_subscription::catalog::{self, context_window, native_model};
use ai::claude_subscription::history::{
    BodyParams, CARRIER, CARRIER_KEY, PREFIX, Reasoning, carrier, prepare_history, projection, request_body,
};
use ai::types::{
    AssistantContent, AssistantMessage, ImageContent, Message, StopReason, TextContent, ThinkingContent, Tool,
    ToolCall, ToolResultMessage, UserContent, UserMessage, UserMessageContent,
};
use claude_subscription_support::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;

const EXPECTED: &[(&str, u64)] = &[
    ("claude-sonnet-5-5[1m]", 1_000_000),
    ("claude-sonnet-5[1m]", 1_000_000),
    ("claude-haiku-5-5[1m]", 1_000_000),
    ("claude-haiku-4-5-20251001", 200_000),
    ("claude-opus-5-5[1m]", 1_000_000),
    ("claude-opus-5[1m]", 1_000_000),
    ("claude-opus-4-8[1m]", 1_000_000),
    ("claude-fable-5-1[1m]", 1_000_000),
];

#[test]
fn ai_claude_subscription_catalog_windows_match_explicit_native_routes() {
    seal();
    let routes: Vec<(String, u64)> = catalog::pinned_routes().into_iter().map(|(r, _, w)| (r, w)).collect();
    let expected: Vec<(String, u64)> = EXPECTED.iter().map(|(r, w)| (r.to_string(), *w)).collect();
    assert_eq!(routes, expected);
    for (route, window) in EXPECTED {
        assert_eq!(context_window(route), *window, "{route}");
        assert_eq!(native_model(route).unwrap(), *route);
        assert_eq!(catalog::model(route).unwrap().context_window, *window);
    }
    // Unpinned: the plain id runs within native's 200K gateway default; a [1m] id is never promised more.
    assert_eq!(context_window("unqualified-future-model"), 200_000);
    assert_eq!(context_window("unqualified-future-model[1m]"), 200_000);
    assert_eq!(
        native_model("unqualified-future-model").unwrap(),
        "unqualified-future-model"
    );
    assert_eq!(
        native_model("claude-haiku-4-5[1m]").unwrap_err(),
        "Haiku 4.5 does not support a 1M context window"
    );
}

#[test]
fn ai_claude_subscription_aliases_resolve_to_pinned_routes_and_models_resolve_by_id() {
    seal();
    let aliases = [
        ("sonnet", "claude-sonnet-5-5[1m]"),
        ("opus", "claude-opus-5-5[1m]"),
        ("haiku", "claude-haiku-5-5[1m]"),
        ("claude-haiku-4-5", "claude-haiku-4-5-20251001"),
        ("fable", "claude-fable-5-1[1m]"),
        ("claude-opus-5-5", "claude-opus-5-5[1m]"),
        ("unqualified-future-model", "unqualified-future-model"),
    ];
    for (requested, expected) in aliases {
        assert_eq!(native_model(requested).unwrap(), expected, "{requested}");
    }
    let sonnet = ai::models::builtin_model("claude-subscription", "sonnet").unwrap();
    assert_eq!(
        (
            sonnet.id.as_str(),
            sonnet.provider.as_str(),
            sonnet.api.as_str(),
            sonnet.context_window
        ),
        ("sonnet", "claude-subscription", "claude-subscription", 1_000_000)
    );
    assert_eq!(sonnet.name, "Sonnet 5.5 (Claude subscription)");
    assert_eq!(sonnet.base_url, "https://api.anthropic.com");
    assert_eq!(sonnet.cost, ai::types::ModelCost::default());
    assert!(std::ptr::eq(
        sonnet,
        ai::models::builtin_model("claude-subscription", "sonnet").unwrap()
    ));
    for invalid in ["claude-haiku-4-5[1m]", "Sonnet", "", "a/b", "sonnet; rm"] {
        assert!(
            ai::models::builtin_model("claude-subscription", invalid).is_none(),
            "{invalid}"
        );
    }
    assert_eq!(catalog::catalog_label("claude-opus-4-8"), "Opus 4.8");
    assert_eq!(catalog::catalog_label("claude-haiku-4-5-20251001"), "Haiku 4.5");
    assert_eq!(
        catalog::version_hint("sonnet").unwrap(),
        "Sonnet 5.5 needs Claude Code 2.1.284 or newer; run `claude update`"
    );
    assert_eq!(
        catalog::version_hint("haiku").unwrap(),
        "Haiku 5.5 needs Claude Code 2.1.293 or newer; run `claude update`"
    );
    assert_eq!(
        catalog::version_hint("opus").unwrap(),
        "Opus 5.5 needs Claude Code 2.1.280 or newer; run `claude update`"
    );
    assert!(catalog::version_hint("claude-opus-4-8").is_none());
    // The CLI login authenticates the provider: there is never a key to look up or send.
    assert_eq!(
        ai::env::env_api_key("claude-subscription", None).as_deref(),
        Some("<claude-code-login>")
    );
}

fn body(model: &str, reasoning: Option<Reasoning>) -> Value {
    request_body(&BodyParams {
        model,
        tools: &[rlm_tool()],
        reasoning,
        ..Default::default()
    })
    .unwrap()
    .value()
}

fn off() -> Option<Reasoning> {
    Some(Reasoning {
        enabled: Some(false),
        effort: None,
    })
}

fn on(effort: &str) -> Option<Reasoning> {
    Some(Reasoning {
        enabled: Some(true),
        effort: Some(effort.into()),
    })
}

#[test]
fn ai_claude_subscription_thinking_policy_follows_each_route() {
    seal();
    let disabled = body("claude-sonnet-5", off());
    assert_eq!(disabled["thinking"], json!({"type": "disabled"}));
    assert_eq!(disabled["context_management"], json!({"edits": []}));
    // Fable, Opus 5.5, Sonnet 5.5 and Haiku 5.5 answer the disable with a 400: it is omitted on every spelling.
    for route in [
        "fable",
        "opus",
        "claude-opus-5-5",
        "claude-opus-5-5[1m]",
        "sonnet",
        "claude-sonnet-5-5",
        "claude-sonnet-5-5[1m]",
        "haiku",
        "claude-haiku-5-5",
        "claude-haiku-5-5[1m]",
    ] {
        let mandatory = body(route, off());
        assert!(mandatory.get("thinking").is_none(), "{route}");
        assert!(mandatory.get("context_management").is_none(), "{route}");
    }
    for route in [
        "claude-opus-5",
        "claude-sonnet-5",
        "claude-sonnet-5[1m]",
        "claude-haiku-4-5",
        "claude-haiku-4-5-20251001",
    ] {
        assert_eq!(body(route, off())["thinking"], json!({"type": "disabled"}), "{route}");
    }
    let effort = body(
        "sonnet",
        Some(Reasoning {
            enabled: None,
            effort: Some("low".into()),
        }),
    );
    assert_eq!(effort["output_config"], json!({"effort": "low"}));
    assert!(effort.get("thinking").is_none());
    let adaptive = body("sonnet", on("medium"));
    assert_eq!(adaptive["thinking"], json!({"type": "adaptive"}));
    assert_eq!(adaptive["output_config"], json!({"effort": "medium"}));
    for route in ["haiku", "claude-haiku-5-5", "claude-haiku-5-5[1m]"] {
        assert_eq!(
            body(route, on("medium"))["thinking"],
            json!({"type": "adaptive"}),
            "{route}"
        );
    }
    for route in ["claude-haiku-4-5", "claude-haiku-4-5-20251001"] {
        // Haiku 4.5 answers `adaptive thinking is not supported on this model` with a 400.
        let haiku = body(route, on("medium"));
        assert!(haiku.get("thinking").is_none(), "{route}");
        assert_eq!(haiku["output_config"], json!({"effort": "medium"}));
    }
    assert_eq!(body("sonnet", on("minimal"))["output_config"], json!({"effort": "low"}));
    assert_eq!(body("sonnet", on("max"))["output_config"], json!({"effort": "max"}));
    assert!(matches!(
        request_body(&BodyParams { model: "sonnet", reasoning: on("extreme"), ..Default::default() }),
        Err(ClaudeError::Invalid(m)) if m == "Unsupported native reasoning effort"
    ));
    let schema = json!({"type": "object", "properties": {"title": {"type": "string"}}, "required": ["title"], "additionalProperties": false});
    let formatted = request_body(&BodyParams {
        model: "sonnet",
        json_schema: Some(schema.clone()),
        ..Default::default()
    })
    .unwrap()
    .value();
    assert_eq!(
        formatted["output_config"]["format"],
        json!({"type": "json_schema", "schema": schema})
    );
    let limited = request_body(&BodyParams {
        model: "sonnet",
        max_tokens: Some(321),
        stop_sequences: Some(vec!["END".into()]),
        ..Default::default()
    })
    .unwrap()
    .value();
    assert_eq!(
        limited,
        json!({"max_tokens": 321, "stop_sequences": ["END"], "tools": []})
    );
    assert!(
        request_body(&BodyParams {
            model: "sonnet",
            max_tokens: Some(0),
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        request_body(&BodyParams {
            model: "sonnet",
            stop_sequences: Some(vec![String::new()]),
            ..Default::default()
        })
        .is_err()
    );
}

#[test]
fn ai_claude_subscription_inventory_is_exactly_rlm_and_other_tools_are_refused() {
    seal();
    let empty = request_body(&BodyParams {
        model: "sonnet",
        ..Default::default()
    })
    .unwrap();
    assert_eq!(empty.value(), json!({"tools": []}));
    assert_eq!(empty.manifest, Vec::<Value>::new());
    assert!(empty.names.is_empty());
    let one = request_body(&BodyParams {
        model: "sonnet",
        tools: &[rlm_tool()],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        one.value()["tools"],
        json!([{"name": "mcp__ultron__rlm", "description": "Run Python in the persistent REPL. TAIL", "input_schema": {"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"]}}])
    );
    assert_eq!(
        one.manifest,
        vec![
            json!({"name": "rlm", "description": "Run Python in the persistent REPL. TAIL", "inputSchema": {"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"]}})
        ]
    );
    assert_eq!(one.names, BTreeSet::from(["rlm".to_string()]));
    let bash = Tool {
        name: "bash".into(),
        ..rlm_tool()
    };
    for tools in [vec![bash.clone()], vec![rlm_tool(), bash], vec![rlm_tool(), rlm_tool()]] {
        assert!(
            request_body(&BodyParams {
                model: "sonnet",
                tools: &tools,
                ..Default::default()
            })
            .is_err()
        );
    }
    assert_eq!(
        request_body(&BodyParams {
            model: "sonnet",
            tools: &[Tool {
                name: "read".into(),
                ..rlm_tool()
            }],
            ..Default::default()
        })
        .unwrap_err()
        .to_string(),
        "claude-subscription advertises exactly one tool, `rlm`; refusing to advertise `read`"
    );
}

#[test]
fn ai_claude_subscription_tool_schemas_are_normalized_for_the_native_validator() {
    seal();
    let mut tool = rlm_tool();
    tool.parameters = json!({
        "type": "object",
        "properties": {
            "mode": {"type": "string"},
            "proposal": {"anyOf": [{"type": "string"}, {"type": "null"}]},
            "ref": {"oneOf": [{"type": "string"}, {"type": "integer"}]},
        },
        "required": ["mode"],
        "allOf": [{"if": {"properties": {"mode": {"const": "proposal"}}}, "then": {"required": ["proposal"]}}],
    });
    let encoded = request_body(&BodyParams {
        model: "sonnet",
        tools: std::slice::from_ref(&tool),
        ..Default::default()
    })
    .unwrap();
    for schema in [
        encoded.value()["tools"][0]["input_schema"].clone(),
        encoded.manifest[0]["inputSchema"].clone(),
    ] {
        assert!(schema.get("allOf").is_none());
        assert_eq!(schema["required"], json!(["mode"]));
        assert_eq!(schema["properties"]["proposal"], json!({"type": "string"}));
        assert_eq!(
            schema["properties"]["ref"],
            json!({"oneOf": [{"type": "string"}, {"type": "integer"}]})
        );
    }
    tool.parameters = json!({"anyOf": [{"type": "object"}]});
    let bare = request_body(&BodyParams {
        model: "sonnet",
        tools: &[tool],
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        bare.value()["tools"][0]["input_schema"],
        json!({"type": "object", "properties": {}})
    );
}

// -------------------------------------------------------------------------------------------------- replay

fn assistant(content: Vec<AssistantContent>) -> AssistantMessage {
    let mut message: AssistantMessage = serde_json::from_value(json!({
        "role": "assistant", "content": [], "api": "claude-subscription", "provider": "claude-subscription",
        "model": "sonnet", "usage": ai::Usage::default(), "stopReason": "toolUse", "timestamp": 0,
    }))
    .unwrap();
    message.content = content;
    message.stop_reason = StopReason::ToolUse;
    message
}

fn text(t: &str) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: t.into(),
        text_signature: None,
    })
}

fn call(id: &str, name: &str, arguments: Value) -> AssistantContent {
    AssistantContent::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        thought_signature: None,
        namespace: None,
    })
}

fn names() -> BTreeSet<String> {
    BTreeSet::from(["rlm".to_string()])
}

#[test]
fn ai_claude_subscription_host_history_edits_replay_canonical_visible_blocks() {
    seal();
    let arguments = json!({"code": "a".repeat(3000)});
    let mut message = assistant(vec![
        AssistantContent::Thinking(ThinkingContent {
            thinking: "private".into(),
            thinking_signature: Some("opaque".into()),
            redacted: None,
        }),
        text("Writing."),
        call("t1", "rlm", arguments.clone()),
    ]);
    let blocks = json!([
        {"type": "thinking", "thinking": "private", "signature": "opaque"},
        {"type": "text", "text": "Writing."},
        {"type": "tool_use", "id": "t1", "name": format!("{PREFIX}rlm"), "input": arguments},
    ]);
    let native = vec![json!({"role": "assistant", "content": blocks})];
    message.provider_metadata = Some(
        [(CARRIER_KEY.to_string(), carrier(native, projection(&message)))]
            .into_iter()
            .collect(),
    );
    let mut history = vec![
        user("write"),
        Message::Assistant(message.clone()),
        tool_result("t1", "ok"),
    ];
    assert_eq!(
        prepare_history(&history, &names()).unwrap()[1]["message"]["content"],
        blocks
    );
    // Compaction demotes tool-result bodies only: the signed native blocks survive verbatim.
    history[2] = tool_result("t1", "[tool result demoted]");
    assert_eq!(
        prepare_history(&history, &names()).unwrap()[1]["message"]["content"],
        blocks
    );
    // A hook edit of visible content drops the stale signed thinking and replays canonical blocks.
    let mut edited = message.clone();
    edited.content[1] = text("Hook replacement");
    history[1] = Message::Assistant(edited.clone());
    let frame = &prepare_history(&history, &names()).unwrap()[1]["message"];
    let kinds: Vec<&str> = frame["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["text", "tool_use"]);
    assert_eq!(frame["content"][0]["text"], "Hook replacement");
    assert_eq!(frame["content"][1]["input"], arguments);
    assert_eq!(frame["content"][1]["name"], "mcp__ultron__rlm");
    // Whitespace around the visible text is harmless normalization.
    let mut spaced = message.clone();
    spaced.content[1] = text("Writing.\n");
    history[1] = Message::Assistant(spaced);
    assert_eq!(
        prepare_history(&history, &names()).unwrap()[1]["message"]["content"],
        blocks
    );
    // A carrier of another version is refused, never guessed at.
    let mut future = message;
    future.provider_metadata.as_mut().unwrap().get_mut(CARRIER_KEY).unwrap()["version"] = json!(999);
    history[1] = Message::Assistant(future);
    let error = prepare_history(&history, &names()).unwrap_err().to_string();
    assert!(error.contains("version"), "{error}");
    assert_eq!(CARRIER, "claude-subscription.native_assistant");
}

#[test]
fn ai_claude_subscription_edited_history_replays_a_call_outside_the_inventory_by_its_own_name() {
    seal();
    let history = vec![
        user("draft"),
        Message::Assistant(assistant(vec![
            text("Drafting."),
            call("t1", "mcp__fastmail__draft_email", json!({})),
            call("t2", "rlm", json!({})),
        ])),
        tool_result("t1", "unknown"),
        tool_result("t2", "ok"),
    ];
    let frames = prepare_history(&history, &names()).unwrap();
    let named: Vec<&str> = frames[1]["message"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .map(|b| b["name"].as_str().unwrap())
        .collect();
    assert_eq!(named, ["mcp__fastmail__draft_email", "mcp__ultron__rlm"]);
    // Both results replay in one queried frame.
    assert_eq!(frames.len(), 3);
    assert_eq!(
        frames[2]["message"]["content"],
        json!([{"type": "tool_result", "tool_use_id": "t1", "content": "unknown"}, {"type": "tool_result", "tool_use_id": "t2", "content": "ok"}])
    );
}

#[test]
fn ai_claude_subscription_images_reach_native_as_image_blocks() {
    seal();
    let image = UserContent::Image(ImageContent {
        data: "iVBORw0KGgo=".into(),
        mime_type: "image/png".into(),
    });
    let result = Message::ToolResult(ToolResultMessage {
        tool_call_id: "c1".into(),
        tool_name: "rlm".into(),
        content: vec![
            UserContent::Text(TextContent {
                text: "image:".into(),
                text_signature: None,
            }),
            image.clone(),
        ],
        details: None,
        usage: None,
        is_error: true,
        timestamp: 0,
    });
    let history = vec![
        user("look"),
        Message::Assistant(assistant(vec![call("c1", "rlm", json!({}))])),
        result,
    ];
    let frames = prepare_history(&history, &names()).unwrap();
    let block = &frames[2]["message"]["content"][0];
    assert_eq!(block["type"], "tool_result");
    assert_eq!(block["tool_use_id"], "c1");
    assert_eq!(block["is_error"], true);
    assert_eq!(
        block["content"][1],
        json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}})
    );
    let looked = vec![Message::User(UserMessage {
        content: UserMessageContent::Blocks(vec![
            UserContent::Text(TextContent {
                text: "look".into(),
                text_signature: None,
            }),
            image,
        ]),
        timestamp: 0,
    })];
    assert_eq!(
        prepare_history(&looked, &names()).unwrap()[0]["message"]["content"],
        json!([{"type": "text", "text": "look"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}}])
    );
}

#[test]
fn ai_claude_subscription_history_must_end_in_a_user_frame_and_later_system_messages_replay_in_place() {
    seal();
    let prefill = vec![user("hi"), Message::Assistant(assistant(vec![text("Sure")]))];
    assert_eq!(
        prepare_history(&prefill, &names()).unwrap_err().to_string(),
        "History must end in a nonempty user/tool-result message; assistant prefill is unsupported"
    );
    assert!(prepare_history(&[], &names()).is_err());
    let history = vec![
        user("first"),
        Message::Assistant(assistant(vec![text("ok")])),
        system("Clock: 10:00", vec![]),
        user("second"),
    ];
    let frames = prepare_history(&history, &names()).unwrap();
    assert_eq!(
        frames,
        vec![
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "first"}]}}),
            json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}}),
            json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": "Clock: 10:00"}, {"type": "text", "text": "second"}]}}),
        ]
    );
}
