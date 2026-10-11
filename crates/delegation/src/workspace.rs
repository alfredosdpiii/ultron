//! Port of `workspace-snapshot.ts` and `checkFiles` of `verdict.ts`: cheap before/after pictures of a working
//! directory (one metadata signature per file: size, mtime and ctime in nanoseconds, mode, inode; never contents),
//! and the check of a subagent verdict's declared `changed_files` against what changed while it ran.
//!
//! Inside a Git working tree the file list is `git ls-files --cached --others --exclude-standard` (ignored output is
//! never listed, deleted tracked files still are); outside one the directory is walked without following links,
//! skipping dependency and cache directories. Both are bounded; a snapshot that hit the bound is `complete: false`.

use crate::verdict::workspace_path;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const DEFAULT_MAX_SNAPSHOT_ENTRIES: usize = 200_000;
pub const DEFAULT_WALK_IGNORE: [&str; 12] = [
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
    ".cache",
];
const LISTED: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub root: PathBuf,
    /// "git" or "walk".
    pub method: &'static str,
    /// Relative path (with `/`) -> metadata signature.
    pub entries: BTreeMap<String, String>,
    pub complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WorkspaceDiff {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    /// Every changed path, sorted.
    pub changed: Vec<String>,
    pub complete: bool,
}

/// One file's signature; `None` when it does not exist.
fn signature(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)
            .map(|t| t.display().to_string())
            .unwrap_or_default();
        return Some(format!("l:{target}"));
    }
    if meta.is_dir() {
        return Some("d".into());
    }
    let ns = |s: i64, n: i64| i128::from(s) * 1_000_000_000 + i128::from(n);
    Some(format!(
        "f:{}:{}:{}:{}:{}",
        meta.len(),
        ns(meta.mtime(), meta.mtime_nsec()),
        ns(meta.ctime(), meta.ctime_nsec()),
        meta.mode(),
        meta.ino()
    ))
}

fn git_files(root: &Path) -> Option<Vec<String>> {
    let output = Command::new("git")
        .args([
            "-c",
            "core.quotepath=off",
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut files: Vec<String> = Vec::new();
    for item in String::from_utf8_lossy(&output.stdout).split('\0') {
        // A nested repository is listed as its directory ("sub/"); it is recorded as one entry.
        let path = item.strip_suffix('/').unwrap_or(item);
        if !path.is_empty() && !files.iter().any(|f| f == path) {
            files.push(path.to_string());
        }
    }
    Some(files)
}

fn walk_files(root: &Path, ignore: &[&str], limit: usize) -> (Vec<String>, bool) {
    let mut files = Vec::new();
    let mut pending = vec![String::new()];
    while let Some(directory) = pending.pop() {
        let dir = if directory.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&directory)
        };
        let Ok(items) = std::fs::read_dir(&dir) else {
            continue;
        };
        for item in items.filter_map(|i| i.ok()) {
            let name = item.file_name().to_string_lossy().into_owned();
            if ignore.contains(&name.as_str()) {
                continue;
            }
            let path = if directory.is_empty() {
                name
            } else {
                format!("{directory}/{name}")
            };
            if item.file_type().is_ok_and(|t| t.is_dir()) {
                pending.push(path);
                continue;
            }
            if files.len() >= limit {
                return (files, false);
            }
            files.push(path);
        }
    }
    (files, true)
}

/// A metadata snapshot of the files under `cwd`; `git: false` always walks.
pub fn snapshot_workspace(cwd: &Path, max_entries: usize, git: bool) -> WorkspaceSnapshot {
    let root = cwd.to_path_buf();
    let listed = if git { git_files(&root) } else { None };
    let (method, files, complete) = match listed {
        Some(listed) => {
            let complete = listed.len() <= max_entries;
            (
                "git",
                listed.into_iter().take(max_entries).collect::<Vec<_>>(),
                complete,
            )
        }
        None => {
            let (files, complete) = walk_files(&root, &DEFAULT_WALK_IGNORE, max_entries);
            ("walk", files, complete)
        }
    };
    let entries = files
        .into_iter()
        .filter_map(|path| signature(&root.join(&path)).map(|s| (path, s)))
        .collect();
    WorkspaceSnapshot {
        root,
        method,
        entries,
        complete,
    }
}

/// What changed between two snapshots of the same directory.
pub fn diff_snapshots(before: &WorkspaceSnapshot, after: &WorkspaceSnapshot) -> Result<WorkspaceDiff, String> {
    if before.root != after.root {
        return Err("Snapshots describe different directories".into());
    }
    let mut diff = WorkspaceDiff {
        complete: before.complete && after.complete,
        ..Default::default()
    };
    for (path, value) in &after.entries {
        match before.entries.get(path) {
            None => diff.added.push(path.clone()),
            Some(previous) if previous != value => diff.modified.push(path.clone()),
            Some(_) => {}
        }
    }
    diff.deleted = before
        .entries
        .keys()
        .filter(|p| !after.entries.contains_key(*p))
        .cloned()
        .collect();
    diff.changed = [diff.added.clone(), diff.modified.clone(), diff.deleted.clone()].concat();
    diff.changed.sort();
    Ok(diff)
}

/// Whether `declared` names `changed` itself or a directory holding it.
fn covers(declared: &str, changed: &str) -> bool {
    declared == changed || changed.starts_with(&format!("{declared}/"))
}

fn listed(snapshot: &WorkspaceSnapshot, path: &str) -> bool {
    snapshot.entries.contains_key(path) || snapshot.entries.keys().any(|e| e.starts_with(&format!("{path}/")))
}

/// `checkFiles`: compares a verdict's declared changes (`changed_files`) with the changes observed during the run.
/// `explained`: paths declared by overlapping tasks; `concurrent`: their ids.
pub fn check_files(
    changed_files: &[String],
    workspace: &Path,
    before: &WorkspaceSnapshot,
    after: &WorkspaceSnapshot,
    diff: &WorkspaceDiff,
    explained: &[String],
    concurrent: &[String],
) -> Value {
    let root = workspace.display().to_string();
    let mut unobserved = Vec::new();
    let mut unlisted = Vec::new();
    for declared in changed_files {
        let Some(path) = workspace_path(declared, Some(&root)) else {
            unlisted.push(declared.clone());
            continue;
        };
        if diff.changed.iter().any(|changed| covers(&path, changed)) {
            continue;
        }
        if listed(before, &path) || listed(after, &path) {
            unobserved.push(declared.clone());
        } else if std::fs::symlink_metadata(workspace.join(&path)).is_ok() {
            // Neither snapshot lists it: an ignored file is not checked.
            unlisted.push(declared.clone());
        } else {
            unobserved.push(declared.clone());
        }
    }
    let declared: Vec<String> = changed_files
        .iter()
        .map(|p| workspace_path(p, Some(&root)).unwrap_or_else(|| p.clone()))
        .collect();
    let unreported: Vec<String> = diff
        .changed
        .iter()
        .filter(|path| !declared.iter().any(|d| covers(d, path)) && !explained.iter().any(|e| covers(e, path)))
        .cloned()
        .collect();
    let outcome = if !unobserved.is_empty() {
        "contradicted"
    } else if diff.complete {
        "verified"
    } else {
        "unchecked"
    };
    let take = |items: &[String]| items.iter().take(LISTED).cloned().collect::<Vec<_>>();
    let mut check = json!({
        "outcome": outcome,
        "unobserved": take(&unobserved),
        "unreported": take(&unreported),
        "unlisted": take(&unlisted),
        "concurrent": take(concurrent),
    });
    if outcome == "unchecked" {
        check["reason"] = json!("the workspace has more files than a snapshot records");
    }
    if unreported.len() > LISTED {
        check["unreportedCount"] = json!(unreported.len());
    }
    check
}
