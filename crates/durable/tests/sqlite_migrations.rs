//! Port of upstream `test/sqlite-migrations.test.ts` ("durable SQLite migrations"): the schema bootstrap and every
//! pending migration apply in one transaction, repeatedly without change; a failing migration rolls everything back
//! and keeps stored data, so a corrected retry succeeds. Like upstream, the failing and later migrations are
//! test-only lists passed to `apply_sqlite_migrations`.

use durable::{
    CURRENT_SQLITE_SCHEMA_VERSION, SQLITE_MIGRATIONS, SqliteMigration, SqliteOptions, SqliteStorage, Storage,
    StorageWrite, apply_sqlite_migrations,
};
use rusqlite::Connection;
use serde_json::json;
use std::path::PathBuf;

fn database_path() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("storage.sqlite");
    (directory, path)
}

fn version(db: &Connection) -> i64 {
    db.query_row("SELECT version FROM durable_schema WHERE singleton = 1", [], |row| {
        row.get(0)
    })
    .unwrap()
}

fn metadata(db: &Connection) -> (String, i64) {
    db.query_row(
        "SELECT next_id, next_seq FROM durable_metadata WHERE singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap()
}

fn count(db: &Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |row| row.get(0)).unwrap()
}

#[test]
fn durable_sqlite_migrations_create_the_current_schema_and_apply_repeatedly() {
    let (_directory, path) = database_path();
    let mut db = Connection::open(&path).unwrap();
    apply_sqlite_migrations(&mut db, SQLITE_MIGRATIONS).unwrap();
    apply_sqlite_migrations(&mut db, SQLITE_MIGRATIONS).unwrap();
    assert_eq!(version(&db), CURRENT_SQLITE_SCHEMA_VERSION);
    assert_eq!(CURRENT_SQLITE_SCHEMA_VERSION, 1);
    assert_eq!(metadata(&db), ("2".to_string(), 1));
}

#[test]
fn durable_sqlite_migrations_reject_a_database_newer_than_the_core() {
    let (_directory, path) = database_path();
    let mut db = Connection::open(&path).unwrap();
    apply_sqlite_migrations(&mut db, SQLITE_MIGRATIONS).unwrap();
    db.execute(
        "UPDATE durable_schema SET version = ? WHERE singleton = 1",
        [CURRENT_SQLITE_SCHEMA_VERSION + 1],
    )
    .unwrap();
    drop(db);
    let Err(error) = SqliteStorage::open(&path, SqliteOptions::default()) else {
        panic!("a newer database opened");
    };
    assert!(
        error
            .to_string()
            .contains("Durable SQLite schema version 2 is newer than supported version 1"),
        "{error}"
    );
}

#[test]
fn durable_sqlite_migrations_must_be_contiguous_from_one() {
    let (_directory, path) = database_path();
    let mut db = Connection::open(&path).unwrap();
    let gap = [SqliteMigration {
        version: 2,
        statements: &["CREATE TABLE never (value TEXT) STRICT"],
    }];
    let error = apply_sqlite_migrations(&mut db, &gap).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Durable SQLite migrations must have contiguous versions starting at 1"),
        "{error}"
    );
    assert_eq!(count(&db, "SELECT count(*) FROM sqlite_schema"), 0);
}

#[test]
fn durable_sqlite_migrations_roll_bootstrap_and_every_pending_migration_back_together() {
    let (_directory, path) = database_path();
    let mut db = Connection::open(&path).unwrap();
    let first = SqliteMigration {
        version: 1,
        statements: &[
            "CREATE TABLE migration_first (value TEXT) STRICT",
            "INSERT INTO migration_first (value) VALUES ('retained')",
        ],
    };
    let failed = [
        first,
        SqliteMigration {
            version: 2,
            statements: &["CREATE TABLE migration_second (value TEXT) STRICT", "THIS IS NOT SQL"],
        },
    ];
    assert!(apply_sqlite_migrations(&mut db, &failed).is_err());
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS count FROM sqlite_schema WHERE name IN ('durable_schema', 'migration_first', 'migration_second')"
        ),
        0
    );

    let fixed = [
        first,
        SqliteMigration {
            version: 2,
            statements: &["CREATE TABLE migration_second (value TEXT) STRICT"],
        },
    ];
    apply_sqlite_migrations(&mut db, &fixed).unwrap();
    assert_eq!(version(&db), 2);
    let value: String = db
        .query_row("SELECT value FROM migration_first", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "retained");
}

#[test]
fn durable_sqlite_migrations_roll_a_failed_migration_back_and_keep_data_for_a_retry() {
    let (_directory, path) = database_path();
    let mut storage = SqliteStorage::open(&path, SqliteOptions::default()).unwrap();
    let writes: Vec<StorageWrite> = serde_json::from_value(json!([
        { "type": "conversation", "value": { "id": 1 } },
        { "type": "entry", "value": { "id": 2, "conversationId": 1, "kind": "retained", "data": { "retained": true } } },
    ]))
    .unwrap();
    storage.commit(writes).unwrap();
    storage.close().unwrap();

    let mut db = Connection::open(&path).unwrap();
    let next = CURRENT_SQLITE_SCHEMA_VERSION + 1;
    let probe_failing = ["CREATE TABLE migration_probe (value TEXT) STRICT", "THIS IS NOT SQL"];
    let failing: Vec<SqliteMigration<'_>> = SQLITE_MIGRATIONS
        .iter()
        .copied()
        .chain([SqliteMigration {
            version: next,
            statements: &probe_failing,
        }])
        .collect();
    assert!(apply_sqlite_migrations(&mut db, &failing).is_err());
    assert_eq!(version(&db), CURRENT_SQLITE_SCHEMA_VERSION);
    assert_eq!(
        count(
            &db,
            "SELECT count(*) AS count FROM sqlite_schema WHERE type = 'table' AND name = 'migration_probe'"
        ),
        0
    );

    let probe = ["CREATE TABLE migration_probe (value TEXT) STRICT"];
    let successful: Vec<SqliteMigration<'_>> = SQLITE_MIGRATIONS
        .iter()
        .copied()
        .chain([SqliteMigration {
            version: next,
            statements: &probe,
        }])
        .collect();
    apply_sqlite_migrations(&mut db, &successful).unwrap();
    assert_eq!(version(&db), next);
    let (record, commit_seq): (String, i64) = db
        .query_row("SELECT record, commit_seq FROM entries WHERE id = 2", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(
        (record.as_str(), commit_seq),
        (
            r#"{"id":2,"conversationId":1,"kind":"retained","data":{"retained":true}}"#,
            1
        )
    );
    assert_eq!(metadata(&db), ("3".to_string(), 2));
}
