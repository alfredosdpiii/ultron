//! `diffWords` from jsdiff 8.0.4 (BSD-3-Clause, Copyright (c) 2009-2015 Kevin Decker): Myers diff over word tokens that
//! carry their surrounding whitespace, then the whitespace dedupe pass. No options (no ignoreCase, no segmenter).

use tui::utils::is_whitespace_char as ws;

/// One change object: `added`/`removed` flags and the joined text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub value: String,
    pub added: bool,
    pub removed: bool,
}

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

struct Node {
    count: usize,
    added: bool,
    removed: bool,
    prev: Option<usize>,
}

#[derive(Clone, Copy)]
struct Path {
    old_pos: isize,
    last: Option<usize>,
}

struct Myers<'a> {
    nodes: Vec<Node>,
    old: &'a [String],
    new: &'a [String],
}

impl Myers<'_> {
    fn add_to_path(&mut self, path: Path, added: bool, removed: bool, inc: isize) -> Path {
        let node = match path.last.map(|i| &self.nodes[i]) {
            Some(l) if l.added == added && l.removed == removed => Node {
                count: l.count + 1,
                added,
                removed,
                prev: l.prev,
            },
            _ => Node {
                count: 1,
                added,
                removed,
                prev: path.last,
            },
        };
        self.nodes.push(node);
        Path {
            old_pos: path.old_pos + inc,
            last: Some(self.nodes.len() - 1),
        }
    }

    fn extract_common(&mut self, path: &mut Path, diagonal: isize) -> isize {
        let (new_len, old_len) = (self.new.len() as isize, self.old.len() as isize);
        let mut old_pos = path.old_pos;
        let mut new_pos = old_pos - diagonal;
        let mut common = 0;
        while new_pos + 1 < new_len
            && old_pos + 1 < old_len
            && equals(&self.old[(old_pos + 1) as usize], &self.new[(new_pos + 1) as usize])
        {
            new_pos += 1;
            old_pos += 1;
            common += 1;
        }
        if common > 0 {
            self.nodes.push(Node {
                count: common,
                added: false,
                removed: false,
                prev: path.last,
            });
            path.last = Some(self.nodes.len() - 1);
        }
        path.old_pos = old_pos;
        new_pos
    }

    fn build(&self, last: Option<usize>) -> Vec<Change> {
        let mut chain = Vec::new();
        let mut cur = last;
        while let Some(i) = cur {
            chain.push(i);
            cur = self.nodes[i].prev;
        }
        chain.reverse();
        let (mut new_pos, mut old_pos) = (0, 0);
        chain
            .into_iter()
            .map(|i| {
                let n = &self.nodes[i];
                let value = if n.removed {
                    old_pos += n.count;
                    join(&self.old[old_pos - n.count..old_pos])
                } else {
                    new_pos += n.count;
                    if !n.added {
                        old_pos += n.count;
                    }
                    join(&self.new[new_pos - n.count..new_pos])
                };
                Change {
                    value,
                    added: n.added,
                    removed: n.removed,
                }
            })
            .collect()
    }

    fn run(&mut self) -> Vec<Change> {
        let (new_len, old_len) = (self.new.len() as isize, self.old.len() as isize);
        let max_edit = new_len + old_len;
        let offset = max_edit + 1;
        let mut best: Vec<Option<Path>> = vec![None; (2 * offset + 1) as usize];
        let mut first = Path {
            old_pos: -1,
            last: None,
        };
        let new_pos = self.extract_common(&mut first, 0);
        if first.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
            return self.build(first.last);
        }
        best[offset as usize] = Some(first);
        let (mut min_diag, mut max_diag) = (isize::MIN, isize::MAX);
        for edit in 1..=max_edit {
            let mut diagonal = min_diag.max(-edit);
            while diagonal <= max_diag.min(edit) {
                let idx = |d: isize| (d + offset) as usize;
                let remove = best[idx(diagonal - 1)].take();
                let add = best[idx(diagonal + 1)];
                let can_add = add.is_some_and(|a| {
                    let p = a.old_pos - diagonal;
                    0 <= p && p < new_len
                });
                let can_remove = remove.is_some_and(|r| r.old_pos + 1 < old_len);
                if !can_add && !can_remove {
                    best[idx(diagonal)] = None;
                    diagonal += 2;
                    continue;
                }
                let mut base = match (add, remove) {
                    (Some(a), r) if !can_remove || r.is_some_and(|r| can_add && r.old_pos < a.old_pos) => {
                        self.add_to_path(a, true, false, 0)
                    }
                    (_, Some(r)) => self.add_to_path(r, false, true, 1),
                    (Some(a), None) => self.add_to_path(a, true, false, 0),
                    (None, None) => unreachable!("one of the paths can extend"),
                };
                let new_pos = self.extract_common(&mut base, diagonal);
                if base.old_pos + 1 >= old_len && new_pos + 1 >= new_len {
                    return self.build(base.last);
                }
                best[idx(diagonal)] = Some(base);
                if base.old_pos + 1 >= old_len {
                    max_diag = max_diag.min(diagonal - 1);
                }
                if new_pos + 1 >= new_len {
                    min_diag = min_diag.max(diagonal + 1);
                }
                diagonal += 2;
            }
        }
        Vec::new()
    }
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
    let mut changes = Myers {
        nodes: Vec::new(),
        old: &old,
        new: &new,
    }
    .run();
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
