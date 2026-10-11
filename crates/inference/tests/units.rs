//! Ported from packages/coding-agent/test/ultron-a47-a50-bounded-inference.test.ts (rlm.load, rlm.infer, A48, A49
//! single-frame cases) with literal expectations, plus this host's own choices (delegation, refusals, cancellation).

mod common;

use common::{Scripted, model};
use inference::Inference;
use kernel::{EchoHandler, HostHandler, Signal};
use serde_json::{Map, Value, json};
use std::sync::Arc;

fn setup(replies: Vec<Value>) -> (Inference, Arc<Scripted>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let transport = Arc::new(Scripted::default());
    *transport.replies.lock().unwrap() = replies;
    let fake = json!({"provider": "fake", "id": "frame-model", "api": "openai-completions", "contextWindow": 32000, "maxTokens": 8192});
    let handler = Inference::new(
        Arc::new(EchoHandler),
        model(&fake),
        Some("k".into()),
        transport.clone(),
        dir.path().join("context"),
    );
    (handler, transport, dir)
}

fn reply(text: &str) -> Value {
    json!({"text": text, "usage": {"input": 40, "output": 8, "cacheRead": 0}})
}

fn call(handler: &Inference, kind: &str, payload: Value) -> Result<Value, String> {
    let Value::Object(payload) = payload else {
        panic!("payload must be an object")
    };
    handler.handle(kind, payload, &Signal::new())
}

fn last_user(transport: &Scripted, index: usize) -> String {
    let bodies = transport.bodies.lock().unwrap();
    let messages = bodies[index]["messages"].as_array().unwrap();
    messages.last().unwrap()["content"].as_str().unwrap().to_string()
}

#[test]
fn inference_load_registers_content_by_digest_and_reopens_it() {
    let (handler, _, dir) = setup(vec![]);
    let hex = "c1c2e0f5a2a9b1b4a5f6e3d7c8b9a0f1e2d3c4b5a6978877665544332211aa00";
    let digest = format!("sha256:{hex}");
    let path = dir.path().join("context").join(hex).to_string_lossy().into_owned();
    let first = call(
        &handler,
        "rlm.load",
        json!({"digest": digest, "label": "notes.txt", "size": 11, "chars": 11}),
    );
    assert_eq!(first, Ok(json!({"path": path, "stored": false})));
    assert_eq!(
        call(&handler, "rlm.load", json!({"digest": digest})),
        Err(format!("Unknown context handle {digest}"))
    );
    std::fs::write(&path, "alpha\nbeta\n").unwrap();
    let again = call(
        &handler,
        "rlm.load",
        json!({"digest": digest, "label": "notes.txt", "size": 11}),
    )
    .unwrap();
    assert_eq!(again["stored"], true);
    let reopened = call(&handler, "rlm.load", json!({"digest": digest})).unwrap();
    assert_eq!(
        reopened,
        json!({"path": path, "stored": true, "label": "notes.txt", "size": 11})
    );
    let escape = call(
        &handler,
        "rlm.load",
        json!({"digest": "sha256:../../etc/passwd", "size": 1}),
    );
    assert_eq!(escape, Err("digest must be sha256:<64 hex>".to_string()));
}

#[test]
fn inference_frame_sees_only_its_task_and_views_under_its_own_system_prompt() {
    let (handler, transport, _dir) = setup(vec![reply("2")]);
    let body = "ok\nERROR one\nok\nERROR two\n";
    let observed = call(
        &handler,
        "rlm.infer",
        json!({
            "task": "Count the ERROR lines.",
            "context": [{"kind": "text", "label": "app.log[0:26]", "text": body}],
            "contract": {"type": "integer"},
        }),
    )
    .unwrap();
    assert_eq!(observed["status"], "complete");
    assert_eq!(observed["value"], 2);
    assert_eq!(observed["spent"], json!({"calls": 1, "tokens": 48}));
    let bodies = transport.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    let system = bodies[0]["messages"][0]["content"].as_str().unwrap();
    assert!(system.starts_with("You are an inference frame inside Ultron"));
    assert!(system.ends_with("You have no tools."));
    assert_eq!(bodies[0].get("tools"), None);
    drop(bodies);
    let message = last_user(&transport, 0);
    assert!(message.starts_with("Task:\nCount the ERROR lines.\n\n--- view 1: app.log[0:26] (26 chars) ---\n"));
    assert!(message.contains(body));
}

#[test]
fn inference_without_a_contract_returns_the_reply_text() {
    let (handler, _, _dir) = setup(vec![reply("a short answer")]);
    let observed = call(
        &handler,
        "rlm.infer",
        json!({"task": "Say something.", "context": [{"kind": "text", "label": "literal", "text": "x"}]}),
    );
    assert_eq!(observed.unwrap()["value"], "a short answer");
}

#[test]
fn inference_malformed_contract_is_refused_before_any_frame_runs() {
    let (handler, transport, _dir) = setup(vec![reply("1")]);
    let refused = call(
        &handler,
        "rlm.infer",
        json!({"task": "t", "context": [], "contract": {"type": "banana"}}),
    );
    assert_eq!(
        refused,
        Err("Invalid JSON schema at $.contract.type: contains an unknown type".to_string())
    );
    assert!(transport.bodies.lock().unwrap().is_empty());
}

#[test]
fn inference_malformed_reply_is_reasked_in_the_same_conversation() {
    let (handler, transport, _dir) = setup(vec![reply("I think the answer is {count: 2"), reply(r#"{"count": 2}"#)]);
    let contract = json!({"type": "object", "properties": {"count": {"type": "integer"}}, "required": ["count"], "additionalProperties": false});
    let observed = call(
        &handler,
        "rlm.infer",
        json!({"task": "Count.", "context": [], "contract": contract}),
    )
    .unwrap();
    assert_eq!(observed["status"], "complete");
    assert_eq!(observed["value"], json!({"count": 2}));
    assert_eq!(observed["spent"]["calls"], 2);
    assert_eq!(
        last_user(&transport, 1),
        format!(
            "Your reply does not satisfy the contract: the reply is not JSON.\nReply again with only a JSON value that satisfies this schema: {contract}"
        )
    );
    assert_eq!(
        transport.bodies.lock().unwrap()[1]["messages"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
}

#[test]
fn inference_exhausted_repairs_are_incomplete_with_spent_remaining_and_outputs() {
    let (handler, _, _dir) = setup(vec![reply("not json at all"), reply("not json at all")]);
    let observed = call(
        &handler,
        "rlm.infer",
        json!({
            "task": "Count.", "context": [], "contract": {"type": "integer"}, "max_repairs": 1,
            "budget": {"calls": 5, "tokens": 100000},
        }),
    )
    .unwrap();
    assert_eq!(observed["status"], "incomplete");
    assert_eq!(observed["reason"], "contract_unmet");
    assert_eq!(
        observed["detail"],
        "no valid answer after 2 attempt(s): the reply is not JSON"
    );
    assert_eq!(observed["spent"], json!({"calls": 2, "tokens": 96}));
    assert_eq!(observed["remaining"], json!({"calls": 3, "tokens": 99904, "depth": 1}));
    assert_eq!(observed["last_outputs"], json!(["not json at all", "not json at all"]));
    assert!(observed["trace_id"].as_str().unwrap().starts_with("frame-"));
}

#[test]
fn inference_running_out_of_calls_mid_repair_is_incomplete() {
    let (handler, transport, _dir) = setup(vec![reply("nope")]);
    let observed = call(
        &handler,
        "rlm.infer",
        json!({"task": "Count.", "contract": {"type": "integer"}, "budget": {"calls": 1}}),
    )
    .unwrap();
    assert_eq!(observed["reason"], "budget_exhausted");
    assert_eq!(observed["remaining"]["calls"], 0);
    assert_eq!(transport.bodies.lock().unwrap().len(), 1);
}

#[test]
fn inference_provider_failure_charges_the_call_but_no_tokens_and_cache_reads_are_discounted() {
    let (handler, _, _dir) = setup(vec![json!({"status": 500})]);
    let failed = call(
        &handler,
        "rlm.infer",
        json!({"task": "This one fails.", "budget": {"tokens": 50000}}),
    )
    .unwrap();
    assert_eq!(failed["status"], "error");
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .starts_with("frame run did not complete: failed ("),
        "{failed}"
    );
    assert_eq!(failed["spent"], json!({"calls": 1, "tokens": 0}));
    assert_eq!(failed["remaining"]["tokens"], 50000);
    let (handler, _, _dir) = setup(vec![
        json!({"text": "7", "usage": {"input": 100, "output": 10, "cacheRead": 1000}}),
    ]);
    let worked = call(
        &handler,
        "rlm.infer",
        json!({"task": "This one works.", "contract": {"type": "integer"}}),
    )
    .unwrap();
    assert_eq!(worked["value"], 7);
    assert_eq!(worked["spent"], json!({"calls": 1, "tokens": 210}));
}

#[test]
fn inference_delegates_other_requests_to_the_inner_handler() {
    let (handler, transport, _dir) = setup(vec![]);
    let echoed = call(&handler, "bash", json!({"command": "true"}));
    assert_eq!(echoed, Ok(json!({"type": "bash", "payload": {"command": "true"}})));
    assert_eq!(
        call(&handler, "test.fail", json!({})),
        Err("refused test.fail".to_string())
    );
    assert!(transport.bodies.lock().unwrap().is_empty());
}

#[test]
fn inference_refuses_what_this_host_cannot_run() {
    let (handler, transport, _dir) = setup(vec![]);
    let deep = call(&handler, "rlm.infer", json!({"task": "t", "budget": {"depth": 2}}));
    assert_eq!(
        deep,
        Err("budget depth 2 needs frames that run code (the rlm tool), which this host does not run yet".to_string())
    );
    let pattern = call(
        &handler,
        "rlm.infer",
        json!({"task": "t", "contract": {"type": "string", "pattern": "^x"}}),
    );
    assert_eq!(
        pattern,
        Err("$.contract.pattern: this contract keyword is not checked by this host yet".to_string())
    );
    let other = call(&handler, "rlm.infer", json!({"task": "t", "model": "other/model"})).unwrap();
    assert_eq!(
        other["error"],
        "Admission refused: model other/model is not available to this host"
    );
    assert_eq!(other["spent"], json!({"calls": 0, "tokens": 0}));
    assert!(transport.bodies.lock().unwrap().is_empty());
}

#[test]
fn inference_aborted_cell_runs_no_frame_and_holds_nothing() {
    let (handler, transport, _dir) = setup(vec![reply("unused")]);
    let signal = Signal::new();
    signal.abort("stop");
    let mut payload = Map::new();
    payload.insert("task".into(), "t".into());
    payload.insert("budget".into(), json!({"calls": 1, "tokens": 1000}));
    let observed = handler.handle("rlm.infer", payload, &signal).unwrap();
    assert_eq!(observed["error"], "cancelled: the calling cell was aborted");
    assert_eq!(observed["remaining"], json!({"calls": 1, "tokens": 1000, "depth": 1}));
    assert_eq!(transport.bodies.lock().unwrap().len(), 0);
}
