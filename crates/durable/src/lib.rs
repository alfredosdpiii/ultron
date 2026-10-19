//! Pi Durable storage: typed records, the [`Storage`] contract, and [`MemoryStorage`].
//!
//! Port of `packages/durable/src/{types,errors}.ts` and `storage/memory-storage.ts` from earendil-works/pi (MIT); the
//! pinned revision is recorded in `fixtures/storage-conformance.json`. Errors carry the upstream error name and
//! message. Upstream's SQLite and JSONL file backends are not ported: the product keeps durable records in the
//! session's format-4 journal (`session-core`'s `JournalStorage`, a MemoryStorage working copy whose commits are
//! journal records).
//!
//! Differences from upstream:
//! - Storage is synchronous and takes no cancellation context; the Session (phase 3) owns scheduling and cancellation.
//! - Records are typed, so malformed records fail to deserialize instead of reaching storage, and a
//!   `document.create` whose content is a delta is rejected at commit (upstream's types forbid it).

mod document_state;
mod documents;
mod memory;
mod observation;
pub mod owner;
mod records;
mod session;
mod transaction;

pub use document_state::{Delivery, DeliveryKind, DocumentState, StateSubscription};
pub use documents::{CheckpointInfo, Doc, DocDefinition, DocSemantics, Owner};
pub use memory::{MemoryStorage, PreparedCommit};
pub use observation::{Observed, Watch, WatchEnd};
pub use records::*;
pub use session::{CommitChange, CommitPublication, Session, SessionEnd, SessionError, Subscription};
pub use transaction::{
    ConversationOwnership, EntryDraft, EntryHead, SubmissionCreate, SubmissionSettlement, TaskDefinition, TaskOptions,
    TaskOwnership, Tx, TxScope,
};

use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;

/// A Storage failure. `Request` fails only that read (upstream `StorageRequestError`, no durable effect); every
/// other error, and any error from `commit` or `mint_id`, is fatal to the Session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageError {
    Request(String),
    Failed(String),
    /// Persisted durable state is invalid (upstream `JsonlCorruptionError`): a journal record that does not parse.
    Corrupt(String),
}

impl StorageError {
    /// The upstream error class name.
    pub fn name(&self) -> &'static str {
        match self {
            StorageError::Request(_) => "StorageRequestError",
            StorageError::Failed(_) => "Error",
            StorageError::Corrupt(_) => "JsonlCorruptionError",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            StorageError::Request(message) | StorageError::Failed(message) | StorageError::Corrupt(message) => message,
        }
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for StorageError {}

pub type Result<T> = std::result::Result<T, StorageError>;

pub(crate) fn failed<T>(message: impl Into<String>) -> Result<T> {
    Err(StorageError::Failed(message.into()))
}

pub(crate) fn request<T>(message: impl Into<String>) -> Result<T> {
    Err(StorageError::Request(message.into()))
}

/// Atomic persistence boundary for Session records.
///
/// Storage trusts the owning Session for semantically valid records, references, ancestry, and transitions. It
/// enforces atomicity, the global ID namespace, immutable conversation/entry creation, and document record
/// consistency; the Session serializes commits. A scan returns at most `limit` items and a cursor when more remain.
pub trait Storage {
    /// Atomically persist one batch and return its sequence; later reads observe it.
    fn commit(&mut self, writes: Vec<StorageWrite>) -> Result<Seq>;
    /// A fresh ID from the Session-global namespace, greater than every ID minted or stored before, across reopen.
    fn mint_id(&mut self) -> Result<u64>;
    fn conversation(&self, id: ConversationId) -> Result<Option<ConversationRecord>>;
    fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>>;
    /// One entry by global ID.
    fn entry(&self, id: EntryId) -> Result<Option<StoredEntry>>;
    /// One entry only when it is visible through the conversation's fork ancestry.
    fn visible_entry(&self, conversation_id: ConversationId, id: EntryId) -> Result<Option<StoredEntry>>;
    /// The newest visible entry with a `head` at or below the optional inclusive cutoff.
    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
    ) -> Result<Option<EntryRecord>>;
    fn scan_entries(&self, query: &EntryQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<EntryRecord>>;
    fn task(&self, id: TaskId) -> Result<Option<TaskRecord>>;
    fn scan_tasks(&self, query: &TaskQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<TaskRecord>>;
    fn submission(&self, id: SubmissionId) -> Result<Option<SubmissionRecord>>;
    fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<SubmissionRecord>>;
    fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> Result<Option<SubmissionRecord>>;
    /// The incarnation occupying one exact logical address at the point.
    fn find_document(&self, address: &DocumentAddress, at: DocumentPoint) -> Result<Option<DocumentRecord>>;
    /// Materialize one incarnation by ID at the point, without following a replacement at its address.
    fn document(&self, id: DocumentId, at: DocumentPoint) -> Result<Option<StoredDocument>>;
    fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<DocumentRecord>>;
    /// Release resources; every later operation fails.
    fn close(&mut self) -> Result<()>;
}

/// Where a scan starts: its order and the last ID a previous page returned.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ScanStart {
    pub order: ScanOrder,
    pub after: Option<i64>,
}

/// A cursor's `after`, which must be a JavaScript safe integer.
pub(crate) fn cursor_after(cursor: &Cursor) -> Result<Option<i64>> {
    match cursor.get("after") {
        None => Ok(None),
        Some(after) => match after.as_i64() {
            Some(after) if after.unsigned_abs() <= MAX_SAFE_ID => Ok(Some(after)),
            _ => request("Invalid storage cursor"),
        },
    }
}

/// Resolve a scan's order and position. A cursor continues in the order it was created with; a different requested
/// order fails. A cursor without an order continues in the scan's default order.
pub(crate) fn scan_start(
    requested: Option<ScanOrder>,
    cursor: Option<&Cursor>,
    fallback: ScanOrder,
) -> Result<ScanStart> {
    let Some(cursor) = cursor else {
        return Ok(ScanStart {
            order: requested.unwrap_or(fallback),
            after: None,
        });
    };
    let Some(after) = cursor_after(cursor)? else {
        return request("Invalid storage cursor");
    };
    let order = match cursor.get("order") {
        None => fallback,
        Some(value) => match serde_json::from_value::<ScanOrder>(value.clone()) {
            Ok(order) => order,
            Err(_) => return request("Invalid storage cursor"),
        },
    };
    if let Some(requested) = requested
        && requested != order
    {
        return request(format!(
            "The cursor continues a {} scan; the query asks for {}",
            order.as_str(),
            requested.as_str()
        ));
    }
    Ok(ScanStart {
        order,
        after: Some(after),
    })
}

/// The continuation of a scan whose last returned item has `id`.
pub(crate) fn next_cursor(id: u64, order: ScanOrder) -> Cursor {
    match json!({ "after": id, "order": order.as_str() }) {
        Value::Object(cursor) => cursor,
        _ => unreachable!("json! object literal"),
    }
}

/// The table that owns an ID in the Session-global namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Table {
    Conversation,
    Entry,
    Task,
    Submission,
    Document,
}

impl Table {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Table::Conversation => "conversation",
            Table::Entry => "entry",
            Table::Task => "task",
            Table::Submission => "submission",
            Table::Document => "document",
        }
    }
}

impl fmt::Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Conversations, entries and documents are created once; tasks and submissions are replaced whole, but never
/// change table. `existing` looks up the table already owning an ID.
pub(crate) fn check_global_ids(
    writes: &[StorageWrite],
    mut existing: impl FnMut(u64) -> Result<Option<Table>>,
) -> Result<()> {
    let mut claimed: HashMap<u64, Table> = HashMap::new();
    for write in writes {
        let (table, id) = match write {
            StorageWrite::Conversation { value } => (Table::Conversation, value.id.0),
            StorageWrite::Entry { value } => (Table::Entry, value.id.0),
            StorageWrite::Task { value } => (Table::Task, value.id.0),
            StorageWrite::Submission { value } => (Table::Submission, value.id.0),
            StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => {
                (Table::Document, record.id.0)
            }
            StorageWrite::DocumentChange { .. } | StorageWrite::DocumentRetire { .. } => continue,
        };
        let existing = existing(id)?;
        let earlier = claimed.get(&id).copied();
        if matches!(table, Table::Conversation | Table::Entry | Table::Document) {
            if let Some(existing) = existing {
                return failed(format!("ID {id} already belongs to {existing}"));
            }
            if earlier.is_some() {
                return failed(format!("ID {id} is written more than once"));
            }
        } else {
            if let Some(existing) = existing.filter(|existing| *existing != table) {
                return failed(format!("ID {id} already belongs to {existing}"));
            }
            if earlier.is_some_and(|earlier| earlier != table) {
                return failed(format!("ID {id} is written as two record types"));
            }
        }
        claimed.insert(id, table);
    }
    Ok(())
}

/// Every document command of one commit for one incarnation.
#[derive(Default)]
pub(crate) struct DocumentAction {
    pub create: Option<DocumentCreate>,
    pub copy: Option<DocumentCopySource>,
    pub content: Option<DocumentContent>,
    pub retire: bool,
}

/// Group document writes by incarnation, in first-write order.
pub(crate) fn document_actions(writes: &[StorageWrite]) -> Result<Vec<(u64, DocumentAction)>> {
    let mut actions: Vec<(u64, DocumentAction)> = Vec::new();
    for write in writes {
        let id = match write {
            StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => record.id.0,
            StorageWrite::DocumentChange { id, .. } | StorageWrite::DocumentRetire { id } => id.0,
            _ => continue,
        };
        let index = match actions.iter().position(|(existing, _)| *existing == id) {
            Some(index) => index,
            None => {
                actions.push((id, DocumentAction::default()));
                actions.len() - 1
            }
        };
        let action = &mut actions[index].1;
        let more_than_one = || failed(format!("Document {id} has more than one content command"));
        match write {
            StorageWrite::DocumentCreate { record, content } => {
                if action.create.is_some() || action.content.is_some() || action.copy.is_some() {
                    return more_than_one();
                }
                action.create = Some(record.clone());
                action.content = Some(content.clone());
            }
            StorageWrite::DocumentCopy { record, source } => {
                if action.create.is_some() || action.content.is_some() || action.copy.is_some() {
                    return more_than_one();
                }
                action.create = Some(record.clone());
                action.copy = Some(*source);
            }
            StorageWrite::DocumentChange { content, .. } => {
                if action.content.is_some() || action.copy.is_some() {
                    return more_than_one();
                }
                action.content = Some(content.clone());
            }
            StorageWrite::DocumentRetire { .. } => {
                if action.retire {
                    return failed(format!("Document {id} is retired more than once"));
                }
                action.retire = true;
            }
            _ => unreachable!("filtered above"),
        }
    }
    Ok(actions)
}

/// At most `limit` items, and a cursor after the last one when more matched.
pub(crate) fn page<T>(mut values: Vec<T>, limit: usize, order: ScanOrder, id: impl Fn(&T) -> u64) -> Page<T> {
    if values.len() <= limit {
        return Page {
            items: values,
            next: None,
        };
    }
    values.truncate(limit);
    let next = values.last().map(|last| next_cursor(id(last), order));
    Page { items: values, next }
}
