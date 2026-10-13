//! A12: the workflow graph validates its complete topology before any effect; routes, skips, fan-in and cycles are
//! explicit.
//! Port of packages/coding-agent/test/ultron-a12-graph-contract.test.ts over the Rust typed-agent host
//! (`workflows.run`), its definition registry and usage ledger, with the TS fixture's deterministic adapter.

mod agents_support;

use agents_support::{Fixture, Memory, Options, adapter, definition, fixture, lock, wait_for};
use delegation::definitions::definition_hash;
use delegation::usage::{UsageLedger, UsageLimits};
use kernel::Signal;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Graph {
    fixture: Fixture,
    adapter_calls: Arc<Mutex<Vec<(String, Value)>>>,
}

fn graph(usage: Option<UsageLedger>, hold: Option<Signal>) -> Graph {
    let adapter_calls: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let calls = adapter_calls.clone();
    let fixture = fixture(Options {
        usage,
        deterministic: Some(adapter(move |id, input, _, _| {
            lock(&calls).push((id.to_string(), input.clone()));
            if id == "doubler" {
                return Ok(json!({ "doubled": input["n"].as_i64().unwrap() * 2 }));
            }
            if input["hold"] == true
                && let Some(hold) = &hold
            {
                while !hold.wait(Duration::from_millis(5)) {}
            }
            if input["fail"] == true {
                return Err("step crashed".into());
            }
            Ok(input.clone())
        })),
        ..Options::default()
    });
    let any = json!({ "inputSchema": {}, "outputSchema": {} });
    fixture
        .call(
            "agents.register",
            json!({ "definition": definition("step", "deterministic", any) }),
        )
        .unwrap();
    fixture
        .call(
            "agents.register",
            json!({ "definition": definition("doubler", "deterministic", json!({})) }),
        )
        .unwrap();
    Graph { fixture, adapter_calls }
}

fn run(fixture: &Fixture, nodes: Value) -> Result<Value, String> {
    fixture.call("workflows.run", json!({ "nodes": nodes }))
}

#[test]
fn acceptance_a12_every_validation_error_is_raised_before_any_task_is_admitted_or_any_node_runs() {
    let g = graph(None, None);
    let valid = json!({ "id": "first", "definition": "step@1", "input": { "v": 1 } });
    let cases = [
        (
            "cycle",
            json!([valid, { "id": "a", "definition": "step@1", "input": 1, "dependsOn": ["b"] },
                          { "id": "b", "definition": "step@1", "input": 2, "dependsOn": ["a"] }]),
        ),
        (
            "cycle",
            json!([valid, { "id": "self", "definition": "step@1", "input": 1, "dependsOn": ["self"] }]),
        ),
        (
            "Unknown workflow dependency",
            json!([valid, { "id": "a", "definition": "step@1", "input": 1, "dependsOn": ["ghost"] }]),
        ),
        ("Duplicate workflow node ID", json!([valid, valid])),
        (
            "doubler@1 input does not match its schema",
            json!([valid, { "id": "a", "definition": "doubler@1", "input": { "n": "x" } }]),
        ),
        (
            "Unknown Ultron agent definition",
            json!([valid, { "id": "a", "definition": "missing@1", "input": 1 }]),
        ),
        (
            "inputFrom must name a dependency",
            json!([valid, { "id": "a", "definition": "step@1", "inputFrom": "first" }]),
        ),
        (
            "Specify input or inputFrom, not both",
            json!([valid, { "id": "a", "definition": "step@1", "input": 1, "inputFrom": "first", "dependsOn": ["first"] }]),
        ),
        (
            "when.node must name a dependency",
            json!([valid, { "id": "a", "definition": "step@1", "input": 1, "when": { "node": "first", "equals": 1 } }]),
        ),
        (
            "when.equals must be a JSON value",
            json!([valid, { "id": "a", "definition": "step@1", "input": 1, "dependsOn": ["first"], "when": { "node": "first" } }]),
        ),
        (
            "Unknown payload field: retries",
            json!([valid, { "id": "a", "definition": "step@1", "input": 1, "retries": 3 }]),
        ),
        (
            "dependsOn must be an array",
            json!([valid, { "id": "a", "definition": "step@1", "input": 1, "dependsOn": "first" }]),
        ),
    ];
    for (message, nodes) in cases {
        let error = run(&g.fixture, nodes).unwrap_err();
        assert!(error.contains(message), "{message}: {error}");
    }
    assert_eq!(
        g.fixture
            .call("workflows.run", json!({ "nodes": "not a list" }))
            .unwrap_err(),
        "nodes must be an array"
    );
    assert_eq!(g.fixture.journal(), Vec::<Value>::new());
    assert!(lock(&g.adapter_calls).is_empty());
    assert!(lock(&g.fixture.lane_calls).is_empty());
    g.fixture.agents.close();
}

#[test]
fn acceptance_a12_a_node_whose_definition_cannot_execute_is_rejected_before_effects() {
    // A stored predict definition is loadable, but this owner has no predict adapter.
    let descriptor = definition("classifier", "predict", json!({}));
    let mut stored = descriptor.clone();
    stored["hash"] = json!(definition_hash(&descriptor));
    let definitions = Memory::default();
    *lock(&definitions.value) = Some(json!({ "version": 1, "definitions": [stored] }));
    let fixture = agents_support::fixture(Options {
        definitions: Some(definitions),
        ..Options::default()
    });
    let error = run(
        &fixture,
        json!([
            { "id": "first", "definition": "identity@1", "input": 1 },
            { "id": "classify", "definition": "classifier@1", "input": { "n": 1 } },
        ]),
    )
    .unwrap_err();
    assert!(error.contains("requires an injected predict adapter"), "{error}");
    assert_eq!(fixture.journal(), Vec::<Value>::new());
    fixture.agents.close();
}

#[test]
fn acceptance_a12_fan_in_receives_every_dependencys_result_keyed_by_dependency_id() {
    let g = graph(None, None);
    let output = run(
        &g.fixture,
        json!([
            { "id": "left", "definition": "step@1", "input": { "side": "left" } },
            { "id": "right", "definition": "doubler@1", "input": { "n": 4 } },
            { "id": "join", "definition": "step@1", "dependsOn": ["left", "right"], "inputFrom": ["left", "right"] },
            { "id": "single", "definition": "step@1", "dependsOn": ["right"], "inputFrom": "right" },
        ]),
    )
    .unwrap();
    assert_eq!(
        output["join"],
        json!({ "status": "succeeded", "value": { "left": { "side": "left" }, "right": { "doubled": 8 } },
                "verification": "unverified" })
    );
    assert_eq!(
        (output["single"]["status"].clone(), output["single"]["value"].clone()),
        (json!("succeeded"), json!({ "doubled": 8 }))
    );
    assert!(
        lock(&g.adapter_calls)
            .iter()
            .any(|(_, input)| input.get("left").is_some())
    );
    g.fixture.agents.close();
}

#[test]
fn acceptance_a12_a_failed_dependency_yields_explicit_transitive_skip_records_while_independent_siblings_complete() {
    let g = graph(None, None);
    let output = run(
        &g.fixture,
        json!([
            { "id": "broken", "definition": "step@1", "input": { "fail": true } },
            { "id": "after", "definition": "step@1", "input": 1, "dependsOn": ["broken"] },
            { "id": "later", "definition": "step@1", "input": 2, "dependsOn": ["after"] },
            { "id": "independent", "definition": "step@1", "input": { "ok": true } },
        ]),
    )
    .unwrap();
    let expected = json!({
        "broken": { "status": "failed", "error": "step crashed", "verification": "unverified" },
        "after": { "status": "skipped", "reason": "Dependency broken did not succeed (failed)" },
        "later": { "status": "skipped", "reason": "Dependency after did not succeed (skipped)" },
        "independent": { "status": "succeeded", "value": { "ok": true }, "verification": "unverified" },
    });
    assert_eq!(output, expected);
    // Skipped nodes admit no task.
    let mut states: Vec<String> = g
        .fixture
        .journal()
        .iter()
        .map(|t| t["state"].as_str().unwrap().into())
        .collect();
    states.sort();
    assert_eq!(states, ["completed", "failed"]);
    g.fixture.agents.close();
}

#[test]
fn acceptance_a12_conditional_routes_are_explicit_the_matching_branch_runs_the_other_is_skipped_with_a_reason() {
    let g = graph(None, None);
    let output = run(
        &g.fixture,
        json!([
            { "id": "classify", "definition": "step@1", "input": { "route": "fix", "detail": 7 } },
            { "id": "fix", "definition": "step@1", "dependsOn": ["classify"], "inputFrom": "classify",
              "when": { "node": "classify", "field": "route", "equals": "fix" } },
            { "id": "ship", "definition": "step@1", "input": { "shipping": true }, "dependsOn": ["classify"],
              "when": { "node": "classify", "field": "route", "equals": "ship" } },
            { "id": "announce", "definition": "step@1", "input": 1, "dependsOn": ["ship"] },
            { "id": "whole", "definition": "step@1", "input": 2, "dependsOn": ["classify"],
              "when": { "node": "classify", "equals": { "detail": 7, "route": "fix" } } },
        ]),
    )
    .unwrap();
    assert_eq!(
        (output["fix"]["status"].clone(), output["fix"]["value"].clone()),
        (json!("succeeded"), json!({ "route": "fix", "detail": 7 }))
    );
    assert_eq!(
        output["ship"],
        json!({ "status": "skipped", "reason": "Route condition on classify.route not met" })
    );
    assert_eq!(
        output["announce"],
        json!({ "status": "skipped", "reason": "Dependency ship did not succeed (skipped)" })
    );
    // Whole-value routes compare canonical JSON, independent of key order.
    assert_eq!(output["whole"]["status"], "succeeded");
    assert_eq!(g.fixture.journal().len(), 3);
    g.fixture.agents.close();
}

#[test]
fn acceptance_a12_a_bound_input_that_fails_the_nodes_schema_is_an_explicit_failed_node_with_nothing_spawned_for_it() {
    let g = graph(None, None);
    let output = run(
        &g.fixture,
        json!([
            { "id": "source", "definition": "step@1", "input": { "x": 1 } },
            { "id": "consumer", "definition": "doubler@1", "dependsOn": ["source"], "inputFrom": "source" },
            { "id": "downstream", "definition": "step@1", "input": 1, "dependsOn": ["consumer"] },
            { "id": "sibling", "definition": "doubler@1", "input": { "n": 2 } },
        ]),
    )
    .unwrap();
    assert_eq!(
        output["consumer"],
        json!({ "status": "failed", "error": "Bound input rejected: doubler@1 input does not match its schema",
                "verification": "unverified" })
    );
    assert_eq!(
        output["downstream"],
        json!({ "status": "skipped", "reason": "Dependency consumer did not succeed (failed)" })
    );
    assert_eq!(
        (output["sibling"]["status"].clone(), output["sibling"]["value"].clone()),
        (json!("succeeded"), json!({ "doubled": 4 }))
    );
    assert_eq!(output["source"]["status"], "succeeded");
    let mut definitions: Vec<String> = g
        .fixture
        .journal()
        .iter()
        .map(|t| t["definition"].as_str().unwrap().into())
        .collect();
    definitions.sort();
    assert_eq!(definitions, ["doubler@1", "step@1"]);
    g.fixture.agents.close();
}

#[test]
fn acceptance_a12_refused_admission_is_an_explicit_failed_node_and_does_not_abort_running_siblings() {
    let hold = Signal::new();
    // The crowded node's refusal is the event the test waits for before releasing the slow one.
    let refused = Signal::new();
    let noticed = refused.clone();
    let usage = UsageLedger::new(
        Box::new(Memory::default()),
        delegation::usage::UsageOptions {
            limits: UsageLimits {
                max_admitted_tasks: Some(1),
                ..UsageLimits::default()
            },
            on_refused: Some(Arc::new(move |reason: &str| noticed.abort(reason))),
            ..delegation::usage::UsageOptions::default()
        },
    )
    .unwrap();
    let g = graph(Some(usage), Some(hold.clone()));
    let agents = g.fixture.agents.clone();
    let pending = std::thread::spawn(move || {
        agents.handle(
            "workflows.run",
            json!({ "nodes": [
                { "id": "slow", "definition": "step@1", "input": { "hold": true } },
                { "id": "crowded", "definition": "step@1", "input": { "crowded": true } },
            ] }),
            "main",
            None,
        )
    });
    wait_for("the crowded node refused", Duration::from_secs(120), || {
        refused.is_aborted()
    });
    assert_eq!(g.fixture.journal().len(), 1);
    hold.abort("resolve");
    let output = pending.join().unwrap().unwrap();
    assert_eq!(
        (output["slow"]["status"].clone(), output["slow"]["value"].clone()),
        (json!("succeeded"), json!({ "hold": true }))
    );
    assert_eq!(
        output["crowded"],
        json!({ "status": "failed", "error": "Admission refused: Usage admitted-task limit exceeded for root ultron-root",
                "verification": "unverified" })
    );
    g.fixture.agents.close();
}
