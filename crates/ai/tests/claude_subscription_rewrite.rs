//! Ports of the Hermes plugin's `tests/test_admission_restoration.py` (pure cases), `test_admission_breakpoint.py`
//! and `test_admission_first_turn.py`, literal fixtures included: the relay restores Ultron's queried tool-result
//! frame only when native's copy is a lossless, unambiguous extension, keeps one message breakpoint on content the
//! next request replays, and puts the first turn ahead of native's prepended context.

#[path = "claude_subscription_support/isolate.rs"]
mod isolate;
use ai::claude_subscription::rewrite::{MISMATCH, QueriedTurnMismatch, pin_message_breakpoint, restore_queried_turn};
use isolate::seal;
use serde_json::{Value, json};

fn result() -> Value {
    json!({"type": "tool_result", "tool_use_id": "t1", "content": "output"})
}

fn assistant() -> Value {
    json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "probe", "input": {}}]})
}

fn with(base: Value, extra: Value) -> Value {
    let mut merged = base;
    for (k, v) in extra.as_object().unwrap() {
        merged[k] = v.clone();
    }
    merged
}

/// `wire(content)`: `{"model": "fixture", "messages": [ASSISTANT, {"role": "user", "content": content}]}`.
fn wire(content: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"model": "fixture", "messages": [assistant(), {"role": "user", "content": content}]}))
        .unwrap()
}

fn restored(payload: &[u8], queried: &[Value]) -> Value {
    serde_json::from_slice(&restore_queried_turn(payload, Some(queried)).unwrap()).unwrap()
}

fn mismatch(payload: &[u8], queried: &[Value]) {
    assert_eq!(restore_queried_turn(payload, Some(queried)), Err(QueriedTurnMismatch));
}

#[test]
fn ai_claude_subscription_equivalent_plain_tool_result_shapes_restore_exact_host_shape() {
    seal();
    for native in [
        json!({"type": "tool_result", "tool_use_id": "t1", "content": "output", "is_error": false}),
        json!({"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "output"}]}),
        json!({"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "output"}], "is_error": false}),
    ] {
        assert_eq!(
            restored(&wire(json!([native])), &[result()])["messages"][1]["content"],
            json!([result()])
        );
    }
}

#[test]
fn ai_claude_subscription_invalid_or_true_error_flags_never_normalize_to_success() {
    seal();
    for flag in [json!(0), Value::Null, json!("false"), json!(true)] {
        mismatch(&wire(json!([with(result(), json!({"is_error": flag}))])), &[result()]);
    }
}

#[test]
fn ai_claude_subscription_nested_result_accepts_only_appended_unmarked_plain_text() {
    seal();
    let host = with(result(), json!({"content": [{"type": "text", "text": "output"}]}));
    let native = with(
        result(),
        json!({"content": [{"type": "text", "text": "output"}, {"type": "text", "text": "native note"}]}),
    );
    assert_eq!(
        restored(&wire(json!([native])), std::slice::from_ref(&host))["messages"][1]["content"],
        json!([host])
    );
    for host_content in [
        json!([{"type": "text", "text": "output"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}}]),
        json!([{"type": "text", "text": "output", "citations": [{"title": "host citation"}]}]),
    ] {
        let host = with(result(), json!({"content": host_content}));
        let mut extended = host_content.as_array().unwrap().clone();
        extended.push(json!({"type": "text", "text": "native note"}));
        let native = with(result(), json!({"content": extended}));
        assert_eq!(
            restored(&wire(json!([native])), std::slice::from_ref(&host))["messages"][1]["content"],
            json!([host])
        );
    }
}

#[test]
fn ai_claude_subscription_split_host_text_restores_original_representation() {
    seal();
    for content in [
        json!([{"type": "text", "text": "out"}, {"type": "text", "text": "put"}]),
        json!("output"),
    ] {
        let host = with(result(), json!({"content": content}));
        let native = with(
            result(),
            json!({"content": [{"type": "text", "text": "o"}, {"type": "text", "text": "utput native suffix"}]}),
        );
        assert_eq!(
            restored(&wire(json!([native])), std::slice::from_ref(&host))["messages"][1]["content"],
            json!([host])
        );
    }
}

#[test]
fn ai_claude_subscription_nested_cache_settings_are_kept_and_conflicts_rejected() {
    seal();
    let marker = json!({"type": "ephemeral", "ttl": "1h"});
    let host = with(result(), json!({"content": [{"type": "text", "text": "output"}]}));
    let native = with(
        host.clone(),
        json!({"content": [{"type": "text", "text": "output", "cache_control": marker}, {"type": "text", "text": "native suffix"}]}),
    );
    assert_eq!(
        restored(&wire(json!([native])), std::slice::from_ref(&host))["messages"][1]["content"],
        json!([with(
            host,
            json!({"content": [{"type": "text", "text": "output", "cache_control": marker}]})
        )])
    );
    let content = json!([{"type": "text", "text": "output", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]);
    let host = with(result(), json!({"content": content}));
    let mut appended = content.as_array().unwrap().clone();
    appended.push(json!({"type": "text", "text": "native suffix"}));
    let native = with(result(), json!({"content": appended}));
    assert_eq!(
        restored(&wire(json!([native])), std::slice::from_ref(&host))["messages"][1]["content"],
        json!([host])
    );
    let host = with(
        result(),
        json!({"content": [{"type": "text", "text": "output", "cache_control": {"type": "ephemeral", "ttl": "1h"}}]}),
    );
    let native = with(
        result(),
        json!({"content": [{"type": "text", "text": "output", "cache_control": {"type": "ephemeral", "ttl": "5m"}}]}),
    );
    mismatch(&wire(json!([native])), &[host]);
}

#[test]
fn ai_claude_subscription_protected_or_changed_text_cannot_be_flattened() {
    seal();
    for content in [
        json!([{"type": "text", "text": "out native insertion put"}]),
        json!([{"type": "text", "text": "output", "unknown": "native metadata"}]),
        json!([{"type": "text", "text": "output", "cache_control": {"type": "ephemeral"}}]),
    ] {
        mismatch(&wire(json!([with(result(), json!({"content": content}))])), &[result()]);
    }
    let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}});
    let host = with(
        result(),
        json!({"content": [{"type": "text", "text": "output"}, image]}),
    );
    let native = with(
        result(),
        json!({"content": [{"type": "text", "text": "output native insertion"}, image]}),
    );
    mismatch(&wire(json!([native])), &[host]);
    for extra in [
        image.clone(),
        json!({"type": "text", "text": "note", "cache_control": {"type": "ephemeral"}}),
        json!({"type": "text", "text": "note", "citations": [{"type": "web_search_result_location"}]}),
    ] {
        let host = with(result(), json!({"content": [{"type": "text", "text": "output"}]}));
        let native = with(
            result(),
            json!({"content": [{"type": "text", "text": "output"}, extra]}),
        );
        mismatch(&wire(json!([native])), &[host]);
    }
}

#[test]
fn ai_claude_subscription_mixed_frame_restores_split_and_extended_user_text() {
    seal();
    let host = [result(), json!({"type": "text", "text": "user words"})];
    let native = json!([result(), {"type": "text", "text": "user "}, {"type": "text", "text": "words native note"}]);
    assert_eq!(restored(&wire(native), &host)["messages"][1]["content"], json!(host));
}

#[test]
fn ai_claude_subscription_unexpected_duplicate_or_appended_results_fail_closed() {
    seal();
    mismatch(
        &wire(json!([result(), {"type": "tool_result", "tool_use_id": "unexpected", "content": "hidden"}])),
        &[result()],
    );
    let duplicate = with(result(), json!({"content": "second output"}));
    mismatch(&wire(json!([result(), duplicate])), &[result(), duplicate]);
    mismatch(&wire(json!([result(), result()])), &[result()]);
    mismatch(
        &wire(json!([with(result(), json!({"tool_use_id": "different"}))])),
        &[result()],
    );
}

#[test]
fn ai_claude_subscription_plain_user_frames_are_out_of_scope_byte_for_byte() {
    seal();
    let question = json!({"type": "text", "text": "hello"});
    let raw = wire(json!([question]));
    assert_eq!(
        restore_queried_turn(&raw, Some(std::slice::from_ref(&question))).unwrap(),
        raw
    );
    let rewritten = wire(json!([{"type": "text", "text": "rewritten"}]));
    assert_eq!(restore_queried_turn(&rewritten, Some(&[question])).unwrap(), rewritten);
    assert_eq!(restore_queried_turn(b"not json", None).unwrap(), b"not json");
}

#[test]
fn ai_claude_subscription_restoration_is_anchored_on_ids_and_content_not_native_wording() {
    seal();
    for annotation in [
        "\n<system-reminder>\n# userEmail\nsomeone@example.invalid\n</system-reminder>",
        "\nSession context: a wording no CLI has shipped yet",
        "\n\nToday's date is 2031-01-01.",
    ] {
        let native = with(result(), json!({"content": format!("output{annotation}")}));
        assert_eq!(
            restored(&wire(json!([native])), &[result()])["messages"][1]["content"],
            json!([result()])
        );
    }
    // An original reminder in the host's own result is kept; only native's addition goes.
    let original = with(
        result(),
        json!({"content": "real output\n<system-reminder>quoted fixture</system-reminder>"}),
    );
    let native = with(
        original.clone(),
        json!({"content": "real output\n<system-reminder>quoted fixture</system-reminder>\n<system-reminder>userEmail</system-reminder>"}),
    );
    assert_eq!(
        restored(&wire(json!([native])), std::slice::from_ref(&original))["messages"][1]["content"],
        json!([original])
    );
    // Parallel results with native's reminder on the first and a trailing native block.
    let second = json!({"type": "tool_result", "tool_use_id": "t2", "content": "second output"});
    let first = json!({"type": "tool_result", "tool_use_id": "t1", "content": "real output"});
    let native = json!([{"type": "tool_result", "tool_use_id": "t1", "content": "real output\n<system-reminder>userEmail</system-reminder>"}, second, {"type": "text", "text": "native session context"}]);
    assert_eq!(
        restored(&wire(native), &[first.clone(), second.clone()])["messages"][1]["content"],
        json!([first, second])
    );
    // A separate native system message after the frame is untouched.
    let messages = json!([assistant(), {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "real output\n<system-reminder>userEmail: matt@example.com</system-reminder>"}]}, {"role": "system", "content": [{"type": "text", "text": "Today's date is 2026-09-30."}]}]);
    let out = restored(
        &serde_json::to_vec(&json!({"model": "m", "messages": messages})).unwrap(),
        std::slice::from_ref(&first),
    );
    assert_eq!(out["messages"][1]["content"], json!([first]));
    assert_eq!(out["messages"][2], messages[2]);
    assert_eq!(MISMATCH, "Native request does not uniquely match Ultron tool results");
}

// ---------------------------------------------------------------------------------------------- the breakpoint

const QUESTION: &str = "the real question";

fn bp_assistant() -> Value {
    json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "signed", "signature": "sig"}, {"type": "tool_use", "id": "t1", "name": "mcp__ultron__probe", "input": {}}]})
}

fn bp_result() -> Value {
    json!({"type": "tool_result", "tool_use_id": "t1", "content": "real output"})
}

fn question() -> Value {
    json!({"type": "text", "text": QUESTION})
}

fn marked(block: Value) -> Value {
    with(block, json!({"cache_control": {"type": "ephemeral"}}))
}

fn bp_wire(messages: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"model": "m", "messages": messages})).unwrap()
}

fn marks(payload: &[u8]) -> Vec<(usize, usize)> {
    let body: Value = serde_json::from_slice(payload).unwrap();
    let mut found = Vec::new();
    for (i, m) in body["messages"].as_array().unwrap().iter().enumerate() {
        for (j, b) in m["content"].as_array().unwrap().iter().enumerate() {
            if b.get("cache_control").is_some() {
                found.push((i, j));
            }
        }
    }
    found
}

fn unmarked(payload: &[u8]) -> Value {
    let mut body: Value = serde_json::from_slice(payload).unwrap();
    for m in body["messages"].as_array_mut().unwrap() {
        for b in m["content"].as_array_mut().unwrap() {
            b.as_object_mut().unwrap().remove("cache_control");
        }
    }
    body
}

#[test]
fn ai_claude_subscription_breakpoint_moves_to_the_last_block_ultron_itself_sent() {
    seal();
    let date = || marked(json!({"type": "text", "text": "Today's date is 2026-09-23."}));
    let cases = [
        (
            vec![bp_result()],
            json!([bp_assistant(), {"role": "user", "content": [with(bp_result(), json!({"content": "real output\n<system-reminder>userEmail</system-reminder>"}))]}, {"role": "system", "content": [date()]}]),
            (0, 1),
        ),
        (
            vec![question()],
            json!([bp_assistant(), {"role": "user", "content": [question(), marked(json!({"type": "text", "text": "Session context: v9 build"}))]}]),
            (1, 0),
        ),
        (
            vec![question()],
            json!([bp_assistant(), {"role": "user", "content": [{"type": "text", "text": "Any new preamble"}, marked(question())]}]),
            (0, 1),
        ),
        (
            vec![bp_result(), question()],
            json!([bp_assistant(), {"role": "user", "content": [bp_result(), marked(json!({"type": "text", "text": "the real question\n<system-reminder>x</system-reminder>"}))]}]),
            (1, 0),
        ),
        (
            vec![bp_result()],
            json!([bp_assistant(), {"role": "user", "content": [bp_result()]}, {"role": "system", "content": [date()]}]),
            (1, 0),
        ),
        (
            vec![bp_result()],
            json!([bp_assistant(), {"role": "user", "content": [bp_result(), marked(json!({"type": "text", "text": "Session context: v9 build"}))]}]),
            (1, 0),
        ),
    ];
    for (queried, messages, expected) in cases {
        let raw = bp_wire(messages);
        let out = pin_message_breakpoint(&raw, Some(&queried));
        assert_eq!(marks(&out), vec![expected]);
        assert_eq!(unmarked(&out), unmarked(&raw), "only the directive moves");
    }
}

#[test]
fn ai_claude_subscription_parallel_results_do_not_pin_inside_a_partly_changed_user_message() {
    seal();
    for count in [2usize, 4] {
        let results: Vec<Value> = (0..count)
            .map(|i| json!({"type": "tool_result", "tool_use_id": format!("t{i}"), "content": format!("output {i}")}))
            .collect();
        let calls: Vec<Value> = (0..count)
            .map(|i| json!({"type": "tool_use", "id": format!("t{i}"), "name": format!("probe_{i}"), "input": {}}))
            .collect();
        let mut content = results[..count - 1].to_vec();
        let last = &results[count - 1];
        content.push(marked(with(last.clone(), json!({"content": format!("{}\n<system-reminder>native note</system-reminder>", last["content"].as_str().unwrap())}))));
        let raw = bp_wire(json!([{"role": "assistant", "content": calls}, {"role": "user", "content": content}]));
        let out = pin_message_breakpoint(&raw, Some(&results));
        assert_eq!(marks(&out), vec![(0, count - 1)]);
        assert_eq!(unmarked(&out), unmarked(&raw));
    }
}

#[test]
fn ai_claude_subscription_stable_breakpoint_or_nothing_to_anchor_forwards_unchanged() {
    seal();
    let cases: Vec<(Vec<u8>, Option<Vec<Value>>)> = vec![
        (
            bp_wire(json!([bp_assistant(), {"role": "user", "content": [bp_result(), marked(question())]}])),
            Some(vec![bp_result(), question()]),
        ),
        (
            bp_wire(json!([{"role": "user", "content": [marked(json!({"type": "text", "text": "Any preamble"}))]}])),
            Some(vec![question()]),
        ),
        (b"not json".to_vec(), Some(vec![question()])),
        (
            bp_wire(json!([bp_assistant(), {"role": "user", "content": [marked(bp_result())]}])),
            None,
        ),
    ];
    for (payload, queried) in cases {
        assert_eq!(pin_message_breakpoint(&payload, queried.as_deref()), payload);
    }
}

// ------------------------------------------------------------------------------------------------ first turn

fn reminder() -> Value {
    json!({"type": "text", "text": "<system-reminder>\nAs you answer the user's questions, you can use the following context:\n# userEmail\nThe user's email address is a@example.invalid.\n</system-reminder>\n"})
}

fn frame() -> Value {
    json!({"type": "text", "text": format!("Use the terminal tool to run `echo alpha`.{}", " source pack".repeat(200))})
}

const ENV: &str = "Primary working directory: /tmp/ultron-claude-cwd-501\nPlatform: darwin";

fn marker() -> Value {
    json!({"type": "ephemeral", "ttl": "1h"})
}

fn call1() -> Value {
    json!([
        {"role": "user", "content": [reminder(), frame()]},
        {"role": "system", "content": [{"type": "text", "text": format!("{ENV}\n\nToday's date is 2026-09-30."), "cache_control": marker()}]},
    ])
}

fn call2() -> Value {
    json!([
        {"role": "user", "content": [frame()]},
        {"role": "system", "content": ENV},
        {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "mcp__ultron__rlm", "input": {"code": "echo alpha"}}]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": format!("alpha\n\n{}", reminder()["text"].as_str().unwrap())}]},
        {"role": "system", "content": [{"type": "text", "text": "Today's date is 2026-09-30.", "cache_control": marker()}]},
    ])
}

fn ft_wire(messages: &Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"model": "m", "messages": messages})).unwrap()
}

fn flat(payload: &[u8], until_marker: bool) -> Option<Vec<(String, String)>> {
    let body: Value = serde_json::from_slice(payload).unwrap();
    let mut blocks = Vec::new();
    for message in body["messages"].as_array().unwrap() {
        let content = match &message["content"] {
            Value::String(text) => vec![json!({"type": "text", "text": text})],
            other => other.as_array().unwrap().clone(),
        };
        for block in content {
            let mut plain = block.clone();
            plain.as_object_mut().unwrap().remove("cache_control");
            blocks.push((message["role"].as_str().unwrap().to_string(), plain.to_string()));
            if until_marker && block.get("cache_control").is_some() {
                return Some(blocks);
            }
        }
    }
    (!until_marker).then_some(blocks)
}

#[test]
fn ai_claude_subscription_first_request_caches_a_prefix_the_second_request_replays() {
    seal();
    let first = pin_message_breakpoint(&ft_wire(&call1()), Some(&[frame()]));
    let second = pin_message_breakpoint(
        &ft_wire(&call2()),
        Some(&[json!({"type": "tool_result", "tool_use_id": "t1", "content": "alpha"})]),
    );
    let prefix = flat(&first, true).unwrap();
    assert_eq!(flat(&second, false).unwrap()[..prefix.len()], prefix[..]);
    assert_eq!(prefix.last().unwrap(), &("user".to_string(), frame().to_string()));
}

#[test]
fn ai_claude_subscription_first_turn_keeps_every_block_and_moves_only_the_marker() {
    seal();
    let out: Value = serde_json::from_slice(&pin_message_breakpoint(&ft_wire(&call1()), Some(&[frame()]))).unwrap();
    let first_turn = out["messages"][0]["content"].as_array().unwrap();
    let plain: Vec<Value> = first_turn
        .iter()
        .map(|b| {
            let mut b = b.clone();
            b.as_object_mut().unwrap().remove("cache_control");
            b
        })
        .collect();
    assert_eq!(plain, vec![frame(), reminder()]);
    assert_eq!(first_turn[0]["cache_control"], marker());
    let count: usize = out["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| b.get("cache_control").is_some())
        .count();
    assert_eq!(count, 1);
    assert!(
        out["messages"][1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with(ENV)
    );
}

#[test]
fn ai_claude_subscription_first_turn_is_only_reordered_when_the_frame_is_unambiguous() {
    seal();
    let image = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}});
    let mut later = call2().as_array().unwrap()[..3].to_vec();
    later.push(json!({"role": "user", "content": [reminder(), frame()]}));
    let cases = [
        Value::Array(later),
        json!([{"role": "user", "content": [reminder(), frame(), frame()]}]),
        json!([{"role": "user", "content": [image, frame()]}]),
        json!([{"role": "user", "content": [reminder(), {"type": "text", "text": "rewritten"}]}]),
    ];
    for mut messages in cases {
        let last = messages.as_array_mut().unwrap().last_mut().unwrap();
        let block = last["content"].as_array_mut().unwrap().last_mut().unwrap();
        block["cache_control"] = marker();
        let raw = ft_wire(&messages);
        let out: Value = serde_json::from_slice(&pin_message_breakpoint(&raw, Some(&[frame()]))).unwrap();
        let strip = |v: &Value| {
            let mut v = v.clone();
            for m in v.as_array_mut().unwrap() {
                if let Some(content) = m["content"].as_array_mut() {
                    for b in content {
                        b.as_object_mut().unwrap().remove("cache_control");
                    }
                }
            }
            v
        };
        assert_eq!(strip(&out["messages"]), strip(&messages));
    }
}
