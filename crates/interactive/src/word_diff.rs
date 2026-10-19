//! `diffWords` from jsdiff 8.0.4 (BSD-3-Clause, Copyright (c) 2009-2015 Kevin Decker): Myers diff over word tokens that
//! carry their surrounding whitespace ([`session_file::diff::diff_tokens`]), then the whitespace dedupe pass. No options
//! (no ignoreCase, no segmenter).

use tui::utils::is_whitespace_char as ws;

pub use session_file::diff::Change;

fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || c == '_'
        || matches!(c as u32, 0xAD | 0xC0..=0xD6 | 0xD8..=0xF6 | 0xF8..=0x2C6 | 0x2C8..=0x2D7 | 0x2DE..=0x2FF | 0x1E00..=0x1EFF)
}

/// `[word]+|\s+|[^word]`, then whitespace runs stitched onto the neighbouring tokens.
fn tokenize(value: &str) -> Vec<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut rest = value;
    while let Some(c) = rest.chars().next() {
        let len = if word_char(c) {
            rest.find(|x: char| !word_char(x)).unwrap_or(rest.len())
        } else if ws(c) {
            rest.find(|x: char| !ws(x)).unwrap_or(rest.len())
        } else {
            c.len_utf8()
        };
        parts.push(&rest[..len]);
        rest = &rest[len..];
    }
    let is_ws = |p: &str| p.chars().any(ws);
    let mut tokens: Vec<String> = Vec::new();
    let mut prev: Option<&str> = None;
    for part in parts {
        if is_ws(part) {
            match (prev, tokens.last_mut()) {
                (Some(_), Some(last)) => last.push_str(part),
                _ => tokens.push(part.to_string()),
            }
        } else if let Some(p) = prev.filter(|p| is_ws(p)) {
            match tokens.last_mut() {
                Some(last) if last == p => last.push_str(part),
                _ => tokens.push(format!("{p}{part}")),
            }
        } else {
            tokens.push(part.to_string());
        }
        prev = Some(part);
    }
    tokens.retain(|t| !t.is_empty());
    tokens
}

fn equals(a: &str, b: &str) -> bool {
    a.trim_matches(ws) == b.trim_matches(ws)
}

fn join(tokens: &[String]) -> String {
    tokens
        .iter()
        .enumerate()
        .map(|(i, t)| if i == 0 { t.as_str() } else { t.trim_start_matches(ws) })
        .collect()
}

fn leading_ws(s: &str) -> &str {
    &s[..s.len() - s.trim_start_matches(ws).len()]
}

fn trailing_ws(s: &str) -> &str {
    &s[s.trim_end_matches(ws).len()..]
}

fn common_prefix<'a>(a: &'a str, b: &str) -> &'a str {
    let n: usize = a
        .chars()
        .zip(b.chars())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum();
    &a[..n]
}

fn common_suffix<'a>(a: &'a str, b: &str) -> &'a str {
    let n: usize = a
        .chars()
        .rev()
        .zip(b.chars().rev())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x.len_utf8())
        .sum();
    &a[a.len() - n..]
}

/// `maximumOverlap`: the longest prefix of `b` that is a suffix of `a`.
fn maximum_overlap<'a>(a: &str, b: &'a str) -> &'a str {
    let mut best = "";
    for (i, c) in b.char_indices() {
        let p = &b[..i + c.len_utf8()];
        if a.ends_with(p) {
            best = p;
        }
    }
    best
}

fn without_prefix(s: &str, p: &str) -> String {
    s.strip_prefix(p).unwrap_or(s).to_string()
}

fn without_suffix(s: &str, p: &str) -> String {
    s.strip_suffix(p).unwrap_or(s).to_string()
}

fn replace_suffix(s: &str, old: &str, new: &str) -> String {
    format!("{}{new}", without_suffix(s, old))
}

fn replace_prefix(s: &str, old: &str, new: &str) -> String {
    format!("{new}{}", without_prefix(s, old))
}

fn dedupe(changes: &mut [Change], start: Option<usize>, del: Option<usize>, ins: Option<usize>, end: Option<usize>) {
    let v = |c: &[Change], i: usize| c[i].value.clone();
    match (del, ins) {
        (Some(d), Some(n)) => {
            let (dv, nv) = (v(changes, d), v(changes, n));
            if let Some(s) = start {
                let common = common_prefix(leading_ws(&dv), leading_ws(&nv)).to_string();
                changes[s].value = replace_suffix(&v(changes, s), leading_ws(&nv), &common);
                changes[d].value = without_prefix(&v(changes, d), &common);
                changes[n].value = without_prefix(&v(changes, n), &common);
            }
            if let Some(e) = end {
                let common = common_suffix(trailing_ws(&dv), trailing_ws(&nv)).to_string();
                changes[e].value = replace_prefix(&v(changes, e), trailing_ws(&nv), &common);
                changes[d].value = without_suffix(&v(changes, d), &common);
                changes[n].value = without_suffix(&v(changes, n), &common);
            }
        }
        (None, Some(n)) => {
            if start.is_some() {
                changes[n].value = changes[n].value.trim_start_matches(ws).to_string();
            }
            if let Some(e) = end {
                changes[e].value = changes[e].value.trim_start_matches(ws).to_string();
            }
        }
        (Some(d), None) => match (start, end) {
            (Some(s), Some(e)) => {
                let (dv, ev) = (v(changes, d), v(changes, e));
                let new_full = leading_ws(&ev);
                let ws_start = common_prefix(new_full, leading_ws(&dv));
                let dv2 = without_prefix(&dv, ws_start);
                let ws_end = common_suffix(&new_full[ws_start.len()..], trailing_ws(&dv)).to_string();
                changes[d].value = without_suffix(&dv2, &ws_end);
                changes[e].value = replace_prefix(&ev, new_full, &ws_end);
                let keep = &new_full[..new_full.len() - ws_end.len()];
                changes[s].value = replace_suffix(&v(changes, s), new_full, keep);
            }
            (None, Some(e)) => {
                let dv = v(changes, d);
                let overlap = maximum_overlap(trailing_ws(&dv), leading_ws(&changes[e].value)).to_string();
                changes[d].value = without_suffix(&dv, &overlap);
            }
            (Some(s), None) => {
                let dv = v(changes, d);
                let overlap = maximum_overlap(trailing_ws(&changes[s].value), leading_ws(&dv)).to_string();
                changes[d].value = without_prefix(&dv, &overlap);
            }
            (None, None) => {}
        },
        (None, None) => {}
    }
}

/// `diffWords(old, new)`.
pub fn diff_words(old: &str, new: &str) -> Vec<Change> {
    let (old, new) = (tokenize(old), tokenize(new));
    let mut changes = session_file::diff::diff_tokens(&old, &new, equals, join);
    let (mut keep, mut ins, mut del) = (None, None, None);
    for i in 0..changes.len() {
        if changes[i].added {
            ins = Some(i);
        } else if changes[i].removed {
            del = Some(i);
        } else {
            if ins.is_some() || del.is_some() {
                dedupe(&mut changes, keep, del, ins, Some(i));
            }
            keep = Some(i);
            ins = None;
            del = None;
        }
    }
    if ins.is_some() || del.is_some() {
        dedupe(&mut changes, keep, del, ins, None);
    }
    changes
}
