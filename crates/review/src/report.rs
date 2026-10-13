//! The `/review` report and what happens to it (`review_api.py` `render_report`, `_quote`, `_save`, `post`):
//! confirmed findings by severity with the real source line quoted, uncertain ones apart, counts, cost and gaps;
//! saved under the repository's git dir; posted to a pull request only with an explicit confirmation.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::args::ReviewError;
use crate::diff::FileDiff;
use crate::findings::{Finding, SEVERITIES};
use crate::prompt_text::Reviewer;
use crate::py;
use crate::scope::{GH_TIMEOUT_S, Git, Runner};

pub const REPORT_FINDINGS: usize = 30;

/// Counts and cost of one review (missing keys read as 0).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub raised: u64,
    pub merged: u64,
    pub tokens: u64,
    pub frames: u64,
    pub find_frames: u64,
    pub verify_frames: u64,
    pub budget: u64,
    pub subagents: u64,
}

fn severity_title(severity: &str) -> &'static str {
    match severity {
        "blocker" => "Blockers",
        "major" => "Major",
        "minor" => "Minor",
        _ => "Nits",
    }
}

/// The cited source line, indented and clipped, or empty when it is not available.
fn quote(read_file: &mut impl FnMut(&str) -> Option<Vec<String>>, finding: &Finding) -> String {
    let Some(source) = read_file(&finding.file) else {
        return String::new();
    };
    let line = finding.line;
    if source.is_empty() || line < 1 || line > source.len() as i64 {
        return String::new();
    }
    format!("    {line} | {}", py::clip(py::rstrip(&source[line as usize - 1]), 200))
}

/// Everything a report shows.
pub struct ReportInput<'a> {
    pub label: &'a str,
    pub files: &'a [FileDiff],
    pub reviewers: &'a [&'a Reviewer],
    pub confirmed: &'a [Finding],
    pub uncertain: &'a [Finding],
    pub rejected: &'a [Finding],
    pub stats: &'a Stats,
    pub not_checked: &'a [String],
    pub path: Option<&'a str>,
    pub post_line: Option<&'a str>,
}

/// Markdown report: confirmed findings by severity, uncertain ones apart, then counts, cost and gaps. Rejected
/// findings contribute only to the counts.
pub fn render_report(input: &ReportInput<'_>, mut read_file: impl FnMut(&str) -> Option<Vec<String>>) -> String {
    let added: usize = input.files.iter().map(FileDiff::added).sum();
    let removed: usize = input.files.iter().map(FileDiff::removed).sum();
    let mut out = vec![format!("# Code review: {}", input.label), String::new()];
    let titles: Vec<&str> = input.reviewers.iter().map(|reviewer| reviewer.title).collect();
    out.push(format!(
        "{} files, +{added} -{removed} lines. Reviewers: {}.",
        input.files.len(),
        titles.join(", ")
    ));
    if input.confirmed.is_empty() {
        out.extend([String::new(), "No confirmed findings.".to_string()]);
    }
    let mut shown = 0;
    for severity in SEVERITIES {
        let group: Vec<&Finding> = input.confirmed.iter().filter(|f| f.severity == severity).collect();
        if group.is_empty() {
            continue;
        }
        out.extend([
            String::new(),
            format!("## {} ({})", severity_title(severity), group.len()),
        ]);
        for finding in group {
            shown += 1;
            let place = format!("{}:{}", finding.file, finding.line);
            if shown > REPORT_FINDINGS {
                out.extend([String::new(), format!("- `{place}` {}", finding.claim)]);
                continue;
            }
            out.extend([
                String::new(),
                format!("**{shown}. `{place}`** {}", finding.claim),
                format!(
                    "_{}; raised by {}; confidence {:.1}_",
                    finding.category,
                    finding.reviewers.join(", "),
                    finding.confidence
                ),
            ]);
            let quoted = quote(&mut read_file, finding);
            if !quoted.is_empty() {
                out.extend([String::new(), quoted, String::new()]);
            }
            if let Some(evidence) = finding.evidence.as_deref().filter(|text| !text.is_empty()) {
                out.push(format!("- Evidence: {evidence}"));
            }
            if !finding.why.is_empty() {
                out.push(format!("- Why: {}", finding.why));
            }
            if !finding.suggested_fix.is_empty() {
                out.push(format!("- Fix: {}", finding.suggested_fix));
            }
        }
    }
    if !input.uncertain.is_empty() {
        out.extend([
            String::new(),
            format!("## Uncertain, not confirmed ({})", input.uncertain.len()),
            String::new(),
        ]);
        for finding in input.uncertain.iter().take(REPORT_FINDINGS) {
            let note = finding
                .verification
                .as_deref()
                .filter(|note| !note.is_empty())
                .map(|note| format!(" Verifier: {note}"))
                .unwrap_or_default();
            out.push(format!(
                "- `{}:{}` ({}) {}{note}",
                finding.file, finding.line, finding.severity, finding.claim
            ));
        }
        if input.uncertain.len() > REPORT_FINDINGS {
            out.push(format!(
                "- and {} more in the saved report",
                input.uncertain.len() - REPORT_FINDINGS
            ));
        }
    }
    let stats = input.stats;
    out.extend([String::new(), "## Summary".to_string(), String::new()]);
    out.push(format!(
        "- Findings: {} raised, {} after merging duplicates, {} confirmed, {} rejected, {} uncertain.",
        stats.raised,
        stats.merged,
        input.confirmed.len(),
        input.rejected.len(),
        input.uncertain.len()
    ));
    let mut cost = format!(
        "- Cost: {} model calls for {} finder and {} verifier frames (re-asks included), {} tokens of a {}-token cap",
        stats.frames,
        stats.find_frames,
        stats.verify_frames,
        py::thousands(stats.tokens as i64),
        py::thousands(stats.budget as i64)
    );
    if stats.subagents > 0 {
        cost.push_str(&format!(
            "; {} verification sub-agents (their tokens count in the session usage)",
            stats.subagents
        ));
    }
    out.push(cost + ".");
    if let Some(path) = input.path.filter(|path| !path.is_empty()) {
        out.push(format!("- Full report: {path}"));
    }
    if let Some(line) = input.post_line.filter(|line| !line.is_empty()) {
        out.push(format!("- {line}"));
    }
    out.extend([
        String::new(),
        "### What this review did not check".to_string(),
        String::new(),
    ]);
    let gaps = [
        "Runtime behavior: nothing was executed (no tests, build or type check); findings come from reading the diff, \
         the code around it and name-matched callers.",
        "Effects on code that does not mention the changed names (dynamic dispatch, reflection, other services).",
    ];
    out.extend(gaps.iter().map(|gap| format!("- {gap}")));
    out.extend(input.not_checked.iter().map(|gap| format!("- {gap}")));
    out.join("\n") + "\n"
}

/// `time.strftime("%Y%m%d-%H%M%S")` of `at`, in UTC (the host has no time zone database; Python used local time).
pub fn stamp(at: SystemTime) -> String {
    let seconds = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// Save the report as `review-<stamp>.md` under the repository's git dir (`ultron-review/`), else under
/// `temp_dir`; the saved path, or `None` when it could not be written.
pub fn save(report: &str, git: Option<&Git<'_>>, stamp: &str, temp_dir: &Path) -> Option<String> {
    let directory = git
        .and_then(|git| git.value(&["rev-parse", "--absolute-git-dir"]))
        .map(|dir| Path::new(&dir).join("ultron-review"))
        .unwrap_or_else(|| temp_dir.join("ultron-review"));
    std::fs::create_dir_all(&directory).ok()?;
    let path: PathBuf = directory.join(format!("review-{stamp}.md"));
    std::fs::write(&path, report).ok()?;
    Some(path.to_string_lossy().into_owned())
}

/// Post a saved report as a PR comment with `gh`. Refused without `confirm` (the user's explicit yes), a pull
/// request, a saved report file, or gh. Returns gh's output, or a line naming the PR.
pub fn post(
    path: Option<&str>,
    pr: Option<u64>,
    confirm: bool,
    runner: &dyn Runner,
    which: impl Fn(&str) -> Option<String>,
    cwd: &str,
) -> Result<String, ReviewError> {
    if !confirm {
        return Err(ReviewError(
            "posting needs confirm=True, and only after the user explicitly agreed to post".to_string(),
        ));
    }
    let Some(number) = pr else {
        return Err(ReviewError("this review has no pull request to post to".to_string()));
    };
    let Some(path) = path.filter(|path| !path.is_empty() && Path::new(path).is_file()) else {
        return Err(ReviewError("the review has no saved report to post".to_string()));
    };
    if which("gh").is_none() {
        return Err(ReviewError("posting needs the GitHub CLI (gh)".to_string()));
    }
    let argv: Vec<String> = ["gh", "pr", "comment", &number.to_string(), "--body-file", path]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    let output = runner.run(&argv, cwd, GH_TIMEOUT_S);
    if output.code != 0 {
        let detail = if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        };
        return Err(ReviewError(format!(
            "gh pr comment failed: {}",
            py::strip(detail).chars().take(300).collect::<String>()
        )));
    }
    let text = py::strip(&output.stdout);
    Ok(if text.is_empty() {
        format!("posted to PR #{number}")
    } else {
        text.to_string()
    })
}
