//! Port of `verdict.ts`: the mechanical checks on a subagent's `rlm.finish` verdict and its one-line completion tag.
//! Nothing here judges quality; a verdict is rejected only for what a program can see.

use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

pub const VERDICT_STATUSES: [&str; 3] = ["passed", "failed", "blocked"];
/// Rejected `rlm.finish` calls a child may fix; the next rejection ends its verdict as `invalid`.
pub const MAX_VERDICT_REJECTIONS: u32 = 2;

const SUMMARY_LIMIT: usize = 2_000;
const EVIDENCE_ITEMS: usize = 40;
const EVIDENCE_CHARS: usize = 1_000;
const CHANGED_FILES: usize = 500;
const OUTPUTS_BYTES: usize = 32 * 1024;

/// JavaScript `\w`, `\d` and `\b` are ASCII; the patterns spell them out so Rust's Unicode classes do not widen them.
static CONCRETE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)(?-u:\b)exit(?:ed)?(?:\s+(?:with\s+)?(?:code|status))?\s*[:=]?\s*-?[0-9]+(?-u:\b)",
        r"(?i)(?-u:\b)[0-9]+\s+(?:passed|failed|passing|failing|tests?|errors?|warnings?|skipped|files?|matches|lines?)(?-u:\b)",
        r#"(?:^|[\s`'"(=])(?:\.{0,2}/)?[A-Za-z0-9_@+.-]+/[A-Za-z0-9_@+./-]*[A-Za-z0-9_@+-]"#,
        r"(?-u:\b)[A-Za-z0-9_-]{2,}\.[A-Za-z][A-Za-z0-9]{0,5}(?-u:\b)",
        r":[0-9]+(?:[-:][0-9]+)?(?-u:\b)",
    ]
    .iter()
    .map(|pattern| Regex::new(pattern).expect("valid pattern"))
    .collect()
});

/// Evidence a program can recognize as concrete: a command's exit status or counts, a file path, a line.
pub fn is_concrete_evidence(item: &str) -> bool {
    CONCRETE.iter().any(|pattern| pattern.is_match(item))
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `path.posix.normalize`.
fn normalize(path: &str) -> String {
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            other => parts.push(other),
        }
    }
    let mut out = parts.join("/");
    if absolute {
        out.insert(0, '/');
    }
    if out.is_empty() {
        return ".".into();
    }
    if trailing && !out.ends_with('/') {
        out.push('/');
    }
    out
}

/// `path.posix.relative` between two absolute paths.
fn relative(from: &str, to: &str) -> String {
    let split = |p: &str| -> Vec<String> {
        normalize(p)
            .split('/')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    };
    let (from, to) = (split(from), split(to));
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".into(); from.len() - common];
    parts.extend(to[common..].iter().cloned());
    parts.join("/")
}

/// A declared path relative to the workspace with `/` separators, or `None` when it lies outside it.
pub fn workspace_path(path: &str, workspace: Option<&str>) -> Option<String> {
    let mut value = path.trim().to_string();
    if value.starts_with('/') {
        value = relative(workspace?, &value);
        if value.is_empty() || value.starts_with("..") || value.starts_with('/') {
            return None;
        }
    }
    let value = normalize(&value).trim_end_matches('/').to_string();
    if value == "." || value.is_empty() || value.starts_with("../") || value == ".." {
        return None;
    }
    Some(value)
}

/// `bounded`: whitespace runs collapsed, trimmed, cut to `limit` UTF-16 units with an ellipsis.
fn bounded(value: &str, limit: usize) -> String {
    let flat = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let units: Vec<u16> = flat.encode_utf16().collect();
    if units.len() <= limit {
        return flat;
    }
    format!("{}…", String::from_utf16_lossy(&units[..limit - 1]))
}

/// Checks an `rlm.finish` payload: the normalized verdict, or every problem found.
pub fn validate_verdict(payload: &Map<String, Value>, workspace: Option<&str>) -> Result<Value, Vec<String>> {
    let mut problems = Vec::new();
    for key in payload.keys() {
        if !["status", "summary", "outputs", "evidence", "changed_files"].contains(&key.as_str()) {
            problems.push(format!(
                "unknown field \"{key}\" (use status, summary, outputs, evidence, changed_files)"
            ));
        }
    }
    let status = payload.get("status");
    let status_text = status.and_then(Value::as_str).filter(|s| VERDICT_STATUSES.contains(s));
    if status_text.is_none() {
        let shown = status.map_or("nothing".to_string(), Value::to_string);
        problems.push(format!(
            "status must be \"passed\", \"failed\" or \"blocked\", not {}",
            bounded(&shown, 200)
        ));
    }
    let summary = payload
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if summary.is_empty() {
        problems.push("summary must say in a sentence or two what was done, or what blocked it".into());
    } else if utf16_len(summary) > SUMMARY_LIMIT {
        problems.push(format!(
            "summary is {} characters; keep it under {SUMMARY_LIMIT} (details go in outputs)",
            utf16_len(summary)
        ));
    }
    let null = Value::Null;
    let outputs = match payload.get("outputs").unwrap_or(&null) {
        Value::Null => json!({}),
        other => other.clone(),
    };
    if !outputs.is_object() {
        problems.push("outputs must be a JSON object (a dict of named results)".into());
    } else if utf16_len(&outputs.to_string()) > OUTPUTS_BYTES {
        problems.push(format!(
            "outputs exceed {OUTPUTS_BYTES} bytes; write large results to a file and return its path"
        ));
    }
    let raw_evidence = match payload.get("evidence").unwrap_or(&null) {
        Value::Null => json!([]),
        other => other.clone(),
    };
    let mut evidence: Vec<String> = Vec::new();
    match raw_evidence.as_array() {
        None => problems.push("evidence must be a list of strings".into()),
        Some(items) => {
            for item in items {
                let Some(text) = item.as_str().filter(|t| !t.trim().is_empty()) else {
                    problems.push("every evidence item must be a nonempty string".into());
                    break;
                };
                if utf16_len(text) > EVIDENCE_CHARS {
                    problems.push(format!(
                        "an evidence item is over {EVIDENCE_CHARS} characters; quote the deciding lines only"
                    ));
                    break;
                }
                evidence.push(text.trim().to_string());
            }
            if items.len() > EVIDENCE_ITEMS {
                problems.push(format!(
                    "evidence has {} items; give at most {EVIDENCE_ITEMS}",
                    items.len()
                ));
            }
            if status_text == Some("passed") {
                if evidence.is_empty() {
                    problems.push("\"passed\" needs evidence: the commands you ran with their exit codes or counts, and the files (with lines) that show the result".into());
                } else if !evidence.iter().any(|e| is_concrete_evidence(e)) {
                    problems.push(format!(
                        "\"passed\" evidence must be concrete, and none of it is: name a command with its outcome (\"npm test: exit 0, 42 passed\") or a file with lines (\"src/app.ts:40-52 handles the empty case\"), not {}",
                        bounded(&Value::String(evidence[0].clone()).to_string(), 80)
                    ));
                }
            }
        }
    }
    let raw_files = match payload.get("changed_files").unwrap_or(&null) {
        Value::Null => json!([]),
        other => other.clone(),
    };
    let mut changed: Vec<String> = Vec::new();
    match raw_files.as_array() {
        Some(items) if items.iter().all(|i| i.as_str().is_some_and(|t| !t.trim().is_empty())) => {
            if items.len() > CHANGED_FILES {
                problems.push(format!(
                    "changed_files has {} paths; list at most {CHANGED_FILES} (or their directories)",
                    items.len()
                ));
            } else {
                for item in items.iter().filter_map(Value::as_str) {
                    let path = workspace_path(item, workspace).unwrap_or_else(|| item.trim().to_string());
                    if !changed.contains(&path) {
                        changed.push(path);
                    }
                }
            }
        }
        _ => problems.push("changed_files must be a list of file paths (relative to the working directory)".into()),
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(json!({
        "status": status_text,
        "summary": summary,
        "outputs": outputs,
        "evidence": evidence,
        "changed_files": changed,
    }))
}

/// One-line tag for a completion event: `[passed, verified]`, `[failed, contradicted; ...]`, `[unverified]`.
pub fn verdict_tag(result: &Value) -> Option<String> {
    let worktree = &result["worktree"];
    let unmerged = match (worktree["commit"].as_str(), worktree["changed_files"].as_array()) {
        (Some(_), Some(files)) => {
            let branch = match &worktree["branch"] {
                Value::String(s) => s.clone(),
                Value::Null => "undefined".into(),
                other => other.to_string(),
            };
            format!("{} file(s) on {branch}, not merged yet", files.len())
        }
        _ => String::new(),
    };
    let check = &result["check"];
    let list = |key: &str| check[key].as_array().cloned().unwrap_or_default();
    if result["verdict"].is_object() {
        let unobserved = list("unobserved");
        let mut extra = vec![unmerged];
        if !unobserved.is_empty() {
            let first: Vec<&str> = unobserved.iter().take(3).filter_map(Value::as_str).collect();
            extra.push(format!("declared but unchanged: {}", first.join(", ")));
        }
        if !list("unreported").is_empty() {
            extra.push(format!("{} undeclared change(s)", list("unreported").len()));
        }
        if !list("outside").is_empty() {
            extra.push(format!("{} change(s) outside its worktree", list("outside").len()));
        }
        extra.retain(|e| !e.is_empty());
        let outcome = check["outcome"].as_str().unwrap_or("unchecked");
        let status = result["verdict"]["status"].as_str().unwrap_or_default();
        let tail = if extra.is_empty() {
            String::new()
        } else {
            format!("; {}", extra.join("; "))
        };
        return Some(format!("[{status}, {outcome}{tail}]"));
    }
    if result["unverified"] == true {
        let rejected = if check["outcome"] == "invalid" {
            ": verdict rejected"
        } else {
            ""
        };
        let tail = if unmerged.is_empty() {
            String::new()
        } else {
            format!("; {unmerged}")
        };
        return Some(format!("[unverified{rejected}{tail}]"));
    }
    None
}
