//! Replays fixtures/session_features.json, recorded by fixtures/session_features.mjs from the TypeScript session
//! worker in RPC mode, against the Rust session actor with the same scripted Anthropic replies:
//! - a run retries a transient provider error (`retry_scheduled` / `retry_end` as RPC's `auto_retry_start` /
//!   `auto_retry_end`, the operation's states with `assistant.retry_wait`), gives up after `retry.maxRetries`, and does
//!   not retry with `set_auto_retry false` or an error that is not transient;
//! - `rlm.find_models` from a cell answers the available models;
//! - `view_image` attaches the image to the cell's tool result, which the next request carries;
//! - the image budget (`ULTRON_CONTEXT_IMAGES`) replaces the oldest tool-result images of a request by a note;
//! - the tool-round nudge (`ULTRON_TOOL_ROUNDS_NUDGE`) steers the root after its third tool round.

mod p_session_support;

use base64::Engine;
use p_session_support::{Setup, actor, in_order, invalid, overloaded, text, tool, tool_results, user_texts};
use serde_json::{Value, json};
use session_core::core::Op;
use std::sync::LazyLock;

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/session_features.json")).expect("fixture parses"));

fn case(name: &str) -> &'static Value {
    &FIXTURE["cases"][name]
}

/// The files a case wrote in the working directory, as recorded.
fn files(name: &str) -> Vec<(&'static str, Vec<u8>)> {
    case(name)["files"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(file, data)| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data.as_str().unwrap())
                .unwrap();
            (&*Box::leak(file.clone().into_boxed_str()), bytes)
        })
        .collect()
}

/// The actor's retry events as TypeScript's RPC mode maps them (rpc-events.ts).
fn rpc_retry_events(a: &p_session_support::Actor) -> Vec<Value> {
    a.events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e["type"].as_str() {
            Some("retry_scheduled") => Some(json!({
                "type": "auto_retry_start", "attempt": e["attempt"], "maxAttempts": e["maxAttempts"],
                "delayMs": e["delayMs"], "errorMessage": e["errorMessage"],
            })),
            Some("retry_end") => {
                let mut end = json!({"type": "auto_retry_end", "success": e["success"], "attempt": e["attempt"]});
                if let Some(error) = e.get("finalError") {
                    end["finalError"] = error.clone();
                }
                Some(end)
            }
            _ => None,
        })
        .collect()
}

/// The ids of a retry-wait state become placeholders (checked separately), its time 0.
fn scrub_wait(wait: &Value) -> Value {
    let mut wait = wait.clone();
    wait["notBefore"] = json!(0);
    wait["latestAssistantEntryId"] = json!("<answer>");
    wait["generationContext"]["stepId"] = json!("<step>");
    wait["generationContext"]["triggerEntryId"] = json!("<prompt>");
    wait
}

/// One retry case: TypeScript's RPC retry events, operation states and retry waits.
fn retry_case(name: &str, replies: Vec<String>, set_off: bool) {
    let expected = case(name);
    let settings = match name {
        "retry-exhausted" => json!({"retry": {"maxRetries": 1, "baseDelayMs": 1}}),
        _ => json!({"retry": {"baseDelayMs": 1}}),
    };
    let a = actor(
        in_order(replies),
        Setup {
            settings: Some(settings),
            ..Setup::default()
        },
    );
    if set_off {
        a.call(Op::SetAutoRetry(false)).unwrap();
    }
    a.prompt("hello");
    assert_eq!(json!(rpc_retry_events(&a)), expected["events"], "{name}: events");
    let states = a.op_states();
    let ats: Vec<Value> = states.iter().map(|s| s["at"].clone()).collect();
    assert_eq!(json!(ats), expected["states"], "{name}: states");
    let waits: Vec<&Value> = states.iter().filter(|s| s["at"] == "assistant.retry_wait").collect();
    let expected_waits = expected["retryWaits"].as_array().unwrap();
    assert_eq!(waits.len(), expected_waits.len(), "{name}: waits");
    for (wait, expected) in waits.iter().zip(expected_waits) {
        assert_eq!(scrub_wait(wait), scrub_wait(expected), "{name}: retry wait");
        // The same step is retried: the attempt before and after the wait share its step id.
        let steps: Vec<&Value> = states
            .iter()
            .filter(|s| s["at"] == "assistant.effect_pending")
            .map(|s| &s["generationContext"]["stepId"])
            .collect();
        assert!(
            steps.iter().all(|s| **s == wait["generationContext"]["stepId"]),
            "{steps:?}"
        );
        // The answer it waits after is the error that committed.
        let writes = a.writes();
        assert!(
            writes.iter().any(
                |w| w.to_string().contains(wait["latestAssistantEntryId"].as_str().unwrap())
                    && w.to_string().contains("\"stopReason\":\"error\"")
            ),
            "{name}: the waited-after answer is the error entry"
        );
    }
    if let Some(requests) = expected["requests"].as_u64() {
        assert_eq!(a.requests().len() as u64, requests, "{name}: requests");
    }
    if let Some(answer) = expected.get("answer") {
        assert_eq!(&a.last_text(), answer);
    }
}

#[test]
fn session_core_retry_answers_after_a_transient_error_like_typescript() {
    retry_case("retry-then-answer", vec![overloaded(), text("Recovered.")], false);
}

#[test]
fn session_core_retry_gives_up_after_max_retries_like_typescript() {
    retry_case("retry-exhausted", vec![overloaded(), overloaded()], false);
}

#[test]
fn session_core_retry_is_off_after_set_auto_retry_false_like_typescript() {
    retry_case("auto-retry-off", vec![overloaded()], true);
}

#[test]
fn session_core_retry_needs_a_transient_error_like_typescript() {
    retry_case("not-retryable", vec![invalid()], false);
}

/// `abort_retry` while the run waits to retry aborts the run; nothing is retried.
#[test]
fn session_core_abort_retry_aborts_a_run_waiting_to_retry() {
    let a = actor(
        in_order(vec![overloaded(), text("never sent")]),
        Setup {
            settings: Some(json!({"retry": {"baseDelayMs": 600000}})),
            ..Setup::default()
        },
    );
    let done = {
        let tx = a.tx.clone();
        let (answer, answered) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = answer.send(session_core::core::call(
                &tx,
                Op::PromptAndWait {
                    message: "hello".into(),
                    images: Vec::new(),
                },
            ));
        });
        answered
    };
    let scheduled = a.wait_event("retry_scheduled");
    // `retryDelayMs` caps the wait at `maxAgentDelayMs`.
    assert_eq!(scheduled["delayMs"], 60000);
    a.call(Op::AbortRetry).unwrap();
    let ended = done.recv_timeout(p_session_support::HANG).unwrap().unwrap().unwrap();
    assert_eq!(ended["error"], Value::Null, "{ended}");
    assert_eq!(a.events("run_end").pop().unwrap()["status"], "aborted");
    assert_eq!(a.requests().len(), 1);
    assert!(a.events("retry_start").is_empty());
}

#[test]
fn session_core_find_models_answers_the_available_models_like_typescript() {
    let code =
        "import json\nprint(json.dumps(await rlm.host_request('rlm.find_models', {})))\nprint(await rlm.find_models())";
    let a = actor(in_order(vec![tool("toolu_1", code), text("ok")]), Setup::default());
    a.prompt("models");
    assert_eq!(
        json!(tool_results(&a.requests()[1])),
        case("find-models")["toolResults"]
    );
}

#[test]
fn session_core_view_image_attaches_the_image_to_the_tool_result_like_typescript() {
    let a = actor(
        in_order(vec![
            tool("toolu_1", "print(await view_image('chart.png'))"),
            text("seen"),
        ]),
        Setup {
            files: files("view-image"),
            ..Setup::default()
        },
    );
    a.prompt("look at chart.png");
    assert_eq!(json!(tool_results(&a.requests()[1])), case("view-image")["toolResults"]);
    // The transcript keeps Pi's shape: the text, then the image.
    let messages = a.call(Op::Messages).unwrap().unwrap();
    let result = messages
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "toolResult")
        .unwrap()
        .clone();
    assert_eq!(
        result["content"][1],
        json!({"type": "image", "data": case("view-image")["png"], "mimeType": "image/png"})
    );
}

#[test]
fn session_core_image_budget_replaces_the_oldest_images_like_typescript() {
    let a = actor(
        in_order(vec![
            tool("toolu_1", "print(await view_image('a.png'))"),
            tool("toolu_2", "print(await view_image('b.png'))"),
            tool("toolu_3", "print(await view_image('c.png'))"),
            text("seen all"),
        ]),
        Setup {
            vars: vec![("ULTRON_CONTEXT_IMAGES", "2".into())],
            files: files("image-budget"),
            ..Setup::default()
        },
    );
    a.prompt("look at the images");
    assert_eq!(
        json!(tool_results(&a.requests()[3])),
        case("image-budget")["toolResults"]
    );
}

#[test]
fn session_core_tool_round_nudge_steers_the_root_to_answer_like_typescript() {
    let a = actor(
        in_order(vec![
            tool("toolu_1", "print('round 1')"),
            tool("toolu_2", "print('round 2')"),
            tool("toolu_3", "print('round 3')"),
            text("ANSWERED AFTER NUDGE"),
        ]),
        Setup {
            vars: vec![("ULTRON_TOOL_ROUNDS_NUDGE", "3".into())],
            ..Setup::default()
        },
    );
    a.prompt("research something open-ended");
    let expected = case("tool-round-nudge");
    let texts: Vec<Value> = a.requests().iter().map(|r| json!(user_texts(r))).collect();
    assert_eq!(json!(texts), expected["userTexts"]);
    assert_eq!(a.last_text(), expected["answer"]);
}
