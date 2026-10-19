//! Typed memory records (kernel_apis::memory_records), file-linked pointers (kernel_apis::memory_pointers) and the
//! import readers (kernel_apis::memory_import). A new feature with no TypeScript reference: literal expectations,
//! through the worker's `memory.*` dispatch over an in-process Hindsight-shaped bank.

use host::file_hooks::{CellChanges, CellObserver, FileHooks, Snapshot};
use kernel::Signal;
use kernel_apis::memory::{MemoryBackend, handle};
use kernel_apis::memory_import::{self, Action, Harness};
use kernel_apis::memory_pointers::{MemoryPointers, file_pointers_enabled, session_pointers};
use kernel_apis::memory_records::{RecordIndex, glob_matches};
use kernel_apis::{DocumentStore, MemoryDocumentStore, MemoryService, worker_gate};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Documents by id, every retain request, with the given scope tags.
struct Bank {
    tags: Value,
    docs: Arc<Mutex<BTreeMap<String, (String, Value)>>>,
    retains: Arc<Mutex<Vec<Value>>>,
}

impl Bank {
    fn new(tags: Value) -> Bank {
        Bank {
            tags,
            docs: Arc::default(),
            retains: Arc::default(),
        }
    }

    /// Another session's view of the same bank.
    fn sibling(&self, tags: Value) -> Bank {
        Bank {
            tags,
            docs: self.docs.clone(),
            retains: self.retains.clone(),
        }
    }
}

impl MemoryBackend for Bank {
    fn namespace(&self) -> String {
        "fake://bank".into()
    }
    fn scope_tags(&self) -> Map<String, Value> {
        self.tags.as_object().unwrap().clone()
    }
    fn recall(&self, _: &Value, _: &Signal) -> Result<Value, String> {
        Ok(json!({"results": []}))
    }
    fn retain(&self, request: &Value, _: &Signal) -> Result<Value, String> {
        self.retains.lock().unwrap().push(request.clone());
        let item = &request["items"][0];
        self.docs.lock().unwrap().insert(
            item["document_id"].as_str().unwrap().to_string(),
            (item["content"].as_str().unwrap().to_string(), item["tags"].clone()),
        );
        Ok(json!({"success": true, "async": false}))
    }
    fn get(&self, id: &str, _: &Signal) -> Result<Value, String> {
        match self.docs.lock().unwrap().get(id) {
            Some((text, tags)) => Ok(json!({"id": id, "original_text": text, "tags": tags})),
            None => Err("HTTP_ERROR".into()),
        }
    }
    fn delete(&self, id: &str, _: &Signal) -> Result<Value, String> {
        self.docs.lock().unwrap().remove(id);
        Ok(json!({"success": true, "document_id": id}))
    }
    fn operation(&self, id: &str, _: &Signal) -> Result<Value, String> {
        Ok(json!({"operation_id": id, "status": "completed"}))
    }
}

fn tags(session: &str, project: &str) -> Value {
    json!({
        "session": [format!("ultron:session:{session}")],
        "project": [format!("ultron:project:{project}")],
        "global": ["ultron:global:home"],
    })
}

fn service(bank: Bank, index: &Arc<RecordIndex>, store: Arc<dyn DocumentStore>) -> MemoryService {
    MemoryService::new(store, Arc::new(bank), worker_gate())
        .unwrap()
        .with_records(index.clone())
}

fn call(service: &MemoryService, kind: &str, payload: Value) -> Result<Value, String> {
    handle(Some(service), kind, payload.as_object().unwrap(), &Signal::new())
}

fn pitfall(text: &str) -> Value {
    json!({
        "text": text, "evidence": [{"ref": "notes"}], "scope": "project", "type": "pitfall",
        "key": "pkill-self-match", "situation": "killing a background process from bash",
        "pathGlobs": ["scripts/**/*.sh"],
    })
}

fn with(mut base: Value, key: &str, value: Value) -> Value {
    base[key] = value;
    base
}

fn without(mut base: Value, key: &str) -> Value {
    base.as_object_mut().unwrap().remove(key);
    base
}

#[test]
fn kernel_apis_memory_typed_propose_validates_fields_and_caps() {
    let index = Arc::new(RecordIndex::in_memory());
    let bank = Bank::new(tags("s1", "p1"));
    let retains = bank.retains.clone();
    let memory = service(bank, &index, Arc::new(MemoryDocumentStore::default()));
    let schema = Err("Invalid memory request: memory.propose".to_string());
    let input = Err("Memory operation did not complete (INVALID_INPUT)".to_string());
    // The worker's schema: a known type, non-empty strings, a list of strings; `source` is import-only.
    for payload in [
        with(pitfall("x"), "type", json!("lesson")),
        with(pitfall("x"), "key", json!("")),
        with(pitfall("x"), "pathGlobs", json!("scripts/*.sh")),
        with(pitfall("x"), "pathGlobs", json!([1])),
        with(pitfall("x"), "source", json!("Codex ~/.codex/memories/MEMORY.md")),
    ] {
        assert_eq!(call(&memory, "memory.propose", payload.clone()), schema, "{payload}");
    }
    // The service's rules.
    let long_situation = "s".repeat(201);
    for payload in [
        without(pitfall("x"), "type"),
        without(pitfall("x"), "situation"),
        with(pitfall("x"), "situation", json!("two\nlines")),
        with(pitfall("x"), "situation", json!(long_situation)),
        with(pitfall("x"), "situation", json!("   ")),
        with(pitfall("x"), "key", json!("Not-Kebab")),
        with(pitfall("x"), "key", json!("double--dash")),
        with(pitfall("x"), "key", json!("k".repeat(65))),
        with(pitfall("x"), "pathGlobs", json!(["../outside/*.rs"])),
        with(pitfall("x"), "pathGlobs", json!(["/abs/*.rs"])),
        with(pitfall("x"), "pathGlobs", json!(["src//x"])),
        with(pitfall("x"), "pathGlobs", json!(["src/[ab"])),
        with(pitfall("x"), "pathGlobs", json!(["src/{a,b"])),
        with(pitfall("x"), "pathGlobs", json!(["a**b"])),
        with(
            pitfall("x"),
            "pathGlobs",
            json!((0..17).map(|i| format!("f{i}")).collect::<Vec<_>>()),
        ),
        pitfall(&"p".repeat(1025)),
        with(
            with(pitfall(&"f".repeat(4097)), "type", json!("fact")),
            "key",
            json!("f"),
        ),
        with(
            with(pitfall(&"r".repeat(2049)), "type", json!("preference")),
            "key",
            json!("r"),
        ),
        with(
            with(pitfall(&"q".repeat(8193)), "type", json!("procedure")),
            "key",
            json!("q"),
        ),
    ] {
        assert_eq!(call(&memory, "memory.propose", payload.clone()), input, "{payload}");
    }
    assert!(
        retains.lock().unwrap().is_empty(),
        "nothing invalid reached the backend"
    );
    // At the caps: stored.
    for (kind, size) in [
        ("pitfall", 1024),
        ("preference", 2048),
        ("fact", 4096),
        ("reference", 4096),
        ("procedure", 8192),
    ] {
        let payload = with(
            with(pitfall(&"c".repeat(size)), "type", json!(kind)),
            "key",
            json!(kind),
        );
        let stored = call(&memory, "memory.propose", payload).unwrap();
        assert_eq!(stored["state"], "stored", "{kind}");
        assert_eq!(stored["record"]["type"], kind);
    }
    let situation = "s".repeat(200);
    let at_cap = with(
        with(pitfall("x"), "situation", json!(situation)),
        "key",
        json!("max-situation"),
    );
    assert_eq!(call(&memory, "memory.propose", at_cap).unwrap()["state"], "stored");
}

#[test]
fn kernel_apis_memory_typed_record_reaches_backend_metadata_and_index() {
    let index = Arc::new(RecordIndex::in_memory());
    let bank = Bank::new(tags("s1", "p1"));
    let retains = bank.retains.clone();
    let memory = service(bank, &index, Arc::new(MemoryDocumentStore::default()));
    let stored = call(
        &memory,
        "memory.propose",
        pitfall("pkill -f matches the tool shell itself"),
    )
    .unwrap();
    assert_eq!(stored["kind"], "propose");
    assert_eq!(stored["state"], "stored");
    assert_eq!(
        stored["record"],
        json!({"type": "pitfall", "key": "pkill-self-match", "situation": "killing a background process from bash",
               "pathGlobs": ["scripts/**/*.sh"], "indexed": true})
    );
    let id = stored["memoryId"].as_str().unwrap().to_string();
    let metadata = retains.lock().unwrap()[0]["items"][0]["metadata"].clone();
    assert_eq!(metadata["ultron_type"], "pitfall");
    assert_eq!(metadata["ultron_key"], "pkill-self-match");
    assert_eq!(metadata["ultron_situation"], "killing a background process from bash");
    let entries = index.entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, id);
    assert_eq!(entries[0].scope, "project");
    assert_eq!(entries[0].tags, vec!["ultron:project:p1".to_string()]);
    assert_eq!(entries[0].typed.path_globs, vec!["scripts/**/*.sh".to_string()]);
    // The journal still holds no memory text or typed text, only hashes.
    let listed = call(&memory, "memory.list", json!({})).unwrap().to_string();
    assert!(!listed.contains("pkill") && !listed.contains("killing"), "{listed}");
    // An untyped record is as before: no `record`, no typed metadata, nothing indexed.
    let plain = call(
        &memory,
        "memory.propose",
        json!({"text": "plain fact", "evidence": [{"ref": "x"}]}),
    )
    .unwrap();
    assert!(plain.get("record").is_none());
    let metadata = retains.lock().unwrap()[1]["items"][0]["metadata"].clone();
    assert_eq!(
        metadata.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["ultron_operation", "ultron_evidence_class"]
    );
    assert_eq!(index.entries().unwrap().len(), 1);
    // get adds the record's typed fields.
    let read = call(&memory, "memory.get", json!({"id": id})).unwrap();
    assert_eq!(read["content"], "pkill -f matches the tool shell itself");
    assert_eq!(read["record"]["situation"], "killing a background process from bash");
}

#[test]
fn kernel_apis_memory_key_upsert_replaces_the_record_in_and_across_sessions() {
    let index = Arc::new(RecordIndex::in_memory());
    let store: Arc<dyn DocumentStore> = Arc::new(MemoryDocumentStore::default());
    let bank = Bank::new(tags("s1", "p1"));
    let docs = bank.docs.clone();
    let other_project = bank.sibling(tags("s3", "p2"));
    let second = bank.sibling(tags("s2", "p1"));
    let memory = service(bank, &index, store.clone());
    let first = call(&memory, "memory.propose", pitfall("first text")).unwrap();
    let id = first["memoryId"].clone();
    // The same key again in the same session: a correction of that document.
    let again = call(&memory, "memory.propose", pitfall("second text")).unwrap();
    assert_eq!(again["kind"], "correct");
    assert_eq!(again["memoryId"], id);
    assert_eq!(again["supersedes"], first["id"]);
    assert_eq!(index.entries().unwrap().len(), 1);
    assert_eq!(docs.lock().unwrap()[id.as_str().unwrap()].0, "second text");
    // The journal it wrote reloads (the TS journal format is unchanged).
    let reopened = service(Bank::new(tags("s1", "p1")), &index, store.clone());
    assert_eq!(
        call(&reopened, "memory.list", json!({}))
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    // Another session of the same project: a new claim on the same document, then its own corrections.
    let store2: Arc<dyn DocumentStore> = Arc::new(MemoryDocumentStore::default());
    let other = service(second, &index, store2.clone());
    let replaced = call(&other, "memory.propose", pitfall("third text")).unwrap();
    assert_eq!(replaced["kind"], "propose");
    assert_eq!(replaced["memoryId"], id);
    assert!(replaced.get("supersedes").is_none());
    assert_eq!(docs.lock().unwrap()[id.as_str().unwrap()].0, "third text");
    let corrected = call(&other, "memory.propose", pitfall("fourth text")).unwrap();
    assert_eq!(corrected["kind"], "correct");
    assert_eq!(corrected["memoryId"], id);
    let reopened2 = service(Bank::new(tags("s2", "p1")), &index, store2);
    assert_eq!(
        call(&reopened2, "memory.list", json!({}))
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(index.entries().unwrap().len(), 1);
    // A session of another project does not see the key: a record of its own.
    let elsewhere = service(other_project, &index, Arc::new(MemoryDocumentStore::default()));
    let own = call(&elsewhere, "memory.propose", pitfall("other project")).unwrap();
    assert_ne!(own["memoryId"], id);
    assert_eq!(index.entries().unwrap().len(), 2);
}

#[test]
fn kernel_apis_memory_get_reads_an_indexed_record_of_another_session() {
    let index = Arc::new(RecordIndex::in_memory());
    let bank = Bank::new(tags("s1", "p1"));
    let later = bank.sibling(tags("s2", "p1"));
    let stranger = bank.sibling(tags("s3", "p2"));
    let writer = service(bank, &index, Arc::new(MemoryDocumentStore::default()));
    let stored = call(&writer, "memory.propose", pitfall("bracket the pkill pattern")).unwrap();
    let id = stored["memoryId"].as_str().unwrap().to_string();
    let reader = service(later, &index, Arc::new(MemoryDocumentStore::default()));
    let read = call(&reader, "memory.get", json!({"id": id})).unwrap();
    assert_eq!(read["content"], "bracket the pkill pattern");
    assert_eq!(read["state"], "stored");
    assert_eq!(read["scope"], "project");
    assert_eq!(read["record"]["key"], "pkill-self-match");
    // Another project's session cannot read it.
    let other = service(stranger, &index, Arc::new(MemoryDocumentStore::default()));
    assert_eq!(
        call(&other, "memory.get", json!({"id": id})),
        Err("Memory operation did not complete (UNKNOWN_MEMORY)".into())
    );
}

#[test]
fn kernel_apis_memory_typed_record_with_a_secret_is_refused_and_not_indexed() {
    let index = Arc::new(RecordIndex::in_memory());
    let bank = Bank::new(tags("s1", "p1"));
    let retains = bank.retains.clone();
    let memory = service(bank, &index, Arc::new(MemoryDocumentStore::default()));
    let secret = format!("when using api_key={}", "Zx9".repeat(6));
    let refused = call(
        &memory,
        "memory.propose",
        with(pitfall("harmless"), "situation", json!(secret)),
    )
    .unwrap();
    assert_eq!(refused["state"], "sensitive");
    assert_eq!(refused["record"]["indexed"], false);
    let token = format!("sk-{}", "a1B2c3D4".repeat(3));
    let refused = call(&memory, "memory.propose", pitfall(&format!("use {token}"))).unwrap();
    assert_eq!(refused["state"], "sensitive");
    assert!(retains.lock().unwrap().is_empty());
    assert!(index.entries().unwrap().is_empty());
}

#[test]
fn kernel_apis_memory_forget_and_correct_keep_the_index_in_step() {
    let index = Arc::new(RecordIndex::in_memory());
    let memory = service(
        Bank::new(tags("s1", "p1")),
        &index,
        Arc::new(MemoryDocumentStore::default()),
    );
    let stored = call(&memory, "memory.propose", pitfall("short")).unwrap();
    let id = stored["memoryId"].clone();
    // A pitfall stays a pitfall: its correction is held to the 1 KiB cap.
    assert_eq!(
        call(
            &memory,
            "memory.correct",
            json!({"id": id, "text": "x".repeat(1025), "evidence": [{"ref": "r"}]})
        ),
        Err("Memory operation did not complete (INVALID_INPUT)".into())
    );
    let corrected = call(
        &memory,
        "memory.correct",
        json!({"id": id, "text": "better", "evidence": [{"ref": "r"}]}),
    )
    .unwrap();
    assert_eq!(corrected["record"]["type"], "pitfall");
    assert_eq!(
        index.entries().unwrap()[0].text_hash,
        kernel_apis::util::sha256_hex(b"better")
    );
    let forgotten = call(&memory, "memory.forget", json!({"id": id})).unwrap();
    assert_eq!(forgotten["state"], "forgotten");
    assert!(index.entries().unwrap().is_empty());
    // A new propose of the key is a new document.
    let fresh = call(&memory, "memory.propose", pitfall("again")).unwrap();
    assert_eq!(fresh["kind"], "propose");
    assert_ne!(fresh["memoryId"], id);
}

#[test]
fn kernel_apis_memory_record_index_file_is_shared_and_locked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory/records.json");
    let a = Arc::new(RecordIndex::file(&path));
    let b = Arc::new(RecordIndex::file(&path));
    let memory_a = service(
        Bank::new(tags("s1", "p1")),
        &a,
        Arc::new(MemoryDocumentStore::default()),
    );
    let memory_b = service(
        Bank::new(tags("s2", "p1")),
        &b,
        Arc::new(MemoryDocumentStore::default()),
    );
    // Both sessions write at once; neither loses the other's records.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for n in 0..10 {
                let payload = with(pitfall("a"), "key", json!(format!("a-{n}")));
                call(&memory_a, "memory.propose", payload).unwrap();
            }
        });
        scope.spawn(|| {
            for n in 0..10 {
                let payload = with(pitfall("b"), "key", json!(format!("b-{n}")));
                call(&memory_b, "memory.propose", payload).unwrap();
            }
        });
    });
    assert_eq!(RecordIndex::file(&path).entries().unwrap().len(), 20);
    let saved: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(saved["version"], 1);
}

#[test]
fn kernel_apis_memory_globs_match_repository_paths() {
    for (pattern, hits, misses) in [
        (
            "**/*.rs",
            &["src/main.rs", "main.rs", "a/b/c.rs"][..],
            &["src/main.py", "src/main.rsx"][..],
        ),
        (
            "src/**/*.rs",
            &["src/bin/main.rs", "src/main.rs"],
            &["tests/main.rs", "src2/main.rs"],
        ),
        (
            "scripts/*.sh",
            &["scripts/run.sh"],
            &["scripts/nested/run.sh", "scripts/run.shx"],
        ),
        ("Cargo.toml", &["Cargo.toml"], &["crates/app/Cargo.toml", "Cargo_toml"]),
        (
            "docs/**",
            &["docs/a.md", "docs/x/y.md"],
            &["docsx/a.md", "src/docs/a.md"],
        ),
        ("src/?.ts", &["src/a.ts"], &["src/ab.ts", "src/a/b.ts"]),
        ("src/[ab].ts", &["src/a.ts", "src/b.ts"], &["src/c.ts"]),
        ("src/[!ab].ts", &["src/c.ts"], &["src/a.ts", "src//.ts"]),
        ("web/*.{ts,tsx}", &["web/a.ts", "web/a.tsx"], &["web/a.js", "web/a.t"]),
        ("a+b (1).txt", &["a+b (1).txt"], &["aab (1).txt"]),
    ] {
        for hit in hits {
            assert!(glob_matches(pattern, hit), "{pattern} must match {hit}");
        }
        for miss in misses {
            assert!(!glob_matches(pattern, miss), "{pattern} must not match {miss}");
        }
    }
}

fn indexed(id: &str, globs: &[&str], scope: &str, tag: &str) -> kernel_apis::memory_records::IndexEntry {
    kernel_apis::memory_records::IndexEntry {
        id: id.into(),
        typed: kernel_apis::memory_records::Typed {
            kind: "pitfall".into(),
            key: None,
            situation: format!("situation of {id}"),
            path_globs: globs.iter().map(|g| g.to_string()).collect(),
            source: None,
        },
        scope: scope.into(),
        tags: vec![tag.into()],
        text_hash: "0".repeat(64),
        updated_at: String::new(),
    }
}

#[test]
fn kernel_apis_memory_pointers_are_capped_deduplicated_and_scoped() {
    let index = Arc::new(RecordIndex::in_memory());
    for n in 1..=5 {
        index
            .upsert(indexed(
                &format!("m{n}"),
                &["src/**/*.rs"],
                "project",
                "ultron:project:p1",
            ))
            .unwrap();
    }
    index
        .upsert(indexed("other", &["src/**/*.rs"], "project", "ultron:project:p2"))
        .unwrap();
    index
        .upsert(indexed("docs", &["docs/*.md"], "global", "ultron:global:home"))
        .unwrap();
    index
        .upsert(indexed("unscoped", &[], "project", "ultron:project:p1"))
        .unwrap();
    let pointers = MemoryPointers::new(index.clone(), tags("s1", "p1").as_object().unwrap().clone());
    let cwd = Path::new("/w");
    let changes = |files: &[&str], checked: &[&str]| CellChanges {
        files: files.iter().map(|f| cwd.join(f)).collect(),
        checked: checked.iter().map(|f| cwd.join(f)).collect(),
        deleted: vec![cwd.join("src/gone.rs")],
        complete: true,
    };
    assert_eq!(
        pointers.cell_changed(&changes(&["src/lib.rs"], &[]), cwd, "main"),
        vec![
            "[memory] m1 pitfall: situation of m1",
            "[memory] m2 pitfall: situation of m2",
            "[memory] m3 pitfall: situation of m3",
        ]
    );
    // Once per record: the next change points at the rest; a checked write counts like any other change.
    assert_eq!(
        pointers.cell_changed(&changes(&[], &["src/main.rs", "docs/a.md"]), cwd, "main"),
        vec![
            "[memory] m4 pitfall: situation of m4",
            "[memory] m5 pitfall: situation of m5",
            "[memory] docs pitfall: situation of docs",
        ]
    );
    assert!(
        pointers
            .cell_changed(&changes(&["src/lib.rs", "docs/a.md"], &[]), cwd, "main")
            .is_empty()
    );
    // Deleted files and files outside the directory point at nothing.
    let fresh = MemoryPointers::new(index, tags("s1", "p1").as_object().unwrap().clone());
    let outside = CellChanges {
        files: vec![PathBuf::from("/elsewhere/src/lib.rs")],
        checked: Vec::new(),
        deleted: vec![cwd.join("src/lib.rs")],
        complete: true,
    };
    assert!(fresh.cell_changed(&outside, cwd, "main").is_empty());
    assert!(file_pointers_enabled(&json!({})));
    assert!(file_pointers_enabled(&json!({"memory": {"filePointers": true}})));
    assert!(!file_pointers_enabled(&json!({"memory": {"filePointers": false}})));
    let filters = tags("s1", "p1").as_object().unwrap().clone();
    let off = json!({"memory": {"filePointers": false}});
    assert!(session_pointers(&off, Arc::new(RecordIndex::in_memory()), filters.clone()).is_none());
    let on = session_pointers(&json!({}), Arc::new(RecordIndex::in_memory()), filters).unwrap();
    assert!(on.enabled());
}

/// The observer through `FileHooks`: snapshots taken because it watches, its lines delivered verbatim with the
/// lane's pending findings.
#[test]
fn kernel_apis_memory_pointers_reach_the_file_hooks_pending_lines() {
    let index = Arc::new(RecordIndex::in_memory());
    index
        .upsert(indexed("m1", &["src/*.rs"], "project", "ultron:project:p1"))
        .unwrap();
    let pictures: Arc<Mutex<Vec<BTreeMap<String, String>>>> = Arc::new(Mutex::new(vec![
        BTreeMap::new(),
        BTreeMap::from([("src/lib.rs".to_string(), "1".to_string())]),
    ]));
    let source = pictures.clone();
    let hooks = FileHooks::with_snapshots(
        "/w",
        Arc::new(move |root: &Path| {
            let mut pictures = source.lock().unwrap();
            let entries = if pictures.len() > 1 {
                pictures.remove(0)
            } else {
                pictures[0].clone()
            };
            Ok(Snapshot {
                root: root.to_path_buf(),
                entries,
                complete: true,
            })
        }),
    );
    assert!(!hooks.watches_cells());
    let pointers = Arc::new(MemoryPointers::new(
        index,
        tags("s1", "p1").as_object().unwrap().clone(),
    ));
    hooks.observe(pointers);
    assert!(hooks.watches_cells());
    hooks.cell_started("main");
    hooks.cell_ended("main");
    hooks.settled();
    assert_eq!(
        hooks.take_pending("main").as_deref(),
        Some("[memory] m1 pitfall: situation of m1")
    );
    assert_eq!(hooks.take_pending("main"), None);
}

// --- import readers --------------------------------------------------------------------------------------------------

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn kernel_apis_memory_import_reads_claude_code_notes_as_mirror_does() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().canonicalize().unwrap().join("home");
    let checkout = home.join("src/my-app");
    std::fs::create_dir_all(&checkout).unwrap();
    let encoded = checkout.display().to_string().replace('/', "-");
    let memory = home.join(".claude/projects").join(&encoded).join("memory");
    write(&memory.join("MEMORY.md"), "- [Index](a.md)\n");
    write(
        &memory.join("build.md"),
        "---\nname: Build Steps\ndescription: when building the app\nmetadata:\n  type: project\n---\n\nRun make build.\n",
    );
    write(
        &memory.join("style.md"),
        "---\nname: style\ndescription: writing code\ntype: feedback\n---\nNo emojis.\n",
    );
    write(&memory.join("plain.md"), "Just a note without frontmatter.\n");
    write(&memory.join("empty.md"), "---\nname: empty\n---\n\n");
    write(&memory.join("old/stale.md"), "superseded\n");
    write(&memory.join(".hidden.md"), "hidden\n");
    write(&memory.join("nested/deep.md"), "---\nname: deep\n---\nDeep note.\n");
    // A project directory whose name maps to no existing directory: its notes are global.
    let orphan = home.join(".claude/projects/-nowhere-at-all/memory");
    write(&orphan.join("x.md"), "orphan note\n");
    let root = memory_import::notes_root(Harness::ClaudeCode, &home, &|_| None);
    assert_eq!(root, home.join(".claude/projects"));
    let sources = memory_import::discover(Harness::ClaudeCode, &root);
    assert_eq!(sources.len(), 2);
    let project = sources.iter().find(|s| s.directory == memory).unwrap();
    assert_eq!(project.project.as_deref(), Some(checkout.as_path()));
    assert_eq!(sources.iter().find(|s| s.directory == orphan).unwrap().project, None);
    let notes = project.read().unwrap();
    let summary: Vec<(&str, &str, &str, &str)> = notes
        .iter()
        .map(|n| (n.key.as_str(), n.kind, n.situation.as_str(), n.content.as_str()))
        .collect();
    assert_eq!(
        summary,
        vec![
            ("build-steps", "fact", "when building the app", "Run make build."),
            ("deep", "reference", "", "Deep note."),
            ("plain", "reference", "", "Just a note without frontmatter."),
            ("style", "preference", "writing code", "No emojis."),
        ]
    );
    // CLAUDE_CONFIG_DIR moves the configuration directory.
    let moved = memory_import::notes_root(Harness::ClaudeCode, &home, &|key| {
        (key == "CLAUDE_CONFIG_DIR").then(|| "/elsewhere/claude".to_string())
    });
    assert_eq!(moved, PathBuf::from("/elsewhere/claude/projects"));
}

#[test]
fn kernel_apis_memory_import_reads_codex_memories_as_mirror_does() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let memories = home.join(".codex/memories");
    write(
        &memories.join("memory_summary.md"),
        "# Summary\n\n## User Profile\nA backend engineer.\nWorks in Rust.\n\n## User preferences\n- Short answers\n  without filler\n- Tests first\n\n## General Tips\n- Run the linter before committing\n## Other\n- ignored\n",
    );
    write(
        &memories.join("MEMORY.md"),
        "## Task 2: Fix the flaky build\n\n## Reusable knowledge\n- The cache lives in target/ [Task 2]\n- Unknown tag stays [Task 9]\n\n## Failures and how to do differently\n- Do not pkill -f the shell\n\n## User preferences\n- [Task 2]\n",
    );
    let root = memory_import::notes_root(Harness::Codex, &home, &|_| None);
    let sources = memory_import::discover(Harness::Codex, &root);
    assert_eq!(sources.len(), 1);
    let notes = sources[0].read().unwrap();
    let summary: Vec<(&str, &str, &str)> = notes
        .iter()
        .map(|n| (n.kind, n.situation.as_str(), n.content.as_str()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (
                "fact",
                "who the user is, as Codex summarized it",
                "A backend engineer. Works in Rust."
            ),
            (
                "preference",
                "how the user wants work done, as Codex summarized it",
                "Short answers without filler"
            ),
            (
                "preference",
                "how the user wants work done, as Codex summarized it",
                "Tests first"
            ),
            (
                "procedure",
                "working practice Codex learned across tasks",
                "Run the linter before committing"
            ),
            ("fact", "Codex task: Fix the flaky build", "The cache lives in target/"),
            ("fact", "from the user's Codex task memory", "Unknown tag stays"),
            (
                "pitfall",
                "from the user's Codex task memory",
                "Do not pkill -f the shell"
            ),
        ]
    );
    assert_eq!(notes[0].key, "codex-user-profile");
    assert_eq!(notes[2].key, "codex-pref-tests-first");
    assert!(memory_import::discover(Harness::Codex, &home.join("nothing")).is_empty());
    let moved = memory_import::notes_root(Harness::Codex, &home, &|key| (key == "CODEX_HOME").then(|| "/c".into()));
    assert_eq!(moved, PathBuf::from("/c/memories"));
}

#[test]
fn kernel_apis_memory_import_plan_keys_refusals_and_reruns() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let memories = home.join(".codex/memories");
    let token = format!("ghp_{}", "a".repeat(36));
    write(
        &memories.join("MEMORY.md"),
        &format!(
            "## Reusable knowledge\n- Same bullet\n- Same bullet\n- The token is {token}\n\n## Failures and how to do differently\n- {}\n",
            "long ".repeat(250)
        ),
    );
    let sources = memory_import::discover(Harness::Codex, &memories);
    let index = Arc::new(RecordIndex::in_memory());
    let filters = |_: Option<&Path>| tags("import", "p1").as_object().unwrap().clone();
    let plan = memory_import::plan(&sources, &home, &index, &filters).unwrap();
    assert_eq!(plan.len(), 4);
    let keys: Vec<&str> = plan.iter().map(|p| p.typed.key.as_deref().unwrap()).collect();
    assert!(keys[0].starts_with("codex-fact-same-bullet-") && keys[0].len() == "codex-fact-same-bullet-".len() + 12);
    assert_ne!(keys[0], keys[1], "a repeated note gets its own key");
    assert_eq!(plan[0].action, Action::Add);
    assert_eq!(plan[1].action, Action::Add);
    assert_eq!(
        plan[2].action,
        Action::Refuse("holds a secret or personal identifier".into())
    );
    assert_eq!(
        plan[3].action,
        Action::Refuse("over the 1024-byte cap of a pitfall record".into())
    );
    assert_eq!(plan[0].scope, "global");
    assert_eq!(
        plan[0].typed.source.as_deref(),
        Some("Codex ~/.codex/memories/MEMORY.md")
    );
    // Import the first through a service: the rerun plans it unchanged; changed text would be a replacement.
    let memory = service(
        Bank::new(tags("import", "p1")),
        &index,
        Arc::new(MemoryDocumentStore::default()),
    );
    let stored = memory_import::import_one(&memory, &plan[0]).unwrap();
    assert_eq!(stored["state"], "stored");
    assert_eq!(stored["evidenceClass"], "user_statement");
    assert_eq!(stored["record"]["source"], "Codex ~/.codex/memories/MEMORY.md");
    let rerun = memory_import::plan(&sources, &home, &index, &filters).unwrap();
    assert_eq!(
        rerun[0].action,
        Action::Unchanged(stored["memoryId"].as_str().unwrap().into())
    );
    assert_eq!(rerun[1].action, Action::Add);
    let mut entry = index.entries().unwrap().remove(0);
    entry.text_hash = "f".repeat(64);
    index.upsert(entry).unwrap();
    let changed = memory_import::plan(&sources, &home, &index, &filters).unwrap();
    assert_eq!(
        changed[0].action,
        Action::Replace(stored["memoryId"].as_str().unwrap().into())
    );
}
