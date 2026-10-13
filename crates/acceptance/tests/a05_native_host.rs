//! A05: the ultron-native-host.test.ts half of cancellation and durable task records, ported over the Rust
//! typed-agent host (`delegation::agents::Agents`: its journal, definition registry, strategies and workflows are
//! the real ones; the lane is faked as the TS `fakeLane` fakes it, as a script to the text at the completed tip).
//! A TS lane result that is not a completed prompt at its returned tip (a missing tip, a suspended prompt) is a lane
//! runner error in Rust: the run reports no assistant result, and the host must fail the task.

mod agents_support;

use agents_support::{Fixture, Memory, Options, Script, aborted, fixture, lock};
use kernel::Signal;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn lane(text: &'static str) -> Script {
    Arc::new(move |_, _, _, _| Ok(text.into()))
}

fn host(script: Script, store: Option<Memory>) -> Fixture {
    fixture(Options {
        script: Some(script),
        store,
        ..Options::default()
    })
}

/// Every key of `want` is in `got` with the same value (`toMatchObject`), recursively for objects.
fn assert_matches(got: &Value, want: &Value) {
    match (got, want) {
        (Value::Object(got_map), Value::Object(want_map)) => {
            for (key, value) in want_map {
                assert_matches(got_map.get(key).unwrap_or(&Value::Null), value);
            }
        }
        (Value::Array(got_items), Value::Array(want_items)) => {
            assert_eq!(got_items.len(), want_items.len(), "{got} vs {want}");
            for (g, w) in got_items.iter().zip(want_items) {
                assert_matches(g, w);
            }
        }
        _ => assert_eq!(got, want),
    }
}

#[test]
fn acceptance_a05_runs_an_explicit_background_job_through_the_same_durable_task_host() {
    let f = host(lane("background answer"), None);
    let started = f.call("background.start", json!({"prompt": "do work"})).unwrap();
    let id = started["id"].clone();
    assert_matches(
        &f.call("background.result", json!({"id": id})).unwrap(),
        &json!({"status": "succeeded", "value": "background answer", "verification": "unverified"}),
    );
    assert_matches(
        &f.call("background.list", json!({})).unwrap(),
        &json!([{"id": id, "definition": "background-job@1", "state": "completed"}]),
    );
    f.agents.close();
}

#[test]
fn acceptance_a05_runs_deterministic_agents_and_preserves_durable_task_records() {
    let f = host(lane("unused"), Some(Memory::default()));
    let result = f
        .call(
            "agents.invoke",
            json!({"definition": "identity@1", "input": {"answer": 42}}),
        )
        .unwrap();
    assert_matches(
        &result,
        &json!({"status": "succeeded", "value": {"answer": 42}, "verification": "unverified"}),
    );
    let status = f.call("agents.status", json!({})).unwrap();
    let last = status["tasks"].as_array().unwrap().last().unwrap().clone();
    assert_matches(&last, &json!({"state": "completed", "result": {"status": "succeeded"}}));
    f.agents.close();
}

#[test]
fn acceptance_a05_concurrent_idempotent_invokes_share_the_same_live_promise() {
    let prompts = Arc::new(AtomicUsize::new(0));
    let counted = prompts.clone();
    let script: Script = Arc::new(move |_, _, _, _| {
        counted.fetch_add(1, Ordering::SeqCst);
        Ok(r#"{"outcome":"no_findings","findings":[]}"#.into())
    });
    let f = host(script, None);
    let request = json!({"definition": "security-reviewer@1", "input": {"request": "review"}, "key": "same"});
    let (a, b) = (f.agents.clone(), f.agents.clone());
    let (ra, rb) = (request.clone(), request);
    let first = std::thread::spawn(move || a.handle("agents.invoke", ra, "main", None));
    let second = std::thread::spawn(move || b.handle("agents.invoke", rb, "main", None));
    let (first, second) = (first.join().unwrap().unwrap(), second.join().unwrap().unwrap());
    assert_eq!(first, second);
    assert_eq!(prompts.load(Ordering::SeqCst), 1);
    f.agents.close();
}

#[test]
fn acceptance_a05_cancellation_commits_a_terminal_result_without_waiting_for_a_stuck_lane() {
    // The lane never returns on its own: it ignores its signal, as the TS lane's prompt never settles.
    let stuck = Arc::new(Signal::new());
    let gate = stuck.clone();
    let script: Script = Arc::new(move |_, _, _, _| {
        while !gate.wait(std::time::Duration::from_secs(3600)) {}
        Ok("late".into())
    });
    let f = host(script, None);
    let spawned = f
        .call(
            "agents.spawn",
            json!({"definition": "security-reviewer@1", "input": {"request": "review"}}),
        )
        .unwrap();
    let id = spawned["id"].clone();
    assert_eq!(
        f.call("agents.cancel", json!({"id": id})).unwrap(),
        json!({"cancelled": true})
    );
    assert_matches(
        &f.call("agents.result", json!({"id": id})).unwrap(),
        &json!({"status": "cancelled", "verification": "unverified"}),
    );
    assert_matches(
        &f.call("agents.inspect", json!({"id": id})).unwrap(),
        &json!({"id": id, "state": "cancelled", "result": {"status": "cancelled"}}),
    );
    stuck.abort("test over");
    f.agents.close();
}

#[test]
fn acceptance_a05_parent_cancellation_before_admission_does_not_start_a_task() {
    let f = host(lane("unused"), None);
    let parent = Signal::new();
    parent.abort("parent stopped");
    let refused = f
        .agents
        .handle(
            "agents.spawn",
            json!({"definition": "identity@1", "input": 1}),
            "main",
            Some(&parent),
        )
        .unwrap_err();
    assert!(refused.contains("parent stopped"), "{refused}");
    assert_matches(&f.call("agents.tasks", json!({})).unwrap(), &json!({"tasks": []}));
    f.agents.close();
}

#[test]
fn acceptance_a05_timeout_has_a_bounded_range_and_commits_cancellation() {
    let f = host(Arc::new(|_, _, signal, _| aborted(signal)), None);
    let spawned = f
        .call(
            "agents.spawn",
            json!({"definition": "security-reviewer@1", "input": {"request": "review"}, "timeout_ms": 1}),
        )
        .unwrap();
    assert_matches(
        &f.call("agents.result", json!({"id": spawned["id"]})).unwrap(),
        &json!({"status": "cancelled"}),
    );
    let refused = f
        .call(
            "agents.spawn",
            json!({"definition": "identity@1", "input": 1, "timeout_ms": 3_600_001}),
        )
        .unwrap_err();
    assert!(refused.contains("timeout_ms"), "{refused}");
    f.agents.close();
}

#[test]
fn acceptance_a05_reopened_invoke_returns_the_durable_result_not_undefined() {
    let store = Memory::default();
    let request = json!({"definition": "identity@1", "input": {"value": 1}, "key": "reopen"});
    let first = host(lane("unused"), Some(store.clone()));
    first.call("agents.invoke", request.clone()).unwrap();
    let reopened = host(lane("unused"), Some(store));
    assert_matches(
        &reopened.call("agents.invoke", request).unwrap(),
        &json!({"status": "succeeded", "value": {"value": 1}}),
    );
    first.agents.close();
    reopened.agents.close();
}

#[test]
fn acceptance_a05_only_a_completed_prompt_at_its_returned_tip_can_succeed() {
    let missing_tip = host(
        Arc::new(|_, _, _, _| Err("Agent produced no assistant result at the completed tip".into())),
        None,
    );
    assert_matches(
        &missing_tip
            .call(
                "agents.invoke",
                json!({"definition": "security-reviewer@1", "input": {"request": "review"}}),
            )
            .unwrap(),
        &json!({"status": "failed"}),
    );
    let suspended = host(
        Arc::new(|_, _, _, _| Err("The prompt was suspended (deferred x), not completed".into())),
        None,
    );
    assert_matches(
        &suspended
            .call(
                "agents.invoke",
                json!({"definition": "security-reviewer@1", "input": {"request": "review"}}),
            )
            .unwrap(),
        &json!({"status": "failed"}),
    );
    missing_tip.agents.close();
    suspended.agents.close();
}

#[test]
fn acceptance_a05_validates_every_workflow_node_normalizes_missing_dependencies_and_skips_failed_dependencies() {
    let f = host(lane("unused"), None);
    let invalid = f
        .call(
            "workflows.run",
            json!({"nodes": [
                {"id": "first", "definition": "identity@1", "input": {"value": 1}},
                {"id": "second", "definition": "security-reviewer@1", "input": {}},
            ]}),
        )
        .unwrap_err();
    assert!(invalid.contains("reviewer input.request"), "{invalid}");
    let cycle = f
        .call(
            "workflows.run",
            json!({"nodes": [
                {"id": "a", "definition": "identity@1", "input": 1, "dependsOn": ["b"]},
                {"id": "b", "definition": "identity@1", "input": 2, "dependsOn": ["a"]},
            ]}),
        )
        .unwrap_err();
    assert!(cycle.contains("cycle"), "{cycle}");
    assert_matches(&f.call("agents.tasks", json!({})).unwrap(), &json!({"tasks": []}));
    assert!(lock(&f.prompts).is_empty());
    f.agents.close();
}

#[test]
fn acceptance_a05_rejects_malformed_reviewer_output_instead_of_claiming_success() {
    let f = host(
        lane(r#"{"outcome":"no_findings","findings":[{"file":"x.ts","line":0,"severity":"low","explanation":"bad"}]}"#),
        None,
    );
    let result = f
        .call(
            "agents.invoke",
            json!({"definition": "security-reviewer@1", "input": {"request": "review"}}),
        )
        .unwrap();
    assert_matches(&result, &json!({"status": "failed", "verification": "unverified"}));
    f.agents.close();
}
