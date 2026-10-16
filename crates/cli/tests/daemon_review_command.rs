//! `/review` typed into any client is expanded where the client's text enters the worker (agent-controller-provider.ts
//! `toTextPrompt`): the agent controller's `prompt`, `steer`, `followUp` and `nextRun`, and the Rust session-ops
//! `op` the TUI and RPC clients send. Through the real server and session worker; the expected texts are the
//! TypeScript controller's (crates/session-core/fixtures/review_command.json). Messages queued while the session is
//! idle reach the next prompt's request before the prompt, in the order they were queued (lane.ts
//! `selectAcceptedInbox`).

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

/// The message the TypeScript controller's `operation` hands its lane for `text`.
fn typescript(operation: &str, text: &str) -> String {
    let fixture: Value = serde_json::from_str(include_str!("../../session-core/fixtures/review_command.json")).unwrap();
    fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["operation"] == operation && c["text"] == text)
        .unwrap()["message"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn cli_daemon_review_command_expands_for_every_client_entry_like_typescript() {
    let provider = provider(vec![text_reply("reviewed")], Duration::ZERO);
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
    let controller = |member: &str, text: &str| {
        connection
            .call(
                session.clone(),
                "pi.agent-controller",
                member,
                json!([{"message": text, "images": null}]),
            )
            .unwrap()
            .unwrap()
    };

    assert_eq!(controller("steer", "/review main")["accepted"], true);
    assert_eq!(controller("followUp", "plain text")["accepted"], true);
    // The TUI's and RPC clients' command set.
    connection
        .call(
            session.clone(),
            "ultron.session-ops",
            "op",
            json!([{"op": "enqueue", "kind": "nextRun", "message": "/review 12 --post"}]),
        )
        .unwrap();
    let answered = controller("prompt", "/review");
    assert_eq!(answered["accepted"], true, "{answered}");
    let deadline = Instant::now() + HANG_GUARD;
    while provider.requests.lock().unwrap().is_empty() {
        assert!(Instant::now() < deadline, "no request");
        std::thread::sleep(Duration::from_millis(20));
    }
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(
        user_texts(&requests[0]),
        [
            typescript("steer", "/review main"),
            typescript("followUp", "plain text"),
            typescript("nextRun", "/review 12 --post"),
            typescript("prompt", "/review"),
        ]
    );
    connection.disconnect();
    drop(server);
    assert_no_processes(&home.root, HANG_GUARD);
}
