//! Reading step results and deciding checks (`autoreview_compiled.py` `_count_of`, `_items_of`, `_render`,
//! `_count_predicate`, and the parts of `Interpreter` that work over recorded results: placeholders, truth,
//! conditions, asserts, finding gates, quote checks, `for_each` expansion, step records, `_public_value`).
//! Results are the JSON objects the Python kept per step (`op`, `status`, `text`, `count`, `items`, `capped`,
//! `truncated`, `outcome`, `answer`, ...). A check is true, false or unknown (`None`): unknown when the step it
//! reads did not run or could not run, when a capped count does not settle it, or when text is missing from a
//! truncated result.
//!
//! Lookups and test steps run here (`_lookup`: the deep pass's [`serve_request`] over the head commit and the
//! review's [`review_tests::TestSession`]), and so does a program's run ([`Interpreter::run`] /
//! [`Interpreter::round`], `Interpreter.run`): rounds of ready steps, conditions, `for_each` expansion, asserts and
//! finding gates. Asks and the small-model resolution of undecided findings stay with the caller (frames are the
//! kernel's), and so does composing an emitted finding: each round hands them back ([`Round`], [`Caller`]).
//!
//! Known differences from the Python, outside recorded behaviour: a hit line number too large for `i64` drops the
//! hit (Python kept a big integer); a placeholder whose value is not a string is rendered with `str()` (Python
//! raised in `re.sub`).

use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::LazyLock;

use indexmap::{IndexMap, IndexSet};
use regex::Regex;
use serde_json::{Map, Value, json};

use crate::compiled::{
    ANSWERS, CAPPED, LOOKUPS, PLACEHOLDER, RECORD_CHARS, RESULT_CHARS, Step, TEST_OPS, assert_leaves, assert_refs,
    decimal, dumps, flat, id_of, op_of, parsed_of, references, strings, when_step,
};
use crate::deep_repo::{Rejected, Repo, TestSession, serve_request};
use crate::levels::truthy;
use crate::py;
use crate::review_tests;

pub type Results = IndexMap<String, Map<String, Value>>;

pub const CELL_VALUE_CHARS: usize = 4_000;
pub const CELL_VALUE_ITEMS: usize = 20;

static HIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\A(.+?):(\d+): ?(.*)\z").expect("pattern"));
static TITLE_COUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-> (\d+) (?:matches|commits)|\((\d+) entries\)").expect("pattern"));

/// The count a lookup's title states (`-> N matches`, `-> N commits`, `(N entries)`); a read counts its lines.
pub fn count_of(op: &str, title: &str, body: &str) -> Option<i64> {
    if let Some(found) = TITLE_COUNT.captures(title) {
        return found
            .get(1)
            .or_else(|| found.get(2))
            .and_then(|digits| decimal(digits.as_str()));
    }
    (op == "read").then(|| py::splitlines(body).len() as i64)
}

/// The items of a lookup's body a `for_each` iterates: `{path, line, text}` hits for grep, references and
/// definition; the non-empty lines (no `...` notes, no "nothing found" sentinels) for the line-shaped lookups.
pub fn items_of(op: &str, body: &str) -> Vec<Value> {
    match op {
        "grep" | "references" | "definition" => py::splitlines(body)
            .into_iter()
            .filter_map(|line| {
                let found = HIT.captures(line)?;
                let number = decimal(&found[2]).filter(|number| *number < i64::MAX)?;
                Some(json!({ "path": &found[1], "line": number, "text": &found[3] }))
            })
            .collect(),
        "list" | "history" | "blame_range" | "pickaxe" => py::splitlines(body)
            .into_iter()
            .filter(|line| {
                !py::strip(line).is_empty()
                    && !line.starts_with("...")
                    && !matches!(
                        *line,
                        "0 matches"
                            | "no commits found"
                            | "no commit added or removed it"
                            | "no history available for these lines"
                    )
            })
            .map(|line| Value::String(line.to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

/// A value as a placeholder shows it: `path:line` for a hit, JSON for other objects, `str()` otherwise.
pub fn render(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => String::new(),
        Some(Value::Object(map)) if map.contains_key("path") && map.contains_key("line") => {
            format!("{}:{}", py::fstr(map.get("path")), py::fstr(map.get("line")))
        }
        Some(value @ Value::Object(_)) => dumps(value),
        Some(value) => py::str_of(value),
    }
}

/// A count predicate over an exact count, or over a lower bound (a count cut at its cap): the predicates the
/// bound settles are decided, the rest are unknown.
pub fn count_predicate(count: i64, capped: bool, op: &str, n: i64) -> (Option<bool>, String) {
    if !capped {
        let value = match op {
            "==" => count == n,
            "!=" => count != n,
            ">=" => count >= n,
            "<=" => count <= n,
            ">" => count > n,
            _ => count < n,
        };
        return (Some(value), format!("count is {count}"));
    }
    let settled = match op {
        "==" => (n < count).then_some(false),
        "!=" => (n < count).then_some(true),
        ">=" => (count >= n).then_some(true),
        ">" => (count > n).then_some(true),
        "<" => (count >= n).then_some(false),
        "<=" => (count > n).then_some(false),
        _ => None,
    };
    (settled, format!("count is at least {count} (cut at its cap)"))
}

fn python_bool(value: Option<bool>) -> &'static str {
    match value {
        None => "?",
        Some(true) => "True",
        Some(false) => "False",
    }
}

/// A step's one-line summary, as records, materials and `{{id}}` placeholders show it.
pub fn summary(op: &str, result: &Map<String, Value>) -> String {
    let text_of = |key: &str| result.get(key).map(py::str_of).unwrap_or_default();
    if TEST_OPS.contains(&op) {
        return format!("{}: {}", text_of("title"), text_of("outcome"));
    }
    match op {
        "ask" => format!(
            "answer {} (quote: {})",
            py::fstr(result.get("answer")),
            py::clip(&text_of("quote"), 160)
        ),
        "assert" => {
            let held = truthy(result.get("value"));
            let mut out = String::from(if held {
                "holds"
            } else if result.get("value").is_none_or(Value::is_null) {
                "unknown"
            } else {
                "does not hold"
            });
            if truthy(result.get("detail")) && !held {
                out.push_str(&format!(" ({})", text_of("detail")));
            }
            if truthy(result.get("contradicted")) {
                out.push_str("; contradicts the expectation");
            }
            out
        }
        "finding" => {
            if truthy(result.get("emitted")) {
                "finding emitted".into()
            } else if truthy(result.get("gate")) {
                text_of("gate")
            } else if truthy(result.get("detail")) {
                text_of("detail")
            } else {
                "no finding".into()
            }
        }
        _ => {
            let title = match result.get("title") {
                Some(title) => py::str_of(title),
                None => op.to_string(),
            };
            match result.get("items") {
                Some(Value::Array(items)) if !items.is_empty() => {
                    let shown: Vec<String> = items.iter().take(5).map(|item| render(Some(item))).collect();
                    format!(
                        "{title}: {}{}",
                        shown.join("; "),
                        if items.len() > 5 { " ..." } else { "" }
                    )
                }
                _ => title,
            }
        }
    }
}

/// (answer, quote, quote_ok, detail) of a small model's reply, the quote checked against the material: an answer
/// other than unclear whose quote is not in the material counts as unclear.
pub fn check_quote(reply: &Value, material: &str) -> (String, String, bool, String) {
    let Some(reply) = reply.as_object() else {
        return ("unclear".into(), String::new(), false, "no reply".into());
    };
    let answer = match reply.get("answer") {
        None => String::new(),
        value => py::fstr(value),
    };
    let answer = py::strip(&answer).to_lowercase();
    let quote = flat(reply.get("quote").and_then(Value::as_str).unwrap_or(""));
    let whole = flat(material);
    let quote_ok = quote.chars().count() >= 4
        && (whole.contains(&quote)
            || py::splitlines(material)
                .into_iter()
                .map(flat)
                .any(|line| line.chars().count() >= 8 && quote.contains(&line)));
    if !ANSWERS.contains(&answer.as_str()) {
        let detail = format!("the answer {} is not yes, no or unclear", py::repr(&answer));
        return ("unclear".into(), quote, quote_ok, detail);
    }
    if answer != "unclear" && !quote_ok {
        let detail = "the quote is not in the material: the answer counts as unclear".to_string();
        return ("unclear".into(), quote, quote_ok, detail);
    }
    (answer, quote, quote_ok, String::new())
}

/// What a planner cell gets back from an `rv` call: the step's result, bounded.
pub fn public_value(result: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("id".into(), result.get("id").cloned().unwrap_or(Value::Null));
    out.insert("status".into(), result.get("status").cloned().unwrap_or(Value::Null));
    for key in [
        "count",
        "capped",
        "truncated",
        "answer",
        "quote",
        "why",
        "value",
        "contradicted",
        "detail",
        "emitted",
        "caught",
        "title",
    ] {
        if let Some(value) = result.get(key).filter(|value| !value.is_null()) {
            out.insert(key.into(), value.clone());
        }
    }
    let op = result.get("op").and_then(Value::as_str).unwrap_or("");
    if TEST_OPS.contains(&op) {
        out.insert("status".into(), result.get("outcome").cloned().unwrap_or(Value::Null));
    }
    if let Some(Value::Array(items)) = result.get("items").filter(|items| truthy(Some(items))) {
        out.insert(
            "items".into(),
            Value::Array(items.iter().take(CELL_VALUE_ITEMS).cloned().collect()),
        );
    }
    if truthy(result.get("text")) {
        let text = result.get("text").map(py::str_of).unwrap_or_default();
        out.insert("text".into(), py::clip(&text, CELL_VALUE_CHARS).into());
    }
    if op == "finding" {
        let gate = if truthy(result.get("gate")) {
            result.get("gate").map(py::str_of).unwrap_or_default()
        } else if truthy(result.get("emitted")) {
            "finding emitted".into()
        } else {
            String::new()
        };
        out.insert("gate".into(), gate.into());
    }
    out
}

/// What a finding step's gate decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// The condition holds: the caller composes and emits the finding.
    Emit,
    /// The check is unknown or contradicted: the caller puts the finding to the small model, saying why.
    Resolve(&'static str),
    /// Nothing is emitted; the step's record says why.
    Settled,
}

/// The state of a program's run that the checks read: the steps by id (expanded instances included), their
/// results in finishing order, the step records, and the counters `stats()` reports.
pub struct Interpreter {
    pub steps_by_id: IndexMap<String, Step>,
    pub results: Results,
    pub records: Vec<Map<String, Value>>,
    /// Finding steps that emitted nothing, by why (`gateFalse`, `undecided`, `askedNo`, `askedUnclear`).
    pub not_emitted: IndexMap<&'static str, i64>,
    pub truncated: Vec<String>,
    pub expanded: usize,
    pub max_steps: usize,
    /// A finding whose test check could not run is left undecided for the caller's verifier.
    pub defer_unrunnable: bool,
    /// (runner, directory) pairs found unavailable (by the automatic run, or a test step of this program): their
    /// test steps are could_not_run at once.
    pub unavailable: IndexSet<(String, String)>,
    clock: Box<dyn Fn() -> f64>,
}

/// The caller's composing of a finding whose gate holds: (interpreter, step, scope, when its gate was read).
pub type Emit<'a> = dyn FnMut(&mut Interpreter, &Step, &Map<String, Value>, f64) + 'a;

/// The review's test session as the interpreter reads it.
pub type Session = RefCell<review_tests::TestSession>;

/// What a round of a program's run leaves to the caller: the ask steps that became ready (with their scopes), and
/// the finding steps whose check came out unknown or against the planner's expectation (with why), to be put to
/// the small model. The caller records a result for each before [`Interpreter::end_round`].
#[derive(Debug, Clone, Default)]
pub struct Round {
    pub asks: Vec<(Step, Map<String, Value>)>,
    pub deferred: Vec<(Step, Map<String, Value>, &'static str)>,
}

/// A program's run in progress: the steps not finished, and the round's ready steps and new instances.
#[derive(Debug, Clone, Default)]
pub struct ProgramRun {
    pending: Vec<Step>,
    ready: Vec<String>,
    added: Vec<Step>,
    /// The asks, resolutions and emitted findings this round handed to the caller.
    handed: Vec<String>,
}

impl ProgramRun {
    pub fn is_done(&self) -> bool {
        self.pending.is_empty()
    }
}

/// The caller's part of a run: asks and resolutions (frames), and composing a finding whose gate holds.
pub trait Caller {
    /// Run a round's ask steps and record each (`_asks`).
    fn asks(&mut self, interpreter: &mut Interpreter, asks: Vec<(Step, Map<String, Value>)>);
    /// Compose and record a finding step whose gate holds (`_emit`); `began` is when its gate was read.
    fn emit(&mut self, interpreter: &mut Interpreter, step: &Step, scope: &Map<String, Value>, began: f64);
    /// Put a round's undecided findings to the small model and record each (`_resolve`).
    fn resolve(&mut self, interpreter: &mut Interpreter, deferred: Vec<(Step, Map<String, Value>, &'static str)>);
}

impl Interpreter {
    pub fn new(max_steps: usize, defer_unrunnable: bool, clock: Box<dyn Fn() -> f64>) -> Self {
        Interpreter {
            steps_by_id: IndexMap::new(),
            results: IndexMap::new(),
            records: Vec::new(),
            not_emitted: ["gateFalse", "undecided", "askedNo", "askedUnclear"]
                .into_iter()
                .map(|key| (key, 0))
                .collect(),
            truncated: Vec::new(),
            expanded: 0,
            max_steps,
            defer_unrunnable,
            unavailable: IndexSet::new(),
            clock,
        }
    }

    /// Know a program's steps (top level and templates) by id.
    pub fn add_steps(&mut self, steps: &[Step]) {
        for step in steps {
            self.steps_by_id.insert(id_of(step).to_string(), step.clone());
            if let Some(Value::Array(subs)) = step.get("steps") {
                for sub in subs.iter().filter_map(Value::as_object) {
                    self.steps_by_id.insert(id_of(sub).to_string(), sub.clone());
                }
            }
        }
    }

    pub fn now(&self) -> f64 {
        (self.clock)()
    }

    fn value(&self, reference: &str, scope: &Map<String, Value>) -> Option<String> {
        let (name, field) = reference.split_once('.').unwrap_or((reference, ""));
        if let Some(value) = scope.get(name) {
            if field.is_empty() {
                return Some(render(Some(value)));
            }
            return value.as_object().map(|map| render(map.get(field)));
        }
        let result = self.results.get(name)?;
        let shown = |value: Option<&Value>| value.filter(|value| !value.is_null()).map(py::str_of);
        if field.is_empty() {
            return shown(result.get("summary"));
        }
        if field == "text" {
            return match result.get("text") {
                None => Some(String::new()),
                value => shown(value),
            };
        }
        if ["count", "status", "answer", "quote", "why", "title", "value"].contains(&field) {
            let op = result.get("op").and_then(Value::as_str).unwrap_or("");
            let key = if field == "status" && TEST_OPS.contains(&op) {
                "outcome"
            } else {
                field
            };
            return Some(render(result.get(key)));
        }
        None
    }

    /// `text` with every `{{placeholder}}` replaced by the named result or item field (objects and lists filled
    /// throughout; other values as they are).
    pub fn fill(&self, value: &Value, scope: &Map<String, Value>) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| (key.clone(), self.fill(item, scope)))
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(|item| self.fill(item, scope)).collect()),
            Value::String(text) => Value::String(self.fill_text(text, scope)),
            other => other.clone(),
        }
    }

    pub fn fill_text(&self, text: &str, scope: &Map<String, Value>) -> String {
        PLACEHOLDER
            .replace_all(text, |found: &regex::Captures<'_>| {
                self.value(&found[1], scope).unwrap_or_else(|| found[0].to_string())
            })
            .into_owned()
    }

    fn ok_result(&self, sid: &str) -> Option<&Map<String, Value>> {
        self.results
            .get(sid)
            .filter(|result| result.get("status").and_then(Value::as_str) == Some("ok"))
    }

    /// Whether an assert or ask step holds: `None` when it is unknown (the assert could not be decided, the ask
    /// was answered unclear, or the step did not run).
    pub fn truth(&self, sid: &str) -> Option<bool> {
        let result = self.ok_result(sid)?;
        match result.get("op").and_then(Value::as_str) {
            Some("assert") => result.get("value").and_then(Value::as_bool),
            Some("ask") => match result.get("answer").and_then(Value::as_str) {
                Some("unclear") => None,
                answer => Some(answer == Some("yes")),
            },
            _ => None,
        }
    }

    /// A step's `when`: true without one; unknown decides nothing, negated or not.
    pub fn condition(&self, when: Option<(&str, bool)>) -> Option<bool> {
        let Some((target, negated)) = when else {
            return Some(true);
        };
        self.truth(target).map(|value| value != negated)
    }

    /// Whether a step's truth depends, transitively, on a model's answer.
    pub fn roots_in_ask(&self, sid: &str) -> bool {
        fn walk(this: &Interpreter, sid: &str, seen: &mut HashSet<String>) -> bool {
            if !seen.insert(sid.to_string()) {
                return false;
            }
            let Some(step) = this.steps_by_id.get(sid) else {
                return false;
            };
            match op_of(step) {
                "ask" => true,
                "assert" => assert_refs(step).iter().any(|target| walk(this, target, seen)),
                _ => false,
            }
        }
        walk(self, sid, &mut HashSet::new())
    }

    /// Whether a deterministic assert, or one of the asserts it combines, came out against the planner's
    /// expectation.
    pub fn contradicted(&self, sid: &str) -> bool {
        fn walk(this: &Interpreter, sid: &str, seen: &mut HashSet<String>) -> bool {
            if !seen.insert(sid.to_string()) {
                return false;
            }
            let (Some(step), Some(result)) = (this.steps_by_id.get(sid), this.results.get(sid)) else {
                return false;
            };
            if op_of(step) != "assert" {
                return false;
            }
            if truthy(result.get("contradicted")) {
                return true;
            }
            strings(step, "all")
                .into_iter()
                .chain(strings(step, "any"))
                .any(|target| walk(this, &target, seen))
        }
        walk(self, sid, &mut HashSet::new())
    }

    /// Whether an assert reads a test step whose run finished (passed or failed): its value is observed.
    pub fn test_decided(&self, sid: &str) -> bool {
        let Some(step) = self.steps_by_id.get(sid).filter(|step| op_of(step) == "assert") else {
            return false;
        };
        let Some(target) = step.get("step").and_then(Value::as_str) else {
            return false;
        };
        self.results.get(target).is_some_and(|result| {
            let op = result.get("op").and_then(Value::as_str).unwrap_or("");
            TEST_OPS.contains(&op) && matches!(result.get("outcome").and_then(Value::as_str), Some("passed" | "failed"))
        })
    }

    /// Whether a gate rests on a test step that could not run: the small model cannot settle that.
    pub fn test_could_not_run(&self, sid: &str) -> bool {
        assert_leaves(sid, &self.steps_by_id).iter().any(|leaf| {
            TEST_OPS.contains(&op_of(leaf))
                && self
                    .results
                    .get(id_of(leaf))
                    .and_then(|result| result.get("outcome"))
                    .and_then(Value::as_str)
                    == Some("could_not_run")
        })
    }

    /// Record a step's result: its summary set, a record appended (input and output bounded), the result kept.
    pub fn record(
        &mut self,
        step: &Step,
        mut result: Map<String, Value>,
        began: f64,
        tokens: i64,
        inputs: &Value,
        extra: &[(&str, Value)],
    ) {
        let op = op_of(step).to_string();
        let shown = summary(&op, &result);
        result.insert("summary".into(), shown.clone().into());
        let output = if truthy(result.get("text")) {
            result.get("text").map(py::str_of).unwrap_or_default()
        } else {
            shown
        };
        let mut record = Map::new();
        record.insert("id".into(), id_of(step).into());
        record.insert("op".into(), op.into());
        record.insert("status".into(), result.get("status").cloned().unwrap_or(Value::Null));
        record.insert("ms".into(), (((self.now() - began) * 1000.0) as i64).into());
        record.insert("tokens".into(), tokens.into());
        record.insert("input".into(), py::clip(&dumps(inputs), RECORD_CHARS).into());
        record.insert("output".into(), py::clip(&output, RECORD_CHARS).into());
        if truthy(result.get("detail")) {
            record.insert("detail".into(), result.get("detail").cloned().unwrap_or(Value::Null));
        }
        for (key, value) in extra {
            if !value.is_null() {
                record.insert((*key).into(), value.clone());
            }
        }
        self.records.push(record);
        self.results.insert(id_of(step).to_string(), result);
    }

    /// Record a step that did not run.
    pub fn skip(&mut self, step: &Step, why: &str, began: f64) {
        let result = json!({ "op": op_of(step), "status": "skipped", "detail": why, "text": "" });
        let Value::Object(result) = result else { return };
        self.record(step, result, began, 0, &Value::Null, &[]);
    }

    /// Decide an assert step over the results it reads, and record it: true, false or unknown, with what was
    /// seen, whether it came out against the planner's expectation, and its `holds` sentence when it held.
    pub fn assert_step(&mut self, step: &Step) {
        let began = self.now();
        let empty = Map::new();
        let (value, detail): (Option<bool>, String) = if let Some(target_id) = step.get("step").and_then(Value::as_str)
        {
            let [kind, op, operand] = parsed_of(step).unwrap_or_default();
            let target = self.ok_result(target_id);
            let target_op = target
                .and_then(|target| target.get("op"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let outcome = target.and_then(|target| target.get("outcome")).and_then(Value::as_str);
            match target {
                None => (None, format!("{target_id} did not run")),
                Some(_)
                    if TEST_OPS.contains(&target_op)
                        && outcome == Some("could_not_run")
                        && !(kind == "status" && operand == "could_not_run") =>
                {
                    (None, format!("{target_id} could not run"))
                }
                Some(target) if kind == "count" => match target.get("count").and_then(Value::as_i64) {
                    None => (None, format!("{target_id} has no count")),
                    Some(count) => {
                        count_predicate(count, truthy(target.get("capped")), &op, decimal(&operand).unwrap_or(0))
                    }
                },
                Some(target) if kind == "status" || kind == "answer" => {
                    let actual = target.get(if kind == "status" { "outcome" } else { "answer" });
                    let equal = actual.and_then(Value::as_str) == Some(operand.as_str());
                    let mut value = Some(if op == "==" { equal } else { !equal });
                    // An unclear answer satisfies nothing but `answer == unclear`.
                    if kind == "answer"
                        && actual.and_then(Value::as_str) == Some("unclear")
                        && !(op == "==" && operand == "unclear")
                    {
                        value = None;
                    }
                    (value, format!("{kind} is {}", py::fstr(actual)))
                }
                Some(target) => {
                    let needle = self.fill_text(&op, &empty);
                    let body = target.get("text").map(py::str_of).unwrap_or_default();
                    let present = flat(&body).contains(&flat(&needle));
                    if !present && truthy(target.get("truncated")) {
                        (
                            None,
                            format!("{} is not in the result, which is truncated", py::repr(&needle)),
                        )
                    } else {
                        let value = if operand.is_empty() { present } else { !present };
                        let verb = if present { "is" } else { "is not" };
                        (Some(value), format!("{} {verb} in the result", py::repr(&needle)))
                    }
                }
            }
        } else {
            let all = strings(step, "all");
            let refs = if !all.is_empty() {
                all.clone()
            } else {
                strings(step, "any")
            };
            let values: Vec<Option<bool>> = refs.iter().map(|target| self.truth(target)).collect();
            let value = if !all.is_empty() {
                if values.contains(&Some(false)) {
                    Some(false)
                } else if values.iter().all(|value| *value == Some(true)) {
                    Some(true)
                } else {
                    None
                }
            } else if values.contains(&Some(true)) {
                Some(true)
            } else if values.iter().all(|value| *value == Some(false)) {
                Some(false)
            } else {
                None
            };
            let detail = refs
                .iter()
                .zip(&values)
                .map(|(target, value)| format!("{target}={}", python_bool(*value)))
                .collect::<Vec<_>>()
                .join(", ");
            (value, detail)
        };
        let expect = step.get("expect").and_then(Value::as_bool);
        let contradicted = matches!((expect, value), (Some(expect), Some(value)) if expect != value)
            && !self.roots_in_ask(id_of(step));
        let holds = if value == Some(true) && !contradicted {
            let holds = step.get("holds").and_then(Value::as_str).unwrap_or("");
            self.fill_text(holds, &empty)
        } else {
            String::new()
        };
        let result = json!({
            "op": "assert", "status": "ok", "value": value, "detail": detail, "holds": holds, "text": "",
            "expected": step.get("expect").cloned().unwrap_or(Value::Null), "contradicted": contradicted,
        });
        let inputs = match step.get("predicate") {
            predicate if truthy(predicate) => predicate.cloned().unwrap_or(Value::Null),
            _ => json!({
                "all": step.get("all").cloned().unwrap_or(Value::Null),
                "any": step.get("any").cloned().unwrap_or(Value::Null),
            }),
        };
        if let Value::Object(result) = result {
            self.record(step, result, began, 0, &inputs, &[]);
        }
    }

    fn settle(&mut self, step: &Step, counter: &'static str, gate: &str, detail: String, began: f64) -> Gate {
        if let Some(count) = self.not_emitted.get_mut(counter) {
            *count += 1;
        }
        let result =
            json!({ "op": "finding", "status": "ok", "emitted": false, "text": "", "gate": gate, "detail": detail });
        if let Value::Object(result) = result {
            self.record(step, result, began, 0, &Value::Null, &[]);
        }
        Gate::Settled
    }

    /// A finding step's gate: emit when its condition holds; put it to the small model when its check is
    /// unknown or came out against the planner's expectation (a test run that finished is ground truth); else
    /// record why nothing is emitted.
    pub fn finding_gate(&mut self, step: &Step) -> Gate {
        self.gate_at(step).0
    }

    /// [`Interpreter::finding_gate`] and when it was read (an emitted finding's record counts from there).
    pub fn gate_at(&mut self, step: &Step) -> (Gate, f64) {
        let began = self.now();
        (self.gate(step, began), began)
    }

    fn gate(&mut self, step: &Step, began: f64) -> Gate {
        let Some((gate_id, negated)) = when_step(step) else {
            return Gate::Settled;
        };
        let gate_id = gate_id.to_string();
        let held = self.condition(Some((&gate_id, negated)));
        if self
            .steps_by_id
            .get(&gate_id)
            .is_some_and(|gate| op_of(gate) == "assert")
        {
            if self.contradicted(&gate_id) && !self.test_decided(&gate_id) {
                return Gate::Resolve("against the planner's expectation");
            }
            if held.is_none() && self.defer_unrunnable && self.test_could_not_run(&gate_id) {
                let detail = format!("its check {gate_id} rests on a test that could not run (left to the verifier)");
                return self.settle(step, "undecided", "gate unknown; the test could not run", detail, began);
            }
            if held.is_none() {
                return Gate::Resolve("unknown");
            }
        }
        match held {
            None => {
                let detail = format!("its condition {gate_id} did not decide (no finding)");
                self.settle(step, "undecided", "gate undecided", detail, began)
            }
            Some(false) => {
                let detail = format!("{gate_id} does not hold (no finding)");
                self.settle(step, "gateFalse", "gate false", detail, began)
            }
            Some(true) => Gate::Emit,
        }
    }

    /// The instances of a `for_each` template over the items of its `over` result: ids renamed `fe[i].id`,
    /// references to sibling sub-steps renamed with them, the item and index as the instance's scope. Items
    /// beyond `max_items`, or beyond the program's step limit, are noted in `truncated`.
    pub fn expand(&mut self, step: &Step) -> Vec<Step> {
        let sid = id_of(step).to_string();
        let over = step.get("over").and_then(Value::as_str).unwrap_or("");
        let items: Vec<Value> = match self.ok_result(over).and_then(|source| source.get("items")) {
            Some(Value::Array(items)) => items.clone(),
            _ => Vec::new(),
        };
        let max_items = step.get("max_items").and_then(Value::as_u64).unwrap_or(0) as usize;
        let template: Vec<&Step> = match step.get("steps") {
            Some(Value::Array(subs)) => subs.iter().filter_map(Value::as_object).collect(),
            _ => Vec::new(),
        };
        let template_ids: HashSet<&str> = template.iter().map(|sub| id_of(sub)).collect();
        let total = self.steps_by_id.len();
        let mut instances: Vec<Step> = Vec::new();
        for (index, item) in items.iter().take(max_items).enumerate() {
            if total + instances.len() + template.len() > self.max_steps {
                self.truncated.push(format!(
                    "{sid}: items from {index} on were not expanded ({} steps at most)",
                    self.max_steps
                ));
                break;
            }
            let rename = |name: &str| {
                if template_ids.contains(name) {
                    format!("{sid}[{index}].{name}")
                } else {
                    name.to_string()
                }
            };
            let scope = json!({ "item": item, "index": index });
            for sub in &template {
                let mut clone = (*sub).clone();
                clone.insert("id".into(), rename(id_of(sub)).into());
                let needs: Vec<String> = strings(sub, "needs").iter().map(|name| rename(name)).collect();
                clone.insert("needs".into(), json!(needs));
                if let Some((target, negated)) = when_step(sub) {
                    clone.insert("when".into(), json!({ "step": rename(target), "not": negated }));
                }
                for key in ["over", "step"] {
                    if let Some(Value::String(target)) = sub.get(key) {
                        clone.insert(key.into(), rename(target).into());
                    }
                }
                for key in ["context", "all", "any", "evidence"] {
                    if truthy(sub.get(key)) {
                        let renamed: Vec<String> = strings(sub, key).iter().map(|name| rename(name)).collect();
                        clone.insert(key.into(), json!(renamed));
                    }
                }
                clone.insert("scope".into(), scope.clone());
                clone.insert("origin".into(), sid.clone().into());
                instances.push(clone);
            }
        }
        if items.len() > max_items {
            self.truncated.push(format!(
                "{sid}: {} items beyond max_items were not visited",
                items.len() - max_items
            ));
        }
        self.expanded += instances.len();
        instances
    }
}

fn fstr_when(step: &Step) -> String {
    when_step(step).map_or_else(|| "None".to_string(), |(target, _)| target.to_string())
}

impl Interpreter {
    /// Why a test step will not run before trying: its runner was unavailable in an earlier run.
    fn unavailable_runner(&self, args: &Map<String, Value>, session: &Session) -> Option<String> {
        let paths = if args.contains_key("paths") {
            args.get("paths")
        } else {
            args.get("tests")
        };
        let session = session.borrow();
        let paths = session.paths(paths).ok()?;
        for (runner, directory, _) in session.plan(&paths) {
            let place = if directory.is_empty() { "." } else { directory.as_str() };
            if self.unavailable.contains(&(runner.name.clone(), place.to_string())) {
                return Some(format!(
                    "the {} runner in {place} was unavailable in an earlier run (missing dependencies); not tried again",
                    runner.name
                ));
            }
        }
        None
    }

    /// Run a lookup or test step through the deep pass's request serving and record its result: the title, the
    /// body (bounded), its count, items, whether it was capped or cut; for a test step, its run, outcome (passed,
    /// failed or could_not_run) and, for a mutation, whether it was caught. A refused request is a failed step; a
    /// refused test step could not run.
    pub fn lookup(&mut self, step: &Step, scope: &Map<String, Value>, repo: &Repo<'_>, session: Option<&Session>) {
        let began = self.now();
        let op = op_of(step).to_string();
        let args = self.fill(step.get("args").unwrap_or(&Value::Null), scope);
        let test = TEST_OPS.contains(&op.as_str());
        let served = if test && session.is_none() {
            Err(Rejected("tests are not run in this review".to_string()))
        } else {
            let empty = Map::new();
            let why = match session {
                Some(session) if test => self.unavailable_runner(args.as_object().unwrap_or(&empty), session),
                _ => None,
            };
            match why {
                Some(why) => Err(Rejected(why)),
                None => {
                    let mut request = Map::new();
                    request.insert(op.clone(), args.clone());
                    serve_request(
                        repo,
                        &Value::Object(request),
                        session.map(|session| session as &dyn TestSession),
                    )
                }
            }
        };
        let (title, body) = match served {
            Ok(answer) => answer,
            Err(Rejected(detail)) => {
                let mut outcome = Map::new();
                outcome.insert("op".into(), op.clone().into());
                outcome.insert("status".into(), if test { "ok" } else { "failed" }.into());
                outcome.insert("detail".into(), detail.into());
                outcome.insert("text".into(), "".into());
                if test {
                    outcome.insert("outcome".into(), "could_not_run".into());
                }
                self.record(step, outcome, began, 0, &args, &[]);
                return;
            }
        };
        let mut result = Map::new();
        result.insert("op".into(), op.clone().into());
        result.insert("status".into(), "ok".into());
        result.insert("title".into(), title.clone().into());
        result.insert("text".into(), py::clip(&body, RESULT_CHARS).into());
        result.insert("count".into(), count_of(&op, &title, &body).into());
        result.insert("items".into(), Value::Array(items_of(&op, &body)));
        result.insert("capped".into(), title.contains(CAPPED).into());
        let truncated = body.chars().count() > RESULT_CHARS || body.contains("... cut");
        result.insert("truncated".into(), truncated.into());
        if let Some(session) = session.filter(|_| test) {
            let last = title.split_whitespace().last().unwrap_or("");
            let number = (!last.is_empty() && last.chars().all(|c| c.is_ascii_digit()))
                .then(|| last.parse::<usize>().ok())
                .flatten();
            let record = session
                .borrow()
                .records
                .iter()
                .find(|item| Some(item.n) == number)
                .cloned();
            let status = record.as_ref().map(|record| record.status.clone());
            result.insert(
                "run".into(),
                record.as_ref().map_or(Value::Null, |record| {
                    serde_json::to_value(record).unwrap_or(Value::Null)
                }),
            );
            let outcome = status
                .as_deref()
                .filter(|status| matches!(*status, "passed" | "failed"))
                .unwrap_or("could_not_run");
            result.insert("outcome".into(), outcome.into());
            if let Some(record) = &record {
                if record.mutation.is_some() {
                    result.insert("caught".into(), (record.caught == Some(true)).into());
                }
                if record.status == "unavailable" {
                    let place = if record.cwd.is_empty() {
                        "."
                    } else {
                        record.cwd.as_str()
                    };
                    self.unavailable.insert((record.runner.clone(), place.to_string()));
                }
            }
        }
        self.record(step, result, began, 0, &args, &[]);
    }

    /// Start a run of `steps` (the program's, or a `for_each` given alone): every step known by id.
    pub fn start(&mut self, steps: &[Step]) -> ProgramRun {
        for step in steps {
            self.steps_by_id.insert(id_of(step).to_string(), step.clone());
        }
        ProgramRun {
            pending: steps.to_vec(),
            ..ProgramRun::default()
        }
    }

    /// One round of a run: every pending step whose references (and instances) have results runs, in order;
    /// lookups, tests, asserts, conditions and `for_each` here, emitted findings through `emit`. Returns what the
    /// caller settles before [`Interpreter::end_round`], or `None` when the run is over (steps whose dependencies
    /// never finish are skipped then).
    pub fn round(
        &mut self,
        run: &mut ProgramRun,
        repo: &Repo<'_>,
        session: Option<&Session>,
        emit: &mut Emit<'_>,
    ) -> Option<Round> {
        if run.pending.is_empty() {
            return None;
        }
        let ready: Vec<usize> = (0..run.pending.len())
            .filter(|index| {
                let step = &run.pending[*index];
                references(step)
                    .iter()
                    .chain(strings(step, "instances").iter())
                    .all(|name| self.results.contains_key(name))
            })
            .collect();
        if ready.is_empty() {
            for step in std::mem::take(&mut run.pending) {
                let now = self.now();
                self.skip(&step, "its dependencies never finished", now);
            }
            return None;
        }
        let mut out = Round::default();
        run.ready = ready
            .iter()
            .map(|index| id_of(&run.pending[*index]).to_string())
            .collect();
        run.added.clear();
        run.handed.clear();
        for index in ready {
            let began = self.now();
            let step = run.pending[index].clone();
            let scope = step
                .get("scope")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let op = op_of(&step);
            let held = if op == "finding" {
                Some(true)
            } else {
                self.condition(when_step(&step))
            };
            match held {
                None => self.skip(
                    &step,
                    &format!("its condition {} did not decide", fstr_when(&step)),
                    began,
                ),
                Some(false) => self.skip(
                    &step,
                    &format!("its condition {} does not hold", fstr_when(&step)),
                    began,
                ),
                Some(true) if op == "for_each" => {
                    if !step.contains_key("instances") {
                        let instances = self.expand(&step);
                        let ids: Vec<Value> = instances.iter().map(|item| id_of(item).into()).collect();
                        run.pending[index].insert("instances".into(), Value::Array(ids.clone()));
                        if let Some(known) = self.steps_by_id.get_mut(id_of(&step)) {
                            known.insert("instances".into(), Value::Array(ids));
                        }
                        for item in &instances {
                            self.steps_by_id.insert(id_of(item).to_string(), item.clone());
                        }
                        let expanded = !instances.is_empty();
                        run.added.extend(instances);
                        if expanded {
                            continue;
                        }
                    }
                    let step = run.pending[index].clone();
                    let instances = strings(&step, "instances");
                    let done: Vec<&Map<String, Value>> =
                        instances.iter().filter_map(|sid| self.results.get(sid)).collect();
                    let emitted = done.iter().filter(|item| truthy(item.get("emitted"))).count();
                    let held_count = done
                        .iter()
                        .filter(|item| {
                            item.get("op").and_then(Value::as_str) == Some("assert") && truthy(item.get("value"))
                        })
                        .count();
                    let result = json!({
                        "op": "for_each", "status": "ok", "count": instances.len(), "text": "", "items": [],
                        "detail": format!("{emitted} finding(s) emitted, {held_count} assert(s) held"),
                    });
                    let inputs =
                        json!({"over": step.get("over").cloned().unwrap_or(Value::Null), "instances": instances.len()});
                    if let Value::Object(result) = result {
                        self.record(&step, result, began, 0, &inputs, &[]);
                    }
                }
                Some(true) if LOOKUPS.contains(&op) || TEST_OPS.contains(&op) => {
                    self.lookup(&step, &scope, repo, session);
                }
                Some(true) if op == "assert" => self.assert_step(&step),
                Some(true) if op == "finding" => match self.gate_at(&step) {
                    (Gate::Emit, began) => {
                        emit(self, &step, &scope, began);
                        run.handed.push(id_of(&step).to_string());
                    }
                    (Gate::Resolve(why), _) => out.deferred.push((step.clone(), scope, why)),
                    (Gate::Settled, _) => {}
                },
                Some(true) if op == "ask" => out.asks.push((step.clone(), scope)),
                Some(true) => {}
            }
        }
        let handed = out
            .asks
            .iter()
            .map(|(step, _)| step)
            .chain(out.deferred.iter().map(|(step, _, _)| step))
            .map(|step| id_of(step).to_string());
        run.handed.extend(handed);
        Some(out)
    }

    /// Close a round once the caller recorded its asks and resolutions: finished steps leave, new instances join.
    /// A step handed to the caller that it left without a result is skipped (the Python's `_asks`, `_resolve` and
    /// `_emit` always recorded one; a caller that does not would otherwise be handed the same step forever).
    pub fn end_round(&mut self, run: &mut ProgramRun) {
        for sid in std::mem::take(&mut run.handed) {
            if !self.results.contains_key(&sid)
                && let Some(step) = run.pending.iter().find(|step| id_of(step) == sid).cloned()
            {
                let now = self.now();
                self.skip(&step, "the caller recorded no result", now);
            }
        }
        let finished: HashSet<&String> = run.ready.iter().filter(|sid| self.results.contains_key(*sid)).collect();
        let mut pending: Vec<Step> = run
            .pending
            .drain(..)
            .filter(|step| !finished.contains(&id_of(step).to_string()))
            .collect();
        pending.append(&mut run.added);
        run.pending = pending;
        run.ready.clear();
    }

    /// Run `steps` to the end (`Interpreter.run`), the caller settling each round's asks, resolutions and
    /// emitted findings.
    pub fn run(&mut self, steps: &[Step], repo: &Repo<'_>, session: Option<&Session>, caller: &mut dyn Caller) {
        let mut run = self.start(steps);
        loop {
            let mut emit = |interpreter: &mut Interpreter, step: &Step, scope: &Map<String, Value>, began: f64| {
                caller.emit(interpreter, step, scope, began);
            };
            let Some(round) = self.round(&mut run, repo, session, &mut emit) else {
                break;
            };
            if !round.asks.is_empty() {
                caller.asks(self, round.asks);
            }
            if !round.deferred.is_empty() {
                caller.resolve(self, round.deferred);
            }
            self.end_round(&mut run);
        }
    }
}
