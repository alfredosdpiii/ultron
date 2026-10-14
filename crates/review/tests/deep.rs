//! Replays crates/review/fixtures/deep.json, recorded from autoreview_deep.py (fixtures/record_deep.py): the
//! read-only repository over local git repositories rebuilt here from the recorded steps (git real, fixed identity
//! and clock), request serving, the map, citation checks, normalization, merging, shapes, regression findings,
//! shared views, the tests block, proofs and the assurance. The structural reference and the test session are the
//! recorder's scripted fakes, mirrored below. Plus the deep-pass cases of ultron-autoreview-engine.test.ts with their
//! literal expectations.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use review::deep::{
    DiffLines, Investigated, Rubric, Started, assurance, check_citations, cited_windows, mentions, merge,
    nearly_identical, no_tests, normalize_deep, prove_unpinned, regression_findings, shapes_by_lens, shared_views,
    start_tests_with, test_line, tests_block,
};
use review::deep_investigate::{Investigator, investigate};
use review::deep_map::{Brief, BriefInput, extract, is_behaviour};
use review::deep_repo::{
    CallAt, Reference, Repo, SymbolAt, TestSession, definition_pattern, dumps, hits_text, numbered, serve,
    serve_blocks, serve_request, summarize,
};
use review::diff::parse_diff;
use review::frames::{Clock, FrameRequest, FrameResult, FrameRunner, MapReply, Scheduler, Settings};
use review::levels::{capped_level, is_code, to_level};
use review::merge::parse_rules;
use review::scope::{Output, ProcessRunner, Runner};
use serde_json::{Map, Value, json};

fn fixture() -> &'static Value {
    static FIXTURE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    FIXTURE.get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/deep.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture json")
    })
}

/// Real git with the recorded identity and clock; every argv recorded.
struct Recording {
    git: ProcessRunner,
    calls: Mutex<Vec<Vec<String>>>,
}

impl Recording {
    fn new(git: &ProcessRunner) -> Self {
        Self {
            git: git.clone(),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().expect("calls").clone()
    }
}

impl Runner for Recording {
    fn run(&self, argv: &[String], cwd: &str, timeout_s: u64) -> Output {
        self.calls.lock().expect("calls").push(argv.to_vec());
        self.git.run(argv, cwd, timeout_s)
    }
}

/// The repositories of the fixture, rebuilt in a temp dir.
struct World {
    _temp: tempfile::TempDir,
    root: String,
    git: ProcessRunner,
}

fn git_runner(env: &Value) -> ProcessRunner {
    let env: HashMap<String, String> = serde_json::from_value(env.clone()).expect("env");
    ProcessRunner {
        env: env.into_iter().collect(),
    }
}

fn build_repo(base: &Path, steps: &Value, runner: &ProcessRunner) {
    std::fs::create_dir_all(base).expect("repo dir");
    let mut runner = runner.clone();
    for step in steps.as_array().expect("steps") {
        let path = |index: usize| base.join(step[index].as_str().expect("path"));
        let write = |bytes: Vec<u8>| {
            std::fs::create_dir_all(path(1).parent().expect("parent")).expect("dirs");
            std::fs::write(path(1), bytes).expect("write");
        };
        match step[0].as_str().expect("op") {
            "write" => match &step[2] {
                Value::String(text) => write(text.clone().into_bytes()),
                bytes => write(serde_json::from_value(bytes.clone()).expect("bytes")),
            },
            "fill" => write(
                step[2]
                    .as_str()
                    .expect("text")
                    .repeat(step[3].as_u64().expect("n") as usize)
                    .into_bytes(),
            ),
            "date" => {
                let when = step[1].as_str().expect("date").to_string();
                runner
                    .env
                    .retain(|(key, _)| key != "GIT_AUTHOR_DATE" && key != "GIT_COMMITTER_DATE");
                runner.env.push(("GIT_AUTHOR_DATE".into(), when.clone()));
                runner.env.push(("GIT_COMMITTER_DATE".into(), when));
            }
            "git" => {
                let argv: Vec<String> = serde_json::from_value(step.clone()).expect("argv");
                let out = runner.run(&argv, base.to_str().expect("utf-8 path"), 30);
                assert_eq!(out.code, 0, "{step}: {}", out.stderr);
            }
            other => panic!("unknown step {other}"),
        }
    }
}

fn world() -> World {
    let fx = fixture();
    let temp = tempfile::tempdir().expect("temp");
    let root_path = std::fs::canonicalize(temp.path()).expect("real temp");
    let git = git_runner(&fx["git_env"]);
    for (name, steps) in fx["repos"].as_object().expect("repos") {
        build_repo(&root_path.join(name), steps, &git);
    }
    let root = root_path.to_str().expect("utf-8").to_string();
    for (name, revs) in fx["revs"].as_object().expect("revs") {
        for which in ["base", "head"] {
            let out = git.run(
                &["git".into(), "rev-parse".into(), which.into()],
                &format!("{root}/{name}"),
                30,
            );
            assert_eq!(
                out.stdout.trim(),
                revs[which],
                "the rebuilt {name} {which} commit matches"
            );
        }
    }
    World { _temp: temp, root, git }
}

impl World {
    fn dir(&self, name: &str) -> String {
        format!("{}/{name}", self.root)
    }

    fn rev(&self, name: &str, which: &str) -> String {
        fixture()["revs"][name][which].as_str().expect("rev").to_string()
    }

    fn repo<'r>(&self, name: &str, runner: &'r Recording) -> Repo<'r> {
        Repo::new(&self.dir(name), &self.rev(name, "head"), runner)
    }

    /// `review_api._rev_reader` at the head commit.
    fn reader(&self, name: &str) -> impl FnMut(&str) -> Option<Vec<String>> + '_ {
        let (dir, rev) = (self.dir(name), self.rev(name, "head"));
        move |path| {
            let out = self
                .git
                .run(&["git".into(), "show".into(), format!("{rev}:{path}")], &dir, 30);
            (out.code == 0).then(|| {
                review::py::splitlines(&out.stdout)
                    .into_iter()
                    .map(str::to_string)
                    .collect()
            })
        }
    }
}

/// A file reader, as `extract` and `build_brief` take one.
type Reader<'a> = Box<dyn FnMut(&str) -> Option<Vec<String>> + 'a>;

/// Reader over recorded sources, or the head commit when the case recorded none.
fn reader_for<'a>(world: &'a World, name: &str, sources: &'a Value) -> Reader<'a> {
    if sources.is_null() {
        Box::new(world.reader(name))
    } else {
        Box::new(move |path: &str| serde_json::from_value(sources[path].clone()).ok())
    }
}

/// The recorder's scripted structural reference.
struct FakeReference {
    data: Value,
}

impl Reference for FakeReference {
    fn serve(&self, kind: &str, args: &Map<String, Value>) -> Option<(String, String)> {
        let name = if kind != "symbol" {
            args.get("symbol")
        } else if review::levels::truthy(args.get("name")) {
            args.get("name")
        } else {
            args.get("symbol")
        };
        let name = name.and_then(Value::as_str).map(review::py::strip).unwrap_or("");
        let answer = self.data["serve"].get(format!("{kind} {name}"))?;
        Some((answer[0].as_str()?.to_string(), answer[1].as_str()?.to_string()))
    }

    fn symbols_named(&self, name: &str) -> Vec<SymbolAt> {
        let leaf = name.rsplit('.').next().unwrap_or(name);
        self.data["symbols"]
            .as_array()
            .expect("symbols")
            .iter()
            .filter(|item| item["qualname"] == name || item["name"] == leaf)
            .map(|item| SymbolAt {
                key: item["key"].as_str().expect("key").into(),
                path: item["path"].as_str().expect("path").into(),
                line: item["line"].as_i64().expect("line"),
            })
            .collect()
    }

    fn symbol_view(&self, symbol: &SymbolAt) -> String {
        format!("VIEW {} at {}:{} body=True", symbol.key, symbol.path, symbol.line)
    }

    fn callers(&self, name: &str, outside_changes: bool) -> Vec<CallAt> {
        let leaf = name.rsplit('.').next().unwrap_or(name);
        self.data["calls"]
            .as_array()
            .expect("calls")
            .iter()
            .filter(|item| item["callee"] == leaf && !(outside_changes && item["changed"] == true))
            .map(|item| CallAt {
                path: item["path"].as_str().expect("path").into(),
                line: item["line"].as_i64().expect("line"),
                text: item["text"].as_str().expect("text").into(),
                enclosing: item["enclosing"].as_str().map(str::to_string),
            })
            .collect()
    }

    fn indexed(&self, path: &str) -> bool {
        self.data["indexed"]
            .as_array()
            .expect("indexed")
            .iter()
            .any(|item| item == path)
    }

    fn block(&self) -> String {
        self.data["block"].as_str().expect("block").into()
    }

    fn stats(&self) -> Map<String, Value> {
        self.data["stats"].as_object().expect("stats").clone()
    }
}

fn fake_reference() -> Arc<dyn Reference> {
    Arc::new(FakeReference {
        data: fixture()["reference"].clone(),
    })
}

fn reference_mode(mode: &Value) -> Result<Arc<dyn Reference>, String> {
    if mode == "fake" {
        Ok(fake_reference())
    } else {
        Err("RuntimeError: the index could not be built".to_string())
    }
}

/// The recorder's scripted test session.
struct FakeSession {
    limit: usize,
    tracked: HashSet<String>,
    records: Mutex<Vec<Value>>,
    regressions: Value,
    planned: bool,
}

fn session_of(spec: &Value) -> Option<FakeSession> {
    if spec.is_null() {
        return None;
    }
    Some(FakeSession {
        limit: spec["limit"].as_u64().expect("limit") as usize,
        tracked: serde_json::from_value(spec["tracked"].clone()).expect("tracked"),
        records: Mutex::new(
            spec.get("records")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        ),
        regressions: spec
            .get("regressions")
            .cloned()
            .filter(|value| !value.is_null())
            .unwrap_or(json!([])),
        planned: spec.get("planned").and_then(Value::as_bool).unwrap_or(true),
    })
}

fn status_of(joined: &str) -> &'static str {
    if joined.contains("gone") {
        "unavailable"
    } else if joined.contains("fail") {
        "failed"
    } else {
        "passed"
    }
}

impl FakeSession {
    fn paths(&self, raw: Option<&Value>) -> Result<Vec<String>, String> {
        let bad = || "paths must be a list of tracked test files".to_string();
        let items = raw
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
            .ok_or_else(bad)?;
        let paths: Vec<String> = items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or_else(bad)?;
        if let Some(missing) = paths.iter().find(|path| !self.tracked.contains(*path)) {
            return Err(format!("{missing} is not a tracked test file"));
        }
        Ok(paths)
    }

    fn record(&self, kind: &str, paths: Vec<String>, status: &str, extra: Map<String, Value>) -> Result<Value, String> {
        let mut records = self.records.lock().expect("records");
        if records.len() >= self.limit {
            return Err("no test executions left in this review".into());
        }
        let failed = status == "failed";
        let tests: Vec<Value> = paths
            .iter()
            .map(|path| {
                json!({"id": format!("{path}::test_x"), "status": if failed { "failed" } else { "passed" },
                               "detail": if failed { "assert 1 == 2" } else { "" }})
            })
            .collect();
        let output = if status == "passed" {
            "ok".to_string()
        } else {
            (0..30)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let mut record = json!({
            "n": records.len() + 1, "kind": kind, "rev": "head", "runner": "pytest",
            "command": format!("pytest -q {}", paths.join(" ")), "cwd": ".", "status": status, "tests": tests,
            "passed": if failed { 0 } else { paths.len() }, "failed": if failed { paths.len() } else { 0 },
            "output": output, "paths": paths,
        });
        record.as_object_mut().expect("record").extend(extra);
        records.push(record.clone());
        Ok(record)
    }
}

impl TestSession for FakeSession {
    fn run(&self, paths: Option<&Value>, select: Option<&Value>) -> Result<Value, String> {
        let paths = self.paths(paths)?;
        let status = status_of(&paths.join(" "));
        let mut extra = Map::new();
        extra.insert("select".into(), select.cloned().unwrap_or(Value::Null));
        self.record("run", paths, status, extra)
    }

    fn mutation(
        &self,
        path: Option<&Value>,
        line: Option<&Value>,
        replacement: Option<&Value>,
        tests: Option<&Value>,
    ) -> Result<Value, String> {
        let line_ok = matches!(line, Some(Value::Bool(_))) || line.is_some_and(|line| line.is_i64() || line.is_u64());
        let (Some(Value::String(path)), true, Some(Value::String(replacement))) = (path, line_ok, replacement) else {
            return Err("a mutation names a path, a line and a replacement".into());
        };
        let paths = self.paths(tests)?;
        let caught = replacement.contains("caught");
        let mut extra = Map::new();
        extra.insert(
            "mutation".into(),
            json!({"path": path, "line": line, "was": "old text", "replacement": replacement}),
        );
        extra.insert("caught".into(), caught.into());
        self.record("mutation", paths, if caught { "failed" } else { "passed" }, extra)
    }

    fn records(&self) -> Vec<Value> {
        self.records.lock().expect("records").clone()
    }

    fn limit(&self) -> usize {
        self.limit
    }

    fn tracked(&self, path: &str) -> bool {
        self.tracked.contains(path)
    }

    fn plan(&self, _paths: &[String]) -> bool {
        self.planned
    }

    fn compare(&self, paths: &[String]) -> Result<Value, String> {
        let runs = paths
            .iter()
            .map(|path| self.record("automatic", vec![path.clone()], status_of(path), Map::new()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(json!({"runs": runs, "regressions": self.regressions}))
    }
}

/// Replace the temp root with `<root>` everywhere, as the recorder did.
fn rooted(value: Value, root: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(root, "<root>")),
        Value::Array(items) => Value::Array(items.into_iter().map(|v| rooted(v, root)).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(k, v)| (k.replace(root, "<root>"), rooted(v, root)))
                .collect(),
        ),
        other => other,
    }
}

fn answer(result: Result<(String, String), review::deep_repo::Rejected>) -> Value {
    match result {
        Ok((title, body)) => json!({"ok": [title, body]}),
        Err(error) => json!({"rejected": error.0}),
    }
}

fn lines_json(lines: Option<Arc<Vec<String>>>) -> Value {
    lines.map_or(Value::Null, |lines| json!(*lines))
}

fn hits_json(hits: &[(String, i64, String)]) -> Value {
    json!(
        hits.iter()
            .map(|(path, line, text)| json!([path, line, text]))
            .collect::<Vec<_>>()
    )
}

#[test]
fn review_deep_repo_reads_the_head_commit_like_python() {
    let fx = fixture();
    let world = world();
    for case in fx["repo_cases"].as_array().expect("cases") {
        let name = case["repo"].as_str().expect("repo");
        let runner = Recording::new(&world.git);
        let repo = world.repo(name, &runner);
        assert_eq!(json!(repo.files()), case["files"], "{name} files");
        for item in case["paths"].as_array().expect("paths") {
            let got = match repo.path(Some(&item["raw"])) {
                Ok(path) => json!({"raw": item["raw"], "ok": path}),
                Err(error) => json!({"raw": item["raw"], "rejected": error.0}),
            };
            assert_eq!(&got, item, "{name} path");
        }
        for (path, lines) in case["lines"].as_object().expect("lines") {
            assert_eq!(&lines_json(repo.lines(path)), lines, "{name} lines of {path}");
        }
        for grep in case["greps"].as_array().expect("greps") {
            let args = &grep["args"];
            let got = repo.grep(
                args[0].as_str().expect("pattern"),
                args[1].as_bool().expect("fixed"),
                args[2].as_bool().expect("word"),
                args[3].as_str(),
                args[4].as_u64().expect("limit") as usize,
            );
            let got = match got {
                Ok(hits) => json!({"args": args, "hits": hits_json(&hits)}),
                Err(error) => json!({"args": args, "rejected": error.0}),
            };
            assert_eq!(&got, grep, "{name} grep");
        }
        let first = repo.files().first().cloned().unwrap_or_default();
        let (file, line_file, needle) = if name == "deep" {
            ("src/app.py", "src/app.py", "KINDS")
        } else {
            ("api.py", "api.py", "PAGE_LIMIT")
        };
        let base = world.rev(name, "base");
        let history = json!({
            "file": repo.history(&first, 3), "app": repo.history(file, 5), "lines": repo.line_history(line_file, 3, 6, 5),
            "pickaxe": repo.pickaxe(needle, 5), "messages": repo.messages(Some(&base), 8),
            "messages_one": repo.messages(Some(&base), 1), "messages_none": repo.messages(None, 8),
            "messages_bad": repo.messages(Some("no-such-rev"), 8),
        });
        assert_eq!(history, case["history"], "{name} history");
        assert_eq!(
            repo.git(&["status"]).expect_err("refused"),
            case["refused"].as_str().expect("refused")
        );
        assert_eq!(json!(runner.calls()), case["calls"], "{name} git argv, in order");
        assert_eq!(repo.calls() as u64, case["count"].as_u64().expect("count"));
        let bad_runner = Recording::new(&world.git);
        let bad = Repo::new(&world.dir(name), &"0".repeat(40), &bad_runner);
        assert_eq!(json!(bad.files()), case["bad_rev_files"]);
    }
}

#[test]
fn review_deep_serve_request_matches_python() {
    let fx = fixture();
    let world = world();
    for case in fx["serve"].as_array().expect("serve") {
        let name = case["repo"].as_str().expect("repo");
        let runner = Recording::new(&world.git);
        let repo = world.repo(name, &runner);
        if case["reference"] == "fake" {
            repo.set_reference(Some(fake_reference()));
        }
        let session = session_of(&case["tests"]);
        let tests = session.as_ref().map(|session| session as &dyn TestSession);
        for (request, expected) in case["requests"]
            .as_array()
            .expect("requests")
            .iter()
            .zip(case["results"].as_array().expect("results"))
        {
            let before = runner.calls().len();
            let mut got = answer(serve_request(&repo, request, tests));
            let git: Vec<String> = runner.calls()[before..].iter().map(|argv| argv[1].clone()).collect();
            got.as_object_mut().expect("object").insert("git".into(), json!(git));
            let mut expected = expected.clone();
            if let (Some(message), Some(mine)) = (expected["rejected"].as_str(), got["rejected"].as_str())
                && message.starts_with("bad regular expression: ")
            {
                assert!(mine.starts_with("bad regular expression: "), "{request}: {mine}");
                expected["rejected"] = json!(mine);
            }
            assert_eq!(
                rooted(got, &world.root),
                expected,
                "{name} ref={} tests={} {request}",
                case["reference"],
                case["tests"]
            );
        }
        let records = session.map_or(Value::Null, |session| json!(session.records()));
        assert_eq!(records, case["records"], "{name} test records");
    }
}

#[test]
fn review_deep_serve_rounds_match_python() {
    let fx = fixture();
    let world = world();
    for case in fx["rounds"].as_array().expect("rounds") {
        let name = case["repo"].as_str().expect("repo");
        let limit = case["limit"].as_u64().expect("limit") as usize;
        let runner = Recording::new(&world.git);
        let session = session_of(&case["tests"]);
        let round = serve_blocks(
            &world.repo(name, &runner),
            &case["requests"],
            limit,
            session.as_ref().map(|s| s as &dyn TestSession),
        );
        let blocks: Vec<Value> = round
            .blocks
            .iter()
            .map(|(title, block)| json!([title, block]))
            .collect();
        assert_eq!(json!(blocks), case["blocks"], "{}", case["requests"]);
        assert_eq!(
            (round.served as u64, round.rejected as u64),
            (
                case["served"].as_u64().expect("n"),
                case["rejected"].as_u64().expect("n")
            )
        );
        let session = session_of(&case["tests"]);
        let (text, served, rejected) = serve(
            &world.repo(name, &runner),
            &case["requests"],
            limit,
            session.as_ref().map(|s| s as &dyn TestSession),
        );
        assert_eq!(text, case["text"].as_str().expect("text"));
        assert_eq!(json!([served, rejected]), case["again"]);
    }
}

#[test]
fn review_deep_helpers_match_python() {
    let helpers = &fixture()["helpers"];
    let lines = |values: &[&str]| values.iter().map(|value| value.to_string()).collect::<Vec<_>>();
    let long = "b".repeat(400);
    assert_eq!(
        numbered(&lines(&["a", long.as_str(), "c"]), 1, 3),
        helpers["numbered"][0]
    );
    assert_eq!(numbered(&lines(&["a", "b"]), 2, 1), helpers["numbered"][1]);
    assert_eq!(hits_text(&[]), helpers["hits"][0]);
    let hits = [
        ("a.py".to_string(), 3, "  x  ".to_string()),
        ("b.py".to_string(), 10, "y".repeat(300)),
    ];
    assert_eq!(hits_text(&hits), helpers["hits"][1]);
    for (name, expected) in ["load", "a.b.c_d", "x$y", "with-dash", "plain"]
        .iter()
        .zip(helpers["definition"].as_array().expect("defs"))
    {
        assert_eq!(definition_pattern(name), *expected, "{name}");
    }
    for (path, expected) in helpers["is_code"].as_object().expect("is_code") {
        assert_eq!(json!(is_code(path)), *expected, "{path}");
    }
    for (line, expected) in helpers["behaviour"].as_object().expect("behaviour") {
        assert_eq!(json!(is_behaviour(line)), *expected, "{line:?}");
    }
    for pair in fixture()["dumps"].as_array().expect("dumps") {
        assert_eq!(dumps(&pair[0], None), pair[1], "{}", pair[0]);
    }
    for case in fixture()["summaries"].as_array().expect("summaries") {
        assert_eq!(summarize(&case["record"]), case["text"], "{}", case["record"]);
    }
}

#[test]
fn review_deep_extract_matches_python() {
    let world = world();
    for case in fixture()["extracts"].as_array().expect("extracts") {
        let name = case["repo"].as_str().expect("repo");
        let files = parse_diff(case["diff"].as_str().expect("diff"));
        let found = extract(&files, reader_for(&world, name, &case["sources"]));
        assert_eq!(found.to_json(), case["found"], "{}", case["diff"]);
    }
}

fn brief_json(brief: &Brief) -> Value {
    json!({
        "text": brief.text, "lenses": brief.lenses, "symbols": brief.symbols, "callers": brief.callers,
        "tests": brief.tests, "claims": brief.claims, "required": brief.required,
        "claim_list": brief.claim_list.iter().map(|(source, text)| json!([source, text])).collect::<Vec<_>>(),
        "helpers": brief.helpers, "extracted": brief.extracted.to_json(), "reference": brief.reference.is_some(),
        "reference_text": brief.reference_text, "reference_stats": brief.reference_stats,
    })
}

fn brief_of(world: &World, runner: &Recording, case: &Value) -> (Brief, bool) {
    let name = case["repo"].as_str().expect("repo");
    let repo = world.repo(name, runner);
    let files = parse_diff(case["diff"].as_str().expect("diff"));
    let base = world.rev(name, "base");
    let input = BriefInput {
        limit: case.get("limit").and_then(Value::as_u64).unwrap_or(8_000) as usize,
        title: case.get("title").and_then(Value::as_str).unwrap_or(""),
        description: case.get("description").and_then(Value::as_str).unwrap_or(""),
        base: (case["base"] == true).then_some(base.as_str()),
        reference: reference_mode(&case["reference"]),
    };
    let brief =
        review::deep_map::build_brief(&repo, &files, reader_for(world, name, &case["sources"]), input).expect("brief");
    let has_reference = repo.reference().is_some();
    (brief, has_reference)
}

#[test]
fn review_deep_build_brief_matches_python() {
    let world = world();
    for case in fixture()["briefs"].as_array().expect("briefs") {
        let runner = Recording::new(&world.git);
        let (brief, has_reference) = brief_of(&world, &runner, case);
        assert_eq!(brief_json(&brief), case["brief"], "{}", case["diff"]);
        assert_eq!(json!(has_reference), case["repo_reference"]);
    }
}

fn diff_lines() -> DiffLines {
    fixture()["diff_lines"]
        .as_object()
        .expect("diff lines")
        .iter()
        .map(|(path, lines)| (path.clone(), serde_json::from_value(lines.clone()).expect("lines")))
        .collect()
}

#[test]
fn review_deep_check_citations_matches_python() {
    let world = world();
    let runner = Recording::new(&world.git);
    let repo = world.repo("deep", &runner);
    for case in fixture()["citations"].as_array().expect("citations") {
        let checked = check_citations(&repo, Some(&case["raw"]).filter(|raw| !raw.is_null()), &diff_lines());
        let got = json!({"raw": case["raw"], "citations": checked.citations, "problem": checked.problem, "beyond": checked.beyond});
        assert_eq!(&got, case, "{}", case["raw"]);
    }
}

fn cap(level: &str, category: &str, scenario: &str) -> String {
    capped_level(level, category, scenario, None).to_string()
}

fn level_of(name: &str) -> Option<String> {
    to_level(Some(&Value::String(name.to_string())), Some("low")).map(str::to_string)
}

/// Serialized, so key order counts.
fn same_json(got: &Value, expected: &Value) -> bool {
    serde_json::to_string(got).expect("json") == serde_json::to_string(expected).expect("json")
}

#[test]
fn review_deep_normalize_deep_matches_python() {
    let world = world();
    let runner = Recording::new(&world.git);
    let repo = world.repo("deep", &runner);
    let runs: Vec<Value> = serde_json::from_value(fixture()["runs"].clone()).expect("runs");
    for case in fixture()["normalized"].as_array().expect("normalized") {
        let rubric = Rubric {
            cap: &cap,
            to_level: (case.get("to_level") == Some(&json!(true)))
                .then_some(&level_of as &dyn Fn(&str) -> Option<String>),
            enrich: (case.get("enrich") == Some(&json!(true)))
                .then_some(&parse_rules as &dyn Fn(&Value, &mut Map<String, Value>)),
        };
        let lens = case["lens"].as_str().expect("lens");
        let (finding, problem) = match normalize_deep(&repo, &case["raw"], lens, &diff_lines(), &rubric, &runs) {
            Ok(finding) => (Value::Object(finding), Value::Null),
            Err(problem) => (Value::Null, Value::String(problem)),
        };
        assert!(
            same_json(&finding, &case["finding"]),
            "{}\n{finding}\n{}",
            case["raw"],
            case["finding"]
        );
        assert_eq!(problem, case["problem"], "{}", case["raw"]);
    }
}

#[test]
fn review_deep_cited_windows_match_python() {
    let world = world();
    let runner = Recording::new(&world.git);
    let repo = world.repo("deep", &runner);
    for case in fixture()["windows"].as_array().expect("windows") {
        let finding = case["finding"].as_object().expect("finding");
        assert_eq!(cited_windows(&repo, finding), case["text"], "{}", case["finding"]);
    }
}

fn objects(value: &Value) -> Vec<Map<String, Value>> {
    serde_json::from_value(value.clone()).expect("objects")
}

#[test]
fn review_deep_merge_matches_python() {
    for case in fixture()["merges"].as_array().expect("merges") {
        let mut records = Vec::new();
        let (merged, duplicates) = merge(objects(&case["fast"]), objects(&case["deep"]), Some(&mut records));
        assert!(
            same_json(&json!(merged), &case["merged"]),
            "{}\n{}",
            json!(merged),
            case["merged"]
        );
        assert_eq!(json!(duplicates), case["duplicates"]);
        assert!(same_json(&json!(records), &case["records"]), "{}", json!(records));
        let (plain, plain_duplicates) = merge(objects(&case["fast"]), objects(&case["deep"]), None);
        assert_eq!(json!([plain == merged, plain_duplicates]), case["plain"]);
    }
    for case in fixture()["identical"].as_array().expect("identical") {
        let (a, b) = (case[0].as_str().expect("a"), case[1].as_str().expect("b"));
        assert_eq!(json!(nearly_identical(a, b)), case[2], "{a} / {b}");
    }
}

#[test]
fn review_deep_shapes_and_mentions_match_python() {
    let shapes: Vec<Value> = serde_json::from_value(fixture()["shapes"].clone()).expect("shapes");
    for case in fixture()["shape_cases"].as_array().expect("shape cases") {
        let lenses: Vec<String> = serde_json::from_value(case["lenses"].clone()).expect("lenses");
        let out = shapes_by_lens(&shapes, &lenses);
        assert!(same_json(&json!(out), &case["out"]), "{lenses:?}: {}", json!(out));
    }
    for case in fixture()["mentions"].as_array().expect("mentions") {
        assert_eq!(
            json!(mentions(case[0].as_str().expect("text"), &case[1])),
            case[2],
            "{case}"
        );
    }
}

#[test]
fn review_deep_regression_findings_match_python() {
    let world = world();
    let runner = Recording::new(&world.git);
    let repo = world.repo("deep", &runner);
    for case in fixture()["test_lines"].as_array().expect("test lines") {
        let (path, line) = test_line(&repo, case[0].as_str().expect("id"));
        assert_eq!(json!([case[0], path, line]), *case);
    }
    let found = regression_findings(&repo, &fixture()["compared"]);
    assert!(same_json(&json!(found), &fixture()["regressions"]), "{}", json!(found));
}

#[test]
fn review_deep_shared_views_match_python() {
    for case in fixture()["views"].as_array().expect("views") {
        let args: Vec<Option<String>> = serde_json::from_value(case["args"].clone()).expect("args");
        let arg = |index: usize| args[index].as_deref().unwrap_or("");
        let views = shared_views(arg(0), args[1].as_deref(), arg(2), arg(3), arg(4), arg(5), arg(6));
        assert_eq!(json!(views), case["views"]);
    }
}

fn started_json(started: &Started) -> Value {
    json!({"mechanism": started.mechanism, "note": started.note, "observed": started.observed, "shown": started.shown,
           "env": started.env, "toolchain": started.toolchain})
}

#[test]
fn review_deep_start_tests_and_block_match_python() {
    let fx = fixture();
    let world = world();
    for case in fx["started"].as_array().expect("started") {
        let brief_case = &fx["briefs"][case["brief"].as_u64().expect("brief") as usize];
        let brief_case = json!({"repo": brief_case["repo"], "diff": brief_case["diff"], "sources": brief_case["sources"],
                                "base": brief_case["base"], "reference": null});
        let runner = Recording::new(&world.git);
        let (brief, _) = brief_of(&world, &runner, &brief_case);
        let name = case["repo"].as_str().expect("repo");
        let repo = world.repo(name, &runner);
        let files = parse_diff(brief_case["diff"].as_str().expect("diff"));
        let session = session_of(&case["session"]);
        let started = match &session {
            None => no_tests(None),
            Some(session) => start_tests_with(
                session,
                "bwrap",
                "uv",
                &["python3.12".to_string()],
                &repo,
                &files,
                &brief,
            )
            .expect("started"),
        };
        let expected = &case["started"];
        assert!(
            same_json(&started_json(&started), expected) || started_json(&started) == *expected,
            "{}",
            started_json(&started)
        );
        assert!(same_json(&json!(started.observed), &expected["observed"]));
        let block = tests_block(session.as_ref().map(|s| s as &dyn TestSession), &started);
        assert_eq!(block, case["block"].as_str().expect("block"));
        let records = session.map_or(Value::Null, |session| json!(session.records()));
        assert_eq!(records, case["records"]);
    }
    let none = no_tests(Some("tests not run: no sandbox available"));
    let recorded = &fx["no_sandbox"];
    assert_eq!(started_json(&none)["note"], recorded["note"]);
    assert_eq!(json!(none.env), recorded["env"]);
}

#[test]
fn review_deep_prove_unpinned_matches_python() {
    let fx = fixture();
    let world = world();
    let runner = Recording::new(&world.git);
    let case0 =
        json!({"repo": "deep", "diff": fx["briefs"][0]["diff"], "sources": null, "base": false, "reference": null});
    let (mut brief, _) = brief_of(&world, &runner, &case0);
    for case in fx["proved"].as_array().expect("proved") {
        let mut findings = objects(&fx["prove_findings"]);
        brief.tests = serde_json::from_value(case["tests"].clone()).expect("tests");
        let session = session_of(&case["session"]);
        prove_unpinned(session.as_ref().map(|s| s as &dyn TestSession), &mut findings, &brief);
        assert!(
            same_json(&json!(findings), &case["findings"]),
            "{}\n{}",
            json!(findings),
            case["findings"]
        );
        let records = session.map_or(Value::Null, |session| json!(session.records()));
        assert_eq!(records, case["records"]);
    }
}

#[test]
fn review_deep_assurance_matches_python() {
    let fx = fixture();
    let world = world();
    for case in fx["assurances"].as_array().expect("assurances") {
        let brief_case = &fx["briefs"][case["brief"].as_u64().expect("brief") as usize];
        let mut brief_case = brief_case.clone();
        brief_case["reference"] = Value::Null;
        brief_case.as_object_mut().expect("case").remove("limit");
        let runner = Recording::new(&world.git);
        let (brief, _) = brief_of(&world, &runner, &brief_case);
        let investigators: Vec<Investigated> = case["investigators"]
            .as_array()
            .expect("investigators")
            .iter()
            .map(|item| Investigated {
                lens: item["lens"].as_str().expect("lens").into(),
                requests: item["requests"].as_u64().expect("requests") as usize,
                status: item["status"].as_str().expect("status").into(),
            })
            .collect();
        let runs: Vec<Value> = serde_json::from_value(case["runs"].clone()).expect("runs");
        let checked: Vec<String> = serde_json::from_value(case["checked"].clone()).expect("checked");
        let out = assurance(&brief, &investigators, &runs, case["mechanism"].as_str(), &checked);
        assert_eq!(json!(out), case["assurance"]);
    }
}

// --- ultron-autoreview-engine.test.ts, "autoreview_deep: investigation beyond the diff, read-only" ---------------

fn deep_case(world: &World) -> (Vec<review::diff::FileDiff>, String) {
    let base = world.rev("deep", "base");
    let head = world.rev("deep", "head");
    let out = world.git.run(
        &["git", "diff", "-U3", base.as_str(), head.as_str(), "--"].map(str::to_string),
        &world.dir("deep"),
        30,
    );
    (parse_diff(&out.stdout), base)
}

#[test]
fn review_deep_ts_map_names_uses_helpers_and_claims() {
    let world = world();
    let runner = Recording::new(&world.git);
    let repo = world.repo("deep", &runner);
    let (files, base) = deep_case(&world);
    let input = BriefInput {
        base: Some(base.as_str()),
        ..BriefInput::default()
    };
    let brief = review::deep_map::build_brief(&repo, &files, world.reader("deep"), input).expect("brief");
    let titled = review::deep_map::build_brief(
        &world.repo("deep", &runner),
        &files,
        world.reader("deep"),
        BriefInput {
            title: "Make `show` safe for missing files",
            description: "Short.\n- Callers get None instead of an exception.",
            ..BriefInput::default()
        },
    )
    .expect("titled")
    .text;
    let found = &brief.extracted;
    assert_eq!(found.changed, ["show"]);
    assert_eq!(found.constants, ["KINDS"]);
    assert_eq!(found.calls, ["load"]);
    assert_eq!(
        found.claims,
        [(
            "src/app.py".to_string(),
            Some(7),
            "Raises OSError when the file is missing.".to_string()
        )]
    );
    assert_eq!(brief.symbols, ["show"]);
    let text = &brief.text;
    assert!(text.contains("Changed or added `show`:"));
    assert!(text.contains("tests: tests/test_app.py:2: from app import KINDS, show"));
    assert!(text.contains("docs and configs: README.md:3"));
    assert!(text.contains("Constant or member `KINDS`:"));
    assert!(text.contains("tests/test_app.py:7: assert kind in KINDS"));
    assert!(text.contains("Called by the change, `load` is defined at src/helper.py:1:"));
    assert!(text.contains("    5 |         return \"ERROR: unreadable\""));
    assert!(text.contains("Structure of tests/test_app.py: 5: @pytest.mark.parametrize(\"kind\", [\"a\", \"b\"])"));
    assert!(text.contains("Beside src/app.py: helper.py, store.py"));
    assert!(text.starts_with("Claims the change makes, and where each must hold (untrusted text;"));
    assert!(text.contains("  [commit message] handle missing files"));
    assert!(text.contains("  [src/app.py:7] Raises OSError when the file is missing."));
    assert!(titled.contains(
        "  [title] Make `show` safe for missing files\n    must hold wherever `show` is used: src/app.py:6, tests/test_app.py:2"
    ));
    assert!(titled.contains("  [description] Callers get None instead of an exception."));
    assert!(text.chars().count() <= 8_000);
    assert_eq!(brief.tests, ["tests/test_app.py"]);
    assert_eq!(brief.lenses, ["claims", "siblings", "tests"]);
    let risky = parse_diff(
        "diff --git a/src/api.py b/src/api.py\n--- a/src/api.py\n+++ b/src/api.py\n@@ -1,1 +1,3 @@\n def handler(request):\n\
         +    limit = int(os.environ[\"PAGE_LIMIT\"])\n\
         +    return db.execute(\"select * from users where token = \" + request.args[\"token\"])\n",
    );
    let risk = review::deep_map::build_brief(
        &world.repo("deep", &runner),
        &risky,
        |_: &str| None,
        BriefInput::default(),
    )
    .expect("risk");
    assert_eq!(risk.lenses, ["claims", "siblings", "deployment", "tests", "inputs"]);
}

#[test]
fn review_deep_ts_requests_are_a_closed_validated_bounded_set() {
    let world = world();
    let runner = Recording::new(&world.git);
    let repo = world.repo("deep", &runner);
    let ok: Vec<(String, String)> = [
        json!({"read": {"path": "src/helper.py", "start": 4, "end": 99999}}),
        json!({"read": {"path": "./src/app.py"}}),
        json!({"grep": {"pattern": "parametrize", "path_glob": "tests/**", "max": 5}}),
        json!({"grep": {"pattern": "no such text anywhere"}}),
        json!({"list": {"dir": "src"}}),
        json!({"definition": {"symbol": "load"}}),
        json!({"references": {"symbol": "KINDS"}}),
    ]
    .iter()
    .map(|request| serve_request(&repo, request, None).expect("served"))
    .collect();
    assert_eq!(
        ok[0],
        (
            "read src/helper.py:4-5 (of 5 lines)".to_string(),
            "    4 |     except OSError:\n    5 |         return \"ERROR: unreadable\"".to_string()
        )
    );
    assert_eq!(ok[1].0, "read src/app.py:1-11 (of 11 lines)");
    assert_eq!(ok[2].0, "grep 'parametrize' in tests/** -> 1 matches");
    assert_eq!(
        ok[2].1,
        "tests/test_app.py:5: @pytest.mark.parametrize(\"kind\", [\"a\", \"b\"])"
    );
    assert_eq!(
        ok[3],
        (
            "grep 'no such text anywhere' -> 0 matches".to_string(),
            "0 matches".to_string()
        )
    );
    assert_eq!(
        ok[4],
        (
            "list src (3 entries)".to_string(),
            "app.py\nhelper.py\nstore.py".to_string()
        )
    );
    assert_eq!(ok[5].0, "definition load -> 1 matches");
    assert!(ok[5].1.contains("src/helper.py:\n    1 | def load(path):"));
    assert!(ok[6].1.contains("tests/test_app.py:7: assert kind in KINDS"));
    let bad: Vec<String> = [
        json!({"read": {"path": "../outside.txt"}}),
        json!({"read": {"path": "src/../../etc/passwd"}}),
        json!({"read": {"path": "/etc/passwd"}}),
        json!({"read": {"path": "secret.txt"}}),
        json!({"read": {"path": "src/helper.py", "start": 900}}),
        json!({"read": {"path": "-x"}}),
        json!({"grep": {"pattern": "(unclosed"}}),
        json!({"grep": {"pattern": "x".repeat(500)}}),
        json!({"grep": {"pattern": "a\nb"}}),
        json!({"grep": {"pattern": "load", "path_glob": "../**"}}),
        json!({"grep": {"pattern": "load", "path_glob": ":(top)x"}}),
        json!({"list": {"dir": "../.."}}),
        json!({"list": {"dir": "nowhere"}}),
        json!({"definition": {"symbol": "load; rm -rf /"}}),
        json!({"run": {"cmd": "ls"}}),
        json!({"read": {"path": "src/app.py"}, "list": {"dir": "."}}),
        json!("read everything"),
    ]
    .iter()
    .map(|request| serve_request(&repo, request, None).expect_err("refused").0)
    .collect();
    assert_eq!(bad.len(), 17);
    assert!(bad[0].contains("leaves the repository"));
    assert!(bad[2].contains("is not a path inside the repository"));
    assert_eq!(bad[3], "secret.txt is not a tracked file at the reviewed commit");
    assert_eq!(bad[4], "src/helper.py has 5 lines");
    assert!(bad[6].contains("bad regular expression"));
    assert!(bad[7].contains("at most 200 characters"));
    assert!(bad[9].contains("path_glob must be a relative glob"));
    assert!(bad[14].contains("unknown request 'run'"));
    assert!(bad[15].contains("exactly one of"));
    let mut requests = vec![json!({"read": {"path": "src/app.py"}}); 9];
    requests.push(json!({"read": {"path": "nope"}}));
    let (text, served, rejected) = serve(&repo, &json!(requests), 24_000, None);
    assert_eq!((served, rejected), (8, 0));
    assert!(text.contains("2 requests beyond the 8 allowed per round"));
    let (big, _, _) = serve(
        &repo,
        &json!(vec![json!({"read": {"path": "src/app.py"}}); 8]),
        300,
        None,
    );
    assert!(big.ends_with("\n... cut: the round's size limit is reached; ask for less at a time"));
    assert!(
        repo.git(&["status"])
            .expect_err("refused")
            .contains("the deep pass does not run git")
    );
}

#[test]
fn review_deep_ts_history_lookups_use_git_log_only() {
    let world = world();
    let runner = Recording::new(&world.git);
    let repo = world.repo("deep", &runner);
    let served: Vec<(String, String)> = [
        json!({"history": {"path": "src/app.py", "n": 5}}),
        json!({"blame_range": {"path": "src/app.py", "start": 3, "end": 3}}),
        json!({"pickaxe": {"string": "except OSError", "n": 5}}),
        json!({"pickaxe": {"string": "never written anywhere"}}),
    ]
    .iter()
    .map(|request| serve_request(&repo, request, None).expect("served"))
    .collect();
    assert_eq!(served[0].0, "history src/app.py -> 2 commits");
    let commit =
        regex::Regex::new(r"^[0-9a-f]{7,} \d{4}-\d\d-\d\d handle missing files\n[0-9a-f]{7,} \d{4}-\d\d-\d\d base$")
            .expect("pattern");
    assert!(commit.is_match(&served[0].1), "{}", served[0].1);
    assert_eq!(served[1].0, "blame_range src/app.py:3-3 -> 2 commits");
    assert_eq!(served[2].0, "pickaxe 'except OSError' -> 2 commits");
    assert_eq!(
        served[3],
        (
            "pickaxe 'never written anywhere' -> 0 commits".to_string(),
            "no commit added or removed it".to_string()
        )
    );
    let bad: Vec<String> = [
        json!({"history": {"path": "../x"}}),
        json!({"history": {"path": "secret.txt"}}),
        json!({"blame_range": {"path": "src/app.py", "start": 500, "end": 600}}),
        json!({"pickaxe": {"string": "x"}}),
        json!({"pickaxe": {"string": "a\nb"}}),
    ]
    .iter()
    .map(|request| serve_request(&repo, request, None).expect_err("refused").0)
    .collect();
    assert_eq!(bad[1], "secret.txt is not a tracked file at the reviewed commit");
    assert_eq!(bad[2], "src/app.py has 11 lines");
    assert!(bad[3].contains("pickaxe takes a one-line string of 3 to 120 characters"));
    let commands: HashSet<String> = runner.calls().iter().map(|argv| argv[..2].join(" ")).collect();
    assert!(
        commands
            .iter()
            .all(|command| ["git log", "git ls-tree", "git show", "git grep"].contains(&command.as_str()))
    );
    assert!(commands.contains("git log"));
}

// --- Investigators over the frame scheduler ----------------------------------------------------------------------

struct VirtualClock {
    now: Mutex<f64>,
}

impl Clock for VirtualClock {
    fn now(&self) -> f64 {
        *self.now.lock().expect("clock")
    }

    fn sleep(&self, seconds: f64) {
        *self.now.lock().expect("clock") += seconds;
    }
}

/// The recorder's scripted `rlm.map`: one reply per frame, a second of clock each, every request recorded.
struct ScriptedMap<'a> {
    script: Mutex<std::collections::VecDeque<Value>>,
    clock: &'a VirtualClock,
    requests: Mutex<Vec<Value>>,
}

impl FrameRunner for ScriptedMap<'_> {
    fn map(&self, request: &FrameRequest) -> Result<MapReply, String> {
        self.requests.lock().expect("requests").push(json!({
            "task": request.task, "item": request.item, "context": request.context, "contract": request.contract,
            "model": request.model, "thinking": request.thinking, "shared_prefix": request.shared_prefix,
        }));
        let step = self
            .script
            .lock()
            .expect("script")
            .pop_front()
            .unwrap_or_else(|| json!({"value": {"findings": [], "requests": [], "done": true}}));
        *self.clock.now.lock().expect("clock") += 1.0;
        let result = if let Some(value) = step.get("value") {
            FrameResult::Value(value.clone())
        } else if let Some(status) = step.get("incomplete") {
            FrameResult::Incomplete {
                status: status.as_str().expect("status").to_string(),
                detail: "d".to_string(),
            }
        } else {
            FrameResult::Error(step["error"].as_str().expect("error").to_string())
        };
        Ok(MapReply {
            results: vec![result],
            calls: 1,
            tokens: step.get("tokens").and_then(Value::as_u64).unwrap_or(100),
            ..MapReply::default()
        })
    }
}

#[test]
fn review_deep_investigate_matches_python() {
    let world = world();
    for case in fixture()["investigations"].as_array().expect("investigations") {
        let name = case["name"].as_str().expect("name");
        let clock = VirtualClock {
            now: Mutex::new(1000.0),
        };
        let runner = ScriptedMap {
            script: Mutex::new(case["script"].as_array().expect("script").iter().cloned().collect()),
            clock: &clock,
            requests: Mutex::new(Vec::new()),
        };
        let scheduler = Scheduler::new(
            Settings {
                concurrency: 1,
                ..Settings::default()
            },
            || 0.5,
        );
        let git = Recording::new(&world.git);
        let repo = world.repo("deep", &git);
        let session = session_of(case.get("tests").unwrap_or(&Value::Null));
        let leads = case.get("leads").and_then(Value::as_str).map(str::to_string);
        let leads_view = move || leads.clone();
        let input = Investigator {
            lens: case["lens"].as_str().expect("lens"),
            base_views: vec!["base view".to_string()],
            rounds: case["rounds"].as_u64().expect("rounds") as usize,
            model: case.get("model").and_then(Value::as_str),
            thinking: case.get("thinking").and_then(Value::as_str),
            cutoff: None,
            tests: session.as_ref().map(|s| s as &dyn TestSession),
            something_outside: case["outside"] == true,
            required: serde_json::from_value(case["required"].clone()).expect("required"),
            shapes: serde_json::from_value(case["shapes"].clone()).expect("shapes"),
            shared: case.get("shared").cloned(),
            leads_view: case
                .get("leads")
                .is_some()
                .then_some(&leads_view as &dyn Fn() -> Option<String>),
        };
        let outcome = investigate(&scheduler, &runner, &clock, &repo, input);
        let requests = runner.requests.lock().expect("requests").clone();
        assert_eq!(json!(requests), case["requests"], "{name}: the frames' requests");
        let expected = &case["outcome"];
        assert_eq!(json!(outcome.lens), expected["lens"]);
        assert!(same_json(&json!(outcome.reply), &expected["reply"]), "{name}: reply");
        assert!(
            same_json(&json!(outcome.record), &expected["record"]),
            "{name}: {}",
            json!(outcome.record)
        );
        let records = session.map_or(Value::Null, |session| json!(session.records()));
        assert_eq!(records, case["records"], "{name}: test records");
    }
}
