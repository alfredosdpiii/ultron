//! Steering, follow-up and next-run messages queued while the session is idle, then a prompt: the run's admission
//! takes them by `selectAcceptedInbox` (every next-run message; steering and follow-ups by their queue modes) and
//! places them before the prompt in the order they were queued; what it leaves waits in the lane's inbox and the
//! run delivers it. Replays crates/session-core/fixtures/idle_queue.json, recorded from the TypeScript AgentHarness
//! lane by generate-idle-queue.mjs: the same provider requests, the same transcript, the same inbox after admission.

mod p_session_support;
mod reopen_support;

use p_session_support::{in_order, text, user_texts};
use reopen_support::Home;
use serde_json::{Value, json};
use session_core::core::{Op, QueueKind};
use session_journal::session::LANE_STATE;
use std::collections::HashMap;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/idle_queue.json")).unwrap()
}

/// The user texts of a request, without the clock lines.
fn users(request: &Value) -> Vec<Value> {
    user_texts(request)
        .into_iter()
        .filter(|t| t != "[clock] <CLOCK>")
        .collect()
}

fn message_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect(),
        _ => String::new(),
    }
}

fn replay(scenario: &Value) {
    let name = scenario["name"].as_str().unwrap();
    let home = Home::new(None);
    let answers: Vec<String> = scenario["answers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| text(a.as_str().unwrap()))
        .collect();
    let w = home.open(in_order(answers), &[], None);
    for (kind, key) in [
        (QueueKind::Steer, "steeringMode"),
        (QueueKind::FollowUp, "followUpMode"),
    ] {
        w.call(Op::SetQueueMode {
            kind,
            mode: scenario[key].as_str().unwrap().into(),
        })
        .unwrap();
    }
    let mut texts = HashMap::new();
    for queued in scenario["queued"].as_array().unwrap() {
        let kind = QueueKind::parse(queued[0].as_str().unwrap()).unwrap();
        let message = queued[1].as_str().unwrap().to_string();
        let entry = w
            .call(Op::Enqueue {
                kind,
                message: message.clone(),
                images: Vec::new(),
                entry_id: None,
            })
            .unwrap()
            .unwrap();
        texts.insert(entry.as_str().unwrap().to_string(), message);
    }
    // Nothing runs while the messages wait.
    assert!(w.requests().is_empty(), "{name}");
    let lanes_before = home.values(LANE_STATE).len();

    let ended = w
        .call(Op::PromptAndWait {
            message: "request".into(),
            images: Vec::new(),
        })
        .unwrap()
        .unwrap();
    assert_eq!(ended["error"], Value::Null, "{name}: {ended}");

    let requests: Vec<Vec<Value>> = w.requests().iter().map(users).collect();
    // Steering the admission left (one-at-a-time, a second steer) is delivered by the durable runtime at the run's
    // first boundary, after the first answer; TypeScript's agent loop takes it before the first request. So the
    // Rust run makes one request more: the first without it, then TypeScript's requests (docs: DECISIONS w3).
    let left_steering: Vec<Value> = scenario["inboxAfterAdmission"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item[0] == "steer")
        .map(|item| item[1].clone())
        .collect();
    let mut expected: Vec<Value> = scenario["requests"].as_array().unwrap().clone();
    if !left_steering.is_empty() {
        let first: Vec<Value> = expected[0]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| !left_steering.contains(t))
            .cloned()
            .collect();
        expected.insert(0, json!(first));
    }
    assert_eq!(json!(requests), json!(expected), "{name}: requests");
    // The admission's lane state (the first one naming the run) keeps what it did not take, in queue order.
    let admitted = home.values(LANE_STATE)[lanes_before..]
        .iter()
        .find(|state| !state["currentOperationId"].is_null())
        .cloned()
        .unwrap();
    let inbox: Vec<Value> = admitted["inbox"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| json!([item["kind"], texts[item["entryId"].as_str().unwrap()]]))
        .collect();
    assert_eq!(json!(inbox), scenario["inboxAfterAdmission"], "{name}: inbox");
    // The transcript, as the branch holds it (with left steering, it follows the first answer).
    if !left_steering.is_empty() {
        assert_eq!(home.values(LANE_STATE).pop().unwrap()["inbox"], json!([]), "{name}");
        w.close();
        return;
    }
    let transcript: Vec<String> = w
        .events("message_end")
        .iter()
        .map(|e| {
            format!(
                "{}: {}",
                e["message"]["role"].as_str().unwrap(),
                message_text(&e["message"])
            )
        })
        .collect();
    assert_eq!(json!(transcript), scenario["branch"], "{name}: transcript");
    assert_eq!(home.values(LANE_STATE).pop().unwrap()["inbox"], json!([]), "{name}");
    w.close();
}

#[test]
fn session_core_idle_queue_takes_every_queued_message_before_the_prompt_in_all_mode() {
    replay(&fixture()["scenarios"][0]);
}

#[test]
fn session_core_idle_queue_takes_the_first_of_each_kind_one_at_a_time_and_delivers_the_rest() {
    replay(&fixture()["scenarios"][1]);
}

#[test]
fn session_core_idle_queue_mixes_one_at_a_time_steering_with_all_follow_ups() {
    replay(&fixture()["scenarios"][2]);
}

#[test]
fn session_core_idle_queue_places_a_lone_follow_up_before_the_prompt() {
    replay(&fixture()["scenarios"][3]);
}
