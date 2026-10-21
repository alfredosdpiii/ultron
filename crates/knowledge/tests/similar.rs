//! The knowledge graph end to end on the shape of AtticAIInc/fisher-ki-saas#2920, a review that missed a defect
//! because it was shown the wrong files. The pull request adds an append-only table whose migration refuses UPDATE
//! but not TRUNCATE; the repository already has an append-only audit table whose migration refuses UPDATE,
//! DELETE inside its retention window and TRUNCATE, and says why. Every Prisma migration is `migration.sql`, so
//! the brief's "same name elsewhere" list showed the five oldest migrations, none of them like the new one.
//!
//! Indexed here by the Rust writer and read by the kernel's Python reader (`rlm/knowledge_api.py`), so a schema
//! change on either side that the other does not follow fails these tests.

use std::path::{Path, PathBuf};
use std::process::Command;

use knowledge::{Note, SCHEMA_VERSION, Store, index_tree};

const REPO: &str = "github.com/example/saas";
const NEW: &str = "backend/prisma/migrations/20261014120000_add_question_topic_list_version/migration.sql";
const AUDIT: &str = "backend/prisma/migrations/20260808000000_add_ki_audit_event/migration.sql";

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=Knowledge Test",
            "-c",
            "user.email=knowledge@test",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

const AUDIT_SQL: &str = r#"
CREATE TABLE "ki_audit_event" (
    "id" UUID NOT NULL,
    "organization_id" UUID NOT NULL REFERENCES "organizations" ("id"),
    "action" TEXT NOT NULL,
    "created_at" TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT "ki_audit_event_pkey" PRIMARY KEY ("id")
);
CREATE INDEX "ki_audit_event_org_idx" ON "ki_audit_event" ("organization_id");

-- UPDATE is refused outright: an audit record that can be edited is not an audit record.
CREATE OR REPLACE FUNCTION "ki_audit_event_no_update"() RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION 'ki_audit_event is append-only (SOC 2 CC7.2): UPDATE is not permitted';
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER "ki_audit_event_no_update_trg"
    BEFORE UPDATE ON "ki_audit_event"
    FOR EACH ROW
    EXECUTE FUNCTION "ki_audit_event_no_update"();

-- DELETE is refused only inside the one-year retention window.
CREATE OR REPLACE FUNCTION "ki_audit_event_retention_guard"() RETURNS TRIGGER AS $$
BEGIN
    IF OLD."created_at" > now() - interval '1 year' THEN
        RAISE EXCEPTION 'ki_audit_event row % is inside the retention window', OLD."id";
    END IF;
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER "ki_audit_event_retention_trg"
    BEFORE DELETE ON "ki_audit_event"
    FOR EACH ROW
    EXECUTE FUNCTION "ki_audit_event_retention_guard"();

-- TRUNCATE fires neither trigger above: Postgres skips row-level DELETE triggers for it. TRUNCATE triggers must
-- be FOR EACH STATEMENT.
CREATE OR REPLACE FUNCTION "ki_audit_event_no_truncate"() RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION 'ki_audit_event is append-only: TRUNCATE is not permitted';
END;
$$ LANGUAGE plpgsql;
CREATE TRIGGER "ki_audit_event_no_truncate_trg"
    BEFORE TRUNCATE ON "ki_audit_event"
    FOR EACH STATEMENT
    EXECUTE FUNCTION "ki_audit_event_no_truncate"();
"#;

const NEW_SQL: &str = r#"
CREATE TABLE "question_topic_list_version" (
  "id" SERIAL PRIMARY KEY,
  "organization_id" UUID NOT NULL,
  "topics" JSONB NOT NULL,
  CONSTRAINT "question_topic_list_version_org_fkey" FOREIGN KEY ("organization_id")
      REFERENCES "organizations" ("id") ON DELETE CASCADE ON UPDATE CASCADE
);

-- Append-only. UPDATE is refused; DELETE is left to the organization cascade.
CREATE OR REPLACE FUNCTION question_topic_list_version_refuse_update()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'question_topic_list_version is append-only';
END $$;
DROP TRIGGER IF EXISTS "question_topic_list_version_no_update" ON "question_topic_list_version";
CREATE TRIGGER "question_topic_list_version_no_update"
  BEFORE UPDATE ON "question_topic_list_version"
  FOR EACH STATEMENT EXECUTE FUNCTION question_topic_list_version_refuse_update();
"#;

/// A repository at two commits: the base (ordinary migrations, an RLS migration and the audit table), then the
/// pull request (the new append-only table and its service).
fn repository(root: &Path) -> (String, String) {
    git(root, &["init", "-q", "-b", "main"]);
    let ordinary = [
        ("20250404191022_add_knowledge_interaction_models", "kis"),
        ("20250405125046_add_ki_chat_features", "chats"),
        ("20250407104145_add_related_images", "images"),
        ("20250429075336_update_user_for_cognito", "users"),
        ("20250430080016_add_organization_enums_and_relations", "organizations"),
        ("20250501000000_add_feedback", "feedback"),
    ];
    for (name, table) in ordinary {
        write(
            root,
            &format!("backend/prisma/migrations/{name}/migration.sql"),
            &format!(
                "CREATE TABLE \"{table}\" (\"id\" UUID NOT NULL, \"organization_id\" UUID REFERENCES \"organizations\" (\"id\") ON DELETE CASCADE);\n\
                 CREATE INDEX \"{table}_org_idx\" ON \"{table}\" (\"organization_id\");\n"
            ),
        );
    }
    write(
        root,
        "backend/prisma/migrations/20260601000000_add_tenant_rls/migration.sql",
        "ALTER TABLE \"kis\" ENABLE ROW LEVEL SECURITY;\nCREATE POLICY tenant_isolation ON \"kis\" USING (true);\n",
    );
    write(root, AUDIT, AUDIT_SQL);
    write(
        root,
        "backend/src/audit/audit.service.ts",
        "import { Injectable } from '@nestjs/common';\n@Injectable()\nexport class AuditService {\n  record() { return this.prisma.kiAuditEvent.create({}); }\n}\n",
    );
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    let base = git(root, &["rev-parse", "HEAD"]);
    write(root, NEW, NEW_SQL);
    write(
        root,
        "backend/src/topics/question-topics.service.ts",
        "import { Injectable } from '@nestjs/common';\n@Injectable()\nexport class QuestionTopicsService {\n  save() { return this.prisma.questionTopicListVersion.create({}); }\n}\n",
    );
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "commit",
            "-q",
            "-m",
            "Store question topic lists as append-only versions",
        ],
    );
    let head = git(root, &["rev-parse", "HEAD"]);
    (base, head)
}

fn python() -> String {
    std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into())
}

fn rlm_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm")
}

/// Runs `body` in the kernel's Python with the runtime modules importable; returns its stdout.
fn kernel_python(body: &str, args: &[&str]) -> String {
    let script = format!(
        "import sys, json\nsys.path.insert(0, {rlm:?})\n{body}\n",
        rlm = rlm_dir().display().to_string()
    );
    let output = Command::new(python())
        .arg("-I")
        .arg("-c")
        .arg(script)
        .args(args)
        .output()
        .expect("python runs");
    assert!(
        output.status.success(),
        "python failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn the_new_migration_is_indexed_with_its_trigger_shape() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (_, head) = repository(&root);
    let db = dir.path().join("knowledge/graph.db");
    let stats = index_tree(&db, REPO, &root, "HEAD").unwrap();
    assert_eq!(stats.rev, head);
    assert_eq!(stats.files, 11, "{stats:?}");
    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    let kinds: Vec<String> = store
        .constructs_at(REPO, &head, NEW)
        .unwrap()
        .into_iter()
        .map(|(kind, _, _)| kind)
        .collect();
    for kind in [
        "sql:create-table",
        "sql:foreign-key",
        "sql:on-delete-cascade",
        "sql:raise-exception",
        "sql:trigger:before-update",
        "sql:trigger:for-each-statement",
    ] {
        assert!(kinds.contains(&kind.to_string()), "{kind} missing from {kinds:?}");
    }
    assert!(
        !kinds.iter().any(|kind| kind == "sql:trigger:before-truncate"),
        "{kinds:?}"
    );
    let audit: Vec<String> = store
        .constructs_at(REPO, &head, AUDIT)
        .unwrap()
        .into_iter()
        .map(|(kind, _, _)| kind)
        .collect();
    assert!(audit.contains(&"sql:trigger:before-truncate".to_string()), "{audit:?}");
}

#[test]
fn a_later_revision_reads_only_its_new_blobs_and_keeps_both_trees() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (base, head) = repository(&root);
    let db = dir.path().join("graph.db");
    let first = index_tree(&db, REPO, &root, &base).unwrap();
    assert_eq!(first.read, 9, "{first:?}");
    let second = index_tree(&db, REPO, &root, &head).unwrap();
    assert_eq!(
        second.read, 2,
        "only the two files the pull request adds are read: {second:?}"
    );
    let again = index_tree(&db, REPO, &root, &head).unwrap();
    assert_eq!(again.read, 0, "{again:?}");
    let store = Store::open(&db).unwrap();
    assert_eq!(store.tree_count(REPO).unwrap(), 2);
    assert!(store.constructs_at(REPO, &base, NEW).unwrap().is_empty());
    assert!(!store.constructs_at(REPO, &head, NEW).unwrap().is_empty());
}

#[test]
fn a_store_from_a_newer_build_is_refused_not_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.db");
    drop(Store::open(&db).unwrap());
    set_schema_version(&db, SCHEMA_VERSION + 1);
    let error = Store::open(&db).err().expect("a newer schema is refused");
    assert!(error.0.contains("newer"), "{error}");
}

/// Sets the stored schema version through Python's `sqlite3`, as another build would have written it.
fn set_schema_version(db: &Path, version: i64) {
    let out = kernel_python(
        "import sqlite3\nconn = sqlite3.connect(sys.argv[1])\nconn.execute(\"UPDATE meta SET value = ? WHERE key = 'schema_version'\", (sys.argv[2],))\nconn.commit()\nprint('ok')",
        &[&db.display().to_string(), &version.to_string()],
    );
    assert_eq!(out, "ok");
}

#[test]
fn the_kernel_finds_the_audit_migration_as_the_one_like_the_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (_, head) = repository(&root);
    let db = dir.path().join("graph.db");
    index_tree(&db, REPO, &root, "HEAD").unwrap();
    let out = kernel_python(
        "from knowledge_api import Knowledge\n\
         k = Knowledge.open(sys.argv[1], sys.argv[2], sys.argv[3])\n\
         print(json.dumps([[s.path, s.shared] for s in k.similar(sys.argv[4])]))",
        &[&db.display().to_string(), REPO, &head, NEW],
    );
    let ranked: Vec<(String, Vec<String>)> = serde_json::from_str(&out).unwrap();
    assert!(!ranked.is_empty(), "{out}");
    assert_eq!(ranked[0].0, AUDIT, "{out}");
    assert!(ranked[0].1.contains(&"sql:trigger:before-update".to_string()), "{out}");
    // The ordinary migrations share only constructs most migrations have; they rank after it, if at all.
    for (path, _) in &ranked[1..] {
        assert_ne!(path, NEW);
    }
}

#[test]
fn the_kernel_reads_nothing_it_does_not_know() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (_, head) = repository(&root);
    let db = dir.path().join("graph.db");
    index_tree(&db, REPO, &root, "HEAD").unwrap();
    let out = kernel_python(
        "from knowledge_api import Knowledge\n\
         print(json.dumps([\n\
             Knowledge.open(sys.argv[1] + '.missing', sys.argv[2], sys.argv[3]) is None,\n\
             Knowledge.open(sys.argv[1], 'github.com/other/repo', sys.argv[3]) is None,\n\
             Knowledge.open(sys.argv[1], sys.argv[2], '0' * 40) is None,\n\
             Knowledge.open(None, sys.argv[2], sys.argv[3]) is None,\n\
         ]))",
        &[&db.display().to_string(), REPO, &head],
    );
    assert_eq!(out, "[true, true, true, true]");
    set_schema_version(&db, SCHEMA_VERSION + 1);
    let out = kernel_python(
        "from knowledge_api import Knowledge\nprint(Knowledge.open(sys.argv[1], sys.argv[2], sys.argv[3]) is None)",
        &[&db.display().to_string(), REPO, &head],
    );
    assert_eq!(out, "True", "another schema version is no knowledge");
}

#[test]
fn the_brief_names_the_audit_migration_instead_of_the_oldest_ones() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (base, head) = repository(&root);
    let db = dir.path().join("graph.db");
    index_tree(&db, REPO, &root, &head).unwrap();
    let body = "\
import autoreview_deep as deep
import review_api as r
from knowledge_api import Knowledge
root, base, head, db, repo = sys.argv[1:6]
files = r.parse_diff(r.Git(root).out('diff', '-U3', base, head, '--'))
reader = r._rev_reader(r.Git(root), head)
without = deep.build_brief(deep.Repo(root, head), files, reader, base=base).text
k = Knowledge.open(db, repo, head)
with_k = deep.build_brief(deep.Repo(root, head), files, reader, base=base, knowledge=k).text
small = deep.build_brief(deep.Repo(root, head), files, reader, base=base, knowledge=k, limit=900).text
print(json.dumps({'without': without, 'with': with_k, 'small': small}))";
    let out = kernel_python(
        body,
        &[
            &root.display().to_string(),
            &base,
            &head,
            &db.display().to_string(),
            REPO,
        ],
    );
    let briefs: serde_json::Value = serde_json::from_str(&out).unwrap();
    let without = briefs["without"].as_str().unwrap();
    let with = briefs["with"].as_str().unwrap();
    // Before: the files of the same name, the oldest first; the audit table is not among them.
    assert!(without.contains("same name elsewhere"), "{without}");
    assert!(
        without.contains("20250404191022_add_knowledge_interaction_models"),
        "{without}"
    );
    assert!(!without.contains("add_ki_audit_event"), "{without}");
    // After: the migration that does what the new one does, with what they share.
    let like = with
        .lines()
        .find(|line| line.starts_with(&format!("Like {NEW}")))
        .unwrap_or_else(|| panic!("no Like line for the new migration in:\n{with}"));
    assert!(like.contains(AUDIT), "{like}");
    assert!(like.contains("trigger before-update"), "{like}");
    assert!(!with.contains("same name elsewhere"), "{with}");
    // The pointer follows the claims, ahead of the long sections: a brief cut at its size limit keeps it (on the
    // real repository a 15-file change filled the brief before it).
    let small = briefs["small"].as_str().unwrap();
    assert!(small.ends_with("(brief cut at its size limit)"), "{small}");
    assert!(small.contains(&format!("Like {NEW}")), "{small}");
}

fn note(pull: i64, path: &str, author: &str, body: &str, state: &str) -> Note {
    Note {
        pull,
        path: path.into(),
        line: Some(65),
        author: author.into(),
        body: body.into(),
        url: format!("https://github.com/example/saas/pull/{pull}#discussion-{author}"),
        at: format!("2026-08-{:02}T10:00:00Z", pull % 28 + 1),
        state: state.into(),
        human: true,
        missed: false,
    }
}

#[test]
fn a_store_of_schema_1_gains_the_notes_and_keeps_its_trees() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (_, head) = repository(&root);
    let db = dir.path().join("graph.db");
    index_tree(&db, REPO, &root, "HEAD").unwrap();
    // As a build of schema 1 left it: no notes table.
    let out = kernel_python(
        "import sqlite3\nconn = sqlite3.connect(sys.argv[1])\nconn.execute('DROP TABLE notes')\nconn.execute(\"UPDATE meta SET value = '1' WHERE key = 'schema_version'\")\nconn.commit()\nprint('ok')",
        &[&db.display().to_string()],
    );
    assert_eq!(out, "ok");
    let mut store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    assert_eq!(store.tree_count(REPO).unwrap(), 1);
    assert!(!store.constructs_at(REPO, &head, NEW).unwrap().is_empty());
    assert_eq!(
        store
            .record_notes(REPO, &[note(2801, AUDIT, "jgericardo", "TRUNCATE?", "open")])
            .unwrap(),
        1
    );
}

#[test]
fn a_note_is_recorded_once_and_its_state_follows_the_thread() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.db");
    let mut store = Store::open(&db).unwrap();
    let first = note(
        2801,
        AUDIT,
        "samsamlopez",
        "TRUNCATE skips the row-level DELETE trigger.",
        "open",
    );
    assert_eq!(store.record_notes(REPO, std::slice::from_ref(&first)).unwrap(), 1);
    let addressed = Note {
        state: "addressed".into(),
        ..first.clone()
    };
    assert_eq!(
        store.record_notes(REPO, &[addressed]).unwrap(),
        0,
        "the same comment is not a new note"
    );
    assert_eq!(store.note_count(REPO).unwrap(), 1);
    let out = kernel_python(
        "import sqlite3\nprint(sqlite3.connect(sys.argv[1]).execute('SELECT state FROM notes').fetchone()[0])",
        &[&db.display().to_string()],
    );
    assert_eq!(out, "addressed");
    assert_eq!(store.meta("notes_backfilled:x").unwrap(), None);
    store
        .set_meta("notes_backfilled:x", "2026-10-21T00:00:00.000Z")
        .unwrap();
    assert_eq!(
        store.meta("notes_backfilled:x").unwrap().as_deref(),
        Some("2026-10-21T00:00:00.000Z")
    );
    assert!(
        store.set_meta("schema_version", "9").is_err(),
        "the version is not a host value"
    );
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
}

/// fisher-ki-saas#2920 with history: the audit table's pull request had a reviewer ask about TRUNCATE. An
/// investigator of the new append-only migration who asks `like_notes` gets that comment, through the constructs
/// the two migrations share, though the new file has no history of its own.
#[test]
fn an_investigator_gets_the_review_notes_of_files_like_the_one_it_reads() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (_, head) = repository(&root);
    let db = dir.path().join("graph.db");
    index_tree(&db, REPO, &root, "HEAD").unwrap();
    let mut store = Store::open(&db).unwrap();
    store
        .record_notes(
            REPO,
            &[
                note(
                    2801,
                    AUDIT,
                    "samsamlopez",
                    "TRUNCATE skips the row-level DELETE trigger; add a statement-level one.",
                    "addressed",
                ),
                note(
                    2801,
                    AUDIT,
                    "julesballaran",
                    "Name the retention window in the comment.",
                    "open",
                ),
                note(
                    2790,
                    "backend/prisma/migrations/20250501000000_add_feedback/migration.sql",
                    "jplbnrs",
                    "Index the organization column.",
                    "addressed",
                ),
            ],
        )
        .unwrap();
    drop(store);
    let body = "\
import autoreview_deep as deep
root, head, db, repo, new, audit = sys.argv[1:7]
r = deep.Repo(root, head)
r.knowledge = {'db': db, 'repo': repo, 'rev': head}
out = {}
for kind, path in (('like_notes', new), ('review_notes', audit), ('review_notes', new)):
    title, body = deep.serve_request(r, {kind: {'path': path}})
    out[kind + ':' + path] = [title, body]
bare = deep.Repo(root, head)
try:
    deep.serve_request(bare, {'like_notes': {'path': new}})
    out['bare'] = 'served'
except deep.Rejected as error:
    out['bare'] = str(error)
print(json.dumps(out))";
    let out = kernel_python(
        body,
        &[
            &root.display().to_string(),
            &head,
            &db.display().to_string(),
            REPO,
            NEW,
            AUDIT,
        ],
    );
    let served: serde_json::Value = serde_json::from_str(&out).unwrap();
    let like = served[format!("like_notes:{NEW}")][1].as_str().unwrap();
    // The addressed comment on the audit migration, with what the two files share; the open one is not offered,
    // nor the comment on a migration that shares only what every migration has.
    assert!(like.contains("#2801 samsamlopez on"), "{like}");
    assert!(like.contains("TRUNCATE skips the row-level DELETE trigger"), "{like}");
    assert!(like.contains("trigger before-update"), "{like}");
    assert!(!like.contains("retention window"), "{like}");
    assert!(!like.contains("Index the organization column"), "{like}");
    // On the file itself: both comments, the addressed one first.
    let own = served[format!("review_notes:{AUDIT}")][1].as_str().unwrap();
    let (addressed, open) = (own.find("samsamlopez").unwrap(), own.find("julesballaran").unwrap());
    assert!(addressed < open, "{own}");
    assert!(own.contains("[open, 2026-08-"), "{own}");
    assert_eq!(
        served[format!("review_notes:{NEW}")][1],
        "no earlier review comment on this file"
    );
    assert_eq!(served["bare"], "no review history is available for this repository");
}

#[test]
fn a_store_of_schema_2_gains_the_rule_columns_and_keeps_its_notes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.db");
    let mut store = Store::open(&db).unwrap();
    store
        .record_notes(REPO, &[note(2801, AUDIT, "samsamlopez", "TRUNCATE?", "addressed")])
        .unwrap();
    store
        .set_meta("notes_backfilled:x", "2026-10-21T00:00:00.000Z")
        .unwrap();
    drop(store);
    // As a build of schema 2 left it: notes without who wrote them, misses or credit.
    let out = kernel_python(
        "import sqlite3\nconn = sqlite3.connect(sys.argv[1])\n\
         conn.executescript('''ALTER TABLE notes RENAME TO old;\n\
         CREATE TABLE notes (id INTEGER PRIMARY KEY, repo INTEGER NOT NULL, pull INTEGER NOT NULL, path TEXT NOT NULL,\n\
         line INTEGER, author TEXT NOT NULL, body TEXT NOT NULL, url TEXT NOT NULL, at TEXT NOT NULL, state TEXT NOT NULL,\n\
         UNIQUE (repo, url));\n\
         INSERT INTO notes SELECT id, repo, pull, path, line, author, body, url, at, state FROM old; DROP TABLE old;\n\
         UPDATE meta SET value = '2' WHERE key = 'schema_version';''')\nconn.commit()\nprint('ok')",
        &[&db.display().to_string()],
    );
    assert_eq!(out, "ok");
    let mut store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    // Schema 2's addressed included outdated threads: none is a rule until the backfill reads them again.
    assert_eq!(store.meta("notes_backfilled:x").unwrap(), None);
    let state = kernel_python(
        "import sqlite3\nprint(sqlite3.connect(sys.argv[1]).execute('SELECT state FROM notes').fetchone()[0])",
        &[&db.display().to_string()],
    );
    assert_eq!(state, "outdated");
    let id = store
        .note_id(REPO, &note(2801, AUDIT, "samsamlopez", "", "").url)
        .unwrap()
        .unwrap();
    assert_eq!(store.rule_credit(id).unwrap(), Some((0, 0, false, true)));
    assert_eq!(
        store
            .credit_rules(REPO, &[(id, true), (id, false), (id, true)])
            .unwrap(),
        3
    );
    assert_eq!(store.credit_rules("github.com/other/repo", &[(id, true)]).unwrap(), 0);
    assert_eq!(store.rule_credit(id).unwrap(), Some((2, 1, false, true)));
    // A miss stays one, and the reviewing accounts' own comments are not a person's.
    let missed = Note {
        missed: true,
        ..note(2801, AUDIT, "samsamlopez", "TRUNCATE?", "addressed")
    };
    store.record_notes(REPO, &[missed]).unwrap();
    store
        .record_notes(REPO, &[note(2801, AUDIT, "samsamlopez", "TRUNCATE?", "addressed")])
        .unwrap();
    assert_eq!(store.rule_credit(id).unwrap(), Some((2, 1, true, true)));
    assert_eq!(store.mark_not_human(REPO, &["SamSamLopez".into()]).unwrap(), 1);
    assert_eq!(store.rule_credit(id).unwrap(), Some((2, 1, true, false)));
}

/// fisher-ki-saas#2920 as a rule: a person's addressed comment on the audit migration, which this reviewer missed,
/// is served for the new append-only migration (they share its rarest constructs), with an id a finding cites.
#[test]
fn rules_for_a_file_rank_by_credit_and_a_finding_cites_only_the_served() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (_, head) = repository(&root);
    let db = dir.path().join("graph.db");
    index_tree(&db, REPO, &root, "HEAD").unwrap();
    let mut store = Store::open(&db).unwrap();
    let truncate = Note {
        missed: true,
        ..note(
            2801,
            AUDIT,
            "samsamlopez",
            "TRUNCATE skips the row-level DELETE trigger.",
            "addressed",
        )
    };
    let window = note(2802, AUDIT, "julesballaran", "Name the retention window.", "addressed");
    let bot = Note {
        human: false,
        ..note(2803, AUDIT, "coderabbitai", "Consider an index.", "addressed")
    };
    let open = note(2804, AUDIT, "jgericardo", "Why not a view?", "open");
    let wrong = note(2805, AUDIT, "jplbnrs", "Use a sequence.", "addressed");
    store
        .record_notes(REPO, &[truncate.clone(), window.clone(), bot, open, wrong.clone()])
        .unwrap();
    let id = |n: &Note| store.note_id(REPO, &n.url).unwrap().unwrap();
    let (truncate_id, window_id, wrong_id) = (id(&truncate), id(&window), id(&wrong));
    // The window rule led to two accepted findings; the sequence rule to three rejected ones.
    store
        .credit_rules(
            REPO,
            &[
                (window_id, true),
                (window_id, true),
                (wrong_id, false),
                (wrong_id, false),
                (wrong_id, false),
            ],
        )
        .unwrap();
    drop(store);
    let body = "\
import autoreview_deep as deep
root, head, db, repo, new = sys.argv[1:6]
r = deep.Repo(root, head)
r.knowledge = {'db': db, 'repo': repo, 'rev': head}
title, served = deep.serve_request(r, {'rules_for': {'path': new}})
quote = r.lines(new)[1]
raw = {'file': new, 'line': 2, 'severity': 'high', 'category': 'correctness', 'claim': 'TRUNCATE is not refused',
       'why': 'w', 'scenario': 'TRUNCATE question_topic_list_versions', 'rules': ['R%d' % min(r.served_rules), 'R99999', 'x'],
       'evidence': [{'path': new, 'line': 2, 'quote': quote}]}
finding, why = deep.normalize_deep(r, raw, 'claims', {new: set(range(1, 200))}, lambda level, c, s: level)
print(json.dumps({'title': title, 'served': served, 'ids': sorted(r.served_rules), 'rules': (finding or {}).get('rules'), 'why': why}))";
    let out = kernel_python(
        body,
        &[&root.display().to_string(), &head, &db.display().to_string(), REPO, NEW],
    );
    let got: serde_json::Value = serde_json::from_str(&out).unwrap();
    let served = got["served"].as_str().unwrap();
    let lines: Vec<&str> = served.lines().collect();
    // The miss first (a person found what this reviewer did not), then the credited one; no bot, no open thread, no
    // discredited rule.
    assert_eq!(lines.len(), 2, "{served}");
    assert!(
        lines[0].starts_with(&format!(
            "R{truncate_id} #2801 samsamlopez on {AUDIT}:65 (like this file: "
        )),
        "{served}"
    );
    assert!(lines[0].contains("[this reviewer missed it on #2801]"), "{served}");
    assert!(lines[1].starts_with(&format!("R{window_id} #2802")), "{served}");
    assert!(
        lines[1].contains("[cited by 2 accepted and 0 rejected findings]"),
        "{served}"
    );
    assert_eq!(got["ids"], serde_json::json!([truncate_id, window_id]));
    assert_eq!(got["rules"], serde_json::json!([truncate_id]), "{got}");
}
