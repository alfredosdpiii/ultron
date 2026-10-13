//! Finder replies to findings: validation, categories, merging, and verifier verdicts (`review_api.py`).

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::diff::Hunk;
use crate::prompt_text::Reviewer;
use crate::py;

pub const SEVERITIES: [&str; 4] = ["blocker", "major", "minor", "nit"];
pub const CONTEXT_LINES: i64 = 12;
pub const DEDUPE_WINDOW: i64 = 3;
pub const MAX_FINDINGS_PER_FRAME: usize = 8;

/// One finding as the review pipeline carries it (the Python dict, with its optional keys).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub file: String,
    pub line: i64,
    pub severity: String,
    pub category: String,
    pub claim: String,
    pub why: String,
    pub suggested_fix: String,
    pub confidence: f64,
    pub reviewers: Vec<String>,
    pub chunk: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicates: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<String>,
}

/// The slice of a diff one finder frame saw.
pub struct Chunk<'a> {
    pub id: i64,
    pub path: &'a str,
    pub hunks: &'a [Hunk],
}

const CATEGORY_WORDS: [(&str, &[&str]); 5] = [
    (
        "security",
        &[
            "secur",
            "inject",
            "auth",
            "secret",
            "credential",
            "xss",
            "ssrf",
            "travers",
            "crypto",
            "vuln",
            "permission",
            "privilege",
            "sanitiz",
        ],
    ),
    (
        "ai",
        &["prompt", "llm", "model output", "token limit", "max_tokens", "hallucin"],
    ),
    ("tests", &["test", "coverage", "flak", "assert", "regression"]),
    (
        "maintainability",
        &[
            "arch",
            "design",
            "maint",
            "duplic",
            "coupl",
            "compat",
            "complex",
            "structure",
            "layer",
            "public api",
            "breaking",
            "readab",
            "dead code",
        ],
    ),
    (
        "correctness",
        &[
            "bug",
            "logic",
            "correct",
            "error",
            "exception",
            "null",
            "none",
            "race",
            "async",
            "concurr",
            "bound",
            "off-by",
            "resource",
            "leak",
            "crash",
            "wrong",
        ],
    ),
];

const REVIEWER_CATEGORY: [(&str, &str); 5] = [
    ("bugs", "correctness"),
    ("security", "security"),
    ("arch", "maintainability"),
    ("tests", "tests"),
    ("ai", "ai"),
];

/// Map a free-text category to one of the review's categories, else the reviewer's own, else the text itself.
pub fn normalize_category(category: &str, reviewer: Option<&str>) -> String {
    let text = py::strip(category).to_lowercase();
    for (key, words) in CATEGORY_WORDS {
        if text == key || words.iter().any(|word| text.contains(word)) {
            return key.to_string();
        }
    }
    if let Some((_, own)) = REVIEWER_CATEGORY.iter().find(|(key, _)| Some(*key) == reviewer) {
        return own.to_string();
    }
    if text.is_empty() { "other".to_string() } else { text }
}

/// Python's `_text`: `str(value)` with whitespace runs squashed, clipped to `limit` characters.
pub(crate) fn text(value: Option<&Value>, limit: usize) -> String {
    py::clip(&py::squash(&value.map(py::str_of).unwrap_or_default()), limit)
}

/// Python's `int(value)` on a JSON value.
pub(crate) fn int_of(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Bool(flag) => Some(i64::from(*flag)),
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float.trunc() as i64)),
        Value::String(text) => {
            let digits = py::strip(text);
            let unsigned = digits.strip_prefix(['+', '-']).unwrap_or(digits);
            let valid = !unsigned.is_empty()
                && unsigned
                    .split('_')
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
            if valid {
                digits.replace('_', "").parse().ok()
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Python's `float(value)` on a JSON value.
pub(crate) fn float_of(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(flag) => Some(f64::from(u8::from(*flag))),
        Value::Number(number) => number.as_f64(),
        Value::String(text) => py::strip(text).parse().ok(),
        _ => None,
    }
}

/// Validate a finder frame's reply: pin the file to the chunk's, keep lines in range, bound every string.
pub fn normalize_findings(
    raw: &Value,
    reviewer: &Reviewer,
    chunk: &Chunk<'_>,
    line_count: Option<i64>,
) -> Vec<Finding> {
    let Value::Array(items) = raw else { return Vec::new() };
    let lo = chunk.hunks.iter().map(Hunk::new_first).min().unwrap_or(1) as i64;
    let hi = chunk.hunks.iter().map(Hunk::new_last).max().unwrap_or(1) as i64;
    let mut out = Vec::new();
    for item in items.iter().take(MAX_FINDINGS_PER_FRAME) {
        let Value::Object(item) = item else { continue };
        let claim = text(item.get("claim"), 300);
        if claim.is_empty() {
            continue;
        }
        let mut line = int_of(item.get("line")).unwrap_or(lo);
        let upper = line_count.filter(|count| *count != 0).unwrap_or(hi + CONTEXT_LINES);
        if !(1..=upper.max(1)).contains(&line) {
            line = lo;
        }
        let severity = item.get("severity").map(py::str_of).unwrap_or_default();
        let severity = py::strip(&severity).to_lowercase();
        let confidence = item.get("confidence").map_or(Some(0.5), float_of).unwrap_or(0.5);
        out.push(Finding {
            file: chunk.path.to_string(),
            line,
            severity: if SEVERITIES.contains(&severity.as_str()) {
                severity
            } else {
                "minor".to_string()
            },
            category: normalize_category(&text(item.get("category"), 40), Some(reviewer.key)),
            claim,
            why: text(item.get("why"), 600),
            suggested_fix: text(item.get("suggested_fix"), 500),
            // Python: min(1.0, max(0.0, nan)) is 0.0.
            confidence: if confidence.is_nan() {
                0.0
            } else {
                confidence.clamp(0.0, 1.0)
            },
            reviewers: vec![reviewer.key.to_string()],
            chunk: chunk.id,
            duplicates: None,
            id: None,
            evidence: None,
            verification: None,
        });
    }
    out
}

const STOP: [&str; 23] = [
    "the", "and", "for", "with", "that", "this", "when", "its", "has", "have", "are", "was", "not", "but", "from",
    "into", "than", "then", "which", "can", "will", "does", "any",
];

static WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[a-z0-9_]+").expect("regex"));

pub(crate) fn words(text: &str) -> HashSet<String> {
    let lower = text.to_lowercase();
    WORD.find_iter(&lower)
        .map(|found| found.as_str())
        .filter(|word| word.len() > 2 && !STOP.contains(word))
        .map(str::to_string)
        .collect()
}

/// Two claims about the same problem in different words: at least half of the shorter claim's words recur.
pub fn similar_claims(a: &str, b: &str) -> bool {
    let (left, right) = (words(a), words(b));
    if left.len() < 3 || right.len() < 3 {
        return false;
    }
    left.intersection(&right).count() as f64 / left.len().min(right.len()) as f64 >= 0.5
}

fn rank(finding: &Finding) -> (usize, f64) {
    let severity = SEVERITIES
        .iter()
        .position(|known| *known == finding.severity)
        .unwrap_or(SEVERITIES.len());
    (severity, -finding.confidence)
}

fn rank_order(a: &Finding, b: &Finding) -> std::cmp::Ordering {
    let ((left_severity, left_confidence), (right_severity, right_confidence)) = (rank(a), rank(b));
    left_severity
        .cmp(&right_severity)
        .then(left_confidence.total_cmp(&right_confidence))
}

/// Merge findings on the same file within `window` lines that share a category or make the same claim. The merged
/// finding keeps the most severe (then most confident) wording and every reviewer that raised it; ids are 1-based.
pub fn dedupe(findings: &[Finding], window: i64) -> Vec<Finding> {
    let mut sorted: Vec<&Finding> = findings.iter().collect();
    sorted.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)).then(rank_order(a, b)));
    let mut merged: Vec<Finding> = Vec::new();
    for finding in sorted {
        let group = merged.iter_mut().find(|group| {
            group.file == finding.file
                && (group.line - finding.line).abs() <= window
                && (group.category == finding.category || similar_claims(&group.claim, &finding.claim))
        });
        let Some(group) = group else {
            merged.push(finding.clone());
            continue;
        };
        let mut reviewers = group.reviewers.clone();
        reviewers.extend(
            finding
                .reviewers
                .iter()
                .filter(|key| !group.reviewers.contains(key))
                .cloned(),
        );
        let duplicates = group.duplicates.unwrap_or(1) + 1;
        let confidence = group.confidence.max(finding.confidence);
        if rank_order(finding, group).is_lt() {
            *group = finding.clone();
        }
        group.reviewers = reviewers;
        group.duplicates = Some(duplicates);
        group.confidence = confidence;
    }
    merged.sort_by(rank_order);
    for (number, finding) in (1..).zip(&mut merged) {
        finding.id = Some(number);
    }
    merged
}

static WINDOW_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[> ]\s*\d+ \| ?(.*)$").expect("regex"));
static HUNK_LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[ \d]{5} [+\- ] (.*)$").expect("regex"));
static ALNUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Za-z0-9]").expect("regex"));
static SNIPPET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"`([^`]{6,})`|"([^"]{6,})"|'([^']{6,})'"#).expect("regex"));

/// The code on each line of a rendered source window or hunk, without gutters.
fn source_bodies(source_text: &str) -> Vec<String> {
    py::splitlines(source_text)
        .into_iter()
        .map(|line| {
            let captures = WINDOW_LINE.captures(line).or_else(|| HUNK_LINE.captures(line));
            py::squash(
                captures
                    .as_ref()
                    .map_or(line, |found| found.get(1).map_or("", |body| body.as_str())),
            )
        })
        .collect()
}

/// True when the evidence quotes the source: a source line (6+ chars) appears in it, or a quoted or backticked
/// snippet (6+ chars) from it appears in the source.
pub fn evidence_quotes_source(evidence: &str, source_text: &str) -> bool {
    let evidence_n = py::squash(evidence);
    let bodies = source_bodies(source_text);
    if evidence_n.is_empty() || bodies.is_empty() {
        return false;
    }
    if bodies
        .iter()
        .any(|body| body.chars().count() >= 6 && ALNUM.is_match(body) && evidence_n.contains(body.as_str()))
    {
        return true;
    }
    let source_n = bodies.join("\n");
    SNIPPET.captures_iter(evidence).any(|captures| {
        let part = captures
            .iter()
            .skip(1)
            .flatten()
            .next()
            .map_or("", |found| found.as_str());
        let snippet = py::squash(part);
        !snippet.is_empty() && source_n.contains(&snippet)
    })
}

/// The last JSON object with a `verdict` key in a sub-agent's reply (an object reply is returned as is).
pub fn parse_json_object(reply: &Value) -> Option<Map<String, Value>> {
    let text = match reply {
        Value::Object(map) => return Some(map.clone()),
        Value::String(text) => text,
        _ => return None,
    };
    let mut found = None;
    for (start, _) in text.match_indices('{') {
        let mut values = serde_json::Deserializer::from_str(&text[start..]).into_iter::<Value>();
        if let Some(Ok(Value::Object(map))) = values.next()
            && map.contains_key("verdict")
        {
            found = Some(map);
        }
    }
    found
}

/// A verifier frame's outcome: its reply, or the reason the frame produced none (budget, timeout, error).
pub type Verdict = Result<Value, String>;

/// Split verified findings into (confirmed, uncertain, rejected). Rejected findings never reach the report.
pub fn apply_verdicts(
    findings: &[Finding],
    verdicts: &[Verdict],
    sources: &[&str],
    line_counts: &[Option<i64>],
) -> (Vec<Finding>, Vec<Finding>, Vec<Finding>) {
    let (mut confirmed, mut uncertain, mut rejected) = (Vec::new(), Vec::new(), Vec::new());
    for (((finding, verdict), source), count) in findings.iter().zip(verdicts).zip(sources).zip(line_counts) {
        let mut finding = finding.clone();
        let verdict = match verdict {
            Ok(Value::Object(verdict)) => verdict,
            Ok(_) => {
                finding.verification = Some("not verified (no verdict)".to_string());
                uncertain.push(finding);
                continue;
            }
            Err(status) => {
                finding.verification = Some(format!("not verified ({})", py::clip(&py::squash(status), 120)));
                uncertain.push(finding);
                continue;
            }
        };
        let evidence = text(verdict.get("evidence"), 500);
        finding.evidence = Some(evidence.clone());
        if let Some(corrected) = verdict.get("corrected_line").and_then(Value::as_i64)
            && corrected >= 1
            && count.is_none_or(|count| corrected <= count)
        {
            finding.line = corrected;
        }
        match verdict.get("verdict").and_then(Value::as_str) {
            Some("rejected") => rejected.push(finding),
            Some("confirmed") if evidence_quotes_source(&evidence, source) => confirmed.push(finding),
            Some("confirmed") => {
                finding.verification = Some("the verifier confirmed it without quoting the source".to_string());
                uncertain.push(finding);
            }
            _ => {
                finding.verification = Some(if evidence.is_empty() {
                    "the verifier could not decide".to_string()
                } else {
                    evidence
                });
                uncertain.push(finding);
            }
        }
    }
    (confirmed, uncertain, rejected)
}
