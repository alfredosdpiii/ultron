//! A run's automatic retry continues across a restart: the worker hands the session over while the run waits to
//! retry (`assistant.retry_wait`), and the next worker opening the session resumes the wait, then retries the same
//! step as the next attempt. The journal across both workers is the one TypeScript writes for a retry in one process
//! (fixtures/session_features.json "retry-then-answer", recorded from the TypeScript session worker): the same
//! operation states, one `assistant.retry_wait` with `nextAttempt: 2`, the retried attempt numbered 2 under the same
//! step, and RPC's `auto_retry_start` (attempt 2 of 4) then `auto_retry_end` (attempt 2, success). Nothing is
//! killed and no signal is sent: the first actor hands over through its own input, from the event that announced
//! the wait (so the hand-over always lands inside the wait).

mod p_session_support;
mod reopen_support;

use p_session_support::{HANG, in_order, overloaded, text};
use reopen_support::{Home, Worker};
use serde_json::{Value, json};
use session_core::core::{Input, Op};
use session_journal::run::OP_STATE;
use std::sync::mpsc::channel;
use std::sync::{Arc, LazyLock, Mutex};

static FIXTURE: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("../fixtures/session_features.json")).expect("fixture parses"));

/// RPC's retry events (rpc-events.ts) of a worker, without the delay (the wait here is longer than the recording's).
fn retry_events(w: &Worker) -> Vec<Value> {
    w.events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|e| match e["type"].as_str() {
            Some("retry_scheduled") => Some(json!({"type": "auto_retry_start", "attempt": e["attempt"],
                "maxAttempts": e["maxAttempts"], "errorMessage": e["errorMessage"]})),
            Some("retry_end") => {
                Some(json!({"type": "auto_retry_end", "success": e["success"], "attempt": e["attempt"]}))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn session_core_retry_continues_from_assistant_retry_wait_after_a_restart() {
    let expected = &FIXTURE["cases"]["retry-then-answer"];
    // A wait long enough that the hand-over (sent from the event announcing it) always lands inside it.
    let home = Home::new(Some(json!({"retry": {"baseDelayMs": 3000}})));
    let (handed_tx, handed) = channel();
    let handed_tx = Arc::new(Mutex::new(Some(handed_tx)));
    let first = home.open(
        in_order(vec![overloaded(), text("never sent by this worker")]),
        &[],
        Some(Box::new(move |event, actor| {
            if event["type"] == "retry_scheduled"
                && let Some(done) = handed_tx.lock().unwrap().take()
            {
                let _ = actor.send(Input::HandOver(done));
            }
        })),
    );
    let tx = first.tx.clone();
    std::thread::spawn(move || {
        let _ = session_core::core::call(
            &tx,
            Op::PromptAndWait {
                message: "hello".into(),
                images: Vec::new(),
            },
        );
    });
    handed
        .recv_timeout(HANG)
        .expect("the first worker hands over during the wait");
    let first_events = retry_events(&first);
    assert_eq!(first.requests().len(), 1);
    drop(first);
    // The journal stops in the wait: the error answer committed, attempt 2 next.
    let states = home.values(OP_STATE);
    let wait = states.last().unwrap();
    assert_eq!(wait["at"], "assistant.retry_wait", "{wait}");
    assert_eq!(wait["nextAttempt"], 2);
    assert_eq!(states.len(), 5);

    // The successor resumes the wait and retries the same step as attempt 2.
    let second = home.open(in_order(vec![text("Recovered.")]), &[], None);
    let ended = second.wait_event("run_end");
    assert_eq!(ended["status"], "completed", "{ended}");
    assert_eq!(second.requests().len(), 1);
    assert_eq!(second.call(Op::LastAssistantText).unwrap().unwrap(), expected["answer"]);
    let retry_start = second.wait_event("retry_start");
    assert_eq!(retry_start["attempt"], 2, "{retry_start}");

    // Across both workers: TypeScript's states and retry events for one retry.
    let states = home.values(OP_STATE);
    let ats: Vec<Value> = states.iter().map(|s| s["at"].clone()).collect();
    assert_eq!(json!(ats), expected["states"]);
    let mut events = first_events;
    events.extend(retry_events(&second));
    let mut recorded = expected["events"].clone();
    recorded[0].as_object_mut().unwrap().remove("delayMs");
    assert_eq!(json!(events), recorded);
    // The retried attempt is attempt 2 of the step that waited.
    let step = &states[4]["generationContext"]["stepId"];
    assert_eq!(states[5]["nextAttempt"], 2, "{}", states[5]);
    assert_eq!(&states[5]["generationContext"]["stepId"], step);
    assert_eq!(states[6]["attempt"], 2, "{}", states[6]);
    assert_eq!(&states[6]["generationContext"]["stepId"], step);
    // One error answer (committed with the wait), then the answer: nothing is committed twice.
    let errors = home
        .writes()
        .iter()
        .filter(|w| w.to_string().contains("\"stopReason\":\"error\"") && w["kind"] == "entry")
        .count();
    assert_eq!(errors, 1);
}
