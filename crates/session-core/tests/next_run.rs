//! The `nextRun` queue (TypeScript lane.ts `nextRun` / `selectAcceptedInbox`, agent-controller `nextRun`): a message
//! held until the next run starts. Ported from packages/agent/test/harness/runtime/accept.test.ts ("captures
//! next-run messages before request messages ...") with literal expectations: the next run places every next-run
//! message on the branch before its prompt, in admission order, in the admission transaction, deletes their
//! pending entries and empties the inbox; the run in progress never takes one; the message waits in the lane's inbox
//! past that run's end, and survives the worker (a successor's recovery does not start a run for it).

mod p_session_support;
mod reopen_support;

use p_session_support::{in_order, text, tool, user_texts};
use reopen_support::Home;
use serde_json::{Value, json};
use session_core::core::{EMPTY_QUEUED, Op, QueueKind};
use session_journal::run::PENDING_ENTRY;
use session_journal::session::LANE_STATE;

fn next_run(w: &reopen_support::Worker, text: &str) -> String {
    w.call(Op::Enqueue {
        kind: QueueKind::NextRun,
        message: text.into(),
        images: Vec::new(),
        entry_id: None,
    })
    .unwrap()
    .unwrap()
    .as_str()
    .unwrap()
    .to_string()
}

fn prompt(w: &reopen_support::Worker, text: &str) -> Value {
    w.call(Op::PromptAndWait {
        message: text.into(),
        images: Vec::new(),
    })
    .unwrap()
    .unwrap()
}

/// The user texts of a request, without the clock lines.
fn users(request: &Value) -> Vec<Value> {
    user_texts(request)
        .into_iter()
        .filter(|t| t != "[clock] <CLOCK>")
        .collect()
}

/// The entries the journal inserted, in order: `(id, parentId, text)`.
fn entries(home: &Home) -> Vec<(String, Value, String)> {
    home.writes()
        .into_iter()
        .filter(|w| w["kind"] == "entry" || w.get("entry").is_some() || w["type"] == "message")
        .filter_map(|w| {
            let entry = w.get("entry").unwrap_or(&w);
            let message = &entry["message"];
            let text = message["content"]
                .as_array()?
                .iter()
                .filter_map(|b| b["text"].as_str())
                .collect::<String>();
            Some((entry["id"].as_str()?.to_string(), entry["parentId"].clone(), text))
        })
        .collect()
}

#[test]
fn session_core_next_run_messages_are_placed_before_the_prompt_like_typescript() {
    let home = Home::new(None);
    let w = home.open(in_order(vec![text("ok")]), &[], None);
    let first = next_run(&w, "first");
    let second = next_run(&w, "second");
    let queued = w.events("queue_update").pop().unwrap();
    assert_eq!(
        queued["queues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|q| json!([q["kind"], q["entryId"]]))
            .collect::<Vec<_>>(),
        [json!(["nextRun", first]), json!(["nextRun", second])]
    );
    let lane = home.values(LANE_STATE).pop().unwrap();
    assert_eq!(
        lane["inbox"],
        json!([{"entryId": first, "kind": "nextRun"}, {"entryId": second, "kind": "nextRun"}])
    );
    let updates = w.events("queue_update").len();

    let ended = prompt(&w, "request");
    assert_eq!(ended["error"], Value::Null, "{ended}");

    // The request carries them before the prompt.
    assert_eq!(
        users(&w.requests()[0]),
        [json!("first"), json!("second"), json!("request")]
    );
    // On the branch in admission order: first, second, then the prompt, each the next one's parent.
    let placed = entries(&home);
    let ids: Vec<&str> = placed.iter().map(|(id, _, _)| id.as_str()).collect();
    let at = ids.iter().position(|id| *id == first).unwrap();
    assert_eq!(&ids[at..at + 2], [first.as_str(), second.as_str()]);
    assert_eq!(placed[at + 1].1, json!(first));
    assert_eq!(placed[at + 2].1, json!(second));
    assert_eq!(placed[at + 2].2, "request");
    // Their pending entries are deleted in the admission transaction; the inbox is empty.
    let deleted: Vec<Value> = home
        .writes()
        .into_iter()
        .filter(|w| w["namespace"] == PENDING_ENTRY && w["op"] == "delete")
        .map(|w| w["key"].clone())
        .collect();
    assert_eq!(deleted, [json!(first), json!(second)]);
    assert_eq!(home.values(LANE_STATE).pop().unwrap()["inbox"], json!([]));
    // The acceptance's one `queue_update` empties the queue.
    let after: Vec<Value> = w.events("queue_update").split_off(updates);
    assert_eq!(after[0]["queues"], json!([]));
    // The messages are announced before the prompt.
    let ends: Vec<String> = w
        .events("message_end")
        .iter()
        .filter(|e| e["message"]["role"] == "user")
        .map(|e| {
            p_session_support::user_texts(&json!({"messages": [e["message"]]}))[0]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(ends, ["first", "second", "request"]);
}

#[test]
fn session_core_next_run_waits_past_the_run_in_progress_and_survives_the_worker() {
    let home = Home::new(None);
    let go = home.cwd.join("go");
    let cell = "import os, time\nwhile not os.path.exists('go'):\n    time.sleep(0.01)\nprint('went')";
    let w = home.open(in_order(vec![tool("toolu_1", cell), text("A."), text("B.")]), &[], None);
    let tx = w.tx.clone();
    let (answer, answered) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = answer.send(session_core::core::call(
            &tx,
            Op::PromptAndWait {
                message: "work".into(),
                images: Vec::new(),
            },
        ));
    });
    w.wait_event("tool_start");
    let later = next_run(&w, "later");
    std::fs::write(&go, "").unwrap();
    let ended = answered
        .recv_timeout(p_session_support::HANG)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(ended["error"], Value::Null, "{ended}");
    // The run's requests never carried it, and its end leaves it in the inbox.
    let requests = w.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert!(!users(request).contains(&json!("later")), "{request}");
    }
    assert_eq!(
        home.values(LANE_STATE).pop().unwrap()["inbox"],
        json!([{"entryId": later, "kind": "nextRun"}])
    );
    let queued = w.events("queue_update").pop().unwrap();
    assert_eq!(queued["queues"][0]["kind"], "nextRun");
    assert_eq!(queued["queues"][0]["entryId"], json!(later));
    w.close();

    // A successor finds it queued; recovering starts no run for it.
    let w = home.open(in_order(vec![text("C.")]), &[], None);
    let recovered = w.call(Op::Recover).unwrap().unwrap();
    assert_eq!(recovered["resumed"], false, "{recovered}");
    assert!(w.requests().is_empty());
    let state = w.call(Op::State).unwrap().unwrap();
    assert_eq!(state["pendingMessageCount"], 1, "{state}");
    // The next run takes it before its prompt.
    prompt(&w, "again");
    assert_eq!(users(&w.requests()[0]), [json!("work"), json!("later"), json!("again")]);
    assert_eq!(home.values(LANE_STATE).pop().unwrap()["inbox"], json!([]));
}

#[test]
fn session_core_next_run_refuses_an_empty_message_like_typescript() {
    let home = Home::new(None);
    let w = home.open(in_order(Vec::new()), &[], None);
    let refused = w.call(Op::Enqueue {
        kind: QueueKind::NextRun,
        message: String::new(),
        images: Vec::new(),
        entry_id: None,
    });
    assert_eq!(refused, Err(EMPTY_QUEUED.to_string()));
    assert_eq!(EMPTY_QUEUED, "Queued input must contain text or an image");
    // `setQueueMode` has no next-run mode.
    assert_eq!(
        Op::from_json(&json!({"op": "setQueueMode", "kind": "nextRun", "mode": "all"})).unwrap_err(),
        "kind must be steer or followUp"
    );
    assert_eq!(
        Op::from_json(&json!({"op": "enqueue", "kind": "nextRun", "message": "m"})).unwrap(),
        Op::Enqueue {
            kind: QueueKind::NextRun,
            message: "m".into(),
            images: Vec::new(),
            entry_id: None
        }
    );
}
