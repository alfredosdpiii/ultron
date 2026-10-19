//! A model of how the product's durable storage reopens (session-core `JournalStorage`): every commit is validated
//! by a `MemoryStorage` working copy and logged as JSON `{seq, writes}` (JournalStorage appends that record to the
//! session journal); a reopen replays the log into a fresh `MemoryStorage`, as `JournalStorage::open` replays the
//! journal. So a reopen keeps exactly the acknowledged commits, through a JSON round trip, and nothing in memory
//! (IDs minted but never stored are not reserved again). The real JournalStorage over a session file is checked
//! against the upstream conformance cases and the kill tests in `crates/session-core/tests`.

#![allow(dead_code)]

use durable::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentId, DocumentPoint,
    DocumentQuery, DocumentRecord, EntryId, EntryQuery, EntryRecord, MemoryStorage, Page, Seq, Storage, StorageError,
    StorageWrite, StoredDocument, StoredEntry, SubmissionId, SubmissionQuery, SubmissionRecord, TaskId, TaskQuery,
    TaskRecord,
};
use std::sync::{Arc, Mutex, PoisonError};

type Result<T> = std::result::Result<T, StorageError>;

/// The durable log every handle opened from it appends to.
#[derive(Clone, Default)]
pub struct ReplayLog(Arc<Mutex<Vec<String>>>);

impl ReplayLog {
    /// A handle over everything committed so far, as a fresh process would open it.
    pub fn open(&self) -> Replayed {
        let mut memory = MemoryStorage::new();
        for record in self.0.lock().unwrap_or_else(PoisonError::into_inner).iter() {
            let record: serde_json::Value = serde_json::from_str(record).expect("a logged commit parses");
            let writes: Vec<StorageWrite> = serde_json::from_value(record["writes"].clone()).expect("logged writes");
            let seq = Seq(record["seq"].as_u64().expect("a logged seq"));
            let prepared = memory
                .prepare_commit(writes, Some(seq))
                .expect("a logged commit replays");
            memory.apply_commit(prepared);
        }
        Replayed {
            memory,
            log: self.clone(),
        }
    }
}

/// One open handle: a MemoryStorage working copy whose acknowledged commits are in the log.
pub struct Replayed {
    memory: MemoryStorage,
    log: ReplayLog,
}

impl Storage for Replayed {
    fn commit(&mut self, writes: Vec<StorageWrite>) -> Result<Seq> {
        let prepared = self.memory.prepare_commit(writes, None)?;
        let record = serde_json::json!({ "seq": prepared.seq.0, "writes": prepared.writes });
        self.log
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record.to_string());
        Ok(self.memory.apply_commit(prepared))
    }
    fn mint_id(&mut self) -> Result<u64> {
        self.memory.mint_id()
    }
    fn conversation(&self, id: ConversationId) -> Result<Option<ConversationRecord>> {
        self.memory.conversation(id)
    }
    fn scan_conversations(
        &self,
        q: &ConversationQuery,
        l: usize,
        c: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>> {
        self.memory.scan_conversations(q, l, c)
    }
    fn entry(&self, id: EntryId) -> Result<Option<StoredEntry>> {
        self.memory.entry(id)
    }
    fn visible_entry(&self, conversation_id: ConversationId, id: EntryId) -> Result<Option<StoredEntry>> {
        self.memory.visible_entry(conversation_id, id)
    }
    fn find_latest_head_marker(&self, id: ConversationId, at: Option<EntryId>) -> Result<Option<EntryRecord>> {
        self.memory.find_latest_head_marker(id, at)
    }
    fn scan_entries(&self, q: &EntryQuery, l: usize, c: Option<&Cursor>) -> Result<Page<EntryRecord>> {
        self.memory.scan_entries(q, l, c)
    }
    fn task(&self, id: TaskId) -> Result<Option<TaskRecord>> {
        self.memory.task(id)
    }
    fn scan_tasks(&self, q: &TaskQuery, l: usize, c: Option<&Cursor>) -> Result<Page<TaskRecord>> {
        self.memory.scan_tasks(q, l, c)
    }
    fn submission(&self, id: SubmissionId) -> Result<Option<SubmissionRecord>> {
        self.memory.submission(id)
    }
    fn scan_submissions(&self, q: &SubmissionQuery, l: usize, c: Option<&Cursor>) -> Result<Page<SubmissionRecord>> {
        self.memory.scan_submissions(q, l, c)
    }
    fn submission_by_request(&self, id: ConversationId, request_id: &str) -> Result<Option<SubmissionRecord>> {
        self.memory.submission_by_request(id, request_id)
    }
    fn find_document(&self, address: &DocumentAddress, at: DocumentPoint) -> Result<Option<DocumentRecord>> {
        self.memory.find_document(address, at)
    }
    fn document(&self, id: DocumentId, at: DocumentPoint) -> Result<Option<StoredDocument>> {
        self.memory.document(id, at)
    }
    fn scan_documents(&self, q: &DocumentQuery, l: usize, c: Option<&Cursor>) -> Result<Page<DocumentRecord>> {
        self.memory.scan_documents(q, l, c)
    }
    fn close(&mut self) -> Result<()> {
        self.memory.close()
    }
}
