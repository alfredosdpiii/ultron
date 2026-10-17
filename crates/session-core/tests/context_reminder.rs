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
    assert_eq!(reminders.reminder("main", "x", 0), None);
    let used = Arc::new(Mutex::new(163_610u64));
    let seen = used.clone();
    reminders.set_meter(Arc::new(move |lane: &str| {
        (lane == "main").then(|| at(*seen.lock().unwrap()))
    }));
    // 163,610 + ceil(20 / 4) = 163,615: one short.
    assert_eq!(reminders.reminder("main", "abcdefghijklmnopqrst", 0), None);
    // 163,610 + ceil(21 / 4) = 163,616.
    assert_eq!(
        reminders.reminder("main", "abcdefghijklmnopqrstu", 0),
        Some(format!(
            "[context: 163.6k of 183.6k tokens used before compaction{TAIL}"
        ))
    );
    assert_eq!(reminders.reminder("scout", &"x".repeat(1_000_000), 0), None);
}

#[test]
fn session_core_context_reminder_counts_a_results_images_as_the_compaction_estimator_does() {
    let reminders = ContextReminders::new(ON);
    reminders.set_limit("main", Some(limit(200_000, 16_384)));
    reminders.set_meter(Arc::new(|_: &str| Some(at(162_000))));
    // 162,000 + ceil(6,000 / 4) = 163,500: below the trigger (163,616).
    assert_eq!(reminders.reminder("main", &"x".repeat(6_000), 0), None);
    // The same text with one image: 162,000 + ceil((6,000 + 4,800) / 4) = 164,700.
    assert_eq!(
        reminders.reminder("main", &"x".repeat(6_000), 1),
        Some(format!(
            "[context: 164.7k of 183.6k tokens used before compaction{TAIL}"
        ))
    );
    let fresh = ContextReminders::new(ON);
    fresh.set_limit("main", Some(limit(200_000, 16_384)));
    fresh.set_meter(Arc::new(|_: &str| Some(at(161_216))));
    // Two images and no text: 161,216 + 2 * 1,200 = 163,616, exactly the trigger.
    assert_eq!(
        fresh.reminder("main", "", 2),
        Some(format!(
            "[context: 163.6k of 183.6k tokens used before compaction{TAIL}"
        ))
    );
}

#[test]
fn session_core_context_reminder_measures_a_frame_lane_by_its_own_meter_until_it_ends() {
    let reminders = ContextReminders::new(ON);
    // The session's meter knows no frame lane.
    reminders.set_meter(Arc::new(|lane: &str| (lane == "main").then(|| at(10))));
    let window = Limit {
        context_window: 32_000,
        reserve_tokens: 0,
        compaction: false,
    };
    let used = Arc::new(Mutex::new(None::<u64>));
    let seen = used.clone();
    reminders.set_frame_lane("frame-1", Some((window, Arc::new(move || *seen.lock().unwrap()))));
    // No answer yet: nothing to measure.
    assert_eq!(reminders.reminder("frame-1", "x", 0), None);
    // A 32,000-token window keeps the 3,200-token margin: the trigger is 28,800.
    *used.lock().unwrap() = Some(28_799);
    assert_eq!(reminders.reminder("frame-1", "", 0), None);
    assert_eq!(
        reminders.reminder("frame-1", "abcd", 0),
        Some(format!(
            "[context: 28.8k of 32k tokens used before the context limit{TAIL}"
        ))
    );
    assert_eq!(reminders.reminder("frame-1", "abcd", 0), None);
    // The frame ended: no limit, no meter, no state.
    reminders.set_frame_lane("frame-1", None);
    assert_eq!(reminders.limit("frame-1"), None);
    *used.lock().unwrap() = Some(31_000);
    assert_eq!(reminders.reminder("frame-1", "x", 0), None);
    // A new frame on the same lane name starts armed.
    let again = used.clone();
    reminders.set_frame_lane("frame-1", Some((window, Arc::new(move || *again.lock().unwrap()))));
    assert!(reminders.reminder("frame-1", "x", 0).is_some());
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

#[test]
fn session_core_context_reminder_ends_an_error_result_too() {
    let a = actor(
        in_order(vec![
            tool_at("toolu_1", "print('one')", 50_000),
            tool_at("toolu_2", "raise ValueError('two')", 95_000),
            tool_at("toolu_3", "raise ValueError('three')", 96_000),
            text("done"),
        ]),
        Setup {
            settings: Some(settings(true)),
            vars: vars(&[]),
            ..Setup::default()
        },
    );
    a.prompt("count");
    let requests = a.requests();
    assert_eq!(requests.len(), 4);
    let results = texts(tool_results(requests.last().unwrap()));
    let traceback = |n: &str| {
        format!(
            "Traceback (most recent call last):\n  File \"<rlm-cell>\", line 1, in <module>\n    raise ValueError('{n}')\nValueError: {n}"
        )
    };
    assert_eq!(
        results,
        vec![
            "one".to_string(),
            format!(
                "{}\n[context: 95k of 100k tokens used before compaction{TAIL}",
                traceback("two")
            ),
            traceback("three"),
        ]
    );
}

/// A text answer whose response reports `input` input tokens.
fn text_at(answer: &str, input: u64) -> String {
    sse(&[
        json!({"type": "message_start", "message": {"id": "msg_t", "type": "message", "role": "assistant", "model": "claude-sonnet-4-5", "content": [], "stop_reason": null, "usage": {"input_tokens": input, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": answer}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ])
}

/// The texts of a request's user messages.
fn user_messages(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .map(|m| match &m["content"] {
            Value::String(text) => text.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        })
        .collect()
}

#[test]
fn session_core_context_reminder_ends_a_backgrounded_cells_completion() {
    // The root's cell waits on its child in the host (`rlm.merge`), so it is detached and its end arrives as a `cell_done`
    // event. The child's cell waits for `release`, which the script writes when the root answers after the detach
    // (with 95,000 input tokens: past the 90,000 trigger).
    let gate = tempfile::tempdir().unwrap();
    let release = gate.path().join("release");
    let child_code = format!(
        "import os, time\nwhile not os.path.exists({:?}):\n    time.sleep(0.01)\nprint('c1')",
        release.display().to_string()
    );
    let (seen_tx, seen_rx) = std::sync::mpsc::channel::<Value>();
    let counters = Arc::new(Mutex::new((0usize, 0usize)));
    let script = Box::new(move |body: &Value, _| {
        let mut counters = counters.lock().unwrap();
        if first_user_text(body).contains("look around") {
            counters.1 += 1;
            return match counters.1 {
                1 => tool_at("toolu_c1", &child_code, 1_000),
                _ => text("child done"),
            };
        }
        counters.0 += 1;
        match counters.0 {
            1 => tool_at(
                "toolu_root",
                "h = await rlm.spawn('look around', name='scout')\nawait rlm.merge([h])",
                1_000,
            ),
            2 => {
                std::fs::write(&release, "go").unwrap();
                text_at("waiting", 95_000)
            }
            _ => {
                if user_messages(body).iter().any(|m| m.contains("kind=\"cell_done\"")) {
                    let _ = seen_tx.send(body.clone());
                }
                // A `child_done` delivered on its own keeps the use where it was.
                text_at("thanks", 95_000)
            }
        }
    });
    let a = actor(
        script,
        Setup {
            settings: Some(settings(true)),
            vars: vars(&[]),
            ..Setup::default()
        },
    );
    a.prompt("delegate");
    let request = seen_rx.recv_timeout(p_session_support::HANG).unwrap_or_else(|_| {
        let seen: Vec<(Vec<String>, Vec<String>)> = a
            .requests()
            .iter()
            .map(|r| (user_messages(r), texts(tool_results(r))))
            .collect();
        panic!("the root got no cell completion: {seen:#?}")
    });
    let event = user_messages(&request)
        .into_iter()
        .find(|m| m.contains("kind=\"cell_done\""))
        .unwrap();
    // The cell's end (with the child's, when they fall in one coalescing window), then the reminder: 95,005 tokens
    // after the last answer plus the message's own estimate.
    let lines: Vec<&str> = event.lines().collect();
    let (reminder, events) = lines.split_last().unwrap();
    assert!(events.iter().all(|l| l.starts_with("<runtime_event kind=")), "{event}");
    assert!(
        events
            .iter()
            .any(|l| l.starts_with("<runtime_event kind=\"cell_done\" id=\"cell-1\" status=\"completed\"")),
        "{event}"
    );
    assert_eq!(
        *reminder,
        format!("[context: 95.1k of 100k tokens used before compaction{TAIL}")
    );
    // The detach note itself carried none.
    let root: Vec<Value> = a
        .requests()
        .into_iter()
        .filter(|r| !first_user_text(r).contains("look around"))
        .collect();
    let detached = texts(tool_results(&root[1]));
    assert!(detached[0].starts_with("Cell "), "{detached:?}");
    assert!(!detached[0].contains("[context:"), "{detached:?}");
}

#[test]
fn session_core_context_reminder_reaches_an_inference_frame_that_runs_code() {
    // The frame runs on anthropic/claude-sonnet-4-5 too: its limit is the 1,000,000-token window (frames never
    // compact), so the trigger is 990,000 with the 10,000-token margin.
    let frame_requests = Arc::new(Mutex::new(Vec::new()));
    let seen = frame_requests.clone();
    let counters = Arc::new(Mutex::new((0usize, 0usize)));
    let script = Box::new(move |body: &Value, _| {
        let mut counters = counters.lock().unwrap();
        if user_messages(body).iter().any(|m| m.contains("count the frame")) {
            seen.lock().unwrap().push(body.clone());
            counters.1 += 1;
            return match counters.1 {
                1 => tool_at("toolu_f1", "print('f1')", 990_000),
                2 => tool_at("toolu_f2", "print('f2')", 991_000),
                _ => text("frame answer"),
            };
        }
        counters.0 += 1;
        match counters.0 {
            1 => tool_at(
                "toolu_root",
                "r = await rlm.infer('count the frame', budget={'depth': 2, 'calls': 8, 'tokens': 5000000})\nprint(r)",
                1_000,
            ),
            _ => text("done"),
        }
    });
    let a = actor(
        script,
        Setup {
            vars: vars(&[]),
            ..Setup::default()
        },
    );
    a.prompt("infer");
    let requests = frame_requests.lock().unwrap().clone();
    let root: Vec<Value> = a
        .requests()
        .into_iter()
        .filter(|r| !user_messages(r).iter().any(|m| m.contains("count the frame")))
        .collect();
    assert_eq!(
        requests.len(),
        3,
        "root: {:?}",
        root.iter().map(|r| texts(tool_results(r))).collect::<Vec<_>>()
    );
    assert_eq!(
        texts(tool_results(requests.last().unwrap())),
        vec![
            format!("f1\n[context: 990k of 1000k tokens used before the context limit{TAIL}"),
            "f2".to_string(),
        ]
    );
    // The frame's answer reached the root's cell; the root's own lane stayed far below its trigger.
    assert_eq!(texts(tool_results(&root[1])), ["frame answer"]);
}

#[test]
fn session_core_context_reminder_counts_an_attached_image() {
    // 88,907 tokens after the answer: the text alone stays below the 90,000 trigger, the image's 1,200 cross it.
    let a = actor(
        in_order(vec![
            tool_at("toolu_1", "print('plain')", 88_900),
            tool_at("toolu_2", "print(await view_image('dot.png'))", 88_900),
            text("done"),
        ]),
        Setup {
            settings: Some(settings(true)),
            vars: vars(&[]),
            files: vec![("dot.png", p_session_support::png(4, 3))],
        },
    );
    a.prompt("look");
    let requests = a.requests();
    assert_eq!(requests.len(), 3);
    let results = tool_results(requests.last().unwrap());
    assert_eq!(texts(vec![results[0].clone()]), ["plain"]);
    let blocks = results[1].as_array().unwrap();
    assert_eq!(blocks[1]["type"], "image", "{blocks:?}");
    let text = blocks[0]["text"].as_str().unwrap();
    assert_eq!(
        text.lines().last().unwrap(),
        format!("[context: 90.1k of 100k tokens used before compaction{TAIL}"),
        "{text}"
    );
}

/// A lane that looks idle but turns out busy when the event's run would start: the batch is steered instead.
#[derive(Default)]
struct RacingLane {
    accepted: Mutex<Vec<Value>>,
    steered: Mutex<Vec<Value>>,
}

impl session_core::async_events::EventLanes for RacingLane {
    fn execution(&self, _lane: &str) -> session_core::async_events::Execution {
        if self.steered.lock().unwrap().is_empty() {
            session_core::async_events::Execution::Idle
        } else {
            session_core::async_events::Execution::Run
        }
    }
    fn accept(&self, _lane: &str, message: &Value, _root: &str) -> session_core::async_events::Accepted {
        self.accepted.lock().unwrap().push(message.clone());
        session_core::async_events::Accepted::Busy
    }
    fn steer(&self, _lane: &str, message: &Value) -> Result<String, String> {
        self.steered.lock().unwrap().push(message.clone());
        Ok("entry-1".into())
    }
    fn next_run(&self, _lane: &str, _message: &Value) -> Result<String, String> {
        Err("unused".into())
    }
    fn cancel_queued(&self, _lane: &str, _entry: &str) -> Result<session_core::async_events::Cancelled, String> {
        Ok(session_core::async_events::Cancelled::NotFound)
    }
}

#[test]
fn session_core_context_reminder_of_a_cell_end_is_decided_once_per_batch() {
    use session_core::async_events::{Dispatcher, DispatcherOptions, RuntimeEvent};
    let lane = Arc::new(RacingLane::default());
    let asked = Arc::new(Mutex::new(Vec::new()));
    let seen = asked.clone();
    let mut options = DispatcherOptions::new(Arc::new(|run: &str| format!("turn:{run}")));
    // As the reminder is: due the first time, not again in the same approach.
    options.note = Some(Arc::new(move |lane: &str, text: &str| {
        let mut asked = seen.lock().unwrap();
        asked.push((lane.to_string(), text.to_string()));
        (asked.len() == 1).then(|| "[context: reminder]".to_string())
    }));
    let dispatcher = Dispatcher::new(options, lane.clone());
    let event = |kind: &str, id: &str| RuntimeEvent {
        kind: kind.into(),
        id: id.into(),
        status: "completed".into(),
        summary: "ok".into(),
        fetch: "await rlm.cells()".into(),
        lane: "main".into(),
        root_id: Some("root-1".into()),
    };
    dispatcher.publish(event("cell_done", "cell-1"));
    dispatcher.drain();
    let content = |m: &Value| m["content"].as_str().unwrap().to_string();
    let line = "<runtime_event kind=\"cell_done\" id=\"cell-1\" status=\"completed\" summary=\"ok\" fetch=\"await rlm.cells()\" />";
    // The run it would have started and the steer it became carry the same note.
    let accepted: Vec<String> = lane.accepted.lock().unwrap().iter().map(content).collect();
    let steered: Vec<String> = lane.steered.lock().unwrap().iter().map(content).collect();
    assert_eq!(accepted, [format!("{line}\n[context: reminder]")]);
    assert_eq!(steered, accepted);
    assert_eq!(*asked.lock().unwrap(), [("main".to_string(), line.to_string())]);
    // A batch without a cell end asks for no note.
    dispatcher.publish(event("job_done", "job-1"));
    dispatcher.drain();
    assert_eq!(asked.lock().unwrap().len(), 1);
    dispatcher.close();
}
