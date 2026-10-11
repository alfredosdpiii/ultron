//! Durable record, write, and query shapes. Field names and tags serialize exactly as upstream `types.ts`.

use chord_delta::Op;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// JSON object used as the root of every durable document.
pub type JsonObject = Map<String, Value>;

/// Backend-owned continuation state that callers only round-trip to the same scan.
pub type Cursor = JsonObject;

macro_rules! id_types {
    ($($(#[$doc:meta])* $name:ident),* $(,)?) => {$(
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);
    )*};
}

id_types!(
    ConversationId,
    EntryId,
    TaskId,
    SubmissionId,
    DocumentId,
    /// Strictly increasing sequence of one atomic storage commit; gaps are permitted.
    Seq,
);

/// The root conversation always uses this reserved ID.
pub const ROOT_CONVERSATION_ID: ConversationId = ConversationId(1);

/// Largest ID a JavaScript number holds exactly; minting stops past it so IDs stay interchangeable with upstream.
pub const MAX_SAFE_ID: u64 = (1 << 53) - 1;

/// `Some(value)` for a present field, `null` included: `data: null` is a stored value, not an absent one.
fn present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConversationRecord {
    pub id: ConversationId,
    /// Fork source and inclusive parent entry through which history is inherited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<ConversationParent>,
    /// Creator edge used for attribution, subtree abort, and subtree idle waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<ConversationOwner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConversationParent {
    pub conversation_id: ConversationId,
    pub at: EntryId,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConversationOwner {
    pub conversation_id: ConversationId,
    pub task_id: TaskId,
}

/// Immutable transcript event. `model` and `edits` stay JSON until the message model is ported (phase 4).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EntryRecord {
    pub id: EntryId,
    pub conversation_id: ConversationId,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Vec<Value>>,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<EntryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edits: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by_task_id: Option<TaskId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskStatus {
    Pending,
    Running,
    Waiting,
    Completing,
    Terminal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum JoinPolicy {
    FailFast,
    AllSettled,
}

/// Complete durable execution state of a task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase", deny_unknown_fields)]
pub enum TaskState {
    Pending {
        checkpoint: Value,
    },
    Running {
        checkpoint: Value,
    },
    Waiting {
        checkpoint: Value,
        on: Vec<TaskId>,
        policy: JoinPolicy,
    },
    Completing {
        outcome: TaskOutcome,
    },
    Terminal {
        outcome: TaskOutcome,
    },
}

impl TaskState {
    pub fn status(&self) -> TaskStatus {
        match self {
            TaskState::Pending { .. } => TaskStatus::Pending,
            TaskState::Running { .. } => TaskStatus::Running,
            TaskState::Waiting { .. } => TaskStatus::Waiting,
            TaskState::Completing { .. } => TaskStatus::Completing,
            TaskState::Terminal { .. } => TaskStatus::Terminal,
        }
    }
}

/// Durable reason and optional result recorded when a task becomes terminal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase", deny_unknown_fields)]
pub enum TaskOutcome {
    Completed {
        result: Value,
    },
    Failed {
        error: TaskOutcomeError,
        #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    Aborted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    Orphaned {
        reason: String,
    },
    Faulted {
        error: TaskOutcomeError,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskOutcomeError {
    pub message: String,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AbortReason {
    Restart,
}

/// Complete replacement record for one durable task state machine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskRecord {
    pub id: TaskId,
    pub conversation_id: ConversationId,
    /// Registered task definition name.
    pub kind: String,
    pub version: u64,
    pub input: Value,
    /// Owning task of a child task; absent for a task its conversation owns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<TaskId>,
    pub background: bool,
    pub abort_requested: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abort_reason: Option<AbortReason>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub abandon_on_restart: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    pub state: TaskState,
    /// Small first-writer-wins values retained while the task can run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memos: Option<JsonObject>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubmissionType {
    Input,
    Write,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubmissionStatus {
    Queued,
    Placed,
    Done,
    Unanswered,
}

/// Durable lifecycle of one admitted user input or passive entry write. The Session supplies valid transitions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubmissionRecord {
    pub id: SubmissionId,
    pub conversation_id: ConversationId,
    /// Host-provided deduplication key, scoped to the conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(rename = "type")]
    pub kind: SubmissionType,
    pub status: SubmissionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<EntryId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

/// Who owns a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum DocumentScope {
    Session,
    Conversation { conversation_id: ConversationId },
    Task { task_id: TaskId },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DocumentHistory {
    Latest,
    Rewindable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DocumentFork {
    AsOf,
    Current,
    Initial,
}

/// Fields supplied when storage creates and stamps a new [`DocumentRecord`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DocumentCreate {
    /// Unique incarnation ID; never reused when the same logical document is recreated.
    pub id: DocumentId,
    pub kind: String,
    /// Family member key; absent for singleton documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub scope: DocumentScope,
    /// Conversation documents only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<DocumentHistory>,
    /// Conversation documents only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork: Option<DocumentFork>,
}

impl DocumentCreate {
    /// Session and task documents, and `latest` conversation documents, keep no history.
    pub fn is_current_only(&self) -> bool {
        !matches!(self.scope, DocumentScope::Conversation { .. }) || self.history == Some(DocumentHistory::Latest)
    }
}

/// Persisted lifecycle record of one create-to-retire document incarnation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentRecord {
    #[serde(flatten)]
    pub identity: DocumentCreate,
    /// Commit that created the incarnation.
    pub created_at: Seq,
    /// Commit that retired the incarnation; absent while it is current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<Seq>,
}

impl DocumentRecord {
    pub fn is_alive_at(&self, at: DocumentPoint) -> bool {
        match at {
            DocumentPoint::Current => self.retired_at.is_none(),
            DocumentPoint::At(seq) => self.created_at <= seq && self.retired_at.is_none_or(|retired| seq < retired),
        }
    }
}

/// Complete checkpoint or Chord operation batch.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum DocumentContent {
    Base {
        version: u64,
        value: JsonObject,
    },
    Delta {
        version: u64,
        #[serde(serialize_with = "ops_out", deserialize_with = "ops_in")]
        ops: Vec<Op>,
    },
}

impl DocumentContent {
    pub fn version(&self) -> u64 {
        match self {
            DocumentContent::Base { version, .. } | DocumentContent::Delta { version, .. } => *version,
        }
    }
}

fn ops_out<S: Serializer>(ops: &[Op], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(ops.iter().map(Op::to_json))
}

fn ops_in<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<Op>, D::Error> {
    Vec::<Value>::deserialize(deserializer)?
        .iter()
        .map(|op| Op::from_json(op).map_err(serde::de::Error::custom))
        .collect()
}

/// Current state or one historical commit, for document membership and content reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocumentPoint {
    Current,
    At(Seq),
}

impl Serialize for DocumentPoint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            DocumentPoint::Current => serializer.serialize_str("current"),
            DocumentPoint::At(seq) => seq.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for DocumentPoint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::String(text) if text == "current" => Ok(DocumentPoint::Current),
            Value::Number(number) if number.as_u64().is_some() => {
                Ok(DocumentPoint::At(Seq(number.as_u64().unwrap_or(0))))
            }
            other => Err(serde::de::Error::custom(format!("invalid document point: {other}"))),
        }
    }
}

/// Exact persisted source of a definition-free document copy.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentCopySource {
    pub id: DocumentId,
    pub at: DocumentPoint,
}

/// One record or document mutation in an atomic storage commit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
#[expect(
    clippy::large_enum_variant,
    reason = "writes are short-lived batch items; boxing adds an allocation each"
)]
pub enum StorageWrite {
    #[serde(rename = "conversation")]
    Conversation { value: ConversationRecord },
    #[serde(rename = "entry")]
    Entry { value: EntryRecord },
    #[serde(rename = "task")]
    Task { value: TaskRecord },
    #[serde(rename = "submission")]
    Submission { value: SubmissionRecord },
    /// `content` must be a base.
    #[serde(rename = "document.create")]
    DocumentCreate {
        record: DocumentCreate,
        content: DocumentContent,
    },
    #[serde(rename = "document.copy")]
    DocumentCopy {
        record: DocumentCreate,
        source: DocumentCopySource,
    },
    #[serde(rename = "document.change")]
    DocumentChange { id: DocumentId, content: DocumentContent },
    #[serde(rename = "document.retire")]
    DocumentRetire { id: DocumentId },
}

/// ID order of a scan: `Ascending` is oldest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScanOrder {
    Ascending,
    Descending,
}

impl ScanOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            ScanOrder::Ascending => "ascending",
            ScanOrder::Descending => "descending",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConversationQuery {
    pub owner_conversation_id: Option<ConversationId>,
    pub owner_task_id: Option<TaskId>,
    /// Default ascending; with a cursor, the cursor's order.
    pub order: Option<ScanOrder>,
}

/// Inclusive ID bounds for a scan of one conversation's fork-aware history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EntryQuery {
    pub conversation_id: ConversationId,
    #[serde(default)]
    pub min_entry_id: Option<EntryId>,
    #[serde(default)]
    pub max_entry_id: Option<EntryId>,
    /// Default descending; with a cursor, the cursor's order.
    #[serde(default)]
    pub order: Option<ScanOrder>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskQuery {
    pub conversation_id: Option<ConversationId>,
    pub kind: Option<String>,
    pub status: Option<TaskStatus>,
    pub abort_requested: Option<bool>,
    pub background: Option<bool>,
    pub order: Option<ScanOrder>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SubmissionQuery {
    pub conversation_id: Option<ConversationId>,
    pub status: Option<SubmissionStatus>,
    pub order: Option<ScanOrder>,
}

/// Exact logical identity of a singleton (`key` absent) or one keyed family member.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentAddress {
    pub kind: String,
    pub scope: DocumentScope,
    #[serde(default)]
    pub key: Option<String>,
}

/// Incarnations alive in one exact scope at one point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentQuery {
    pub scope: DocumentScope,
    pub at: DocumentPoint,
    #[serde(default)]
    pub kind: Option<String>,
}

/// One ordered scan result and its optional continuation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<Cursor>,
}

/// An entry and the sequence of the commit that persisted it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredEntry {
    pub entry: EntryRecord,
    pub commit_seq: Seq,
}

/// Materialized value and stored definition version at a selected point.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredDocument {
    pub record: DocumentRecord,
    pub version: u64,
    pub value: JsonObject,
    /// Deltas replayed after the selected base to materialize `value`.
    pub deltas_since_base: usize,
}
