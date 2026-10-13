//! A42: human child intervention and subtree stop preserve unrelated work.
//! Ported from packages/coding-agent/test/ultron-a42-intervention-subtree-stop.test.ts (both cases) with literal
//! expectations. The TS cases run `NativeRlmHost` over controlled fake lanes (a prompt that blocks until the test
//! finishes it, a steer log, an abort count). Here the task runtime is the real `harness` task supervisor (admission,
//! `agents.inspect` / `agents.status` / `agents.cancel` / `agents.result`, the cancellation cascade with its error
//! texts, the abort hook of each task whose work is stopped, the durable journal), and the modules are the real
//! `kernel_apis` family and progress modules, all through `g3_support::Host`; the lanes are the same controlled fakes
//! (`Tasks::finish` is the prompt returning, the abort hook counts aborts).

mod g3_support;

use g3_support::{Host, JournalStore, Tasks, rejects};
use kernel_apis::family::{FamilyModule, FamilyOptions};
use kernel_apis::{ApiModule, MemoryDocumentStore, ProgressModule};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

fn lane_of(id: &str) -> String {
    format!("ultron.rlm-child.{id}")
}

fn spawn(host: &Host, prompt: &str, lane: Option<&str>) -> String {
    let payload = json!({"definition": "rlm-child@1", "input": {"prompt": prompt}});
    let task = match lane {
        Some(lane) => host.call_in(lane, "agents.spawn", payload),
        None => host.call("agents.spawn", payload),
    };
    task.unwrap()["id"].as_str().unwrap().to_string()
}

fn subset(actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Object(a), Value::Object(e)) => {
            for (key, value) in e {
                subset(a.get(key).unwrap_or(&Value::Null), value);
            }
        }
        (Value::Array(a), Value::Array(e)) => {
            assert_eq!(a.len(), e.len(), "{actual} vs {expected}");
            for (a, e) in a.iter().zip(e) {
                subset(a, e);
            }
        }
        _ => assert_eq!(actual, expected),
    }
}

#[test]
fn acceptance_a42_inspect_correct_and_stop_one_childs_subtree_while_the_sibling_continues_and_completes() {
    let journal = JournalStore::default();
    let tasks = Tasks::new(journal.clone(), Arc::new(AtomicI64::new(0)));
    let host = Host::new(
        tasks.clone(),
        vec![
            Arc::new(FamilyModule::new(
                Arc::new(MemoryDocumentStore::default()),
                FamilyOptions::default(),
            )) as Arc<dyn ApiModule>,
            Arc::new(ProgressModule::new(Arc::new(MemoryDocumentStore::default()))),
        ],
    );

    // Two children of the root; child A delegates to a grandchild, which delegates further.
    let a = spawn(&host, "child A: audit", None);
    let b = spawn(&host, "child B: summarize", None);
    let grandchild = spawn(&host, "grandchild of A", Some(&lane_of(&a)));
    let great = spawn(&host, "great-grandchild of A", Some(&lane_of(&grandchild)));

    // Both children report progress from their own lanes.
    host.call_in(
        &lane_of(&a),
        "progress.report",
        json!({"summary": "A scanned 3 files", "evidence": [{"kind": "file", "ref": "a.ts"}]}),
    )
    .unwrap();
    host.call_in(
        &lane_of(&b),
        "progress.report",
        json!({"summary": "B read input", "evidence": [{"kind": "file", "ref": "b.md"}]}),
    )
    .unwrap();

    // Inspect child A: bounded task state plus its progress history, with no model call.
    assert_eq!(
        host.call("agents.inspect", json!({"id": a})).unwrap(),
        json!({"id": a, "definition": "rlm-child@1", "state": "running"})
    );
    let history = host.call("progress.history", json!({"task_id": a})).unwrap();
    subset(
        &history["receipts"],
        &json!([{"summary": "A scanned 3 files", "reporter_task_id": a}]),
    );
    subset(
        &host.call("agents.status", json!({})).unwrap(),
        &json!({"tasks": [
            {"id": a, "state": "running"},
            {"id": b, "state": "running"},
            {"id": grandchild, "parentId": a, "state": "running"},
            {"id": great, "parentId": grandchild, "state": "running"},
        ]}),
    );

    // Human correction: a steering message routed to child A only, delivered into its live lane.
    let sent = host
        .call(
            "agent_message.send",
            json!({"message": "Stop auditing tests; focus on src/", "receiver_role": "child", "receiver_id": a, "steer": true}),
        )
        .unwrap();
    subset(&sent, &json!({"state": "delivered", "steered": true}));
    let steered = tasks.probe(&a).steered;
    assert_eq!(steered.len(), 1);
    assert!(steered[0].contains("Verified sender: the root agent"));
    assert!(steered[0].contains(&json!("Stop auditing tests; focus on src/").to_string()));
    assert!(tasks.probe(&b).steered.is_empty());

    // Stop child A's subtree.
    assert_eq!(
        host.call("agents.cancel", json!({"id": a})).unwrap(),
        json!({"cancelled": true})
    );
    for id in [&a, &grandchild, &great] {
        subset(
            &host.call("agents.result", json!({"id": id})).unwrap(),
            &json!({"status": "cancelled", "verification": "unverified"}),
        );
        assert_eq!(tasks.probe(id).aborts, 1, "{id}");
    }
    assert_eq!(tasks.probe(&b).aborts, 0);
    subset(
        &host.call("agents.inspect", json!({"id": b})).unwrap(),
        &json!({"state": "running"}),
    );

    // A stopped child can no longer receive corrections; the sibling can.
    rejects(
        host.call(
            "agent_message.send",
            json!({"message": "late", "receiver_role": "child", "receiver_id": a, "steer": true}),
        ),
        "already finished",
    );
    host.call(
        "agent_message.send",
        json!({"message": "keep going", "receiver_role": "child", "receiver_id": b}),
    )
    .unwrap();

    // The unrelated child keeps working, reports new evidence, and completes.
    host.call_in(
        &lane_of(&b),
        "progress.report",
        json!({"summary": "B drafted", "evidence": [{"kind": "file", "ref": "summary.md"}]}),
    )
    .unwrap();
    let inbox = host.call_in(&lane_of(&b), "agent_message.receive", json!({})).unwrap();
    let contents: Vec<_> = inbox["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["content"].clone())
        .collect();
    assert_eq!(contents, vec![json!("keep going")]);
    tasks.finish(&b, "B summary");
    subset(
        &host.call("agents.result", json!({"id": b})).unwrap(),
        &json!({"status": "succeeded", "value": "B summary"}),
    );

    // Evidence readback from durable records shows both outcomes.
    let status = host.call("agents.status", json!({})).unwrap();
    let by_id: HashMap<String, Value> = status["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["id"].as_str().unwrap().to_string(), t.clone()))
        .collect();
    subset(
        &by_id[&a],
        &json!({"state": "cancelled", "result": {"error": "Ultron task cancelled"}}),
    );
    subset(
        &by_id[&grandchild],
        &json!({"state": "cancelled", "result": {"error": format!("Ancestor {a} cancelled: Ultron task cancelled")}}),
    );
    subset(&by_id[&great], &json!({"state": "cancelled"}));
    subset(
        &by_id[&b],
        &json!({"state": "completed", "result": {"status": "succeeded"}}),
    );
    let stored = journal.0.lock().unwrap().clone().unwrap();
    let states: HashMap<String, Value> = stored["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["id"].as_str().unwrap().to_string(), t["state"].clone()))
        .collect();
    let expected: HashMap<String, Value> = [
        (a.clone(), json!("cancelled")),
        (b.clone(), json!("completed")),
        (grandchild.clone(), json!("cancelled")),
        (great.clone(), json!("cancelled")),
    ]
    .into_iter()
    .collect();
    assert_eq!(states, expected);
    let messages = host.call("agent_message.list", json!({})).unwrap();
    subset(
        &messages["sent"],
        &json!([
            {"receiver_id": a, "state": "delivered", "delivered_via": "steer"},
            {"receiver_id": b, "state": "delivered", "delivered_via": "receive"},
        ]),
    );
    let b_history = host.call("progress.history", json!({"task_id": b})).unwrap();
    let summaries: Vec<_> = b_history["receipts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["summary"].clone())
        .collect();
    assert_eq!(summaries, vec![json!("B read input"), json!("B drafted")]);
    subset(
        &host.call("progress.assess", json!({"task_id": a})).unwrap(),
        &json!({"classification": "finished", "result_status": "cancelled"}),
    );
    tasks.close();
}

#[test]
fn acceptance_a42_stopping_a_finished_child_still_stops_its_live_descendants_and_leaves_siblings_alone() {
    let tasks = Tasks::new(JournalStore::default(), Arc::new(AtomicI64::new(0)));
    let host = Host::new(tasks.clone(), Vec::new());
    let parent = spawn(&host, "parent", None);
    let sibling = spawn(&host, "sibling", None);
    let orphanable = spawn(&host, "background grandchild", Some(&lane_of(&parent)));
    tasks.finish(&parent, "parent done");
    subset(
        &host.call("agents.result", json!({"id": parent})).unwrap(),
        &json!({"status": "succeeded"}),
    );
    subset(
        &host.call("agents.inspect", json!({"id": orphanable})).unwrap(),
        &json!({"state": "running"}),
    );

    // The parent's own terminal result is not rewritten, but its live subtree is stopped.
    assert_eq!(
        host.call("agents.cancel", json!({"id": parent})).unwrap(),
        json!({"cancelled": false})
    );
    subset(
        &host.call("agents.result", json!({"id": parent})).unwrap(),
        &json!({"status": "succeeded"}),
    );
    subset(
        &host.call("agents.result", json!({"id": orphanable})).unwrap(),
        &json!({"status": "cancelled"}),
    );
    assert_eq!(tasks.probe(&orphanable).aborts, 1);
    assert_eq!(tasks.probe(&parent).aborts, 0);
    subset(
        &host.call("agents.inspect", json!({"id": sibling})).unwrap(),
        &json!({"state": "running"}),
    );
    tasks.finish(&sibling, "sibling done");
    subset(
        &host.call("agents.result", json!({"id": sibling})).unwrap(),
        &json!({"status": "succeeded", "value": "sibling done"}),
    );
    tasks.close();
}
