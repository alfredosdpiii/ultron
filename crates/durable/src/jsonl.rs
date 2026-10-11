//! JSONL backend, a port of upstream `storage/jsonl/storage.ts`: a `main.jsonl` of commit markers plus per-task and
//! per-document sidecar files, replayed into [`MemoryStorage`] on open. A commit appends its sidecar records first and
//! then its marker; a marker confirms the sidecar records it names, and recovery drops unconfirmed tails.
//!
//! Files use upstream's format 1 line shapes, so directories are interchangeable with upstream Pi Durable 1.1.0.
//! Filesystem access goes through [`JsonlFs`] so tests can inject failures; [`StdFs`] is the real one.

use crate::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentContent, DocumentCreate,
    DocumentId, DocumentPoint, DocumentQuery, DocumentRecord, EntryId, EntryQuery, EntryRecord, MAX_SAFE_ID,
    MemoryStorage, Page, Result, Seq, Storage, StorageError, StorageWrite, StoredDocument, StoredEntry, SubmissionId,
    SubmissionQuery, SubmissionRecord, TaskId, TaskQuery, TaskRecord, TaskStatus, failed,
};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const FORMAT_VERSION: u64 = 1;
const MAIN_FILE: &str = "main.jsonl";
const RECLAIM_SUFFIX: &str = ".reclaim";

/// The filesystem operations JSONL storage performs. Each maps to one upstream `FileSystem` call.
pub trait JsonlFs {
    fn create_dir_all(&mut self, path: &Path) -> io::Result<()>;
    /// File names and whether each entry is a regular file.
    fn list(&mut self, dir: &Path) -> io::Result<Vec<(String, bool)>>;
    /// `None` when the file does not exist.
    fn read(&mut self, path: &Path) -> io::Result<Option<Vec<u8>>>;
    fn append(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Make a file's contents durable (fsync).
    fn flush(&mut self, path: &Path) -> io::Result<()>;
    fn write(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()>;
    /// Remove a file; a missing file is not an error.
    fn remove(&mut self, path: &Path) -> io::Result<()>;
    fn truncate(&mut self, path: &Path, len: u64) -> io::Result<()>;
}

/// The local filesystem.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdFs;

impl JsonlFs for StdFs {
    fn create_dir_all(&mut self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn list(&mut self, dir: &Path) -> io::Result<Vec<(String, bool)>> {
        std::fs::read_dir(dir)?
            .map(|entry| {
                let entry = entry?;
                Ok((
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.file_type()?.is_file(),
                ))
            })
            .collect()
    }

    fn read(&mut self, path: &Path) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn append(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(bytes)
    }

    fn flush(&mut self, path: &Path) -> io::Result<()> {
        File::open(path)?.sync_all()
    }

    fn write(&mut self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        std::fs::write(path, bytes)
    }

    fn rename(&mut self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn remove(&mut self, path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        }
    }

    fn truncate(&mut self, path: &Path, len: u64) -> io::Result<()> {
        OpenOptions::new().write(true).open(path)?.set_len(len)
    }
}

/// JSONL storage settings.
#[derive(Clone, Copy, Debug, Default)]
pub struct JsonlOptions {
    /// Flush every affected sidecar before appending the main marker, and main before reclaiming. Default false.
    pub fsync: bool,
}

fn file_error(action: &str, error: io::Error) -> StorageError {
    StorageError::Failed(format!("JSONL {action} failed: {error}"))
}

fn corrupt<T>(message: impl Into<String>) -> Result<T> {
    Err(StorageError::Corrupt(message.into()))
}

fn sidecar_file(kind: &str, id: u64) -> String {
    format!("{kind}-{id}.jsonl")
}

/// `doc-N.jsonl` or `task-N.jsonl` (with `suffix`), and the numeric ID.
fn sidecar_id(name: &str, suffix: &str) -> Option<u64> {
    let rest = name.strip_suffix(suffix)?;
    let digits = rest.strip_prefix("doc-").or_else(|| rest.strip_prefix("task-"))?;
    let canonical = digits == "0" || (!digits.starts_with('0') && !digits.is_empty());
    (canonical && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

fn line(value: &Value) -> String {
    let mut text = serde_json::to_string(value).expect("JSON values serialize");
    text.push('\n');
    text
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("records serialize")
}

fn safe_int(value: &Value) -> Option<u64> {
    value.as_u64().filter(|n| *n <= MAX_SAFE_ID)
}

fn decode<T: DeserializeOwned>(value: &Value, message: impl FnOnce() -> String) -> Result<T> {
    serde_json::from_value(value.clone()).or_else(|_| corrupt(message()))
}

/// One write of a commit marker in `main.jsonl`.
#[derive(Clone, Debug)]
enum MainOp {
    /// A conversation, entry, submission, document retirement, or terminal task, stored inline.
    Inline(Box<StorageWrite>),
    TaskSidecar {
        id: u64,
        ordinal: u64,
    },
    DocumentCreate {
        record: DocumentCreate,
        ordinal: u64,
    },
    DocumentChange {
        id: u64,
        ordinal: u64,
    },
}

impl MainOp {
    fn to_json(&self) -> Value {
        match self {
            MainOp::Inline(write) => to_value(write),
            MainOp::TaskSidecar { id, ordinal } => json!({ "type": "task.sidecar", "id": id, "ordinal": ordinal }),
            MainOp::DocumentCreate { record, ordinal } => {
                json!({ "type": "document.create", "record": record, "ordinal": ordinal })
            }
            MainOp::DocumentChange { id, ordinal } => {
                json!({ "type": "document.change", "id": id, "ordinal": ordinal })
            }
        }
    }

    fn parse(value: &Value, description: &str) -> Result<MainOp> {
        let Some(kind) = value.get("type").and_then(Value::as_str).filter(|_| value.is_object()) else {
            return corrupt(format!("Invalid write in {description}"));
        };
        let ordinal = || value.get("ordinal").and_then(safe_int);
        match kind {
            "conversation" | "entry" | "submission" => {
                if value
                    .get("value")
                    .and_then(|v| v.get("id"))
                    .and_then(safe_int)
                    .is_none()
                {
                    return corrupt(format!("Invalid {kind} write in {description}"));
                }
                decode(value, || format!("Invalid {kind} write in {description}"))
                    .map(|write| MainOp::Inline(Box::new(write)))
            }
            "task" => {
                let terminal = value
                    .get("value")
                    .and_then(|v| v.get("state"))
                    .and_then(|s| s.get("status"))
                    == Some(&json!("terminal"));
                if value
                    .get("value")
                    .and_then(|v| v.get("id"))
                    .and_then(safe_int)
                    .is_none()
                    || !terminal
                {
                    return corrupt(format!("Invalid terminal task write in {description}"));
                }
                decode(value, || format!("Invalid terminal task write in {description}"))
                    .map(|write| MainOp::Inline(Box::new(write)))
            }
            "document.retire" => match value.get("id").and_then(safe_int) {
                Some(_) => decode(value, || format!("Invalid document retirement in {description}"))
                    .map(|write| MainOp::Inline(Box::new(write))),
                None => corrupt(format!("Invalid document retirement in {description}")),
            },
            "task.sidecar" => match (value.get("id").and_then(safe_int), ordinal()) {
                (Some(id), Some(ordinal)) => Ok(MainOp::TaskSidecar { id, ordinal }),
                _ => corrupt(format!("Invalid task sidecar write in {description}")),
            },
            "document.create" => {
                let message = || format!("Invalid document creation in {description}");
                let id = value.get("record").and_then(|r| r.get("id")).and_then(safe_int);
                let (Some(_), Some(ordinal)) = (id, ordinal()) else {
                    return corrupt(message());
                };
                Ok(MainOp::DocumentCreate {
                    record: decode(&value["record"], message)?,
                    ordinal,
                })
            }
            "document.change" => match (value.get("id").and_then(safe_int), ordinal()) {
                (Some(id), Some(ordinal)) => Ok(MainOp::DocumentChange { id, ordinal }),
                _ => corrupt(format!("Invalid document change in {description}")),
            },
            _ => corrupt(format!("Unknown write type in {description}")),
        }
    }
}

struct Marker {
    seq: u64,
    writes: Vec<MainOp>,
}

fn parse_marker(text: &str, line_number: usize) -> Result<Marker> {
    let description = format!("{MAIN_FILE} line {line_number}");
    let value: Value = serde_json::from_str(text).or_else(|_| corrupt(format!("Malformed complete {description}")))?;
    let seq = value.get("seq").and_then(safe_int).filter(|seq| *seq >= 1);
    let writes = value.get("writes").and_then(Value::as_array);
    let (Some(seq), Some(writes), true, true) = (
        seq,
        writes,
        value.get("format") == Some(&json!(FORMAT_VERSION)),
        value.get("type") == Some(&json!("commit")),
    ) else {
        return corrupt(format!("Invalid commit marker in {description}"));
    };
    let writes = writes
        .iter()
        .map(|write| MainOp::parse(write, &description))
        .collect::<Result<_>>()?;
    Ok(Marker { seq, writes })
}

#[derive(Clone, Debug)]
enum Payload {
    Task(Box<TaskRecord>),
    Document { id: u64, content: DocumentContent },
}

#[derive(Clone, Debug)]
struct SidecarRecord {
    seq: u64,
    ordinal: u64,
    payload: Payload,
}

impl SidecarRecord {
    fn to_json(&self) -> Value {
        let payload = match &self.payload {
            Payload::Task(task) => json!({ "type": "task", "value": task }),
            Payload::Document { id, content } => json!({ "type": "document", "id": id, "content": content }),
        };
        json!({ "format": FORMAT_VERSION, "type": "record", "seq": self.seq, "ordinal": self.ordinal, "payload": payload })
    }
}

fn parse_sidecar(text: &str, file: &str, line_number: usize) -> Result<SidecarRecord> {
    let description = format!("{file} line {line_number}");
    let value: Value = serde_json::from_str(text).or_else(|_| corrupt(format!("Malformed complete {description}")))?;
    let seq = value.get("seq").and_then(safe_int).filter(|seq| *seq >= 1);
    let ordinal = value.get("ordinal").and_then(safe_int);
    let payload = value.get("payload").filter(|p| p.is_object());
    let payload_type = payload.and_then(|p| p.get("type")).and_then(Value::as_str);
    let (Some(seq), Some(ordinal), Some(payload), Some(payload_type), true, true) = (
        seq,
        ordinal,
        payload,
        payload_type,
        value.get("format") == Some(&json!(FORMAT_VERSION)),
        value.get("type") == Some(&json!("record")),
    ) else {
        return corrupt(format!("Invalid sidecar record in {description}"));
    };
    let payload = match payload_type {
        "task" => {
            let task = &payload["value"];
            let status = task.get("state").and_then(|s| s.get("status"));
            if task.get("id").and_then(safe_int).is_none() || status.is_none() || status == Some(&json!("terminal")) {
                return corrupt(format!("Invalid live task record in {description}"));
            }
            Payload::Task(Box::new(decode(task, || {
                format!("Invalid live task record in {description}")
            })?))
        }
        "document" => {
            let Some(id) = payload.get("id").and_then(safe_int) else {
                return corrupt(format!("Invalid document record in {description}"));
            };
            Payload::Document {
                id,
                content: parse_content(&payload["content"], &description)?,
            }
        }
        _ => return corrupt(format!("Unknown sidecar record type in {description}")),
    };
    Ok(SidecarRecord { seq, ordinal, payload })
}

fn parse_content(value: &Value, description: &str) -> Result<DocumentContent> {
    let message = || format!("Invalid document content in {description}");
    let version_ok = value.get("version").and_then(safe_int).is_some_and(|v| v >= 1);
    let shape_ok = match value.get("kind").and_then(Value::as_str) {
        Some("base") => value.get("value").is_some_and(Value::is_object),
        Some("delta") => value.get("ops").is_some_and(Value::is_array),
        _ => false,
    };
    if !value.is_object() || !version_ok || !shape_ok {
        return corrupt(message());
    }
    decode(value, message)
}

/// Complete lines of a file and each line's byte offset. A torn final line is truncated away on disk.
fn read_lines<T>(
    fs: &mut impl JsonlFs,
    path: &Path,
    name: &str,
    parse: impl Fn(&str, usize) -> Result<T>,
) -> Result<Vec<(T, u64)>> {
    let Some(bytes) = fs.read(path).map_err(|e| file_error(&format!("read of {name}"), e))? else {
        return Ok(Vec::new());
    };
    let complete = match bytes.last() {
        Some(b'\n') | None => bytes.len(),
        Some(_) => {
            let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            fs.truncate(path, complete as u64)
                .map_err(|e| file_error(&format!("torn-line truncation of {name}"), e))?;
            complete
        }
    };
    let mut lines = Vec::new();
    let mut start = 0;
    for (index, raw) in bytes[..complete].split_inclusive(|b| *b == b'\n').enumerate() {
        let Ok(text) = std::str::from_utf8(&raw[..raw.len() - 1]) else {
            return corrupt(format!("Invalid UTF-8 in complete {name} line {}", index + 1));
        };
        lines.push((parse(text, index + 1)?, start as u64));
        start += raw.len();
    }
    Ok(lines)
}

/// A commit encoded as its main marker and the sidecar text appended per file, in first-write order.
struct Encoded {
    marker: String,
    sidecars: Vec<(String, String)>,
}

/// Portable JSONL implementation of [`Storage`].
pub struct JsonlStorage<F: JsonlFs = StdFs> {
    fs: F,
    directory: PathBuf,
    fsync: bool,
    memory: MemoryStorage,
    current_only_documents: HashSet<u64>,
    live_task_sidecars: HashSet<u64>,
    closed: bool,
    poisoned: Option<String>,
}

impl JsonlStorage<StdFs> {
    /// Open or create a JSONL storage directory on the local filesystem.
    pub fn open(directory: impl AsRef<Path>, options: JsonlOptions) -> Result<JsonlStorage<StdFs>> {
        JsonlStorage::open_with(directory, StdFs, options)
    }
}

impl<F: JsonlFs> JsonlStorage<F> {
    /// Open or create a JSONL storage directory through `fs`, recovering committed state.
    pub fn open_with(directory: impl AsRef<Path>, mut fs: F, options: JsonlOptions) -> Result<JsonlStorage<F>> {
        let directory = std::path::absolute(directory.as_ref()).map_err(|e| file_error("path resolution", e))?;
        fs.create_dir_all(&directory)
            .map_err(|e| file_error("directory creation", e))?;
        let mut storage = JsonlStorage {
            fs,
            directory,
            fsync: options.fsync,
            memory: MemoryStorage::new(),
            current_only_documents: HashSet::new(),
            live_task_sidecars: HashSet::new(),
            closed: false,
            poisoned: None,
        };
        storage.recover()?;
        Ok(storage)
    }

    /// The filesystem, for tests that inspect or reconfigure an injected one.
    pub fn fs_mut(&mut self) -> &mut F {
        &mut self.fs
    }

    fn path(&self, file: &str) -> PathBuf {
        self.directory.join(file)
    }

    fn assert_usable(&self) -> Result<()> {
        if self.closed {
            return failed("JsonlStorage is closed");
        }
        match &self.poisoned {
            Some(cause) => Err(StorageError::Poisoned(cause.clone())),
            None => Ok(()),
        }
    }

    fn store(&self) -> Result<&MemoryStorage> {
        self.assert_usable()?;
        Ok(&self.memory)
    }

    fn poison(&mut self, error: StorageError) -> StorageError {
        let cause = self.poisoned.get_or_insert_with(|| error.message().to_owned());
        StorageError::Poisoned(cause.clone())
    }

    fn encode(seq: Seq, writes: &[StorageWrite]) -> Encoded {
        let mut main = Vec::new();
        let mut records: Vec<(String, Vec<SidecarRecord>)> = Vec::new();
        let mut next_ordinal = 0;
        let mut add = |file: String, payload: Payload| {
            let ordinal = next_ordinal;
            next_ordinal += 1;
            let record = SidecarRecord {
                seq: seq.0,
                ordinal,
                payload,
            };
            match records.iter_mut().find(|(name, _)| *name == file) {
                Some((_, list)) => list.push(record),
                None => records.push((file, vec![record])),
            }
            ordinal
        };
        for write in writes {
            match write {
                StorageWrite::Conversation { .. }
                | StorageWrite::Entry { .. }
                | StorageWrite::Submission { .. }
                | StorageWrite::DocumentRetire { .. } => main.push(MainOp::Inline(Box::new(write.clone()))),
                StorageWrite::Task { value } if value.state.status() == TaskStatus::Terminal => {
                    main.push(MainOp::Inline(Box::new(write.clone())));
                }
                StorageWrite::Task { value } => {
                    let ordinal = add(sidecar_file("task", value.id.0), Payload::Task(Box::new(value.clone())));
                    main.push(MainOp::TaskSidecar {
                        id: value.id.0,
                        ordinal,
                    });
                }
                StorageWrite::DocumentCreate { record, content } => {
                    let payload = Payload::Document {
                        id: record.id.0,
                        content: content.clone(),
                    };
                    let ordinal = add(sidecar_file("doc", record.id.0), payload);
                    main.push(MainOp::DocumentCreate {
                        record: record.clone(),
                        ordinal,
                    });
                }
                StorageWrite::DocumentChange { id, content } => {
                    let ordinal = add(
                        sidecar_file("doc", id.0),
                        Payload::Document {
                            id: id.0,
                            content: content.clone(),
                        },
                    );
                    main.push(MainOp::DocumentChange { id: id.0, ordinal });
                }
                StorageWrite::DocumentCopy { .. } => unreachable!("prepared commits resolve copies"),
            }
        }
        let marker = json!({
            "format": FORMAT_VERSION,
            "type": "commit",
            "seq": seq.0,
            "writes": main.iter().map(MainOp::to_json).collect::<Vec<_>>(),
        });
        let sidecars = records
            .into_iter()
            .map(|(file, list)| (file, list.iter().map(|record| line(&record.to_json())).collect()))
            .collect();
        Encoded {
            marker: line(&marker),
            sidecars,
        }
    }

    /// Sidecar files to replace after the marker publishes: emptied ("" removes the file) or rewritten.
    fn plan_reclamations(&self, writes: &[StorageWrite], encoded: &Encoded) -> Vec<(String, String)> {
        let mut created_current_only = HashSet::new();
        let mut retired = Vec::new();
        let mut bases = Vec::new();
        let mut final_tasks: Vec<(u64, bool)> = Vec::new();
        for write in writes {
            match write {
                StorageWrite::DocumentCreate { record, .. } if record.is_current_only() => {
                    created_current_only.insert(record.id.0);
                }
                StorageWrite::DocumentChange {
                    id,
                    content: DocumentContent::Base { .. },
                } => bases.push(id.0),
                StorageWrite::DocumentRetire { id } => retired.push(id.0),
                StorageWrite::Task { value } => {
                    let terminal = value.state.status() == TaskStatus::Terminal;
                    match final_tasks.iter_mut().find(|(id, _)| *id == value.id.0) {
                        Some(entry) => entry.1 = terminal,
                        None => final_tasks.push((value.id.0, terminal)),
                    }
                }
                _ => {}
            }
        }
        let current_only = |id: &u64| self.current_only_documents.contains(id) || created_current_only.contains(id);
        let mut replacements: Vec<(String, String)> = Vec::new();
        let mut set = |file: String, content: String| match replacements.iter_mut().find(|(name, _)| *name == file) {
            Some(entry) => entry.1 = content,
            None => replacements.push((file, content)),
        };
        for id in retired.iter().filter(|id| current_only(id)) {
            set(sidecar_file("doc", *id), String::new());
        }
        for id in bases.iter().filter(|id| current_only(id) && !retired.contains(id)) {
            let file = sidecar_file("doc", *id);
            if let Some((_, content)) = encoded.sidecars.iter().find(|(name, _)| *name == file) {
                set(file, content.clone());
            }
        }
        for (id, terminal) in final_tasks {
            let file = sidecar_file("task", id);
            let has_sidecar =
                self.live_task_sidecars.contains(&id) || encoded.sidecars.iter().any(|(name, _)| *name == file);
            if terminal && has_sidecar {
                set(file, String::new());
            }
        }
        replacements
    }

    fn adopt_sidecar_state(&mut self, writes: &[StorageWrite]) {
        for write in writes {
            match write {
                StorageWrite::DocumentCreate { record, .. } if record.is_current_only() => {
                    self.current_only_documents.insert(record.id.0);
                }
                StorageWrite::Task { value } if value.state.status() == TaskStatus::Terminal => {
                    self.live_task_sidecars.remove(&value.id.0);
                }
                StorageWrite::Task { value } => {
                    self.live_task_sidecars.insert(value.id.0);
                }
                _ => {}
            }
        }
    }

    /// The marker already published this state, so reclamation is retryable best-effort maintenance.
    fn reclaim(&mut self, replacements: Vec<(String, String)>) {
        if replacements.is_empty() {
            return;
        }
        if self.fsync && self.fs.flush(&self.path(MAIN_FILE)).is_err() {
            return;
        }
        for (file, content) in replacements {
            let path = self.path(&file);
            if content.is_empty() {
                let _ = self.fs.remove(&path);
                continue;
            }
            let temporary = self.path(&format!("{file}{RECLAIM_SUFFIX}"));
            if self.fs.write(&temporary, content.as_bytes()).is_err() {
                continue;
            }
            if self.fsync && self.fs.flush(&temporary).is_err() {
                continue;
            }
            let _ = self.fs.rename(&temporary, &path);
        }
    }

    fn recover(&mut self) -> Result<()> {
        let main_path = self.path(MAIN_FILE);
        let markers = read_lines(&mut self.fs, &main_path, MAIN_FILE, parse_marker)?;
        let mut previous = 0;
        for (marker, _) in &markers {
            if marker.seq <= previous {
                return corrupt(format!("Commit sequence does not strictly increase in {MAIN_FILE}"));
            }
            previous = marker.seq;
        }

        let listed = self
            .fs
            .list(&self.directory)
            .map_err(|e| file_error("directory listing", e))?;
        for (name, is_file) in &listed {
            if *is_file && sidecar_id(name, &format!(".jsonl{RECLAIM_SUFFIX}")).is_some() {
                let _ = self.fs.remove(&self.directory.join(name));
            }
        }
        let mut sidecar_files: Vec<String> = listed
            .into_iter()
            .filter(|(name, is_file)| *is_file && sidecar_id(name, ".jsonl").is_some())
            .map(|(name, _)| name)
            .collect();
        sidecar_files.sort();
        let mut parsed: Vec<(String, Vec<(SidecarRecord, u64)>)> = Vec::new();
        let mut by_key: HashMap<(String, u64, u64), SidecarRecord> = HashMap::new();
        for file in sidecar_files {
            let path = self.directory.join(&file);
            let lines = read_lines(&mut self.fs, &path, &file, |text, n| parse_sidecar(text, &file, n))?;
            for pair in lines.windows(2) {
                let (a, b) = (&pair[0].0, &pair[1].0);
                if b.seq < a.seq || (b.seq == a.seq && b.ordinal <= a.ordinal) {
                    return corrupt(format!("Sidecar records are out of order in {file}"));
                }
            }
            for (record, _) in &lines {
                by_key.insert((file.clone(), record.seq, record.ordinal), record.clone());
            }
            parsed.push((file, lines));
        }

        let mut current_only = HashSet::new();
        let mut retired = HashSet::new();
        let mut final_task_live: BTreeMap<u64, bool> = BTreeMap::new();
        for (marker, _) in &markers {
            for op in &marker.writes {
                match op {
                    MainOp::DocumentCreate { record, .. } if record.is_current_only() => {
                        current_only.insert(record.id.0);
                    }
                    MainOp::Inline(write) => match &**write {
                        StorageWrite::DocumentRetire { id } => {
                            retired.insert(id.0);
                        }
                        StorageWrite::Task { value } => {
                            final_task_live.insert(value.id.0, false);
                        }
                        _ => {}
                    },
                    MainOp::TaskSidecar { id, .. } => {
                        final_task_live.insert(*id, true);
                    }
                    _ => {}
                }
            }
        }
        let retired_current_only: HashSet<u64> = retired.intersection(&current_only).copied().collect();

        let mut latest_bases: HashMap<u64, (u64, u64)> = HashMap::new();
        for (marker, _) in &markers {
            for op in &marker.writes {
                let (id, ordinal) = match op {
                    MainOp::DocumentCreate { record, ordinal } => (record.id.0, *ordinal),
                    MainOp::DocumentChange { id, ordinal } => (*id, *ordinal),
                    _ => continue,
                };
                if !current_only.contains(&id) {
                    continue;
                }
                let Some(record) = by_key.get(&(sidecar_file("doc", id), marker.seq, ordinal)) else {
                    continue;
                };
                if !matches!(&record.payload, Payload::Document { id: pid, content: DocumentContent::Base { .. } } if *pid == id)
                {
                    continue;
                }
                let at = (record.seq, record.ordinal);
                if latest_bases.get(&id).is_none_or(|previous| at > *previous) {
                    latest_bases.insert(id, at);
                }
            }
        }
        let before_latest_base =
            |id: u64, seq: u64, ordinal: u64| latest_bases.get(&id).is_some_and(|base| (seq, ordinal) < *base);
        let terminal_tasks: HashSet<u64> = final_task_live
            .iter()
            .filter(|(_, live)| !**live)
            .map(|(id, _)| *id)
            .collect();

        let mut confirmed: HashSet<(String, u64, u64)> = HashSet::new();
        let mut confirm = |seq: u64, ordinal: u64, file: String, optional: bool| -> Result<Option<SidecarRecord>> {
            let key = (file, seq, ordinal);
            if confirmed.contains(&key) {
                return corrupt("Sidecar record is confirmed more than once");
            }
            let Some(record) = by_key.get(&key) else {
                if optional {
                    return Ok(None);
                }
                return corrupt(format!("Missing confirmed sidecar record {} at sequence {seq}", key.0));
            };
            let record = record.clone();
            confirmed.insert(key);
            Ok(Some(record))
        };
        for (marker, _) in &markers {
            let mut writes = Vec::new();
            for op in &marker.writes {
                match op {
                    MainOp::Inline(write) => writes.push((**write).clone()),
                    MainOp::TaskSidecar { id, ordinal } => {
                        let optional = terminal_tasks.contains(id);
                        if let Some(record) = confirm(marker.seq, *ordinal, sidecar_file("task", *id), optional)? {
                            let Payload::Task(task) = record.payload.clone() else {
                                return corrupt(format!(
                                    "Confirmed task sidecar data does not match commit {}",
                                    marker.seq
                                ));
                            };
                            if task.id.0 != *id {
                                return corrupt(format!(
                                    "Confirmed task sidecar data does not match commit {}",
                                    marker.seq
                                ));
                            }
                            if !optional {
                                writes.push(StorageWrite::Task { value: *task });
                            }
                        }
                    }
                    MainOp::DocumentCreate { record: _, ordinal } | MainOp::DocumentChange { id: _, ordinal } => {
                        let id = match op {
                            MainOp::DocumentCreate { record, .. } => record.id.0,
                            MainOp::DocumentChange { id, .. } => *id,
                            MainOp::Inline(_) | MainOp::TaskSidecar { .. } => unreachable!("matched above"),
                        };
                        let reclaimed =
                            retired_current_only.contains(&id) || before_latest_base(id, marker.seq, *ordinal);
                        let content = match confirm(marker.seq, *ordinal, sidecar_file("doc", id), reclaimed)? {
                            Some(SidecarRecord {
                                payload: Payload::Document { id: pid, content },
                                ..
                            }) if pid == id => Some(content),
                            Some(_) => {
                                return corrupt(format!(
                                    "Confirmed document sidecar data does not match commit {}",
                                    marker.seq
                                ));
                            }
                            None => None,
                        };
                        match op {
                            MainOp::DocumentCreate { record, .. } => {
                                if content
                                    .as_ref()
                                    .is_some_and(|c| !matches!(c, DocumentContent::Base { .. }))
                                {
                                    return corrupt(format!(
                                        "Document creation lacks a confirmed base in commit {}",
                                        marker.seq
                                    ));
                                }
                                let content = match content {
                                    Some(content) if !reclaimed => content,
                                    _ => DocumentContent::Base {
                                        version: 1,
                                        value: Map::new(),
                                    },
                                };
                                writes.push(StorageWrite::DocumentCreate {
                                    record: record.clone(),
                                    content,
                                });
                            }
                            _ => {
                                if let (false, Some(content)) = (reclaimed, content) {
                                    writes.push(StorageWrite::DocumentChange {
                                        id: DocumentId(id),
                                        content,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            let applied = self.memory.prepare_commit(writes, Some(Seq(marker.seq)));
            match applied {
                Ok(prepared) => {
                    self.memory.apply_commit(prepared);
                }
                Err(_) => return corrupt(format!("Invalid committed state at sequence {}", marker.seq)),
            }
        }

        let mut reclamations = Vec::new();
        for (file, lines) in &parsed {
            let mut unconfirmed_at = None;
            for (record, start) in lines {
                if confirmed.contains(&(file.clone(), record.seq, record.ordinal)) {
                    if unconfirmed_at.is_some() {
                        return corrupt(format!("Confirmed record follows an unconfirmed tail in {file}"));
                    }
                } else if unconfirmed_at.is_none() {
                    unconfirmed_at = Some(*start);
                }
            }
            if let Some(at) = unconfirmed_at {
                self.fs
                    .truncate(&self.directory.join(file), at)
                    .map_err(|e| file_error(&format!("tail truncation of {file}"), e))?;
            }
            let id = sidecar_id(file, ".jsonl").expect("listed sidecars have IDs");
            let confirmed_lines: Vec<&SidecarRecord> = lines
                .iter()
                .map(|(record, _)| record)
                .filter(|record| confirmed.contains(&(file.clone(), record.seq, record.ordinal)))
                .collect();
            let (task, doc) = (file.starts_with("task-"), file.starts_with("doc-"));
            let emptied = (task && terminal_tasks.contains(&id)) || (doc && retired_current_only.contains(&id));
            let retained: Option<Vec<&SidecarRecord>> = if emptied {
                Some(Vec::new())
            } else if file.starts_with("doc-") && latest_bases.contains_key(&id) {
                Some(
                    confirmed_lines
                        .iter()
                        .copied()
                        .filter(|r| !before_latest_base(id, r.seq, r.ordinal))
                        .collect(),
                )
            } else {
                None
            };
            if let Some(retained) = retained
                && (retained.len() < confirmed_lines.len() || retained.is_empty())
            {
                reclamations.push((
                    file.clone(),
                    retained.iter().map(|record| line(&record.to_json())).collect(),
                ));
            }
        }
        self.reclaim(reclamations);
        self.current_only_documents.extend(current_only);
        self.live_task_sidecars
            .extend(final_task_live.into_iter().filter(|(_, live)| *live).map(|(id, _)| id));
        Ok(())
    }
}

impl<F: JsonlFs> Storage for JsonlStorage<F> {
    fn commit(&mut self, writes: Vec<StorageWrite>) -> Result<Seq> {
        self.assert_usable()?;
        let prepared = self.memory.prepare_commit(writes, None)?;
        let encoded = Self::encode(prepared.seq, &prepared.writes);
        let reclamations = self.plan_reclamations(&prepared.writes, &encoded);
        for (file, content) in &encoded.sidecars {
            if let Err(error) = self.fs.append(&self.directory.join(file), content.as_bytes()) {
                return Err(self.poison(file_error(&format!("append to {file}"), error)));
            }
        }
        if self.fsync {
            for (file, _) in &encoded.sidecars {
                if let Err(error) = self.fs.flush(&self.directory.join(file)) {
                    return Err(self.poison(file_error(&format!("flush of {file}"), error)));
                }
            }
        }
        if let Err(error) = self.fs.append(&self.path(MAIN_FILE), encoded.marker.as_bytes()) {
            return Err(self.poison(file_error(&format!("append to {MAIN_FILE}"), error)));
        }
        let writes = prepared.writes.clone();
        let seq = self.memory.apply_commit(prepared);
        self.adopt_sidecar_state(&writes);
        self.reclaim(reclamations);
        Ok(seq)
    }

    fn mint_id(&mut self) -> Result<u64> {
        self.assert_usable()?;
        self.memory.mint_id()
    }

    fn conversation(&self, id: ConversationId) -> Result<Option<ConversationRecord>> {
        self.store()?.conversation(id)
    }

    fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>> {
        self.store()?.scan_conversations(query, limit, cursor)
    }

    fn entry(&self, id: EntryId) -> Result<Option<StoredEntry>> {
        self.store()?.entry(id)
    }

    fn visible_entry(&self, conversation_id: ConversationId, id: EntryId) -> Result<Option<StoredEntry>> {
        self.store()?.visible_entry(conversation_id, id)
    }

    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
    ) -> Result<Option<EntryRecord>> {
        self.store()?.find_latest_head_marker(conversation_id, at_or_before)
    }

    fn scan_entries(&self, query: &EntryQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<EntryRecord>> {
        self.store()?.scan_entries(query, limit, cursor)
    }

    fn task(&self, id: TaskId) -> Result<Option<TaskRecord>> {
        self.store()?.task(id)
    }

    fn scan_tasks(&self, query: &TaskQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<TaskRecord>> {
        self.store()?.scan_tasks(query, limit, cursor)
    }

    fn submission(&self, id: SubmissionId) -> Result<Option<SubmissionRecord>> {
        self.store()?.submission(id)
    }

    fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<SubmissionRecord>> {
        self.store()?.scan_submissions(query, limit, cursor)
    }

    fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> Result<Option<SubmissionRecord>> {
        self.store()?.submission_by_request(conversation_id, request_id)
    }

    fn find_document(&self, address: &DocumentAddress, at: DocumentPoint) -> Result<Option<DocumentRecord>> {
        self.store()?.find_document(address, at)
    }

    fn document(&self, id: DocumentId, at: DocumentPoint) -> Result<Option<StoredDocument>> {
        self.store()?.document(id, at)
    }

    fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<DocumentRecord>> {
        self.store()?.scan_documents(query, limit, cursor)
    }

    fn close(&mut self) -> Result<()> {
        if !self.closed {
            self.closed = true;
            self.memory.close()?;
        }
        Ok(())
    }
}
