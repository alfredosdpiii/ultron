//! Autoreview's finding records (`autoreview_api.py` `parse_rules`, `_extras`, `level_rank`, `nearly_identical`,
//! `duplicate_record`, `dedupe_fast`, `group_root_causes`, `check_view`, `_public`). Findings are the JSON objects
//! the kernel API passes, read and updated as the Python dicts were.

use std::cmp::Ordering;

use serde_json::{Map, Value, json};

use crate::findings::{float_of, int_of, similar_claims, text, words};
use crate::levels::{LEVELS, capped_level, finding_kind, level_to_old, set_level, to_level, truthy};
use crate::py;

pub const MERGE_WINDOW: i64 = 5;
pub const IDENTICAL: f64 = 0.8;
pub const MAX_REPLACEMENT_CHARS: usize = 2_000;
pub const MAX_REPLACEMENT_SPAN: i64 = 30;

type Finding = Map<String, Value>;

fn field<'a>(finding: &'a Finding, key: &str) -> Option<&'a Value> {
    finding.get(key)
}

fn string(finding: &Finding, key: &str) -> String {
    finding.get(key).map(py::str_of).unwrap_or_default()
}

fn line_of(finding: &Finding) -> i64 {
    int_of(finding.get("line")).unwrap_or(0)
}

/// `value or default` as the Python wrote it.
fn or<'a>(value: Option<&'a Value>, default: &'a Value) -> &'a Value {
    if truthy(value) {
        value.unwrap_or(default)
    } else {
        default
    }
}

/// The fields the rubric reads from a finder's or an investigator's finding: `unpinned` and `consequence`,
/// bounded and validated.
pub fn parse_rules(raw: &Value, finding: &mut Finding) {
    let raw = raw.as_object();
    if let Some(unpinned) = raw.and_then(|raw| raw.get("unpinned")).and_then(Value::as_object) {
        let mut out = Map::new();
        out.insert("behaviour".into(), text(unpinned.get("behaviour"), 300).into());
        out.insert("change".into(), text(unpinned.get("change"), 300).into());
        if let Some(closest) = unpinned.get("closest_test").and_then(Value::as_object)
            && let Some(path) = closest.get("path").and_then(Value::as_str)
        {
            let line = closest.get("line");
            let line = if truthy(line) {
                int_of(line).map_or(1, |line| line.max(1))
            } else {
                1
            };
            out.insert("closest_test".into(), json!({ "path": py::strip(path), "line": line }));
        }
        if let Some(mutation) = unpinned.get("mutation").and_then(Value::as_object)
            && let (Some(path), Some(replacement)) = (
                mutation.get("path").and_then(Value::as_str),
                mutation.get("replacement").and_then(Value::as_str),
            )
            && !replacement.contains('\n')
            && let Some(line) = int_of(mutation.get("line"))
        {
            let replacement: String = replacement.chars().take(300).collect();
            out.insert(
                "mutation".into(),
                json!({ "path": py::strip(path), "line": line, "replacement": replacement }),
            );
        }
        finding.insert("unpinned".into(), Value::Object(out));
    }
    let consequence = raw.and_then(|raw| raw.get("consequence"));
    finding.insert("consequence".into(), text(consequence, 400).into());
}

/// The autoreview-only fields of one finder reply item: the level (the finder's own kept as `finder_level`, `level`
/// capped by the rubric), the scenario, a line range and an exact replacement. A line the normalizer had to move
/// drops the range and the replacement.
pub fn extras(raw: &Value, finding: &mut Finding) {
    let get = |key: &str| raw.get(key);
    let scenario = text(get("scenario"), 500);
    let finder_level = to_level(get("severity"), Some("low")).unwrap_or("low");
    finding.insert("scenario".into(), scenario.clone().into());
    finding.insert("finder_level".into(), finder_level.into());
    let category = string(finding, "category");
    set_level(finding, capped_level(finder_level, &category, &scenario, None));
    parse_rules(raw, finding);
    let line = line_of(finding);
    if int_of(get("line")) != Some(line) {
        return;
    }
    if let Some(end) = get("end_line").and_then(Value::as_i64)
        && line < end
        && end <= line + MAX_REPLACEMENT_SPAN
    {
        finding.insert("end_line".into(), end.into());
    }
    if let Some(replacement) = get("replacement").and_then(Value::as_str)
        && !py::strip(replacement).is_empty()
        && replacement.chars().count() <= MAX_REPLACEMENT_CHARS
    {
        let replacement = replacement.replace("\r\n", "\n");
        finding.insert("replacement".into(), replacement.trim_end_matches('\n').into());
    }
}

/// The level's rank (critical first) and the negated confidence: the order findings are ranked in.
pub fn level_rank(finding: &Finding) -> (usize, f64) {
    let level = match field(finding, "level") {
        level if truthy(level) => level.map(py::str_of).unwrap_or_default(),
        _ => to_level(finding.get("severity"), Some("low"))
            .unwrap_or("low")
            .to_string(),
    };
    let rank = LEVELS.iter().position(|known| *known == level).unwrap_or(LEVELS.len());
    let confidence = finding.get("confidence");
    let confidence = if truthy(confidence) {
        confidence.and_then(float_of).unwrap_or(0.0)
    } else {
        0.0
    };
    (rank, -confidence)
}

fn compare(a: (usize, f64), b: (usize, f64)) -> Ordering {
    a.0.cmp(&b.0).then(a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal))
}

/// Two texts that say the same thing in nearly the same words (overlap of at least `threshold` of the shorter).
pub fn nearly_identical(a: &str, b: &str, threshold: f64) -> bool {
    let (left, right) = (words(a), words(b));
    if left.len() < 3 || right.len() < 3 {
        return false;
    }
    left.intersection(&right).count() as f64 / left.len().min(right.len()) as f64 >= threshold
}

/// What was merged into what, for `dropped.duplicateOf`.
pub fn duplicate_record(dropped: &Finding, into: &Finding, stage: &str) -> Value {
    let brief = |item: &Finding| {
        let source = item.get("source");
        json!({
            "file": item.get("file").cloned().unwrap_or(Value::Null),
            "line": item.get("line").cloned().unwrap_or(Value::Null),
            "category": item.get("category").cloned().unwrap_or(Value::Null),
            "claim": text(item.get("claim"), 160),
            "source": or(source, &Value::from("fast")),
        })
    };
    json!({ "dropped": brief(dropped), "into": brief(into), "stage": stage })
}

fn number(value: Option<&Value>) -> f64 {
    value.and_then(float_of).unwrap_or(0.0)
}

fn reviewers(finding: &Finding) -> Vec<Value> {
    finding
        .get("reviewers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// The fast pass's findings, one per problem: findings in one file within [`MERGE_WINDOW`] lines of one category
/// merge into the most serious, as do findings of one category whose claims are nearly identical. Findings of
/// different categories never merge. Each merge is recorded in `records`; ids are 1-based in rank order.
pub fn dedupe_fast(findings: &[Finding], records: &mut Vec<Value>) -> Vec<Finding> {
    let mut sorted: Vec<&Finding> = findings.iter().collect();
    sorted.sort_by(|a, b| {
        string(a, "file")
            .cmp(&string(b, "file"))
            .then(line_of(a).cmp(&line_of(b)))
            .then(compare(level_rank(a), level_rank(b)))
    });
    let mut merged: Vec<Finding> = Vec::new();
    for finding in sorted {
        let twin = merged.iter().position(|group| {
            group.get("category") == finding.get("category")
                && ((group.get("file") == finding.get("file")
                    && (line_of(group) - line_of(finding)).abs() <= MERGE_WINDOW)
                    || nearly_identical(&string(group, "claim"), &string(finding, "claim"), IDENTICAL))
        });
        let Some(twin) = twin else {
            merged.push(finding.clone());
            continue;
        };
        let mut joined = reviewers(&merged[twin]);
        for key in reviewers(finding) {
            if !joined.contains(&key) {
                joined.push(key);
            }
        }
        let confidence = merged[twin].get("confidence").cloned();
        if compare(level_rank(finding), level_rank(&merged[twin])) == Ordering::Less {
            records.push(duplicate_record(&merged[twin], finding, "fast"));
            let mut kept = finding.clone();
            kept.insert("reviewers".into(), Value::Array(joined));
            merged[twin] = kept;
        } else {
            records.push(duplicate_record(finding, &merged[twin], "fast"));
            merged[twin].insert("reviewers".into(), Value::Array(joined));
        }
        // max(twin's, finding's): the earlier twin's value unless the finding's is greater.
        let (old, new) = (
            confidence.unwrap_or(Value::Null),
            finding.get("confidence").cloned().unwrap_or(Value::Null),
        );
        let best = if number(Some(&new)) > number(Some(&old)) {
            new
        } else {
            old
        };
        merged[twin].insert("confidence".into(), best);
    }
    merged.sort_by(|a, b| compare(level_rank(a), level_rank(b)));
    for (number, finding) in (1..).zip(merged.iter_mut()) {
        finding.insert("id".into(), Value::from(number));
    }
    merged
}

/// One finding per root cause: confirmed findings of one category that say the same thing in different places
/// (nearly identical claims, or scenarios that describe the same failing input) become the most serious of them,
/// with the other places in `also_at` (at most 8). Returns the kept findings and how many merged.
pub fn group_root_causes(confirmed: &[Finding], mut records: Option<&mut Vec<Value>>) -> (Vec<Finding>, usize) {
    let strength = |item: &Finding| -> u8 {
        if truthy(item.get("test_run")) || truthy(item.get("host_confirmed")) {
            0
        } else if truthy(item.get("beyond_diff")) {
            1
        } else {
            2
        }
    };
    let mut order: Vec<&Finding> = confirmed.iter().collect();
    order.sort_by(|a, b| {
        let (ra, rb) = (level_rank(a), level_rank(b));
        ra.0.cmp(&rb.0)
            .then(strength(a).cmp(&strength(b)))
            .then(ra.1.partial_cmp(&rb.1).unwrap_or(Ordering::Equal))
    });
    let mut kept: Vec<Finding> = Vec::new();
    let mut merged = 0;
    for finding in order {
        let twin = kept.iter().position(|item| {
            item.get("category") == finding.get("category")
                && (nearly_identical(&string(item, "claim"), &string(finding, "claim"), IDENTICAL)
                    || (truthy(item.get("scenario"))
                        && truthy(finding.get("scenario"))
                        && nearly_identical(&string(item, "scenario"), &string(finding, "scenario"), 0.9)
                        && similar_claims(&string(item, "claim"), &string(finding, "claim"))))
        });
        let Some(twin) = twin else {
            kept.push(finding.clone());
            continue;
        };
        merged += 1;
        if let Some(records) = records.as_deref_mut() {
            records.push(duplicate_record(finding, &kept[twin], "root-cause"));
        }
        let place = json!({
            "file": finding.get("file").cloned().unwrap_or(Value::Null),
            "line": finding.get("line").cloned().unwrap_or(Value::Null),
        });
        let twin = &mut kept[twin];
        let also = twin.entry("also_at").or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(also) = also
            && !also.contains(&place)
            && also.len() < 8
        {
            also.push(place);
        }
        let mut names = reviewers(twin);
        for name in reviewers(finding) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        twin.insert("reviewers".into(), Value::Array(names));
    }
    (kept, merged)
}

/// What the verifier frame is told about a host-written check that did not decide a candidate.
pub fn check_view(check: &Value) -> String {
    let get = |key: &str| check.get(key);
    let none = Value::from("none");
    let no_template = Value::from("no template");
    let undecided = Value::from("undecided");
    let detail = if truthy(get("detail")) {
        get("detail")
    } else {
        Some(or(get("gate"), &undecided))
    };
    let mut out = format!(
        "A host-written check ran first and did not decide this finding (shape {}; {}): {}.",
        py::fstr(Some(or(get("shape"), &none))),
        py::fstr(Some(or(get("template"), &no_template))),
        text(detail, 240)
    );
    if let Some(asked) = get("asked").and_then(Value::as_object)
        && truthy(asked.get("answer"))
    {
        out.push_str(&format!(
            "\nThe small model it asked answered {}",
            py::fstr(asked.get("answer"))
        ));
        if truthy(asked.get("quote")) {
            out.push_str(&format!(", quoting `{}`", py::fstr(asked.get("quote"))));
        }
        if truthy(asked.get("why")) {
            out.push_str(&format!(": {}", text(asked.get("why"), 300)));
        }
        out.push_str(". Untrusted; judge from the source.");
    }
    out
}

/// Python `round(x, 2)`: correctly rounded, ties to even on the exact binary value.
fn round2(value: f64) -> f64 {
    format!("{value:.2}").parse().unwrap_or(value)
}

/// A finding as the engine result reports it (`_public`): camelCase fields, its level and the finder's, how it
/// was verified, how strong the evidence is, and its kind.
pub fn public(finding: &Finding, verification: &str) -> Map<String, Value> {
    let get = |key: &str| finding.get(key);
    let level = if truthy(get("level")) {
        string(finding, "level")
    } else {
        to_level(get("severity"), Some("low")).unwrap_or("low").to_string()
    };
    let mut out = Map::new();
    let mut put = |key: &str, value: Value| {
        out.insert(key.to_string(), value);
    };
    let file = string(finding, "file");
    put("id", get("id").cloned().unwrap_or(Value::Null));
    put("file", get("file").cloned().unwrap_or(Value::Null));
    put("line", get("line").cloned().unwrap_or(Value::Null));
    put("level", level.clone().into());
    put("severity", level_to_old(&level).into());
    put("category", get("category").cloned().unwrap_or(Value::Null));
    put("claim", get("claim").cloned().unwrap_or(Value::Null));
    put("why", get("why").cloned().unwrap_or(Value::Null));
    put("verification", verification.into());
    put("confidence", json!(round2(number(get("confidence")))));
    put("reviewers", Value::Array(reviewers(finding)));
    put("source", or(get("source"), &Value::from("fast")).clone());
    if truthy(get("verified_by")) {
        put("verifiedBy", get("verified_by").cloned().unwrap_or(Value::Null));
    }
    let citations: Vec<Value> = if truthy(get("citations")) {
        get("citations").and_then(Value::as_array).cloned().unwrap_or_default()
    } else {
        Vec::new()
    };
    if !citations.is_empty() {
        put("citations", Value::Array(citations.clone()));
    }
    let place = |item: &Value| format!("{}:{}", py::fstr(item.get("path")), py::fstr(item.get("line")));
    let places = citations.iter().take(4).map(place).collect::<Vec<_>>().join(", ");
    if truthy(get("how_verified")) {
        put("howVerified", get("how_verified").cloned().unwrap_or(Value::Null));
    } else if verification == "confirmed" {
        let mut how = String::new();
        if truthy(get("test_run")) {
            how.push_str(&format!(
                "test run {} by the host in a sandbox; ",
                py::fstr(get("test_run"))
            ));
        }
        if !citations.is_empty() {
            let plural = if citations.len() == 1 { "" } else { "s" };
            how.push_str(&format!(
                "{} quoted line{plural} checked at the reviewed commit ({places}); ",
                citations.len()
            ));
        }
        how.push_str(&format!("a verifier confirmed it against the source of {file}"));
        put("howVerified", how.into());
    } else {
        let why = Value::from("the verifier could not decide");
        put(
            "howVerified",
            format!("not confirmed: {}", text(Some(or(get("verification"), &why)), 160)).into(),
        );
    }
    let finder_level = if truthy(get("finder_level")) {
        string(finding, "finder_level")
    } else {
        level.clone()
    };
    put("finderLevel", finder_level.clone().into());
    if finding.contains_key("verifier_level") {
        put("verifierLevel", get("verifier_level").cloned().unwrap_or(Value::Null));
        put(
            "verifierScenarioHolds",
            get("verifier_holds").cloned().unwrap_or(Value::Null),
        );
    }
    put("finderSeverity", level_to_old(&finder_level).into());
    let strength = if truthy(get("test_run")) {
        "test"
    } else if !citations.is_empty() && truthy(get("beyond_diff")) {
        "outside"
    } else {
        "diff"
    };
    put("strength", strength.into());
    if truthy(get("also_at")) {
        put("alsoAt", get("also_at").cloned().unwrap_or(Value::Null));
    }
    if truthy(get("unpinned")) {
        let unpinned = get("unpinned").and_then(Value::as_object).cloned().unwrap_or_default();
        let mut shown = Map::new();
        shown.insert(
            "behaviour".into(),
            unpinned.get("behaviour").cloned().unwrap_or_else(|| "".into()),
        );
        shown.insert(
            "change".into(),
            unpinned.get("change").cloned().unwrap_or_else(|| "".into()),
        );
        shown.insert(
            "closestTest".into(),
            unpinned.get("closest_test").cloned().unwrap_or(Value::Null),
        );
        if truthy(unpinned.get("mutation")) {
            shown.insert(
                "mutation".into(),
                unpinned.get("mutation").cloned().unwrap_or(Value::Null),
            );
        }
        if truthy(get("proof")) {
            shown.insert("proof".into(), get("proof").cloned().unwrap_or(Value::Null));
        }
        put("unpinned", Value::Object(shown));
    }
    if truthy(get("consequence")) {
        put("consequence", get("consequence").cloned().unwrap_or(Value::Null));
    }
    if truthy(get("unclear")) {
        put("unclear", true.into());
    }
    put("scenario", or(get("scenario"), &Value::from("")).clone());
    if let Some(end) = get("end_line").and_then(Value::as_i64)
        && end > line_of(finding)
    {
        put("endLine", end.into());
    }
    if truthy(get("suggested_fix")) {
        put("suggestedFix", get("suggested_fix").cloned().unwrap_or(Value::Null));
    }
    if truthy(get("replacement")) {
        put("replacement", get("replacement").cloned().unwrap_or(Value::Null));
    }
    let cited = citations
        .iter()
        .map(|item| format!("{} `{}`", place(item), py::fstr(item.get("quote"))))
        .collect::<Vec<_>>()
        .join("; ");
    if !cited.is_empty() || truthy(get("evidence")) {
        let evidence = if truthy(get("evidence")) {
            string(finding, "evidence")
        } else {
            String::new()
        };
        let parts: Vec<&str> = [cited.as_str(), evidence.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect();
        put("evidence", parts.join(" | ").into());
    }
    if verification == "uncertain" && truthy(get("verification")) {
        put("note", get("verification").cloned().unwrap_or(Value::Null));
    }
    put("kind", finding_kind(finding).into());
    if truthy(get("shape")) {
        put("shape", get("shape").cloned().unwrap_or(Value::Null));
    }
    if truthy(get("lesson")) {
        put("lesson", get("lesson").cloned().unwrap_or(Value::Null));
    }
    out
}
