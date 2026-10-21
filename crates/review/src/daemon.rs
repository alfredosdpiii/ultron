//! The autoreview loop (`autoreview/daemon.ts`): per account, discover pull requests that ask for a review
//! (notifications, plus a search fallback on the first cycle and every fifth so nothing is missed). Each one found is
//! decided and acknowledged at once, outside the review limit, so the author hears back within seconds; the reviews
//! themselves then run up to `concurrency` at a time. A pull request is never handled twice at the same time by one
//! account, rate limits pause the account they hit, and a failed review is retried on a later cycle (three attempts
//! per commit, counted by the reviewer).
//!
//! Host code: reviews go through [`crate::engine::ReviewEngine`] via the reviewer. Decisions and reviews run on
//! threads; the loop waits on a condition variable that every finished task, an ended sleep and [`Stopper::stop`]
//! signal (the TypeScript `Promise.race` over the tasks and the sleep). The clock is [`ReviewerDeps::now`] and the
//! sleep [`DaemonDeps::sleep`], both injectable as in the TypeScript.
//!
//! Known differences from the TypeScript:
//! - `run` returns once no decision or review is left, including reviews that a decision finishing during shutdown
//!   started (the TypeScript awaited only the tasks running when the signal came).
//! - An injected sleep runs on a thread of its own; its return wakes the loop like the TypeScript's resolved promise.
//! - `Date.parse` is [`crate::ledger::parse_iso_ms`] (ISO 8601 forms only): a notification time in another format
//!   (RFC 2822) is no notification time here.
//! - A task that panics still leaves the decision or review set, so the loop never waits on it forever.
//! - Log lines are masked by the closure given to [`create_logger`] (the cli passes the host's secret masking).

use std::collections::{HashMap, HashSet};
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, UNIX_EPOCH};

use indexmap::{IndexMap, IndexSet};
use regex::Regex;

use crate::accounts::{Account, account_key};
use crate::github::{Clock, Error, GitHub, PullRef, iso_from_ms, pull_key};
use crate::js::{Num, head16};
use crate::ledger::parse_iso_ms;
use crate::reviewer::{
    Candidate, Log, Outcome, Preparation, Prepared, ReviewOptions, ReviewerDeps, prepare_pull, run_review,
};
use crate::settings::RETENTION_DAYS;
use crate::state::{AccountState, AutoreviewState, DaemonInfo, StateStore};

/// The search fallback runs on the first cycle and then every this many cycles.
pub const SEARCH_EVERY: u64 = 5;
pub const FIRST_POLL_WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
pub const SEARCH_WINDOW_MS: i64 = 2 * 24 * 60 * 60 * 1000;
pub const SINCE_OVERLAP_MS: i64 = 2 * 60 * 1000;
pub const RETRY_DELAY_MS: i64 = 60_000;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// A pull request this large (added plus removed lines, as GitHub counts them) is a large review. Large reviews
/// leave one slot to the rest: on 2026-10-16 three large pull requests (fisher-ki-saas#2945, #2951, #2926) held all
/// three slots for hours while a 40-second review waited 71 minutes.
pub const LARGE_PULL_LINES: u64 = 10_000;

/// Whether a pull request is a large review.
pub fn is_large(pull: &crate::github::PullRequest) -> bool {
    pull.additions + pull.deletions >= LARGE_PULL_LINES
}

/// How many large reviews may run at once: all but one slot, and the only slot when there is one.
pub fn large_slots(concurrency: usize) -> usize {
    concurrency.saturating_sub(1).max(1)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Discovered {
    pub pull: PullRef,
    pub reasons: Vec<String>,
    /// The notification's update time (epoch ms), when a notification led here.
    pub notified_at: Option<i64>,
}

/// Remove files in `dirs` last modified more than `days` ago. Never fails: a directory that cannot be read (or a
/// file that cannot be read in it) ends that directory's pruning.
pub fn prune_old(dirs: &[&str], now: i64, days: u32) -> usize {
    let mut removed = 0;
    let limit = f64::from(days) * DAY_MS as f64;
    for dir in dirs {
        let mut prune = || -> std::io::Result<()> {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let path = entry.path();
                let modified = std::fs::metadata(&path)?.modified()?;
                let mtime_ms = match modified.duration_since(UNIX_EPOCH) {
                    Ok(after) => after.as_secs_f64() * 1000.0,
                    Err(before) => -before.duration().as_secs_f64() * 1000.0,
                };
                if now as f64 - mtime_ms <= limit {
                    continue;
                }
                let _ = std::fs::remove_file(&path);
                removed += 1;
            }
            Ok(())
        };
        let _ = prune();
    }
    removed
}

/// Open pull requests on which this account's last review requested changes: it is blocking them, so they are
/// looked at again on the search cycles even when no notification arrives for a push.
pub fn blocked_pulls(state: &AutoreviewState, account: &Account) -> Vec<PullRef> {
    let prefix = format!("{}@{}/", account.login, account.host);
    let pattern = Regex::new(r"^([^/]+)/([^#]+)#([0-9]+)$").expect("static pattern");
    let mut out = Vec::new();
    for (key, pull) in &state.pulls {
        let Some(rest) = key.strip_prefix(&prefix) else {
            continue;
        };
        if pull.last_verdict.as_deref() != Some("request_changes") || pull.closed == Some(true) {
            continue;
        }
        if pull.last_review_dry_run == Some(true) {
            continue;
        }
        if let Some(found) = pattern.captures(rest)
            && let Ok(number) = found[3].parse()
        {
            out.push(PullRef::new(&account.host, &found[1], &found[2], number));
        }
    }
    out
}

/// Pull requests that may need a review by this account, and the account's next notification cursor.
pub fn discover(
    github: &GitHub,
    state: &AccountState,
    now: i64,
    search: bool,
    blocked: &[PullRef],
) -> Result<(Vec<Discovered>, AccountState), Error> {
    let mut found: IndexMap<String, Discovered> = IndexMap::new();
    let mut add = |pull: PullRef, reason: &str, updated_at: Option<&str>| {
        let entry = found.entry(pull_key(&pull)).or_insert_with(|| Discovered {
            pull,
            reasons: Vec::new(),
            notified_at: None,
        });
        if !entry.reasons.iter().any(|known| known == reason) {
            entry.reasons.push(reason.into());
        }
        if let Some(at) = updated_at.and_then(parse_iso_ms)
            && entry.notified_at.is_none_or(|known| at > known)
        {
            entry.notified_at = Some(at);
        }
    };
    let last = state.last_poll_at.as_deref().and_then(parse_iso_ms);
    let since = iso_from_ms(last.map_or(now - FIRST_POLL_WINDOW_MS, |last| last - SINCE_OVERLAP_MS));
    let page = github.notifications(&since, state.last_modified.as_deref(), None)?;
    for notified in page.pulls {
        add(notified.pull, &notified.reason, Some(&notified.updated_at));
    }
    if search {
        let login = github.account.login.clone();
        let day = iso_from_ms(now - SEARCH_WINDOW_MS)[..10].to_string();
        for found in github.search(&format!("is:open is:pr review-requested:{login} archived:false"))? {
            add(found, "review_requested", None);
        }
        for found in github.search(&format!("is:pr mentions:{login} updated:>={day} archived:false"))? {
            add(found, "search_mention", None);
        }
        for pull in blocked {
            add(pull.clone(), "requested_changes", None);
        }
    }
    let mut next = state.clone();
    next.last_poll_at = Some(iso_from_ms(now));
    if let Some(last_modified) = page.last_modified {
        next.last_modified = Some(last_modified);
    }
    if let Some(interval) = page.poll_interval {
        next.poll_interval = Some(interval);
    }
    next.last_error = None;
    Ok((found.into_values().collect(), next))
}

pub type LogWrite = Arc<dyn Fn(&str) + Send + Sync>;
pub type Mask = Arc<dyn Fn(&str) -> String + Send + Sync>;

/// Writes to stderr (the logger's default output).
pub fn stderr_writer() -> LogWrite {
    Arc::new(|text: &str| {
        let _ = std::io::stderr().write_all(text.as_bytes());
    })
}

/// Lines go to `write` and to `<logs>/autoreview-<date>.log` (0600, in a 0700 directory), stamped with the ISO time
/// and with known secrets masked by `mask`. Logging never fails.
pub fn create_logger(logs_dir: Option<String>, now: Clock, write: LogWrite, mask: Mask) -> Log {
    Arc::new(move |line: &str| {
        let stamp = iso_from_ms(now());
        let text = format!("{stamp} {}\n", mask(line));
        write(&text);
        let Some(dir) = &logs_dir else { return };
        let append = || -> std::io::Result<()> {
            DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
            let path = Path::new(dir).join(format!("autoreview-{}.log", &stamp[..10]));
            let mut file = OpenOptions::new().append(true).create(true).mode(0o600).open(path)?;
            file.write_all(text.as_bytes())
        };
        let _ = append();
    })
}

/// Record the running daemon in the state (`ultron autoreview status` shows it).
pub fn record_daemon_start(
    store: &StateStore,
    pid: u32,
    started_at: i64,
    engine_start_ms: Option<f64>,
) -> Result<(), Error> {
    store.update(|state| {
        state.daemon = Some(DaemonInfo {
            pid: Num(f64::from(pid)),
            started_at: iso_from_ms(started_at),
            engine_start_ms: engine_start_ms.map(Num),
            extra: IndexMap::new(),
        });
    })
}

/// Remove the daemon and its queue from the state when it stops.
pub fn record_daemon_stop(store: &StateStore) -> Result<(), Error> {
    store.update(|state| {
        state.daemon = None;
        state.queue = None;
    })
}

/// What the daemon reports for a finished pull request: the reviewer's outcome, or a failure that is retried.
#[derive(Debug, Clone, PartialEq)]
pub enum Reported {
    Outcome(Outcome),
    Failed(String),
}

impl Reported {
    pub fn kind(&self) -> &'static str {
        match self {
            Reported::Outcome(outcome) => outcome.kind(),
            Reported::Failed(_) => "failed",
        }
    }
}

/// Sleeps `ms` milliseconds (or less); its return wakes the loop.
pub type Sleep = Arc<dyn Fn(u64) + Send + Sync>;
/// Told every finished pull request (tests, `once --json`).
pub type OnOutcome = Arc<dyn Fn(&Candidate, &Reported) + Send + Sync>;

#[derive(Clone)]
pub struct DaemonDeps {
    pub reviewer: ReviewerDeps,
    pub accounts: Vec<Account>,
    /// Replaces the timed wait between polls (tests advance a fake clock in it).
    pub sleep: Option<Sleep>,
    pub on_outcome: Option<OnOutcome>,
}

struct Waiting {
    candidate: Candidate,
    not_before: f64,
}

struct Inner {
    /// Found by discovery, not yet decided.
    candidates: IndexMap<String, Waiting>,
    /// Being decided and acknowledged; not limited by the review concurrency.
    preparing: IndexSet<String>,
    /// Due and acknowledged, waiting for a review slot.
    queue: IndexMap<String, Prepared>,
    running: IndexSet<String>,
    /// The running reviews that are large ([`is_large`]).
    large: HashSet<String>,
    /// Resume time (epoch ms) by account key.
    paused: HashMap<String, f64>,
    cycles: u64,
    poll_seconds: f64,
    pruned_at: Option<i64>,
    /// Bumped by every finished task, ended sleep and stop: what the loop waits on.
    events: u64,
    stopped: bool,
}

impl Inner {
    fn paused(&self, account: &Account, now: f64) -> bool {
        self.paused.get(&account_key(account)).copied().unwrap_or(0.0) > now
    }

    fn enqueue(&mut self, candidate: Candidate, not_before: f64) {
        let key = task_key(&candidate.account, &candidate.pull);
        if let Some(waiting) = self.candidates.get_mut(&key) {
            // Keep the earliest pickup; merge the reasons.
            for reason in candidate.reasons {
                if !waiting.candidate.reasons.contains(&reason) {
                    waiting.candidate.reasons.push(reason);
                }
            }
            waiting.not_before = waiting.not_before.min(not_before);
            return;
        }
        self.candidates.insert(key, Waiting { candidate, not_before });
    }
}

struct Shared {
    deps: DaemonDeps,
    inner: Mutex<Inner>,
    changed: Condvar,
}

fn task_key(account: &Account, pull: &PullRef) -> String {
    format!("{} {}", account_key(account), pull_key(pull))
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> i64 {
        (self.deps.reviewer.now)()
    }

    fn log(&self, line: &str) {
        (self.deps.reviewer.log)(line);
    }

    fn report(&self, candidate: &Candidate, reported: Reported) {
        if let Some(on_outcome) = &self.deps.on_outcome {
            on_outcome(candidate, &reported);
        }
    }

    fn wake(&self, inner: &mut Inner) {
        inner.events += 1;
        self.changed.notify_all();
    }

    fn enqueue(&self, candidate: Candidate, not_before: f64) {
        self.lock().enqueue(candidate, not_before);
    }

    /// An account's failure: a rate limit pauses it until GitHub's time. Noted in the state as `lastError`.
    fn failed(&self, account: &Account, error: &Error) {
        let key = account_key(account);
        let message = error.message().to_string();
        if let Error::RateLimit { resume_at, .. } = error {
            self.lock().paused.insert(key.clone(), *resume_at);
            self.log(&format!(
                "{key}: {message}; paused until {}",
                iso_from_ms(*resume_at as i64)
            ));
        } else {
            self.log(&format!("{key}: {message}"));
        }
        let _ = self.deps.reviewer.store.update(|all| {
            let state = all.accounts.entry(key.clone()).or_default();
            state.last_error = Some(head16(&message, 300).to_string());
            if let Error::RateLimit { resume_at, .. } = error {
                state.paused_until = Some(iso_from_ms(*resume_at as i64));
            }
        });
    }

    /// A failure of one pull request: wait out a rate limit, otherwise try again on a later cycle.
    fn retry(&self, candidate: Candidate, error: Error, what: &str) {
        let message = error.message().to_string();
        if let Error::RateLimit { resume_at, .. } = &error {
            self.failed(&candidate.account, &error);
            self.enqueue(candidate.clone(), *resume_at);
        } else {
            self.log(&format!(
                "{} as {}: {what} failed: {message}",
                pull_key(&candidate.pull),
                candidate.account.login
            ));
            // prepare_pull gives up after three failed reviews of one commit.
            self.enqueue(candidate.clone(), (self.now() + RETRY_DELAY_MS) as f64);
        }
        self.report(&candidate, Reported::Failed(message));
    }

    /// Start queued reviews, up to the concurrency limit; large ones leave a slot to the rest ([`large_slots`]).
    fn pump(self: &Arc<Self>, inner: &mut Inner) {
        let now = self.now() as f64;
        let concurrency = self.deps.reviewer.config.concurrency.0.max(1.0) as usize;
        let keys: Vec<String> = inner.queue.keys().cloned().collect();
        for key in keys {
            if inner.running.len() >= concurrency {
                break;
            }
            if inner.running.contains(&key) {
                continue;
            }
            let Some(prepared) = inner.queue.get(&key) else {
                continue;
            };
            if inner.paused(&prepared.candidate.account, now) {
                continue;
            }
            let large = is_large(&prepared.pull);
            if large && inner.large.len() >= large_slots(concurrency) {
                continue;
            }
            let Some(prepared) = inner.queue.shift_remove(&key) else {
                continue;
            };
            inner.running.insert(key.clone());
            if large {
                inner.large.insert(key.clone());
            }
            let shared = self.clone();
            std::thread::spawn(move || {
                let _done = Done {
                    shared: shared.clone(),
                    key,
                    review: true,
                };
                shared.review(prepared);
            });
        }
    }

    fn review(&self, prepared: Prepared) {
        let candidate = prepared.candidate.clone();
        match run_review(&self.deps.reviewer, &prepared) {
            Ok(outcome) => {
                if matches!(outcome, Outcome::Requeue(_)) {
                    let mut again = candidate.clone();
                    again.picked_at = self.now();
                    self.enqueue(again, 0.0);
                }
                self.report(&candidate, Reported::Outcome(outcome));
            }
            Err(error) => self.retry(candidate, error, "the review"),
        }
    }

    fn decide(&self, key: &str, candidate: Candidate) {
        match prepare_pull(&self.deps.reviewer, &candidate, ReviewOptions::default()) {
            Ok(Preparation::Done(outcome)) => {
                if let Outcome::Skipped(reason) = &outcome {
                    self.log(&format!(
                        "{} as {}: skipped ({reason})",
                        pull_key(&candidate.pull),
                        candidate.account.login
                    ));
                }
                self.report(&candidate, Reported::Outcome(outcome));
            }
            Ok(Preparation::Due(prepared)) => {
                self.lock().queue.insert(key.into(), *prepared);
            }
            Err(error) => self.retry(candidate, error, "the decision"),
        }
    }
}

/// Ends a decision or a review (also when it panicked): out of its set, the queue pumped after a decision, the loop
/// woken.
struct Done {
    shared: Arc<Shared>,
    key: String,
    review: bool,
}

impl Drop for Done {
    fn drop(&mut self) {
        let mut inner = self.shared.lock();
        if self.review {
            inner.running.shift_remove(&self.key);
            inner.large.remove(&self.key);
        } else {
            inner.preparing.shift_remove(&self.key);
            self.shared.pump(&mut inner);
        }
        self.shared.wake(&mut inner);
    }
}

/// Stops [`Daemon::run`]: no new poll starts; running decisions and reviews finish first.
#[derive(Clone)]
pub struct Stopper {
    shared: Arc<Shared>,
}

impl Stopper {
    pub fn stop(&self) {
        let mut inner = self.shared.lock();
        inner.stopped = true;
        self.shared.wake(&mut inner);
    }

    pub fn stopped(&self) -> bool {
        self.shared.lock().stopped
    }
}

pub struct Daemon {
    shared: Arc<Shared>,
}

impl Daemon {
    pub fn new(deps: DaemonDeps) -> Self {
        let poll_seconds = deps.reviewer.config.poll_seconds.0;
        Self {
            shared: Arc::new(Shared {
                deps,
                inner: Mutex::new(Inner {
                    candidates: IndexMap::new(),
                    preparing: IndexSet::new(),
                    queue: IndexMap::new(),
                    running: IndexSet::new(),
                    large: HashSet::new(),
                    paused: HashMap::new(),
                    cycles: 0,
                    poll_seconds,
                    pruned_at: None,
                    events: 0,
                    stopped: false,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    pub fn stopper(&self) -> Stopper {
        Stopper {
            shared: self.shared.clone(),
        }
    }

    /// Found and not yet reviewing: undecided candidates and acknowledged pull requests waiting for a slot.
    pub fn queued(&self) -> usize {
        let inner = self.shared.lock();
        inner.candidates.len() + inner.queue.len()
    }

    /// Pull requests being decided and acknowledged.
    pub fn deciding(&self) -> usize {
        self.shared.lock().preparing.len()
    }

    pub fn running(&self) -> usize {
        self.shared.lock().running.len()
    }

    /// The seconds between polls: the configured ones, or GitHub's `X-Poll-Interval` when that is longer.
    pub fn poll_seconds(&self) -> f64 {
        self.shared.lock().poll_seconds
    }

    /// Remove dry-run files and logs past retention, at most once a day.
    pub fn prune(&self) {
        let now = self.shared.now();
        {
            let mut inner = self.shared.lock();
            if inner.pruned_at.is_some_and(|at| now - at < DAY_MS) {
                return;
            }
            inner.pruned_at = Some(now);
        }
        let paths = &self.shared.deps.reviewer.paths;
        let removed = prune_old(&[&paths.dry_run, &paths.logs], now, RETENTION_DAYS);
        if removed > 0 {
            self.shared.log(&format!(
                "removed {removed} dry-run and log files older than {RETENTION_DAYS} days"
            ));
        }
    }

    /// Poll every account once and note what was found.
    pub fn poll(&self) {
        let shared = &self.shared;
        let search = {
            let mut inner = shared.lock();
            let search = inner.cycles.is_multiple_of(SEARCH_EVERY);
            inner.cycles += 1;
            search
        };
        let deps = &shared.deps.reviewer;
        for account in &shared.deps.accounts {
            let key = account_key(account);
            let now = shared.now();
            if shared.lock().paused(account, now as f64) {
                continue;
            }
            let tokens = deps.tokens.clone();
            let owner = account.clone();
            let github = GitHub::new(
                deps.runner.clone(),
                account.clone(),
                Arc::new(move || tokens.env(&owner)),
                Some(deps.now.clone()),
            );
            let all = deps.store.read();
            let saved = all.accounts.get(&key).cloned().unwrap_or_default();
            let blocked = if search {
                blocked_pulls(&all, account)
            } else {
                Vec::new()
            };
            let polled = discover(&github, &saved, now, search, &blocked).and_then(|(pulls, mut state)| {
                state.paused_until = None;
                let interval = state.poll_interval;
                deps.store.update(|current| {
                    current.accounts.insert(key.clone(), state);
                })?;
                Ok((pulls, interval))
            });
            match polled {
                Ok((pulls, interval)) => {
                    let mut inner = shared.lock();
                    if let Some(interval) = interval {
                        inner.poll_seconds = deps.config.poll_seconds.0.max(interval.0);
                    }
                    for pull in pulls {
                        inner.enqueue(
                            Candidate {
                                account: account.clone(),
                                pull: pull.pull,
                                reasons: pull.reasons,
                                picked_at: now,
                                notified_at: pull.notified_at,
                            },
                            0.0,
                        );
                    }
                }
                Err(error) => shared.failed(account, &error),
            }
        }
    }

    /// Decide and acknowledge every candidate that is due, all at once: acknowledgements do not wait for a review
    /// slot. What is due for a review joins the queue.
    pub fn prepare(&self) {
        let now = self.shared.now() as f64;
        let mut inner = self.shared.lock();
        let keys: Vec<String> = inner.candidates.keys().cloned().collect();
        for key in keys {
            let Some(waiting) = inner.candidates.get(&key) else {
                continue;
            };
            if waiting.not_before > now
                || inner.preparing.contains(&key)
                || inner.queue.contains_key(&key)
                || inner.running.contains(&key)
            {
                continue;
            }
            if inner.paused(&waiting.candidate.account, now) {
                continue;
            }
            let Some(waiting) = inner.candidates.shift_remove(&key) else {
                continue;
            };
            inner.preparing.insert(key.clone());
            let shared = self.shared.clone();
            std::thread::spawn(move || {
                let _done = Done {
                    shared: shared.clone(),
                    key: key.clone(),
                    review: false,
                };
                shared.decide(&key, waiting.candidate);
            });
        }
    }

    /// Start queued reviews, up to the concurrency limit.
    pub fn pump(&self) {
        let mut inner = self.shared.lock();
        self.shared.pump(&mut inner);
    }

    /// Write the queue to the state, for `ultron autoreview status`: the reviews running and waiting, and what waits
    /// for a retry. Not what a poll is deciding: that changes with every decision, and each change rewrote the
    /// whole state file (about 70 times a poll on 2026-10-21).
    fn record(&self) {
        let now = self.shared.now() as f64;
        let queue: Vec<String> = {
            let inner = self.shared.lock();
            inner
                .running
                .iter()
                .map(|key| format!("{key} (reviewing)"))
                .chain(inner.queue.keys().map(|key| format!("{key} (acknowledged, waiting)")))
                .chain(
                    inner
                        .candidates
                        .iter()
                        .filter(|(_, waiting)| waiting.not_before > now)
                        .map(|(key, _)| key.clone()),
                )
                .collect()
        };
        let _ = self.shared.deps.reviewer.store.update(|all| all.queue = Some(queue));
    }

    /// Whether a queued review could start now, or a candidate is due (its account is not paused).
    fn startable(&self, inner: &Inner) -> bool {
        let now = self.shared.now() as f64;
        inner
            .candidates
            .values()
            .any(|waiting| waiting.not_before <= now && !inner.paused(&waiting.candidate.account, now))
            || inner
                .queue
                .values()
                .any(|prepared| !inner.paused(&prepared.candidate.account, now))
    }

    /// One cycle: poll, acknowledge, then review everything that is due and wait for it.
    pub fn once(&self) {
        self.prune();
        self.poll();
        loop {
            self.prepare();
            self.pump();
            self.record();
            let mut inner = self.shared.lock();
            if inner.preparing.is_empty() && inner.running.is_empty() {
                if inner.queue.is_empty() || !self.startable(&inner) {
                    break;
                }
                continue;
            }
            let seen = inner.events;
            while inner.events == seen {
                inner = self.shared.changed.wait(inner).unwrap_or_else(|e| e.into_inner());
            }
        }
        self.record();
    }

    /// Wait up to `ms` (the injected sleep, or a timed wait) or until a decision or review finishes or the daemon is
    /// stopped.
    fn wait(&self, ms: u64) {
        let mut inner = self.shared.lock();
        if inner.stopped {
            return;
        }
        let seen = inner.events;
        if let Some(sleep) = self.shared.deps.sleep.clone() {
            let shared = self.shared.clone();
            std::thread::spawn(move || {
                sleep(ms);
                let mut inner = shared.lock();
                shared.wake(&mut inner);
            });
            while inner.events == seen && !inner.stopped {
                inner = self.shared.changed.wait(inner).unwrap_or_else(|e| e.into_inner());
            }
            return;
        }
        let until = Instant::now() + Duration::from_millis(ms);
        while inner.events == seen && !inner.stopped {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            inner = self
                .shared
                .changed
                .wait_timeout(inner, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Poll and review until [`Stopper::stop`]; running decisions and reviews finish first.
    pub fn run(&self) {
        let stopped = || self.shared.lock().stopped;
        while !stopped() {
            self.prune();
            self.poll();
            self.prepare();
            self.pump();
            self.record();
            // Reviews that finish free a slot: fill it without waiting for the next poll.
            let deadline = self.shared.now() as f64 + self.poll_seconds() * 1000.0;
            while !stopped() && (self.shared.now() as f64) < deadline {
                let left = deadline - self.shared.now() as f64;
                self.wait(left.clamp(50.0, 5_000.0) as u64);
                self.prepare();
                self.pump();
            }
        }
        let mut inner = self.shared.lock();
        while !(inner.preparing.is_empty() && inner.running.is_empty()) {
            inner = self.shared.changed.wait(inner).unwrap_or_else(|e| e.into_inner());
        }
        drop(inner);
        self.record();
    }
}
