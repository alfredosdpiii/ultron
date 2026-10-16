//! The agent controller's queue members through the real server and session worker (agent-controller-provider.ts
//! `queue`): `nextRun`, `steer` and `followUp` answer an `AgentQueueResponse` (`{accepted, entryId, error}`, a
//! refused message as `invalid_message`), and a message queued with `nextRun` while the session is idle starts no
//! run: the next prompt's run carries it before the prompt.

#[path = "daemon_common/mod.rs"]
mod common;

use client::runtime::Connection;
use common::{HANG_GUARD, Home, SERVER_ID, assert_no_processes, provider, start_server, text_reply};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn user_texts(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "user")
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|c| c["text"].as_str().map(str::to_string))
        .filter(|t| !t.starts_with("[clock]"))
        .collect()
}

#[test]
fn cli_daemon_next_run_waits_for_the_next_prompt_and_queue_members_answer_like_typescript() {
    let provider = provider(vec![text_reply("first answer")], Duration::ZERO);
    let home = Home::new(provider.port);
    let server = start_server(&home, SERVER_ID);
    let connection = Connection::connect(&server.socket, SERVER_ID).unwrap();
    let created = connection
        .call(
            connection.server_target(),
            "pi.session-management",
            "create",
            json!([{}]),
        )
        .unwrap()
        .unwrap();
    let id = created["sessionId"].as_str().unwrap().to_string();
    connection
        .call(
            connection.server_target(),
            "pi.session-management",
            "attach",
            json!([id]),
        )
        .unwrap();
    connection
        .wait_attachment(|a| a.is_some_and(|a| a.session_id == id))
        .unwrap();
    let session = connection.session_target().unwrap();
    let controller = |member: &str, request: Value| {
        connection
            .call(session.clone(), "pi.agent-controller", member, json!([request]))
            .unwrap()
            .unwrap()
    };

    let queued = controller("nextRun", json!({"message": "later", "images": null}));
    assert_eq!(queued["accepted"], true, "{queued}");
    assert_eq!(queued["error"], Value::Null);
    let entry = queued["entryId"].as_str().unwrap().to_string();
    assert!(!entry.is_empty());
    for member in ["nextRun", "steer", "followUp"] {
        assert_eq!(
            controller(member, json!({"message": "", "images": null})),
            json!({"accepted": false, "entryId": null, "error": {"code": "invalid_message",
                "message": "Queued input must contain text or an image"}}),
            "{member}"
        );
    }
    // Nothing runs for it.
    let state = connection
        .call(session.clone(), "ultron.session-ops", "op", json!([{"op": "state"}]))
        .unwrap()
        .unwrap();
    assert_eq!(state["pendingMessageCount"], 1, "{state}");
    assert!(provider.requests.lock().unwrap().is_empty());

    let answered = controller("prompt", json!({"message": "now", "images": null}));
    assert_eq!(answered["accepted"], true, "{answered}");
    let deadline = Instant::now() + HANG_GUARD;
    while provider.requests.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "no request");
        std::thread::sleep(Duration::from_millis(20));
    }
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(user_texts(&requests[0]), ["later", "now"]);
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
}
