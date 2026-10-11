//! One Session commit callback's transaction: table reads before the first table write, staged table writes,
//! document acquisition and retirement, fork copies, and the atomic batch assembled from them.
//!
//! Port of `packages/durable/src/session/{transaction,forks}.ts`. Every Tx operation runs synchronously while the
//! commit holds the Session line.

mod chat_access;
mod scheduler_access;

use crate::documents::{
    Doc, Owner, Resolved, address_id, address_of, check_record_scope, check_record_version, document_create,
    materialize_value, object, resolve_address,
};
use crate::session::{CommitChange, Inner, LineState, Loaded, LoadedDocument, Result, SessionError, lock};
use crate::{
    ConversationId, ConversationOwner, ConversationParent, ConversationQuery, ConversationRecord, Cursor,
    DocumentContent, DocumentCopySource, DocumentCreate, DocumentFork, DocumentId, DocumentPoint, DocumentQuery,
    DocumentRecord, DocumentScope, EntryId, EntryQuery, EntryRecord, Page, Seq, StorageWrite, SubmissionId,
    SubmissionRecord, SubmissionStatus, SubmissionType, TaskId, TaskQuery, TaskRecord, TaskState, TaskStatus,
};
use chord_delta::{Draft, Prepared, Tracker};
use indexmap::IndexMap;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

const INTERNAL_SCAN_PAGE_SIZE: usize = 256;

/// Executable task definition as far as the Session needs it: its registered name, version, and first checkpoint.
pub struct TaskDefinition {
    pub name: String,
    pub version: u64,
    pub initial: Box<dyn Fn(&Value) -> Value + Send + Sync>,
}

impl TaskDefinition {
    pub fn new(name: &str, version: u64, initial: impl Fn(&Value) -> Value + Send + Sync + 'static) -> Self {
        TaskDefinition {
            name: name.to_owned(),
            version,
            initial: Box::new(initial),
        }
    }
}

/// Who owns a task: its conversation (a top-level task) or another task of the same conversation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TaskOwnership {
    #[default]
    Conversation,
    Task(TaskId),
}

/// Creation options for a durable task.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TaskOptions {
    pub ownership: TaskOwnership,
    /// Default: the owner task's conversation, or the transaction's bound conversation.
    pub conversation_id: Option<ConversationId>,
    /// Conversation-owned tasks only.
    pub background: bool,
    pub abandon_on_restart: bool,
}

/// Ownership selected explicitly whenever a conversation is created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversationOwnership {
    Ownerless,
    Task(TaskId),
}

/// First entry of the active context an entry selects; `SelfId` is the entry's own, newly assigned ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryHead {
    Entry(EntryId),
    SelfId,
}

/// Entry content supplied before the Session assigns identity and task attribution.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EntryDraft {
    pub kind: String,
    pub model: Option<Vec<Value>>,
    pub data: Option<Value>,
    pub head: Option<EntryHead>,
    pub edits: Option<Vec<Value>>,
}

impl EntryDraft {
    pub fn new(kind: &str) -> EntryDraft {
        EntryDraft {
            kind: kind.to_owned(),
            ..EntryDraft::default()
        }
    }
}

/// Submission fields supplied before the Session assigns an ID. No admission rules apply to a raw creation.
#[derive(Clone, Debug, PartialEq)]
pub struct SubmissionCreate {
    pub conversation_id: ConversationId,
    pub request_id: Option<String>,
    pub kind: SubmissionType,
    pub status: SubmissionStatus,
    pub entry: Option<EntryId>,
    pub answer: Option<EntryId>,
    pub reason: Option<String>,
    pub detail: Option<Value>,
}

/// Terminal status staged for a submission.
#[derive(Clone, Debug, PartialEq)]
pub enum SubmissionSettlement {
    Done { answer: EntryId },
    Unanswered { reason: String, detail: Option<Value> },
}

enum SubmissionChange {
    Settle(SubmissionSettlement),
    Place(EntryId),
}

/// Complete record after one change: placement turns a queued input `placed` and a queued write `done`; only a placed
/// input can be answered; a settled record stays.
fn apply_submission_change(current: &SubmissionRecord, change: &SubmissionChange) -> Result<SubmissionRecord> {
    if matches!(current.status, SubmissionStatus::Done | SubmissionStatus::Unanswered) {
        return Ok(current.clone());
    }
    let mut next = current.clone();
    match change {
        SubmissionChange::Place(entry) => {
            if current.status != SubmissionStatus::Queued {
                return Err(SessionError::Error(format!(
                    "Submission {} is not queued",
                    current.id.0
                )));
            }
            next.status = match current.kind {
                SubmissionType::Input => SubmissionStatus::Placed,
                SubmissionType::Write => SubmissionStatus::Done,
            };
            next.entry = Some(*entry);
        }
        SubmissionChange::Settle(SubmissionSettlement::Done { answer }) => {
            if current.status != SubmissionStatus::Placed {
                return Err(SessionError::Error(format!(
                    "Submission {} is not a placed input",
                    current.id.0
                )));
            }
            next.status = SubmissionStatus::Done;
            next.answer = Some(*answer);
        }
        SubmissionChange::Settle(SubmissionSettlement::Unanswered { reason, detail }) => {
            // An unanswered input keeps its entry.
            next.status = SubmissionStatus::Unanswered;
            next.reason = Some(reason.clone());
            next.detail = detail.clone();
        }
    }
    Ok(next)
}

/// Defaults a commit binds to: the `create_task` conversation and the task attributed to appended entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxScope {
    pub conversation_id: Option<ConversationId>,
    pub task_id: Option<TaskId>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskWrite {
    Create,
    Replace,
}

#[derive(Default)]
struct TxTask {
    committed: Option<Option<TaskRecord>>,
    write: Option<(TaskWrite, TaskRecord)>,
    publication_conversation: Option<ConversationId>,
}

/// Storage/cache provenance of one staged incarnation.
enum Target {
    Loaded(Loaded),
    Created {
        record: DocumentCreate,
        version: u64,
        tracker: Tracker,
    },
    ForkCopy {
        record: DocumentCreate,
        source: DocumentCopySource,
    },
    RetireOnly(DocumentRecord),
}

/// One incarnation acquired, created, copied, or retired by this transaction.
struct DocEntry {
    address_id: String,
    /// Absent for definition-free fork copies.
    definition: Option<Doc>,
    draft: Option<Draft>,
    target: Option<Target>,
    retire: bool,
}

enum PlanRecord {
    New(DocumentCreate),
    Committed(DocumentRecord),
}

impl PlanRecord {
    fn identity(&self) -> &DocumentCreate {
        match self {
            PlanRecord::New(record) => record,
            PlanRecord::Committed(record) => &record.identity,
        }
    }
}

enum PlanTracker {
    Created(Tracker),
    Loaded(Loaded),
}

struct PlanChange {
    tracker: PlanTracker,
    prepared: Prepared,
    version: u64,
    definition: Option<Doc>,
}

/// What one staged incarnation writes and publishes, decided before Storage admission so adoption only applies it.
pub(crate) struct Plan {
    address_id: String,
    record: PlanRecord,
    retire: bool,
    content: Option<StorageWrite>,
    change: Option<PlanChange>,
    conversation_id: Option<ConversationId>,
}

impl Plan {
    /// Every creation, copy, and retirement publishes, and a loaded incarnation that writes content (a
    /// migration-only base included, so observers of the older shape receive the new value).
    fn publishes(&self) -> bool {
        self.retire
            || !matches!(
                self.change,
                Some(PlanChange {
                    tracker: PlanTracker::Loaded(_),
                    ..
                })
            )
            || self.content.is_some()
    }
}

fn object_of(value: &Value) -> crate::JsonObject {
    value.as_object().cloned().expect("document values are objects")
}

/// The transaction of one Session commit callback. Table reads fail with [`SessionError::ReadAfterWrite`] after the
/// first table write; document access stays available.
pub struct Tx<'a> {
    inner: &'a Inner,
    line: &'a mut LineState,
    scope: TxScope,
    has_table_write: bool,
    /// Conversation and entry writes, staged eagerly in order.
    writes: Vec<StorageWrite>,
    created_conversations: HashSet<ConversationId>,
    fork_source_conversations: HashSet<ConversationId>,
    fork_source_documents: HashSet<DocumentId>,
    tasks: IndexMap<TaskId, TxTask>,
    submissions: IndexMap<SubmissionId, SubmissionRecord>,
    submission_changes: Vec<(SubmissionId, SubmissionChange)>,
    documents: Vec<DocEntry>,
    /// Latest entry (index into `documents`) at each logical address.
    latest: HashMap<String, usize>,
}

impl<'a> Tx<'a> {
    pub(crate) fn new(inner: &'a Inner, line: &'a mut LineState, scope: TxScope) -> Tx<'a> {
        Tx {
            inner,
            line,
            scope,
            has_table_write: false,
            writes: Vec::new(),
            created_conversations: HashSet::new(),
            fork_source_conversations: HashSet::new(),
            fork_source_documents: HashSet::new(),
            tasks: IndexMap::new(),
            submissions: IndexMap::new(),
            submission_changes: Vec::new(),
            documents: Vec::new(),
            latest: HashMap::new(),
        }
    }

    fn read<T>(&mut self, call: impl FnOnce(&mut dyn crate::Storage) -> crate::Result<T>) -> Result<T> {
        self.inner.guard(self.line, true, call)
    }

    fn mint(&mut self) -> Result<u64> {
        self.inner.guard(self.line, false, |storage| storage.mint_id())
    }

    fn table_read(&self, method: &'static str) -> Result<()> {
        if self.has_table_write {
            Err(SessionError::ReadAfterWrite(method))
        } else {
            Ok(())
        }
    }

    // ─── Table reads ────────────────────────────────────────────────────────

    pub fn conversation(&mut self, id: ConversationId) -> Result<Option<ConversationRecord>> {
        self.table_read("conversation")?;
        self.read(|storage| storage.conversation(id))
    }

    pub fn entry(&mut self, id: EntryId) -> Result<Option<EntryRecord>> {
        self.table_read("entry")?;
        Ok(self.read(|storage| storage.entry(id))?.map(|stored| stored.entry))
    }

    /// `None` when the entry is absent or has another kind.
    pub fn entry_of(&mut self, kind: &str, id: EntryId) -> Result<Option<EntryRecord>> {
        Ok(self.entry(id)?.filter(|entry| entry.kind == kind))
    }

    pub fn task(&mut self, id: TaskId) -> Result<Option<TaskRecord>> {
        self.table_read("task")?;
        self.committed_task(id)
    }

    pub fn scan_conversations(
        &mut self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>> {
        self.table_read("scanConversations")?;
        self.read(|storage| storage.scan_conversations(query, limit, cursor))
    }

    pub fn scan_entries(
        &mut self,
        query: &EntryQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<EntryRecord>> {
        self.table_read("scanEntries")?;
        self.read(|storage| storage.scan_entries(query, limit, cursor))
    }

    /// Newest visible entry of the conversation that carries a `head`.
    pub fn latest_head_marker(&mut self, conversation_id: ConversationId) -> Result<Option<EntryRecord>> {
        self.table_read("latestHeadMarker")?;
        self.read(|storage| storage.find_latest_head_marker(conversation_id, None))
    }

    pub fn scan_tasks(&mut self, query: &TaskQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<TaskRecord>> {
        self.table_read("scanTasks")?;
        self.read(|storage| storage.scan_tasks(query, limit, cursor))
    }

    pub fn submission(&mut self, id: SubmissionId) -> Result<Option<SubmissionRecord>> {
        self.table_read("submission")?;
        self.read(|storage| storage.submission(id))
    }

    /// Committed submission with a conversation-scoped request ID, for host deduplication.
    pub fn submission_by_request(
        &mut self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> Result<Option<SubmissionRecord>> {
        self.table_read("submissionByRequest")?;
        self.read(|storage| storage.submission_by_request(conversation_id, request_id))
    }

    // ─── Table writes ───────────────────────────────────────────────────────

    pub fn create_conversation(&mut self, ownership: ConversationOwnership) -> Result<ConversationRecord> {
        self.has_table_write = true;
        self.stage_conversation(None, ownership)
    }

    /// Fork history at one visible entry; conversation documents are copied by their fork policy.
    pub fn fork_conversation(
        &mut self,
        parent: ConversationId,
        at: EntryId,
        ownership: ConversationOwnership,
    ) -> Result<ConversationRecord> {
        self.has_table_write = true;
        self.stage_conversation(
            Some(ConversationParent {
                conversation_id: parent,
                at,
            }),
            ownership,
        )
    }

    fn stage_conversation(
        &mut self,
        parent: Option<ConversationParent>,
        ownership: ConversationOwnership,
    ) -> Result<ConversationRecord> {
        let id = ConversationId(self.mint()?);
        let owner = match ownership {
            ConversationOwnership::Ownerless => None,
            ConversationOwnership::Task(task_id) => {
                let Some(task) = self.current_task(task_id)? else {
                    return Err(SessionError::Error(format!(
                        "Conversation owner task {} does not exist",
                        task_id.0
                    )));
                };
                Some(ConversationOwner {
                    conversation_id: task.conversation_id,
                    task_id,
                })
            }
        };
        let record = ConversationRecord { id, parent, owner };
        if let Some(parent) = parent {
            for (copy, source) in self.fork_copies(parent.conversation_id, parent.at, id)? {
                self.fork_source_documents.insert(source.id);
                let address = address_of(&copy);
                let entry = DocEntry {
                    address_id: address_id(&address),
                    definition: None,
                    draft: None,
                    target: Some(Target::ForkCopy { record: copy, source }),
                    retire: false,
                };
                self.latest.insert(entry.address_id.clone(), self.documents.len());
                self.documents.push(entry);
            }
            self.fork_source_conversations.insert(parent.conversation_id);
        }
        self.created_conversations.insert(id);
        self.writes.push(StorageWrite::Conversation { value: record.clone() });
        Ok(record)
    }

    /// Every persisted conversation document one fork copies: `asOf` documents of the entry's own conversation at the
    /// entry's commit, then `current` documents of the immediate parent (upstream `prepareForkDocumentCopies`).
    fn fork_copies(
        &mut self,
        parent: ConversationId,
        at: EntryId,
        child: ConversationId,
    ) -> Result<Vec<(DocumentCreate, DocumentCopySource)>> {
        let Some(entry) = self.read(|storage| storage.visible_entry(parent, at))? else {
            return Err(SessionError::Error(format!(
                "Entry {} is not visible from conversation {}",
                at.0, parent.0
            )));
        };
        let mut copies = Vec::new();
        let mut copied = HashSet::new();
        let selections = [
            (
                entry.entry.conversation_id,
                DocumentPoint::At(entry.commit_seq),
                DocumentFork::AsOf,
            ),
            (parent, DocumentPoint::Current, DocumentFork::Current),
        ];
        for (conversation_id, point, policy) in selections {
            let query = DocumentQuery {
                scope: DocumentScope::Conversation { conversation_id },
                at: point,
                kind: None,
            };
            let mut cursor: Option<Cursor> = None;
            loop {
                let page =
                    self.read(|storage| storage.scan_documents(&query, INTERNAL_SCAN_PAGE_SIZE, cursor.as_ref()))?;
                for source in page.items {
                    if !matches!(source.identity.scope, DocumentScope::Conversation { .. })
                        || source.identity.fork != Some(policy)
                    {
                        continue;
                    }
                    let id = DocumentId(self.mint()?);
                    let record = DocumentCreate {
                        id,
                        kind: source.identity.kind.clone(),
                        key: source.identity.key.clone(),
                        scope: DocumentScope::Conversation { conversation_id: child },
                        history: source.identity.history,
                        fork: source.identity.fork,
                    };
                    if !copied.insert(address_id(&address_of(&record))) {
                        let member = match &record.key {
                            None => record.kind.clone(),
                            Some(key) => format!("{}/{}", record.kind, key),
                        };
                        return Err(SessionError::Error(format!(
                            "Fork selects multiple source documents for {member}"
                        )));
                    }
                    copies.push((
                        record,
                        DocumentCopySource {
                            id: source.identity.id,
                            at: point,
                        },
                    ));
                }
                match page.next {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
        }
        Ok(copies)
    }

    /// Append an entry; `head: SelfId` starts the active context at the new entry.
    pub fn append_entry(&mut self, conversation_id: ConversationId, draft: EntryDraft) -> Result<EntryRecord> {
        self.has_table_write = true;
        self.require_conversation(conversation_id)?;
        let id = EntryId(self.mint()?);
        let record = EntryRecord {
            id,
            conversation_id,
            kind: draft.kind,
            model: draft.model,
            data: draft.data,
            head: draft.head.map(|head| match head {
                EntryHead::Entry(entry) => entry,
                EntryHead::SelfId => id,
            }),
            edits: draft.edits,
            by_task_id: self.scope.task_id,
        };
        self.writes.push(StorageWrite::Entry { value: record.clone() });
        Ok(record)
    }

    pub fn create_task(&mut self, task: &TaskDefinition, input: Value, options: TaskOptions) -> Result<TaskId> {
        self.has_table_write = true;
        let owner = match options.ownership {
            TaskOwnership::Conversation => None,
            TaskOwnership::Task(owner_id) => {
                // Validated again against the owner's final candidate during assembly.
                let Some(owner) = self.current_task(owner_id)? else {
                    return Err(SessionError::Error(format!("Task owner {} does not exist", owner_id.0)));
                };
                if options.background {
                    return Err(SessionError::Type("A child task cannot be background".into()));
                }
                if options.conversation_id.is_some_and(|id| id != owner.conversation_id) {
                    return Err(SessionError::Error(format!(
                        "A child task lives in its owner's conversation {}",
                        owner.conversation_id.0
                    )));
                }
                Some(owner)
            }
        };
        let Some(conversation_id) = owner
            .as_ref()
            .map(|owner| owner.conversation_id)
            .or(options.conversation_id)
            .or(self.scope.conversation_id)
        else {
            return Err(SessionError::Type(
                "Tx.createTask() requires options.conversationId".into(),
            ));
        };
        self.require_conversation(conversation_id)?;
        let checkpoint = (task.initial)(&input);
        let id = TaskId(self.mint()?);
        let record = TaskRecord {
            id,
            conversation_id,
            kind: task.name.clone(),
            version: task.version,
            input,
            owner: owner.map(|owner| owner.id),
            background: options.background,
            abort_requested: false,
            abort_reason: None,
            abandon_on_restart: options.abandon_on_restart,
            started_at: None,
            ended_at: None,
            state: TaskState::Pending { checkpoint },
            memos: None,
        };
        self.tasks.insert(
            id,
            TxTask {
                write: Some((TaskWrite::Create, record)),
                ..TxTask::default()
            },
        );
        Ok(id)
    }

    /// Create a raw submission record with a fresh ID. No admission rules apply.
    pub fn create_submission(&mut self, create: SubmissionCreate) -> Result<SubmissionRecord> {
        self.has_table_write = true;
        self.require_conversation(create.conversation_id)?;
        let id = SubmissionId(self.mint()?);
        let record = SubmissionRecord {
            id,
            conversation_id: create.conversation_id,
            request_id: create.request_id,
            kind: create.kind,
            status: create.status,
            entry: create.entry,
            answer: create.answer,
            reason: create.reason,
            detail: create.detail,
        };
        self.submissions.insert(id, record.clone());
        Ok(record)
    }

    /// Settle a queued or placed submission; resolved during assembly against this transaction's latest record.
    pub fn settle_submission(&mut self, id: SubmissionId, settlement: SubmissionSettlement) {
        self.has_table_write = true;
        self.submission_changes.push((id, SubmissionChange::Settle(settlement)));
    }

    /// Place a queued submission at `entry`: an input becomes `placed`, a write `done`.
    pub fn place_submission(&mut self, id: SubmissionId, entry: EntryId) {
        self.has_table_write = true;
        self.submission_changes.push((id, SubmissionChange::Place(entry)));
    }

    /// Replace one task record completely, stamping `startedAt` on its first change to `running` and `endedAt` on the
    /// change to `terminal`.
    pub fn set_task(&mut self, value: TaskRecord) -> Result<()> {
        self.has_table_write = true;
        let now = &self.inner.now;
        let task = self.tasks.entry(value.id).or_default();
        let candidate = task.write.as_ref().map(|(_, record)| record);
        if let Some(candidate) = candidate {
            if candidate.state.status() == TaskStatus::Terminal {
                return Err(SessionError::Error(format!(
                    "Task {} already has a terminal candidate",
                    value.id.0
                )));
            }
            if candidate.conversation_id != value.conversation_id {
                return Err(SessionError::Error(format!(
                    "Task {} cannot change conversations",
                    value.id.0
                )));
            }
        }
        let status = value.state.status();
        let started_at = candidate
            .and_then(|candidate| candidate.started_at)
            .or(value.started_at)
            .or_else(|| (status == TaskStatus::Running).then(now));
        let ended_at = candidate
            .and_then(|candidate| candidate.ended_at)
            .or(value.ended_at)
            .or_else(|| (status == TaskStatus::Terminal).then(now));
        let kind = match task.write {
            Some((TaskWrite::Create, _)) => TaskWrite::Create,
            _ => TaskWrite::Replace,
        };
        task.write = Some((
            kind,
            TaskRecord {
                started_at,
                ended_at,
                ..value
            },
        ));
        Ok(())
    }

    // ─── Documents ──────────────────────────────────────────────────────────

    /// Acquire a draft of a singleton document, creating it from `initial` when absent.
    pub fn doc(&mut self, doc: &Doc, owner: Owner) -> Result<Draft> {
        self.doc_at(doc, owner, None, Value::Null)
    }

    /// Acquire a draft of one family member; `seed` initializes only an absent member.
    pub fn doc_member(&mut self, doc: &Doc, owner: Owner, key: &str, seed: Value) -> Result<Draft> {
        self.doc_at(doc, owner, Some(key), seed)
    }

    fn doc_at(&mut self, doc: &Doc, owner: Owner, key: Option<&str>, seed: Value) -> Result<Draft> {
        let resolved = resolve_address(doc.definition(), owner, key)?;
        if let DocumentScope::Task { task_id } = resolved.address.scope
            && self
                .tasks
                .get(&task_id)
                .and_then(|task| task.write.as_ref())
                .is_some_and(|(_, record)| record.state.status() == TaskStatus::Terminal)
        {
            return Err(SessionError::Error(format!("Task {} is terminal", task_id.0)));
        }
        let latest = self.latest.get(&resolved.id).copied();
        if let Some(index) = latest
            && !self.documents[index].retire
        {
            if let Some(draft) = &self.documents[index].draft {
                return Ok(draft.clone());
            }
            if matches!(self.documents[index].target, Some(Target::ForkCopy { .. })) {
                return self.acquire_fork_copy(index, doc);
            }
        }
        let skip_load = latest.is_some_and(|index| self.documents[index].retire);
        let (target, draft) = self.acquire(doc, &resolved, seed, skip_load)?;
        self.latest.insert(resolved.id.clone(), self.documents.len());
        self.documents.push(DocEntry {
            address_id: resolved.id,
            definition: Some(doc.clone()),
            draft: Some(draft.clone()),
            target: Some(target),
            retire: false,
        });
        Ok(draft)
    }

    fn acquire(&mut self, doc: &Doc, resolved: &Resolved, seed: Value, skip_load: bool) -> Result<(Target, Draft)> {
        let definition = doc.definition();
        let loaded = if skip_load {
            None
        } else {
            self.inner
                .load(self.line, definition, &resolved.id, &resolved.address)?
        };
        if let Some(loaded) = loaded {
            let draft = {
                let document = lock(&loaded);
                check_record_scope(definition, &document.record.identity)?;
                check_record_version(definition, &document.record.identity, document.stored_version)?;
                document.tracker.begin_change()?
            };
            return Ok((Target::Loaded(loaded), draft));
        }
        match resolved.address.scope {
            DocumentScope::Conversation { conversation_id } => self.require_conversation(conversation_id)?,
            DocumentScope::Task { task_id } => match self.current_task(task_id)? {
                None => return Err(SessionError::Error(format!("Task {} does not exist", task_id.0))),
                Some(task) if task.state.status() == TaskStatus::Terminal => {
                    return Err(SessionError::Error(format!("Task {} is terminal", task_id.0)));
                }
                Some(_) => {}
            },
            DocumentScope::Session => {}
        }
        let value = object((definition.initial)(&seed), &definition.kind)?;
        let id = DocumentId(self.mint()?);
        let tracker = Tracker::new(value);
        let draft = tracker.begin_change()?;
        let record = document_create(definition, &resolved.address, id);
        Ok((
            Target::Created {
                record,
                version: definition.version,
                tracker,
            },
            draft,
        ))
    }

    /// Hydrate a staged fork copy from its exact source for typed access; the copy then writes a creation base.
    fn acquire_fork_copy(&mut self, index: usize, doc: &Doc) -> Result<Draft> {
        let Some(Target::ForkCopy { record, source }) = &self.documents[index].target else {
            unreachable!("checked by the caller");
        };
        let (record, source) = (record.clone(), *source);
        let Some(stored) = self.read(|storage| storage.document(source.id, source.at))? else {
            return Err(SessionError::Error(format!(
                "Fork source document {} cannot be read",
                source.id.0
            )));
        };
        let original = &stored.record.identity;
        if !matches!(original.scope, DocumentScope::Conversation { .. })
            || original.kind != record.kind
            || original.key != record.key
            || original.history != record.history
            || original.fork != record.fork
        {
            return Err(SessionError::Error(format!(
                "Fork source document {} does not match the copied record",
                source.id.0
            )));
        }
        let definition = doc.definition();
        let value = materialize_value(definition, &record, stored.version, &stored.value)?;
        let tracker = Tracker::new(value);
        let draft = tracker.begin_change()?;
        let entry = &mut self.documents[index];
        entry.definition = Some(doc.clone());
        entry.target = Some(Target::Created {
            record,
            version: definition.version,
            tracker,
        });
        entry.draft = Some(draft.clone());
        Ok(draft)
    }

    /// Retire the current incarnation at an address when the transaction commits; an acquired draft's final content
    /// is written first. Retiring an absent address does nothing.
    pub fn retire_doc(&mut self, doc: &Doc, owner: Owner) -> Result<()> {
        self.retire_at(doc, owner, None)
    }

    pub fn retire_member(&mut self, doc: &Doc, owner: Owner, key: &str) -> Result<()> {
        self.retire_at(doc, owner, Some(key))
    }

    fn retire_at(&mut self, doc: &Doc, owner: Owner, key: Option<&str>) -> Result<()> {
        let definition = doc.definition();
        let resolved = resolve_address(definition, owner, key)?;
        if let Some(&index) = self.latest.get(&resolved.id) {
            let entry = &mut self.documents[index];
            if entry.retire {
                return Ok(());
            }
            if entry.draft.is_none()
                && let Some(Target::ForkCopy { record, .. }) = &entry.target
            {
                check_record_scope(definition, record)?;
                entry.retire = true;
                return Ok(());
            }
            if entry.draft.is_some() {
                entry.retire = true;
                return Ok(());
            }
        }
        let cached = lock(&self.inner.documents)
            .get(&resolved.id)
            .map(|loaded| lock(loaded).record.clone());
        let record = match cached {
            Some(record) => Some(record),
            None => {
                let address = resolved.address.clone();
                self.read(|storage| storage.find_document(&address, DocumentPoint::Current))?
            }
        };
        if let Some(record) = &record {
            check_record_scope(definition, &record.identity)?;
        }
        self.latest.insert(resolved.id.clone(), self.documents.len());
        self.documents.push(DocEntry {
            address_id: resolved.id,
            definition: Some(doc.clone()),
            draft: None,
            target: record.map(Target::RetireOnly),
            retire: true,
        });
        Ok(())
    }

    // ─── Settlement ─────────────────────────────────────────────────────────

    /// Revoke every draft without writing anything.
    pub(crate) fn discard(&self) {
        for entry in &self.documents {
            if let Some(draft) = &entry.draft {
                draft.abort();
            }
        }
    }

    /// Prepare every draft (revoking it) and assemble the atomic batch and the adoption plans. Any failure writes
    /// nothing and leaves every committed revision unchanged.
    pub(crate) fn settle(mut self) -> Result<(Vec<StorageWrite>, Vec<Plan>)> {
        let result = self.assemble();
        if result.is_err() {
            self.discard();
        }
        result
    }

    fn assemble(&mut self) -> Result<(Vec<StorageWrite>, Vec<Plan>)> {
        let mut plans = Vec::new();
        for entry in std::mem::take(&mut self.documents) {
            if let Some(plan) = plan_document(entry)? {
                plans.push(plan);
            }
        }
        self.reject_fork_source_writes(&plans)?;
        self.validate_owners()?;
        let replaced: Vec<(TaskId, ConversationId)> = self
            .tasks
            .iter()
            .filter_map(|(id, task)| match &task.write {
                Some((TaskWrite::Replace, record)) => Some((*id, record.conversation_id)),
                _ => None,
            })
            .collect();
        for (id, conversation_id) in replaced {
            let Some(committed) = self.committed_task(id)? else {
                return Err(SessionError::Error(format!("Task {} does not exist", id.0)));
            };
            if committed.state.status() == TaskStatus::Terminal {
                return Err(SessionError::Error(format!("Task {} is already terminal", id.0)));
            }
            if committed.conversation_id != conversation_id {
                return Err(SessionError::Error(format!(
                    "Task {} cannot change conversations",
                    id.0
                )));
            }
        }

        // Terminal settlement retires every task document, including ones created by this transaction.
        let terminal: Vec<(TaskId, bool)> = self
            .tasks
            .values()
            .filter_map(|task| match &task.write {
                Some((kind, record)) if record.state.status() == TaskStatus::Terminal => {
                    Some((record.id, *kind == TaskWrite::Create))
                }
                _ => None,
            })
            .collect();
        if !terminal.is_empty() {
            let mut retiring = HashSet::new();
            for plan in &mut plans {
                if let DocumentScope::Task { task_id } = plan.record.identity().scope
                    && terminal.iter().any(|(id, _)| *id == task_id)
                {
                    plan.retire = true;
                    retiring.insert(plan.record.identity().id);
                }
            }
            for (task_id, created) in terminal {
                if created {
                    continue;
                }
                let query = DocumentQuery {
                    scope: DocumentScope::Task { task_id },
                    at: DocumentPoint::Current,
                    kind: None,
                };
                let mut cursor: Option<Cursor> = None;
                loop {
                    let page =
                        self.read(|storage| storage.scan_documents(&query, INTERNAL_SCAN_PAGE_SIZE, cursor.as_ref()))?;
                    for record in page.items {
                        if !retiring.insert(record.identity.id) {
                            continue;
                        }
                        plans.push(Plan {
                            address_id: address_id(&address_of(&record.identity)),
                            record: PlanRecord::Committed(record),
                            retire: true,
                            content: None,
                            change: None,
                            conversation_id: None,
                        });
                    }
                    match page.next {
                        Some(next) => cursor = Some(next),
                        None => break,
                    }
                }
            }
        }

        // Resolve publication ownership before Storage admission so adoption needs no reads.
        for plan in &mut plans {
            if !plan.publishes() {
                continue;
            }
            match plan.record.identity().scope {
                DocumentScope::Conversation { conversation_id } => plan.conversation_id = Some(conversation_id),
                DocumentScope::Task { task_id } => {
                    let known = self.tasks.get(&task_id).and_then(|task| task.publication_conversation);
                    let conversation = match known {
                        Some(conversation) => Some(conversation),
                        None => self.current_task(task_id)?.map(|task| task.conversation_id),
                    };
                    if let Some(conversation) = conversation {
                        self.tasks.entry(task_id).or_default().publication_conversation = Some(conversation);
                    }
                    plan.conversation_id = conversation;
                }
                DocumentScope::Session => {}
            }
        }

        for (id, change) in std::mem::take(&mut self.submission_changes) {
            let current = match self.submissions.get(&id) {
                Some(current) => Some(current.clone()),
                None => self.read(|storage| storage.submission(id))?,
            };
            let Some(current) = current else {
                return Err(SessionError::Error(format!("Submission {} does not exist", id.0)));
            };
            let next = apply_submission_change(&current, &change)?;
            if next != current {
                self.submissions.insert(id, next);
            }
        }

        let mut writes = std::mem::take(&mut self.writes);
        writes.extend(
            self.submissions
                .values()
                .map(|value| StorageWrite::Submission { value: value.clone() }),
        );
        for task in self.tasks.values() {
            if let Some((_, record)) = &task.write {
                writes.push(StorageWrite::Task { value: record.clone() });
            }
        }
        for plan in &mut plans {
            // Checkpoint predicates run last, after every validation.
            if let (
                Some(StorageWrite::DocumentChange {
                    id,
                    content: DocumentContent::Delta { .. },
                }),
                Some(PlanChange {
                    tracker: PlanTracker::Loaded(loaded),
                    prepared,
                    version,
                    definition: Some(doc),
                }),
            ) = (&plan.content, &plan.change)
                && let Some(predicate) = &doc.definition().checkpoint_when
            {
                let info = crate::documents::CheckpointInfo {
                    deltas_since_base: lock(loaded).deltas_since_base,
                };
                if predicate(doc.definition(), &prepared.value, &prepared.ops, info).map_err(SessionError::Error)? {
                    plan.content = Some(StorageWrite::DocumentChange {
                        id: *id,
                        content: DocumentContent::Base {
                            version: *version,
                            value: object_of(&prepared.value),
                        },
                    });
                }
            }
            if let Some(content) = &plan.content {
                writes.push(content.clone());
            }
            if plan.retire {
                writes.push(StorageWrite::DocumentRetire {
                    id: plan.record.identity().id,
                });
            }
        }
        Ok((writes, plans))
    }

    /// New owned work needs a live owner, judged on its final candidate: not `completing`, terminal, or abort-marked.
    fn validate_owners(&mut self) -> Result<()> {
        let mut owners: Vec<(&'static str, TaskId)> = Vec::new();
        for write in &self.writes {
            if let StorageWrite::Conversation { value } = write
                && let Some(owner) = value.owner
            {
                owners.push(("Conversation owner task", owner.task_id));
            }
        }
        for task in self.tasks.values() {
            if let Some((TaskWrite::Create, record)) = &task.write
                && let Some(owner) = record.owner
            {
                owners.push(("Task owner", owner));
            }
        }
        for (what, task_id) in owners {
            let Some(task) = self.current_task(task_id)? else {
                return Err(SessionError::Error(format!("{what} {} does not exist", task_id.0)));
            };
            match task.state.status() {
                TaskStatus::Terminal => return Err(SessionError::Error(format!("{what} {} is terminal", task_id.0))),
                TaskStatus::Completing => {
                    return Err(SessionError::Error(format!("{what} {} is completing", task_id.0)));
                }
                _ => {}
            }
            if task.abort_requested {
                return Err(SessionError::Error(format!("{what} {} is abort-marked", task_id.0)));
            }
        }
        Ok(())
    }

    fn reject_fork_source_writes(&self, plans: &[Plan]) -> Result<()> {
        for plan in plans {
            if plan.content.is_none() && !plan.retire {
                continue;
            }
            let record = plan.record.identity();
            if self.fork_source_documents.contains(&record.id) {
                return Err(SessionError::Error(format!(
                    "Cannot change fork source document {} in the fork transaction",
                    record.id.0
                )));
            }
            if let DocumentScope::Conversation { conversation_id } = record.scope
                && self.fork_source_conversations.contains(&conversation_id)
                && record.fork == Some(DocumentFork::Current)
            {
                return Err(SessionError::Error(format!(
                    "Cannot fork conversation {} while changing its current-policy documents",
                    conversation_id.0
                )));
            }
        }
        Ok(())
    }

    fn require_conversation(&mut self, id: ConversationId) -> Result<()> {
        if self.created_conversations.contains(&id) || self.read(|storage| storage.conversation(id))?.is_some() {
            Ok(())
        } else {
            Err(SessionError::Error(format!("Conversation {} does not exist", id.0)))
        }
    }

    /// Latest candidate task record, falling back to committed state; not a caller table read.
    fn current_task(&mut self, id: TaskId) -> Result<Option<TaskRecord>> {
        if let Some((_, record)) = self.tasks.get(&id).and_then(|task| task.write.as_ref()) {
            return Ok(Some(record.clone()));
        }
        self.committed_task(id)
    }

    fn committed_task(&mut self, id: TaskId) -> Result<Option<TaskRecord>> {
        if let Some(Some(committed)) = self.tasks.get(&id).map(|task| &task.committed) {
            return Ok(committed.clone());
        }
        let committed = self.read(|storage| storage.task(id))?;
        self.tasks.entry(id).or_default().committed = Some(committed.clone());
        Ok(committed)
    }
}

/// Plan of one staged document: its record, content write, and prepared change. Retirement is decided later.
fn plan_document(entry: DocEntry) -> Result<Option<Plan>> {
    let DocEntry {
        address_id,
        definition,
        draft,
        target,
        retire,
        ..
    } = entry;
    let Some(target) = target else {
        return Ok(None);
    };
    let prepared = match &draft {
        Some(draft) => Some(draft.prepare()?),
        None => None,
    };
    let plan = |record, content, change| Plan {
        address_id: address_id.clone(),
        record,
        retire,
        content,
        change,
        conversation_id: None,
    };
    Ok(Some(match target {
        Target::Created {
            record,
            version,
            tracker,
        } => {
            let prepared = prepared.expect("a created incarnation has a draft");
            let content = StorageWrite::DocumentCreate {
                record: record.clone(),
                content: DocumentContent::Base {
                    version,
                    value: object_of(&prepared.value),
                },
            };
            plan(
                PlanRecord::New(record),
                Some(content),
                Some(PlanChange {
                    tracker: PlanTracker::Created(tracker),
                    prepared,
                    version,
                    definition,
                }),
            )
        }
        Target::ForkCopy { record, source } => {
            let content = StorageWrite::DocumentCopy {
                record: record.clone(),
                source,
            };
            plan(PlanRecord::New(record), Some(content), None)
        }
        Target::RetireOnly(record) => plan(PlanRecord::Committed(record), None, None),
        Target::Loaded(loaded) => {
            let prepared = prepared.expect("a loaded incarnation has a draft");
            let doc = definition.expect("a loaded incarnation has a definition");
            let version = doc.definition().version;
            let (record, stored_version) = {
                let document = lock(&loaded);
                (document.record.clone(), document.stored_version)
            };
            let id = record.identity.id;
            // A version change stores a base even without operations; otherwise only a change stores a delta.
            let content = if stored_version < version {
                Some(DocumentContent::Base {
                    version,
                    value: object_of(&prepared.value),
                })
            } else if !prepared.ops.is_empty() {
                Some(DocumentContent::Delta {
                    version,
                    ops: prepared.ops.clone(),
                })
            } else {
                None
            };
            plan(
                PlanRecord::Committed(record),
                content.map(|content| StorageWrite::DocumentChange { id, content }),
                Some(PlanChange {
                    tracker: PlanTracker::Loaded(loaded),
                    prepared,
                    version,
                    definition: Some(doc),
                }),
            )
        }
    }))
}

/// Adopt every prepared change after Storage acknowledged `seq`, install new incarnations, evict retired ones, and
/// describe the document publication.
pub(crate) fn adopt(inner: &Inner, plans: Vec<Plan>, seq: Seq) -> Result<Vec<CommitChange>> {
    let mut publications = Vec::new();
    for plan in plans {
        let publishes = plan.publishes();
        let committed = matches!(plan.record, PlanRecord::Committed(_));
        let mut record = match plan.record {
            PlanRecord::Committed(record) => record,
            PlanRecord::New(identity) => DocumentRecord {
                identity,
                created_at: seq,
                retired_at: None,
            },
        };
        if plan.retire {
            record.retired_at = Some(seq);
        }
        let mut published = None;
        if let Some(change) = plan.change {
            let PlanChange {
                tracker,
                prepared,
                version,
                ..
            } = change;
            match tracker {
                PlanTracker::Created(mut tracker) => {
                    // A new incarnation is adopted unless it retires in the same commit.
                    if !plan.retire {
                        tracker.adopt(&prepared)?;
                        let loaded = LoadedDocument {
                            record: record.clone(),
                            stored_version: version,
                            value_version: version,
                            deltas_since_base: 0,
                            tracker,
                        };
                        lock(&inner.documents).insert(plan.address_id.clone(), Arc::new(Mutex::new(loaded)));
                    }
                    published = Some((version, prepared.value, Vec::new()));
                }
                PlanTracker::Loaded(loaded) => {
                    let mut document = lock(&loaded);
                    if !prepared.ops.is_empty() {
                        document.tracker.adopt(&prepared)?;
                    }
                    if document.stored_version < version {
                        document.stored_version = version;
                    }
                    if let Some(StorageWrite::DocumentChange { content, .. }) = &plan.content {
                        match content {
                            DocumentContent::Base { .. } => document.deltas_since_base = 0,
                            DocumentContent::Delta { .. } => document.deltas_since_base += 1,
                        }
                    }
                    published = Some((version, prepared.value, prepared.ops));
                }
            }
        }
        if plan.retire {
            if committed {
                let mut documents = lock(&inner.documents);
                if documents
                    .get(&plan.address_id)
                    .is_some_and(|loaded| lock(loaded).record.identity.id == record.identity.id)
                {
                    documents.remove(&plan.address_id);
                }
            }
            publications.push(CommitChange::Document {
                record,
                conversation_id: plan.conversation_id,
                version: None,
                value: None,
                ops: Vec::new(),
            });
        } else if let Some(StorageWrite::DocumentCopy { source, .. }) = plan.content {
            let conversation_id = plan.conversation_id.expect("copies are conversation documents");
            publications.push(CommitChange::DocumentCopy {
                record,
                conversation_id,
                source,
            });
        } else if let Some((version, value, ops)) = published
            && publishes
        {
            publications.push(CommitChange::Document {
                record,
                conversation_id: plan.conversation_id,
                version: Some(version),
                value: Some(value),
                ops,
            });
        }
    }
    Ok(publications)
}
