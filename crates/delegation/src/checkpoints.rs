//! Per-turn working-tree checkpoints (Rust-only feature; the design follows reflection-oss/mirror
//! `mirror-agent/src/checkpoints.rs`): the working tree pictured as an unreferenced commit, made from a copy of the
//! index exactly as a worktree child's base is ([`super::snapshot_working_tree`]'s method: `add -A` into a temporary
//! index, `write-tree`, `commit-tree` on top of HEAD), so HEAD, branches, the real index and the files stay as they
//! were. Nothing references the commits: Git's garbage collection may prune old ones (`gc.pruneExpire`, two weeks
//! by default).
//!
//! In a Jujutsu repository (a `.jj` directory above the working directory) with `jj` on the session's PATH, jj's own
//! snapshot is recorded instead (`--no-integrate-operation`: no operation, bookmark or checkout changes).
//!
//! Bounded: a tree of more than [`Limits::max_files`] listed files is not recorded (a note instead), and untracked
//! files larger than [`Limits::max_file_bytes`] are left out of the checkpoint (`excluded`).
//!
//! [`restore`] writes a checkpoint's files back into the working tree after recording the current files as another
//! checkpoint (so the restore is undoable), and refuses to overwrite a file that safety checkpoint cannot hold
//! (ignored or too large). Ignored files, HEAD and the index are never changed.

use super::{
    GitOptions, TempIndex, copy_index, git_ok, git_ok_with, identity_env, opts, quiet, repo_info, temporary_index,
};
use serde_json::{Value, json};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub const DEFAULT_MAX_FILES: usize = 100_000;
pub const DEFAULT_MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// The checkpoint commits' message.
pub const MESSAGE: &str = "Ultron checkpoint";
const GITLINK: &str = "160000";
const JJ_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_files: usize,
    pub max_file_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_files: DEFAULT_MAX_FILES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }
}

/// One recorded working tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// `git` or `jj`.
    pub vcs: String,
    /// The repository's top directory.
    pub root: String,
    /// HEAD (jj: `@` before the snapshot) when it was taken; `None` in a repository without commits.
    pub head: Option<String>,
    pub commit: String,
    /// Git only: the commit's tree.
    pub tree: Option<String>,
    /// Untracked files left out for their size.
    pub excluded: Vec<String>,
}

impl Checkpoint {
    pub fn to_json(&self) -> Value {
        json!({
            "vcs": self.vcs, "root": self.root, "head": self.head, "commit": self.commit, "tree": self.tree,
            "excluded": self.excluded,
        })
    }

    pub fn from_json(value: &Value) -> Option<Checkpoint> {
        let text = |key: &str| value[key].as_str().map(str::to_string);
        Some(Checkpoint {
            vcs: text("vcs")?,
            root: text("root")?,
            head: text("head"),
            commit: text("commit")?,
            tree: text("tree"),
            excluded: value["excluded"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        })
    }
}

/// What [`capture`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Capture {
    /// A new checkpoint (its commit may be HEAD itself when nothing differs from it).
    Taken(Checkpoint),
    /// The tree is the one `previous` recorded: `previous` again, no new commit.
    Unchanged(Checkpoint),
    /// Not recorded, with the reason (a bounded tree).
    Skipped(String),
    /// Not in a Git working tree (or Git is missing).
    Outside,
}

/// The nearest directory at or above `cwd` holding a `.jj` directory.
pub fn jj_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".jj").is_dir())
        .map(Path::to_path_buf)
}

/// Record `cwd`'s working tree. `previous`: the session's last checkpoint (an unchanged tree reuses it); `jj`: the
/// `jj` executable when the session's PATH has one.
pub fn capture(
    cwd: &Path,
    previous: Option<&Checkpoint>,
    limits: &Limits,
    jj: Option<&Path>,
) -> Result<Capture, String> {
    if let (Some(jj), Some(root)) = (jj, jj_root(cwd)) {
        return jj_capture(&root, jj, previous, limits);
    }
    let info = match repo_info(cwd) {
        Ok(info) => info,
        Err(error) if error.fallback => return Ok(Capture::Outside),
        Err(error) => return Err(error.message),
    };
    let top = info.top;
    let listed = git_ok(
        &top,
        &[
            "-c",
            "core.quotepath=off",
            "ls-files",
            "-z",
            "-t",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )
    .map_err(|e| e.message)?;
    let items: Vec<&str> = listed.split('\0').filter(|item| item.len() > 2).collect();
    if items.len() > limits.max_files {
        return Ok(Capture::Skipped(format!(
            "the working tree lists {} files, more than checkpoints.maxFiles ({})",
            items.len(),
            limits.max_files
        )));
    }
    let mut excluded: Vec<String> = items
        .iter()
        .filter(|item| item.starts_with("? "))
        .map(|item| item[2..].trim_end_matches('/'))
        .filter(|path| {
            std::fs::symlink_metadata(top.join(path)).is_ok_and(|m| m.is_file() && m.size() > limits.max_file_bytes)
        })
        .map(str::to_string)
        .collect();
    excluded.sort();
    let (commit, tree) = commit_tree(&top, info.head.as_deref(), &excluded)?;
    if let Some(previous) = previous
        && previous.vcs == "git"
        && previous.root == top.display().to_string()
        && previous.tree.as_deref() == Some(tree.as_str())
    {
        return Ok(Capture::Unchanged(previous.clone()));
    }
    let commit = match commit {
        Some(commit) => commit,
        None => make_commit(&top, &tree, info.head.as_deref())?,
    };
    Ok(Capture::Taken(Checkpoint {
        vcs: "git".into(),
        root: top.display().to_string(),
        head: info.head,
        commit,
        tree: Some(tree),
        excluded,
    }))
}

/// The working tree's tree object (from a copy of the index, `excluded` left out); `Some(HEAD)` as the commit when
/// it is HEAD's tree.
fn commit_tree(top: &Path, head: Option<&str>, excluded: &[String]) -> Result<(Option<String>, String), String> {
    let error = |e: super::WorktreeError| e.message;
    let index_path = git_ok(top, &["rev-parse", "--path-format=absolute", "--git-path", "index"]).map_err(error)?;
    let temporary = TempIndex(temporary_index("checkpoint-index"));
    let index = temporary.0.display().to_string();
    let env = [("GIT_INDEX_FILE", index.as_str()), ("GIT_OPTIONAL_LOCKS", "0")];
    if copy_index(Path::new(index_path.trim()), &temporary.0).is_err()
        && let Some(head) = head
    {
        git_ok_with(top, &["read-tree", head], opts(&env)).map_err(error)?;
    }
    clear_index_flags(top, &env)?;
    let mut specs = vec![".".to_string()];
    specs.extend(excluded.iter().map(|path| format!(":(exclude,literal){path}")));
    let mut input = specs.join("\0").into_bytes();
    input.push(0);
    git_ok_with(
        top,
        &quiet(&["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"]),
        GitOptions {
            env: env.iter().map(|(k, v)| (*k, v.to_string())).collect(),
            input: Some(input),
        },
    )
    .map_err(error)?;
    let tree = git_ok_with(top, &["write-tree"], opts(&env))
        .map_err(error)?
        .trim()
        .to_string();
    if let Some(head) = head {
        let head_tree = git_ok(top, &["rev-parse", &format!("{head}^{{tree}}")]).map_err(error)?;
        if head_tree.trim() == tree {
            return Ok((Some(head.to_string()), tree));
        }
    }
    Ok((None, tree))
}

/// Entries the index marks assume-unchanged or skip-worktree (with the file present) are hashed again, as Mirror's
/// `clear_index_flags` does: a flagged entry would otherwise keep its stale content in the checkpoint.
fn clear_index_flags(top: &Path, env: &[(&str, &str)]) -> Result<(), String> {
    let listed =
        git_ok_with(top, &["-c", "core.quotepath=off", "ls-files", "-v", "-z"], opts(env)).map_err(|e| e.message)?;
    let (mut assumed, mut skipped) = (Vec::new(), Vec::new());
    for item in listed.split('\0').filter(|item| item.len() > 2) {
        let (tag, path) = (item.as_bytes()[0], &item[2..]);
        if tag.is_ascii_lowercase() {
            assumed.push(path);
        }
        if tag.eq_ignore_ascii_case(&b'S') && std::fs::symlink_metadata(top.join(path)).is_ok() {
            skipped.push(path);
        }
    }
    for (flag, paths) in [("--no-assume-unchanged", assumed), ("--no-skip-worktree", skipped)] {
        if paths.is_empty() {
            continue;
        }
        let mut input = paths.join("\0").into_bytes();
        input.push(0);
        git_ok_with(
            top,
            &["update-index", flag, "-z", "--stdin"],
            GitOptions {
                env: env.iter().map(|(k, v)| (*k, v.to_string())).collect(),
                input: Some(input),
            },
        )
        .map_err(|e| e.message)?;
    }
    Ok(())
}

fn make_commit(top: &Path, tree: &str, head: Option<&str>) -> Result<String, String> {
    let mut args = vec!["commit-tree", tree, "-m", MESSAGE];
    if let Some(head) = head {
        args.extend(["-p", head]);
    }
    let identity = identity_env(top).map_err(|e| e.message)?;
    git_ok_with(
        top,
        &quiet(&args),
        GitOptions {
            env: identity,
            input: None,
        },
    )
    .map(|out| out.trim().to_string())
    .map_err(|e| e.message)
}

/// What [`restore`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restored {
    /// The files as they were before the restore.
    pub safety: Checkpoint,
    pub written: Vec<String>,
    pub deleted: Vec<String>,
}

/// One `diff-tree --raw` change.
struct Change {
    src_mode: String,
    dst_mode: String,
    status: char,
    path: String,
}

fn parse_raw(output: &str) -> Vec<Change> {
    let mut changes = Vec::new();
    let mut parts = output.split('\0');
    while let Some(head) = parts.next() {
        let Some(head) = head.strip_prefix(':') else {
            continue;
        };
        let Some(path) = parts.next() else { break };
        let fields: Vec<&str> = head.split(' ').collect();
        if fields.len() < 5 {
            continue;
        }
        changes.push(Change {
            src_mode: fields[0].to_string(),
            dst_mode: fields[1].to_string(),
            status: fields[4].chars().next().unwrap_or('M'),
            path: path.to_string(),
        });
    }
    changes
}

/// Remove the empty directories from `path`'s parent up to (not including) `top`.
fn prune_empty_parents(top: &Path, path: &Path) {
    let mut dir = path.parent();
    while let Some(current) = dir {
        if current == top || !current.starts_with(top) || std::fs::remove_dir(current).is_err() {
            return;
        }
        dir = current.parent();
    }
}

/// Make the working tree hold `target`'s files: the current files are recorded first (the returned
/// [`Restored::safety`]); files the target lacks are deleted, the others written from it.
pub fn restore(cwd: &Path, target: &Checkpoint, limits: &Limits, jj: Option<&Path>) -> Result<Restored, String> {
    if target.vcs == "jj" {
        let jj = jj.ok_or("This checkpoint was recorded by jj, and jj is not on the session's PATH")?;
        return jj_restore(Path::new(&target.root), jj, target, limits);
    }
    let info = repo_info(cwd).map_err(|e| e.message)?;
    let top = info.top.clone();
    if top.display().to_string() != target.root {
        return Err(format!(
            "The checkpoint belongs to {}, not to this working tree ({})",
            target.root,
            top.display()
        ));
    }
    let short = &target.commit[..target.commit.len().min(12)];
    if git_ok(&top, &["cat-file", "-e", &format!("{}^{{commit}}", target.commit)]).is_err() {
        return Err(format!(
            "Checkpoint {short} is no longer in the repository (Git may have pruned it); nothing was restored"
        ));
    }
    let safety = match capture(cwd, None, limits, None)? {
        Capture::Taken(checkpoint) | Capture::Unchanged(checkpoint) => checkpoint,
        Capture::Skipped(note) => {
            return Err(format!(
                "The current files could not be saved first ({note}); nothing was restored"
            ));
        }
        Capture::Outside => return Err("Not in a Git working tree; nothing was restored".into()),
    };
    let safety_tree = safety.tree.clone().unwrap_or_default();
    let target_tree = match &target.tree {
        Some(tree) => tree.clone(),
        None => git_ok(&top, &["rev-parse", &format!("{}^{{tree}}", target.commit)])
            .map_err(|e| e.message)?
            .trim()
            .to_string(),
    };
    let raw = git_ok(
        &top,
        &[
            "-c",
            "core.quotepath=off",
            "diff-tree",
            "-r",
            "-z",
            "--no-renames",
            "--raw",
            &safety_tree,
            &target_tree,
        ],
    )
    .map_err(|e| e.message)?;
    let changes = parse_raw(&raw);
    // A file the safety checkpoint does not hold (ignored, or left out for its size) is never overwritten.
    let unsaved: Vec<&str> = changes
        .iter()
        .filter(|c| c.status == 'A' && c.dst_mode != GITLINK)
        .filter(|c| std::fs::symlink_metadata(top.join(&c.path)).is_ok())
        .map(|c| c.path.as_str())
        .collect();
    if !unsaved.is_empty() {
        return Err(format!(
            "Restoring would overwrite files that cannot be saved first (ignored or too large): {}; nothing was restored",
            unsaved.join(", ")
        ));
    }
    let mut deleted = Vec::new();
    for change in changes.iter().filter(|c| c.status == 'D' && c.src_mode != GITLINK) {
        let path = top.join(&change.path);
        if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_dir()) {
            std::fs::remove_file(&path).map_err(|e| interrupted(&safety, &format!("{}: {e}", change.path)))?;
            deleted.push(change.path.clone());
        }
        prune_empty_parents(&top, &path);
    }
    let written: Vec<String> = changes
        .iter()
        .filter(|c| c.status != 'D' && c.dst_mode != GITLINK)
        .map(|c| c.path.clone())
        .collect();
    if !written.is_empty() {
        let temporary = TempIndex(temporary_index("restore-index"));
        let index = temporary.0.display().to_string();
        let env = [("GIT_INDEX_FILE", index.as_str())];
        git_ok_with(&top, &["read-tree", &target_tree], opts(&env)).map_err(|e| interrupted(&safety, &e.message))?;
        for path in &written {
            let full = top.join(path);
            // A directory where the checkpoint has a file: emptied by the deletions above, or the restore stops.
            if std::fs::symlink_metadata(&full).is_ok_and(|m| m.is_dir()) {
                std::fs::remove_dir(&full).map_err(|e| interrupted(&safety, &format!("{path}: {e}")))?;
            }
        }
        let mut input = written.join("\0").into_bytes();
        input.push(0);
        git_ok_with(
            &top,
            &quiet(&["checkout-index", "-f", "-z", "--stdin"]),
            GitOptions {
                env: env.iter().map(|(k, v)| (*k, v.to_string())).collect(),
                input: Some(input),
            },
        )
        .map_err(|e| interrupted(&safety, &e.message))?;
    }
    Ok(Restored {
        safety,
        written,
        deleted,
    })
}

/// A restore that stopped part way: the files before it are in `safety`.
fn interrupted(safety: &Checkpoint, why: &str) -> String {
    format!(
        "The restore stopped part way ({why}); the files from before it are saved as checkpoint {}",
        &safety.commit[..safety.commit.len().min(12)]
    )
}

// ---------------------------------------------------------------------------------------------------------------
// Jujutsu

/// Run `jj` in `root`; stdout trimmed. A hang guard kills it after two minutes.
fn jj_run(jj: &Path, root: &Path, args: &[&str]) -> Result<String, String> {
    let mut child = Command::new(jj)
        .args(args)
        .current_dir(root)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("jj could not start: {e}"))?;
    let pid = child.id();
    let (done, finished) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if finished.recv_timeout(JJ_TIMEOUT).is_err()
            && let Some(pid) = rustix::process::Pid::from_raw(pid as i32)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
    });
    let mut stderr_pipe = child.stderr.take();
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    });
    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let _ = done.send(());
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(format!("jj {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(stdout.trim().to_string())
}

fn jj_commit(jj: &Path, root: &Path, snapshot: bool, limits: &Limits) -> Result<String, String> {
    let size = format!("snapshot.max-new-file-size={}", limits.max_file_bytes);
    jj_run(
        jj,
        root,
        &[
            // Keep the snapshot operation uncommitted: no change to @, bookmarks or the checkout.
            if snapshot {
                "--no-integrate-operation"
            } else {
                "--ignore-working-copy"
            },
            "--color=never",
            "--no-pager",
            "--config",
            &size,
            "--config",
            "snapshot.auto-track=\"all()\"",
            "log",
            "--no-graph",
            "-r",
            "@",
            "-T",
            "commit_id",
        ],
    )
}

fn jj_capture(root: &Path, jj: &Path, previous: Option<&Checkpoint>, limits: &Limits) -> Result<Capture, String> {
    let head = jj_commit(jj, root, false, limits)?;
    let commit = jj_commit(jj, root, true, limits)?;
    if let Some(previous) = previous
        && previous.vcs == "jj"
        && previous.commit == commit
    {
        return Ok(Capture::Unchanged(previous.clone()));
    }
    Ok(Capture::Taken(Checkpoint {
        vcs: "jj".into(),
        root: root.display().to_string(),
        head: Some(head),
        commit,
        tree: None,
        excluded: Vec::new(),
    }))
}

fn jj_restore(root: &Path, jj: &Path, target: &Checkpoint, limits: &Limits) -> Result<Restored, String> {
    let safety = match jj_capture(root, jj, None, limits)? {
        Capture::Taken(checkpoint) | Capture::Unchanged(checkpoint) => checkpoint,
        _ => return Err("The current files could not be saved first; nothing was restored".into()),
    };
    jj_run(
        jj,
        root,
        &["--color=never", "--no-pager", "restore", "--from", &target.commit],
    )
    .map_err(|e| interrupted(&safety, &e))?;
    Ok(Restored {
        safety,
        written: Vec::new(),
        deleted: Vec::new(),
    })
}
