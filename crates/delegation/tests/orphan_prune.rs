//! Startup pruning (native-host.ts `loadTasks`: `pruneWorktrees(workspace)` unless `keepWorktrees()`): the
//! worktrees of dead owners go when a session's host starts, never with ULTRON_KEEP_WORKTREES, and a repository
//! without Ultron's worktrees directory is left alone. Live owners (this process) are kept, and so are the starting
//! session's own (the durable host reattaches its children with their worktrees).

use delegation::host::{Delegation, DelegationOptions};
use delegation::worktrees::{self as wt, CreateWorktreeOptions, WorktreeRecord};
use std::path::Path;
use std::sync::Arc;

fn git(cwd: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", cwd)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

fn repo(dir: &Path) {
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("a.txt"), "a\n").unwrap();
    git(dir, &["add", "a.txt"]);
    git(
        dir,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
    );
}

/// A pid that is certainly not running: a child this test spawned and reaped.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

fn worktree(cwd: &Path, session_id: &str, name: &str, dead: bool) -> WorktreeRecord {
    let record = wt::create_child_worktree(&CreateWorktreeOptions {
        cwd,
        session_id,
        name,
        setup: None,
        now: 0.0,
    })
    .unwrap();
    if dead {
        let file = Path::new(&record.common_dir)
            .join(wt::WORKTREES_DIR)
            .join(format!("{}.json", record.id));
        let mut saved: WorktreeRecord = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        saved.owner.pid = dead_pid();
        std::fs::write(&file, wt::record_json(&saved)).unwrap();
    }
    record
}

fn host(workspace: &Path, keep: bool) -> Delegation {
    let mut options = DelegationOptions::new(Arc::new(|_| Ok(String::new())));
    options.workspace = Some(workspace.to_path_buf());
    options.keep_worktrees = keep;
    Delegation::new(options).0
}

#[test]
fn delegation_startup_prunes_dead_owners_worktrees_unless_kept() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    repo(&cwd);
    let orphan = worktree(&cwd, "old", "orphan", true);
    let live = worktree(&cwd, "old", "live", false);
    // The starting session's own children are reattached with their worktrees, whoever owned them.
    let own = worktree(&cwd, "session", "mine", true);
    // ULTRON_KEEP_WORKTREES: nothing is pruned.
    assert_eq!(host(&cwd, true).prune_orphan_worktrees(), wt::PruneReport::default());
    assert!(Path::new(&orphan.path).exists());
    let report = host(&cwd, false).prune_orphan_worktrees();
    assert_eq!(report.removed, vec![orphan.path.clone()]);
    assert!(!Path::new(&orphan.path).exists());
    assert!(Path::new(&live.path).exists());
    assert!(Path::new(&own.path).exists());
    // No workspace: nothing to prune.
    let none = Delegation::new(DelegationOptions::new(Arc::new(|_| Ok(String::new())))).0;
    assert_eq!(none.prune_orphan_worktrees(), wt::PruneReport::default());
}

/// A repository Ultron never made worktrees in is not touched: no record directory appears and `git worktree
/// prune` does not run (a stale admin entry of the user's own stays).
#[test]
fn delegation_startup_pruning_leaves_repositories_without_ultron_worktrees_alone() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().canonicalize().unwrap();
    repo(&cwd);
    let other = dir.path().join("other-tree");
    git(&cwd, &["worktree", "add", "-q", "--detach", other.to_str().unwrap()]);
    std::fs::remove_dir_all(&other).unwrap();
    let admin = cwd.join(".git/worktrees/other-tree");
    assert!(admin.exists());
    assert_eq!(host(&cwd, false).prune_orphan_worktrees(), wt::PruneReport::default());
    assert!(admin.exists(), "git worktree prune ran");
    assert!(!cwd.join(".git").join(wt::WORKTREES_DIR).exists());
}
