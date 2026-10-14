//! Replays crates/review/fixtures/glue.json, recorded from the Python deep pass, structural reference, test session
//! and compiled interpreter working together (fixtures/record_glue.py): `build_brief` with the real reference built
//! from the head commit (git and the Python indexer for real), the reference's lookups through `serve_request`,
//! `start_tests` from the sandbox choice to the automatic runs (real `git archive` exports, a scripted sandbox
//! executor that answers from the exported tree), the planner's runner availability, `Interpreter.run` over
//! lookups, tests, asserts, conditions and `for_each` (asks replayed from the recorded frames, emitted and undecided
//! findings settled by a stub caller as the recorder's), and `stated_intent`. The repository is rebuilt here from
//! the recorded steps (fixed identity and clock, no user config).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use review::compiled::{MAX_PROGRAM_STEPS, MAX_STEPS, Step, validate};
use review::compiled_eval::{Caller, Interpreter};
use review::compiled_hybrid::{Source, session_availability};
use review::context::stated_intent;
use review::deep::{Started, TestsSpec, start_tests};
use review::deep_map::{Brief, BriefInput, build_brief};
use review::deep_repo::{Reference, Rejected, Repo, Structural, TestSession, serve_request, summarize};
use review::diff::{FileDiff, parse_diff};
use review::reference::ProcessPython;
use review::review_tests::{
    Env, GitExport, Mechanism, Record, Sandbox, SessionOptions, TestSession as Session, detect_sandbox_with,
    summarize as summarize_record,
};
use review::scope::{Git, ProcessRunner, Runner};
use serde_json::{Map, Value, json};

fn fixture() -> &'static Value {
    static FIXTURE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    FIXTURE.get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/glue.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture json")
    })
}

/// The recorded repository, rebuilt in a temp dir.
struct World {
    _temp: tempfile::TempDir,
    root: String,
    dir: String,
    git: Arc<ProcessRunner>,
    base: String,
    head: String,
}

fn world() -> World {
    let fx = fixture();
    let temp = tempfile::tempdir().expect("temp");
    let root = std::fs::canonicalize(temp.path())
        .expect("real temp")
        .to_str()
        .expect("utf-8")
        .to_string();
    let env: HashMap<String, String> = serde_json::from_value(fx["git_env"].clone()).expect("env");
    let git = ProcessRunner {
        env: env.into_iter().collect(),
    };
    let dir = format!("{root}/glue");
    std::fs::create_dir_all(&dir).expect("repo dir");
    let mut runner = git.clone();
    for step in fx["steps"].as_array().expect("steps") {
        match step[0].as_str().expect("op") {
            "write" => {
                let path = Path::new(&dir).join(step[1].as_str().expect("path"));
                std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
                std::fs::write(path, step[2].as_str().expect("text")).expect("write");
            }
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
                let out = runner.run(&argv, &dir, 30);
                assert_eq!(out.code, 0, "{step}: {}", out.stderr);
            }
            other => panic!("unknown step {other}"),
        }
    }
    let rev = |name: &str| {
        let out = git.run(&["git".into(), "rev-parse".into(), name.into()], &dir, 30);
        assert_eq!(out.code, 0, "{}", out.stderr);
        out.stdout.trim().to_string()
    };
    let (base, head) = (rev("base"), rev("head"));
    assert_eq!(base, fx["revs"]["base"], "the rebuilt base commit matches");
    assert_eq!(head, fx["revs"]["head"], "the rebuilt head commit matches");
    World {
        _temp: temp,
        root,
        dir,
        git: Arc::new(git),
        base,
        head,
    }
}

impl World {
    fn repo(&self) -> Repo<'static> {
        Repo::owned(&self.dir, &self.head, self.git.clone())
    }

    fn files(&self, web: bool) -> Vec<FileDiff> {
        let files = parse_diff(fixture()["diff"].as_str().expect("diff"));
        if web {
            files.into_iter().filter(|item| item.path.starts_with("web/")).collect()
        } else {
            files
        }
    }

    /// The map with the reference built from the head commit (and the base commit's signatures when `base`).
    fn brief(&self, repo: &Repo<'static>, files: &[FileDiff], base: bool, case: &Value) -> Brief {
        let base = base.then_some(self.base.as_str());
        let reference = Structural::build(repo, files, base, &mut ProcessPython::from_env(), &|| 0.0);
        let input = BriefInput {
            limit: case.get("limit").and_then(Value::as_u64).unwrap_or(8_000) as usize,
            title: case.get("title").and_then(Value::as_str).unwrap_or(""),
            description: case.get("description").and_then(Value::as_str).unwrap_or(""),
            base,
            reference: Ok(Arc::new(reference)),
        };
        build_brief(repo, files, |path| repo.lines(path).map(|lines| lines.to_vec()), input).expect("brief")
    }

    /// `<root>` back to the temp root, in recorded inputs.
    fn unrooted(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(text.replace("<root>", &self.root)),
            Value::Array(items) => Value::Array(items.iter().map(|item| self.unrooted(item)).collect()),
            Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), self.unrooted(v))).collect()),
            other => other.clone(),
        }
    }

    fn rooted(&self, value: Value) -> Value {
        match value {
            Value::String(text) => Value::String(text.replace(&self.root, "<root>")),
            Value::Array(items) => Value::Array(items.into_iter().map(|item| self.rooted(item)).collect()),
            Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, self.rooted(v))).collect()),
            other => other,
        }
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

fn answer(result: Result<(String, String), Rejected>) -> Value {
    match result {
        Ok((title, body)) => json!({"ok": [title, body]}),
        Err(error) => json!({"rejected": error.0}),
    }
}

#[test]
fn review_glue_build_brief_with_the_real_reference_matches_python() {
    let world = world();
    for case in fixture()["briefs"].as_array().expect("briefs") {
        let name = case["name"].as_str().expect("name");
        let repo = world.repo();
        let files = world.files(case["web"] == true);
        let brief = world.brief(&repo, &files, case["base"] == true, case);
        assert_eq!(brief_json(&brief), case["brief"], "{name}: brief");
        assert_eq!(json!(repo.reference().is_some()), case["repo_reference"], "{name}");
        for pair in case["lookups"].as_array().expect("lookups") {
            assert_eq!(
                answer(serve_request(&repo, &pair[0], None)),
                pair[1],
                "{name}: {}",
                pair[0]
            );
        }
        let reference = brief.reference.as_ref().expect("reference");
        assert_eq!(json!(reference.facts()), case["facts"], "{name}: facts");
        assert_eq!(
            json!(reference.structural_findings()),
            case["structural"],
            "{name}: structural findings"
        );
    }
}

#[test]
fn review_glue_reference_reads_through_the_deep_repo() {
    // The reference reads through a reader of the deep pass's repository: one cache, one call count.
    let world = world();
    let repo = world.repo();
    let files = world.files(false);
    let before = repo.calls();
    let reference = Structural::build(&repo, &files, None, &mut ProcessPython::from_env(), &|| 0.0);
    let built = repo.calls();
    assert!(built > before, "the build read through the deep repo");
    assert!(reference.indexed("src/app.py") && reference.indexed("src/util.py"));
    assert!(!reference.indexed("secret.txt"));
    // Files the build read are served from the shared cache: no more git calls.
    repo.lines("src/app.py").expect("app");
    assert_eq!(repo.calls(), built);
    // The base repository is its own: reading it does not count against the head's.
    let base = repo.at(&world.base);
    assert_eq!(base.lines("src/app.py").expect("base app").len(), 7);
    assert_eq!(repo.calls(), built);
    // A symbol the reference does not know has no view.
    let unknown = review::deep_repo::SymbolAt {
        key: "src/app.py::nope".into(),
        path: "src/app.py".into(),
        line: 1,
    };
    assert_eq!(reference.symbol_view(&unknown), "");
    let page = reference.symbols_named("page");
    assert_eq!(page.len(), 1);
    assert!(
        reference
            .symbol_view(&page[0])
            .starts_with("src/app.py:8-11 def page(items, size=None, start=0)")
    );
    // Without a Python interpreter the Python files go unindexed, and the stats say so (Python had no such key).
    let mut missing = ProcessPython::new("/nonexistent/python3");
    let bare = Structural::build(&repo, &files, None, &mut missing, &|| 0.0);
    assert!(!bare.indexed("src/app.py"));
    assert!(bare.indexed("web/format.ts"));
    // The two changed Python files; without their imports nothing else of Python is reached.
    assert_eq!(bare.stats()["python_unindexed"], json!(2));
    assert!(reference.stats().get("python_unindexed").is_none());
}

#[test]
fn review_glue_source_reads_the_deep_repo() {
    let world = world();
    let repo = world.repo();
    let source: &dyn Source = &repo;
    assert_eq!(source.files(), repo.files().to_vec());
    assert_eq!(source.lines("src/util.py").expect("util").len(), 2);
    assert_eq!(source.lines("secret.txt"), None);
}

/// The sandbox, scripted as the recorder's: it answers from the exported tree it runs in.
fn scripted(argv: &[String], cwd: &str, _env: &Env, _timeout: f64) -> Result<(i32, String), String> {
    let app = std::fs::read_to_string(Path::new(cwd).join("src/app.py")).map_err(|error| error.to_string())?;
    if app.contains("MUTANT") {
        return Ok((
            1,
            "FAILED tests/test_app.py::test_page - AssertionError: mutant\n1 failed\n".into(),
        ));
    }
    if argv.join(" ").contains("UNAVAILABLE") || Path::new(cwd).join("UNAVAILABLE").exists() {
        return Ok((4, "ModuleNotFoundError: No module named 'app'\n".into()));
    }
    if app.contains("PAGE_SIZE") {
        return Ok((
            1,
            "PASSED tests/test_app.py::test_page\nFAILED tests/test_app.py::test_size - AssertionError: offset\n\
             1 failed, 1 passed\n"
                .into(),
        ));
    }
    Ok((
        0,
        "PASSED tests/test_app.py::test_page\nPASSED tests/test_app.py::test_size\n2 passed\n".into(),
    ))
}

fn unavailable(_argv: &[String], _cwd: &str, _env: &Env, _timeout: f64) -> Result<(i32, String), String> {
    Ok((4, "ModuleNotFoundError: No module named 'app'\n".into()))
}

fn options(executor: &str, runs: i64) -> SessionOptions {
    SessionOptions {
        runs,
        executor: match executor {
            "unavailable" => Box::new(unavailable),
            _ => Box::new(scripted),
        },
        export: Box::new(GitExport),
        clock: Box::new(|| 0.0),
        launch_env: Some(Env::new()),
        ..SessionOptions::default()
    }
}

fn started_json(started: &Started) -> Value {
    json!({
        "mechanism": started.mechanism, "note": started.note, "observed": started.observed, "shown": started.shown,
        "env": started.env, "toolchain": started.toolchain,
    })
}

fn records_json(records: &[Record]) -> Value {
    serde_json::to_value(records).expect("records")
}

fn session_json(session: Option<&RefCell<Session>>) -> Value {
    let Some(session) = session else {
        return Value::Null;
    };
    let session = session.borrow();
    let (lines, mut unavailable) = session_availability(Some(&session));
    unavailable.sort();
    json!({
        "records": records_json(&session.records), "limit": session.limit, "env": session.env_kind,
        "runner": session.runner.as_ref().map(|runner| json!([runner.name, runner.because])),
        "availability": [lines, unavailable.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>()],
    })
}

#[test]
fn review_glue_start_tests_matches_python() {
    let world = world();
    for case in fixture()["starts"].as_array().expect("starts") {
        let name = case["name"].as_str().expect("name");
        let repo = world.repo();
        let files = world.files(case["web"] == true);
        let brief = world.brief(&repo, &files, true, &json!({}));
        let spec = &case["tests"];
        let found = spec["detect"].as_str().unwrap_or("").to_string();
        let tests = (!spec.is_null()).then(|| TestsSpec {
            base: (spec["base"] == true).then(|| world.base.clone()),
            sandbox: if spec.get("detect").is_some() {
                None
            } else if spec.get("sandbox").is_some() {
                Some(None)
            } else {
                Some(Some(Sandbox::new(Mechanism::Bwrap, None)))
            },
            image: None,
            options: options(
                spec["executor"].as_str().unwrap_or("scripted"),
                spec["runs"].as_i64().unwrap_or(6),
            ),
        });
        let detect = |image: Option<&str>| {
            let which = |tool: &str| (tool == found).then(|| format!("/usr/bin/{tool}"));
            let probe = |_: &[String], _: &str, _: &Env, _: f64| Ok((0, String::new()));
            detect_sandbox_with(image, &which, &probe, None, &Sandbox::new)
        };
        let (session, started) =
            start_tests(&repo, &files, tests, &brief, &world.dir, &world.head, &detect).expect("started");
        assert_eq!(world.rooted(started_json(&started)), case["started"], "{name}: started");
        assert_eq!(
            world.rooted(session_json(session.as_ref())),
            case["session"],
            "{name}: session"
        );
        if let Some(session) = &session {
            // The deep pass's view of the session: the same records, rendered the same way.
            let records = TestSession::records(session);
            let typed = &session.borrow().records;
            assert_eq!(records.len(), typed.len());
            for (value, record) in records.iter().zip(typed.iter()) {
                assert_eq!(summarize(value), summarize_record(record), "{name}");
            }
            let exports = session.borrow().exports();
            assert!(
                exports.iter().all(|path| Path::new(path).exists()),
                "{name}: exports live with the session"
            );
        }
    }
}

#[test]
fn review_glue_session_serves_test_requests() {
    let world = world();
    let repo = world.repo();
    let files = world.files(false);
    let brief = world.brief(&repo, &files, true, &json!({}));
    let tests = TestsSpec {
        base: Some(world.base.clone()),
        sandbox: Some(Some(Sandbox::new(Mechanism::Bwrap, None))),
        image: None,
        options: options("scripted", 3),
    };
    let detect = |_: Option<&str>| None;
    let (session, _) =
        start_tests(&repo, &files, Some(tests), &brief, &world.dir, &world.head, &detect).expect("start");
    let session = session.expect("session");
    let exports = session.borrow().exports();
    assert_eq!(exports.len(), 2, "head and base were exported for the regression");
    let tests: &dyn TestSession = &session;
    assert_eq!(tests.limit(), 3);
    assert!(tests.tracked("tests/test_app.py") && !tests.tracked("secret.txt"));
    // Two automatic runs (head and base) used two of the three executions.
    let served = serve_request(
        &repo,
        &json!({"run_tests": {"paths": ["tests/test_app.py"], "select": "test_page"}}),
        Some(tests),
    );
    let (title, body) = served.expect("served");
    assert_eq!(title, "test run 3");
    assert!(body.starts_with("run 3 (run, at the head commit, sandboxed, no network): `python3 -m pytest"));
    assert!(body.contains("-k test_page tests/test_app.py"));
    let refused = serve_request(
        &repo,
        &json!({"run_tests": {"paths": ["tests/test_app.py"]}}),
        Some(tests),
    );
    assert_eq!(
        refused,
        Err(Rejected("the limit of 3 test executions per review is reached".into()))
    );
    let refused = serve_request(
        &repo,
        &json!({"mutation_check": {"path": "nope.py", "line": 1, "replacement": "x", "tests": ["tests/test_app.py"]}}),
        Some(tests),
    );
    assert_eq!(
        refused,
        Err(Rejected(
            "nope.py is not a tracked file or directory at the reviewed commit".into()
        ))
    );
    drop(session);
    assert!(
        exports.iter().all(|path| !Path::new(path).exists()),
        "dropping the session removes its exports"
    );
}

/// The caller's part of a run, as the recorder's: asks replayed from the recorded frames' results, emitted findings
/// and resolutions stubbed.
struct Replay<'a> {
    asks: HashMap<String, &'a Value>,
}

fn object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

impl Caller for Replay<'_> {
    fn asks(&mut self, interpreter: &mut Interpreter, asks: Vec<(Step, Map<String, Value>)>) {
        let began: Vec<f64> = asks.iter().map(|_| interpreter.now()).collect();
        for ((step, _), began) in asks.iter().zip(began) {
            let id = step["id"].as_str().expect("id");
            let replay = self.asks.get(id).unwrap_or_else(|| panic!("no recorded ask {id}"));
            let result = object(replay["result"].clone());
            interpreter.record(step, result, began, 0, &replay["input"], &[]);
        }
    }

    fn emit(&mut self, interpreter: &mut Interpreter, step: &Step, _scope: &Map<String, Value>, began: f64) {
        let result =
            json!({"op": "finding", "status": "ok", "emitted": true, "text": "", "detail": "emitted by the caller"});
        interpreter.record(step, object(result), began, 0, &Value::Null, &[]);
    }

    fn resolve(&mut self, interpreter: &mut Interpreter, deferred: Vec<(Step, Map<String, Value>, &'static str)>) {
        for (step, _, why) in deferred {
            let began = interpreter.now();
            let detail = format!("deferred: {why}");
            let result = json!({"op": "finding", "status": "ok", "emitted": false, "text": "", "detail": detail});
            interpreter.record(&step, object(result), began, 0, &Value::Null, &[]);
        }
    }
}

fn session_for(world: &World, repo: &Repo<'static>, executor: &str) -> RefCell<Session> {
    let reader = repo.reader();
    let read = Box::new(move |path: &str| Some(reader.lines(path).map(|lines| lines.join("\n")).unwrap_or_default()));
    RefCell::new(Session::new(
        &world.dir,
        &world.head,
        Some(&world.base),
        repo.files(),
        read,
        Sandbox::new(Mechanism::Bwrap, None),
        options(executor, 6),
    ))
}

#[test]
fn review_glue_interpreter_runs_match_python() {
    let world = world();
    for case in fixture()["runs"].as_array().expect("runs") {
        let name = case["name"].as_str().expect("name");
        let program = validate(&case["program"], None, MAX_PROGRAM_STEPS).expect("valid program");
        let repo = world.repo();
        let files = world.files(false);
        world.brief(&repo, &files, true, &json!({}));
        let session = case["session"]
            .as_str()
            .map(|executor| session_for(&world, &repo, executor));
        let ticks = Cell::new(0.0);
        let mut interpreter = Interpreter::new(
            MAX_STEPS,
            false,
            Box::new(move || {
                ticks.set(ticks.get() + 1.0);
                ticks.get()
            }),
        );
        for pair in case["unavailable_before"].as_array().expect("unavailable") {
            let pair: (String, String) = serde_json::from_value(pair.clone()).expect("pair");
            interpreter.unavailable.insert(pair);
        }
        let asks = case["asks"]
            .as_array()
            .expect("asks")
            .iter()
            .map(|item| (item["id"].as_str().expect("id").to_string(), item))
            .collect();
        let mut caller = Replay { asks };
        interpreter.run(&program.steps, &repo, session.as_ref(), &mut caller);
        let records: Vec<Value> = interpreter.records.iter().cloned().map(Value::Object).collect();
        let expected = case["records"].as_array().expect("records");
        assert_eq!(records.len(), expected.len(), "{name}: records");
        for (got, want) in records.iter().zip(expected) {
            assert_eq!(&world.rooted(got.clone()), want, "{name}: record");
        }
        let results: Map<String, Value> = interpreter
            .results
            .iter()
            .map(|(id, result)| (id.clone(), Value::Object(result.clone())))
            .collect();
        for (id, want) in case["results"].as_object().expect("results") {
            assert_eq!(world.rooted(results[id].clone()), *want, "{name}: result of {id}");
        }
        assert_eq!(results.len(), case["results"].as_object().expect("results").len());
        assert_eq!(json!(interpreter.truncated), case["truncated"], "{name}");
        assert_eq!(json!(interpreter.expanded), case["expanded"], "{name}");
        assert_eq!(json!(interpreter.not_emitted), case["not_emitted"], "{name}");
        let mut unavailable: Vec<(String, String)> = interpreter.unavailable.iter().cloned().collect();
        unavailable.sort();
        assert_eq!(json!(unavailable), case["unavailable"], "{name}");
        let session_records = session.as_ref().map(|session| records_json(&session.borrow().records));
        assert_eq!(
            world.rooted(session_records.unwrap_or(Value::Null)),
            case["session_records"],
            "{name}"
        );
    }
}

#[test]
fn review_glue_rounds_hand_asks_and_undecided_findings_back() {
    // The stepping API the kernel drives: a round stops at its asks; the run goes on once they are recorded.
    let world = world();
    let program = validate(
        &json!({"steps": [
            {"id": "g", "op": "grep", "args": {"pattern": "clamp", "count_only": true}},
            {"id": "q", "op": "ask", "question": "bounded?", "context": ["g"]},
            {"id": "a", "op": "assert", "step": "g", "predicate": "count == 0", "expect": true},
            {"id": "f", "op": "finding", "when": {"step": "q", "not": false}, "file": "src/app.py", "line": 9,
             "level": "low", "claim": "c", "evidence": ["q"]},
            {"id": "f2", "op": "finding", "when": {"step": "a", "not": false}, "file": "src/app.py", "line": 9,
             "level": "low", "claim": "c", "evidence": ["g"]},
        ]}),
        None,
        MAX_PROGRAM_STEPS,
    )
    .expect("valid");
    let repo = world.repo();
    let mut interpreter = Interpreter::new(MAX_STEPS, false, Box::new(|| 0.0));
    let mut run = interpreter.start(&program.steps);
    let mut emitted: Vec<String> = Vec::new();
    let mut emit = |interpreter: &mut Interpreter, step: &Step, _: &Map<String, Value>, began: f64| {
        emitted.push(step["id"].as_str().unwrap_or("").to_string());
        let result = json!({"op": "finding", "status": "ok", "emitted": true, "text": ""});
        interpreter.record(step, object(result), began, 0, &Value::Null, &[]);
    };
    let ids = |steps: Vec<&Step>| -> Vec<String> {
        steps
            .iter()
            .map(|step| step["id"].as_str().unwrap_or("").to_string())
            .collect()
    };
    // Round 1: the grep.
    let first = interpreter
        .round(&mut run, &repo, None, &mut emit)
        .expect("first round");
    assert!(first.asks.is_empty() && first.deferred.is_empty());
    interpreter.end_round(&mut run);
    assert!(interpreter.results.contains_key("g"));
    // Round 2: the assert runs here, the ask goes to the kernel.
    let second = interpreter
        .round(&mut run, &repo, None, &mut emit)
        .expect("second round");
    assert_eq!(ids(second.asks.iter().map(|(step, _)| step).collect()), ["q"]);
    assert!(second.deferred.is_empty());
    assert_eq!(interpreter.results["a"]["contradicted"], true);
    let (step, _) = &second.asks[0];
    let answer = json!({"op": "ask", "status": "ok", "answer": "yes", "quote": "clamp", "text": ""});
    interpreter.record(step, object(answer), 0.0, 0, &Value::Null, &[]);
    interpreter.end_round(&mut run);
    // Round 3: the finding on the ask is emitted by the caller; the one on the contradicted check is handed back.
    let third = interpreter
        .round(&mut run, &repo, None, &mut emit)
        .expect("third round");
    assert!(third.asks.is_empty());
    let deferred: Vec<(&str, &str)> = third
        .deferred
        .iter()
        .map(|(step, _, why)| (step["id"].as_str().unwrap_or(""), *why))
        .collect();
    assert_eq!(deferred, [("f2", "against the planner's expectation")]);
    let (step, _, _) = &third.deferred[0];
    let settled = json!({"op": "finding", "status": "ok", "emitted": false, "text": ""});
    interpreter.record(step, object(settled), 0.0, 0, &Value::Null, &[]);
    assert!(!run.is_done());
    interpreter.end_round(&mut run);
    assert!(run.is_done());
    assert!(interpreter.round(&mut run, &repo, None, &mut emit).is_none());
    assert_eq!(emitted, ["f"]);
}

#[test]
fn review_glue_unanswered_steps_are_skipped_at_the_end() {
    // An ask or an emitted finding the caller leaves without a result is skipped when the round ends (else it would
    // be handed back forever); what reads it then decides nothing.
    let world = world();
    let program = validate(
        &json!({"steps": [
            {"id": "g", "op": "grep", "args": {"pattern": "clamp", "count_only": true}},
            {"id": "q", "op": "ask", "question": "bounded?", "context": ["g"]},
            {"id": "a", "op": "assert", "all": ["q"], "expect": true},
            {"id": "ac", "op": "assert", "step": "g", "predicate": "count >= 1", "expect": true},
            {"id": "f", "op": "finding", "when": {"step": "ac", "not": false}, "file": "src/app.py", "line": 9,
             "level": "low", "claim": "c", "evidence": ["g"]},
        ]}),
        None,
        MAX_PROGRAM_STEPS,
    )
    .expect("valid");
    let repo = world.repo();
    let mut interpreter = Interpreter::new(MAX_STEPS, false, Box::new(|| 0.0));
    struct Silent;
    impl Caller for Silent {
        fn asks(&mut self, _: &mut Interpreter, _: Vec<(Step, Map<String, Value>)>) {}
        fn emit(&mut self, _: &mut Interpreter, _: &Step, _: &Map<String, Value>, _: f64) {}
        fn resolve(&mut self, _: &mut Interpreter, _: Vec<(Step, Map<String, Value>, &'static str)>) {}
    }
    interpreter.run(&program.steps, &repo, None, &mut Silent);
    let skipped: Vec<(&str, &str)> = interpreter
        .records
        .iter()
        .filter(|record| record["status"] == "skipped")
        .map(|record| {
            (
                record["id"].as_str().unwrap_or(""),
                record["detail"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        skipped,
        [
            ("q", "the caller recorded no result"),
            ("f", "the caller recorded no result")
        ]
    );
    assert_eq!(interpreter.results["a"]["value"], Value::Null);
    assert_eq!(interpreter.results["a"]["detail"], "q=?");
}

#[test]
fn review_glue_stated_intent_matches_python() {
    let world = world();
    let git = Git::new(&world.dir, world.git.as_ref());
    for case in fixture()["intents"].as_array().expect("intents") {
        let repo = world.repo();
        let files = match case["diff"].as_str() {
            None => world.files(false),
            Some(text) => parse_diff(text),
        };
        let spec = world.unrooted(&case["spec"]);
        let got = stated_intent(
            &case["context"],
            &files,
            |path| repo.lines(path).map(|lines| lines.to_vec()),
            (case["git"] == true).then_some(&git),
            &spec,
        );
        assert_eq!(got, case["out"], "{}", case["spec"]);
    }
}
