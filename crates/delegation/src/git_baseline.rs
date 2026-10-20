//! A spawn contract's workspace baseline that a restart keeps (wave 41). The metadata snapshot a child's start
//! takes ([`crate::workspace`]) lives in memory only, so `changed_files_within` / `no_new_dependencies` of a child
//! reattached by the next worker had nothing to compare against. When the child's working directory is in a Git
//! work tree, its start also records a small baseline kept with the contract in the session: the `HEAD` commit and
//! the id of one Git tree holding the work tree as it was then (wave 43): a copy of the repository's index in a
//! temporary file (`GIT_INDEX_FILE`), `git add -A` into it (tracked edits and deletions, untracked files, never
//! ignored ones: what `git status` names), then `git write-tree`. The real index and the work tree are never
//! touched; only the objects of the new tree land in the repository's object store. After a restart the same is
//! done again and the change set is `git diff --name-only <then> <now>` under the working directory: any number of
//! dirty paths, one bounded value in the session.
//!
//! Baselines recorded before (`{"commit", "status", "dirty": {path: blob id}}`, at most [`MAX_DIRTY`] paths) are
//! still read and compared as they were: `git diff --name-only <commit>` plus the untracked files, minus the paths
//! already dirty at the start with the same content, plus the dirty ones since restored.
//!
//! Git runs in the working directory with the session's environment (its PATH finds git), never a remote: only
//! `rev-parse`, `status`, `diff`, `ls-files`, `hash-object`, `cat-file`, `read-tree`, `add` and `write-tree`
//! against the local repository.

use crate::workspace::WorkspaceDiff;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// Most paths dirty at the start that a baseline of the earlier format ([`DirtyState::Paths`]) records; a busier
/// tree kept none (the checks then failed closed). The tree format has no such limit.
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

/// The work tree's state when the child started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirtyState {
    /// The id of a Git tree holding every path of the work tree as it was (wave 43).
    Tree(String),
    /// The earlier format (wave 41): the blob id of `git status --porcelain -z -uall --no-renames -- .` and each
    /// path (relative to the working directory) dirty then, with the blob id of its content ([`ABSENT`]).
    Paths {
        status: String,
        dirty: BTreeMap<String, String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitBaseline {
    /// `HEAD` when the child started.
    pub commit: String,
    pub state: DirtyState,
}

/// Runs git in `cwd` with `env` (cleared first), on the index file `index` when given; `None` when it cannot start
/// or fails.
fn git_on(
    cwd: &Path,
    env: &[(String, String)],
    index: Option<&Path>,
    args: &[&str],
    input: Option<&[u8]>,
) -> Option<Vec<u8>> {
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
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
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

fn git(cwd: &Path, env: &[(String, String)], args: &[&str], input: Option<&[u8]>) -> Option<Vec<u8>> {
    git_on(cwd, env, None, args, input)
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

/// A private directory for one temporary index, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Option<Scratch> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let name = format!(
            "ultron-git-baseline-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(name);
        // `create_dir` fails on an existing path, so nothing planted there is ever used.
        std::fs::create_dir(&dir).ok()?;
        Some(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The id of a Git tree holding the work tree as it is now (every path; ignored files never), built in a temporary
/// copy of the index so the real one is never written.
fn work_tree(cwd: &Path, env: &[(String, String)]) -> Option<String> {
    let scratch = Scratch::new()?;
    let index = scratch.0.join("index");
    let real = text(git(cwd, env, &["rev-parse", "--git-path", "index"], None)?);
    let real = cwd.join(real);
    // The real index's stat data spares hashing every unchanged file; without one, start from HEAD.
    let seeded = real.is_file() && std::fs::copy(&real, &index).is_ok();
    if !seeded {
        git_on(cwd, env, Some(&index), &["read-tree", "HEAD"], None)?;
    }
    // The whole tree (`:/`): unmerged or intent-to-add entries anywhere would stop write-tree.
    git_on(cwd, env, Some(&index), &["add", "-A", "--", ":/"], None)?;
    let tree = text(git_on(cwd, env, Some(&index), &["write-tree"], None)?);
    (!tree.is_empty()).then_some(tree)
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

fn head(cwd: &Path, env: &[(String, String)]) -> Option<String> {
    let commit = text(git(cwd, env, &["rev-parse", "--verify", "-q", "HEAD^{commit}"], None)?);
    (!commit.is_empty()).then_some(commit)
}

impl GitBaseline {
    /// The baseline of `cwd` now; `None` outside a Git work tree, before its first commit, or when git is missing
    /// or cannot write the tree.
    pub fn capture(cwd: &Path, env: &[(String, String)]) -> Option<GitBaseline> {
        let commit = head(cwd, env)?;
        let tree = work_tree(cwd, env)?;
        Some(GitBaseline {
            commit,
            state: DirtyState::Tree(tree),
        })
    }

    /// A baseline in the earlier format ([`DirtyState::Paths`]) as wave 41 recorded it; `None` also when more than
    /// [`MAX_DIRTY`] paths are dirty. Kept so baselines of that format are compared as they always were.
    pub fn capture_paths(cwd: &Path, env: &[(String, String)]) -> Option<GitBaseline> {
        let commit = head(cwd, env)?;
        let (raw, paths) = status(cwd, env)?;
        if paths.len() > MAX_DIRTY {
            return None;
        }
        let digest = text(git(cwd, env, &["hash-object", "--stdin"], Some(&raw))?);
        let dirty = contents(cwd, env, &paths.iter().collect::<Vec<_>>())?;
        Some(GitBaseline {
            commit,
            state: DirtyState::Paths { status: digest, dirty },
        })
    }

    /// What changed in `cwd` since the baseline, as a [`WorkspaceDiff`] (`changed` sorted); an error when Git can
    /// no longer answer (the commit or tree is gone, git is missing).
    pub fn changes(&self, cwd: &Path, env: &[(String, String)]) -> Result<WorkspaceDiff, String> {
        let unknown = |what: &str| format!("git could not list the changes since the child started: {what}");
        let object = format!("{}^{{commit}}", self.commit);
        git(cwd, env, &["cat-file", "-e", &object], None)
            .ok_or_else(|| unknown(&format!("commit {} is not in the repository", self.commit)))?;
        let changed = match &self.state {
            DirtyState::Tree(tree) => {
                let object = format!("{tree}^{{tree}}");
                git(cwd, env, &["cat-file", "-e", &object], None)
                    .ok_or_else(|| unknown(&format!("tree {tree} is not in the repository")))?;
                let now = work_tree(cwd, env).ok_or_else(|| unknown("git could not record the work tree"))?;
                if &now == tree {
                    Vec::new()
                } else {
                    let out = git(
                        cwd,
                        env,
                        &[
                            "diff",
                            "--name-only",
                            "-z",
                            "--no-renames",
                            "--relative",
                            tree,
                            &now,
                            "--",
                        ],
                        None,
                    )
                    .ok_or_else(|| unknown("git diff failed"))?;
                    nul_list(&out)
                        .into_iter()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect()
                }
            }
            DirtyState::Paths { status: digest, dirty } => self.changed_paths(cwd, env, digest, dirty, &unknown)?,
        };
        Ok(WorkspaceDiff {
            modified: changed.clone(),
            changed,
            complete: true,
            ..WorkspaceDiff::default()
        })
    }

    /// [`GitBaseline::changes`] of a baseline in the earlier format.
    fn changed_paths(
        &self,
        cwd: &Path,
        env: &[(String, String)],
        digest: &str,
        dirty: &BTreeMap<String, String>,
        unknown: &dyn Fn(&str) -> String,
    ) -> Result<Vec<String>, String> {
        // Unchanged since the start: the same commit and the same status, every dirty file as it was.
        if head(cwd, env).as_deref() == Some(self.commit.as_str()) && dirty.is_empty() {
            let (raw, _) = status(cwd, env).ok_or_else(|| unknown("git status failed"))?;
            let now = git(cwd, env, &["hash-object", "--stdin"], Some(&raw)).map(text);
            if now.as_deref() == Some(digest) {
                return Ok(Vec::new());
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
        let paths: Vec<&String> = dirty.keys().collect();
        let now = contents(cwd, env, &paths).ok_or_else(|| unknown("git hash-object failed"))?;
        let mut changed: BTreeSet<String> = listed
            .iter()
            .filter(|path| dirty.get(*path).is_none_or(|before| now.get(*path) != Some(before)))
            .cloned()
            .collect();
        changed.extend(dirty.keys().filter(|path| !listed.contains(*path)).cloned());
        Ok(changed.into_iter().collect())
    }

    /// The baseline as the session keeps it: `{"commit", "tree"}` (earlier: `{"commit", "status", "dirty"}`).
    pub fn describe(&self) -> Value {
        match &self.state {
            DirtyState::Tree(tree) => json!({ "commit": self.commit, "tree": tree }),
            DirtyState::Paths { status, dirty } => {
                json!({ "commit": self.commit, "status": status, "dirty": dirty })
            }
        }
    }

    /// A kept baseline, of either format; `None` when `value` is not one.
    pub fn from_value(value: &Value) -> Option<GitBaseline> {
        let map: &Map<String, Value> = value.as_object()?;
        let commit = map.get("commit")?.as_str().filter(|c| !c.is_empty())?.to_string();
        let state = match map.get("tree") {
            Some(tree) => DirtyState::Tree(tree.as_str().filter(|t| !t.is_empty())?.to_string()),
            None => DirtyState::Paths {
                status: map.get("status")?.as_str()?.to_string(),
                dirty: map
                    .get("dirty")?
                    .as_object()?
                    .iter()
                    .map(|(path, id)| id.as_str().map(|id| (path.clone(), id.to_string())))
                    .collect::<Option<BTreeMap<_, _>>>()?,
            },
        };
        Some(GitBaseline { commit, state })
    }
}
