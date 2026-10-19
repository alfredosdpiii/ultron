//! A spawn contract's workspace baseline that a restart keeps (wave 41). The metadata snapshot a child's start
//! takes ([`crate::workspace`]) lives in memory only, so `changed_files_within` / `no_new_dependencies` of a child
//! reattached by the next worker had nothing to compare against. When the child's working directory is in a Git
//! work tree, its start also records a small baseline kept with the contract in the session: the `HEAD` commit, a
//! digest of `git status --porcelain` (the blob id of its output) and, for each path dirty then, the blob id of its
//! content. After a restart the change set is `git diff --name-only <commit>` plus the untracked files, minus the
//! paths that were already dirty at the start and still have the same content, plus the dirty ones since restored.
//!
//! Git runs in the working directory with the session's environment (its PATH finds git), never a remote: only
//! `rev-parse`, `status`, `diff`, `ls-files`, `hash-object` and `cat-file` against the local repository.

use crate::workspace::WorkspaceDiff;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// Most paths dirty at the start that a baseline records; a busier tree keeps none (the checks then fail closed).
pub const MAX_DIRTY: usize = 256;
/// The PATH git is looked up on when the session environment has none.
const FALLBACK_PATH: &str = "/usr/local/bin:/usr/bin:/bin";
/// Variables of the session environment that would point git at another repository than the working directory's.
const REDIRECTS: [&str; 6] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
];
/// The content id of a path that does not exist (or is not a file).
const ABSENT: &str = "-";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitBaseline {
    /// `HEAD` when the child started.
    pub commit: String,
    /// The blob id of `git status --porcelain -z -uall --no-renames -- .` then.
    pub status: String,
    /// Each path (relative to the working directory) dirty then, with the blob id of its content ([`ABSENT`]).
    pub dirty: BTreeMap<String, String>,
}

/// Runs git in `cwd` with `env` (cleared first); `None` when it cannot start or fails.
fn git(cwd: &Path, env: &[(String, String)], args: &[&str], input: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.quotepath=off", "-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(
            env.iter()
                .filter(|(key, _)| !REDIRECTS.contains(&key.as_str()))
                .map(|(k, v)| (k, v)),
        )
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if !env.iter().any(|(key, _)| key == "PATH") {
        command.env("PATH", FALLBACK_PATH);
    }
    let mut child = command.spawn().ok()?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        let _ = stdin.write_all(input);
    }
    let output = child.wait_with_output().ok()?;
    output.status.success().then_some(output.stdout)
}

fn text(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).trim().to_string()
}

fn nul_list(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split('\0')
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

/// The blob ids of `paths` (relative to `cwd`) as they are now; [`ABSENT`] for one that is not a file.
fn contents(cwd: &Path, env: &[(String, String)], paths: &[&String]) -> Option<BTreeMap<String, String>> {
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let files: Vec<&String> = paths
        .iter()
        .copied()
        .filter(|path| std::fs::metadata(cwd.join(path)).is_ok_and(|m| m.is_file()))
        .collect();
    if !files.is_empty() {
        // Absolute: git reads `--stdin-paths` relative to the repository's top, not the working directory.
        let list: String = files
            .iter()
            .map(|path| format!("{}\n", cwd.join(path).display()))
            .collect();
        let out = git(
            cwd,
            env,
            &["hash-object", "--no-filters", "--stdin-paths"],
            Some(list.as_bytes()),
        )?;
        let hashes: Vec<String> = String::from_utf8_lossy(&out).lines().map(String::from).collect();
        if hashes.len() != files.len() {
            return None;
        }
        ids.extend(files.into_iter().cloned().zip(hashes));
    }
    for path in paths {
        ids.entry((*path).clone()).or_insert_with(|| ABSENT.to_string());
    }
    Some(ids)
}

/// The status of the working directory: its porcelain bytes and the paths it names, relative to `cwd`.
fn status(cwd: &Path, env: &[(String, String)]) -> Option<(Vec<u8>, Vec<String>)> {
    let prefix = text(git(cwd, env, &["rev-parse", "--show-prefix"], None)?);
    let raw = git(
        cwd,
        env,
        &["status", "--porcelain", "-z", "-uall", "--no-renames", "--", "."],
        None,
    )?;
    let paths = nul_list(&raw)
        .into_iter()
        .filter_map(|entry| entry.get(3..).map(String::from))
        // Porcelain paths are relative to the repository's top; the pathspec kept those under `cwd`.
        .map(|path| path.strip_prefix(&prefix).map(String::from).unwrap_or(path))
        .map(|path| path.strip_suffix('/').map(String::from).unwrap_or(path))
        .collect();
    Some((raw, paths))
}

impl GitBaseline {
    /// The baseline of `cwd` now; `None` outside a Git work tree, before its first commit, when git is missing, or
    /// when more than [`MAX_DIRTY`] paths are dirty.
    pub fn capture(cwd: &Path, env: &[(String, String)]) -> Option<GitBaseline> {
        let commit = text(git(cwd, env, &["rev-parse", "--verify", "-q", "HEAD^{commit}"], None)?);
        if commit.is_empty() {
            return None;
        }
        let (raw, paths) = status(cwd, env)?;
        if paths.len() > MAX_DIRTY {
            return None;
        }
        let digest = text(git(cwd, env, &["hash-object", "--stdin"], Some(&raw))?);
        let dirty = contents(cwd, env, &paths.iter().collect::<Vec<_>>())?;
        Some(GitBaseline {
            commit,
            status: digest,
            dirty,
        })
    }

    /// What changed in `cwd` since the baseline, as a [`WorkspaceDiff`] (`changed` sorted); an error when Git can
    /// no longer answer (the commit is gone, git is missing).
    pub fn changes(&self, cwd: &Path, env: &[(String, String)]) -> Result<WorkspaceDiff, String> {
        let unknown = |what: &str| format!("git could not list the changes since the child started: {what}");
        let object = format!("{}^{{commit}}", self.commit);
        git(cwd, env, &["cat-file", "-e", &object], None)
            .ok_or_else(|| unknown(&format!("commit {} is not in the repository", self.commit)))?;
        // Unchanged since the start: the same commit and the same status, every dirty file as it was.
        let head = git(cwd, env, &["rev-parse", "--verify", "-q", "HEAD^{commit}"], None).map(text);
        if head.as_deref() == Some(self.commit.as_str()) && self.dirty.is_empty() {
            let (raw, _) = status(cwd, env).ok_or_else(|| unknown("git status failed"))?;
            let digest = git(cwd, env, &["hash-object", "--stdin"], Some(&raw)).map(text);
            if digest.as_deref() == Some(self.status.as_str()) {
                return Ok(WorkspaceDiff {
                    complete: true,
                    ..WorkspaceDiff::default()
                });
            }
        }
        let tracked = git(
            cwd,
            env,
            &[
                "diff",
                "--name-only",
                "-z",
                "--no-renames",
                "--relative",
                &self.commit,
                "--",
            ],
            None,
        )
        .ok_or_else(|| unknown("git diff failed"))?;
        let untracked = git(cwd, env, &["ls-files", "-z", "--others", "--exclude-standard"], None)
            .ok_or_else(|| unknown("git ls-files failed"))?;
        let listed: BTreeSet<String> = nul_list(&tracked).into_iter().chain(nul_list(&untracked)).collect();
        // Dirty at the start: changed only when its content differs now (or it is back to the commit's).
        let dirty: Vec<&String> = self.dirty.keys().collect();
        let now = contents(cwd, env, &dirty).ok_or_else(|| unknown("git hash-object failed"))?;
        let mut changed: BTreeSet<String> = listed
            .iter()
            .filter(|path| {
                self.dirty
                    .get(*path)
                    .is_none_or(|before| now.get(*path) != Some(before))
            })
            .cloned()
            .collect();
        changed.extend(self.dirty.keys().filter(|path| !listed.contains(*path)).cloned());
        let changed: Vec<String> = changed.into_iter().collect();
        Ok(WorkspaceDiff {
            modified: changed.clone(),
            changed,
            complete: true,
            ..WorkspaceDiff::default()
        })
    }

    /// The baseline as the session keeps it.
    pub fn describe(&self) -> Value {
        json!({ "commit": self.commit, "status": self.status, "dirty": self.dirty })
    }

    /// A kept baseline; `None` when `value` is not one.
    pub fn from_value(value: &Value) -> Option<GitBaseline> {
        let map: &Map<String, Value> = value.as_object()?;
        let dirty = map
            .get("dirty")?
            .as_object()?
            .iter()
            .map(|(path, id)| id.as_str().map(|id| (path.clone(), id.to_string())))
            .collect::<Option<BTreeMap<_, _>>>()?;
        Some(GitBaseline {
            commit: map.get("commit")?.as_str().filter(|c| !c.is_empty())?.to_string(),
            status: map.get("status")?.as_str()?.to_string(),
            dirty,
        })
    }
}
