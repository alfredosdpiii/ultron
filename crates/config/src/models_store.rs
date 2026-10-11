//! `<agent>/models-store.json` (`models-store.ts`): provider id to stored model catalog entry, sharing the
//! `auth.json` file backend (lock, 0600 mode, `{}` when missing).

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::auth::{parse_data, with_locked_file};
use crate::json::stringify_pretty;

pub struct ModelsStore {
    path: PathBuf,
}

impl ModelsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        ModelsStore { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self, provider: &str) -> Result<Option<Value>, String> {
        with_locked_file(&self.path, |content| {
            Ok((parse_data(content)?.get(provider).cloned(), None))
        })
    }

    /// Store `entry` for `provider`, keeping every other provider as the file has it now.
    pub fn write(&self, provider: &str, entry: Value) -> Result<(), String> {
        with_locked_file(&self.path, |content| {
            let mut data = parse_data(content)?;
            data.insert(provider.to_string(), entry);
            Ok(((), Some(stringify_pretty(&Value::Object(data)))))
        })
    }

    pub fn delete(&self, provider: &str) -> Result<(), String> {
        with_locked_file(&self.path, |content| {
            let mut data = parse_data(content)?;
            data.shift_remove(provider);
            Ok(((), Some(stringify_pretty(&Value::Object(data)))))
        })
    }
}
