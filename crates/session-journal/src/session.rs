//! `session.ts` (`StorageBackedSession`) over one [`JsonlStorage`]: values, names, labels, branches and appends.
//! One process owns an open session; `&mut self` serializes its mutations (the TypeScript mutation line).

use crate::context;
use crate::fork::{ForkInput, ForkOptions};
use crate::repo::{Metadata, SessionRepo};
use crate::state::{CommitResult, State, Write};
use crate::storage::JsonlStorage;
use crate::{SharedClock, lock};
use serde_json::{Map, Value};

pub const SESSION_NAME: &str = "pi.session.name";
pub const ENTRY_LABEL: &str = "pi.entry.label";
pub const BRANCH_TIP: &str = "pi.branch.tip";
pub const LANE_CONFIG: &str = "pi.lane.config";
pub const LANE_STATE: &str = "pi.lane.state";

/// An open session.
pub struct Session {
    metadata: Metadata,
    storage: JsonlStorage,
    clock: SharedClock,
}

impl Session {
    pub(crate) fn new(metadata: Metadata, storage: JsonlStorage, clock: SharedClock) -> Self {
        Session {
            metadata,
            storage,
            clock,
        }
    }

    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    pub fn id(&self) -> &str {
        &self.metadata.id
    }

    pub fn storage(&self) -> &JsonlStorage {
        &self.storage
    }

    pub fn state(&self) -> &State {
        self.storage.state()
    }

    pub fn clock(&self) -> SharedClock {
        self.clock.clone()
    }

    /// The stored value at `namespace`/`key`.
    pub fn value(&self, namespace: &str, key: &str) -> Option<&Value> {
        self.state().value(namespace, key).map(|v| &v.value)
    }

    /// One transaction (a pending assistant message is refused, as `SessionPendingAssistantMessageError`).
    pub fn commit(&mut self, writes: &[Write]) -> Result<CommitResult, String> {
        for write in writes {
            if let Write::Entry(entry) = write
                && entry.get("type").and_then(Value::as_str) == Some("message")
                && entry["message"]["role"] == "assistant"
                && entry["message"]["stopReason"] == "pending"
            {
                return Err("Cannot persist a pending assistant message".into());
            }
        }
        self.storage.commit(writes, &mut *lock(&self.clock))
    }

    /// `getName`.
    pub fn name(&self) -> Option<String> {
        self.value(SESSION_NAME, "").and_then(Value::as_str).map(str::to_string)
    }

    /// `setName`: a value, or its deletion for `None`.
    pub fn set_name(&mut self, name: Option<&str>) -> Result<(), String> {
        let write = match name {
            Some(name) => Write::set(SESSION_NAME, "", name.into()),
            None => Write::delete(SESSION_NAME, ""),
        };
        self.commit(&[write]).map(|_| ())
    }

    /// `getLabel`.
    pub fn label(&self, target: &str) -> Option<String> {
        self.value(ENTRY_LABEL, target)
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// `setLabel`.
    pub fn set_label(&mut self, target: &str, label: Option<&str>) -> Result<(), String> {
        let write = match label {
            Some(label) => Write::set(ENTRY_LABEL, target, label.into()),
            None => Write::delete(ENTRY_LABEL, target),
        };
        self.commit(&[write]).map(|_| ())
    }

    /// The tip of `branch`: `None` for an unknown branch, `Some(None)` for an empty one.
    pub fn branch_tip(&self, branch: &str) -> Option<Option<String>> {
        self.value(BRANCH_TIP, branch).map(|v| v.as_str().map(str::to_string))
    }

    /// `appendToBranch` for a message: the entry and the new tip in one transaction; returns the entry id.
    pub fn append_message(&mut self, branch: &str, message: Value) -> Result<String, String> {
        let tip = self
            .branch_tip(branch)
            .ok_or_else(|| format!("Unknown branch: {branch}"))?;
        let id = lock(&self.clock).next_id();
        let mut entry = Map::new();
        entry.insert("id".into(), id.clone().into());
        entry.insert("parentId".into(), tip.map_or(Value::Null, Value::from));
        entry.insert("type".into(), "message".into());
        entry.insert("message".into(), message);
        self.commit(&[Write::Entry(entry), Write::set(BRANCH_TIP, branch, id.clone().into())])?;
        Ok(id)
    }

    /// `appendCustomEntry`: a custom entry (with `data` when given) and the new tip.
    pub fn append_custom(&mut self, branch: &str, custom_type: &str, data: Option<Value>) -> Result<String, String> {
        let tip = self
            .branch_tip(branch)
            .ok_or_else(|| format!("Unknown branch: {branch}"))?;
        let id = lock(&self.clock).next_id();
        let mut entry = Map::new();
        entry.insert("id".into(), id.clone().into());
        entry.insert("parentId".into(), tip.map_or(Value::Null, Value::from));
        entry.insert("type".into(), "custom".into());
        entry.insert("customType".into(), custom_type.into());
        if let Some(data) = data {
            entry.insert("data".into(), data);
        }
        self.commit(&[Write::Entry(entry), Write::set(BRANCH_TIP, branch, id.clone().into())])?;
        Ok(id)
    }

    /// The entries from the root to `branch`'s tip, oldest first.
    pub fn branch_entries(&self, branch: &str) -> Result<Vec<Value>, String> {
        let tip = self.branch_tip(branch).flatten();
        self.state().branch_path(tip.as_deref())
    }

    /// `buildSessionContext` of `branch`: the messages the model sees (no custom-entry projectors).
    pub fn context_messages(&self, branch: &str) -> Result<Vec<Value>, String> {
        Ok(context::build_session_context(&self.branch_entries(branch)?))
    }

    /// Fork this open session at its current sequence boundary.
    pub fn fork_into(&self, repo: &SessionRepo, options: &ForkOptions, id: Option<&str>) -> Result<Session, String> {
        if self.storage.is_legacy_v3() {
            return Err("Cannot fork an open legacy v3 JSONL session; commit a non-empty transaction to upgrade it to format 4 first".into());
        }
        let input = ForkInput::V4 {
            path: &self.metadata.path,
            id: &self.metadata.id,
            cwd: &self.metadata.cwd,
            next_seq: Some(self.state().next_seq()),
        };
        repo.fork_from(&input, &self.metadata.id, &self.metadata.cwd, options, id)
    }
}
