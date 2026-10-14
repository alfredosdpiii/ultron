//! The deep pass's evidence and results (`autoreview_deep.py` `check_citations`, `normalize_deep`, `cited_windows`,
//! `merge` with `_record` and `_nearly_identical`, `shapes_by_lens`, `_mentions`, `_test_line`,
//! `regression_findings`, `shared_views`, `no_tests`, `tests_block`, `assurance`, `prove_unpinned`, and the part of
//! `start_tests` after the session is open). Findings are the JSON objects the kernel API passes, built and updated
//! as the Python dicts were.
//!
//! Known differences, all outside recorded behaviour: `merge` takes its lists by value and returns the updated deep
//! findings (the Python also mutated the caller's dicts); a finding whose level is not one of the five sorts after
//! all of them (the Python raised `KeyError`); [`start_tests`] opens the session ([`review_tests::TestSession`] over
//! a [`Repo`] that owns its runner, since the session reads files lazily for the review's lifetime) and hands it to
//! the caller, or drops it (removing its exports) when the automatic runs fail, as the Python's
//! `except BaseException` closed it; `start_tests_with` is the part after the session is open.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::deep_map::{Brief, TEST};
use crate::deep_repo::{Repo, TestSession, numbered, py_int, summarize};
use crate::diff::FileDiff;
use crate::findings::{float_of, normalize_category, text};
use crate::levels::{LEVELS, level_to_old, truthy};
use crate::merge::duplicate_record;
use crate::py;
use crate::review_tests::{self, Sandbox, SessionOptions};

pub type Finding = Map<String, Value>;

pub const DIFF_CHARS: usize = 40_000;
pub const LEADS_CHARS: usize = 4_000;
pub const MAX_DEEP_FINDINGS: usize = 8;
pub const MAX_CITATIONS: usize = 6;
pub const CITATION_SLACK: i64 = 2;
pub const CITED_WINDOW: i64 = 6;
pub const DEDUPE_WINDOW: i64 = 3;
pub const MERGE_WINDOW: i64 = 5;

/// Which part owes each catalogue check shape; a shape whose part does not run on a change falls to the first part
/// that does.
pub const SHAPE_LENS: [(&str, &str); 12] = [
    ("registry-member", "siblings"),
    ("unpinned-behaviour", "tests"),
    ("test-asserts-behaviour", "tests"),
    ("env-in-deploy", "deployment"),
    ("manifest-reference", "deployment"),
    ("workflow-siblings", "deployment"),
    ("input-defeats-guard", "inputs"),
    ("guard-after-effect", "siblings"),
    ("error-path", "siblings"),
    ("comment-vs-code", "claims"),
    ("sibling-implementation", "siblings"),
    ("failure-retry", "siblings"),
];

/// Lines of each changed file that a citation may sit on without counting as beyond the diff.
pub type DiffLines = HashMap<String, HashSet<i64>>;

/// A finding's citations checked against the head commit.
#[derive(Debug, Clone, PartialEq)]
pub struct Checked {
    pub citations: Vec<Value>,
    /// Why the citations fail, or `None`.
    pub problem: Option<String>,
    /// Whether any citation is outside the diff.
    pub beyond: bool,
}

fn failed(problem: String) -> Checked {
    Checked {
        citations: Vec::new(),
        problem: Some(problem),
        beyond: false,
    }
}

/// A finding's citations checked against the head commit. A quote must be on its cited line, give or take two
/// lines.
pub fn check_citations(repo: &Repo<'_>, raw: Option<&Value>, diff_lines: &DiffLines) -> Checked {
    let empty = Vec::new();
    let items = raw.and_then(Value::as_array).unwrap_or(&empty);
    let mut citations = Vec::new();
    let mut beyond = false;
    for item in items.iter().take(MAX_CITATIONS) {
        let Some(item) = item.as_object() else {
            return failed("a citation is not an object".to_string());
        };
        let located = repo
            .path(item.get("path"))
            .map_err(|error| error.0)
            .and_then(|path| py_int(item.get("line")).map(|line| (path, line)));
        let (path, line) = match located {
            Ok(found) => found,
            Err(error) => return failed(format!("a citation names no tracked file and line ({error})")),
        };
        let quote = py::squash(item.get("quote").and_then(Value::as_str).unwrap_or(""));
        let lines = repo.lines(&path).filter(|lines| !lines.is_empty());
        let Some(lines) = lines.filter(|_| quote.chars().count() >= 4) else {
            return failed(format!("the citation of {path}:{line} quotes nothing"));
        };
        let (low, high) = (
            (line - CITATION_SLACK).max(1),
            (line + CITATION_SLACK).min(lines.len() as i64),
        );
        let at = (low..=high).find(|number| {
            let flat = py::squash(&lines[(*number - 1) as usize]);
            flat.contains(&quote) || (flat.chars().count() >= 8 && quote.contains(&flat))
        });
        let Some(at) = at else {
            return failed(format!(
                "{path}:{line} does not say {}",
                py::repr(&py::clip(&quote, 80))
            ));
        };
        citations.push(json!({"path": path, "line": at, "quote": py::clip(py::strip(&lines[(at - 1) as usize]), 240)}));
        if !diff_lines.get(&path).is_some_and(|lines| lines.contains(&at)) {
            beyond = true;
        }
    }
    if citations.is_empty() {
        return failed("no evidence".to_string());
    }
    Checked {
        citations,
        problem: None,
        beyond,
    }
}

/// Reads a level from what a model wrote (`to_level`).
pub type LevelReader = dyn Fn(&str) -> Option<String>;
/// Adds the rubric's fields to a finding from its raw reply (`parse_rules`).
pub type Enrich = dyn Fn(&Value, &mut Finding);

/// The rubric hooks `normalize_deep` takes: the level cap (`capped_level`), an optional level reader (`to_level`),
/// and an optional enrichment (`parse_rules`).
pub struct Rubric<'a> {
    pub cap: &'a dyn Fn(&str, &str, &str) -> String,
    pub to_level: Option<&'a LevelReader>,
    pub enrich: Option<&'a Enrich>,
}

/// One investigator finding, validated: `Ok(finding)` or `Err(why it was dropped)`.
pub fn normalize_deep(
    repo: &Repo<'_>,
    raw: &Value,
    lens: &str,
    diff_lines: &DiffLines,
    rubric: &Rubric<'_>,
    runs: &[Value],
) -> Result<Finding, String> {
    let Some(object) = raw.as_object() else {
        return Err("not an object".to_string());
    };
    let get = |key: &str| object.get(key);
    let claim = text(get("claim"), 300);
    if claim.is_empty() {
        return Err("no claim".to_string());
    }
    let (path, line) = repo
        .path(get("file"))
        .map_err(|error| error.0)
        .and_then(|path| py_int(get("line")).map(|line| (path, line)))
        .map_err(|error| format!("no tracked file and line ({error})"))?;
    let lines = repo.lines(&path).filter(|lines| !lines.is_empty());
    if !lines.is_some_and(|lines| 1 <= line && line <= lines.len() as i64) {
        return Err(format!("{path} has no line {line}"));
    }
    let checked = check_citations(repo, get("evidence"), diff_lines);
    // A test run the host made can stand in for quoted lines; a run that could not run proves nothing.
    let run = match get("test_run") {
        Some(Value::Number(number)) if number.is_i64() || number.is_u64() => runs.iter().find(|item| {
            item.get("n") == Some(&Value::Number(number.clone())) && item.get("status") != Some(&json!("unavailable"))
        }),
        _ => None,
    };
    if let Some(problem) = &checked.problem
        && (run.is_none() || problem != "no evidence")
    {
        return Err(problem.clone());
    }
    let beyond = checked.beyond || run.is_some();
    let name = py::strip(&py::fstr(get("severity"))).to_lowercase();
    let name = if get("severity").is_none() { String::new() } else { name };
    let stated = match rubric.to_level {
        Some(to_level) => to_level(&name),
        None => LEVELS.contains(&name.as_str()).then(|| name.clone()),
    }
    .filter(|level| !level.is_empty())
    .unwrap_or_else(|| "low".to_string());
    let reviewer = if lens == "tests" { "tests" } else { "correctness" };
    let category = normalize_category(&text(get("category"), 40), Some(reviewer));
    let scenario = text(get("scenario"), 500);
    let confidence = match get("confidence") {
        None => 0.6,
        Some(value) => float_of(value).map_or(0.6, |number| if number.is_nan() { 0.0 } else { number.clamp(0.0, 1.0) }),
    };
    let level = (rubric.cap)(&stated, &category, &scenario);
    let mut finding = Finding::new();
    finding.insert("file".into(), path.into());
    finding.insert("line".into(), line.into());
    finding.insert("level".into(), level.clone().into());
    finding.insert("severity".into(), level_to_old(&level).into());
    finding.insert("finder_level".into(), stated.into());
    finding.insert("category".into(), category.into());
    finding.insert("claim".into(), claim.into());
    finding.insert("why".into(), text(get("why"), 600).into());
    finding.insert("scenario".into(), scenario.into());
    finding.insert("suggested_fix".into(), text(get("suggested_fix"), 500).into());
    finding.insert("confidence".into(), confidence.into());
    finding.insert("reviewers".into(), json!([format!("deep:{lens}")]));
    finding.insert("source".into(), format!("deep:{lens}").into());
    finding.insert("citations".into(), Value::Array(checked.citations));
    finding.insert("beyond_diff".into(), beyond.into());
    if let Some(run) = run {
        finding.insert("test_run".into(), run.get("n").cloned().unwrap_or(Value::Null));
        finding.insert("test_evidence".into(), evidence_of(run).into());
    }
    if let Some(enrich) = rubric.enrich {
        enrich(raw, &mut finding);
    }
    Ok(finding)
}

/// A run's summary without its output: `summarize(run).split("\n  output")[0]`.
fn evidence_of(run: &Value) -> String {
    let summary = summarize(run);
    summary.split("\n  output").next().unwrap_or("").to_string()
}

/// The source around each citation of a finding, as the host reads it at the head commit.
pub fn cited_windows(repo: &Repo<'_>, finding: &Finding) -> String {
    let empty = Vec::new();
    let citations = finding.get("citations").and_then(Value::as_array).unwrap_or(&empty);
    let mut parts = Vec::new();
    for citation in citations {
        let path = py::str_of(citation.get("path").unwrap_or(&Value::Null));
        let line = citation.get("line").and_then(Value::as_i64).unwrap_or(0);
        if let Some(lines) = repo.lines(&path).filter(|lines| !lines.is_empty()) {
            let (start, end) = (
                (line - CITED_WINDOW).max(1),
                (line + CITED_WINDOW).min(lines.len() as i64),
            );
            parts.push(format!("{path} (cited line {line}):\n{}", numbered(&lines, start, end)));
        }
    }
    parts.join("\n\n")
}

static WORDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z0-9_]+").expect("pattern"));

fn word_set(text: &str) -> HashSet<String> {
    let lower = text.to_lowercase();
    WORDS
        .find_iter(&lower)
        .map(|word| word.as_str())
        .filter(|word| word.len() > 2)
        .map(str::to_string)
        .collect()
}

/// Two claims in nearly the same words: at least 80% of the shorter one's words (of three letters or more).
pub fn nearly_identical(a: &str, b: &str) -> bool {
    let (left, right) = (word_set(a), word_set(b));
    left.len() >= 3
        && right.len() >= 3
        && left.intersection(&right).count() as f64 / left.len().min(right.len()) as f64 >= 0.8
}

fn field<'a>(finding: &'a Finding, key: &str) -> &'a Value {
    finding.get(key).unwrap_or(&Value::Null)
}

fn line_of(finding: &Finding) -> i64 {
    finding.get("line").and_then(Value::as_i64).unwrap_or(0)
}

/// Two findings that are one: of one category, and either nearly identical claims in one file, or one file within
/// `MERGE_WINDOW` lines and different sources.
fn same(a: &Finding, b: &Finding) -> bool {
    if field(a, "category") != field(b, "category") {
        return false;
    }
    let claim = |finding: &Finding| py::str_of(field(finding, "claim"));
    if nearly_identical(&claim(a), &claim(b)) && field(a, "file") == field(b, "file") {
        return true;
    }
    // One investigator's two findings on neighbouring lines are two findings.
    field(a, "file") == field(b, "file")
        && (line_of(a) - line_of(b)).abs() <= MERGE_WINDOW
        && a.get("source") != b.get("source")
}

fn reviewers(finding: &Finding) -> Vec<Value> {
    finding
        .get("reviewers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `twin["reviewers"] += [name for name in names if name not in twin["reviewers"]]`.
fn absorb(twin: &mut Finding, names: Vec<Value>) {
    let mut mine = reviewers(twin);
    let added: Vec<Value> = names.into_iter().filter(|name| !mine.contains(name)).collect();
    mine.extend(added);
    twin.insert("reviewers".into(), Value::Array(mine));
}

fn level_order(finding: &Finding) -> usize {
    let level = py::str_of(field(finding, "level"));
    LEVELS.iter().position(|known| *known == level).unwrap_or(LEVELS.len())
}

/// Deep findings deduplicated across investigators, then against the fast pass. Two findings are one when they
/// are of one category and either sit in one file within `MERGE_WINDOW` lines of each other and come from
/// different passes, or make nearly identical claims. A deep finding with evidence from outside the diff
/// supersedes the fast one it extends; one whose evidence is all inside the diff gives way to it. Returns (all,
/// duplicates), recording each merge in `records` when given.
pub fn merge(fast: Vec<Finding>, deep: Vec<Finding>, mut records: Option<&mut Vec<Value>>) -> (Vec<Finding>, usize) {
    let mut ordered = deep;
    ordered.sort_by_key(|item| {
        let cited = item.get("citations").and_then(Value::as_array).map_or(0, Vec::len);
        (level_order(item), std::cmp::Reverse(cited))
    });
    let mut kept: Vec<Finding> = Vec::new();
    let mut duplicates = 0;
    for finding in ordered {
        let Some(index) = kept.iter().position(|item| same(item, &finding)) else {
            kept.push(finding);
            continue;
        };
        duplicates += 1;
        if let Some(records) = records.as_deref_mut() {
            records.push(duplicate_record(&finding, &kept[index], "deep"));
        }
        absorb(&mut kept[index], reviewers(&finding));
    }
    let inside: Vec<Finding> = kept
        .iter()
        .filter(|item| !truthy(item.get("beyond_diff")))
        .cloned()
        .collect();
    for finding in inside {
        if let Some(other) = fast.iter().find(|item| same(&finding, item)) {
            if let Some(index) = kept.iter().position(|item| *item == finding) {
                kept.remove(index);
            }
            duplicates += 1;
            if let Some(records) = records.as_deref_mut() {
                records.push(duplicate_record(&finding, other, "deep-into-fast"));
            }
        }
    }
    let mut remaining = Vec::new();
    for finding in fast {
        let Some(index) = kept.iter().position(|item| same(item, &finding)) else {
            remaining.push(finding);
            continue;
        };
        duplicates += 1;
        if let Some(records) = records.as_deref_mut() {
            records.push(duplicate_record(&finding, &kept[index], "fast-into-deep"));
        }
        let twin = &mut kept[index];
        absorb(twin, reviewers(&finding));
        if !truthy(twin.get("replacement"))
            && truthy(finding.get("replacement"))
            && twin.get("line") == finding.get("line")
        {
            twin.insert("replacement".into(), field(&finding, "replacement").clone());
            if truthy(finding.get("end_line")) {
                twin.insert("end_line".into(), field(&finding, "end_line").clone());
            }
        }
    }
    remaining.extend(kept);
    (remaining, duplicates)
}

/// The catalogue checks each running part owes (`shapes` are `{"id", "name", "text"}` items).
pub fn shapes_by_lens(shapes: &[Value], lenses: &[String]) -> IndexMap<String, Vec<Value>> {
    let mut out: IndexMap<String, Vec<Value>> = lenses.iter().map(|lens| (lens.clone(), Vec::new())).collect();
    for item in shapes {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("");
        let owner = SHAPE_LENS
            .iter()
            .find(|(shape, _)| *shape == name)
            .map(|(_, lens)| *lens);
        let lens = match owner {
            Some(lens) if out.contains_key(lens) => Some(lens.to_string()),
            _ => lenses.first().cloned(),
        };
        if let Some(lens) = lens
            && let Some(owed) = out.get_mut(&lens)
        {
            owed.push(item.clone());
        }
    }
    out
}

/// Whether `text` names a check: its id (not inside a longer id or word) or its shape name.
pub fn mentions(text: &str, item: &Value) -> bool {
    let id = py::str_of(item.get("id").unwrap_or(&Value::Null));
    // `re.search(r"(?<![A-Za-z0-9])" + re.escape(id) + r"(?![0-9])", text)`, tried at every position.
    let by_id = (0..=text.len()).filter(|at| text.is_char_boundary(*at)).any(|at| {
        text[at..].starts_with(id.as_str())
            && !text[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            && !text[at + id.len()..].chars().next().is_some_and(|c| c.is_ascii_digit())
    });
    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
    by_id || text.contains(name)
}

static LEAF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\[:]").expect("pattern"));

/// The file and line of a test from its id (`path::name`, or a path).
pub fn test_line(repo: &Repo<'_>, test_id: &str) -> (Option<String>, i64) {
    let (path, name) = test_id.split_once("::").unwrap_or((test_id, ""));
    let Ok(path) = repo.path(Some(&Value::String(path.to_string()))) else {
        return (None, 1);
    };
    let leaf = if name.is_empty() {
        ""
    } else {
        let last = name.rsplit("::").next().unwrap_or(name);
        LEAF.split(last).next().unwrap_or("")
    };
    if !leaf.is_empty()
        && let Ok(pattern) = Regex::new(&format!(r"\b{}\b", regex::escape(leaf)))
    {
        for (index, line) in repo.lines(&path).unwrap_or_default().iter().enumerate() {
            if pattern.is_match(line) {
                return (Some(path), index as i64 + 1);
            }
        }
    }
    (Some(path), 1)
}

/// A test that fails at the reviewed commit and passed at the base commit is a finding by itself: the host ran
/// both, so it is confirmed without a verifier.
pub fn regression_findings(repo: &Repo<'_>, compared: &Value) -> Vec<Finding> {
    let empty = Vec::new();
    let regressions = compared.get("regressions").and_then(Value::as_array).unwrap_or(&empty);
    let mut out = Vec::new();
    for item in regressions.iter().take(5) {
        let head = field_of(item, "head");
        let base = field_of(item, "base");
        let id = py::fstr(item.get("id"));
        let (Some(path), line) = test_line(repo, &id) else {
            continue;
        };
        let detail_value = item
            .get("detail")
            .filter(|value| truthy(Some(value)))
            .cloned()
            .unwrap_or(json!(""));
        let detail = text(Some(&detail_value), 240);
        let failure = if detail.is_empty() {
            String::new()
        } else {
            format!(" Failure: {detail}")
        };
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        };
        let (head_n, base_n) = (py::fstr(head.get("n")), py::fstr(base.get("n")));
        let finding = json!({
            "file": path, "line": line, "level": "high", "severity": "major", "finder_level": "high",
            "category": "correctness",
            "claim": format!("{id} fails at this commit and passed at the base commit."),
            "why": format!("The host ran the test at both commits in a sandbox.{failure}"),
            "scenario": format!("Running {id} at the reviewed commit fails; at the base commit it passes."),
            "suggested_fix": "", "confidence": 0.95, "reviewers": ["deep:test-run"], "source": "deep:test-run",
            "citations": [], "beyond_diff": true, "host_confirmed": true,
            "test_run": head.get("n").cloned().unwrap_or(Value::Null),
            "evidence": format!(
                "`{}` failed at the reviewed commit (run {head_n}) and passed at the base commit (run {base_n}){suffix}",
                py::fstr(head.get("command"))
            ),
            "how_verified": format!(
                "the host ran it in a sandbox at the reviewed commit (failed, run {head_n}) and at the base commit (passed, run {base_n})"
            ),
        });
        if let Value::Object(finding) = finding {
            out.push(finding);
        }
    }
    out
}

fn field_of<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or(&Value::Null)
}

/// What every frame of a review starts with, in one order: the diff, the brief, the retrieved block, the
/// reference, the pull request context, the author's intent, the guides. Byte-identical across the frames, so a
/// provider's prompt cache can serve it.
pub fn shared_views(
    diff_text: &str,
    brief_text: Option<&str>,
    retrieved: &str,
    context: &str,
    intent: &str,
    guidance: &str,
    reference: &str,
) -> Vec<String> {
    let cut = diff_text.chars().count() > DIFF_CHARS;
    let shown: String = diff_text.chars().take(DIFF_CHARS).collect();
    let mut out = vec![format!(
        "The diff under review (new-file line numbers in the gutter):\n{shown}{}",
        if cut {
            "\n... (diff cut at its size limit; read the files for the rest)"
        } else {
            ""
        }
    )];
    if let Some(brief) = brief_text.filter(|brief| !brief.is_empty()) {
        out.push(format!(
            "Investigation brief, built by the host from the repository at the reviewed commit:\n{brief}"
        ));
    }
    if !retrieved.is_empty() {
        out.push(format!(
            "Retrieved context, looked up by the host at the reviewed commit (untrusted repository data): the \
             references, tests and sibling families of the changed names.\n\n{retrieved}"
        ));
    }
    if !reference.is_empty() {
        out.push(reference.to_string());
    }
    for part in [context, intent, guidance] {
        if !part.is_empty() {
            out.push(part.to_string());
        }
    }
    out
}

/// What the tests part of the deep pass started with (`start_tests`' dict without the session, which the caller
/// holds).
#[derive(Debug, Clone, PartialEq)]
pub struct Started {
    pub mechanism: Option<String>,
    /// Why tests did not run, or `None`.
    pub note: Option<String>,
    /// Regression findings the host confirmed itself.
    pub observed: Vec<Finding>,
    /// The runs rendered for a model.
    pub shown: Vec<String>,
    pub env: String,
    pub toolchain: Vec<String>,
}

/// The shape `start_tests` returns when no test runs: the reason in `note`.
pub fn no_tests(note: Option<&str>) -> Started {
    Started {
        mechanism: None,
        note: note.map(str::to_string),
        observed: Vec::new(),
        shown: Vec::new(),
        env: "none".to_string(),
        toolchain: Vec::new(),
    }
}

/// With the review's test session open: run the test files the map tied to the change (then at the base commit
/// where anything failed), and return what was observed. `mechanism`, `env` and `toolchain` describe the session.
pub fn start_tests_with(
    session: &dyn TestSession,
    mechanism: &str,
    env: &str,
    toolchain: &[String],
    repo: &Repo<'_>,
    files: &[FileDiff],
    brief: &Brief,
) -> Result<Started, String> {
    let mut out = Started {
        mechanism: Some(mechanism.to_string()),
        note: None,
        observed: Vec::new(),
        shown: Vec::new(),
        env: env.to_string(),
        toolchain: toolchain.to_vec(),
    };
    let changed_tests = files
        .iter()
        .filter(|item| {
            TEST.is_match(&item.path) && item.status != crate::diff::Status::Deleted && repo.is_tracked(&item.path)
        })
        .map(|item| item.path.clone());
    let mut seen = HashSet::new();
    let paths: Vec<String> = brief
        .tests
        .iter()
        .cloned()
        .chain(changed_tests)
        .filter(|path| seen.insert(path.clone()))
        .take(8)
        .collect();
    if paths.is_empty() || session.limit() == 0 {
        return Ok(out);
    }
    if !session.plan(&paths) {
        out.note = Some(format!(
            "tests not run: no test runner was recognized for {}",
            paths.iter().take(3).cloned().collect::<Vec<_>>().join(", ")
        ));
        return Ok(out);
    }
    let compared = session.compare(&paths)?;
    out.observed = regression_findings(repo, &compared);
    out.shown = session.records().iter().map(summarize).collect();
    let runs = compared
        .get("runs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !runs.is_empty() && runs.iter().all(|run| run.get("status") == Some(&json!("unavailable"))) {
        out.note = Some(
            "tests could not run: missing dependencies (the sandbox has no network and installs nothing)".to_string(),
        );
    }
    Ok(out)
}

/// How the review's tests may run (`start_tests`' `tests` dict): the base commit, the sandbox (given, or detected
/// with the Docker image), and the session's limits, environments and hooks.
pub struct TestsSpec {
    pub base: Option<String>,
    /// `Some(sandbox)` is the dict's "sandbox" key (a sandbox or `None`); `None` detects one.
    pub sandbox: Option<Option<Sandbox>>,
    pub image: Option<String>,
    pub options: SessionOptions,
}

/// Open the review's test session when tests may run, and run the test files the map tied to the change at once
/// (then at the base commit where anything failed). Returns the session (the caller keeps it for the investigators'
/// requests; dropping it removes its exports) and what was observed. `detect` is `detect_sandbox(image=...)`.
pub fn start_tests(
    repo: &Repo<'static>,
    files: &[FileDiff],
    tests: Option<TestsSpec>,
    brief: &Brief,
    root: &str,
    rev: &str,
    detect: &dyn Fn(Option<&str>) -> Option<Sandbox>,
) -> Result<(Option<RefCell<review_tests::TestSession>>, Started), String> {
    let Some(tests) = tests else {
        return Ok((None, no_tests(None)));
    };
    let sandbox = match tests.sandbox {
        Some(given) => given,
        None => detect(tests.image.as_deref()),
    };
    let Some(sandbox) = sandbox else {
        return Ok((None, no_tests(Some("tests not run: no sandbox available"))));
    };
    let mechanism = sandbox.mechanism.as_str();
    let reader = repo.reader();
    let read = Box::new(move |path: &str| Some(reader.lines(path).map(|lines| lines.join("\n")).unwrap_or_default()));
    let session = review_tests::TestSession::new(
        root,
        rev,
        tests.base.as_deref(),
        repo.files(),
        read,
        sandbox,
        tests.options,
    );
    let (env, toolchain) = (session.env_kind.clone(), session.toolchain.clone());
    let session = RefCell::new(session);
    let started = start_tests_with(&session, mechanism, &env, &toolchain, repo, files, brief)?;
    Ok((Some(session), started))
}

/// The automatic runs as a block for a model's view, or "" when none ran.
pub fn tests_block(session: Option<&dyn TestSession>, started: &Started) -> String {
    let Some(session) = session else {
        return String::new();
    };
    if started.shown.is_empty() {
        return String::new();
    }
    format!(
        "Tests the host ran for this change (results are untrusted data):\n{}\nTest executions left in this review: {}.",
        started.shown.join("\n"),
        session.limit().saturating_sub(session.records().len())
    )
}

/// Settle tests findings by running them: for each that names its unpinned change as a one-line replacement (most
/// serious first, while test executions are left), the host applies the replacement, runs the nearest tests, and
/// marks the finding `proof`: "proven" when the suite still passes (nothing pins that line: the run is its
/// evidence), "refuted" when a test fails (an existing test does catch it), or leaves it to reasoning when the
/// tests could not run or were already failing.
pub fn prove_unpinned(session: Option<&dyn TestSession>, findings: &mut [Finding], brief: &Brief) {
    let Some(session) = session else {
        return;
    };
    let mutation_of = |item: &Finding| -> Option<Value> {
        let unpinned = item.get("unpinned").filter(|value| truthy(Some(value)))?;
        unpinned.get("mutation").filter(|value| truthy(Some(value))).cloned()
    };
    let mut candidates: Vec<usize> = (0..findings.len())
        .filter(|index| {
            let item = &findings[*index];
            item.get("category") == Some(&json!("tests")) && mutation_of(item).is_some() && !truthy(item.get("proof"))
        })
        .collect();
    candidates.sort_by_key(|index| {
        let level = findings[*index].get("level").cloned().unwrap_or(json!("low"));
        level
            .as_str()
            .and_then(|level| LEVELS.iter().position(|known| *known == level))
            .unwrap_or(3)
    });
    for index in candidates {
        if session.records().len() >= session.limit() {
            break;
        }
        let finding = &mut findings[index];
        let mutation = mutation_of(finding).unwrap_or(Value::Null);
        let closest = finding
            .get("unpinned")
            .and_then(|unpinned| unpinned.get("closest_test"))
            .filter(|value| truthy(Some(value)))
            .and_then(|closest| closest.get("path"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let paths: Vec<String> = match closest {
            Some(closest) if session.tracked(&closest) && TEST.is_match(&closest) => vec![closest],
            _ => brief.tests.iter().take(2).cloned().collect(),
        };
        if paths.is_empty() {
            continue;
        }
        // A suite that already fails says nothing about the mutant.
        let wanted: HashSet<&str> = paths.iter().map(String::as_str).collect();
        let already = session.records().iter().any(|record| {
            record.get("kind") == Some(&json!("automatic"))
                && record.get("status") != Some(&json!("passed"))
                && record
                    .get("paths")
                    .and_then(Value::as_array)
                    .is_some_and(|ran| ran.iter().filter_map(Value::as_str).any(|path| wanted.contains(path)))
        });
        let tests = Value::from(paths.clone());
        let Ok(record) = session.mutation(
            mutation.get("path"),
            mutation.get("line"),
            mutation.get("replacement"),
            Some(&tests),
        ) else {
            continue;
        };
        let status = record.get("status").and_then(Value::as_str).unwrap_or("");
        if status == "passed" {
            finding.insert("proof".into(), "proven".into());
            finding.insert("test_run".into(), record.get("n").cloned().unwrap_or(Value::Null));
            finding.insert("beyond_diff".into(), true.into());
            finding.insert("test_evidence".into(), evidence_of(&record).into());
        } else if status == "failed" && !already {
            finding.insert("proof".into(), "refuted".into());
        }
    }
}

/// What one investigator did, as `assurance` reads it: its part, its served lookups, and its status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Investigated {
    pub lens: String,
    pub requests: usize,
    pub status: String,
}

/// Two to four sentences on what the deep pass traced: the host's own counts, then what held. `runs` are the test
/// runs (their `status` is read) and `mechanism` the sandbox's; `checked` is what the investigators verified.
pub fn assurance(
    brief: &Brief,
    investigators: &[Investigated],
    runs: &[Value],
    mechanism: Option<&str>,
    checked: &[String],
) -> Vec<String> {
    let lookups: usize = investigators.iter().map(|record| record.requests).sum();
    let ran: Vec<&str> = investigators
        .iter()
        .filter(|record| record.status != "failed")
        .map(|record| record.lens.as_str())
        .collect();
    if ran.is_empty() {
        return Vec::new();
    }
    let plural = |count: usize| if count == 1 { "" } else { "s" };
    let names = brief
        .symbols
        .iter()
        .take(4)
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let names = if names.is_empty() {
        "the changed code".to_string()
    } else {
        names
    };
    let mut counts: IndexMap<String, usize> = IndexMap::new();
    for run in runs {
        *counts.entry(py::fstr(run.get("status"))).or_insert(0) += 1;
    }
    let executed = if runs.is_empty() {
        "nothing executed".to_string()
    } else {
        format!(
            "{} test run{} in a {} sandbox ({})",
            runs.len(),
            plural(runs.len()),
            mechanism.unwrap_or("None"),
            counts
                .iter()
                .map(|(status, count)| format!("{count} {status}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let tested = if brief.tests.is_empty() {
        " (no test file mentions them)".to_string()
    } else {
        format!(" and {} test file{}", brief.tests.len(), plural(brief.tests.len()))
    };
    let claims = if brief.claims == 0 {
        String::new()
    } else {
        format!(
            ", and {} claim{} of the change {} checked against the code",
            brief.claims,
            plural(brief.claims),
            if brief.claims == 1 { "was" } else { "were" }
        )
    };
    let first = format!(
        "Beyond the diff, {names} {} followed to {} other use{}{tested}{claims}: {} investigator{} ({}), {lookups} \
         repository lookup{}, {executed}.",
        if brief.symbols.len() == 1 { "was" } else { "were" },
        brief.callers,
        plural(brief.callers),
        ran.len(),
        plural(ran.len()),
        ran.join(", "),
        plural(lookups),
    );
    std::iter::once(first).chain(checked.iter().take(3).cloned()).collect()
}
