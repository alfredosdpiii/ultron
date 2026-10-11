//! Autoreview's rubric and frame-failure rules (`autoreview_api.py`): levels and the old severity scale, the hard caps
//! on critical and high, the level a verified finding is posted with, generic findings that are never posted, the
//! repository's lessons, and which frame failures are worth a retry. Findings are the JSON objects the kernel API
//! passes; these functions read and update them in place as the Python did.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

use crate::py;

pub const LEVELS: [&str; 5] = ["critical", "high", "medium", "low", "nit"];
/// Levels that need a concrete failing scenario, shown to hold.
pub const SERIOUS: [&str; 2] = ["critical", "high"];
/// Never above medium by themselves.
pub const NEVER_SERIOUS: [&str; 1] = ["tests"];
/// Above medium only when the scenario was shown to fail.
pub const SERIOUS_ONLY_IF_FAILS: [&str; 1] = ["maintainability"];
pub const DEADLINE_ERROR: &str = "not finished before the review deadline";
pub const AUTH_RETRY_S: f64 = 8.0;

/// Python truthiness of a JSON value.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|n| n != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// `value or default` for a JSON field.
fn or<'a>(value: Option<&'a Value>, default: &'a Value) -> &'a Value {
    if truthy(value) {
        value.unwrap_or(default)
    } else {
        default
    }
}

fn old_to_level(name: &str) -> Option<&'static str> {
    match name {
        "blocker" => Some("critical"),
        "major" => Some("high"),
        "minor" => Some("low"),
        _ => None,
    }
}

/// A level on the old scale; medium is `minor`.
pub fn level_to_old(level: &str) -> &'static str {
    match level {
        "critical" => "blocker",
        "high" => "major",
        "nit" => "nit",
        _ => "minor",
    }
}

/// A level from what a model (or an older state file) wrote: a level name or one of the old four names; else
/// `default`.
pub fn to_level(value: Option<&Value>, default: Option<&'static str>) -> Option<&'static str> {
    let name = value
        .map(|value| py::strip(&py::str_of(value)).to_lowercase())
        .unwrap_or_default();
    LEVELS
        .iter()
        .find(|level| **level == name)
        .copied()
        .or_else(|| old_to_level(&name))
        .or(default)
}

/// Set a finding's level and, in step, its severity on the old scale.
pub fn set_level(finding: &mut Map<String, Value>, level: &str) {
    finding.insert("level".into(), level.into());
    finding.insert("severity".into(), level_to_old(level).into());
}

fn rank(level: &str) -> usize {
    LEVELS.iter().position(|item| *item == level).unwrap_or(LEVELS.len())
}

/// A verifier's `scenario_holds` as true, false or unknown, however it spelled it.
pub fn holds(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(flag)) => Some(*flag),
        Some(other) => match py::strip(&py::str_of(other)).to_lowercase().as_str() {
            "true" | "yes" => Some(true),
            "false" | "no" => Some(false),
            _ => None,
        },
        None => None,
    }
}

/// `level` under the rubric's hard rules: critical and high need a concrete scenario; missing tests are never above
/// medium; maintainability is above medium only when its scenario was shown to fail.
pub fn capped_level<'a>(level: &'a str, category: &str, scenario: &str, holds: Option<bool>) -> &'a str {
    if !SERIOUS.contains(&level) {
        return level;
    }
    if py::strip(scenario).is_empty() || NEVER_SERIOUS.contains(&category) {
        return "medium";
    }
    if SERIOUS_ONLY_IF_FAILS.contains(&category) && holds != Some(true) {
        return "medium";
    }
    level
}

fn field(finding: &Map<String, Value>, key: &str) -> String {
    finding.get(key).map(py::str_of).unwrap_or_default()
}

/// The kind a repository's lessons are keyed by: `fast`, `deep:<lens>`, `structure:<shape>`, `compiled` or `hybrid`.
pub fn finding_kind(finding: &Map<String, Value>) -> String {
    let fast = Value::from("fast");
    let source = py::str_of(or(finding.get("source"), &fast));
    if source == "deep:structure" {
        let unknown = Value::from("unknown");
        return format!("structure:{}", py::str_of(or(finding.get("shape"), &unknown)));
    }
    if source.starts_with("compiled") {
        return "compiled".into();
    }
    if source.starts_with("hybrid") {
        return "hybrid".into();
    }
    source
}

/// The level a confirmed finding is posted with: the verifier's own rating. Critical and high stand only when the
/// verifier found the stated scenario really fails; a tests finding the host did not prove is a note (`low`).
/// Records `verifier_level` and `verifier_holds` on the finding.
pub fn final_level(finding: &mut Map<String, Value>, verdict: &Value) -> String {
    let verdict = verdict.as_object();
    let rated = verdict.and_then(|verdict| to_level(verdict.get("severity"), None));
    let level = rated.map_or_else(|| field(finding, "level"), str::to_string);
    let holds = verdict.and_then(|verdict| holds(verdict.get("scenario_holds")));
    finding.insert("verifier_level".into(), rated.map_or(Value::Null, Value::from));
    finding.insert(
        "verifier_holds".into(),
        holds.map_or(Value::from("unknown"), Value::from),
    );
    if SERIOUS.contains(&level.as_str()) && holds != Some(true) {
        return "low".into();
    }
    let category = field(finding, "category");
    if category == "tests" && finding.get("proof") != Some(&Value::from("proven")) && !truthy(finding.get("test_run")) {
        return "low".into();
    }
    let empty = Value::from("");
    let scenario = py::str_of(or(finding.get("scenario"), &empty));
    capped_level(&level, &category, &scenario, holds).to_string()
}

static NON_CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\.(ya?ml|json|toml|ini|cfg|env|tf|tfvars|hcl|sql|sh|bash|zsh|properties|xml|txt|csv|lock|md|mdx|rst|adoc)\n?\z|(^|/)(Dockerfile|Makefile|\.env[^/]*)\n?\z",
    )
    .expect("pattern")
});
static CITED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)[\w./-]+\.\w+:\d+|\bline \d+").expect("pattern"));

/// Whether a file holds code a test can exercise, as against configuration, manifests, scripts or prose. (`\n?\z`
/// is Python's `$`.)
pub fn is_code(path: &str) -> bool {
    !NON_CODE.is_match(path)
}

/// `str(value)`, with `None` for a missing or null value.
fn py_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(value) => py::str_of(value),
    }
}

fn py_len(value: Option<&Value>) -> usize {
    match value {
        Some(Value::String(text)) => text.chars().count(),
        Some(Value::Array(items)) => items.len(),
        Some(Value::Object(map)) => map.len(),
        _ => 0,
    }
}

/// Why a finding is generic and not posted, or none. A tests finding must name the behaviour and the specific change
/// no test would notice, in code; a maintainability finding (above nit) must show a problem that exists now, with
/// its place.
pub fn generic_reason(finding: &Map<String, Value>) -> Option<&'static str> {
    let empty = Value::Object(Map::new());
    match field(finding, "category").as_str() {
        "tests" => {
            let unpinned = or(finding.get("unpinned"), &empty);
            if py_len(unpinned.get("behaviour")) < 8 || py_len(unpinned.get("change")) < 8 {
                return Some("a tests finding that names no change an existing test would miss");
            }
            let file = finding
                .get("file")
                .filter(|file| truthy(Some(file)))
                .map(py::str_of)
                .unwrap_or_default();
            let mutation = or(unpinned.get("mutation"), &empty);
            let mutated = mutation
                .get("path")
                .filter(|path| truthy(Some(path)))
                .map(py::str_of)
                .unwrap_or(file.clone());
            if !is_code(&file) || !is_code(&mutated) {
                return Some("a tests finding about a configuration, manifest, script or document value");
            }
            None
        }
        "maintainability" if finding.get("level") != Some(&Value::from("nit")) => {
            let empty_text = Value::from("");
            let consequence = py::str_of(or(finding.get("consequence"), &empty_text));
            (consequence.chars().count() < 12 || !CITED.is_match(&consequence))
                .then_some("a maintainability finding without a problem that exists now")
        }
        _ => None,
    }
}

/// A lesson's `(category, kind)`.
type LessonKey = (String, String);

/// What this repository taught: a finding of a category and kind it rarely accepted carries the count as its
/// `lesson` and, above `low`, is lowered to `low`. Returns how many were lowered. Nothing is dropped.
pub fn apply_lessons(findings: &mut [Map<String, Value>], lessons: &[Value]) -> usize {
    let mut lowered: Vec<(LessonKey, &Map<String, Value>)> = Vec::new();
    for item in lessons
        .iter()
        .filter_map(Value::as_object)
        .filter(|item| truthy(item.get("lowered")))
    {
        let key = (py_str(item.get("category")), py_str(item.get("kind")));
        // A later rule for the same key replaces an earlier one, as a dict comprehension does.
        lowered.retain(|(existing, _)| *existing != key);
        lowered.push((key, item));
    }
    let count = |rule: &Map<String, Value>, key: &str| -> i64 {
        if truthy(rule.get(key)) {
            rule.get(key).and_then(Value::as_f64).map_or(0, |n| n.trunc() as i64)
        } else {
            0
        }
    };
    let mut changed = 0;
    for finding in findings.iter_mut() {
        let key = (field(finding, "category"), finding_kind(finding));
        let Some((_, rule)) = lowered.iter().find(|(existing, _)| *existing == key) else {
            continue;
        };
        let accepted = count(rule, "accepted");
        let total = accepted + count(rule, "rejected");
        finding.insert(
            "lesson".into(),
            format!("accepted {accepted} of {total} such findings in the last 180 days").into(),
        );
        let level = if truthy(finding.get("level")) {
            field(finding, "level")
        } else {
            to_level(finding.get("severity"), Some("low"))
                .unwrap_or("low")
                .to_string()
        };
        if rank(&level) < rank("low") {
            set_level(finding, "low");
            changed += 1;
        }
    }
    changed
}

static TRANSIENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(408|409|425|429|500|502|503|504|529)\b|rate.?lim|too many requests|overload|temporar|unavailable|timed? ?out|timeout|econnreset|etimedout|epipe|socket hang up|fetch failed|network|connection (reset|closed)",
    )
    .expect("pattern")
});
static AUTH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)not logged in|failed to authenticate|session expired|unauthori[sz]ed|\b401\b|auth login")
        .expect("pattern")
});
static RETRY_AFTER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)retry[-_ ]?after\D{0,12}(\d+(?:\.\d+)?)|try again in (\d+(?:\.\d+)?) ?s|reset[-_ ]?(?:seconds|in|after)\D{0,6}(\d+(?:\.\d+)?)",
    )
    .expect("pattern")
});
static THROTTLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b429\b|rate.?limit|too many requests|cooldown|model_cooldown|reset_seconds|overloaded")
        .expect("pattern")
});

/// A frame failure worth another try: a rate limit, a timeout, an overloaded or unreachable provider, or a provider
/// whose credentials are being refreshed. A missed deadline never is.
pub fn is_transient(error: &str) -> bool {
    error != DEADLINE_ERROR && (TRANSIENT.is_match(error) || AUTH.is_match(error) || THROTTLE.is_match(error))
}

/// A provider that cannot authenticate right now (retried after `AUTH_RETRY_S`).
pub fn is_auth_failure(error: &str) -> bool {
    AUTH.is_match(error)
}

/// A provider that throttles the model: its in-flight limit is halved for the rest of the review.
pub fn is_throttled(error: &str) -> bool {
    THROTTLE.is_match(error)
}

/// Seconds the provider asked to wait, when its error says so.
pub fn retry_after(error: &str) -> Option<f64> {
    let found = RETRY_AFTER.captures(error)?;
    (1..=3)
        .find_map(|group| found.get(group))
        .and_then(|number| number.as_str().parse().ok())
}
