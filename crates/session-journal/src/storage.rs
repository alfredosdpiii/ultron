//! `jsonl/storage.ts`: one format-4 session file. Opening replays every complete transaction (a torn final line is
//! dropped and the file rewritten without it); each commit appends one line. A legacy v3 file is read through
//! [`LegacyV3Source`] and rewritten as format 4 by its first non-empty commit, which also records the imported
//! usage as one adjustment row (`details.source = "v3-import"`).

use crate::Clock;
use crate::codec::{self, JSONL_STORAGE_VERSION, ParsedHeader};
use crate::io;
use crate::legacy_v3::LegacyV3Source;
use crate::state::{CommitResult, State, Write};
use crate::usage::num;
use serde_json::{Map, Value, json};
use session_file::js;
use std::path::{Path, PathBuf};

enum Backing {
    V4,
    V3(Box<LegacyV3Source>),
}

/// `JsonlStorage`.
pub struct JsonlStorage {
    path: PathBuf,
    header: Map<String, Value>,
    backing: Backing,
    state: State,
}

fn header_line(header: &Map<String, Value>) -> String {
    format!("{}\n", js::stringify(&Value::Object(header.clone())))
}

impl JsonlStorage {
    /// `JsonlStorage.create`: publish the header and the initial writes (one transaction when non-empty).
    pub fn create(
        path: &Path,
        header: Map<String, Value>,
        initial: &[Write],
        clock: &mut dyn Clock,
    ) -> Result<Self, String> {
        let mut state = State::default();
        let (writes, _) = state.prepare(initial, clock.now())?;
        let mut content = header_line(&header);
        if !writes.is_empty() {
            content.push_str(&codec::serialize_transaction(&writes));
            content.push('\n');
        }
        io::publish(path, &content)?;
        state.apply(&writes);
        Ok(JsonlStorage {
            path: path.to_path_buf(),
            header,
            backing: Backing::V4,
            state,
        })
    }

    /// `JsonlStorage.open`: a format-4 file, or a legacy v3 file read as format-4 writes.
    pub fn open(path: &Path, clock: &mut dyn Clock) -> Result<Self, String> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read JSONL storage {}: {e}", path.display()))?;
        let (lines, torn) = io::split_complete_lines(&content);
        let Some(first) = lines.first().filter(|l| !l.is_empty()) else {
            return Err(format!("Invalid JSONL storage {}: missing header", path.display()));
        };
        let header = match codec::parse_header(first) {
            Ok(ParsedHeader::V4(header)) => header,
            Ok(ParsedHeader::V3(_)) => return Self::open_legacy_v3(path, clock),
            Err(_) => return Err(format!("Invalid JSONL storage {}: invalid header", path.display())),
        };
        let id = codec::header_str(&header, "id").unwrap_or("");
        let storage_version = codec::header_i64(&header, "storageVersion").unwrap_or(0);
        if storage_version != JSONL_STORAGE_VERSION {
            return Err(format!(
                "Session {id} uses unsupported storage version {storage_version}"
            ));
        }
        let mut state = State::default();
        for (index, line) in lines.iter().enumerate().skip(1) {
            let replay = codec::parse_transaction(line).and_then(|writes| {
                state.validate(&writes)?;
                state.apply(&writes);
                Ok(())
            });
            if let Err(cause) = replay {
                return Err(format!(
                    "Invalid JSONL storage {}: line {}: {cause}",
                    path.display(),
                    index + 1
                ));
            }
        }
        if let Some(next) = codec::header_i64(&header, "nextSeq") {
            state.advance_next_seq(next)?;
        }
        if torn {
            io::publish(path, &format!("{}\n", lines.join("\n")))?;
        }
        Ok(JsonlStorage {
            path: path.to_path_buf(),
            header,
            backing: Backing::V4,
            state,
        })
    }

    fn open_legacy_v3(path: &Path, clock: &mut dyn Clock) -> Result<Self, String> {
        let source = LegacyV3Source::read(path, clock)?;
        let mut state = State::default();
        for write in source.writes(None)? {
            let writes = [write];
            state.validate(&writes)?;
            state.apply(&writes);
        }
        Ok(JsonlStorage {
            path: path.to_path_buf(),
            header: source.header_with_next_seq(),
            backing: Backing::V3(Box::new(source)),
            state,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn header(&self) -> &Map<String, Value> {
        &self.header
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn is_legacy_v3(&self) -> bool {
        matches!(self.backing, Backing::V3(_))
    }

    /// `getStats`: a legacy file reports its imported usage until it is upgraded.
    pub fn stats(&self) -> Value {
        let mut stats = self.state.stats();
        if let Backing::V3(source) = &self.backing {
            stats["usage"] = source.imported_usage.clone();
        }
        stats
    }

    /// `commit`: one appended transaction (nothing is written for no writes).
    pub fn commit(&mut self, writes: &[Write], clock: &mut dyn Clock) -> Result<CommitResult, String> {
        if let Backing::V3(source) = &self.backing
            && !writes.is_empty()
        {
            let source = source.clone();
            return self.upgrade_legacy_v3(&source, writes, clock);
        }
        let timestamp = clock.now();
        let (committed, first_seq) = self.state.prepare(writes, timestamp)?;
        if !committed.is_empty() {
            io::append(&self.path, &format!("{}\n", codec::serialize_transaction(&committed)))?;
        }
        self.state.apply(&committed);
        Ok(CommitResult {
            first_seq,
            seqs: committed.iter().map(crate::state::seq_of).collect(),
            timestamp,
            stats: self.stats(),
        })
    }

    /// `upgradeLegacyV3ToV4`: rewrite the file as format 4 with the usage adjustment and the caller's writes as
    /// its last transaction.
    fn upgrade_legacy_v3(
        &mut self,
        source: &LegacyV3Source,
        writes: &[Write],
        clock: &mut dyn Clock,
    ) -> Result<CommitResult, String> {
        let timestamp = clock.now();
        let mut row = Map::new();
        row.insert("id".into(), clock.uuidv7(timestamp).into());
        row.insert("usage".into(), source.imported_usage.clone());
        row.insert("adjustment".into(), true.into());
        row.insert("details".into(), json!({"source": "v3-import"}));
        let mut all = vec![Write::Usage(row)];
        all.extend(writes.iter().cloned());
        let (committed, first_seq) = self.state.prepare(&all, timestamp)?;
        let mut header = self.header.clone();
        header.insert("nextSeq".into(), num((first_seq + committed.len() as i64) as f64));
        let mut content = header_line(&header);
        for write in source.writes(None)? {
            content.push_str(&codec::serialize_transaction(&[write]));
            content.push('\n');
        }
        content.push_str(&codec::serialize_transaction(&committed));
        content.push('\n');
        io::publish(&self.path, &content)?;
        self.state.apply(&committed);
        self.header = header;
        self.backing = Backing::V4;
        Ok(CommitResult {
            first_seq: first_seq + 1,
            seqs: committed.iter().skip(1).map(crate::state::seq_of).collect(),
            timestamp,
            stats: self.stats(),
        })
    }
}
