//! `refinements.*` and `experiments.*` of `ultron/local-services.ts` (`NativeLocalServices`): versioned
//! refinements (lessons, instruction and skill changes) with their proposal, approval, activation, rejection and
//! rollback flow over one append-only event ledger, and the experiment history of recorded runs.
//!
//! Requests (Python reaches them through `refinements.*` of `memory_api.py` and `rlm.host_request`):
//! - `refinements.propose` `{kind, target, baseVersion, content, evidence?, scope?}`
//! - `refinements.get` / `activate` / `approve` / `reject` / `rollback` `{id}`; `current` `{kind, target}`; `list` `{}`
//! - `experiments.record` `{run}`; `compare` `{baseline, candidate}`; `list` `{}`
//!
//! Guarantees the host enforces here, never the model: a target names a refinement kind and never human-owned
//! authority (policy, grants, acceptance, approval, trusted instructions); content cannot request tools, grants,
//! models, budgets or permissions, at proposal and again at activation; activation needs nonempty evidence, the
//! current base version and the domain validation; approval (off by default) never overrides validation; a
//! decided record is never re-opened. Every mutation is one appended event; reads replay the ledger, keeping only
//! the events anchored on the caller's conversation branch (A07). Experiments are append-only, and only runs on
//! the same frozen fixture compare. Calls serialize; a failed write makes the service unusable until reopened.

use kernel_apis::artifacts::KeyedDocuments;
use kernel_apis::store::read_versioned;
use kernel_apis::util::{Clock, iso, lock, safe_integer, system_clock, uuid};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

const REFINEMENT_KEY: &str = "ultron.refinements";
const EXPERIMENT_KEY: &str = "ultron.experiments";
const KINDS: [&str; 4] = ["observation", "instruction", "skill", "agent"];
const PROTECTED_NAMES: [&str; 3] = ["agents.md", "security", "policy"];
/// Name segments of human-owned authority: trusted instructions, security policy, grants, the acceptance instrument,
/// judge thresholds and approval. Compared case- and separator-insensitively against every segment of a target.
const PROTECTED_TOKENS: [&str; 19] = [
    "agents",
    "agentsmd",
    "security",
    "policy",
    "policies",
    "grant",
    "grants",
    "permission",
    "permissions",
    "capability",
    "capabilities",
    "acceptance",
    "instrument",
    "judge",
    "threshold",
    "thresholds",
    "approval",
    "approvals",
    "sandbox",
];
/// Content keys that would grant tools, models, budgets or permissions if a consumer honored them.
const CAPABILITY_KEYS: [&str; 16] = [
    "allowedtools",
    "tools",
    "tool",
    "grant",
    "grants",
    "permission",
    "permissions",
    "capability",
    "capabilities",
    "model",
    "budget",
    "timeout",
    "policy",
    "approval",
    "sandbox",
    "env",
];
const MALFORMED: &str = "Malformed local service fields";
const UNCERTAIN: &str = "Document durability is uncertain; reopen the service before use";

/// The domain check run on activation only: `Ok(true)` passes, `Ok(false)` refuses it as failed validation, an
/// error refuses it with that message (TS `validate` returning true/undefined, false, or throwing).
pub type Validator = Arc<dyn Fn(&str, &Value) -> Result<bool, String> + Send + Sync>;
/// A fresh record id (`crypto.randomUUID()`).
pub type Ids = Arc<dyn Fn() -> String + Send + Sync>;
/// Whether an anchor lies on the conversation branch in use.
pub type OnBranch = Arc<dyn Fn(&str) -> bool + Send + Sync>;
/// Active refinements of a definition id as `[{id, version, text}]` (`delegation::agents::RefinementsHook`).
pub type Hook = Arc<dyn Fn(&str) -> Result<Vec<Value>, String> + Send + Sync>;

/// `NativeLocalServicesOptions` for refinements and experiments.
#[derive(Clone)]
pub struct LocalOptions {
    pub validate: Option<Validator>,
    /// Opt-in control: activation requires a prior `refinements.approve`. Off by default.
    pub require_approval: bool,
    pub now: Clock,
    pub ids: Ids,
}

impl Default for LocalOptions {
    fn default() -> Self {
        LocalOptions {
            validate: None,
            require_approval: false,
            now: system_clock(),
            ids: Arc::new(uuid),
        }
    }
}

/// The conversation branch a request is made on (`RefinementBranch`).
#[derive(Clone, Default)]
pub struct Branch {
    /// The entry the change is made at; `None` for an empty conversation.
    pub anchor: Option<String>,
    /// Whether an anchor lies on the branch in use; `None` applies every event.
    pub on_branch: Option<OnBranch>,
}

/// JavaScript `\s`.
fn js_space(ch: char) -> bool {
    (ch.is_whitespace() && ch != '\u{85}') || ch == '\u{feff}'
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(js_space)
}

/// `value.toLowerCase().replace(/[-_\s]/g, "")`.
fn normalize(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .filter(|ch| *ch != '-' && *ch != '_' && !js_space(*ch))
        .collect()
}

/// Positions where `text` has `\r?\n---` (`/\r?\n---/` tried at each index, `\r` first).
fn closing_fence(text: &str, from: usize, then_newline: bool) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let newline_at = |at: usize| -> Option<usize> {
        if bytes.get(at) == Some(&b'\r') && bytes.get(at + 1) == Some(&b'\n') {
            Some(at + 2)
        } else if bytes.get(at) == Some(&b'\n') {
            Some(at + 1)
        } else {
            None
        }
    };
    (from..=text.len()).find_map(|at| {
        let after = newline_at(at)?;
        if !text[after..].starts_with("---") {
            return None;
        }
        let end = after + 3;
        if !then_newline {
            return Some((at, end));
        }
        newline_at(end).map(|body| (at, body))
    })
}

/// `/^---\r?\n([\s\S]*?)\r?\n---/` on `text`: the frontmatter, and where the text after the closing fence starts
/// (past its newline when `then_newline`, as `/...\r?\n---\r?\n([\s\S]*)$/`).
pub(crate) fn frontmatter(text: &str, then_newline: bool) -> Option<(&str, &str)> {
    let start = if text.starts_with("---\r\n") {
        5
    } else if text.starts_with("---\n") {
        4
    } else {
        return None;
    };
    let (close, rest) = closing_fence(text, start, then_newline)?;
    Some((&text[start..close], &text[rest..]))
}

/// Capability non-escalation: a structured or frontmatter key that would claim authority.
fn escalation(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let (front, _) = frontmatter(text.trim_start_matches(js_space), false)?;
            for line in front.split('\n') {
                let line = line.strip_suffix('\r').unwrap_or(line);
                // /^\s*([A-Za-z][\w-]*)\s*:/
                let rest = line.trim_start_matches(js_space);
                let mut chars = rest.char_indices();
                if !chars.next().is_some_and(|(_, ch)| ch.is_ascii_alphabetic()) {
                    continue;
                }
                let end = chars
                    .find(|(_, ch)| !(ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-'))
                    .map_or(rest.len(), |(at, _)| at);
                let key = &rest[..end];
                if rest[end..].trim_start_matches(js_space).starts_with(':')
                    && CAPABILITY_KEYS.contains(&normalize(key).as_str())
                {
                    return Some(key.to_string());
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(escalation),
        Value::Object(map) => map.iter().find_map(|(key, item)| {
            if CAPABILITY_KEYS.contains(&normalize(key).as_str()) {
                Some(key.clone())
            } else {
                escalation(item)
            }
        }),
        _ => None,
    }
}

/// `sameJson`: deep equality with object keys in any order (numbers compare as JavaScript numbers).
pub fn same_json(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_json(x, y)),
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(key, x)| b.get(key).is_some_and(|y| same_json(x, y)))
        }
        _ => left == right,
    }
}

/// `fields(value, required, optional)`: an object with every required key and no other than the optional ones.
fn fields<'a>(value: &'a Value, required: &[&str], optional: &[&str]) -> Result<&'a Map<String, Value>, String> {
    let map = value.as_object().ok_or(MALFORMED)?;
    if required.iter().any(|key| !map.contains_key(*key))
        || map
            .keys()
            .any(|key| !required.contains(&key.as_str()) && !optional.contains(&key.as_str()))
    {
        return Err(MALFORMED.into());
    }
    Ok(map)
}

/// `/^[a-z]+:[a-zA-Z0-9_-]+(?:\.[a-zA-Z0-9_-]+)*$/`.
fn target_shape(target: &str) -> bool {
    let Some((namespace, name)) = target.split_once(':') else {
        return false;
    };
    let word = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    !namespace.is_empty() && namespace.bytes().all(|b| b.is_ascii_lowercase()) && name.split('.').all(word)
}

/// `identity(kind, target)`: a known kind, and a `kind:name` target that names no protected authority.
fn identity(kind: &Value, target: &Value) -> Result<(String, String), String> {
    let kind = kind
        .as_str()
        .filter(|kind| KINDS.contains(kind))
        .ok_or("Invalid refinement kind")?;
    let target = target
        .as_str()
        .filter(|target| target_shape(target))
        .ok_or("Target must be an explicit namespace:name ID, not a path")?;
    let (namespace, name) = target.split_once(':').unwrap_or_default();
    if namespace != kind
        || PROTECTED_NAMES.contains(&name.to_lowercase().as_str())
        || PROTECTED_TOKENS.contains(&normalize(name).as_str())
        || name
            .split(['.', '-', '_'])
            .any(|segment| PROTECTED_TOKENS.contains(&normalize(segment).as_str()))
    {
        return Err("Forbidden refinement target".into());
    }
    Ok((kind.to_string(), target.to_string()))
}

fn nonempty(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !js_trim(text).is_empty(),
        Value::Array(items) => items.iter().any(nonempty),
        Value::Object(map) => map.values().any(nonempty),
        _ => true,
    }
}

/// `proposal(value)`: `{kind, target, baseVersion, content, evidence, scope}` with the defaults filled in.
fn proposal(value: &Value) -> Result<Value, String> {
    let map = fields(
        value,
        &["kind", "target", "baseVersion", "content"],
        &["evidence", "scope"],
    )?;
    let (kind, target) = identity(&map["kind"], &map["target"])?;
    let base = &map["baseVersion"];
    if !safe_integer(base).is_some_and(|version| version >= 0) {
        return Err("baseVersion must be a nonnegative safe integer".into());
    }
    let content = &map["content"];
    if !(content.is_string() || content.is_object() || content.is_array()) || !nonempty(content) {
        return Err("Content must be nonempty JSON text, an object, or an array".into());
    }
    let evidence = map.get("evidence").cloned().unwrap_or_else(|| json!([]));
    if evidence.is_number() || evidence.is_boolean() {
        return Err("Evidence must be JSON text, an object, or an array".into());
    }
    let scope = map.get("scope").cloned().unwrap_or_else(|| json!("session"));
    if !scope.as_str().is_some_and(|scope| !js_trim(scope).is_empty()) {
        return Err("Scope must be a nonempty string".into());
    }
    Ok(json!({
        "kind": kind, "target": target, "baseVersion": base, "content": content, "evidence": evidence, "scope": scope,
    }))
}

/// `Number.isFinite(Date.parse(text))` for the date forms the services write and read: `YYYY-MM-DD`, optionally
/// followed by `THH:MM`, `:SS`, `.fraction` and `Z` or `±HH:MM`.
fn parsable_date(text: &str) -> bool {
    let digits = |part: &str, n: usize| part.len() == n && part.bytes().all(|b| b.is_ascii_digit());
    let in_range = |part: &str, low: u32, high: u32| part.parse::<u32>().is_ok_and(|n| (low..=high).contains(&n));
    let (date, time) = text.split_once('T').map_or((text, None), |(d, t)| (d, Some(t)));
    let mut parts = date.split('-');
    let (Some(year), Some(month), Some(day), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    if !(digits(year, 4) && digits(month, 2) && digits(day, 2) && in_range(month, 1, 12) && in_range(day, 1, 31)) {
        return false;
    }
    let Some(time) = time else {
        return true;
    };
    let (clock, zone) = if let Some(clock) = time.strip_suffix('Z') {
        (clock, None)
    } else if let Some(at) = time.rfind(['+', '-']) {
        (&time[..at], Some(&time[at + 1..]))
    } else {
        (time, None)
    };
    if let Some(zone) = zone {
        let Some((hours, minutes)) = zone.split_once(':') else {
            return false;
        };
        if !(digits(hours, 2) && digits(minutes, 2) && in_range(hours, 0, 23) && in_range(minutes, 0, 59)) {
            return false;
        }
    }
    let (clock, fraction) = clock.split_once('.').map_or((clock, None), |(c, f)| (c, Some(f)));
    if fraction.is_some_and(|f| f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit())) {
        return false;
    }
    let units: Vec<&str> = clock.split(':').collect();
    match units.as_slice() {
        [hours, minutes] => {
            digits(hours, 2) && digits(minutes, 2) && in_range(hours, 0, 24) && in_range(minutes, 0, 59)
        }
        [hours, minutes, seconds] => {
            digits(hours, 2)
                && digits(minutes, 2)
                && digits(seconds, 2)
                && in_range(hours, 0, 24)
                && in_range(minutes, 0, 59)
                && in_range(seconds, 0, 59)
        }
        _ => false,
    }
}

/// One refinement (`RefinementRecord`).
#[derive(Clone, Debug)]
struct Record {
    id: String,
    kind: String,
    target: String,
    base_version: Value,
    content: Value,
    evidence: Value,
    scope: Value,
    version: Option<i64>,
    previous_id: Option<String>,
    state: &'static str,
    approval: Option<&'static str>,
    history: Vec<Value>,
}

impl Record {
    fn json(&self) -> Value {
        json!({
            "id": self.id, "kind": self.kind, "target": self.target, "baseVersion": self.base_version,
            "content": self.content, "evidence": self.evidence, "scope": self.scope, "version": self.version,
            "previousId": self.previous_id, "state": self.state, "approval": self.approval, "history": self.history,
        })
    }

    fn transition(&mut self, state: &'static str, at: &str, cause: &str) {
        self.state = state;
        self.history.push(json!({"state": state, "at": at, "cause": cause}));
    }
}

fn lookup(records: &[Record], id: &Value) -> Result<usize, String> {
    let id = id.as_str().filter(|id| !id.is_empty()).ok_or("Invalid refinement ID")?;
    records
        .iter()
        .position(|record| record.id == id)
        .ok_or_else(|| format!("Unknown refinement: {id}"))
}

fn active(records: &[Record], kind: &str, target: &str) -> Option<usize> {
    records
        .iter()
        .position(|record| record.kind == kind && record.target == target && record.state == "active")
}

fn on_branch(event: &Value, branch: Option<&Branch>) -> bool {
    let Some(test) = branch.and_then(|branch| branch.on_branch.as_ref()) else {
        return true;
    };
    match event.as_object().and_then(|event| event.get("anchor")) {
        Some(Value::String(anchor)) => test(anchor),
        _ => true,
    }
}

/// `apply(records, event)`: one ledger event; returns the index of the record it changed.
fn apply(records: &mut Vec<Record>, event: &Value) -> Result<usize, String> {
    let map = event.as_object().ok_or("Malformed refinement event")?;
    let action = map.get("action").and_then(Value::as_str).unwrap_or_default();
    let optional: &[&str] = match action {
        "propose" => &["proposal"],
        "activate" => &["approval", "anchor"],
        "rollback" => &["anchor"],
        _ => &[],
    };
    fields(event, &["action", "id", "at"], optional)?;
    let (Some(id), Some(at)) = (
        map["id"].as_str().filter(|id| !id.is_empty()),
        map["at"].as_str().filter(|at| parsable_date(at)),
    ) else {
        return Err("Malformed refinement event".into());
    };
    if action == "propose" {
        let data = proposal(map.get("proposal").unwrap_or(&Value::Null))?;
        if records.iter().any(|record| record.id == id) {
            return Err("Duplicate refinement ID".into());
        }
        let text = |key: &str| data[key].as_str().unwrap_or_default().to_string();
        let mut record = Record {
            id: id.into(),
            kind: text("kind"),
            target: text("target"),
            base_version: data["baseVersion"].clone(),
            content: data["content"].clone(),
            evidence: data["evidence"].clone(),
            scope: data["scope"].clone(),
            version: None,
            previous_id: None,
            state: "proposed",
            approval: None,
            history: Vec::new(),
        };
        record.transition("proposed", at, id);
        records.push(record);
        return Ok(records.len() - 1);
    }
    let index = lookup(records, &map["id"])?;
    match action {
        "activate" => {
            let record = &records[index];
            if record.state != "proposed" {
                return Err("Only a proposed refinement can be activated".into());
            }
            identity(&json!(record.kind), &json!(record.target))?;
            if !nonempty(&record.evidence) {
                return Err("Activation requires nonempty evidence".into());
            }
            let previous = active(records, &record.kind, &record.target);
            let current = previous.and_then(|p| records[p].version).unwrap_or(0);
            if record.base_version.as_f64() != Some(current as f64) {
                return Err("Stale base version".into());
            }
            let version = records
                .iter()
                .filter(|item| item.kind == record.kind && item.target == record.target)
                .map(|item| item.version.unwrap_or(0))
                .fold(0, i64::max);
            if version + 1 > 9_007_199_254_740_991 {
                return Err("Version exhausted".into());
            }
            let approval = map.get("approval");
            if approval.is_some_and(|a| a != "approved" && a != "not_required") {
                return Err("Malformed refinement approval".into());
            }
            let claims = approval.is_some_and(|a| a == "approved");
            if claims && record.approval != Some("approved") {
                return Err("Activation claims an approval that was not recorded".into());
            }
            let previous_id = previous.map(|p| records[p].id.clone());
            let record = &mut records[index];
            record.version = Some(version + 1);
            record.previous_id = previous_id;
            if record.approval != Some("approved") {
                record.approval = approval.is_some_and(|a| a == "not_required").then_some("not_required");
            }
            if let Some(previous) = previous {
                records[previous].transition("superseded", at, id);
            }
            records[index].transition("active", at, id);
        }
        "approve" => {
            // Approval never overrides validation: activation still runs every check.
            if records[index].state != "proposed" {
                return Err("Only a proposed refinement can be approved".into());
            }
            records[index].approval = Some("approved");
        }
        "reject" => {
            if records[index].state != "proposed" {
                return Err("Only a proposed refinement can be rejected".into());
            }
            records[index].transition("rejected", at, id);
        }
        "rollback" => {
            let record = &records[index];
            if active(records, &record.kind, &record.target) != Some(index) {
                return Err("Only the latest active refinement can be rolled back".into());
            }
            let previous = record.previous_id.clone();
            records[index].transition("rolled_back", at, id);
            if let Some(previous) = previous {
                let previous = lookup(records, &json!(previous))?;
                records[previous].transition("active", at, id);
            }
        }
        _ => return Err("Unknown refinement action".into()),
    }
    Ok(index)
}

/// `experiment(value)`: a recorded run with valid metadata whose usage, when present, reconciles.
fn experiment(value: &Value) -> Result<(), String> {
    const INVALID: &str = "Experiment requires variant, fixtureHash, outcome and valid record metadata";
    let map = value.as_object().ok_or(INVALID)?;
    let outcome = map.get("outcome").and_then(Value::as_str);
    if !map.get("variant").is_some_and(Value::is_string)
        || !map.get("fixtureHash").is_some_and(Value::is_string)
        || !outcome.is_some_and(|o| ["passed", "failed", "incomplete"].contains(&o))
        || !map.get("id").and_then(Value::as_str).is_some_and(|id| !id.is_empty())
        || !map.get("recordedAt").and_then(Value::as_str).is_some_and(parsable_date)
    {
        return Err(INVALID.into());
    }
    // Usage is optional, but when present its parts must reconcile with its declared total.
    if let Some(usage) = map.get("usage") {
        let usage = usage.as_object().ok_or("Experiment usage must be an object")?;
        let mut sum = 0.0;
        for (key, amount) in usage {
            let amount = amount
                .as_f64()
                .filter(|a| a.is_finite() && *a >= 0.0)
                .ok_or("Experiment usage amounts must be finite and nonnegative")?;
            if key != "total" {
                sum += amount;
            }
        }
        let total = usage.get("total").and_then(Value::as_f64);
        if !total.is_some_and(|total| (total - sum).abs() <= 1e-9 * sum.max(1.0)) {
            return Err("Experiment usage total does not reconcile with its parts".into());
        }
    }
    Ok(())
}

/// Refinements and experiments over keyed durable documents (one service instance per owner).
pub struct LocalServices {
    documents: Box<dyn KeyedDocuments>,
    options: LocalOptions,
    /// A failed write left durability unknown.
    uncertain: Mutex<bool>,
}

impl LocalServices {
    pub fn new(documents: Box<dyn KeyedDocuments>, options: LocalOptions) -> Self {
        LocalServices {
            documents,
            options,
            uncertain: Mutex::new(false),
        }
    }

    /// The session worker's service (`createWorkerServices`): activation runs the worker's content checks
    /// ([`crate::validation::validate_refinement_content`]); approval stays off.
    pub fn worker(documents: Box<dyn KeyedDocuments>) -> Self {
        LocalServices::new(
            documents,
            LocalOptions {
                validate: Some(crate::validation::worker_validator()),
                ..LocalOptions::default()
            },
        )
    }

    /// Serves one request; `branch` is the caller's conversation branch (none: every event applies).
    pub fn handle(&self, kind: &str, payload: &Value, branch: Option<&Branch>) -> Result<Value, String> {
        let mut uncertain = lock(&self.uncertain);
        if *uncertain {
            return Err(UNCERTAIN.into());
        }
        let persist = |key: &str, value: &Value, uncertain: &mut bool| -> Result<(), String> {
            self.documents.set(key, value).inspect_err(|_| *uncertain = true)
        };
        match kind {
            "refinements.propose"
            | "refinements.list"
            | "refinements.get"
            | "refinements.current"
            | "refinements.activate"
            | "refinements.approve"
            | "refinements.reject"
            | "refinements.rollback" => self.refinements(kind, payload, branch, &mut |key, value| {
                persist(key, value, &mut uncertain)
            }),
            "experiments.record" | "experiments.list" | "experiments.compare" => {
                self.experiments(kind, payload, &mut |key, value| persist(key, value, &mut uncertain))
            }
            _ => Err(format!("Unknown local service request: {kind}")),
        }
    }

    /// The active refinement of `kind`/`target` on `branch`, or `None`.
    pub fn current(&self, kind: &str, target: &str, branch: Option<&Branch>) -> Result<Option<Value>, String> {
        let value = self.handle("refinements.current", &json!({"kind": kind, "target": target}), branch)?;
        Ok((!value.is_null()).then_some(value))
    }

    fn now(&self) -> String {
        iso((self.options.now)())
    }

    fn records(&self, branch: Option<&Branch>) -> Result<(Vec<Record>, Vec<Value>), String> {
        let saved = self.documents.get(REFINEMENT_KEY)?;
        let mut records = Vec::new();
        let mut events = Vec::new();
        if let Some(stored) = read_versioned("ultron.local/ultron.refinements", saved, "formatVersion")? {
            let replay = (|| -> Result<(), String> {
                let document = fields(&stored, &["formatVersion", "events"], &[])?;
                let list = document["events"].as_array();
                if document["formatVersion"].as_f64() != Some(1.0) || list.is_none() {
                    return Err("Unsupported document format".into());
                }
                events = list.cloned().unwrap_or_default();
                for event in &events {
                    if on_branch(event, branch) {
                        apply(&mut records, event)?;
                    }
                }
                Ok(())
            })();
            replay.map_err(|_| "Invalid refinement document".to_string())?;
        }
        Ok((records, events))
    }

    fn refinements(
        &self,
        kind: &str,
        payload: &Value,
        branch: Option<&Branch>,
        persist: &mut dyn FnMut(&str, &Value) -> Result<(), String>,
    ) -> Result<Value, String> {
        let (mut records, events) = self.records(branch)?;
        if kind == "refinements.list" {
            fields(payload, &[], &[])?;
            return Ok(Value::Array(records.iter().map(Record::json).collect()));
        }
        if kind == "refinements.current" {
            let map = fields(payload, &["kind", "target"], &[])?;
            let (kind, target) = identity(&map["kind"], &map["target"])?;
            return Ok(active(&records, &kind, &target).map_or(Value::Null, |index| records[index].json()));
        }
        let mut event = Map::new();
        if kind == "refinements.propose" {
            let data = proposal(payload)?;
            if let Some(key) = escalation(&data["content"]) {
                return Err(format!("Refinement content cannot request capabilities ({key})"));
            }
            // An equivalent pending or active proposal, or a rejected one resubmitted without new evidence, is a
            // duplicate rather than a new proposal.
            let duplicate = records.iter().find(|record| {
                record.kind == data["kind"]
                    && record.target == data["target"]
                    && record.scope == data["scope"]
                    && same_json(&record.content, &data["content"])
                    && (record.state == "proposed"
                        || record.state == "active"
                        || (record.state == "rejected" && same_json(&record.evidence, &data["evidence"])))
            });
            if let Some(duplicate) = duplicate {
                return Err(format!(
                    "Duplicate refinement proposal: {} ({})",
                    duplicate.id, duplicate.state
                ));
            }
            event.insert("action".into(), "propose".into());
            event.insert("id".into(), (self.options.ids)().into());
            event.insert("proposal".into(), data);
            event.insert("at".into(), self.now().into());
        } else {
            let map = fields(payload, &["id"], &[])?;
            if kind == "refinements.get" {
                return Ok(records[lookup(&records, &map["id"])?].json());
            }
            let action = &kind["refinements.".len()..];
            event.insert("action".into(), action.into());
            event.insert("id".into(), map["id"].clone());
            event.insert("at".into(), self.now().into());
            if (action == "activate" || action == "rollback")
                && let Some(anchor) = branch.and_then(|branch| branch.anchor.clone())
            {
                event.insert("anchor".into(), anchor.into());
            }
            if action == "activate" {
                let pending = &records[lookup(&records, &map["id"])?];
                if let Some(key) = escalation(&pending.content) {
                    return Err(format!("Refinement content cannot request capabilities ({key})"));
                }
                if self.options.require_approval && pending.approval != Some("approved") {
                    return Err("Activation requires approval".into());
                }
                let approval = if pending.approval == Some("approved") {
                    "approved"
                } else {
                    "not_required"
                };
                event.insert("approval".into(), approval.into());
            }
        }
        let event = Value::Object(event);
        let index = apply(&mut records, &event)?;
        let result = &records[index];
        if event["action"] == "activate"
            && let Some(validate) = &self.options.validate
            && !validate(&result.kind, &result.content)?
        {
            return Err("Refinement content validation failed".into());
        }
        let mut next = events;
        next.push(event);
        persist(REFINEMENT_KEY, &json!({"formatVersion": 1, "events": next}))?;
        Ok(result.json())
    }

    fn experiments(
        &self,
        kind: &str,
        payload: &Value,
        persist: &mut dyn FnMut(&str, &Value) -> Result<(), String>,
    ) -> Result<Value, String> {
        let saved = self.documents.get(EXPERIMENT_KEY)?;
        let mut runs: Vec<Value> = Vec::new();
        if let Some(stored) = read_versioned("ultron.local/ultron.experiments", saved, "formatVersion")? {
            let document = fields(&stored, &["formatVersion", "runs"], &[])?;
            let list = document["runs"].as_array();
            if document["formatVersion"].as_f64() != Some(1.0) || list.is_none() {
                return Err("Invalid experiment document".into());
            }
            let mut seen = std::collections::HashSet::new();
            for run in list.into_iter().flatten() {
                experiment(run)?;
                if !seen.insert(run["id"].as_str().unwrap_or_default().to_string()) {
                    return Err("Duplicate experiment ID".into());
                }
                runs.push(run.clone());
            }
        }
        match kind {
            "experiments.list" => {
                fields(payload, &[], &[])?;
                Ok(Value::Array(runs))
            }
            "experiments.record" => {
                let map = fields(payload, &["run"], &[])?;
                let mut value = map["run"].as_object().cloned().ok_or("Invalid experiment run")?;
                value.insert("id".into(), (self.options.ids)().into());
                value.insert("recordedAt".into(), self.now().into());
                let value = Value::Object(value);
                experiment(&value)?;
                runs.push(value.clone());
                persist(EXPERIMENT_KEY, &json!({"formatVersion": 1, "runs": runs}))?;
                Ok(value)
            }
            _ => {
                let map = fields(payload, &["baseline", "candidate"], &[])?;
                let find = |id: &Value| runs.iter().find(|run| run["id"] == *id);
                let (Some(baseline), Some(candidate)) = (find(&map["baseline"]), find(&map["candidate"])) else {
                    return Err("Unknown experiment".into());
                };
                if baseline["fixtureHash"] != candidate["fixtureHash"] {
                    return Err("Different fixtures cannot form a matched comparison".into());
                }
                Ok(json!({
                    "baseline": baseline, "candidate": candidate,
                    "claim": "Observed runs only; no statistical superiority established",
                }))
            }
        }
    }
}

/// `String(value)` of JavaScript, for refinement content handed to a prompt.
pub fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".into(),
        Value::Array(items) => items
            .iter()
            .map(|item| if item.is_null() { String::new() } else { js_string(item) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
        other => kernel_apis::util::stringify(other),
    }
}

/// The active instruction refinements of a definition id as a typed-agent host's refinement hook
/// (`refinements` of `NativeHostOptions`): `[{id, version, text}]` of `instruction:<definition id>`.
pub fn instruction_hook(services: Arc<LocalServices>) -> Hook {
    Arc::new(move |definition: &str| {
        let current = services.current("instruction", &format!("instruction:{definition}"), None)?;
        Ok(current
            .map(|record| {
                vec![json!({"id": record["id"], "version": record["version"], "text": js_string(&record["content"])})]
            })
            .unwrap_or_default())
    })
}

/// `refinements.*` and `experiments.*` as a host module.
pub struct LocalServicesModule {
    pub services: Arc<LocalServices>,
}

impl kernel_apis::ApiModule for LocalServicesModule {
    fn prefixes(&self) -> &[&str] {
        &["refinements.", "experiments."]
    }

    fn handle(&self, request: &kernel_apis::Request<'_>) -> Result<Value, String> {
        self.services
            .handle(request.kind, &Value::Object(request.payload.clone()), None)
    }
}
