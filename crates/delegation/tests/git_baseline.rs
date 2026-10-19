//! The Git baseline a spawn contract keeps so its workspace checks still work after a restart
//! (`delegation::git_baseline`): HEAD, a digest of `git status --porcelain` and the content ids of the paths dirty
//! at the start; later, `git diff --name-only <commit>` plus untracked files, minus dirty paths whose content is
//! unchanged. Real git in a temp repository with no global or system configuration and a temp HOME, run through the
//! environment the test passes in (the session's). New in Rust (wave 41): literal expectations.

use delegation::git_baseline::GitBaseline;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    env: Vec<(String, String)>,
}

impl Repo {
    /// An empty directory; `init` makes it a repository with `files` committed.
    fn new() -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("repo");
        let home = dir.path().canonicalize().unwrap().join("home");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let env = vec![
            ("PATH".to_string(), "/usr/local/bin:/usr/bin:/bin".to_string()),
            ("HOME".to_string(), home.display().to_string()),
            ("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string()),
            ("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string()),
        ];
        Repo { _dir: dir, root, env }
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("git")
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write(&self, path: &str, text: &str) {
        let target = self.root.join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(target, text).unwrap();
    }

    fn init(files: &[(&str, &str)]) -> Repo {
        let repo = Repo::new();
        repo.git(&["init", "-q"]);
        for (path, text) in files {
            repo.write(path, text);
        }
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "init"]);
        repo
    }

    fn capture(&self, cwd: &Path) -> GitBaseline {
        GitBaseline::capture(cwd, &self.env).expect("a baseline")
    }

    fn changed(&self, baseline: &GitBaseline, cwd: &Path) -> Vec<String> {
        baseline.changes(cwd, &self.env).unwrap().changed
    }
}

#[test]
fn delegation_w18s_git_baseline_of_an_unchanged_clean_tree_lists_nothing() {
    let repo = Repo::init(&[("a.txt", "a\n"), ("src/b.rs", "b\n")]);
    let baseline = repo.capture(&repo.root);
    assert!(baseline.dirty.is_empty());
    assert_eq!(baseline.commit.len(), 40, "{baseline:?}");
    assert_eq!(repo.changed(&baseline, &repo.root), Vec::<String>::new());
}

#[test]
fn delegation_w18s_git_baseline_lists_edits_additions_and_deletions_since_the_start() {
    let repo = Repo::init(&[("a.txt", "a\n"), ("gone.txt", "g\n"), ("src/b.rs", "b\n")]);
    let baseline = repo.capture(&repo.root);
    repo.write("a.txt", "changed\n");
    repo.write("new/c.txt", "c\n");
    std::fs::remove_file(repo.root.join("gone.txt")).unwrap();
    assert_eq!(
        repo.changed(&baseline, &repo.root),
        vec!["a.txt", "gone.txt", "new/c.txt"]
    );
    // Ignored output is never listed.
    repo.write(".gitignore", "target/\n");
    repo.write("target/out.bin", "x");
    assert_eq!(
        repo.changed(&baseline, &repo.root),
        vec![".gitignore", "a.txt", "gone.txt", "new/c.txt"]
    );
}

#[test]
fn delegation_w18s_git_baseline_paths_dirty_at_the_start_count_only_when_their_content_changes() {
    let repo = Repo::init(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
    // Dirty before the child starts: a modified, b modified, u untracked.
    repo.write("a.txt", "a dirty\n");
    repo.write("b.txt", "b dirty\n");
    repo.write("u.txt", "u\n");
    let baseline = repo.capture(&repo.root);
    assert_eq!(
        baseline.dirty.keys().cloned().collect::<Vec<_>>(),
        vec!["a.txt", "b.txt", "u.txt"]
    );
    // Nothing the child did yet.
    assert_eq!(repo.changed(&baseline, &repo.root), Vec::<String>::new());
    // The child edits a again, restores b to the commit's content, deletes u and edits c.
    repo.write("a.txt", "a edited by the child\n");
    repo.write("b.txt", "b\n");
    std::fs::remove_file(repo.root.join("u.txt")).unwrap();
    repo.write("c.txt", "c edited\n");
    assert_eq!(
        repo.changed(&baseline, &repo.root),
        vec!["a.txt", "b.txt", "c.txt", "u.txt"]
    );
}

#[test]
fn delegation_w18s_git_baseline_sees_work_the_child_committed() {
    let repo = Repo::init(&[("a.txt", "a\n")]);
    let baseline = repo.capture(&repo.root);
    repo.write("Cargo.toml", "[package]\n");
    repo.git(&["add", "Cargo.toml"]);
    repo.git(&["commit", "-q", "-m", "child work"]);
    assert_eq!(repo.changed(&baseline, &repo.root), vec!["Cargo.toml"]);
}

#[test]
fn delegation_w18s_git_baseline_in_a_subdirectory_is_relative_to_it_and_ignores_the_rest() {
    let repo = Repo::init(&[("top.txt", "t\n"), ("sub/in.txt", "i\n")]);
    repo.write("sub/dirty.txt", "d\n");
    let sub = repo.root.join("sub");
    let baseline = repo.capture(&sub);
    assert_eq!(baseline.dirty.keys().cloned().collect::<Vec<_>>(), vec!["dirty.txt"]);
    repo.write("top.txt", "outside\n");
    repo.write("sub/in.txt", "inside\n");
    repo.write("sub/deep/new.txt", "n\n");
    assert_eq!(repo.changed(&baseline, &sub), vec!["deep/new.txt", "in.txt"]);
}

#[test]
fn delegation_w18s_git_baseline_is_none_outside_git_before_a_commit_or_without_git_on_the_path() {
    let plain = Repo::new();
    plain.write("a.txt", "a\n");
    assert_eq!(GitBaseline::capture(&plain.root, &plain.env), None);
    plain.git(&["init", "-q"]);
    assert_eq!(GitBaseline::capture(&plain.root, &plain.env), None, "no commit yet");
    let repo = Repo::init(&[("a.txt", "a\n")]);
    // The session's PATH finds git, never this process's.
    let no_git: Vec<(String, String)> = repo
        .env
        .iter()
        .map(|(k, v)| match k.as_str() {
            "PATH" => (k.clone(), "/nonexistent".to_string()),
            _ => (k.clone(), v.clone()),
        })
        .collect();
    assert_eq!(GitBaseline::capture(&repo.root, &no_git), None);
    // GIT_DIR in the session environment never points git elsewhere.
    let mut redirected = repo.env.clone();
    redirected.push(("GIT_DIR".into(), plain.root.join(".git").display().to_string()));
    let baseline = GitBaseline::capture(&repo.root, &redirected).unwrap();
    assert_eq!(baseline, repo.capture(&repo.root));
}

#[test]
fn delegation_w18s_git_baseline_round_trips_and_a_lost_commit_is_an_error() {
    let repo = Repo::init(&[("a.txt", "a\n")]);
    repo.write("d.txt", "d\n");
    let baseline = repo.capture(&repo.root);
    assert_eq!(GitBaseline::from_value(&baseline.describe()), Some(baseline.clone()));
    assert_eq!(
        GitBaseline::from_value(&serde_json::json!({"commit": "", "status": "", "dirty": {}})),
        None
    );
    assert_eq!(GitBaseline::from_value(&serde_json::Value::Null), None);
    let lost = GitBaseline {
        commit: "0".repeat(40),
        ..baseline
    };
    let error = lost.changes(&repo.root, &repo.env).unwrap_err();
    assert_eq!(
        error,
        format!(
            "git could not list the changes since the child started: commit {} is not in the repository",
            "0".repeat(40)
        )
    );
}

#[test]
fn delegation_w18s_git_baseline_status_digest_tells_a_changed_clean_tree() {
    let repo = Repo::init(&[("a.txt", "a\n")]);
    let baseline = repo.capture(&repo.root);
    // Same commit, but a new untracked file: the digest differs, the full comparison names it.
    repo.write("n.txt", "n\n");
    assert_eq!(repo.changed(&baseline, &repo.root), vec!["n.txt"]);
    let digest = repo.capture(&repo.root).status;
    assert_ne!(digest, baseline.status);
}
