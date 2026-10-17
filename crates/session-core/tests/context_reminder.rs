//! The context-pressure reminder (session_core::context_reminder), a Rust-only feature with literal expectations:
//! the threshold arithmetic, the environment's margin and off switch, once per approach with its re-arming, and
//! sessions against a scripted provider whose usage crosses the trigger, on the root lane and on a subagent's.

mod p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool_results};
use serde_json::{Value, json};
use session_core::context_reminder::{
    ContextReminders, Limit, Measure, ReminderSettings, format_tokens, lane_limit, margin, reminder_line,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ON: ReminderSettings = ReminderSettings {
    enabled: true,
    margin: None,
};

fn limit(window: u64, reserve: u64) -> Limit {
    Limit {
        context_window: window,
        reserve_tokens: reserve,
        compaction: true,
    }
}

fn at(tokens: u64) -> Measure {
    Measure { tokens, epoch: None }
}

const TAIL: &str = "; free space with ctx now: await ctx.history() lists items with their bytes, await ctx.forget(ids, reason) or await ctx.summarize(ids, text) drops or shortens them]";

#[test]
fn session_core_context_reminder_margin_is_the_larger_of_2048_and_a_tenth_of_the_window() {
    assert_eq!(margin(200_000, None), 20_000);
    assert_eq!(margin(1_000_000, None), 100_000);
    // A small window keeps the 2,048-token floor.
    assert_eq!(margin(16_000, None), 2048);
    assert_eq!(margin(20_480, None), 2048);
    assert_eq!(margin(20_490, None), 2049);
    // The environment's margin wins, even below the floor.
    assert_eq!(margin(200_000, Some(500)), 500);
    assert_eq!(margin(200_000, Some(0)), 0);
}

#[test]
fn session_core_context_reminder_triggers_at_the_compaction_threshold_minus_the_margin() {
    let l = limit(200_000, 16_384);
    assert_eq!(l.tokens(), 183_616);
    assert_eq!(l.trigger(None), 163_616);
    assert_eq!(l.trigger(Some(2048)), 181_568);
    // A margin past the limit triggers at once; a reserve past the window leaves no room.
    assert_eq!(limit(10_000, 9_000).trigger(None), 0);
    assert_eq!(limit(10_000, 20_000).tokens(), 0);
}

#[test]
fn session_core_context_reminder_line_names_the_use_the_limit_and_ctx() {
    assert_eq!(format_tokens(999), "999");
    assert_eq!(format_tokens(1000), "1k");
    assert_eq!(format_tokens(182_345), "182.3k");
    assert_eq!(format_tokens(183_616), "183.6k");
    assert_eq!(format_tokens(200_000), "200k");
    assert_eq!(format_tokens(200_099), "200k");
    assert_eq!(
        reminder_line(182_345, &limit(200_000, 16_384)),
        format!("[context: 182.3k of 183.6k tokens used before compaction{TAIL}")
    );
    // With auto-compaction off the limit is the usable window.
    let off = Limit {
        compaction: false,
        ..limit(200_000, 16_384)
    };
    assert_eq!(
        reminder_line(170_000, &off),
        format!("[context: 170k of 183.6k tokens used before the context limit{TAIL}")
    );
}

#[test]
fn session_core_context_reminder_reminds_once_per_approach_and_rearms_below_the_trigger() {
    let reminders = ContextReminders::new(ON);
    reminders.set_limit("main", Some(limit(200_000, 16_384)));
    assert_eq!(reminders.observe("main", at(163_615)), None);
    assert_eq!(
        reminders.observe("main", at(163_616)),
        Some(format!(
            "[context: 163.6k of 183.6k tokens used before compaction{TAIL}"
        ))
    );
    // Still above the trigger: no second reminder.
    assert_eq!(reminders.observe("main", at(170_000)), None);
    assert_eq!(reminders.observe("main", at(190_000)), None);
    // ctx edits brought it below the trigger: the next crossing reminds again.
    assert_eq!(reminders.observe("main", at(120_000)), None);
    assert_eq!(
        reminders.observe("main", at(175_500)),
        Some(format!(
            "[context: 175.5k of 183.6k tokens used before compaction{TAIL}"
        ))
    );
    assert_eq!(reminders.observe("main", at(176_000)), None);
}

#[test]
fn session_core_context_reminder_rearms_after_a_compaction() {
    let reminders = ContextReminders::new(ON);
    reminders.set_limit("main", Some(limit(200_000, 16_384)));
    assert!(reminders.observe("main", at(170_000)).is_some());
    assert_eq!(reminders.observe("main", at(171_000)), None);
    // The actor compacted the root: still above the trigger, it reminds again.
    reminders.rearm("main");
    assert!(reminders.observe("main", at(172_000)).is_some());
    assert_eq!(reminders.observe("main", at(172_500)), None);
    // A child conversation's compaction shows as a new summary entry (epoch).
    let compacted = Measure {
        tokens: 173_000,
        epoch: Some(42),
    };
    assert!(reminders.observe("main", compacted).is_some());
    assert_eq!(reminders.observe("main", compacted), None);
}

#[test]
fn session_core_context_reminder_keeps_each_lane_apart() {
    let reminders = ContextReminders::new(ON);
    reminders.set_limit("main", Some(limit(200_000, 16_384)));
    reminders.set_limit("scout", Some(limit(100_000, 10_000)));
    assert_eq!(reminders.observe("scout", at(79_999)), None);
    assert_eq!(
        reminders.observe("scout", at(80_000)),
        Some(format!("[context: 80k of 90k tokens used before compaction{TAIL}"))
    );
    assert!(reminders.observe("main", at(170_000)).is_some());
    assert_eq!(reminders.observe("scout", at(85_000)), None);
    // A lane without a limit (unknown, Claude Code) never reminds; a window of 0 is unknown.
    assert_eq!(reminders.observe("other", at(1_000_000)), None);
    reminders.set_limit("zero", Some(limit(0, 0)));
    assert_eq!(reminders.observe("zero", at(1_000_000)), None);
    reminders.set_limit("scout", None);
    assert_eq!(reminders.observe("scout", at(60_000)), None);
    assert_eq!(reminders.observe("scout", at(99_000)), None);
}

#[test]
fn session_core_context_reminder_reads_its_environment() {
    let parse = ReminderSettings::from_vars;
    assert_eq!(parse(None, None), ON);
    for off in ["off", "OFF", " 0 ", "false", "False"] {
        assert!(!parse(Some(off), None).enabled, "{off}");
    }
    for on in ["on", "1", "true", "", "no"] {
        assert!(parse(Some(on), None).enabled, "{on}");
    }
    assert_eq!(parse(None, Some("4096")).margin, Some(4096));
    assert_eq!(parse(None, Some(" 0 ")).margin, Some(0));
    for invalid in ["-1", "4k", "", "1.5"] {
        assert_eq!(parse(None, Some(invalid)).margin, None, "{invalid}");
    }
    // The environment's margin moves the trigger; off never reminds.
    let reminders = ContextReminders::new(parse(None, Some("4096")));
    reminders.set_limit("main", Some(limit(200_000, 16_384)));
    assert_eq!(reminders.observe("main", at(179_519)), None);
    assert!(reminders.observe("main", at(179_520)).is_some());
    let off = ContextReminders::new(parse(Some("off"), Some("4096")));
    off.set_limit("main", Some(limit(200_000, 16_384)));
    assert_eq!(off.observe("main", at(199_000)), None);
}

#[test]
fn session_core_context_reminder_adds_the_result_to_the_meter() {
    let reminders = ContextReminders::new(ON);
    reminders.set_limit("main", Some(limit(200_000, 16_384)));
    // No meter (the external Claude Code root has no session actor): no reminder.
    assert_eq!(reminders.reminder("main", "x"), None);
    let used = Arc::new(Mutex::new(163_610u64));
    let seen = used.clone();
    reminders.set_meter(Arc::new(move |lane: &str| {
        (lane == "main").then(|| at(*seen.lock().unwrap()))
    }));
    // 163,610 + ceil(20 / 4) = 163,615: one short.
    assert_eq!(reminders.reminder("main", "abcdefghijklmnopqrst"), None);
    // 163,610 + ceil(21 / 4) = 163,616.
    assert_eq!(
        reminders.reminder("main", "abcdefghijklmnopqrstu"),
        Some(format!(
            "[context: 163.6k of 183.6k tokens used before compaction{TAIL}"
        ))
    );
    assert_eq!(reminders.reminder("scout", &"x".repeat(1_000_000)), None);
}

#[test]
fn session_core_context_reminder_skips_claude_code_lanes() {
    let mut model: ai::Model = serde_json::from_value(json!({
        "id": "m", "name": "m", "api": "anthropic-messages", "provider": "anthropic", "baseUrl": "",
        "reasoning": false, "input": ["text"], "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
        "contextWindow": 200000, "maxTokens": 8000
    }))
    .unwrap();
    assert_eq!(
        lane_limit(&model, false, 1000),
        Some(Limit {
            compaction: false,
            ..limit(200_000, 1000)
        })
    );
    model.provider = ai::claude_code::PROVIDER.into();
    assert_eq!(lane_limit(&model, true, 1000), None);
}

// Sessions on anthropic/claude-sonnet-4-5 (a 1,000,000-token window). With `compaction.reserveTokens` 900,000 the
// limit is 100,000; `ULTRON_CONTEXT_REMINDER_TOKENS=10000` puts the trigger at 90,000.

fn sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

/// One `rlm` call whose response reports `input` input tokens (and 7 output tokens).
fn tool_at(id: &str, code: &str, input: u64) -> String {
    sse(&[
        json!({"type": "message_start", "message": {"id": id, "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": input, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": id, "name": "rlm", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": json!({"code": code}).to_string()}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 7}}),
        json!({"type": "message_stop"}),
    ])
}

fn settings(enabled: bool) -> Value {
    json!({"compaction": {"enabled": enabled, "reserveTokens": 900_000}})
}

fn vars(extra: &[(&'static str, &str)]) -> Vec<(&'static str, String)> {
    let mut vars = vec![
        ("ULTRON_CONTEXT_REMINDER_TOKENS", "10000".to_string()),
        ("ULTRON_COMPACTION_TRIM", "off".to_string()),
    ];
    vars.extend(extra.iter().map(|(k, v)| (*k, v.to_string())));
    vars
}

fn texts(results: Vec<Value>) -> Vec<String> {
    results
        .into_iter()
        .map(|content| match content {
            Value::String(text) => text,
            blocks => blocks
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
        })
        .collect()
}

fn root_results(enabled: bool, extra: &[(&'static str, &str)]) -> Vec<String> {
    let a = actor(
        in_order(vec![
            tool_at("toolu_1", "print('one')", 50_000),
            tool_at("toolu_2", "print('two')", 95_000),
            tool_at("toolu_3", "print('three')", 96_000),
            tool_at("toolu_4", "print('four')", 40_000),
            tool_at("toolu_5", "print('five')", 97_500),
            text("done"),
        ]),
        Setup {
            settings: Some(settings(enabled)),
            vars: vars(extra),
            ..Setup::default()
        },
    );
    a.prompt("count");
    let requests = a.requests();
    assert_eq!(requests.len(), 6);
    texts(tool_results(requests.last().unwrap()))
}

#[test]
fn session_core_context_reminder_ends_the_root_result_that_crosses_the_trigger() {
    assert_eq!(
        root_results(true, &[]),
        vec![
            "one".to_string(),
            format!("two\n[context: 95k of 100k tokens used before compaction{TAIL}"),
            "three".to_string(),
            "four".to_string(),
            format!("five\n[context: 97.5k of 100k tokens used before compaction{TAIL}"),
        ]
    );
}

#[test]
fn session_core_context_reminder_names_the_context_limit_with_auto_compaction_off() {
    assert_eq!(
        root_results(false, &[]),
        vec![
            "one".to_string(),
            format!("two\n[context: 95k of 100k tokens used before the context limit{TAIL}"),
            "three".to_string(),
            "four".to_string(),
            format!("five\n[context: 97.5k of 100k tokens used before the context limit{TAIL}"),
        ]
    );
}

#[test]
fn session_core_context_reminder_is_off_with_the_environment_switch() {
    assert_eq!(
        root_results(true, &[("ULTRON_CONTEXT_REMINDER", "off")]),
        ["one", "two", "three", "four", "five"]
    );
}

#[test]
fn session_core_context_reminder_comes_after_the_budget_line() {
    let results = root_results(true, &[("ULTRON_MAX_TOTAL_TURNS", "20")]);
    assert_eq!(
        results[1],
        format!(
            "two\n[budget left for this request: 18 model turns]\n[context: 95k of 100k tokens used before compaction{TAIL}"
        )
    );
    assert_eq!(results[2], "three\n[budget left for this request: 17 model turns]");
}

#[test]
fn session_core_context_reminder_rearms_after_the_root_compacts() {
    let a = actor(
        in_order(vec![
            tool_at("toolu_1", "print('one')", 95_000),
            // Past the limit: no second reminder, and the next checkpoint compacts.
            tool_at("toolu_2", "print('two')", 101_000),
            text("## Goal\nsummary"),
            tool_at("toolu_3", "print('three')", 96_000),
            text("done"),
        ]),
        Setup {
            settings: Some(json!({"compaction": {"reserveTokens": 900_000, "keepRecentTokens": 1}})),
            vars: vars(&[]),
            ..Setup::default()
        },
    );
    a.prompt("count");
    assert_eq!(a.events("compaction_end").len(), 1, "{:?}", a.events("compaction_end"));
    let requests = a.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(
        texts(tool_results(&requests[1])),
        [format!(
            "one\n[context: 95k of 100k tokens used before compaction{TAIL}"
        )]
    );
    // requests[2] is the summary's; after it the context keeps the second result, which was not reminded.
    assert!(first_user_text(&requests[2]).starts_with("# Conversation"));
    assert_eq!(texts(tool_results(&requests[3])), ["two"]);
    let after = texts(tool_results(requests.last().unwrap()));
    assert_eq!(
        after.last().unwrap(),
        &format!("three\n[context: 96k of 100k tokens used before compaction{TAIL}")
    );
}

#[test]
fn session_core_context_reminder_reaches_a_subagent_lane() {
    let parent = [
        tool_at(
            "toolu_spawn",
            "h = await rlm.spawn('look around', name='scout')\nprint('spawned')",
            1_000,
        ),
        text("waiting"),
    ];
    // The child compacts at the harness's default threshold: 1,000,000 - 16,384 = 983,616; trigger 973,616.
    let child = [
        tool_at("toolu_c1", "print('c1')", 970_000),
        tool_at("toolu_c2", "print('c2')", 975_000),
        tool_at("toolu_c3", "print('c3')", 976_000),
        text("child done"),
    ];
    let child_requests = Arc::new(Mutex::new(Vec::new()));
    let seen = child_requests.clone();
    let counters = Arc::new(Mutex::new((0usize, 0usize)));
    let script = Box::new(move |body: &Value, _| {
        let mut counters = counters.lock().unwrap();
        if first_user_text(body).contains("look around") {
            seen.lock().unwrap().push(body.clone());
            counters.1 += 1;
            child.get(counters.1 - 1).cloned().unwrap_or_else(|| text("(no more)"))
        } else {
            counters.0 += 1;
            parent.get(counters.0 - 1).cloned().unwrap_or_else(|| text("(no more)"))
        }
    });
    let a = actor(
        script,
        Setup {
            vars: vars(&[]),
            ..Setup::default()
        },
    );
    a.prompt("delegate");
    // The child runs on after the root's turn: wait for its last request (a hang guard, never a pass condition).
    let deadline = Instant::now() + p_session_support::HANG;
    let requests = loop {
        let requests = child_requests.lock().unwrap().clone();
        if requests.len() >= 4 {
            break requests;
        }
        assert!(Instant::now() < deadline, "the child made {} requests", requests.len());
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(requests.len(), 4);
    let results = texts(tool_results(requests.last().unwrap()));
    assert_eq!(
        results,
        vec![
            "c1".to_string(),
            format!("c2\n[context: 975k of 983.6k tokens used before compaction{TAIL}"),
            "c3".to_string(),
        ]
    );
    // The root lane stayed far below its own trigger.
    let parent: Vec<Value> = a
        .requests()
        .into_iter()
        .filter(|r| !first_user_text(r).contains("look around"))
        .collect();
    assert_eq!(texts(tool_results(&parent[1])), ["spawned"]);
}

fn first_user_text(body: &Value) -> String {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["role"] == "user")
        .map(|m| match &m["content"] {
            Value::String(text) => text.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        })
        .unwrap_or_default()
}
