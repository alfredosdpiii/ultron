//! Replays the upstream storage conformance cases (fixtures/generate.mjs) call by call against MemoryStorage, and
//! against the test model of the product's reopen-by-replay (`replay_support`, reopened after every commit), and
//! requires the same results and the same error names and messages as upstream's MemoryStorage and reopened file
//! backend. The product backend itself (session-core `JournalStorage`) replays the same cases in
//! `crates/session-core/tests/durable_journal_conformance.rs`.

#[path = "replay_support/mod.rs"]
mod replay_support;

use durable::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentId, DocumentPoint,
    DocumentQuery, DocumentRecord, EntryId, EntryQuery, EntryRecord, MemoryStorage, Page, Seq, Storage, StorageError,
    StorageWrite, StoredDocument, StoredEntry, SubmissionId, SubmissionQuery, SubmissionRecord, TaskId, TaskQuery,
    TaskRecord,
};
use replay_support::{ReplayLog, Replayed};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::LazyLock;

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

/// The recorded result; a reopened file backend's closed error names the class that refused, here MemoryStorage.
fn expected(recorded: &Value) -> Value {
    match recorded.get("error") {
        Some(error) if error["message"] == "JsonlStorage is closed" => {
            json!({ "error": { "name": error["name"], "message": "MemoryStorage is closed" } })
        }
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

/// Upstream's `ReopeningStorage` (test/sqlite-storage.test.ts): every commit, failed or not, closes and reopens.
struct Reopening<S> {
    current: S,
    reopen: Box<dyn Fn() -> S>,
    closed: bool,
}

impl Reopening<Replayed> {
    fn replaying() -> Self {
        let log = ReplayLog::default();
        Reopening::new(move || log.open())
    }
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
    ($([$memory:ident, $replay:ident] => $name:literal),* $(,)?) => {
        $(#[test]
        fn $memory() {
            replay($name, "memory", MemoryStorage::new());
        }

        #[test]
        fn $replay() {
            replay($name, "jsonlReopen", Reopening::replaying());
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
    [memory_reserves_id_1_for_the_immutable_root_conversation, durable_replay_reopen_reserves_id_1_for_the_immutable_root_conversation] => "reserves ID 1 for the immutable root conversation",
    [memory_commits_mixed_table_writes_atomically_and_rolls_all_of_them_back_on_failure, durable_replay_reopen_commits_mixed_table_writes_atomically_and_rolls_all_of_them_back_on_failure] => "commits mixed table writes atomically and rolls all of them back on failure",
    [memory_detaches_retained_writes_and_every_returned_record, durable_replay_reopen_detaches_retained_writes_and_every_returned_record] => "detaches retained writes and every returned record",
    [memory_detaches_prototype_like_json_keys_without_changing_object_prototypes, durable_replay_reopen_detaches_prototype_like_json_keys_without_changing_object_prototypes] => "detaches prototype-like JSON keys without changing object prototypes",
    [memory_indexes_entries_committed_out_of_id_order, durable_replay_reopen_indexes_entries_committed_out_of_id_order] => "indexes entries committed out of ID order",
    [memory_continues_an_entry_cursor_below_its_last_item_after_a_newer_commit, durable_replay_reopen_continues_an_entry_cursor_below_its_last_item_after_a_newer_commit] => "continues an entry cursor below its last item after a newer commit",
    [memory_paginates_conversations_by_opaque_cursor_in_ascending_id_order, durable_replay_reopen_paginates_conversations_by_opaque_cursor_in_ascending_id_order] => "paginates conversations by opaque cursor in ascending ID order",
    [memory_filters_and_pages_conversations_by_durable_owner_edges, durable_replay_reopen_filters_and_pages_conversations_by_durable_owner_edges] => "filters and pages conversations by durable owner edges",
    [memory_scans_deep_fork_history_newest_first_through_every_ancestor_cap, durable_replay_reopen_scans_deep_fork_history_newest_first_through_every_ancestor_cap] => "scans deep fork history newest-first through every ancestor cap",
    [memory_scans_tables_in_either_id_order_and_continues_a_cursor_in_its_order, durable_replay_reopen_scans_tables_in_either_id_order_and_continues_a_cursor_in_its_order] => "scans tables in either ID order and continues a cursor in its order",
    [memory_replaces_complete_task_records_and_pages_filtered_task_scans, durable_replay_reopen_replaces_complete_task_records_and_pages_filtered_task_scans] => "replaces complete task records and pages filtered task scans",
    [memory_stores_owners_and_scans_waiting_and_completing_tasks_by_status, durable_replay_reopen_stores_owners_and_scans_waiting_and_completing_tasks_by_status] => "stores owners and scans waiting and completing tasks by status",
    [memory_indexes_request_ids_per_conversation_and_replaces_complete_submission_records, durable_replay_reopen_indexes_request_ids_per_conversation_and_replaces_complete_submission_records] => "indexes request IDs per conversation and replaces complete submission records",
    [memory_stores_passive_write_submissions_without_input_only_lifecycle_states, durable_replay_reopen_stores_passive_write_submissions_without_input_only_lifecycle_states] => "stores passive write submissions without input-only lifecycle states",
    [memory_reconstructs_rewindable_documents_and_preserves_half_open_incarnations, durable_replay_reopen_reconstructs_rewindable_documents_and_preserves_half_open_incarnations] => "reconstructs rewindable documents and preserves half-open incarnations",
    [memory_streams_long_document_tails_across_root_replacement_deltas, durable_replay_reopen_streams_long_document_tails_across_root_replacement_deltas] => "streams long document tails across root replacement deltas",
    [memory_copies_stored_document_bases_independently_and_rejects_ambiguous_sources, durable_replay_reopen_copies_stored_document_bases_independently_and_rejects_ambiguous_sources] => "copies stored document bases independently and rejects ambiguous sources",
    [memory_uses_bases_for_version_transitions_and_rejects_historical_reads_of_current_only_documents, durable_replay_reopen_uses_bases_for_version_transitions_and_rejects_historical_reads_of_current_only_documents] => "uses bases for version transitions and rejects historical reads of current-only documents",
    [memory_indexes_logical_addresses_and_exact_scope_scans_independently, durable_replay_reopen_indexes_logical_addresses_and_exact_scope_scans_independently] => "indexes logical addresses and exact-scope scans independently",
    [memory_keeps_document_lifecycle_failures_atomic_and_gives_create_plus_retire_an_empty_lifetime, durable_replay_reopen_keeps_document_lifecycle_failures_atomic_and_gives_create_plus_retire_an_empty_lifetime] => "keeps document lifecycle failures atomic and gives create-plus-retire an empty lifetime",
    [memory_rolls_back_record_tables_and_secondary_indexes_when_a_document_command_fails, durable_replay_reopen_rolls_back_record_tables_and_secondary_indexes_when_a_document_command_fails] => "rolls back record tables and secondary indexes when a document command fails",
    [memory_keeps_indexed_string_identities_lossless, durable_replay_reopen_keeps_indexed_string_identities_lossless] => "keeps indexed string identities lossless",
    [memory_keeps_one_global_record_id_namespace_and_rejects_exhausted_id_minting, durable_replay_reopen_keeps_one_global_record_id_namespace_and_rejects_exhausted_id_minting] => "keeps one global record ID namespace and rejects exhausted ID minting",
    [memory_rejects_every_operation_after_close, durable_replay_reopen_rejects_every_operation_after_close] => "rejects every operation after close",
    [memory_reindexes_a_changed_submission_request_id, durable_replay_reopen_reindexes_a_changed_submission_request_id] => "extra: a submission whose request ID changes leaves the old request ID unindexed",
);

/// Upstream stores lone UTF-16 surrogates; a Rust string cannot, so such input is refused at the JSON boundary instead
/// of being stored lossily (the fixture maps them to distinct supplementary code points).
#[test]
fn lone_surrogates_are_refused_at_the_json_boundary() {
    let write = r#"{"type":"submission","value":{"id":4,"conversationId":1,"requestId":"\ud800","type":"input","status":"queued"}}"#;
    let error = serde_json::from_str::<durable::StorageWrite>(write).unwrap_err();
    assert!(error.to_string().contains("unexpected end of hex escape"), "{error}");
}
