//! A44 (partial): progress and reassessment use evidence and cannot extend budgets or declare unsupported success.
//! Ported from packages/coding-agent/test/ultron-progress.test.ts with literal expectations: the real
//! `kernel_apis::ProgressModule` behind `g3_support::Host` (task identity, lineage, lanes and terminal results from the
//! real `harness` task supervisor; background jobs on waiting lanes, as the TS tests use them).
//! Ported: classification (the finished task is a background job whose lane returned, instead of a deterministic
//! `identity@1` task), reporter identity and scope, strict validation and the receipt bound (on a background job
//! instead of an `identity@1` task), budget extension denial, persistence and interrupted-verification settling,
//! corrupt and newer-format documents.
//! Not ported: "never verifies a completion claim without a passing verifier" needs `agents.register` and verifier
//! runs of deterministic definitions (definition registry and deterministic adapters are not in Rust).

mod g3_support;

use g3_support::{Host, JournalStore, Tasks, lane_name, rejects};
use kernel_apis::{ApiModule, DocumentStore, MemoryDocumentStore, ProgressModule, TaskHost};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct Setup {
    host: Host,
    tasks: Arc<Tasks>,
    clock: Arc<AtomicI64>,
    running: String,
    worker_lane: String,
}

fn setup(store: Arc<MemoryDocumentStore>) -> Setup {
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let tasks = Tasks::new(JournalStore::default(), clock.clone());
    let module = ProgressModule::with_limits(store, 60_000, 1000);
    let host = Host::new(tasks.clone(), vec![Arc::new(module) as Arc<dyn ApiModule>]);
    let running = host.call("background.start", json!({"prompt": "work"})).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let worker_lane = lane_name("background-job@1", &running);
    Setup {
        host,
        tasks,
        clock,
        running,
        worker_lane,
    }
}

fn fresh() -> Setup {
    setup(Arc::default())
}

#[test]
fn acceptance_a44_classifies_stalled_progressing_busy_and_finished_from_recorded_evidence_ported_part() {
    let s = fresh();
    let assess = |id: &str| s.host.call("progress.assess", json!({"task_id": id})).unwrap();
    let report = |summary: &str, sha: &str| {
        s.host
            .call_in(
                &s.worker_lane,
                "progress.report",
                json!({"summary": summary, "evidence": [{"kind": "file", "ref": "src/parser.ts", "sha256": sha}]}),
            )
            .unwrap()
    };

    let never = assess(&s.running);
    assert_eq!(never["classification"], "stalled");
    assert!(never["reasons"][0].as_str().unwrap().contains("never reported"));

    let first = report("wrote parser", SHA_A);
    assert_eq!(first["reporter_task_id"], s.running.as_str());
    assert_eq!(first["new_evidence"], json!(["file:src/parser.ts"]));
    let progressing = assess(&s.running);
    assert_eq!(progressing["classification"], "progressing");
    assert_eq!(progressing["receipts"], json!([first["id"]]));

    // Past the window, repeating identical evidence is activity, not progress.
    s.clock.fetch_add(61_000, Ordering::SeqCst);
    let repeat = report("still on parser", SHA_A);
    assert_eq!(repeat["new_evidence"], json!([]));
    let busy = assess(&s.running);
    assert_eq!(busy["classification"], "busy");
    assert_eq!(busy["receipts"], json!([repeat["id"]]));

    // A changed hash on the same ref is new evidence.
    let changed = report("parser rewritten", SHA_B);
    assert_eq!(changed["new_evidence"], json!(["file:src/parser.ts"]));
    assert_eq!(assess(&s.running)["classification"], "progressing");

    s.clock.fetch_add(60_001, Ordering::SeqCst);
    let stalled = assess(&s.running);
    assert_eq!(stalled["classification"], "stalled");
    assert!(stalled["reasons"][0].as_str().unwrap().contains("60001ms old"));

    let done = s.host.call("background.start", json!({"prompt": "quick"})).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    s.tasks.finish(&done, "ok");
    let finished = assess(&done);
    assert_eq!(
        (
            &finished["classification"],
            &finished["task_state"],
            &finished["result_status"],
            &finished["verification"]
        ),
        (
            &json!("finished"),
            &json!("completed"),
            &json!("succeeded"),
            &json!("unverified")
        )
    );
}

#[test]
fn acceptance_a44_takes_the_reporter_from_the_lane_and_rejects_reports_for_unrelated_tasks() {
    let s = fresh();
    let child = s
        .host
        .call_in(&s.worker_lane, "background.start", json!({"prompt": "child"}))
        .unwrap()["id"]
        .clone();
    let sibling = s.host.call("background.start", json!({"prompt": "sibling"})).unwrap()["id"].clone();
    let evidence = json!([{"kind": "log", "ref": "run-1"}]);

    let for_parent = s
        .host
        .call_in(
            &s.worker_lane,
            "progress.report",
            json!({"task_id": child, "summary": "child started", "evidence": evidence}),
        )
        .unwrap();
    assert_eq!(
        (&for_parent["task_id"], &for_parent["reporter_task_id"]),
        (&child, &json!(s.running))
    );
    rejects(
        s.host.call_in(
            &s.worker_lane,
            "progress.report",
            json!({"task_id": sibling, "summary": "not mine", "evidence": evidence}),
        ),
        "itself or its direct children",
    );
    // The root may report for its direct children but not for grandchildren.
    let root_report = s
        .host
        .call(
            "progress.report",
            json!({"task_id": s.running, "summary": "root view", "evidence": evidence}),
        )
        .unwrap();
    assert_eq!(root_report["reporter_task_id"], Value::Null);
    rejects(
        s.host.call(
            "progress.report",
            json!({"task_id": child, "summary": "grandchild", "evidence": evidence}),
        ),
        "itself or its direct children",
    );
    rejects(
        s.host
            .call("progress.report", json!({"summary": "who?", "evidence": evidence})),
        "must name the task_id",
    );
    // A child cannot read or reassess its parent.
    rejects(
        s.host.call_in(
            &lane_name("background-job@1", child.as_str().unwrap()),
            "progress.history",
            json!({"task_id": s.running}),
        ),
        "descendants",
    );
}

#[test]
fn acceptance_a44_validates_payloads_strictly_and_bounds_evidence_and_receipts() {
    let s = fresh();
    let lane = &s.worker_lane;
    let report = |payload: Value| s.host.call_in(lane, "progress.report", payload);
    rejects(
        report(json!({"summary": "x", "evidence": [{"kind": "file", "ref": "a"}], "reporter": "forged"})),
        "Unknown payload field: reporter",
    );
    rejects(
        report(json!({"summary": "x", "evidence": [{"kind": "file", "ref": "a", "extra": 1}]})),
        "Unknown payload field: extra",
    );
    rejects(report(json!({"summary": "x", "evidence": []})), "evidence");
    rejects(
        report(json!({"summary": "x", "evidence": [{"kind": "file", "ref": "a", "sha256": "nope"}]})),
        "sha256",
    );
    let many: Vec<Value> = (0..33)
        .map(|i| json!({"kind": "file", "ref": format!("f{i}")}))
        .collect();
    rejects(report(json!({"summary": "x", "evidence": many})), "1 to 32");
    rejects(
        report(json!({"summary": "x", "evidence": [{"kind": "f", "ref": "a"}], "metrics": {"n": "1"}})),
        "finite number",
    );
    let running = &s.running;
    rejects(
        s.host
            .call("progress.assess", json!({"task_id": running, "verbose": true})),
        "Unknown payload field: verbose",
    );
    rejects(
        s.host
            .call("progress.reassess", json!({"task_id": running, "claim": "done"})),
        "claim must be",
    );
    rejects(
        s.host.call(
            "progress.reassess",
            json!({"task_id": running, "verifier": "identity@1"}),
        ),
        "require claim",
    );
    rejects(
        s.host
            .call("progress.reassess", json!({"task_id": running, "approve": true})),
        "Unknown payload field: approve",
    );
    rejects(
        s.host
            .call("progress.assess", json!({"task_id": "ultron-task-missing"})),
        "Unknown Ultron task",
    );
    rejects(s.host.call("progress.nope", json!({})), "Unknown progress request");

    // Receipts per task are bounded.
    let tasks = Tasks::new(JournalStore::default(), Arc::new(AtomicI64::new(0)));
    let bounded = Host::new(
        tasks,
        vec![Arc::new(ProgressModule::with_limits(
            Arc::new(MemoryDocumentStore::default()),
            60_000,
            2,
        )) as Arc<dyn ApiModule>],
    );
    let task = bounded.call("background.start", json!({"prompt": "subject"})).unwrap()["id"].clone();
    let report = || {
        bounded.call(
            "progress.report",
            json!({"task_id": task, "summary": "s", "evidence": [{"kind": "k", "ref": "r"}]}),
        )
    };
    report().unwrap();
    report().unwrap();
    rejects(report(), "maximum of 2 progress receipts");
}

#[test]
fn acceptance_a44_always_denies_budget_extension_and_records_the_reason() {
    let s = fresh();
    let decision = s
        .host
        .call_in(
            &s.worker_lane,
            "progress.reassess",
            json!({"task_id": s.running, "extend_budget": {"tasks": 10}}),
        )
        .unwrap();
    assert_eq!(decision["requester_task_id"], s.running.as_str());
    assert_eq!(decision["budget"]["decision"], "denied");
    assert!(
        decision["budget"]["reason"]
            .as_str()
            .unwrap()
            .contains("fixed at admission")
    );
    assert_eq!(decision["assessment"]["classification"], "stalled");
    let root = s
        .host
        .call(
            "progress.reassess",
            json!({"task_id": s.running, "extend_budget": true}),
        )
        .unwrap();
    assert_eq!(root["budget"]["decision"], "denied");
    let history = s.host.call("progress.history", json!({"task_id": s.running})).unwrap();
    let decisions: Vec<_> = history["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["budget"]["decision"].clone())
        .collect();
    assert_eq!(decisions, vec![json!("denied"), json!("denied")]);
}

#[test]
fn acceptance_a44_persists_receipts_and_decisions_across_module_instances_and_settles_interrupted_verifications() {
    let store: Arc<MemoryDocumentStore> = Arc::default();
    let first = setup(store.clone());
    first
        .host
        .call_in(
            &first.worker_lane,
            "progress.report",
            json!({"summary": "step", "evidence": [{"kind": "test", "ref": "suite-a"}], "metrics": {"passed": 3}}),
        )
        .unwrap();
    first
        .host
        .call(
            "progress.reassess",
            json!({"task_id": first.running, "extend_budget": true}),
        )
        .unwrap();
    let mut saved = store.read().unwrap().unwrap();
    // Simulate an owner that died while a verifier was running.
    saved["decisions"].as_array_mut().unwrap().push(json!({
        "id": "progress-decision-crashed", "task_id": first.running, "requester_task_id": null, "at": 1,
        "assessment": {"classification": "busy", "reasons": [], "receipts": []},
        "claim": {"claim": "complete", "decision": "pending", "reason": "Verifier running", "verifier": "identity@1"},
    }));
    store.write(&saved).unwrap();

    // A new module over the same store, against the restarted owner's journal of the same task.
    let reopened = Host::new(
        first.tasks.clone(),
        vec![Arc::new(ProgressModule::new(store)) as Arc<dyn ApiModule>],
    );
    let history = reopened
        .call("progress.history", json!({"task_id": first.running}))
        .unwrap();
    let receipts = history["receipts"].as_array().unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0]["metrics"], json!({"passed": 3}));
    let outcomes: Vec<_> = history["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            if d["budget"]["decision"].is_null() {
                d["claim"]["decision"].clone()
            } else {
                d["budget"]["decision"].clone()
            }
        })
        .collect();
    assert_eq!(outcomes, vec![json!("denied"), json!("unverified")]);
    assert!(
        history["decisions"][1]["claim"]["reason"]
            .as_str()
            .unwrap()
            .contains("Owner ended")
    );
}

fn start_error(document: Value) -> String {
    let module = ProgressModule::new(Arc::new(MemoryDocumentStore::new(Some(document))));
    let tasks = Tasks::new(JournalStore::default(), Arc::new(AtomicI64::new(0)));
    ApiModule::start(&module, tasks.as_ref() as &dyn TaskHost).unwrap_err()
}

#[test]
fn acceptance_a44_rejects_a_corrupt_stored_document() {
    assert!(start_error(json!({"version": 1})).contains("Invalid progress document"));
}

#[test]
fn acceptance_a44_a_document_in_a_newer_format_fails_naming_its_namespace_and_version() {
    assert!(start_error(json!({"version": 2})).contains("ultron.module/progress has format version 2"));
}
