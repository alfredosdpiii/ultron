//! Small helpers shared by the conversation runtime.

use chord_delta::{DeltaError, Draft, Seg};
use durable::{ConversationId, Owner, SessionError};
use serde_json::Value;

use crate::scheduler::{HarnessError, Result};

/// A Chord draft error as a Harness error.
pub(crate) fn d<T>(result: std::result::Result<T, DeltaError>) -> Result<T> {
    result.map_err(|error| HarnessError::Session(SessionError::Delta(error)))
}

pub(crate) fn owner(conversation_id: ConversationId) -> Owner {
    Owner::Conversation(conversation_id)
}

/// `path!` for string keys only.
pub(crate) fn keys(path: &[&str]) -> Vec<Seg> {
    path.iter().map(|key| Seg::from(*key)).collect()
}

/// The value at a key path of a draft, if present.
pub(crate) fn get(draft: &Draft, path: &[&str]) -> Result<Option<Value>> {
    d(draft.get(&keys(path)))
}

pub(crate) fn set(draft: &Draft, path: &[&str], value: Value) -> Result<()> {
    d(draft.set(&keys(path), value))
}

pub(crate) fn delete(draft: &Draft, path: &[&str]) -> Result<()> {
    d(draft.delete(&keys(path)))
}

/// A JSON value without its `null` object members, recursively (upstream `copyJson(..., { omitUndefinedProperties })`
/// for values that came from optional Rust fields).
pub(crate) fn strict(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, value)| (key, strict(value)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(strict).collect()),
        other => other,
    }
}

/// Serialize a value that always serializes.
pub(crate) fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("serializable")
}

/// Deserialize a stored value; a malformed one is a Harness error.
pub(crate) fn from_value<T: serde::de::DeserializeOwned>(value: Value, what: &str) -> Result<T> {
    serde_json::from_value(value).map_err(|error| HarnessError::Error(format!("Malformed {what}: {error}")))
}
