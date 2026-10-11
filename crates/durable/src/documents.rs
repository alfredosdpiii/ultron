//! Document definitions, logical addresses, and the checks typed access applies to stored incarnations.
//!
//! Port of `packages/durable/src/documents.ts`. Upstream overloads pick the owner and family key from positional
//! arguments; here [`Owner`] and an explicit key say the same, and a definition is shared as a [`Doc`] token.

use crate::session::SessionError;
use crate::{
    DocumentAddress, DocumentCreate, DocumentFork, DocumentHistory, DocumentId, DocumentScope, JsonObject,
    StoredDocument,
};
use chord_delta::Op;
use serde_json::{Value, json};
use std::fmt;
use std::sync::Arc;

/// Ownership and lifetime of a document; only conversation documents declare history and fork behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocSemantics {
    Session,
    Conversation {
        history: DocumentHistory,
        fork: DocumentFork,
    },
    Task,
}

/// Stored replay state supplied to a document's checkpoint predicate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointInfo {
    /// Deltas already stored after the newest base, excluding the change being evaluated.
    pub deltas_since_base: usize,
}

type Initial = dyn Fn(&Value) -> Value + Send + Sync;
type Migrate = dyn Fn(&JsonObject, u64) -> Result<Value, String> + Send + Sync;
type CheckpointWhen = dyn Fn(&DocDefinition, &Value, &[Op], CheckpointInfo) -> Result<bool, String> + Send + Sync;

/// A document definition. `initial` receives the family seed (`null` for a singleton) and runs only when the
/// document is absent; `migrate` converts a stored value of an older version; `checkpoint_when` returning true
/// stores an ordinary change as a complete base instead of a delta.
pub struct DocDefinition {
    /// Stable persisted kind; part of the public protocol.
    pub kind: String,
    /// Positive integer version of the stored value shape.
    pub version: u64,
    pub semantics: DocSemantics,
    pub family: bool,
    pub initial: Box<Initial>,
    pub migrate: Option<Box<Migrate>>,
    pub checkpoint_when: Option<Box<CheckpointWhen>>,
}

impl fmt::Debug for DocDefinition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DocDefinition")
            .field("kind", &self.kind)
            .field("version", &self.version)
            .field("semantics", &self.semantics)
            .field("family", &self.family)
            .finish_non_exhaustive()
    }
}

impl DocDefinition {
    pub fn new(
        kind: &str,
        version: u64,
        semantics: DocSemantics,
        initial: impl Fn(&Value) -> Value + Send + Sync + 'static,
    ) -> DocDefinition {
        DocDefinition {
            kind: kind.to_owned(),
            version,
            semantics,
            family: false,
            initial: Box::new(initial),
            migrate: None,
            checkpoint_when: None,
        }
    }

    /// A keyed family: `initial` receives the seed of the first acquisition of each member.
    pub fn family(mut self) -> DocDefinition {
        self.family = true;
        self
    }

    pub fn migrate(
        mut self,
        migrate: impl Fn(&JsonObject, u64) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Self {
        self.migrate = Some(Box::new(migrate));
        self
    }

    pub fn checkpoint_when(
        mut self,
        predicate: impl Fn(&DocDefinition, &Value, &[Op], CheckpointInfo) -> Result<bool, String> + Send + Sync + 'static,
    ) -> Self {
        self.checkpoint_when = Some(Box::new(predicate));
        self
    }

    /// Validate and share the definition (upstream `defineDoc`/`defineDocFamily`).
    pub fn define(self) -> Result<Doc, SessionError> {
        if self.version < 1 || self.version > crate::MAX_SAFE_ID {
            return Err(SessionError::Type(format!(
                "Document {} version must be a positive integer",
                self.kind
            )));
        }
        Ok(Doc(Arc::new(self)))
    }

    fn scope_name(&self) -> &'static str {
        match self.semantics {
            DocSemantics::Session => "session",
            DocSemantics::Conversation { .. } => "conversation",
            DocSemantics::Task => "task",
        }
    }
}

/// A shared document definition passed explicitly to typed access.
#[derive(Clone, Debug)]
pub struct Doc(pub(crate) Arc<DocDefinition>);

impl Doc {
    pub fn definition(&self) -> &DocDefinition {
        &self.0
    }
}

/// The owner a document address names; it must match the definition's scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    Session,
    Conversation(crate::ConversationId),
    Task(crate::TaskId),
}

/// A logical address and its string identity for maps.
pub(crate) struct Resolved {
    pub address: DocumentAddress,
    pub id: String,
}

/// Resolve an owner and optional family key against a definition.
pub(crate) fn resolve_address(
    definition: &DocDefinition,
    owner: Owner,
    key: Option<&str>,
) -> Result<Resolved, SessionError> {
    let scope = match (definition.semantics, owner) {
        (DocSemantics::Session, Owner::Session) => DocumentScope::Session,
        (DocSemantics::Conversation { .. }, Owner::Conversation(conversation_id)) => {
            DocumentScope::Conversation { conversation_id }
        }
        (DocSemantics::Task, Owner::Task(task_id)) => DocumentScope::Task { task_id },
        _ => {
            return Err(SessionError::Type(format!(
                "Document {} requires a {} ID",
                definition.kind,
                definition.scope_name()
            )));
        }
    };
    match (definition.family, key) {
        (true, None) => {
            return Err(SessionError::Type(format!(
                "Document {} requires a family key",
                definition.kind
            )));
        }
        (false, Some(_)) => {
            return Err(SessionError::Type(format!(
                "Document {} is not a family",
                definition.kind
            )));
        }
        _ => {}
    }
    let address = DocumentAddress {
        kind: definition.kind.clone(),
        scope,
        key: key.map(str::to_owned),
    };
    let id = address_id(&address);
    Ok(Resolved { address, id })
}

/// Stable string identity of one logical address (upstream `addressId`).
pub(crate) fn address_id(address: &DocumentAddress) -> String {
    let (scope, owner) = match address.scope {
        DocumentScope::Session => ("session", Value::Null),
        DocumentScope::Conversation { conversation_id } => ("conversation", json!(conversation_id.0)),
        DocumentScope::Task { task_id } => ("task", json!(task_id.0)),
    };
    json!([address.kind, scope, owner, address.key]).to_string()
}

pub(crate) fn address_of(record: &DocumentCreate) -> DocumentAddress {
    DocumentAddress {
        kind: record.kind.clone(),
        scope: record.scope,
        key: record.key.clone(),
    }
}

/// The storage create record of a new incarnation at an address.
pub(crate) fn document_create(definition: &DocDefinition, address: &DocumentAddress, id: DocumentId) -> DocumentCreate {
    let (history, fork) = match definition.semantics {
        DocSemantics::Conversation { history, fork } => (Some(history), Some(fork)),
        _ => (None, None),
    };
    DocumentCreate {
        id,
        kind: address.kind.clone(),
        key: address.key.clone(),
        scope: address.scope,
        history,
        fork,
    }
}

/// Reject typed access whose definition disagrees with the persisted scope, history, or fork semantics.
pub(crate) fn check_record_scope(definition: &DocDefinition, record: &DocumentCreate) -> Result<(), SessionError> {
    let matches = match (definition.semantics, record.scope) {
        (DocSemantics::Session, DocumentScope::Session) | (DocSemantics::Task, DocumentScope::Task { .. }) => true,
        (DocSemantics::Conversation { history, fork }, DocumentScope::Conversation { .. }) => {
            record.history == Some(history) && record.fork == Some(fork)
        }
        _ => false,
    };
    if matches {
        Ok(())
    } else {
        Err(SessionError::Type(format!(
            "Document {} ({}) does not match the supplied definition semantics",
            record.id.0, record.kind
        )))
    }
}

/// Reject typed access to a stored version the definition cannot use.
pub(crate) fn check_record_version(
    definition: &DocDefinition,
    record: &DocumentCreate,
    version: u64,
) -> Result<(), SessionError> {
    if version > definition.version {
        return Err(SessionError::Error(format!(
            "Document {} ({}) has newer version {} than {}",
            record.id.0, record.kind, version, definition.version
        )));
    }
    if version < definition.version && definition.migrate.is_none() {
        return Err(SessionError::Error(format!(
            "Document {} ({}) requires migration from version {}",
            record.id.0, record.kind, version
        )));
    }
    Ok(())
}

/// Validate and materialize a stored value for typed access.
pub(crate) fn materialize_document(definition: &DocDefinition, stored: &StoredDocument) -> Result<Value, SessionError> {
    materialize_value(definition, &stored.record.identity, stored.version, &stored.value)
}

/// Validate and materialize one stored value, migrating an older version.
pub(crate) fn materialize_value(
    definition: &DocDefinition,
    record: &DocumentCreate,
    version: u64,
    value: &JsonObject,
) -> Result<Value, SessionError> {
    check_record_scope(definition, record)?;
    check_record_version(definition, record, version)?;
    if version == definition.version {
        return Ok(Value::Object(value.clone()));
    }
    let migrate = definition.migrate.as_ref().expect("checked by check_record_version");
    object(migrate(value, version).map_err(SessionError::Error)?, &definition.kind)
}

/// A document value must be a JSON object.
pub(crate) fn object(value: Value, kind: &str) -> Result<Value, SessionError> {
    if value.is_object() {
        Ok(value)
    } else {
        Err(SessionError::Type(format!(
            "Document {kind} value must be a JSON object"
        )))
    }
}
