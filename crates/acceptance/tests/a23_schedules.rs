//! A23 (partial): schedules, goals and verification compose within budgets.
//! Ported from packages/coding-agent/test/ultron-schedules.test.ts with literal expectations: the real
//! `kernel_apis::SchedulesModule` behind `g3_support::Host`, whose firings are admitted by the real `harness` task
//! supervisor (slot keys `schedule:<id>:<slot>` as idempotency keys, restart recovery over the same journal, host
//! close). The fired tasks stay running on waiting lanes; these cases never read their results.
//! Ported: same-slot ticks run once, restart without re-running a claimed slot, coalesced missed ticks, max_runs and
//! pause, strict payloads.
//! The goal cases ("a paused goal stops attached schedule firings", "required gate", "max_tasks caps"), "admission
//! failure from the usage budget" and "the background timer fires due slots and stops on close" run over the Rust
//! typed-agent host (`delegation::agents::Agents`: its definition registry runs `identity@1` through the injected
//! deterministic adapter, as the TS fixture does, and its usage ledger admits tasks), with the
//! `kernel_apis::ScheduleTicker` on a manual clock as the background timer.

mod agents_support;
mod common;
mod g3_support;

use agents_support::{Options, adapter};
use common::null_handler;
use delegation::usage::{UsageLedger, UsageLimits, UsageOptions};
use g3_support::{Host, JournalStore, Tasks, rejects};
use kernel::{HostHandler, Signal};
use kernel_apis::{
    ApiModule, KernelApis, ManualTickClock, MemoryDocumentStore, ScheduleTicker, SchedulesModule, TaskHost,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc;

struct Fixture {
    host: Arc<Host>,
    tasks: Arc<Tasks>,
    clock: Arc<AtomicI64>,
}

fn fixture_on(module_store: Arc<MemoryDocumentStore>, journal: JournalStore, clock: Arc<AtomicI64>) -> Fixture {
    let now = clock.clone();
    let module = SchedulesModule::new(module_store, Some(Arc::new(move || now.load(Ordering::SeqCst))));
    let tasks = Tasks::new(journal, clock.clone());
    Fixture {
        host: Arc::new(Host::new(tasks.clone(), vec![Arc::new(module) as Arc<dyn ApiModule>])),
        tasks,
        clock,
    }
}

fn fixture() -> Fixture {
    fixture_on(
        Arc::default(),
        JournalStore::default(),
        Arc::new(AtomicI64::new(1_000_000)),
    )
}

impl Fixture {
    fn call(&self, kind: &str, payload: Value) -> Value {
        self.host.call(kind, payload).unwrap()
    }
    fn advance(&self, ms: i64) {
        self.clock.fetch_add(ms, Ordering::SeqCst);
    }
    fn create(&self, extra: Value) -> Value {
        let mut payload = json!({"definition": "identity@1", "input": {}, "every_ms": 1000});
        for (key, value) in extra.as_object().unwrap() {
            payload[key] = value.clone();
        }
        self.call("schedules.create", payload)
    }
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
fn acceptance_a23_duplicate_and_concurrent_ticks_in_the_same_slot_run_the_slot_once() {
    let f = fixture();
    let schedule = f.create(json!({"input": {"n": 1}}));
    let first = f.call("schedules.tick", json!({}));
    subset(
        &first["fired"],
        &json!([{"schedule_id": schedule["id"], "slot": 0, "skipped": 0}]),
    );
    f.advance(500);
    assert_eq!(f.call("schedules.tick", json!({}))["fired"], json!([]));
    f.advance(500);
    let ticks: Vec<_> = (0..2)
        .map(|_| {
            let host = f.host.clone();
            std::thread::spawn(move || host.call("schedules.tick", json!({})).unwrap())
        })
        .collect();
    let fired: Vec<Value> = ticks
        .into_iter()
        .flat_map(|t| t.join().unwrap()["fired"].as_array().unwrap().clone())
        .collect();
    subset(&Value::Array(fired), &json!([{"slot": 1}]));
    let id = schedule["id"].as_str().unwrap();
    let keys: Vec<_> = f.tasks.journal().iter().map(|t| t["key"].clone()).collect();
    assert_eq!(
        keys,
        vec![json!(format!("schedule:{id}:0")), json!(format!("schedule:{id}:1"))]
    );
    f.tasks.close();
}

#[test]
fn acceptance_a23_a_restart_on_the_same_stores_does_not_rerun_a_completed_or_claimed_slot() {
    let module_store: Arc<MemoryDocumentStore> = Arc::default();
    let journal = JournalStore::default();
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let before = fixture_on(module_store.clone(), journal.clone(), clock.clone());
    let schedule = before.create(json!({}));
    let fired = before.call("schedules.tick", json!({}));
    before.tasks.close();

    let after = fixture_on(module_store.clone(), journal.clone(), clock.clone());
    assert_eq!(after.call("schedules.tick", json!({}))["fired"], json!([]));
    assert_eq!(after.tasks.journal().len(), 1);
    after.tasks.close();

    // Crash after claiming but before recording the admission: completion reuses the slot key.
    use kernel_apis::DocumentStore;
    let mut saved = module_store.read().unwrap().unwrap();
    saved["schedules"][0]["pending_slot"] = 0.into();
    module_store.write(&saved).unwrap();
    let recovered = fixture_on(module_store, journal, clock);
    let report = recovered.call("schedules.tick", json!({}));
    assert_eq!(
        report["fired"],
        json!([{"schedule_id": schedule["id"], "slot": 0, "task_id": fired["fired"][0]["task_id"], "skipped": 0}])
    );
    assert_eq!(recovered.tasks.journal().len(), 1);
    recovered.tasks.close();
}

#[test]
fn acceptance_a23_missed_ticks_coalesce_into_one_run_for_the_latest_slot_and_record_the_skipped_count() {
    let f = fixture();
    let schedule = f.create(json!({}));
    f.call("schedules.tick", json!({}));
    f.advance(5500);
    let report = f.call("schedules.tick", json!({}));
    subset(
        &report["fired"],
        &json!([{"schedule_id": schedule["id"], "slot": 5, "skipped": 4}]),
    );
    let listed = f.call("schedules.list", json!({}));
    subset(
        &listed[0],
        &json!({"runs": 2, "last_slot": 5, "last_skipped": 4, "skipped_total": 4}),
    );
    assert_eq!(f.tasks.journal().len(), 2);
    f.tasks.close();
}

#[test]
fn acceptance_a23_max_runs_completes_a_schedule_and_pause_stops_firings() {
    let f = fixture();
    let schedule = f.create(json!({"max_runs": 2}));
    f.call("schedules.tick", json!({}));
    f.call("schedules.pause", json!({"id": schedule["id"]}));
    f.advance(1000);
    assert_eq!(f.call("schedules.tick", json!({}))["fired"], json!([]));
    f.call("schedules.resume", json!({"id": schedule["id"]}));
    assert_eq!(
        f.call("schedules.tick", json!({}))["fired"].as_array().unwrap().len(),
        1
    );
    f.advance(1000);
    assert_eq!(f.call("schedules.tick", json!({}))["fired"], json!([]));
    subset(
        &f.call("schedules.list", json!({}))[0],
        &json!({"state": "completed", "runs": 2}),
    );
    f.call("schedules.delete", json!({"id": schedule["id"]}));
    assert_eq!(f.call("schedules.list", json!({})), json!([]));
    f.tasks.close();
}

#[test]
fn acceptance_a23_rejects_unknown_payload_fields_and_forged_identity() {
    let f = fixture();
    let call = |kind: &str, payload: Value| f.host.call(kind, payload);
    rejects(
        call(
            "schedules.create",
            json!({"definition": "identity@1", "input": {}, "every_ms": 1000, "owner": "task-x"}),
        ),
        "Unknown payload field: owner",
    );
    rejects(
        call("schedules.tick", json!({"force": true})),
        "Unknown payload field: force",
    );
    rejects(
        call(
            "goals.create",
            json!({"title": "t", "required_checks": [], "state": "achieved"}),
        ),
        "Unknown payload field: state",
    );
    rejects(
        call("goals.verify", json!({"id": "goal-x", "passed": true})),
        "Unknown payload field: passed",
    );
    rejects(
        call("goals.create", json!({"title": "t", "required_checks": ["nope"]})),
        "id@version",
    );
    rejects(
        call(
            "schedules.create",
            json!({"definition": "identity@1", "input": {}, "every_ms": 0}),
        ),
        "every_ms",
    );
    let schedule = f.create(json!({}));
    assert_eq!(schedule["owner"], Value::Null);
    f.tasks.close();
}

/// The TS `fixture()` over the typed-agent host: `identity@1` runs through the deterministic adapter, which echoes its
/// input and fails when the input asks it to ("check crashed").
struct Typed {
    fixture: agents_support::Fixture,
    apis: KernelApis,
    schedules: Arc<SchedulesModule>,
    clock: Arc<ManualTickClock>,
}

fn typed(usage: Option<UsageLedger>) -> Typed {
    let clock = ManualTickClock::new(1_000_000);
    let fixture = agents_support::fixture(Options {
        usage,
        deterministic: Some(adapter(|_, input, _, _| {
            if input["fail"] == true {
                return Err("check crashed".into());
            }
            Ok(input.clone())
        })),
        ..Options::default()
    });
    let schedules = Arc::new(SchedulesModule::new(
        Arc::new(MemoryDocumentStore::default()),
        Some(clock.as_clock()),
    ));
    let apis = KernelApis::new(Arc::new(fixture.agents.handler("main", null_handler())) as Arc<dyn HostHandler>)
        .with_host(Arc::new(fixture.agents.clone()), "main")
        .with(schedules.clone());
    Typed {
        fixture,
        apis,
        schedules,
        clock,
    }
}

impl Typed {
    fn try_call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        let Value::Object(payload) = payload else {
            unreachable!()
        };
        self.apis.handle(kind, payload, &Signal::new())
    }
    fn call(&self, kind: &str, payload: Value) -> Value {
        self.try_call(kind, payload).unwrap_or_else(|e| panic!("{kind}: {e}"))
    }
    fn tasks(&self) -> Vec<Value> {
        self.fixture.agents.tasks().unwrap()
    }
    fn advance(&self, ms: i64) {
        self.clock.advance(ms);
    }
}

#[test]
fn acceptance_a23_a_paused_goal_stops_attached_schedule_firings_and_resume_re_enables_them() {
    let f = typed(None);
    let goal = f.call(
        "goals.create",
        json!({"title": "keep green", "required_checks": ["identity@1"]}),
    );
    let schedule = f.call(
        "schedules.create",
        json!({"definition": "identity@1", "input": {"poll": true}, "every_ms": 1000, "goal_id": goal["id"]}),
    );
    f.call("goals.pause", json!({"id": goal["id"]}));
    assert_eq!(f.call("schedules.tick", json!({}))["fired"], json!([]));
    f.advance(2000);
    assert_eq!(f.call("schedules.tick", json!({}))["fired"], json!([]));
    rejects(f.try_call("goals.verify", json!({"id": goal["id"]})), "Goal is paused");
    assert_eq!(f.tasks(), Vec::<Value>::new());

    f.call("goals.resume", json!({"id": goal["id"]}));
    let report = f.call("schedules.tick", json!({}));
    subset(
        &report["fired"],
        &json!([{"schedule_id": schedule["id"], "slot": 2, "skipped": 2}]),
    );
    assert_eq!(
        f.call("goals.get", json!({"id": goal["id"]}))["task_ids"],
        json!([report["fired"][0]["task_id"]])
    );

    // An achieved goal is finished: its schedules stop.
    f.call("goals.verify", json!({"id": goal["id"], "input": {"passed": true}}));
    f.advance(1000);
    assert_eq!(f.call("schedules.tick", json!({}))["fired"], json!([]));
    f.fixture.agents.close();
}

#[test]
fn acceptance_a23_required_gate_achieved_only_when_every_required_check_succeeds_with_passed_true() {
    let f = typed(None);
    let goal = f.call(
        "goals.create",
        json!({"title": "ship", "required_checks": ["identity@1"]}),
    );
    let verify = |input: Value| {
        let mut payload = json!({"id": goal["id"]});
        if !input.is_null() {
            payload["input"] = input;
        }
        f.call("goals.verify", payload)
    };
    subset(
        &verify(json!({"passed": false})),
        &json!({"state": "active", "verification": {"status": "unachieved"}}),
    );
    subset(
        &verify(json!({"passed": "yes"}))["verification"],
        &json!({"status": "unachieved", "reasons": ["identity@1 did not report a boolean passed value"]}),
    );
    assert_eq!(verify(Value::Null)["verification"]["status"], "unachieved");
    let crashed = verify(json!({"fail": true, "passed": true}));
    subset(
        &crashed["verification"]["checks"],
        &json!([{"status": "failed", "passed": false}]),
    );
    assert_eq!(crashed["state"], "active");

    let passed = verify(json!({"passed": true}));
    subset(
        &passed,
        &json!({"state": "achieved", "verification": {"status": "achieved", "round": 5}}),
    );
    assert_eq!(passed["task_ids"].as_array().unwrap().len(), 5);
    rejects(
        f.try_call("goals.verify", json!({"id": goal["id"]})),
        "already achieved",
    );

    // One unknown check keeps the gate closed even when the others pass.
    let partial = f.call(
        "goals.create",
        json!({"title": "partial", "required_checks": ["identity@1", "not-registered@1"]}),
    );
    let partial_result = f.call("goals.verify", json!({"id": partial["id"], "input": {"passed": true}}));
    assert_eq!(partial_result["state"], "active");
    subset(
        &partial_result["verification"]["checks"],
        &json!([
            {"definition": "identity@1", "status": "succeeded", "passed": true},
            {"definition": "not-registered@1", "status": "missing", "passed": false, "task_id": null},
        ]),
    );

    let unchecked = f.call("goals.create", json!({"title": "vibes", "required_checks": []}));
    subset(
        &f.call(
            "goals.verify",
            json!({"id": unchecked["id"], "input": {"passed": true}}),
        ),
        &json!({"state": "active", "verification": {"status": "unverified"}}),
    );
    f.fixture.agents.close();
}

#[test]
fn acceptance_a23_max_tasks_caps_schedule_firings_verification_and_attachments() {
    let f = typed(None);
    let goal = f.call(
        "goals.create",
        json!({"title": "capped", "required_checks": ["identity@1"], "max_tasks": 2}),
    );
    let schedule = f.call(
        "schedules.create",
        json!({"definition": "identity@1", "input": {}, "every_ms": 1000, "goal_id": goal["id"]}),
    );
    f.call("schedules.tick", json!({}));
    f.advance(1000);
    f.call("schedules.tick", json!({}));
    f.advance(1000);
    let capped = f.call("schedules.tick", json!({}));
    assert_eq!(
        capped["failed"],
        json!([{"schedule_id": schedule["id"], "slot": 2, "error": "Goal task limit reached (2)"}])
    );
    rejects(
        f.try_call("goals.verify", json!({"id": goal["id"], "input": {"passed": true}})),
        "Goal task limit reached",
    );
    let extra = f.call("agents.spawn", json!({"definition": "identity@1", "input": {}}));
    rejects(
        f.try_call("goals.attach", json!({"id": goal["id"], "task_id": extra["id"]})),
        "Goal task limit",
    );
    assert_eq!(
        f.call("goals.get", json!({"id": goal["id"]}))["task_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(f.tasks().len(), 3);
    f.fixture.agents.close();
}

#[test]
fn acceptance_a23_admission_failure_from_the_usage_budget_is_recorded_once_per_slot_without_a_retry_storm() {
    let usage = UsageLedger::new(
        Box::new(delegation::task_store::MemoryStore::default()),
        UsageOptions {
            limits: UsageLimits {
                max_admitted_tasks: Some(0),
                ..UsageLimits::default()
            },
            ..UsageOptions::default()
        },
    )
    .unwrap();
    let f = typed(Some(usage));
    let schedule = f.call(
        "schedules.create",
        json!({"definition": "identity@1", "input": {}, "every_ms": 1000}),
    );
    let failed = f.call("schedules.tick", json!({}));
    subset(&failed["failed"], &json!([{"schedule_id": schedule["id"], "slot": 0}]));
    assert!(
        failed["failed"][0]["error"]
            .as_str()
            .unwrap()
            .contains("admitted-task limit"),
        "{failed}"
    );
    for _ in 0..5 {
        assert_eq!(f.call("schedules.tick", json!({})), json!({"fired": [], "failed": []}));
    }
    f.advance(1000);
    subset(&f.call("schedules.tick", json!({}))["failed"], &json!([{"slot": 1}]));
    subset(
        &f.call("schedules.list", json!({}))[0],
        &json!({"runs": 0, "failures": 2, "last_slot": 1, "pending_slot": null, "last_failure": {"slot": 1}}),
    );
    assert_eq!(f.tasks().len(), 0);
    f.fixture.agents.close();
}

/// TS "the background timer fires due slots and stops on close": the ticker fires slot 0 on its own (no
/// `schedules.tick` request), each later slot when the clock reaches it, and nothing after it is closed.
#[test]
fn acceptance_a23_the_background_timer_fires_due_slots_and_stops_on_close() {
    let f = typed(None);
    let (reports, ticks) = mpsc::channel();
    let ticker = ScheduleTicker::start(
        f.schedules.clone(),
        Arc::new(f.fixture.agents.clone()) as Arc<dyn TaskHost>,
        f.clock.clone(),
        Some(Arc::new(move |report: &Result<Value, String>| {
            let _ = reports.send(report.clone());
        })),
    );
    let next = || ticks.recv_timeout(common::HANG).expect("a tick").unwrap();
    let schedule = f.call(
        "schedules.create",
        json!({"definition": "identity@1", "input": {}, "every_ms": 1000}),
    );
    subset(
        &next()["fired"],
        &json!([{"schedule_id": schedule["id"], "slot": 0, "skipped": 0}]),
    );
    let id = schedule["id"].as_str().unwrap();
    let keys = |f: &Typed| -> Vec<Value> { f.tasks().iter().map(|t| t["key"].clone()).collect() };
    assert_eq!(keys(&f), vec![json!(format!("schedule:{id}:0"))]);
    // Not yet due: waking the ticker fires nothing.
    f.advance(999);
    ticker.wake();
    f.advance(1);
    ticker.wake();
    subset(&next()["fired"], &json!([{"slot": 1, "skipped": 0}]));
    ticker.close();
    f.advance(5000);
    ticker.wake();
    // Closing joined the tick thread and dropped its observer: no report was or will be sent.
    assert!(ticks.recv().is_err(), "no tick after close");
    assert_eq!(
        keys(&f),
        vec![json!(format!("schedule:{id}:0")), json!(format!("schedule:{id}:1"))]
    );
    f.fixture.agents.close();
}
