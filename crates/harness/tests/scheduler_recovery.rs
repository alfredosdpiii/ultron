//! Port of upstream Pi Durable `test/harness-tasks-recovery.test.ts`, each case run against Memory, SQLite and
//! JSONL storage. Close/reopen goes through the same database (a fresh SQLite connection / JSONL open); a crash
//! abandons the Harness without close, drops its held commit, and reopens what was committed.

mod support;

use durable::{
    DocDefinition, DocSemantics, Owner, StorageError, StorageWrite, TaskId, TaskOptions, TaskQuery, TaskStatus,
};
use harness::scheduler::{
    AbortResult, Extension, Harness, HarnessError, HarnessOptions, Registry, RegistryReader, RegistrySnapshot, Task,
    TaskInspectionState,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use support::*;

fn create_in(harness: &Harness, task: &Task, input: Value) -> TaskId {
    let root = harness.root().unwrap();
    root.commit(|tx| Ok(tx.create_task(task.definition(), input, TaskOptions::default())?))
        .unwrap()
}

/// Fake external service whose operations are idempotent by request key.
#[derive(Default)]
struct TransferService {
    applied: Mutex<HashMap<String, u64>>,
    calls: AtomicUsize,
}

impl TransferService {
    fn apply(&self, key: &str, amount: u64) -> u64 {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self
            .applied
            .lock()
            .unwrap()
            .entry(key.to_owned())
            .or_insert(amount * 10)
    }
}

/// Intent/effect/outcome task: `prepare` commits the intent, `apply` performs the effect and commits the outcome.
/// While `interrupt` is set, the first `apply` blocks after the effect until the invocation is signalled.
fn transfer_task(service: Arc<TransferService>, interrupt: Arc<Mutex<bool>>) -> Task {
    Task::define("test.transfer", 1, |_| json!({ "phase": "prepare" }))
        .phase("prepare", |task, runtime| {
            runtime.memo_set("requested", task.input["amount"].clone(), &runtime.ctx())?;
            let key = format!("transfer-{}", task.id.0);
            runtime.commit(&runtime.ctx(), |_, _| {
                Ok(running(json!({ "phase": "apply", "key": key })))
            })
        })
        .phase("apply", move |task, runtime| {
            let key = js(&task.state)["checkpoint"]["key"].as_str().unwrap().to_owned();
            let receipt = service.apply(&key, task.input["amount"].as_u64().unwrap());
            if std::mem::take(&mut *interrupt.lock().unwrap()) {
                aborted(runtime)?;
            }
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(json!({ "receipt": receipt }))))
        })
        .abort(|_, runtime| {
            runtime.commit(&runtime.ctx(), |_, _| {
                Ok(Some(harness::scheduler::NextState::Terminal {
                    outcome: durable::TaskOutcome::Aborted {
                        reason: None,
                        result: None,
                    },
                }))
            })
        })
        .build()
}

/// A versioned one-phase task that completes with `result`, optionally migrating older records.
fn versioned(version: u64, result: &str, migrate: Option<harness::scheduler::Migrate>) -> Task {
    let (r1, r2) = (result.to_owned(), result.to_owned());
    let mut builder = Task::define("test.versioned", version, |_| step())
        .phase("run", move |_, runtime| {
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(json!(r1))))
        })
        .abort(move |_, runtime| runtime.commit(&runtime.ctx(), |_, _| Ok(aborted_with(&r2))));
    if let Some(migrate) = migrate {
        builder = builder.migrate(move |input, checkpoint, from| migrate(input, checkpoint, from));
    }
    builder.build()
}

fn status(harness: &Harness, id: TaskId) -> TaskStatus {
    harness.get_task(id).unwrap().unwrap().state.status()
}

// ─── task recovery ──────────────────────────────────────────────────────────────

#[test]
fn harness_recovery_resumes_an_intent_effect_outcome_task_interrupted_after_its_intent() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let service = Arc::new(TransferService::default());
        let transfer = transfer_task(service.clone(), Arc::new(Mutex::new(true)));
        let first = open_tasks(storage.clone(), vec![transfer.clone()], OpenOptions::default()).unwrap();
        let id = create_in(&first.harness, &transfer, json!({ "amount": 7 }));
        first.harness.resume().unwrap();
        eventually(|| service.calls.load(Ordering::SeqCst) == 1);
        first.harness.close().unwrap();

        let second = open_tasks(storage.reopen(), vec![transfer.clone()], OpenOptions::default()).unwrap();
        // Open reconciled `running` to `pending` and kept the checkpoint and memos; nothing ran yet.
        assert_matches(
            &js(&second.harness.get_task(id).unwrap().unwrap()),
            json!({
                "abortRequested": false,
                "memos": { "requested": 7 },
                "state": { "status": "pending", "checkpoint": { "phase": "apply", "key": format!("transfer-{}", id.0) } },
            }),
        );
        assert_eq!(service.calls.load(Ordering::SeqCst), 1);
        second.harness.resume().unwrap();
        let receipt = second.harness.wait_for_task(id, &bg()).unwrap();
        assert_eq!(
            outcome(&receipt),
            json!({ "status": "completed", "result": { "receipt": 70 } })
        );
        assert_eq!(service.calls.load(Ordering::SeqCst), 2);
        assert_eq!(service.applied.lock().unwrap().len(), 1);
        second.harness.close().unwrap();

        let third = open_tasks(storage.reopen(), vec![transfer], OpenOptions::default()).unwrap();
        assert_eq!(third.harness.get_task(id).unwrap(), Some(receipt));
        third.harness.close().unwrap();
    });
}

// #10549
#[test]
fn harness_recovery_keeps_started_at_across_close_and_reopen_and_stamps_ended_at() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let service = Arc::new(TransferService::default());
        let transfer = transfer_task(service.clone(), Arc::new(Mutex::new(true)));
        let clock = Arc::new(Mutex::new(2_000u64));
        let options = || {
            let clock = clock.clone();
            OpenOptions {
                now: Some(Arc::new(move || *clock.lock().unwrap())),
                ..OpenOptions::default()
            }
        };
        let first = open_tasks(storage.clone(), vec![transfer.clone()], options()).unwrap();
        let id = create_in(&first.harness, &transfer, json!({ "amount": 7 }));
        first.harness.resume().unwrap();
        eventually(|| service.calls.load(Ordering::SeqCst) == 1);
        *clock.lock().unwrap() = 3_000;
        first.harness.close().unwrap();

        *clock.lock().unwrap() = 9_000;
        let second = open_tasks(storage.reopen(), vec![transfer], options()).unwrap();
        let reopened = second.harness.get_task(id).unwrap().unwrap();
        assert_eq!(
            (reopened.state.status(), reopened.started_at, reopened.ended_at),
            (TaskStatus::Pending, Some(2_000), None)
        );
        second.harness.resume().unwrap();
        let receipt = second.harness.wait_for_task(id, &bg()).unwrap();
        assert_eq!((receipt.started_at, receipt.ended_at), (Some(2_000), Some(9_000)));
        second.harness.close().unwrap();
    });
}

#[test]
fn harness_recovery_resumes_abort_work_after_close_at_every_direct_task_abort_stage() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let block = Arc::new(Mutex::new(true));
        let (abort_reached, run_release) = (Deferred::new(), Deferred::new());
        let (l1, l2, b, ar, rr) = (
            log.clone(),
            log.clone(),
            block.clone(),
            abort_reached.clone(),
            run_release.clone(),
        );
        let abortable = Task::define("test.abortable", 1, |_| step())
            .phase("run", move |_, _| {
                l1.lock().unwrap().push("run".into());
                // Ignores the abort signal until released, so the mark is durable while the run is active.
                rr.wait();
                Ok(())
            })
            .abort(move |_, runtime| {
                l2.lock().unwrap().push("abort".into());
                if *b.lock().unwrap() {
                    ar.resolve();
                    aborted(runtime)?;
                }
                runtime.commit(&runtime.ctx(), |_, _| Ok(aborted_with("stop")))
            })
            .build();
        let log_now = || log.lock().unwrap().clone();

        // Stage 1: the mark is committed while the run invocation is still active.
        let opened = open_tasks(storage.clone(), vec![abortable.clone()], OpenOptions::default()).unwrap();
        let id = create_in(&opened.harness, &abortable, Value::Null);
        opened.harness.resume().unwrap();
        eventually(|| log.lock().unwrap().len() == 1);
        let h = opened.harness.clone();
        let aborting = spawn(move || h.abort_task(id, &bg()));
        eventually(|| opened.harness.get_task(id).unwrap().unwrap().abort_requested);
        let h = opened.harness.clone();
        let closing = spawn(move || h.close());
        flush();
        run_release.resolve();
        closing.join().unwrap().unwrap();
        assert_eq!(aborting.join().unwrap().unwrap(), AbortResult::Marked);
        assert_eq!(log_now(), vec!["run"]);

        // Stage 2: reopen dispatches the abort invocation, never the run; close while it is active.
        let opened = open_tasks(storage.reopen(), vec![abortable.clone()], OpenOptions::default()).unwrap();
        assert_matches(
            &js(&opened.harness.get_task(id).unwrap().unwrap()),
            json!({ "abortRequested": true, "state": { "status": "pending" } }),
        );
        opened.harness.resume().unwrap();
        abort_reached.wait();
        opened.harness.close().unwrap();
        assert_eq!(log_now(), vec!["run", "abort"]);

        // Stage 3: a fresh abort invocation settles the task.
        *block.lock().unwrap() = false;
        let opened = open_tasks(storage.reopen(), vec![abortable.clone()], OpenOptions::default()).unwrap();
        opened.harness.resume().unwrap();
        assert_eq!(
            outcome(&opened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "aborted", "reason": "stop" })
        );
        opened.harness.close().unwrap();
        assert_eq!(log_now(), vec!["run", "abort", "abort"]);

        // Stage 4: the terminal receipt survives reopen and nothing runs again.
        let opened = open_tasks(storage.reopen(), vec![abortable], OpenOptions::default()).unwrap();
        opened.harness.resume().unwrap();
        flush();
        assert_eq!(opened.harness.abort_task(id, &bg()).unwrap(), AbortResult::Terminal);
        opened.harness.close().unwrap();
        assert_eq!(log_now(), vec!["run", "abort", "abort"]);
    });
}

// ─── task crash recovery ─────────────────────────────────────────────────────────
//
// The crashed Harness is abandoned without close, its held storage commit never lands, and its blocked handlers
// never return. A new Harness then opens the same storage.

type Log = Arc<Mutex<Vec<String>>>;

/// A task whose run ignores its signal and whose abort handler blocks while `block_abort` is set.
fn crash_task(log: &Log, block_abort: bool) -> Task {
    let (l1, l2) = (log.clone(), log.clone());
    Task::define("test.crash", 1, |_| step())
        .phase("run", move |_, _| {
            l1.lock().unwrap().push("run".into());
            Deferred::new().wait();
            Ok(())
        })
        .abort(move |_, runtime| {
            l2.lock().unwrap().push("abort".into());
            if block_abort {
                Deferred::new().wait();
            }
            runtime.commit(&runtime.ctx(), |_, _| Ok(aborted_with("recovered")))
        })
        .build()
}

fn recover(storage: &Controlled, log: &Log) -> (Opened, TaskId) {
    let reopened = storage.crash();
    let opened = open_tasks(reopened, vec![crash_task(log, false)], OpenOptions::default()).unwrap();
    let query = TaskQuery {
        kind: Some("test.crash".into()),
        ..TaskQuery::default()
    };
    let page = opened.harness.commit(|tx| Ok(tx.scan_tasks(&query, 1, None)?)).unwrap();
    let id = page.items[0].id;
    (opened, id)
}

fn crashed_run(backend: Backend, log: &Log) -> (Controlled, Opened, TaskId) {
    let storage = Controlled::new(backend);
    let opened = open_tasks(storage.clone(), vec![crash_task(log, true)], OpenOptions::default()).unwrap();
    let id = create_in(&opened.harness, &crash_task(log, true), Value::Null);
    opened.harness.resume().unwrap();
    eventually(|| log.lock().unwrap().len() == 1);
    (storage, opened, id)
}

fn logged(log: &Log) -> Vec<String> {
    log.lock().unwrap().clone()
}

#[test]
fn harness_crash_while_the_mark_commit_is_in_storage_the_run_resumes() {
    each_backend(|backend| {
        let log: Log = Arc::default();
        let (storage, opened, id) = crashed_run(backend, &log);
        let held = storage.hold_commits();
        let h = opened.harness.clone();
        spawn(move || h.abort_task(id, &bg()));
        held.wait_entered();
        let (recovered, _) = recover(&storage, &log);
        assert_matches(
            &js(&recovered.harness.get_task(id).unwrap().unwrap()),
            json!({ "abortRequested": false, "state": { "status": "pending" } }),
        );
        recovered.harness.resume().unwrap();
        eventually(|| log.lock().unwrap().len() == 2);
        assert_eq!(logged(&log), vec!["run", "run"]);
    });
}

#[test]
fn harness_crash_after_the_mark_before_the_run_joins_only_the_abort_handler_runs() {
    each_backend(|backend| {
        let log: Log = Arc::default();
        let (storage, opened, id) = crashed_run(backend, &log);
        // The run ignores its signal, so abort_task never finishes joining it.
        let h = opened.harness.clone();
        let joining = spawn(move || h.abort_task(id, &bg()));
        eventually(|| opened.harness.get_task(id).unwrap().unwrap().abort_requested);
        flush();
        assert!(!joining.is_finished());
        let (recovered, _) = recover(&storage, &log);
        assert_matches(
            &js(&recovered.harness.get_task(id).unwrap().unwrap()),
            json!({ "abortRequested": true, "state": { "status": "pending" } }),
        );
        recovered.harness.resume().unwrap();
        assert_eq!(
            outcome(&recovered.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "aborted", "reason": "recovered" })
        );
        assert_eq!(logged(&log), vec!["run", "abort"]);
        recovered.harness.close().unwrap();
    });
}

#[test]
fn harness_crash_while_the_abort_handler_runs_a_fresh_abort_invocation_settles_the_task() {
    each_backend(|backend| {
        let log: Log = Arc::default();
        let storage = Controlled::new(backend);
        let crash = crash_task(&log, true);
        let opened = open_tasks(storage.clone(), vec![crash.clone()], OpenOptions::default()).unwrap();
        let id = create_in(&opened.harness, &crash, Value::Null);
        opened.harness.abort_task(id, &bg()).unwrap();
        opened.harness.resume().unwrap();
        eventually(|| log.lock().unwrap().len() == 1);
        assert_eq!(logged(&log), vec!["abort"]);
        assert_eq!(status(&opened.harness, id), TaskStatus::Running);
        let (recovered, _) = recover(&storage, &log);
        recovered.harness.resume().unwrap();
        assert_eq!(
            outcome(&recovered.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "aborted", "reason": "recovered" })
        );
        assert_eq!(logged(&log), vec!["abort", "abort"]);
        recovered.harness.close().unwrap();
    });
}

#[test]
fn harness_crash_while_the_abort_outcome_is_in_storage_the_abort_handler_runs_again() {
    each_backend(|backend| {
        let log: Log = Arc::default();
        let storage = Controlled::new(backend);
        let (reached, proceed) = (Deferred::new(), Deferred::new());
        let (l, re, pr) = (log.clone(), reached.clone(), proceed.clone());
        let crash = Task::define("test.crash", 1, |_| step())
            .phase("run", |_, _| Ok(()))
            .abort(move |_, runtime| {
                l.lock().unwrap().push("abort".into());
                re.resolve();
                pr.wait();
                runtime.commit(&runtime.ctx(), |_, _| Ok(aborted_with("lost")))
            })
            .build();
        let opened = open_tasks(storage.clone(), vec![crash.clone()], OpenOptions::default()).unwrap();
        let id = create_in(&opened.harness, &crash, Value::Null);
        opened.harness.abort_task(id, &bg()).unwrap();
        opened.harness.resume().unwrap();
        reached.wait();
        let held = storage.hold_commits();
        proceed.resolve();
        held.wait_entered();
        let (recovered, _) = recover(&storage, &log);
        recovered.harness.resume().unwrap();
        assert_eq!(
            outcome(&recovered.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "aborted", "reason": "recovered" })
        );
        assert_eq!(logged(&log), vec!["abort", "abort"]);
        recovered.harness.close().unwrap();
    });
}

#[test]
fn harness_crash_after_the_terminal_outcome_nothing_runs_again() {
    each_backend(|backend| {
        let log: Log = Arc::default();
        let storage = Controlled::new(backend);
        let crash = crash_task(&log, false);
        let opened = open_tasks(storage.clone(), vec![crash.clone()], OpenOptions::default()).unwrap();
        let id = create_in(&opened.harness, &crash, Value::Null);
        opened.harness.abort_task(id, &bg()).unwrap();
        opened.harness.resume().unwrap();
        opened.harness.wait_for_task(id, &bg()).unwrap();
        let (recovered, _) = recover(&storage, &log);
        recovered.harness.resume().unwrap();
        flush();
        assert_eq!(logged(&log), vec!["abort"]);
        assert_eq!(
            js(&recovered.harness.get_task(id).unwrap().unwrap().state),
            json!({ "status": "terminal", "outcome": { "status": "aborted", "reason": "recovered" } })
        );
        recovered.harness.close().unwrap();
    });
}

#[test]
fn harness_crash_while_the_reservation_commit_is_in_storage_the_task_is_still_pending() {
    each_backend(|backend| {
        let log: Log = Arc::default();
        let storage = Controlled::new(backend);
        let crash = crash_task(&log, false);
        let opened = open_tasks(storage.clone(), vec![crash.clone()], OpenOptions::default()).unwrap();
        let id = create_in(&opened.harness, &crash, Value::Null);
        let held = storage.hold_commits();
        opened.harness.resume().unwrap();
        held.wait_entered();
        let (recovered, _) = recover(&storage, &log);
        assert_eq!(status(&recovered.harness, id), TaskStatus::Pending);
        assert!(logged(&log).is_empty());
        recovered.harness.abort_task(id, &bg()).unwrap();
        recovered.harness.resume().unwrap();
        recovered.harness.wait_for_task(id, &bg()).unwrap();
        assert_eq!(logged(&log), vec!["abort"]);
        recovered.harness.close().unwrap();
    });
}

// ─── blocked tasks ───────────────────────────────────────────────────────────────

#[test]
fn harness_blocked_keeps_a_task_with_a_missing_definition_pending_and_live_until_registration() {
    each_backend(|backend| {
        let v1 = versioned(1, "v1", None);
        let opened = open_tasks(Controlled::new(backend), vec![], OpenOptions::default()).unwrap();
        let id = create_in(&opened.harness, &v1, Value::Null);
        opened.harness.resume().unwrap();
        flush();
        assert_eq!(status(&opened.harness, id), TaskStatus::Pending);
        let h = opened.harness.clone();
        let idle = spawn(move || h.wait_for_idle(&bg()));
        assert!(!settled(&idle));
        opened
            .registry
            .install(Extension::tasks("task:test.versioned", vec![v1]))
            .unwrap();
        idle.join().unwrap().unwrap();
        assert_eq!(
            js(&opened.harness.get_task(id).unwrap().unwrap().state),
            json!({ "status": "terminal", "outcome": { "status": "completed", "result": "v1" } })
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_blocked_keeps_a_task_stored_by_a_newer_version_pending_until_a_fitting_definition() {
    each_backend(|backend| {
        let registry = Registry::new();
        registry
            .install(Extension::tasks("task:test.versioned", vec![versioned(1, "old", None)]))
            .unwrap();
        let opened = open_tasks(
            Controlled::new(backend),
            vec![],
            OpenOptions {
                registry: Some(registry.clone()),
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let id = create_in(&opened.harness, &versioned(2, "new", None), Value::Null);
        opened.harness.resume().unwrap();
        flush();
        assert_eq!(status(&opened.harness, id), TaskStatus::Pending);
        // The same extension name replaces the old one in place.
        registry
            .install(Extension::tasks("task:test.versioned", vec![versioned(2, "new", None)]))
            .unwrap();
        assert_eq!(
            outcome(&opened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "completed", "result": "new" })
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_blocked_migrates_at_reservation_and_retries_a_failed_migration_only_for_a_new_definition() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let opened = open_tasks(storage.clone(), vec![], OpenOptions::default()).unwrap();
        let id = create_in(&opened.harness, &versioned(1, "v1", None), Value::Null);
        opened.harness.close().unwrap();

        let registry = Registry::new();
        let failures = Arc::new(AtomicUsize::new(0));
        let f = failures.clone();
        let failing: harness::scheduler::Migrate = Arc::new(move |_, _, _| {
            f.fetch_add(1, Ordering::SeqCst);
            Err(HarnessError::msg("cannot migrate"))
        });
        registry
            .install(Extension::tasks(
                "task:test.versioned",
                vec![versioned(2, "v2", Some(failing))],
            ))
            .unwrap();
        let opened = open_tasks(
            storage.reopen(),
            vec![],
            OpenOptions {
                registry: Some(registry.clone()),
                ..OpenOptions::default()
            },
        )
        .unwrap();
        opened.harness.resume().unwrap();
        eventually(|| reports(&opened).len() == 1);
        // An unrelated registry change wakes the scheduler without retrying the same failed definition.
        registry
            .install(Extension {
                name: "tool:unrelated".into(),
                tasks: vec![],
                tools: vec!["unrelated".into()],
            })
            .unwrap();
        flush();
        assert_eq!(failures.load(Ordering::SeqCst), 1);
        assert_eq!(reports(&opened), vec![HarnessError::msg("cannot migrate")]);
        assert_matches(
            &js(&opened.harness.get_task(id).unwrap().unwrap()),
            json!({ "version": 1, "state": { "status": "pending", "checkpoint": { "phase": "run" } } }),
        );

        let migrations = Arc::new(Mutex::new(Vec::new()));
        let m = migrations.clone();
        let migrating: harness::scheduler::Migrate = Arc::new(move |_, checkpoint, from| {
            m.lock().unwrap().push(from);
            Ok((Value::Null, checkpoint.clone()))
        });
        // The same extension name replaces the old one in place.
        registry
            .install(Extension::tasks(
                "task:test.versioned",
                vec![versioned(2, "v2", Some(migrating))],
            ))
            .unwrap();
        let receipt = opened.harness.wait_for_task(id, &bg()).unwrap();
        assert_matches(
            &js(&receipt),
            json!({ "version": 2, "state": { "outcome": { "status": "completed", "result": "v2" } } }),
        );
        assert_eq!(*migrations.lock().unwrap(), vec![1]);
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_blocked_blocks_an_older_record_whose_newer_definition_has_no_migration() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let opened = open_tasks(storage.clone(), vec![], OpenOptions::default()).unwrap();
        let id = create_in(&opened.harness, &versioned(1, "v1", None), Value::Null);
        opened.harness.close().unwrap();
        let opened = open_tasks(storage.reopen(), vec![versioned(2, "v2", None)], OpenOptions::default()).unwrap();
        opened.harness.resume().unwrap();
        eventually(|| reports(&opened).len() == 1);
        assert!(reports(&opened)[0].message().contains("has no migration from 1"));
        assert_matches(
            &js(&opened.harness.get_task(id).unwrap().unwrap()),
            json!({ "version": 1, "state": { "status": "pending" } }),
        );
        // Inspection shows why, without running task code.
        let inspection = opened.harness.inspect().unwrap();
        assert!(matches!(
            &inspection.tasks[0].state,
            TaskInspectionState::Blocked {
                reason: harness::scheduler::BlockedReason::MigrationFailed,
                ..
            }
        ));
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_blocked_settles_an_aborted_blocked_task_as_orphaned_and_retires_its_documents() {
    each_backend(|backend| {
        let scratch = DocDefinition::new("test.orphan-scratch", 1, DocSemantics::Task, |_| json!({ "n": 0 }))
            .define()
            .unwrap();
        let opened = open_tasks(Controlled::new(backend), vec![], OpenOptions::default()).unwrap();
        let root = opened.harness.root().unwrap();
        let id = root
            .commit(|tx| {
                let created = tx.create_task(
                    versioned(1, "x", None).definition(),
                    Value::Null,
                    TaskOptions::default(),
                )?;
                tx.doc(&scratch, Owner::Task(created))?
                    .set(&[chord_delta::Seg::Key("n".into())], json!(1))
                    .unwrap();
                Ok(created)
            })
            .unwrap();
        // Before resume: the marking commit settles the blocked task directly.
        assert_eq!(opened.harness.abort_task(id, &bg()).unwrap(), AbortResult::Marked);
        assert_eq!(
            js(&opened.harness.get_task(id).unwrap().unwrap().state),
            json!({ "status": "terminal", "outcome": { "status": "orphaned", "reason": "missing_task" } })
        );
        assert_eq!(opened.harness.snapshot(&scratch, Owner::Task(id)).unwrap(), None);
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_blocked_orphans_a_marked_task_whose_definition_disappeared_while_its_run_was_active() {
    each_backend(|backend| {
        let reached = Deferred::new();
        let re = reached.clone();
        let vanishing = Task::define("test.vanishing", 1, |_| step())
            .phase("run", move |_, runtime| {
                re.resolve();
                aborted(runtime)
            })
            .abort(|_, _| Err(HarnessError::msg("must not run")))
            .build();
        let registry = Registry::new();
        registry
            .install(Extension::tasks("task:test.vanishing", vec![vanishing.clone()]))
            .unwrap();
        let opened = open_tasks(
            Controlled::new(backend),
            vec![],
            OpenOptions {
                registry: Some(registry.clone()),
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let id = create_in(&opened.harness, &vanishing, Value::Null);
        opened.harness.resume().unwrap();
        reached.wait();
        registry.uninstall("task:test.vanishing");
        // An active run means the mark does not orphan directly; the scheduler orphans once the run has ended.
        assert_eq!(opened.harness.abort_task(id, &bg()).unwrap(), AbortResult::Marked);
        assert_eq!(
            outcome(&opened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "orphaned", "reason": "missing_task" })
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_blocked_orphans_a_reopened_abort_marked_task_without_a_definition_once_scheduling_resumes() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let opened = open_tasks(storage.clone(), vec![versioned(1, "x", None)], OpenOptions::default()).unwrap();
        let id = create_in(&opened.harness, &versioned(1, "x", None), Value::Null);
        assert_eq!(opened.harness.abort_task(id, &bg()).unwrap(), AbortResult::Marked);
        opened.harness.close().unwrap();

        let opened = open_tasks(storage.reopen(), vec![], OpenOptions::default()).unwrap();
        assert_matches(
            &js(&opened.harness.get_task(id).unwrap().unwrap()),
            json!({ "abortRequested": true, "state": { "status": "pending" } }),
        );
        opened.harness.wait_for_idle(&bg()).unwrap();
        assert_eq!(
            js(&opened.harness.get_task(id).unwrap().unwrap().state),
            json!({ "status": "terminal", "outcome": { "status": "orphaned", "reason": "missing_task" } })
        );
        opened.harness.close().unwrap();
    });
}

// ─── definition handover ─────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct Gates {
    a: Option<Deferred>,
    b: Option<Deferred>,
    on_end: Option<Arc<dyn Fn() + Send + Sync>>,
}

fn handover_task(
    label: &str,
    version: u64,
    log: &Log,
    gates: Gates,
    migrate: Option<harness::scheduler::Migrate>,
) -> Task {
    let advance = |phase: &'static str, next: Value| {
        let (label, log, gates) = (label.to_owned(), log.clone(), gates.clone());
        move |_: &durable::TaskRecord, runtime: &harness::scheduler::TaskRuntime| {
            log.lock().unwrap().push(format!("{label}:{phase} start"));
            let gate = if phase == "a" { gates.a.clone() } else { gates.b.clone() };
            if let Some(gate) = gate {
                gate.wait();
            }
            let next = next.clone();
            runtime.commit(&runtime.ctx(), |_, _| Ok(running(next)))?;
            // Leave room for a wrongly dispatched successor before this invocation ends.
            flush();
            if let Some(on_end) = &gates.on_end {
                on_end();
            }
            log.lock().unwrap().push(format!("{label}:{phase} end"));
            Ok(())
        }
    };
    let (lc, ll, la, laa) = (label.to_owned(), log.clone(), label.to_owned(), log.clone());
    let mut builder = Task::define("test.handover", version, |_| json!({ "phase": "a" }))
        .phase("a", advance("a", json!({ "phase": "b" })))
        .phase("b", advance("b", json!({ "phase": "c" })))
        .phase("c", move |_, runtime| {
            ll.lock().unwrap().push(format!("{lc}:c"));
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
        })
        .abort(move |_, runtime| {
            laa.lock().unwrap().push(format!("{la}:abort"));
            runtime.commit(&runtime.ctx(), |_, _| Ok(aborted_with(&la)))
        });
    if let Some(migrate) = migrate {
        builder = builder.migrate(move |input, checkpoint, from| migrate(input, checkpoint, from));
    }
    builder.build()
}

const HANDOVER: &str = "task:test.handover";

fn start_handover(log: &Log, gates: Gates, storage: Controlled) -> (Opened, TaskId) {
    let registry = Registry::new();
    registry
        .install(Extension::tasks(
            HANDOVER,
            vec![handover_task("old", 1, log, gates, None)],
        ))
        .unwrap();
    let opened = open_tasks(
        storage,
        vec![],
        OpenOptions {
            registry: Some(registry),
            ..OpenOptions::default()
        },
    )
    .unwrap();
    let id = create_in(
        &opened.harness,
        &handover_task("old", 1, log, Gates::default(), None),
        Value::Null,
    );
    opened.harness.resume().unwrap();
    eventually(|| log.lock().unwrap().len() == 1);
    (opened, id)
}

fn replace(opened: &Opened, task: Task) {
    // The same extension name replaces the old one in place.
    opened.registry.install(Extension::tasks(HANDOVER, vec![task])).unwrap();
}

#[test]
fn harness_handover_at_the_next_phase_boundary_to_a_same_version_replacement() {
    each_backend(|backend| {
        let log: Log = Arc::default();
        let gate = Deferred::new();
        let gates = Gates {
            a: Some(gate.clone()),
            ..Gates::default()
        };
        let (opened, id) = start_handover(&log, gates, Controlled::new(backend));
        replace(&opened, handover_task("new", 1, &log, Gates::default(), None));
        gate.resolve();
        opened.harness.wait_for_task(id, &bg()).unwrap();
        assert_eq!(
            logged(&log),
            vec!["old:a start", "old:a end", "new:b start", "new:b end", "new:c"]
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_handover_keeps_the_memos() {
    each_backend(|backend| {
        let log: Log = Arc::default();
        let (gate, reached) = (Deferred::new(), Deferred::new());
        let memo_task = |label: &str| {
            let (l1, l2, lg, g, r) = (
                label.to_owned(),
                label.to_owned(),
                log.clone(),
                gate.clone(),
                reached.clone(),
            );
            Task::define("test.handover-memo", 1, |_| json!({ "phase": "a" }))
                .phase("a", move |_, runtime| {
                    runtime.memo_set("picked", json!(l1), &runtime.ctx())?;
                    r.resolve();
                    g.wait();
                    runtime.commit(&runtime.ctx(), |_, _| Ok(running(json!({ "phase": "b" }))))
                })
                .phase("b", move |_, runtime| {
                    // The candidate loses to the memo the old definition stored.
                    let picked = runtime.memo_set("picked", json!(l2), &runtime.ctx())?;
                    lg.lock().unwrap().push(format!("{l2}:b {}", picked.as_str().unwrap()));
                    runtime.commit(&runtime.ctx(), |_, _| Ok(completed(picked)))
                })
                .build()
        };
        let registry = Registry::new();
        registry
            .install(Extension::tasks("task:memo", vec![memo_task("old")]))
            .unwrap();
        let opened = open_tasks(
            Controlled::new(backend),
            vec![],
            OpenOptions {
                registry: Some(registry.clone()),
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let id = create_in(&opened.harness, &memo_task("old"), Value::Null);
        opened.harness.resume().unwrap();
        reached.wait();
        registry
            .install(Extension::tasks("task:memo", vec![memo_task("new")]))
            .unwrap();
        gate.resolve();
        let receipt = opened.harness.wait_for_task(id, &bg()).unwrap();
        assert_eq!(outcome(&receipt), json!({ "status": "completed", "result": "old" }));
        assert_eq!(logged(&log), vec!["new:b old"]);
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_handover_to_a_newer_version_with_a_migration() {
    each_backend(|backend| {
        let log: Log = Arc::default();
        let gate = Deferred::new();
        let (opened, id) = start_handover(
            &log,
            Gates {
                a: Some(gate.clone()),
                ..Gates::default()
            },
            Controlled::new(backend),
        );
        let migrate: harness::scheduler::Migrate = Arc::new(|_, _, _| Ok((Value::Null, json!({ "phase": "c" }))));
        replace(&opened, handover_task("v2", 2, &log, Gates::default(), Some(migrate)));
        gate.resolve();
        let receipt = opened.harness.wait_for_task(id, &bg()).unwrap();
        assert_eq!(receipt.version, 2);
        assert_eq!(logged(&log), vec!["old:a start", "old:a end", "v2:c"]);
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_handover_to_a_newer_version_whose_migration_fails_leaves_the_task_blocked() {
    each_backend(|backend| {
        let log: Log = Arc::default();
        let gate = Deferred::new();
        let (opened, id) = start_handover(
            &log,
            Gates {
                a: Some(gate.clone()),
                ..Gates::default()
            },
            Controlled::new(backend),
        );
        let migrate: harness::scheduler::Migrate = Arc::new(|_, _, _| Err(HarnessError::msg("broken migration")));
        replace(
            &opened,
            handover_task("broken", 2, &log, Gates::default(), Some(migrate)),
        );
        gate.resolve();
        eventually(|| reports(&opened).len() == 1);
        flush();
        assert_matches(
            &js(&opened.harness.get_task(id).unwrap().unwrap()),
            json!({ "version": 1, "state": { "status": "pending", "checkpoint": { "phase": "b" } } }),
        );
        assert_eq!(logged(&log), vec!["old:a start", "old:a end"]);
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_handover_keeps_running_under_the_old_definition_when_no_replacement_can_take_the_task() {
    each_backend(|backend| {
        let log: Log = Arc::default();
        let (gate_a, gate_b) = (Deferred::new(), Deferred::new());
        let gates = Gates {
            a: Some(gate_a.clone()),
            b: Some(gate_b.clone()),
            on_end: None,
        };
        let (opened, id) = start_handover(&log, gates, Controlled::new(backend));
        opened.registry.uninstall(HANDOVER);
        gate_a.resolve();
        eventually(|| logged(&log).contains(&"old:b start".to_owned()));
        // A newer definition without a migration cannot take the task either.
        replace(&opened, handover_task("incompatible", 2, &log, Gates::default(), None));
        gate_b.resolve();
        opened.harness.wait_for_task(id, &bg()).unwrap();
        assert_eq!(
            logged(&log),
            vec!["old:a start", "old:a end", "old:b start", "old:b end", "old:c"]
        );
        let causes: Vec<Option<String>> = reports(&opened)
            .iter()
            .map(|report| report.cause().map(str::to_owned))
            .collect();
        assert_eq!(
            causes,
            vec![Some("missing_task".to_owned()), Some("incompatible_task".to_owned())]
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_handover_preserves_an_abort_mark_that_races_the_handover_commit() {
    each_backend(|backend| {
        let log: Log = Arc::default();
        let gate = Deferred::new();
        let storage = Controlled::new(backend);
        let held: Arc<Mutex<Option<Gate>>> = Arc::default();
        let (st, he) = (storage.clone(), held.clone());
        // The progress commit landed; hold the next commit, the handover, and queue the abort mark behind it.
        let gates = Gates {
            a: Some(gate.clone()),
            b: None,
            on_end: Some(Arc::new(move || {
                let mut held = he.lock().unwrap();
                if held.is_none() {
                    *held = Some(st.hold_commits());
                }
            })),
        };
        let (opened, id) = start_handover(&log, gates, storage.clone());
        replace(&opened, handover_task("new", 1, &log, Gates::default(), None));
        gate.resolve();
        eventually(|| held.lock().unwrap().is_some());
        let gate = held.lock().unwrap().clone().unwrap();
        gate.wait_entered();
        let h = opened.harness.clone();
        let aborting = spawn(move || h.abort_task(id, &bg()));
        flush();
        gate.release();
        assert_eq!(aborting.join().unwrap().unwrap(), AbortResult::Marked);
        assert_eq!(
            outcome(&opened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "aborted", "reason": "new" })
        );
        let states: Vec<String> = storage
            .commits()
            .iter()
            .flatten()
            .filter_map(|write| match write {
                StorageWrite::Task { value } if value.id == id => Some(format!(
                    "{}{}",
                    js(&value.state)["status"].as_str().unwrap(),
                    if value.abort_requested { "+mark" } else { "" }
                )),
                _ => None,
            })
            .collect();
        // created, reserved, progress, handover, mark, abort reservation, aborted
        assert_eq!(
            states,
            vec![
                "pending",
                "running",
                "running",
                "pending",
                "pending+mark",
                "running+mark",
                "terminal+mark"
            ]
        );
        assert_eq!(logged(&log), vec!["old:a start", "old:a end", "new:abort"]);
        opened.harness.close().unwrap();
    });
}

/// Registry reader that counts live subscriptions.
struct CountingReader {
    registry: Registry,
    count: Arc<AtomicUsize>,
}

impl RegistryReader for CountingReader {
    fn snapshot(&self) -> Arc<RegistrySnapshot> {
        self.registry.snapshot()
    }

    fn subscribe(&self, listener: Box<dyn Fn() + Send + Sync>) -> Box<dyn FnOnce() + Send> {
        self.count.fetch_add(1, Ordering::SeqCst);
        let unsubscribe = self.registry.subscribe(listener);
        let count = self.count.clone();
        Box::new(move || {
            count.fetch_sub(1, Ordering::SeqCst);
            unsubscribe();
        })
    }
}

#[test]
fn harness_open_releases_its_registry_subscription_and_closes_the_session_when_open_fails() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let log: Log = Arc::default();
        let l = log.clone();
        let stuck = Task::define("test.stuck", 1, |_| step())
            .phase("run", move |_, _| {
                l.lock().unwrap().push("run".into());
                Deferred::new().wait();
                Ok(())
            })
            .build();
        // Leave a running task behind so open has a reconciliation commit to fail.
        let first = open_tasks(storage.clone(), vec![stuck.clone()], OpenOptions::default()).unwrap();
        create_in(&first.harness, &stuck, Value::Null);
        first.harness.resume().unwrap();
        eventually(|| log.lock().unwrap().len() == 1);

        let count = Arc::new(AtomicUsize::new(0));
        let reader = CountingReader {
            registry: Registry::new(),
            count: count.clone(),
        };
        storage.fail_next_commit(StorageError::Failed("disk full".into()));
        let error = Harness::open(storage.clone(), HarnessOptions::new(Arc::new(reader)))
            .err()
            .unwrap();
        assert_eq!(error.message(), "disk full");
        assert_eq!(count.load(Ordering::SeqCst), 0);
        // The second Harness closed its Storage handle; the shared backend refuses its reads.
        assert!(durable::Storage::task(&storage, TaskId(1)).is_err());
    });
}

#[test]
fn harness_handover_rejects_a_runtime_commit_of_the_old_invocation_queued_behind_its_handover_commit() {
    each_backend(|backend| {
        let log: Log = Arc::default();
        let gate = Deferred::new();
        let storage = Controlled::new(backend);
        let held: Arc<Mutex<Option<Gate>>> = Arc::default();
        let old_runtime: Arc<Mutex<Option<harness::scheduler::TaskRuntime>>> = Arc::default();
        let harness_ref: Arc<Mutex<Option<Harness>>> = Arc::default();
        let (g, st, he, or, hr) = (
            gate.clone(),
            storage.clone(),
            held.clone(),
            old_runtime.clone(),
            harness_ref.clone(),
        );
        let old = Task::define("test.handover", 1, |_| json!({ "phase": "a" }))
            .phase("a", move |task, runtime| {
                *or.lock().unwrap() = Some(runtime.clone());
                g.wait();
                runtime.commit(&runtime.ctx(), |_, _| Ok(running(json!({ "phase": "b" }))))?;
                // Hold the line with an unrelated commit so the handover commit queues behind it.
                *he.lock().unwrap() = Some(st.hold_commits());
                let harness = hr.lock().unwrap().clone().unwrap();
                let conversation = task.conversation_id;
                spawn(move || {
                    harness.commit(|tx| {
                        Ok(tx
                            .append_entry(conversation, durable::EntryDraft::new("blocker"))
                            .map(drop)?)
                    })
                });
                flush();
                Ok(())
            })
            .phase("b", |_, _| Ok(()))
            .phase("c", |_, _| Ok(()))
            .build();
        let registry = Registry::new();
        registry.install(Extension::tasks(HANDOVER, vec![old.clone()])).unwrap();
        let opened = open_tasks(
            storage.clone(),
            vec![],
            OpenOptions {
                registry: Some(registry),
                ..OpenOptions::default()
            },
        )
        .unwrap();
        *harness_ref.lock().unwrap() = Some(opened.harness.clone());
        let id = create_in(&opened.harness, &old, Value::Null);
        opened.harness.resume().unwrap();
        eventually(|| old_runtime.lock().unwrap().is_some());
        replace(&opened, handover_task("new", 1, &log, Gates::default(), None));
        gate.resolve();
        eventually(|| held.lock().unwrap().is_some());
        let gate = held.lock().unwrap().clone().unwrap();
        gate.wait_entered();
        flush();
        // Queued behind the handover commit while the invocation has not ended yet.
        let runtime = old_runtime.lock().unwrap().clone().unwrap();
        let late = spawn(move || runtime.commit(&bg(), |_, _| Ok(completed(Value::Null))));
        flush();
        gate.release();
        assert!(
            late.join()
                .unwrap()
                .unwrap_err()
                .message()
                .contains("invocation has ended")
        );
        opened.harness.wait_for_task(id, &bg()).unwrap();
        assert_eq!(logged(&log), vec!["new:b start", "new:b end", "new:c"]);
        opened.harness.close().unwrap();
    });
}
