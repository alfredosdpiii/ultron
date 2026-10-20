//! The Git baseline a spawn contract keeps so its workspace checks still work after a restart
//! (`delegation::git_baseline`): HEAD and one Git tree of the work tree at the start (wave 43; built in a temporary
//! index), later compared with a tree of the work tree then. Baselines of the earlier format (a digest of
//! `git status --porcelain` and the content ids of the paths dirty at the start) are still read and compared as
//! before. Real git in a temp repository with no global or system configuration and a temp HOME, run through the
//! environment the test passes in (the session's). New in Rust (waves 41, 43): literal expectations.

use delegation::git_baseline::{DirtyState, GitBaseline, MAX_DIRTY};
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

    /// `git` with its stdout.
    fn out(&self, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        output.stdout
    }
}

/// A baseline as the session keeps it, read back as a restarted host does.
fn restarted(baseline: &GitBaseline) -> GitBaseline {
    GitBaseline::from_value(&serde_json::from_str(&baseline.describe().to_string()).unwrap()).unwrap()
}

#[test]
fn delegation_w18s_git_baseline_of_an_unchanged_clean_tree_lists_nothing() {
    let repo = Repo::init(&[("a.txt", "a\n"), ("src/b.rs", "b\n")]);
    let baseline = repo.capture(&repo.root);
    assert!(
        matches!(&baseline.state, DirtyState::Tree(t) if t.len() == 40),
        "{baseline:?}"
    );
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

/// Dirty before the child starts: a modified, b modified, u untracked; then the child edits a again, restores b
/// to the commit's content, deletes u and edits c. `capture` takes the baseline.
type Capture = fn(&Path, &[(String, String)]) -> Option<GitBaseline>;

fn dirty_at_the_start(capture: Capture) -> GitBaseline {
    let repo = Repo::init(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
    repo.write("a.txt", "a dirty\n");
    repo.write("b.txt", "b dirty\n");
    repo.write("u.txt", "u\n");
    let baseline = restarted(&capture(&repo.root, &repo.env).unwrap());
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
    baseline
}

#[test]
fn delegation_w18s_git_baseline_paths_dirty_at_the_start_count_only_when_their_content_changes() {
    let baseline = dirty_at_the_start(GitBaseline::capture);
    assert!(matches!(baseline.state, DirtyState::Tree(_)), "{baseline:?}");
}

/// A baseline of the earlier format (wave 41) a session kept before is still compared as it was.
#[test]
fn delegation_w21c_git_baseline_of_the_earlier_format_still_compares_as_before() {
    let baseline = dirty_at_the_start(GitBaseline::capture_paths);
    let DirtyState::Paths { dirty, .. } = &baseline.state else {
        panic!("{baseline:?}")
    };
    assert_eq!(
        dirty.keys().cloned().collect::<Vec<_>>(),
        vec!["a.txt", "b.txt", "u.txt"]
    );
    assert_eq!(
        baseline
            .describe()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["commit", "status", "dirty"]
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
    let baseline = restarted(&repo.capture(&sub));
    assert_eq!(repo.changed(&baseline, &sub), Vec::<String>::new());
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
    let DirtyState::Tree(tree) = &baseline.state else {
        panic!("{baseline:?}")
    };
    assert_eq!(
        baseline.describe(),
        serde_json::json!({"commit": baseline.commit, "tree": tree})
    );
    let earlier = GitBaseline::capture_paths(&repo.root, &repo.env).unwrap();
    assert_eq!(GitBaseline::from_value(&earlier.describe()), Some(earlier));
    for bad in [
        serde_json::json!({"commit": "", "status": "", "dirty": {}}),
        serde_json::json!({"commit": "0".repeat(40), "tree": ""}),
        serde_json::json!({"commit": "0".repeat(40), "tree": 1, "status": "", "dirty": {}}),
        serde_json::json!({"commit": "0".repeat(40)}),
    ] {
        assert_eq!(GitBaseline::from_value(&bad), None, "{bad}");
    }
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
    let gone = GitBaseline {
        state: DirtyState::Tree("1".repeat(40)),
        ..repo.capture(&repo.root)
    };
    assert_eq!(
        gone.changes(&repo.root, &repo.env).unwrap_err(),
        format!(
            "git could not list the changes since the child started: tree {} is not in the repository",
            "1".repeat(40)
        )
    );
}

#[test]
fn delegation_w18s_git_baseline_status_digest_tells_a_changed_clean_tree() {
    let repo = Repo::init(&[("a.txt", "a\n")]);
    let baseline = repo.capture(&repo.root);
    // Same commit, but a new untracked file: the tree differs, the comparison names it.
    repo.write("n.txt", "n\n");
    assert_eq!(repo.changed(&baseline, &repo.root), vec!["n.txt"]);
    assert_ne!(repo.capture(&repo.root).state, baseline.state);
    // The earlier format: the status digest differs.
    std::fs::remove_file(repo.root.join("n.txt")).unwrap();
    let earlier = GitBaseline::capture_paths(&repo.root, &repo.env).unwrap();
    repo.write("n.txt", "n\n");
    assert_eq!(repo.changed(&earlier, &repo.root), vec!["n.txt"]);
}

/// Wave 43: a work tree with far more dirty paths than the earlier format kept ([`MAX_DIRTY`]) still has a
/// baseline, one bounded value, and after a restart only what changed since is listed.
#[test]
fn delegation_w21c_git_baseline_covers_a_thousand_dirty_paths_across_a_restart() {
    let repo = Repo::init(&[("a.txt", "a\n"), ("keep/t0.txt", "0\n")]);
    for n in 0..1000 {
        repo.write(&format!("gen/f{n:04}.txt"), &format!("{n}\n"));
    }
    repo.write("keep/t0.txt", "dirty\n");
    assert_eq!(
        GitBaseline::capture_paths(&repo.root, &repo.env),
        None,
        "the earlier format's cap"
    );
    const { assert!(1001 > MAX_DIRTY) };
    let baseline = repo.capture(&repo.root);
    let kept = baseline.describe().to_string();
    assert!(kept.len() < 200, "{kept}");
    let baseline = restarted(&baseline);
    assert_eq!(repo.changed(&baseline, &repo.root), Vec::<String>::new());
    repo.write("gen/f0500.txt", "child\n");
    std::fs::remove_file(repo.root.join("gen/f0999.txt")).unwrap();
    repo.write("keep/t0.txt", "0\n");
    repo.write("child.txt", "c\n");
    assert_eq!(
        repo.changed(&baseline, &repo.root),
        vec!["child.txt", "gen/f0500.txt", "gen/f0999.txt", "keep/t0.txt"]
    );
}

/// The tree is built in a temporary index: the repository's own index (what is staged) and the work tree are as
/// they were; staged, intent-to-add and ignored paths are handled as `git status` sees them.
#[test]
fn delegation_w21c_git_baseline_never_touches_the_real_index() {
    let repo = Repo::init(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("sub/c.txt", "c\n")]);
    repo.write(".gitignore", "*.log\n");
    repo.write("a.txt", "staged\n");
    repo.git(&["add", "a.txt"]);
    repo.write("a.txt", "staged then edited\n");
    repo.write("new.txt", "n\n");
    repo.git(&["add", "-N", "new.txt"]);
    repo.write("noise.log", "x");
    let index = repo.root.join(".git/index");
    let before = std::fs::read(&index).unwrap();
    let staged = repo.out(&["diff", "--cached", "--raw"]);
    let sub = repo.root.join("sub");
    let baseline = repo.capture(&sub);
    let root = repo.capture(&repo.root);
    repo.write("sub/c.txt", "child\n");
    repo.write("b.txt", "child\n");
    repo.write("other.log", "y");
    assert_eq!(repo.changed(&baseline, &sub), vec!["c.txt"]);
    assert_eq!(repo.changed(&root, &repo.root), vec!["b.txt", "sub/c.txt"]);
    assert_eq!(std::fs::read(&index).unwrap(), before, "the real index");
    assert_eq!(repo.out(&["diff", "--cached", "--raw"]), staged, "what is staged");
    assert_eq!(
        std::fs::read_to_string(repo.root.join("a.txt")).unwrap(),
        "staged then edited\n"
    );
}
