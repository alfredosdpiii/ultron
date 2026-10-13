//! `artifacts.*` of `ultron/local-services.ts` (`NativeLocalServices`): content-addressed text artifacts with
//! per-item and total size limits and an optional quota that never evicts to make room, byte-exact range reads
//! (a lossy UTF-8 `text` and an exact `base64`), retention by named holders (`pin` / `unpin`), explicit deletion
//! that leaves a tombstone so later reads fail by name, and `reclaim` of unpinned artifacts in listing order.
//!
//! Requests (Python reaches them through `rlm.host_request`):
//! - `artifacts.put` `{text, options?: {mediaType?, label?}}` -> `{id, bytes, mediaType, label, preview}`
//! - `artifacts.read` `{id, options?: {offset?, length?}}` -> `{id, text, base64, offset, length, total}`
//! - `artifacts.list` `{}`; `artifacts.usage` `{}`; `artifacts.reclaim` `{bytes, reason?}`
//! - `artifacts.pin` / `artifacts.unpin` `{id, holder}`; `artifacts.delete` `{id, reason?}`
//!
//! Calls serialize; a failed write makes the service unusable until it is reopened.

use crate::util::{Clock, iso, lock, safe_integer, sha256_hex, system_clock};
use crate::{ApiModule, Request};
use serde_json::{Map, Value, json};
use std::path::PathBuf;
use std::sync::Mutex;

const PREFIX: &str = "ultron.artifacts/";
/// Deliberately outside the artifact prefix so listings never see it.
const REFS_KEY: &str = "ultron.artifact-refs";
pub const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
pub const DEFAULT_MAX_ARTIFACT_STORAGE_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_RANGE: i64 = 1_048_576;

/// Keyed durable documents (`DurableDocumentStorage`): listings keep insertion order.
pub trait KeyedDocuments: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Value>, String>;
    fn set(&self, key: &str, value: &Value) -> Result<(), String>;
    /// Every `(key, value)` whose key starts with `prefix`, in insertion order.
    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String>;
}

/// In-memory documents (a JavaScript `Map`).
#[derive(Default)]
pub struct MemoryKeyedDocuments {
    values: Mutex<Map<String, Value>>,
}

impl KeyedDocuments for MemoryKeyedDocuments {
    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        Ok(lock(&self.values).get(key).cloned())
    }
    fn set(&self, key: &str, value: &Value) -> Result<(), String> {
        lock(&self.values).insert(key.into(), value.clone());
        Ok(())
    }
    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String> {
        Ok(lock(&self.values)
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect())
    }
}

/// Documents in one JSON file (`{key: value}` in insertion order), replaced atomically on every write.
pub struct FileKeyedDocuments {
    store: crate::store::FileDocumentStore,
    guard: Mutex<()>,
}

impl FileKeyedDocuments {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        FileKeyedDocuments {
            store: crate::store::FileDocumentStore::new(path),
            guard: Mutex::new(()),
        }
    }

    fn read(&self) -> Result<Map<String, Value>, String> {
        use crate::store::DocumentStore;
        match self.store.read()? {
            None => Ok(Map::new()),
            Some(Value::Object(map)) => Ok(map),
            Some(_) => Err("Invalid artifact document file".into()),
        }
    }
}

impl KeyedDocuments for FileKeyedDocuments {
    fn get(&self, key: &str) -> Result<Option<Value>, String> {
        let _guard = lock(&self.guard);
        Ok(self.read()?.get(key).cloned())
    }
    fn set(&self, key: &str, value: &Value) -> Result<(), String> {
        use crate::store::DocumentStore;
        let _guard = lock(&self.guard);
        let mut map = self.read()?;
        map.insert(key.into(), value.clone());
        self.store.write(&Value::Object(map))
    }
    fn list(&self, prefix: &str) -> Result<Vec<(String, Value)>, String> {
        let _guard = lock(&self.guard);
        Ok(self
            .read()?
            .into_iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .collect())
    }
}

/// Size limits (`NativeLocalServicesOptions`).
#[derive(Clone, Debug)]
pub struct ArtifactLimits {
    /// Largest single artifact in UTF-8 bytes.
    pub max_artifact_bytes: u64,
    /// Total bytes across all stored artifacts.
    pub max_storage_bytes: u64,
    /// Total bytes of live artifacts this service may hold; a put beyond it fails and nothing is evicted.
    pub quota_bytes: Option<u64>,
}

impl Default for ArtifactLimits {
    fn default() -> Self {
        ArtifactLimits {
            max_artifact_bytes: DEFAULT_MAX_ARTIFACT_BYTES,
            max_storage_bytes: DEFAULT_MAX_ARTIFACT_STORAGE_BYTES,
            quota_bytes: None,
        }
    }
}

/// `{id, bytes, mediaType, label}`.
#[derive(Clone, Debug, PartialEq)]
struct Record {
    id: String,
    bytes: u64,
    media_type: String,
    label: String,
}

impl Record {
    fn json(&self) -> Value {
        json!({ "id": self.id, "bytes": self.bytes, "mediaType": self.media_type, "label": self.label })
    }
}

enum Parsed {
    Live(Record, Vec<u8>),
    Deleted(String),
}

/// `fields(value, required, optional)` of local-services.ts.
fn shape(value: &Value, required: &[&str], optional: &[&str]) -> Result<Map<String, Value>, String> {
    let malformed = || "Malformed local service fields".to_string();
    let map = value.as_object().ok_or_else(malformed)?;
    if required.iter().any(|key| !map.contains_key(*key))
        || map
            .keys()
            .any(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        return Err(malformed());
    }
    Ok(map.clone())
}

fn is_hash(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `readVersioned` with the `formatVersion` field.
fn versioned(key: &str, stored: Value) -> Result<Value, String> {
    crate::store::read_versioned(&format!("ultron.local/{key}"), Some(stored), "formatVersion")
        .map(|v| v.unwrap_or(Value::Null))
}

/// `artifactDocument`: a live artifact (integrity checked) or its tombstone.
fn parse(stored: Value, id: &str) -> Result<Parsed, String> {
    let value = versioned(&format!("{PREFIX}{id}"), stored)?;
    let invalid = || "Invalid artifact document".to_string();
    if value.as_object().is_some_and(|map| map.contains_key("deleted")) {
        let map = shape(
            &value,
            &["formatVersion", "id", "bytes", "mediaType", "label", "deleted"],
            &[],
        )?;
        let deleted = shape(&map["deleted"], &["at", "reason"], &[])?;
        let ok = map["formatVersion"].as_f64() == Some(1.0)
            && map["id"] == id
            && is_hash(id)
            && map["bytes"].is_number()
            && map["mediaType"].is_string()
            && map["label"].is_string()
            && deleted["at"].is_string()
            && deleted["reason"].is_string();
        if !ok {
            return Err(invalid());
        }
        return Ok(Parsed::Deleted(deleted["reason"].as_str().unwrap_or_default().into()));
    }
    let map = shape(
        &value,
        &["formatVersion", "id", "bytes", "mediaType", "label", "text"],
        &[],
    )?;
    let (Some(text), Some(media_type), Some(label)) =
        (map["text"].as_str(), map["mediaType"].as_str(), map["label"].as_str())
    else {
        return Err(invalid());
    };
    if map["formatVersion"].as_f64() != Some(1.0) || map["id"] != id || !is_hash(id) {
        return Err(invalid());
    }
    let bytes = text.as_bytes().to_vec();
    if sha256_hex(&bytes) != id || map["bytes"].as_f64() != Some(bytes.len() as f64) {
        return Err("Artifact integrity failure".into());
    }
    let record = Record {
        id: id.into(),
        bytes: bytes.len() as u64,
        media_type: media_type.into(),
        label: label.into(),
    };
    Ok(Parsed::Live(record, bytes))
}

/// `artifactRefs`: holders per artifact id.
fn refs(stored: Option<Value>) -> Result<Map<String, Value>, String> {
    let Some(stored) = stored else {
        return Ok(Map::new());
    };
    let value = versioned(REFS_KEY, stored)?;
    let map = shape(&value, &["formatVersion", "refs"], &[])?;
    let invalid = || "Invalid artifact reference document".to_string();
    let Some(refs) = map["refs"]
        .as_object()
        .filter(|_| map["formatVersion"].as_f64() == Some(1.0))
    else {
        return Err(invalid());
    };
    if refs
        .values()
        .any(|holders| !holders.as_array().is_some_and(|h| h.iter().all(Value::is_string)))
    {
        return Err(invalid());
    }
    Ok(refs.clone())
}

fn holder_count(refs: &Map<String, Value>, id: &str) -> usize {
    refs.get(id).and_then(Value::as_array).map_or(0, Vec::len)
}

/// Standard base64 with padding (`Buffer.toString("base64")`).
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> shift) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The artifact service; one per session, shared by every lane.
pub struct Artifacts {
    documents: Box<dyn KeyedDocuments>,
    limits: ArtifactLimits,
    now: Clock,
    /// Serializes calls; `true` once a write failed.
    uncertain: Mutex<bool>,
}

impl Artifacts {
    pub fn new(documents: Box<dyn KeyedDocuments>, limits: ArtifactLimits) -> Result<Self, String> {
        Self::with_clock(documents, limits, system_clock())
    }

    pub fn with_clock(documents: Box<dyn KeyedDocuments>, limits: ArtifactLimits, now: Clock) -> Result<Self, String> {
        let safe = |n: u64| (1..=9_007_199_254_740_991).contains(&n);
        if !safe(limits.max_artifact_bytes) || !safe(limits.max_storage_bytes) {
            return Err("Artifact limits must be positive integers".into());
        }
        if limits.quota_bytes.is_some_and(|q| q > 9_007_199_254_740_991) {
            return Err("artifactQuotaBytes must be a nonnegative safe integer".into());
        }
        Ok(Artifacts {
            documents,
            limits,
            now,
            uncertain: Mutex::new(false),
        })
    }

    /// Serve one `artifacts.*` request.
    pub fn handle(&self, kind: &str, payload: &Value) -> Result<Value, String> {
        let mut uncertain = lock(&self.uncertain);
        if *uncertain {
            return Err("Document durability is uncertain; reopen the service before use".into());
        }
        let outcome = self.dispatch(kind, payload);
        if let Err(Failure::Write(_)) = &outcome {
            *uncertain = true;
        }
        outcome.map_err(Failure::message)
    }

    fn persist(&self, key: &str, value: &Value) -> Result<(), Failure> {
        self.documents.set(key, value).map_err(Failure::Write)
    }

    fn index(&self) -> Result<(Vec<Record>, Map<String, Value>), String> {
        let mut live = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for (key, value) in self.documents.list(PREFIX)? {
            if !key.starts_with(PREFIX) || seen.contains(&key) {
                return Err("Invalid artifact listing".into());
            }
            seen.push(key.clone());
            if let Parsed::Live(record, _) = parse(value, &key[PREFIX.len()..])? {
                live.push(record);
            }
        }
        Ok((live, refs(self.documents.get(REFS_KEY)?)?))
    }

    fn tombstone(&self, record: &Record, reason: &str) -> Result<(), Failure> {
        let mut document = json!({ "formatVersion": 1 });
        if let (Some(target), Value::Object(fields)) = (document.as_object_mut(), record.json()) {
            target.extend(fields);
        }
        document["deleted"] = json!({ "at": iso((self.now)()), "reason": reason });
        self.persist(&format!("{PREFIX}{}", record.id), &document)
    }

    fn dispatch(&self, kind: &str, payload: &Value) -> Result<Value, Failure> {
        match kind {
            "artifacts.list" => {
                shape(payload, &[], &[])?;
                Ok(Value::from(
                    self.index()?.0.iter().map(Record::json).collect::<Vec<_>>(),
                ))
            }
            "artifacts.usage" => {
                shape(payload, &[], &[])?;
                let (live, refs) = self.index()?;
                Ok(json!({
                    "usedBytes": live.iter().map(|r| r.bytes).sum::<u64>(),
                    "quotaBytes": self.limits.quota_bytes,
                    "count": live.len(),
                    "pinned": live.iter().filter(|r| holder_count(&refs, &r.id) > 0).map(|r| r.id.clone()).collect::<Vec<_>>(),
                }))
            }
            "artifacts.reclaim" => {
                let map = shape(payload, &["bytes"], &["reason"])?;
                let wanted = safe_integer(&map["bytes"])
                    .filter(|n| *n >= 0)
                    .ok_or_else(|| "reclaim bytes must be a nonnegative safe integer".to_string())?
                    as u64;
                let reason = match map.get("reason") {
                    None => "reclaimed".to_string(),
                    Some(Value::String(reason)) => reason.clone(),
                    Some(_) => return Err("reclaim reason must be a string".to_string().into()),
                };
                let (live, refs) = self.index()?;
                let mut deleted = Vec::new();
                let mut freed = 0;
                for record in &live {
                    if freed >= wanted {
                        break;
                    }
                    if holder_count(&refs, &record.id) > 0 {
                        continue;
                    }
                    self.tombstone(record, &reason)?;
                    deleted.push(record.id.clone());
                    freed += record.bytes;
                }
                let retained: Vec<String> = live
                    .iter()
                    .filter(|r| holder_count(&refs, &r.id) > 0)
                    .map(|r| r.id.clone())
                    .collect();
                Ok(
                    json!({ "deleted": deleted, "freedBytes": freed, "satisfied": freed >= wanted, "retained": retained }),
                )
            }
            "artifacts.put" => self.put(payload),
            "artifacts.read" | "artifacts.pin" | "artifacts.unpin" | "artifacts.delete" => self.by_id(kind, payload),
            other => Err(format!("Unknown local service request: {other}").into()),
        }
    }

    fn put(&self, payload: &Value) -> Result<Value, Failure> {
        let map = shape(payload, &["text"], &["options"])?;
        let Some(text) = map["text"].as_str() else {
            return Err("artifact content must be a string".to_string().into());
        };
        let options = shape(map.get("options").unwrap_or(&json!({})), &[], &["mediaType", "label"])?;
        let media_type = options.get("mediaType").cloned().unwrap_or(json!("text/plain"));
        let label = options.get("label").cloned().unwrap_or(json!(""));
        let (Some(media_type), Some(label)) = (media_type.as_str(), label.as_str()) else {
            return Err("Invalid artifact metadata".to_string().into());
        };
        let bytes = text.as_bytes();
        let size = bytes.len() as u64;
        if size > self.limits.max_artifact_bytes {
            return Err(format!(
                "Artifact of {size} bytes exceeds the {}-byte artifact limit",
                self.limits.max_artifact_bytes
            )
            .into());
        }
        let id = sha256_hex(bytes);
        let key = format!("{PREFIX}{id}");
        let existing = match self.documents.get(&key)? {
            None => None,
            Some(previous) => Some(parse(previous, &id)?),
        };
        if !matches!(existing, Some(Parsed::Live(..))) {
            let (live, _) = self.index()?;
            let used: u64 = live.iter().map(|r| r.bytes).sum();
            if let Some(quota) = self.limits.quota_bytes
                && used + size > quota
            {
                return Err(format!(
                    "Artifact quota exceeded: {used} bytes used + {size} requested > {quota} byte quota. Delete or reclaim unpinned artifacts first."
                )
                .into());
            }
            if used + size > self.limits.max_storage_bytes {
                return Err(format!(
                    "Artifact storage quota of {} bytes exceeded ({used} stored, {size} requested)",
                    self.limits.max_storage_bytes
                )
                .into());
            }
        }
        let record = Record {
            id: id.clone(),
            bytes: size,
            media_type: media_type.into(),
            label: label.into(),
        };
        let mut document = json!({ "formatVersion": 1 });
        if let (Some(target), Value::Object(fields)) = (document.as_object_mut(), record.json()) {
            target.extend(fields);
        }
        document["text"] = json!(text);
        self.persist(&key, &document)?;
        let mut out = record.json();
        out["preview"] = json!(String::from_utf8_lossy(&bytes[..bytes.len().min(2048)]));
        Ok(out)
    }

    fn by_id(&self, kind: &str, payload: &Value) -> Result<Value, Failure> {
        let optional: &[&str] = if kind == "artifacts.read" {
            &["options"]
        } else {
            &["holder", "reason"]
        };
        let map = shape(payload, &["id"], optional)?;
        let Some(id) = map["id"].as_str().filter(|id| is_hash(id)) else {
            return Err("Unknown artifact".to_string().into());
        };
        let Some(stored) = self.documents.get(&format!("{PREFIX}{id}"))? else {
            return Err("Unknown artifact".to_string().into());
        };
        let (record, bytes) = match parse(stored, id)? {
            // An explicit invalidation, never a silently dangling handle.
            Parsed::Deleted(reason) => return Err(format!("Artifact {id} was deleted ({reason})").into()),
            Parsed::Live(record, bytes) => (record, bytes),
        };
        match kind {
            "artifacts.pin" | "artifacts.unpin" => {
                let holder = map
                    .get("holder")
                    .and_then(Value::as_str)
                    .filter(|h| !crate::util::js_trim(h).is_empty() && crate::util::utf16_len(h) <= 200)
                    .ok_or_else(|| "Artifact holder must be a nonempty string of at most 200 characters".to_string())?
                    .to_string();
                let mut all = refs(self.documents.get(REFS_KEY)?)?;
                let mut holders: Vec<String> = all
                    .get(id)
                    .and_then(Value::as_array)
                    .map(|h| h.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                if kind == "artifacts.pin" {
                    if !holders.contains(&holder) {
                        holders.push(holder);
                    }
                } else {
                    holders.retain(|h| h != &holder);
                }
                if holders.is_empty() {
                    all.shift_remove(id);
                } else {
                    all.insert(id.into(), json!(holders));
                }
                self.persist(REFS_KEY, &json!({ "formatVersion": 1, "refs": all }))?;
                Ok(json!({ "id": id, "holders": holders }))
            }
            "artifacts.delete" => {
                let reason = match map.get("reason") {
                    None => "deleted".to_string(),
                    Some(Value::String(reason)) => reason.clone(),
                    Some(_) => return Err("delete reason must be a string".to_string().into()),
                };
                let all = refs(self.documents.get(REFS_KEY)?)?;
                let holders: Vec<String> = all
                    .get(id)
                    .and_then(Value::as_array)
                    .map(|h| h.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                if !holders.is_empty() {
                    return Err(format!("Artifact {id} is retained by {}; unpin it first", holders.join(", ")).into());
                }
                self.tombstone(&record, &reason)?;
                Ok(json!({ "id": id, "deleted": true }))
            }
            _ => {
                let options = shape(map.get("options").unwrap_or(&json!({})), &[], &["offset", "length"])?;
                let number = |key: &str, fallback: i64| match options.get(key) {
                    None => Some(fallback),
                    Some(value) => safe_integer(value),
                };
                let (Some(offset), Some(length)) = (number("offset", 0), number("length", 8192)) else {
                    return Err("Invalid artifact range".to_string().into());
                };
                if offset < 0 || !(0..=MAX_RANGE).contains(&length) {
                    return Err("Invalid artifact range".to_string().into());
                }
                let start = (offset as usize).min(bytes.len());
                let end = (start + length as usize).min(bytes.len());
                let range = &bytes[start..end];
                // text is a lossy UTF-8 decoding when the range splits a character; base64 is exact.
                Ok(
                    json!({ "id": id, "text": String::from_utf8_lossy(range), "base64": base64(range),
                           "offset": offset, "length": range.len(), "total": bytes.len() }),
                )
            }
        }
    }
}

/// An error, and whether it came from a write (which leaves durability uncertain).
enum Failure {
    Write(String),
    Other(String),
}

impl Failure {
    fn message(self) -> String {
        match self {
            Failure::Write(message) | Failure::Other(message) => message,
        }
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Failure::Other(message)
    }
}

/// `artifacts.*` as a host module.
pub struct ArtifactsModule {
    artifacts: Artifacts,
}

impl ArtifactsModule {
    pub fn new(artifacts: Artifacts) -> Self {
        ArtifactsModule { artifacts }
    }
}

impl ApiModule for ArtifactsModule {
    fn prefixes(&self) -> &[&str] {
        &["artifacts."]
    }

    fn handle(&self, request: &Request<'_>) -> Result<Value, String> {
        self.artifacts
            .handle(request.kind, &Value::Object(request.payload.clone()))
    }
}
