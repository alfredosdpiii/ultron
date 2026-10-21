//! The graph a review queries (CodexGraph, arXiv 2408.03910): symbols with their ranges and parents, calls by the
//! symbol they are made from, imports with the files they name. Indexed by the Rust writer and queried through the
//! kernel's reader (`rlm/knowledge_api.py`, `Knowledge.query`) and the investigators' `graph` request
//! (`rlm/autoreview_deep.py`), so a change on either side that the other does not follow fails here.
//!
//! The shape is fisher-ki-saas's: handlers that bypass row-level security through `runWithBypass`, which a review
//! of one new handler needs to see across the whole repository, not only in the files the change reaches.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use knowledge::{SCHEMA_VERSION, Store, index_tree};

const REPO: &str = "github.com/example/saas";

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
        .stdin(Stdio::null())
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

const DB: &str = "\
export class DbService {
  runWithBypass(work) {
    return work(this.client);
  }
}
";

const TOPICS: &str = "\
import { Controller, Get, Post, UseGuards } from '@nestjs/common';
import { DbService } from '../db.service';
import { AdminGuard } from '../auth/admin.guard';

@Controller('topics')
export class TopicsController {
  constructor(private readonly db: DbService) {}

  @Get()
  list() {
    return this.db.runWithBypass((client) => client.topic.findMany());
  }

  @UseGuards(AdminGuard)
  @Post('purge')
  purge() {
    return this.db.runWithBypass((client) => client.topic.deleteMany());
  }
}
";

const AUDIT: &str = "\
import { DbService } from '../db.service';

export class AuditService {
  record(event) {
    return this.db.runWithBypass((client) => client.audit.create({ data: event }));
  }
}
";

const ROUTES: &str = "\
from fastapi import APIRouter
from .store import Store

router = APIRouter()

@router.post('/modify')
def modify(request):
    return Store().save(request)
";

const STORE: &str = "\
class Store:
    def save(self, request):
        return self.session.add(request)
";

/// Two commits: the base, and one adding a file the base does not have.
fn repository(root: &Path) -> (String, String) {
    git(root, &["init", "-q", "-b", "main"]);
    write(root, "backend/src/db.service.ts", DB);
    write(root, "backend/src/topics/topics.controller.ts", TOPICS);
    write(root, "backend/src/audit/audit.service.ts", AUDIT);
    write(root, "backend/src/auth/admin.guard.ts", "export class AdminGuard {}\n");
    write(root, "ml/app/routes.py", ROUTES);
    write(root, "ml/app/store.py", STORE);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    let base = git(root, &["rev-parse", "HEAD"]);
    write(
        root,
        "backend/src/reports/reports.service.ts",
        "export class ReportsService {\n  export() {\n    return this.db.runWithBypass((client) => client.report.findMany());\n  }\n}\n",
    );
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "Export reports"]);
    (base, git(root, &["rev-parse", "HEAD"]))
}

fn rlm_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/rlm")
}

/// Runs `body` in the kernel's Python with the runtime modules importable; returns its stdout as JSON.
fn kernel_json(body: &str, args: &[&str]) -> serde_json::Value {
    let script = format!(
        "import sys, json\nsys.path.insert(0, {rlm:?})\n{body}\n",
        rlm = rlm_dir().display().to_string()
    );
    let python = std::env::var("ULTRON_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    let output = Command::new(python)
        .arg("-I")
        .arg("-c")
        .arg(script)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("python runs");
    assert!(
        output.status.success(),
        "python failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stdout)))
}

fn indexed() -> (tempfile::TempDir, PathBuf, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (base, head) = repository(&root);
    let db = dir.path().join("knowledge/graph.db");
    index_tree(&db, REPO, &root, &base).unwrap();
    index_tree(&db, REPO, &root, &head).unwrap();
    (dir, db, base, head)
}

#[test]
fn a_graph_query_answers_across_the_repository_at_the_reviewed_revision() {
    let (_dir, db, base, head) = indexed();
    let body = "\
from knowledge_api import Knowledge
db, repo, base, head = sys.argv[1:5]
out = {}
for name, rev in (('base', base), ('head', head)):
    k = Knowledge.open(db, repo, rev)
    out[name] = k.query(\"SELECT path, caller, line FROM calls WHERE callee = 'runWithBypass' ORDER BY path, line\")
    k.close()
k = Knowledge.open(db, repo, base)
out['methods'] = k.query(\"SELECT name, line, end_line FROM symbols WHERE kind = 'method' AND parent = 'TopicsController' ORDER BY line\")
out['targets'] = k.query(\"SELECT path, name, target FROM imports WHERE module LIKE '.%' ORDER BY path, name\")
out['guarded'] = k.query(\"SELECT DISTINCT c.path FROM calls c JOIN constructs k ON k.path = c.path\"
                         \" WHERE c.callee = 'runWithBypass' AND k.kind = 'ts:decorator:UseGuards'\")
k.close()
print(json.dumps(out))";
    let got = kernel_json(body, &[&db.display().to_string(), REPO, &base, &head]);
    let callers = serde_json::json!([
        ["path", "caller", "line"],
        [
            ["backend/src/audit/audit.service.ts", "record", 5],
            ["backend/src/topics/topics.controller.ts", "list", 11],
            ["backend/src/topics/topics.controller.ts", "purge", 17]
        ],
        false
    ]);
    assert_eq!(got["base"], callers);
    // Each revision sees its own files: the export added by the second commit is only in its graph.
    assert_eq!(got["head"][1].as_array().unwrap().len(), 4, "{}", got["head"]);
    assert_eq!(
        got["head"][1][1],
        serde_json::json!(["backend/src/reports/reports.service.ts", "export", 3])
    );
    assert_eq!(
        got["methods"][1],
        serde_json::json!([["constructor", 7, 7], ["list", 10, 12], ["purge", 16, 18]])
    );
    assert_eq!(
        got["targets"][1],
        serde_json::json!([
            [
                "backend/src/audit/audit.service.ts",
                "DbService",
                "backend/src/db.service.ts"
            ],
            [
                "backend/src/topics/topics.controller.ts",
                "AdminGuard",
                "backend/src/auth/admin.guard.ts"
            ],
            [
                "backend/src/topics/topics.controller.ts",
                "DbService",
                "backend/src/db.service.ts"
            ],
            ["ml/app/routes.py", "Store", "ml/app/store.py"]
        ])
    );
    assert_eq!(
        got["guarded"][1],
        serde_json::json!([["backend/src/topics/topics.controller.ts"]])
    );
}

#[test]
fn a_graph_query_reads_only_the_revisions_views_within_its_limits() {
    let (_dir, db, base, _) = indexed();
    // A review note: the graph queries never reach the notes, which reach investigators only as rules.
    let mut store = Store::open(&db).unwrap();
    store
        .record_notes(
            REPO,
            &[knowledge::Note {
                pull: 1,
                path: "ml/app/store.py".into(),
                line: Some(2),
                author: "bob".into(),
                body: "b".into(),
                url: "u".into(),
                at: "2026-10-01T00:00:00Z".into(),
                state: "addressed".into(),
                human: true,
                missed: false,
            }],
        )
        .unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    drop(store);
    let body = "\
from knowledge_api import Knowledge, QueryError
db, repo, base = sys.argv[1:4]
k = Knowledge.open(db, repo, base)
refused = {}
for name, sql in (
    ('notes', 'SELECT * FROM notes'),
    ('other revision', 'SELECT * FROM main.entries'),
    ('table under a view', 'SELECT * FROM main.symbols'),
    ('notes in a common table', 'WITH n AS (SELECT * FROM main.notes) SELECT * FROM n'),
    ('shadowed view', 'WITH symbols AS (SELECT * FROM notes) SELECT * FROM symbols'),
    ('pragma', 'PRAGMA table_info(notes)'),
    ('write', \"INSERT INTO meta (key, value) VALUES ('x', 'y')\"),
    ('create', 'CREATE TEMP TABLE t (x)'),
    ('attach', \"ATTACH DATABASE ':memory:' AS other\"),
    ('two statements', 'SELECT 1; SELECT 2'),
    ('unknown column', 'SELECT nonsense FROM symbols'),
    ('empty', '   '),
):
    try:
        k.query(sql)
        refused[name] = None
    except QueryError as error:
        refused[name] = str(error)
try:
    k.query('WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM r) SELECT count(*) FROM r', seconds=0.2)
    refused['endless'] = None
except QueryError as error:
    refused['endless'] = str(error)
columns, rows, more = k.query('WITH RECURSIVE r(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM r LIMIT 100) SELECT x FROM r')
k.close()
print(json.dumps({'refused': refused, 'rows': len(rows), 'more': more}))";
    let got = kernel_json(body, &[&db.display().to_string(), REPO, &base]);
    let refused = got["refused"].as_object().unwrap();
    for name in [
        "notes",
        "other revision",
        "table under a view",
        "notes in a common table",
        "shadowed view",
        "pragma",
        "write",
        "create",
        "attach",
    ] {
        let message = refused[name].as_str().unwrap_or_else(|| panic!("{name} was allowed"));
        assert!(message.starts_with("not authorized"), "{name}: {message}");
    }
    assert!(refused["two statements"].is_string(), "{got}");
    let unknown = refused["unknown column"].as_str().unwrap();
    assert!(
        unknown.contains("no such column: nonsense") && unknown.contains("symbols(path, name, kind"),
        "{unknown}"
    );
    assert!(refused["empty"].is_string(), "{got}");
    assert!(
        refused["endless"]
            .as_str()
            .unwrap()
            .starts_with("the query ran longer than 0.2s"),
        "{got}"
    );
    assert_eq!((got["rows"].as_i64(), got["more"].as_bool()), (Some(30), Some(true)));
}

#[test]
fn an_investigator_asks_the_graph_and_is_told_how_when_the_query_fails() {
    let (dir, db, base, _) = indexed();
    let root = dir.path().join("repo");
    let body = "\
import autoreview_deep as deep
from review_prompts import deep_task
root, db, repo, base = sys.argv[1:5]
r = deep.Repo(root, base)
r.graph = {'db': db, 'repo': repo, 'rev': base}
served = deep.serve_request(r, {'graph': {'sql': \"SELECT path, caller FROM calls WHERE callee = 'runWithBypass' ORDER BY path\"}})
try:
    deep.serve_request(r, {'graph': {'sql': 'SELECT caller FROM call'}})
    failed = None
except deep.Rejected as error:
    failed = str(error)
bare = deep.Repo(root, base)
try:
    deep.serve_request(bare, {'graph': {'sql': 'SELECT 1'}})
    unavailable = None
except deep.Rejected as error:
    unavailable = str(error)
print(json.dumps({'served': served, 'failed': failed, 'unavailable': unavailable, 'queries': r.graph_queries,
                  'offered': '\"graph\"' in deep_task('claims', graph=deep.GRAPH_SCHEMA),
                  'not offered': '\"graph\"' in deep_task('claims')}))";
    let got = kernel_json(
        body,
        &[&root.display().to_string(), &db.display().to_string(), REPO, &base],
    );
    assert_eq!(
        got["served"],
        serde_json::json!([
            "graph SELECT path, caller FROM calls WHERE callee = 'runWithBypass' ORDER BY path -> 3 rows",
            "path | caller\nbackend/src/audit/audit.service.ts | record\nbackend/src/topics/topics.controller.ts | list\nbackend/src/topics/topics.controller.ts | purge"
        ])
    );
    let failed = got["failed"].as_str().unwrap();
    assert!(
        failed.starts_with("the query failed: no such table: call. The views: files(path, lang)"),
        "{failed}"
    );
    assert_eq!(got["unavailable"], "no code graph is available for this repository");
    assert_eq!(got["queries"], 1);
    assert_eq!(
        (got["offered"].as_bool(), got["not offered"].as_bool()),
        (Some(true), Some(false))
    );
}

/// A store of schema 3 gains the graph: its symbols get their ranges, and its blobs, read by the older extractor,
/// are read again on the next index.
#[test]
fn a_store_of_schema_3_gains_the_graph_on_the_next_index() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let (base, _) = repository(&root);
    let db = dir.path().join("graph.db");
    index_tree(&db, REPO, &root, &base).unwrap();
    // As a build of schema 3 left it: symbols without ranges, no calls or imports, blobs of extractor 1.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "DROP TABLE calls; DROP TABLE imports;
         ALTER TABLE symbols DROP COLUMN end_line; ALTER TABLE symbols DROP COLUMN parent;
         UPDATE blobs SET extractor = 1; UPDATE meta SET value = '3' WHERE key = 'schema_version';",
    )
    .unwrap();
    drop(conn);
    let stats = index_tree(&db, REPO, &root, &base).unwrap();
    assert_eq!(stats.read, stats.files, "{stats:?}");
    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), SCHEMA_VERSION);
    drop(store);
    let body = "\
from knowledge_api import Knowledge
db, repo, base = sys.argv[1:4]
k = Knowledge.open(db, repo, base)
print(json.dumps(k.query(\"SELECT caller, end_line FROM calls JOIN symbols ON symbols.name = calls.caller AND symbols.path = calls.path WHERE callee = 'save'\")))";
    let got = kernel_json(body, &[&db.display().to_string(), REPO, &base]);
    assert_eq!(got[1], serde_json::json!([["modify", 8]]));
}
