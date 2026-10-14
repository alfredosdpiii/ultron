//! The compiled mode's program language (`autoreview_compiled.py`): constants, predicates, step normalization,
//! references, grounding, coverage (the map's items and the catalogue shapes a change triggers) and `validate`.
//! Steps are the JSON objects the planner wrote, normalized into the same objects the Python held (the parsed
//! predicate as a 3-item array under `predicate_parsed`), so the interpreter (`compiled_eval`) and the hybrid
//! templates (`compiled_hybrid`) read them as the Python did.
//!
//! Known differences from the Python, all outside recorded behaviour:
//! - `material` that is a truthy number or `true` makes Python raise (`TypeError` iterating it); here it attaches
//!   nothing. A string or an object is iterated as Python does (its characters, its keys).
//! - `max_items` of an infinite float raised `OverflowError` in Python; here it is the default.
//! - A finding's `line` string counts as a number when every character is numeric (`char::is_numeric`); Python's
//!   `isdigit` also refuses vulgar fractions and letter numerals (`½`, `Ⅻ`).
//! - `for_each.over` that is a list made the Python raise (`unhashable`); here it names no step.

use std::collections::HashSet;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::diff::{FileDiff, file_kind, skip_reason};
use crate::findings::{int_of, text};
use crate::levels::{is_code, truthy};
use crate::prompt_text::{CHECK_CATALOGUE, TRIGGERED_SHAPES};
use crate::py;

pub type Step = Map<String, Value>;

pub const LOOKUPS: [&str; 8] = [
    "read",
    "grep",
    "list",
    "definition",
    "references",
    "history",
    "blame_range",
    "pickaxe",
];
pub const TEST_OPS: [&str; 2] = ["run_tests", "mutation_check"];
/// Results with items a `for_each` may iterate.
pub const ITERABLE: [&str; 6] = ["grep", "references", "list", "history", "blame_range", "pickaxe"];
pub const OPS: [&str; 14] = [
    "read",
    "grep",
    "list",
    "definition",
    "references",
    "history",
    "blame_range",
    "pickaxe",
    "run_tests",
    "mutation_check",
    "for_each",
    "ask",
    "assert",
    "finding",
];
pub const LEVELS: [&str; 5] = ["critical", "high", "medium", "low", "nit"];
pub const OLD_LEVELS: [(&str, &str); 3] = [("blocker", "critical"), ("major", "high"), ("minor", "low")];
pub const MAX_PROGRAM_STEPS: usize = 80;
pub const MAX_STEPS: usize = 120;
/// A diff with more changed lines than this gets the larger step limits.
pub const LARGE_DIFF_LINES: usize = 100;
pub const LARGE_PROGRAM_STEPS: usize = 120;
pub const LARGE_STEPS: usize = 200;
pub const MAX_ASKS: usize = 40;
pub const MAX_FINDINGS: usize = 12;
pub const MAX_FOR_EACH_ITEMS: i64 = 20;
pub const DEFAULT_FOR_EACH_ITEMS: i64 = 8;
pub const MAX_TEMPLATE_STEPS: usize = 8;
pub const MAX_QUESTION_CHARS: usize = 1_500;
pub const ASK_CONTEXT_CHARS: usize = 12_000;
pub const ASK_MATERIAL_CHARS: usize = 4_000;
pub const MAX_ASK_MATERIAL: usize = 4;
/// A result's text is kept up to this; a `contains` over a cut text that finds nothing is unknown.
pub const RESULT_CHARS: usize = 24_000;
pub const RECORD_CHARS: usize = 1_200;
pub const EVIDENCE_CHARS: usize = 1_200;
pub const MAX_HOLDS: usize = 3;
pub const STATUSES: [&str; 3] = ["passed", "failed", "could_not_run"];
pub const ANSWERS: [&str; 3] = ["yes", "no", "unclear"];
pub const CAPPED: &str = "(more not shown)";
/// The planner's id pattern as the Python wrote it (shown in errors).
pub const ID_PATTERN: &str = "^[A-Za-z_][A-Za-z0-9_-]{0,39}$";

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

// Python's `$` (without MULTILINE) also matches before a final newline: `\n?\z`.
static ID: LazyLock<Regex> = LazyLock::new(|| regex(r"\A[A-Za-z_][A-Za-z0-9_-]{0,39}\n?\z"));
pub(crate) static PLACEHOLDER: LazyLock<Regex> =
    LazyLock::new(|| regex(r"\{\{\s*([A-Za-z_][A-Za-z0-9_\-]*(?:\[\d+\])?(?:\.[A-Za-z_][A-Za-z0-9_\-]*)*)\s*\}\}"));
static COUNT: LazyLock<Regex> = LazyLock::new(|| regex(r"\Acount\s*(==|!=|>=|<=|>|<)\s*(\d+)\n?\z"));
static WORD: LazyLock<Regex> = LazyLock::new(|| regex(r"\A(status|answer)\s*(==|!=)\s*([a-z_]+)\n?\z"));
static CONTAINS: LazyLock<Regex> = LazyLock::new(|| regex(r"(?s)\A(not\s+)?contains\s+(.+)\n?\z"));

/// `" ".join(text.split())`.
pub fn flat(text: &str) -> String {
    py::squash(text)
}

/// Python's `int(text)` of a string of decimal digits (any script's, as `\d` matched them); saturating.
pub(crate) fn decimal(text: &str) -> Option<i64> {
    static DIGIT: LazyLock<Regex> = LazyLock::new(|| regex(r"\A\d\z"));
    let is_digit = |c: char| DIGIT.is_match(c.encode_utf8(&mut [0; 4]));
    let mut value: i64 = 0;
    for c in text.chars() {
        // Unicode decimal digits come in runs of ten from zero: the value is the offset in its run.
        let digit = match c.to_digit(10) {
            Some(digit) => digit,
            None if is_digit(c) => {
                let mut start = c as u32;
                while let Some(previous) = start.checked_sub(1).and_then(char::from_u32)
                    && is_digit(previous)
                {
                    start -= 1;
                }
                (c as u32 - start) % 10
            }
            None => return None,
        };
        value = value.saturating_mul(10).saturating_add(i64::from(digit));
    }
    (!text.is_empty()).then_some(value)
}

/// A predicate as `[kind, a, b]`: `["count", op, n]`, `["status"|"answer", op, word]`, `["contains", needle,
/// "not"|""]`; `None` when it is not one.
pub fn parse_predicate(raw: Option<&Value>) -> Option<[String; 3]> {
    let raw = raw?.as_str()?;
    let text = py::strip(raw);
    if text.is_empty() {
        return None;
    }
    if let Some(found) = COUNT.captures(text) {
        return Some(["count".into(), found[1].into(), found[2].into()]);
    }
    if let Some(found) = WORD.captures(text) {
        let allowed: &[&str] = if &found[1] == "status" { &STATUSES } else { &ANSWERS };
        return allowed
            .contains(&&found[3])
            .then(|| [found[1].into(), found[2].into(), found[3].into()]);
    }
    if let Some(found) = CONTAINS.captures(text) {
        let mut needle = py::strip(&found[2]).to_string();
        let chars: Vec<char> = needle.chars().collect();
        if chars.len() >= 2 && chars[0] == chars[chars.len() - 1] && matches!(chars[0], '"' | '\'') {
            needle = chars[1..chars.len() - 1].iter().collect();
        }
        let negated = if found.get(1).is_some() { "not" } else { "" };
        return (!needle.is_empty()).then(|| ["contains".into(), needle, negated.into()]);
    }
    None
}

/// The predicate stored on a normalized step, as a 3-item array or null.
pub(crate) fn parsed_of(step: &Step) -> Option<[String; 3]> {
    let items = step.get("predicate_parsed")?.as_array()?;
    let get = |index: usize| items.get(index).and_then(Value::as_str).map(str::to_string);
    Some([get(0)?, get(1)?, get(2)?])
}

/// `_when`: `{"step", "not"}` or nothing, and the problem with a malformed one.
fn when_of(raw: Option<&Value>) -> (Option<Value>, Option<&'static str>) {
    match raw {
        None | Some(Value::Null) => (None, None),
        Some(Value::String(step)) => (Some(json!({ "step": step, "not": false })), None),
        Some(Value::Object(map)) if map.get("step").is_some_and(Value::is_string) => (
            Some(json!({ "step": map["step"], "not": map.get("not") == Some(&Value::Bool(true)) })),
            None,
        ),
        Some(_) => (
            None,
            Some("when must be {\"step\": <id>} or {\"step\": <id>, \"not\": true}"),
        ),
    }
}

/// `_ids`: the strings of a list.
fn ids(raw: Option<&Value>) -> Vec<Value> {
    match raw {
        Some(Value::Array(items)) => items.iter().filter(|item| item.is_string()).cloned().collect(),
        _ => Vec::new(),
    }
}

/// (steps as written, steps after expansion) a program may have for a diff of `changed_lines` lines.
pub fn limits_for(changed_lines: usize) -> (usize, usize) {
    if changed_lines > LARGE_DIFF_LINES {
        (LARGE_PROGRAM_STEPS, LARGE_STEPS)
    } else {
        (MAX_PROGRAM_STEPS, MAX_STEPS)
    }
}

/// A validated program: top-level steps in order, each normalized (a `for_each` carries its template steps), plus
/// the coverage items it declares it could not check, with the planner's reasons.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub steps: Vec<Step>,
    pub summary: String,
    pub uncovered: Vec<String>,
}

impl Program {
    pub fn as_json(&self) -> Value {
        let mut out = Map::new();
        out.insert("summary".into(), self.summary.clone().into());
        out.insert(
            "steps".into(),
            Value::Array(self.steps.iter().map(|step| Value::Object(public_step(step))).collect()),
        );
        if !self.uncovered.is_empty() {
            out.insert("uncovered".into(), json!(self.uncovered));
        }
        Value::Object(out)
    }
}

fn empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
        _ => false,
    }
}

/// A step as the program's JSON shows it: no parsed predicate, no empty values.
pub fn public_step(step: &Step) -> Step {
    let mut out: Step = step
        .iter()
        .filter(|(key, value)| key.as_str() != "predicate_parsed" && !empty(value))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if let Some(Value::Array(steps)) = out.get_mut("steps") {
        for item in steps.iter_mut() {
            if let Value::Object(map) = item {
                *map = public_step(map);
            }
        }
    }
    out
}

pub(crate) fn op_of(step: &Step) -> &str {
    step.get("op").and_then(Value::as_str).unwrap_or("")
}

pub(crate) fn id_of(step: &Step) -> &str {
    step.get("id").and_then(Value::as_str).unwrap_or("")
}

pub(crate) fn strings(step: &Step, key: &str) -> Vec<String> {
    match step.get(key) {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => Vec::new(),
    }
}

/// The step a `when` names, and whether it is negated.
pub(crate) fn when_step(step: &Step) -> Option<(&str, bool)> {
    let when = step.get("when")?.as_object()?;
    Some((when.get("step")?.as_str()?, when.get("not") == Some(&Value::Bool(true))))
}

fn is_lookup_or_test(op: &str) -> bool {
    LOOKUPS.contains(&op) || TEST_OPS.contains(&op)
}

/// The items `for item in value` yields in Python, for the iterables a planner may write.
fn iterate(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) => items.clone(),
        Value::String(text) => text.chars().map(|c| Value::String(c.to_string())).collect(),
        Value::Object(map) => map.keys().map(|key| Value::String(key.clone())).collect(),
        _ => Vec::new(),
    }
}

fn line_ok(line: Option<&Value>) -> bool {
    match line {
        Some(Value::Number(number)) => number.is_i64() || number.is_u64(),
        Some(Value::String(text)) => {
            let stripped = py::strip(text);
            (!stripped.is_empty() && stripped.chars().all(char::is_numeric)) || PLACEHOLDER.is_match(text)
        }
        _ => false,
    }
}

/// One step's shape checked and normalized; errors are appended with the step's id.
pub fn normalize_step(raw: &Value, errors: &mut Vec<String>, inside: Option<&str>) -> Option<Step> {
    let Some(raw) = raw.as_object() else {
        errors.push("a step is not an object".into());
        return None;
    };
    let sid = raw.get("id").and_then(Value::as_str);
    let label = sid.map_or_else(
        || "a step without id".to_string(),
        |sid| format!("step {}", py::repr(sid)),
    );
    let Some(sid) = sid.filter(|sid| ID.is_match(sid)) else {
        errors.push(format!("{label}: id must match {ID_PATTERN}"));
        return None;
    };
    let op = match raw.get("op") {
        Some(Value::String(op)) if OPS.contains(&op.as_str()) => op.clone(),
        other => {
            let shown = py::repr_value(other.unwrap_or(&Value::Null));
            errors.push(format!("{label}: unknown op {shown}; use one of {}", OPS.join(", ")));
            return None;
        }
    };
    let mut step = Step::new();
    step.insert("id".into(), sid.into());
    step.insert("op".into(), op.clone().into());
    step.insert("needs".into(), Value::Array(ids(raw.get("needs"))));
    step.insert("covers".into(), Value::Array(ids(raw.get("covers"))));
    let (when, problem) = when_of(raw.get("when"));
    if let Some(problem) = problem {
        errors.push(format!("{label}: {problem}"));
    }
    step.insert("when".into(), when.unwrap_or(Value::Null));
    let get = |key: &str| raw.get(key).filter(|value| !value.is_null());
    if is_lookup_or_test(&op) {
        let Some(Value::Object(args)) = raw.get("args") else {
            errors.push(format!("{label}: {op} takes an args object"));
            return None;
        };
        let needed: &[&str] = match op.as_str() {
            "read" | "history" | "blame_range" => &["path"],
            "grep" => &["pattern"],
            "list" => &["dir"],
            "definition" | "references" => &["symbol"],
            "pickaxe" => &["string"],
            "run_tests" => &["paths"],
            _ => &["path", "line", "replacement", "tests"],
        };
        for key in needed {
            if !args.contains_key(*key) {
                errors.push(format!("{label}: {op} needs args.{key}"));
            }
        }
        step.insert("args".into(), Value::Object(args.clone()));
    } else if op == "for_each" {
        if inside.is_some() {
            errors.push(format!("{label}: for_each cannot be nested"));
            return None;
        }
        if !raw.get("over").is_some_and(Value::is_string) {
            errors.push(format!(
                "{label}: for_each needs over (the id of a grep, references, list or history step)"
            ));
        }
        step.insert("over".into(), raw.get("over").cloned().unwrap_or(Value::Null));
        let max_items = if truthy(raw.get("max_items")) {
            int_of(raw.get("max_items")).unwrap_or(DEFAULT_FOR_EACH_ITEMS)
        } else {
            DEFAULT_FOR_EACH_ITEMS
        };
        step.insert("max_items".into(), max_items.clamp(1, MAX_FOR_EACH_ITEMS).into());
        let template = match raw.get("steps") {
            Some(Value::Array(items)) if (1..=MAX_TEMPLATE_STEPS).contains(&items.len()) => items.clone(),
            _ => {
                errors.push(format!("{label}: for_each takes 1 to {MAX_TEMPLATE_STEPS} sub-steps"));
                Vec::new()
            }
        };
        let subs: Vec<Value> = template
            .iter()
            .filter_map(|sub| normalize_step(sub, errors, Some(sid)))
            .map(Value::Object)
            .collect();
        step.insert("steps".into(), Value::Array(subs));
    } else if op == "ask" {
        let question = raw.get("question").and_then(Value::as_str);
        match question {
            Some(question) if !py::strip(question).is_empty() => {
                if question.chars().count() > MAX_QUESTION_CHARS {
                    errors.push(format!(
                        "{label}: the question is longer than {MAX_QUESTION_CHARS} characters"
                    ));
                }
            }
            _ => errors.push(format!("{label}: ask needs a question")),
        }
        step.insert("question".into(), question.unwrap_or("").into());
        let context = ids(raw.get("context"));
        if context.is_empty() {
            errors.push(format!(
                "{label}: ask needs context: the ids of the steps whose results answer it"
            ));
        }
        step.insert("context".into(), Value::Array(context));
        let material: Vec<Value> = if truthy(raw.get("material")) {
            raw.get("material").map(iterate).unwrap_or_default()
        } else {
            Vec::new()
        };
        let material: Vec<Value> = material
            .iter()
            .filter_map(Value::as_str)
            .filter(|item| !py::strip(item).is_empty())
            .map(|item| Value::String(py::clip(item, ASK_MATERIAL_CHARS)))
            .take(MAX_ASK_MATERIAL)
            .collect();
        step.insert("material".into(), Value::Array(material));
    } else if op == "assert" {
        let forms = ["step", "all", "any"].iter().filter(|key| get(key).is_some()).count();
        if forms != 1 {
            errors.push(format!(
                "{label}: assert takes exactly one of step (with predicate), all, any"
            ));
        }
        if let Some(target) = get("step") {
            if !target.is_string() {
                errors.push(format!("{label}: assert.step must be an id"));
            }
            let parsed = parse_predicate(raw.get("predicate"));
            if parsed.is_none() {
                let shown = py::repr_value(raw.get("predicate").unwrap_or(&Value::Null));
                errors.push(format!(
                    "{label}: predicate {shown} is not one of count ==|!=|>=|<=|>|< N, status == \
                     passed|failed|could_not_run, answer == yes|no|unclear, contains <text>, not contains <text>"
                ));
            }
            step.insert("step".into(), target.clone());
            let predicate = raw.get("predicate").and_then(Value::as_str).unwrap_or("");
            step.insert("predicate".into(), predicate.into());
            step.insert(
                "predicate_parsed".into(),
                parsed.map_or(Value::Null, |parsed| json!(parsed)),
            );
        }
        for key in ["all", "any"] {
            if get(key).is_some() {
                let list = ids(raw.get(key));
                if list.is_empty() {
                    errors.push(format!("{label}: assert.{key} must list ids"));
                }
                step.insert(key.into(), Value::Array(list));
            }
        }
        let expect = raw.get("expect").filter(|value| value.is_boolean());
        if expect.is_none() {
            errors.push(format!(
                "{label}: assert needs expect: true or false, the value you believe it will have"
            ));
        }
        step.insert("expect".into(), expect.cloned().unwrap_or(Value::Null));
        let holds = raw.get("holds").and_then(Value::as_str).unwrap_or("");
        step.insert("holds".into(), holds.into());
    } else if op == "finding" {
        let (when, _) = when_of(raw.get("when"));
        if when.is_none() {
            errors.push(format!("{label}: a finding needs when, naming an assert or an ask"));
        }
        step.insert("when".into(), when.unwrap_or(Value::Null));
        for key in ["file", "claim"] {
            if !raw
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|text| !py::strip(text).is_empty())
            {
                errors.push(format!("{label}: finding needs {key}"));
            }
        }
        if !line_ok(raw.get("line")) {
            errors.push(format!("{label}: finding needs line (a number or a placeholder)"));
        }
        let level = match raw.get("level") {
            None => String::new(),
            Some(value) => py::fstr(Some(value)),
        };
        let level = py::strip(&level).to_lowercase();
        if !LEVELS.contains(&level.as_str()) && !OLD_LEVELS.iter().any(|(old, _)| *old == level) {
            errors.push(format!("{label}: level must be one of {}", LEVELS.join(", ")));
        }
        let evidence = ids(raw.get("evidence"));
        let citations: Vec<Value> = match raw.get("citations") {
            Some(Value::Array(items)) => items.iter().filter(|item| item.is_object()).cloned().collect(),
            _ => Vec::new(),
        };
        if evidence.is_empty() && citations.is_empty() {
            errors.push(format!("{label}: finding needs evidence (step ids) or citations"));
        }
        step.insert("evidence".into(), Value::Array(evidence));
        step.insert("citations".into(), Value::Array(citations));
        for key in [
            "file",
            "line",
            "level",
            "category",
            "claim",
            "why",
            "fix",
            "scenario",
            "unpinned",
            "consequence",
        ] {
            step.insert(key.into(), raw.get(key).cloned().unwrap_or(Value::Null));
        }
    }
    Some(step)
}

/// Every step id a step depends on (its needs and the ids its op reads), first occurrence first.
pub fn references(step: &Step) -> Vec<String> {
    let mut refs = strings(step, "needs");
    if let Some((target, _)) = when_step(step) {
        refs.push(target.to_string());
    }
    for key in ["over", "step"] {
        if let Some(Value::String(target)) = step.get(key) {
            refs.push(target.clone());
        }
    }
    for key in ["context", "all", "any", "evidence"] {
        refs.extend(strings(step, key));
    }
    let mut seen = HashSet::new();
    refs.retain(|item| seen.insert(item.clone()));
    refs
}

/// The steps an assert reads directly: its `step`, then `all` and `any`.
pub(crate) fn assert_refs(step: &Step) -> Vec<String> {
    let mut refs: Vec<String> = step
        .get("step")
        .and_then(Value::as_str)
        .map(str::to_string)
        .into_iter()
        .collect();
    refs.extend(strings(step, "all"));
    refs.extend(strings(step, "any"));
    refs
}

/// The non-assert steps an assert (or ask) ultimately reads, through all/any.
pub fn assert_leaves<'a>(sid: &str, by_id: &'a IndexMap<String, Step>) -> Vec<&'a Step> {
    fn walk<'a>(sid: &str, by_id: &'a IndexMap<String, Step>, seen: &mut HashSet<String>, out: &mut Vec<&'a Step>) {
        let Some(step) = by_id.get(sid) else { return };
        if !seen.insert(sid.to_string()) {
            return;
        }
        if op_of(step) != "assert" {
            out.push(step);
            return;
        }
        for target in assert_refs(step) {
            walk(&target, by_id, seen, out);
        }
    }
    let mut out = Vec::new();
    walk(sid, by_id, &mut HashSet::new(), &mut out);
    out
}

/// Whether a finding rests on something that can establish it: an ask (a semantic judgement), a test run, or an
/// exact-count presence check (a `count_only` grep). A capped grep or a read alone establishes nothing.
pub fn grounded(step: &Step, by_id: &IndexMap<String, Step>) -> bool {
    let Some((gate, _)) = when_step(step).filter(|(gate, _)| by_id.contains_key(*gate)) else {
        return true; // reported elsewhere
    };
    let leaves = assert_leaves(gate, by_id);
    if leaves.iter().any(|leaf| op_of(leaf) == "ask") {
        return true;
    }
    let decisive = |op: &str| TEST_OPS.contains(&op) || op == "ask";
    if strings(step, "evidence")
        .iter()
        .any(|target| by_id.get(target).is_some_and(|found| decisive(op_of(found))))
    {
        return true;
    }
    !leaves.is_empty()
        && leaves.iter().all(|leaf| {
            TEST_OPS.contains(&op_of(leaf))
                || (op_of(leaf) == "grep"
                    && leaf.get("args").and_then(|args| args.get("count_only")) == Some(&Value::Bool(true)))
        })
}

/// One thing a program must cover: a symbol's references (`S`), a key's family (`K`), a claim (`C`) or a
/// catalogue shape the change triggers (`T`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageItem {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub text: String,
}

/// What the host's map knows about the change (the parts of `autoreview_deep`'s brief the compiled mode reads).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Brief {
    #[serde(default)]
    pub extracted: Map<String, Value>,
    #[serde(default)]
    pub required: Map<String, Value>,
    /// (source, text) of each claim of the change.
    #[serde(default)]
    pub claim_list: Vec<(Value, Value)>,
    /// The structural index's facts (`brief.reference.facts()`).
    #[serde(default)]
    pub facts: Vec<Value>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tests: Vec<String>,
}

pub(crate) fn list<'a>(map: &'a Map<String, Value>, key: &str) -> &'a [Value] {
    match map.get(key) {
        Some(Value::Array(items)) => items,
        _ => &[],
    }
}

static EFFECT: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)execute\(|\bDELETE\b|\bINSERT\b|\bUPDATE\b|\bDROP\b|\brm\b|\bmv\b|unlink|\.write\(|commit\(|\bsend|requests\.|fetch\(|subprocess|os\.system|shutil\.",
    )
});
static GUARD: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)validate|check|assert|verify|sanitiz|raise ValueError|throw new"));
static ERROR_PATH: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)\b(except|catch|raise|throw|finally|rollback|on_error|errexit)\b|set -e"));
static RETRY: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)\b(retry|retries|attempt|timeout|poll|backoff|sleep)\b"));
static WORKFLOW: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)(^|/)\.github/workflows/|(^|/)\.gitlab-ci|(^|/)ci/|Jenkinsfile|\.circleci/"));
static MANIFEST: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)(^|/)(k8s|helm|charts|deploy|infra|terraform|manifests?)/|\.tf\n?\z|docker-compose|(^|/)Dockerfile|\.(ya?ml)\n?\z",
    )
});
/// `autoreview_deep._INPUT_HINT`.
static INPUT_HINT: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"(?i)\bre\.|regex|RegExp|\.match\(|\.test\(|\.search\(|parse|split\(|strip\(|startswith|endswith|validate|sanitiz|escape|limit|max_|min_|len\(|\.length|truncate|slice\(|\bint\(|float\(|isinstance|typeof|\bif .*(<|>|<=|>=|==|!=)",
    )
});
static LIST_MEMBER: LazyLock<Regex> = LazyLock::new(|| regex(r#"(?m)^\s*['"][\w.-]+['"],?\s*$"#));

fn take_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// The catalogue shapes whose trigger the map detects in this change, as coverage items (`T<n>`): the planner
/// must run that shape's check or declare it uncovered.
pub fn shape_items(brief: &Brief, files: &[FileDiff]) -> Vec<CoverageItem> {
    let extracted = &brief.extracted;
    let reviewable: Vec<&FileDiff> = files.iter().filter(|item| skip_reason(item).is_none()).collect();
    let code: Vec<&FileDiff> = reviewable
        .iter()
        .copied()
        .filter(|item| file_kind(&item.path) == "code")
        .collect();
    let tests: Vec<&FileDiff> = reviewable
        .iter()
        .copied()
        .filter(|item| file_kind(&item.path) == "test")
        .collect();
    let lines_of = |kind: char| {
        reviewable
            .iter()
            .flat_map(|item| &item.hunks)
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.kind == kind)
            .map(|line| line.text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (added, removed) = (lines_of('+'), lines_of('-'));
    let paths: Vec<&str> = files.iter().map(|item| item.path.as_str()).collect();
    let mut triggered: IndexMap<&str, String> = IndexMap::new();
    let family: Vec<&Value> = brief
        .facts
        .iter()
        .filter(|fact| fact.get("kind").and_then(Value::as_str) == Some("family"))
        .collect();
    let named = if truthy(extracted.get("fields")) {
        list(extracted, "fields")
    } else {
        list(extracted, "constants")
    };
    if !named.is_empty() || !family.is_empty() || LIST_MEMBER.is_match(&added) {
        let names: Vec<String> = named
            .iter()
            .take(4)
            .map(|name| format!("`{}`", py::fstr(Some(name))))
            .collect();
        let families: Vec<String> = family.iter().take(3).map(|fact| py::fstr(fact.get("id"))).collect();
        let text = if !names.is_empty() {
            names.join(", ")
        } else if !families.is_empty() {
            families.join("; ")
        } else {
            "a new list member".to_string()
        };
        triggered.insert("registry-member", text);
    }
    let real_code: Vec<&str> = code
        .iter()
        .map(|item| item.path.as_str())
        .filter(|path| is_code(path))
        .collect();
    if !real_code.is_empty() {
        triggered.insert(
            "unpinned-behaviour",
            real_code.iter().take(3).copied().collect::<Vec<_>>().join(", "),
        );
    }
    if !tests.is_empty() {
        let shown: Vec<&str> = tests.iter().take(3).map(|item| item.path.as_str()).collect();
        triggered.insert("test-asserts-behaviour", shown.join(", "));
    }
    if ["env", "flags", "config"].iter().any(|key| truthy(extracted.get(*key))) {
        let names: Vec<String> = list(extracted, "env")
            .iter()
            .chain(list(extracted, "flags"))
            .chain(list(extracted, "config").iter().take(4))
            .map(|name| format!("`{}`", py::fstr(Some(name))))
            .collect();
        triggered.insert("env-in-deploy", names.join(", "));
    }
    let workflows: Vec<&str> = paths.iter().copied().filter(|path| WORKFLOW.is_match(path)).collect();
    if !workflows.is_empty() {
        triggered.insert("workflow-siblings", take_chars(&workflows.join(", "), 200));
    }
    let manifests: Vec<&str> = paths
        .iter()
        .copied()
        .filter(|path| MANIFEST.is_match(path) && !WORKFLOW.is_match(path))
        .collect();
    if !manifests.is_empty() {
        triggered.insert("manifest-reference", take_chars(&manifests.join(", "), 200));
    }
    if !code.is_empty() && INPUT_HINT.is_match(&added) {
        triggered.insert(
            "input-defeats-guard",
            "the new guard, pattern, limit or parser in the diff".into(),
        );
    }
    if !code.is_empty() && GUARD.is_match(&added) && EFFECT.is_match(&format!("{added}\n{removed}")) {
        triggered.insert(
            "guard-after-effect",
            "the new check beside a side-effecting operation".into(),
        );
    }
    if ERROR_PATH.is_match(&added) || ERROR_PATH.is_match(&removed) {
        triggered.insert("error-path", "the changed except/catch/raise/rollback lines".into());
    }
    let claims = list(extracted, "claims");
    if !claims.is_empty() || reviewable.iter().any(|item| file_kind(&item.path) == "doc") {
        let shown: Vec<String> = claims
            .iter()
            .take(3)
            .map(|claim| py::clip(&py::str_of(claim.get(2).unwrap_or(&Value::Null)), 80))
            .collect();
        let text = if shown.is_empty() {
            "the changed documents".to_string()
        } else {
            shown.join("; ")
        };
        triggered.insert("comment-vs-code", text);
    }
    if brief.text.contains("same name elsewhere:") {
        triggered.insert("sibling-implementation", "the twin file(s) the brief names".into());
    }
    if !code.is_empty() && RETRY.is_match(&added) {
        triggered.insert("failure-retry", "the new retry, timeout or polling code".into());
    }
    TRIGGERED_SHAPES
        .iter()
        .filter_map(|key| Some((*key, triggered.get(key)?)))
        .filter_map(|(key, why)| Some((CHECK_CATALOGUE.iter().find(|shape| shape.key == key)?, why)))
        .enumerate()
        .map(|(index, (shape, why))| CoverageItem {
            id: format!("T{}", index + 1),
            kind: "shape".into(),
            name: shape.key.into(),
            text: format!(
                "catalogue shape {} ({why}): when {}; how: {}",
                shape.key, shape.when, shape.how
            ),
        })
        .collect()
}

/// What a program must cover, from the map: the references of each changed signature or exported name (`S`), the
/// siblings and consumers of each new config key, field, flag or environment variable (`K`), each claim of the
/// change (`C`), and with `files` the catalogue shapes the change triggers (`T`).
pub fn coverage_items(brief: &Brief, files: Option<&[FileDiff]>) -> Vec<CoverageItem> {
    let mut items = Vec::new();
    for (index, name) in list(&brief.required, "siblings").iter().enumerate() {
        let name = py::fstr(Some(name));
        items.push(CoverageItem {
            id: format!("S{}", index + 1),
            kind: "symbol".into(),
            text: format!("the references of `{name}` (changed signature or exported name): every caller still fits"),
            name,
        });
    }
    let mut keys: Vec<&Value> = Vec::new();
    for key in list(&brief.required, "claims")
        .iter()
        .chain(list(&brief.extracted, "fields"))
    {
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    for (index, name) in keys.into_iter().take(8).enumerate() {
        let name = py::fstr(Some(name));
        items.push(CoverageItem {
            id: format!("K{}", index + 1),
            kind: "key".into(),
            text: format!(
                "the siblings and consumers of the new key, field, flag or variable `{name}`: the family it joins \
                 (registry lists, sibling declarations) and every reader"
            ),
            name,
        });
    }
    for (index, (source, claim)) in brief.claim_list.iter().take(8).enumerate() {
        items.push(CoverageItem {
            id: format!("C{}", index + 1),
            kind: "claim".into(),
            name: String::new(),
            text: format!("the claim [{}] {}", py::fstr(Some(source)), py::fstr(Some(claim))),
        });
    }
    if let Some(files) = files {
        items.extend(shape_items(brief, files));
    }
    items
}

/// The text of a step a coverage name may appear in: its args, question, predicate, holds, claim and why.
fn step_texts(step: &Step) -> String {
    let args = step
        .get("args")
        .filter(|args| truthy(Some(args)))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let text_or_empty = |key: &str| match step.get(key) {
        Some(Value::String(text)) => Some(text.clone()),
        value if !truthy(value) => Some(String::new()),
        _ => None,
    };
    let str_or_empty = |key: &str| {
        let value = step.get(key);
        if truthy(value) { py::fstr(value) } else { String::new() }
    };
    let parts = [
        Some(dumps(&args)),
        text_or_empty("question"),
        text_or_empty("predicate"),
        text_or_empty("holds"),
        Some(str_or_empty("claim")),
        Some(str_or_empty("why")),
    ];
    parts.into_iter().flatten().collect::<Vec<_>>().join("\n")
}

/// Whether `name` occurs in `text` with no ASCII word character on either side.
fn names(text: &str, name: &str) -> bool {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
    let mut start = 0;
    while let Some(offset) = text[start..].find(name) {
        let at = start + offset;
        if !word(text[..at].chars().next_back()) && !word(text[at + name.len()..].chars().next()) {
            return true;
        }
        start = at + text[at..].chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// The id a declared `uncovered` entry names: the text before its first colon.
fn declared_id(item: &str) -> &str {
    py::strip(item.split(':').next().unwrap_or(""))
}

/// The coverage items no step covers and the planner did not declare uncovered. A step covers an item it names in
/// `covers`; a symbol or key is also covered by a step whose text names it.
pub fn uncovered_items(coverage: &[CoverageItem], steps: &[&Step], declared: &[String]) -> Vec<CoverageItem> {
    let declared_ids: HashSet<&str> = declared.iter().map(|item| declared_id(item)).collect();
    let mut named: HashSet<String> = HashSet::new();
    let mut texts = Vec::new();
    for step in steps {
        if let Some(Value::Array(covers)) = step.get("covers") {
            named.extend(covers.iter().map(|item| py::fstr(Some(item))));
        }
        texts.push(step_texts(step));
    }
    let text = texts.join("\n");
    coverage
        .iter()
        .filter(|item| !declared_ids.contains(item.id.as_str()) && !named.contains(&item.id))
        .filter(|item| {
            !(matches!(item.kind.as_str(), "symbol" | "key") && !item.name.is_empty() && names(&text, &item.name))
        })
        .cloned()
        .collect()
}

/// The reference rules of one step against the steps it may name.
fn check_step(step: &Step, by_id: &IndexMap<String, Step>, scope: &HashSet<String>, errors: &mut Vec<String>) {
    let sid = py::repr(id_of(step));
    let op_at = |target: &str| by_id.get(target).map(op_of);
    for target in references(step) {
        if !by_id.contains_key(&target) {
            errors.push(format!("step {sid}: names unknown step {}", py::repr(&target)));
        } else if !scope.contains(&target) {
            errors.push(format!(
                "step {sid}: cannot name {}, a sub-step of another for_each",
                py::repr(&target)
            ));
        }
    }
    let op = op_of(step);
    if op == "ask" {
        for target in strings(step, "context") {
            if let Some(found) = op_at(&target).filter(|found| !is_lookup_or_test(found)) {
                errors.push(format!(
                    "step {sid}: context must name lookups or test steps, not {} {}",
                    py::repr(found),
                    py::repr(&target)
                ));
            }
        }
    }
    if op == "assert" {
        if let Some(target) = step.get("step").and_then(Value::as_str)
            && let Some(found) = op_at(target).filter(|found| matches!(*found, "assert" | "finding" | "for_each"))
        {
            errors.push(format!(
                "step {sid}: a predicate applies to a lookup, test or ask, not to {} {}",
                py::repr(found),
                py::repr(target)
            ));
        }
        for target in strings(step, "all").into_iter().chain(strings(step, "any")) {
            if let Some(found) = op_at(&target).filter(|found| !matches!(*found, "assert" | "ask")) {
                errors.push(format!(
                    "step {sid}: all/any list asserts and asks, not {} {}",
                    py::repr(found),
                    py::repr(&target)
                ));
            }
        }
    }
    if let Some((gate, _)) = when_step(step)
        && let Some(found) = op_at(gate).filter(|found| !matches!(*found, "assert" | "ask"))
    {
        errors.push(format!(
            "step {sid}: when must name an assert or an ask, not {}",
            py::repr(found)
        ));
    }
    if op == "for_each"
        && let Some(found) = step
            .get("over")
            .and_then(Value::as_str)
            .and_then(op_at)
            .filter(|found| !ITERABLE.contains(found))
    {
        errors.push(format!(
            "step {sid}: for_each iterates a grep, references, list or history result, not {}",
            py::repr(found)
        ));
    }
    if op == "finding" {
        for target in strings(step, "evidence") {
            if let Some(found) = op_at(&target).filter(|found| !is_lookup_or_test(found) && *found != "ask") {
                errors.push(format!(
                    "step {sid}: evidence names lookups, tests or asks, not {} {}",
                    py::repr(found),
                    py::repr(&target)
                ));
            }
        }
        if !grounded(step, by_id) {
            errors.push(format!(
                "step {sid}: a finding must rest on an ask (for code semantics), a test run, or an exact-count \
                 presence check (a grep with \"count_only\": true); a capped grep count or a read alone does not \
                 establish it"
            ));
        }
    }
}

fn sub_steps(step: &Step) -> Vec<&Step> {
    match step.get("steps") {
        Some(Value::Array(items)) => items.iter().filter_map(Value::as_object).collect(),
        _ => Vec::new(),
    }
}

/// The program `raw` (the planner's reply) checked against the language and, with `coverage`, against what the
/// map says it must cover: the program, or the errors (deduplicated, at most 40).
pub fn validate(raw: &Value, coverage: Option<&[CoverageItem]>, max_planned: usize) -> Result<Program, Vec<String>> {
    let Some(raw_steps) = raw.get("steps").and_then(Value::as_array) else {
        return Err(vec!["the program must be an object with a steps array".into()]);
    };
    if raw_steps.is_empty() {
        return Err(vec!["the program has no steps".into()]);
    }
    if raw_steps.len() > max_planned {
        return Err(vec![format!(
            "the program has {} steps; at most {max_planned} as written",
            raw_steps.len()
        )]);
    }
    let declared: Vec<String> = match raw.get("uncovered") {
        Some(Value::Array(items)) => items
            .iter()
            .filter(|item| item.is_string())
            .map(|item| text(Some(item), 200))
            .collect(),
        _ => Vec::new(),
    };
    let mut errors = Vec::new();
    let steps: Vec<Step> = raw_steps
        .iter()
        .filter_map(|item| normalize_step(item, &mut errors, None))
        .collect();
    let mut by_id: IndexMap<String, Step> = IndexMap::new();
    let mut owner: IndexMap<String, String> = IndexMap::new();
    for step in &steps {
        let sid = id_of(step).to_string();
        if by_id.contains_key(&sid) {
            errors.push(format!("step {}: duplicate id", py::repr(&sid)));
        }
        by_id.insert(sid.clone(), step.clone());
        for sub in sub_steps(step) {
            let sub_id = id_of(sub).to_string();
            if by_id.contains_key(&sub_id) {
                errors.push(format!(
                    "step {}: duplicate id (sub-step ids must be unique in the program too)",
                    py::repr(&sub_id)
                ));
            }
            by_id.insert(sub_id, sub.clone());
        }
    }
    for step in &steps {
        for sub in sub_steps(step) {
            owner.insert(id_of(sub).to_string(), id_of(step).to_string());
        }
    }
    let top: HashSet<String> = steps.iter().map(|step| id_of(step).to_string()).collect();
    for step in &steps {
        check_step(step, &by_id, &top, &mut errors);
        let subs = sub_steps(step);
        let mut scope = top.clone();
        scope.extend(subs.iter().map(|sub| id_of(sub).to_string()));
        for sub in subs {
            check_step(sub, &by_id, &scope, &mut errors);
            if op_of(sub) == "for_each" {
                errors.push(format!("step {}: for_each cannot be nested", py::repr(id_of(sub))));
            }
        }
    }
    // No cycles: every reference must resolve to an earlier step in a topological order. A for_each finishes
    // after its sub-steps, so it depends on them.
    let mut graph: IndexMap<String, Vec<String>> = by_id
        .iter()
        .map(|(sid, step)| {
            let refs = references(step)
                .into_iter()
                .filter(|target| by_id.contains_key(target))
                .collect();
            (sid.clone(), refs)
        })
        .collect();
    for (sid, parent) in &owner {
        if let Some(edges) = graph.get_mut(parent) {
            edges.push(sid.clone());
        }
    }
    let mut state: IndexMap<String, u8> = IndexMap::new();
    fn visit(
        node: &str,
        path: &mut Vec<String>,
        graph: &IndexMap<String, Vec<String>>,
        state: &mut IndexMap<String, u8>,
        errors: &mut Vec<String>,
    ) {
        match state.get(node) {
            Some(2) => return,
            Some(1) => {
                let start = path.iter().position(|item| item == node).unwrap_or(0);
                let mut cycle = path[start..].to_vec();
                cycle.push(node.to_string());
                errors.push(format!("the dependencies form a cycle: {}", cycle.join(" -> ")));
                return;
            }
            _ => {}
        }
        state.insert(node.to_string(), 1);
        path.push(node.to_string());
        for target in graph.get(node).map(Vec::as_slice).unwrap_or(&[]) {
            visit(target, path, graph, state, errors);
        }
        path.pop();
        state.insert(node.to_string(), 2);
    }
    for sid in by_id.keys() {
        visit(sid, &mut Vec::new(), &graph, &mut state, &mut errors);
    }
    let asks = by_id.values().filter(|step| op_of(step) == "ask").count();
    if asks > MAX_ASKS {
        errors.push(format!("{asks} ask steps; at most {MAX_ASKS}"));
    }
    let findings = by_id.values().filter(|step| op_of(step) == "finding").count();
    if findings > MAX_FINDINGS {
        errors.push(format!("{findings} finding steps; at most {MAX_FINDINGS}"));
    }
    if let Some(coverage) = coverage.filter(|coverage| !coverage.is_empty()) {
        let all: Vec<&Step> = steps.iter().chain(steps.iter().flat_map(sub_steps)).collect();
        for item in uncovered_items(coverage, &all, &declared) {
            errors.push(format!(
                "coverage: {} ({}) has no check: add a step that checks it and name {} in its \"covers\", or list \
                 it in the program's \"uncovered\" with why",
                item.id, item.text, item.id
            ));
        }
        let known: HashSet<&str> = coverage.iter().map(|item| item.id.as_str()).collect();
        for item in &declared {
            let named = declared_id(item);
            if !known.contains(named) {
                errors.push(format!(
                    "uncovered names {}, which is not a coverage item",
                    py::repr(named)
                ));
            }
        }
    }
    if !errors.is_empty() {
        let mut seen = HashSet::new();
        errors.retain(|item| seen.insert(item.clone()));
        errors.truncate(40);
        return Err(errors);
    }
    Ok(Program {
        steps,
        summary: text(raw.get("summary"), 300),
        uncovered: declared,
    })
}

/// Python's `json.dumps(value)`: `", "` and `": "` separators, non-ASCII escaped, floats as Python writes them.
pub fn dumps(value: &Value) -> String {
    let mut out = String::new();
    write_json(value, &mut out);
    out
}

fn write_json(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => match number.as_f64() {
            Some(float) if number.is_f64() => out.push_str(&py::float_repr(float)),
            _ => out.push_str(&number.to_string()),
        },
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_string(key, out);
                out.push_str(": ");
                write_json(item, out);
            }
            out.push('}');
        }
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}
