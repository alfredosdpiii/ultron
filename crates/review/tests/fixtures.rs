//! Replays crates/review/fixtures/review.json, recorded from the Python /review implementation
//! (fixtures/generate.mjs), and requires identical outputs.

use std::collections::HashMap;

use review::args::{Options, classify_targets, parse_args};
use review::diff::{FileDiff, added_file, file_kind, parse_diff, render_hunk, skip_reason};
use review::findings::{
    Chunk, DEDUPE_WINDOW, Finding, apply_verdicts, dedupe, evidence_quotes_source, normalize_category,
    normalize_findings, parse_json_object, similar_claims,
};
use review::prompt_text::REVIEWERS;
use review::prompts::{
    autoreview_finder_task, catalogue_text, cell_planner_task, compiled_planner_task, deep_task, finder_task, reviewer,
};
use serde_json::{Value, json};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/review.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture json")
}

fn outcome<T: serde::Serialize>(result: Result<T, review::args::ReviewError>) -> Value {
    match result {
        Ok(value) => json!({ "ok": value }),
        Err(error) => json!({ "error": error.0 }),
    }
}

#[test]
fn review_prompts_match_python() {
    let fx = fixture();
    let prompts = &fx["prompts"];
    for reviewer in REVIEWERS {
        assert_eq!(
            prompts["finder"][reviewer.key],
            finder_task(reviewer),
            "finder {}",
            reviewer.key
        );
        assert_eq!(
            prompts["autoreview_finder"][reviewer.key],
            autoreview_finder_task(reviewer),
            "{}",
            reviewer.key
        );
    }
    let deep = prompts["deep"].as_object().expect("deep");
    assert_eq!(deep.len(), 10);
    for (key, expected) in deep {
        let (lens, tests) = key.split_once('/').expect("lens/tests");
        assert_eq!(
            Some(expected.as_str().expect("text").to_string()),
            deep_task(lens, tests == "True"),
            "{key}"
        );
    }
    assert_eq!(deep_task("nope", false), None);
    for tests in [false, true] {
        let key = if tests { "True" } else { "False" };
        assert_eq!(prompts["cell_planner"][key], cell_planner_task(tests));
        assert_eq!(prompts["compiled_planner"][key], compiled_planner_task(tests));
    }
    assert_eq!(prompts["catalogue"], catalogue_text());
    assert_eq!(
        reviewer("arch").map(|found| found.title),
        Some("Architecture and maintainability")
    );
}

#[test]
fn review_parse_args_matches_python() {
    let fx = fixture();
    for case in fx["args"].as_array().expect("args") {
        let text = case["text"].as_str().expect("text");
        let env: HashMap<String, String> = serde_json::from_value(case["env"].clone()).expect("env");
        assert_eq!(outcome(parse_args(text, &env)), case["result"], "{text:?} {env:?}");
    }
}

#[test]
fn review_classify_targets_matches_python() {
    let fx = fixture();
    for case in fx["targets"].as_array().expect("targets") {
        let strings = |key: &str| -> Vec<String> { serde_json::from_value(case[key].clone()).expect("strings") };
        let (exists, refs) = (strings("exists"), strings("refs"));
        let options = Options {
            targets: strings("tokens"),
            explicit_paths: vec!["given".to_string()],
            ..Options::default()
        };
        let result = classify_targets(
            &options,
            |p| exists.iter().any(|e| e == p),
            |r| refs.iter().any(|e| e == r),
        );
        assert_eq!(outcome(result), case["result"], "{:?}", case["tokens"]);
    }
}

fn file_json(file: &FileDiff) -> Value {
    let hunks: Vec<Value> = file
        .hunks
        .iter()
        .map(|hunk| {
            json!({
                "old_start": hunk.old_start, "old_len": hunk.old_len, "new_start": hunk.new_start,
                "new_len": hunk.new_len, "header": hunk.header, "new_first": hunk.new_first(),
                "new_last": hunk.new_last(),
                "lines": hunk.lines.iter().map(|l| json!([l.kind.to_string(), l.old, l.new, l.text])).collect::<Vec<_>>(),
                "rendered": render_hunk(hunk),
            })
        })
        .collect();
    json!({
        "path": file.path, "old_path": file.old_path, "status": file.status, "binary": file.binary,
        "added": file.added(), "removed": file.removed(), "skip": skip_reason(file), "kind": file_kind(&file.path),
        "hunks": hunks,
    })
}

#[test]
fn review_parse_diff_matches_python() {
    let fx = fixture();
    for case in fx["diffs"].as_array().expect("diffs") {
        let files: Vec<Value> = parse_diff(case["text"].as_str().expect("text"))
            .iter()
            .map(file_json)
            .collect();
        assert_eq!(Value::Array(files), case["files"], "{}", case["text"]);
    }
    for case in fx["added"].as_array().expect("added") {
        let file = added_file(
            case["path"].as_str().expect("path"),
            case["text"].as_str().expect("text"),
        );
        assert_eq!(file_json(&file), case["file"]);
    }
    for (path, kind) in fx["kinds"].as_object().expect("kinds") {
        assert_eq!(file_kind(path), kind, "{path}");
    }
}

#[test]
fn review_categories_and_similar_claims_match_python() {
    let fx = fixture();
    for case in fx["categories"].as_array().expect("categories") {
        let category = case[0].as_str().expect("category");
        assert_eq!(normalize_category(category, case[1].as_str()), case[2], "{case}");
    }
    for case in fx["claims"].as_array().expect("claims") {
        let (a, b) = (case[0].as_str().expect("a"), case[1].as_str().expect("b"));
        assert_eq!(Value::Bool(similar_claims(a, b)), case[2], "{a:?} / {b:?}");
    }
}

#[test]
fn review_normalize_findings_matches_python() {
    let fx = fixture();
    let files = parse_diff(fx["diffs"][0]["text"].as_str().expect("diff"));
    let chunk = Chunk {
        id: 7,
        path: &files[0].path,
        hunks: &files[0].hunks,
    };
    for case in fx["normalized"].as_array().expect("normalized") {
        let reviewer = reviewer(case["reviewer"].as_str().expect("key")).expect("reviewer");
        let found = normalize_findings(&case["raw"], reviewer, &chunk, case["line_count"].as_i64());
        assert_eq!(
            serde_json::to_value(found).expect("json"),
            case["result"],
            "{}",
            case["raw"]
        );
    }
}

#[test]
fn review_dedupe_matches_python() {
    let fx = fixture();
    for case in fx["dedupe"].as_array().expect("dedupe") {
        let input: Vec<Finding> = serde_json::from_value(case["input"].clone()).expect("findings");
        assert_eq!(
            serde_json::to_value(dedupe(&input, DEDUPE_WINDOW)).expect("json"),
            case["result"]
        );
    }
}

#[test]
fn review_evidence_and_json_replies_match_python() {
    let fx = fixture();
    for case in fx["evidence"].as_array().expect("evidence") {
        let (evidence, source) = (case[0].as_str().expect("e"), case[1].as_str().expect("s"));
        assert_eq!(
            Value::Bool(evidence_quotes_source(evidence, source)),
            case[2],
            "{evidence:?}"
        );
    }
    for case in fx["json_objects"].as_array().expect("json") {
        assert_eq!(
            parse_json_object(&case[0]).map(Value::Object).unwrap_or(Value::Null),
            case[1],
            "{}",
            case[0]
        );
    }
}

#[test]
fn review_apply_verdicts_matches_python() {
    let fx = fixture();
    let verdicts = &fx["verdicts"];
    let cases = verdicts["cases"].as_array().expect("cases");
    let finding: Finding = serde_json::from_value(verdicts["finding"].clone()).expect("finding");
    let findings = vec![finding; cases.len()];
    let replies: Vec<_> = cases.iter().map(|case| Ok(case[0].clone())).collect();
    let sources: Vec<&str> = cases.iter().map(|case| case[1].as_str().expect("source")).collect();
    let counts: Vec<Option<i64>> = cases.iter().map(|case| case[2].as_i64()).collect();
    let (confirmed, uncertain, rejected) = apply_verdicts(&findings, &replies, &sources, &counts);
    assert_eq!(serde_json::to_value(confirmed).expect("json"), verdicts["confirmed"]);
    assert_eq!(serde_json::to_value(uncertain).expect("json"), verdicts["uncertain"]);
    assert_eq!(serde_json::to_value(rejected).expect("json"), verdicts["rejected"]);
}

#[test]
fn review_autoreview_levels_and_rubric_match_python() {
    use review::levels::{capped_level, finding_kind, generic_reason, holds, to_level};
    let fx = fixture();
    let ar = &fx["autoreview"];
    for case in ar["to_level"].as_array().expect("to_level") {
        let value = (!case[0].is_null()).then_some(&case[0]);
        assert_eq!(json!(to_level(value, Some("low"))), case[1], "{}", case[0]);
        assert_eq!(json!(to_level(value, None)), case[2], "{}", case[0]);
    }
    for case in ar["holds"].as_array().expect("holds") {
        let value = (!case[0].is_null()).then_some(&case[0]);
        assert_eq!(json!(holds(value)), case[1], "{}", case[0]);
    }
    for case in ar["caps"].as_array().expect("caps") {
        let level = capped_level(
            case[0].as_str().expect("level"),
            case[1].as_str().expect("category"),
            case[2].as_str().expect("scenario"),
            case[3].as_bool(),
        );
        assert_eq!(json!(level), case[4], "{case}");
    }
    for case in ar["kinds"].as_array().expect("kinds") {
        assert_eq!(
            json!(finding_kind(case[0].as_object().expect("finding"))),
            case[1],
            "{}",
            case[0]
        );
    }
    for case in ar["generic"].as_array().expect("generic") {
        assert_eq!(
            json!(generic_reason(case[0].as_object().expect("finding"))),
            case[1],
            "{}",
            case[0]
        );
    }
}

#[test]
fn review_autoreview_final_level_and_lessons_match_python() {
    use review::levels::{apply_lessons, final_level};
    let fx = fixture();
    let ar = &fx["autoreview"];
    for case in ar["final"].as_array().expect("final") {
        let mut finding = case["finding"].as_object().expect("finding").clone();
        assert_eq!(
            json!(final_level(&mut finding, &case["verdict"])),
            case["level"],
            "{case}"
        );
        assert_eq!(Value::Object(finding), case["after"], "{case}");
    }
    let lessons = &ar["lessons"];
    let mut findings: Vec<serde_json::Map<String, Value>> =
        serde_json::from_value(lessons["findings"].clone()).expect("findings");
    let changed = apply_lessons(&mut findings, lessons["lessons"].as_array().expect("lessons"));
    assert_eq!(json!(changed), lessons["changed"]);
    assert_eq!(json!(findings), lessons["after"]);
}

#[test]
fn review_autoreview_retry_classification_matches_python() {
    use review::levels::{DEADLINE_ERROR, is_auth_failure, is_throttled, is_transient, retry_after};
    let fx = fixture();
    let ar = &fx["autoreview"];
    assert_eq!(ar["deadline_error"], DEADLINE_ERROR);
    for case in ar["retries"].as_array().expect("retries") {
        let error = case["error"].as_str().expect("error");
        assert_eq!(json!(is_transient(error)), case["transient"], "{error}");
        assert_eq!(json!(is_auth_failure(error)), case["auth"], "{error}");
        assert_eq!(json!(is_throttled(error)), case["throttled"], "{error}");
        assert_eq!(retry_after(error), case["after"].as_f64(), "{error}");
    }
}
