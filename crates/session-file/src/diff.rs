//! Text diffs from jsdiff 8.0.4 (BSD-3-Clause, Copyright (c) 2009-2015 Kevin Decker): the Myers diff of `Diff.diff`
//! over any tokens ([`diff_tokens`]), `diffLines` ([`diff_lines`]), and the display diff the TypeScript edit tool
//! stores in a tool result's `details.diff` (`generateDiffString` of core/tools/edit-diff.ts,
//! [`generate_diff_string`]), which the interactive mode's `renderDiff` draws.

/// One change object: `added`/`removed` flags and the joined text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub value: String,
    pub added: bool,
    pub removed: bool,
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
    equals: fn(&str, &str) -> bool,
    join: fn(&[String]) -> String,
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
            && (self.equals)(&self.old[(old_pos + 1) as usize], &self.new[(new_pos + 1) as usize])
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
                    (self.join)(&self.old[old_pos - n.count..old_pos])
                } else {
                    new_pos += n.count;
                    if !n.added {
                        old_pos += n.count;
                    }
                    (self.join)(&self.new[new_pos - n.count..new_pos])
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

/// `Diff.diff` over `old` and `new` tokens (empty tokens already removed) with the subclass's `equals` and `join`.
pub fn diff_tokens(
    old: &[String],
    new: &[String],
    equals: fn(&str, &str) -> bool,
    join: fn(&[String]) -> String,
) -> Vec<Change> {
    Myers {
        nodes: Vec::new(),
        old,
        new,
        equals,
        join,
    }
    .run()
}

/// `lineDiff.tokenize` without options: lines with their `\n` (a `\r` stays on its line), no final empty line.
fn line_tokens(value: &str) -> Vec<String> {
    let mut lines: Vec<String> = value.split_inclusive('\n').map(str::to_string).collect();
    lines.retain(|line| !line.is_empty());
    lines
}

/// `diffLines(old, new)` without options.
pub fn diff_lines(old: &str, new: &str) -> Vec<Change> {
    diff_tokens(
        &line_tokens(old),
        &line_tokens(new),
        |a, b| a == b,
        |tokens| tokens.concat(),
    )
}

/// `generateDiffString(old, new, contextLines)`: `+N line` / `-N line` / ` N line` rows with line numbers padded to
/// the widest, `contextLines` of context around changes and ` <pad> ...` where context was skipped; and the first
/// changed line (in the new file).
pub fn generate_diff_string(old: &str, new: &str, context_lines: usize) -> (String, Option<usize>) {
    let parts = diff_lines(old, new);
    let mut output: Vec<String> = Vec::new();
    let max_line = old.split('\n').count().max(new.split('\n').count());
    let width = max_line.to_string().len();
    let (mut old_line, mut new_line) = (1usize, 1usize);
    let mut last_was_change = false;
    let mut first_changed = None;
    let skip = format!(" {} ...", " ".repeat(width));
    for (i, part) in parts.iter().enumerate() {
        let mut raw: Vec<&str> = part.value.split('\n').collect();
        if raw.last() == Some(&"") {
            raw.pop();
        }
        if part.added || part.removed {
            first_changed.get_or_insert(new_line);
            for line in raw {
                if part.added {
                    output.push(format!("+{new_line:>width$} {line}"));
                    new_line += 1;
                } else {
                    output.push(format!("-{old_line:>width$} {line}"));
                    old_line += 1;
                }
            }
            last_was_change = true;
            continue;
        }
        let next_is_change = parts.get(i + 1).is_some_and(|next| next.added || next.removed);
        let context = |output: &mut Vec<String>, lines: &[&str], old_line: &mut usize, new_line: &mut usize| {
            for line in lines {
                output.push(format!(" {old_line:>width$} {line}"));
                *old_line += 1;
                *new_line += 1;
            }
        };
        match (last_was_change, next_is_change) {
            (true, true) if raw.len() <= context_lines * 2 => context(&mut output, &raw, &mut old_line, &mut new_line),
            (true, true) => {
                let skipped = raw.len() - 2 * context_lines;
                context(&mut output, &raw[..context_lines], &mut old_line, &mut new_line);
                output.push(skip.clone());
                old_line += skipped;
                new_line += skipped;
                context(
                    &mut output,
                    &raw[raw.len() - context_lines..],
                    &mut old_line,
                    &mut new_line,
                );
            }
            (true, false) => {
                let shown = raw.len().min(context_lines);
                context(&mut output, &raw[..shown], &mut old_line, &mut new_line);
                let skipped = raw.len() - shown;
                if skipped > 0 {
                    output.push(skip.clone());
                    old_line += skipped;
                    new_line += skipped;
                }
            }
            (false, true) => {
                let skipped = raw.len().saturating_sub(context_lines);
                if skipped > 0 {
                    output.push(skip.clone());
                    old_line += skipped;
                    new_line += skipped;
                }
                context(&mut output, &raw[skipped..], &mut old_line, &mut new_line);
            }
            (false, false) => {
                old_line += raw.len();
                new_line += raw.len();
            }
        }
        last_was_change = false;
    }
    (output.join("\n"), first_changed)
}
