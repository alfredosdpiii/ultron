//! Port of upstream `test/session-support.ts`: a controlled storage that records admitted commits, counts mints and
//! document reads, holds calls at a gate, injects one commit failure, and reopens after close. Each ported Session
//! test runs against a MemoryStorage kept across reopen and against a storage that reopens by replaying its
//! acknowledged commits, the way the product's journal-backed storage reopens (`replay_support`).

#![allow(dead_code)]

use durable::{
    CommitChange, CommitPublication, ConversationId, ConversationOwnership, ConversationQuery, ConversationRecord,
    Cursor, DocumentAddress, DocumentId, DocumentPoint, DocumentQuery, DocumentRecord, EntryId, EntryQuery,
    EntryRecord, MemoryStorage, Page, Seq, Session, Storage, StorageError, StorageWrite, StoredDocument, StoredEntry,
    SubmissionId, SubmissionQuery, SubmissionRecord, TaskId, TaskQuery, TaskRecord,
};
use serde_json::Value;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

#[path = "../replay_support/mod.rs"]
mod replay;

use replay::ReplayLog;

type Result<T> = std::result::Result<T, StorageError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Memory,
    /// Reopens by replaying the acknowledged commits into a fresh working copy.
    Replay,
}

pub const BACKENDS: [Backend; 2] = [Backend::Memory, Backend::Replay];

/// A gate that holds calls until released and reports when the first held call arrives.
#[derive(Clone, Default)]
pub struct Gate(Arc<(Mutex<(bool, bool)>, Condvar)>);

impl Gate {
    pub fn wait_entered(&self) {
        let (state, changed) = &*self.0;
        let state = state.lock().unwrap();
        let (state, timeout) = changed
            .wait_timeout_while(state, Duration::from_secs(120), |state| !state.0)
            .unwrap();
        assert!(!timeout.timed_out() && state.0, "gate never entered");
    }

    pub fn release(&self) {
        let (state, changed) = &*self.0;
        state.lock().unwrap().1 = true;
        changed.notify_all();
    }

    fn pass(&self) {
        let (state, changed) = &*self.0;
        let mut state = state.lock().unwrap();
        state.0 = true;
        changed.notify_all();
        while !state.1 {
            state = changed.wait(state).unwrap();
        }
    }
}

#[derive(Default)]
struct Control {
    commits: Vec<Vec<StorageWrite>>,
    mint_count: usize,
    document_reads: usize,
    commit_gate: Option<Gate>,
    find_gate: Option<Gate>,
    commit_failure: Option<StorageError>,
    closed: bool,
}

struct Shared {
    backend: Backend,
    log: ReplayLog,
    inner: Mutex<Box<dyn Storage + Send>>,
    control: Mutex<Control>,
}

/// A Storage handle over the shared controlled backend; the Session owns one and tests read through another.
#[derive(Clone)]
pub struct Controlled(Arc<Shared>);

fn open(backend: Backend, log: &ReplayLog) -> Box<dyn Storage + Send> {
    match backend {
        Backend::Memory => Box::new(MemoryStorage::new()),
        Backend::Replay => Box::new(log.open()),
    }
}

impl Controlled {
    pub fn new(backend: Backend) -> Controlled {
        let log = ReplayLog::default();
        let inner = open(backend, &log);
        Controlled(Arc::new(Shared {
            backend,
            log,
            inner: Mutex::new(inner),
            control: Mutex::new(Control::default()),
        }))
    }

    fn control(&self) -> MutexGuard<'_, Control> {
        self.0.control.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn inner(&self) -> MutexGuard<'_, Box<dyn Storage + Send>> {
        self.0.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn open_check(&self) -> Result<()> {
        if self.control().closed {
            Err(StorageError::Failed("Storage is closed".into()))
        } else {
            Ok(())
        }
    }

    /// Batches admitted by the Session, in order.
    pub fn commits(&self) -> Vec<Vec<StorageWrite>> {
        self.control().commits.clone()
    }

    pub fn commit_count(&self) -> usize {
        self.control().commits.len()
    }

    pub fn last_commit(&self) -> Vec<StorageWrite> {
        self.control().commits.last().cloned().expect("a commit")
    }

    pub fn mint_count(&self) -> usize {
        self.control().mint_count
    }

    pub fn document_reads(&self) -> usize {
        self.control().document_reads
    }

    pub fn hold_commits(&self) -> Gate {
        let gate = Gate::default();
        self.control().commit_gate = Some(gate.clone());
        gate
    }

    pub fn hold_find_document(&self) -> Gate {
        let gate = Gate::default();
        self.control().find_gate = Some(gate.clone());
        gate
    }

    pub fn fail_next_commit(&self, error: StorageError) {
        self.control().commit_failure = Some(error);
    }

    /// Reopen after close, as a fresh process would reopen the same database, keeping every committed record.
    pub fn reopen(&self) {
        if self.0.backend != Backend::Memory {
            *self.inner() = open(self.0.backend, &self.0.log);
        }
        self.control().closed = false;
    }

    fn read<T>(&self, call: impl FnOnce(&dyn Storage) -> Result<T>) -> Result<T> {
        self.open_check()?;
        call(self.inner().as_ref())
    }
}

impl Storage for Controlled {
    fn commit(&mut self, writes: Vec<StorageWrite>) -> Result<Seq> {
        self.open_check()?;
        let gate = {
            let mut control = self.control();
            control.commits.push(writes.clone());
            control.commit_gate.clone()
        };
        if let Some(gate) = gate {
            gate.pass();
            self.control().commit_gate = None;
        }
        if let Some(failure) = self.control().commit_failure.take() {
            return Err(failure);
        }
        self.inner().commit(writes)
    }

    fn mint_id(&mut self) -> Result<u64> {
        self.open_check()?;
        self.control().mint_count += 1;
        self.inner().mint_id()
    }

    fn conversation(&self, id: ConversationId) -> Result<Option<ConversationRecord>> {
        self.read(|storage| storage.conversation(id))
    }

    fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>> {
        self.read(|storage| storage.scan_conversations(query, limit, cursor))
    }

    fn entry(&self, id: EntryId) -> Result<Option<StoredEntry>> {
        self.read(|storage| storage.entry(id))
    }

    fn visible_entry(&self, conversation_id: ConversationId, id: EntryId) -> Result<Option<StoredEntry>> {
        self.read(|storage| storage.visible_entry(conversation_id, id))
    }

    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
    ) -> Result<Option<EntryRecord>> {
        self.read(|storage| storage.find_latest_head_marker(conversation_id, at_or_before))
    }

    fn scan_entries(&self, query: &EntryQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<EntryRecord>> {
        self.read(|storage| storage.scan_entries(query, limit, cursor))
    }

    fn task(&self, id: TaskId) -> Result<Option<TaskRecord>> {
        self.read(|storage| storage.task(id))
    }

    fn scan_tasks(&self, query: &TaskQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<TaskRecord>> {
        self.read(|storage| storage.scan_tasks(query, limit, cursor))
    }

    fn submission(&self, id: SubmissionId) -> Result<Option<SubmissionRecord>> {
        self.read(|storage| storage.submission(id))
    }

    fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<SubmissionRecord>> {
        self.read(|storage| storage.scan_submissions(query, limit, cursor))
    }

    fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> Result<Option<SubmissionRecord>> {
        self.read(|storage| storage.submission_by_request(conversation_id, request_id))
    }

    fn find_document(&self, address: &DocumentAddress, at: DocumentPoint) -> Result<Option<DocumentRecord>> {
        let gate = self.control().find_gate.take();
        if let Some(gate) = gate {
            gate.pass();
        }
        self.read(|storage| storage.find_document(address, at))
    }

    fn document(&self, id: DocumentId, at: DocumentPoint) -> Result<Option<StoredDocument>> {
        self.control().document_reads += 1;
        self.read(|storage| storage.document(id, at))
    }

    fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<DocumentRecord>> {
        self.read(|storage| storage.scan_documents(query, limit, cursor))
    }

    fn close(&mut self) -> Result<()> {
        self.open_check()?;
        self.control().closed = true;
        if self.0.backend == Backend::Memory {
            Ok(())
        } else {
            self.inner().close()
        }
    }
}

/// Session kernel plus its controlled storage and every committed publication.
pub struct TestSession {
    pub backend: Backend,
    pub session: Session,
    pub storage: Controlled,
    pub publications: Arc<Mutex<Vec<CommitPublication>>>,
}

impl TestSession {
    pub fn open(backend: Backend) -> TestSession {
        let storage = Controlled::new(backend);
        let session = Session::new(storage.clone());
        let publications = Arc::new(Mutex::new(Vec::new()));
        let sink = publications.clone();
        session
            .subscribe_commits(move |publication| sink.lock().unwrap().push(publication.clone()))
            .unwrap();
        TestSession {
            backend,
            session,
            storage,
            publications,
        }
    }

    pub fn publication_count(&self) -> usize {
        self.publications.lock().unwrap().len()
    }

    pub fn last_publication(&self) -> CommitPublication {
        self.publications
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("a publication")
    }

    /// Create one ownerless conversation and return its ID.
    pub fn create_conversation(&self) -> ConversationId {
        self.session
            .commit(|tx| Ok(tx.create_conversation(ConversationOwnership::Ownerless)?.id))
            .unwrap()
    }
}

/// Document changes of a publication: `(record, conversation, version, value, ops)`.
pub struct Published {
    pub record: DocumentRecord,
    pub conversation_id: Option<ConversationId>,
    pub version: Option<u64>,
    pub value: Option<Arc<Value>>,
    pub ops: Vec<Value>,
}

pub fn document_changes(publication: &CommitPublication) -> Vec<Published> {
    publication
        .changes
        .iter()
        .filter_map(|change| match change {
            CommitChange::Document {
                record,
                conversation_id,
                version,
                value,
                ops,
            } => Some(Published {
                record: record.clone(),
                conversation_id: *conversation_id,
                version: *version,
                value: value.clone(),
                ops: ops.iter().map(chord_delta::Op::to_json).collect(),
            }),
            _ => None,
        })
        .collect()
}

pub fn document_copy_changes(publication: &CommitPublication) -> Vec<(DocumentRecord, ConversationId, Value)> {
    publication
        .changes
        .iter()
        .filter_map(|change| match change {
            CommitChange::DocumentCopy {
                record,
                conversation_id,
                source,
            } => Some((record.clone(), *conversation_id, serde_json::to_value(source).unwrap())),
            _ => None,
        })
        .collect()
}

/// A storage write as upstream JSON, for `toMatchObject`-style comparisons.
pub fn json(write: &StorageWrite) -> Value {
    serde_json::to_value(write).unwrap()
}

pub fn types(writes: &[StorageWrite]) -> Vec<String> {
    writes
        .iter()
        .map(|write| json(write)["type"].as_str().unwrap().to_owned())
        .collect()
}

/// Every key of `expected` is present in `actual` with a matching value (vitest `toMatchObject`).
pub fn matches(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|actual| matches(actual, value))),
        (Value::Array(actual), Value::Array(expected)) => {
            actual.len() == expected.len() && actual.iter().zip(expected).all(|(a, e)| matches(a, e))
        }
        _ => actual == expected,
    }
}

#[track_caller]
pub fn assert_matches(actual: &Value, expected: Value) {
    assert!(matches(actual, &expected), "{actual}\n  does not match\n{expected}");
}

/// Run one ported test body against every backend, naming the backend on failure.
pub fn each_backend(test: impl Fn(Backend)) {
    for backend in BACKENDS {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| test(backend)));
        if let Err(panic) = result {
            let message = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_default();
            panic!("{backend:?} backend: {message}");
        }
    }
}

/// Wait for a condition another thread makes true: polls until `condition` holds. The deadline only guards against hangs: a loaded machine must slow a
/// test down, never fail it.
pub fn eventually(mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while std::time::Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("condition never became true");
}

/// Give other threads a moment, only for asserting that something did NOT happen (a slow machine can make such a
/// check pass wrongly, never fail). Never use it to wait for something.
pub fn settle() {
    std::thread::sleep(Duration::from_millis(50));
}
