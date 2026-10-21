//! Cherry-picks and promotions (`autoreview/promotion.ts`): a pull request that carries already merged pull requests
//! to another branch (fisher-ki-saas#2945 cherry-picks #2931, #2941, #2936 and #2937 to `soc2-staging`: +43,060
//! lines, reviewed line by line for 4 to 6 hours). Their lines were reviewed where they merged; what needs a review is
//! what differs from them: conflict resolutions, adaptations, and what was left out.
//!
//! The sources are the commits named by `git cherry-pick -x` trailers in the pull request's commits, and the merged
//! pull requests its title or description names that merged into another base branch. [`promotion_delta`] keeps the
//! pull request's changed lines that no source changes the same way in the same file (with their context), and lists
//! the lines the sources add that the head does not have. Pure functions over diff texts.

use std::collections::HashSet;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde::Serialize;

/// One source: a label for the review body (`#2931`, or a short commit) and its diff (first parent to commit).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PromotionSource {
    pub label: String,
    /// The source's commit, and its pull request when the description named it.
    pub sha: String,
    pub pull: Option<u64>,
    pub diff: String,
    /// Files that differ between the source's first parent and the pull request's merge base: there the source
    /// was reviewed against other code than the lines land in (fisher-ki-saas#2951: staging is not dev), so the
    /// pull request's changes to them are read in full.
    pub context_differs: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Missing {
    pub path: String,
    pub lines: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromotionDelta {
    /// The pull request diff without the lines the sources change the same way.
    pub diff: String,
    /// The sources that the pull request carries (most of their added lines are in it), in order.
    pub sources: Vec<String>,
    /// Changed lines (added and removed) that match a source.
    pub carried_lines: u64,
    /// Files of the pull request whose every change matches a source; left out of `diff`.
    pub carried_files: u64,
    /// Files read in full although they match a source: the code around them differs on this branch.
    pub context_files: u64,
    /// Lines a source adds that no source removes and the head does not have, in files the pull request leaves
    /// out of `diff` (a file it reads shows its adaptations there): the most first.
    pub missing: Vec<Missing>,
}

/// A commit of the pull request: its sha, subject and full message.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CommitMessage {
    pub sha: String,
    pub subject: String,
    pub message: String,
}

/// `git log --format=%H%x1f%s%x1f%B%x1e` output as commits.
pub fn parse_commits(out: &str) -> Vec<CommitMessage> {
    out.split('\x1e')
        .filter_map(|record| {
            let record = record.trim_start_matches('\n');
            let mut fields = record.splitn(3, '\x1f');
            let (sha, subject, message) = (fields.next()?, fields.next()?, fields.next()?);
            Some(CommitMessage {
                sha: sha.to_string(),
                subject: subject.to_string(),
                message: message.to_string(),
            })
        })
        .collect()
}

/// Context lines kept around each changed line that does not match a source, as `git diff -U3`.
pub const PROMOTION_CONTEXT: usize = 3;
/// A source counts when at least this share of its added lines are in the pull request.
pub const MIN_CARRIED_SHARE: f64 = 0.5;
/// Lines shorter than this (trimmed) are not listed as missing: blank lines, braces.
const MIN_MISSING_CHARS: usize = 3;
/// Merged pull requests read from a title and description at most.
pub const MAX_REFERENCED_PULLS: usize = 20;

static TRAILER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(cherry picked from commit ([0-9a-f]{7,64})\)").expect("static pattern"));
static HUNK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$").expect("static pattern"));
static GIT_PATHS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" a/(.*) b/(.*)$").expect("static pattern"));
static REFERENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[^A-Za-z0-9_/#-])#([0-9]+)").expect("static pattern"));
static MERGE_SUBJECT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Merge pull request #([0-9]+) ").expect("static pattern"));
static SQUASH_SUBJECT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\(#([0-9]+)\)[ \t]*$").expect("static pattern"));

/// The commit each picked commit names: its last `(cherry picked from commit X)` trailer, the nearest source.
pub fn cherry_sources(commits: &[CommitMessage]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for commit in commits {
        let source = TRAILER
            .captures_iter(&commit.message)
            .last()
            .map(|captures| captures[1].to_string());
        if let Some(source) = source
            && !out.contains(&source)
        {
            out.push(source);
        }
    }
    out
}

/// The label of a picked commit: the pull request its subject names (`Merge pull request #N`, `... (#N)`).
pub fn source_label(subject: &str, sha: &str) -> String {
    let number = MERGE_SUBJECT
        .captures(subject)
        .or_else(|| SQUASH_SUBJECT.captures(subject))
        .map(|captures| captures[1].to_string());
    match number {
        Some(number) => format!("#{number}"),
        None => format!("`{}`", sha.chars().take(7).collect::<String>()),
    }
}

/// Pull requests of the same repository that a title and description name (`#N`, not `owner/repo#N`).
pub fn referenced_pulls(text: &str, own: u64) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    for captures in REFERENCE.captures_iter(text) {
        let digits = captures.get(2).expect("group");
        // As the TypeScript `#([0-9]{1,7})(?![A-Za-z0-9_])`: a number of at most 7 digits, not part of a word.
        let next = text[digits.end()..].chars().next();
        if digits.as_str().len() > 7 || next.is_some_and(|c| c.is_ascii_alphabetic() || c == '_') {
            continue;
        }
        let Ok(number) = digits.as_str().parse::<u64>() else {
            continue;
        };
        if number != own && !out.contains(&number) {
            out.push(number);
        }
        if out.len() >= MAX_REFERENCED_PULLS {
            break;
        }
    }
    out
}

struct Hunk {
    old_start: i64,
    new_start: i64,
    tail: String,
    lines: Vec<String>,
}

struct FileDiff {
    header: Vec<String>,
    old_path: String,
    new_path: String,
    hunks: Vec<Hunk>,
}

fn strip_prefix(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

fn parse_diff(text: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    for line in lines {
        if line.starts_with("diff --git ") {
            let paths = GIT_PATHS.captures(line);
            let path = |group: usize| paths.as_ref().map_or(String::new(), |c| c[group].to_string());
            files.push(FileDiff {
                header: vec![line.to_string()],
                old_path: path(1),
                new_path: path(2),
                hunks: Vec::new(),
            });
            continue;
        }
        let Some(file) = files.last_mut() else {
            continue;
        };
        if let Some(header) = HUNK.captures(line) {
            file.hunks.push(Hunk {
                old_start: header[1].parse().unwrap_or(0),
                new_start: header[3].parse().unwrap_or(0),
                tail: header[5].to_string(),
                lines: Vec::new(),
            });
            continue;
        }
        match file.hunks.last_mut() {
            Some(hunk) => hunk.lines.push(line.to_string()),
            None => {
                file.header.push(line.to_string());
                if let Some(path) = line.strip_prefix("--- ").filter(|_| line != "--- /dev/null") {
                    file.old_path = strip_prefix(path);
                }
                if let Some(path) = line.strip_prefix("+++ ").filter(|_| line != "+++ /dev/null") {
                    file.new_path = strip_prefix(path);
                }
            }
        }
    }
    files
}

type Counts = IndexMap<String, IndexMap<String, u64>>;

fn add(counts: &mut Counts, path: &str, line: &str, n: u64) {
    *counts
        .entry(path.to_string())
        .or_default()
        .entry(line.to_string())
        .or_insert(0) += n;
}

/// Take one `line` of `path` from the counts: whether there was one.
fn take(counts: &mut Counts, path: &str, line: &str) -> bool {
    match counts.get_mut(path).and_then(|lines| lines.get_mut(line)) {
        Some(count) if *count > 0 => {
            *count -= 1;
            true
        }
        _ => false,
    }
}

fn changes(files: &[FileDiff]) -> (Counts, Counts) {
    let (mut added, mut removed) = (Counts::new(), Counts::new());
    for file in files {
        for hunk in &file.hunks {
            for line in &hunk.lines {
                if let Some(text) = line.strip_prefix('+') {
                    add(&mut added, &file.new_path, text, 1);
                } else if let Some(text) = line.strip_prefix('-') {
                    add(&mut removed, &file.old_path, text, 1);
                }
            }
        }
    }
    (added, removed)
}

fn total(counts: &Counts) -> u64 {
    counts.values().flat_map(|lines| lines.values()).sum()
}

fn old_side(line: &str) -> bool {
    !line.starts_with('+') && !line.starts_with('\\')
}

fn new_side(line: &str) -> bool {
    !line.starts_with('-') && !line.starts_with('\\')
}

/// The hunk's lines at `[from, to)` as a hunk of their own, its header recounted.
fn sub_hunk(hunk: &Hunk, from: usize, to: usize) -> Vec<String> {
    let before = &hunk.lines[..from];
    let old_line = hunk.old_start + before.iter().filter(|line| old_side(line)).count() as i64;
    let new_line = hunk.new_start + before.iter().filter(|line| new_side(line)).count() as i64;
    let body = &hunk.lines[from..to];
    let old_count = body.iter().filter(|line| old_side(line)).count();
    let new_count = body.iter().filter(|line| new_side(line)).count();
    let tail = if from == 0 { hunk.tail.as_str() } else { "" };
    std::iter::once(format!("@@ -{old_line},{old_count} +{new_line},{new_count} @@{tail}"))
        .chain(body.iter().cloned())
        .collect()
}

/// The pull request diff less what the sources change the same way. `head_lines(path)` reads a file of the head
/// (for what is missing); `None` when it is not there.
pub fn promotion_delta(
    pull_diff: &str,
    sources: &[PromotionSource],
    head_lines: &dyn Fn(&str) -> Option<Vec<String>>,
) -> PromotionDelta {
    let files = parse_diff(pull_diff);
    let (pull_added, _) = changes(&files);
    // A source the pull request does not carry (named for context only) is left out: its lines are not missing.
    let mut carried: Vec<(String, Counts, Counts)> = Vec::new();
    let mut recontext: HashSet<String> = HashSet::new();
    for source in sources {
        let (added, removed) = changes(&parse_diff(&source.diff));
        let all = total(&added);
        if all == 0 {
            continue;
        }
        let found: u64 = added
            .iter()
            .flat_map(|(path, lines)| {
                let pull = pull_added.get(path);
                lines
                    .iter()
                    .map(move |(line, count)| (*count).min(pull.and_then(|p| p.get(line)).copied().unwrap_or(0)))
            })
            .sum();
        if found as f64 >= all as f64 * MIN_CARRIED_SHARE {
            recontext.extend(
                added
                    .keys()
                    .chain(removed.keys())
                    .filter(|path| source.context_differs.contains(*path))
                    .cloned(),
            );
            carried.push((source.label.clone(), added, removed));
        }
    }
    if carried.is_empty() {
        return PromotionDelta {
            diff: pull_diff.to_string(),
            sources: Vec::new(),
            carried_lines: 0,
            carried_files: 0,
            context_files: 0,
            missing: Vec::new(),
        };
    }
    let mut labels: Vec<String> = Vec::new();
    let (mut added, mut removed) = (Counts::new(), Counts::new());
    for (label, source_added, source_removed) in &carried {
        if !labels.contains(label) {
            labels.push(label.clone());
        }
        for (path, lines) in source_added {
            for (line, count) in lines {
                add(&mut added, path, line, *count);
            }
        }
        for (path, lines) in source_removed {
            for (line, count) in lines {
                add(&mut removed, path, line, *count);
            }
        }
    }
    let touched: HashSet<String> = added.keys().chain(removed.keys()).cloned().collect();
    // A line one source adds and a later one replaces (a chain of follow-ups) is not missing.
    let removed_by_sources = removed.clone();
    let mut read: HashSet<&str> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let (mut carried_lines, mut carried_files, mut context_files) = (0u64, 0u64, 0u64);
    for file in &files {
        if recontext.contains(&file.new_path) || recontext.contains(&file.old_path) {
            // Reviewed against other surrounding code: read as the pull request has it.
            context_files += 1;
            read.insert(&file.new_path);
            out.extend(file.header.iter().cloned());
            for hunk in &file.hunks {
                out.extend(sub_hunk(hunk, 0, hunk.lines.len()));
            }
            continue;
        }
        if file.hunks.is_empty() {
            // A rename, mode change or binary file: carried when a source touches the same path.
            if touched.contains(&file.new_path) || touched.contains(&file.old_path) {
                carried_files += 1;
            } else {
                out.extend(file.header.iter().cloned());
            }
            continue;
        }
        let mut kept: Vec<String> = Vec::new();
        for hunk in &file.hunks {
            let mut open: Vec<usize> = Vec::new();
            for (index, line) in hunk.lines.iter().enumerate() {
                let matched = if let Some(text) = line.strip_prefix('+') {
                    Some(take(&mut added, &file.new_path, text))
                } else {
                    line.strip_prefix('-')
                        .map(|text| take(&mut removed, &file.old_path, text))
                };
                match matched {
                    Some(true) => carried_lines += 1,
                    Some(false) => open.push(index),
                    None => {}
                }
            }
            // Windows of context around each unmatched line, merged where they touch.
            let mut window: Option<(usize, usize)> = None;
            for index in open {
                let from = index.saturating_sub(PROMOTION_CONTEXT);
                let to = (index + PROMOTION_CONTEXT + 1).min(hunk.lines.len());
                window = match window {
                    Some((start, end)) if from <= end => Some((start, end.max(to))),
                    Some((start, end)) => {
                        kept.extend(sub_hunk(hunk, start, end));
                        Some((from, to))
                    }
                    None => Some((from, to)),
                };
            }
            if let Some((start, end)) = window {
                kept.extend(sub_hunk(hunk, start, end));
            }
        }
        if kept.is_empty() {
            carried_files += 1;
        } else {
            read.insert(&file.new_path);
            out.extend(file.header.iter().cloned());
            out.extend(kept);
        }
    }
    let mut missing: Vec<Missing> = Vec::new();
    for (path, lines) in &added {
        if read.contains(path.as_str()) || lines.values().all(|left| *left == 0) {
            continue;
        }
        let head: HashSet<String> = head_lines(path).unwrap_or_default().into_iter().collect();
        let replaced = removed_by_sources.get(path);
        let count: u64 = lines
            .iter()
            .map(|(line, left)| {
                let replaced = replaced.and_then(|lines| lines.get(line)).copied().unwrap_or(0);
                (line, left.saturating_sub(replaced))
            })
            .filter(|(line, left)| {
                *left > 0 && line.trim().chars().count() >= MIN_MISSING_CHARS && !head.contains(*line)
            })
            .map(|(_, left)| left)
            .sum();
        if count > 0 {
            missing.push(Missing {
                path: path.clone(),
                lines: count,
            });
        }
    }
    missing.sort_by(|a, b| b.lines.cmp(&a.lines).then_with(|| a.path.cmp(&b.path)));
    PromotionDelta {
        diff: if out.is_empty() {
            String::new()
        } else {
            format!("{}\n", out.join("\n"))
        },
        sources: labels,
        carried_lines,
        carried_files,
        context_files,
        missing,
    }
}
