//! Port of upstream `test/jsonl-storage.test.ts` ("Pico JsonlStorage publication and recovery"), with upstream's
//! `InstrumentedEnv` as [`Instrumented`]: a filesystem that records operations and injects one failure.

use durable::{
    DocumentId, DocumentPoint, EntryId, JsonlFs, JsonlOptions, JsonlStorage, ROOT_CONVERSATION_ID, Seq, StdFs, Storage,
    StorageWrite, TaskId,
};
use serde_json::{Value, json};
use std::io;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    Append,
    Flush,
    Write,
    Rename,
    Remove,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Before,
    After,
    Short,
}

#[derive(Default)]
struct Instrumented {
    operations: Vec<String>,
    failure: Option<(Op, usize, Mode)>,
    calls: Vec<Op>,
}

impl Instrumented {
    fn fail(&mut self, op: Op, call: usize, mode: Mode) {
        self.clear();
        self.failure = Some((op, call, mode));
    }

    fn clear(&mut self) {
        self.failure = None;
        self.operations.clear();
        self.calls.clear();
    }

    /// Record the call; the injected mode when this is the failing call.
    fn observe(&mut self, op: Op, label: String) -> Option<Mode> {
        self.operations.push(label);
        self.calls.push(op);
        let count = self.calls.iter().filter(|o| **o == op).count();
        match self.failure {
            Some((failing, call, mode)) if failing == op && call == count => Some(mode),
            _ => None,
        }
    }
}

fn name(path: &Path) -> String {
    path.file_name().unwrap().to_string_lossy().into_owned()
}

fn injected(what: &str) -> io::Error {
    io::Error::other(format!("injected {what}"))
}

impl JsonlFs for Instrumented {
    fn create_dir_all(&mut self, path: &Path) -> io::Result<()> {
        StdFs.create_dir_all(path)
    }
    fn list(&mut self, dir: &Path) -> io::Result<Vec<(String, bool)>> {
        StdFs.list(dir)
    }
    fn read(&mut self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        StdFs.read(path)
    }
    fn truncate(&mut self, path: &Path, len: u64) -> io::Result<()> {
        StdFs.truncate(path, len)
    }
    fn append(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        match self.observe(Op::Append, format!("append:{}", name(path))) {
            None => StdFs.append(path, bytes),
            Some(Mode::Before) => Err(injected("append failure")),
            Some(Mode::Short) => {
                StdFs.append(path, &bytes[..(bytes.len() / 2).max(1)])?;
                Err(injected("short append"))
            }
            Some(Mode::After) => {
                StdFs.append(path, bytes)?;
                Err(injected("post-append failure"))
            }
        }
    }
    fn flush(&mut self, path: &Path) -> io::Result<()> {
        match self.observe(Op::Flush, format!("flush:{}", name(path))) {
            None => StdFs.flush(path),
            Some(Mode::After) => {
                StdFs.flush(path)?;
                Err(injected("flush failure"))
            }
            Some(_) => Err(injected("flush failure")),
        }
    }
    fn write(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        match self.observe(Op::Write, format!("write:{}", name(path))) {
            None => StdFs.write(path, bytes),
            Some(Mode::Before) => Err(injected("write failure")),
            Some(Mode::Short) => {
                StdFs.write(path, &bytes[..(bytes.len() / 2).max(1)])?;
                Err(injected("short write"))
            }
            Some(Mode::After) => {
                StdFs.write(path, bytes)?;
                Err(injected("post-write failure"))
            }
        }
    }
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        match self.observe(Op::Rename, format!("rename:{}->{}", name(from), name(to))) {
            None => StdFs.rename(from, to),
            Some(Mode::After) => {
                StdFs.rename(from, to)?;
                Err(injected("rename failure"))
            }
            Some(_) => Err(injected("rename failure")),
        }
    }
    fn remove(&mut self, path: &Path) -> io::Result<()> {
        match self.observe(Op::Remove, format!("remove:{}", name(path))) {
            None => StdFs.remove(path),
            Some(Mode::After) => {
                StdFs.remove(path)?;
                Err(injected("remove failure"))
            }
            Some(_) => Err(injected("remove failure")),
        }
    }
}

type Instrumented_ = JsonlStorage<Instrumented>;

fn open(directory: &Path) -> JsonlStorage {
    JsonlStorage::open(directory, JsonlOptions::default()).unwrap()
}

fn open_instrumented(directory: &Path, fsync: bool) -> Instrumented_ {
    JsonlStorage::open_with(directory, Instrumented::default(), JsonlOptions { fsync }).unwrap()
}

fn writes(value: Value) -> Vec<StorageWrite> {
    serde_json::from_value(value).unwrap()
}

fn commit(storage: &mut impl Storage, value: Value) -> Seq {
    storage.commit(writes(value)).unwrap()
}

fn create_root(storage: &mut impl Storage) {
    commit(storage, json!([{ "type": "conversation", "value": { "id": 1 } }]));
}

fn pending_task(id: u64, phase: &str) -> Value {
    json!({ "id": id, "conversationId": 1, "kind": "test.task", "version": 1, "input": null,
            "state": { "status": "pending", "checkpoint": { "phase": phase } },
            "background": false, "abortRequested": false })
}

fn terminal_task(id: u64) -> Value {
    json!({ "id": id, "conversationId": 1, "kind": "test.task", "version": 1, "input": null,
            "state": { "status": "terminal", "outcome": { "status": "completed", "result": null } },
            "background": false, "abortRequested": false })
}

fn session_document(id: u64, kind: &str, value: Value) -> Value {
    json!({ "type": "document.create", "record": { "id": id, "kind": kind, "scope": { "kind": "session" } },
            "content": { "kind": "base", "version": 1, "value": value } })
}

fn change(id: u64, content: Value) -> Value {
    json!([{ "type": "document.change", "id": id, "content": content }])
}

fn base(value: Value) -> Value {
    json!({ "kind": "base", "version": 1, "value": value })
}

fn delta(ops: Value) -> Value {
    json!({ "kind": "delta", "version": 1, "ops": ops })
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn current(storage: &impl Storage, id: u64) -> Option<Value> {
    storage
        .document(DocumentId(id), DocumentPoint::Current)
        .unwrap()
        .map(|stored| Value::Object(stored.value))
}

fn task(storage: &impl Storage, id: u64) -> Option<Value> {
    storage
        .task(TaskId(id))
        .unwrap()
        .map(|task| serde_json::to_value(task).unwrap())
}

fn reclaim_files(directory: &Path) -> Vec<String> {
    std::fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".reclaim"))
        .collect()
}

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn jsonl_opens_on_the_local_filesystem() {
    let directory = temp();
    let mut storage = open(directory.path());
    create_root(&mut storage);
    assert_eq!(
        serde_json::to_value(storage.conversation(ROOT_CONVERSATION_ID).unwrap()).unwrap(),
        json!({ "id": 1 })
    );
}

#[test]
fn jsonl_publishes_every_commit_before_reclaiming_current_only_document_and_terminal_task_sidecars() {
    let directory = temp();
    let dir = directory.path();
    let mut storage = open(dir);
    create_root(&mut storage);
    let task_id = storage.mint_id().unwrap();
    let document_id = storage.mint_id().unwrap();
    commit(
        &mut storage,
        json!([{ "type": "task", "value": pending_task(task_id, "ready") }]),
    );
    commit(
        &mut storage,
        json!([session_document(document_id, "test.document", json!({ "count": 0 }))]),
    );
    commit(&mut storage, change(document_id, delta(json!([]))));
    commit(&mut storage, change(document_id, base(json!({ "count": 1 }))));
    assert_eq!(lines(&dir.join(format!("task-{task_id}.jsonl"))).len(), 1);
    assert_eq!(lines(&dir.join(format!("doc-{document_id}.jsonl"))).len(), 1);

    commit(&mut storage, json!([{ "type": "document.retire", "id": document_id }]));
    commit(
        &mut storage,
        json!([{ "type": "task", "value": terminal_task(task_id) }]),
    );
    let main = lines(&dir.join("main.jsonl"));
    assert_eq!(main.len(), 7);
    assert!(!dir.join(format!("task-{task_id}.jsonl")).exists());
    assert!(!dir.join(format!("doc-{document_id}.jsonl")).exists());
    for marker in main {
        assert_eq!(serde_json::from_str::<Value>(&marker).unwrap()["type"], "commit");
    }
}

#[test]
fn jsonl_orders_multiple_live_task_replacements_in_one_sidecar_by_commit_ordinal() {
    let directory = temp();
    let mut storage = open(directory.path());
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    commit(
        &mut storage,
        json!([{ "type": "task", "value": pending_task(id, "first") }, { "type": "task", "value": pending_task(id, "second") }]),
    );
    assert_eq!(lines(&directory.path().join(format!("task-{id}.jsonl"))).len(), 2);
    assert_eq!(task(&open(directory.path()), id), Some(pending_task(id, "second")));
}

#[test]
fn jsonl_removes_a_complete_plus_torn_unconfirmed_multi_record_sidecar_append() {
    let directory = temp();
    let mut storage = open_instrumented(directory.path(), false);
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    storage.fs_mut().fail(Op::Append, 1, Mode::Short);
    let long = format!("second-{}", "x".repeat(512));
    let error = storage
        .commit(writes(json!([{ "type": "task", "value": pending_task(id, "first") }, { "type": "task", "value": pending_task(id, &long) }])))
        .unwrap_err();
    assert!(error.message().contains("poisoned"), "{error}");
    let path = directory.path().join(format!("task-{id}.jsonl"));
    let partial = std::fs::read(&path).unwrap();
    assert_ne!(partial.last(), Some(&b'\n'));
    assert_eq!(partial.iter().filter(|b| **b == b'\n').count(), 1);

    let reopened = open(directory.path());
    assert_eq!(task(&reopened, id), None);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
}

#[test]
fn jsonl_leaves_preparation_failures_usable_without_io() {
    // Upstream fails serialization with a BigInt, which Rust values cannot hold; a commit rejected during
    // preparation must likewise perform no I/O and leave the storage usable.
    let directory = temp();
    let mut storage = open_instrumented(directory.path(), false);
    create_root(&mut storage);
    storage.fs_mut().clear();
    let id = storage.mint_id().unwrap();
    let error = storage
        .commit(writes(json!([{ "type": "conversation", "value": { "id": 1 } }])))
        .unwrap_err();
    assert_eq!(error.message(), "ID 1 already belongs to conversation");
    assert!(storage.fs_mut().operations.is_empty());
    assert_eq!(storage.entry(EntryId(id)).unwrap(), None);
    let seq = commit(
        &mut storage,
        json!([{ "type": "entry", "value": { "id": id, "conversationId": 1, "kind": "good" } }]),
    );
    assert_eq!(seq, Seq(2));
}

#[test]
fn jsonl_poisons_after_append_failures_and_recovers_only_confirmed_state() {
    use Mode::*;
    for (call, mode) in [
        (1, Before),
        (1, After),
        (1, Short),
        (2, Before),
        (2, After),
        (3, Before),
        (3, Short),
        (3, After),
    ] {
        let directory = temp();
        let mut storage = open_instrumented(directory.path(), false);
        create_root(&mut storage);
        let first = storage.mint_id().unwrap();
        let second = storage.mint_id().unwrap();
        storage.fs_mut().fail(Op::Append, call, mode);
        let error = storage
            .commit(writes(json!([
                session_document(first, "first", json!({ "text": "α" })),
                session_document(second, "second", json!({ "text": "β" })),
            ])))
            .unwrap_err();
        assert!(error.message().contains("poisoned"), "{call} {mode:?}: {error}");
        let read = storage.document(DocumentId(first), DocumentPoint::Current).unwrap_err();
        assert!(read.message().contains("poisoned"), "{call} {mode:?}: {read}");

        let reopened = open(directory.path());
        let survived = call == 3 && mode == After;
        assert_eq!(
            current(&reopened, first),
            survived.then(|| json!({ "text": "α" })),
            "{call} {mode:?}"
        );
        assert_eq!(
            current(&reopened, second),
            survived.then(|| json!({ "text": "β" })),
            "{call} {mode:?}"
        );
    }
}

#[test]
fn jsonl_poisons_after_flush_failures_and_never_writes_a_marker() {
    for (call, mode) in [(1, Mode::Before), (1, Mode::After), (2, Mode::Before), (2, Mode::After)] {
        let directory = temp();
        let mut storage = open_instrumented(directory.path(), true);
        create_root(&mut storage);
        let first = storage.mint_id().unwrap();
        let second = storage.mint_id().unwrap();
        storage.fs_mut().fail(Op::Flush, call, mode);
        let error = storage
            .commit(writes(json!([
                session_document(first, "flush.first", json!({})),
                session_document(second, "flush.second", json!({})),
            ])))
            .unwrap_err();
        assert!(error.message().contains("poisoned"), "{call} {mode:?}: {error}");
        assert!(!storage.fs_mut().operations.contains(&"append:main.jsonl".to_owned()));
        let reopened = open(directory.path());
        assert_eq!(current(&reopened, first), None);
        assert_eq!(current(&reopened, second), None);
    }
}

#[test]
fn jsonl_orders_publication_flushes_exactly_and_flushes_main_only_to_authorize_reclamation() {
    for fsync in [false, true] {
        let directory = temp();
        let mut storage = open_instrumented(directory.path(), fsync);
        create_root(&mut storage);
        let first = storage.mint_id().unwrap();
        let second = storage.mint_id().unwrap();
        storage.fs_mut().clear();
        commit(
            &mut storage,
            json!([
                session_document(second, "second", json!({})),
                session_document(first, "first", json!({}))
            ]),
        );
        let mut expected = vec![
            format!("append:doc-{second}.jsonl"),
            format!("append:doc-{first}.jsonl"),
        ];
        if fsync {
            expected.extend([format!("flush:doc-{second}.jsonl"), format!("flush:doc-{first}.jsonl")]);
        }
        expected.push("append:main.jsonl".to_owned());
        assert_eq!(storage.fs_mut().operations, expected, "fsync {fsync}");

        storage.fs_mut().clear();
        commit(&mut storage, change(first, base(json!({ "checkpoint": true }))));
        let mut expected = vec![format!("append:doc-{first}.jsonl")];
        if fsync {
            expected.push(format!("flush:doc-{first}.jsonl"));
        }
        expected.push("append:main.jsonl".to_owned());
        if fsync {
            expected.push("flush:main.jsonl".to_owned());
        }
        expected.push(format!("write:doc-{first}.jsonl.reclaim"));
        if fsync {
            expected.push(format!("flush:doc-{first}.jsonl.reclaim"));
        }
        expected.push(format!("rename:doc-{first}.jsonl.reclaim->doc-{first}.jsonl"));
        assert_eq!(storage.fs_mut().operations, expected, "fsync {fsync}");

        storage.fs_mut().clear();
        let entry = storage.mint_id().unwrap();
        commit(
            &mut storage,
            json!([{ "type": "entry", "value": { "id": entry, "conversationId": 1, "kind": "main-only" } }]),
        );
        assert_eq!(storage.fs_mut().operations, vec!["append:main.jsonl".to_owned()]);

        let task_id = storage.mint_id().unwrap();
        commit(
            &mut storage,
            json!([{ "type": "task", "value": pending_task(task_id, "ready") }]),
        );
        storage.fs_mut().clear();
        commit(
            &mut storage,
            json!([{ "type": "task", "value": terminal_task(task_id) }]),
        );
        let mut expected = vec!["append:main.jsonl".to_owned()];
        if fsync {
            expected.push("flush:main.jsonl".to_owned());
        }
        expected.push(format!("remove:task-{task_id}.jsonl"));
        assert_eq!(storage.fs_mut().operations, expected, "fsync {fsync}");
    }
}

#[test]
fn jsonl_recovers_a_committed_base_across_reclaim_write_and_rename_failures() {
    use Mode::*;
    for (op, mode) in [
        (Op::Write, Before),
        (Op::Write, Short),
        (Op::Write, After),
        (Op::Rename, Before),
        (Op::Rename, After),
    ] {
        let directory = temp();
        let mut storage = open_instrumented(directory.path(), false);
        create_root(&mut storage);
        let id = storage.mint_id().unwrap();
        commit(
            &mut storage,
            json!([session_document(id, "test.document", json!({ "count": 0 }))]),
        );
        commit(&mut storage, change(id, delta(json!([["s", ["count"], 1]]))));
        storage.fs_mut().fail(op, 1, mode);
        assert_eq!(
            commit(&mut storage, change(id, base(json!({ "count": 2 })))),
            Seq(4),
            "{op:?} {mode:?}"
        );
        assert_eq!(current(&storage, id), Some(json!({ "count": 2 })));
        storage.close().unwrap();

        let reopened = open(directory.path());
        assert_eq!(current(&reopened, id), Some(json!({ "count": 2 })), "{op:?} {mode:?}");
        assert_eq!(
            lines(&directory.path().join(format!("doc-{id}.jsonl"))).len(),
            1,
            "{op:?} {mode:?}"
        );
        assert!(reclaim_files(directory.path()).is_empty(), "{op:?} {mode:?}");
    }
}

#[test]
fn jsonl_recovers_document_retirement_reclamation_across_remove_failures() {
    for mode in [Mode::Before, Mode::After] {
        let directory = temp();
        let mut storage = open_instrumented(directory.path(), false);
        create_root(&mut storage);
        let task_id = storage.mint_id().unwrap();
        let id = storage.mint_id().unwrap();
        commit(
            &mut storage,
            json!([
                { "type": "task", "value": pending_task(task_id, "ready") },
                { "type": "document.create", "record": { "id": id, "kind": "task.document", "scope": { "kind": "task", "taskId": task_id } },
                  "content": base(json!({ "count": 1 })) },
            ]),
        );
        storage.fs_mut().fail(Op::Remove, 1, mode);
        assert_eq!(
            commit(&mut storage, json!([{ "type": "document.retire", "id": id }])),
            Seq(3)
        );
        assert_eq!(current(&storage, id), None);
        storage.close().unwrap();

        let reopened = open(directory.path());
        assert_eq!(current(&reopened, id), None);
        assert_eq!(task(&reopened, task_id), Some(pending_task(task_id, "ready")));
        assert!(!directory.path().join(format!("doc-{id}.jsonl")).exists(), "{mode:?}");
        assert!(reclaim_files(directory.path()).is_empty());
    }
}

#[test]
fn jsonl_defers_reclamation_after_authorizing_main_flush_failure() {
    for mode in [Mode::Before, Mode::After] {
        let directory = temp();
        let dir = directory.path();
        let mut storage = open_instrumented(dir, true);
        create_root(&mut storage);
        let id = storage.mint_id().unwrap();
        commit(
            &mut storage,
            json!([session_document(id, "test.document", json!({ "count": 0 }))]),
        );
        storage.fs_mut().fail(Op::Flush, 2, mode);
        assert_eq!(commit(&mut storage, change(id, base(json!({ "count": 2 })))), Seq(3));
        assert_eq!(
            storage.fs_mut().operations,
            [
                format!("append:doc-{id}.jsonl"),
                format!("flush:doc-{id}.jsonl"),
                "append:main.jsonl".to_owned(),
                "flush:main.jsonl".to_owned()
            ]
        );
        assert_eq!(current(&storage, id), Some(json!({ "count": 2 })));
        assert_eq!(lines(&dir.join(format!("doc-{id}.jsonl"))).len(), 2);
        storage.close().unwrap();

        let mut recovery = Instrumented::default();
        recovery.fail(Op::Flush, 1, mode);
        let mut deferred = JsonlStorage::open_with(dir, recovery, JsonlOptions { fsync: true }).unwrap();
        assert_eq!(current(&deferred, id), Some(json!({ "count": 2 })));
        assert_eq!(deferred.fs_mut().operations, ["flush:main.jsonl"]);
        assert_eq!(lines(&dir.join(format!("doc-{id}.jsonl"))).len(), 2);
        deferred.close().unwrap();

        let reclaimed = JsonlStorage::open(dir, JsonlOptions { fsync: true }).unwrap();
        assert_eq!(current(&reclaimed, id), Some(json!({ "count": 2 })));
        assert_eq!(lines(&dir.join(format!("doc-{id}.jsonl"))).len(), 1);
    }
}

#[test]
fn jsonl_keeps_a_committed_base_usable_after_reclaim_temp_flush_failure() {
    for mode in [Mode::Before, Mode::After] {
        let directory = temp();
        let mut storage = open_instrumented(directory.path(), true);
        create_root(&mut storage);
        let id = storage.mint_id().unwrap();
        commit(
            &mut storage,
            json!([session_document(id, "test.document", json!({ "count": 0 }))]),
        );
        storage.fs_mut().fail(Op::Flush, 3, mode);
        assert_eq!(commit(&mut storage, change(id, base(json!({ "count": 2 })))), Seq(3));
        storage.fs_mut().clear();
        commit(&mut storage, change(id, delta(json!([["s", ["count"], 3]]))));
        storage.close().unwrap();

        let reopened = JsonlStorage::open(directory.path(), JsonlOptions { fsync: true }).unwrap();
        assert_eq!(current(&reopened, id), Some(json!({ "count": 3 })), "{mode:?}");
        assert_eq!(
            lines(&directory.path().join(format!("doc-{id}.jsonl"))).len(),
            2,
            "{mode:?}"
        );
    }
}

#[test]
fn jsonl_recovers_terminal_task_reclamation_across_remove_failures() {
    for mode in [Mode::Before, Mode::After] {
        let directory = temp();
        let mut storage = open_instrumented(directory.path(), false);
        create_root(&mut storage);
        let id = storage.mint_id().unwrap();
        commit(
            &mut storage,
            json!([{ "type": "task", "value": pending_task(id, "ready") }]),
        );
        storage.fs_mut().fail(Op::Remove, 1, mode);
        assert_eq!(
            commit(&mut storage, json!([{ "type": "task", "value": terminal_task(id) }])),
            Seq(3)
        );
        assert_eq!(task(&storage, id), Some(terminal_task(id)));
        storage.close().unwrap();

        let reopened = open(directory.path());
        assert_eq!(task(&reopened, id), Some(terminal_task(id)));
        assert!(!directory.path().join(format!("task-{id}.jsonl")).exists(), "{mode:?}");
        assert!(reclaim_files(directory.path()).is_empty());
    }
}

#[test]
fn jsonl_appends_later_deltas_to_the_replacement_sidecar_after_a_current_only_base() {
    let directory = temp();
    let mut storage = open(directory.path());
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    commit(
        &mut storage,
        json!([session_document(id, "test.document", json!({ "count": 0 }))]),
    );
    commit(&mut storage, change(id, base(json!({ "count": 10 }))));
    commit(&mut storage, change(id, delta(json!([["s", ["count"], 11]]))));
    assert_eq!(lines(&directory.path().join(format!("doc-{id}.jsonl"))).len(), 2);
    assert_eq!(current(&open(directory.path()), id), Some(json!({ "count": 11 })));
}

#[test]
fn jsonl_never_reclaims_rewindable_document_history() {
    let directory = temp();
    let mut storage = open(directory.path());
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    let created = commit(
        &mut storage,
        json!([{ "type": "document.create",
                 "record": { "id": id, "kind": "rewindable", "scope": { "kind": "conversation", "conversationId": 1 },
                             "history": "rewindable", "fork": "asOf" },
                 "content": base(json!({ "count": 0 })) }]),
    );
    let changed = commit(&mut storage, change(id, delta(json!([["s", ["count"], 1]]))));
    commit(&mut storage, change(id, base(json!({ "count": 2 }))));
    commit(&mut storage, json!([{ "type": "document.retire", "id": id }]));
    let path = directory.path().join(format!("doc-{id}.jsonl"));
    assert_eq!(lines(&path).len(), 3);
    let reopened = open(directory.path());
    let at = |seq| {
        reopened
            .document(DocumentId(id), DocumentPoint::At(seq))
            .unwrap()
            .map(|d| Value::Object(d.value))
    };
    assert_eq!(at(created), Some(json!({ "count": 0 })));
    assert_eq!(at(changed), Some(json!({ "count": 1 })));
    assert_eq!(current(&reopened, id), None);
    assert_eq!(lines(&path).len(), 3);
}

#[test]
fn jsonl_reclaims_retired_task_session_and_latest_conversation_document_sidecars() {
    let directory = temp();
    let dir = directory.path();
    let mut storage = open(dir);
    create_root(&mut storage);
    let task_id = storage.mint_id().unwrap();
    let session = storage.mint_id().unwrap();
    let latest = storage.mint_id().unwrap();
    let task_document = storage.mint_id().unwrap();
    let created = commit(
        &mut storage,
        json!([
            { "type": "task", "value": pending_task(task_id, "ready") },
            session_document(session, "session", json!({})),
            { "type": "document.create",
              "record": { "id": latest, "kind": "latest", "scope": { "kind": "conversation", "conversationId": 1 },
                          "history": "latest", "fork": "current" },
              "content": base(json!({})) },
            { "type": "document.create", "record": { "id": task_document, "kind": "task", "scope": { "kind": "task", "taskId": task_id } },
              "content": base(json!({})) },
        ]),
    );
    let retired = commit(
        &mut storage,
        json!([
            { "type": "document.retire", "id": session },
            { "type": "document.retire", "id": latest },
            { "type": "document.retire", "id": task_document },
            { "type": "task", "value": terminal_task(task_id) },
        ]),
    );
    for file in [
        format!("doc-{session}.jsonl"),
        format!("doc-{latest}.jsonl"),
        format!("doc-{task_document}.jsonl"),
        format!("task-{task_id}.jsonl"),
    ] {
        assert!(!dir.join(&file).exists(), "{file}");
    }
    let reopened = open(dir);
    assert_eq!(task(&reopened, task_id), Some(terminal_task(task_id)));
    for id in [session, latest, task_document] {
        assert_eq!(current(&reopened, id), None);
    }
    let address = serde_json::from_value(json!({ "kind": "session", "scope": { "kind": "session" } })).unwrap();
    let found = reopened
        .find_document(&address, DocumentPoint::At(created))
        .unwrap()
        .unwrap();
    assert_eq!(
        (found.identity.id, found.created_at, found.retired_at),
        (DocumentId(session), created, Some(retired))
    );
    assert_eq!(
        reopened.find_document(&address, DocumentPoint::At(retired)).unwrap(),
        None
    );
}

#[test]
fn jsonl_truncates_torn_utf8_tails_at_exact_byte_offsets_and_reuses_the_unconfirmed_sequence() {
    let directory = temp();
    let dir = directory.path();
    let mut storage = open(dir);
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    commit(
        &mut storage,
        json!([session_document(id, "test.document", json!({ "text": "kept" }))]),
    );
    let sidecar = dir.join(format!("doc-{id}.jsonl"));
    let main = dir.join("main.jsonl");
    let (sidecar_size, main_size) = (
        std::fs::metadata(&sidecar).unwrap().len(),
        std::fs::metadata(&main).unwrap().len(),
    );
    let torn = "{\"text\":\"€".as_bytes();
    StdFs.append(&sidecar, &torn[..torn.len() - 1]).unwrap();
    StdFs.append(&main, &torn[..torn.len() - 1]).unwrap();
    drop(storage);

    let mut reopened = open(dir);
    assert_eq!(std::fs::metadata(&sidecar).unwrap().len(), sidecar_size);
    assert_eq!(std::fs::metadata(&main).unwrap().len(), main_size);
    assert_eq!(commit(&mut reopened, change(id, delta(json!([])))), Seq(3));
}

#[test]
fn jsonl_removes_complete_unconfirmed_sidecar_tails_without_resurrecting_terminal_tasks() {
    let directory = temp();
    let mut storage = open(directory.path());
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    commit(
        &mut storage,
        json!([{ "type": "task", "value": pending_task(id, "ready") }]),
    );
    commit(&mut storage, json!([{ "type": "task", "value": terminal_task(id) }]));
    let sidecar = directory.path().join(format!("task-{id}.jsonl"));
    assert!(!sidecar.exists());
    let stale = json!({ "format": 1, "type": "record", "seq": 4, "ordinal": 0,
                        "payload": { "type": "task", "value": pending_task(id, "stale") } });
    StdFs.append(&sidecar, format!("{stale}\n").as_bytes()).unwrap();

    let mut reopened = open(directory.path());
    assert!(!sidecar.exists());
    assert_eq!(task(&reopened, id), Some(terminal_task(id)));
    assert_eq!(reopened.commit(Vec::new()).unwrap(), Seq(4));
}

fn open_error(directory: &Path) -> String {
    match JsonlStorage::open(directory, JsonlOptions::default()) {
        Ok(_) => panic!("open should fail"),
        Err(error) => format!("{}: {}", error.name(), error.message()),
    }
}

#[test]
fn jsonl_fails_open_when_confirmed_sidecar_data_is_missing() {
    let directory = temp();
    let mut storage = open(directory.path());
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    commit(&mut storage, json!([session_document(id, "test.document", json!({}))]));
    std::fs::write(directory.path().join(format!("doc-{id}.jsonl")), "").unwrap();
    let error = open_error(directory.path());
    assert!(
        error.starts_with("JsonlCorruptionError: Missing confirmed sidecar record"),
        "{error}"
    );
}

#[test]
fn jsonl_rejects_a_confirmed_record_after_an_unconfirmed_sidecar_record() {
    let directory = temp();
    let mut storage = open(directory.path());
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    commit(
        &mut storage,
        json!([session_document(id, "test.document", json!({ "count": 0 }))]),
    );
    commit(&mut storage, change(id, delta(json!([["s", ["count"], 1]]))));
    let path = directory.path().join(format!("doc-{id}.jsonl"));
    let existing = lines(&path);
    let unconfirmed = json!({ "format": 1, "type": "record", "seq": 2, "ordinal": 999,
                              "payload": { "type": "document", "id": id, "content": delta(json!([])) } });
    std::fs::write(&path, format!("{}\n{unconfirmed}\n{}\n", existing[0], existing[1])).unwrap();
    assert!(open_error(directory.path()).contains("Confirmed record follows an unconfirmed tail"));
}

#[test]
fn jsonl_rejects_non_increasing_main_commit_sequences() {
    let directory = temp();
    let mut storage = open(directory.path());
    create_root(&mut storage);
    let path = directory.path().join("main.jsonl");
    let content = std::fs::read(&path).unwrap();
    StdFs.append(&path, &content).unwrap();
    assert!(open_error(directory.path()).contains("Commit sequence does not strictly increase"));
}

#[test]
fn jsonl_rejects_structurally_invalid_confirmed_document_content() {
    let directory = temp();
    let mut storage = open(directory.path());
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    commit(&mut storage, json!([session_document(id, "test.document", json!({}))]));
    let path = directory.path().join(format!("doc-{id}.jsonl"));
    let mut record: Value = serde_json::from_str(lines(&path)[0].as_str()).unwrap();
    record["payload"]["content"].as_object_mut().unwrap().remove("value");
    std::fs::write(&path, format!("{record}\n")).unwrap();
    assert!(open_error(directory.path()).contains("Invalid document content"));
}

#[test]
fn jsonl_rejects_malformed_complete_main_and_sidecar_lines() {
    let main = temp();
    create_root(&mut open(main.path()));
    StdFs.append(&main.path().join("main.jsonl"), b"{bad}\n").unwrap();
    assert!(open_error(main.path()).contains("Malformed complete main.jsonl"));

    let sidecar = temp();
    create_root(&mut open(sidecar.path()));
    std::fs::write(sidecar.path().join("doc-99.jsonl"), "{bad}\n").unwrap();
    assert!(open_error(sidecar.path()).contains("Malformed complete doc-99.jsonl"));
}

/// Not covered upstream: recovery refuses a sidecar whose records do not strictly increase by (seq, ordinal).
#[test]
fn jsonl_rejects_out_of_order_sidecar_records() {
    let directory = temp();
    let mut storage = open(directory.path());
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    commit(
        &mut storage,
        json!([session_document(id, "test.document", json!({ "count": 0 }))]),
    );
    commit(&mut storage, change(id, delta(json!([["s", ["count"], 1]]))));
    let path = directory.path().join(format!("doc-{id}.jsonl"));
    let existing = lines(&path);
    std::fs::write(&path, format!("{}\n{}\n", existing[1], existing[0])).unwrap();
    let error = open_error(directory.path());
    assert_eq!(
        error,
        format!("JsonlCorruptionError: Sidecar records are out of order in doc-{id}.jsonl")
    );
}
