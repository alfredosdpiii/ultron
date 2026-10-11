//! Git branch, working-tree counts and pull request for the status line, gathered off the paint path (port of omp
//! packages/tui/src/status-line/git-utils.ts plus the component's branch/status/PR refresh).
//!
//! [`GitWatcher`] owns a thread: `refresh` only signals it; the thread reads `HEAD`, runs `git status`, and asks the
//! [`PrLookup`] (network, `gh` by default) only when the branch or repository changed. The status line reads the
//! latest [`GitSnapshot`] without blocking.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::status_line::GitCounts;

/// `owner/repo` from a GitHub remote URL (HTTPS, scp-style SSH, ssh://, git://); `None` for other hosts.
pub fn parse_github_repo(remote_url: &str) -> Option<String> {
    let idx = remote_url.find("github.com")?;
    let rest = &remote_url[idx + "github.com".len()..];
    let rest = rest.strip_prefix(':').or_else(|| rest.strip_prefix('/'))?;
    let mut parts = rest.splitn(3, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    Some(format!("{owner}/{}", repo.strip_suffix(".git").unwrap_or(repo)))
}

/// Branch name from a remote HEAD ref (`origin/main` -> `main`).
pub fn parse_default_branch(r: &str) -> String {
    r.split_once('/').map_or(r, |(_, b)| b).to_string()
}

/// Where a cached PR answer applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrCacheContext {
    pub branch: String,
    pub repo_id: Option<String>,
}

/// A cached PR (or cached absence, `Some(None)`) is reusable while branch and repository are unchanged.
pub fn can_reuse_cached_pr(
    cached: Option<&Option<(u64, String)>>,
    cached_context: Option<&PrCacheContext>,
    current: Option<&PrCacheContext>,
) -> bool {
    cached.is_some() && current.is_some() && cached_context.is_some_and(|c| Some(c) == current)
}

/// The repository's git directory: `.git` directly, or the `gitdir:` a worktree's `.git` file points to.
pub fn find_git_dir(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        let dot = dir.join(".git");
        if dot.is_dir() {
            return Some(dot);
        }
        if dot.is_file() {
            let text = std::fs::read_to_string(&dot).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            let p = Path::new(target);
            return Some(if p.is_absolute() { p.to_path_buf() } else { dir.join(p) });
        }
    }
    None
}

/// Branch from `HEAD` contents: `ref: refs/heads/x` -> `x`; a detached commit -> its first 7 characters.
pub fn parse_head(head: &str) -> Option<String> {
    let head = head.trim();
    if let Some(r) = head.strip_prefix("ref:") {
        let r = r.trim();
        return Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_string());
    }
    (head.len() >= 7 && head.chars().all(|c| c.is_ascii_hexdigit())).then(|| head[..7].to_string())
}

/// Counts from `git status --porcelain=v1` output.
pub fn parse_porcelain(out: &str) -> GitCounts {
    let mut c = GitCounts::default();
    for line in out.lines() {
        let b = line.as_bytes();
        if b.len() < 2 {
            continue;
        }
        if &b[..2] == b"??" {
            c.untracked += 1;
            continue;
        }
        if b[0] != b' ' && b[0] != b'!' {
            c.staged += 1;
        }
        if b[1] != b' ' && b[1] != b'!' {
            c.unstaged += 1;
        }
    }
    c
}

/// Pull request lookup for a branch (network; runs on the watcher thread only).
pub trait PrLookup: Send {
    fn lookup(&mut self, cwd: &Path, branch: &str) -> Option<(u64, String)>;
}

/// `gh pr view --json number,url` for the current branch.
pub struct GhPrLookup;

impl PrLookup for GhPrLookup {
    fn lookup(&mut self, cwd: &Path, branch: &str) -> Option<(u64, String)> {
        let out = Command::new("gh")
            .args(["pr", "view", branch, "--json", "number,url"])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        Some((v["number"].as_u64()?, v["url"].as_str()?.to_string()))
    }
}

/// What the status line shows for git.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitSnapshot {
    pub branch: Option<String>,
    pub status: Option<GitCounts>,
    pub pr: Option<(u64, String)>,
}

/// One synchronous refresh step, shared by the watcher thread and tests.
pub struct GitProbe<L: PrLookup> {
    pub cwd: PathBuf,
    pub lookup: L,
    pr_cache: Option<(PrCacheContext, Option<(u64, String)>)>,
    /// PR lookups performed (each is a network call).
    pub lookups: usize,
}

impl<L: PrLookup> GitProbe<L> {
    pub fn new(cwd: PathBuf, lookup: L) -> Self {
        Self {
            cwd,
            lookup,
            pr_cache: None,
            lookups: 0,
        }
    }

    pub fn probe(&mut self) -> GitSnapshot {
        let Some(git_dir) = find_git_dir(&self.cwd) else {
            return GitSnapshot::default();
        };
        let branch = std::fs::read_to_string(git_dir.join("HEAD"))
            .ok()
            .and_then(|h| parse_head(&h));
        let status = Command::new("git")
            .args(["status", "--porcelain=v1", "--untracked-files=normal"])
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| parse_porcelain(&String::from_utf8_lossy(&o.stdout)));
        let pr = branch.as_ref().and_then(|b| {
            let ctx = PrCacheContext {
                branch: b.clone(),
                repo_id: Some(git_dir.join("HEAD").to_string_lossy().into_owned()),
            };
            let reuse = can_reuse_cached_pr(
                self.pr_cache.as_ref().map(|(_, pr)| pr),
                self.pr_cache.as_ref().map(|(c, _)| c),
                Some(&ctx),
            );
            if !reuse {
                self.lookups += 1;
                let pr = self.lookup.lookup(&self.cwd, b);
                self.pr_cache = Some((ctx, pr));
            }
            self.pr_cache.as_ref().and_then(|(_, pr)| pr.clone())
        });
        GitSnapshot { branch, status, pr }
    }
}

/// Background git refresher.
pub struct GitWatcher {
    snapshot: Arc<Mutex<GitSnapshot>>,
    tx: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl GitWatcher {
    /// Start the thread; `on_update` runs after each refresh that changed the snapshot (request a render there).
    pub fn spawn<L: PrLookup + 'static>(cwd: PathBuf, lookup: L, on_update: impl Fn() + Send + 'static) -> Self {
        let snapshot = Arc::new(Mutex::new(GitSnapshot::default()));
        let shared = Arc::clone(&snapshot);
        let (tx, rx) = mpsc::channel::<()>();
        let worker = thread::Builder::new()
            .name("status-git".into())
            .spawn(move || {
                let mut probe = GitProbe::new(cwd, lookup);
                while rx.recv().is_ok() {
                    while rx.try_recv().is_ok() {} // coalesce bursts
                    let next = probe.probe();
                    let changed = {
                        let mut cur = shared.lock().unwrap_or_else(|e| e.into_inner());
                        let changed = *cur != next;
                        *cur = next;
                        changed
                    };
                    if changed {
                        on_update();
                    }
                }
            })
            .ok();
        let w = Self {
            snapshot,
            tx: Some(tx),
            worker,
        };
        w.refresh();
        w
    }

    /// Ask for a refresh (after a turn, a cell that ran git, or a cwd change). Never blocks.
    pub fn refresh(&self) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(());
        }
    }

    /// The latest snapshot (cheap clone; never waits on git).
    pub fn snapshot(&self) -> GitSnapshot {
        self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Stop after the in-flight refresh.
    pub fn shutdown(mut self) {
        self.tx.take();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl Drop for GitWatcher {
    fn drop(&mut self) {
        self.tx.take();
    }
}
