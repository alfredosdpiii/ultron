//! Autoreview's finder slices (`autoreview_api.py` `pack_chunks`, `merged_reviewers`, `member_for`,
//! `describe_failure`): file chunks packed into slices of one kind, the architecture specialist folded into the
//! correctness one on small diffs, and the file chunk a finding of a slice is about.

use indexmap::IndexSet;
use serde_json::Value;

use crate::chunks::{Chunk, chars};
use crate::diff::Status;
use crate::findings::int_of;
use crate::levels::DEADLINE_ERROR;
use crate::prompt_text::Reviewer;
use crate::prompts::{autoreview_finder_task_for, finder_task_for};
use crate::py;

const OTHER_FILES: &str = "Other files changed in this review: ";
/// Diffs under this many changed lines go to each finder as one slice.
pub const SINGLE_SLICE_LINES: usize = 150;
/// Under this many changed lines the architecture specialist is folded into the correctness one.
pub const MERGE_ARCH_LINES: usize = 150;

pub fn slice_separator() -> String {
    format!("\n\n{}\n\n", "=".repeat(40))
}

fn kind_order(kind: &str) -> u8 {
    match kind {
        "code" => 0,
        "test" => 1,
        _ => 2,
    }
}

/// Pack file chunks into slices of up to `max_chars`: files of one kind (code, tests, docs) in path order, a
/// file's chunk never split further; with `single`, everything into one slice. Returns the slices (as chunks, for
/// planning) and each slice's members (indices into `chunks`).
pub fn pack_chunks(
    chunks: &[Chunk],
    changed: &[String],
    max_chars: usize,
    single: bool,
) -> (Vec<Chunk>, Vec<Vec<usize>>) {
    let mut listing = changed.iter().take(30).cloned().collect::<Vec<_>>().join(", ");
    if changed.len() > 30 {
        listing.push_str(&format!(" (+{} more)", changed.len() - 30));
    }
    let header = if changed.len() > 1 {
        format!("Files changed in this pull request: {listing}")
    } else {
        String::new()
    };
    let mut order: Vec<usize> = (0..chunks.len()).collect();
    order.sort_by(|&a, &b| {
        let (a, b) = (&chunks[a], &chunks[b]);
        (kind_order(a.kind), &a.path, a.part).cmp(&(kind_order(b.kind), &b.path, b.part))
    });
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut size = 0;
    for index in order {
        let body = chars(&chunks[index].text);
        let joins = groups
            .last()
            .is_some_and(|group| single || (chunks[group[0]].kind == chunks[index].kind && size + body <= max_chars));
        if joins {
            groups.last_mut().expect("a group").push(index);
            size += body;
        } else {
            groups.push(vec![index]);
            size = body;
        }
    }
    let separator = slice_separator();
    let slices = groups
        .iter()
        .enumerate()
        .map(|(number, group)| {
            let texts: Vec<String> = group
                .iter()
                .map(|&index| {
                    chunks[index]
                        .text
                        .split('\n')
                        .filter(|line| !line.starts_with(OTHER_FILES))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .collect();
            let mut text = if header.is_empty() {
                String::new()
            } else {
                format!("{header}\n\n")
            };
            text.push_str(&texts.join(&separator));
            let paths: IndexSet<&str> = group.iter().map(|&index| chunks[index].path.as_str()).collect();
            let mut label = paths.iter().take(3).copied().collect::<Vec<_>>().join(", ");
            if paths.len() > 3 {
                label.push_str(&format!(" and {} more", paths.len() - 3));
            }
            Chunk {
                id: number as i64 + 1,
                path: label,
                kind: chunks[group[0]].kind,
                status: Status::Modified,
                hunks: group.iter().flat_map(|&index| chunks[index].hunks.clone()).collect(),
                text,
                part: 1,
                parts: 1,
            }
        })
        .collect();
    (slices, groups)
}

/// A finder specialist with owned parts: a reviewer, or the correctness reviewer with architecture folded in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Specialist {
    pub key: &'static str,
    pub title: String,
    pub focus: String,
    pub checklist: Vec<String>,
    pub reads_docs: bool,
    pub llm_only: bool,
}

impl From<&Reviewer> for Specialist {
    fn from(reviewer: &Reviewer) -> Self {
        Self {
            key: reviewer.key,
            title: reviewer.title.to_string(),
            focus: reviewer.focus.to_string(),
            checklist: reviewer.checklist.iter().map(|item| item.to_string()).collect(),
            reads_docs: reviewer.reads_docs,
            llm_only: reviewer.llm_only,
        }
    }
}

impl Specialist {
    pub fn finder_task(&self) -> String {
        finder_task_for(&self.title, &self.focus, &self.checklist)
    }

    pub fn autoreview_finder_task(&self) -> String {
        autoreview_finder_task_for(&self.title, &self.focus, &self.checklist)
    }
}

/// Under [`MERGE_ARCH_LINES`] changed lines the architecture specialist rides with the correctness one: one frame
/// with both checklists (its findings keep their own categories).
pub fn merged_reviewers(reviewers: &[&Reviewer], changed_lines: usize) -> Vec<Specialist> {
    let arch = reviewers.iter().find(|item| item.key == "arch");
    let has_bugs = reviewers.iter().any(|item| item.key == "bugs");
    let all = || reviewers.iter().map(|item| Specialist::from(*item)).collect();
    let Some(arch) = arch.filter(|_| changed_lines < MERGE_ARCH_LINES && has_bugs) else {
        return all();
    };
    let mut out = Vec::new();
    for item in reviewers {
        if item.key == "arch" {
            continue;
        }
        let mut specialist = Specialist::from(*item);
        if item.key == "bugs" {
            specialist.title = format!("{}; also {}", item.title, arch.title.to_lowercase());
            specialist.focus = format!("{} Also: {}", item.focus, arch.focus);
            specialist
                .checklist
                .extend(arch.checklist.iter().map(|line| line.to_string()));
        }
        out.push(specialist);
    }
    out
}

/// The file chunk of a slice (`group`, indices into `chunks`) that a finder's finding is about, by the path it
/// names; `None` when the path is none of the slice's files and the slice has several.
pub fn member_for(chunks: &[Chunk], group: &[usize], raw: &Value) -> Option<usize> {
    let paths: IndexSet<&str> = group.iter().map(|&index| chunks[index].path.as_str()).collect();
    let mut named = match raw {
        Value::Object(raw) if crate::levels::truthy(raw.get("file")) => {
            let file = raw.get("file").map(py::str_of).unwrap_or_default();
            py::strip(&file).trim_matches(['`', '"', '\'']).to_string()
        }
        _ => String::new(),
    };
    for prefix in ["./", "a/", "b/"] {
        if named.starts_with(prefix) && !paths.contains(named.as_str()) {
            named = named[prefix.len()..].to_string();
        }
    }
    let mut path = paths.get(named.as_str()).copied();
    if path.is_none() && !named.is_empty() {
        let close: Vec<&str> = paths
            .iter()
            .copied()
            .filter(|item| item.ends_with(&format!("/{named}")) || named.ends_with(&format!("/{item}")))
            .collect();
        path = (close.len() == 1).then(|| close[0]);
    }
    let path = match path {
        Some(path) => path,
        None if paths.len() == 1 => paths[0],
        None => return None,
    };
    let parts: Vec<usize> = group
        .iter()
        .copied()
        .filter(|&index| chunks[index].path == path)
        .collect();
    let line = match raw {
        Value::Object(raw) => int_of(raw.get("line")),
        _ => None,
    };
    let Some(line) = line else {
        return parts.first().copied();
    };
    let distance = |index: usize| -> i64 {
        chunks[index]
            .hunks
            .iter()
            .map(|hunk| {
                let (first, last) = (hunk.new_first() as i64, hunk.new_last() as i64);
                if first <= line && line <= last {
                    0
                } else {
                    (first - line).abs().min((last - line).abs())
                }
            })
            .min()
            .unwrap_or(0)
    };
    parts.into_iter().min_by_key(|&index| distance(index))
}

/// Why a frame produced nothing, for the list of what was not checked: it ran out (`incomplete`, its status) or
/// failed (`error`).
pub fn describe_failure(incomplete: Option<&str>, error: Option<&str>) -> Option<String> {
    if let Some(status) = incomplete {
        return Some(format!("ran out ({status})"));
    }
    let error = error?;
    if error == DEADLINE_ERROR {
        return Some("were not finished at the review deadline".to_string());
    }
    Some(format!(
        "failed ({})",
        crate::findings::text(Some(&Value::from(error)), 120)
    ))
}
