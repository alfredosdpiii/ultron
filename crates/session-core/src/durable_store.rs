//! The session's Pi Durable storage, kept in the session's own format-4 journal: the durable conversation runtime
//! (`harness::chat`: generations, REPL cells, the inbox, children) commits through [`JournalStorage`], which
//! validates each commit against an in-memory [`MemoryStorage`] and appends it to the session file as one
//! `ultron.module` list element (`durable`: `{seq, writes}`) before applying it. Reopening the session replays those
//! elements, so every durable record survives a crash with the transcript it belongs to, in one file.
//!
//! `ultron.module` is the namespace of Ultron's own module state: the TypeScript product keeps it opaque, and the
//! cross-implementation journal comparison leaves it out, so the TypeScript-visible records are unchanged.
//!
//! Each commit is also shown to an [`Observer`] after it is durable: the session actor projects the runtime's
//! progress (answers, cell results) onto the TypeScript transcript from there.

use durable::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentId, DocumentPoint,
    DocumentQuery, DocumentRecord, EntryId, EntryQuery, EntryRecord, MemoryStorage, Page, Seq, Storage, StorageError,
    StorageWrite, StoredDocument, StoredEntry, SubmissionId, SubmissionQuery, SubmissionRecord, TaskId, TaskQuery,
    TaskRecord,
};
use serde_json::json;
use session_journal::{Session, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// The namespace and list key the durable records live under.
pub const NAMESPACE: &str = "ultron.module";
pub const KEY: &str = "durable";

/// The session journal, shared by the session actor and the durable runtime's commits.
pub type SharedSession = Arc<Mutex<Session>>;

pub fn lock(session: &SharedSession) -> MutexGuard<'_, Session> {
    session.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Sees every durable commit after it is in the journal (on the committing thread; it must not block).
pub type Observer = Arc<dyn Fn(&[StorageWrite]) + Send + Sync>;

/// Pi Durable storage inside a format-4 session file.
pub struct JournalStorage {
    session: SharedSession,
    memory: MemoryStorage,
    observer: Option<Observer>,
}

fn failed(message: impl Into<String>) -> StorageError {
    StorageError::Failed(message.into())
}

/// The durable commits a session file holds, in order, as `(seq, writes)`.
pub fn stored_commits(session: &Session) -> Result<Vec<(u64, Vec<StorageWrite>)>, StorageError> {
    session
        .state()
        .read_list(NAMESPACE, KEY)
        .into_iter()
        .map(|element| {
            let seq = element.value["seq"]
                .as_u64()
                .ok_or_else(|| StorageError::Corrupt("A durable record has no seq".into()))?;
            let writes: Vec<StorageWrite> = serde_json::from_value(element.value["writes"].clone())
                .map_err(|e| StorageError::Corrupt(format!("A durable record does not parse: {e}")))?;
            Ok((seq, writes))
        })
        .collect()
}

impl JournalStorage {
    /// Replay the durable records of `session`.
    pub fn open(session: SharedSession, observer: Option<Observer>) -> Result<JournalStorage, StorageError> {
        let mut memory = MemoryStorage::new();
        for (seq, writes) in stored_commits(&lock(&session))? {
            let prepared = memory.prepare_commit(writes, Some(Seq(seq)))?;
            memory.apply_commit(prepared);
        }
        Ok(JournalStorage {
            session,
            memory,
            observer,
        })
    }
}

impl Storage for JournalStorage {
    fn commit(&mut self, writes: Vec<StorageWrite>) -> durable::Result<Seq> {
        let prepared = self.memory.prepare_commit(writes, None)?;
        let record = json!({
            "seq": prepared.seq.0,
            "writes": serde_json::to_value(&prepared.writes).map_err(|e| failed(e.to_string()))?,
        });
        lock(&self.session)
            .commit(&[Write::append(NAMESPACE, KEY, record)])
            .map_err(failed)?;
        let observed = self.observer.as_ref().map(|_| prepared.writes.clone());
        let seq = self.memory.apply_commit(prepared);
        if let (Some(observer), Some(writes)) = (&self.observer, observed) {
            observer(&writes);
        }
        Ok(seq)
    }

    fn mint_id(&mut self) -> durable::Result<u64> {
        self.memory.mint_id()
    }

    fn conversation(&self, id: ConversationId) -> durable::Result<Option<ConversationRecord>> {
        self.memory.conversation(id)
    }

    fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> durable::Result<Page<ConversationRecord>> {
        self.memory.scan_conversations(query, limit, cursor)
    }

    fn entry(&self, id: EntryId) -> durable::Result<Option<StoredEntry>> {
        self.memory.entry(id)
    }

    fn visible_entry(&self, conversation_id: ConversationId, id: EntryId) -> durable::Result<Option<StoredEntry>> {
        self.memory.visible_entry(conversation_id, id)
    }

    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
    ) -> durable::Result<Option<EntryRecord>> {
        self.memory.find_latest_head_marker(conversation_id, at_or_before)
    }

    fn scan_entries(
        &self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> durable::Result<Page<EntryRecord>> {
        self.memory.scan_entries(query, limit, cursor)
    }

    fn task(&self, id: TaskId) -> durable::Result<Option<TaskRecord>> {
        self.memory.task(id)
    }

    fn scan_tasks(
        &self,
        query: &TaskQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> durable::Result<Page<TaskRecord>> {
        self.memory.scan_tasks(query, limit, cursor)
    }

    fn submission(&self, id: SubmissionId) -> durable::Result<Option<SubmissionRecord>> {
        self.memory.submission(id)
    }

    fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> durable::Result<Page<SubmissionRecord>> {
        self.memory.scan_submissions(query, limit, cursor)
    }

    fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> durable::Result<Option<SubmissionRecord>> {
        self.memory.submission_by_request(conversation_id, request_id)
    }

    fn find_document(&self, address: &DocumentAddress, at: DocumentPoint) -> durable::Result<Option<DocumentRecord>> {
        self.memory.find_document(address, at)
    }

    fn document(&self, id: DocumentId, at: DocumentPoint) -> durable::Result<Option<StoredDocument>> {
        self.memory.document(id, at)
    }

    fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> durable::Result<Page<DocumentRecord>> {
        self.memory.scan_documents(query, limit, cursor)
    }

    fn close(&mut self) -> durable::Result<()> {
        self.memory.close()
    }
}

/// Whether a session file holds durable runtime records.
pub fn has_records(session: &Session) -> bool {
    !session.state().read_list(NAMESPACE, KEY).is_empty()
}
