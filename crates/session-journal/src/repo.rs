//! `jsonl/repo.ts`: the file-backed session repository. Sessions live at
//! `<root>/--<cwd with separators as dashes>--/<createdAt ISO with ':' and '.' as '-'>_<encodeURIComponent(id)>.jsonl`.

use crate::codec::{self, JSONL_FORMAT_VERSION, JSONL_STORAGE_VERSION, ParsedHeader};
use crate::fork::{ForkInput, ForkOptions, run_fork};
use crate::legacy_v3::{LegacyV3Source, normalize_header};
use crate::session::Session;
use crate::storage::JsonlStorage;
use crate::{SharedClock, io, lock};
use serde_json::{Map, Value};
use session_file::js;
use std::path::{Path, PathBuf};

/// `JsonlSessionMetadata`.
#[derive(Clone, Debug, PartialEq)]
pub struct Metadata {
    pub id: String,
    pub created_at: i64,
    pub storage_version: i64,
    pub cwd: String,
    pub path: PathBuf,
    /// File modification time, milliseconds since the epoch.
    pub modified_at: f64,
    pub parent_session_id: Option<String>,
    pub legacy_parent_session_path: Option<String>,
}

fn metadata_from_header(header: &Map<String, Value>, path: &Path, modified_at: f64) -> Metadata {
    Metadata {
        id: codec::header_str(header, "id").unwrap_or("").into(),
        created_at: codec::header_i64(header, "createdAt").unwrap_or(0),
        storage_version: codec::header_i64(header, "storageVersion").unwrap_or(1),
        cwd: codec::header_str(header, "cwd").unwrap_or("").into(),
        path: path.to_path_buf(),
        modified_at,
        parent_session_id: codec::header_str(header, "parentSessionId").map(str::to_string),
        legacy_parent_session_path: codec::header_str(header, "legacyParentSessionPath").map(str::to_string),
    }
}

/// `sessionDirectoryName`.
pub fn session_directory_name(cwd: &str) -> String {
    let trimmed = cwd.strip_prefix(['/', '\\']).unwrap_or(cwd);
    format!("--{}--", trimmed.replace(['/', '\\', ':'], "-"))
}

/// `encodeURIComponent`.
pub fn encode_uri_component(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || "-_.!~*'()".contains(c) {
            out.push(c);
        } else {
            let mut buf = [0; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// `sessionFileName`.
pub fn session_file_name(created_at: i64, id: &str) -> String {
    let timestamp = js::iso_string(created_at).replace([':', '.'], "-");
    format!("{timestamp}_{}.jsonl", encode_uri_component(id))
}

/// `path.resolve(p)` against the process cwd, without resolving symlinks.
pub fn absolute(path: &str) -> String {
    let joined = if Path::new(path).is_absolute() {
        path.to_string()
    } else {
        let cwd = std::env::current_dir()
            .map(|d| d.display().to_string())
            .unwrap_or_default();
        format!("{cwd}/{path}")
    };
    let normalized = session_file::paths::normalize(&joined);
    match normalized.strip_suffix('/') {
        Some(trimmed) if !trimmed.is_empty() => trimmed.to_string(),
        _ => normalized,
    }
}

/// The metadata of one session file from its first line, or `None` when it is not a session.
pub fn read_metadata(path: &Path) -> Option<Metadata> {
    let text = std::fs::read(path).ok()?;
    let end = text.iter().position(|b| *b == b'\n').unwrap_or(text.len());
    let line = String::from_utf8_lossy(&text[..end]);
    let modified = io::mtime_ms(path).unwrap_or(0.0);
    match codec::parse_header(&line).ok()? {
        ParsedHeader::V4(header) => Some(metadata_from_header(&header, path, modified)),
        ParsedHeader::V3(header) => Some(metadata_from_header(&normalize_header(&header), path, modified)),
    }
}

/// `JsonlSessionRepo`.
pub struct SessionRepo {
    root: String,
    clock: SharedClock,
}

impl SessionRepo {
    pub fn new(root: &Path, clock: SharedClock) -> Self {
        SessionRepo {
            root: absolute(&root.display().to_string()),
            clock,
        }
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    fn directory(&self, cwd: &str) -> PathBuf {
        Path::new(&self.root).join(session_directory_name(cwd))
    }

    /// `resolveNewSessionPath`: refuses an id that already has a file in the cwd's directory.
    fn new_session_path(&self, cwd: &str, created_at: i64, id: &str) -> Result<PathBuf, String> {
        let directory = self.directory(cwd);
        let suffix = format!("_{}.jsonl", encode_uri_component(id));
        if let Ok(entries) = std::fs::read_dir(&directory) {
            for entry in entries.flatten() {
                if !entry.file_type().is_ok_and(|t| t.is_dir())
                    && entry.file_name().to_string_lossy().ends_with(&suffix)
                {
                    return Err(format!("Session already exists: {id}"));
                }
            }
        }
        std::fs::create_dir_all(&directory)
            .map_err(|e| format!("Failed to create sessions directory {}: {e}", directory.display()))?;
        Ok(directory.join(session_file_name(created_at, id)))
    }

    /// `create`: an empty session (header only) for `cwd`.
    pub fn create(&self, cwd: &str, id: Option<&str>, parent_session_id: Option<&str>) -> Result<Session, String> {
        let (created_at, id) = {
            let mut clock = lock(&self.clock);
            let created_at = clock.now();
            let id = id.map_or_else(|| clock.uuidv7(created_at), str::to_string);
            (created_at, id)
        };
        let cwd = absolute(cwd);
        let path = self.new_session_path(&cwd, created_at, &id)?;
        let mut header = Map::new();
        header.insert("v".into(), JSONL_FORMAT_VERSION.into());
        header.insert("kind".into(), "header".into());
        header.insert("id".into(), id.clone().into());
        header.insert("storageVersion".into(), JSONL_STORAGE_VERSION.into());
        header.insert("createdAt".into(), created_at.into());
        header.insert("cwd".into(), cwd.into());
        if let Some(parent) = parent_session_id {
            header.insert("parentSessionId".into(), parent.into());
        }
        let storage = JsonlStorage::create(&path, header.clone(), &[], &mut *lock(&self.clock))?;
        let metadata = metadata_from_header(&header, &path, io::mtime_ms(&path).unwrap_or(0.0));
        Ok(Session::new(metadata, storage, self.clock.clone()))
    }

    /// `open`: the header must match the metadata's id and cwd.
    pub fn open(&self, metadata: &Metadata) -> Result<Session, String> {
        if !metadata.path.exists() {
            return Err(format!("Session file does not exist: {}", metadata.path.display()));
        }
        let storage = JsonlStorage::open(&metadata.path, &mut *lock(&self.clock))?;
        if codec::header_str(storage.header(), "id") != Some(metadata.id.as_str())
            || codec::header_str(storage.header(), "cwd") != Some(metadata.cwd.as_str())
        {
            return Err(format!("Session identity does not match header: {}", metadata.id));
        }
        Ok(Session::new(metadata.clone(), storage, self.clock.clone()))
    }

    /// `list`: sessions of `cwd` (every cwd when `None`), newest `createdAt` first.
    pub fn list(&self, cwd: Option<&str>) -> Vec<Metadata> {
        let cwd = cwd.map(absolute);
        let directories: Vec<PathBuf> = match &cwd {
            Some(cwd) => vec![self.directory(cwd)],
            None => std::fs::read_dir(&self.root)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                        .map(|e| e.path())
                        .collect()
                })
                .unwrap_or_default(),
        };
        let mut found = Vec::new();
        for directory in directories {
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir())
                    || !entry.file_name().to_string_lossy().ends_with(".jsonl")
                {
                    continue;
                }
                if let Some(metadata) = read_metadata(&entry.path())
                    && cwd.as_ref().is_none_or(|c| *c == metadata.cwd)
                {
                    found.push(metadata);
                }
            }
        }
        found.sort_by(|l, r| {
            r.created_at
                .cmp(&l.created_at)
                .then_with(|| l.id.cmp(&r.id))
                .then_with(|| l.cwd.cmp(&r.cwd))
        });
        found
    }

    /// `delete`.
    pub fn delete(&self, metadata: &Metadata) -> Result<(), String> {
        if !metadata.path.exists() {
            return Err(format!("Session file does not exist: {}", metadata.path.display()));
        }
        std::fs::remove_file(&metadata.path)
            .map_err(|e| format!("Failed to delete session {}: {e}", metadata.path.display()))
    }

    /// `fork` of a closed session (an open one is forked through [`Session::fork_into`]).
    pub fn fork(&self, source: &Metadata, options: &ForkOptions, id: Option<&str>) -> Result<Session, String> {
        let first = io::read_first_line(&source.path).ok().flatten();
        let legacy = matches!(first.as_deref().map(codec::parse_header), Some(Ok(ParsedHeader::V3(_))));
        let legacy_source = if legacy {
            let source_v3 = LegacyV3Source::read(&source.path, &mut *lock(&self.clock))?;
            if codec::header_str(&source_v3.header, "id") != Some(source.id.as_str())
                || codec::header_str(&source_v3.header, "cwd") != Some(source.cwd.as_str())
            {
                return Err(format!("Session identity does not match header: {}", source.id));
            }
            Some(source_v3)
        } else {
            None
        };
        let input = match &legacy_source {
            Some(v3) => ForkInput::LegacyV3(v3),
            None => ForkInput::V4 {
                path: &source.path,
                id: &source.id,
                cwd: &source.cwd,
                next_seq: None,
            },
        };
        self.fork_from(&input, &source.id, &source.cwd, options, id)
    }

    pub(crate) fn fork_from(
        &self,
        input: &ForkInput<'_>,
        source_id: &str,
        cwd: &str,
        options: &ForkOptions,
        id: Option<&str>,
    ) -> Result<Session, String> {
        let (created_at, id) = {
            let mut clock = lock(&self.clock);
            let created_at = clock.now();
            let id = id.map_or_else(|| clock.uuidv7(created_at), str::to_string);
            (created_at, id)
        };
        let path = self.new_session_path(cwd, created_at, &id)?;
        let mut header = Map::new();
        header.insert("v".into(), JSONL_FORMAT_VERSION.into());
        header.insert("kind".into(), "header".into());
        header.insert("id".into(), id.clone().into());
        header.insert("storageVersion".into(), JSONL_STORAGE_VERSION.into());
        header.insert("createdAt".into(), created_at.into());
        header.insert("cwd".into(), cwd.into());
        header.insert("parentSessionId".into(), source_id.into());
        if let Err(e) = run_fork(input, &path, header.clone(), options) {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        let storage = JsonlStorage::open(&path, &mut *lock(&self.clock))?;
        let metadata = metadata_from_header(&header, &path, io::mtime_ms(&path).unwrap_or(0.0));
        Ok(Session::new(metadata, storage, self.clock.clone()))
    }
}
