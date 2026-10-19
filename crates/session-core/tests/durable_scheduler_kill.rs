//! Kill/reopen process test: a child process (this test binary, re-run in child mode) opens a Harness over the
//! product's durable store (a format-4 session file through `JournalStorage`), commits task intents and starts their effects, then freezes Storage so the cascade commit
//! that follows an abort mark never lands; the parent SIGKILLs it mid-phase, reopens the same session file, and asserts the
//! recovery contract (spec §5.2, §5.4, §5.5, §10.1): a replay-safe phase reruns, an unsafe one yields an interrupted
//! outcome, abort cascades and `completing` holds are re-derived from durable records, abandoned work is aborted with
//! reason `restart`, and every acknowledged commit survived.

mod journal_kill_support;
#[path = "../../harness/tests/support/mod.rs"]
mod support;

use durable::{Seq, Storage, StorageError, StorageWrite, TaskId, TaskOptions, TaskRecord, TaskStatus};
use harness::scheduler::{Cancel, Extension, Harness, HarnessError, HarnessOptions, Registry, Task};
use journal_kill_support::{create_session, durable_commits, open_journal};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::{Deferred, aborted_with, bg, completed, eventually, js, running};

const CHILD_ENV: &str = "ULTRON_HARNESS_KILL_CHILD";
const DIR_ENV: &str = "ULTRON_HARNESS_KILL_DIR";

/// Append one line with a single write: task threads log concurrently, and `writeln!` on a `File` issues the text
/// and its newline as separate writes, so two appenders interleaved ("hold:abandonhold:owner", then two newlines)
/// and the child never saw both effects (it waited forever; the parent's guard then failed the test).
fn append(path: &Path, line: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(format!("{line}\n").as_bytes()).unwrap();
    file.sync_all().unwrap();
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Gates the parent opens by name; in the child nothing opens them.
#[derive(Clone, Default)]
struct Gates(Arc<Mutex<HashMap<String, Cancel>>>);

impl Gates {
    fn gate(&self, name: &str) -> Cancel {
        self.0.lock().unwrap().entry(name.to_owned()).or_default().clone()
    }
}

/// The task definitions both processes register. `effects` logs every external effect, before the child blocks.
fn kill_tasks(effects: PathBuf, child: bool, gates: Gates) -> Vec<Task> {
    // Intent/effect: `prepare` commits the intent and performs the effect in the same invocation (the child is
    // killed while it runs); `execute` runs only when reopening finds the intent.
    let effect = |name: &'static str| {
        let (e1, e2) = (effects.clone(), effects.clone());
        Task::define(
            name,
            1,
            |input| json!({ "phase": "prepare", "replay": input["replay"] }),
        )
        .phase("prepare", move |task, runtime| {
            let replay = task.input["replay"].clone();
            runtime.commit(&runtime.ctx(), |_, _| {
                Ok(running(json!({ "phase": "execute", "replay": replay })))
            })?;
            append(&e1, name);
            if child {
                Deferred::new().wait();
            }
            runtime.commit(&runtime.ctx(), |_, _| Ok(completed(json!({ "runs": 1 }))))
        })
        .phase("execute", move |task, runtime| {
            if task.input["replay"] == "safe" {
                append(&e2, name);
                let runs = lines(&e2).iter().filter(|line| *line == name).count();
                return runtime.commit(&runtime.ctx(), |_, _| Ok(completed(json!({ "runs": runs }))));
            }
            let message = format!("Effect {name} was interrupted and may have partially run");
            runtime.commit(&runtime.ctx(), |_, _| {
                Ok(Some(harness::scheduler::NextState::Terminal {
                    outcome: durable::TaskOutcome::Failed {
                        error: durable::TaskOutcomeError { message, detail: None },
                        result: None,
                    },
                }))
            })
        })
        .build()
    };
    let (g, e3) = (gates, effects.clone());
    let hold = Task::define("kill.hold", 1, |_| json!({ "phase": "hold" }))
        .phase("hold", move |task, runtime| {
            let name = task.input["name"].as_str().unwrap().to_owned();
            append(&e3, &format!("hold:{name}"));
            if task.input["done"] == true {
                return runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)));
            }
            let gate = g.gate(&name);
            Cancel::any(&[&gate, runtime.signal()]).signal().wait();
            if gate.is_cancelled() {
                runtime.commit(&runtime.ctx(), |_, _| Ok(completed(Value::Null)))
            } else {
                Err(runtime.signal().reason().unwrap())
            }
        })
        .abort(|task, runtime| {
            let reason = if task.abort_reason.is_some() {
                "restart"
            } else {
                "request"
            };
            runtime.commit(&runtime.ctx(), |_, _| Ok(aborted_with(reason)))
        })
        .build();
    vec![effect("kill.safe"), effect("kill.unsafe"), hold]
}

/// Storage that never finishes a commit after the one that abort-marks `freeze_after`: the process is killed with
/// the next commit (the abort cascade) in flight.
struct Freezing {
    inner: Box<dyn Storage + Send>,
    freeze_after: Arc<Mutex<Option<TaskId>>>,
    frozen: Arc<AtomicU8>,
    ready: PathBuf,
}

impl Storage for Freezing {
    fn commit(&mut self, writes: Vec<StorageWrite>) -> Result<Seq, StorageError> {
        if self.frozen.load(Ordering::SeqCst) == 1 {
            append(&self.ready, "frozen");
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        let target = *self.freeze_after.lock().unwrap();
        let marks = writes.iter().any(
            |write| matches!(write, StorageWrite::Task { value } if Some(value.id) == target && value.abort_requested),
        );
        let seq = self.inner.commit(writes)?;
        if marks {
            self.frozen.store(1, Ordering::SeqCst);
        }
        Ok(seq)
    }
    fn mint_id(&mut self) -> Result<u64, StorageError> {
        self.inner.mint_id()
    }
    fn conversation(&self, id: durable::ConversationId) -> Result<Option<durable::ConversationRecord>, StorageError> {
        self.inner.conversation(id)
    }
    fn scan_conversations(
        &self,
        query: &durable::ConversationQuery,
        limit: usize,
        cursor: Option<&durable::Cursor>,
    ) -> Result<durable::Page<durable::ConversationRecord>, StorageError> {
        self.inner.scan_conversations(query, limit, cursor)
    }
    fn entry(&self, id: durable::EntryId) -> Result<Option<durable::StoredEntry>, StorageError> {
        self.inner.entry(id)
    }
    fn visible_entry(
        &self,
        conversation_id: durable::ConversationId,
        id: durable::EntryId,
    ) -> Result<Option<durable::StoredEntry>, StorageError> {
        self.inner.visible_entry(conversation_id, id)
    }
    fn find_latest_head_marker(
        &self,
        conversation_id: durable::ConversationId,
        at_or_before: Option<durable::EntryId>,
    ) -> Result<Option<durable::EntryRecord>, StorageError> {
        self.inner.find_latest_head_marker(conversation_id, at_or_before)
    }
    fn scan_entries(
        &self,
        query: &durable::EntryQuery,
        limit: usize,
        cursor: Option<&durable::Cursor>,
    ) -> Result<durable::Page<durable::EntryRecord>, StorageError> {
        self.inner.scan_entries(query, limit, cursor)
    }
    fn task(&self, id: TaskId) -> Result<Option<TaskRecord>, StorageError> {
        self.inner.task(id)
    }
    fn scan_tasks(
        &self,
        query: &durable::TaskQuery,
        limit: usize,
        cursor: Option<&durable::Cursor>,
    ) -> Result<durable::Page<TaskRecord>, StorageError> {
        self.inner.scan_tasks(query, limit, cursor)
    }
    fn submission(&self, id: durable::SubmissionId) -> Result<Option<durable::SubmissionRecord>, StorageError> {
        self.inner.submission(id)
    }
    fn scan_submissions(
        &self,
        query: &durable::SubmissionQuery,
        limit: usize,
        cursor: Option<&durable::Cursor>,
    ) -> Result<durable::Page<durable::SubmissionRecord>, StorageError> {
        self.inner.scan_submissions(query, limit, cursor)
    }
    fn submission_by_request(
        &self,
        conversation_id: durable::ConversationId,
        request_id: &str,
    ) -> Result<Option<durable::SubmissionRecord>, StorageError> {
        self.inner.submission_by_request(conversation_id, request_id)
    }
    fn find_document(
        &self,
        address: &durable::DocumentAddress,
        at: durable::DocumentPoint,
    ) -> Result<Option<durable::DocumentRecord>, StorageError> {
        self.inner.find_document(address, at)
    }
    fn document(
        &self,
        id: durable::DocumentId,
        at: durable::DocumentPoint,
    ) -> Result<Option<durable::StoredDocument>, StorageError> {
        self.inner.document(id, at)
    }
    fn scan_documents(
        &self,
        query: &durable::DocumentQuery,
        limit: usize,
        cursor: Option<&durable::Cursor>,
    ) -> Result<durable::Page<durable::DocumentRecord>, StorageError> {
        self.inner.scan_documents(query, limit, cursor)
    }
    fn close(&mut self) -> Result<(), StorageError> {
        self.inner.close()
    }
}

fn hold_input(name: &str) -> Value {
    json!({ "name": name })
}

/// Child mode: build the state, start every effect, abort-mark one owner, and block until killed.
fn child_main(journal: &Path, dir: &Path) {
    let effects = dir.join("effects.log");
    let freeze_after: Arc<Mutex<Option<TaskId>>> = Arc::default();
    let storage = Freezing {
        inner: Box::new(open_journal(journal)),
        freeze_after: freeze_after.clone(),
        frozen: Arc::default(),
        ready: dir.join("ready"),
    };
    let registry = Registry::new();
    let tasks = kill_tasks(effects.clone(), true, Gates::default());
    registry.install(Extension::tasks("kill", tasks.clone())).unwrap();
    let harness = Harness::open(storage, HarnessOptions::new(Arc::new(registry))).unwrap();
    let root = harness.root().unwrap();
    let ids = root
        .commit(|tx| {
            let (safe, unsafe_, hold) = (&tasks[0], &tasks[1], &tasks[2]);
            let mut ids = serde_json::Map::new();
            let mut create = |tx: &mut durable::Tx<'_>, key: &str, task: &Task, input: Value, options: TaskOptions| {
                let id = tx.create_task(task.definition(), input, options)?;
                ids.insert(key.to_owned(), json!(id.0));
                Ok::<TaskId, HarnessError>(id)
            };
            create(tx, "safe", safe, json!({ "replay": "safe" }), TaskOptions::default())?;
            create(
                tx,
                "unsafe",
                unsafe_,
                json!({ "replay": "unsafe" }),
                TaskOptions::default(),
            )?;
            let abandon = TaskOptions {
                abandon_on_restart: true,
                ..TaskOptions::default()
            };
            create(tx, "abandon", hold, hold_input("abandon"), abandon)?;
            let owner = create(tx, "owner", hold, hold_input("owner"), TaskOptions::default())?;
            let child = tx.create_conversation(durable::ConversationOwnership::Task(owner))?;
            let in_child = TaskOptions {
                conversation_id: Some(child.id),
                ..TaskOptions::default()
            };
            create(tx, "inner", hold, hold_input("inner"), in_child)?;
            let done = json!({ "name": "owner2", "done": true });
            let owner2 = create(tx, "owner2", hold, done, TaskOptions::default())?;
            let child2 = tx.create_conversation(durable::ConversationOwnership::Task(owner2))?;
            let in_child2 = TaskOptions {
                conversation_id: Some(child2.id),
                ..TaskOptions::default()
            };
            create(tx, "inner2", hold, hold_input("inner2"), in_child2)?;
            Ok(Value::Object(ids))
        })
        .unwrap();
    // Acknowledged: the commit returned.
    std::fs::write(dir.join("ids.json"), ids.to_string()).unwrap();
    harness.resume().unwrap();
    let expected = [
        "kill.safe",
        "kill.unsafe",
        "hold:abandon",
        "hold:owner",
        "hold:inner",
        "hold:owner2",
        "hold:inner2",
    ];
    // No deadline of its own: the parent bounds the whole child and kills it (a 10 s `eventually` here panicked
    // under a loaded `cargo test --workspace` before the effects ran, and the child exited early).
    let stage = dir.join("stage.log");
    append(&stage, "resumed");
    until(|| {
        let seen = lines(&effects);
        expected.iter().all(|line| seen.iter().any(|each| each == line))
    });
    append(&stage, "effects");
    let id = |key: &str| TaskId(ids[key].as_u64().unwrap());
    let dumped = std::cell::Cell::new(false);
    let started = Instant::now();
    until(|| {
        // Diagnostics only (the parent owns the deadline): what every task looks like when this takes long.
        if !dumped.get() && started.elapsed() > Duration::from_secs(30) {
            dumped.set(true);
            for key in ["safe", "unsafe", "abandon", "owner", "inner", "owner2", "inner2"] {
                append(
                    &stage,
                    &format!("{key}: {}", js(&harness.get_task(id(key)).unwrap().unwrap())),
                );
            }
        }
        harness.get_task(id("owner2")).unwrap().unwrap().state.status() == TaskStatus::Completing
    });
    append(&stage, "completing");
    let owner = id("owner");
    *freeze_after.lock().unwrap() = Some(owner);
    harness
        .commit(|tx| {
            let record = tx.task(owner)?.unwrap();
            tx.set_task(TaskRecord {
                abort_requested: true,
                ..record
            })?;
            Ok(())
        })
        .unwrap();
    append(&dir.join("acked.log"), "mark");
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

/// Poll until `check` holds; the child's caller (the parent test) owns the deadline.
fn until(mut check: impl FnMut() -> bool) {
    while !check() {
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Child entry point: does nothing unless the parent test re-ran this binary in child mode.
#[test]
fn session_core_durable_kill_child_process() {
    let (Ok(journal), Ok(dir)) = (std::env::var(CHILD_ENV), std::env::var(DIR_ENV)) else {
        return;
    };
    child_main(Path::new(&journal), Path::new(&dir));
}

#[test]
fn session_core_durable_kill_reopen_journal_recovers_after_sigkill_mid_phase() {
    let dir = tempfile::tempdir().unwrap();
    let path = create_session(dir.path());
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "session_core_durable_kill_child_process",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, &path)
        .env(DIR_ENV, dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(dir.path().join("child.stderr")).unwrap())
        .spawn()
        .unwrap();
    let stderr = || std::fs::read_to_string(dir.path().join("child.stderr")).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(120);
    while !(dir.path().join("ready").exists() && dir.path().join("acked.log").exists()) {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let seen = |name: &str| dir.path().join(name).exists();
            panic!(
                "child never reached the kill point (stages: {:?}, ready: {}, acked: {}, effects: {:?}); its stderr:\n{}",
                lines(&dir.path().join("stage.log")),
                seen("ready"),
                seen("acked.log"),
                lines(&dir.path().join("effects.log")),
                stderr()
            );
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited early ({status}); its stderr:\n{}", stderr());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // SIGKILL mid-phase: handlers are blocked in their effects and the cascade commit is in flight.
    child.kill().unwrap();
    child.wait().unwrap();

    let effects = dir.path().join("effects.log");
    let ids: Value = serde_json::from_str(&std::fs::read_to_string(dir.path().join("ids.json")).unwrap()).unwrap();
    let id = |key: &str| TaskId(ids[key].as_u64().unwrap());

    // What the kill left durable, read before any Harness derives anything.
    let acknowledged = durable_commits(&path);
    assert!(
        acknowledged > 0,
        "the child's acknowledged commits are in the session file"
    );
    {
        let mut raw = open_journal(&path);
        let state = |key: &str| js(&raw.task(id(key)).unwrap().unwrap());
        support::assert_matches(
            &state("owner"),
            json!({ "abortRequested": true, "state": { "status": "running" } }),
        );
        // The cascade commit never landed.
        support::assert_matches(
            &state("inner"),
            json!({ "abortRequested": false, "state": { "status": "running" } }),
        );
        support::assert_matches(
            &state("owner2"),
            json!({ "state": { "status": "completing", "outcome": { "status": "completed" } } }),
        );
        for key in ["safe", "unsafe"] {
            support::assert_matches(
                &state(key),
                json!({ "state": { "status": "running", "checkpoint": { "phase": "execute" } } }),
            );
        }
        raw.close().unwrap();
    }
    assert_eq!(lines(&effects).iter().filter(|line| *line == "kill.safe").count(), 1);

    let gates = Gates::default();
    let registry = Registry::new();
    registry
        .install(Extension::tasks(
            "kill",
            kill_tasks(effects.clone(), false, gates.clone()),
        ))
        .unwrap();
    let storage = Freezing {
        inner: Box::new(open_journal(&path)),
        freeze_after: Arc::default(),
        frozen: Arc::default(),
        ready: dir.path().join("unused"),
    };
    let harness = Harness::open(storage, HarnessOptions::new(Arc::new(registry))).unwrap();
    // Nothing acknowledged is lost: every task the child created is there.
    for key in ["safe", "unsafe", "abandon", "owner", "inner", "owner2", "inner2"] {
        assert!(harness.get_task(id(key)).unwrap().is_some(), "{key} lost");
    }
    // Open reconciled `running` to `pending`, keeping the intent checkpoint.
    support::assert_matches(
        &js(&harness.get_task(id("safe")).unwrap().unwrap()),
        json!({ "state": { "status": "pending", "checkpoint": { "phase": "execute", "replay": "safe" } } }),
    );
    harness.resume().unwrap();
    let outcome = |key: &str| js(&harness.wait_for_task(id(key), &bg()).unwrap().state)["outcome"].clone();
    // The replay-safe phase reruns its effect.
    assert_eq!(
        outcome("safe"),
        json!({ "status": "completed", "result": { "runs": 2 } })
    );
    // The unsafe one yields an interrupted outcome and never runs its effect again.
    assert_eq!(
        outcome("unsafe"),
        json!({ "status": "failed", "error": { "message": "Effect kill.unsafe was interrupted and may have partially run" } })
    );
    assert_eq!(lines(&effects).iter().filter(|line| *line == "kill.unsafe").count(), 1);
    // The cascade below the abort-marked owner is re-derived; the owner aborts after the work below it.
    assert_eq!(outcome("inner"), json!({ "status": "aborted", "reason": "request" }));
    assert_eq!(outcome("owner"), json!({ "status": "aborted", "reason": "request" }));
    // Work abandoned by the dead Harness is aborted with reason `restart`.
    assert_eq!(outcome("abandon"), json!({ "status": "aborted", "reason": "restart" }));
    let abandoned = harness.get_task(id("abandon")).unwrap().unwrap();
    assert_eq!(abandoned.abort_reason, Some(durable::AbortReason::Restart));
    // The `completing` hold is re-derived: the completed owner waits for its live owned work.
    eventually(|| lines(&effects).iter().filter(|line| *line == "hold:inner2").count() == 2);
    // Settle, then check the owner is still completing: a slow machine can only let this pass wrongly.
    support::flush();
    assert_eq!(
        harness.get_task(id("owner2")).unwrap().unwrap().state.status(),
        TaskStatus::Completing
    );
    gates.gate("inner2").abort();
    assert_eq!(outcome("inner2"), json!({ "status": "completed", "result": null }));
    assert_eq!(outcome("owner2"), json!({ "status": "completed", "result": null }));
    harness.close().unwrap();
    // Recovery committed through the same journal: the reopened file holds more durable commits than the kill left.
    assert!(durable_commits(&path) > acknowledged);
}
