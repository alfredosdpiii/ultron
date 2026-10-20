//! One pull request, one account (`autoreview/reviewer.ts`): decide whether a review is due, acknowledge, check the
//! source out, run the engine, and post the review (or write it to the dry-run directory). Everything read from the
//! pull request (description, comments, the mention itself, repository files) is data for the review, never
//! instructions.
//!
//! The engine is a [`ReviewEngine`] (the coordinator serves it with the kernel-backed engine); the lesson memory a
//! [`LessonMemory`] (`crates/hindsight`'s `HindsightLessons`); building a test environment an optional
//! [`EnvironmentPreparer`] (`prepareEnvironment`, which becomes a host job).
//!
//! Known differences from the TypeScript:
//! - The engine returns its result as JSON (the TypeScript `EngineResult`): `plan::EngineResult` reads only what
//!   the plan needs and is not serializable, while the reviewer also needs `guideNames`, `timing.stages`, each
//!   finding's `kind`, the earlier findings' ids, and the whole result for the dry-run file. A result that
//!   `plan::EngineResult` cannot read fails the review like an engine error.
//! - Without an [`EnvironmentPreparer`] no environment is prepared (the TypeScript always tried); the review then
//!   falls back to a local checkout, as it did when nothing could be prepared. The command line's daemon always
//!   supplies one (`crate::prepare_env::RunnerPreparer`).
//! - `mentions` checks the character after the login by hand (the Rust regex has no lookahead).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::accounts::{Account, TokenStore, account_key};
use crate::checkout::{Checkout, CheckoutManager, PullHeads, make_private_dir, random_hex};
use crate::github::{
    Clock, Comment, Error, GitHub, PullRef, PullRequest, Review, ReviewCommentInput, ReviewInput, iso_from_ms, pull_key,
};
use crate::js::{Num, cmp16, flatten, is_space, len16, path_join, to_fixed};
use crate::ledger::{Fate, FateInput, FateKind, Lesson, ThreadState, fates_of, lessons_for, merge_fates, parse_iso_ms};
use crate::local::{expand_path, find_checkout, without_guide_mentions};
use crate::plan::{EngineResult, PlanOptions, ReviewPlan, Verdict, plan_review, without_inline};
use crate::runner::Runner;
use crate::settings::{AutoreviewConfig, AutoreviewPaths, MAX_ATTEMPTS, engine_settings, tests_eligible};
use crate::state::{PostedFinding, PullState, ReviewRecord, StateStore, claim_hash, to_json_1};

/// Runs one review: `spec` is the TypeScript `EngineSpec` as JSON; the result is the `EngineResult` as JSON
/// (`crate::engine::KernelEngine` in the service, a scripted engine in tests).
pub use crate::engine::ReviewEngine;

/// Where fates and rules are also retained (Hindsight).
pub trait LessonMemory: Send + Sync {
    fn retain(&self, repo: &str, fates: &[Fate], lessons: &[Lesson]) -> Result<(), String>;
}

/// A prepared test environment (`prepareEnvironment`'s result).
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedEnvironment {
    pub hash: String,
    pub cached: bool,
    pub dir: String,
    pub prepared: Vec<String>,
    pub failures: Vec<String>,
    /// The toolchain entries' paths.
    pub toolchain: Vec<String>,
}

/// Builds a test environment for a worktree (with the network, before the review).
pub trait EnvironmentPreparer: Send + Sync {
    fn prepare(&self, work_dir: &str, log: &dyn Fn(&str)) -> Result<PreparedEnvironment, String>;
}

pub type Log = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Clone)]
pub struct ReviewerDeps {
    pub runner: Arc<dyn Runner>,
    pub tokens: Arc<TokenStore>,
    pub engine: Arc<dyn ReviewEngine>,
    pub store: Arc<StateStore>,
    pub checkouts: Arc<CheckoutManager>,
    pub config: AutoreviewConfig,
    pub paths: AutoreviewPaths,
    pub log: Log,
    pub now: Clock,
    /// A number in [0, 1): picks the acknowledgement line.
    pub random: Arc<dyn Fn() -> f64 + Send + Sync>,
    pub lessons: Option<Arc<dyn LessonMemory>>,
    pub environments: Option<Arc<dyn EnvironmentPreparer>>,
    /// The home directory `~` in guide paths and checkout roots expands to.
    pub home: String,
}

/// `host/owner/repo`: the key of a repository's ledger of fates.
pub fn repo_key(r: &PullRef) -> String {
    format!("{}/{}/{}", r.host, r.owner, r.repo).to_lowercase()
}

/// Indexes the checkout at `rev` into the knowledge graph and returns the spec's `knowledge` (store, repository,
/// commit) for the kernel to read. Never fails a review: an error is logged and the review runs without it. A
/// directory that is not a git checkout (a test's) is not indexed.
pub fn index_knowledge(deps: &ReviewerDeps, repo: &str, work_dir: &str, rev: &str) -> Option<Value> {
    if !std::path::Path::new(work_dir).join(".git").exists() {
        return None;
    }
    let db = std::path::Path::new(&deps.paths.knowledge);
    match knowledge::index_tree(db, repo, std::path::Path::new(work_dir), rev) {
        Ok(stats) => Some(json!({ "db": deps.paths.knowledge, "repo": repo, "rev": stats.rev })),
        Err(error) => {
            (deps.log)(&format!("{repo}: the knowledge graph was not updated: {error}"));
            None
        }
    }
}

/// Record what became of posted findings: into the state's ledger and, when configured, into the lesson memory.
/// Never fails a review: an error is logged.
fn learn(deps: &ReviewerDeps, r: &PullRef, fates: &[Fate], name: &str) -> Result<(), Error> {
    if fates.is_empty() || !deps.config.learn {
        return Ok(());
    }
    let repo = repo_key(r);
    let now = iso_from_ms((deps.now)());
    let lessons = deps.store.update(|state| {
        let ledger = state.lessons.get_or_insert_with(Default::default);
        let merged = merge_fates(ledger.get(&repo).map_or(&[][..], Vec::as_slice), fates);
        let lessons = lessons_for(&merged, &now);
        ledger.insert(repo.clone(), merged);
        lessons
    })?;
    let accepted = fates.iter().filter(|fate| fate.fate == FateKind::Accepted).count();
    let lowered: Vec<String> = lessons
        .iter()
        .filter(|lesson| lesson.lowered)
        .map(|lesson| format!("{}/{}", lesson.category, lesson.kind))
        .collect();
    (deps.log)(&format!(
        "{name}: learned the fate of {} finding{} ({accepted} accepted, {} rejected){}",
        fates.len(),
        if fates.len() == 1 { "" } else { "s" },
        fates.len() - accepted,
        if lowered.is_empty() {
            String::new()
        } else {
            format!("; lowered: {}", lowered.join(", "))
        }
    ));
    if let Some(memory) = &deps.lessons
        && let Err(error) = memory.retain(&repo, fates, &lessons)
    {
        (deps.log)(&format!("{name}: the fates were not retained in memory: {error}"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub account: Account,
    pub pull: PullRef,
    /// Why discovery picked it: notification reasons and search names.
    pub reasons: Vec<String>,
    /// When discovery found it (epoch ms).
    pub picked_at: i64,
    /// When GitHub last updated the notification that led here (epoch ms); absent for a search result.
    pub notified_at: Option<i64>,
}

/// A posted (or dry-run) review.
#[derive(Debug, Clone, PartialEq)]
pub struct Reviewed {
    pub verdict: &'static str,
    pub sha: String,
    pub result: Value,
    pub plan: ReviewPlan,
    pub review_id: Option<i64>,
    /// The dry-run files' path without extension.
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Skipped(String),
    Requeue(String),
    GaveUp(String),
    Posted(Box<Reviewed>),
    DryRun(Box<Reviewed>),
}

impl Outcome {
    /// The TypeScript `kind`.
    pub fn kind(&self) -> &'static str {
        match self {
            Outcome::Skipped(_) => "skipped",
            Outcome::Requeue(_) => "requeue",
            Outcome::GaveUp(_) => "gave-up",
            Outcome::Posted(_) => "posted",
            Outcome::DryRun(_) => "dry-run",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Outcome::Skipped(reason) | Outcome::Requeue(reason) | Outcome::GaveUp(reason) => Some(reason),
            _ => None,
        }
    }

    pub fn reviewed(&self) -> Option<&Reviewed> {
        match self {
            Outcome::Posted(reviewed) | Outcome::DryRun(reviewed) => Some(reviewed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReviewOptions {
    /// Review even when nothing new asks for it (`ultron autoreview review`).
    pub force: bool,
    pub dry_run: Option<bool>,
}

/// State key of an account's view of a pull request.
pub fn pull_state_key(account: &Account, r: &PullRef) -> String {
    format!("{}@{}", account.login, pull_key(r))
}

/// Whether `body` @mentions `login` (not as part of a longer name or an email address).
pub fn mentions(body: &str, login: &str) -> bool {
    let Ok(at) = Regex::new(&format!("(?i)^@{}", regex::escape(login))) else {
        return false;
    };
    let before_ok = |c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '-'));
    let after_ok = |c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '/'));
    body.char_indices().filter(|(_, c)| *c == '@').any(|(index, _)| {
        let Some(found) = at.find(&body[index..]) else {
            return false;
        };
        let before = body[..index].chars().next_back();
        let after = body[index + found.end()..].chars().next();
        before.is_none_or(before_ok) && after.is_none_or(after_ok)
    })
}

/// When the account was last @mentioned on the pull request by somebody else (ISO time), if ever.
pub fn latest_mention(login: &str, pull: &PullRequest, comments: &[Comment], reviews: &[Review]) -> Option<String> {
    let me = login.to_lowercase();
    let mut latest: Option<String> = None;
    let mut consider = |author: &str, body: &str, at: &str| {
        if author.to_lowercase() == me || at.is_empty() || !mentions(body, login) {
            return;
        }
        if latest.as_deref().is_none_or(|latest| cmp16(at, latest).is_gt()) {
            latest = Some(at.into());
        }
    };
    consider(&pull.author, &pull.body, &pull.created_at);
    for comment in comments {
        consider(&comment.user, &comment.body, &comment.updated_at);
    }
    for review in reviews {
        consider(&review.user, &review.body, &review.submitted_at);
    }
    latest
}

pub struct DecisionInput<'a> {
    pub pull: &'a PullRequest,
    /// The commit of this account's last review, and when it was made.
    pub last_sha: Option<String>,
    pub last_at: Option<String>,
    /// That last review requested changes: new commits are reviewed so the block is lifted or confirmed.
    pub last_requested_changes: bool,
    pub mention_at: Option<String>,
    /// A review by this account is requested now: directly, or through a requested team it belongs to.
    pub requested: bool,
    /// `autoreview.skipBots`: a bot's pull request is reviewed only when the account is @mentioned.
    pub skip_bots: bool,
    /// When the review was last requested; asked only when the head was already reviewed.
    pub requested_at: &'a dyn Fn() -> Result<Option<String>, Error>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Decision {
    pub review: bool,
    pub reason: String,
}

fn decision(review: bool, reason: impl Into<String>) -> Decision {
    Decision {
        review,
        reason: reason.into(),
    }
}

fn short(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// Whether a review is due. A review ends the account's part unless something asks again: a pending request, a
/// re-request, a new mention, or (because this account then blocks the merge) its own request for changes.
pub fn decide(input: &DecisionInput) -> Result<Decision, Error> {
    let pull = input.pull;
    let later = |a: &str, b: &str| cmp16(a, b).is_gt();
    let new_mention = input
        .mention_at
        .as_deref()
        .is_some_and(|at| input.last_at.as_deref().is_none_or(|last| later(at, last)));
    let ended = if pull.merged { "merged" } else { "closed" };
    if pull.state != "open" {
        return Ok(if new_mention {
            decision(true, format!("mentioned on a {ended} pull request"))
        } else {
            decision(false, format!("the pull request is {ended}"))
        });
    }
    if pull.draft && !new_mention {
        return Ok(decision(false, "the pull request is a draft"));
    }
    if input.skip_bots && pull.author_is_bot && !new_mention {
        return Ok(decision(
            false,
            format!("the author {} is a bot (autoreview.skipBots)", pull.author),
        ));
    }
    let Some(last_sha) = &input.last_sha else {
        if input.requested {
            return Ok(decision(true, "review requested"));
        }
        if new_mention {
            return Ok(decision(true, "mentioned"));
        }
        return Ok(decision(false, "no review request or mention for this account"));
    };
    if new_mention {
        return Ok(decision(true, "mentioned again"));
    }
    if *last_sha != pull.head_sha {
        let since = format!("new commits since {}", short(last_sha));
        if input.requested {
            return Ok(decision(true, format!("{since}, review requested")));
        }
        if input.last_requested_changes {
            return Ok(decision(true, format!("{since}, after this account requested changes")));
        }
        return Ok(decision(
            false,
            format!(
                "{} was reviewed and nothing asks for a review of the new commits",
                short(last_sha)
            ),
        ));
    }
    if input.requested {
        let requested_at = (input.requested_at)()?;
        if let (Some(requested_at), Some(last_at)) = (&requested_at, &input.last_at)
            && later(requested_at, last_at)
        {
            return Ok(decision(true, "review requested again"));
        }
    }
    Ok(decision(
        false,
        format!(
            "{} is already reviewed and nothing new asks for it",
            short(&pull.head_sha)
        ),
    ))
}

/// GitHub rejects a comment body over 65,536 characters; the acknowledgement stays well under it.
pub const MAX_ACK_CHARS: usize = 60_000;

/// The acknowledgement posted when a review starts: the line as a quotation with its attribution, the commit, and
/// (with `autoreview.ackArt`) the art in a fenced block longer than any backtick run in the art; art that would take
/// the comment past the size limit is left out.
pub fn ack_body(line: &str, sha: &str, art: &str) -> String {
    let quote = flatten(line);
    let body = format!("> *{quote}*\n> — Ultron\n\nReviewing `{}`.", short(sha));
    let art = art.replace("\r\n", "\n");
    let banner = art.trim_start_matches('\n').trim_end_matches(is_space);
    if banner.is_empty() {
        return body;
    }
    let longest = Regex::new("`+")
        .expect("static pattern")
        .find_iter(banner)
        .map(|run| run.as_str().len())
        .fold(2, usize::max);
    let fence = "`".repeat(longest + 1);
    let full = format!("{body}\n\n{fence}text\n{banner}\n{fence}");
    if len16(&full) <= MAX_ACK_CHARS { full } else { body }
}

/// A line from `lines`, at random, other than the one used last on this pull request.
pub fn pick_ack_line(lines: &[String], previous: Option<&str>, random: f64) -> String {
    let choices: Vec<&String> = if lines.len() > 1 {
        lines.iter().filter(|line| Some(line.as_str()) != previous).collect()
    } else {
        lines.iter().collect()
    };
    if choices.is_empty() {
        return String::new();
    }
    let index = ((random * choices.len() as f64).floor().max(0.0) as usize).min(choices.len() - 1);
    choices[index].clone()
}

fn latest_own_review<'a>(reviews: &'a [Review], login: &str) -> Option<&'a Review> {
    let me = login.to_lowercase();
    let mut latest: Option<&Review> = None;
    for review in reviews {
        if review.user.to_lowercase() != me || review.state == "PENDING" || review.submitted_at.is_empty() {
            continue;
        }
        if latest.is_none_or(|latest| cmp16(&review.submitted_at, &latest.submitted_at).is_gt()) {
            latest = Some(review);
        }
    }
    latest
}

/// The dry-run markdown file.
pub fn dry_run_markdown(candidate: &Candidate, sha: &str, plan: &ReviewPlan, ack: Option<&str>) -> String {
    let mut lines = vec![
        format!(
            "# Would-be review of {} at {} as {}",
            pull_key(&candidate.pull),
            short(sha),
            candidate.account.login
        ),
        String::new(),
        format!("Event: {}", plan.event),
        String::new(),
    ];
    if let Some(ack) = ack {
        lines.extend([
            "## Acknowledgement comment".into(),
            String::new(),
            ack.into(),
            String::new(),
        ]);
    }
    lines.extend(["## Summary".into(), String::new(), plan.body.clone(), String::new()]);
    if !plan.comments.is_empty() {
        lines.extend(["## Inline comments".into(), String::new()]);
    }
    for comment in &plan.comments {
        let place = match comment.start_line {
            None => format!("{}:{}", comment.path, comment.line),
            Some(start) => format!("{}:{start}-{}", comment.path, comment.line),
        };
        lines.extend([
            format!("### {place}"),
            String::new(),
            comment.body.clone(),
            String::new(),
        ]);
    }
    format!("{}\n", lines.join("\n"))
}

/// A pull request whose review is due: what the decision read, and the acknowledgement.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub candidate: Candidate,
    pub dry_run: bool,
    pub pull: PullRequest,
    pub reason: String,
    pub last_sha: Option<String>,
    /// The acknowledgement comment (posted, or in a dry run only written to the dry-run file).
    pub ack: Option<String>,
    /// When the acknowledgement for this head commit was posted (epoch ms).
    pub ack_at: Option<i64>,
    /// Milliseconds from the notification to the acknowledgement, when both are known and it was posted now.
    pub tag_to_ack_ms: Option<i64>,
}

/// The verdict as the state and the outcome name it.
pub fn verdict_name(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Approve => "approve",
        Verdict::RequestChanges => "request_changes",
        Verdict::Comment => "comment",
    }
}

fn seconds(ms: i64) -> String {
    format!("{} s", to_fixed(ms as f64 / 1000.0, 1))
}

fn github_of(deps: &ReviewerDeps, account: &Account) -> GitHub {
    let tokens = deps.tokens.clone();
    let owner = account.clone();
    GitHub::new(
        deps.runner.clone(),
        account.clone(),
        Arc::new(move || tokens.env(&owner)),
        Some(deps.now.clone()),
    )
}

/// Either a prepared review or an outcome that ends here.
#[derive(Debug, Clone, PartialEq)]
pub enum Preparation {
    Due(Box<Prepared>),
    Done(Outcome),
}

/// Decide whether a pull request is due for a review by an account and, if so, acknowledge it at once (one comment
/// per pull request and head commit; none on a closed or merged pull request). The review itself is [`run_review`].
pub fn prepare_pull(deps: &ReviewerDeps, candidate: &Candidate, options: ReviewOptions) -> Result<Preparation, Error> {
    let (account, r) = (&candidate.account, &candidate.pull);
    let dry_run = options.dry_run.unwrap_or(deps.config.dry_run);
    let github = github_of(deps, account);
    let key = pull_state_key(account, r);
    let name = format!("{} as {}", pull_key(r), account.login);

    let pull = github.pull(r)?;
    if pull.head_sha.is_empty() {
        return Err(Error::GitHub {
            message: format!("{name}: the pull request has no head commit"),
            status: 0,
        });
    }
    let head = pull.head_sha.clone();
    let reviews = github.reviews(r)?;
    let saved: PullState = deps.store.read().pulls.get(&key).cloned().unwrap_or_default();
    // A dry-run "review" counts only while still in dry-run: switching to posting reviews the commit for real.
    let saved_counts = saved.last_reviewed_sha.is_some() && (dry_run || saved.last_review_dry_run != Some(true));
    let own = latest_own_review(&reviews, &account.login);
    let mut last_sha = own.map(|review| review.commit_id.clone());
    let mut last_at = own.map(|review| review.submitted_at.clone());
    let mut last_requested_changes = own.is_some_and(|review| review.state == "CHANGES_REQUESTED");
    let saved_at = saved.last_reviewed_at.clone().unwrap_or_default();
    if saved_counts && last_at.as_deref().is_none_or(|at| cmp16(&saved_at, at).is_gt()) {
        last_sha = saved.last_reviewed_sha.clone();
        last_at = saved.last_reviewed_at.clone();
        last_requested_changes = saved.last_verdict.as_deref() == Some("request_changes");
    }

    let me = account.login.to_lowercase();
    let mut requested = pull.requested_reviewers.iter().any(|login| login.to_lowercase() == me);
    let decided = if options.force {
        decision(true, "asked for on the command line")
    } else {
        // A requested team counts only when this account is a member of it.
        if !requested && pull.state == "open" && !pull.draft {
            for team in &pull.requested_teams {
                let member = github.in_team(&r.owner, team)?;
                if member.is_none() {
                    (deps.log)(&format!(
                        "{name}: membership of the requested team {}/{team} cannot be read; not counted",
                        r.owner
                    ));
                }
                if member == Some(true) {
                    requested = true;
                    break;
                }
            }
        }
        let mut mention_at = None;
        if !(requested && last_sha.is_none() && pull.state == "open" && !pull.draft) {
            let mut comments = github.issue_comments(r)?;
            comments.extend(github.review_comments(r)?);
            mention_at = latest_mention(&account.login, &pull, &comments, &reviews);
        }
        let requested_at = || github.last_review_request_at(r);
        decide(&DecisionInput {
            pull: &pull,
            last_sha: last_sha.clone(),
            last_at,
            last_requested_changes,
            mention_at,
            requested,
            skip_bots: deps.config.skip_bots,
            requested_at: &requested_at,
        })?
    };
    if !decided.review {
        // Nothing more to watch for once the pull request is closed: what was still open then is its findings' fate.
        if pull.state != "open" && saved.last_verdict.is_some() {
            if saved.closed != Some(true)
                && deps.config.learn
                && saved.findings.iter().any(|finding| finding.status == "open")
            {
                let learned = (|| {
                    let threads = if pull.merged {
                        Some(github.review_threads(r)?)
                    } else {
                        None
                    };
                    let fates = fates_of(&FateInput {
                        pull: pull_key(r),
                        now: iso_from_ms((deps.now)()),
                        findings: saved.findings.iter().map(PostedFinding::ledger).collect(),
                        threads: threads.map(thread_states).unwrap_or_default(),
                        merged: pull.merged,
                        ..FateInput::default()
                    });
                    learn(deps, r, &fates, &format!("{} as {}", pull_key(r), account.login))
                })();
                if let Err(error) = learned {
                    (deps.log)(&format!("{}: the fates at close were not read: {error}", pull_key(r)));
                }
            }
            deps.store.update_pull(&key, |state, _| state.closed = Some(true))?;
        }
        return Ok(Preparation::Done(Outcome::Skipped(decided.reason)));
    }

    if !options.force && saved.gave_up_sha.as_deref() == Some(head.as_str()) {
        return Ok(Preparation::Done(Outcome::Skipped(
            "gave up on this commit earlier".into(),
        )));
    }
    if !options.force && saved.attempts.get(&head).copied().unwrap_or(0) >= MAX_ATTEMPTS {
        if !dry_run {
            github.post_comment(
                r,
                &format!(
                    "I could not review `{}` after {MAX_ATTEMPTS} attempts. Push a new commit or mention me again to retry.",
                    short(&head)
                ),
            )?;
        }
        deps.store
            .update_pull(&key, |state, _| state.gave_up_sha = Some(head.clone()))?;
        (deps.log)(&format!(
            "{name}: gave up on {} after {MAX_ATTEMPTS} attempts",
            short(&head)
        ));
        return Ok(Preparation::Done(Outcome::GaveUp(format!(
            "{MAX_ATTEMPTS} attempts failed"
        ))));
    }

    // Acknowledge once per head commit, as soon as the review is known to be due.
    let mut ack = None;
    let mut ack_at = if saved.last_ack_sha.as_deref() == Some(head.as_str()) {
        saved.last_ack_at.as_deref().and_then(parse_iso_ms)
    } else {
        None
    };
    let mut tag_to_ack_ms = None;
    if deps.config.ack
        && !deps.config.ack_lines.is_empty()
        && saved.last_ack_sha.as_deref() != Some(head.as_str())
        && pull.state == "open"
    {
        let line = pick_ack_line(&deps.config.ack_lines, saved.last_ack_line.as_deref(), (deps.random)());
        let body = ack_body(&line, &head, &deps.config.ack_art);
        if !dry_run {
            let posted = github.post_comment(r, &body).and_then(|_| {
                let posted = (deps.now)();
                ack_at = Some(posted);
                if let Some(notified) = candidate.notified_at {
                    tag_to_ack_ms = Some((posted - notified).max(0));
                }
                deps.store.update_pull(&key, |state, _| {
                    state.last_ack_sha = Some(head.clone());
                    state.last_ack_line = Some(line.clone());
                    state.last_ack_at = Some(iso_from_ms(posted));
                })?;
                (deps.log)(&format!(
                    "{name}: acknowledged {} ({}){}",
                    short(&head),
                    decided.reason,
                    tag_to_ack_ms.map_or(String::new(), |ms| format!(", {} after the notification", seconds(ms)))
                ));
                Ok(())
            });
            match posted {
                Err(error @ Error::RateLimit { .. }) => return Err(error),
                Err(error) => (deps.log)(&format!("{name}: the acknowledgement was not posted: {error}")),
                Ok(()) => {}
            }
        }
        ack = Some(body);
    }
    Ok(Preparation::Due(Box::new(Prepared {
        candidate: candidate.clone(),
        dry_run,
        pull,
        reason: decided.reason,
        last_sha,
        ack,
        ack_at,
        tag_to_ack_ms,
    })))
}

/// Review one pull request as one account, if a review is due: the acknowledgement, then the review.
pub fn review_pull(deps: &ReviewerDeps, candidate: &Candidate, options: ReviewOptions) -> Result<Outcome, Error> {
    match prepare_pull(deps, candidate, options)? {
        Preparation::Done(outcome) => Ok(outcome),
        Preparation::Due(prepared) => run_review(deps, &prepared),
    }
}

fn thread_states(threads: indexmap::IndexMap<i64, crate::github::ReviewThread>) -> HashMap<i64, ThreadState> {
    threads
        .into_iter()
        .map(|(id, thread)| {
            (
                id,
                ThreadState {
                    id: thread.id,
                    resolved: thread.resolved,
                    thumbs_up: thread.thumbs_up,
                    thumbs_down: thread.thumbs_down,
                },
            )
        })
        .collect()
}

/// An earlier finding's status as the re-review settled it.
struct EarlierStatus {
    id: String,
    status: String,
    line: i64,
}

fn earlier_statuses(result: &Value) -> Vec<EarlierStatus> {
    let mut out = Vec::new();
    if let Some(Value::Array(items)) = result.get("earlier") {
        for item in items {
            out.push(EarlierStatus {
                id: crate::github::text(item.get("id")),
                status: crate::github::text(item.get("status")),
                line: item.get("line").and_then(Value::as_i64).unwrap_or(0),
            });
        }
    }
    out
}

fn write_private(path: &str, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(text.as_bytes())
}

fn review_input(head: &str, plan: &ReviewPlan) -> ReviewInput {
    ReviewInput {
        commit_id: head.into(),
        event: plan.event.into(),
        body: plan.body.clone(),
        comments: plan
            .comments
            .iter()
            .map(|comment| ReviewCommentInput {
                path: comment.path.clone(),
                line: comment.line,
                side: comment.side.into(),
                start_line: comment.start_line,
                start_side: comment.start_line.and(comment.start_side.map(String::from)),
                body: comment.body.clone(),
            })
            .collect(),
    }
}

/// Review a prepared pull request and post the review (or write it to the dry-run directory).
pub fn run_review(deps: &ReviewerDeps, prepared: &Prepared) -> Result<Outcome, Error> {
    let candidate = &prepared.candidate;
    let (account, r) = (&candidate.account, &candidate.pull);
    let key = pull_state_key(account, r);
    let name = format!("{} as {}", pull_key(r), account.login);
    let head = prepared.pull.head_sha.clone();
    let saved: PullState = deps.store.read().pulls.get(&key).cloned().unwrap_or_default();
    (deps.log)(&format!("{name}: reviewing {} ({})", short(&head), prepared.reason));
    deps.store.update_pull(&key, |state, _| {
        *state.attempts.entry(head.clone()).or_insert(0) += 1;
    })?;
    let forget_attempt = || {
        deps.store.update_pull(&key, |state, _| {
            let count = state.attempts.get(&head).copied().unwrap_or(1).saturating_sub(1);
            if count == 0 {
                state.attempts.shift_remove(&head);
            } else {
                state.attempts.insert(head.clone(), count);
            }
        })
    };
    let scratch = path_join(&[
        &std::env::temp_dir().to_string_lossy(),
        &format!("ultron-autoreview-diff-{}", random_hex(6)),
    ]);
    make_private_dir(&scratch).map_err(|e| Error::Other(format!("{scratch}: {e}")))?;
    let mut checkout: Option<Checkout> = None;
    let outcome = review_in(deps, prepared, &saved, &scratch, &mut checkout, &forget_attempt);
    if let Err(Error::RateLimit { .. }) = &outcome {
        // Waiting for a rate limit is not a failed attempt.
        let _ = forget_attempt();
    }
    let _ = std::fs::remove_dir_all(&scratch);
    if let Some(checkout) = &checkout {
        checkout.cleanup();
    }
    outcome
}

fn review_in(
    deps: &ReviewerDeps,
    prepared: &Prepared,
    saved: &PullState,
    scratch: &str,
    checkout_slot: &mut Option<Checkout>,
    forget_attempt: &dyn Fn() -> Result<(), Error>,
) -> Result<Outcome, Error> {
    let candidate = &prepared.candidate;
    let (account, r) = (&candidate.account, &candidate.pull);
    let (dry_run, pull, last_sha, ack) = (prepared.dry_run, &prepared.pull, &prepared.last_sha, &prepared.ack);
    let github = github_of(deps, account);
    let key = pull_state_key(account, r);
    let name = format!("{} as {}", pull_key(r), account.login);
    let head = pull.head_sha.clone();
    let me = account.login.to_lowercase();
    let now = || (deps.now)();
    let io = |e: std::io::Error| Error::Other(e.to_string());

    // Source for the verifier; without it the review reads the diff alone and cannot approve.
    let heads = PullHeads {
        head_sha: head.clone(),
        base_ref: pull.base_ref.clone(),
        base_sha: pull.base_sha.clone(),
    };
    match deps
        .tokens
        .env(account)
        .and_then(|env| deps.checkouts.prepare(r, &heads, &env))
    {
        Ok(prepared_checkout) => *checkout_slot = Some(prepared_checkout),
        Err(Error::StaleHead(message)) => {
            forget_attempt()?;
            return Ok(Outcome::Requeue(message));
        }
        Err(error) => (deps.log)(&format!("{name}: no checkout, reviewing the diff only: {error}")),
    }
    let checkout = checkout_slot.as_ref();
    let review_comments = github.review_comments(r)?;
    let others: Vec<Value> = review_comments
        .iter()
        .filter(|comment| comment.user.to_lowercase() != me && comment.path.is_some())
        .map(|comment| {
            let mut item = Map::new();
            item.insert("author".into(), json!(comment.user));
            item.insert("body".into(), json!(comment.body));
            if let Some(path) = &comment.path {
                item.insert("path".into(), json!(path));
            }
            if let Some(line) = comment.line {
                item.insert("line".into(), json!(line));
            }
            Value::Object(item)
        })
        .collect();
    let ci = github.check_summary(r, &head)?;

    let earlier: Vec<PostedFinding> = saved.findings.iter().filter(|f| f.status == "open").cloned().collect();
    let lessons = if deps.config.learn {
        let ledger = deps
            .store
            .read()
            .lessons
            .and_then(|mut all| all.shift_remove(&repo_key(r)));
        lessons_for(&ledger.unwrap_or_default(), &iso_from_ms(now()))
    } else {
        Vec::new()
    };
    let mut since_sha = None;
    let full_diff;
    let mut review_diff;
    let mut earlier_diff = None;
    if let Some(checkout) = checkout {
        full_diff = checkout.diff(&checkout.merge_base, &head)?;
        review_diff = full_diff.clone();
        if let Some(last) = last_sha.as_deref().filter(|last| *last != head)
            && checkout.is_ancestor(last, &head)
        {
            // The head moved forward: review what is new. After a force-push the whole diff is reviewed again.
            since_sha = Some(last.to_string());
            review_diff = checkout.diff(last, &head)?;
            earlier_diff = Some(review_diff.clone());
        } else if last_sha.as_deref() == Some(head.as_str()) {
            earlier_diff = Some(String::new());
        }
    } else {
        full_diff = github.pull_diff(r)?;
        review_diff = full_diff.clone();
        if last_sha.as_deref() == Some(head.as_str()) {
            earlier_diff = Some(String::new());
        }
    }
    let diff_path = path_join(&[scratch, "review.diff"]);
    let post_diff_path = path_join(&[scratch, "pull.diff"]);
    let earlier_diff_path = path_join(&[scratch, "earlier.diff"]);
    std::fs::write(&diff_path, &review_diff).map_err(io)?;
    std::fs::write(&post_diff_path, &full_diff).map_err(io)?;
    if let Some(earlier_diff) = &earlier_diff {
        std::fs::write(&earlier_diff_path, earlier_diff).map_err(io)?;
    }
    // Tests of the reviewed project run only where this account could push anyway, or the owner is trusted.
    let run_tests = checkout.is_some()
        && deps.config.mode != "fast"
        && deps.config.run_tests
        && tests_eligible(&deps.config, &r.owner, github.can_push(r)?);
    let test_env = deps
        .config
        .test_env
        .get(&format!("{}/{}", r.owner, r.repo).to_lowercase())
        .cloned();
    // Without an explicit environment: a prepared one, else an existing local checkout of this repository.
    let prepared_env = match (checkout, &deps.environments) {
        (Some(checkout), Some(environments)) if run_tests && test_env.is_none() && deps.config.prepare_envs => {
            let log = |line: &str| (deps.log)(&format!("{}: {line}", pull_key(r)));
            match environments.prepare(&checkout.work_dir, &log) {
                Ok(env) => Some(env),
                Err(error) => {
                    (deps.log)(&format!("{}: prepare failed: {error}", pull_key(r)));
                    None
                }
            }
        }
        _ => None,
    };
    if let Some(env) = &prepared_env {
        (deps.log)(&format!(
            "{}: environment {}{}: {}{}",
            pull_key(r),
            env.hash,
            if env.cached { " (cached)" } else { "" },
            if env.prepared.is_empty() {
                "nothing prepared".to_string()
            } else {
                env.prepared.join("; ")
            },
            if env.failures.is_empty() {
                String::new()
            } else {
                format!("; failed: {}", env.failures.join("; "))
            }
        ));
    }
    let use_prepared = prepared_env
        .as_ref()
        .filter(|env| env.failures.is_empty() && !env.prepared.is_empty());
    let test_checkout = match use_prepared {
        Some(env) => Some(env.dir.clone()),
        None if run_tests && test_env.is_none() && !deps.config.checkout_roots.is_empty() => {
            find_checkout(deps.runner.as_ref(), &deps.config.checkout_roots, r)
        }
        None => None,
    };

    let mut spec = Map::new();
    if let Some(checkout) = checkout {
        spec.insert("workDir".into(), json!(checkout.work_dir));
        spec.insert("baseSha".into(), json!(checkout.merge_base));
        if let Some(knowledge) = index_knowledge(deps, &repo_key(r), &checkout.work_dir, "HEAD") {
            spec.insert("knowledge".into(), knowledge);
        }
    }
    spec.insert("runTests".into(), json!(run_tests));
    if let Some(test_env) = test_env.as_ref().filter(|_| run_tests) {
        spec.insert("testEnv".into(), json!(test_env));
        spec.insert("testEnvKind".into(), json!("testEnv"));
    }
    if let Some(test_checkout) = &test_checkout {
        spec.insert("testCheckout".into(), json!(test_checkout));
        spec.insert(
            "testEnvKind".into(),
            json!(if use_prepared.is_some() { "prepared" } else { "checkout" }),
        );
    }
    if let Some(env) = use_prepared {
        spec.insert("testToolchain".into(), json!(env.toolchain));
    }
    if !deps.config.guides.is_empty() {
        let guides: Vec<String> = deps
            .config
            .guides
            .iter()
            .map(|path| expand_path(path, &deps.home))
            .collect();
        spec.insert("guides".into(), json!(guides));
    }
    spec.insert("repo".into(), json!(format!("{}/{}", r.owner, r.repo)));
    spec.insert("diffPath".into(), json!(diff_path));
    spec.insert("postDiffPath".into(), json!(post_diff_path));
    spec.insert("label".into(), json!(format!("{}/{}#{}", r.owner, r.repo, r.number)));
    if let Ok(Value::Object(settings)) = serde_json::to_value(engine_settings(&deps.config)) {
        spec.extend(settings);
    }
    let mut context = Map::new();
    context.insert("title".into(), json!(pull.title));
    context.insert("description".into(), json!(pull.body));
    if let Some(ci) = &ci {
        context.insert("ci".into(), json!(ci));
    }
    context.insert("comments".into(), Value::Array(others));
    spec.insert("context".into(), Value::Object(context));
    if !lessons.is_empty() {
        spec.insert("lessons".into(), serde_json::to_value(&lessons).unwrap_or(Value::Null));
    }
    if !earlier.is_empty() {
        let items: Vec<Value> = earlier
            .iter()
            .map(|f| json!({ "id": f.id, "file": f.file, "line": f.line, "severity": f.severity, "claim": f.claim }))
            .collect();
        spec.insert("earlier".into(), Value::Array(items));
        if earlier_diff.is_some() {
            spec.insert("earlierDiffPath".into(), json!(earlier_diff_path));
        }
    }
    let raw = deps.engine.review(&Value::Object(spec), None).map_err(Error::Other)?;
    let result: EngineResult = serde_json::from_value(raw.clone())
        .map_err(|error| Error::Other(format!("the engine returned an unreadable result: {error}")))?;
    let reviewed_at_ms = now();

    // A review of a commit that is no longer the head would be stale: drop it and come back.
    let current = github.pull(r)?;
    if current.head_sha != head {
        forget_attempt()?;
        (deps.log)(&format!(
            "{name}: the head moved to {} during the review; discarded",
            short(&current.head_sha)
        ));
        return Ok(Outcome::Requeue("the head commit moved during the review".into()));
    }

    let state = if current.state == "open" {
        "open"
    } else if current.merged {
        "merged"
    } else {
        "closed"
    };
    let mut plan = plan_review(
        &result,
        &PlanOptions {
            self_authored: pull.author.to_lowercase() == me,
            state: state.into(),
            head_sha: head.clone(),
            since_sha: since_sha.clone(),
            signature: deps.config.signature,
            block_at: Some(deps.config.block_at.clone()),
            max_comments: Some(deps.config.max_comments.0 as i64),
        },
    );
    // Nothing that names a private review guide is posted.
    let guide_names: Vec<String> = match raw.get("guideNames") {
        Some(Value::Array(names)) => names.iter().filter_map(|n| n.as_str().map(String::from)).collect(),
        _ => Vec::new(),
    };
    let (checked, withheld) = without_guide_mentions(&plan, &guide_names);
    if !withheld.is_empty() {
        plan = checked;
        (deps.log)(&format!(
            "{name}: withheld {}: it named a private review guide",
            withheld.join("; ")
        ));
    }
    let reviewed_at = iso_from_ms(now());
    let short_sha = short(&head);
    let raw_findings = match raw.get("findings") {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    };
    let mut posted: Vec<PostedFinding> = Vec::new();
    let mut track = |index: usize| -> usize {
        let finding = &result.findings[index];
        let kind = raw_findings
            .get(index)
            .and_then(|f| f.get("kind"))
            .and_then(Value::as_str);
        posted.push(PostedFinding {
            id: format!("{short_sha}-{}", index + 1),
            file: finding.file.clone(),
            line: finding.line,
            severity: finding.level().into(),
            claim: finding.claim.clone(),
            claim_hash: claim_hash(&finding.claim),
            category: Some(finding.category.clone()),
            kind: kind.map(String::from),
            sha: head.clone(),
            comment_id: None,
            thread_id: None,
            status: "open".into(),
            extra: Default::default(),
        });
        posted.len() - 1
    };
    let mut review_id = None;
    let mut path = None;
    let mut resolved: Vec<String> = Vec::new();
    let statuses = earlier_statuses(&raw);
    if dry_run {
        make_private_dir(&deps.paths.dry_run).map_err(io)?;
        let base = path_join(&[
            &deps.paths.dry_run,
            &format!("{}-{}-{}-{short_sha}-{}", r.owner, r.repo, r.number, account.login),
        ]);
        let mut saved_json = Map::new();
        saved_json.insert("account".into(), json!(account_key(account)));
        saved_json.insert("pull".into(), json!(pull_key(r)));
        saved_json.insert("sha".into(), json!(head));
        if let Some(ack) = ack {
            saved_json.insert("ack".into(), json!(ack));
        }
        saved_json.insert(
            "review".into(),
            serde_json::to_value(review_input(&head, &plan)).unwrap_or(Value::Null),
        );
        saved_json.insert("result".into(), raw.clone());
        write_private(&format!("{base}.json"), &format!("{}\n", to_json_1(&saved_json))).map_err(io)?;
        write_private(
            &format!("{base}.md"),
            &dry_run_markdown(candidate, &head, &plan, ack.as_deref()),
        )
        .map_err(io)?;
        for comment in &plan.comments {
            track(comment.finding);
        }
        for index in &plan.in_summary {
            track(*index);
        }
        path = Some(base);
    } else {
        let id = match github.post_review(r, &review_input(&head, &plan)) {
            Ok(id) => id,
            // GitHub rejects the whole review when one inline comment cannot be placed: post it without them.
            Err(Error::GitHub { message, status: 422 }) if !plan.comments.is_empty() => {
                (deps.log)(&format!(
                    "{name}: GitHub rejected the inline comments ({message}); posting them in the summary"
                ));
                plan = without_inline(&plan, &result);
                github.post_review(r, &review_input(&head, &plan))?
            }
            Err(error) => return Err(error),
        };
        review_id = Some(id);
        let mut entries: Vec<(String, usize)> = Vec::new();
        for comment in &plan.comments {
            entries.push((comment.marker.clone(), track(comment.finding)));
        }
        for index in &plan.in_summary {
            track(*index);
        }
        // Read the review back: an inline comment GitHub dropped is posted again on its own.
        if !plan.comments.is_empty() {
            let read_back = (|| -> Result<(), Error> {
                let kept = github.comments_of_review(r, id)?;
                for (comment, (_, entry)) in plan.comments.iter().zip(&entries) {
                    if let Some(found) = kept.iter().find(|item| item.body.contains(&comment.marker)) {
                        posted[*entry].comment_id = Some(found.id);
                        continue;
                    }
                    let input = ReviewCommentInput {
                        path: comment.path.clone(),
                        line: comment.line,
                        side: comment.side.into(),
                        start_line: comment.start_line.filter(|_| comment.start_side.is_some()),
                        start_side: comment
                            .start_side
                            .filter(|_| comment.start_line.is_some())
                            .map(String::from),
                        body: comment.body.clone(),
                    };
                    match github.post_review_comment(r, &head, &input) {
                        Ok(comment_id) => {
                            posted[*entry].comment_id = Some(comment_id);
                            (deps.log)(&format!(
                                "{name}: re-posted a dropped inline comment on {}:{}",
                                comment.path, comment.line
                            ));
                        }
                        Err(error @ Error::RateLimit { .. }) => return Err(error),
                        Err(error) => (deps.log)(&format!(
                            "{name}: an inline comment on {}:{} could not be posted: {error}",
                            comment.path, comment.line
                        )),
                    }
                }
                let threads = github.review_threads(r)?;
                for (_, entry) in &entries {
                    if let Some(thread) = posted[*entry].comment_id.and_then(|id| threads.get(&id)) {
                        posted[*entry].thread_id = Some(thread.id.clone());
                    }
                }
                Ok(())
            })();
            if let Err(error) = read_back {
                (deps.log)(&format!("{name}: the posted review could not be read back: {error}"));
            }
        }
        // What the re-review settled about earlier findings: fixed ones were accepted; a thread a human resolved
        // without a fix, or a thumbs-down, rejected one. Read before our own resolutions below.
        if deps.config.learn && !earlier.is_empty() {
            let learned = (|| -> Result<(), Error> {
                let threads = github.review_threads(r)?;
                let fates = fates_of(&FateInput {
                    pull: pull_key(r),
                    now: iso_from_ms(now()),
                    findings: earlier.iter().map(PostedFinding::ledger).collect(),
                    statuses: statuses.iter().map(|s| (s.id.clone(), s.status.clone())).collect(),
                    threads: thread_states(threads),
                    resolved_by_us: saved
                        .findings
                        .iter()
                        .filter(|f| f.status == "fixed")
                        .filter_map(|f| f.thread_id.clone().filter(|id| !id.is_empty()))
                        .collect::<HashSet<String>>(),
                    merged: false,
                });
                learn(deps, r, &fates, &name)
            })();
            if let Err(error) = learned {
                (deps.log)(&format!("{name}: the fates of earlier findings were not read: {error}"));
            }
        }
        // Resolve our own threads whose finding is fixed, by the thread id stored when the comment was posted.
        for status in statuses.iter().filter(|status| status.status == "fixed") {
            let Some(finding) = saved.findings.iter().find(|item| item.id == status.id) else {
                continue;
            };
            let Some(thread_id) = &finding.thread_id else { continue };
            match github.resolve_thread(thread_id) {
                Ok(()) => resolved.push(finding.id.clone()),
                Err(error) => (deps.log)(&format!("{name}: thread of {} was not resolved: {error}", finding.id)),
            }
        }
    }

    let ack_to_post_ms = prepared.ack_at.map(|at| (now() - at).max(0));
    let verdict = verdict_name(plan.verdict).to_string();
    let stages = match raw.get("timing").and_then(|timing| timing.get("stages")) {
        Some(Value::Object(stages)) => stages.clone().into_iter().collect(),
        _ => indexmap::IndexMap::new(),
    };
    deps.store.update_pull(&key, |pull_state, all| {
        pull_state.last_reviewed_sha = Some(head.clone());
        pull_state.last_reviewed_at = Some(reviewed_at.clone());
        pull_state.last_verdict = Some(verdict.clone());
        pull_state.closed = None;
        pull_state.last_review_dry_run = dry_run.then_some(true);
        if let Some(id) = review_id {
            pull_state.last_review_id = Some(id);
        }
        pull_state.attempts.shift_remove(&head);
        for status in &statuses {
            let Some(finding) = pull_state.findings.iter_mut().find(|item| item.id == status.id) else {
                continue;
            };
            if status.status == "fixed" || status.status == "not_applicable" {
                finding.status = status.status.clone();
            } else {
                finding.line = status.line;
            }
        }
        pull_state.findings.extend(posted.iter().cloned());
        if pull_state.findings.len() > 200 {
            let excess = pull_state.findings.len() - 200;
            pull_state.findings.drain(..excess);
        }
        let mut stages = stages.clone();
        stages.insert("postMs".into(), json!(Num(((now() - reviewed_at_ms).max(0)) as f64)));
        all.recent.push(ReviewRecord {
            account: account_key(account),
            pull: pull_key(r),
            sha: head.clone(),
            at: reviewed_at.clone(),
            outcome: if dry_run { "dry-run" } else { "posted" }.into(),
            verdict: Some(verdict.clone()),
            findings: Some(Num(result.findings.len() as f64)),
            total_ms: Some(Num(result.timing.total_ms)),
            stages: Some(stages),
            pickup_to_post_ms: Some(Num((now() - candidate.picked_at) as f64)),
            tag_to_ack_ms: prepared.tag_to_ack_ms.map(|ms| Num(ms as f64)),
            ack_to_post_ms: ack_to_post_ms.map(|ms| Num(ms as f64)),
            cost_usd: Some(Num(result.usage.cost_usd)),
            dry_run: dry_run.then_some(true),
            extra: Default::default(),
        });
    })?;
    (deps.log)(&format!(
        "{name}: {} {} for {short_sha}: {} findings, {} inline, {} threads resolved, {} s from pickup{}",
        if dry_run { "dry-run" } else { "posted" },
        plan.event,
        result.findings.len(),
        plan.comments.len(),
        resolved.len(),
        crate::js::number(crate::js::round((now() - candidate.picked_at) as f64 / 1000.0)),
        ack_to_post_ms.map_or(String::new(), |ms| format!(
            ", {} after the acknowledgement",
            seconds(ms)
        ))
    ));
    let reviewed = Box::new(Reviewed {
        verdict: verdict_name(plan.verdict),
        sha: head,
        result: raw,
        plan,
        review_id,
        path,
    });
    Ok(if dry_run {
        Outcome::DryRun(reviewed)
    } else {
        Outcome::Posted(reviewed)
    })
}
