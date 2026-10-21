//! From an engine result to what is posted (`autoreview/plan.ts`, levels from `types.ts`): the verdict, a few inline
//! comments, and the body. Pure functions; nothing here talks to GitHub.
//!
//! - Verdict: REQUEST_CHANGES when a confirmed finding is at or above `blockAt`, or an earlier one is still present
//!   at that level; APPROVE only when coverage was complete; otherwise COMMENT. The account's own pull request, and
//!   a closed or merged one, always get COMMENT.
//! - Few, heavy comments, ranked by level and evidence strength; nits are never inline.
//! - An inline comment sits on its line or the nearest diff line within three; else the body names it.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::js::{flatten, head16, len16, number, round, to_fixed};
use crate::settings::SIGNATURE;

pub const LEVELS: [&str; 5] = ["critical", "high", "medium", "low", "nit"];
pub const DEFAULT_BLOCK_AT: &str = "medium";
pub const DEFAULT_MAX_COMMENTS: i64 = 5;
pub const RELOCATE_WITHIN: i64 = 3;
/// An inline comment's sentences (before any code block) aim to stay under this.
pub const COMMENT_TARGET_CHARS: usize = 600;
const MAX_BLOCKING_LISTED: usize = 4;
const MAX_OUTSIDE_LISTED: usize = 3;
const MAX_NOT_CHECKED: usize = 3;
const MAX_BODY_LINES: usize = 60;
const MAX_SOURCES_LISTED: usize = 6;
const MAX_MISSING_LISTED: usize = 6;
const STRENGTHS: [&str; 3] = ["test", "outside", "diff"];

/// A finding's level: its `level`, or its old-scale `severity` (blocker, major, minor) mapped to one; else `low`.
pub fn level_of(level: Option<&str>, severity: Option<&str>) -> &'static str {
    for name in [level, severity].into_iter().flatten() {
        if let Some(level) = LEVELS.iter().find(|item| **item == name) {
            return level;
        }
        match name {
            "blocker" => return "critical",
            "major" => return "high",
            "minor" => return "low",
            _ => {}
        }
    }
    "low"
}

/// A level on the old scale; medium has no equal there and is written as `minor`.
pub fn severity_of(level: &str) -> &'static str {
    match level {
        "critical" => "blocker",
        "high" => "major",
        "nit" => "nit",
        _ => "minor",
    }
}

fn rank(level: &str) -> usize {
    LEVELS.iter().position(|item| *item == level).unwrap_or(LEVELS.len())
}

fn at_or_above(level: &str, threshold: &str) -> bool {
    rank(level) <= rank(threshold)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Place {
    pub file: String,
    pub line: i64,
}

/// The fields of an engine finding the plan reads.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineFinding {
    pub file: String,
    pub line: i64,
    pub end_line: Option<i64>,
    pub level: Option<String>,
    pub severity: Option<String>,
    /// `test`, `outside` or `diff` (absent: `diff`).
    pub strength: Option<String>,
    pub also_at: Option<Vec<Place>>,
    pub scenario: Option<String>,
    #[serde(default)]
    pub category: String,
    pub claim: String,
    #[serde(default)]
    pub why: String,
    pub suggested_fix: Option<String>,
    /// The exact new text of lines `line..endLine`, when the fix is a drop-in replacement.
    pub replacement: Option<String>,
    pub source: Option<String>,
    pub lesson: Option<String>,
    /// `confirmed` or `uncertain`.
    pub verification: String,
}

impl EngineFinding {
    pub fn level(&self) -> &'static str {
        level_of(self.level.as_deref(), self.severity.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EarlierStatus {
    pub file: String,
    pub line: i64,
    pub level: Option<String>,
    pub severity: Option<String>,
    /// `fixed`, `still_present`, `not_applicable` or `unknown`.
    pub status: String,
    #[serde(default)]
    pub claim: String,
    /// What the re-check saw (the quoted line).
    #[serde(default)]
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AlsoRaised {
    pub by: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Dropped {
    #[serde(default)]
    pub rejected: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Timing {
    pub total_ms: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub cost_usd: f64,
    pub frames: f64,
    pub tokens: f64,
}

/// The fields of an engine result the plan reads.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineResult {
    pub complete: bool,
    pub files: i64,
    pub added: i64,
    pub removed: i64,
    pub findings: Vec<EngineFinding>,
    #[serde(default)]
    pub also_raised: Vec<AlsoRaised>,
    #[serde(default)]
    pub earlier: Vec<EarlierStatus>,
    #[serde(default)]
    pub dropped: Dropped,
    pub timing: Timing,
    pub usage: Usage,
    pub assurance: Option<Vec<String>>,
    #[serde(default)]
    pub not_checked: Vec<String>,
    #[serde(default)]
    pub incomplete: Vec<String>,
    /// New-file line ranges of the diff's hunks, by path.
    #[serde(default)]
    pub diff_lines: IndexMap<String, Vec<(i64, i64)>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanOptions {
    /// The reviewing account wrote the pull request.
    pub self_authored: bool,
    /// `open`, `closed` or `merged`.
    pub state: String,
    pub head_sha: String,
    /// A re-review of the changes since this commit.
    pub since_sha: Option<String>,
    pub signature: bool,
    pub block_at: Option<String>,
    pub max_comments: Option<i64>,
    /// A cherry-pick or promotion: only what differs from the merged pull requests it carries was read.
    #[serde(default)]
    pub promotion: Option<PromotionNote>,
}

/// What a cherry-pick or promotion carries (`promotion.rs`): its sources and what matched them.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromotionNote {
    pub sources: Vec<String>,
    pub carried_lines: u64,
    pub carried_files: u64,
    #[serde(default)]
    pub context_files: u64,
    pub missing: Vec<crate::promotion::Missing>,
    /// Every changed line was carried: nothing was left to read.
    #[serde(default)]
    pub entire: bool,
}

fn counted_noun(n: u64, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// `a`, `a and b`, `a, b and c`; past `max`, `a, b, ... and N more`.
fn listed(items: &[String], max: usize) -> String {
    let shown: Vec<&str> = items.iter().take(max).map(String::as_str).collect();
    let rest = items.len().saturating_sub(max);
    match (shown.as_slice(), rest) {
        ([], _) => String::new(),
        ([one], 0) => (*one).to_string(),
        (_, 0) => format!("{} and {}", shown[..shown.len() - 1].join(", "), shown[shown.len() - 1]),
        (_, rest) => format!("{} and {rest} more", shown.join(", ")),
    }
}

/// The lead of the body for a cherry-pick: what it carries and that the rest was read, or that there was no rest.
fn promotion_lead(note: &PromotionNote) -> String {
    let names = listed(&note.sources, MAX_SOURCES_LISTED);
    let matches = if note.carried_lines == 1 { "matches" } else { "match" };
    let them = if note.sources.len() == 1 { "it" } else { "them" };
    if note.entire {
        let what = if note.carried_lines == 1 {
            "its one changed line matches".to_string()
        } else {
            format!("all {} changed lines match", note.carried_lines)
        };
        let was = if note.carried_lines == 1 { "was" } else { "were" };
        return format!(
            "Cherry-pick of {names}: {what} {them} line for line and {was} reviewed there; nothing else changed."
        );
    }
    let files = if note.carried_files > 0 {
        format!(" ({} entirely)", counted_noun(note.carried_files, "file"))
    } else {
        String::new()
    };
    let context = if note.context_files > 0 {
        format!(
            ", and in full {} where this branch's code differs from what {} reviewed against",
            counted_noun(note.context_files, "file"),
            if note.sources.len() == 1 { "it was" } else { "they were" }
        )
    } else {
        String::new()
    };
    format!(
        "Cherry-pick of {names}: {} {matches} {them} line for line and {} reviewed there{files}; this review reads the \
         rest{context}.",
        counted_noun(note.carried_lines, "changed line"),
        if note.carried_lines == 1 { "was" } else { "were" }
    )
}

/// The lines the sources add that the head does not have, for the author to confirm.
fn promotion_missing(note: &PromotionNote) -> Option<String> {
    if note.missing.is_empty() {
        return None;
    }
    let named: Vec<String> = note
        .missing
        .iter()
        .take(MAX_MISSING_LISTED)
        .map(|missing| format!("`{}` ({})", missing.path, counted_noun(missing.lines, "line")))
        .collect();
    let more = note.missing.len().saturating_sub(MAX_MISSING_LISTED);
    let more = if more > 0 {
        format!(", and {} more", counted_noun(more as u64, "file"))
    } else {
        String::new()
    };
    let verb = if note.sources.len() == 1 { "adds" } else { "add" };
    Some(format!(
        "Not carried over: lines {} {verb} are not in this head, in {}{more}. Worth confirming they are left out on purpose.",
        listed(&note.sources, MAX_SOURCES_LISTED),
        named.join(", ")
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    RequestChanges,
    Comment,
}

impl Verdict {
    pub fn event(self) -> &'static str {
        match self {
            Verdict::Approve => "APPROVE",
            Verdict::RequestChanges => "REQUEST_CHANGES",
            Verdict::Comment => "COMMENT",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Verdict::Approve => "Approve",
            Verdict::RequestChanges => "Request changes",
            Verdict::Comment => "Comment",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decision {
    pub verdict: Verdict,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Placed {
    pub line: i64,
    pub relocated: bool,
    pub range: (i64, i64),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlannedComment {
    /// Index into the result's findings.
    pub finding: usize,
    /// The marker in the body that identifies the comment when the review is read back.
    pub marker: String,
    pub relocated: bool,
    pub suggestion: bool,
    pub path: String,
    pub line: i64,
    pub side: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_side: Option<&'static str>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewPlan {
    pub verdict: Verdict,
    pub event: &'static str,
    pub body: String,
    pub comments: Vec<PlannedComment>,
    /// Confirmed findings named in the body instead of inline, by index.
    pub in_summary: Vec<usize>,
    /// Confirmed findings neither inline nor named: past the comment limit, or nits. By index.
    pub over_cap: Vec<usize>,
}

/// Where an inline comment for `line` can go: the line itself, a diff line within three lines, or nowhere.
pub fn place_line(ranges: Option<&[(i64, i64)]>, line: i64) -> Option<Placed> {
    let mut best: Option<(i64, i64, (i64, i64))> = None;
    for &(start, end) in ranges.unwrap_or_default() {
        let nearest = line.max(start).min(end);
        let distance = (nearest - line).abs();
        if best.is_none_or(|(_, best_distance, _)| distance < best_distance) {
            best = Some((nearest, distance, (start, end)));
        }
    }
    let (line, distance, range) = best?;
    (distance <= RELOCATE_WITHIN).then_some(Placed {
        line,
        relocated: distance > 0,
        range,
    })
}

fn fence(code: &str, info: &str) -> String {
    let mut longest = 2;
    let mut run = 0;
    for c in code.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let ticks = "`".repeat(longest + 1);
    format!("{ticks}{info}\n{code}\n{ticks}")
}

pub fn comment_marker(key: &str) -> String {
    format!("<!-- ultron-autoreview:{key} -->")
}

fn short(text: &str, limit: usize) -> String {
    let flat = flatten(text);
    if len16(&flat) <= limit {
        return flat;
    }
    format!(
        "{}…",
        head16(&flat, limit.saturating_sub(1).max(1)).trim_end_matches(crate::js::is_space)
    )
}

/// `text` ending as a sentence.
fn sentence(text: &str) -> String {
    let flat = flatten(text);
    if flat.is_empty() || flat.ends_with(['.', '!', '?', '…', ':']) {
        flat
    } else {
        format!("{flat}.")
    }
}

fn place(file: &str, line: i64) -> String {
    format!("`{file}:{line}`")
}

fn where_(finding: &EngineFinding) -> String {
    place(&finding.file, finding.line)
}

fn non_empty(text: &Option<String>) -> Option<&str> {
    text.as_deref().filter(|text| !text.is_empty())
}

/// `[level] what is wrong, with the evidence. The fix.`: the sentences of an inline comment, within the target.
pub fn comment_text(finding: &EngineFinding) -> String {
    let head = format!("[{}] {}", finding.level(), sentence(&finding.claim));
    let fix = match (&finding.replacement, non_empty(&finding.suggested_fix)) {
        (None, Some(fix)) => sentence(fix),
        _ => String::new(),
    };
    let room =
        COMMENT_TARGET_CHARS as i64 - len16(&head) as i64 - if fix.is_empty() { 0 } else { len16(&fix) as i64 + 1 } - 1;
    // The scenario is the evidence when there is one; else the reason.
    let evidence = match &finding.scenario {
        Some(scenario) if !crate::js::trim(scenario).is_empty() => scenario.as_str(),
        _ => finding.why.as_str(),
    };
    let middle = if room >= 40 && !evidence.is_empty() {
        sentence(&short(evidence, room as usize))
    } else {
        String::new()
    };
    [head, middle, fix, certainty(finding)]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// How sure the review is, when it is more than the verifier's reading: a run the host made, or a fact read from the
/// files' structure; and when the repository's history lowered it.
pub fn certainty(finding: &EngineFinding) -> String {
    let mut parts = Vec::new();
    if finding.strength.as_deref() == Some("test") {
        parts.push("Proven by a test run the reviewer made.".to_string());
    } else if finding.source.as_deref() == Some("deep:structure") {
        parts
            .push("Stated by the reviewer from the files' structure; the verifier agreed it matters here.".to_string());
    }
    if let Some(lesson) = non_empty(&finding.lesson) {
        parts.push(format!("Posted as a note: this repository {lesson}."));
    }
    parts.join(" ")
}

/// The inline comment of one finding, or none when its line cannot be placed on the diff.
pub fn plan_comment(
    finding: &EngineFinding,
    index: usize,
    ranges: Option<&[(i64, i64)]>,
    key: &str,
) -> Option<PlannedComment> {
    let placed = place_line(ranges, finding.line)?;
    let end = finding
        .end_line
        .filter(|end| *end > finding.line)
        .unwrap_or(finding.line);
    let span_in_hunk = !placed.relocated && end >= placed.range.0 && end <= placed.range.1;
    let multi = span_in_hunk && end > finding.line;
    let suggestion = finding.replacement.is_some() && span_in_hunk;
    let mut parts = vec![comment_text(finding)];
    if placed.relocated {
        parts.push(format!(
            "This is about line {}, which is not part of the diff.",
            finding.line
        ));
    }
    if let Some(also) = finding.also_at.as_ref().filter(|also| !also.is_empty()) {
        let named: Vec<String> = also.iter().take(6).map(|item| place(&item.file, item.line)).collect();
        parts.push(format!("Same at {}.", named.join(", ")));
    }
    if let Some(replacement) = &finding.replacement {
        if suggestion {
            parts.push(fence(replacement, "suggestion"));
        } else {
            let lines = if end > finding.line {
                format!("lines {}-{end}", finding.line)
            } else {
                format!("line {}", finding.line)
            };
            parts.push(format!(
                "Suggested replacement for {lines}:\n\n{}",
                fence(replacement, "")
            ));
        }
    }
    let marker = comment_marker(key);
    parts.push(marker.clone());
    Some(PlannedComment {
        finding: index,
        marker,
        relocated: placed.relocated,
        suggestion,
        path: finding.file.clone(),
        line: if multi { end } else { placed.line },
        side: "RIGHT",
        start_line: multi.then_some(finding.line),
        start_side: multi.then_some("RIGHT"),
        body: parts.join("\n\n"),
    })
}

fn confirmed(finding: &EngineFinding) -> bool {
    finding.verification == "confirmed"
}

pub fn decide_verdict(result: &EngineResult, self_authored: bool, state: &str, block_at: Option<&str>) -> Decision {
    let block_at = block_at.unwrap_or(DEFAULT_BLOCK_AT);
    let blocking = result
        .findings
        .iter()
        .filter(|finding| confirmed(finding) && at_or_above(finding.level(), block_at))
        .count();
    let still_open = result
        .earlier
        .iter()
        .filter(|item| {
            item.status == "still_present"
                && at_or_above(level_of(item.level.as_deref(), item.severity.as_deref()), block_at)
        })
        .count();
    let decision = |verdict, reason: String| Decision { verdict, reason };
    let plural = |count: usize| if count == 1 { "" } else { "s" };
    if self_authored {
        decision(Verdict::Comment, "this account opened the pull request".into())
    } else if state != "open" {
        decision(Verdict::Comment, format!("the pull request is {state}"))
    } else if blocking > 0 {
        decision(
            Verdict::RequestChanges,
            format!(
                "{blocking} confirmed finding{} at {block_at} or above",
                plural(blocking)
            ),
        )
    } else if still_open > 0 {
        decision(
            Verdict::RequestChanges,
            format!(
                "{still_open} finding{} from an earlier review still present",
                plural(still_open)
            ),
        )
    } else if !result.complete {
        decision(Verdict::Comment, "the review did not cover everything".into())
    } else {
        decision(Verdict::Approve, format!("no confirmed finding at {block_at} or above"))
    }
}

fn seconds(ms: f64) -> String {
    if ms < 10_000.0 {
        format!("{} s", to_fixed(ms / 1000.0, 1))
    } else {
        format!("{} s", number(round(ms / 1000.0)))
    }
}

fn tokens(count: f64) -> String {
    if count >= 1000.0 {
        format!("{}k", number(round(count / 1000.0)))
    } else {
        number(count)
    }
}

/// One line of timing and cost.
pub fn cost_line(result: &EngineResult) -> String {
    let cost = if result.usage.cost_usd > 0.0 {
        format!(", ${}", to_fixed(result.usage.cost_usd, 2))
    } else {
        String::new()
    };
    format!(
        "Reviewed in {}: {} model calls, {} tokens{cost}.",
        seconds(result.timing.total_ms),
        number(result.usage.frames),
        tokens(result.usage.tokens)
    )
}

/// Confirmed findings, the ones to post first: those that are serious (critical, high) or proven beyond the diff,
/// then the rest; within each by level, then by evidence strength. Indices into `result.findings`.
pub fn rank_findings(result: &EngineResult) -> Vec<usize> {
    let strength = |finding: &EngineFinding| {
        let name = finding.strength.as_deref().unwrap_or("diff");
        STRENGTHS
            .iter()
            .position(|item| *item == name)
            .map_or(-1, |position| position as i64)
    };
    let tier = |finding: &EngineFinding| {
        if at_or_above(finding.level(), "high") || finding.strength.as_deref().unwrap_or("diff") != "diff" {
            0
        } else {
            1
        }
    };
    let mut ranked: Vec<usize> = (0..result.findings.len())
        .filter(|index| confirmed(&result.findings[*index]))
        .collect();
    ranked.sort_by_key(|index| {
        let finding = &result.findings[*index];
        (tier(finding), rank(finding.level()), strength(finding), *index)
    });
    ranked
}

fn counted(indices: &[usize], result: &EngineResult) -> String {
    LEVELS
        .iter()
        .filter_map(|level| {
            let count = indices
                .iter()
                .filter(|index| result.findings[**index].level() == *level)
                .count();
            (count > 0).then(|| format!("{count} {level}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| format!("{}{}", first.to_uppercase(), chars.as_str()))
        .unwrap_or_default()
}

pub fn plan_review(result: &EngineResult, options: &PlanOptions) -> ReviewPlan {
    let block_at = options.block_at.as_deref().unwrap_or(DEFAULT_BLOCK_AT);
    let max_comments = options.max_comments.unwrap_or(DEFAULT_MAX_COMMENTS).max(0) as usize;
    let Decision { verdict, reason } = decide_verdict(
        result,
        options.self_authored,
        &options.state,
        options.block_at.as_deref(),
    );
    let mut comments: Vec<PlannedComment> = Vec::new();
    let mut in_summary = Vec::new();
    let mut over_cap = Vec::new();
    let short_sha = head16(&options.head_sha, 7);
    let ranked = rank_findings(result);
    let ranges = |file: &str| result.diff_lines.get(file).map(Vec::as_slice);
    for &index in &ranked {
        let finding = &result.findings[index];
        let level = finding.level();
        let blocking = at_or_above(level, block_at);
        let key = format!("{short_sha}-{}", index + 1);
        let mut comment = if level == "nit" {
            None
        } else {
            plan_comment(finding, index, ranges(&finding.file), &key)
        };
        // The same root cause at another place that is in the diff: the comment goes there, naming this place.
        if comment.is_none() && level != "nit" {
            let also = finding.also_at.clone().unwrap_or_default();
            for (position, other) in also.iter().enumerate() {
                let mut also_at = vec![Place {
                    file: finding.file.clone(),
                    line: finding.line,
                }];
                also_at.extend(
                    also.iter()
                        .enumerate()
                        .filter(|(i, _)| *i != position)
                        .map(|(_, item)| item.clone()),
                );
                let moved = EngineFinding {
                    file: other.file.clone(),
                    line: other.line,
                    end_line: None,
                    replacement: None,
                    also_at: Some(also_at),
                    ..finding.clone()
                };
                comment = plan_comment(&moved, index, ranges(&other.file), &key);
                if comment.is_some() {
                    break;
                }
            }
        }
        match comment {
            Some(comment) if comments.len() < max_comments => comments.push(comment),
            // What cannot be inline is named in the body when it must be resolved, or when its line is outside the diff.
            ref other if blocking || (other.is_none() && level != "nit") => in_summary.push(index),
            _ => over_cap.push(index),
        }
    }
    let inline: Vec<usize> = comments.iter().map(|comment| comment.finding).collect();
    let blockers: Vec<usize> = ranked
        .iter()
        .copied()
        .filter(|index| at_or_above(result.findings[*index].level(), block_at))
        .collect();
    let uncertain = result
        .findings
        .iter()
        .filter(|finding| finding.verification == "uncertain")
        .count();
    let since = options.since_sha.as_deref().map(|sha| head16(sha, 7));

    // Paragraph 1: what was checked and holds.
    let assurance: Vec<String> = result
        .assurance
        .iter()
        .flatten()
        .take(4)
        .map(|item| short(item, 320))
        .collect();
    let scope = format!(
        "{} file{}, +{} -{}",
        result.files,
        if result.files == 1 { "" } else { "s" },
        result.added,
        result.removed
    );
    let first = if !assurance.is_empty() {
        let lead = since
            .map(|sha| format!("Re-review of the changes since `{sha}`. "))
            .unwrap_or_default();
        format!("{lead}{}", assurance.join(" "))
    } else {
        let what = match since {
            None => format!("the diff of `{short_sha}`"),
            Some(sha) => format!("the changes from `{sha}` to `{short_sha}`"),
        };
        let rejected = if result.dropped.rejected != 0 {
            format!(", which rejected {}", result.dropped.rejected)
        } else {
            String::new()
        };
        format!(
            "Read {what} ({scope}) with the code around it; every finding below was checked against the source by a second pass{rejected}."
        )
    };

    let promotion = options.promotion.as_ref().filter(|note| !note.sources.is_empty());
    let first = match promotion {
        Some(note) => format!("{} {first}", promotion_lead(note)),
        None => first,
    };

    // Paragraph 2: the verdict and what must be resolved before merge.
    let mut second = vec![format!("**{}.**", verdict.title())];
    if !blockers.is_empty() {
        let named: Vec<String> = blockers
            .iter()
            .take(MAX_BLOCKING_LISTED)
            .enumerate()
            .map(|(position, index)| {
                let finding = &result.findings[*index];
                let at = if inline.contains(index) {
                    format!("{} (inline)", where_(finding))
                } else {
                    where_(finding)
                };
                let number = if blockers.len() > 1 {
                    format!("({}) ", position + 1)
                } else {
                    String::new()
                };
                format!("{number}{at} {}", sentence(&short(&finding.claim, 220)))
            })
            .collect();
        let lead = if verdict == Verdict::RequestChanges {
            "To resolve before merge:"
        } else {
            "Worth resolving:"
        };
        second.push(format!("{lead} {}", named.join(" ")));
        if blockers.len() > MAX_BLOCKING_LISTED {
            second.push(format!(
                "And {} more at {block_at} or above.",
                blockers.len() - MAX_BLOCKING_LISTED
            ));
        }
        if verdict != Verdict::RequestChanges {
            second.push(format!("{}.", capitalized(&reason)));
        }
    } else {
        second.push(format!("{}.", capitalized(&reason)));
    }
    // An earlier finding that still blocks is named with what the re-check saw, so the author can fix or dispute it
    // (fisher-ki-saas#2917: two reviews requested changes for "1 finding from an earlier review" and named none).
    let earlier_blockers: Vec<&EarlierStatus> = result
        .earlier
        .iter()
        .filter(|item| {
            item.status == "still_present"
                && at_or_above(level_of(item.level.as_deref(), item.severity.as_deref()), block_at)
        })
        .collect();
    if verdict == Verdict::RequestChanges && !earlier_blockers.is_empty() {
        let named: Vec<String> = earlier_blockers
            .iter()
            .take(MAX_BLOCKING_LISTED)
            .enumerate()
            .map(|(position, item)| {
                let number = if earlier_blockers.len() > 1 {
                    format!("({}) ", position + 1)
                } else {
                    String::new()
                };
                let seen = if item.evidence.is_empty() {
                    String::new()
                } else {
                    format!(" Re-check: {}", sentence(&short(&item.evidence, 200)))
                };
                format!(
                    "{number}{} {}{seen}",
                    place(&item.file, item.line),
                    sentence(&short(&item.claim, 220))
                )
            })
            .collect();
        second.push(format!("Still present from an earlier review: {}", named.join(" ")));
        if earlier_blockers.len() > MAX_BLOCKING_LISTED {
            second.push(format!(
                "And {} more from earlier reviews.",
                earlier_blockers.len() - MAX_BLOCKING_LISTED
            ));
        }
    }
    let with_status =
        |status: &str| -> Vec<&EarlierStatus> { result.earlier.iter().filter(|item| item.status == status).collect() };
    let (still_open, fixed) = (with_status("still_present"), with_status("fixed"));
    if !result.earlier.is_empty() {
        let list = |items: &[&EarlierStatus]| {
            items
                .iter()
                .take(4)
                .map(|item| place(&item.file, item.line))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut parts = Vec::new();
        if !fixed.is_empty() {
            parts.push(format!("{} fixed ({})", fixed.len(), list(&fixed)));
        }
        if !still_open.is_empty() {
            parts.push(format!("{} still present ({})", still_open.len(), list(&still_open)));
        }
        let other = result.earlier.len() - fixed.len() - still_open.len();
        if other > 0 {
            parts.push(format!("{other} no longer applicable or not re-checked"));
        }
        second.push(format!("Earlier findings: {}.", parts.join("; ")));
    }

    // Paragraph 3: the non-blocking notes.
    let mut notes = Vec::new();
    let inline_notes = comments
        .iter()
        .filter(|comment| !at_or_above(result.findings[comment.finding].level(), block_at))
        .count();
    if inline_notes > 0 {
        notes.push(format!(
            "{inline_notes} non-blocking note{} inline.",
            if inline_notes == 1 { "" } else { "s" }
        ));
    }
    let outside: Vec<usize> = in_summary
        .iter()
        .copied()
        .filter(|index| !at_or_above(result.findings[*index].level(), block_at))
        .collect();
    if !outside.is_empty() {
        let named: Vec<String> = outside
            .iter()
            .take(MAX_OUTSIDE_LISTED)
            .map(|index| {
                let finding = &result.findings[*index];
                format!(
                    "{} [{}] {}",
                    where_(finding),
                    finding.level(),
                    sentence(&short(&finding.claim, 160))
                )
            })
            .collect();
        let more = if outside.len() > MAX_OUTSIDE_LISTED {
            format!(" And {} more.", outside.len() - MAX_OUTSIDE_LISTED)
        } else {
            String::new()
        };
        notes.push(format!("Outside the diff: {}{more}", named.join(" ")));
    }
    let mut rest = Vec::new();
    if !over_cap.is_empty() {
        rest.push(format!(
            "{} lower-ranked finding{} ({})",
            over_cap.len(),
            if over_cap.len() == 1 { "" } else { "s" },
            counted(&over_cap, result)
        ));
    }
    if uncertain > 0 {
        rest.push(format!("{uncertain} unconfirmed"));
    }
    if !rest.is_empty() {
        notes.push(format!("Not posted: {}.", rest.join(" and ")));
    }
    let lowered = result
        .findings
        .iter()
        .filter(|finding| non_empty(&finding.lesson).is_some())
        .count();
    if lowered > 0 {
        notes.push(format!(
            "{lowered} finding{} notes because this repository rarely accepted their kind.",
            if lowered == 1 { " is" } else { "s are" }
        ));
    }
    let raised = result.also_raised.len();
    if raised > 0 {
        let mut by: Vec<&str> = Vec::new();
        for name in result.also_raised.iter().flat_map(|item| &item.by) {
            if !by.contains(&name.as_str()) {
                by.push(name);
            }
        }
        let by: Vec<String> = by.iter().take(4).map(|name| format!("@{name}")).collect();
        notes.push(format!(
            "{raised} finding{} already raised by {} and {} not repeated.",
            if raised == 1 { " was" } else { "s were" },
            by.join(", "),
            if raised == 1 { "is" } else { "are" }
        ));
    }

    // What was not checked: the reasons coverage is incomplete lead; routine skips follow.
    let mut gaps: Vec<&String> = result.incomplete.iter().collect();
    gaps.extend(
        result
            .not_checked
            .iter()
            .filter(|item| !result.incomplete.contains(item)),
    );
    let mut paragraphs = vec![first, second.join(" ")];
    if !notes.is_empty() {
        paragraphs.push(notes.join(" "));
    }
    if let Some(missing) = promotion.and_then(promotion_missing) {
        paragraphs.push(missing);
    }
    if !gaps.is_empty() {
        let listed: Vec<String> = gaps
            .iter()
            .take(MAX_NOT_CHECKED)
            .map(|gap| {
                let gap = short(gap, 200);
                gap.strip_suffix('.').map(str::to_string).unwrap_or(gap)
            })
            .collect();
        let more = if gaps.len() > MAX_NOT_CHECKED {
            format!("; and {} more", gaps.len() - MAX_NOT_CHECKED)
        } else {
            String::new()
        };
        paragraphs.push(format!("Not checked: {}{more}.", listed.join("; ")));
    }
    paragraphs.push(cost_line(result));
    let mut lines: Vec<String> = Vec::new();
    for (index, paragraph) in paragraphs.into_iter().enumerate() {
        if index > 0 {
            lines.push(String::new());
        }
        lines.push(paragraph);
    }
    let tail: Vec<String> = if options.signature {
        vec![String::new(), SIGNATURE.into()]
    } else {
        Vec::new()
    };
    lines.truncate(MAX_BODY_LINES - tail.len());
    lines.extend(tail);
    ReviewPlan {
        verdict,
        event: verdict.event(),
        body: lines.join("\n"),
        comments,
        in_summary,
        over_cap,
    }
}

/// The plan with every inline comment named in the body instead (when GitHub rejects the inline comments).
pub fn without_inline(plan: &ReviewPlan, result: &EngineResult) -> ReviewPlan {
    if plan.comments.is_empty() {
        return plan.clone();
    }
    let named: Vec<String> = plan
        .comments
        .iter()
        .map(|comment| {
            let finding = &result.findings[comment.finding];
            format!("{} {}", where_(finding), comment_text(finding))
        })
        .collect();
    let extra = format!("These could not be attached to the diff: {}", named.join(" "));
    let lines: Vec<&str> = plan.body.split('\n').collect();
    let signed = lines.last() == Some(&SIGNATURE);
    let head = if signed {
        &lines[..lines.len().saturating_sub(2)]
    } else {
        &lines[..]
    };
    let mut body: Vec<&str> = head.to_vec();
    body.extend(["", &extra]);
    if signed {
        body.extend(["", SIGNATURE]);
    }
    let mut in_summary = plan.in_summary.clone();
    in_summary.extend(plan.comments.iter().map(|comment| comment.finding));
    ReviewPlan {
        body: body.join("\n"),
        comments: Vec::new(),
        in_summary,
        ..plan.clone()
    }
}
