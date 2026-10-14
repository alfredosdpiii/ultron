//! `minimatch(text, pattern, { nocase: true })` (minimatch 10) as the model scope uses it: `#` comments, `!`
//! negation, brace expansion (`{a,b}`, `{1..3}`, `{a..c}`), `/`-separated segments with `**` globstar, `*`, `?`,
//! `[...]` classes (ranges, `!`/`^` negation, POSIX classes), `\` escapes, extglobs (`+(a|b)`, `*(..)`, `?(..)`,
//! `@(..)`, `!(..)`), and the dot rule (a wildcard never matches a segment's leading `.`).
//!
//! Evidence: `crates/ai/fixtures/generate-model-resolver.mjs` records minimatch's answers (`globs`);
//! `crates/ai/tests/model_resolver.rs` replays them.

/// Whether `text` matches `pattern`, case-insensitively.
pub fn minimatch(text: &str, pattern: &str) -> bool {
    if pattern.starts_with('#') {
        return false;
    }
    let bangs = pattern.chars().take_while(|c| *c == '!').count();
    let body = &pattern[bangs..];
    let matched = brace_expand(body).iter().any(|p| match_path(text, p));
    if bangs % 2 == 1 { !matched } else { matched }
}

// ---------------------------------------------------------------------------------------------- brace expansion

/// `brace-expansion`: every alternative of the first `{...}` group with a top-level comma or a `..` range,
/// recursively; anything else stays literal.
pub fn brace_expand(pattern: &str) -> Vec<String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '{' => {
                if let Some(close) = matching_brace(&chars, i) {
                    let inner: String = chars[i + 1..close].iter().collect();
                    let head: String = chars[..i].iter().collect();
                    let tail: String = chars[close + 1..].iter().collect();
                    let options = split_top_level(&inner)
                        .filter(|parts| parts.len() > 1)
                        .or_else(|| range(&inner));
                    if let Some(options) = options {
                        let mut out = Vec::new();
                        for option in options {
                            for expanded in brace_expand(&format!("{head}{option}{tail}")) {
                                out.push(expanded);
                            }
                        }
                        return out;
                    }
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    vec![pattern.to_string()]
}

fn matching_brace(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The comma-separated parts of a brace body, nested braces kept whole.
fn split_top_level(inner: &str) -> Option<Vec<String>> {
    let mut parts = vec![String::new()];
    let mut depth = 0;
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                parts.last_mut()?.push(c);
                if let Some(next) = chars.next() {
                    parts.last_mut()?.push(next);
                }
                continue;
            }
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(String::new());
                continue;
            }
            _ => {}
        }
        parts.last_mut()?.push(c);
    }
    Some(parts)
}

/// `{x..y}` / `{x..y..step}` over integers (zero-padded when an end is) or single characters.
fn range(inner: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = inner.split("..").collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let step = match parts.get(2) {
        Some(step) => step.parse::<i64>().ok()?.unsigned_abs().max(1) as i64,
        None => 1,
    };
    let (start, end) = (parts[0], parts[1]);
    if let (Ok(a), Ok(b)) = (start.parse::<i64>(), end.parse::<i64>()) {
        let padded = |s: &str| s.trim_start_matches('-').len() > 1 && s.trim_start_matches('-').starts_with('0');
        let width = if padded(start) || padded(end) {
            start.len().max(end.len())
        } else {
            0
        };
        let mut out = Vec::new();
        let mut n = a;
        loop {
            out.push(if width > 0 {
                format!("{n:0width$}")
            } else {
                n.to_string()
            });
            if n == b {
                break;
            }
            n += if a < b { step } else { -step };
            if (a < b && n > b) || (a > b && n < b) {
                break;
            }
        }
        return Some(out);
    }
    let (mut a, mut b) = (start.chars(), end.chars());
    let (Some(a), None, Some(b), None) = (a.next(), a.next(), b.next(), b.next()) else {
        return None;
    };
    let (a, b) = (a as i64, b as i64);
    let mut out = Vec::new();
    let mut n = a;
    loop {
        out.push(char::from_u32(n as u32)?.to_string());
        if n == b {
            break;
        }
        n += if a < b { step } else { -step };
        if (a < b && n > b) || (a > b && n < b) {
            break;
        }
    }
    Some(out)
}

// -------------------------------------------------------------------------------------------------- path matching

fn match_path(text: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return text.is_empty();
    }
    let parts: Vec<&str> = pattern.split('/').collect();
    let files: Vec<&str> = text.split('/').collect();
    match_parts(&files, &parts)
}

/// `matchOne`: segment by segment; `**` takes zero or more segments, never one starting with `.`.
fn match_parts(files: &[&str], parts: &[&str]) -> bool {
    match parts.split_first() {
        None => files.is_empty(),
        Some((&"**", rest)) => {
            for skip in 0..=files.len() {
                if match_parts(&files[skip..], rest) {
                    return true;
                }
                match files.get(skip) {
                    Some(f) if !f.starts_with('.') => {}
                    _ => return false,
                }
            }
            false
        }
        Some((part, rest)) => match files.split_first() {
            Some((file, files)) => match_segment(file, part) && match_parts(files, rest),
            None => false,
        },
    }
}

#[derive(Debug)]
enum Node {
    Lit(char),
    Any,
    Star,
    Class { negated: bool, items: Vec<ClassItem> },
    Ext { kind: char, alternatives: Vec<Vec<Node>> },
}

#[derive(Debug)]
enum ClassItem {
    Char(char),
    Range(char, char),
    Posix(String),
}

fn match_segment(text: &str, pattern: &str) -> bool {
    let nodes = parse(&pattern.chars().collect::<Vec<_>>());
    let text: Vec<char> = text.chars().collect();
    if text.is_empty() {
        return nodes.is_empty();
    }
    // A wildcard never matches a leading dot; `.` and `..` only match themselves.
    if text[0] == '.' && !matches!(nodes.first(), Some(Node::Lit('.'))) {
        return false;
    }
    match_nodes(&text, &nodes)
}

fn parse(chars: &[char]) -> Vec<Node> {
    let mut nodes = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if matches!(c, '+' | '*' | '?' | '@' | '!')
            && chars.get(i + 1) == Some(&'(')
            && let Some(close) = closing_paren(chars, i + 1)
        {
            let alternatives = split_alternatives(&chars[i + 2..close])
                .iter()
                .map(|alt| parse(alt))
                .collect();
            nodes.push(Node::Ext { kind: c, alternatives });
            i = close + 1;
            continue;
        }
        match c {
            '\\' if i + 1 < chars.len() => {
                nodes.push(Node::Lit(chars[i + 1]));
                i += 2;
            }
            '*' => {
                if !matches!(nodes.last(), Some(Node::Star)) {
                    nodes.push(Node::Star);
                }
                i += 1;
            }
            '?' => {
                nodes.push(Node::Any);
                i += 1;
            }
            '[' => match parse_class(chars, i) {
                Some((node, next)) => {
                    nodes.push(node);
                    i = next;
                }
                None => {
                    nodes.push(Node::Lit('['));
                    i += 1;
                }
            },
            _ => {
                nodes.push(Node::Lit(c));
                i += 1;
            }
        }
    }
    nodes
}

fn closing_paren(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_alternatives(chars: &[char]) -> Vec<Vec<char>> {
    let mut out = vec![Vec::new()];
    let mut depth = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '\\' if i + 1 < chars.len() => {
                if let Some(last) = out.last_mut() {
                    last.extend([c, chars[i + 1]]);
                }
                i += 2;
                continue;
            }
            '(' => depth += 1,
            ')' => depth -= 1,
            '|' if depth == 0 => {
                out.push(Vec::new());
                i += 1;
                continue;
            }
            _ => {}
        }
        if let Some(last) = out.last_mut() {
            last.push(c);
        }
        i += 1;
    }
    out
}

/// A `[...]` class starting at `open`, and the index after it; None when it never closes.
fn parse_class(chars: &[char], open: usize) -> Option<(Node, usize)> {
    let mut i = open + 1;
    let negated = matches!(chars.get(i), Some('!' | '^'));
    if negated {
        i += 1;
    }
    let mut items = Vec::new();
    let mut first = true;
    loop {
        let c = *chars.get(i)?;
        if c == ']' && !first {
            return Some((Node::Class { negated, items }, i + 1));
        }
        first = false;
        if c == '[' && chars.get(i + 1) == Some(&':') {
            let rest: String = chars[i + 2..].iter().collect();
            if let Some(end) = rest.find(":]") {
                items.push(ClassItem::Posix(rest[..end].to_string()));
                i += 2 + rest[..end].chars().count() + 2;
                continue;
            }
        }
        let (lo, next) = if c == '\\' {
            (*chars.get(i + 1)?, i + 2)
        } else {
            (c, i + 1)
        };
        if chars.get(next) == Some(&'-') && chars.get(next + 1).is_some_and(|c| *c != ']') {
            let hi_at = next + 1;
            let (hi, after) = if chars[hi_at] == '\\' {
                (*chars.get(hi_at + 1)?, hi_at + 2)
            } else {
                (chars[hi_at], hi_at + 1)
            };
            items.push(ClassItem::Range(lo, hi));
            i = after;
        } else {
            items.push(ClassItem::Char(lo));
            i = next;
        }
    }
}

fn eq_nocase(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

fn class_has(items: &[ClassItem], c: char) -> bool {
    let variants = [c, c.to_ascii_lowercase(), c.to_ascii_uppercase()];
    items.iter().any(|item| match item {
        ClassItem::Char(x) => eq_nocase(*x, c),
        ClassItem::Range(lo, hi) => variants.iter().any(|v| lo <= v && v <= hi),
        ClassItem::Posix(name) => match name.as_str() {
            "alpha" => c.is_alphabetic(),
            "digit" => c.is_ascii_digit(),
            "alnum" => c.is_alphanumeric(),
            "upper" | "lower" => c.is_alphabetic(),
            "space" => c.is_whitespace(),
            "punct" => c.is_ascii_punctuation(),
            "xdigit" => c.is_ascii_hexdigit(),
            "word" => c.is_alphanumeric() || c == '_',
            "blank" => c == ' ' || c == '\t',
            "cntrl" => c.is_control(),
            "print" => !c.is_control(),
            "graph" => !c.is_control() && !c.is_whitespace(),
            "ascii" => c.is_ascii(),
            _ => false,
        },
    })
}

/// Whether all of `text` matches `nodes`.
fn match_nodes(text: &[char], nodes: &[Node]) -> bool {
    let Some((node, rest)) = nodes.split_first() else {
        return text.is_empty();
    };
    match node {
        Node::Lit(c) => text.first().is_some_and(|t| eq_nocase(*t, *c)) && match_nodes(&text[1..], rest),
        Node::Any => !text.is_empty() && match_nodes(&text[1..], rest),
        Node::Class { negated, items } => {
            text.first().is_some_and(|t| class_has(items, *t) != *negated) && match_nodes(&text[1..], rest)
        }
        Node::Star => (0..=text.len()).any(|n| match_nodes(&text[n..], rest)),
        Node::Ext { kind, alternatives } => {
            let one = |chunk: &[char]| alternatives.iter().any(|alt| match_nodes(chunk, alt));
            match kind {
                '@' => (0..=text.len()).any(|n| one(&text[..n]) && match_nodes(&text[n..], rest)),
                '?' => {
                    match_nodes(text, rest)
                        || (0..=text.len()).any(|n| one(&text[..n]) && match_nodes(&text[n..], rest))
                }
                '+' => repeat(text, rest, &one, false),
                '*' => repeat(text, rest, &one, true),
                _ => (0..=text.len()).any(|n| !one(&text[..n]) && match_nodes(&text[n..], rest)),
            }
        }
    }
}

/// One or more (zero or more with `empty`) chunks matching an alternative, then `rest`.
fn repeat(text: &[char], rest: &[Node], one: &dyn Fn(&[char]) -> bool, empty: bool) -> bool {
    if empty && match_nodes(text, rest) {
        return true;
    }
    (1..=text.len()).any(|n| one(&text[..n]) && repeat(&text[n..], rest, one, true))
}
