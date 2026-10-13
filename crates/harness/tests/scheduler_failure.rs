//! Port of upstream Pi Durable `test/harness-storage-failure.test.ts` (the Harness/task part), each case run
//! against Memory, SQLite and JSONL storage. Cases about context reads, conversation views, event streams and
//! JavaScript promise containment have no counterpart in the thread-based runtime and are not ported.

mod support;

use durable::{EntryDraft, SessionEnd, SessionError, StorageError, TaskId, TaskStatus};
use harness::scheduler::{Extension, Harness, HarnessError, HarnessOptions, Registry, Task, safe_now};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use support::*;

fn disk_gone() -> StorageError {
    StorageError::Failed("disk gone".into())
}

fn storage_error(error: StorageError) -> HarnessError {
    HarnessError::Session(SessionError::Storage(error))
}

fn failed(cause: StorageError) -> HarnessError {
    HarnessError::Session(SessionError::Failed(Box::new(SessionError::Storage(cause))))
}

/// Waits for its signal, records that it saw it, and ends `aborted` through its abort handler.
fn waiting(signals: &Arc<Mutex<Vec<String>>>) -> Task {
    let signals = signals.clone();
    Task::define("test.waiting", 1, |_| json!({ "phase": "work" }))
        .phase("work", move |_, runtime| {
            let _ = aborted(runtime);
            signals.lock().unwrap().push("aborted".into());
            Ok(())
        })
        .abort(|_, runtime| runtime.commit(&runtime.ctx(), |_, _| Ok(aborted_with("aborted"))))
        .build()
}

fn note(harness: &Harness, conversation: durable::ConversationId) -> Result<(), HarnessError> {
    harness.commit(|tx| Ok(tx.append_entry(conversation, EntryDraft::new("note")).map(drop)?))
}

fn running(harness: &Harness, id: TaskId) {
    eventually(|| harness.get_task(id).unwrap().unwrap().state.status() == TaskStatus::Running);
}

#[test]
fn harness_failure_ends_everything_with_the_first_error() {
    each_backend(|backend| {
        let signals: Arc<Mutex<Vec<String>>> = Arc::default();
        let task = waiting(&signals);
        let (opened, root, storage) = open_root(backend, vec![task.clone()], OpenOptions::default());
        let harness = opened.harness.clone();
        let id = start(&root, &task, false);
        harness.resume().unwrap();
        running(&harness, id);
        // The task wait registers on the Session line before the failure: queue it behind a held commit, then run
        // one more line job, which (FIFO) ends after the registration.
        let held = storage.hold_commits();
        let h = harness.clone();
        let blocker = spawn(move || note(&h, root.id));
        held.wait_entered();
        let h = harness.clone();
        let task_wait = spawn(move || h.wait_for_task(id, &bg()));
        line_reaches(&harness, 2);
        held.release();
        blocker.join().unwrap().unwrap();
        harness.get_task(id).unwrap();
        // A wait for idle registers off the line; one that starts after the failure gets the same error.
        let h = harness.clone();
        let idle = spawn(move || h.wait_for_idle(&bg()));
        storage.fail_next_commit(disk_gone());
        // The call that hit it gets the storage error itself.
        assert_eq!(note(&harness, root.id).unwrap_err(), storage_error(disk_gone()));
        assert_eq!(task_wait.join().unwrap().unwrap_err(), failed(disk_gone()));
        assert_eq!(idle.join().unwrap().unwrap_err(), failed(disk_gone()));
        // Every later call, and a wait that starts now.
        assert_eq!(harness.get_task(id).unwrap_err(), failed(disk_gone()));
        assert_eq!(harness.wait_for_idle(&bg()).unwrap_err(), failed(disk_gone()));
        assert_eq!(harness.closed(), SessionEnd::Failed(SessionError::Storage(disk_gone())));
        assert_eq!(*signals.lock().unwrap(), vec!["aborted"]);
        assert_eq!(reports(&opened), vec![storage_error(disk_gone())]);
        harness.close().unwrap();
    });
}

#[test]
fn harness_failure_when_storage_cannot_close_an_earlier_failure_stays_the_cause() {
    each_backend(|backend| {
        let close_error = StorageError::Failed("close failed".into());
        let storage = Controlled::new(backend);
        storage.fail_close(close_error.clone());
        let healthy = open_tasks(storage, vec![], OpenOptions::default()).unwrap();
        assert_eq!(healthy.harness.close().unwrap_err(), storage_error(close_error.clone()));
        assert_eq!(
            healthy.harness.closed(),
            SessionEnd::Failed(SessionError::Storage(close_error.clone()))
        );
        assert_eq!(reports(&healthy), vec![storage_error(close_error.clone())]);

        let storage = Controlled::new(backend);
        storage.fail_close(close_error);
        let opened = open_tasks(storage.clone(), vec![], OpenOptions::default()).unwrap();
        let root = opened.harness.root().unwrap();
        storage.fail_next_commit(disk_gone());
        assert_eq!(note(&opened.harness, root.id).unwrap_err(), storage_error(disk_gone()));
        assert_eq!(
            opened.harness.closed(),
            SessionEnd::Failed(SessionError::Storage(disk_gone()))
        );
        assert_eq!(reports(&opened), vec![storage_error(disk_gone())]);
    });
}

#[test]
fn harness_failure_rejects_the_harnesss_own_calls_with_session_failed() {
    each_backend(|backend| {
        let (opened, root, storage) = open_root(backend, vec![], OpenOptions::default());
        storage.fail_next_commit(disk_gone());
        assert_eq!(note(&opened.harness, root.id).unwrap_err(), storage_error(disk_gone()));
        let harness = &opened.harness;
        assert_eq!(harness.resume().unwrap_err(), failed(disk_gone()));
        assert_eq!(harness.resume().unwrap_err().name(), "SessionFailed");
        assert_eq!(harness.root().err().unwrap(), failed(disk_gone()));
        assert_eq!(harness.conversation(root.id).err().unwrap(), failed(disk_gone()));
        assert_eq!(
            harness
                .create_conversation(durable::ConversationOwnership::Ownerless)
                .err()
                .unwrap(),
            failed(disk_gone())
        );
        harness.close().unwrap();
    });
}

#[test]
fn harness_failure_from_a_failed_id_mint() {
    each_backend(|backend| {
        let (opened, root, storage) = open_root(backend, vec![], OpenOptions::default());
        storage.fail_next_mint(disk_gone());
        assert_eq!(note(&opened.harness, root.id).unwrap_err(), storage_error(disk_gone()));
        assert_eq!(
            opened.harness.closed(),
            SessionEnd::Failed(SessionError::Storage(disk_gone()))
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_failure_does_not_commit_what_a_callback_did_after_it_caught_a_failed_read() {
    each_backend(|backend| {
        let (opened, root, storage) = open_root(backend, vec![], OpenOptions::default());
        let written = storage.commit_count();
        storage.fail_next_task_read(disk_gone());
        let result = opened.harness.commit(|tx| {
            let _ = tx.task(TaskId(1));
            tx.append_entry(root.id, EntryDraft::new("note"))?;
            Ok(())
        });
        assert_eq!(result.unwrap_err(), failed(disk_gone()));
        assert_eq!(storage.commit_count(), written);
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_failure_for_a_storage_request_error_from_a_commit_whose_effect_is_unknown() {
    each_backend(|backend| {
        let (opened, root, storage) = open_root(backend, vec![], OpenOptions::default());
        let rejection = StorageError::Request("bad batch".into());
        storage.fail_next_commit(rejection.clone());
        assert_eq!(
            note(&opened.harness, root.id).unwrap_err(),
            storage_error(rejection.clone())
        );
        assert_eq!(
            opened.harness.closed(),
            SessionEnd::Failed(SessionError::Storage(rejection))
        );
        opened.harness.close().unwrap();
    });
}

#[test]
fn harness_failure_for_a_storage_error_during_a_host_close_before_the_backend_closes() {
    each_backend(|backend| {
        let (opened, root, storage) = open_root(backend, vec![], OpenOptions::default());
        let held = storage.hold_commits();
        storage.fail_next_commit(disk_gone());
        let h = opened.harness.clone();
        let committing = spawn(move || note(&h, root.id));
        held.wait_entered();
        let h = opened.harness.clone();
        let closing = spawn(move || h.close());
        // The close has queued behind the held commit.
        line_reaches(&opened.harness, 2);
        held.release();
        assert_eq!(committing.join().unwrap().unwrap_err(), storage_error(disk_gone()));
        let _ = closing.join().unwrap();
        assert_eq!(
            opened.harness.closed(),
            SessionEnd::Failed(SessionError::Storage(disk_gone()))
        );
        assert_eq!(reports(&opened), vec![storage_error(disk_gone())]);
    });
}

#[test]
fn harness_failure_ends_an_abort_that_joins_a_run_ignoring_its_signal_and_a_reentrant_report_finds_it_failed() {
    each_backend(|backend| {
        let (release, entered) = (Deferred::new(), Deferred::new());
        let (r, e) = (release.clone(), entered.clone());
        let stubborn = Task::define("test.stubborn", 1, |_| json!({ "phase": "work" }))
            .phase("work", move |_, _| {
                e.resolve();
                r.wait();
                Ok(())
            })
            .build();
        let storage = Controlled::new(backend);
        let registry = Registry::new();
        registry
            .install(Extension::tasks("tasks", vec![stubborn.clone()]))
            .unwrap();
        let harness_slot: Arc<Mutex<Option<Harness>>> = Arc::default();
        let reentered: Arc<Mutex<Option<Result<(), HarnessError>>>> = Arc::default();
        let (slot, re) = (harness_slot.clone(), reentered.clone());
        let mut options = HarnessOptions::new(Arc::new(registry));
        options.on_report = Some(Arc::new(move |_| {
            let harness = slot.lock().unwrap().clone().unwrap();
            let mut reentered = re.lock().unwrap();
            if reentered.is_none() {
                *reentered = Some(harness.wait_for_idle(&bg()));
            }
        }));
        let harness = Harness::open(storage.clone(), options).unwrap();
        *harness_slot.lock().unwrap() = Some(harness.clone());
        let root = harness.root().unwrap();
        let id = start(&root, &stubborn, false);
        harness.resume().unwrap();
        entered.wait();
        let h = harness.clone();
        let aborting = spawn(move || h.abort_task(id, &bg()));
        // The abort mark commits and the abort joins the run, which ignores its signal; then the Session fails.
        eventually(|| harness.get_task(id).unwrap().unwrap().abort_requested);
        // Settle only: an abort that has not joined the run yet when the Session fails ends with the same error.
        flush();
        storage.fail_next_commit(disk_gone());
        assert_eq!(note(&harness, root.id).unwrap_err(), storage_error(disk_gone()));
        assert_eq!(aborting.join().unwrap().unwrap_err(), failed(disk_gone()));
        assert_eq!(
            reentered.lock().unwrap().clone().unwrap().unwrap_err(),
            failed(disk_gone())
        );
        release.resolve();
        harness.close().unwrap();
    });
}

#[test]
fn harness_failure_reports_a_failing_clock_once_also_when_the_report_handler_reads_it_again() {
    let reports: Arc<Mutex<Vec<String>>> = Arc::default();
    let clock_slot: Arc<Mutex<Option<harness::scheduler::Clock>>> = Arc::default();
    let (sink, slot) = (reports.clone(), clock_slot.clone());
    let clock = safe_now(
        Some(Arc::new(|| Err(HarnessError::msg("clock broke")))),
        Arc::new(move |error| {
            let count = {
                let mut reports = sink.lock().unwrap();
                reports.push(error.message());
                reports.len()
            };
            // Reentry, as a handler that commits does: the clock is read again before this report returns.
            if count < 5 {
                let clock = slot.lock().unwrap().clone().unwrap();
                clock();
            }
        }),
    );
    *clock_slot.lock().unwrap() = Some(clock.clone());
    assert!(clock() > 0);
    clock();
    assert_eq!(*reports.lock().unwrap(), vec!["clock broke"]);
}

#[test]
fn harness_failure_keeps_working_when_on_report_panics_and_falls_back_to_the_wall_clock() {
    each_backend(|backend| {
        let signals: Arc<Mutex<Vec<String>>> = Arc::default();
        let task = waiting(&signals);
        let registry = Registry::new();
        registry.install(Extension::tasks("tasks", vec![task.clone()])).unwrap();
        let mut options = HarnessOptions::new(Arc::new(registry));
        options.now = Some(Arc::new(|| Err(HarnessError::msg("clock broke"))));
        let reported = Arc::new(AtomicUsize::new(0));
        let r = reported.clone();
        options.on_report = Some(Arc::new(move |_| {
            r.fetch_add(1, Ordering::SeqCst);
            panic!("report broke");
        }));
        let harness = Harness::open(Controlled::new(backend), options).unwrap();
        let root = harness.root().unwrap();
        let id = start(&root, &task, false);
        harness.resume().unwrap();
        running(&harness, id);
        assert!(harness.get_task(id).unwrap().unwrap().started_at.unwrap() > 0);
        harness.abort_task(id, &bg()).unwrap();
        assert_eq!(outcome(&harness.wait_for_task(id, &bg()).unwrap())["status"], "aborted");
        assert_eq!(reported.load(Ordering::SeqCst), 1);
        harness.close().unwrap();
        let _ = Value::Null;
    });
}
