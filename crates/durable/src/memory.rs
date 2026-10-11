//! In-memory reference backend, a port of upstream `storage/memory.ts`.

use crate::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentContent, DocumentCreate,
    DocumentId, DocumentPoint, DocumentQuery, DocumentRecord, DocumentScope, EntryId, EntryQuery, EntryRecord,
    MAX_SAFE_ID, Page, Result, ScanOrder, ScanStart, Seq, Storage, StorageWrite, StoredDocument, StoredEntry,
    SubmissionId, SubmissionQuery, SubmissionRecord, SubmissionStatus, TaskId, TaskQuery, TaskRecord, TaskStatus,
    cursor_after, failed, request, scan_start,
};
use crate::{DocumentAction, Table, check_global_ids, document_actions, page};
use chord_delta::{Op, apply_immutable_batches};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::ops::Bound::{Excluded, Included, Unbounded};

struct Revision {
    seq: Seq,
    content: DocumentContent,
}

struct StoredDoc {
    record: DocumentRecord,
    revisions: Vec<Revision>,
}

#[derive(Default)]
struct AddressIndex {
    ids: BTreeSet<u64>,
    current: Option<u64>,
}

type AddressKey = (String, DocumentScope, Option<String>);

fn address_key(kind: &str, scope: DocumentScope, key: Option<&str>) -> AddressKey {
    (kind.to_owned(), scope, key.map(str::to_owned))
}

fn create_address(create: &DocumentCreate) -> AddressKey {
    address_key(&create.kind, create.scope, create.key.as_deref())
}

/// Sorted IDs in scan order, after the cursor's ID when there is one.
fn scan_ids(ids: &BTreeSet<u64>, start: ScanStart) -> Box<dyn Iterator<Item = u64> + '_> {
    let after = start.after.map(|after| u64::try_from(after).ok());
    match (start.order, after) {
        (ScanOrder::Ascending, None | Some(None)) => Box::new(ids.iter().copied()),
        (ScanOrder::Ascending, Some(Some(after))) => Box::new(ids.range((Excluded(after), Unbounded)).copied()),
        (ScanOrder::Descending, None) => Box::new(ids.iter().rev().copied()),
        (ScanOrder::Descending, Some(None)) => Box::new(std::iter::empty()),
        (ScanOrder::Descending, Some(Some(after))) => Box::new(ids.range(..after).rev().copied()),
    }
}

/// IDs in `ids` within the inclusive signed bounds, ascending.
fn bounded(ids: &BTreeSet<u64>, min: i64, max: i64) -> impl DoubleEndedIterator<Item = u64> + '_ {
    let empty = max < 0 || min > max;
    let low = if empty { 1 } else { u64::try_from(min).unwrap_or(0) };
    let high = if empty { 0 } else { u64::try_from(max).unwrap_or(0) };
    ids.range((Included(low), Included(high.max(low.saturating_sub(1)))))
        .copied()
        .filter(move |_| !empty)
}

fn insert_index<K: std::hash::Hash + Eq>(index: &mut HashMap<K, BTreeSet<u64>>, key: K, id: u64) {
    index.entry(key).or_default().insert(id);
}

/// A validated commit whose writes have their document copies resolved to creations.
pub struct PreparedCommit {
    pub seq: Seq,
    /// The writes as they will be applied: no `DocumentCopy` remains.
    pub writes: Vec<StorageWrite>,
    actions: Vec<(u64, DocumentAction)>,
}

/// Detached in-memory reference implementation of [`Storage`]. Reads return owned copies.
pub struct MemoryStorage {
    record_types: HashMap<u64, Table>,
    conversations: HashMap<u64, ConversationRecord>,
    conversation_ids: BTreeSet<u64>,
    conversations_by_owner_conversation: HashMap<u64, BTreeSet<u64>>,
    conversations_by_owner_task: HashMap<u64, BTreeSet<u64>>,
    entries: HashMap<u64, (EntryRecord, Seq)>,
    entry_ids: HashMap<u64, BTreeSet<u64>>,
    head_entry_ids: HashMap<u64, BTreeSet<u64>>,
    tasks: HashMap<u64, TaskRecord>,
    task_ids: BTreeSet<u64>,
    task_ids_by_status: HashMap<TaskStatus, BTreeSet<u64>>,
    submissions: HashMap<u64, SubmissionRecord>,
    submission_ids: BTreeSet<u64>,
    submission_ids_by_status: HashMap<SubmissionStatus, BTreeSet<u64>>,
    submission_ids_by_request: HashMap<(u64, String), u64>,
    documents: HashMap<u64, StoredDoc>,
    document_addresses: HashMap<AddressKey, AddressIndex>,
    document_ids_by_scope: HashMap<DocumentScope, BTreeSet<u64>>,
    next_id: u64,
    next_seq: u64,
    closed: bool,
}

impl Default for MemoryStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStorage {
    pub fn new() -> Self {
        MemoryStorage {
            record_types: HashMap::new(),
            conversations: HashMap::new(),
            conversation_ids: BTreeSet::new(),
            conversations_by_owner_conversation: HashMap::new(),
            conversations_by_owner_task: HashMap::new(),
            entries: HashMap::new(),
            entry_ids: HashMap::new(),
            head_entry_ids: HashMap::new(),
            tasks: HashMap::new(),
            task_ids: BTreeSet::new(),
            task_ids_by_status: HashMap::new(),
            submissions: HashMap::new(),
            submission_ids: BTreeSet::new(),
            submission_ids_by_status: HashMap::new(),
            submission_ids_by_request: HashMap::new(),
            documents: HashMap::new(),
            document_addresses: HashMap::new(),
            document_ids_by_scope: HashMap::new(),
            next_id: 2,
            next_seq: 1,
            closed: false,
        }
    }

    /// Validate one commit and resolve its document copies without changing observable state. `seq` defaults to the
    /// next sequence; a replaying caller (JSONL recovery) supplies the recorded one, which must not go backwards.
    pub fn prepare_commit(&self, mut writes: Vec<StorageWrite>, seq: Option<Seq>) -> Result<PreparedCommit> {
        self.assert_open()?;
        let seq = seq.unwrap_or(Seq(self.next_seq));
        if seq.0 < self.next_seq || seq.0 > MAX_SAFE_ID {
            return failed(format!("Commit sequence {} does not strictly increase", seq.0));
        }
        self.resolve_document_copies(&mut writes)?;
        check_global_ids(&writes, |id| Ok(self.record_types.get(&id).copied()))?;
        let actions = document_actions(&writes)?;
        self.check_document_actions(&actions)?;
        Ok(PreparedCommit { seq, writes, actions })
    }

    /// Apply a commit prepared against the current state; nothing in it can fail.
    pub fn apply_commit(&mut self, prepared: PreparedCommit) -> Seq {
        let PreparedCommit { seq, writes, actions } = prepared;
        for write in writes {
            self.apply_write(write, seq);
        }
        self.apply_document_actions(actions, seq);
        self.next_seq = seq.0 + 1;
        seq
    }

    fn assert_open(&self) -> Result<()> {
        if self.closed {
            failed("MemoryStorage is closed")
        } else {
            Ok(())
        }
    }

    fn materialize(&self, id: DocumentId, at: DocumentPoint) -> Result<Option<StoredDocument>> {
        let Some(stored) = self.documents.get(&id.0) else {
            return Ok(None);
        };
        if at != DocumentPoint::Current && stored.record.identity.is_current_only() {
            return request(format!("Document {} does not retain historical content", id.0));
        }
        if !stored.record.is_alive_at(at) {
            return Ok(None);
        }
        let revisions = match at {
            DocumentPoint::Current => &stored.revisions[..],
            DocumentPoint::At(seq) => &stored.revisions[..stored.revisions.partition_point(|r| r.seq <= seq)],
        };
        let Some(base_index) = revisions
            .iter()
            .rposition(|r| matches!(r.content, DocumentContent::Base { .. }))
        else {
            return failed(format!("Document {} is missing a required base", id.0));
        };
        let DocumentContent::Base { version, value } = &revisions[base_index].content else {
            unreachable!()
        };
        let mut batches: Vec<&[Op]> = Vec::new();
        for revision in &revisions[base_index + 1..] {
            match &revision.content {
                DocumentContent::Delta { version: v, ops } if v == version => batches.push(ops),
                _ => {
                    return failed(format!(
                        "Document {} crosses a stored version boundary without a base",
                        id.0
                    ));
                }
            }
        }
        let value = match apply_immutable_batches(&Value::Object(value.clone()), batches) {
            Ok(Value::Object(value)) => value,
            Ok(_) => return failed(format!("Document {} is not an object", id.0)),
            Err(error) => return failed(error.to_string()),
        };
        Ok(Some(StoredDocument {
            record: stored.record.clone(),
            version: *version,
            value,
            deltas_since_base: revisions.len() - base_index - 1,
        }))
    }

    fn check_conversation(&self, id: ConversationId) -> Result<()> {
        if self.conversations.contains_key(&id.0) {
            Ok(())
        } else {
            request(format!("Unknown conversation: {}", id.0))
        }
    }

    /// Visible entries newest first through the fork chain, at most `take` of them.
    fn visible_descending(&self, id: ConversationId, min: i64, max: i64, take: usize) -> Result<Vec<&EntryRecord>> {
        self.check_conversation(id)?;
        let mut out = Vec::new();
        let (mut current, mut upper) = (id.0, max);
        loop {
            if let Some(ids) = self.entry_ids.get(&current) {
                for entry in bounded(ids, min, upper).rev() {
                    if out.len() == take {
                        return Ok(out);
                    }
                    out.push(&self.entries[&entry].0);
                }
            }
            let Some(parent) = self.conversations[&current].parent else {
                break;
            };
            upper = upper.min(parent.at.0 as i64);
            if upper < min {
                break;
            }
            current = parent.conversation_id.0;
        }
        Ok(out)
    }

    /// Visible entries oldest first: the fork chain's segments from the root conversation forward.
    fn visible_ascending(&self, id: ConversationId, min: i64, max: i64, take: usize) -> Result<Vec<&EntryRecord>> {
        self.check_conversation(id)?;
        let mut segments = Vec::new();
        let (mut current, mut upper) = (id.0, max);
        loop {
            segments.push((current, upper));
            let Some(parent) = self.conversations[&current].parent else {
                break;
            };
            upper = upper.min(parent.at.0 as i64);
            if upper < min {
                break;
            }
            current = parent.conversation_id.0;
        }
        let mut out = Vec::new();
        for (conversation, upper) in segments.into_iter().rev() {
            let Some(ids) = self.entry_ids.get(&conversation) else {
                continue;
            };
            for entry in bounded(ids, min, upper) {
                if out.len() == take {
                    return Ok(out);
                }
                out.push(&self.entries[&entry].0);
            }
        }
        Ok(out)
    }

    /// Replace each `document.copy` with a `document.create` of the source's materialized base.
    fn resolve_document_copies(&self, writes: &mut [StorageWrite]) -> Result<()> {
        let changed: BTreeSet<u64> = writes
            .iter()
            .filter_map(|write| match write {
                StorageWrite::DocumentCreate { record, .. } | StorageWrite::DocumentCopy { record, .. } => {
                    Some(record.id.0)
                }
                StorageWrite::DocumentChange { id, .. } | StorageWrite::DocumentRetire { id } => Some(id.0),
                _ => None,
            })
            .collect();
        for write in writes.iter_mut() {
            let StorageWrite::DocumentCopy { record, source } = write else {
                continue;
            };
            let resolved = (|| -> Result<DocumentContent> {
                if changed.contains(&source.id.0) {
                    return failed(format!(
                        "Fork source document {} is changed in the copy batch",
                        source.id.0
                    ));
                }
                let Some(stored) = self.materialize(source.id, source.at)? else {
                    return failed(format!("Fork source document {} cannot be read", source.id.0));
                };
                let from = &stored.record.identity;
                if !matches!(from.scope, DocumentScope::Conversation { .. })
                    || !matches!(record.scope, DocumentScope::Conversation { .. })
                    || from.kind != record.kind
                    || from.key != record.key
                    || from.history != record.history
                    || from.fork != record.fork
                {
                    return failed(format!(
                        "Fork source document {} does not match the copied record",
                        source.id.0
                    ));
                }
                Ok(DocumentContent::Base {
                    version: stored.version,
                    value: stored.value,
                })
            })();
            let Ok(content) = resolved else {
                return failed(format!("Document copy {} was rejected", record.id.0));
            };
            *write = StorageWrite::DocumentCreate {
                record: record.clone(),
                content,
            };
        }
        Ok(())
    }

    fn check_document_actions(&self, actions: &[(u64, DocumentAction)]) -> Result<()> {
        let mut live_counts: Vec<(AddressKey, i64)> = Vec::new();
        for (id, action) in actions {
            let existing = self.documents.get(id);
            if action.create.is_none() && existing.is_none() {
                return failed(format!("Unknown document: {id}"));
            }
            if action.create.is_some() && existing.is_some() {
                return failed(format!("Document {id} already exists"));
            }
            if existing.is_some_and(|existing| existing.record.retired_at.is_some()) {
                return failed(format!("Document {id} is retired"));
            }
            if action.create.is_some() && !matches!(action.content, Some(DocumentContent::Base { .. })) {
                return failed(format!("Document {id} is created without a base"));
            }
            if let Some(DocumentContent::Delta { version, .. }) = &action.content {
                let Some(previous) = existing.and_then(|existing| existing.revisions.last()) else {
                    return failed(format!("Document {id} delta has no base"));
                };
                if previous.content.version() != *version {
                    return failed(format!("Document {id} version transition requires a base"));
                }
            }
            let key = match (&action.create, existing) {
                (Some(create), _) => create_address(create),
                (None, Some(existing)) => create_address(&existing.record.identity),
                (None, None) => unreachable!("checked above"),
            };
            let current = self.document_addresses.get(&key).and_then(|address| address.current);
            let index = match live_counts.iter().position(|(k, _)| *k == key) {
                Some(index) => index,
                None => {
                    live_counts.push((key, i64::from(current.is_some())));
                    live_counts.len() - 1
                }
            };
            if action.retire && current == Some(*id) {
                live_counts[index].1 -= 1;
            }
            if action.create.is_some() && !action.retire {
                live_counts[index].1 += 1;
            }
        }
        if live_counts.iter().any(|(_, live)| *live > 1) {
            return failed("Document address already has a current incarnation");
        }
        Ok(())
    }

    fn apply_document_actions(&mut self, actions: Vec<(u64, DocumentAction)>, seq: Seq) {
        for (id, action) in actions {
            let created = action.create.is_some();
            if let Some(create) = action.create {
                let key = create_address(&create);
                let scope = create.scope;
                let content = action.content.expect("checked: creation carries a base");
                let record = DocumentRecord {
                    identity: create,
                    created_at: seq,
                    retired_at: action.retire.then_some(seq),
                };
                self.documents.insert(
                    id,
                    StoredDoc {
                        record,
                        revisions: vec![Revision { seq, content }],
                    },
                );
                self.record_types.insert(id, Table::Document);
                self.document_addresses.entry(key).or_default().ids.insert(id);
                insert_index(&mut self.document_ids_by_scope, scope, id);
                self.next_id = self.next_id.max(id + 1);
            } else if let Some(content) = action.content {
                let stored = self.documents.get_mut(&id).expect("checked: document exists");
                let base = matches!(content, DocumentContent::Base { .. });
                let revision = Revision { seq, content };
                if base && stored.record.identity.is_current_only() {
                    stored.revisions = vec![revision];
                } else {
                    stored.revisions.push(revision);
                }
            }
            let stored = self.documents.get_mut(&id).expect("checked: document exists");
            if action.retire && !created {
                stored.record.retired_at = Some(seq);
            }
            if action.retire && stored.record.identity.is_current_only() {
                stored.revisions.clear();
            }
            if created || action.retire {
                let key = create_address(&stored.record.identity);
                let address = self.document_addresses.get_mut(&key).expect("indexed at creation");
                if action.retire && address.current == Some(id) {
                    address.current = None;
                }
                if created && !action.retire {
                    address.current = Some(id);
                }
            }
        }
    }

    fn apply_write(&mut self, write: StorageWrite, seq: Seq) {
        match write {
            StorageWrite::Conversation { value } => {
                let id = value.id.0;
                self.record_types.insert(id, Table::Conversation);
                self.conversation_ids.insert(id);
                if let Some(owner) = value.owner {
                    insert_index(
                        &mut self.conversations_by_owner_conversation,
                        owner.conversation_id.0,
                        id,
                    );
                    insert_index(&mut self.conversations_by_owner_task, owner.task_id.0, id);
                }
                self.conversations.insert(id, value);
                self.next_id = self.next_id.max(id + 1);
            }
            StorageWrite::Entry { value } => {
                let (id, conversation) = (value.id.0, value.conversation_id.0);
                self.record_types.insert(id, Table::Entry);
                insert_index(&mut self.entry_ids, conversation, id);
                if value.head.is_some() {
                    insert_index(&mut self.head_entry_ids, conversation, id);
                }
                self.entries.insert(id, (value, seq));
                self.next_id = self.next_id.max(id + 1);
            }
            StorageWrite::Task { value } => {
                let (id, status) = (value.id.0, value.state.status());
                self.record_types.insert(id, Table::Task);
                match self.tasks.get(&id).map(|previous| previous.state.status()) {
                    None => {
                        self.task_ids.insert(id);
                        insert_index(&mut self.task_ids_by_status, status, id);
                    }
                    Some(previous) if previous != status => {
                        self.task_ids_by_status.entry(previous).or_default().remove(&id);
                        insert_index(&mut self.task_ids_by_status, status, id);
                    }
                    Some(_) => {}
                }
                self.tasks.insert(id, value);
                self.next_id = self.next_id.max(id + 1);
            }
            StorageWrite::Submission { value } => {
                let id = value.id.0;
                self.record_types.insert(id, Table::Submission);
                let previous = self.submissions.get(&id);
                match previous.map(|previous| previous.status) {
                    None => {
                        self.submission_ids.insert(id);
                        insert_index(&mut self.submission_ids_by_status, value.status, id);
                    }
                    Some(status) if status != value.status => {
                        self.submission_ids_by_status.entry(status).or_default().remove(&id);
                        insert_index(&mut self.submission_ids_by_status, value.status, id);
                    }
                    Some(_) => {}
                }
                if let Some(previous) = previous
                    && let Some(request_id) = &previous.request_id
                {
                    let key = (previous.conversation_id.0, request_id.clone());
                    if self.submission_ids_by_request.get(&key) == Some(&id) {
                        self.submission_ids_by_request.remove(&key);
                    }
                }
                if let Some(request_id) = &value.request_id {
                    self.submission_ids_by_request
                        .insert((value.conversation_id.0, request_id.clone()), id);
                }
                self.submissions.insert(id, value);
                self.next_id = self.next_id.max(id + 1);
            }
            StorageWrite::DocumentCreate { .. }
            | StorageWrite::DocumentCopy { .. }
            | StorageWrite::DocumentChange { .. }
            | StorageWrite::DocumentRetire { .. } => {}
        }
    }
}

impl Storage for MemoryStorage {
    fn commit(&mut self, writes: Vec<StorageWrite>) -> Result<Seq> {
        let prepared = self.prepare_commit(writes, None)?;
        Ok(self.apply_commit(prepared))
    }

    fn mint_id(&mut self) -> Result<u64> {
        self.assert_open()?;
        if self.next_id > MAX_SAFE_ID {
            return failed("ID space is exhausted");
        }
        self.next_id += 1;
        Ok(self.next_id - 1)
    }

    fn conversation(&self, id: ConversationId) -> Result<Option<ConversationRecord>> {
        self.assert_open()?;
        Ok(self.conversations.get(&id.0).cloned())
    }

    fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>> {
        self.assert_open()?;
        let empty = BTreeSet::new();
        let ids = match (query.owner_task_id, query.owner_conversation_id) {
            (Some(task), _) => self.conversations_by_owner_task.get(&task.0).unwrap_or(&empty),
            (None, Some(owner)) => self.conversations_by_owner_conversation.get(&owner.0).unwrap_or(&empty),
            (None, None) => &self.conversation_ids,
        };
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let values = scan_ids(ids, start)
            .map(|id| &self.conversations[&id])
            .filter(|value| {
                query.owner_conversation_id.is_none_or(|owner| {
                    value
                        .owner
                        .is_some_and(|value_owner| value_owner.conversation_id == owner)
                })
            })
            .take(limit + 1)
            .cloned()
            .collect();
        Ok(page(values, limit, start.order, |value: &ConversationRecord| {
            value.id.0
        }))
    }

    fn entry(&self, id: EntryId) -> Result<Option<StoredEntry>> {
        self.assert_open()?;
        Ok(self.entries.get(&id.0).map(|(entry, commit_seq)| StoredEntry {
            entry: entry.clone(),
            commit_seq: *commit_seq,
        }))
    }

    fn visible_entry(&self, conversation_id: ConversationId, id: EntryId) -> Result<Option<StoredEntry>> {
        self.assert_open()?;
        let bound = id.0 as i64;
        let found = self.visible_descending(conversation_id, bound, bound, 1)?;
        Ok(found.first().map(|entry| StoredEntry {
            entry: (*entry).clone(),
            commit_seq: self.entries[&entry.id.0].1,
        }))
    }

    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
    ) -> Result<Option<EntryRecord>> {
        self.assert_open()?;
        self.check_conversation(conversation_id)?;
        let (mut current, mut upper) = (conversation_id.0, at_or_before.map_or(i64::MAX, |at| at.0 as i64));
        loop {
            if let Some(ids) = self.head_entry_ids.get(&current)
                && let Some(id) = bounded(ids, i64::MIN, upper).next_back()
            {
                return Ok(Some(self.entries[&id].0.clone()));
            }
            let Some(parent) = self.conversations[&current].parent else {
                return Ok(None);
            };
            upper = upper.min(parent.at.0 as i64);
            current = parent.conversation_id.0;
        }
    }

    fn scan_entries(&self, query: &EntryQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<EntryRecord>> {
        self.assert_open()?;
        let ScanStart { order, after } = scan_start(query.order, cursor, ScanOrder::Descending)?;
        // The cursor narrows the bound on the side the scan moves away from.
        let mut min = query.min_entry_id.map_or(i64::MIN, |id| id.0 as i64);
        let mut max = query.max_entry_id.map_or(i64::MAX, |id| id.0 as i64);
        if let Some(after) = after {
            match order {
                ScanOrder::Descending => max = query.max_entry_id.map_or(after, |id| id.0 as i64).min(after - 1),
                ScanOrder::Ascending => min = query.min_entry_id.map_or(after, |id| id.0 as i64).max(after + 1),
            }
        }
        let visible = match order {
            ScanOrder::Descending => self.visible_descending(query.conversation_id, min, max, limit + 1)?,
            ScanOrder::Ascending => self.visible_ascending(query.conversation_id, min, max, limit + 1)?,
        };
        Ok(page(
            visible.into_iter().cloned().collect(),
            limit,
            order,
            |entry: &EntryRecord| entry.id.0,
        ))
    }

    fn task(&self, id: TaskId) -> Result<Option<TaskRecord>> {
        self.assert_open()?;
        Ok(self.tasks.get(&id.0).cloned())
    }

    fn scan_tasks(&self, query: &TaskQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<TaskRecord>> {
        self.assert_open()?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let empty = BTreeSet::new();
        let ids = match query.status {
            None => &self.task_ids,
            Some(status) => self.task_ids_by_status.get(&status).unwrap_or(&empty),
        };
        let values = scan_ids(ids, start)
            .map(|id| &self.tasks[&id])
            .filter(|task| {
                query.conversation_id.is_none_or(|id| task.conversation_id == id)
                    && query.kind.as_ref().is_none_or(|kind| task.kind == *kind)
                    && query.abort_requested.is_none_or(|abort| task.abort_requested == abort)
                    && query.background.is_none_or(|background| task.background == background)
            })
            .take(limit + 1)
            .cloned()
            .collect();
        Ok(page(values, limit, start.order, |task: &TaskRecord| task.id.0))
    }

    fn submission(&self, id: SubmissionId) -> Result<Option<SubmissionRecord>> {
        self.assert_open()?;
        Ok(self.submissions.get(&id.0).cloned())
    }

    fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<SubmissionRecord>> {
        self.assert_open()?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let empty = BTreeSet::new();
        let ids = match query.status {
            None => &self.submission_ids,
            Some(status) => self.submission_ids_by_status.get(&status).unwrap_or(&empty),
        };
        let values = scan_ids(ids, start)
            .map(|id| &self.submissions[&id])
            .filter(|submission| query.conversation_id.is_none_or(|id| submission.conversation_id == id))
            .take(limit + 1)
            .cloned()
            .collect();
        Ok(page(values, limit, start.order, |submission: &SubmissionRecord| {
            submission.id.0
        }))
    }

    fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> Result<Option<SubmissionRecord>> {
        self.assert_open()?;
        let id = self
            .submission_ids_by_request
            .get(&(conversation_id.0, request_id.to_owned()));
        Ok(id.map(|id| self.submissions[id].clone()))
    }

    fn find_document(&self, address: &DocumentAddress, at: DocumentPoint) -> Result<Option<DocumentRecord>> {
        self.assert_open()?;
        let key = address_key(&address.kind, address.scope, address.key.as_deref());
        let Some(index) = self.document_addresses.get(&key) else {
            return Ok(None);
        };
        let id = match at {
            DocumentPoint::Current => index.current,
            DocumentPoint::At(_) => index
                .ids
                .iter()
                .copied()
                .find(|id| self.documents[id].record.is_alive_at(at)),
        };
        Ok(id.map(|id| self.documents[&id].record.clone()))
    }

    fn document(&self, id: DocumentId, at: DocumentPoint) -> Result<Option<StoredDocument>> {
        self.assert_open()?;
        self.materialize(id, at)
    }

    fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<DocumentRecord>> {
        self.assert_open()?;
        let empty = BTreeSet::new();
        let ids = self.document_ids_by_scope.get(&query.scope).unwrap_or(&empty);
        let after = match cursor {
            Some(cursor) => cursor_after(cursor)?,
            None => None,
        };
        let values = scan_ids(
            ids,
            ScanStart {
                order: ScanOrder::Ascending,
                after,
            },
        )
        .map(|id| &self.documents[&id].record)
        .filter(|record| {
            query.kind.as_ref().is_none_or(|kind| record.identity.kind == *kind) && record.is_alive_at(query.at)
        })
        .take(limit + 1)
        .cloned()
        .collect();
        Ok(page(values, limit, ScanOrder::Ascending, |record: &DocumentRecord| {
            record.identity.id.0
        }))
    }

    fn close(&mut self) -> Result<()> {
        self.closed = true;
        Ok(())
    }
}
