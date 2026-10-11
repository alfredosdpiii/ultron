//! Port of upstream `test/sqlite-storage.test.ts` ("Pico SqliteStorage"): behavior specific to the SQLite file.

use durable::{
    DocumentContent, DocumentCreate, DocumentFork, DocumentHistory, DocumentId, DocumentPoint, DocumentScope, EntryId,
    EntryRecord, ROOT_CONVERSATION_ID, Seq, SqliteOptions, SqliteStorage, Storage, StorageWrite, TaskId, TaskRecord,
};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn create(options: SqliteOptions) -> (TempDir, PathBuf, SqliteStorage) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("storage.sqlite");
    let storage = SqliteStorage::open(&path, options).unwrap();
    (directory, path, storage)
}

fn reopen(path: &Path) -> SqliteStorage {
    SqliteStorage::open(path, SqliteOptions::default()).unwrap()
}

fn read_only(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

fn scalar(db: &Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |row| row.get(0)).unwrap()
}

fn entry(id: u64, data: Option<Value>) -> EntryRecord {
    EntryRecord {
        id: EntryId(id),
        conversation_id: ROOT_CONVERSATION_ID,
        kind: "message".to_owned(),
        model: None,
        data,
        head: None,
        edits: None,
        by_task_id: None,
    }
}

fn pending_task(id: u64) -> TaskRecord {
    serde_json::from_value(json!({
        "id": id, "conversationId": 1, "kind": "test.task", "version": 1, "input": null,
        "state": { "status": "pending", "checkpoint": { "phase": "ready" } },
        "background": false, "abortRequested": false,
    }))
    .unwrap()
}

fn create_root(storage: &mut impl Storage) -> Seq {
    storage
        .commit(writes(json!([{ "type": "conversation", "value": { "id": 1 } }])))
        .unwrap()
}

fn writes(value: Value) -> Vec<StorageWrite> {
    serde_json::from_value(value).unwrap()
}

fn change(storage: &mut impl Storage, id: u64, content: Value) -> Seq {
    storage
        .commit(writes(
            json!([{ "type": "document.change", "id": id, "content": content }]),
        ))
        .unwrap()
}

fn create_document(storage: &mut impl Storage, id: u64, kind: &str, scope: Value, value: Value) -> Seq {
    storage
        .commit(writes(json!([{
            "type": "document.create",
            "record": { "id": id, "kind": kind, "scope": scope },
            "content": { "kind": "base", "version": 1, "value": value },
        }])))
        .unwrap()
}

fn revision_count(path: &Path, id: u64) -> i64 {
    read_only(path)
        .query_row(
            "SELECT count(*) FROM document_revisions WHERE document_id = ?",
            [id as i64],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn sqlite_persists_records_sequence_and_id_allocation_across_reopen() {
    let (_directory, path, mut storage) = create(SqliteOptions::default());
    assert_eq!(create_root(&mut storage), Seq(1));
    let entry_id = storage.mint_id().unwrap();
    let seq = storage
        .commit(vec![StorageWrite::Entry {
            value: entry(entry_id, None),
        }])
        .unwrap();
    assert_eq!(seq, Seq(2));
    storage.close().unwrap();

    let mut reopened = reopen(&path);
    let stored = reopened.entry(EntryId(entry_id)).unwrap().unwrap();
    assert_eq!((stored.entry, stored.commit_seq), (entry(entry_id, None), Seq(2)));
    assert_eq!(reopened.mint_id().unwrap(), entry_id + 1);
    let task = pending_task(reopened.mint_id().unwrap());
    assert_eq!(
        reopened.commit(vec![StorageWrite::Task { value: task }]).unwrap(),
        Seq(3)
    );
}

#[test]
fn sqlite_rejects_persisted_metadata_corruption_on_reopen() {
    let (_directory, path, mut storage) = create(SqliteOptions::default());
    storage.close().unwrap();
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM durable_metadata", [])
        .unwrap();
    let error = SqliteStorage::open(&path, SqliteOptions::default()).err().unwrap();
    assert_eq!(error.message(), "Durable SQLite metadata is missing");
}

#[test]
fn sqlite_refuses_a_newer_schema_version() {
    let (_directory, path, mut storage) = create(SqliteOptions::default());
    storage.close().unwrap();
    Connection::open(&path)
        .unwrap()
        .execute("UPDATE durable_schema SET version = 2", [])
        .unwrap();
    let error = SqliteStorage::open(&path, SqliteOptions::default()).err().unwrap();
    assert_eq!(
        error.message(),
        "Durable SQLite schema version 2 is newer than supported version 1"
    );
}

#[test]
fn sqlite_rejects_a_document_whose_required_base_is_missing() {
    let (_directory, path, mut storage) = create(SqliteOptions::default());
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    create_document(
        &mut storage,
        id,
        "corrupt",
        json!({ "kind": "session" }),
        json!({ "retained": true }),
    );
    Connection::open(&path)
        .unwrap()
        .execute("DELETE FROM document_revisions WHERE document_id = ?", [id as i64])
        .unwrap();
    let error = storage.document(DocumentId(id), DocumentPoint::Current).unwrap_err();
    assert_eq!(
        (error.name(), error.message()),
        ("Error", format!("Document {id} is missing a required base").as_str())
    );
}

#[test]
fn sqlite_replays_root_replacements_and_edits_and_rejects_corrupt_operations() {
    let (_directory, path, mut storage) = create(SqliteOptions::default());
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    create_document(
        &mut storage,
        id,
        "replay",
        json!({ "kind": "session" }),
        json!({ "nested": { "value": 1 }, "rows": [] }),
    );
    let delta = |ops: Value| json!({ "kind": "delta", "version": 1, "ops": ops });
    change(
        &mut storage,
        id,
        delta(json!([["r", { "nested": { "value": 2 }, "rows": [{ "id": 1 }] }]])),
    );
    change(
        &mut storage,
        id,
        delta(json!([["s", ["nested", "value"], 3], ["p", ["rows"], 1, 0, [{ "id": 2 }]], ["m", ["rows"], [1, 0]]])),
    );
    change(&mut storage, id, delta(json!([["s", ["nested", "value"], 4]])));
    let expected = json!({ "nested": { "value": 4 }, "rows": [{ "id": 2 }, { "id": 1 }] });
    let current = storage
        .document(DocumentId(id), DocumentPoint::Current)
        .unwrap()
        .unwrap();
    assert_eq!(Value::Object(current.value), expected);

    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE document_revisions SET content = ? WHERE document_id = ? AND seq =
                (SELECT max(seq) FROM document_revisions WHERE document_id = ?)",
            rusqlite::params![r#"[["unknown"]]"#, id as i64, id as i64],
        )
        .unwrap();
    let error = storage.document(DocumentId(id), DocumentPoint::Current).unwrap_err();
    assert!(error.message().contains("unknown op verb"), "{error}");
}

#[test]
fn sqlite_rolls_rows_and_sequence_allocation_back_as_one_transaction() {
    let (_directory, path, mut storage) = create(SqliteOptions::default());
    create_root(&mut storage);
    let transient = storage.mint_id().unwrap();
    let transient_task = storage.mint_id().unwrap();
    let session_document = storage.mint_id().unwrap();
    create_document(
        &mut storage,
        session_document,
        "session",
        json!({ "kind": "session" }),
        json!({}),
    );
    let copy = storage.mint_id().unwrap();
    // Upstream fails on a circular task input, which Rust values cannot express; this copy fails after the rows
    // above it are written, because a session document cannot be a fork source.
    let error = storage
        .commit(writes(json!([
            { "type": "entry", "value": { "id": transient, "conversationId": 1, "kind": "message" } },
            { "type": "task", "value": pending_task(transient_task) },
            {
                "type": "document.copy",
                "record": { "id": copy, "kind": "session", "scope": { "kind": "conversation", "conversationId": 1 },
                            "history": "latest", "fork": "current" },
                "source": { "id": session_document, "at": "current" },
            },
        ])))
        .unwrap_err();
    assert_eq!(error.message(), format!("Document copy {copy} was rejected"));
    assert_eq!(storage.entry(EntryId(transient)).unwrap(), None);
    assert_eq!(storage.task(TaskId(transient_task)).unwrap(), None);
    let committed = storage.mint_id().unwrap();
    assert_eq!(
        storage
            .commit(vec![StorageWrite::Entry {
                value: entry(committed, None)
            }])
            .unwrap(),
        Seq(3)
    );
    let db = read_only(&path);
    assert_eq!(scalar(&db, "SELECT count(*) FROM entries"), 1);
    assert_eq!(scalar(&db, "SELECT count(*) FROM record_ids"), 3);
    assert_eq!(
        scalar(&db, "SELECT next_seq FROM durable_metadata WHERE singleton = 1"),
        4
    );
}

#[test]
fn sqlite_reconstructs_recent_and_ancient_rewindable_points_after_reopen() {
    let (_directory, path, mut storage) = create(SqliteOptions::default());
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    let record = DocumentCreate {
        id: DocumentId(id),
        kind: "history".to_owned(),
        key: None,
        scope: DocumentScope::Conversation {
            conversation_id: ROOT_CONVERSATION_ID,
        },
        history: Some(DocumentHistory::Rewindable),
        fork: Some(DocumentFork::AsOf),
    };
    let base = |count: u64| DocumentContent::Base {
        version: 1,
        value: json!({ "count": count }).as_object().unwrap().clone(),
    };
    let created = storage
        .commit(vec![StorageWrite::DocumentCreate {
            record,
            content: base(0),
        }])
        .unwrap();
    let (mut ancient, mut recent) = (created, created);
    for count in 1..=40u64 {
        let content = if count == 20 {
            json!(base(count))
        } else {
            json!({ "kind": "delta", "version": 1, "ops": [["s", ["count"], count]] })
        };
        recent = change(&mut storage, id, content);
        if count == 5 {
            ancient = recent;
        }
    }
    storage.close().unwrap();
    let reopened = reopen(&path);
    let at = |seq| {
        Value::Object(
            reopened
                .document(DocumentId(id), DocumentPoint::At(seq))
                .unwrap()
                .unwrap()
                .value,
        )
    };
    assert_eq!(at(ancient), json!({ "count": 5 }));
    assert_eq!(at(recent), json!({ "count": 40 }));
}

#[test]
fn sqlite_uses_indexes_for_addresses_scopes_entry_history_and_revision_tails() {
    let (_directory, path, mut storage) = create(SqliteOptions::default());
    storage.close().unwrap();
    let db = read_only(&path);
    let plan = |sql: &str, params: Vec<rusqlite::types::Value>| -> String {
        let mut statement = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let details = statement
            .query_map(rusqlite::params_from_iter(params), |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        details.join("\n")
    };
    use rusqlite::types::Value::{Integer as I, Text as T};
    let t = |s: &str| T(s.to_owned());
    let details = [
        plan(
            "SELECT record FROM documents WHERE kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ?
                AND retired_at IS NULL ORDER BY created_at DESC LIMIT 1",
            vec![t("kind"), t("session"), I(0), I(0), t("")],
        ),
        plan(
            "SELECT record FROM documents WHERE kind = ? AND scope_kind = ? AND owner_id = ? AND family = ? AND key_value = ?
                AND created_at <= ? AND (retired_at IS NULL OR retired_at > ?) ORDER BY created_at DESC LIMIT 1",
            vec![t("kind"), t("conversation"), I(1), I(0), t(""), I(10), I(10)],
        ),
        plan(
            "SELECT record FROM documents WHERE scope_kind = ? AND owner_id = ? AND kind = ? AND id > ? ORDER BY id LIMIT ?",
            vec![t("task"), I(1), t("kind"), I(0), I(10)],
        ),
        plan(
            "SELECT record FROM entries WHERE conversation_id = ? AND id <= ? ORDER BY id DESC LIMIT ?",
            vec![I(1), I(10), I(10)],
        ),
        plan(
            "SELECT record FROM entries WHERE conversation_id = ? AND head IS NOT NULL AND id <= ? ORDER BY id DESC LIMIT 1",
            vec![I(1), I(10)],
        ),
        plan("SELECT record FROM tasks WHERE status = ? AND id > ? ORDER BY id LIMIT ?", vec![t("pending"), I(0), I(10)]),
        plan(
            "SELECT seq, kind, version, content FROM document_revisions
                WHERE document_id = ? AND kind = 'base' AND seq <= ? ORDER BY seq DESC LIMIT 1",
            vec![I(1), I(10)],
        ),
        plan(
            "SELECT seq, kind, version, content FROM document_revisions WHERE document_id = ? AND seq > ? AND seq <= ? ORDER BY seq",
            vec![I(1), I(5), I(10)],
        ),
    ];
    let expected = [
        "documents_by_address",
        "documents_by_address",
        "documents_by_scope_kind",
        "entries_by_conversation",
        "entry_heads_by_conversation",
        "tasks_by_status",
        "document_revisions_by_kind",
        "sqlite_autoindex_document_revisions_1",
    ];
    for (detail, index) in details.iter().zip(expected) {
        assert!(detail.contains(index), "{detail} should use {index}");
    }
    for detail in &details[..3] {
        assert!(!detail.contains("SCAN documents"), "{detail}");
    }
    assert!(!details[7].contains("SCAN document_revisions"), "{}", details[7]);
}

#[test]
fn sqlite_reclaims_current_only_revisions_only_after_a_base_or_retirement() {
    let (_directory, path, mut storage) = create(SqliteOptions::default());
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    create_document(
        &mut storage,
        id,
        "latest",
        json!({ "kind": "session" }),
        json!({ "count": 0 }),
    );
    for count in 1..=10 {
        change(
            &mut storage,
            id,
            json!({ "kind": "delta", "version": 1, "ops": [["s", ["count"], count]] }),
        );
    }
    assert_eq!(revision_count(&path, id), 11);
    let latest: String = read_only(&path)
        .query_row(
            "SELECT content FROM document_revisions WHERE document_id = ? AND kind = 'delta' ORDER BY seq DESC LIMIT 1",
            [id as i64],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&latest).unwrap(),
        json!([["s", ["count"], 10]])
    );
    change(
        &mut storage,
        id,
        json!({ "kind": "base", "version": 1, "value": { "count": 11 } }),
    );
    assert_eq!(revision_count(&path, id), 1);
    change(
        &mut storage,
        id,
        json!({ "kind": "delta", "version": 1, "ops": [["r", { "count": 12 }]] }),
    );
    assert_eq!(revision_count(&path, id), 2);
    storage
        .commit(writes(json!([{ "type": "document.retire", "id": id }])))
        .unwrap();
    assert_eq!(revision_count(&path, id), 0);
}

#[test]
fn sqlite_auto_checkpoints_wal_frames_and_truncates_the_wal_on_close() {
    let (_directory, path, mut storage) = create(SqliteOptions {
        wal_autocheckpoint_pages: 1,
        ..SqliteOptions::default()
    });
    create_root(&mut storage);
    for index in 0..20 {
        let id = storage.mint_id().unwrap();
        let data = json!({ "text": "x".repeat(32 * 1024), "index": index });
        storage
            .commit(vec![StorageWrite::Entry {
                value: entry(id, Some(data)),
            }])
            .unwrap();
    }
    let wal = path.with_extension("sqlite-wal");
    assert!(std::fs::metadata(&wal).unwrap().len() < 512 * 1024);
    let observer = read_only(&path);
    assert_eq!(scalar(&observer, "SELECT count(*) FROM entries"), 20);
    storage.close().unwrap();
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
}

#[test]
fn sqlite_reuses_pages_released_by_current_only_checkpoints() {
    let (_directory, path, mut storage) = create(SqliteOptions::default());
    create_root(&mut storage);
    let id = storage.mint_id().unwrap();
    let large = "x".repeat(512 * 1024);
    create_document(
        &mut storage,
        id,
        "reuse",
        json!({ "kind": "session" }),
        json!({ "text": large }),
    );
    change(
        &mut storage,
        id,
        json!({ "kind": "base", "version": 1, "value": { "text": "small" } }),
    );
    let (pages_after_delete, free_after_delete) = {
        let db = read_only(&path);
        (
            scalar(&db, "SELECT page_count FROM pragma_page_count()"),
            scalar(&db, "SELECT freelist_count FROM pragma_freelist_count()"),
        )
    };
    assert!(free_after_delete > 0);
    change(
        &mut storage,
        id,
        json!({ "kind": "base", "version": 1, "value": { "text": large } }),
    );
    let db = read_only(&path);
    assert!(scalar(&db, "SELECT page_count FROM pragma_page_count()") <= pages_after_delete + 2);
    assert!(scalar(&db, "SELECT freelist_count FROM pragma_freelist_count()") < free_after_delete);
}

#[test]
fn sqlite_keeps_representative_row_and_document_storage_bounded() {
    let (_directory, path, mut storage) = create(SqliteOptions::default());
    create_root(&mut storage);
    for index in 0..100 {
        let id = storage.mint_id().unwrap();
        let data = json!({ "index": index, "text": "x".repeat(1_024) });
        storage
            .commit(vec![StorageWrite::Entry {
                value: entry(id, Some(data)),
            }])
            .unwrap();
    }
    let id = storage.mint_id().unwrap();
    storage
        .commit(writes(json!([{
            "type": "document.create",
            "record": { "id": id, "kind": "size.history", "scope": { "kind": "conversation", "conversationId": 1 },
                        "history": "rewindable", "fork": "asOf" },
            "content": { "kind": "base", "version": 1, "value": { "count": 0 } },
        }])))
        .unwrap();
    for count in 1..=100 {
        change(
            &mut storage,
            id,
            json!({ "kind": "delta", "version": 1, "ops": [["s", ["count"], count]] }),
        );
    }
    storage.close().unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() < 1024 * 1024);
}

#[test]
fn sqlite_rejects_every_operation_after_close_and_closes_idempotently() {
    let (_directory, _path, mut storage) = create(SqliteOptions::default());
    storage.close().unwrap();
    storage.close().unwrap();
    assert_eq!(storage.mint_id().unwrap_err().message(), "SqliteStorage is closed");
    assert_eq!(
        storage.task(TaskId(1)).unwrap_err().message(),
        "SqliteStorage is closed"
    );
}
