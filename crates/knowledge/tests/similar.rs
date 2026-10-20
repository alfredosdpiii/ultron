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

use knowledge::{SCHEMA_VERSION, Store, index_tree};

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
    git(root, &["commit", "-q", "-m", "Store question topic lists as append-only versions"]);
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
    assert!(!kinds.iter().any(|kind| kind == "sql:trigger:before-truncate"), "{kinds:?}");
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
    assert_eq!(second.read, 2, "only the two files the pull request adds are read: {second:?}");
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
print(json.dumps({'without': without, 'with': with_k}))";
    let out = kernel_python(
        body,
        &[&root.display().to_string(), &base, &head, &db.display().to_string(), REPO],
    );
    let briefs: serde_json::Value = serde_json::from_str(&out).unwrap();
    let without = briefs["without"].as_str().unwrap();
    let with = briefs["with"].as_str().unwrap();
    // Before: the files of the same name, the oldest first; the audit table is not among them.
    assert!(without.contains("same name elsewhere"), "{without}");
    assert!(without.contains("20250404191022_add_knowledge_interaction_models"), "{without}");
    assert!(!without.contains("add_ki_audit_event"), "{without}");
    // After: the migration that does what the new one does, with what they share.
    let like = with
        .lines()
        .find(|line| line.starts_with(&format!("Like {NEW}")))
        .unwrap_or_else(|| panic!("no Like line for the new migration in:\n{with}"));
    assert!(like.contains(AUDIT), "{like}");
    assert!(like.contains("trigger before-update"), "{like}");
    assert!(!with.contains("same name elsewhere"), "{with}");
}
