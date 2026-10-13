//! Chunks and the find plan (`review_api.py` `build_chunks`, `applies`, `plan_find`, `_plan_report`): each
//! reviewable file's hunks rendered with new-file line numbers and the unchanged source around them, packed into
//! chunks of bounded size; then the (reviewer, chunk) finder frames that fit the find budget.

use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;

use crate::diff::{FileDiff, Hunk, Status, file_kind, render_hunk, skip_reason};
use crate::prompt_text::{REVIEWERS, Reviewer};
use crate::prompts::finder_task;
use crate::py;

pub const CHUNK_CHARS: usize = 14_000;
pub const CONTEXT_LINES: i64 = 12;
pub const FIND_SHARE: f64 = 0.6;

/// One finder input: a file's hunks (or part of them) as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub id: i64,
    pub path: String,
    /// `"code"`, `"test"` or `"doc"`.
    pub kind: &'static str,
    pub status: Status,
    pub hunks: Vec<Hunk>,
    pub text: String,
    pub part: usize,
    pub parts: usize,
}

impl Chunk {
    /// The view `normalize_findings` takes.
    pub fn as_finding_chunk(&self) -> crate::findings::Chunk<'_> {
        crate::findings::Chunk {
            id: self.id,
            path: &self.path,
            hunks: &self.hunks,
        }
    }
}

/// Python `len(text)`: code points.
pub fn chars(text: &str) -> usize {
    text.chars().count()
}

/// Unchanged source lines `[start, end]` (1-based, inclusive), gutter-numbered.
fn context(source: Option<&[String]>, start: i64, end: i64) -> Vec<String> {
    let Some(source) = source.filter(|source| !source.is_empty()) else {
        return Vec::new();
    };
    if end < start {
        return Vec::new();
    }
    let (start, end) = (start.max(1), end.min(source.len() as i64));
    (start..=end)
        .map(|number| {
            format!(
                "{number:>5}   {}",
                py::clip(&source[number as usize - 1], crate::diff::MAX_LINE_CHARS)
            )
        })
        .collect()
}

fn split_hunk(hunk: &Hunk, max_lines: usize) -> Vec<Hunk> {
    hunk.lines
        .chunks(max_lines)
        .map(|lines| {
            let news: Vec<u64> = lines.iter().filter_map(|line| line.new).collect();
            Hunk {
                old_start: hunk.old_start,
                old_len: 0,
                new_start: news.first().copied().unwrap_or(hunk.new_start),
                new_len: news.len() as u64,
                header: hunk.header.clone(),
                lines: lines.to_vec(),
            }
        })
        .collect()
}

fn status_name(status: Status) -> &'static str {
    match status {
        Status::Added => "added",
        Status::Deleted => "deleted",
        Status::Renamed => "renamed",
        Status::Modified => "modified",
    }
}

/// Render reviewable files as gutter-numbered chunks with surrounding source; return `(chunks, skipped)`.
/// `read_file` gives a file's current lines, or `None` when they are not available.
pub fn build_chunks(
    files: &[FileDiff],
    mut read_file: impl FnMut(&str) -> Option<Vec<String>>,
    max_chars: usize,
    context_lines: i64,
) -> (Vec<Chunk>, Vec<(String, String)>) {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut skipped = Vec::new();
    for item in files {
        if let Some(reason) = skip_reason(item) {
            skipped.push((item.path.clone(), reason.to_string()));
            continue;
        }
        let source = read_file(&item.path);
        let others: Vec<&str> = files
            .iter()
            .map(|other| other.path.as_str())
            .filter(|path| *path != item.path)
            .collect();
        let mut listing = others.iter().take(30).copied().collect::<Vec<_>>().join(", ");
        if others.len() > 30 {
            listing.push_str(&format!(" (+{} more)", others.len() - 30));
        }
        let max_lines = 20.max(max_chars / 120);
        let hunks: Vec<Hunk> = item
            .hunks
            .iter()
            .flat_map(|hunk| {
                if hunk.lines.len() <= max_lines {
                    vec![hunk.clone()]
                } else {
                    split_hunk(hunk, max_lines)
                }
            })
            .collect();
        let mut blocks: Vec<(&Hunk, String)> = Vec::new();
        for (position, hunk) in hunks.iter().enumerate() {
            let (first, last) = (hunk.new_first() as i64, hunk.new_last() as i64);
            let before_floor = if position > 0 {
                hunks[position - 1].new_last() as i64 + 1
            } else {
                1
            };
            let after_ceiling = hunks
                .get(position + 1)
                .map_or(last + context_lines, |next| next.new_first() as i64 - 1);
            let mut lines = vec![if hunk.header.is_empty() {
                "@@".to_string()
            } else {
                format!("@@ {}", hunk.header).trim_end_matches(py::is_space).to_string()
            }];
            let source = source.as_deref();
            lines.extend(context(source, before_floor.max(first - context_lines), first - 1));
            lines.extend(render_hunk(hunk));
            lines.extend(context(source, last + 1, after_ceiling.min(last + context_lines)));
            blocks.push((hunk, lines.join("\n")));
        }
        let mut groups: Vec<Vec<(&Hunk, String)>> = vec![Vec::new()];
        let mut size = 0;
        for block in blocks {
            let length = chars(&block.1);
            if !groups.last().expect("a group").is_empty() && size + length > max_chars {
                groups.push(Vec::new());
                size = 0;
            }
            groups.last_mut().expect("a group").push(block);
            size += length;
        }
        let parts = groups.len();
        for (part, group) in (1..).zip(groups) {
            let mut label = status_name(item.status).to_string();
            if parts > 1 {
                label.push_str(&format!(", part {part} of {parts}"));
            }
            let mut header = vec![format!("File: {} ({label})", item.path)];
            if let (Some(old), Status::Renamed) = (&item.old_path, item.status)
                && !old.is_empty()
            {
                header.push(format!("Renamed from: {old}"));
            }
            if !listing.is_empty() {
                header.push(format!("Other files changed in this review: {listing}"));
            }
            if source.is_none() && item.status != Status::Added {
                header.push("Surrounding source was not available; only the diff is shown.".to_string());
            }
            let text = format!(
                "{}\n\n{}",
                header.join("\n"),
                group
                    .iter()
                    .map(|(_, block)| block.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n")
            );
            chunks.push(Chunk {
                id: chunks.len() as i64 + 1,
                path: item.path.clone(),
                kind: file_kind(&item.path),
                status: item.status,
                hunks: group.into_iter().map(|(hunk, _)| hunk.clone()).collect(),
                text,
                part,
                parts,
            });
        }
    }
    (chunks, skipped)
}

static LLM_HINT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)openai|anthropic|\bllm|language model|chat[._]?completion|completions?\.create|messages\.create|\bprompt|max_tokens|temperature|embedding|gemini|ollama|bedrock|huggingface|rlm\.(infer|map|spawn)|tool_call|system_prompt",
    )
    .expect("valid regex")
});

/// Whether `reviewer` reads `chunk`: documentation only for reviewers that read docs, LLM-related code only for
/// the AI reviewer.
pub fn applies(reviewer: &Reviewer, chunk: &Chunk) -> bool {
    if chunk.kind == "doc" {
        return reviewer.reads_docs;
    }
    if reviewer.llm_only {
        return LLM_HINT.is_match(&chunk.text);
    }
    true
}

/// The token estimate for a frame: characters / 3 plus the expected output.
pub fn estimate_tokens(texts: &[&str], output: u64) -> u64 {
    texts.iter().map(|text| chars(text) as u64).sum::<u64>() / 3 + output
}

/// The finder frames that fit the find budget, as indices into the reviewer and chunk lists.
#[derive(Clone, Default)]
pub struct FindPlan {
    /// `(reviewer, chunk)` frames to run, grouped by reviewer (their prompts share a cacheable prefix).
    pub frames: Vec<(&'static Reviewer, usize)>,
    /// Frames over the budget.
    pub dropped: Vec<(&'static Reviewer, usize)>,
    /// Chunks each reviewer does not read, by reviewer key in first-seen order.
    pub not_applicable: IndexMap<&'static str, usize>,
    pub estimate: u64,
}

fn order(kind: &str) -> u8 {
    match kind {
        "code" => 0,
        "test" => 1,
        _ => 2,
    }
}

/// Pick `(reviewer, chunk)` frames that fit the find budget: code first, then tests, then docs.
pub fn plan_find(chunks: &[Chunk], reviewers: &[&'static Reviewer], budget_tokens: u64) -> FindPlan {
    let mut plan = FindPlan::default();
    let mut sorted: Vec<usize> = (0..chunks.len()).collect();
    sorted.sort_by_key(|&index| (order(chunks[index].kind), chunks[index].id));
    for index in sorted {
        let chunk = &chunks[index];
        for &reviewer in reviewers {
            if !applies(reviewer, chunk) {
                *plan.not_applicable.entry(reviewer.key).or_insert(0) += 1;
                continue;
            }
            let cost = estimate_tokens(&[&finder_task(reviewer), &chunk.text], 1_500);
            if plan.estimate + cost > budget_tokens {
                plan.dropped.push((reviewer, index));
                continue;
            }
            plan.estimate += cost;
            plan.frames.push((reviewer, index));
        }
    }
    let position = |reviewer: &Reviewer| REVIEWERS.iter().position(|known| known.key == reviewer.key);
    plan.frames
        .sort_by_key(|(reviewer, index)| (position(reviewer), chunks[*index].id));
    plan
}

/// The `--plan` report: chunks, the reviewers each gets, skipped files and frames over budget.
pub fn plan_report(
    label: &str,
    chunks: &[Chunk],
    plan: &FindPlan,
    skipped: &[(String, String)],
    budget_tokens: u64,
) -> String {
    let find_budget = (budget_tokens as f64 * FIND_SHARE) as u64;
    let mut out = vec![
        format!("# Review plan: {label}"),
        String::new(),
        format!(
            "{} chunks; {} finder frames, about {} tokens of the {}-token find budget.",
            chunks.len(),
            plan.frames.len(),
            py::thousands(plan.estimate as i64),
            py::thousands(find_budget as i64)
        ),
        String::new(),
    ];
    for (index, chunk) in chunks.iter().enumerate() {
        let keys: Vec<&str> = plan
            .frames
            .iter()
            .filter(|(_, item)| *item == index)
            .map(|(reviewer, _)| reviewer.key)
            .collect();
        let part = if chunk.parts > 1 {
            format!(" part {}/{}", chunk.part, chunk.parts)
        } else {
            String::new()
        };
        let keys = if keys.is_empty() {
            "none".to_string()
        } else {
            keys.join(", ")
        };
        out.push(format!(
            "- {}{part} ({}, {} chars): {keys}",
            chunk.path,
            chunk.kind,
            py::thousands(chars(&chunk.text) as i64)
        ));
    }
    for (path, reason) in skipped {
        out.push(format!("- skipped {path}: {reason}"));
    }
    if !plan.dropped.is_empty() {
        out.push(format!("- over budget: {} frames would not run", plan.dropped.len()));
    }
    out.join("\n") + "\n"
}
