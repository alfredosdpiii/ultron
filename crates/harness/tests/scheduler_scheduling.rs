//! Port of upstream Pi Durable `test/harness-tasks.test.ts` ("task scheduling", "task abort", "task close"), each
//! case run against Memory, SQLite and JSONL storage.

mod support;

use durable::{
    ConversationOwnership, EntryDraft, ScanOrder, SessionEnd, SessionError, StorageError, StorageWrite, TaskId,
    TaskQuery, TaskStatus,
};
use harness::scheduler::{AbortResult, Cancel, Ctx, Harness, HarnessError, Task, TaskRuntime};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use support::*;

fn disk_gone() -> StorageError {
    StorageError::Failed("disk gone".into())
}

fn session_failed(cause: StorageError) -> HarnessError {
    HarnessError::Session(SessionError::Failed(Box::new(SessionError::Storage(cause))))
}

#[test]
fn harness_scheduling_fails_the_harness_on_a_failed_reservation_commit_and_reopening_runs_the_task() {
    each_backend(|backend| {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let once = one_step("test.once", move |_, runtime| {
            r.fetch_add(1, Ordering::SeqCst);
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
        });
        let (opened, root, storage) = open_root(backend, vec![once.clone()], OpenOptions::default());
        let id = start(&root, &once, false);
        storage.fail_next_commit(disk_gone());
        let harness = opened.harness.clone();
        let waiting = spawn(move || harness.wait_for_task(id, &bg()));
        assert_eq!(
            opened.harness.closed(),
            SessionEnd::Failed(SessionError::Storage(disk_gone()))
        );
        assert_eq!(
            reports(&opened),
            vec![HarnessError::Session(SessionError::Storage(disk_gone()))]
        );
        // The pending wait and every later call get SessionFailed with the storage error.
        let error = waiting.join().unwrap().unwrap_err();
        assert_eq!(error.name(), "SessionFailed");
        assert_eq!(error, session_failed(disk_gone()));
        assert_eq!(opened.harness.get_task(id).unwrap_err(), session_failed(disk_gone()));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        opened.harness.close().unwrap();

        let reopened = open_tasks(storage.reopen(), vec![once.clone()], OpenOptions::default()).unwrap();
        reopened.harness.resume().unwrap();
        let receipt = reopened.harness.wait_for_task(id, &bg()).unwrap();
        assert_eq!(outcome(&receipt)["status"], "completed");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        reopened.harness.close().unwrap();
    });
}

#[test]
fn harness_scheduling_fails_the_harness_instead_of_running_a_task_again_when_its_fault_write_fails() {
    each_backend(|backend| {
        let runs = Arc::new(AtomicUsize::new(0));
        let storage = Controlled::new(backend);
        let (r, s) = (runs.clone(), storage.clone());
        let throws = one_step("test.failed-fault", move |_, _| {
            // The next commit is the step's fault write.
            if r.fetch_add(1, Ordering::SeqCst) == 0 {
                s.fail_next_commit(disk_gone());
            }
            Err(HarnessError::msg("boom"))
        });
        let opened = open_tasks(storage.clone(), vec![throws.clone()], OpenOptions::default()).unwrap();
        let root = opened.harness.root().unwrap();
        let id = start(&root, &throws, false);
        opened.harness.resume().unwrap();
        assert_eq!(
            opened.harness.closed(),
            SessionEnd::Failed(SessionError::Storage(disk_gone()))
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(
            reports(&opened),
            vec![HarnessError::Session(SessionError::Storage(disk_gone()))]
        );
        opened.harness.close().unwrap();
        // Reopening recovers the task as after a crash: still running, so its phase runs again.
        let reopened = open_tasks(storage.reopen(), vec![throws.clone()], OpenOptions::default()).unwrap();
        reopened.harness.resume().unwrap();
        assert_eq!(
            outcome(&reopened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "faulted", "error": { "message": "boom" } })
        );
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        reopened.harness.close().unwrap();
    });
}

#[test]
fn harness_scheduling_waits_for_harness_and_conversation_idleness() {
    each_backend(|backend| {
        let gates: Arc<Mutex<Vec<(TaskId, Deferred)>>> = Arc::default();
        let g = gates.clone();
        let gated_task = one_step("test.gated", move |task, runtime| {
            let gate = Deferred::new();
            g.lock().unwrap().push((task.id, gate.clone()));
            gate.wait();
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
        });
        let open = |id: TaskId| {
            let gate = gates
                .lock()
                .unwrap()
                .iter()
                .find(|(each, _)| *each == id)
                .unwrap()
                .1
                .clone();
            gate.resolve();
        };
        let (opened, root, _) = open_root(backend, vec![gated_task.clone()], OpenOptions::default());
        let harness = opened.harness.clone();
        harness.wait_for_idle(&bg()).unwrap();
        // wait_for_idle enables scheduling, as upstream; pause what it started by creating work only now.
        let other = harness.create_conversation(ConversationOwnership::Ownerless).unwrap();
        let foreground = start(&root, &gated_task, false);
        let background = start(&root, &gated_task, true);
        let elsewhere = start(&other, &gated_task, false);
        // Cancelling a wait only rejects that wait.
        let cancelled = Cancel::new();
        let ctx = Ctx::with_signal(&cancelled);
        let h = harness.clone();
        let cancelled_wait = spawn(move || h.wait_for_idle(&ctx));
        // Settle only: a wait that starts after its signal was cancelled is rejected with the same reason.
        flush();
        cancelled.cancel(HarnessError::msg("stop waiting"));
        assert_eq!(cancelled_wait.join().unwrap().unwrap_err().message(), "stop waiting");
        let already = Cancel::new();
        already.cancel(HarnessError::msg("already cancelled"));
        assert_eq!(
            root.wait_for_idle(&Ctx::with_signal(&already)).unwrap_err().message(),
            "already cancelled"
        );
        harness.resume().unwrap();
        eventually(|| gates.lock().unwrap().len() == 3);
        let r = root.clone();
        let root_idle = spawn(move || r.wait_for_idle(&bg()));
        let h = harness.clone();
        let harness_idle = spawn(move || h.wait_for_idle(&bg()));
        open(foreground);
        root_idle.join().unwrap().unwrap();
        assert!(!settled(&harness_idle));
        open(elsewhere);
        harness_idle.join().unwrap().unwrap();
        assert_eq!(
            harness.get_task(background).unwrap().unwrap().state.status(),
            TaskStatus::Running
        );
        open(background);
        harness.wait_for_task(background, &bg()).unwrap();
        harness.close().unwrap();
        let error = harness.wait_for_idle(&bg()).unwrap_err().message();
        assert!(error.contains("closed"), "{error}");
        let error = root.wait_for_idle(&bg()).unwrap_err().message();
        assert!(error.contains("closed"), "{error}");
    });
}

// #10546
#[test]
fn harness_scheduling_pages_the_newest_tasks_first() {
    each_backend(|backend| {
        let idle = one_step("test.idle", |_, _| Ok(()));
        let (opened, root, _) = open_root(backend, vec![idle.clone()], OpenOptions::default());
        let ids: Vec<TaskId> = (0..5).map(|_| start(&root, &idle, false)).collect();
        let first = opened
            .harness
            .commit(|tx| {
                let query = TaskQuery {
                    order: Some(ScanOrder::Descending),
                    ..TaskQuery::default()
                };
                Ok(tx.scan_tasks(&query, 2, None)?)
            })
            .unwrap();
        assert_eq!(
            first.items.iter().map(|task| task.id).collect::<Vec<_>>(),
            vec![ids[4], ids[3]]
        );
        let second = opened
            .harness
            .commit(|tx| Ok(tx.scan_tasks(&TaskQuery::default(), 2, first.next.as_ref())?))
            .unwrap();
        assert_eq!(
            second.items.iter().map(|task| task.id).collect::<Vec<_>>(),
            vec![ids[2], ids[1]]
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_scheduling_rejects_unknown_tasks_and_reports_terminal_tasks() {
    each_backend(|backend| {
        let done = one_step("test.quick", |_, runtime| {
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
        });
        let (opened, root, _) = open_root(backend, vec![done.clone()], OpenOptions::default());
        let harness = &opened.harness;
        let unknown = TaskId(999_999);
        assert_eq!(harness.get_task(unknown).unwrap(), None);
        assert!(
            harness
                .wait_for_task(unknown, &bg())
                .unwrap_err()
                .message()
                .contains("does not exist")
        );
        assert!(
            harness
                .abort_task(unknown, &bg())
                .unwrap_err()
                .message()
                .contains("does not exist")
        );
        let id = start(&root, &done, false);
        harness.resume().unwrap();
        harness.wait_for_task(id, &bg()).unwrap();
        assert_eq!(
            harness.wait_for_task(id, &bg()).unwrap().state.status(),
            TaskStatus::Terminal
        );
        assert_eq!(harness.abort_task(id, &bg()).unwrap(), AbortResult::Terminal);
        harness.close().unwrap();
        assert!(harness.resume().unwrap_err().message().contains("closed"));
    });
}

#[test]
fn harness_scheduling_rejects_task_waits_cancelled_or_closed_while_queued_on_the_line() {
    each_backend(|backend| {
        let gate = Deferred::new();
        let blocking = gated("test.wait-close", gate.clone());
        let (opened, root, storage) = open_root(backend, vec![blocking.clone()], OpenOptions::default());
        let harness = opened.harness.clone();
        let id = start(&root, &blocking, false);
        let blocker = |harness: &Harness| {
            let harness = harness.clone();
            let root = root.id;
            spawn(move || harness.commit(|tx| Ok(tx.append_entry(root, EntryDraft::new("blocker")).map(drop)?)))
        };

        // Hold the line with a commit, then queue waits behind it.
        let held = storage.hold_commits();
        let first = blocker(&harness);
        held.wait_entered();
        let controller = Cancel::new();
        let ctx = Ctx::with_signal(&controller);
        let h = harness.clone();
        let cancelled_while_queued = spawn(move || h.wait_for_task(id, &ctx));
        line_reaches(&harness, 2);
        controller.cancel(HarnessError::msg("wait cancelled"));
        held.release();
        first.join().unwrap().unwrap();
        assert_eq!(
            cancelled_while_queued.join().unwrap().unwrap_err().message(),
            "wait cancelled"
        );

        let h = harness.clone();
        let pending = spawn(move || h.wait_for_task(id, &bg()));
        let h = harness.clone();
        let idle = spawn(move || h.wait_for_idle(&bg()));
        let r = root.clone();
        let conversation_idle = spawn(move || r.wait_for_idle(&bg()));
        // Settle only: a wait that registers late, behind the second hold or after the close, is rejected as closed
        // all the same.
        flush();
        let held_again = storage.hold_commits();
        let second = blocker(&harness);
        held_again.wait_entered();
        let h = harness.clone();
        let closed_while_queued = spawn(move || h.wait_for_task(id, &bg()));
        line_reaches(&harness, 2);
        let h = harness.clone();
        let closing = spawn(move || h.close());
        sealed(&harness);
        held_again.release();
        second.join().unwrap().unwrap();
        for wait in [closed_while_queued, pending] {
            assert!(wait.join().unwrap().unwrap_err().message().contains("closed"));
        }
        for wait in [idle, conversation_idle] {
            assert!(wait.join().unwrap().unwrap_err().message().contains("closed"));
        }
        gate.resolve();
        closing.join().unwrap().unwrap();
    });
}

/// Start `abort_task()` and return once its mark is durable, before it has joined the run.
fn mark_durably(harness: &Harness, id: TaskId) -> std::thread::JoinHandle<Result<AbortResult, HarnessError>> {
    let h = harness.clone();
    let aborting = spawn(move || h.abort_task(id, &bg()));
    eventually(|| harness.get_task(id).unwrap().unwrap().abort_requested);
    aborting
}

fn two_phase(name: &str, reached: Deferred, proceed: Deferred, phases: Arc<Mutex<Vec<String>>>, abort: bool) -> Task {
    let (p1, p2, p3) = (phases.clone(), phases.clone(), phases);
    Task::define(name, 1, |_| json!({ "phase": "one" }))
        .phase("one", move |_, runtime| {
            p1.lock().unwrap().push("one".into());
            runtime.commit(&runtime.ctx(), |_, _| Ok(running(json!({ "phase": "two" }))))?;
            reached.resolve();
            // Ignores the signal and returns normally once released.
            proceed.wait();
            Ok(())
        })
        .phase("two", move |_, _| {
            p2.lock().unwrap().push("two".into());
            Ok(())
        })
        .abort(move |_, runtime| {
            p3.lock().unwrap().push("abort".into());
            if abort {
                runtime.commit(&runtime.ctx(), |_, _| Ok(aborted_with("boundary")))
            } else {
                Ok(())
            }
        })
        .build()
}

#[test]
fn harness_abort_rejects_the_runs_commits_after_the_mark_and_settles_through_a_fresh_abort_invocation() {
    each_backend(|backend| {
        let reached = Deferred::new();
        let errors = Arc::new(Mutex::new(Vec::<String>::new()));
        let memo_read = Arc::new(Mutex::new(Value::Null));
        let abort_runtimes = Arc::new(AtomicUsize::new(0));
        let checks = Arc::new(Mutex::new(Vec::new()));
        let (re, er, mr, ar, ch) = (
            reached.clone(),
            errors.clone(),
            memo_read.clone(),
            abort_runtimes.clone(),
            checks.clone(),
        );
        let marked = one_step_abort(
            "test.marked",
            move |_, runtime| {
                runtime.memo_set("kept", json!(1), &runtime.ctx())?;
                re.resolve();
                // Keep working after the signal: every later write of this run must reject.
                let _ = aborted(runtime);
                *mr.lock().unwrap() = runtime.memo("kept")?.unwrap_or(Value::Null);
                if let Err(error) = runtime.memo_set("late", json!(2), &bg()) {
                    er.lock().unwrap().push(error.message());
                }
                if let Err(error) = runtime.commit(&bg(), |_, _| Ok(completed(Value::Null))) {
                    er.lock().unwrap().push(error.message());
                }
                Ok(())
            },
            move |_, runtime| {
                ar.fetch_add(1, Ordering::SeqCst);
                runtime.commit(&runtime.ctx(), |_, current| {
                    ch.lock().unwrap().push(json!([current.abort_requested, current.memos]));
                    Ok(aborted_with("mark"))
                })
            },
        );
        let (opened, root, _) = open_root(backend, vec![marked.clone()], OpenOptions::default());
        let id = start(&root, &marked, false);
        opened.harness.resume().unwrap();
        reached.wait();
        assert_eq!(opened.harness.abort_task(id, &bg()).unwrap(), AbortResult::Marked);
        assert_eq!(
            outcome(&opened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "aborted", "reason": "mark" })
        );
        assert_eq!(*memo_read.lock().unwrap(), json!(1));
        let mark = format!("Task {} has a durable abort mark", id.0);
        assert_eq!(*errors.lock().unwrap(), vec![mark.clone(), mark]);
        assert_eq!(abort_runtimes.load(Ordering::SeqCst), 1);
        assert_eq!(*checks.lock().unwrap(), vec![json!([true, { "kept": 1 }])]);
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_abort_starts_no_further_phase_after_a_mark_that_lands_during_a_phase_with_progress() {
    each_backend(|backend| {
        let (reached, proceed) = (Deferred::new(), Deferred::new());
        let phases: Arc<Mutex<Vec<String>>> = Arc::default();
        let two = two_phase(
            "test.mark-boundary",
            reached.clone(),
            proceed.clone(),
            phases.clone(),
            true,
        );
        let (opened, root, _) = open_root(backend, vec![two.clone()], OpenOptions::default());
        let id = start(&root, &two, false);
        opened.harness.resume().unwrap();
        reached.wait();
        let aborting = mark_durably(&opened.harness, id);
        proceed.resolve();
        assert_eq!(aborting.join().unwrap().unwrap(), AbortResult::Marked);
        assert_eq!(
            outcome(&opened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "aborted", "reason": "boundary" })
        );
        assert_eq!(*phases.lock().unwrap(), vec!["one", "abort"]);
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_abort_signals_and_joins_the_run_before_returning() {
    each_backend(|backend| {
        let (reached, finish) = (Deferred::new(), Deferred::new());
        let run_ended = Arc::new(Mutex::new(false));
        let (re, en, fi) = (reached.clone(), run_ended.clone(), finish.clone());
        // The abort handler commits its outcome only once the test let it: the second abort must find the task
        // still aborting (upstream's microtask order guarantees it; here threads race, so the test orders it).
        let signalled = one_step_abort(
            "test.signalled",
            move |_, runtime| {
                re.resolve();
                let result = aborted(runtime);
                *en.lock().unwrap() = true;
                result
            },
            move |_, runtime| {
                fi.wait();
                runtime.commit(&runtime.ctx(), |_, _| Ok(aborted_with("test")))
            },
        );
        let (opened, root, _) = open_root(backend, vec![signalled.clone()], OpenOptions::default());
        let id = start(&root, &signalled, false);
        opened.harness.resume().unwrap();
        reached.wait();
        assert_eq!(opened.harness.abort_task(id, &bg()).unwrap(), AbortResult::Marked);
        assert!(*run_ended.lock().unwrap());
        assert_eq!(opened.harness.abort_task(id, &bg()).unwrap(), AbortResult::Marked);
        finish.resolve();
        assert_eq!(
            outcome(&opened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "aborted", "reason": "test" })
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_abort_aborts_waiting_work_and_faults_abort_handlers_that_throw_or_settle_nothing() {
    each_backend(|backend| {
        let gate = Deferred::new();
        let first_task = gated("test.dependency", gate.clone());
        let first_id: Arc<Mutex<Option<TaskId>>> = Arc::default();
        let wait = |first_id: Arc<Mutex<Option<TaskId>>>| {
            move |_: &durable::TaskRecord, runtime: &TaskRuntime| {
                let on = vec![first_id.lock().unwrap().unwrap()];
                runtime.commit(&runtime.ctx(), |_, _| {
                    Ok(Some(harness::scheduler::NextState::Waiting {
                        checkpoint: step(),
                        on,
                        policy: durable::JoinPolicy::AllSettled,
                    }))
                })
            }
        };
        let lazy = one_step_abort("test.lazy-abort", wait(first_id.clone()), |_, _| Ok(()));
        let throwing = one_step_abort("test.throwing-abort", wait(first_id.clone()), |_, _| {
            Err(HarnessError::msg("abort failed"))
        });
        let (opened, root, _) = open_root(
            backend,
            vec![first_task.clone(), lazy.clone(), throwing.clone()],
            OpenOptions::default(),
        );
        let harness = &opened.harness;
        let first = start(&root, &first_task, false);
        *first_id.lock().unwrap() = Some(first);
        let lazy_id = start(&root, &lazy, false);
        let throwing_id = start(&root, &throwing, false);
        harness.resume().unwrap();
        eventually(|| harness.get_task(throwing_id).unwrap().unwrap().state.status() == TaskStatus::Waiting);
        eventually(|| harness.get_task(lazy_id).unwrap().unwrap().state.status() == TaskStatus::Waiting);
        harness.abort_task(lazy_id, &bg()).unwrap();
        harness.abort_task(throwing_id, &bg()).unwrap();
        assert_eq!(
            outcome(&harness.wait_for_task(lazy_id, &bg()).unwrap()),
            json!({ "status": "faulted", "error": { "message": format!("Abort handler of task {} returned without a terminal outcome", lazy_id.0) } })
        );
        assert_eq!(
            outcome(&harness.wait_for_task(throwing_id, &bg()).unwrap()),
            json!({ "status": "faulted", "error": { "message": "abort failed" } })
        );
        assert_eq!(
            harness.get_task(first).unwrap().unwrap().state.status(),
            TaskStatus::Running
        );
        gate.resolve();
        harness.wait_for_task(first, &bg()).unwrap();
        harness.close().unwrap();
    });
}

#[test]
fn harness_abort_does_not_signal_a_running_abort_handler_when_aborted_again() {
    each_backend(|backend| {
        let (entered, reached, proceed, run_release) =
            (Deferred::new(), Deferred::new(), Deferred::new(), Deferred::new());
        let abort_signalled = Arc::new(Mutex::new(None));
        let aborts = Arc::new(AtomicUsize::new(0));
        let (en, rr, re, pr, sig, ab) = (
            entered.clone(),
            run_release.clone(),
            reached.clone(),
            proceed.clone(),
            abort_signalled.clone(),
            aborts.clone(),
        );
        let run = one_step_abort(
            "test.abort-again",
            move |_, _| {
                en.resolve();
                // Ignores the signal, so the first caller is still joining when it gives up.
                rr.wait();
                Ok(())
            },
            move |_, runtime| {
                ab.fetch_add(1, Ordering::SeqCst);
                re.resolve();
                pr.wait();
                *sig.lock().unwrap() = Some(runtime.signal().is_cancelled());
                runtime.commit(&runtime.ctx(), |_, _| Ok(aborted_with("once")))
            },
        );
        let (opened, root, _) = open_root(backend, vec![run.clone()], OpenOptions::default());
        let harness = opened.harness.clone();
        let id = start(&root, &run, false);
        harness.resume().unwrap();
        // The phase handler itself, not only the `running` record (TS: `await flush()`): a mark committed before the
        // run invocation's first step ends it without a phase, and the join would return before the caller gives up.
        entered.wait();
        // This caller gives up while joining; the mark and the abort invocation are unaffected.
        let controller = Cancel::new();
        let ctx = Ctx::with_signal(&controller);
        let h = harness.clone();
        let cancelled = spawn(move || h.abort_task(id, &ctx));
        eventually(|| harness.get_task(id).unwrap().unwrap().abort_requested);
        controller.cancel(HarnessError::msg("caller gave up"));
        assert_eq!(cancelled.join().unwrap().unwrap_err().message(), "caller gave up");
        run_release.resolve();
        reached.wait();
        assert_eq!(harness.abort_task(id, &bg()).unwrap(), AbortResult::Marked);
        proceed.resolve();
        assert_eq!(
            outcome(&harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "aborted", "reason": "once" })
        );
        assert_eq!(*abort_signalled.lock().unwrap(), Some(false));
        assert_eq!(aborts.load(Ordering::SeqCst), 1);
        harness.close().unwrap();
    });
}

#[test]
fn harness_abort_keeps_a_terminal_outcome_committed_by_an_abort_handler_that_throws_afterwards() {
    each_backend(|backend| {
        let run = one_step_abort(
            "test.abort-then-throw",
            |_, runtime| aborted(runtime),
            |_, runtime| {
                runtime.commit(&runtime.ctx(), |_, _| Ok(aborted_with("done")))?;
                Err(HarnessError::msg("after terminal"))
            },
        );
        let (opened, root, _) = open_root(backend, vec![run.clone()], OpenOptions::default());
        let id = start(&root, &run, false);
        opened.harness.resume().unwrap();
        eventually(|| opened.harness.get_task(id).unwrap().unwrap().state.status() == TaskStatus::Running);
        opened.harness.abort_task(id, &bg()).unwrap();
        assert_eq!(
            outcome(&opened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "aborted", "reason": "done" })
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_abort_never_starts_phase_one_when_the_abort_lands_while_the_reservation_settles() {
    each_backend(|backend| {
        let ran = Arc::new(Mutex::new(false));
        let r = ran.clone();
        let reserved = one_step("test.reserved", move |_, runtime| {
            *r.lock().unwrap() = true;
            aborted(runtime)
        });
        let (opened, root, storage) = open_root(backend, vec![reserved.clone()], OpenOptions::default());
        let id = start(&root, &reserved, false);
        let held = storage.hold_commits();
        opened.harness.resume().unwrap();
        held.wait_entered();
        // The reservation commit is in storage; the abort mark commit queues behind it.
        let h = opened.harness.clone();
        let aborting = spawn(move || h.abort_task(id, &bg()));
        line_reaches(&opened.harness, 2);
        held.release();
        assert_eq!(aborting.join().unwrap().unwrap(), AbortResult::Marked);
        assert_eq!(
            outcome(&opened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "aborted", "reason": "test" })
        );
        assert!(!*ran.lock().unwrap());
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_abort_mark_wins_over_a_fault_that_races_it() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let held: Arc<Mutex<Option<Gate>>> = Arc::default();
        let marking: Pending<AbortResult> = Arc::default();
        let harness_ref: Arc<Mutex<Option<Harness>>> = Arc::default();
        let (st, he, mk, hr) = (storage.clone(), held.clone(), marking.clone(), harness_ref.clone());
        let racing = one_step("test.racing-fault", move |task, _| {
            let gate = st.hold_commits();
            *he.lock().unwrap() = Some(gate.clone());
            let harness = hr.lock().unwrap().clone().unwrap();
            let id = task.id;
            *mk.lock().unwrap() = Some(spawn(move || harness.abort_task(id, &bg())));
            gate.wait_entered();
            // The mark is in storage but not committed when the handler throws; the fault commit queues behind it.
            Err(HarnessError::msg("would fault"))
        });
        let opened = open_tasks(storage.clone(), vec![racing.clone()], OpenOptions::default()).unwrap();
        *harness_ref.lock().unwrap() = Some(opened.harness.clone());
        let root = opened.harness.root().unwrap();
        let id = start(&root, &racing, false);
        opened.harness.resume().unwrap();
        eventually(|| held.lock().unwrap().is_some());
        let gate = held.lock().unwrap().clone().unwrap();
        gate.wait_entered();
        // The fault commit has queued behind the held abort mark.
        line_reaches(&opened.harness, 2);
        gate.release();
        assert_eq!(
            outcome(&opened.harness.wait_for_task(id, &bg()).unwrap()),
            json!({ "status": "aborted", "reason": "test" })
        );
        assert_eq!(
            marking.lock().unwrap().take().unwrap().join().unwrap().unwrap(),
            AbortResult::Marked
        );
        opened.harness.close().unwrap();
    });
}

fn task_writes(storage: &Controlled, id: TaskId) -> Vec<Value> {
    storage
        .commits()
        .iter()
        .flatten()
        .filter_map(|write| match write {
            StorageWrite::Task { value } if value.id == id => Some(js(value)),
            _ => None,
        })
        .collect()
}

fn statuses(writes: &[Value]) -> Vec<String> {
    writes
        .iter()
        .map(|write| write["state"]["status"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn harness_close_stops_without_outcomes_and_starts_no_fresh_phase_or_abort_invocation() {
    each_backend(|backend| {
        let (reached, proceed) = (Deferred::new(), Deferred::new());
        let phases: Arc<Mutex<Vec<String>>> = Arc::default();
        let two = two_phase("test.two", reached.clone(), proceed.clone(), phases.clone(), false);
        let (opened, root, storage) = open_root(backend, vec![two.clone()], OpenOptions::default());
        let id = start(&root, &two, false);
        opened.harness.resume().unwrap();
        reached.wait();
        let aborting = mark_durably(&opened.harness, id);
        let record = opened.harness.get_task(id).unwrap().unwrap();
        let commits = storage.commit_count();
        let h = opened.harness.clone();
        let closing = spawn(move || h.close());
        sealed(&opened.harness);
        proceed.resolve();
        closing.join().unwrap().unwrap();
        assert_eq!(aborting.join().unwrap().unwrap(), AbortResult::Marked);
        assert_eq!(storage.commit_count(), commits);
        assert_eq!(*phases.lock().unwrap(), vec!["one"]);
        assert_matches(
            &js(&record),
            json!({ "abortRequested": true, "state": { "status": "running", "checkpoint": { "phase": "two" } } }),
        );
        assert!(
            opened
                .harness
                .commit(|_| Ok(()))
                .unwrap_err()
                .message()
                .contains("closed")
        );
    });
}

#[test]
fn harness_close_seals_admission_before_signalling_handlers() {
    each_backend(|backend| {
        let reached = Deferred::new();
        let from_listener: Arc<Mutex<Option<Result<(), HarnessError>>>> = Arc::default();
        let harness_ref: Arc<Mutex<Option<Harness>>> = Arc::default();
        let (re, fl, hr) = (reached.clone(), from_listener.clone(), harness_ref.clone());
        let stubborn = one_step("test.stubborn", move |_, runtime| {
            re.resolve();
            let _ = aborted(runtime);
            let harness = hr.lock().unwrap().clone().unwrap();
            *fl.lock().unwrap() = Some(harness.commit(|_| Ok(())));
            Ok(())
        });
        let (opened, root, _) = open_root(backend, vec![stubborn.clone()], OpenOptions::default());
        *harness_ref.lock().unwrap() = Some(opened.harness.clone());
        start(&root, &stubborn, false);
        opened.harness.resume().unwrap();
        reached.wait();
        opened.harness.close().unwrap();
        let rejected = from_listener.lock().unwrap().clone().unwrap().unwrap_err();
        assert!(rejected.message().contains("closed"), "{rejected}");
    });
}

#[test]
fn harness_close_writes_no_fault_when_it_seals_while_the_step_after_a_failed_phase_is_queued() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let held: Arc<Mutex<Option<Gate>>> = Arc::default();
        let harness_ref: Arc<Mutex<Option<Harness>>> = Arc::default();
        let (st, he, hr) = (storage.clone(), held.clone(), harness_ref.clone());
        let throws = one_step("test.close-fault", move |_, runtime| {
            let gate = st.hold_commits();
            *he.lock().unwrap() = Some(gate.clone());
            let harness = hr.lock().unwrap().clone().unwrap();
            let conversation = runtime.conversation_id();
            spawn(move || {
                harness.commit(|tx| Ok(tx.append_entry(conversation, EntryDraft::new("blocker")).map(drop)?))
            });
            gate.wait_entered();
            Err(HarnessError::msg("would fault"))
        });
        let opened = open_tasks(storage.clone(), vec![throws.clone()], OpenOptions::default()).unwrap();
        *harness_ref.lock().unwrap() = Some(opened.harness.clone());
        let root = opened.harness.root().unwrap();
        let id = start(&root, &throws, false);
        opened.harness.resume().unwrap();
        eventually(|| held.lock().unwrap().is_some());
        let gate = held.lock().unwrap().clone().unwrap();
        gate.wait_entered();
        // The step after the failed phase has queued behind the held commit.
        line_reaches(&opened.harness, 2);
        let h = opened.harness.clone();
        let closing = spawn(move || h.close());
        sealed(&opened.harness);
        gate.release();
        closing.join().unwrap().unwrap();
        assert_eq!(statuses(&task_writes(&storage, id)), vec!["pending", "running"]);
    });
}

#[test]
fn harness_close_starts_no_abort_handler_whose_reservation_settles_while_closing() {
    each_backend(|backend| {
        let ran = Arc::new(Mutex::new(false));
        let r = ran.clone();
        let marked = one_step_abort(
            "test.close-abort-reservation",
            |_, _| Ok(()),
            move |_, _| {
                *r.lock().unwrap() = true;
                Ok(())
            },
        );
        let (opened, root, storage) = open_root(backend, vec![marked.clone()], OpenOptions::default());
        let id = start(&root, &marked, false);
        assert_eq!(opened.harness.abort_task(id, &bg()).unwrap(), AbortResult::Marked);
        let held = storage.hold_commits();
        opened.harness.resume().unwrap();
        held.wait_entered();
        let h = opened.harness.clone();
        let closing = spawn(move || h.close());
        sealed(&opened.harness);
        held.release();
        closing.join().unwrap().unwrap();
        assert!(!*ran.lock().unwrap());
        let last: Vec<Value> = storage
            .commits()
            .last()
            .unwrap()
            .iter()
            .filter_map(|write| match write {
                StorageWrite::Task { value } => Some(js(value)),
                _ => None,
            })
            .collect();
        assert_eq!(last.len(), 1);
        assert_matches(
            &last[0],
            json!({ "id": id.0, "abortRequested": true, "state": { "status": "running" } }),
        );
    });
}

#[test]
fn harness_close_rejects_a_runtime_commit_that_was_queued_on_the_line_when_it_sealed() {
    each_backend(|backend| {
        let storage = Controlled::new(backend);
        let held: Arc<Mutex<Option<Gate>>> = Arc::default();
        let queued: Pending<()> = Arc::default();
        let harness_ref: Arc<Mutex<Option<Harness>>> = Arc::default();
        let proceed = Deferred::new();
        let (st, he, qu, hr, pr) = (
            storage.clone(),
            held.clone(),
            queued.clone(),
            harness_ref.clone(),
            proceed.clone(),
        );
        let task = one_step("test.close-queued-commit", move |_, runtime| {
            let gate = st.hold_commits();
            *he.lock().unwrap() = Some(gate.clone());
            let harness = hr.lock().unwrap().clone().unwrap();
            let conversation = runtime.conversation_id();
            spawn(move || {
                harness.commit(|tx| Ok(tx.append_entry(conversation, EntryDraft::new("blocker")).map(drop)?))
            });
            gate.wait_entered();
            let late = runtime.clone();
            *qu.lock().unwrap() = Some(spawn(move || late.commit(&bg(), |_, _| Ok(completed(Value::Null)))));
            // Ignores the close signal, so the invocation is still alive when the queued commit reaches the line.
            pr.wait();
            Ok(())
        });
        let opened = open_tasks(storage.clone(), vec![task.clone()], OpenOptions::default()).unwrap();
        *harness_ref.lock().unwrap() = Some(opened.harness.clone());
        let root = opened.harness.root().unwrap();
        let id = start(&root, &task, false);
        opened.harness.resume().unwrap();
        eventually(|| queued.lock().unwrap().is_some());
        let gate = held.lock().unwrap().clone().unwrap();
        gate.wait_entered();
        // The runtime commit has queued behind the held one.
        line_reaches(&opened.harness, 2);
        let h = opened.harness.clone();
        let closing = spawn(move || h.close());
        sealed(&opened.harness);
        gate.release();
        let rejected = queued.lock().unwrap().take().unwrap().join().unwrap().unwrap_err();
        assert!(rejected.message().contains("Harness is closed"), "{rejected}");
        proceed.resolve();
        closing.join().unwrap().unwrap();
        assert_eq!(task_writes(&storage, id).len(), 2);
    });
}

/// A registry reader that starts closing the Harness from inside the step's snapshot refresh.
struct ClosingReader {
    registry: harness::scheduler::Registry,
    close: Arc<Mutex<bool>>,
    harness: Arc<Mutex<Option<Harness>>>,
    closing: Pending<()>,
}

impl harness::scheduler::RegistryReader for ClosingReader {
    fn snapshot(&self) -> Arc<harness::scheduler::RegistrySnapshot> {
        if std::mem::take(&mut *self.close.lock().unwrap()) {
            let harness = self.harness.lock().unwrap().clone().unwrap();
            let closer = harness.clone();
            *self.closing.lock().unwrap() = Some(spawn(move || closer.close()));
            // Upstream's close seals synchronously; wait here (on the line) until it has.
            eventually(|| harness.resume().is_err());
        }
        harness::scheduler::RegistryReader::snapshot(&self.registry)
    }

    fn subscribe(&self, listener: Box<dyn Fn() + Send + Sync>) -> Box<dyn FnOnce() + Send> {
        self.registry.subscribe(listener)
    }
}

#[test]
fn harness_close_starts_no_next_phase_when_it_seals_during_a_step_that_decided_to_continue() {
    each_backend(|backend| {
        let phases: Arc<Mutex<Vec<String>>> = Arc::default();
        let close = Arc::new(Mutex::new(false));
        let (p1, p2, c) = (phases.clone(), phases.clone(), close.clone());
        let two = Task::define("test.close-in-step", 1, |_| json!({ "phase": "one" }))
            .phase("one", move |_, runtime| {
                p1.lock().unwrap().push("one".into());
                runtime.commit(&runtime.ctx(), |_, _| Ok(running(json!({ "phase": "two" }))))?;
                *c.lock().unwrap() = true;
                Ok(())
            })
            .phase("two", move |_, _| {
                p2.lock().unwrap().push("two".into());
                Ok(())
            })
            .build();
        let registry = harness::scheduler::Registry::new();
        registry
            .install(harness::scheduler::Extension::tasks(
                "task:test.close-in-step",
                vec![two.clone()],
            ))
            .unwrap();
        let reader = ClosingReader {
            registry,
            close,
            harness: Arc::default(),
            closing: Arc::default(),
        };
        let (harness_slot, closing) = (reader.harness.clone(), reader.closing.clone());
        let harness = Harness::open(
            Controlled::new(backend),
            harness::scheduler::HarnessOptions::new(Arc::new(reader)),
        )
        .unwrap();
        *harness_slot.lock().unwrap() = Some(harness.clone());
        let root = harness.root().unwrap();
        start(&root, &two, false);
        harness.resume().unwrap();
        eventually(|| closing.lock().unwrap().is_some());
        closing.lock().unwrap().take().unwrap().join().unwrap().unwrap();
        assert_eq!(*phases.lock().unwrap(), vec!["one"]);
    });
}

#[test]
fn harness_close_joins_a_reservation_that_settles_while_closing_without_starting_its_handler() {
    each_backend(|backend| {
        let ran = Arc::new(Mutex::new(false));
        let r = ran.clone();
        let never = one_step("test.close-reservation", move |_, _| {
            *r.lock().unwrap() = true;
            Ok(())
        });
        let (opened, root, storage) = open_root(backend, vec![never.clone()], OpenOptions::default());
        let id = start(&root, &never, false);
        let held = storage.hold_commits();
        opened.harness.resume().unwrap();
        held.wait_entered();
        let h = opened.harness.clone();
        let closing = spawn(move || h.close());
        sealed(&opened.harness);
        held.release();
        closing.join().unwrap().unwrap();
        assert!(!*ran.lock().unwrap());
        let last = storage.commits().last().unwrap().clone();
        assert_eq!(last.len(), 1);
        assert_matches(
            &js(&last[0]),
            json!({ "type": "task", "value": { "id": id.0, "state": { "status": "running" } } }),
        );
    });
}
