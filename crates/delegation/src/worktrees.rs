//! Port of `worktrees.ts`: Git worktrees for subagents. `rlm.spawn(brief, name=..., worktree=True)` gives a child a
//! private checkout of its parent's repository, and `rlm.merge(children)` brings each child's work back into the
//! parent's working tree.
//!
//! Creating ([`create_child_worktree`]): the parent's working tree is pictured as a commit from a copy of its index
//! (`git add -A`, `write-tree`, `commit-tree` on top of HEAD; HEAD itself when nothing is dirty), and the child's
//! branch `ultron/<session>/<name>` starts there, checked out in `<git common dir>/ultron-worktrees/<id>`. Gitignored
//! dependency directories are linked in and small env files copied ([`WorktreeSetup`]).
//!
//! Finishing ([`commit_child_worktree`]): what the child changed is committed on its branch (no hooks, no signing).
//!
//! Merging ([`merge_child_worktree`]): a three-way merge of trees (base = the child's snapshot, ours = a fresh
//! snapshot of the parent's tree, theirs = the child's commit) by `git merge-tree --write-tree`, written into the
//! parent's working tree through a temporary index after checking none of the written files changed meanwhile. A
//! conflict writes nothing unless conflict markers were asked for. Nothing is committed on the parent's branch.
//!
//! Crashes: each worktree has a record (`<id>.json`) naming its owner process; [`prune_worktrees`] removes those of
//! gone owners, committing uncommitted work first and keeping branches that hold work.

#[path = "checkpoints.rs"]
pub mod checkpoints;

use kernel::Signal;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

/// Directory under the Git common dir that holds Ultron's worktrees and their records.
pub const WORKTREES_DIR: &str = "ultron-worktrees";
/// Branches of subagent worktrees: `ultron/<session>/<name>`.
pub const BRANCH_PREFIX: &str = "ultron";
/// Keep worktrees and branches after merges, on session end and at startup pruning (debugging).
pub const KEEP_WORKTREES_ENV: &str = "ULTRON_KEEP_WORKTREES";
/// Gitignored directories linked into a worktree by default.
pub const DEFAULT_LINK_NAMES: [&str; 3] = ["node_modules", ".venv", "venv"];
const MAX_COPY_BYTES: u64 = 256 * 1024;
const MAX_SETUP_DEPTH: usize = 3;
const MAX_SETUP_ENTRIES: usize = 64;
const DEFAULT_SETUP_TIMEOUT_MS: u64 = 10 * 60 * 1000;
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_HUNKS_PER_FILE: usize = 5;
const MAX_CONFLICT_FILES: usize = 20;
const HUNK_TEXT_CHARS: usize = 400;
const ZERO_MODE: &str = "000000";

/// A worktree operation failed. `fallback`: a shared working directory is a fine substitute (`worktree="auto"`):
/// no Git, no repository, no commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeError {
    pub message: String,
    pub fallback: bool,
}

impl WorktreeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fallback: false,
        }
    }
    fn fallback(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            fallback: true,
        }
    }
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<std::io::Error> for WorktreeError {
    fn from(error: std::io::Error) -> Self {
        WorktreeError::new(error.to_string())
    }
}

type Result<T> = std::result::Result<T, WorktreeError>;

/// `ULTRON_KEEP_WORKTREES` set to anything but empty, 0, false, no or off.
pub fn keep_worktrees(value: Option<&str>) -> bool {
    value
        .map(|v| v.trim().to_lowercase())
        .is_some_and(|v| !v.is_empty() && !["0", "false", "no", "off"].contains(&v.as_str()))
}

/// How a worktree is prepared for its child after checkout.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorktreeSetup {
    /// Gitignored paths to link from the parent's checkout; default every gitignored node_modules/.venv/venv a few
    /// levels deep. `Some(vec![])` links nothing.
    pub link: Option<Vec<String>>,
    /// Gitignored files to copy; default small `.env` / `.env.*` files.
    pub copy: Option<Vec<String>>,
    /// A shell command run in the child's working directory before it starts.
    pub command: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub pid: u32,
    pub host: String,
}

/// A subagent worktree, as recorded next to it (`<common dir>/ultron-worktrees/<id>.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorktreeRecord {
    pub version: u32,
    pub id: String,
    pub session_id: String,
    pub name: String,
    pub owner: Owner,
    pub repo: String,
    pub common_dir: String,
    pub path: String,
    pub cwd: String,
    pub branch: String,
    pub head: String,
    pub base: String,
    pub dirty: bool,
    pub linked: Vec<String>,
    pub copied: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub command_timeout_ms: Option<u64>,
    pub created_at: f64,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub task_id: Option<String>,
    /// The branch's final commit once committed: `Some(None)` when the child changed nothing.
    #[serde(skip_serializing_if = "Option::is_none", default, deserialize_with = "present")]
    pub commit: Option<Option<String>>,
}

/// `commit` is three-state (absent, null, a commit), as in the TS record: a present null is `Some(None)`.
fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Option<Option<String>>, D::Error> {
    Ok(Some(Option::deserialize(deserializer)?))
}

// ---------------------------------------------------------------------------------------------------------------
// Git

#[derive(Debug, Clone, Default)]
pub struct GitResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Default)]
struct GitOptions<'a> {
    env: Vec<(&'a str, String)>,
    input: Option<Vec<u8>>,
}

fn opts<'a>(env: &[(&'a str, &str)]) -> GitOptions<'a> {
    GitOptions {
        env: env.iter().map(|(k, v)| (*k, v.to_string())).collect(),
        input: None,
    }
}

/// Runs `git` without a shell or terminal; stdout and stderr as text. Fails (fallback) when Git cannot start.
fn git_with(cwd: &Path, args: &[&str], options: GitOptions<'_>) -> Result<GitResult> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if options.input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &options.env {
        command.env(key, value);
    }
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            WorktreeError::fallback("worktree=True needs Git, and `git` is not installed or not on PATH")
        } else {
            WorktreeError::new(error.to_string())
        }
    })?;
    if let (Some(input), Some(mut stdin)) = (options.input, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let pid = child.id();
    let (done, finished) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if finished.recv_timeout(GIT_TIMEOUT).is_err()
            && let Some(pid) = rustix::process::Pid::from_raw(pid as i32)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
    });
    let mut stderr_pipe = child.stderr.take();
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    });
    let mut stdout = Vec::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_end(&mut stdout);
    }
    let status = child.wait()?;
    let _ = done.send(());
    let mut stderr = String::from_utf8_lossy(&stderr_reader.join().unwrap_or_default()).into_owned();
    stderr.truncate(stderr.floor_char_boundary(100_000.min(stderr.len())));
    Ok(GitResult {
        code: status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr,
    })
}

/// Public for tests and the host: `git` in `cwd`.
pub fn git(cwd: &Path, args: &[&str]) -> Result<GitResult> {
    git_with(cwd, args, GitOptions::default())
}

fn first_lines(text: &str, count: usize) -> String {
    text.split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(count)
        .collect::<Vec<_>>()
        .join(" | ")
}

/// `git` that must succeed; its stderr names what failed.
fn git_ok_with(cwd: &Path, args: &[&str], options: GitOptions<'_>) -> Result<String> {
    let result = git_with(cwd, args, options)?;
    if result.code != 0 {
        let shown: Vec<&str> = args
            .iter()
            .copied()
            .filter(|arg| !arg.starts_with("-c") && !arg.contains('='))
            .collect();
        let why = first_lines(
            if result.stderr.is_empty() {
                &result.stdout
            } else {
                &result.stderr
            },
            3,
        );
        let why = if why.is_empty() {
            format!("exit {}", result.code)
        } else {
            why
        };
        return Err(WorktreeError::new(format!("git {} failed: {why}", shown.join(" "))));
    }
    Ok(result.stdout)
}

fn git_ok(cwd: &Path, args: &[&str]) -> Result<String> {
    git_ok_with(cwd, args, GitOptions::default())
}

/// No user hooks, no signing prompts: Ultron's own bookkeeping commits and checkouts.
fn quiet<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut out = vec![
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "tag.gpgsign=false",
    ];
    out.extend_from_slice(args);
    out
}

type Identity = Vec<(&'static str, String)>;

static IDENTITIES: LazyLock<Mutex<HashMap<PathBuf, Identity>>> = LazyLock::new(Mutex::default);

/// Author/committer for Ultron's commits: the repository's configured identity, else "Ultron".
fn identity_env(cwd: &Path) -> Result<Identity> {
    if let Some(cached) = IDENTITIES.lock().unwrap_or_else(PoisonError::into_inner).get(cwd) {
        return Ok(cached.clone());
    }
    let name = git(cwd, &["config", "user.name"])?;
    let email = git(cwd, &["config", "user.email"])?;
    let mut env = Vec::new();
    if name.stdout.trim().is_empty() && std::env::var_os("GIT_AUTHOR_NAME").is_none() {
        env.push(("GIT_AUTHOR_NAME", "Ultron".to_string()));
        env.push(("GIT_COMMITTER_NAME", "Ultron".to_string()));
    }
    if email.stdout.trim().is_empty() && std::env::var_os("GIT_AUTHOR_EMAIL").is_none() {
        env.push(("GIT_AUTHOR_EMAIL", "ultron@localhost".to_string()));
        env.push(("GIT_COMMITTER_EMAIL", "ultron@localhost".to_string()));
    }
    IDENTITIES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(cwd.to_path_buf(), env.clone());
    Ok(env)
}

/// Lexical normalization of an absolute path (node's `path.resolve` without a cwd).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn text(path: &Path) -> String {
    path.display().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoInfo {
    /// The working tree's top directory.
    pub top: PathBuf,
    pub common_dir: PathBuf,
    /// HEAD's commit; `None` in a repository without commits.
    pub head: Option<String>,
}

/// The repository `cwd` is in. Fails (fallback) outside Git or a working tree.
pub fn repo_info(cwd: &Path) -> Result<RepoInfo> {
    let result = git(
        cwd,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
            "--is-bare-repository",
        ],
    )?;
    if result.code != 0 {
        if result.stderr.to_lowercase().contains("not a git repository") {
            return Err(WorktreeError::fallback(format!(
                "worktree=True needs a Git repository, and {} is not inside one",
                text(cwd)
            )));
        }
        return Err(WorktreeError::fallback(format!(
            "git could not read the repository at {}: {}",
            text(cwd),
            first_lines(&result.stderr, 3)
        )));
    }
    let lines: Vec<&str> = result.stdout.trim().split('\n').collect();
    let (top, common, bare) = (
        lines.first().copied().unwrap_or(""),
        lines.get(1).copied().unwrap_or(""),
        lines.get(2).copied().unwrap_or(""),
    );
    if bare == "true" || top.is_empty() {
        return Err(WorktreeError::fallback(format!(
            "{} is not inside a Git working tree",
            text(cwd)
        )));
    }
    let head = git(cwd, &["rev-parse", "--verify", "-q", "HEAD^{commit}"])?;
    Ok(RepoInfo {
        top: normalize(Path::new(top)),
        common_dir: normalize(Path::new(common)),
        head: (head.code == 0).then(|| head.stdout.trim().to_string()),
    })
}

fn random_hex(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    let _ = getrandom::fill(&mut buffer);
    buffer.iter().map(|b| format!("{b:02x}")).collect()
}

fn temporary_index(kind: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ultron-{kind}-{}-{}", std::process::id(), random_hex(6)))
}

struct TempIndex(PathBuf);

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let mut lock = self.0.clone().into_os_string();
        lock.push(".lock");
        let _ = std::fs::remove_file(lock);
    }
}

/// Copy the index with its modification time. Git trusts an entry whose cached stat matches the file (seconds
/// only) unless the entry is racily clean: not older than the index file itself. A plain copy is newer than every
/// entry, so a file rewritten with the same size in the second it was staged (an edit right after a commit) looked
/// unchanged once the copy landed a second later, and the snapshot left the edit out: under load the fixture's
/// parent edit went missing and a conflicting merge came back as "changed during the merge".
fn copy_index(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::copy(from, to)?;
    let modified = std::fs::metadata(from)?.modified()?;
    std::fs::File::options().write(true).open(to)?.set_modified(modified)
}

/// A commit of `top`'s working tree as it is now, made from a copy of its index (the working tree and the real index
/// stay untouched); HEAD itself (`dirty: false`) when nothing differs from it.
pub fn snapshot_working_tree(top: &Path, head: &str, message: &str) -> Result<(String, bool)> {
    let index_path = git_ok(top, &["rev-parse", "--path-format=absolute", "--git-path", "index"])?;
    let temporary = TempIndex(temporary_index("index"));
    let index = text(&temporary.0);
    let env = [("GIT_INDEX_FILE", index.as_str()), ("GIT_OPTIONAL_LOCKS", "0")];
    if copy_index(Path::new(index_path.trim()), &temporary.0).is_err() {
        git_ok_with(top, &["read-tree", head], opts(&env))?;
    }
    git_ok_with(top, &quiet(&["add", "-A", "--", "."]), opts(&env))?;
    let tree = git_ok_with(top, &["write-tree"], opts(&env))?.trim().to_string();
    let head_tree = git_ok(top, &["rev-parse", &format!("{head}^{{tree}}")])?
        .trim()
        .to_string();
    if tree == head_tree {
        return Ok((head.to_string(), false));
    }
    let identity = identity_env(top)?;
    let commit = git_ok_with(
        top,
        &quiet(&["commit-tree", &tree, "-p", head, "-m", message]),
        GitOptions {
            env: identity,
            input: None,
        },
    )?
    .trim()
    .to_string();
    Ok((commit, true))
}

// ---------------------------------------------------------------------------------------------------------------
// Records

fn records_dir(common_dir: &Path) -> PathBuf {
    common_dir.join(WORKTREES_DIR)
}

fn record_path(record: &WorktreeRecord) -> PathBuf {
    records_dir(Path::new(&record.common_dir)).join(format!("{}.json", record.id))
}

/// The record as `JSON.stringify(record, null, "\t")` writes it.
pub fn record_json(record: &WorktreeRecord) -> String {
    let value = serde_json::to_value(record).unwrap_or_default();
    let mut out = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut serializer = serde_json::Serializer::with_formatter(&mut out, formatter);
    let _ = serde::Serialize::serialize(&js_numbers(value), &mut serializer);
    String::from_utf8_lossy(&out).into_owned()
}

/// Whole floats as integers, as JavaScript prints them.
fn js_numbers(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Number(n) => match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 9e15 => serde_json::Value::from(f as i64),
            _ => serde_json::Value::Number(n),
        },
        serde_json::Value::Array(items) => serde_json::Value::Array(items.into_iter().map(js_numbers).collect()),
        serde_json::Value::Object(map) => {
            serde_json::Value::Object(map.into_iter().map(|(k, v)| (k, js_numbers(v))).collect())
        }
        other => other,
    }
}

pub fn save_record(record: &WorktreeRecord) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(records_dir(Path::new(&record.common_dir)))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(record_path(record))?;
    file.write_all(format!("{}\n", record_json(record)).as_bytes())?;
    Ok(())
}

fn delete_record(record: &WorktreeRecord) {
    let _ = std::fs::remove_file(record_path(record));
}

/// Every recorded worktree of the repository `cwd` is in (any session).
pub fn list_records(cwd: &Path) -> Vec<WorktreeRecord> {
    match repo_info(cwd) {
        Ok(info) => read_records(&info.common_dir),
        Err(_) => Vec::new(),
    }
}

fn read_records(common_dir: &Path) -> Vec<WorktreeRecord> {
    let dir = records_dir(common_dir);
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".json"))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
        .iter()
        .filter_map(|name| std::fs::read_to_string(dir.join(name)).ok())
        .filter_map(|text| serde_json::from_str::<WorktreeRecord>(&text).ok())
        .filter(|record| record.version == 1)
        .collect()
}

pub fn hostname() -> String {
    rustix::system::uname().nodename().to_string_lossy().into_owned()
}

/// Whether a record's owning process still runs (on this machine).
pub fn owner_alive(owner: &Owner) -> bool {
    if owner.host != hostname() || owner.pid == std::process::id() {
        return true;
    }
    let Some(pid) = rustix::process::Pid::from_raw(owner.pid as i32) else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(error) => error == rustix::io::Errno::PERM,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Create

static SLUG_RULES: LazyLock<[Regex; 5]> = LazyLock::new(|| {
    [
        Regex::new(r"[^a-z0-9._-]+").unwrap_or_else(|_| unreachable!()),
        Regex::new(r"\.{2,}").unwrap_or_else(|_| unreachable!()),
        Regex::new(r"-{2,}").unwrap_or_else(|_| unreachable!()),
        Regex::new(r"^[-.]+|[-.]+$").unwrap_or_else(|_| unreachable!()),
        Regex::new(r"[-.]+$").unwrap_or_else(|_| unreachable!()),
    ]
});

/// JavaScript `toLowerCase` then `slice(0, n)` counts UTF-16 units; the slug is ASCII by then.
pub fn slug(name: &str) -> String {
    let rules = &*SLUG_RULES;
    let text = name.to_lowercase();
    let text = rules[0].replace_all(&text, "-");
    let text = rules[1].replace_all(&text, ".");
    let text = rules[2].replace_all(&text, "-");
    let text = rules[3].replace_all(&text, "");
    let text: String = text.chars().take(40).collect();
    let text = rules[4].replace_all(&text, "").into_owned();
    if text.is_empty() { "child".into() } else { text }
}

fn branch_exists(top: &Path, branch: &str) -> Result<bool> {
    Ok(git(
        top,
        &["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")],
    )?
    .code
        == 0)
}

/// A new branch for a child: `ultron/<session>/<name>`, suffixed when taken (never reused).
fn unique_branch(top: &Path, session_id: &str, name: &str) -> Result<String> {
    let session: String = slug(session_id).chars().take(8).collect();
    let stem = format!("{BRANCH_PREFIX}/{session}/{}", slug(name));
    for attempt in 1..1000 {
        let candidate = if attempt == 1 {
            stem.clone()
        } else {
            format!("{stem}-{attempt}")
        };
        if git(top, &["check-ref-format", "--branch", &candidate])?.code != 0 {
            return Err(WorktreeError::new(format!(
                "cannot make a branch name from {}",
                serde_json::Value::from(name)
            )));
        }
        if !branch_exists(top, &candidate)? {
            return Ok(candidate);
        }
    }
    Err(WorktreeError::new(format!("no free branch name for {name}")))
}

/// `path` relative to `root` with "/" separators, "" for the root itself, `None` outside it.
pub fn relative_inside(root: &Path, path: &Path) -> Option<String> {
    let rel = normalize(path).strip_prefix(normalize(root)).ok()?.to_path_buf();
    Some(
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn safe_relative(path: &str, what: &str) -> Result<String> {
    let normalized = path.replace('\\', "/");
    let normalized = normalized.trim_end_matches('/');
    if normalized.is_empty() || normalized.starts_with('/') || normalized.split('/').any(|p| p == ".." || p.is_empty())
    {
        return Err(WorktreeError::new(format!(
            "worktree setup {what} path {} must be relative to the repository root",
            serde_json::Value::from(path)
        )));
    }
    Ok(normalized.to_string())
}

/// Gitignored entries of the parent's checkout: directories end with "/".
fn ignored_entries(top: &Path) -> Result<Vec<String>> {
    let result = git_with(
        top,
        &[
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "--no-empty-directory",
        ],
        opts(&[("GIT_OPTIONAL_LOCKS", "0")]),
    )?;
    if result.code != 0 {
        return Ok(Vec::new());
    }
    Ok(result
        .stdout
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

fn sorted_entries(dir: &Path) -> Vec<String> {
    // Node's readdir order is the directory's order; sorting keeps runs comparable.
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Link a node_modules directory entry by entry; links back into the repository (workspace packages) point to the
/// same place in the worktree.
fn link_node_modules(source: &Path, target: &Path, repo: &Path, worktree: &Path) -> Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in sorted_entries(source) {
        let from = source.join(&entry);
        let to = target.join(&entry);
        let Ok(info) = std::fs::symlink_metadata(&from) else {
            continue;
        };
        if entry.starts_with('@') && info.is_dir() {
            link_node_modules(&from, &to, repo, worktree)?;
            continue;
        }
        let mut destination = from.clone();
        if info.file_type().is_symlink()
            && let Ok(resolved) = std::fs::canonicalize(&from)
            && let Some(inside) = relative_inside(repo, &resolved)
            && !inside.split('/').any(|p| p == "node_modules")
        {
            destination = worktree.join(inside);
        }
        let _ = symlink(&destination, &to);
    }
    Ok(())
}

fn is_env_name(name: &str) -> bool {
    name == ".env" || name.strip_prefix(".env.").is_some_and(|rest| !rest.is_empty())
}

fn apply_setup(top: &Path, worktree: &Path, setup: &WorktreeSetup) -> Result<(Vec<String>, Vec<String>)> {
    let ignored = if setup.link.is_none() || setup.copy.is_none() {
        ignored_entries(top)?
    } else {
        Vec::new()
    };
    let depth = |path: &str| path.trim_end_matches('/').split('/').count();
    let links: Vec<String> = match &setup.link {
        Some(paths) => paths.iter().map(|p| safe_relative(p, "link")).collect::<Result<_>>()?,
        None => ignored
            .iter()
            .filter(|e| e.ends_with('/') && depth(e) <= MAX_SETUP_DEPTH)
            .map(|e| e[..e.len() - 1].to_string())
            .filter(|p| DEFAULT_LINK_NAMES.contains(&p.rsplit('/').next().unwrap_or("")))
            .take(MAX_SETUP_ENTRIES)
            .collect(),
    };
    let in_linked = |path: &str| {
        links
            .iter()
            .any(|link| path == link || path.starts_with(&format!("{link}/")))
    };
    let copies: Vec<String> = match &setup.copy {
        Some(paths) => paths.iter().map(|p| safe_relative(p, "copy")).collect::<Result<_>>()?,
        None => ignored
            .iter()
            .filter(|e| {
                !e.ends_with('/')
                    && depth(e) <= MAX_SETUP_DEPTH
                    && is_env_name(e.rsplit('/').next().unwrap_or(""))
                    && !in_linked(e)
            })
            .take(MAX_SETUP_ENTRIES)
            .cloned()
            .collect(),
    };
    let mut linked = Vec::new();
    for path in &links {
        let (from, to) = (top.join(path), worktree.join(path));
        let Ok(source) = std::fs::metadata(&from) else {
            if setup.link.is_some() {
                return Err(WorktreeError::new(format!(
                    "worktree setup link {path} does not exist in {}",
                    text(top)
                )));
            }
            continue;
        };
        if std::fs::symlink_metadata(&to).is_ok() {
            continue;
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.rsplit('/').next() == Some("node_modules") && source.is_dir() {
            link_node_modules(&from, &to, top, worktree)?;
        } else if source.is_dir() {
            std::fs::create_dir_all(&to)?;
            for entry in sorted_entries(&from) {
                let _ = symlink(from.join(&entry), to.join(&entry));
            }
        } else {
            symlink(&from, &to)?;
        }
        linked.push(path.clone());
    }
    let mut copied = Vec::new();
    for path in &copies {
        let (from, to) = (top.join(path), worktree.join(path));
        let source = std::fs::metadata(&from).ok().filter(|m| m.is_file());
        let Some(source) = source else {
            if setup.copy.is_some() {
                return Err(WorktreeError::new(format!(
                    "worktree setup copy {path} is not a file in {}",
                    text(top)
                )));
            }
            continue;
        };
        if setup.copy.is_none() && source.len() > MAX_COPY_BYTES {
            continue;
        }
        if std::fs::symlink_metadata(&to).is_ok() {
            continue;
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(&from, &to)?;
        copied.push(path.clone());
    }
    Ok((linked, copied))
}

pub struct CreateWorktreeOptions<'a> {
    /// The parent's working directory: the worktree branches from its repository's working tree.
    pub cwd: &'a Path,
    pub session_id: &'a str,
    pub name: &'a str,
    pub setup: Option<&'a WorktreeSetup>,
    /// Milliseconds since the epoch for `createdAt`.
    pub now: f64,
}

/// Creations per repository run one at a time, so branch names are picked and taken without a race.
static CREATING: LazyLock<Mutex<()>> = LazyLock::new(Mutex::default);

/// Create a child's worktree (see the module comment).
pub fn create_child_worktree(options: &CreateWorktreeOptions<'_>) -> Result<WorktreeRecord> {
    let info = repo_info(options.cwd)?;
    let _serial = CREATING.lock().unwrap_or_else(PoisonError::into_inner);
    create_in(&info, options)
}

fn create_in(info: &RepoInfo, options: &CreateWorktreeOptions<'_>) -> Result<WorktreeRecord> {
    let Some(head) = info.head.clone() else {
        return Err(WorktreeError::fallback(format!(
            "worktree=True needs a commit to branch from, and the repository at {} has none yet",
            text(&info.top)
        )));
    };
    let (base, dirty) = snapshot_working_tree(
        &info.top,
        &head,
        &format!("ultron: snapshot of the working tree for sub-agent {}", options.name),
    )?;
    let branch = unique_branch(&info.top, options.session_id, options.name)?;
    let id = format!(
        "{}-{}-{}",
        slug(options.session_id).chars().take(8).collect::<String>(),
        slug(options.name).chars().take(24).collect::<String>(),
        random_hex(3)
    );
    let path = records_dir(&info.common_dir).join(&id);
    let real = std::fs::canonicalize(options.cwd).unwrap_or_else(|_| normalize(options.cwd));
    let at = relative_inside(&info.top, &real).unwrap_or_default();
    let setup = options.setup.cloned().unwrap_or_default();
    let mut record = WorktreeRecord {
        version: 1,
        id,
        session_id: options.session_id.to_string(),
        name: options.name.to_string(),
        owner: Owner {
            pid: std::process::id(),
            host: hostname(),
        },
        repo: text(&info.top),
        common_dir: text(&info.common_dir),
        path: text(&path),
        cwd: if at.is_empty() {
            text(&path)
        } else {
            text(&path.join(&at))
        },
        branch: branch.clone(),
        head,
        base: base.clone(),
        dirty,
        linked: Vec::new(),
        copied: Vec::new(),
        command: setup
            .command
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(String::from),
        command_timeout_ms: setup.timeout_ms,
        created_at: options.now,
        state: "creating".into(),
        task_id: None,
        commit: None,
    };
    // Recorded first: a crash in the middle leaves something the next session can prune.
    save_record(&record)?;
    let made = (|| -> Result<()> {
        git_ok(
            &info.top,
            &quiet(&["worktree", "add", "--quiet", "-b", &branch, &record.path, &base]),
        )?;
        std::fs::create_dir_all(&record.cwd)?;
        let (linked, copied) = apply_setup(&info.top, &path, &setup)?;
        record.linked.extend(linked);
        record.copied.extend(copied);
        record.state = "active".into();
        save_record(&record)
    })();
    match made {
        Ok(()) => Ok(record),
        Err(error) => {
            remove_child_worktree(&record, true);
            Err(error)
        }
    }
}

/// Environment of every process working in a child's worktree.
pub fn worktree_env(record: &WorktreeRecord) -> Vec<(String, String)> {
    vec![
        ("ULTRON_WORKTREE".into(), record.path.clone()),
        ("ULTRON_PARENT_REPO".into(), record.repo.clone()),
        ("ULTRON_WORKTREE_BRANCH".into(), record.branch.clone()),
        ("PWD".into(), record.cwd.clone()),
    ]
}

/// Run the setup command (`worktree_setup={"command": ...}`) in the child's working directory, in its own process
/// group (killed on timeout or when `signal` is aborted).
pub fn run_setup_command(record: &WorktreeRecord, signal: Option<&Signal>) -> Result<()> {
    let Some(command) = record.command.as_deref() else {
        return Ok(());
    };
    // One pipe for stdout and stderr: the output keeps the order the command wrote it in. Two pipes drained by two
    // threads (or two event-loop callbacks) can reorder `echo out; echo err >&2` into "err\nout".
    let (reader, writer) = std::io::pipe()?;
    let mut child = {
        let mut sh = Command::new("/bin/sh");
        sh.args(["-c", command])
            .current_dir(&record.cwd)
            .envs(worktree_env(record))
            .stdin(Stdio::null())
            .stdout(writer.try_clone()?)
            .stderr(writer)
            .process_group(0);
        // The Command (and with it our copies of the write end) is dropped here, so the reader sees EOF once the
        // command and anything it left running close the pipe.
        sh.spawn()
    };
    let output = std::sync::Arc::new(Mutex::new(String::new()));
    let (code, timed_out) = match &mut child {
        Err(error) => {
            *output.lock().unwrap_or_else(PoisonError::into_inner) = error.to_string();
            (None, false)
        }
        Ok(child) => {
            let drain = {
                let output = output.clone();
                let mut pipe = reader;
                std::thread::spawn(move || {
                    let mut chunk = [0u8; 8192];
                    while let Ok(n) = pipe.read(&mut chunk) {
                        if n == 0 {
                            break;
                        }
                        let mut out = output.lock().unwrap_or_else(PoisonError::into_inner);
                        out.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        let excess = out.chars().count().saturating_sub(4000);
                        if excess > 0 {
                            *out = out.chars().skip(excess).collect();
                        }
                    }
                })
            };
            let group = rustix::process::Pid::from_raw(child.id() as i32);
            let timeout = Duration::from_millis(record.command_timeout_ms.unwrap_or(DEFAULT_SETUP_TIMEOUT_MS));
            let started = std::time::Instant::now();
            let mut timed_out = false;
            let status = loop {
                if let Some(status) = child.try_wait()? {
                    break status;
                }
                let aborted = signal.is_some_and(Signal::is_aborted);
                if (started.elapsed() >= timeout || aborted)
                    && let Some(group) = group
                {
                    timed_out |= !aborted;
                    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::TERM);
                    break child.wait()?;
                }
                match signal {
                    Some(signal) => {
                        signal.wait(Duration::from_millis(10));
                    }
                    None => std::thread::sleep(Duration::from_millis(10)),
                }
            };
            let _ = drain.join();
            (status.code(), timed_out)
        }
    };
    if let Some(reason) = signal.and_then(Signal::reason) {
        return Err(WorktreeError::new(reason));
    }
    if code != Some(0) {
        let output = output.lock().unwrap_or_else(PoisonError::into_inner).trim().to_string();
        let tail: String = {
            let chars: Vec<char> = output.chars().collect();
            chars[chars.len().saturating_sub(1500)..].iter().collect()
        };
        let how = if timed_out {
            "timed out".to_string()
        } else {
            format!("failed (exit {})", code.map_or("?".to_string(), |c| c.to_string()))
        };
        return Err(WorktreeError::new(format!(
            "worktree setup command {} {how}: {tail}",
            serde_json::Value::from(command)
        )));
    }
    Ok(())
}

/// The note added to a worktree child's brief.
pub fn worktree_brief_note(record: &WorktreeRecord) -> String {
    format!(
        "[Worktree] You work in your own Git worktree {} (branch {}): a private copy of your parent's checkout, uncommitted changes included. Use paths relative to it; read/edit/write map absolute paths under {} into it, but shell commands do not, so never cd there. Your changes are committed on your branch when you finish and your parent merges them: do not commit, switch branches or push yourself, even if your brief asks you to commit.",
        record.cwd, record.branch, record.repo
    )
}

/// An absolute path into the parent's checkout, as the same path in a child's worktree; any other path as it is.
pub fn in_worktree(path: &str, record: &WorktreeRecord) -> String {
    if !path.starts_with('/') {
        return path.to_string();
    }
    if relative_inside(Path::new(&record.path), Path::new(path)).is_some() {
        return path.to_string();
    }
    match relative_inside(Path::new(&record.repo), Path::new(path)) {
        Some(rel) if rel.is_empty() => record.path.clone(),
        Some(rel) => text(&Path::new(&record.path).join(rel)),
        None => path.to_string(),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Commit

/// Paths (relative to the root) that differ from HEAD in a working tree, untracked ones included; `None` on error.
pub fn list_dirty(root: &Path) -> Option<Vec<String>> {
    let status = git_with(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
            "--ignore-submodules=all",
        ],
        opts(&[("GIT_OPTIONAL_LOCKS", "0")]),
    )
    .ok()?;
    if status.code != 0 {
        return None;
    }
    Some(
        status
            .stdout
            .split('\0')
            .filter(|item| item.len() > 3)
            .map(|item| item[3..].to_string())
            .collect(),
    )
}

fn outside_setup(record: &WorktreeRecord, path: &str) -> bool {
    !record
        .linked
        .iter()
        .chain(&record.copied)
        .any(|entry| path == entry || path.starts_with(&format!("{entry}/")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    pub commit: Option<String>,
    pub changed_files: Vec<String>,
    pub diffstat: String,
}

/// Commit what the child changed on its branch; `paths` (relative to the worktree root) limits the commit, by default
/// every change Git sees minus the setup links and copies.
pub fn commit_child_worktree(
    record: &WorktreeRecord,
    message: &str,
    paths: Option<&[String]>,
) -> Result<CommitOutcome> {
    let root = Path::new(&record.path);
    let candidates = match paths {
        Some(paths) => paths.to_vec(),
        None => list_dirty(root).unwrap_or_default(),
    };
    let wanted: Vec<String> = candidates.into_iter().filter(|p| outside_setup(record, p)).collect();
    if !wanted.is_empty() {
        let literal = [("GIT_LITERAL_PATHSPECS", "1")];
        let mut input = wanted.join("\0").into_bytes();
        input.push(0);
        let added = git_ok_with(
            root,
            &quiet(&[
                "add",
                "-A",
                "--ignore-errors",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ]),
            GitOptions {
                env: literal.iter().map(|(k, v)| (*k, v.to_string())).collect(),
                input: Some(input),
            },
        );
        if added.is_err() {
            // One bad path (ignored, vanished) fails the batch: add the rest one by one.
            for path in &wanted {
                git_with(root, &quiet(&["add", "-A", "--", path]), opts(&literal))?;
            }
        }
        if git(root, &["diff", "--cached", "--quiet"])?.code == 1 {
            git_ok_with(
                root,
                &quiet(&["commit", "-q", "--no-verify", "-m", message]),
                GitOptions {
                    env: identity_env(root)?,
                    input: None,
                },
            )?;
        }
    }
    let head = git_ok(root, &["rev-parse", "HEAD"])?.trim().to_string();
    let same = git(root, &["diff", "--quiet", &record.base, &head])?;
    if head == record.base || same.code == 0 {
        return Ok(CommitOutcome {
            commit: None,
            changed_files: Vec::new(),
            diffstat: String::new(),
        });
    }
    let names = git_ok(
        root,
        &["diff", "--name-only", "-z", "--no-renames", &record.base, &head],
    )?;
    let stat = git_ok(root, &["diff", "--shortstat", &record.base, &head])?;
    Ok(CommitOutcome {
        commit: Some(head),
        changed_files: names.split('\0').filter(|s| !s.is_empty()).map(String::from).collect(),
        diffstat: stat.trim().to_string(),
    })
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn utf16_prefix(text: &str, units: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        used += c.len_utf16();
        if used > units {
            break;
        }
        out.push(c);
    }
    out
}

/// A commit message from a child's summary, with its name and task (no trailers).
pub fn commit_message(summary: &str, name: &str, task_id: Option<&str>, status: &str) -> String {
    let trimmed = summary.trim();
    let text = if trimmed.is_empty() {
        format!("Sub-agent {name} ({status})")
    } else {
        trimmed.to_string()
    };
    let mut lines = text.split('\n');
    let first = lines.next().unwrap_or("");
    let rest: Vec<&str> = lines.collect();
    let long = utf16_len(first) > 72;
    let subject = if long {
        format!("{}…", utf16_prefix(first, 71))
    } else {
        first.to_string()
    };
    let mut body_lines = vec![if long { first } else { "" }];
    body_lines.extend(rest);
    let body = body_lines.join("\n").trim().to_string();
    let mut out = vec![subject];
    if !body.is_empty() {
        out.push(String::new());
        out.push(body);
    }
    out.push(String::new());
    out.push(format!(
        "Ultron sub-agent \"{name}\"{}, {status}.",
        task_id.map_or(String::new(), |id| format!(" (task {id})"))
    ));
    out.join("\n")
}

// ---------------------------------------------------------------------------------------------------------------
// Merge

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictHunk {
    /// First line of the conflict in the merged file (1-based).
    pub line: usize,
    pub ours: String,
    pub theirs: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictFile {
    pub path: String,
    pub kind: String,
    pub hunks: Vec<ConflictHunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Merged {
        files: Vec<String>,
        diffstat: String,
    },
    Empty,
    Conflict {
        files: Vec<String>,
        conflicts: Vec<ConflictFile>,
        /// Conflict markers were written into the working tree.
        markers: bool,
        message: String,
    },
}

impl MergeOutcome {
    pub fn status(&self) -> &'static str {
        match self {
            MergeOutcome::Merged { .. } => "merged",
            MergeOutcome::Empty => "empty",
            MergeOutcome::Conflict { .. } => "conflict",
        }
    }
}

#[derive(Debug, Clone)]
struct TreeChange {
    src_mode: String,
    dst_mode: String,
    src_oid: String,
    path: String,
}

fn tree_changes(top: &Path, from: &str, to: &str) -> Result<Vec<TreeChange>> {
    let raw = git_ok(top, &["diff-tree", "-r", "-z", "--no-renames", from, to])?;
    let parts: Vec<&str> = raw.split('\0').collect();
    let mut changes = Vec::new();
    let mut index = 0;
    while index + 1 < parts.len() {
        let meta = parts[index];
        let path = parts[index + 1];
        let Some(meta) = meta.strip_prefix(':') else {
            break;
        };
        let fields: Vec<&str> = meta.split(' ').collect();
        changes.push(TreeChange {
            src_mode: fields.first().copied().unwrap_or("").into(),
            dst_mode: fields.get(1).copied().unwrap_or("").into(),
            src_oid: fields.get(2).copied().unwrap_or("").into(),
            path: path.into(),
        });
        index += 2;
    }
    Ok(changes)
}

/// Paths whose working-tree content no longer matches `changes`' source side.
fn drifted(top: &Path, changes: &[TreeChange]) -> Result<Vec<String>> {
    let mut moved = Vec::new();
    let mut files = Vec::new();
    for change in changes {
        if change.src_mode == "160000" || change.dst_mode == "160000" {
            continue;
        }
        let info = std::fs::symlink_metadata(top.join(&change.path)).ok();
        if change.src_mode == ZERO_MODE {
            if info.is_some() {
                moved.push(change.path.clone());
            }
            continue;
        }
        let Some(info) = info else {
            moved.push(change.path.clone());
            continue;
        };
        if change.src_mode == "120000" {
            if !info.file_type().is_symlink() {
                moved.push(change.path.clone());
            } else {
                let target = std::fs::read_link(top.join(&change.path))?;
                let oid = git_ok_with(
                    top,
                    &["hash-object", "--stdin"],
                    GitOptions {
                        env: Vec::new(),
                        input: Some(target.as_os_str().as_encoded_bytes().to_vec()),
                    },
                )?;
                if oid.trim() != change.src_oid {
                    moved.push(change.path.clone());
                }
            }
            continue;
        }
        files.push(change);
    }
    if !files.is_empty() {
        let input = files.iter().map(|c| format!("{}\n", c.path)).collect::<String>();
        let oids = git_ok_with(
            top,
            &["hash-object", "--stdin-paths"],
            GitOptions {
                env: Vec::new(),
                input: Some(input.into_bytes()),
            },
        )?;
        let oids: Vec<&str> = oids.trim().split('\n').collect();
        for (index, change) in files.iter().enumerate() {
            if oids.get(index).copied() != Some(change.src_oid.as_str()) {
                moved.push(change.path.clone());
            }
        }
    }
    Ok(moved)
}

/// Write `tree`'s side of `changes` into the working tree through a temporary index (the real index is untouched).
fn write_changes(top: &Path, tree: &str, changes: &[TreeChange]) -> Result<()> {
    let temporary = TempIndex(temporary_index("merge-index"));
    let index = text(&temporary.0);
    let env = [("GIT_INDEX_FILE", index.as_str())];
    let written: Vec<&TreeChange> = changes
        .iter()
        .filter(|c| c.dst_mode != ZERO_MODE && c.dst_mode != "160000")
        .collect();
    if !written.is_empty() {
        git_ok_with(top, &["read-tree", tree], opts(&env))?;
        // A path that is a directory now but a file in the result is cleared first.
        for change in &written {
            let target = top.join(&change.path);
            if std::fs::symlink_metadata(&target).is_ok_and(|m| m.is_dir()) {
                let _ = std::fs::remove_dir_all(&target);
            }
        }
        let mut input = written
            .iter()
            .map(|c| c.path.as_str())
            .collect::<Vec<_>>()
            .join("\0")
            .into_bytes();
        input.push(0);
        git_ok_with(
            top,
            &quiet(&["checkout-index", "-f", "-z", "--stdin"]),
            GitOptions {
                env: env.iter().map(|(k, v)| (*k, v.to_string())).collect(),
                input: Some(input),
            },
        )?;
    }
    for change in changes.iter().filter(|c| c.dst_mode == ZERO_MODE) {
        let target = top.join(&change.path);
        let _ = std::fs::remove_file(&target);
        // Directories the deletion emptied go too, as a checkout would remove them.
        let mut dir = target.parent().map(Path::to_path_buf);
        while let Some(current) = dir {
            if current == top || !current.starts_with(top) || std::fs::remove_dir(&current).is_err() {
                break;
            }
            dir = current.parent().map(Path::to_path_buf);
        }
    }
    Ok(())
}

fn clip(text: &str) -> String {
    if utf16_len(text) > HUNK_TEXT_CHARS {
        format!("{}…", utf16_prefix(text, HUNK_TEXT_CHARS - 1))
    } else {
        text.to_string()
    }
}

/// The conflict regions of a merged file with markers.
pub fn conflict_hunks(text: &str) -> Vec<ConflictHunk> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut hunks = Vec::new();
    let mut index = 0;
    while index < lines.len() && hunks.len() < MAX_HUNKS_PER_FILE {
        if !lines[index].starts_with("<<<<<<< ") {
            index += 1;
            continue;
        }
        let start = index;
        let (mut ours, mut theirs) = (Vec::new(), Vec::new());
        let mut side = 0; // 0 ours, 1 base, 2 theirs
        index += 1;
        while index < lines.len() {
            let line = lines[index];
            if line.starts_with("||||||| ") {
                side = 1;
            } else if line == "=======" {
                side = 2;
            } else if line.starts_with(">>>>>>> ") {
                break;
            } else if side == 0 {
                ours.push(line);
            } else if side == 2 {
                theirs.push(line);
            }
            index += 1;
        }
        hunks.push(ConflictHunk {
            line: start + 1,
            ours: clip(&ours.join("\n")),
            theirs: clip(&theirs.join("\n")),
        });
        index += 1;
    }
    hunks
}

struct MergeTree {
    tree: String,
    paths: Vec<String>,
    messages: Vec<(Vec<String>, String, String)>,
}

/// Parse `git merge-tree --write-tree -z --name-only` output.
fn parse_merge_tree(stdout: &str) -> MergeTree {
    let parts: Vec<&str> = stdout.split('\0').collect();
    let tree = parts.first().copied().unwrap_or("").trim().to_string();
    let mut index = 1;
    let mut paths: Vec<String> = Vec::new();
    while index < parts.len() && !parts[index].is_empty() {
        if !paths.iter().any(|p| p == parts[index]) {
            paths.push(parts[index].to_string());
        }
        index += 1;
    }
    index += 1;
    let mut messages = Vec::new();
    while index < parts.len() {
        let Ok(count) = parts[index].parse::<usize>() else {
            break;
        };
        let own: Vec<String> = parts
            .iter()
            .skip(index + 1)
            .take(count)
            .map(|s| s.to_string())
            .collect();
        let kind = parts.get(index + 1 + count).copied().unwrap_or("").to_string();
        let text = parts.get(index + 2 + count).copied().unwrap_or("").trim().to_string();
        messages.push((own, kind, text));
        index += 3 + count;
    }
    MergeTree { tree, paths, messages }
}

/// What `rlm.merge` merges from a child.
#[derive(Debug, Clone)]
pub struct MergeSource {
    pub name: String,
    pub branch: String,
    pub base: String,
    pub commit: Option<String>,
}

fn drift_conflict(moved: Vec<String>) -> MergeOutcome {
    MergeOutcome::Conflict {
        conflicts: moved
            .iter()
            .map(|path| ConflictFile {
                path: path.clone(),
                kind: "CONFLICT (changed during the merge)".into(),
                hunks: Vec::new(),
            })
            .collect(),
        message: format!("files changed in the working tree while merging: {}", moved.join(", ")),
        files: moved,
        markers: false,
    }
}

/// Merge a child's branch into the working tree at `target_cwd` as uncommitted changes. `markers` writes the
/// conflicted result with conflict markers instead of leaving the tree as it was.
pub fn merge_child_worktree(target_cwd: &Path, source: &MergeSource, markers: bool) -> Result<MergeOutcome> {
    let Some(commit) = source.commit.as_deref() else {
        return Ok(MergeOutcome::Empty);
    };
    let target = repo_info(target_cwd)?;
    let Some(head) = target.head.as_deref() else {
        return Err(WorktreeError::new(format!(
            "the repository at {} has no commits to merge into",
            text(&target.top)
        )));
    };
    let top = target.top.as_path();
    if git(top, &["cat-file", "-e", &format!("{commit}^{{commit}}")])?.code != 0 {
        return Err(WorktreeError::new(format!(
            "{}'s commit {} is not in the repository at {}",
            source.name,
            &commit[..commit.len().min(12)],
            text(top)
        )));
    }
    let (ours, _) = snapshot_working_tree(
        top,
        head,
        &format!("ultron: working tree before merging sub-agent {}", source.name),
    )?;
    let merged = git(
        top,
        &[
            "merge-tree",
            "--write-tree",
            "-z",
            "--name-only",
            &format!("--merge-base={}", source.base),
            &ours,
            commit,
        ],
    )?;
    if merged.code != 0 && merged.code != 1 {
        let lower = merged.stderr.to_lowercase();
        return Err(WorktreeError::new(
            if lower.contains("unknown option") || lower.contains("usage: git merge-tree") {
                "rlm.merge needs Git 2.40 or newer (git merge-tree --write-tree --merge-base); update Git".to_string()
            } else {
                let why = first_lines(&merged.stderr, 3);
                format!(
                    "git merge-tree failed: {}",
                    if why.is_empty() {
                        format!("exit {}", merged.code)
                    } else {
                        why
                    }
                )
            },
        ));
    }
    let parsed = parse_merge_tree(&merged.stdout);
    let changes = tree_changes(top, &ours, &parsed.tree)?;
    if changes.is_empty() && merged.code == 0 {
        return Ok(MergeOutcome::Empty);
    }
    if merged.code == 1 {
        let mut conflicts = Vec::new();
        for path in parsed.paths.iter().take(MAX_CONFLICT_FILES) {
            let blob = git(top, &["cat-file", "blob", &format!("{}:{path}", parsed.tree)])?;
            let kind = parsed
                .messages
                .iter()
                .find(|(paths, kind, _)| paths.contains(path) && kind.starts_with("CONFLICT"))
                .map_or("CONFLICT".to_string(), |(_, kind, _)| kind.clone());
            conflicts.push(ConflictFile {
                path: path.clone(),
                kind,
                hunks: if blob.code == 0 {
                    conflict_hunks(&blob.stdout)
                } else {
                    Vec::new()
                },
            });
        }
        let message = parsed
            .messages
            .iter()
            .filter(|(_, kind, _)| kind.starts_with("CONFLICT"))
            .map(|(_, _, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !markers {
            return Ok(MergeOutcome::Conflict {
                files: parsed.paths,
                conflicts,
                markers: false,
                message,
            });
        }
        let moved = drifted(top, &changes)?;
        if !moved.is_empty() {
            return Ok(drift_conflict(moved));
        }
        write_changes(top, &parsed.tree, &changes)?;
        // Readable labels instead of commit ids on the markers.
        for path in &parsed.paths {
            let file = top.join(path);
            let Ok(content) = std::fs::read_to_string(&file) else {
                continue;
            };
            let relabeled = content
                .replace(&format!("<<<<<<< {ours}"), "<<<<<<< working tree")
                .replace(&format!(">>>>>>> {commit}"), &format!(">>>>>>> {}", source.branch));
            std::fs::write(&file, relabeled)?;
        }
        return Ok(MergeOutcome::Conflict {
            files: parsed.paths,
            conflicts,
            markers: true,
            message,
        });
    }
    let moved = drifted(top, &changes)?;
    if !moved.is_empty() {
        return Ok(drift_conflict(moved));
    }
    write_changes(top, &parsed.tree, &changes)?;
    let stat = git(top, &["diff", "--shortstat", &ours, &parsed.tree])?;
    Ok(MergeOutcome::Merged {
        files: changes.into_iter().map(|c| c.path).collect(),
        diffstat: stat.stdout.trim().to_string(),
    })
}

// ---------------------------------------------------------------------------------------------------------------
// Remove and prune

fn unlink_tree(dir: &Path) {
    for entry in sorted_entries(dir) {
        let path = dir.join(entry);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            unlink_tree(&path);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// The directory git commands for a record run in, and the `--git-dir` argument when the checkout is gone.
fn record_git(record: &WorktreeRecord) -> (PathBuf, Option<String>) {
    if Path::new(&record.repo).exists() {
        (PathBuf::from(&record.repo), None)
    } else {
        (
            PathBuf::from(&record.common_dir),
            Some(format!("--git-dir={}", record.common_dir)),
        )
    }
}

/// Remove a worktree's directory (its setup links first, never what they point to) and, optionally, its branch.
/// Best effort, like the TS removal.
pub fn remove_child_worktree(record: &WorktreeRecord, delete_branch: bool) {
    for path in record.linked.iter().chain(&record.copied) {
        let target = Path::new(&record.path).join(path);
        let Ok(info) = std::fs::symlink_metadata(&target) else {
            continue;
        };
        if info.file_type().is_symlink() || info.is_file() {
            let _ = std::fs::remove_file(&target);
        } else if info.is_dir() {
            unlink_tree(&target);
        }
    }
    let (cwd, git_dir) = record_git(record);
    let with = |args: &[&str]| -> Vec<String> {
        git_dir
            .iter()
            .cloned()
            .chain(args.iter().map(|s| s.to_string()))
            .collect()
    };
    let run = |args: Vec<String>| {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = git(&cwd, &refs);
    };
    run(with(&quiet(&[
        "worktree",
        "remove",
        "--force",
        "--force",
        &record.path,
    ])));
    let _ = std::fs::remove_dir_all(&record.path);
    run(with(&["worktree", "prune"]));
    if delete_branch {
        run(with(&["branch", "-D", "--", &record.branch]));
    }
    delete_record(record);
}

/// Whether a branch holds work beyond the base it started at.
fn branch_has_work(record: &WorktreeRecord) -> bool {
    let (cwd, git_dir) = record_git(record);
    let prefix: Vec<&str> = git_dir.iter().map(String::as_str).collect();
    let tip_ref = format!("refs/heads/{}", record.branch);
    let mut args = prefix.clone();
    args.extend(["rev-parse", "--verify", "-q", tip_ref.as_str()]);
    let Ok(tip) = git(&cwd, &args) else {
        return false;
    };
    if tip.code != 0 {
        return false;
    }
    let tip = tip.stdout.trim().to_string();
    let mut args = prefix;
    args.extend(["diff", "--quiet", record.base.as_str(), tip.as_str()]);
    git(&cwd, &args).is_ok_and(|same| same.code != 0)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: Vec<String>,
    /// Branches kept because they hold work.
    pub kept_branches: Vec<String>,
}

#[derive(Default)]
pub struct PruneOptions<'a> {
    pub select: Option<&'a dyn Fn(&WorktreeRecord) -> bool>,
    /// Also take records whose owner lives.
    pub force: bool,
    pub delete_branches: bool,
}

/// [`prune_worktrees`] of every record whose owner is gone, as session `session_id` starts, but that session's own:
/// its open children are reattached with their worktrees (the durable host resumes them; TypeScript, which does not,
/// prunes them too). A repository without Ultron's worktrees directory has nothing of ours to prune: no `git
/// worktree prune` runs there.
pub fn prune_orphan_worktrees(cwd: &Path, session_id: &str) -> PruneReport {
    let Ok(info) = repo_info(cwd) else {
        return PruneReport::default();
    };
    if !records_dir(&info.common_dir).exists() {
        return PruneReport::default();
    }
    let others = |record: &WorktreeRecord| record.session_id != session_id;
    let mut report = prune_worktrees(
        cwd,
        &PruneOptions {
            select: Some(&others),
            ..PruneOptions::default()
        },
    );
    remove_torn(&info, &read_records(&info.common_dir), &mut report);
    report
}

/// Directories of ours with no record (a torn record) are leftovers too.
fn remove_torn(info: &RepoInfo, records: &[WorktreeRecord], report: &mut PruneReport) {
    let dir = records_dir(&info.common_dir);
    for entry in sorted_entries(&dir) {
        if entry.ends_with(".json") || records.iter().any(|r| r.id == entry) {
            continue;
        }
        let path = text(&dir.join(&entry));
        let _ = git(&info.top, &quiet(&["worktree", "remove", "--force", "--force", &path]));
        let _ = std::fs::remove_dir_all(&path);
        report.removed.push(path);
    }
}

/// Remove the worktrees of owners that are gone: uncommitted work is committed on the branch first, the directory is
/// removed, and the branch deleted unless it holds work. Then `git worktree prune`.
pub fn prune_worktrees(cwd: &Path, options: &PruneOptions<'_>) -> PruneReport {
    let Ok(info) = repo_info(cwd) else {
        return PruneReport::default();
    };
    let mut report = PruneReport::default();
    let records = read_records(&info.common_dir);
    for record in &records {
        if options.select.is_some_and(|select| !select(record)) {
            continue;
        }
        if !options.force && owner_alive(&record.owner) {
            continue;
        }
        if Path::new(&record.path).exists() && (record.state == "active" || record.state == "creating") {
            let message = commit_message(
                &format!(
                    "Uncommitted work of sub-agent {}, saved when its worktree was pruned",
                    record.name
                ),
                &record.name,
                record.task_id.as_deref(),
                "interrupted",
            );
            let _ = commit_child_worktree(record, &message, None);
        }
        let keep = !options.delete_branches && branch_has_work(record);
        remove_child_worktree(record, !keep);
        report.removed.push(record.path.clone());
        if keep {
            report.kept_branches.push(record.branch.clone());
        }
    }
    if options.select.is_none() {
        remove_torn(&info, &records, &mut report);
    }
    let _ = git(&info.top, &["worktree", "prune"]);
    report
}

// ---------------------------------------------------------------------------------------------------------------
// JSON views, as the TS objects serialize

impl CommitOutcome {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "commit": self.commit, "changedFiles": self.changed_files, "diffstat": self.diffstat })
    }
}

impl ConflictFile {
    pub fn to_json(&self) -> serde_json::Value {
        let hunks: Vec<serde_json::Value> = self
            .hunks
            .iter()
            .map(|h| serde_json::json!({ "line": h.line, "ours": h.ours, "theirs": h.theirs }))
            .collect();
        serde_json::json!({ "path": self.path, "kind": self.kind, "hunks": hunks })
    }
}

impl MergeOutcome {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            MergeOutcome::Merged { files, diffstat } => {
                serde_json::json!({ "status": "merged", "files": files, "diffstat": diffstat })
            }
            MergeOutcome::Empty => serde_json::json!({ "status": "empty" }),
            MergeOutcome::Conflict {
                files,
                conflicts,
                markers,
                message,
            } => serde_json::json!({
                "status": "conflict",
                "files": files,
                "conflicts": conflicts.iter().map(ConflictFile::to_json).collect::<Vec<_>>(),
                "markers": markers,
                "message": message,
            }),
        }
    }
}

impl PruneReport {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "removed": self.removed, "keptBranches": self.kept_branches })
    }
}
