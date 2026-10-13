//! What `/review` reviews (`review_api.py` processes, `resolve_scope`, readers, untracked files, `related_code`):
//! a diff from git (the branch since its merge base with the default branch, or since a given ref, plus uncommitted
//! and untracked changes) or from `gh pr diff`, with a reader for the source around it. Every git and gh call goes
//! through a [`Runner`], so tests drive local repositories and script gh.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::args::{Options, ReviewError, classify_targets};
use crate::diff::{FileDiff, added_file, parse_diff};
use crate::py;
use crate::views::{VERIFY_WINDOW, symbols_for};

pub const GIT_TIMEOUT_S: u64 = 30;
pub const GH_TIMEOUT_S: u64 = 60;
pub const MAX_UNTRACKED_FILES: usize = 40;
pub const MAX_UNTRACKED_BYTES: usize = 200_000;
pub const RELATED_CHARS: usize = 3_000;
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// A finished process: exit code (127 not found, 124 timed out), stdout and stderr decoded leniently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `argv` in `cwd` with a timeout in seconds. The host's process supervision implements it; tests script it.
pub trait Runner {
    fn run(&self, argv: &[String], cwd: &str, timeout_s: u64) -> Output;
}

/// Runs real processes with pagers, prompts and colour off, stdin closed, and `env` added.
#[derive(Debug, Clone, Default)]
pub struct ProcessRunner {
    pub env: Vec<(String, String)>,
}

fn drain(mut source: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = source.read_to_end(&mut out);
        out
    })
}

impl Runner for ProcessRunner {
    fn run(&self, argv: &[String], cwd: &str, timeout_s: u64) -> Output {
        let program = argv.first().map_or("", String::as_str);
        let mut command = Command::new(program);
        command
            .args(&argv[1.min(argv.len())..])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in [
            ("GIT_PAGER", "cat"),
            ("PAGER", "cat"),
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GH_PROMPT_DISABLED", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("NO_COLOR", "1"),
        ] {
            command.env(key, value);
        }
        command.envs(self.env.iter().map(|(key, value)| (key, value)));
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => {
                return Output {
                    code: 127,
                    stdout: String::new(),
                    stderr: format!("{program}: not found"),
                };
            }
        };
        let stdout = child.stdout.take().map(drain);
        let stderr = child.stderr.take().map(drain);
        let deadline = Instant::now() + Duration::from_secs(timeout_s);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let text = |handle: Option<std::thread::JoinHandle<Vec<u8>>>| {
            String::from_utf8_lossy(&handle.and_then(|h| h.join().ok()).unwrap_or_default()).into_owned()
        };
        let (stdout, stderr) = (text(stdout), text(stderr));
        match status {
            Some(status) => Output {
                code: status.code().unwrap_or(-1),
                stdout,
                stderr,
            },
            None => Output {
                code: 124,
                stdout: String::new(),
                stderr: format!(
                    "{}: timed out after {timeout_s}s",
                    argv.iter().take(3).cloned().collect::<Vec<_>>().join(" ")
                ),
            },
        }
    }
}

/// git in one directory.
#[derive(Clone)]
pub struct Git<'r> {
    pub cwd: String,
    pub runner: &'r dyn Runner,
}

impl<'r> Git<'r> {
    pub fn new(cwd: &str, runner: &'r dyn Runner) -> Self {
        Self {
            cwd: cwd.to_string(),
            runner,
        }
    }

    pub fn call(&self, args: &[&str]) -> Output {
        let argv: Vec<String> = std::iter::once("git")
            .chain(args.iter().copied())
            .map(String::from)
            .collect();
        self.runner.run(&argv, &self.cwd, GIT_TIMEOUT_S)
    }

    /// stdout, or the error a failed call reports.
    pub fn out(&self, args: &[&str]) -> Result<String, ReviewError> {
        let output = self.call(args);
        if output.code != 0 {
            let detail = if output.stderr.is_empty() {
                &output.stdout
            } else {
                &output.stderr
            };
            return Err(ReviewError(format!(
                "git {} failed: {}",
                args.iter().take(2).copied().collect::<Vec<_>>().join(" "),
                py::strip(detail).chars().take(300).collect::<String>()
            )));
        }
        Ok(output.stdout)
    }

    pub fn ok(&self, args: &[&str]) -> bool {
        self.call(args).code == 0
    }

    /// Trimmed stdout of a successful call, `None` when it failed or printed nothing.
    pub fn value(&self, args: &[&str]) -> Option<String> {
        let output = self.call(args);
        let text = py::strip(&output.stdout);
        (output.code == 0 && !text.is_empty()).then(|| text.to_string())
    }
}

/// Where `git grep` looks for related code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrepIn {
    /// The working tree, untracked files included.
    WorkingTree,
    /// A commit (the PR head when it is in the clone but not checked out).
    Commit(String),
    /// Nowhere: the reviewed code is not available.
    Nowhere,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    Local(PathBuf),
    Rev(String),
    Nothing,
}

/// The changes under review and how to read the code around them.
pub struct Scope<'r> {
    pub label: String,
    pub files: Vec<FileDiff>,
    pub root: String,
    pub grep: GrepIn,
    pub pr: Option<u64>,
    pub pr_url: Option<String>,
    pub notes: Vec<String>,
    git: Git<'r>,
    source: Source,
    cache: RefCell<HashMap<String, Option<Vec<String>>>>,
}

fn lines(text: &str) -> Vec<String> {
    py::splitlines(text).into_iter().map(String::from).collect()
}

impl<'r> Scope<'r> {
    fn new(git: Git<'r>, source: Source) -> Self {
        Self {
            label: String::new(),
            files: Vec::new(),
            root: git.cwd.clone(),
            grep: GrepIn::WorkingTree,
            pr: None,
            pr_url: None,
            notes: Vec::new(),
            git,
            source,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// git at the repository's top level.
    pub fn git(&self) -> &Git<'r> {
        &self.git
    }

    /// A file's lines as reviewed (working tree, or the PR head commit), `None` when not available. A path that
    /// resolves outside the repository reads as `None`. Reads are cached for the scope's life.
    pub fn read_file(&self, path: &str) -> Option<Vec<String>> {
        if let Some(cached) = self.cache.borrow().get(path) {
            return cached.clone();
        }
        let read = match &self.source {
            Source::Local(root) => std::fs::canonicalize(root.join(path))
                .ok()
                .filter(|target| target.starts_with(root))
                .and_then(|target| std::fs::read(target).ok())
                .map(|data| lines(&String::from_utf8_lossy(&data))),
            Source::Rev(rev) => {
                let output = self.git.call(&["show", &format!("{rev}:{path}")]);
                (output.code == 0).then(|| lines(&output.stdout))
            }
            Source::Nothing => None,
        };
        self.cache.borrow_mut().insert(path.to_string(), read.clone());
        read
    }
}

fn local(root: &str) -> Source {
    Source::Local(std::fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root)))
}

/// `refs/remotes/origin/HEAD`, else the first of main, master, trunk, develop (local, then origin) that exists.
pub fn default_branch(git: &Git<'_>) -> Option<String> {
    if let Some(head) = git.value(&["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"]) {
        return Some(head);
    }
    for name in ["main", "master", "trunk", "develop", "origin/main", "origin/master"] {
        let reference = if name.contains('/') {
            format!("refs/remotes/{name}")
        } else {
            format!("refs/heads/{name}")
        };
        if git.ok(&["rev-parse", "--verify", "--quiet", &reference]) {
            return Some(name.to_string());
        }
    }
    None
}

/// Untracked files under `paths` as added files (the first [`MAX_UNTRACKED_FILES`]), and notes for the rest.
pub fn untracked(git: &Git<'_>, cwd: &str, paths: &[String]) -> (Vec<FileDiff>, Vec<String>) {
    let mut args = vec!["ls-files", "--others", "--exclude-standard", "-z", "--"];
    args.extend(paths.iter().map(String::as_str));
    let output = git.call(&args);
    if output.code != 0 {
        return (Vec::new(), Vec::new());
    }
    let names: Vec<&str> = output.stdout.split('\0').filter(|name| !name.is_empty()).collect();
    let (mut files, mut notes) = (Vec::new(), Vec::new());
    for name in names.iter().take(MAX_UNTRACKED_FILES) {
        let Ok(data) = std::fs::read(Path::new(cwd).join(name)) else {
            continue;
        };
        if data.len() > MAX_UNTRACKED_BYTES || data[..data.len().min(8192)].contains(&0) {
            notes.push(format!(
                "untracked {name} (binary or over {} KB)",
                MAX_UNTRACKED_BYTES / 1000
            ));
            continue;
        }
        files.push(added_file(name, &String::from_utf8_lossy(&data)));
    }
    if names.len() > MAX_UNTRACKED_FILES {
        notes.push(format!(
            "{} more untracked files (only the first {MAX_UNTRACKED_FILES} were read)",
            names.len() - MAX_UNTRACKED_FILES
        ));
    }
    (files, notes)
}

/// Whether `path` is one of `prefixes` or below one (every path when there are none).
pub fn within(path: &str, prefixes: &[String]) -> bool {
    prefixes.is_empty()
        || prefixes.iter().any(|prefix| {
            let prefix = prefix.trim_end_matches('/');
            path == prefix || path.starts_with(&format!("{prefix}/"))
        })
}

/// `posixpath.normpath`.
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let initial = if path.starts_with("//") && !path.starts_with("///") {
        2
    } else if path.starts_with('/') {
        1
    } else {
        0
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part != ".." || (initial == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
            parts.push(part);
        } else if !parts.is_empty() {
            parts.pop();
        }
    }
    let joined = format!("{}{}", "/".repeat(initial), parts.join("/"));
    if joined.is_empty() { ".".to_string() } else { joined }
}

/// `posixpath.join(a, b)` for two parts.
pub fn join(a: &str, b: &str) -> String {
    if b.starts_with('/') {
        b.to_string()
    } else if a.is_empty() || a.ends_with('/') {
        format!("{a}{b}")
    } else {
        format!("{a}/{b}")
    }
}

fn abspath(path: &str) -> String {
    if path.starts_with('/') {
        normpath(path)
    } else {
        let here = std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default();
        normpath(&join(&here, path))
    }
}

/// `posixpath.relpath(path, start)`.
pub fn relpath(path: &str, start: &str) -> String {
    let (path, start) = (abspath(path), abspath(start));
    let split = |text: &str| -> Vec<String> {
        text.split('/')
            .filter(|part| !part.is_empty())
            .map(String::from)
            .collect()
    };
    let (path, start) = (split(&path), split(&start));
    let common = path.iter().zip(&start).take_while(|(a, b)| a == b).count();
    let mut parts = vec!["..".to_string(); start.len() - common];
    parts.extend(path[common..].iter().cloned());
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// Resolve what `options` asks to review, from `cwd`. `which` finds a program on PATH (gh for pull requests).
pub fn resolve_scope<'r>(
    options: &Options,
    cwd: &str,
    runner: &'r dyn Runner,
    which: impl Fn(&str) -> Option<String>,
) -> Result<Scope<'r>, ReviewError> {
    let here = Git::new(cwd, runner);
    if !here.ok(&["rev-parse", "--git-dir"]) {
        return Err(ReviewError(format!(
            "{cwd} is not inside a git repository, so there is no diff to review"
        )));
    }
    let top = here
        .value(&["rev-parse", "--show-toplevel"])
        .unwrap_or_else(|| cwd.to_string());
    let top = top.as_str();
    let git = Git::new(top, runner);
    let prefix = if abspath(cwd) != abspath(top) {
        relpath(cwd, top)
    } else {
        String::new()
    };
    let exists = |token: &str| Path::new(cwd).join(token).exists();
    let is_ref = |token: &str| {
        !token.starts_with('-') && git.ok(&["rev-parse", "--verify", "--quiet", &format!("{token}^{{commit}}")])
    };
    let target = classify_targets(options, exists, is_ref)?;
    // Paths are given relative to cwd; git diff paths are relative to the top level.
    let mut paths: Vec<String> = target
        .paths
        .iter()
        .map(|path| {
            let path = if prefix.is_empty() {
                normpath(path)
            } else {
                normpath(&join(&prefix, path))
            };
            if path == "." { String::new() } else { path }
        })
        .collect();
    if paths.iter().any(String::is_empty) {
        paths.clear();
    }
    let place = if target.paths.is_empty() {
        String::new()
    } else {
        format!(" in {}", target.paths.join(", "))
    };

    if let Some(pr) = target.pr {
        if which("gh").is_none() {
            return Err(ReviewError(
                "reviewing a pull request needs the GitHub CLI (gh), which is not installed".to_string(),
            ));
        }
        let argv = |args: &[&str]| -> Vec<String> { args.iter().map(|arg| arg.to_string()).collect() };
        let number = pr.to_string();
        let diff = runner.run(
            &argv(&["gh", "pr", "diff", &number, "--color=never"]),
            top,
            GH_TIMEOUT_S,
        );
        if diff.code != 0 {
            let detail = if diff.stderr.is_empty() {
                &diff.stdout
            } else {
                &diff.stderr
            };
            return Err(ReviewError(format!(
                "gh pr diff {pr} failed: {}",
                py::strip(detail).chars().take(300).collect::<String>()
            )));
        }
        let view = runner.run(
            &argv(&[
                "gh",
                "pr",
                "view",
                &number,
                "--json",
                "number,title,url,headRefOid,baseRefName",
            ]),
            top,
            GH_TIMEOUT_S,
        );
        let meta = if view.code == 0 {
            serde_json::from_str::<Value>(&view.stdout).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let text = |key: &str| meta.get(key).and_then(Value::as_str).filter(|text| !text.is_empty());
        let files = parse_diff(&diff.stdout)
            .into_iter()
            .filter(|item| within(&item.path, &paths))
            .collect();
        let mut notes = Vec::new();
        let head = text("headRefOid");
        let (source, grep) = match head {
            Some(head) if git.value(&["rev-parse", "HEAD"]).as_deref() == Some(head) => {
                (local(top), GrepIn::WorkingTree)
            }
            Some(head) if git.ok(&["cat-file", "-e", &format!("{head}^{{commit}}")]) => {
                (Source::Rev(head.to_string()), GrepIn::Commit(head.to_string()))
            }
            _ => {
                notes.push(
                    "code around the diff and callers (the PR head commit is not in this clone; fetch it or check out \
                     the PR branch to include them)"
                        .to_string(),
                );
                (Source::Nothing, GrepIn::Nowhere)
            }
        };
        let title = text("title").map(|title| format!(": {title}")).unwrap_or_default();
        let mut scope = Scope::new(git, source);
        scope.label = format!("PR #{pr}{title}{place}");
        scope.files = files;
        scope.grep = grep;
        scope.pr = Some(pr);
        scope.pr_url = text("url").map(String::from);
        scope.notes = notes;
        return Ok(scope);
    }

    let has_head = git.ok(&["rev-parse", "--verify", "--quiet", "HEAD"]);
    let branch = git
        .value(&["branch", "--show-current"])
        .unwrap_or_else(|| "HEAD".to_string());
    let (base, label) = if let Some(given) = &target.base {
        let merge_base = if has_head {
            git.value(&["merge-base", given, "HEAD"])
        } else {
            None
        };
        let base = match merge_base {
            Some(base) => base,
            None => py::strip(&git.out(&["rev-parse", "--verify", &format!("{given}^{{commit}}")])?).to_string(),
        };
        let short: String = base.chars().take(8).collect();
        let label = format!("changes since {given} (merge base {short}), including uncommitted{place}");
        (base, label)
    } else if !has_head {
        (
            EMPTY_TREE.to_string(),
            format!("all files (the repository has no commits yet){place}"),
        )
    } else {
        let default = default_branch(&git);
        let merge_base = default
            .as_deref()
            .and_then(|default| git.value(&["merge-base", default, "HEAD"]));
        let head = git.value(&["rev-parse", "HEAD"]);
        match merge_base {
            Some(merge_base) if Some(&merge_base) != head.as_ref() => {
                let short: String = merge_base.chars().take(8).collect();
                let label = format!(
                    "branch {branch} vs {} (merge base {short}) plus uncommitted changes{place}",
                    default.unwrap_or_default()
                );
                (merge_base, label)
            }
            _ => ("HEAD".to_string(), format!("uncommitted changes on {branch}{place}")),
        }
    };
    let mut args = vec![
        "-c",
        "diff.noprefix=false",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "-M",
        "-U3",
        &base,
        "--",
    ];
    args.extend(paths.iter().map(String::as_str));
    let diff = git.out(&args)?;
    let mut files = parse_diff(&diff);
    let (more, notes) = untracked(&git, top, &paths);
    files.extend(more);
    let mut scope = Scope::new(git, local(top));
    scope.label = label;
    scope.files = files;
    scope.notes = notes.into_iter().map(|note| format!("skipped {note}")).collect();
    Ok(scope)
}

/// Where the names around a finding are defined or used elsewhere (`git grep`), bounded to [`RELATED_CHARS`].
pub fn related_code(scope: &Scope<'_>, file: &str, line: i64, source: Option<&[String]>) -> String {
    let rev = match &scope.grep {
        GrepIn::Nowhere => return String::new(),
        GrepIn::Commit(rev) => Some(rev.as_str()),
        GrepIn::WorkingTree => None,
    };
    let mut lines: Vec<String> = Vec::new();
    let mut size = 0;
    for name in symbols_for(source, line, 3) {
        let mut args = vec!["grep", "-n", "-w", "-I", "--max-count=4", "-e", &name];
        args.push(rev.unwrap_or("--untracked"));
        args.extend(["--", ".", ":(exclude)*.lock", ":(exclude)*.min.js"]);
        let output = scope.git.call(&args);
        if output.code != 0 {
            continue;
        }
        for hit in py::splitlines(&output.stdout).into_iter().take(8) {
            let hit = match rev {
                Some(rev) => hit.strip_prefix(&format!("{rev}:")).unwrap_or(hit),
                None => hit,
            };
            let (path, rest) = hit.split_once(':').unwrap_or((hit, ""));
            let (number, text) = rest.split_once(':').unwrap_or((rest, ""));
            let numeric = !number.is_empty() && number.chars().all(|c| c.is_ascii_digit());
            if path == file
                && numeric
                && number
                    .parse::<i64>()
                    .is_ok_and(|number| (number - line).abs() <= VERIFY_WINDOW)
            {
                continue;
            }
            let entry = format!("{path}:{number}: {}", py::clip(py::strip(text), 200));
            let length = entry.chars().count();
            if size + length > RELATED_CHARS {
                break;
            }
            lines.push(entry);
            size += length;
        }
    }
    lines.join("\n")
}
