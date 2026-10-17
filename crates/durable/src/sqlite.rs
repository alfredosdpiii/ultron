//! SQLite backend, a port of upstream `storage/sqlite/{storage,migrations,node}.ts`. It writes the same schema
//! (version 1, identical statement text) and row encoding, so files are interchangeable with upstream Pi Durable 1.1.0.

use crate::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentContent, DocumentCreate,
    DocumentId, DocumentPoint, DocumentQuery, DocumentRecord, DocumentScope, EntryId, EntryQuery, EntryRecord,
    JsonObject, MAX_SAFE_ID, Page, Result, ScanOrder, ScanStart, Seq, Storage, StorageError, StorageWrite,
    StoredDocument, StoredEntry, SubmissionId, SubmissionQuery, SubmissionRecord, TaskId, TaskQuery, TaskRecord,
};
use crate::{
    DocumentAction, Table, candidate_next_id, check_global_ids, cursor_after, document_actions, failed, page, request,
    scan_start,
};
use chord_delta::{Op, apply};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, params_from_iter};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

const INITIAL_SCHEMA: &[&str] = &[
    "CREATE TABLE durable_metadata (\n\t\tsingleton INTEGER PRIMARY KEY CHECK (singleton = 1),\n\t\tnext_id TEXT NOT NULL,\n\t\tnext_seq INTEGER NOT NULL\n\t) STRICT",
    "INSERT INTO durable_metadata (singleton, next_id, next_seq) VALUES (1, '2', 1)",
    "CREATE TABLE record_ids (\n\t\tid INTEGER PRIMARY KEY,\n\t\trecord_type TEXT NOT NULL CHECK (record_type IN ('conversation', 'entry', 'task', 'submission', 'document'))\n\t) STRICT",
    "CREATE TABLE conversations (\n\t\tid INTEGER PRIMARY KEY,\n\t\towner_conversation_id INTEGER,\n\t\towner_task_id INTEGER,\n\t\trecord TEXT NOT NULL CHECK (json_valid(record))\n\t) STRICT",
    "CREATE INDEX conversations_by_owner_conversation ON conversations (owner_conversation_id, id)",
    "CREATE INDEX conversations_by_owner_task ON conversations (owner_task_id, id)",
    "CREATE TABLE entries (\n\t\tid INTEGER PRIMARY KEY,\n\t\tconversation_id INTEGER NOT NULL,\n\t\thead INTEGER,\n\t\tcommit_seq INTEGER NOT NULL,\n\t\trecord TEXT NOT NULL CHECK (json_valid(record))\n\t) STRICT",
    "CREATE INDEX entries_by_conversation ON entries (conversation_id, id DESC)",
    "CREATE INDEX entry_heads_by_conversation ON entries (conversation_id, id DESC) WHERE head IS NOT NULL",
    "CREATE TABLE tasks (\n\t\tid INTEGER PRIMARY KEY,\n\t\tconversation_id INTEGER NOT NULL,\n\t\tkind TEXT NOT NULL,\n\t\tstatus TEXT NOT NULL CHECK (status IN ('pending', 'running', 'waiting', 'completing', 'terminal')),\n\t\tabort_requested INTEGER NOT NULL CHECK (abort_requested IN (0, 1)),\n\t\tbackground INTEGER NOT NULL CHECK (background IN (0, 1)),\n\t\trecord TEXT NOT NULL CHECK (json_valid(record))\n\t) STRICT",
    "CREATE INDEX tasks_by_status ON tasks (status, id)",
    "CREATE INDEX tasks_by_conversation ON tasks (conversation_id, id)",
    "CREATE INDEX tasks_by_kind ON tasks (kind, id)",
    "CREATE INDEX tasks_by_abort_requested ON tasks (abort_requested, id)",
    "CREATE INDEX tasks_by_background ON tasks (background, id)",
    "CREATE TABLE submissions (\n\t\tid INTEGER PRIMARY KEY,\n\t\tconversation_id INTEGER NOT NULL,\n\t\trequest_id TEXT,\n\t\tstatus TEXT NOT NULL CHECK (status IN ('queued', 'placed', 'done', 'unanswered')),\n\t\trecord TEXT NOT NULL CHECK (json_valid(record))\n\t) STRICT",
    "CREATE INDEX submissions_by_request ON submissions (conversation_id, request_id)",
    "CREATE INDEX submissions_by_conversation ON submissions (conversation_id, id)",
    "CREATE INDEX submissions_by_status ON submissions (status, id)",
    "CREATE TABLE documents (\n\t\tid INTEGER PRIMARY KEY,\n\t\tkind TEXT NOT NULL,\n\t\tfamily INTEGER NOT NULL CHECK (family IN (0, 1)),\n\t\tkey_value TEXT NOT NULL,\n\t\tscope_kind TEXT NOT NULL CHECK (scope_kind IN ('session', 'conversation', 'task')),\n\t\towner_id INTEGER NOT NULL,\n\t\tcreated_at INTEGER NOT NULL,\n\t\tretired_at INTEGER,\n\t\trecord TEXT NOT NULL CHECK (json_valid(record))\n\t) STRICT",
    "CREATE INDEX documents_by_address\n\t\tON documents (kind, scope_kind, owner_id, family, key_value, created_at DESC, retired_at)",
    "CREATE INDEX documents_by_scope ON documents (scope_kind, owner_id, id)",
    "CREATE INDEX documents_by_scope_kind ON documents (scope_kind, owner_id, kind, id)",
    "CREATE TABLE document_revisions (\n\t\tdocument_id INTEGER NOT NULL,\n\t\tseq INTEGER NOT NULL,\n\t\tkind TEXT NOT NULL CHECK (kind IN ('base', 'delta')),\n\t\tversion INTEGER NOT NULL,\n\t\tcontent TEXT NOT NULL CHECK (json_valid(content)),\n\t\tPRIMARY KEY (document_id, seq)\n\t) STRICT",
    "CREATE INDEX document_revisions_by_kind ON document_revisions (document_id, kind, seq DESC)",
];

/// One schema migration: its version and the statements that bring the previous version to it.
#[derive(Clone, Copy, Debug)]
pub struct SqliteMigration<'a> {
    pub version: i64,
    pub statements: &'a [&'a str],
}

/// Immutable, ordered schema history (`SQLITE_MIGRATIONS`). Append new migrations; never edit released ones.
pub const SQLITE_MIGRATIONS: &[SqliteMigration<'static>] = &[SqliteMigration {
    version: 1,
    statements: INITIAL_SCHEMA,
}];

/// Schema version this backend writes; a file with a newer version is refused.
pub const CURRENT_SQLITE_SCHEMA_VERSION: i64 = 1;

impl From<rusqlite::Error> for StorageError {
    fn from(error: rusqlite::Error) -> Self {
        StorageError::Failed(error.to_string())
    }
}

/// Connection settings for a durable storage file.
#[derive(Clone, Copy, Debug)]
pub struct SqliteOptions {
    /// WAL auto-checkpoint threshold in pages; 0 disables it.
    pub wal_autocheckpoint_pages: u32,
    /// How long to wait for a competing file lock.
    pub busy_timeout: Duration,
}

impl Default for SqliteOptions {
    fn default() -> Self {
        SqliteOptions {
            wal_autocheckpoint_pages: 1_000,
            busy_timeout: Duration::from_millis(5_000),
        }
    }
}

fn decode<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text).or_else(|error| failed(format!("Durable SQLite row is corrupt: {error}")))
}

fn encode<T: Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).expect("records serialize")
}

/// Indexed strings are stored JSON-encoded, as upstream does to keep lone surrogates lossless.
fn indexed(text: &str) -> String {
    encode(text)
}

fn int(id: u64) -> Sql {
    Sql::Integer(id as i64)
}

fn record<T: DeserializeOwned>(conn: &Connection, sql: &str, params: Vec<Sql>) -> Result<Option<T>> {
    let text: Option<String> = conn
        .prepare_cached(sql)?
        .query_row(params_from_iter(params), |row| row.get(0))
        .optional()?;
    text.map(|text| decode(&text)).transpose()
}

fn records<T: DeserializeOwned>(conn: &Connection, sql: &str, params: Vec<Sql>) -> Result<Vec<T>> {
    let mut statement = conn.prepare_cached(sql)?;
    let texts = statement.query_map(params_from_iter(params), |row| row.get::<_, String>(0))?;
    texts.map(|text| decode(&text?)).collect()
}

/// The ID condition, its parameter, and the `ORDER BY` direction of a table scan.
fn scan_sql(start: ScanStart) -> (&'static str, i64, &'static str) {
    match start.order {
        ScanOrder::Ascending => ("id > ?", start.after.unwrap_or(-1), "ASC"),
        ScanOrder::Descending => ("id < ?", start.after.unwrap_or(MAX_SAFE_ID as i64), "DESC"),
    }
}

fn scope_columns(scope: DocumentScope) -> (&'static str, u64) {
    match scope {
        DocumentScope::Session => ("session", 0),
        DocumentScope::Conversation { conversation_id } => ("conversation", conversation_id.0),
        DocumentScope::Task { task_id } => ("task", task_id.0),
    }
}

/// `kind, scope_kind, owner_id, family, key_value` of a document address.
fn address_params(kind: &str, scope: DocumentScope, key: Option<&str>) -> Vec<Sql> {
    let (scope_kind, owner) = scope_columns(scope);
    vec![
        Sql::Text(indexed(kind)),
        Sql::Text(scope_kind.to_owned()),
        int(owner),
        Sql::Integer(i64::from(key.is_some())),
        Sql::Text(indexed(key.unwrap_or(""))),
    ]
}

fn create_address(create: &DocumentCreate) -> Vec<Sql> {
    address_params(&create.kind, create.scope, create.key.as_deref())
}

fn conversation(conn: &Connection, id: u64) -> Result<Option<ConversationRecord>> {
    record(conn, "SELECT record FROM conversations WHERE id = ?", vec![int(id)])
}

fn known_conversation(conn: &Connection, id: ConversationId) -> Result<ConversationRecord> {
    match conversation(conn, id.0)? {
        Some(conversation) => Ok(conversation),
        None => request(format!("Unknown conversation: {}", id.0)),
    }
}

fn parent_conversation(conn: &Connection, id: ConversationId) -> Result<ConversationRecord> {
    conversation(conn, id.0)?.map_or_else(|| failed(format!("Fork parent conversation {} is missing", id.0)), Ok)
}

fn document_record(conn: &Connection, id: u64) -> Result<Option<DocumentRecord>> {
    record(conn, "SELECT record FROM documents WHERE id = ?", vec![int(id)])
}

fn materialize(conn: &Connection, id: DocumentId, at: DocumentPoint) -> Result<Option<StoredDocument>> {
    let Some(record) = document_record(conn, id.0)? else {
        return Ok(None);
    };
    if at != DocumentPoint::Current && record.identity.is_current_only() {
        return request(format!("Document {} does not retain historical content", id.0));
    }
    if !record.is_alive_at(at) {
        return Ok(None);
    }
    let upper = match at {
        DocumentPoint::Current => MAX_SAFE_ID as i64,
        DocumentPoint::At(seq) => seq.0 as i64,
    };
    let base: Option<(i64, i64, String)> = conn
        .prepare_cached(
            "SELECT seq, version, content FROM document_revisions
                WHERE document_id = ? AND kind = 'base' AND seq <= ? ORDER BY seq DESC LIMIT 1",
        )?
        .query_row(params![id.0 as i64, upper], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .optional()?;
    let Some((base_seq, version, content)) = base else {
        return failed(format!("Document {} is missing a required base", id.0));
    };
    let mut value = Value::Object(decode::<JsonObject>(&content)?);
    let mut statement = conn.prepare_cached(
        "SELECT kind, version, content FROM document_revisions
            WHERE document_id = ? AND seq > ? AND seq <= ? ORDER BY seq",
    )?;
    let tail = statement
        .query_map(params![id.0 as i64, base_seq, upper], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (kind, revision_version, content) in &tail {
        if kind != "delta" || *revision_version != version {
            return failed(format!(
                "Document {} crosses a stored version boundary without a base",
                id.0
            ));
        }
        let ops = decode::<Vec<Value>>(content)?
            .iter()
            .map(Op::from_json)
            .collect::<std::result::Result<Vec<_>, _>>()
            .or_else(|error| failed(error.to_string()))?;
        value = apply(value, ops).or_else(|error| failed(error.to_string()))?;
    }
    let Value::Object(value) = value else {
        return failed(format!("Document {} is not an object", id.0));
    };
    Ok(Some(StoredDocument {
        record,
        version: version as u64,
        value,
        deltas_since_base: tail.len(),
    }))
}

fn current_document_id(conn: &Connection, create: &DocumentCreate) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached(
            "SELECT id FROM documents
                WHERE kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ? AND retired_at IS NULL
                LIMIT 1",
        )?
        .query_row(params_from_iter(create_address(create)), |row| row.get(0))
        .optional()?)
}

fn check_document_actions(conn: &Connection, actions: &[(u64, DocumentAction)]) -> Result<()> {
    let mut live_counts: Vec<(Vec<Sql>, i64)> = Vec::new();
    for (id, action) in actions {
        if let Some(copy) = action.copy
            && actions.iter().any(|(other, _)| *other == copy.id.0)
        {
            return failed(format!("Document copy {id} source is changed in the copy batch"));
        }
        let existing = document_record(conn, *id)?;
        if action.create.is_none() && existing.is_none() {
            return failed(format!("Unknown document: {id}"));
        }
        if action.create.is_some() && existing.is_some() {
            return failed(format!("Document {id} already exists"));
        }
        if existing.as_ref().is_some_and(|existing| existing.retired_at.is_some()) {
            return failed(format!("Document {id} is retired"));
        }
        if action.create.is_some()
            && action.copy.is_none()
            && !matches!(action.content, Some(DocumentContent::Base { .. }))
        {
            return failed(format!("Document {id} is created without a base"));
        }
        if let Some(DocumentContent::Delta { version, .. }) = &action.content {
            let previous: Option<i64> = conn
                .prepare_cached(
                    "SELECT version FROM document_revisions WHERE document_id = ? ORDER BY seq DESC LIMIT 1",
                )?
                .query_row([*id as i64], |row| row.get(0))
                .optional()?;
            let Some(previous) = previous else {
                return failed(format!("Document {id} delta has no base"));
            };
            if previous as u64 != *version {
                return failed(format!("Document {id} version transition requires a base"));
            }
        }
        let identity = match (&action.create, &existing) {
            (Some(create), _) => create,
            (None, Some(existing)) => &existing.identity,
            (None, None) => unreachable!("checked above"),
        };
        let key = create_address(identity);
        let index = match live_counts.iter().position(|(k, _)| *k == key) {
            Some(index) => index,
            None => {
                let live = i64::from(current_document_id(conn, identity)?.is_some());
                live_counts.push((key, live));
                live_counts.len() - 1
            }
        };
        if action.retire && existing.is_some() {
            live_counts[index].1 -= 1;
        }
        if action.create.is_some() && !action.retire {
            live_counts[index].1 += 1;
        }
    }
    if live_counts.iter().any(|(_, live)| *live > 1) {
        return failed("Document address already has a current incarnation");
    }
    Ok(())
}

fn claim_id(conn: &Connection, id: u64, table: Table) -> Result<()> {
    conn.prepare_cached("INSERT OR IGNORE INTO record_ids (id, record_type) VALUES (?, ?)")?
        .execute(params![id as i64, table.as_str()])?;
    Ok(())
}

fn status_name<T: Serialize>(status: &T) -> String {
    match serde_json::to_value(status) {
        Ok(Value::String(name)) => name,
        _ => unreachable!("statuses serialize as strings"),
    }
}

fn apply_table_write(conn: &Connection, write: &StorageWrite, seq: Seq) -> Result<()> {
    match write {
        StorageWrite::Conversation { value } => {
            claim_id(conn, value.id.0, Table::Conversation)?;
            conn.prepare_cached(
                "INSERT INTO conversations (id, owner_conversation_id, owner_task_id, record) VALUES (?, ?, ?, ?)",
            )?
            .execute(params![
                value.id.0 as i64,
                value.owner.map(|owner| owner.conversation_id.0 as i64),
                value.owner.map(|owner| owner.task_id.0 as i64),
                encode(value),
            ])?;
        }
        StorageWrite::Entry { value } => {
            claim_id(conn, value.id.0, Table::Entry)?;
            conn.prepare_cached(
                "INSERT INTO entries (id, conversation_id, head, commit_seq, record) VALUES (?, ?, ?, ?, ?)",
            )?
            .execute(params![
                value.id.0 as i64,
                value.conversation_id.0 as i64,
                value.head.map(|head| head.0 as i64),
                seq.0 as i64,
                encode(value),
            ])?;
        }
        StorageWrite::Task { value } => {
            claim_id(conn, value.id.0, Table::Task)?;
            conn.prepare_cached(
                "INSERT INTO tasks (id, conversation_id, kind, status, abort_requested, background, record)
                    VALUES (?, ?, ?, ?, ?, ?, ?)
                    ON CONFLICT(id) DO UPDATE SET conversation_id = excluded.conversation_id, kind = excluded.kind,
                    status = excluded.status, abort_requested = excluded.abort_requested,
                    background = excluded.background, record = excluded.record",
            )?
            .execute(params![
                value.id.0 as i64,
                value.conversation_id.0 as i64,
                indexed(&value.kind),
                status_name(&value.state.status()),
                value.abort_requested,
                value.background,
                encode(value),
            ])?;
        }
        StorageWrite::Submission { value } => {
            claim_id(conn, value.id.0, Table::Submission)?;
            conn.prepare_cached(
                "INSERT INTO submissions (id, conversation_id, request_id, status, record) VALUES (?, ?, ?, ?, ?)
                    ON CONFLICT(id) DO UPDATE SET conversation_id = excluded.conversation_id,
                    request_id = excluded.request_id, status = excluded.status, record = excluded.record",
            )?
            .execute(params![
                value.id.0 as i64,
                value.conversation_id.0 as i64,
                value.request_id.as_deref().map(indexed),
                status_name(&value.status),
                encode(value),
            ])?;
        }
        StorageWrite::DocumentCreate { .. }
        | StorageWrite::DocumentCopy { .. }
        | StorageWrite::DocumentChange { .. }
        | StorageWrite::DocumentRetire { .. } => {}
    }
    Ok(())
}

/// The base a `document.copy` starts from: the source's materialized value, when the source matches the copy.
fn copied_content(
    conn: &Connection,
    id: u64,
    create: &DocumentCreate,
    action: &DocumentAction,
) -> Result<DocumentContent> {
    let Some(source) = action.copy else {
        unreachable!("called for copies")
    };
    let resolved = (|| -> Result<DocumentContent> {
        let Some(stored) = materialize(conn, source.id, source.at)? else {
            return failed(format!("Fork source document {} cannot be read", source.id.0));
        };
        let from = &stored.record.identity;
        if !matches!(from.scope, DocumentScope::Conversation { .. })
            || !matches!(create.scope, DocumentScope::Conversation { .. })
            || from.kind != create.kind
            || from.key != create.key
            || from.history != create.history
            || from.fork != create.fork
        {
            return failed(format!(
                "Fork source document {} does not match the copied record",
                source.id.0
            ));
        }
        Ok(DocumentContent::Base {
            version: stored.version,
            value: stored.value,
        })
    })();
    resolved.or_else(|_| failed(format!("Document copy {id} was rejected")))
}

fn apply_document_actions(conn: &Connection, actions: &[(u64, DocumentAction)], seq: Seq) -> Result<()> {
    for (id, action) in actions {
        let id = *id;
        let mut content = action.content.clone();
        let mut record = match &action.create {
            Some(create) => {
                if action.copy.is_some() {
                    content = Some(copied_content(conn, id, create, action)?);
                }
                let record = DocumentRecord {
                    identity: create.clone(),
                    created_at: seq,
                    retired_at: action.retire.then_some(seq),
                };
                claim_id(conn, id, Table::Document)?;
                let mut values = create_address(create);
                let (scope_kind, owner, family, key_value) =
                    (values.remove(1), values.remove(1), values.remove(1), values.remove(1));
                conn.prepare_cached(
                    "INSERT INTO documents
                        (id, kind, family, key_value, scope_kind, owner_id, created_at, retired_at, record)
                        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )?
                .execute(params![
                    id as i64,
                    values.remove(0),
                    family,
                    key_value,
                    scope_kind,
                    owner,
                    seq.0 as i64,
                    action.retire.then_some(seq.0 as i64),
                    encode(&record),
                ])?;
                record
            }
            None => document_record(conn, id)?.expect("checked: document exists"),
        };
        if let Some(content) = content {
            if matches!(content, DocumentContent::Base { .. }) && record.identity.is_current_only() {
                conn.prepare_cached("DELETE FROM document_revisions WHERE document_id = ?")?
                    .execute([id as i64])?;
            }
            let (kind, version, encoded) = match &content {
                DocumentContent::Base { version, value } => ("base", *version, encode(value)),
                DocumentContent::Delta { version, ops } => (
                    "delta",
                    *version,
                    encode(&ops.iter().map(Op::to_json).collect::<Vec<_>>()),
                ),
            };
            conn.prepare_cached(
                "INSERT INTO document_revisions (document_id, seq, kind, version, content) VALUES (?, ?, ?, ?, ?)",
            )?
            .execute(params![id as i64, seq.0 as i64, kind, version as i64, encoded])?;
        }
        if action.retire {
            if action.create.is_none() {
                record.retired_at = Some(seq);
                conn.prepare_cached("UPDATE documents SET retired_at = ?, record = ? WHERE id = ?")?
                    .execute(params![seq.0 as i64, encode(&record), id as i64])?;
            }
            if record.identity.is_current_only() {
                conn.prepare_cached("DELETE FROM document_revisions WHERE document_id = ?")?
                    .execute([id as i64])?;
            }
        }
    }
    Ok(())
}

fn metadata(conn: &Connection) -> Result<(u64, u64)> {
    let row: Option<(String, i64)> = conn
        .prepare_cached("SELECT next_id, next_seq FROM durable_metadata WHERE singleton = 1")?
        .query_row([], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    let Some((next_id, next_seq)) = row else {
        return failed("Durable SQLite metadata is missing");
    };
    match (next_id.parse::<u64>(), u64::try_from(next_seq)) {
        (Ok(next_id), Ok(next_seq)) => Ok((next_id, next_seq)),
        _ => failed("Durable SQLite metadata is corrupt"),
    }
}

/// `applySqliteMigrations`: every pending migration of `migrations` (contiguous versions from 1) in one immediate
/// transaction with the schema bootstrap, so a failure leaves the database as it was.
pub fn apply_sqlite_migrations(conn: &mut Connection, migrations: &[SqliteMigration<'_>]) -> Result<()> {
    if migrations
        .iter()
        .enumerate()
        .any(|(index, m)| m.version != index as i64 + 1)
    {
        return failed("Durable SQLite migrations must have contiguous versions starting at 1");
    }
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS durable_schema (
			singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
			version INTEGER NOT NULL CHECK (version >= 0)
		) STRICT",
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO durable_schema (singleton, version) VALUES (1, 0)",
        [],
    )?;
    let version: Option<i64> = tx
        .query_row("SELECT version FROM durable_schema WHERE singleton = 1", [], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(version) = version else {
        return failed("Durable SQLite schema metadata is missing");
    };
    let current = migrations.last().map_or(0, |m| m.version);
    if version > current {
        return failed(format!(
            "Durable SQLite schema version {version} is newer than supported version {current}"
        ));
    }
    for migration in migrations.iter().filter(|m| m.version > version) {
        for statement in migration.statements {
            tx.execute_batch(statement)?;
        }
        tx.execute(
            "UPDATE durable_schema SET version = ? WHERE singleton = 1",
            [migration.version],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// SQLite implementation of [`Storage`] over one connection.
pub struct SqliteStorage {
    conn: Option<Connection>,
    next_id: u64,
}

impl SqliteStorage {
    /// Open or create file-backed storage (`":memory:"` for a private in-memory database) in WAL mode.
    pub fn open(path: impl AsRef<Path>, options: SqliteOptions) -> Result<SqliteStorage> {
        let path = path.as_ref();
        if path != Path::new(":memory:")
            && let Some(parent) = path.parent()
        {
            std::fs::create_dir_all(parent).or_else(|error| failed(error.to_string()))?;
        }
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(options.busy_timeout)?;
        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA wal_autocheckpoint = {};",
            options.wal_autocheckpoint_pages
        ))?;
        apply_sqlite_migrations(&mut conn, SQLITE_MIGRATIONS)?;
        let (next_id, _) = metadata(&conn)?;
        Ok(SqliteStorage {
            conn: Some(conn),
            next_id,
        })
    }

    fn conn(&self) -> Result<&Connection> {
        match &self.conn {
            Some(conn) => Ok(conn),
            None => failed("SqliteStorage is closed"),
        }
    }

    fn scan<T: DeserializeOwned>(
        &self,
        table: &str,
        start: ScanStart,
        mut clauses: Vec<String>,
        mut params: Vec<Sql>,
        limit: usize,
    ) -> Result<Vec<T>> {
        let (clause, param, direction) = scan_sql(start);
        clauses.insert(0, clause.to_owned());
        params.insert(0, Sql::Integer(param));
        params.push(Sql::Integer(limit as i64 + 1));
        let sql = format!(
            "SELECT record FROM {table} WHERE {} ORDER BY id {direction} LIMIT ?",
            clauses.join(" AND ")
        );
        records(self.conn()?, &sql, params)
    }

    fn entries_descending(&self, query: &EntryQuery, limit: usize, after: Option<i64>) -> Result<Vec<EntryRecord>> {
        let conn = self.conn()?;
        let mut conversation = known_conversation(conn, query.conversation_id)?;
        let mut upper = query.max_entry_id.map(|id| id.0 as i64);
        if let Some(after) = after {
            upper = Some(upper.unwrap_or(MAX_SAFE_ID as i64).min(after - 1));
        }
        let mut values = Vec::new();
        loop {
            values.extend(self.entry_segment(
                conversation.id,
                query.min_entry_id,
                None,
                upper,
                limit + 1 - values.len(),
                "DESC",
            )?);
            let Some(parent) = conversation.parent else { break };
            if values.len() > limit {
                break;
            }
            let at = parent.at.0 as i64;
            upper = Some(upper.map_or(at, |upper| upper.min(at)));
            if query.min_entry_id.is_some_and(|min| upper < Some(min.0 as i64)) {
                break;
            }
            conversation = parent_conversation(conn, parent.conversation_id)?;
        }
        Ok(values)
    }

    /// Oldest first: the fork chain's segments from the root conversation forward, each up to its fork point.
    fn entries_ascending(&self, query: &EntryQuery, limit: usize, after: Option<i64>) -> Result<Vec<EntryRecord>> {
        let conn = self.conn()?;
        let mut conversation = known_conversation(conn, query.conversation_id)?;
        let mut upper = query.max_entry_id.map(|id| id.0 as i64);
        let mut segments = Vec::new();
        loop {
            segments.push((conversation.id, upper));
            let Some(parent) = conversation.parent else { break };
            let at = parent.at.0 as i64;
            upper = Some(upper.map_or(at, |upper| upper.min(at)));
            if query.min_entry_id.is_some_and(|min| upper < Some(min.0 as i64)) {
                break;
            }
            conversation = parent_conversation(conn, parent.conversation_id)?;
        }
        let mut lower = query.min_entry_id.map(|id| id.0 as i64);
        if let Some(after) = after {
            lower = Some(lower.unwrap_or(after + 1).max(after + 1));
        }
        let mut values = Vec::new();
        for (id, upper) in segments.into_iter().rev() {
            values.extend(self.entry_segment(id, None, lower, upper, limit + 1 - values.len(), "ASC")?);
            if values.len() > limit {
                break;
            }
        }
        Ok(values)
    }

    fn entry_segment(
        &self,
        conversation_id: ConversationId,
        min: Option<EntryId>,
        lower: Option<i64>,
        upper: Option<i64>,
        take: usize,
        direction: &str,
    ) -> Result<Vec<EntryRecord>> {
        let mut clauses = vec!["conversation_id = ?"];
        let mut params = vec![int(conversation_id.0)];
        if let Some(lower) = min.map(|min| min.0 as i64).or(lower) {
            clauses.push("id >= ?");
            params.push(Sql::Integer(lower));
        }
        if let Some(upper) = upper {
            clauses.push("id <= ?");
            params.push(Sql::Integer(upper));
        }
        params.push(Sql::Integer(take as i64));
        let sql = format!(
            "SELECT record FROM entries WHERE {} ORDER BY id {direction} LIMIT ?",
            clauses.join(" AND ")
        );
        records(self.conn()?, &sql, params)
    }
}

impl Storage for SqliteStorage {
    fn commit(&mut self, writes: Vec<StorageWrite>) -> Result<Seq> {
        let next_id = self.next_id;
        let Some(conn) = self.conn.as_mut() else {
            return failed("SqliteStorage is closed");
        };
        let actions = document_actions(&writes)?;
        let candidate = candidate_next_id(next_id, &writes);
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (stored_next_id, next_seq) = metadata(&tx)?;
        let seq = Seq(next_seq);
        check_global_ids(&writes, |id| {
            let table: Option<String> = tx
                .prepare_cached("SELECT record_type FROM record_ids WHERE id = ?")?
                .query_row([id as i64], |row| row.get(0))
                .optional()?;
            table.map(|table| Table::parse(&table)).transpose()
        })?;
        check_document_actions(&tx, &actions)?;
        for write in &writes {
            apply_table_write(&tx, write, seq)?;
        }
        apply_document_actions(&tx, &actions, seq)?;
        tx.execute(
            "UPDATE durable_metadata SET next_id = ?, next_seq = ? WHERE singleton = 1",
            params![stored_next_id.max(candidate).to_string(), next_seq as i64 + 1],
        )?;
        tx.commit()?;
        self.next_id = self.next_id.max(candidate);
        Ok(seq)
    }

    fn mint_id(&mut self) -> Result<u64> {
        self.conn()?;
        if self.next_id > MAX_SAFE_ID {
            return failed("ID space is exhausted");
        }
        self.next_id += 1;
        Ok(self.next_id - 1)
    }

    fn conversation(&self, id: ConversationId) -> Result<Option<ConversationRecord>> {
        conversation(self.conn()?, id.0)
    }

    fn scan_conversations(
        &self,
        query: &ConversationQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>> {
        self.conn()?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let (mut clauses, mut params) = (Vec::new(), Vec::new());
        if let Some(owner) = query.owner_conversation_id {
            clauses.push("owner_conversation_id = ?".to_owned());
            params.push(int(owner.0));
        }
        if let Some(owner) = query.owner_task_id {
            clauses.push("owner_task_id = ?".to_owned());
            params.push(int(owner.0));
        }
        let values = self.scan("conversations", start, clauses, params, limit)?;
        Ok(page(values, limit, start.order, |value: &ConversationRecord| {
            value.id.0
        }))
    }

    fn entry(&self, id: EntryId) -> Result<Option<StoredEntry>> {
        let row: Option<(String, i64)> = self
            .conn()?
            .prepare_cached("SELECT record, commit_seq FROM entries WHERE id = ?")?
            .query_row([id.0 as i64], |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        row.map(|(entry, seq)| {
            Ok(StoredEntry {
                entry: decode(&entry)?,
                commit_seq: Seq(seq as u64),
            })
        })
        .transpose()
    }

    fn visible_entry(&self, conversation_id: ConversationId, id: EntryId) -> Result<Option<StoredEntry>> {
        let conn = self.conn()?;
        let mut conversation = known_conversation(conn, conversation_id)?;
        let Some(stored) = self.entry(id)? else { return Ok(None) };
        let mut upper = u64::MAX;
        while conversation.id != stored.entry.conversation_id {
            let Some(parent) = conversation.parent else {
                return Ok(None);
            };
            upper = upper.min(parent.at.0);
            conversation = parent_conversation(conn, parent.conversation_id)?;
        }
        Ok((stored.entry.id.0 <= upper).then_some(stored))
    }

    fn find_latest_head_marker(
        &self,
        conversation_id: ConversationId,
        at_or_before: Option<EntryId>,
    ) -> Result<Option<EntryRecord>> {
        let conn = self.conn()?;
        let mut conversation = known_conversation(conn, conversation_id)?;
        let mut upper = at_or_before.map(|id| id.0);
        loop {
            let found = match upper {
                None => record(
                    conn,
                    "SELECT record FROM entries WHERE conversation_id = ? AND head IS NOT NULL ORDER BY id DESC LIMIT 1",
                    vec![int(conversation.id.0)],
                )?,
                Some(upper) => record(
                    conn,
                    "SELECT record FROM entries WHERE conversation_id = ? AND head IS NOT NULL AND id <= ? ORDER BY id DESC LIMIT 1",
                    vec![int(conversation.id.0), int(upper)],
                )?,
            };
            if found.is_some() {
                return Ok(found);
            }
            let Some(parent) = conversation.parent else {
                return Ok(None);
            };
            upper = Some(upper.map_or(parent.at.0, |upper| upper.min(parent.at.0)));
            conversation = parent_conversation(conn, parent.conversation_id)?;
        }
    }

    fn scan_entries(&self, query: &EntryQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<EntryRecord>> {
        self.conn()?;
        let ScanStart { order, after } = scan_start(query.order, cursor, ScanOrder::Descending)?;
        let values = match order {
            ScanOrder::Ascending => self.entries_ascending(query, limit, after)?,
            ScanOrder::Descending => self.entries_descending(query, limit, after)?,
        };
        Ok(page(values, limit, order, |entry: &EntryRecord| entry.id.0))
    }

    fn task(&self, id: TaskId) -> Result<Option<TaskRecord>> {
        record(self.conn()?, "SELECT record FROM tasks WHERE id = ?", vec![int(id.0)])
    }

    fn scan_tasks(&self, query: &TaskQuery, limit: usize, cursor: Option<&Cursor>) -> Result<Page<TaskRecord>> {
        self.conn()?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let (mut clauses, mut params) = (Vec::new(), Vec::new());
        let mut filter = |clause: &str, param: Sql| {
            clauses.push(clause.to_owned());
            params.push(param);
        };
        if let Some(id) = query.conversation_id {
            filter("conversation_id = ?", int(id.0));
        }
        if let Some(kind) = &query.kind {
            filter("kind = ?", Sql::Text(indexed(kind)));
        }
        if let Some(status) = query.status {
            filter("status = ?", Sql::Text(status_name(&status)));
        }
        if let Some(abort) = query.abort_requested {
            filter("abort_requested = ?", Sql::Integer(i64::from(abort)));
        }
        if let Some(background) = query.background {
            filter("background = ?", Sql::Integer(i64::from(background)));
        }
        let values = self.scan("tasks", start, clauses, params, limit)?;
        Ok(page(values, limit, start.order, |task: &TaskRecord| task.id.0))
    }

    fn submission(&self, id: SubmissionId) -> Result<Option<SubmissionRecord>> {
        record(
            self.conn()?,
            "SELECT record FROM submissions WHERE id = ?",
            vec![int(id.0)],
        )
    }

    fn scan_submissions(
        &self,
        query: &SubmissionQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<SubmissionRecord>> {
        self.conn()?;
        let start = scan_start(query.order, cursor, ScanOrder::Ascending)?;
        let (mut clauses, mut params) = (Vec::new(), Vec::new());
        if let Some(id) = query.conversation_id {
            clauses.push("conversation_id = ?".to_owned());
            params.push(int(id.0));
        }
        if let Some(status) = query.status {
            clauses.push("status = ?".to_owned());
            params.push(Sql::Text(status_name(&status)));
        }
        let values = self.scan("submissions", start, clauses, params, limit)?;
        Ok(page(values, limit, start.order, |submission: &SubmissionRecord| {
            submission.id.0
        }))
    }

    fn submission_by_request(
        &self,
        conversation_id: ConversationId,
        request_id: &str,
    ) -> Result<Option<SubmissionRecord>> {
        record(
            self.conn()?,
            "SELECT record FROM submissions WHERE conversation_id = ? AND request_id = ?",
            vec![int(conversation_id.0), Sql::Text(indexed(request_id))],
        )
    }

    fn find_document(&self, address: &DocumentAddress, at: DocumentPoint) -> Result<Option<DocumentRecord>> {
        let mut params = address_params(&address.kind, address.scope, address.key.as_deref());
        let sql = match at {
            DocumentPoint::Current => {
                "SELECT record FROM documents
                    WHERE kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ?
                    AND retired_at IS NULL ORDER BY created_at DESC LIMIT 1"
            }
            DocumentPoint::At(seq) => {
                params.extend([int(seq.0), int(seq.0)]);
                "SELECT record FROM documents
                    WHERE kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ?
                    AND created_at <= ? AND (retired_at IS NULL OR retired_at > ?)
                    ORDER BY created_at DESC LIMIT 1"
            }
        };
        record(self.conn()?, sql, params)
    }

    fn document(&self, id: DocumentId, at: DocumentPoint) -> Result<Option<StoredDocument>> {
        // The record and revision reads must observe one committed state; another writer can replace the base.
        let tx = self.conn()?.unchecked_transaction()?;
        materialize(&tx, id, at)
    }

    fn scan_documents(
        &self,
        query: &DocumentQuery,
        limit: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<DocumentRecord>> {
        let conn = self.conn()?;
        let (scope_kind, owner) = scope_columns(query.scope);
        let after = match cursor {
            Some(cursor) => cursor_after(cursor)?,
            None => None,
        };
        let mut clauses = vec!["scope_kind = ?", "owner_id = ?", "id > ?"];
        let mut params = vec![
            Sql::Text(scope_kind.to_owned()),
            int(owner),
            Sql::Integer(after.unwrap_or(-1)),
        ];
        if let Some(kind) = &query.kind {
            clauses.push("kind = ?");
            params.push(Sql::Text(indexed(kind)));
        }
        match query.at {
            DocumentPoint::Current => clauses.push("retired_at IS NULL"),
            DocumentPoint::At(seq) => {
                clauses.extend(["created_at <= ?", "(retired_at IS NULL OR retired_at > ?)"]);
                params.extend([int(seq.0), int(seq.0)]);
            }
        }
        params.push(Sql::Integer(limit as i64 + 1));
        let sql = format!(
            "SELECT record FROM documents WHERE {} ORDER BY id LIMIT ?",
            clauses.join(" AND ")
        );
        let values = records(conn, &sql, params)?;
        Ok(page(values, limit, ScanOrder::Ascending, |record: &DocumentRecord| {
            record.identity.id.0
        }))
    }

    fn close(&mut self) -> Result<()> {
        let Some(conn) = self.conn.take() else { return Ok(()) };
        let checkpoint = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        conn.close().map_err(|(_, error)| error)?;
        Ok(checkpoint?)
    }
}
