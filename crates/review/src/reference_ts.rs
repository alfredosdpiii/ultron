//! The TypeScript and JavaScript structural scanner of the review reference (`autoreview_reference.py`
//! `_ts_block_end`, `index_ts`): definitions by line patterns with brace-counted extents, imports, `require`s and
//! exports, call sites with their enclosing definition, and literal arrays of strings.
//!
//! Known differences from the source: none in behaviour. Python's call-site pattern has a lookbehind
//! (`(?<![\w$])`), which the `regex` crate lacks; the scan below finds the leftmost match and retries one character
//! later when the character before it is a word character or `$`, which yields the same matches.

use std::sync::LazyLock;

use regex::Regex;

use crate::py;
use crate::reference::{
    CallSite, Family, FileIndex, MAX_CALLS_PER_FILE, MAX_FAMILIES_PER_FILE, MIN_FAMILY_ITEMS, Symbol, clip, is_upper,
};

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

static FUNC: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)\s*[<(]")
});
static ARROW: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*(?::\s*[^=]+)?=>",
    )
});
static CLASS: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)(?:\s+extends\s+([\w$.]+))?")
});
static TYPE: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^\s*(?:export\s+)?(?:declare\s+)?(interface|type|enum)\s+([A-Za-z_$][\w$]*)"));
static CONST: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?="));
static METHOD: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^\s+(?:(?:public|private|protected|static|async|readonly|override|get|set)\s+)*([A-Za-z_$][\w$]*)\s*(?:<[^>]*>)?\([^)]*\)\s*(?::\s*[^{;=]+)?\{",
    )
});
static IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"import\s+(?:type\s+)?(?:([A-Za-z_$][\w$]*)\s*,?\s*)?(?:\{([^}]*)\})?\s*(?:\*\s+as\s+([A-Za-z_$][\w$]*))?\s*from\s*['"]([^'"]+)['"]"#,
    )
});
static REQUIRE: LazyLock<Regex> = LazyLock::new(|| {
    regex(r#"(?:const|let|var)\s+(?:\{([^}]*)\}|([A-Za-z_$][\w$]*))\s*=\s*require\(\s*['"]([^'"]+)['"]\s*\)"#)
});
/// Anchored at the start of the text only (no multiline flag), as in the source.
static EXPORT: LazyLock<Regex> = LazyLock::new(|| regex(r"^\s*export\s+\{([^}]*)\}"));
static ARRAY: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"(?s)\[\s*((?:['"`][^'"`\n]*['"`]\s*,\s*){1,}['"`][^'"`\n]*['"`]\s*,?\s*)\]"#));
static ARRAY_ITEM: LazyLock<Regex> = LazyLock::new(|| regex(r#"['"`][^'"`\n]*['"`]"#));
static NAMED: LazyLock<Regex> = LazyLock::new(|| regex(r"([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*$"));
static LINE_COMMENT: LazyLock<Regex> = LazyLock::new(|| regex(r"//.*$"));
static STRINGS: LazyLock<Regex> =
    LazyLock::new(|| regex(r#"//.*$|'(?:\\.|[^'\\])*'|"(?:\\.|[^"\\])*"|`(?:\\.|[^`\\])*`"#));
static CALL: LazyLock<Regex> = LazyLock::new(|| regex(r"((?:[A-Za-z_$][\w$]*\.)*[A-Za-z_$][\w$]*)\s*\("));

pub const KEYWORDS: [&str; 13] = [
    "if", "for", "while", "switch", "catch", "function", "return", "new", "typeof", "await", "else", "do", "try",
];

fn keyword(name: &str) -> bool {
    KEYWORDS.contains(&name)
}

/// The line where the block opened on `start` (1-based) closes, by brace count (strings and comments skipped
/// roughly); `start` when no brace opens there or on the next line.
pub fn ts_block_end(lines: &[&str], start: usize) -> usize {
    let mut depth: i64 = 0;
    let mut opened = false;
    for number in start..=lines.len() {
        let line = STRINGS.replace_all(lines[number - 1], "");
        for c in line.chars() {
            if c == '{' {
                depth += 1;
                opened = true;
            } else if c == '}' {
                depth -= 1;
                if opened && depth <= 0 {
                    return number;
                }
            }
        }
        if !opened && number > start {
            return start;
        }
    }
    lines.len()
}

fn group(captures: &regex::Captures<'_>, index: usize) -> Option<String> {
    captures.get(index).map(|found| found.as_str().to_string())
}

fn symbol(path: &str, name: &str, qualname: &str, kind: &str, line: usize, end: usize, signature: String) -> Symbol {
    Symbol {
        path: path.to_string(),
        name: name.to_string(),
        qualname: qualname.to_string(),
        kind: kind.to_string(),
        line: line as u64,
        end: end as u64,
        signature,
        parent: None,
        bases: Vec::new(),
        decorators: Vec::new(),
        doc: String::new(),
        exported: true,
    }
}

/// `str.lstrip().startswith(prefix)`.
fn lstarts(line: &str, prefix: &str) -> bool {
    line.trim_start_matches(py::is_space).starts_with(prefix)
}

/// The call sites of a comment-stripped line: `(callee)` of every `name(` or `a.b.name(` not preceded by a word
/// character or `$` (Python's `(?<![\w$])`).
fn call_names(stripped: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0;
    while at <= stripped.len() {
        let Some(found) = CALL.captures_at(stripped, at) else {
            break;
        };
        let whole = found.get(0).expect("match");
        let before = stripped[..whole.start()].chars().next_back();
        if before.is_some_and(|c| c == '$' || is_word(c)) {
            let step = stripped[whole.start()..].chars().next().map_or(1, char::len_utf8);
            at = whole.start() + step;
            continue;
        }
        out.push(found[1].to_string());
        at = whole.end();
    }
    out
}

/// `\w` for one character.
fn is_word(c: char) -> bool {
    let mut buffer = [0u8; 4];
    WORD.is_match(c.encode_utf8(&mut buffer))
}

static WORD: LazyLock<Regex> = LazyLock::new(|| regex(r"^\w$"));

/// Index a TypeScript or JavaScript file.
pub fn index_ts(path: &str, text: &str) -> FileIndex {
    let mut index = FileIndex::new(path, "ts");
    let lines = py::splitlines(text);
    let mut class_stack: Vec<(String, usize)> = Vec::new();
    for (number, line) in lines.iter().enumerate().map(|(i, line)| (i + 1, *line)) {
        while class_stack.last().is_some_and(|(_, end)| number > *end) {
            class_stack.pop();
        }
        let exported = lstarts(line, "export");
        if let Some(found) = CLASS.captures(line) {
            let name = found[1].to_string();
            let end = ts_block_end(&lines, number);
            let mut item = symbol(path, &name, &name, "class", number, end, clip(line, 120));
            item.bases = group(&found, 2).into_iter().collect();
            item.exported = exported;
            index.symbols.push(item);
            class_stack.push((name.clone(), end));
            if exported {
                index.exports.insert(name);
            }
            continue;
        }
        if let Some(found) = FUNC.captures(line).or_else(|| ARROW.captures(line)) {
            let name = found[1].to_string();
            let end = ts_block_end(&lines, number);
            let parent = class_stack.last().map(|(name, _)| name.clone());
            let qualname = parent
                .as_ref()
                .map_or(name.clone(), |parent| format!("{parent}.{name}"));
            let kind = if parent.is_some() { "method" } else { "function" };
            let mut item = symbol(path, &name, &qualname, kind, number, end, clip(line, 120));
            item.parent = parent;
            item.exported = exported;
            index.symbols.push(item);
            if exported {
                index.exports.insert(name);
            }
            continue;
        }
        if let Some(found) = TYPE.captures(line) {
            let name = found[2].to_string();
            let end = ts_block_end(&lines, number);
            let mut item = symbol(path, &name, &name, &found[1], number, end, clip(line, 120));
            item.exported = exported;
            index.symbols.push(item);
            if exported {
                index.exports.insert(name);
            }
            continue;
        }
        if let Some((parent, _)) = class_stack.last()
            && let Some(found) = METHOD.captures(line)
            && !keyword(&found[1])
        {
            let name = found[1].to_string();
            let end = ts_block_end(&lines, number);
            let mut item = symbol(
                path,
                &name,
                &format!("{parent}.{name}"),
                "method",
                number,
                end,
                clip(line, 120),
            );
            item.parent = Some(parent.clone());
            index.symbols.push(item);
            continue;
        }
        if let Some(found) = CONST.captures(line) {
            let name = found[1].to_string();
            let kind = if is_upper(&name) { "constant" } else { "variable" };
            let mut item = symbol(path, &name, &name, kind, number, number, clip(line, 120));
            item.exported = exported;
            index.symbols.push(item);
            if exported {
                index.exports.insert(name);
            }
        }
    }
    for found in IMPORT.captures_iter(text) {
        let spec = &found[4];
        if let Some(default) = found.get(1) {
            index
                .imports
                .insert(default.as_str().to_string(), (None, format!("{spec}:default")));
        }
        if let Some(star) = found.get(3) {
            index
                .imports
                .insert(star.as_str().to_string(), (None, format!("{spec}:*")));
        }
        for part in found.get(2).map_or("", |names| names.as_str()).split(',') {
            let part = py::strip(part);
            let part = py::strip(part.strip_prefix("type ").unwrap_or(part));
            if part.is_empty() {
                continue;
            }
            let (original, alias) = part.split_once(" as ").unwrap_or((part, ""));
            let local = if alias.is_empty() { original } else { alias };
            index.imports.insert(
                py::strip(local).to_string(),
                (None, format!("{spec}:{}", py::strip(original))),
            );
        }
    }
    for found in REQUIRE.captures_iter(text) {
        let spec = &found[3];
        if let Some(single) = found.get(2) {
            index
                .imports
                .insert(single.as_str().to_string(), (None, format!("{spec}:*")));
        }
        for part in found.get(1).map_or("", |names| names.as_str()).split(',') {
            let part = py::strip(part);
            if part.is_empty() {
                continue;
            }
            let (original, alias) = part.split_once(':').unwrap_or((part, ""));
            let local = if alias.is_empty() { original } else { alias };
            index.imports.insert(
                py::strip(local).to_string(),
                (None, format!("{spec}:{}", py::strip(original))),
            );
        }
    }
    for found in EXPORT.captures_iter(text) {
        for part in found[1].split(',') {
            let name = py::strip(py::strip(part).rsplit(" as ").next().unwrap_or(""));
            if !name.is_empty() {
                index.exports.insert(name.to_string());
            }
        }
    }
    // Call sites, each with the innermost definition open on its line.
    let mut enclosing: Vec<(usize, u64)> = Vec::new(); // (symbol position, end line)
    for (number, line) in lines.iter().enumerate().map(|(i, line)| (i + 1, *line)) {
        for (position, item) in index.symbols.iter().enumerate() {
            if item.line == number as u64 && matches!(item.kind.as_str(), "function" | "method" | "class") {
                enclosing.push((position, item.end));
            }
        }
        enclosing.retain(|(_, end)| number as u64 <= *end);
        let stripped = LINE_COMMENT.replace(line, "");
        for callee in call_names(&stripped) {
            let leaf = callee.rsplit('.').next().unwrap_or("");
            if keyword(leaf) || index.calls.len() >= MAX_CALLS_PER_FILE {
                continue;
            }
            let inner = enclosing.last().map(|(position, _)| &index.symbols[*position]);
            if inner.is_some_and(|inner| inner.line == number as u64) {
                continue; // the definition line itself
            }
            let enclosing_name = inner.map(|inner| inner.qualname.clone());
            index.calls.push(CallSite {
                path: path.to_string(),
                line: number as u64,
                callee,
                text: clip(line, crate::reference::MAX_LINE_CHARS),
                enclosing: enclosing_name,
                targets: Vec::new(),
            });
        }
    }
    for found in ARRAY.captures_iter(text) {
        let items: Vec<String> = ARRAY_ITEM
            .find_iter(&found[1])
            .map(|item| {
                let item = py::strip(item.as_str());
                let mut chars = item.chars();
                chars.next();
                chars.next_back();
                chars.as_str().to_string()
            })
            .collect();
        if items.len() >= MIN_FAMILY_ITEMS && index.families.len() < MAX_FAMILIES_PER_FILE {
            let start = found.get(0).expect("match").start();
            let number = text[..start].matches('\n').count() + 1;
            let before_chars: Vec<char> = text[..start].chars().collect();
            let before: String = before_chars[before_chars.len().saturating_sub(120)..].iter().collect();
            let name = NAMED.captures(&before).map(|named| named[1].to_string());
            index.families.push(Family {
                path: path.to_string(),
                line: number as u64,
                name,
                items,
                enclosing: None,
            });
        }
    }
    index
}
