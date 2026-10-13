//! A24: costs, task transitions and inspection agree on one record.
//! Port of packages/coding-agent/test/ultron-a24-accounting-reconciliation.test.ts: a mixed run through the Rust
//! typed-agent host with its usage ledger and the real progress and schedule modules (`kernel_apis`), every record
//! in an owner-fenced durable document; `delegation::reconcile` rebuilds the run from those documents and must agree
//! with `agents.status`, before and after a crashed owner is replaced, and must detect tampering.

mod agents_support;
mod common;

use agents_support::{Options, aborted, adapter};
use common::null_handler;
use delegation::agents::Agents;
use delegation::reconcile::{ReconcileInput, inspection_discrepancies, reconcile_records};
use delegation::task_store::TaskStore;
use delegation::usage::{Measurement, UsageLedger, UsageLimits, UsageOptions};
use kernel::{HostHandler, Signal};
use kernel_apis::{DocumentStore, KernelApis, ProgressModule, SchedulesModule};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One durable document with owner fencing: opening a new owner makes the previous owner's writes fail.
#[derive(Clone, Default)]
struct Durable {
    document: Arc<Mutex<Option<Value>>>,
    generation: Arc<AtomicUsize>,
}

struct Owner {
    durable: Durable,
    mine: usize,
}

impl Durable {
    fn open(&self) -> Owner {
        Owner {
            durable: self.clone(),
            mine: self.generation.fetch_add(1, Ordering::SeqCst) + 1,
        }
    }
    fn get(&self) -> Option<Value> {
        self.document.lock().unwrap().clone()
    }
}

impl Owner {
    fn put(&self, next: &Value) -> Result<(), String> {
        if self.mine != self.durable.generation.load(Ordering::SeqCst) {
            return Err("owner fenced".into());
        }
        *self.durable.document.lock().unwrap() = Some(next.clone());
        Ok(())
    }
}

impl TaskStore for Owner {
    fn read(&mut self) -> Result<Option<Value>, String> {
        Ok(self.durable.get())
    }
    fn write(&mut self, document: &Value) -> Result<(), String> {
        self.put(document)
    }
}

impl DocumentStore for Owner {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(self.durable.get())
    }
    fn write(&self, document: &Value) -> Result<(), String> {
        self.put(document)
    }
}

#[derive(Clone, Default)]
struct Stores {
    tasks: Durable,
    usage: Durable,
    progress: Durable,
    schedules: Durable,
    definitions: Durable,
}

struct Host {
    agents: Agents,
    apis: KernelApis,
}

impl Host {
    fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        let Value::Object(payload) = payload else {
            unreachable!()
        };
        self.apis.handle(kind, payload, &Signal::new())
    }
}

/// Lanes whose behavior follows the child prompt: "work" completes two provider turns with reported usage,
/// "unmetered" completes without usage, "fail" fails the model call, "hang" waits for abort.
fn build(state: &Stores) -> Host {
    let usage = UsageLedger::new(
        Box::new(state.usage.open()),
        UsageOptions {
            limits: UsageLimits {
                max_admitted_tasks: Some(24),
                ..UsageLimits::default()
            },
            ..UsageOptions::default()
        },
    )
    .unwrap();
    let options = Options {
        usage: Some(usage),
        deterministic: Some(adapter(|id, input, _, _| {
            Ok(match id {
                "check-pass" => json!({ "passed": true }),
                "check-fail" => json!({ "passed": false }),
                _ => input.clone(),
            })
        })),
        script: Some(Arc::new(|_, prompt, signal, _| match prompt {
            "fail" => Err(json!({ "kind": "provider", "message": "503" }).to_string()),
            "hang" => aborted(signal),
            _ => Ok("done".into()),
        })),
        metered: Some(Arc::new(|_, prompt| {
            (prompt == "work").then_some(Measurement {
                input_tokens: Some(150.0),
                output_tokens: Some(30.0),
                total_tokens: Some(180.0),
                cost: Some(0.01 + 0.005),
                wall_ms: None,
            })
        })),
        ..Options::default()
    };
    let fixture = agents_support::fixture_with_stores(
        options,
        Box::new(state.tasks.open()),
        Box::new(state.definitions.open()),
    );
    let agents = fixture.agents.clone();
    let apis = KernelApis::new(Arc::new(agents.handler("main", null_handler())) as Arc<dyn HostHandler>)
        .with_host(Arc::new(agents.clone()), "main")
        .with(Arc::new(ProgressModule::new(Arc::new(state.progress.open()))))
        .with(Arc::new(SchedulesModule::new(Arc::new(state.schedules.open()), None)));
    Host { agents, apis }
}

fn check(id: &str) -> Value {
    json!({
        "id": id, "version": "1", "strategy": "deterministic", "instructions": "Report whether the check passed.",
        "inputSchema": {}, "outputSchema": { "type": "object", "properties": { "passed": { "type": "boolean" } },
                                             "required": ["passed"] },
        "maxRepairs": 0, "inputDescription": "Any JSON value", "outputDescription": "{passed:boolean}",
    })
}

fn records(state: &Stores) -> Value {
    let tasks = state.tasks.get();
    let usage = state.usage.get();
    let progress = state.progress.get();
    let schedules = state.schedules.get();
    reconcile_records(&ReconcileInput {
        tasks: tasks.as_ref(),
        usage: usage.as_ref(),
        progress: progress.as_ref(),
        schedules: schedules.as_ref(),
        root_id: None,
    })
    .unwrap()
}

fn task_of<'a>(reconciliation: &'a Value, id: &Value) -> &'a Value {
    reconciliation["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| &t["id"] == id)
        .unwrap()
}

/// `toMatchObject`: every key of `want` is in `got` with a matching value.
fn matches(got: &Value, want: &Value) -> bool {
    match (got, want) {
        (Value::Object(got), Value::Object(want)) => {
            want.iter().all(|(k, w)| got.get(k).is_some_and(|g| matches(g, w)))
        }
        _ => got == want,
    }
}

#[test]
fn acceptance_a24_reconstructs_a_mixed_run_from_the_journal_ledger_progress_and_schedule_records_and_matches_agents_status()
 {
    let state = Stores::default();
    let host = build(&state);
    host.call("agents.register", json!({ "definition": check("check-pass") }))
        .unwrap();
    host.call("agents.register", json!({ "definition": check("check-fail") }))
        .unwrap();

    let direct = host
        .call(
            "agents.invoke",
            json!({ "definition": "identity@1", "input": { "n": 1 }, "key": "direct" }),
        )
        .unwrap();
    assert_eq!(direct["status"], "succeeded");
    // Idempotent lookup: no second admission, charge, or execution.
    assert_eq!(
        host.call(
            "agents.invoke",
            json!({ "definition": "identity@1", "input": { "n": 1 }, "key": "direct" })
        )
        .unwrap(),
        direct
    );
    assert!(
        host.call(
            "agents.invoke",
            json!({ "definition": "identity@1", "input": { "n": 2 }, "key": "direct" })
        )
        .unwrap_err()
        .contains("Idempotency key reused")
    );
    let spawn = |prompt: &str| {
        host.call(
            "agents.spawn",
            json!({ "definition": "rlm-child@1", "input": { "prompt": prompt } }),
        )
        .unwrap()
    };
    let settle = |task: &Value| host.call("agents.result", json!({ "id": task["id"] })).unwrap();
    let metered = spawn("work");
    assert!(matches(
        &settle(&metered),
        &json!({ "status": "succeeded", "value": "done" })
    ));
    let unmetered = spawn("unmetered");
    assert_eq!(settle(&unmetered)["status"], "succeeded");
    let failed = spawn("fail");
    assert_eq!(settle(&failed)["status"], "failed");
    let hanging = spawn("hang");
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(
        host.call("agents.cancel", json!({ "id": hanging["id"] })).unwrap(),
        json!({ "cancelled": true })
    );

    host.call(
        "progress.report",
        json!({ "task_id": metered["id"], "summary": "computed answer", "evidence": [{ "kind": "file", "ref": "out.txt" }] }),
    )
    .unwrap();
    let decision = host
        .call(
            "progress.reassess",
            json!({ "task_id": metered["id"], "claim": "complete", "verifier": "check-pass@1" }),
        )
        .unwrap();
    assert_eq!(decision["claim"]["decision"], "verified");
    let goal = host
        .call(
            "goals.create",
            json!({ "title": "release", "required_checks": ["check-pass@1", "check-fail@1"] }),
        )
        .unwrap();
    let verified = host.call("goals.verify", json!({ "id": goal["id"] })).unwrap();
    assert_eq!(verified["verification"]["status"], "unachieved");
    host.call(
        "schedules.create",
        json!({ "definition": "identity@1", "input": { "tick": true }, "every_ms": 60_000 }),
    )
    .unwrap();
    let tick = host.call("schedules.tick", json!({})).unwrap();
    assert_eq!(tick["fired"].as_array().unwrap().len(), 1);
    agents_support::wait_for("every task ended", Duration::from_secs(120), || {
        state.tasks.get().unwrap()["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t.get("result").is_some())
    });

    let status = host.call("agents.status", json!({})).unwrap();
    let reconciliation = records(&state);
    assert_eq!(reconciliation["discrepancies"], json!([]));
    assert_eq!(
        inspection_discrepancies(&reconciliation, &status).unwrap(),
        Vec::<String>::new()
    );

    // Per-task accounting: every journal task has exactly one admission settlement matching its state.
    assert_eq!(reconciliation["tasks"].as_array().unwrap().len(), 9);
    let metered_record = task_of(&reconciliation, &metered["id"]);
    assert!(matches(
        metered_record,
        &json!({
            "state": "completed", "admission": "succeeded",
            "usage": { "taskCalls": 1, "modelCalls": 1, "unknownCalls": 1 },
            "modelUsage": { "modelCalls": 1, "inputTokens": 150, "outputTokens": 30, "totalTokens": 180, "unknownCalls": 0 },
        })
    ));
    assert!((metered_record["modelUsage"]["cost"].as_f64().unwrap() - 0.015).abs() < 1e-12);
    // An admission is not a provider call and a run without reported usage stays unknown, never zero.
    assert_eq!(metered_record["usage"]["inputTokens"], Value::Null);
    assert!(matches(
        &task_of(&reconciliation, &unmetered["id"])["modelUsage"],
        &json!({ "modelCalls": 1, "cost": null, "unknownCalls": 1 })
    ));
    assert!(matches(
        task_of(&reconciliation, &failed["id"]),
        &json!({ "state": "failed", "admission": "failed", "usage": { "modelCalls": 1 } })
    ));
    assert!(matches(
        task_of(&reconciliation, &hanging["id"]),
        &json!({ "state": "cancelled", "admission": "cancelled" })
    ));
    assert_eq!(
        task_of(&reconciliation, &decision["claim"]["verifier_task_id"])["resultStatus"],
        "succeeded"
    );

    // Sum of per-task usage plus unattributed calls equals the ledger's own totals.
    let ledger = &status["usage"]["usage"];
    let sum = |field: &str| {
        reconciliation["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["usage"][field].as_f64().unwrap())
            .sum::<f64>()
            + reconciliation["unattributed"][field].as_f64().unwrap()
    };
    for field in ["calls", "taskCalls", "modelCalls"] {
        assert_eq!(sum(field), ledger[field].as_f64().unwrap(), "{field}");
    }
    assert!((sum("wallMs") - ledger["wallMs"].as_f64().unwrap()).abs() < 1e-6);
    assert_eq!(reconciliation["unattributed"]["calls"], 0);
    assert!(matches(
        ledger,
        &json!({ "taskCalls": 9, "modelCalls": 4, "calls": 13, "cost": null })
    ));

    // Timeline: every task is admitted before it settles, and receipts/decisions/verification appear.
    let timeline = reconciliation["timeline"].as_array().unwrap();
    for task in reconciliation["tasks"].as_array().unwrap() {
        let events: Vec<&Value> = timeline.iter().filter(|e| e["taskId"] == task["id"]).collect();
        let admitted = events.iter().find(|e| e["kind"] == "task.admitted").expect("admitted");
        let settled = events.iter().find(|e| e["kind"] == "task.settled").expect("settled");
        assert!(settled["at"].as_f64() >= admitted["at"].as_f64());
    }
    let kinds: Vec<&str> = timeline.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    for kind in [
        "model.call",
        "progress.receipt",
        "progress.decision",
        "goal.verification",
        "schedule.fired",
    ] {
        assert!(kinds.contains(&kind), "{kind}");
    }
    assert_eq!(
        timeline.iter().find(|e| e["kind"] == "progress.decision").unwrap()["detail"],
        "finished/verified"
    );
    let times: Vec<f64> = timeline.iter().map(|e| e["at"].as_f64().unwrap()).collect();
    assert!(times.windows(2).all(|w| w[0] <= w[1]));
    host.agents.close();
}

#[test]
fn acceptance_a24_a_crashed_owners_open_work_reconciles_as_interrupted_and_unknown_after_restart_and_tampering_is_detected()
 {
    let state = Stores::default();
    let first = build(&state);
    let done = first
        .call(
            "agents.spawn",
            json!({ "definition": "rlm-child@1", "input": { "prompt": "work" } }),
        )
        .unwrap();
    first.call("agents.result", json!({ "id": done["id"] })).unwrap();
    let open = first
        .call(
            "agents.spawn",
            json!({ "definition": "rlm-child@1", "input": { "prompt": "hang" } }),
        )
        .unwrap();
    agents_support::wait_for("the open task's model reservation", Duration::from_secs(120), || {
        records(&state)["activeReservations"] == 2
    });
    // Before the crash the ledger holds active task and model reservations for the open task.
    assert_eq!(records(&state)["activeReservations"], 2);

    // Crash: a new owner opens the same records; the old owner's writes are fenced.
    let restarted = build(&state);
    let status = restarted.call("agents.status", json!({})).unwrap();
    let reconciliation = records(&state);
    assert_eq!(reconciliation["discrepancies"], json!([]));
    assert_eq!(
        inspection_discrepancies(&reconciliation, &status).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(reconciliation["activeReservations"], 0);
    assert!(matches(
        task_of(&reconciliation, &open["id"]),
        &json!({
            "state": "interrupted", "resultStatus": "interrupted", "admission": "unknown",
            "usage": { "taskCalls": 1, "modelCalls": 1, "unknownCalls": 2, "cost": null },
        })
    ));
    assert!(matches(
        task_of(&reconciliation, &done["id"]),
        &json!({ "admission": "succeeded", "modelUsage": { "inputTokens": 150, "outputTokens": 30 } })
    ));

    // Negative controls: the checker is not vacuous.
    let mut usage = state.usage.get().unwrap();
    let root = usage["roots"].as_object_mut().unwrap().values_mut().next().unwrap();
    let calls = root["calls"].as_array_mut().unwrap();
    let dropped = calls.iter().position(|c| c["kind"] == "task").unwrap();
    calls.remove(dropped);
    let tasks = state.tasks.get();
    let tampered = reconcile_records(&ReconcileInput {
        tasks: tasks.as_ref(),
        usage: Some(&usage),
        ..ReconcileInput::default()
    })
    .unwrap();
    let text: Vec<&str> = tampered["discrepancies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d.as_str().unwrap())
        .collect();
    assert!(text.join("\n").contains("admission is missing"), "{text:?}");
    let mut forged = status.clone();
    forged["tasks"][0]["state"] = json!("failed");
    let calls = forged["usage"]["usage"]["calls"].as_f64().unwrap();
    forged["usage"]["usage"]["calls"] = json!(calls + 1.0);
    let problems = inspection_discrepancies(&reconciliation, &forged).unwrap();
    assert!(problems.iter().any(|p| p.contains("inspected failed")), "{problems:?}");
    assert!(
        problems.iter().any(|p| p.starts_with("inspected usage calls")),
        "{problems:?}"
    );
    restarted.agents.close();
    first.agents.close();
}
