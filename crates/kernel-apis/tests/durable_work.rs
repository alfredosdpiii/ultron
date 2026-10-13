//! Durable work in kernel-apis: the next fire time a reopened schedules module derives from its durable fields, and
//! the session goal reconciled with a task journal on reopen (every journal state a goal job can be in), plus the
//! ticker on a manual clock. Kill/reopen evidence over real processes is in crates/acceptance/tests/a23_durable_work.rs.

mod common;

use common::fixed;
use kernel::Signal;
use kernel_apis::goal::{GoalCheckResult, GoalDriver, GoalJobs, HOST_CLOSED, JobState};
use kernel_apis::schedules::next_fire_at;
use kernel_apis::util::lock;
use kernel_apis::{DocumentStore, ManualTickClock, MemoryDocumentStore, ScheduleTicker, SchedulesModule, TaskHost};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

/// Hang guard of every wait (never a timing expectation).
const HANG: Duration = Duration::from_secs(120);
use std::sync::{Arc, Mutex};

fn schedule(extra: Value) -> Value {
    let mut out = json!({"id": "schedule-1", "state": "active", "start_at": 10_000, "every_ms": 1000,
        "last_slot": null, "pending_slot": null, "goal_id": null});
    for (key, value) in extra.as_object().unwrap() {
        out[key] = value.clone();
    }
    out
}

#[test]
fn kernel_apis_schedules_next_fire_follows_the_durable_fields() {
    let none: &[Value] = &[];
    assert_eq!(next_fire_at(&schedule(json!({})), none), Some(10_000));
    assert_eq!(next_fire_at(&schedule(json!({"last_slot": 0})), none), Some(11_000));
    assert_eq!(next_fire_at(&schedule(json!({"last_slot": 5})), none), Some(16_000));
    assert_eq!(
        next_fire_at(&schedule(json!({"last_slot": 5, "pending_slot": 5})), none),
        Some(i64::MIN)
    );
    assert_eq!(next_fire_at(&schedule(json!({"state": "paused"})), none), None);
    assert_eq!(next_fire_at(&schedule(json!({"state": "completed"})), none), None);
    let goals = [
        json!({"id": "goal-a", "state": "active"}),
        json!({"id": "goal-p", "state": "paused"}),
    ];
    assert_eq!(
        next_fire_at(&schedule(json!({"goal_id": "goal-a"})), &goals),
        Some(10_000)
    );
    assert_eq!(
        next_fire_at(&schedule(json!({"goal_id": "goal-p", "pending_slot": 0})), &goals),
        None
    );
    assert_eq!(next_fire_at(&schedule(json!({"goal_id": "goal-gone"})), &goals), None);
}

/// A task host whose spawns are idempotent by key and complete at once.
#[derive(Default)]
struct Journal {
    tasks: Mutex<Vec<Value>>,
}

impl TaskHost for Journal {
    fn caller_task_id(&self, _: &str) -> Option<String> {
        None
    }
    fn task_lane(&self, _: &str) -> Option<String> {
        None
    }
    fn strategy(&self, _: &str) -> Result<String, String> {
        Ok("deterministic".into())
    }
    fn tasks(&self) -> Result<Vec<Value>, String> {
        Ok(lock(&self.tasks).clone())
    }
    fn spawn(&self, request: &Value, _: Option<&str>, _: &Signal) -> Result<Value, String> {
        let mut tasks = lock(&self.tasks);
        if let Some(task) = tasks.iter().find(|t| t["key"] == request["key"]) {
            return Ok(task.clone());
        }
        let task = json!({"id": format!("task-{}", tasks.len() + 1), "key": request["key"]});
        tasks.push(task.clone());
        Ok(task)
    }
    fn result(&self, _: &str) -> Result<Value, String> {
        Ok(json!({"status": "succeeded", "value": null}))
    }
    fn cancel(&self, _: &str, _: &str) -> Result<Value, String> {
        Ok(json!({}))
    }
    fn steer(&self, _: &str, _: &str, _: &Signal) -> Result<bool, String> {
        Ok(false)
    }
    fn now(&self) -> i64 {
        0
    }
}

#[test]
fn kernel_apis_schedule_ticker_fires_on_its_own_and_recomputes_after_reopen() {
    let store: Arc<MemoryDocumentStore> = Arc::default();
    let clock = ManualTickClock::new(10_000);
    let journal = Arc::new(Journal::default());
    let open = || {
        let module = Arc::new(SchedulesModule::new(store.clone(), Some(clock.as_clock())));
        module.start(journal.as_ref()).unwrap();
        let (reports, ticks) = mpsc::channel();
        let reports = Mutex::new(reports);
        let ticker = ScheduleTicker::start(
            module.clone(),
            journal.clone(),
            clock.clone(),
            Some(Arc::new(move |report: &Result<Value, String>| {
                let _ = lock(&reports).send(report.clone().unwrap());
            })),
        );
        (module, ticker, ticks)
    };
    let (module, ticker, ticks) = open();
    let created = module
        .handle(
            "schedules.create",
            json!({"definition": "identity@1", "input": {}, "every_ms": 1000, "max_runs": 3})
                .as_object()
                .unwrap(),
            None,
            journal.as_ref(),
            &Signal::new(),
        )
        .unwrap();
    let id = created["id"].clone();
    assert_eq!(
        ticks.recv_timeout(HANG).unwrap(),
        json!({"fired": [{"schedule_id": id, "slot": 0, "task_id": "task-1", "skipped": 0}], "failed": []})
    );
    assert_eq!(module.next_fire().unwrap(), Some(11_000));
    ticker.close();
    assert!(ticks.recv().is_err());

    // Reopened 2.5 slots later: the missed slots coalesce into one run, fired without a request.
    clock.set(12_500);
    let (module, ticker, ticks) = open();
    assert_eq!(
        ticks.recv_timeout(HANG).unwrap(),
        json!({"fired": [{"schedule_id": id, "slot": 2, "task_id": "task-2", "skipped": 1}], "failed": []})
    );
    clock.set(13_000);
    ticker.wake();
    assert_eq!(ticks.recv_timeout(HANG).unwrap()["fired"][0]["slot"], 3);
    // max_runs reached: completed, nothing more is due.
    assert_eq!(module.next_fire().unwrap(), None);
    ticker.close();
    let keys: Vec<Value> = journal.tasks().unwrap().iter().map(|t| t["key"].clone()).collect();
    let id = id.as_str().unwrap();
    assert_eq!(keys, [0, 2, 3].map(|slot| json!(format!("schedule:{id}:{slot}"))));
}

/// GoalJobs over a scripted journal: task id → its state, key → task id; every start and stop is logged.
#[derive(Default)]
struct Jobs {
    journal: Option<Mutex<HashMap<String, JobState>>>,
    keys: Mutex<HashMap<String, String>>,
    log: Mutex<Vec<Value>>,
}

impl Jobs {
    fn with_journal(states: &[(&str, JobState)], keys: &[(&str, &str)]) -> Arc<Jobs> {
        Arc::new(Jobs {
            journal: Some(Mutex::new(
                states.iter().map(|(id, s)| (id.to_string(), s.clone())).collect(),
            )),
            keys: Mutex::new(keys.iter().map(|(k, id)| (k.to_string(), id.to_string())).collect()),
            log: Mutex::default(),
        })
    }
    fn log(&self) -> Vec<Value> {
        lock(&self.log).clone()
    }
}

impl GoalJobs for Jobs {
    fn run_check(&self, _: &str, _: u64) -> Result<GoalCheckResult, String> {
        Ok(GoalCheckResult {
            exit_code: Some(1),
            timed_out: false,
            output: "1 failed".into(),
        })
    }
    fn start_job(&self, _: &str, key: &str, _: &str, _: &Signal) -> Result<String, String> {
        lock(&self.log).push(json!(["start", key]));
        Ok(format!("job-{key}"))
    }
    fn stop_job(&self, task_id: &str, reason: &str) -> Result<(), String> {
        lock(&self.log).push(json!(["stop", task_id, reason]));
        Ok(())
    }
    fn lane_of_job(&self, _: &str) -> Option<String> {
        None
    }
    fn job_state(&self, task_id: &str) -> Option<JobState> {
        let journal = lock(self.journal.as_ref()?);
        journal.get(task_id).cloned()
    }
    fn job_with_key(&self, key: &str) -> Option<String> {
        lock(&self.keys).get(key).cloned()
    }
}

fn saved_goal(extra: Value) -> Arc<MemoryDocumentStore> {
    let mut goal = json!({
        "id": "goal-0000abcd", "revision": 1, "objective": "ship it", "check": null, "status": "active",
        "reason": null, "job": "task-1", "runs": 1, "checks": 3, "stuck": 2, "last_signature": "x",
        "run_progress": false, "last_check": null, "last_answer": null, "created_at": 1, "updated_at": 1,
        "completion": null,
    });
    for (key, value) in extra.as_object().unwrap() {
        goal[key] = value.clone();
    }
    Arc::new(MemoryDocumentStore::new(Some(json!({"version": 1, "goal": goal}))))
}

fn recover(store: Arc<MemoryDocumentStore>, jobs: Arc<Jobs>) -> Value {
    let driver = GoalDriver::new(store.clone(), jobs, fixed(5));
    let goal = driver.recover().unwrap().unwrap();
    // What recovery found is durable.
    if goal["status"] == "active" {
        assert_eq!(store.read().unwrap().unwrap()["goal"], goal);
    }
    goal
}

fn pick(goal: &Value, keys: &[&str]) -> Value {
    Value::Object(keys.iter().map(|k| (k.to_string(), goal[*k].clone())).collect())
}

const FIELDS: &[&str] = &["status", "reason", "job", "runs", "checks", "stuck", "last_answer"];

#[test]
fn kernel_apis_goal_recovery_reconciles_the_goal_with_the_task_journal() {
    // Still running under this worker: kept as is.
    let jobs = Jobs::with_journal(&[("task-1", JobState::Running)], &[]);
    let goal = recover(saved_goal(json!({})), jobs.clone());
    assert_eq!(
        pick(&goal, FIELDS),
        json!({"status": "active", "reason": null, "job": "task-1", "runs": 1, "checks": 3, "stuck": 2, "last_answer": null})
    );
    assert_eq!(jobs.log(), Vec::<Value>::new());

    // Interrupted by the restart, or stopped because its host closed: the next run starts; checks are kept.
    for result in [
        json!({"status": "interrupted", "error": "Owner ended; automatic replay is disabled"}),
        json!({"status": "cancelled", "error": HOST_CLOSED}),
    ] {
        let jobs = Jobs::with_journal(&[("task-1", JobState::Ended(result))], &[]);
        let goal = recover(saved_goal(json!({})), jobs.clone());
        assert_eq!(
            pick(&goal, FIELDS),
            json!({"status": "active", "reason": null, "job": "job-goal-0000abcd:2", "runs": 2, "checks": 3, "stuck": 2,
                "last_answer": "The previous run was interrupted when Ultron restarted; continue from the goal's state and the files."})
        );
        assert_eq!(jobs.log(), vec![json!(["start", "goal-0000abcd:2"])]);
    }

    // Ended on its own while no worker ran: settled as task_ended would have (no progress: paused).
    let jobs = Jobs::with_journal(
        &[(
            "task-1",
            JobState::Ended(json!({"status": "succeeded", "value": "gave up"})),
        )],
        &[],
    );
    let goal = recover(saved_goal(json!({})), jobs.clone());
    assert_eq!(
        pick(&goal, FIELDS),
        json!({"status": "paused", "reason": "the goal job ended without progress; /goal resume continues", "job": null,
            "runs": 1, "checks": 3, "stuck": 2, "last_answer": "gave up"})
    );
    // ... and with progress: the next run.
    let jobs = Jobs::with_journal(
        &[("task-1", JobState::Ended(json!({"status": "failed", "error": "boom"})))],
        &[],
    );
    let goal = recover(saved_goal(json!({"run_progress": true})), jobs.clone());
    assert_eq!(
        pick(&goal, &["status", "job", "runs", "last_answer"]),
        json!({"status": "active", "job": "job-goal-0000abcd:2", "runs": 2, "last_answer": "boom"})
    );

    // The run was written but its task not recorded: the admitted task is adopted, not started again.
    let jobs = Jobs::with_journal(&[("task-9", JobState::Running)], &[("goal-0000abcd:2", "task-9")]);
    let goal = recover(saved_goal(json!({"job": null, "runs": 2})), jobs.clone());
    assert_eq!(
        pick(&goal, &["status", "job", "runs"]),
        json!({"status": "active", "job": "task-9", "runs": 2})
    );
    assert_eq!(jobs.log(), Vec::<Value>::new());
    // ... and when the admission never happened, the run starts now.
    let jobs = Jobs::with_journal(&[], &[]);
    let goal = recover(saved_goal(json!({"job": null, "runs": 2})), jobs.clone());
    assert_eq!(
        pick(&goal, &["status", "job", "runs"]),
        json!({"status": "active", "job": "job-goal-0000abcd:3", "runs": 3})
    );

    // A goal that is not active runs nothing.
    let jobs = Jobs::with_journal(&[("task-1", JobState::Ended(json!({"status": "interrupted"})))], &[]);
    let goal = recover(
        saved_goal(json!({"status": "paused", "reason": "paused by you; /goal resume continues"})),
        jobs.clone(),
    );
    assert_eq!(
        pick(&goal, &["status", "job", "runs"]),
        json!({"status": "paused", "job": "task-1", "runs": 1})
    );
    assert_eq!(jobs.log(), Vec::<Value>::new());

    // Without a task journal: TypeScript's behavior, the goal pauses.
    let goal = recover(saved_goal(json!({})), Arc::new(Jobs::default()));
    assert_eq!(
        pick(&goal, &["status", "reason", "job"]),
        json!({"status": "paused", "reason": "Ultron restarted while the goal job ran; /goal resume continues", "job": null})
    );
}

/// The run is written before its task is admitted: the document holds `runs` with no job while the start is in
/// flight, so a reopen finds the admitted task by `<goal id>:<runs>`.
#[test]
fn kernel_apis_goal_writes_the_run_before_admitting_its_task() {
    struct Watching {
        store: Arc<MemoryDocumentStore>,
        seen: Mutex<Option<Value>>,
    }
    impl GoalJobs for Watching {
        fn run_check(&self, _: &str, _: u64) -> Result<GoalCheckResult, String> {
            Err("unused".into())
        }
        fn start_job(&self, _: &str, key: &str, _: &str, _: &Signal) -> Result<String, String> {
            let goal = self.store.read().unwrap().unwrap()["goal"].clone();
            *lock(&self.seen) = Some(json!({"key": key, "runs": goal["runs"], "job": goal["job"]}));
            Ok("task-1".into())
        }
        fn stop_job(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn lane_of_job(&self, _: &str) -> Option<String> {
            None
        }
    }
    let store: Arc<MemoryDocumentStore> = Arc::default();
    let jobs = Arc::new(Watching {
        store: store.clone(),
        seen: Mutex::default(),
    });
    let driver = GoalDriver::new(store.clone(), jobs.clone(), fixed(5));
    driver.command("ship it", &Signal::new()).unwrap();
    let id = driver.get().unwrap().unwrap()["id"].as_str().unwrap().to_string();
    assert_eq!(
        lock(&jobs.seen).clone().unwrap(),
        json!({"key": format!("{id}:1"), "runs": 1, "job": null})
    );
    assert_eq!(store.read().unwrap().unwrap()["goal"]["job"], "task-1");
}

/// Every committed change calls the change hook (the ticker's wake-up), so a new or resumed schedule is seen without
/// waiting for a timeout; a rejected request commits nothing and calls nothing.
#[test]
fn kernel_apis_schedule_changes_wake_the_ticker() {
    let journal = Journal::default();
    let module = SchedulesModule::new(Arc::new(MemoryDocumentStore::default()), Some(fixed(10_000)));
    let calls = Arc::new(Mutex::new(0));
    let counted = calls.clone();
    module.on_change(Some(Arc::new(move || *lock(&counted) += 1)));
    let call =
        |kind: &str, payload: Value| module.handle(kind, payload.as_object().unwrap(), None, &journal, &Signal::new());
    let created = call(
        "schedules.create",
        json!({"definition": "identity@1", "input": {}, "every_ms": 1000}),
    )
    .unwrap();
    assert_eq!(*lock(&calls), 1);
    call("schedules.pause", json!({"id": created["id"]})).unwrap();
    call("schedules.resume", json!({"id": created["id"]})).unwrap();
    assert_eq!(*lock(&calls), 3);
    assert!(call("schedules.create", json!({"every_ms": 0})).is_err());
    assert_eq!(*lock(&calls), 3);
    module.on_change(None);
    call("schedules.delete", json!({"id": created["id"]})).unwrap();
    assert_eq!(*lock(&calls), 3);
}
