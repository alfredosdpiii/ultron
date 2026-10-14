//! Replays crates/review/fixtures/compiled.json, recorded from autoreview_compiled.py (fixtures/record_compiled.py):
//! predicates, validation, coverage and catalogue shapes, result reading, the interpreter's checks over recorded
//! results, for_each expansion, candidate shapes and template programs, runner availability, planner views and
//! the values a planner cell sees. Plus the cases of ultron-autoreview-compiled.test.ts these functions cover,
//! with their literal expectations.

use std::collections::BTreeMap;

use review::compiled::{
    Brief, CoverageItem, Program, Step, coverage_items, limits_for, parse_predicate, uncovered_items, validate,
};
use review::compiled_eval::{
    Gate, Interpreter, check_quote, count_of, count_predicate, items_of, public_value, render, summary,
};
use review::compiled_hybrid::{
    PlannerSituation, RunRecord, Runnable, Source, ask_material, candidate_shape, finding_fields, planner_views,
    quote_in, quote_in_reads, runner_availability, template_steps,
};
use review::diff::parse_diff;
use review::py;
use serde_json::{Map, Value, json};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/compiled.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture json")
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().expect("object").clone()
}

fn objects(value: &Value) -> Vec<Map<String, Value>> {
    serde_json::from_value(value.clone()).expect("objects")
}

fn coverage(value: &Value) -> Option<Vec<CoverageItem>> {
    (!value.is_null()).then(|| serde_json::from_value(value.clone()).expect("coverage"))
}

fn str_of(value: &Value) -> &str {
    value.as_str().expect("string")
}

fn program(raw: &Value) -> Program {
    validate(raw, None, 80).expect("valid program")
}

fn interpreter(program: &Program, results: &Value, max_steps: usize, defer: bool) -> Interpreter {
    let mut it = Interpreter::new(max_steps, defer, Box::new(|| 0.0));
    it.add_steps(&program.steps);
    for (sid, result) in results.as_object().expect("results") {
        it.results.insert(sid.clone(), object(result));
    }
    it
}

fn when(value: &Value) -> Option<(&str, bool)> {
    value
        .as_object()
        .map(|when| (str_of(&when["step"]), when["not"].as_bool().expect("not")))
}

struct Repo(Vec<(String, String)>);

impl Source for Repo {
    fn lines(&self, path: &str) -> Option<Vec<String>> {
        let (_, text) = self.0.iter().find(|(name, _)| name == path)?;
        Some(py::splitlines(text).into_iter().map(str::to_string).collect())
    }

    fn files(&self) -> Vec<String> {
        self.0.iter().map(|(name, _)| name.clone()).collect()
    }
}

fn repo(sources: &Value) -> Repo {
    Repo(
        sources
            .as_object()
            .expect("sources")
            .iter()
            .map(|(path, text)| (path.clone(), str_of(text).to_string()))
            .collect(),
    )
}

fn errors(raw: Value) -> Vec<String> {
    validate(&raw, None, 80).err().unwrap_or_default()
}

#[test]
fn review_compiled_predicates_match_python() {
    let fx = fixture();
    for case in fx["predicates"].as_array().expect("predicates") {
        let parsed = parse_predicate(Some(&case[0]));
        assert_eq!(json!(parsed), case[1], "{}", case[0]);
    }
}

#[test]
fn review_compiled_validate_matches_python() {
    let fx = fixture();
    for case in fx["validations"].as_array().expect("validations") {
        let max = case["max_planned"].as_u64().expect("max") as usize;
        let items = coverage(&case["coverage"]);
        match validate(&case["raw"], items.as_deref(), max) {
            Ok(program) => {
                assert_eq!(case["errors"], json!([]), "{}", case["raw"]);
                let steps: Vec<Value> = program.steps.iter().cloned().map(Value::Object).collect();
                assert_eq!(Value::Array(steps), case["steps"], "{}", case["raw"]);
                assert_eq!(program.as_json(), case["json"], "{}", case["raw"]);
            }
            Err(errors) => assert_eq!(json!(errors), case["errors"], "{}", case["raw"]),
        }
    }
}

#[test]
fn review_compiled_coverage_items_match_python() {
    let fx = fixture();
    for case in fx["coverage"].as_array().expect("coverage") {
        let brief: Brief = serde_json::from_value(case["brief"].clone()).expect("brief");
        let files = case["diff"].as_str().map(parse_diff);
        let items = coverage_items(&brief, files.as_deref());
        assert_eq!(json!(items), case["items"], "{}", case["brief"]);
    }
    for case in fx["uncovered"].as_array().expect("uncovered") {
        let items = coverage(&case["coverage"]).expect("items");
        let steps = objects(&case["steps"]);
        let refs: Vec<&Step> = steps.iter().collect();
        let declared: Vec<String> = serde_json::from_value(case["declared"].clone()).expect("declared");
        assert_eq!(json!(uncovered_items(&items, &refs, &declared)), case["out"], "{case}");
    }
}

#[test]
fn review_compiled_result_reading_matches_python() {
    let fx = fixture();
    for case in fx["counts"].as_array().expect("counts") {
        let count = count_of(str_of(&case[0]), str_of(&case[1]), str_of(&case[2]));
        assert_eq!(json!(count), case[3], "{case}");
    }
    for case in fx["items"].as_array().expect("items") {
        assert_eq!(
            Value::Array(items_of(str_of(&case[0]), str_of(&case[1]))),
            case[2],
            "{case}"
        );
    }
    for case in fx["renders"].as_array().expect("renders") {
        assert_eq!(render(Some(&case[0])), str_of(&case[1]), "{case}");
    }
    for case in fx["count_predicates"].as_array().expect("count predicates") {
        let (value, detail) = count_predicate(
            case[0].as_i64().expect("count"),
            case[1].as_bool().expect("capped"),
            str_of(&case[2]),
            case[3].as_i64().expect("n"),
        );
        assert_eq!(json!([value, detail]), case[4], "{case}");
    }
    for case in fx["limits"].as_array().expect("limits") {
        let (planned, expanded) = limits_for(case[0].as_u64().expect("lines") as usize);
        assert_eq!(json!([planned, expanded]), case[1]);
    }
}

fn replay_evaluation(fx: &Value, recorded: &Value, defer: bool) {
    let program = program(&fx["eval_program"]);
    let mut it = interpreter(&program, &fx["eval_results"], 120, defer);
    for case in recorded["asserts"].as_array().expect("asserts") {
        let sid = str_of(&case["id"]);
        let step = it.steps_by_id[sid].clone();
        it.assert_step(&step);
        assert_eq!(Value::Object(it.results[sid].clone()), case["result"], "{sid}");
        assert_eq!(
            Value::Object(it.records.last().expect("record").clone()),
            case["record"],
            "{sid}"
        );
    }
    for (sid, expected) in recorded["queries"].as_object().expect("queries") {
        let got = json!({
            "truth": it.truth(sid), "roots_in_ask": it.roots_in_ask(sid), "contradicted": it.contradicted(sid),
            "test_decided": it.test_decided(sid), "test_could_not_run": it.test_could_not_run(sid),
        });
        assert_eq!(&got, expected, "{sid}");
    }
    for case in recorded["conditions"].as_array().expect("conditions") {
        assert_eq!(json!(it.condition(when(&case[0]))), case[1], "{}", case[0]);
    }
    for case in recorded["gates"].as_array().expect("gates") {
        let sid = str_of(&case["id"]);
        let before = (it.records.len(), it.not_emitted.clone());
        let step = it.steps_by_id[sid].clone();
        let gate = it.finding_gate(&step);
        let resolve = match gate {
            Gate::Resolve(why) => json!(why),
            _ => Value::Null,
        };
        assert_eq!(resolve, case["resolve"], "{sid}");
        assert_eq!(gate == Gate::Emit, case["emitted"].as_bool().expect("emitted"), "{sid}");
        let record = it.records.get(before.0).cloned().map_or(Value::Null, Value::Object);
        assert_eq!(record, case["record"], "{sid}");
        let deltas: Map<String, Value> = it
            .not_emitted
            .iter()
            .map(|(key, count)| (key.to_string(), json!(count - before.1[key])))
            .collect();
        assert_eq!(Value::Object(deltas), case["not_emitted"], "{sid}");
    }
}

#[test]
fn review_compiled_checks_over_results_match_python() {
    let fx = fixture();
    replay_evaluation(&fx, &fx["evaluation"], false);
    replay_evaluation(&fx, &fx["evaluation_deferred"], true);
}

#[test]
fn review_compiled_fill_summary_quotes_match_python() {
    let fx = fixture();
    let program = program(&fx["eval_program"]);
    let it = interpreter(&program, &fx["fill_results"], 120, false);
    for case in fx["fills"].as_array().expect("fills") {
        assert_eq!(
            it.fill(&case[0], case[1].as_object().expect("scope")),
            case[2],
            "{}",
            case[0]
        );
    }
    for case in fx["summaries"].as_array().expect("summaries") {
        assert_eq!(summary(str_of(&case[0]), &object(&case[1])), str_of(&case[2]), "{case}");
    }
    for case in fx["quotes"].as_array().expect("quotes") {
        let (answer, quote, ok, detail) = check_quote(&case[0], str_of(&case[1]));
        assert_eq!(json!([answer, quote, ok, detail]), case[2], "{case}");
    }
    for case in fx["public"].as_array().expect("public") {
        assert_eq!(Value::Object(public_value(&object(&case[0]))), case[1], "{case}");
    }
}

#[test]
fn review_compiled_for_each_expansion_matches_python() {
    let fx = fixture();
    let program = program(&fx["expand_program"]);
    for case in fx["expands"].as_array().expect("expands") {
        let max_steps = case["max_steps"].as_u64().expect("max") as usize;
        let mut it = interpreter(&program, &json!({ "g": case["source"] }), max_steps, false);
        let extra = case["known"].as_u64().expect("known") as usize - it.steps_by_id.len();
        for number in 0..extra {
            let step = object(&json!({ "id": format!("extra{number}"), "op": "read" }));
            it.steps_by_id.insert(format!("extra{number}"), step);
        }
        let step = it.steps_by_id[str_of(&case["step"])].clone();
        let instances: Vec<Value> = it.expand(&step).into_iter().map(Value::Object).collect();
        assert_eq!(Value::Array(instances), case["instances"], "{case}");
        assert_eq!(json!(it.truncated), case["truncated"]);
        assert_eq!(json!(it.expanded), case["expanded"]);
    }
}

#[test]
fn review_compiled_candidate_templates_match_python() {
    let fx = fixture();
    let source = repo(&fx["sources"]);
    let files = parse_diff(str_of(&fx["fixture_diff"]));
    let brief: Brief = serde_json::from_value(fx["candidate_brief"].clone()).expect("brief");
    let retrieval = object(&fx["retrieval"]);
    let never = |_: &[String]| false;
    for case in fx["candidates"].as_array().expect("candidates") {
        let candidate = object(&case["candidate"]);
        let name = str_of(&case["name"]);
        let (shape, details) = candidate_shape(&candidate, &brief, &retrieval);
        assert_eq!(
            json!([shape, details]),
            json!([case["shape"], case["details"]]),
            "{name}"
        );
        assert_eq!(
            finding_fields(&candidate).map(Value::Object),
            Some(case["fields"].clone()),
            "{name}"
        );
        assert_eq!(
            json!(ask_material(&candidate, Some(&files), Some(&retrieval))),
            case["material"],
            "{name}"
        );
        assert_eq!(
            json!(ask_material(&candidate, None, None)),
            case["material_bare"],
            "{name}"
        );
        for variant in case["templates"].as_array().into_iter().flatten() {
            let runnable: Option<Runnable<'_>> = if variant["runnable"].is_null() {
                None
            } else {
                Some(&never)
            };
            let tests = variant["tests_available"].as_bool().expect("tests");
            let (steps, how) = template_steps(
                "c1",
                &candidate,
                &shape,
                &details,
                &source,
                &brief,
                tests,
                Some(&files),
                Some(&retrieval),
                runnable,
            );
            let shown: Vec<Value> = steps.iter().cloned().map(Value::Object).collect();
            assert_eq!(Value::Array(shown.clone()), variant["steps"], "{name} {variant}");
            assert_eq!(how, str_of(&variant["how"]), "{name}");
            let problems = if steps.is_empty() {
                Vec::new()
            } else {
                validate(&json!({ "steps": shown }), None, 80).err().unwrap_or_default()
            };
            assert_eq!(json!(problems), variant["errors"], "{name}");
        }
    }
    let error = object(
        &fx["candidates"]
            .as_array()
            .expect("candidates")
            .iter()
            .find(|case| case["name"] == "error")
            .expect("error")["candidate"],
    );
    for case in fx["forced"].as_array().expect("forced") {
        let (steps, how) = template_steps(
            "k9",
            &error,
            str_of(&case["shape"]),
            &case["details"],
            &source,
            &brief,
            true,
            None,
            None,
            None,
        );
        let shown: Vec<Value> = steps.into_iter().map(Value::Object).collect();
        assert_eq!(Value::Array(shown), case["steps"], "{}", case["shape"]);
        assert_eq!(how, str_of(&case["how"]));
    }
    for case in fx["quote_in"].as_array().expect("quote_in") {
        let found = quote_in(
            &source,
            str_of(&case[0]),
            case[1].as_i64().expect("line"),
            str_of(&case[2]),
            case[3].as_i64().expect("radius"),
        );
        assert_eq!(json!(found), case[4], "{case}");
    }
    for case in fx["quote_reads"].as_array().expect("quote_reads") {
        assert_eq!(
            json!(quote_in_reads(&source, &objects(&case[0]), str_of(&case[1]))),
            case[2],
            "{case}"
        );
    }
}

#[test]
fn review_compiled_runners_views_match_python() {
    let fx = fixture();
    for case in fx["availability"].as_array().expect("availability") {
        let session = &case["session"];
        let runs: Option<Vec<RunRecord>> = session.get("records").map(|records| {
            records
                .as_array()
                .expect("records")
                .iter()
                .map(|record| RunRecord {
                    runner: str_of(&record["runner"]).into(),
                    cwd: record["cwd"].as_str().map(str::to_string),
                    status: str_of(&record["status"]).into(),
                })
                .collect()
        });
        let runner = session
            .get("runner")
            .filter(|runner| !runner.is_null())
            .map(|runner| (str_of(&runner["name"]), str_of(&runner["because"])));
        let (lines, unavailable) = runner_availability(runs.as_deref(), runner);
        let mut unavailable: Vec<[String; 2]> = unavailable.into_iter().map(|(a, b)| [a, b]).collect();
        unavailable.sort();
        assert_eq!(json!(lines), case["lines"], "{session}");
        assert_eq!(json!(unavailable), case["unavailable"]);
    }
    for case in fx["planner"].as_array().expect("planner") {
        let options = &case["options"];
        let runners: Vec<String> = serde_json::from_value(options["runners"].clone()).expect("runners");
        let items = coverage(options.get("coverage").unwrap_or(&Value::Null)).unwrap_or_default();
        let limits = options.get("limits").map(|limits| {
            (
                limits[0].as_u64().expect("planned") as usize,
                limits[1].as_u64().expect("expanded") as usize,
            )
        });
        let situation = PlannerSituation {
            context: str_of(&options["context"]),
            intent: str_of(&options["intent"]),
            guidance: str_of(&options["guidance"]),
            tests_block: str_of(&options["tests_block"]),
            runners: &runners,
            tests_allowed: options["tests_allowed"].as_bool().expect("allowed"),
            runs_left: options["runs_left"].as_i64().expect("left"),
            coverage: &items,
            limits,
            retrieved: options.get("retrieved").and_then(Value::as_str).unwrap_or(""),
        };
        let (views, cut) = planner_views(str_of(&case["diff"]), str_of(&case["brief"]), &situation);
        assert_eq!(json!(views), case["views"]);
        assert_eq!(json!(cut), case["cut"]);
    }
}

// --- Ported from ultron-autoreview-compiled.test.ts -----------------------------------------------------------

fn finding(sid: &str, when: Value, evidence: Value) -> Value {
    json!({ "id": sid, "op": "finding", "when": when, "file": "x", "line": 1, "level": "low", "claim": "c", "evidence": evidence })
}

#[test]
fn review_compiled_validation_errors_from_ts() {
    assert_eq!(
        errors(json!({ "steps": "no" })),
        ["the program must be an object with a steps array"]
    );
    assert_eq!(errors(json!({ "steps": [] })), ["the program has no steps"]);
    let ids = errors(json!({ "steps": [{ "id": "bad id", "op": "grep", "args": { "pattern": "x" } },
        { "id": "g", "op": "grep", "args": { "pattern": "x" } }, { "id": "g", "op": "list", "args": { "dir": "." } }] }))
    .join("\n");
    assert!(ids.contains("step 'bad id': id must match"));
    assert!(ids.contains("step 'g': duplicate id"));
    let ops = errors(json!({ "steps": [{ "id": "s", "op": "shell", "args": { "cmd": "rm -rf /" } }, { "id": "w", "op": "write", "args": {} }] }))
        .join("\n");
    assert!(ops.contains("step 's': unknown op 'shell'"));
    assert!(ops.contains("step 'w': unknown op 'write'"));
    assert_eq!(
        errors(
            json!({ "steps": [{ "id": "g", "op": "grep", "args": {} }, { "id": "m", "op": "mutation_check", "args": { "path": "x" } },
            { "id": "r", "op": "read" }] })
        ),
        [
            "step 'g': grep needs args.pattern",
            "step 'm': mutation_check needs args.line",
            "step 'm': mutation_check needs args.replacement",
            "step 'm': mutation_check needs args.tests",
            "step 'r': read takes an args object",
        ]
    );
    let refs = errors(json!({ "steps": [{ "id": "a", "op": "assert", "step": "nope", "predicate": "count == 0", "expect": true },
        { "id": "q", "op": "ask", "question": "?", "context": ["a"] }, finding("f", json!({ "step": "q" }), json!(["zz"]))] }))
    .join("\n");
    assert!(refs.contains("step 'a': names unknown step 'nope'"));
    assert!(refs.contains("step 'q': context must name lookups or test steps, not 'assert' 'a'"));
    assert!(refs.contains("step 'f': names unknown step 'zz'"));
    let cycle = errors(
        json!({ "steps": [{ "id": "g", "op": "grep", "args": { "pattern": "x" }, "needs": ["h"] },
        { "id": "h", "op": "grep", "args": { "pattern": "y" }, "needs": ["g"] }] }),
    );
    assert!(
        cycle
            .iter()
            .any(|item| item == "the dependencies form a cycle: g -> h -> g"
                || item == "the dependencies form a cycle: h -> g -> h")
    );
    let predicates = errors(
        json!({ "steps": [{ "id": "g", "op": "grep", "args": { "pattern": "x" } },
        { "id": "a1", "op": "assert", "step": "g", "predicate": "count ~ 3", "expect": true },
        { "id": "a2", "op": "assert", "step": "g", "predicate": "status == maybe", "expect": true },
        { "id": "a3", "op": "assert", "step": "g", "predicate": "answer == perhaps", "expect": true },
        { "id": "a4", "op": "assert", "all": [], "step": "g", "predicate": "count == 0", "expect": true }] }),
    );
    assert_eq!(
        predicates
            .iter()
            .filter(|item| item.contains("is not one of count"))
            .count(),
        3
    );
    assert!(
        predicates
            .join("\n")
            .contains("step 'a4': assert takes exactly one of step (with predicate), all, any")
    );
    let when = errors(json!({ "steps": [{ "id": "g", "op": "grep", "args": { "pattern": "x" } },
        { "id": "f", "op": "finding", "when": { "step": "g" }, "file": "x", "line": 1, "level": "sev", "claim": "c", "evidence": ["g"] },
        { "id": "f2", "op": "finding", "file": "x", "line": "two", "level": "low", "claim": "c", "evidence": [] }] }))
    .join("\n");
    for expected in [
        "step 'f': when must name an assert or an ask, not 'grep'",
        "step 'f': level must be one of critical, high, medium, low, nit",
        "step 'f2': a finding needs when, naming an assert or an ask",
        "step 'f2': finding needs line (a number or a placeholder)",
        "step 'f2': finding needs evidence (step ids) or citations",
    ] {
        assert!(when.contains(expected), "{expected}");
    }
    let for_each = errors(json!({ "steps": [{ "id": "r", "op": "read", "args": { "path": "x" } },
        { "id": "fe", "op": "for_each", "over": "r", "steps": [
            { "id": "inner", "op": "for_each", "over": "r", "steps": [{ "id": "z", "op": "list", "args": { "dir": "." } }] }] },
        { "id": "fe2", "op": "for_each", "over": "r", "steps": [] },
        { "id": "g", "op": "grep", "args": { "pattern": "x" } },
        { "id": "fe3", "op": "for_each", "over": "g", "steps": [{ "id": "z", "op": "list", "args": { "dir": "." } }] },
        { "id": "top", "op": "assert", "step": "z", "predicate": "count == 0", "expect": true }] }))
    .join("\n");
    for expected in [
        "step 'inner': for_each cannot be nested",
        "step 'fe2': for_each takes 1 to 8 sub-steps",
        "step 'fe': for_each iterates a grep, references, list or history result, not 'read'",
        "step 'top': cannot name 'z', a sub-step of another for_each",
    ] {
        assert!(for_each.contains(expected), "{expected}");
    }
    let mut asks: Vec<Value> = (0..41)
        .map(|i| json!({ "id": format!("q{i}"), "op": "ask", "question": "?", "context": ["r"] }))
        .collect();
    asks.push(json!({ "id": "r", "op": "read", "args": { "path": "x" } }));
    assert_eq!(errors(json!({ "steps": asks })), ["41 ask steps; at most 40"]);
    assert_eq!(
        errors(
            json!({ "steps": [{ "id": "g", "op": "grep", "args": { "pattern": "x" } },
            { "id": "a", "op": "assert", "step": "g", "predicate": "count == 0" }] })
        ),
        ["step 'a': assert needs expect: true or false, the value you believe it will have"]
    );
    let reads: Vec<Value> = (0..81)
        .map(|i| json!({ "id": format!("r{i}"), "op": "read", "args": { "path": "x" } }))
        .collect();
    assert_eq!(
        errors(json!({ "steps": reads })),
        ["the program has 81 steps; at most 80 as written"]
    );
}

#[test]
fn review_compiled_grounding_from_ts() {
    // A finding must rest on an ask, a test or an exact count: a capped grep or a read alone is refused.
    let grounded = errors(json!({ "steps": [
        { "id": "g", "op": "grep", "args": { "pattern": "x" } },
        { "id": "a", "op": "assert", "step": "g", "predicate": "count == 0", "expect": true },
        finding("f_capped", json!({ "step": "a" }), json!(["g"])),
        { "id": "r", "op": "read", "args": { "path": "x" } },
        { "id": "ar", "op": "assert", "step": "r", "predicate": "contains x", "expect": true },
        finding("f_read", json!({ "step": "ar" }), json!(["r"])),
        { "id": "ge", "op": "grep", "args": { "pattern": "x", "count_only": true } },
        { "id": "ae", "op": "assert", "step": "ge", "predicate": "count == 0", "expect": true },
        finding("f_exact", json!({ "step": "ae" }), json!(["ge"])),
        { "id": "m", "op": "mutation_check", "args": { "path": "x", "line": 1, "replacement": "y", "tests": ["t"] } },
        finding("f_test", json!({ "step": "a" }), json!(["m"])),
        { "id": "q", "op": "ask", "question": "?", "context": ["r"] },
        finding("f_ask", json!({ "step": "q" }), json!(["r"])),
    ] }));
    let refused: Vec<&str> = grounded
        .iter()
        .filter(|item| {
            item.contains(
                "a finding must rest on an ask (for code semantics), a test run, or an exact-count presence check",
            )
        })
        .map(|item| item.split(':').next().unwrap_or(""))
        .collect();
    assert_eq!(refused, ["step 'f_capped'", "step 'f_read'"]);
}

#[test]
fn review_compiled_coverage_from_ts() {
    let items: Vec<CoverageItem> = serde_json::from_value(json!([
        { "id": "S1", "kind": "symbol", "name": "show", "text": "the references of `show`" },
        { "id": "K1", "kind": "key", "name": "metrics_ingress_cidrs", "text": "the siblings and consumers of `metrics_ingress_cidrs`" },
        { "id": "C1", "kind": "claim", "name": "", "text": "the claim [title] Add kind c" },
    ]))
    .expect("items");
    let bare = json!({ "steps": [{ "id": "r", "op": "read", "args": { "path": "src/app.py" } }] });
    assert_eq!(
        validate(&bare, Some(&items), 80).expect_err("errors"),
        [
            "coverage: S1 (the references of `show`) has no check: add a step that checks it and name S1 in its \"covers\", or list it in the program's \"uncovered\" with why",
            "coverage: K1 (the siblings and consumers of `metrics_ingress_cidrs`) has no check: add a step that checks it and name K1 in its \"covers\", or list it in the program's \"uncovered\" with why",
            "coverage: C1 (the claim [title] Add kind c) has no check: add a step that checks it and name C1 in its \"covers\", or list it in the program's \"uncovered\" with why",
        ]
    );
    // A symbol or key is covered by a step that names it; a claim needs an explicit covers.
    let covered = json!({ "steps": [{ "id": "refs", "op": "references", "args": { "symbol": "show" } },
        { "id": "g", "op": "grep", "args": { "pattern": "metrics_ingress_cidrs", "count_only": true } },
        { "id": "q", "op": "ask", "question": "Does the title hold?", "context": ["g"], "covers": ["C1"] }] });
    assert!(validate(&covered, Some(&items), 80).is_ok());
    let mut unknown = covered.clone();
    unknown["uncovered"] = json!(["Z9: nothing"]);
    assert_eq!(
        validate(&unknown, Some(&items), 80).expect_err("errors"),
        ["uncovered names 'Z9', which is not a coverage item"]
    );
    // Larger limits for a diff of more than 100 changed lines.
    let limits: Vec<(usize, usize)> = [3, 100, 101, 400].into_iter().map(limits_for).collect();
    assert_eq!(limits, [(80, 120), (80, 120), (120, 200), (120, 200)]);
}

/// The TS test's repository at the head commit.
fn ts_repo() -> Repo {
    let files = [
        ("pytest.ini", "[pytest]\n"),
        (
            "src/app.py",
            "KINDS = [\"a\", \"b\", \"c\"]\n\n\ndef show(kind):\n    # Upper-cases the kind.\n    return kind.upper()\n",
        ),
        (
            "src/cli.py",
            "from app import show\n\n\ndef main(kind):\n    print(show(kind))\n",
        ),
        (
            "tests/test_app.py",
            "from app import KINDS, show\n\n\ndef test_show():\n    assert show(\"a\")\n",
        ),
        ("README.md", "# app\n"),
    ];
    Repo(
        files
            .iter()
            .map(|(path, text)| (path.to_string(), text.to_string()))
            .collect(),
    )
}

#[test]
fn review_compiled_templates_from_ts() {
    let source = ts_repo();
    let brief = Brief {
        extracted: object(&json!({ "env": ["APP_MODE"] })),
        ..Brief::default()
    };
    let retrieval = object(&json!({ "keys": ["KINDS"], "registries": { "KINDS": ["src/cli.py"] } }));
    let cands = json!({
        "tests_mut": { "file": "src/app.py", "line": 1, "category": "tests", "claim": "Nothing pins KINDS.", "why": "w", "level": "medium",
            "unpinned": { "behaviour": "KINDS lists c (src/app.py:1)", "change": "drop c from KINDS", "closest_test": { "path": "tests/test_app.py", "line": 5 },
                "mutation": { "path": "src/app.py", "line": 1, "replacement": "KINDS = [\"a\", \"b\"]" } } },
        "tests_ask": { "file": "src/app.py", "line": 6, "category": "tests", "claim": "Nothing pins the upper-casing.", "why": "w", "level": "medium",
            "unpinned": { "behaviour": "show() upper-cases (src/app.py:6)", "change": "return the kind unchanged", "closest_test": { "path": "tests/test_app.py", "line": 5 } } },
        "registry": { "file": "src/app.py", "line": 1, "category": "correctness", "claim": "KINDS gained c but the cli registry was not updated.", "why": "w", "level": "medium" },
        "env": { "file": "src/app.py", "line": 1, "category": "correctness", "claim": "APP_MODE is read here but set nowhere.", "why": "w", "level": "medium" },
        "comment": { "file": "src/app.py", "line": 5, "category": "docs", "claim": "The comment says it upper-cases.", "why": "w", "level": "low" },
        "error": { "file": "src/cli.py", "line": 5, "category": "correctness", "claim": "The except can never fire.", "why": "w", "level": "high" },
        "input": { "file": "src/app.py", "line": 6, "category": "security", "claim": "The regex guard misses unicode.", "why": "w", "level": "medium" },
        "other": { "file": "src/cli.py", "line": 5, "category": "correctness", "claim": "main() prints the raw value.", "why": "w", "level": "low",
            "citations": [{ "path": "src/app.py", "line": 4, "quote": "def show(kind):" }] },
        "regression": { "file": "tests/test_app.py", "line": 4, "category": "correctness", "claim": "x", "host_confirmed": true, "test_run": 1 },
    });
    let mut shapes = BTreeMap::new();
    let mut programs = BTreeMap::new();
    let mut how = BTreeMap::new();
    for (name, cand) in cands.as_object().expect("candidates") {
        let cand = object(cand);
        let (shape, details) = candidate_shape(&cand, &brief, &retrieval);
        if name == "registry" {
            assert_eq!(details, json!({ "key": "KINDS", "registries": ["src/cli.py"] }));
        }
        if shape != "regression" {
            let (steps, text) = template_steps(
                "c1",
                &cand,
                &shape,
                &details,
                &source,
                &brief,
                name != "tests_ask",
                None,
                None,
                None,
            );
            let shown: Vec<Value> = steps.iter().cloned().map(Value::Object).collect();
            assert!(validate(&json!({ "steps": shown }), None, 80).is_ok(), "{name}");
            let ids: Vec<(String, String)> = steps
                .iter()
                .map(|step| (str_of(&step["id"]).to_string(), str_of(&step["op"]).to_string()))
                .collect();
            programs.insert(name.clone(), ids);
            how.insert(name.clone(), text);
        }
        shapes.insert(name.clone(), shape);
    }
    let expected: BTreeMap<String, String> = [
        ("tests_mut", "unpinned-behaviour"),
        ("tests_ask", "unpinned-behaviour"),
        ("registry", "registry-member"),
        ("env", "env-in-deploy"),
        ("comment", "comment-vs-code"),
        ("error", "error-path"),
        ("input", "input-defeats-guard"),
        ("other", "consistency"),
        ("regression", "regression"),
    ]
    .into_iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    assert_eq!(shapes, expected);
    let pairs = |items: &[(&str, &str)]| -> Vec<(String, String)> {
        items.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    };
    // A mutation check gated by status == passed; without a runner, a read of the closest test and an ask.
    assert_eq!(
        programs["tests_mut"],
        pairs(&[("c1_m", "mutation_check"), ("c1_a", "assert"), ("c1_f", "finding")])
    );
    assert_eq!(
        how["tests_mut"],
        "mutation_check of src/app.py:1 against tests/test_app.py"
    );
    assert_eq!(
        programs["tests_ask"],
        pairs(&[("c1_t", "read"), ("c1_r", "read"), ("c1_q", "ask"), ("c1_f", "finding")])
    );
    // A registry member: count_only grep per registry file; a deploy variable: greps over the deploy globs, all.
    assert_eq!(
        programs["registry"],
        pairs(&[("c1_g1", "grep"), ("c1_a1", "assert"), ("c1_f", "finding")])
    );
    assert_eq!(how["registry"], "count_only grep of `KINDS` in src/cli.py");
    let env_ops: Vec<&str> = programs["env"].iter().map(|(_, op)| op.as_str()).collect();
    assert_eq!(
        env_ops,
        [
            "grep", "assert", "grep", "assert", "grep", "assert", "grep", "assert", "assert", "finding"
        ]
    );
    assert!(how["env"].contains("count_only grep of `APP_MODE` in .github/**, **/*.yml"));
    // The rest: the cited lines read, one ask with the claim; a citation in another file is read too.
    for name in ["comment", "error", "input"] {
        assert_eq!(
            programs[name],
            pairs(&[("c1_r", "read"), ("c1_q", "ask"), ("c1_f", "finding")]),
            "{name}"
        );
    }
    assert_eq!(
        programs["other"],
        pairs(&[
            ("c1_r", "read"),
            ("c1_c1", "read"),
            ("c1_q", "ask"),
            ("c1_f", "finding")
        ])
    );
    assert_eq!(how["other"], "ask over c1_r, c1_c1 with the claim, reason and scenario");
}

#[test]
fn review_compiled_three_valued_checks_from_ts() {
    // The TS interpreter test's asserts, over results as its run produced them.
    let raw = json!({ "steps": [
        { "id": "g", "op": "grep", "args": { "pattern": "show\\(" } },
        { "id": "ls", "op": "list", "args": { "dir": "src" } },
        { "id": "secret", "op": "read", "args": { "path": "secret.txt" } },
        { "id": "r1", "op": "read", "args": { "path": "tests/test_app.py" } },
        { "id": "n0", "op": "assert", "step": "g", "predicate": "count == 0", "holds": "never", "expect": false },
        { "id": "n3", "op": "assert", "step": "g", "predicate": "count >= 3", "holds": "show is called {{g.count}} times", "expect": true },
        { "id": "lt", "op": "assert", "step": "g", "predicate": "count < 2", "expect": false },
        { "id": "entries", "op": "assert", "step": "ls", "predicate": "count == 2", "holds": "src holds {{ls.count}} files: {{ls}}", "expect": true },
        { "id": "onSecret", "op": "assert", "step": "secret", "predicate": "contains hunter2", "expect": true },
        { "id": "q1", "op": "ask", "question": "Does any test assert the value show() returns?", "context": ["r1"] },
        { "id": "q2", "op": "ask", "question": "Is there a test for KINDS?", "context": ["r1"] },
        { "id": "yes1", "op": "assert", "step": "q1", "predicate": "answer == no", "expect": true },
        { "id": "notyes2", "op": "assert", "step": "q2", "predicate": "answer != yes", "expect": true },
        { "id": "neither", "op": "assert", "any": ["n0", "lt", "q2"], "expect": false },
        { "id": "gated", "op": "read", "args": { "path": "README.md" }, "when": { "step": "n0" } },
        { "id": "onGated", "op": "assert", "step": "gated", "predicate": "count >= 1", "expect": true },
        { "id": "f_unclear", "op": "finding", "when": { "step": "q2" }, "file": "src/app.py", "line": 1, "level": "low", "claim": "c", "evidence": ["q2"] },
        { "id": "f_unclear_not", "op": "finding", "when": { "step": "q2", "not": true }, "file": "src/app.py", "line": 1, "level": "low", "claim": "c", "evidence": ["q2"] },
        { "id": "f_false", "op": "finding", "when": { "step": "yes1", "not": true }, "file": "src/app.py", "line": 1, "level": "low", "claim": "c", "evidence": ["q1"] },
    ] });
    let program = program(&raw);
    let results = json!({
        "g": { "op": "grep", "status": "ok", "title": "grep show\\( -> 4 matches", "text": "src/cli.py:5: print(show(kind))", "count": 4, "capped": false, "truncated": false, "summary": "grep" },
        "ls": { "op": "list", "status": "ok", "title": "list src (2 entries)", "text": "app.py\ncli.py", "count": 2, "items": ["app.py", "cli.py"], "summary": "list src (2 entries): app.py; cli.py" },
        "secret": { "op": "read", "status": "failed", "detail": "secret.txt is not a tracked file at the reviewed commit", "text": "" },
        "r1": { "op": "read", "status": "ok", "text": "def test_show():\n    assert show(\"a\")", "count": 2 },
        "q1": { "op": "ask", "status": "ok", "answer": "no", "quote": "assert show(\"a\")" },
        "q2": { "op": "ask", "status": "ok", "answer": "unclear", "quote": "this line is not in the material" },
    });
    let mut it = interpreter(&program, &results, 120, false);
    let gated = it.steps_by_id["gated"].clone();
    for sid in ["n0", "n3", "lt", "entries", "onSecret", "yes1", "notyes2", "neither"] {
        let step = it.steps_by_id[sid].clone();
        it.assert_step(&step);
    }
    // A step gated on a false assert is skipped.
    assert_eq!(it.condition(Some(("n0", false))), Some(false));
    it.skip(&gated, "its condition n0 does not hold", 0.0);
    let step = it.steps_by_id["onGated"].clone();
    it.assert_step(&step);
    let records: BTreeMap<String, Map<String, Value>> = it
        .records
        .iter()
        .map(|record| (str_of(&record["id"]).to_string(), record.clone()))
        .collect();
    let output = |sid: &str| str_of(&records[sid]["output"]).to_string();
    assert_eq!(output("n0"), "does not hold (count is 4)");
    assert_eq!(output("n3"), "holds");
    assert_eq!(output("entries"), "holds");
    assert_eq!(output("onSecret"), "unknown (secret did not run)");
    assert_eq!(output("yes1"), "holds");
    // q2's quote was not in the material: unclear satisfies nothing, not even `answer != yes`.
    assert_eq!(output("notyes2"), "unknown (answer is unclear)");
    // any over two false asserts and an unclear ask is unknown, not false.
    assert_eq!(output("neither"), "unknown (n0=False, lt=False, q2=?)");
    assert_eq!(output("onGated"), "unknown (gated did not run)");
    assert_eq!(records["gated"]["detail"], "its condition n0 does not hold");
    assert_eq!(records["n3"]["input"], "\"count >= 3\"");
    assert_eq!(it.results["n3"]["holds"], "show is called 4 times");
    assert_eq!(
        it.results["entries"]["holds"],
        "src holds 2 files: list src (2 entries): app.py; cli.py"
    );
    for sid in ["f_unclear", "f_unclear_not", "f_false"] {
        let step = it.steps_by_id[sid].clone();
        assert_eq!(it.finding_gate(&step), Gate::Settled, "{sid}");
    }
    let outputs: Vec<String> = it
        .records
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(|record| str_of(&record["output"]).to_string())
        .collect();
    assert_eq!(outputs, ["gate undecided", "gate undecided", "gate false"]);
    assert_eq!(it.not_emitted["undecided"], 2);
    assert_eq!(it.not_emitted["gateFalse"], 1);
    // The ask's checked quote: one not in the material makes the answer unclear.
    let (answer, _, ok, detail) = check_quote(
        &json!({ "answer": "no", "quote": "this line is not in the material", "why": "x" }),
        "def test_show():\n    assert show(\"a\")",
    );
    assert_eq!((answer.as_str(), ok), ("unclear", false));
    assert_eq!(detail, "the quote is not in the material: the answer counts as unclear");
}

#[test]
fn review_compiled_expansion_from_ts() {
    // for_each: 4 hits, max_items 2: two instances of the four sub-steps, placeholders substituted, the rest noted.
    let raw = json!({ "steps": [
        { "id": "g", "op": "grep", "args": { "pattern": "show\\(", "max": 20 } },
        { "id": "fe", "op": "for_each", "over": "g", "max_items": 2, "steps": [
            { "id": "line", "op": "read", "args": { "path": "{{item.path}}", "start": "{{item.line}}", "end": "{{item.line}}" } },
            { "id": "has", "op": "assert", "step": "line", "predicate": "contains show", "expect": true, "holds": "{{item.path}}:{{item.line}} calls show" },
            { "id": "q", "op": "ask", "question": "Is show called with exactly one argument at {{item.path}} line {{item.line}}?", "context": ["line"] },
            { "id": "fq", "op": "finding", "when": { "step": "q", "not": true }, "file": "{{item.path}}", "line": "{{item.line}}", "level": "high",
              "category": "correctness", "claim": "c", "evidence": ["q", "line"] }] },
    ] });
    let program = program(&raw);
    let body = "src/app.py:4: def show(kind):\nsrc/cli.py:5:     print(show(kind))\ntests/test_app.py:1: from app import KINDS, show\ntests/test_app.py:5:     assert show(\"a\")";
    let results = json!({ "g": { "op": "grep", "status": "ok", "items": items_of("grep", body) } });
    let mut it = interpreter(&program, &results, 120, false);
    let fe = it.steps_by_id["fe"].clone();
    let instances = it.expand(&fe);
    let ids: Vec<&str> = instances.iter().map(|step| str_of(&step["id"])).collect();
    assert_eq!(
        ids,
        [
            "fe[0].line",
            "fe[0].has",
            "fe[0].q",
            "fe[0].fq",
            "fe[1].line",
            "fe[1].has",
            "fe[1].q",
            "fe[1].fq"
        ]
    );
    assert_eq!(it.truncated, ["fe: 2 items beyond max_items were not visited"]);
    assert_eq!(instances[2]["context"], json!(["fe[0].line"]));
    assert_eq!(instances[3]["when"], json!({ "step": "fe[0].q", "not": true }));
    let scope = instances[4]["scope"].as_object().expect("scope");
    let args = it.fill(&instances[4]["args"], scope);
    assert_eq!(
        dumps_args(&args),
        "{\"path\": \"src/cli.py\", \"start\": \"5\", \"end\": \"5\"}"
    );
}

fn dumps_args(value: &Value) -> String {
    review::compiled::dumps(value)
}
