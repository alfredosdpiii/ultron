//! What Pi's footer reads besides the session (core/footer-data-provider.ts and experimental/client-tui-footer.ts):
//! the git branch of the working directory, and Claude Code's subscription usage from the transcript.
//!
//! The branch is read off the paint path: [`FooterBranch`] owns a thread that reads it at start (`HEAD`, with
//! `git symbolic-ref` for a reftable repository's `.invalid` placeholder), then watches `HEAD` and the reftable files
//! and reads it again after a change (TypeScript watches with `fs.watch` and a 500 ms debounce; here the files'
//! metadata is compared every 500 ms). The footer reads the latest value without waiting.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// `GitPaths`: the repository directory, its common git directory and its `HEAD` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitPaths {
    pub repo_dir: PathBuf,
    pub common_git_dir: PathBuf,
    pub head_path: PathBuf,
}

/// `path.resolve(base, p)` for POSIX paths: `p` when absolute, else joined, `.` and `..` folded.
fn resolve(base: &Path, p: &str) -> PathBuf {
    let joined = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        base.join(p)
    };
    let mut out = PathBuf::from("/");
    for part in joined.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::Normal(n) => out.push(n),
            _ => {}
        }
    }
    out
}

/// `findGitPaths`: walk up from `cwd` to the first `.git`: a directory (its `HEAD` must exist), or a worktree's
/// `gitdir: <dir>` file (its `HEAD` must exist; `commondir` names the shared git directory).
pub fn find_git_paths(cwd: &Path) -> Option<GitPaths> {
    let mut dir = cwd.to_path_buf();
    loop {
        let git = dir.join(".git");
        if git.exists() {
            let meta = std::fs::metadata(&git).ok()?;
            if meta.is_file() {
                let content = std::fs::read_to_string(&git).ok()?;
                let content = content.trim();
                if let Some(target) = content.strip_prefix("gitdir: ") {
                    let git_dir = resolve(&dir, target.trim());
                    let head_path = git_dir.join("HEAD");
                    if !head_path.exists() {
                        return None;
                    }
                    let common = git_dir.join("commondir");
                    let common_git_dir = if common.exists() {
                        resolve(&git_dir, std::fs::read_to_string(&common).ok()?.trim())
                    } else {
                        git_dir.clone()
                    };
                    return Some(GitPaths {
                        repo_dir: dir,
                        common_git_dir,
                        head_path,
                    });
                }
            } else if meta.is_dir() {
                let head_path = git.join("HEAD");
                if !head_path.exists() {
                    return None;
                }
                return Some(GitPaths {
                    repo_dir: dir,
                    common_git_dir: git,
                    head_path,
                });
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// `resolveBranchWithGitSync`: `git --no-optional-locks symbolic-ref --quiet --short HEAD` in the repository, with
/// `git` looked up on `path_var` only (never the process PATH); `None` when it fails or prints nothing.
pub fn branch_from_git(repo_dir: &Path, path_var: Option<&str>) -> Option<String> {
    let git = std::env::split_paths(path_var?)
        .map(|dir| dir.join("git"))
        .find(|p| p.is_file())?;
    let out = Command::new(git)
        .args(["--no-optional-locks", "symbolic-ref", "--quiet", "--short", "HEAD"])
        .current_dir(repo_dir)
        .env("PATH", path_var.unwrap_or_default())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|b| !b.is_empty())
}

/// `resolveGitBranchSync`: `None` outside a repository, the branch `HEAD` names, `detached` for a detached `HEAD`;
/// a reftable repository's `.invalid` asks `git` (`detached` when that fails).
pub fn read_branch(paths: Option<&GitPaths>, path_var: Option<&str>) -> Option<String> {
    let paths = paths?;
    let content = std::fs::read_to_string(&paths.head_path).ok()?;
    let content = content.trim();
    match content.strip_prefix("ref: refs/heads/") {
        Some(".invalid") => Some(branch_from_git(&paths.repo_dir, path_var).unwrap_or_else(|| "detached".into())),
        Some(branch) => Some(branch.to_string()),
        None => Some("detached".into()),
    }
}

/// The files whose change means the branch may have changed: `HEAD`'s directory entry for `HEAD`, and the
/// reftable directory and its `tables.list` when the repository uses reftable.
fn watched(paths: &GitPaths) -> Vec<PathBuf> {
    let mut files = vec![paths.head_path.clone()];
    let reftable = paths.common_git_dir.join("reftable");
    if reftable.exists() {
        files.push(reftable.join("tables.list"));
        files.push(reftable);
    }
    files
}

type Stamp = Vec<Option<(SystemTime, u64)>>;

fn stamp(files: &[PathBuf]) -> Stamp {
    files
        .iter()
        .map(|f| {
            std::fs::metadata(f)
                .ok()
                .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
        })
        .collect()
}

/// `WATCH_DEBOUNCE_MS`: how often the watched files are compared.
const WATCH_INTERVAL: Duration = Duration::from_millis(500);

/// The footer's git branch (`FooterDataProvider.getGitBranch`), kept current on its own thread.
pub struct FooterBranch {
    branch: Arc<Mutex<Option<String>>>,
    read: Arc<AtomicBool>,
    stop: Option<Sender<()>>,
}

impl FooterBranch {
    /// Read the branch of `cwd` and keep it current; `path_var` is the PATH `git` is looked up on. `on_change` runs
    /// on the watcher thread after each change (wake the paint loop there).
    pub fn spawn(cwd: PathBuf, path_var: Option<String>, on_change: impl Fn() + Send + 'static) -> FooterBranch {
        let branch = Arc::new(Mutex::new(None));
        let read = Arc::new(AtomicBool::new(false));
        let (shared, first) = (Arc::clone(&branch), Arc::clone(&read));
        let (stop, stopped) = mpsc::channel::<()>();
        let _ = std::thread::Builder::new().name("footer-branch".into()).spawn(move || {
            let paths = find_git_paths(&cwd);
            let set = |next: Option<String>| {
                let mut current = shared.lock().unwrap_or_else(|e| e.into_inner());
                let changed = *current != next;
                *current = next;
                changed
            };
            let changed = set(read_branch(paths.as_ref(), path_var.as_deref()));
            first.store(true, Ordering::SeqCst);
            if changed {
                on_change();
            }
            let Some(paths) = paths else { return };
            let files = watched(&paths);
            let mut last = stamp(&files);
            loop {
                match stopped.recv_timeout(WATCH_INTERVAL) {
                    Err(RecvTimeoutError::Timeout) => {}
                    _ => return,
                }
                let now = stamp(&files);
                if now == last {
                    continue;
                }
                last = now;
                if set(read_branch(Some(&paths), path_var.as_deref())) {
                    on_change();
                }
            }
        });
        FooterBranch {
            branch,
            read,
            stop: Some(stop),
        }
    }

    /// Whether the first read finished (its result, possibly no branch, is in [`FooterBranch::branch`]).
    pub fn has_read(&self) -> bool {
        self.read.load(Ordering::SeqCst)
    }

    /// The latest branch (`None` outside a repository or before the first read).
    pub fn branch(&self) -> Option<String> {
        self.branch.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Drop for FooterBranch {
    fn drop(&mut self) {
        self.stop.take();
    }
}

/// `CLAUDE_USAGE_STATUS`: the footer status key of the usage text.
pub const CLAUDE_USAGE_STATUS: &str = "claude-code-usage";

/// `WINDOW_LABELS`.
fn window_label(name: &str) -> String {
    match name {
        "five_hour" => "5h".into(),
        "seven_day" => "7d".into(),
        "seven_day_opus" => "7d opus".into(),
        _ => name.replace('_', " "),
    }
}

/// Providers whose responses carry Claude Code's usage: `claude-code` (TypeScript's root runner) and
/// `claude-subscription` (the Rust root under `--claude`).
fn reports_claude_usage(provider: &str) -> bool {
    provider == "claude-code" || provider == "claude-subscription"
}

/// `Math.round(x * 100)` printed as JavaScript prints it.
fn percent(utilization: f64) -> String {
    let rounded = (utilization * 100.0 + 0.5).floor();
    if rounded == 0.0 {
        "0".into()
    } else {
        format!("{rounded}")
    }
}

/// `claudeCodeUsageText`: the subscription usage the newest Claude Code response reported ("Claude Code 5h 9% ·
/// 7d 62%", "(limit reached)" after a rejected one); `None` when no response reported any.
pub fn claude_code_usage_text(messages: &[Value]) -> Option<String> {
    for message in messages.iter().rev() {
        if message["role"] != "assistant" || !message["provider"].as_str().is_some_and(reports_claude_usage) {
            continue;
        }
        let diagnostic = message["diagnostics"]
            .as_array()
            .and_then(|d| d.iter().find(|d| d["type"] == "claude_code_usage"));
        let Some(windows) = diagnostic.and_then(|d| d["details"]["windows"].as_object()) else {
            continue;
        };
        let parts: Vec<String> = windows
            .iter()
            .filter_map(|(name, value)| {
                let utilization = value.as_object()?.get("utilization")?.as_f64()?;
                Some(format!("{} {}%", window_label(name), percent(utilization)))
            })
            .collect();
        let limited = diagnostic.is_some_and(|d| d["details"]["status"] == "rejected");
        if parts.is_empty() {
            return None;
        }
        return Some(format!(
            "Claude Code {}{}",
            parts.join(" · "),
            if limited { " (limit reached)" } else { "" }
        ));
    }
    None
}

/// `isUsingSubscription` of the client footer: Claude Code's costs are notional (its own login pays).
pub fn using_subscription(provider: &str) -> bool {
    reports_claude_usage(provider)
}
