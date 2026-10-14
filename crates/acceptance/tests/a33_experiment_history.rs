//! A33: experiment history includes all attempts, and variants do not contaminate each other. Every case of
//! packages/coding-agent/test/ultron-a33-experiment-history.test.ts with its literal expectations: the experiment
//! ledger and refinements of `services::LocalServices` (replayed against local-services.ts in
//! `crates/services/tests/local_fixture.rs`), and memory through `kernel_apis::MemoryService` over the real
//! Hindsight adapter with the fake Hindsight as its transport (`memory_support`), one shared server as in production.

mod memory_support;
mod services_support;

use kernel_apis::MemoryService;
use memory_support::{FakeHindsight, durable_store, keep_gate, p, sha256, sig};
use serde_json::{Value, json};
use services::local::{LocalOptions, LocalServices};
use services_support::{Documents, rejects};
use std::collections::{BTreeMap, HashSet};

fn ledger(documents: &Documents) -> LocalServices {
    LocalServices::new(Box::new(documents.clone()), LocalOptions::default())
}

fn call(services: &LocalServices, kind: &str, payload: Value) -> Result<Value, String> {
    services.handle(kind, &payload, None)
}

/// The frozen fixture: its hash is computed from the content, not chosen by the caller.
fn fixture_hash() -> String {
    sha256(
        &json!({ "task": "fix off-by-one in pager", "files": { "src/pager.ts": "..." }, "checks": ["npm test"] })
            .to_string(),
    )
}

fn config_hash() -> String {
    sha256(&json!({ "model": "provider/model-a", "thinking": "medium", "definitions": { "rlm-child": 1 } }).to_string())
}

#[test]
fn acceptance_a33_every_attempt_including_crashed_failed_and_incomplete_ones_is_recorded_against_the_frozen_fixture() {
    let documents = Documents::default();
    let record = |run: Value| call(&ledger(&documents), "experiments.record", json!({ "run": run }));
    let attempts = [
        json!({ "variant": "baseline", "attempt": 1, "outcome": "incomplete", "reason": "worker crashed" }),
        json!({ "variant": "baseline", "attempt": 2, "outcome": "failed", "reason": "npm test failed" }),
        json!({ "variant": "baseline", "attempt": 3, "outcome": "passed" }),
        json!({ "variant": "candidate", "attempt": 1, "outcome": "incomplete", "reason": "cancelled at budget" }),
        json!({ "variant": "candidate", "attempt": 2, "outcome": "passed" }),
    ];
    let mut recorded = Vec::new();
    for attempt in &attempts {
        let mut run = attempt.clone();
        run["fixtureHash"] = json!(fixture_hash());
        run["configHash"] = json!(config_hash());
        recorded.push(record(run).unwrap());
    }

    // Restart: the ledger returns every attempt in order, with the same frozen hashes.
    let listed = call(&ledger(&documents), "experiments.list", json!({})).unwrap();
    assert_eq!(listed, Value::Array(recorded));
    let listed = listed.as_array().unwrap().clone();
    let triple = |run: &Value| json!([run["variant"], run["attempt"], run["outcome"]]);
    assert_eq!(
        listed.iter().map(triple).collect::<Vec<_>>(),
        attempts.iter().map(triple).collect::<Vec<_>>()
    );
    let hashes: HashSet<&str> = listed.iter().map(|r| r["fixtureHash"].as_str().unwrap()).collect();
    assert_eq!(hashes, HashSet::from([fixture_hash().as_str()]));
    let ids: HashSet<&str> = listed.iter().map(|r| r["id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), attempts.len());

    // Records are append-only: no update or delete request exists, and an unknown outcome is refused.
    let services = ledger(&documents);
    for kind in ["experiments.update", "experiments.delete"] {
        rejects(
            call(&services, kind, json!({ "id": listed[0]["id"] })),
            "Unknown local service",
        );
    }
    rejects(
        call(
            &services,
            "experiments.record",
            json!({ "run": { "variant": "baseline", "fixtureHash": fixture_hash(), "outcome": "crashed" } }),
        ),
        "outcome",
    );
    // Only runs on the same frozen fixture compare; a changed fixture cannot.
    let comparison = call(
        &services,
        "experiments.compare",
        json!({ "baseline": listed[2]["id"], "candidate": listed[4]["id"] }),
    )
    .unwrap();
    assert_eq!(
        comparison["claim"],
        "Observed runs only; no statistical superiority established"
    );
    let drifted =
        record(json!({ "variant": "candidate", "attempt": 3, "outcome": "passed", "fixtureHash": sha256("edited") }))
            .unwrap();
    rejects(
        call(
            &services,
            "experiments.compare",
            json!({ "baseline": listed[2]["id"], "candidate": drifted["id"] }),
        ),
        "Different fixtures",
    );
    assert_eq!(
        call(&services, "experiments.list", json!({}))
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        6
    );
}

struct Variant {
    name: &'static str,
    tags: Value,
    journal: std::sync::Arc<kernel_apis::MemoryDocumentStore>,
    memory: MemoryService,
    refinements: LocalServices,
}

fn variant(hindsight: &FakeHindsight, name: &'static str) -> Variant {
    let tags = json!({ "session": [format!("ultron:session:experiment-{name}")] });
    let journal = durable_store();
    let memory = MemoryService::new(
        journal.clone(),
        hindsight.backend_for(tags.clone(), "bank-1"),
        keep_gate(),
    )
    .unwrap();
    Variant {
        name,
        tags,
        journal,
        memory,
        refinements: ledger(&Documents::default()),
    }
}

#[test]
fn acceptance_a33_variants_with_isolated_memory_and_refinement_state_never_see_each_others_writes() {
    let hindsight = FakeHindsight::new(); // one shared Hindsight server, as in production
    let a = variant(&hindsight, "a");
    let b = variant(&hindsight, "b");

    // Variant A learns a lesson and activates a refinement.
    let lesson = a
        .memory
        .propose(
            &p(json!({ "text": "A-only lesson", "evidence": [{ "ref": "run:a-1" }] })),
            &sig(),
        )
        .unwrap();
    let memory_id = lesson["memoryId"].as_str().unwrap().to_string();
    a.memory.get(&p(json!({ "id": memory_id })), &sig()).unwrap();
    let refinement = call(
        &a.refinements,
        "refinements.propose",
        json!({ "kind": "instruction", "target": "instruction:rlm-child", "baseVersion": 0,
                "content": "A-only instruction", "evidence": [{ "run": "a-1" }] }),
    )
    .unwrap();
    call(
        &a.refinements,
        "refinements.activate",
        json!({ "id": refinement["id"] }),
    )
    .unwrap();

    // Variant B sees none of it: no recalled memory, no active refinement.
    let recalled_b = b
        .memory
        .prepare(&p(json!({ "query": "lesson", "taskId": "b-1" })), &sig())
        .unwrap();
    assert_eq!(recalled_b["results"], json!([]));
    assert_eq!(
        call(
            &b.refinements,
            "refinements.current",
            json!({ "kind": "instruction", "target": "instruction:rlm-child" })
        )
        .unwrap(),
        Value::Null
    );
    let recalled_a = a
        .memory
        .prepare(&p(json!({ "query": "lesson", "taskId": "a-2" })), &sig())
        .unwrap();
    let texts: Vec<&Value> = recalled_a["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| &item["text"])
        .collect();
    assert_eq!(texts, vec![&json!("A-only lesson")]);
    let recall_tags: Vec<Value> = hindsight.recalls().iter().map(|body| body["tags"].clone()).collect();
    assert_eq!(recall_tags, vec![b.tags["session"].clone(), a.tags["session"].clone()]);

    // B cannot borrow A's journal to read A's document: the handle is bound to A's scope tags.
    let borrowed = MemoryService::new(
        a.journal.clone(),
        hindsight.backend_for(b.tags.clone(), "bank-1"),
        keep_gate(),
    )
    .unwrap();
    assert_eq!(
        borrowed.get(&p(json!({ "id": memory_id })), &sig()).unwrap_err().code,
        "UNSUPPORTED_SCOPE"
    );
    // A journal from a different memory bank is refused outright.
    let other_bank = MemoryService::new(
        a.journal.clone(),
        hindsight.backend_for(b.tags.clone(), "bank-b"),
        keep_gate(),
    );
    let refused = match other_bank {
        Err(error) => error,
        Ok(service) => service.list().unwrap_err(),
    };
    assert_eq!(refused.code, "BACKEND_MISMATCH");

    // Each variant's recorded memory state names only its own scope and writes.
    let shared = ledger(&Documents::default());
    for item in [&a, &b] {
        let operations = item.memory.list().unwrap();
        let writes = operations
            .as_array()
            .unwrap()
            .iter()
            .filter(|op| op["kind"] == "propose")
            .count();
        call(
            &shared,
            "experiments.record",
            json!({ "run": { "variant": item.name, "fixtureHash": fixture_hash(), "outcome": "passed",
                             "memory": { "tags": item.tags["session"], "writes": writes } } }),
        )
        .unwrap();
    }
    let runs = call(&shared, "experiments.list", json!({})).unwrap();
    let memory: Vec<&Value> = runs.as_array().unwrap().iter().map(|run| &run["memory"]).collect();
    assert_eq!(
        memory,
        vec![
            &json!({ "tags": a.tags["session"], "writes": 1 }),
            &json!({ "tags": b.tags["session"], "writes": 0 }),
        ]
    );
}

#[test]
fn acceptance_a33_per_variant_usage_including_failed_and_incomplete_attempts_reconciles_with_independent_metering() {
    let mut meter: BTreeMap<String, f64> = BTreeMap::new(); // independent provider-side accounting
    let services = ledger(&Documents::default());
    let attempts = [
        ("baseline", "incomplete", [1200.0, 0.0, 40.0, 0.0]),
        ("baseline", "failed", [3400.0, 900.0, 40.0, 0.0]),
        ("baseline", "passed", [2800.0, 700.0, 40.0, 0.0]),
        ("memory", "failed", [2600.0, 300.0, 80.0, 120.0]),
        ("memory", "passed", [2100.0, 250.0, 80.0, 120.0]),
    ];
    for (variant, outcome, [root, descendants, frames, memory]) in attempts {
        let total = root + descendants + frames + memory;
        *meter.entry(variant.into()).or_default() += total;
        call(
            &services,
            "experiments.record",
            json!({ "run": { "variant": variant, "outcome": outcome, "fixtureHash": fixture_hash(),
                             "usage": { "root": root, "descendants": descendants, "frames": frames,
                                        "memory": memory, "total": total } } }),
        )
        .unwrap();
    }
    // A usage record whose parts do not add up is refused rather than silently stored.
    rejects(
        call(
            &services,
            "experiments.record",
            json!({ "run": { "variant": "memory", "fixtureHash": fixture_hash(), "outcome": "failed",
                             "usage": { "root": 10, "frames": 5, "total": 12 } } }),
        ),
        "does not reconcile",
    );
    rejects(
        call(
            &services,
            "experiments.record",
            json!({ "run": { "variant": "memory", "fixtureHash": fixture_hash(), "outcome": "failed",
                             "usage": { "root": -1, "total": -1 } } }),
        ),
        "nonnegative",
    );

    let runs = call(&services, "experiments.list", json!({})).unwrap();
    let runs = runs.as_array().unwrap();
    assert_eq!(runs.len(), attempts.len());
    let mut per_variant: BTreeMap<String, f64> = BTreeMap::new();
    for run in runs {
        *per_variant.entry(run["variant"].as_str().unwrap().into()).or_default() +=
            run["usage"]["total"].as_f64().unwrap();
    }
    assert_eq!(per_variant, meter);
    // Failed and incomplete attempts carry real cost and are part of each variant's total.
    let unsuccessful: f64 = runs
        .iter()
        .filter(|run| run["outcome"] != "passed")
        .map(|run| run["usage"]["total"].as_f64().unwrap())
        .sum();
    assert_eq!(unsuccessful, 1240.0 + 4340.0 + 3100.0);
}
