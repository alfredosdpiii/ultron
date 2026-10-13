//! What a verifier frame sees around a finding (`review_api.py` `symbols_for`, `source_window`, `hunk_for`).

use std::sync::LazyLock;

use regex::Regex;

use crate::diff::{FileDiff, Hunk, MAX_LINE_CHARS};
use crate::py;

pub const VERIFY_WINDOW: i64 = 15;

static DEF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:export\s+)?(?:async\s+)?(?:def|function|class|func|fn)\s+([A-Za-z_]\w*)|^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_]\w*)\s*=\s*(?:async\s*)?(?:function|\()",
    )
    .expect("valid regex")
});
static CALL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b([A-Za-z_]\w{2,})\s*\(").expect("valid regex"));
const COMMON: &[&str] = &[
    "if",
    "for",
    "while",
    "return",
    "print",
    "len",
    "range",
    "str",
    "int",
    "float",
    "list",
    "dict",
    "set",
    "tuple",
    "isinstance",
    "super",
    "self",
    "this",
    "new",
    "await",
    "async",
    "function",
    "typeof",
    "catch",
    "switch",
    "open",
    "map",
    "filter",
    "sorted",
    "min",
    "max",
    "sum",
    "any",
    "all",
    "enumerate",
    "zip",
    "format",
    "append",
    "push",
    "get",
    "join",
    "split",
    "strip",
    "require",
    "console",
    "log",
    "Error",
    "Exception",
    "assert",
    "expect",
];

/// Names worth searching for around a cited line: its enclosing definition and what the line calls.
pub fn symbols_for(source: Option<&[String]>, line: i64, limit: usize) -> Vec<String> {
    let Some(source) = source.filter(|source| !source.is_empty()) else {
        return Vec::new();
    };
    let count = source.len() as i64;
    let mut names: Vec<String> = Vec::new();
    let mut index = line.min(count) - 1;
    let stop = (-1i64).max(line - 80);
    while index > stop {
        if let Some(found) = source.get(index as usize).and_then(|text| DEF.captures(text)) {
            let name = found.get(1).or_else(|| found.get(2)).map_or("", |m| m.as_str());
            names.push(name.to_string());
            break;
        }
        index -= 1;
    }
    if (1..=count).contains(&line) {
        for found in CALL.captures_iter(&source[line as usize - 1]) {
            let name = &found[1];
            if !COMMON.contains(&name) && !names.iter().any(|known| known == name) {
                names.push(name.to_string());
            }
        }
    }
    names.truncate(limit);
    names
}

/// The source around `line`, gutter-numbered, `>` marking the cited line.
pub fn source_window(source: Option<&[String]>, line: i64, radius: i64) -> String {
    let Some(source) = source.filter(|source| !source.is_empty()) else {
        return "(source unavailable)".to_string();
    };
    let (start, end) = (1.max(line - radius), (source.len() as i64).min(line + radius));
    (start..=end)
        .map(|number| {
            let mark = if number == line { '>' } else { ' ' };
            format!(
                "{mark}{number:>5} | {}",
                py::clip(&source[number as usize - 1], MAX_LINE_CHARS)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The hunk containing `line`, else the nearest one.
pub fn hunk_for(item: Option<&FileDiff>, line: i64) -> Option<&Hunk> {
    let item = item?;
    item.hunks.iter().min_by_key(|hunk| {
        let (first, last) = (hunk.new_first() as i64, hunk.new_last() as i64);
        if first <= line && line <= last {
            0
        } else {
            (first - line).abs().min((last - line).abs())
        }
    })
}
