//! The product's Pi Durable storage, [`JournalStorage`] (durable records inside the session's format-4 journal),
//! replays every upstream storage conformance case (`crates/durable/fixtures/generate.mjs`) call by call and must
//! give the same results and the same error names and messages as upstream.
//!
//! Two runs per case: one JournalStorage over a fresh session file, compared with upstream's MemoryStorage
//! recording (JournalStorage validates against a MemoryStorage working copy); and one closed and reopened from the
//! file after every commit, failed or not, compared with upstream's reopened file backend (`jsonlReopen`): a reopen
//! replays the journal, so IDs minted but never stored are not reserved again, as upstream. The one difference is the
//! closed-storage message, which names the class that refused (`MemoryStorage is closed` here).

use durable::{
    ConversationId, ConversationQuery, ConversationRecord, Cursor, DocumentAddress, DocumentId, DocumentPoint,
    DocumentQuery, DocumentRecord, EntryId, EntryQuery, EntryRecord, Page, Seq, Storage, StorageError, StorageWrite,
    StoredDocument, StoredEntry, SubmissionId, SubmissionQuery, SubmissionRecord, TaskId, TaskQuery, TaskRecord,
};
use serde_json::{Value, json};
use session_core::durable_store::{JournalStorage, SharedSession, stored_commits};
use session_journal::{Metadata, SessionRepo, TestClock};
use std::sync::{Arc, LazyLock, Mutex};
use tempfile::TempDir;

type Result<T> = std::result::Result<T, StorageError>;

static FIXTURE: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../../durable/fixtures/storage-conformance.json")).expect("fixture parses")
});

fn arg<T: serde::de::DeserializeOwned>(args: &[Value], index: usize) -> T {
    serde_json::from_value(args[index].clone()).unwrap_or_else(|e| panic!("argument {index} {}: {e}", args[index]))
}

fn cursor(args: &[Value], index: usize) -> Option<Cursor> {
    arg::<Option<Cursor>>(args, index)
}

fn out<T: serde::Serialize>(result: Result<T>) -> Value {
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
        Some(error) if error["message"] == "JsonlStorage is closed" => {
            json!({ "error": { "name": error["name"], "message": "MemoryStorage is closed" } })
        }
        Some(error) => json!({ "error": error }),
        None => json!({ "ok": recorded["ok"] }),
    }
}

fn recording<'a>(name: &str, backend: &str) -> &'a [Value] {
    let case = FIXTURE["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name);
    let case = case.unwrap_or_else(|| panic!("fixture has no case {name:?}"));
    case[backend]
        .as_array()
        .unwrap_or_else(|| panic!("{name}: no {backend} recording"))
}

fn replay(name: &str, backend: &str, mut storage: impl Storage) {
    for (index, recorded) in recording(name, backend).iter().enumerate() {
        let method = recorded["method"].as_str().unwrap();
        let actual = call(&mut storage, method, recorded["args"].as_array().unwrap());
        assert_eq!(
            actual,
            expected(recorded),
            "{name} (journal over {backend}): call {index} {method}({})",
            recorded["args"]
        );
    }
}

/// A fresh, empty session file and its metadata.
fn session_file() -> (TempDir, SessionRepo, Metadata) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let repo = SessionRepo::new(&dir.path().join("sessions"), TestClock::shared(1_790_000_000_000, 1));
    let session = repo
        .create(&dir.path().display().to_string(), None, None)
        .expect("create session");
    let metadata = session.metadata().clone();
    (dir, repo, metadata)
}

/// The session file opened anew, as a restarted process opens it.
fn open_session(repo: &SessionRepo, metadata: &Metadata) -> SharedSession {
    Arc::new(Mutex::new(repo.open(metadata).expect("open session")))
}

fn open_journal(repo: &SessionRepo, metadata: &Metadata) -> JournalStorage {
    JournalStorage::open(open_session(repo, metadata), None).expect("open journal storage")
}

/// The sequences of every commit the recording acknowledged, in order.
fn acknowledged(name: &str, backend: &str) -> Vec<u64> {
    recording(name, backend)
        .iter()
        .filter(|recorded| recorded["method"] == "commit")
        .filter_map(|recorded| recorded["ok"].as_u64())
        .collect()
}

/// The journal file holds exactly the acknowledged commits, by sequence, in order: no failed commit left a record.
fn assert_journal_holds(name: &str, backend: &str, repo: &SessionRepo, metadata: &Metadata) {
    let session = open_session(repo, metadata);
    let stored: Vec<u64> = stored_commits(&session_core::durable_store::lock(&session))
        .unwrap()
        .into_iter()
        .map(|(seq, _)| seq)
        .collect();
    assert_eq!(stored, acknowledged(name, backend), "{name}: journal records");
}

/// Upstream's `ReopeningStorage` (test/sqlite-storage.test.ts): every commit, failed or not, closes and reopens.
struct Reopening {
    current: JournalStorage,
    reopen: Box<dyn Fn() -> JournalStorage>,
    closed: bool,
}

impl Reopening {
    fn new(reopen: impl Fn() -> JournalStorage + 'static) -> Self {
        Reopening {
            current: reopen(),
            reopen: Box::new(reopen),
            closed: false,
        }
    }
}

impl Storage for Reopening {
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
    ($([$journal:ident, $reopen:ident] => $name:literal),* $(,)?) => {
        $(#[test]
        fn $journal() {
            let (_dir, repo, metadata) = session_file();
            replay($name, "memory", open_journal(&repo, &metadata));
            assert_journal_holds($name, "memory", &repo, &metadata);
        }

        #[test]
        fn $reopen() {
            let (_dir, repo, metadata) = session_file();
            let path = metadata.clone();
            replay($name, "jsonlReopen", Reopening::new(move || open_journal(&repo, &path)));
        })*

        #[test]
        fn session_core_journal_replays_every_upstream_conformance_case() {
            let names: Vec<&str> =
                FIXTURE["cases"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
            assert_eq!(names, vec![$($name),*]);
        }
    };
}

conformance!(
    [session_core_journal_reserves_id_1_for_the_immutable_root_conversation, session_core_journal_reopen_reserves_id_1_for_the_immutable_root_conversation] => "reserves ID 1 for the immutable root conversation",
    [session_core_journal_commits_mixed_table_writes_atomically_and_rolls_all_of_them_back_on_failure, session_core_journal_reopen_commits_mixed_table_writes_atomically_and_rolls_all_of_them_back_on_failure] => "commits mixed table writes atomically and rolls all of them back on failure",
    [session_core_journal_detaches_retained_writes_and_every_returned_record, session_core_journal_reopen_detaches_retained_writes_and_every_returned_record] => "detaches retained writes and every returned record",
    [session_core_journal_detaches_prototype_like_json_keys_without_changing_object_prototypes, session_core_journal_reopen_detaches_prototype_like_json_keys_without_changing_object_prototypes] => "detaches prototype-like JSON keys without changing object prototypes",
    [session_core_journal_indexes_entries_committed_out_of_id_order, session_core_journal_reopen_indexes_entries_committed_out_of_id_order] => "indexes entries committed out of ID order",
    [session_core_journal_continues_an_entry_cursor_below_its_last_item_after_a_newer_commit, session_core_journal_reopen_continues_an_entry_cursor_below_its_last_item_after_a_newer_commit] => "continues an entry cursor below its last item after a newer commit",
    [session_core_journal_paginates_conversations_by_opaque_cursor_in_ascending_id_order, session_core_journal_reopen_paginates_conversations_by_opaque_cursor_in_ascending_id_order] => "paginates conversations by opaque cursor in ascending ID order",
    [session_core_journal_filters_and_pages_conversations_by_durable_owner_edges, session_core_journal_reopen_filters_and_pages_conversations_by_durable_owner_edges] => "filters and pages conversations by durable owner edges",
    [session_core_journal_scans_deep_fork_history_newest_first_through_every_ancestor_cap, session_core_journal_reopen_scans_deep_fork_history_newest_first_through_every_ancestor_cap] => "scans deep fork history newest-first through every ancestor cap",
    [session_core_journal_scans_tables_in_either_id_order_and_continues_a_cursor_in_its_order, session_core_journal_reopen_scans_tables_in_either_id_order_and_continues_a_cursor_in_its_order] => "scans tables in either ID order and continues a cursor in its order",
    [session_core_journal_replaces_complete_task_records_and_pages_filtered_task_scans, session_core_journal_reopen_replaces_complete_task_records_and_pages_filtered_task_scans] => "replaces complete task records and pages filtered task scans",
    [session_core_journal_stores_owners_and_scans_waiting_and_completing_tasks_by_status, session_core_journal_reopen_stores_owners_and_scans_waiting_and_completing_tasks_by_status] => "stores owners and scans waiting and completing tasks by status",
    [session_core_journal_indexes_request_ids_per_conversation_and_replaces_complete_submission_records, session_core_journal_reopen_indexes_request_ids_per_conversation_and_replaces_complete_submission_records] => "indexes request IDs per conversation and replaces complete submission records",
    [session_core_journal_stores_passive_write_submissions_without_input_only_lifecycle_states, session_core_journal_reopen_stores_passive_write_submissions_without_input_only_lifecycle_states] => "stores passive write submissions without input-only lifecycle states",
    [session_core_journal_reconstructs_rewindable_documents_and_preserves_half_open_incarnations, session_core_journal_reopen_reconstructs_rewindable_documents_and_preserves_half_open_incarnations] => "reconstructs rewindable documents and preserves half-open incarnations",
    [session_core_journal_streams_long_document_tails_across_root_replacement_deltas, session_core_journal_reopen_streams_long_document_tails_across_root_replacement_deltas] => "streams long document tails across root replacement deltas",
    [session_core_journal_copies_stored_document_bases_independently_and_rejects_ambiguous_sources, session_core_journal_reopen_copies_stored_document_bases_independently_and_rejects_ambiguous_sources] => "copies stored document bases independently and rejects ambiguous sources",
    [session_core_journal_uses_bases_for_version_transitions_and_rejects_historical_reads_of_current_only_documents, session_core_journal_reopen_uses_bases_for_version_transitions_and_rejects_historical_reads_of_current_only_documents] => "uses bases for version transitions and rejects historical reads of current-only documents",
    [session_core_journal_indexes_logical_addresses_and_exact_scope_scans_independently, session_core_journal_reopen_indexes_logical_addresses_and_exact_scope_scans_independently] => "indexes logical addresses and exact-scope scans independently",
    [session_core_journal_keeps_document_lifecycle_failures_atomic_and_gives_create_plus_retire_an_empty_lifetime, session_core_journal_reopen_keeps_document_lifecycle_failures_atomic_and_gives_create_plus_retire_an_empty_lifetime] => "keeps document lifecycle failures atomic and gives create-plus-retire an empty lifetime",
    [session_core_journal_rolls_back_record_tables_and_secondary_indexes_when_a_document_command_fails, session_core_journal_reopen_rolls_back_record_tables_and_secondary_indexes_when_a_document_command_fails] => "rolls back record tables and secondary indexes when a document command fails",
    [session_core_journal_keeps_indexed_string_identities_lossless, session_core_journal_reopen_keeps_indexed_string_identities_lossless] => "keeps indexed string identities lossless",
    [session_core_journal_keeps_one_global_record_id_namespace_and_rejects_exhausted_id_minting, session_core_journal_reopen_keeps_one_global_record_id_namespace_and_rejects_exhausted_id_minting] => "keeps one global record ID namespace and rejects exhausted ID minting",
    [session_core_journal_rejects_every_operation_after_close, session_core_journal_reopen_rejects_every_operation_after_close] => "rejects every operation after close",
    [session_core_journal_reindexes_a_changed_submission_request_id, session_core_journal_reopen_reindexes_a_changed_submission_request_id] => "extra: a submission whose request ID changes leaves the old request ID unindexed",
);
