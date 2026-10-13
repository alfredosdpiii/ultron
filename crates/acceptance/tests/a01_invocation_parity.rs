//! A01: one definition, invoked directly (`agents.invoke`), as a graph node (`workflows.run`), and from a real
//! Python RLM kernel (`await agents.invoke(...)` in a cell), shares one result contract and one durable task-record
//! contract; invalid output and invalid input fail identically on every path.
//! Port of packages/coding-agent/test/ultron-a01-invocation-parity.test.ts over the Rust typed-agent host
//! (`delegation::agents`) and the real runtime.py kernel, with the TS fixture's scripted lane.

mod agents_support;
mod common;

use agents_support::{Fixture, Options, definition, fixture, input_of, lock};
use common::{kernel_in, null_handler, run};
use kernel::{Kernel, Status};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::Arc;

/// RLM lane: doubles `n`; for negative `n` it answers with a schema-invalid result.
fn doubler() -> Fixture {
    let fixture = fixture(Options {
        script: Some(Arc::new(|_, prompt, _, _| {
            let n = input_of(prompt)["n"].as_i64().unwrap();
            Ok(if n < 0 {
                json!({ "doubled": "not a number" }).to_string()
            } else {
                json!({ "doubled": n * 2 }).to_string()
            })
        })),
        ..Options::default()
    });
    fixture
        .call(
            "agents.register",
            json!({ "definition": definition("doubler", "rlm", json!({})) }),
        )
        .unwrap();
    fixture
}

fn kernel(fixture: &Fixture) -> Kernel {
    kernel_in(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        Arc::new(fixture.agents.handler("main", null_handler())),
    )
}

fn direct(fixture: &Fixture, input: Value) -> Result<Value, String> {
    fixture.call("agents.invoke", json!({ "definition": "doubler@1", "input": input }))
}

fn graph(fixture: &Fixture, input: Value) -> Result<Value, String> {
    fixture
        .call(
            "workflows.run",
            json!({ "nodes": [{ "id": "only", "definition": "doubler@1", "input": input }] }),
        )
        .map(|out| out["only"].clone())
}

fn rlm(kernel: &Kernel, input: Value) -> Result<Value, String> {
    let code = format!(
        "import json\nresult = await agents.invoke(\"doubler@1\", json.loads({}))\nprint(json.dumps(result))",
        json!(input.to_string())
    );
    let result = run(kernel, &code);
    if result.status != Status::Ok {
        let error = result.error.unwrap();
        return Err(format!("{}: {}", error.ename, error.evalue));
    }
    Ok(serde_json::from_str(result.stdout.trim()).unwrap())
}

#[test]
fn acceptance_a01_the_same_definition_yields_the_same_validated_result_and_durable_task_record_on_all_three_paths() {
    let fixture = doubler();
    let kernel = kernel(&fixture);
    let expected = json!({ "status": "succeeded", "value": { "doubled": 42 }, "verification": "unverified" });
    assert_eq!(direct(&fixture, json!({ "n": 21 })).unwrap(), expected);
    assert_eq!(graph(&fixture, json!({ "n": 21 })).unwrap(), expected);
    assert_eq!(rlm(&kernel, json!({ "n": 21 })).unwrap(), expected);
    let tasks = fixture.journal();
    assert_eq!(tasks.len(), 3);
    for task in &tasks {
        assert_eq!(task["definition"], "doubler@1");
        assert_eq!(task["state"], "completed");
        assert_eq!(task["result"], expected);
        assert!(task.get("parentId").is_none());
    }
    // Same definition, input, model, and timeout: one fingerprint, three distinct task identities.
    let fingerprints: HashSet<String> = tasks.iter().map(|t| t["fingerprint"].to_string()).collect();
    let ids: HashSet<String> = tasks.iter().map(|t| t["id"].to_string()).collect();
    assert_eq!((fingerprints.len(), ids.len()), (1, 3));
    // Each path executed the identical definition prompt on its own lane.
    let prompts = lock(&fixture.prompts).clone();
    assert_eq!(prompts.len(), 3);
    assert_eq!(prompts.iter().map(|(_, p)| p).collect::<HashSet<_>>().len(), 1);
    assert_eq!(prompts.iter().map(|(l, _)| l).collect::<HashSet<_>>().len(), 3);
    kernel.shutdown();
    fixture.agents.close();
}

#[test]
fn acceptance_a01_schema_invalid_output_fails_identically_on_all_three_paths_and_is_never_recorded_as_success() {
    let fixture = doubler();
    let kernel = kernel(&fixture);
    let results = [
        direct(&fixture, json!({ "n": -1 })).unwrap(),
        graph(&fixture, json!({ "n": -1 })).unwrap(),
        rlm(&kernel, json!({ "n": -1 })).unwrap(),
    ];
    let expected = json!({ "status": "failed", "error": "doubler@1 output does not match its schema",
                           "verification": "unverified" });
    for result in &results {
        assert_eq!(result, &expected);
    }
    let tasks = fixture.journal();
    assert_eq!(
        tasks.iter().map(|t| t["state"].clone()).collect::<Vec<_>>(),
        vec![json!("failed"); 3]
    );
    for task in &tasks {
        assert_eq!(task["result"], expected);
    }
    kernel.shutdown();
    fixture.agents.close();
}

#[test]
fn acceptance_a01_invalid_input_is_rejected_before_admission_on_all_three_paths_with_the_same_error() {
    let fixture = doubler();
    let kernel = kernel(&fixture);
    let message = "doubler@1 input does not match its schema";
    assert_eq!(direct(&fixture, json!({ "n": "x" })).unwrap_err(), message);
    assert_eq!(graph(&fixture, json!({ "n": "x" })).unwrap_err(), message);
    assert_eq!(
        rlm(&kernel, json!({ "n": "x" })).unwrap_err(),
        format!("RuntimeError: {message}")
    );
    assert_eq!(fixture.journal(), Vec::<Value>::new());
    assert!(lock(&fixture.lane_calls).is_empty());
    kernel.shutdown();
    fixture.agents.close();
}
