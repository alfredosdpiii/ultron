//! The server's session store (the `JsonlSessionRepo` role in experimental/server.ts): one directory per cwd under
//! the session root (`sessionDirectoryName`), one format-4 journal per session named `<ISO time>_<id>.jsonl`
//! (crates/session-journal; legacy Pi v3 files are listed and upgraded by their first commit).

use serde_json::{Value, json};
use session_journal::fork::ForkOptions;
use session_journal::repo::Metadata;
use session_journal::{SessionRepo, SystemClock, names};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Meta {
    pub id: String,
    pub created_at: i64,
    pub storage_version: i64,
    pub cwd: String,
    pub path: PathBuf,
    pub modified_at: f64,
    pub parent_session_id: Option<String>,
}

impl Meta {
    /// `SessionWorkerMetadataSchema`.
    pub fn to_json(&self) -> Value {
        let mut value = json!({
            "id": self.id, "createdAt": self.created_at, "storageVersion": self.storage_version,
            "cwd": self.cwd, "path": self.path.display().to_string(), "modifiedAt": self.modified_at,
        });
        if let Some(parent) = &self.parent_session_id {
            value["parentSessionId"] = json!(parent);
        }
        value
    }

    pub fn from_json(value: &Value) -> Option<Meta> {
        Some(Meta {
            id: value["id"].as_str()?.to_string(),
            created_at: value["createdAt"].as_i64()?,
            storage_version: value["storageVersion"].as_i64()?,
            cwd: value["cwd"].as_str()?.to_string(),
            path: PathBuf::from(value["path"].as_str()?),
            modified_at: value["modifiedAt"].as_f64()?,
            parent_session_id: value["parentSessionId"].as_str().map(str::to_string),
        })
    }

    /// `summarize`: what `SessionDirectory` and `SessionManagement.create` report.
    pub fn summary(&self, server_id: &str) -> Value {
        json!({"serverId": server_id, "sessionId": self.id, "createdAt": self.created_at, "modifiedAt": self.modified_at})
    }

    fn metadata(&self) -> Metadata {
        Metadata {
            id: self.id.clone(),
            created_at: self.created_at,
            storage_version: self.storage_version,
            cwd: self.cwd.clone(),
            path: self.path.clone(),
            modified_at: self.modified_at,
            parent_session_id: self.parent_session_id.clone(),
            legacy_parent_session_path: None,
        }
    }
}

impl From<Metadata> for Meta {
    fn from(m: Metadata) -> Meta {
        Meta {
            id: m.id,
            created_at: m.created_at,
            storage_version: m.storage_version,
            cwd: m.cwd,
            path: m.path,
            modified_at: m.modified_at,
            parent_session_id: m.parent_session_id,
        }
    }
}

/// `sessionDirectoryName`: the cwd without its leading separator, separators and colons as dashes, between `--`.
pub fn session_directory_name(cwd: &str) -> String {
    session_journal::repo::session_directory_name(cwd)
}

/// The metadata a session file's header line gives, for either header format.
pub fn read_meta(path: &Path) -> Option<Meta> {
    session_journal::repo::read_metadata(path).map(Meta::from)
}

fn repo(root: &Path) -> SessionRepo {
    SessionRepo::new(root, SystemClock::shared())
}

/// Every session under `root`, in no particular order (callers sort).
pub fn list(root: &Path) -> Vec<Meta> {
    repo(root).list(None).into_iter().map(Meta::from).collect()
}

pub fn resolve(root: &Path, session_id: &str, tracked: &[Meta]) -> Result<Meta, crate::chord::ServiceError> {
    let mut matches: Vec<Meta> = list(root).into_iter().filter(|m| m.id == session_id).collect();
    for meta in tracked.iter().filter(|m| m.id == session_id) {
        if !matches.iter().any(|m| m.path == meta.path) {
            matches.push(meta.clone());
        }
    }
    match matches.len() {
        0 => Err(crate::chord::ServiceError::coded(
            "session_not_found",
            format!("Unknown session: {session_id}"),
        )),
        1 => Ok(matches.remove(0)),
        _ => Err(crate::chord::ServiceError::coded(
            "session_ambiguous",
            "Session ID matches more than one session",
        )),
    }
}

/// `repo.create`: a new session for `cwd` holding only its header (plus its name).
pub fn create(root: &Path, cwd: &str, id: Option<String>, name: Option<&str>) -> Result<Meta, String> {
    let mut session = repo(root).create(cwd, id.as_deref(), None)?;
    if let Some(name) = name {
        names::set_user_name(&mut session, Some(name))?;
    }
    read_meta(&session.metadata().path).ok_or_else(|| "Created session is unreadable".to_string())
}

/// Where a fork copies to (`forkPath`): the whole tree, or `main`'s path up to an entry (before it, or at it; no
/// entry: the tip).
#[derive(Clone, Debug, PartialEq)]
pub enum ForkPath {
    Tree,
    Branch { entry_id: Option<String>, before: bool },
}

impl ForkPath {
    /// `forkPath` of the create options (`{entryId?, position: "before" | "at"}`; absent: the whole tree).
    pub fn from_json(value: &Value) -> ForkPath {
        if !value.is_object() {
            return ForkPath::Tree;
        }
        ForkPath::Branch {
            entry_id: value["entryId"].as_str().map(str::to_string),
            before: value["position"] == "before",
        }
    }
}

/// `repo.fork`: a copy of `source` (see [`ForkPath`]) under a new ID.
pub fn fork(
    root: &Path,
    source: &Meta,
    id: Option<String>,
    name: Option<&str>,
    path: &ForkPath,
) -> Result<Meta, String> {
    let options = match path {
        ForkPath::Tree => ForkOptions::Tree,
        ForkPath::Branch { entry_id, before } => ForkOptions::Branch {
            branch: "main".into(),
            entry_id: entry_id.clone(),
            any_tree_entry: entry_id.is_some(),
            before: *before,
        },
    };
    let mut session = repo(root).fork(&source.metadata(), &options, id.as_deref())?;
    if let Some(name) = name {
        names::set_user_name(&mut session, Some(name))?;
    }
    read_meta(&session.metadata().path).ok_or_else(|| "Forked session is unreadable".to_string())
}

/// `session.setName`.
pub fn rename(path: &Path, name: &str) -> Result<(), String> {
    let meta = read_meta(path).ok_or_else(|| format!("Not a session file: {}", path.display()))?;
    let root = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| format!("Not in a session root: {}", path.display()))?;
    let mut session = repo(root).open(&meta.metadata())?;
    names::set_user_name(&mut session, Some(name)).map(drop)
}

pub fn remove(meta: &Meta) -> Result<(), String> {
    match std::fs::remove_file(&meta.path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
        _ => {
            // Its single-owner record (crates/durable `owner`) goes with it.
            let _ = std::fs::remove_file(durable::owner::owner_path(&meta.path));
            Ok(())
        }
    }
}

/// `readNativeSessionDetails`: name, first user message text and message count.
pub fn details(meta: &Meta) -> (Option<String>, String, usize) {
    let listing = names::listing(meta.metadata());
    (listing.name, listing.first_message, listing.message_count)
}
