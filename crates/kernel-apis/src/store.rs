//! Durable module documents (`HostModuleStore`): one JSON document per module, replaced whole on every write.

use crate::util::{lock, uuid};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Mutex;

pub trait DocumentStore: Send + Sync {
    /// The stored document, or `None` before the first write.
    fn read(&self) -> Result<Option<Value>, String>;
    fn write(&self, document: &Value) -> Result<(), String>;
}

/// In-memory store (`createMemoryModuleStore`).
#[derive(Default)]
pub struct MemoryDocumentStore {
    document: Mutex<Option<Value>>,
}

impl MemoryDocumentStore {
    pub fn new(initial: Option<Value>) -> Self {
        Self {
            document: Mutex::new(initial),
        }
    }
}

impl DocumentStore for MemoryDocumentStore {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(lock(&self.document).clone())
    }

    fn write(&self, document: &Value) -> Result<(), String> {
        *lock(&self.document) = Some(document.clone());
        Ok(())
    }
}

/// A JSON file replaced atomically (temporary file, then rename) on every write.
pub struct FileDocumentStore {
    path: PathBuf,
}

impl FileDocumentStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl DocumentStore for FileDocumentStore {
    fn read(&self) -> Result<Option<Value>, String> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|error| format!("{}: {error}", self.path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{}: {error}", self.path.display())),
        }
    }

    fn write(&self, document: &Value) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let temporary = PathBuf::from(format!("{}.{}.tmp", self.path.display(), uuid()));
        std::fs::write(&temporary, serde_json::to_string(document).unwrap_or_default())
            .map_err(|error| error.to_string())?;
        std::fs::rename(&temporary, &self.path).map_err(|error| error.to_string())
    }
}

/// `readVersioned`: a document without its version field reads as version 1; another version is refused.
pub fn read_versioned(namespace: &str, saved: Option<Value>, field: &str) -> Result<Option<Value>, String> {
    let Some(Value::Object(mut document)) = saved else {
        return Ok(saved);
    };
    match document.get(field) {
        None => {
            document.insert(field.to_string(), Value::from(1));
            Ok(Some(Value::Object(document)))
        }
        Some(Value::Number(number)) if number.as_f64() == Some(1.0) => Ok(Some(Value::Object(document))),
        Some(version) => Err(format!(
            "Cannot resume: session value {namespace} has format version {version}, but this Ultron reads only format \
             version 1. It was left unchanged; open the session with the Ultron version that wrote it, or upgrade."
        )),
    }
}
