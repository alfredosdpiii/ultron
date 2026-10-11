//! Replays the upstream storage conformance cases (fixtures/generate.mjs) call by call against each Rust backend and
//! requires the same results and the same error names and messages as the matching upstream backend.

use durable::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentId, DocumentPoint,
    DocumentQuery, DocumentRecord, EntryId, EntryQuery, EntryRecord, JsonlOptions, JsonlStorage, MemoryStorage, Page,
    Seq, SqliteOptions, SqliteStorage, Storage, StorageError, StorageWrite, StoredDocument, StoredEntry, SubmissionId,
    SubmissionQuery, SubmissionRecord, TaskId, TaskQuery, TaskRecord,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use tempfile::TempDir;

type Result<T> = std::result::Result<T, StorageError>;

static FIXTURE: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../fixtures/storage-conformance.json")).expect("fixture parses")
});

fn arg<T: DeserializeOwned>(args: &[Value], index: usize) -> T {
    serde_json::from_value(args[index].clone()).unwrap_or_else(|e| panic!("argument {index} {}: {e}", args[index]))
}

fn cursor(args: &[Value], index: usize) -> Option<Cursor> {
    arg::<Option<Cursor>>(args, index)
}

fn out<T: Serialize>(result: Result<T>) -> Value {
    match result {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({ "error": { "name": error.name(), "message": error.message() } }),
    }
}

/// One recorded upstream call against `storage`, as `{ok}` or `{error: {name, message}}`.
fn call(storage: &mut impl Storage, method: &str, a: &[Value]) -> Value {
    let limit = |index: usize| arg::<usize>(a, index);
    match method {
        "commit" => out(storage.commit(arg(a, 0))),
        "mintId" => out(storage.mint_id()),
        "conversation" => out(storage.conversation(arg(a, 0))),
        "scanConversations" => out(storage.scan_conversations(&arg(a, 0), limit(1), cursor(a, 2).as_ref())),
        "entry" if a.len() == 1 => out(storage.entry(arg(a, 0))),
        "entry" => out(storage.visible_entry(arg::<ConversationId>(a, 0), arg::<EntryId>(a, 1))),
        "findLatestHeadMarker" => out(storage.find_latest_head_marker(arg(a, 0), arg(a, 1))),
        "scanEntries" => out(storage.scan_entries(&arg(a, 0), limit(1), cursor(a, 2).as_ref())),
        "task" => out(storage.task(arg::<TaskId>(a, 0))),
        "scanTasks" => out(storage.scan_tasks(&arg(a, 0), limit(1), cursor(a, 2).as_ref())),
        "submission" => out(storage.submission(arg::<SubmissionId>(a, 0))),
        "scanSubmissions" => out(storage.scan_submissions(&arg(a, 0), limit(1), cursor(a, 2).as_ref())),
        "submissionByRequest" => out(storage.submission_by_request(arg(a, 0), &arg::<String>(a, 1))),
        "findDocument" => out(storage.find_document(&arg(a, 0), arg::<DocumentPoint>(a, 1))),
        "document" => out(storage.document(arg::<DocumentId>(a, 0), arg::<DocumentPoint>(a, 1))),
        "scanDocuments" => out(storage.scan_documents(&arg(a, 0), limit(1), cursor(a, 2).as_ref())),
        "close" => out(storage.close()),
        other => panic!("unknown storage method {other}"),
    }
}

fn expected(recorded: &Value) -> Value {
    match recorded.get("error") {
        Some(error) => json!({ "error": error }),
        None => json!({ "ok": recorded["ok"] }),
    }
}

fn replay(name: &str, backend: &str, mut storage: impl Storage) {
    let case = FIXTURE["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name);
    let case = case.unwrap_or_else(|| panic!("fixture has no case {name:?}"));
    let calls = case[backend]
        .as_array()
        .unwrap_or_else(|| panic!("{name}: no {backend} recording"));
    for (index, recorded) in calls.iter().enumerate() {
        let method = recorded["method"].as_str().unwrap();
        let actual = call(&mut storage, method, recorded["args"].as_array().unwrap());
        assert_eq!(
            actual,
            expected(recorded),
            "{name} ({backend}): call {index} {method}({})",
            recorded["args"]
        );
    }
}

fn sqlite_file() -> (TempDir, PathBuf) {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("storage.sqlite");
    (directory, path)
}

fn open(path: &Path) -> SqliteStorage {
    SqliteStorage::open(path, SqliteOptions::default()).expect("open storage")
}

/// Columns holding JSON text, compared parsed: key order and escapes are not part of the file format.
fn json_columns(table: &str) -> &'static [&'static str] {
    match table {
        "conversations" | "entries" => &["record"],
        "tasks" => &["kind", "record"],
        "submissions" => &["request_id", "record"],
        "documents" => &["kind", "key_value", "record"],
        "document_revisions" => &["content"],
        _ => &[],
    }
}

/// Schema text and every row of a closed storage file, shaped like the generator's dump.
fn dump(path: &Path) -> Value {
    let db = rusqlite::Connection::open(path).expect("open dump");
    let mut schema = db
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY name")
        .unwrap();
    let schema: Vec<Value> = schema
        .query_map([], |row| {
            Ok(json!({
                "type": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "tbl_name": row.get::<_, String>(2)?,
                "sql": row.get::<_, Option<String>>(3)?,
            }))
        })
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let mut tables = serde_json::Map::new();
    for table in schema.iter().filter(|row| row["type"] == "table") {
        let name = table["name"].as_str().unwrap();
        let mut statement = db.prepare(&format!("SELECT * FROM \"{name}\" ORDER BY 1, 2")).unwrap();
        let columns: Vec<String> = statement.column_names().into_iter().map(str::to_owned).collect();
        let rows: Vec<Value> = statement
            .query_map([], |row| {
                let mut out = serde_json::Map::new();
                for (index, column) in columns.iter().enumerate() {
                    let value = match row.get::<_, rusqlite::types::Value>(index)? {
                        rusqlite::types::Value::Null => Value::Null,
                        rusqlite::types::Value::Integer(n) => json!(n),
                        rusqlite::types::Value::Real(n) => json!(n),
                        rusqlite::types::Value::Text(text) if json_columns(name).contains(&column.as_str()) => {
                            serde_json::from_str(&text).expect("JSON column")
                        }
                        rusqlite::types::Value::Text(text) => json!(text),
                        rusqlite::types::Value::Blob(_) => panic!("unexpected blob in {name}.{column}"),
                    };
                    out.insert(column.clone(), value);
                }
                Ok(Value::Object(out))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        tables.insert(name.to_owned(), Value::Array(rows));
    }
    json!({ "schema": schema, "tables": tables })
}

/// The Rust file holds exactly the schema text and rows upstream wrote for the same calls, so either reads the other.
fn assert_same_file(name: &str, path: &Path) {
    let case = FIXTURE["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .unwrap();
    let actual = dump(path);
    let expected = &case["sqliteDump"];
    assert_eq!(actual["schema"], expected["schema"], "{name}: schema");
    for (table, rows) in expected["tables"].as_object().unwrap() {
        assert_eq!(&actual["tables"][table], rows, "{name}: table {table}");
    }
    assert_eq!(
        actual["tables"].as_object().unwrap().len(),
        expected["tables"].as_object().unwrap().len()
    );
}

/// The JSONL directory holds the same files with the same lines (compared parsed) that upstream wrote.
fn assert_same_directory(name: &str, directory: &Path) {
    let case = FIXTURE["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .unwrap();
    let mut files = serde_json::Map::new();
    let mut names: Vec<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    for file in names {
        let text = std::fs::read_to_string(directory.join(&file)).unwrap();
        let lines: Vec<Value> = text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        files.insert(file, Value::Array(lines));
    }
    assert_eq!(Value::Object(files), case["jsonlDump"], "{name}: JSONL files");
}

/// Upstream's `ReopeningStorage` (test/sqlite-storage.test.ts): every commit, failed or not, closes and reopens.
struct Reopening<S> {
    current: S,
    reopen: Box<dyn Fn() -> S>,
    closed: bool,
}

impl<S: Storage> Reopening<S> {
    fn new(reopen: impl Fn() -> S + 'static) -> Self {
        Reopening {
            current: reopen(),
            reopen: Box::new(reopen),
            closed: false,
        }
    }
}

impl<S: Storage> Storage for Reopening<S> {
    fn commit(&mut self, writes: Vec<StorageWrite>) -> Result<Seq> {
        let result = self.current.commit(writes);
        if !self.closed {
            self.current.close().expect("close");
            self.current = (self.reopen)();
        }
        result
    }
    fn mint_id(&mut self) -> Result<u64> {
        self.current.mint_id()
    }
    fn conversation(&self, id: ConversationId) -> Result<Option<ConversationRecord>> {
        self.current.conversation(id)
    }
    fn scan_conversations(
        &self,
        q: &ConversationQuery,
        l: usize,
        c: Option<&Cursor>,
    ) -> Result<Page<ConversationRecord>> {
        self.current.scan_conversations(q, l, c)
    }
    fn entry(&self, id: EntryId) -> Result<Option<StoredEntry>> {
        self.current.entry(id)
    }
    fn visible_entry(&self, conversation_id: ConversationId, id: EntryId) -> Result<Option<StoredEntry>> {
        self.current.visible_entry(conversation_id, id)
    }
    fn find_latest_head_marker(&self, id: ConversationId, at: Option<EntryId>) -> Result<Option<EntryRecord>> {
        self.current.find_latest_head_marker(id, at)
    }
    fn scan_entries(&self, q: &EntryQuery, l: usize, c: Option<&Cursor>) -> Result<Page<EntryRecord>> {
        self.current.scan_entries(q, l, c)
    }
    fn task(&self, id: TaskId) -> Result<Option<TaskRecord>> {
        self.current.task(id)
    }
    fn scan_tasks(&self, q: &TaskQuery, l: usize, c: Option<&Cursor>) -> Result<Page<TaskRecord>> {
        self.current.scan_tasks(q, l, c)
    }
    fn submission(&self, id: SubmissionId) -> Result<Option<SubmissionRecord>> {
        self.current.submission(id)
    }
    fn scan_submissions(&self, q: &SubmissionQuery, l: usize, c: Option<&Cursor>) -> Result<Page<SubmissionRecord>> {
        self.current.scan_submissions(q, l, c)
    }
    fn submission_by_request(&self, id: ConversationId, request_id: &str) -> Result<Option<SubmissionRecord>> {
        self.current.submission_by_request(id, request_id)
    }
    fn find_document(&self, address: &DocumentAddress, at: DocumentPoint) -> Result<Option<DocumentRecord>> {
        self.current.find_document(address, at)
    }
    fn document(&self, id: DocumentId, at: DocumentPoint) -> Result<Option<StoredDocument>> {
        self.current.document(id, at)
    }
    fn scan_documents(&self, q: &DocumentQuery, l: usize, c: Option<&Cursor>) -> Result<Page<DocumentRecord>> {
        self.current.scan_documents(q, l, c)
    }
    fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.current.close()
    }
}

macro_rules! conformance {
    ($([$memory:ident, $sqlite:ident, $reopen:ident, $jsonl:ident, $jsonl_reopen:ident] => $name:literal),* $(,)?) => {
        $(#[test]
        fn $memory() {
            replay($name, "memory", MemoryStorage::new());
        }

        #[test]
        fn $sqlite() {
            let (_directory, path) = sqlite_file();
            replay($name, "sqlite", open(&path));
            assert_same_file($name, &path);
        }

        #[test]
        fn $reopen() {
            let (_directory, path) = sqlite_file();
            open(&path).close().expect("close");
            replay($name, "sqliteReopen", Reopening::new(move || open(&path)));
        }

        #[test]
        fn $jsonl() {
            let directory = tempfile::tempdir().expect("temporary directory");
            replay($name, "jsonl", JsonlStorage::open(directory.path(), JsonlOptions::default()).unwrap());
            assert_same_directory($name, directory.path());
        }

        #[test]
        fn $jsonl_reopen() {
            let directory = tempfile::tempdir().expect("temporary directory");
            let path = directory.path().to_owned();
            replay($name, "jsonlReopen", Reopening::new(move || JsonlStorage::open(&path, JsonlOptions::default()).unwrap()));
        })*

        #[test]
        fn every_upstream_conformance_case_is_replayed_on_every_backend() {
            let names: Vec<&str> =
                FIXTURE["cases"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
            assert_eq!(names, vec![$($name),*]);
        }
    };
}

conformance!(
    [memory_reserves_id_1_for_the_immutable_root_conversation, sqlite_reserves_id_1_for_the_immutable_root_conversation, sqlite_reopen_reserves_id_1_for_the_immutable_root_conversation, jsonl_reserves_id_1_for_the_immutable_root_conversation, jsonl_reopen_reserves_id_1_for_the_immutable_root_conversation] => "reserves ID 1 for the immutable root conversation",
    [memory_commits_mixed_table_writes_atomically_and_rolls_all_of_them_back_on_failure, sqlite_commits_mixed_table_writes_atomically_and_rolls_all_of_them_back_on_failure, sqlite_reopen_commits_mixed_table_writes_atomically_and_rolls_all_of_them_back_on_failure, jsonl_commits_mixed_table_writes_atomically_and_rolls_all_of_them_back_on_failure, jsonl_reopen_commits_mixed_table_writes_atomically_and_rolls_all_of_them_back_on_failure] => "commits mixed table writes atomically and rolls all of them back on failure",
    [memory_detaches_retained_writes_and_every_returned_record, sqlite_detaches_retained_writes_and_every_returned_record, sqlite_reopen_detaches_retained_writes_and_every_returned_record, jsonl_detaches_retained_writes_and_every_returned_record, jsonl_reopen_detaches_retained_writes_and_every_returned_record] => "detaches retained writes and every returned record",
    [memory_detaches_prototype_like_json_keys_without_changing_object_prototypes, sqlite_detaches_prototype_like_json_keys_without_changing_object_prototypes, sqlite_reopen_detaches_prototype_like_json_keys_without_changing_object_prototypes, jsonl_detaches_prototype_like_json_keys_without_changing_object_prototypes, jsonl_reopen_detaches_prototype_like_json_keys_without_changing_object_prototypes] => "detaches prototype-like JSON keys without changing object prototypes",
    [memory_indexes_entries_committed_out_of_id_order, sqlite_indexes_entries_committed_out_of_id_order, sqlite_reopen_indexes_entries_committed_out_of_id_order, jsonl_indexes_entries_committed_out_of_id_order, jsonl_reopen_indexes_entries_committed_out_of_id_order] => "indexes entries committed out of ID order",
    [memory_continues_an_entry_cursor_below_its_last_item_after_a_newer_commit, sqlite_continues_an_entry_cursor_below_its_last_item_after_a_newer_commit, sqlite_reopen_continues_an_entry_cursor_below_its_last_item_after_a_newer_commit, jsonl_continues_an_entry_cursor_below_its_last_item_after_a_newer_commit, jsonl_reopen_continues_an_entry_cursor_below_its_last_item_after_a_newer_commit] => "continues an entry cursor below its last item after a newer commit",
    [memory_paginates_conversations_by_opaque_cursor_in_ascending_id_order, sqlite_paginates_conversations_by_opaque_cursor_in_ascending_id_order, sqlite_reopen_paginates_conversations_by_opaque_cursor_in_ascending_id_order, jsonl_paginates_conversations_by_opaque_cursor_in_ascending_id_order, jsonl_reopen_paginates_conversations_by_opaque_cursor_in_ascending_id_order] => "paginates conversations by opaque cursor in ascending ID order",
    [memory_filters_and_pages_conversations_by_durable_owner_edges, sqlite_filters_and_pages_conversations_by_durable_owner_edges, sqlite_reopen_filters_and_pages_conversations_by_durable_owner_edges, jsonl_filters_and_pages_conversations_by_durable_owner_edges, jsonl_reopen_filters_and_pages_conversations_by_durable_owner_edges] => "filters and pages conversations by durable owner edges",
    [memory_scans_deep_fork_history_newest_first_through_every_ancestor_cap, sqlite_scans_deep_fork_history_newest_first_through_every_ancestor_cap, sqlite_reopen_scans_deep_fork_history_newest_first_through_every_ancestor_cap, jsonl_scans_deep_fork_history_newest_first_through_every_ancestor_cap, jsonl_reopen_scans_deep_fork_history_newest_first_through_every_ancestor_cap] => "scans deep fork history newest-first through every ancestor cap",
    [memory_scans_tables_in_either_id_order_and_continues_a_cursor_in_its_order, sqlite_scans_tables_in_either_id_order_and_continues_a_cursor_in_its_order, sqlite_reopen_scans_tables_in_either_id_order_and_continues_a_cursor_in_its_order, jsonl_scans_tables_in_either_id_order_and_continues_a_cursor_in_its_order, jsonl_reopen_scans_tables_in_either_id_order_and_continues_a_cursor_in_its_order] => "scans tables in either ID order and continues a cursor in its order",
    [memory_replaces_complete_task_records_and_pages_filtered_task_scans, sqlite_replaces_complete_task_records_and_pages_filtered_task_scans, sqlite_reopen_replaces_complete_task_records_and_pages_filtered_task_scans, jsonl_replaces_complete_task_records_and_pages_filtered_task_scans, jsonl_reopen_replaces_complete_task_records_and_pages_filtered_task_scans] => "replaces complete task records and pages filtered task scans",
    [memory_stores_owners_and_scans_waiting_and_completing_tasks_by_status, sqlite_stores_owners_and_scans_waiting_and_completing_tasks_by_status, sqlite_reopen_stores_owners_and_scans_waiting_and_completing_tasks_by_status, jsonl_stores_owners_and_scans_waiting_and_completing_tasks_by_status, jsonl_reopen_stores_owners_and_scans_waiting_and_completing_tasks_by_status] => "stores owners and scans waiting and completing tasks by status",
    [memory_indexes_request_ids_per_conversation_and_replaces_complete_submission_records, sqlite_indexes_request_ids_per_conversation_and_replaces_complete_submission_records, sqlite_reopen_indexes_request_ids_per_conversation_and_replaces_complete_submission_records, jsonl_indexes_request_ids_per_conversation_and_replaces_complete_submission_records, jsonl_reopen_indexes_request_ids_per_conversation_and_replaces_complete_submission_records] => "indexes request IDs per conversation and replaces complete submission records",
    [memory_stores_passive_write_submissions_without_input_only_lifecycle_states, sqlite_stores_passive_write_submissions_without_input_only_lifecycle_states, sqlite_reopen_stores_passive_write_submissions_without_input_only_lifecycle_states, jsonl_stores_passive_write_submissions_without_input_only_lifecycle_states, jsonl_reopen_stores_passive_write_submissions_without_input_only_lifecycle_states] => "stores passive write submissions without input-only lifecycle states",
    [memory_reconstructs_rewindable_documents_and_preserves_half_open_incarnations, sqlite_reconstructs_rewindable_documents_and_preserves_half_open_incarnations, sqlite_reopen_reconstructs_rewindable_documents_and_preserves_half_open_incarnations, jsonl_reconstructs_rewindable_documents_and_preserves_half_open_incarnations, jsonl_reopen_reconstructs_rewindable_documents_and_preserves_half_open_incarnations] => "reconstructs rewindable documents and preserves half-open incarnations",
    [memory_streams_long_document_tails_across_root_replacement_deltas, sqlite_streams_long_document_tails_across_root_replacement_deltas, sqlite_reopen_streams_long_document_tails_across_root_replacement_deltas, jsonl_streams_long_document_tails_across_root_replacement_deltas, jsonl_reopen_streams_long_document_tails_across_root_replacement_deltas] => "streams long document tails across root replacement deltas",
    [memory_copies_stored_document_bases_independently_and_rejects_ambiguous_sources, sqlite_copies_stored_document_bases_independently_and_rejects_ambiguous_sources, sqlite_reopen_copies_stored_document_bases_independently_and_rejects_ambiguous_sources, jsonl_copies_stored_document_bases_independently_and_rejects_ambiguous_sources, jsonl_reopen_copies_stored_document_bases_independently_and_rejects_ambiguous_sources] => "copies stored document bases independently and rejects ambiguous sources",
    [memory_uses_bases_for_version_transitions_and_rejects_historical_reads_of_current_only_documents, sqlite_uses_bases_for_version_transitions_and_rejects_historical_reads_of_current_only_documents, sqlite_reopen_uses_bases_for_version_transitions_and_rejects_historical_reads_of_current_only_documents, jsonl_uses_bases_for_version_transitions_and_rejects_historical_reads_of_current_only_documents, jsonl_reopen_uses_bases_for_version_transitions_and_rejects_historical_reads_of_current_only_documents] => "uses bases for version transitions and rejects historical reads of current-only documents",
    [memory_indexes_logical_addresses_and_exact_scope_scans_independently, sqlite_indexes_logical_addresses_and_exact_scope_scans_independently, sqlite_reopen_indexes_logical_addresses_and_exact_scope_scans_independently, jsonl_indexes_logical_addresses_and_exact_scope_scans_independently, jsonl_reopen_indexes_logical_addresses_and_exact_scope_scans_independently] => "indexes logical addresses and exact-scope scans independently",
    [memory_keeps_document_lifecycle_failures_atomic_and_gives_create_plus_retire_an_empty_lifetime, sqlite_keeps_document_lifecycle_failures_atomic_and_gives_create_plus_retire_an_empty_lifetime, sqlite_reopen_keeps_document_lifecycle_failures_atomic_and_gives_create_plus_retire_an_empty_lifetime, jsonl_keeps_document_lifecycle_failures_atomic_and_gives_create_plus_retire_an_empty_lifetime, jsonl_reopen_keeps_document_lifecycle_failures_atomic_and_gives_create_plus_retire_an_empty_lifetime] => "keeps document lifecycle failures atomic and gives create-plus-retire an empty lifetime",
    [memory_rolls_back_record_tables_and_secondary_indexes_when_a_document_command_fails, sqlite_rolls_back_record_tables_and_secondary_indexes_when_a_document_command_fails, sqlite_reopen_rolls_back_record_tables_and_secondary_indexes_when_a_document_command_fails, jsonl_rolls_back_record_tables_and_secondary_indexes_when_a_document_command_fails, jsonl_reopen_rolls_back_record_tables_and_secondary_indexes_when_a_document_command_fails] => "rolls back record tables and secondary indexes when a document command fails",
    [memory_keeps_indexed_string_identities_lossless, sqlite_keeps_indexed_string_identities_lossless, sqlite_reopen_keeps_indexed_string_identities_lossless, jsonl_keeps_indexed_string_identities_lossless, jsonl_reopen_keeps_indexed_string_identities_lossless] => "keeps indexed string identities lossless",
    [memory_keeps_one_global_record_id_namespace_and_rejects_exhausted_id_minting, sqlite_keeps_one_global_record_id_namespace_and_rejects_exhausted_id_minting, sqlite_reopen_keeps_one_global_record_id_namespace_and_rejects_exhausted_id_minting, jsonl_keeps_one_global_record_id_namespace_and_rejects_exhausted_id_minting, jsonl_reopen_keeps_one_global_record_id_namespace_and_rejects_exhausted_id_minting] => "keeps one global record ID namespace and rejects exhausted ID minting",
    [memory_rejects_every_operation_after_close, sqlite_rejects_every_operation_after_close, sqlite_reopen_rejects_every_operation_after_close, jsonl_rejects_every_operation_after_close, jsonl_reopen_rejects_every_operation_after_close] => "rejects every operation after close",
    [memory_reindexes_a_changed_submission_request_id, sqlite_reindexes_a_changed_submission_request_id, sqlite_reopen_reindexes_a_changed_submission_request_id, jsonl_reindexes_a_changed_submission_request_id, jsonl_reopen_reindexes_a_changed_submission_request_id] => "extra: a submission whose request ID changes leaves the old request ID unindexed",
);

/// Upstream stores lone UTF-16 surrogates; a Rust string cannot, so such input is refused at the JSON boundary instead
/// of being stored lossily (the fixture maps them to distinct supplementary code points).
#[test]
fn lone_surrogates_are_refused_at_the_json_boundary() {
    let write = r#"{"type":"submission","value":{"id":4,"conversationId":1,"requestId":"\ud800","type":"input","status":"queued"}}"#;
    let error = serde_json::from_str::<durable::StorageWrite>(write).unwrap_err();
    assert!(error.to_string().contains("unexpected end of hex escape"), "{error}");
}
