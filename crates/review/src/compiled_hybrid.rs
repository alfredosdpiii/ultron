//! The hybrid mode's host-written check programs (`autoreview_compiled.py` `candidate_shape`, `_finding_fields`,
//! `ask_material`, `template_steps`, `_quote_in`, `_quote_in_reads`), the planner's test situation
//! (`runner_availability`) and views (`planner_views`). A candidate finding from discovery gets a check shape; the
//! shape's template is a small program (the steps `compiled::validate` accepts) gated on its decisive step.
//!
//! Known differences from the Python, outside recorded behaviour: a candidate without an integer line gets no
//! template ("no line": Python raised); a registry-member shape with no registries gets none either (Python
//! raised `IndexError`); a read step whose `start`/`end` is not a number counts it as absent (Python raised).

use std::collections::HashSet;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::compiled::{
    Brief, CoverageItem, MAX_ASKS, MAX_PROGRAM_STEPS, MAX_QUESTION_CHARS, MAX_STEPS, Step, flat, list,
};
use crate::diff::{FileDiff, render_hunk};
use crate::findings::int_of;
use crate::levels::truthy;
use crate::py;
use crate::views::hunk_for;

pub const CANDIDATE_READ_RADIUS: i64 = 12;
pub const CITATION_READ_RADIUS: i64 = 6;
pub const CLOSEST_TEST_RADIUS: i64 = 25;
/// Where a variable, flag or config key must be set when the feature runs: manifests, compose files, CI, infra.
pub const DEPLOY_GLOBS: [&str; 7] = [
    ".github/**",
    "**/*.yml",
    "**/*.yaml",
    "**/*.tf",
    "**/*.env*",
    "**/Dockerfile*",
    "**/*.toml",
];
/// `autoreview_deep.DIFF_CHARS`: the diff a planner sees is cut at this many characters.
pub const DIFF_CHARS: usize = 40_000;

/// The repository at the reviewed commit, as the templates read it (the deep pass's `Repo`).
pub trait Source {
    /// The lines of a tracked text file; `None` when it is not one (untracked, binary, too large).
    fn lines(&self, path: &str) -> Option<Vec<String>>;
    /// Every tracked path.
    fn files(&self) -> Vec<String>;
}

impl Source for crate::deep_repo::Repo<'_> {
    fn lines(&self, path: &str) -> Option<Vec<String>> {
        crate::deep_repo::Repo::lines(self, path).map(|lines| lines.as_ref().clone())
    }

    fn files(&self) -> Vec<String> {
        crate::deep_repo::Repo::files(self).to_vec()
    }
}

static COMMENTISH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(comment|docstring|readme|description|documentation|help text|says|claims)\b").expect("pattern")
});
static ERRORISH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(except|catch|raise|throw|rollback|swallow|sentinel|exception|error path|cleanup|finally)\b")
        .expect("pattern")
});
static INPUTISH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(regex|regular expression|pattern|guard|input|validat|parse|boundary|escape|unicode|limit)\w*")
        .expect("pattern")
});
static WORDS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z_][A-Za-z0-9_.-]{2,}").expect("pattern"));

/// `f"{candidate.get(key, '')}"`.
fn field_text(candidate: &Map<String, Value>, key: &str) -> String {
    match candidate.get(key) {
        None => String::new(),
        value => py::fstr(value),
    }
}

/// The words of a candidate's claim and reason (names it may use).
fn names_of(candidate: &Map<String, Value>) -> HashSet<String> {
    let text = format!("{} {}", field_text(candidate, "claim"), field_text(candidate, "why"));
    WORDS.find_iter(&text).map(|found| found.as_str().to_string()).collect()
}

/// The check shape a candidate finding gets, and what its template needs.
pub fn candidate_shape(
    candidate: &Map<String, Value>,
    brief: &Brief,
    retrieval: &Map<String, Value>,
) -> (String, Value) {
    if truthy(candidate.get("host_confirmed")) {
        return ("regression".into(), json!({}));
    }
    let text = format!("{} {}", field_text(candidate, "claim"), field_text(candidate, "why"));
    let names = names_of(candidate);
    let category = candidate.get("category").and_then(Value::as_str);
    if category == Some("tests") {
        return ("unpinned-behaviour".into(), json!({}));
    }
    let empty = Map::new();
    let registries = retrieval.get("registries").and_then(Value::as_object).unwrap_or(&empty);
    for key in list(retrieval, "keys").iter().filter_map(Value::as_str) {
        if names.contains(key)
            && let Some(Value::Array(found)) = registries.get(key)
            && !found.is_empty()
        {
            let first: Vec<Value> = found.iter().take(3).cloned().collect();
            return ("registry-member".into(), json!({ "key": key, "registries": first }));
        }
    }
    let extracted = &brief.extracted;
    for name in list(extracted, "env")
        .iter()
        .chain(list(extracted, "flags"))
        .chain(list(extracted, "config"))
    {
        if let Some(name) = name.as_str().filter(|name| names.contains(*name)) {
            return ("env-in-deploy".into(), json!({ "name": name }));
        }
    }
    let shape = if category == Some("docs") || COMMENTISH.is_match(&text) {
        "comment-vs-code"
    } else if ERRORISH.is_match(&text) {
        "error-path"
    } else if INPUTISH.is_match(&text) {
        "input-defeats-guard"
    } else {
        "consistency"
    };
    (shape.into(), json!({}))
}

/// `value or default`.
fn or(value: Option<&Value>, default: Value) -> Value {
    if truthy(value) {
        value.cloned().unwrap_or(default)
    } else {
        default
    }
}

/// A candidate as the fields of a `finding` step: the discovering pass's wording, level and rules. `None` when it
/// has no integer line.
pub fn finding_fields(candidate: &Map<String, Value>) -> Option<Map<String, Value>> {
    let line = int_of(candidate.get("line"))?;
    let level = if truthy(candidate.get("finder_level")) {
        candidate.get("finder_level")
    } else {
        candidate.get("level")
    };
    let citations = match candidate.get("citations") {
        Some(Value::Array(items)) => Value::Array(items.clone()),
        _ => json!([]),
    };
    let fields = json!({
        "file": candidate.get("file").cloned().unwrap_or(Value::Null), "line": line,
        "level": or(level, "low".into()), "category": or(candidate.get("category"), "correctness".into()),
        "claim": or(candidate.get("claim"), "".into()), "why": or(candidate.get("why"), "".into()),
        "fix": or(candidate.get("suggested_fix"), "".into()), "scenario": or(candidate.get("scenario"), "".into()),
        "citations": citations, "unpinned": or(candidate.get("unpinned"), Value::Null),
        "consequence": or(candidate.get("consequence"), "".into()),
    });
    fields.as_object().cloned()
}

/// What an ask about a candidate gets besides the cited lines: the diff hunk at the finding's line and the
/// retrieved references of the names its claim uses.
pub fn ask_material(
    candidate: &Map<String, Value>,
    files: Option<&[FileDiff]>,
    retrieval: Option<&Map<String, Value>>,
) -> Vec<String> {
    let mut out = Vec::new();
    let path = candidate.get("file");
    let line = if truthy(candidate.get("line")) {
        int_of(candidate.get("line")).unwrap_or(0)
    } else {
        0
    };
    let item = files
        .unwrap_or(&[])
        .iter()
        .find(|item| path.and_then(Value::as_str) == Some(item.path.as_str()));
    if let Some(hunk) = hunk_for(item, line) {
        out.push(format!(
            "Diff hunk of {} (new-file line numbers; + added, - removed):\n{}",
            py::fstr(path),
            render_hunk(hunk).join("\n")
        ));
    }
    let names = names_of(candidate);
    let used: Vec<String> = retrieval
        .and_then(|retrieval| retrieval.get("sections"))
        .and_then(Value::as_object)
        .map(|sections| {
            sections
                .iter()
                .filter(|(name, _)| names.contains(name.as_str()))
                .take(2)
                .map(|(_, text)| py::str_of(text))
                .collect()
        })
        .unwrap_or_default();
    if !used.is_empty() {
        out.push(format!(
            "Retrieved references of the names the claim uses (outside the changed lines, at the reviewed commit):\n\n{}",
            used.join("\n\n")
        ));
    }
    out
}

/// Python's `re.escape`: the characters special in a pattern, and whitespace, get a backslash.
pub fn re_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "()[]{}?*+-|^$\\.&~# \t\n\r\u{b}\u{c}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn object(value: Value) -> Step {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

struct Template<'a> {
    cid: &'a str,
    repo: &'a dyn Source,
    fields: Map<String, Value>,
    steps: Vec<Step>,
}

impl Template<'_> {
    /// A read of `target` around `at`; false when the file has no lines.
    fn read(&mut self, sid: &str, target: &str, at: i64, radius: i64) -> bool {
        let total = self.repo.lines(target).map_or(0, |lines| lines.len()) as i64;
        if total == 0 {
            return false;
        }
        let (start, end) = ((at - radius).max(1), total.min(at + radius));
        self.steps.push(object(json!({
            "id": sid, "op": "read", "args": { "path": target, "start": start, "end": end },
        })));
        true
    }

    fn finding(&mut self, gate: &str, evidence: Vec<String>, negate: bool) {
        let mut step = object(json!({
            "id": format!("{}_f", self.cid), "op": "finding", "when": { "step": gate, "not": negate }, "evidence": evidence,
        }));
        step.extend(self.fields.clone());
        self.steps.push(step);
    }
}

/// Whether the runner of these test paths is available.
pub type Runnable<'a> = &'a dyn Fn(&[String]) -> bool;

/// The check program of one candidate, from its shape: (steps, what the template does). The finding step carries
/// the candidate's own wording and is gated on the decisive check. `runnable(test_paths)` says whether the runner
/// of those tests is available (a mutation check is planned only then; else the ask variant).
#[allow(clippy::too_many_arguments)]
pub fn template_steps(
    cid: &str,
    candidate: &Map<String, Value>,
    shape: &str,
    details: &Value,
    repo: &dyn Source,
    brief: &Brief,
    tests_available: bool,
    files: Option<&[FileDiff]>,
    retrieval: Option<&Map<String, Value>>,
    runnable: Option<Runnable<'_>>,
) -> (Vec<Step>, String) {
    let Some(fields) = finding_fields(candidate) else {
        return (Vec::new(), "no line".into());
    };
    let path = py::fstr(fields.get("file"));
    let line = fields.get("line").and_then(Value::as_i64).unwrap_or(0);
    let claim = py::fstr(fields.get("claim"));
    let material = ask_material(candidate, files, retrieval);
    let mut template = Template {
        cid,
        repo,
        fields,
        steps: Vec::new(),
    };
    let id = |suffix: &str| format!("{cid}_{suffix}");
    if shape == "unpinned-behaviour" {
        let unpinned = candidate
            .get("unpinned")
            .and_then(Value::as_object)
            .filter(|_| truthy(candidate.get("unpinned")));
        let mutation = unpinned.and_then(|unpinned| unpinned.get("mutation"));
        let closest = unpinned
            .and_then(|unpinned| unpinned.get("closest_test"))
            .and_then(Value::as_object);
        let closest_path = closest.and_then(|closest| closest.get("path")).and_then(Value::as_str);
        let test_paths: Vec<String> = match closest_path {
            Some(found) if repo.files().iter().any(|file| file == found) => vec![found.to_string()],
            _ => brief.tests.iter().take(2).cloned().collect(),
        };
        if let Some(mutation) = mutation
            .filter(|mutation| truthy(Some(mutation)))
            .and_then(Value::as_object)
            && tests_available
            && !test_paths.is_empty()
            && runnable.is_none_or(|runnable| runnable(&test_paths))
        {
            let (at, number) = (py::fstr(mutation.get("path")), py::fstr(mutation.get("line")));
            template.steps.push(object(json!({
                "id": id("m"), "op": "mutation_check", "args": {
                    "path": mutation.get("path"), "line": mutation.get("line"), "replacement": mutation.get("replacement"),
                    "tests": test_paths,
                },
            })));
            template.steps.push(object(json!({
                "id": id("a"), "op": "assert", "step": id("m"), "predicate": "status == passed", "expect": true,
                "holds": format!("nothing pins {at}:{number}: the mutant survived"),
            })));
            template.finding(&id("a"), vec![id("m")], false);
            let how = format!("mutation_check of {at}:{number} against {}", test_paths.join(", "));
            return (template.steps, how);
        }
        let test_path = test_paths.first().cloned();
        let mut context = Vec::new();
        if let Some(test_path) = &test_path {
            let at = closest.map_or(1, |closest| {
                if truthy(closest.get("line")) {
                    int_of(closest.get("line")).unwrap_or(1)
                } else {
                    1
                }
            });
            if template.read(&id("t"), test_path, at, CLOSEST_TEST_RADIUS) {
                context.push(id("t"));
            }
        }
        if template.read(&id("r"), &path, line, CANDIDATE_READ_RADIUS) {
            context.push(id("r"));
        }
        if context.is_empty() {
            return (Vec::new(), "no source to read".into());
        }
        let said = |key: &str| {
            unpinned
                .and_then(|unpinned| unpinned.get(key))
                .filter(|value| truthy(Some(value)))
        };
        let behaviour = said("behaviour").map_or_else(|| claim.clone(), |value| py::fstr(Some(value)));
        let change = said("change").map_or_else(|| "undo the new behaviour".to_string(), |value| py::fstr(Some(value)));
        let question = format!(
            "Behaviour: {behaviour}. Change to it: {change}. Looking at the test lines shown, would every assertion \
             still pass after that change (that is, does no shown test pin this behaviour)? Say first which shown \
             assertion would fail if one would. Answer yes if the behaviour is unpinned, quoting the closest \
             assertion that still passes; no if a shown assertion would fail, quoting it; unclear if the lines \
             cannot tell."
        );
        template.steps.push(object(json!({
            "id": id("q"), "op": "ask", "question": question, "context": context, "material": material,
        })));
        let mut evidence = vec![id("q")];
        evidence.extend(context);
        template.finding(&id("q"), evidence, false);
        let how = format!(
            "ask over {} whether the change would be caught",
            test_path.unwrap_or_else(|| path.clone())
        );
        return (template.steps, how);
    }
    if shape == "registry-member" {
        let key = py::fstr(details.get("key"));
        let registries: Vec<String> = details
            .get("registries")
            .and_then(Value::as_array)
            .map(|items| items.iter().take(3).map(|item| py::fstr(Some(item))).collect())
            .unwrap_or_default();
        if registries.is_empty() {
            return (Vec::new(), "no registry to check".into());
        }
        let mut asserts = Vec::new();
        for (index, registry) in registries.iter().enumerate() {
            let number = index + 1;
            template.steps.push(object(json!({
                "id": id(&format!("g{number}")), "op": "grep",
                "args": { "pattern": re_escape(&key), "path_glob": registry, "count_only": true },
            })));
            template.steps.push(object(json!({
                "id": id(&format!("a{number}")), "op": "assert", "step": id(&format!("g{number}")),
                "predicate": "count == 0", "expect": true, "holds": format!("`{key}` is registered in {registry}"),
            })));
            asserts.push(id(&format!("a{number}")));
        }
        let mut gate = asserts[0].clone();
        if asserts.len() > 1 {
            template.steps.push(object(
                json!({ "id": id("all"), "op": "assert", "any": asserts, "expect": true }),
            ));
            gate = id("all");
        }
        let greps = (1..=asserts.len()).map(|number| id(&format!("g{number}"))).collect();
        template.finding(&gate, greps, false);
        return (
            template.steps,
            format!("count_only grep of `{key}` in {}", registries.join(", ")),
        );
    }
    if shape == "env-in-deploy" {
        let name = py::fstr(details.get("name"));
        let globs = &DEPLOY_GLOBS[..4];
        let mut asserts = Vec::new();
        for (index, glob) in globs.iter().enumerate() {
            let number = index + 1;
            template.steps.push(object(json!({
                "id": id(&format!("g{number}")), "op": "grep",
                "args": { "pattern": re_escape(&name), "path_glob": glob, "count_only": true },
            })));
            template.steps.push(object(json!({
                "id": id(&format!("a{number}")), "op": "assert", "step": id(&format!("g{number}")),
                "predicate": "count == 0", "expect": true,
            })));
            asserts.push(id(&format!("a{number}")));
        }
        template.steps.push(object(json!({
            "id": id("all"), "op": "assert", "all": asserts, "expect": true,
            "holds": format!("`{name}` is set in no manifest, compose file, workflow or infrastructure file"),
        })));
        let greps = (1..=asserts.len()).map(|number| id(&format!("g{number}"))).collect();
        template.finding(&id("all"), greps, false);
        return (
            template.steps,
            format!("count_only grep of `{name}` in {}", globs.join(", ")),
        );
    }
    // comment-vs-code, error-path, input-defeats-guard, consistency: the exact lines and one yes/no question.
    let mut context = Vec::new();
    if template.read(&id("r"), &path, line, CANDIDATE_READ_RADIUS) {
        context.push(id("r"));
    }
    let citations: Vec<Value> = match candidate.get("citations") {
        Some(Value::Array(items)) => items.iter().take(3).cloned().collect(),
        _ => Vec::new(),
    };
    for (index, citation) in citations.iter().enumerate() {
        let cited_line = |value: Option<&Value>| if truthy(value) { int_of(value).unwrap_or(0) } else { 0 };
        if citation.get("path") == template.fields.get("file")
            && (cited_line(citation.get("line")) - line).abs() <= CANDIDATE_READ_RADIUS
        {
            continue;
        }
        let target = py::fstr(citation.get("path"));
        let sid = id(&format!("c{}", index + 1));
        if template.read(
            &sid,
            &target,
            int_of(citation.get("line")).unwrap_or(0),
            CITATION_READ_RADIUS,
        ) {
            context.push(sid);
        }
    }
    if context.is_empty() {
        return (Vec::new(), "no source to read".into());
    }
    let why = py::fstr(template.fields.get("why"));
    let scenario = py::fstr(template.fields.get("scenario"));
    let question = format!(
        "Candidate finding at {path}:{line}: {claim} Reason given: {}{} Two sides: first, what would the material \
         have to show for this claim to be false (a guard, a handler, a caller that validates, a test that pins it, \
         a different order)? Is that there? Then answer yes only if the cited lines have the problem as described \
         and nothing shown prevents it, quoting the cited line that has it; no if the code shown prevents it or the \
         claim misreads the code, quoting the line that does; unclear if the material cannot tell.",
        if why.is_empty() { "(none)" } else { &why },
        if scenario.is_empty() {
            String::new()
        } else {
            format!(" Scenario: {scenario}")
        },
    );
    let attached = if material.len() == 2 {
        ", the diff hunk and retrieved references attached"
    } else if material.first().is_some_and(|first| first.starts_with("Diff hunk")) {
        ", the diff hunk attached"
    } else if !material.is_empty() {
        ", retrieved references attached"
    } else {
        ""
    };
    let how = format!(
        "ask over {} with the claim, reason and scenario{attached}",
        context.join(", ")
    );
    template.steps.push(object(json!({
        "id": id("q"), "op": "ask", "question": py::clip(&question, MAX_QUESTION_CHARS), "context": context,
        "material": material,
    })));
    let mut evidence = vec![id("q")];
    evidence.extend(context);
    template.finding(&id("q"), evidence, false);
    (template.steps, how)
}

/// Whether `quote` comes from the lines of `path` within `radius` of `line`: a line holds the quote, or a line
/// of 8 or more characters is inside it (whitespace runs squashed).
pub fn quote_in(repo: &dyn Source, path: &str, line: i64, quote: &str, radius: i64) -> bool {
    let lines = repo.lines(path).unwrap_or_default();
    let quote = flat(quote);
    if quote.chars().count() < 4 {
        return false;
    }
    let (start, end) = ((line - radius).max(1), (lines.len() as i64).min(line + radius));
    (start..=end).any(|number| {
        let text = flat(&lines[number as usize - 1]);
        text.contains(&quote) || (text.chars().count() >= 8 && quote.contains(&text))
    })
}

/// Whether a quote comes from the lines a check program read (the cited lines, not the material attached).
pub fn quote_in_reads(repo: &dyn Source, steps: &[Step], quote: &str) -> bool {
    steps.iter().any(|step| {
        let Some(args) = step
            .get("args")
            .and_then(Value::as_object)
            .filter(|_| step.get("op") == Some(&json!("read")))
        else {
            return false;
        };
        let number = |key: &str, default: i64| {
            if truthy(args.get(key)) {
                int_of(args.get(key)).unwrap_or(default)
            } else {
                default
            }
        };
        let (start, end) = (number("start", 1), number("end", 0));
        let middle = (start + end).div_euclid(2);
        let radius = ((end - start).div_euclid(2) + 1).max(1);
        quote_in(repo, &py::fstr(args.get("path")), middle, quote, radius)
    })
}

/// One test run as the session recorded it (the parts availability reads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub runner: String,
    pub cwd: Option<String>,
    pub status: String,
}

/// What the automatic run showed per runner and directory: lines for the planner, and the (runner, directory)
/// pairs found unavailable (test steps for them are could_not_run at once). `runs` is `None` without a test
/// session; `runner` is the repository's runner (name, why) when one was recognized.
pub fn runner_availability(
    runs: Option<&[RunRecord]>,
    runner: Option<(&str, &str)>,
) -> (Vec<String>, Vec<(String, String)>) {
    let Some(runs) = runs else {
        return (Vec::new(), Vec::new());
    };
    let mut seen: IndexMap<(String, String), String> = IndexMap::new();
    for run in runs {
        let directory = run
            .cwd
            .clone()
            .filter(|cwd| !cwd.is_empty())
            .unwrap_or_else(|| ".".into());
        let key = (run.runner.clone(), directory);
        if !seen.contains_key(&key) || run.status != "passed" {
            seen.insert(key, run.status.clone());
        }
    }
    let mut lines = Vec::new();
    let mut unavailable = Vec::new();
    for ((name, directory), status) in &seen {
        let place = format!("{name} in {directory}");
        if status == "unavailable" {
            unavailable.push((name.clone(), directory.clone()));
            lines.push(format!(
                "{place}: unavailable (missing dependencies; run_tests and mutation_check on its files will not run: \
                 read the tests and ask instead)"
            ));
        } else {
            lines.push(format!("{place}: available (the automatic run {status})"));
        }
    }
    if lines.is_empty()
        && let Some((name, because)) = runner
    {
        lines.push(format!("{name} (the repository's runner, {because}): not tried yet"));
    }
    (lines, unavailable)
}

/// [`runner_availability`] of the review's test session (`runner_availability(session)`): its records, and the
/// repository's runner with why it was recognized.
pub fn session_availability(
    session: Option<&crate::review_tests::TestSession>,
) -> (Vec<String>, Vec<(String, String)>) {
    let Some(session) = session else {
        return runner_availability(None, None);
    };
    let runs: Vec<RunRecord> = session
        .records
        .iter()
        .map(|record| RunRecord {
            runner: record.runner.clone(),
            cwd: Some(record.cwd.clone()),
            status: record.status.clone(),
        })
        .collect();
    let runner = session
        .runner
        .as_ref()
        .map(|runner| (runner.name.as_str(), runner.because.as_str()));
    runner_availability(Some(&runs), runner)
}

/// What the planner is told besides the diff and the brief.
#[derive(Debug, Clone, Default)]
pub struct PlannerSituation<'a> {
    pub context: &'a str,
    pub intent: &'a str,
    pub guidance: &'a str,
    pub tests_block: &'a str,
    pub runners: &'a [String],
    pub tests_allowed: bool,
    pub runs_left: i64,
    pub coverage: &'a [CoverageItem],
    /// (steps as written, steps after expansion); `None` is the default limits.
    pub limits: Option<(usize, usize)>,
    pub retrieved: &'a str,
}

/// The planner's views: the diff (cut at its size limit, and whether it was), the brief, the retrieved context,
/// the intent and guides, the coverage the program must have, the limits, the test situation.
pub fn planner_views(diff_text: &str, brief_text: &str, situation: &PlannerSituation<'_>) -> (Vec<String>, bool) {
    let cut = diff_text.chars().count() > DIFF_CHARS;
    let shown: String = diff_text.chars().take(DIFF_CHARS).collect();
    let mut views = vec![
        format!(
            "The diff under review (new-file line numbers in the gutter):\n{shown}{}",
            if cut {
                "\n... (diff cut at its size limit; read the files for the rest)"
            } else {
                ""
            }
        ),
        format!("Investigation brief, built by the host from the repository at the reviewed commit:\n{brief_text}"),
    ];
    if !situation.retrieved.is_empty() {
        views.push(format!(
            "Retrieved context, looked up by the host at the reviewed commit (untrusted repository data): the \
             references, tests and sibling families of the changed names, and the helpers the change calls.\n\n{}",
            situation.retrieved
        ));
    }
    for part in [
        situation.context,
        situation.intent,
        situation.guidance,
        situation.tests_block,
    ] {
        if !part.is_empty() {
            views.push(part.to_string());
        }
    }
    if !situation.coverage.is_empty() {
        let items: Vec<String> = situation
            .coverage
            .iter()
            .map(|item| format!("- {}: {}", item.id, item.text))
            .collect();
        views.push(format!(
            "Coverage the program must have (name the id in a step's \"covers\", or list it in the program's \
             \"uncovered\" as \"<id>: why\"; T-items are catalogue shapes whose trigger is in this change):\n{}",
            items.join("\n")
        ));
    }
    let (planned, expanded) = situation.limits.unwrap_or((MAX_PROGRAM_STEPS, MAX_STEPS));
    views.push(format!(
        "Limits: {planned} steps as written, {expanded} after for_each expansion, {MAX_ASKS} asks."
    ));
    if situation.tests_allowed {
        let runners: Vec<String> = situation.runners.iter().map(|line| format!("- {line}")).collect();
        let runners = if runners.is_empty() {
            "- none recognized".to_string()
        } else {
            runners.join("\n")
        };
        views.push(format!(
            "Tests may run: yes; executions left: {}. Test runners:\n{runners}",
            situation.runs_left
        ));
    } else {
        views.push("Tests may run: no.".into());
    }
    (views, cut)
}
