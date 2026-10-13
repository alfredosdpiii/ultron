//! The Session: one serialized mutation line over a [`Storage`], the loaded-document cache, and committed
//! publication.
//!
//! Port of `packages/durable/src/session/session.ts` and the Session errors of `errors.ts`.
//!
//! Differences from upstream:
//! - Synchronous. Commits, cold reads, watch acquisition and close run on a FIFO ticket line, so they settle in
//!   admission order exactly as upstream's promise chain does; a cached snapshot is served off the line. A Session is
//!   `Send + Sync` and clones share it.
//! - No `Context`: there are no context values to carry to listeners and no abort signals. Watches are cancelled
//!   with [`Watch::cancel`]; a commit admitted to the line always runs.
//! - Commit callbacks are synchronous, so a callback cannot settle with Tx operations still pending and the Tx
//!   cannot be used after it settles (the borrow ends); escaped drafts are revoked.
//! - Storage writes own copies of document values; published values are shared (`Arc`) with snapshots and watches.
//! - `documentState` (Chord replicated state) is not ported; watches cover committed observation.

mod scheduler_access;

use crate::documents::{
    Doc, DocDefinition, DocSemantics, Owner, check_record_scope, check_record_version, materialize_document,
    resolve_address,
};
use crate::observation::{Observed, Watch, WatchEnd, WatchShared};
use crate::transaction::{Tx, TxScope};
use crate::{
    ConversationId, DocumentAddress, DocumentCopySource, DocumentPoint, DocumentRecord, EntryId, Seq, Storage,
    StorageError, StorageWrite,
};
use chord_delta::{DeltaError, Op, Tracker};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

/// A Session operation failure. `name()` is the upstream error class name.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionError {
    /// The Session failed, so nothing runs or commits any more; holds the first error (upstream `SessionFailed`).
    Failed(Box<SessionError>),
    /// A transaction read a table after its first table write; holds the `Tx` method name.
    ReadAfterWrite(&'static str),
    /// A submission reached a busy conversation and was not admitted.
    Busy(ConversationId),
    /// The Session is closing or closed.
    Closed,
    Storage(StorageError),
    Delta(DeltaError),
    /// Upstream `TypeError`.
    Type(String),
    /// Upstream plain `Error`, also what a commit callback returns for its own failures.
    Error(String),
}

impl SessionError {
    pub fn name(&self) -> &'static str {
        match self {
            SessionError::Failed(_) => "SessionFailed",
            SessionError::ReadAfterWrite(_) => "ReadAfterWrite",
            SessionError::Busy(_) => "ConversationBusy",
            SessionError::Closed | SessionError::Error(_) => "Error",
            SessionError::Storage(error) => error.name(),
            SessionError::Delta(error) => error.name(),
            SessionError::Type(_) => "TypeError",
        }
    }

    /// The first error of a failed Session.
    pub fn cause(&self) -> Option<&SessionError> {
        match self {
            SessionError::Failed(cause) => Some(cause),
            _ => None,
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Failed(_) => f.write_str("Session failed after a storage error; close and reopen it"),
            SessionError::ReadAfterWrite(method) => {
                write!(f, "Tx.{method}() cannot read tables after the first table write")
            }
            SessionError::Busy(id) => write!(f, "Conversation {} is busy", id.0),
            SessionError::Closed => f.write_str("Session is closed"),
            SessionError::Storage(error) => error.fmt(f),
            SessionError::Delta(error) => error.fmt(f),
            SessionError::Type(message) | SessionError::Error(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<StorageError> for SessionError {
    fn from(error: StorageError) -> Self {
        SessionError::Storage(error)
    }
}

impl From<DeltaError> for SessionError {
    fn from(error: DeltaError) -> Self {
        SessionError::Delta(error)
    }
}

pub type Result<T> = std::result::Result<T, SessionError>;

/// Why a Session ended: `close()`, or a failed Storage call, whose error it carries.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEnd {
    Closed,
    Failed(SessionError),
}

/// One change of a successful commit.
#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "short-lived publication value; boxing would only complicate matching"
)]
pub enum CommitChange {
    /// A complete conversation, entry, task, or submission record, as written.
    Table(StorageWrite),
    /// A created, changed, or retired incarnation; `value` is `None` and `ops` empty when this commit retired it.
    Document {
        record: DocumentRecord,
        /// Owning conversation; task documents derive it from their task. `None` only for Session documents.
        conversation_id: Option<ConversationId>,
        version: Option<u64>,
        value: Observed,
        /// Exact operations of an ordinary update; empty for creation and retirement.
        ops: Vec<Op>,
    },
    /// A definition-free fork copy; consumers hydrate through reads or watches.
    DocumentCopy {
        record: DocumentRecord,
        conversation_id: ConversationId,
        source: DocumentCopySource,
    },
}

/// Every change of one successful commit.
#[derive(Clone, Debug, PartialEq)]
pub struct CommitPublication {
    pub seq: Seq,
    pub changes: Vec<CommitChange>,
}

/// One committed incarnation in the tracker cache.
pub(crate) struct LoadedDocument {
    pub record: DocumentRecord,
    /// Persisted definition version; older while the tracked value is migrated only in memory.
    pub stored_version: u64,
    /// Definition version whose shape the tracked value has; access with another version reloads.
    pub value_version: u64,
    /// Stored deltas after the newest base; advanced by adoption.
    pub deltas_since_base: usize,
    pub tracker: Tracker,
}

pub(crate) type Loaded = Arc<Mutex<LoadedDocument>>;

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// What the line guards: the backend and whether it has been closed.
pub(crate) struct LineState {
    storage: Box<dyn Storage + Send>,
    storage_closed: bool,
}

/// FIFO mutation line: jobs run one at a time in the order they were admitted.
struct Line {
    turn: Mutex<(u64, u64)>,
    advanced: Condvar,
    state: Mutex<LineState>,
}

pub(crate) struct LineGuard<'a> {
    line: &'a Line,
    state: Option<MutexGuard<'a, LineState>>,
}

impl Line {
    fn enter(&self) -> LineGuard<'_> {
        let mut turn = lock(&self.turn);
        let ticket = turn.0;
        turn.0 += 1;
        while turn.1 != ticket {
            turn = self
                .advanced
                .wait(turn)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        drop(turn);
        LineGuard {
            line: self,
            state: Some(lock(&self.state)),
        }
    }
}

impl Drop for LineGuard<'_> {
    fn drop(&mut self) {
        self.state.take();
        lock(&self.line.turn).1 += 1;
        self.line.advanced.notify_all();
    }
}

impl std::ops::Deref for LineGuard<'_> {
    type Target = LineState;
    fn deref(&self) -> &LineState {
        self.state.as_ref().expect("held")
    }
}

impl std::ops::DerefMut for LineGuard<'_> {
    fn deref_mut(&mut self) -> &mut LineState {
        self.state.as_mut().expect("held")
    }
}

type CommitListener = Arc<dyn Fn(&CommitPublication) + Send + Sync>;
type CloseListener = Arc<dyn Fn() + Send + Sync>;

struct Observer {
    watch: Arc<WatchShared>,
    /// Definition version of the watch's value; another version's change arrives as a root replacement.
    version: u64,
}

#[derive(Default)]
struct Listeners {
    next: u64,
    commits: Vec<(u64, CommitListener)>,
    closes: Vec<(u64, CloseListener)>,
    observers: Vec<Observer>,
}

#[derive(Default)]
struct Status {
    closing: bool,
    failure: Option<SessionError>,
    end: Option<SessionEnd>,
}

pub(crate) struct Inner {
    line: Line,
    pub(crate) documents: Mutex<HashMap<String, Loaded>>,
    listeners: Mutex<Listeners>,
    status: Mutex<Status>,
    ended: Condvar,
    pub(crate) now: Box<dyn Fn() -> u64 + Send + Sync>,
}

/// Owner of one mutation line, its records, and its tracked documents.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

/// Removes a listener when called; dropping it keeps the listener.
pub struct Subscription {
    inner: Arc<Inner>,
    id: u64,
}

impl Subscription {
    pub fn unsubscribe(self) {
        let mut listeners = lock(&self.inner.listeners);
        listeners.commits.retain(|(id, _)| *id != self.id);
        listeners.closes.retain(|(id, _)| *id != self.id);
    }
}

fn wall_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

impl Session {
    /// Open a Session over one storage backend. The first error a Storage call returns, other than a
    /// [`StorageError::Request`] from a read, fails the Session.
    pub fn new(storage: impl Storage + Send + 'static) -> Session {
        Session::with_clock(storage, wall_clock)
    }

    /// `now` is the wall clock (milliseconds) stamped on task lifecycle times.
    pub fn with_clock(
        storage: impl Storage + Send + 'static,
        now: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Session {
        Session {
            inner: Arc::new(Inner {
                line: Line {
                    turn: Mutex::new((0, 0)),
                    advanced: Condvar::new(),
                    state: Mutex::new(LineState {
                        storage: Box::new(storage),
                        storage_closed: false,
                    }),
                },
                documents: Mutex::new(HashMap::new()),
                listeners: Mutex::new(Listeners::default()),
                status: Mutex::new(Status::default()),
                ended: Condvar::new(),
                now: Box::new(now),
            }),
        }
    }

    /// Run one atomic transaction on the mutation line. Its changes publish only after Storage acknowledges them.
    pub fn commit<T>(&self, change: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        self.commit_with(TxScope::default(), change)
    }

    /// `commit` with defaults: the conversation of `Tx::create_task` and the task stamped on appended entries.
    pub fn commit_with<T>(&self, scope: TxScope, change: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        self.inner.assert_usable()?;
        let mut line = self.inner.line.enter();
        self.inner.assert_healthy()?;
        let mut tx = Tx::new(&self.inner, &mut line, scope);
        let result = match change(&mut tx) {
            Ok(result) => result,
            Err(error) => {
                tx.discard();
                return Err(error);
            }
        };
        // A callback that caught a failed read must not commit, nor succeed as if it had.
        if let Err(error) = self.inner.assert_healthy() {
            tx.discard();
            return Err(error);
        }
        let (writes, plans) = tx.settle()?;
        if writes.is_empty() {
            return Ok(result);
        }
        let tables: Vec<StorageWrite> = writes
            .iter()
            .filter(|write| {
                matches!(
                    write,
                    StorageWrite::Conversation { .. }
                        | StorageWrite::Entry { .. }
                        | StorageWrite::Task { .. }
                        | StorageWrite::Submission { .. }
                )
            })
            .cloned()
            .collect();
        let seq = self.inner.guard(&mut line, false, |storage| storage.commit(writes))?;
        let documents = match crate::transaction::adopt(&self.inner, plans, seq) {
            Ok(documents) => documents,
            Err(error) => {
                // Storage already committed; a failed adoption leaves memory behind durable state.
                self.inner.fail(&mut line, error.clone());
                return Err(error);
            }
        };
        let mut changes: Vec<CommitChange> = tables.into_iter().map(CommitChange::Table).collect();
        changes.extend(documents);
        self.inner.publish(&CommitPublication { seq, changes });
        Ok(result)
    }

    /// The committed value of a document; never creates one.
    pub fn snapshot(&self, doc: &Doc, owner: Owner) -> Result<Option<Arc<Value>>> {
        self.snapshot_at(doc, owner, None)
    }

    /// `snapshot` of one family member.
    pub fn snapshot_member(&self, doc: &Doc, owner: Owner, key: &str) -> Result<Option<Arc<Value>>> {
        self.snapshot_at(doc, owner, Some(key))
    }

    fn snapshot_at(&self, doc: &Doc, owner: Owner, key: Option<&str>) -> Result<Option<Arc<Value>>> {
        self.inner.assert_usable()?;
        let definition = doc.definition();
        let resolved = resolve_address(definition, owner, key)?;
        let cached = lock(&self.inner.documents).get(&resolved.id).cloned();
        let loaded = match cached.filter(|loaded| lock(loaded).value_version == definition.version) {
            Some(loaded) => Some(loaded),
            None => {
                let mut line = self.inner.line.enter();
                self.inner.assert_healthy()?;
                self.inner
                    .load(&mut line, definition, &resolved.id, &resolved.address)?
            }
        };
        let Some(loaded) = loaded else { return Ok(None) };
        let loaded = lock(&loaded);
        check_record_scope(definition, &loaded.record.identity)?;
        check_record_version(definition, &loaded.record.identity, loaded.stored_version)?;
        Ok(Some(loaded.tracker.value().clone()))
    }

    /// Watch a committed document; never creates one.
    pub fn watch_doc(&self, doc: &Doc, owner: Owner) -> Result<Option<Watch>> {
        self.watch_at(doc, owner, None)
    }

    pub fn watch_member(&self, doc: &Doc, owner: Owner, key: &str) -> Result<Option<Watch>> {
        self.watch_at(doc, owner, Some(key))
    }

    fn watch_at(&self, doc: &Doc, owner: Owner, key: Option<&str>) -> Result<Option<Watch>> {
        self.inner.assert_usable()?;
        let definition = doc.definition();
        let resolved = resolve_address(definition, owner, key)?;
        let mut line = self.inner.line.enter();
        self.inner.assert_healthy()?;
        let Some(loaded) = self
            .inner
            .load(&mut line, definition, &resolved.id, &resolved.address)?
        else {
            return Ok(None);
        };
        let loaded = lock(&loaded);
        check_record_scope(definition, &loaded.record.identity)?;
        check_record_version(definition, &loaded.record.identity, loaded.stored_version)?;
        let watch = WatchShared::new(loaded.record.identity.id, loaded.tracker.value().clone());
        lock(&self.inner.listeners).observers.push(Observer {
            watch: watch.clone(),
            version: loaded.value_version,
        });
        Ok(Some(Watch { shared: watch }))
    }

    /// The value of a rewindable conversation document as of the commit that persisted the visible entry `at`.
    pub fn snapshot_as_of(
        &self,
        doc: &Doc,
        conversation_id: ConversationId,
        key: Option<&str>,
        at: EntryId,
    ) -> Result<Option<Arc<Value>>> {
        self.inner.assert_usable()?;
        let definition = doc.definition();
        if !matches!(definition.semantics, DocSemantics::Conversation { .. }) {
            return Err(SessionError::Type(
                "Session.snapshotAsOf() requires a conversation document".into(),
            ));
        }
        let resolved = resolve_address(definition, Owner::Conversation(conversation_id), key)?;
        let mut line = self.inner.line.enter();
        self.inner.assert_healthy()?;
        let inner = &self.inner;
        let Some(stored_entry) = inner.guard(&mut line, true, |storage| storage.visible_entry(conversation_id, at))?
        else {
            return Err(SessionError::Error(format!(
                "Entry {} is not visible from conversation {}",
                at.0, conversation_id.0
            )));
        };
        let address = DocumentAddress {
            scope: crate::DocumentScope::Conversation {
                conversation_id: stored_entry.entry.conversation_id,
            },
            ..resolved.address
        };
        let point = DocumentPoint::At(stored_entry.commit_seq);
        let Some(record) = inner.guard(&mut line, true, |storage| storage.find_document(&address, point))? else {
            return Ok(None);
        };
        let id = record.identity.id;
        let Some(stored) = inner.guard(&mut line, true, |storage| storage.document(id, point))? else {
            return Err(SessionError::Error(format!(
                "Historical document {} ({}) cannot be read",
                id.0, record.identity.kind
            )));
        };
        Ok(Some(Arc::new(materialize_document(definition, &stored)?)))
    }

    /// Drop every loaded tracker on the line; later access cold-loads from Storage.
    pub fn unload_documents(&self) -> Result<()> {
        self.inner.assert_usable()?;
        let _line = self.inner.line.enter();
        lock(&self.inner.documents).clear();
        Ok(())
    }

    /// Observe complete commits synchronously after adoption, on the committing thread. The listener must not block
    /// or call Session APIs.
    pub fn subscribe_commits(
        &self,
        listener: impl Fn(&CommitPublication) + Send + Sync + 'static,
    ) -> Result<Subscription> {
        self.inner.assert_usable()?;
        let mut listeners = lock(&self.inner.listeners);
        listeners.next += 1;
        let id = listeners.next;
        listeners.commits.push((id, Arc::new(listener)));
        Ok(Subscription {
            inner: self.inner.clone(),
            id,
        })
    }

    /// Observe close synchronously when it begins, also when a failed Storage call closes the Session.
    pub fn subscribe_close(&self, listener: impl Fn() + Send + Sync + 'static) -> Result<Subscription> {
        self.inner.assert_usable()?;
        let mut listeners = lock(&self.inner.listeners);
        listeners.next += 1;
        let id = listeners.next;
        listeners.closes.push((id, Arc::new(listener)));
        Ok(Subscription {
            inner: self.inner.clone(),
            id,
        })
    }

    /// Seal admission, settle commits already admitted, then close Storage. Later calls wait for the first.
    pub fn close(&self) -> Result<()> {
        let first = {
            let mut status = lock(&self.inner.status);
            !std::mem::replace(&mut status.closing, true)
        };
        if !first {
            self.wait_closed();
            return Ok(());
        }
        self.inner.notify_close();
        let mut line = self.inner.line.enter();
        lock(&self.inner.listeners).commits.clear();
        lock(&self.inner.documents).clear();
        let result = if line.storage_closed {
            Ok(())
        } else {
            line.storage_closed = true;
            line.storage.close()
        };
        let mut status = lock(&self.inner.status);
        if let Err(error) = &result
            && status.failure.is_none()
        {
            status.failure = Some(SessionError::Storage(error.clone()));
        }
        status.end = Some(match &status.failure {
            None => SessionEnd::Closed,
            Some(failure) => SessionEnd::Failed(failure.clone()),
        });
        self.inner.ended.notify_all();
        result.map_err(SessionError::Storage)
    }

    /// Wait until the Session has closed, Storage included: after `close()`, or after the first failed Storage call.
    pub fn wait_closed(&self) -> SessionEnd {
        let status = lock(&self.inner.status);
        let status = self
            .inner
            .ended
            .wait_while(status, |status| status.end.is_none())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        status.end.clone().expect("ended")
    }

    /// The error that failed the Session, if one did.
    pub fn failure(&self) -> Option<SessionError> {
        lock(&self.inner.status).failure.clone()
    }

    /// Jobs admitted to the mutation line and not yet finished, the running one included. Lets a caller (a test)
    /// know that a commit or close has queued behind another, instead of guessing with a sleep.
    pub fn line_len(&self) -> u64 {
        let turn = lock(&self.inner.line.turn);
        turn.0 - turn.1
    }
}

impl Inner {
    pub(crate) fn assert_healthy(&self) -> Result<()> {
        match &lock(&self.status).failure {
            Some(failure) => Err(SessionError::Failed(Box::new(failure.clone()))),
            None => Ok(()),
        }
    }

    fn assert_usable(&self) -> Result<()> {
        let status = lock(&self.status);
        if let Some(failure) = &status.failure {
            return Err(SessionError::Failed(Box::new(failure.clone())));
        }
        if status.closing {
            return Err(SessionError::Closed);
        }
        Ok(())
    }

    /// Run one backend call under the failure guard. A failed Session refuses the call; an error fails the Session
    /// unless it is a [`StorageError::Request`] from a read (`read`), which fails only that call.
    pub(crate) fn guard<T>(
        &self,
        line: &mut LineState,
        read: bool,
        call: impl FnOnce(&mut dyn Storage) -> crate::Result<T>,
    ) -> Result<T> {
        self.assert_healthy()?;
        if line.storage_closed {
            return Err(SessionError::Closed);
        }
        match call(line.storage.as_mut()) {
            Ok(value) => Ok(value),
            Err(error) => {
                let exempt = read && matches!(error, StorageError::Request(_));
                if !exempt {
                    self.fail(line, SessionError::Storage(error.clone()));
                }
                Err(SessionError::Storage(error))
            }
        }
    }

    /// Fail the Session with `error`, the first wins: seal admission, run close listeners, end every watch, and close
    /// Storage. Nothing retries; reopening recovers from what was committed.
    pub(crate) fn fail(&self, line: &mut LineState, error: SessionError) {
        {
            let mut status = lock(&self.status);
            if status.failure.is_some() {
                return;
            }
            status.failure = Some(error.clone());
            status.closing = true;
        }
        self.notify_close();
        lock(&self.listeners).commits.clear();
        lock(&self.documents).clear();
        if !line.storage_closed {
            line.storage_closed = true;
            // A failing backend close is nobody's to see here; the first error stays the cause.
            let _ = line.storage.close();
        }
        lock(&self.status).end = Some(SessionEnd::Failed(error));
        self.ended.notify_all();
    }

    /// Run the close listeners once, then end every watch.
    fn notify_close(&self) {
        let (closes, observers) = {
            let mut listeners = lock(&self.listeners);
            (
                std::mem::take(&mut listeners.closes),
                std::mem::take(&mut listeners.observers),
            )
        };
        for (_, listener) in closes {
            listener();
        }
        let end = match &lock(&self.status).failure {
            None => WatchEnd::SessionClosed,
            Some(failure) => WatchEnd::SessionFailed(failure.clone()),
        };
        for observer in observers {
            observer.watch.terminate(end.clone());
        }
    }

    /// Deliver a durable commit: watches queue their frames first, then host listeners run.
    fn publish(&self, publication: &CommitPublication) {
        let commits = {
            let mut listeners = lock(&self.listeners);
            listeners.observers.retain(|observer| !observer.watch.is_ended());
            for observer in &mut listeners.observers {
                for change in &publication.changes {
                    let CommitChange::Document {
                        record,
                        version,
                        value,
                        ops,
                        ..
                    } = change
                    else {
                        continue;
                    };
                    if record.identity.id != observer.watch.record {
                        continue;
                    }
                    let ops = match (value, version) {
                        (None, _) => vec![Op::Replace(Value::Null)],
                        (Some(_), Some(version)) if *version == observer.version => ops.clone(),
                        (Some(value), version) => {
                            // An observer of another shape gets the new value as a root replacement.
                            observer.version = version.unwrap_or(observer.version);
                            vec![Op::Replace((**value).clone())]
                        }
                    };
                    // A migration-only base changes nothing for an observer of the new version.
                    if !ops.is_empty() {
                        observer.watch.advance(value.clone(), ops);
                    }
                }
            }
            listeners.commits.clone()
        };
        for (_, listener) in commits {
            listener(publication);
        }
    }

    /// The cached current incarnation, cold-loading and migrating it for this definition version when necessary.
    pub(crate) fn load(
        &self,
        line: &mut LineState,
        definition: &DocDefinition,
        address_id: &str,
        address: &DocumentAddress,
    ) -> Result<Option<Loaded>> {
        let cached = lock(&self.documents).get(address_id).cloned();
        if let Some(cached) = cached {
            // A tracker serves only definitions of the version its value was materialized for; others reload.
            if lock(&cached).value_version == definition.version {
                return Ok(Some(cached));
            }
            lock(&self.documents).remove(address_id);
        }
        let Some(record) = self.guard(line, true, |storage| {
            storage.find_document(address, DocumentPoint::Current)
        })?
        else {
            return Ok(None);
        };
        let id = record.identity.id;
        let Some(stored) = self.guard(line, true, |storage| storage.document(id, DocumentPoint::Current))? else {
            return Err(SessionError::Error(format!(
                "Current document {} ({}) cannot be read",
                id.0, record.identity.kind
            )));
        };
        let value = materialize_document(definition, &stored)?;
        let loaded = Arc::new(Mutex::new(LoadedDocument {
            record: stored.record,
            stored_version: stored.version,
            value_version: definition.version,
            deltas_since_base: stored.deltas_since_base,
            tracker: Tracker::new(value),
        }));
        lock(&self.documents).insert(address_id.to_owned(), loaded.clone());
        Ok(Some(loaded))
    }
}
