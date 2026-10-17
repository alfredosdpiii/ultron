//! Loki's session setup (loki.ts `autoInitEligibility`, `autoCommitSkipReason`, `commitLoki`, `setupLoki`'s notice),
//! ported from ultron-loki.test.ts ("Loki switches", "auto-install eligibility", "auto-commit", "setup creates only
//! .loki/, commits it") with its literal expectations. Every git process sees only the test's variables: a private
//! HOME, no system config, a fixed identity.

#[path = "../../cli/tests/executable_support/mod.rs"]
mod executable_support;

use loki::auto_setup::{
    Setup, SetupContext, auto_commit, auto_commit_skip_reason, auto_init, auto_init_eligibility, commit_loki,
    commit_notice, setup,
};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

struct Work {
    dir: PathBuf,
    _tmp: tempfile::TempDir,
}

fn work() -> Work {
    let tmp = tempfile::tempdir().unwrap();
    Work {
        dir: tmp.path().canonicalize().unwrap(),
        _tmp: tmp,
    }
}

/// The environment of these tests: no CI, no switches, a fixed identity, HOME = `home`.
fn base_env(home: &Path) -> Vec<(String, String)> {
    [
        ("PATH", "/usr/local/bin:/usr/bin:/bin".to_string()),
        ("HOME", home.display().to_string()),
        ("GIT_CONFIG_NOSYSTEM", "1".to_string()),
        ("GIT_AUTHOR_NAME", "Test User".to_string()),
        ("GIT_AUTHOR_EMAIL", "test@example.com".to_string()),
        ("GIT_COMMITTER_NAME", "Test User".to_string()),
        ("GIT_COMMITTER_EMAIL", "test@example.com".to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

fn git(cwd: &Path, home: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(base_env(home))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn repository(parent: &Path, name: &str, home: &Path) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).unwrap();
    git(&root, home, &["init", "-q", "-b", "main"]);
    git(&root, home, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join("a.txt"), "a\n").unwrap();
    std::fs::write(root.join("c.txt"), "c\n").unwrap();
    git(&root, home, &["add", "a.txt", "c.txt"]);
    git(&root, home, &["commit", "-qm", "init"]);
    root
}

fn fake_loki_dir(root: &Path) {
    std::fs::create_dir_all(root.join(".loki")).unwrap();
    std::fs::write(root.join(".loki/loki.py"), "# engine\n").unwrap();
    std::fs::write(root.join(".loki/loki.json"), "{}\n").unwrap();
}

/// Runs `f` with a context over `cwd`, `home`, the base environment plus `extra`, and `settings`.
fn with_context<T>(
    cwd: &Path,
    home: &Path,
    extra: &[(&str, &str)],
    settings: Value,
    f: impl FnOnce(&SetupContext<'_>) -> T,
) -> T {
    let mut env = base_env(home);
    env.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let var = move |key: &str| env.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    f(&SetupContext {
        cwd,
        home,
        var: &var,
        settings: &settings,
    })
}

#[test]
fn loki_auto_setup_switches_environment_overrides_settings_defaults_on() {
    let w = work();
    let on = |extra: &[(&str, &str)], settings: Value| {
        with_context(&w.dir, &w.dir, extra, settings, |c| (auto_init(c), auto_commit(c)))
    };
    assert_eq!(on(&[], json!({})), (true, true));
    assert!(!on(&[("ULTRON_LOKI_AUTOINIT", "off")], json!({"autoInit": true})).0);
    assert!(!on(&[], json!({"autoInit": false})).0);
    assert!(!on(&[("ULTRON_LOKI_AUTOCOMMIT", "0")], json!({})).1);
    assert!(on(&[("ULTRON_LOKI_AUTOCOMMIT", "on")], json!({"autoCommit": false})).1);
    assert!(!on(&[("ULTRON_LOKI_AUTOINIT", " NONE ")], json!({})).0);
}

#[test]
fn loki_auto_setup_a_writable_repository_qualifies_from_any_directory_inside_it() {
    let w = work();
    let root = repository(&w.dir, "repo", &w.dir);
    std::fs::create_dir(root.join("src")).unwrap();
    let got = with_context(&root.join("src"), &w.dir, &[], json!({}), auto_init_eligibility);
    assert_eq!(got, Ok(root));
}

#[test]
fn loki_auto_setup_every_skip_condition_names_its_reason() {
    let w = work();
    let root = repository(&w.dir, "repo", &w.dir);
    let reason = |cwd: &Path, home: &Path, extra: &[(&str, &str)], settings: Value| {
        with_context(cwd, home, extra, settings, auto_init_eligibility).err()
    };
    let plain = w.dir.join("plain");
    std::fs::create_dir(&plain).unwrap();
    let r = |s: &str| Some(s.to_string());
    assert_eq!(reason(&plain, &w.dir, &[], json!({})), r("not a Git repository"));
    assert_eq!(
        reason(&root, &root, &[], json!({})),
        r("the repository root is the home directory")
    );
    let package = repository(&w.dir.join("node_modules"), "pkg", &w.dir);
    assert_eq!(
        reason(&package, &w.dir, &[], json!({})),
        r("the repository is inside node_modules")
    );
    assert_eq!(reason(&root, &w.dir, &[("CI", "true")], json!({})), r("CI is set"));
    assert_eq!(
        reason(&root, &w.dir, &[("ULTRON_LOKI_AUTOINIT", "off")], json!({})),
        r("auto-install is off (ULTRON_LOKI_AUTOINIT)")
    );
    assert_eq!(
        reason(&root, &w.dir, &[], json!({"autoInit": false})),
        r("auto-install is off (loki.autoInit setting)")
    );
    assert_eq!(
        reason(&root, &w.dir, &[], json!({"ignoreRepos": [root.display().to_string()]})),
        r("the repository is listed in loki.ignoreRepos")
    );
    assert_eq!(
        reason(&root, &w.dir, &[], json!({"ignoreRepos": ["~/repo"]})),
        r("the repository is listed in loki.ignoreRepos")
    );
    assert_eq!(
        reason(&root, &w.dir, &[("ULTRON_WORKTREE", "1")], json!({})),
        r("this is an Ultron sub-agent worktree")
    );
    let nested = repository(&w.dir.join("ultron-worktrees"), "child", &w.dir);
    assert_eq!(
        reason(&nested, &w.dir, &[], json!({})),
        r("this is an Ultron sub-agent worktree")
    );
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
    let read_only = reason(&root, &w.dir, &[], json!({}));
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(read_only, r("the checkout is read-only"));
}

#[test]
fn loki_auto_setup_commit_skip_reasons_switches_ci_operations_detached_ignored() {
    let w = work();
    let root = repository(&w.dir, "repo", &w.dir);
    let reason = |extra: &[(&str, &str)], settings: Value| {
        with_context(&root, &w.dir, extra, settings, |c| auto_commit_skip_reason(c, &root))
    };
    let r = |s: &str| Some(s.to_string());
    assert_eq!(reason(&[], json!({})), None);
    assert_eq!(
        reason(&[("ULTRON_LOKI_AUTOCOMMIT", "off")], json!({})),
        r("auto-commit is off (ULTRON_LOKI_AUTOCOMMIT)")
    );
    assert_eq!(
        reason(&[], json!({"autoCommit": false})),
        r("auto-commit is off (loki.autoCommit setting)")
    );
    assert_eq!(reason(&[("CI", "1")], json!({})), r("CI is set"));
    for (name, expected) in [
        ("MERGE_HEAD", "a merge is in progress"),
        ("rebase-merge", "a rebase is in progress"),
        ("rebase-apply", "a rebase or am is in progress"),
        ("CHERRY_PICK_HEAD", "a cherry-pick is in progress"),
        ("REVERT_HEAD", "a revert is in progress"),
        ("BISECT_LOG", "a bisect is in progress"),
    ] {
        let path = root.join(".git").join(name);
        if name.starts_with("rebase") {
            std::fs::create_dir(&path).unwrap();
        } else {
            std::fs::write(&path, "").unwrap();
        }
        assert_eq!(reason(&[], json!({})), r(expected), "{name}");
        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::remove_file(&path);
    }
    git(&root, &w.dir, &["checkout", "-q", "--detach"]);
    assert_eq!(reason(&[], json!({})), r("HEAD is detached"));
    git(&root, &w.dir, &["checkout", "-q", "main"]);
    std::fs::write(root.join(".gitignore"), ".loki/\n").unwrap();
    assert_eq!(reason(&[], json!({})), r(".loki is gitignored"));
}

#[test]
fn loki_auto_setup_commits_loki_only_and_keeps_other_changes_exactly() {
    let w = work();
    let root = repository(&w.dir, "repo", &w.dir);
    std::fs::write(root.join("a.txt"), "a staged\n").unwrap();
    std::fs::write(root.join("b.txt"), "b new and staged\n").unwrap();
    git(&root, &w.dir, &["add", "a.txt", "b.txt"]);
    std::fs::write(root.join("a.txt"), "a staged, then edited again\n").unwrap();
    std::fs::write(root.join("c.txt"), "c unstaged\n").unwrap();
    std::fs::write(root.join("untracked.txt"), "untracked\n").unwrap();
    let staged_before = git(&root, &w.dir, &["diff", "--cached"]);
    let unstaged_before = git(&root, &w.dir, &["diff"]);
    fake_loki_dir(&root);

    let outcome = with_context(&root, &w.dir, &[], json!({}), |c| commit_loki(c, &root, None));
    assert_eq!(outcome, Ok(git(&root, &w.dir, &["rev-parse", "--short", "HEAD"])));
    let mut names: Vec<String> = git(&root, &w.dir, &["show", "--name-only", "--format=", "HEAD"])
        .lines()
        .map(str::to_string)
        .collect();
    names.sort();
    assert_eq!(names, [".loki/loki.json", ".loki/loki.py"]);
    assert_eq!(git(&root, &w.dir, &["log", "-1", "--format=%B"]), "Add Loki guardrails");
    assert_eq!(
        git(&root, &w.dir, &["log", "-1", "--format=%an <%ae>"]),
        "Test User <test@example.com>"
    );
    assert_eq!(git(&root, &w.dir, &["diff", "--cached"]), staged_before);
    assert_eq!(git(&root, &w.dir, &["diff"]), unstaged_before);
    assert_eq!(
        git(&root, &w.dir, &["status", "--porcelain", "--", "untracked.txt"]),
        "?? untracked.txt"
    );
}

#[test]
fn loki_auto_setup_a_rejecting_hook_leaves_loki_uncommitted_and_unstaged() {
    let w = work();
    let root = repository(&w.dir, "repo", &w.dir);
    std::fs::write(root.join("a.txt"), "a staged\n").unwrap();
    git(&root, &w.dir, &["add", "a.txt"]);
    let hook = root.join(".git/hooks/pre-commit");
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    executable_support::install(&hook, "#!/bin/sh\necho 'policy: commits need a ticket' >&2\nexit 1\n");
    fake_loki_dir(&root);
    let head = git(&root, &w.dir, &["rev-parse", "HEAD"]);

    let outcome = with_context(&root, &w.dir, &[], json!({}), |c| commit_loki(c, &root, None));
    assert_eq!(
        outcome,
        Err("git commit failed: policy: commits need a ticket".to_string())
    );
    assert_eq!(git(&root, &w.dir, &["rev-parse", "HEAD"]), head);
    assert!(root.join(".loki/loki.py").exists());
    assert_eq!(git(&root, &w.dir, &["diff", "--cached", "--name-only"]), "a.txt");
    assert_eq!(
        git(&root, &w.dir, &["status", "--porcelain", "--", ".loki"]),
        "?? .loki/"
    );
    // The note says why.
    let note = with_context(&root, &w.dir, &[], json!({}), |c| commit_notice(c, &root));
    assert_eq!(
        note,
        "Loki set up in .loki/ but not committed (git commit failed: policy: commits need a ticket). Before-write \
         checks run with the default policy; commit .loki/ to enable post-write checks."
    );
}

#[test]
fn loki_auto_setup_a_failing_or_hanging_signer_never_prompts() {
    let w = work();
    let root = repository(&w.dir, "repo", &w.dir);
    let signer = w.dir.join("signer.sh");
    // Executed by git: written without a write descriptor in this process (ETXTBSY under parallel tests).
    executable_support::install(
        &signer,
        "#!/bin/sh\necho 'gpg: signing failed: No pinentry' >&2\nexit 2\n",
    );
    git(&root, &w.dir, &["config", "commit.gpgsign", "true"]);
    git(&root, &w.dir, &["config", "gpg.program", &signer.display().to_string()]);
    fake_loki_dir(&root);
    let failed = with_context(&root, &w.dir, &[], json!({}), |c| commit_loki(c, &root, None)).unwrap_err();
    assert!(failed.starts_with("git commit failed: "), "{failed}");
    assert!(
        failed.to_lowercase().contains("gpg") || failed.contains("sign"),
        "{failed}"
    );
    assert_eq!(
        git(&root, &w.dir, &["status", "--porcelain", "--", ".loki"]),
        "?? .loki/"
    );

    // The signer waits on a FIFO the test writes only after the commit gave up: only the time limit ends it.
    let fifo = w.dir.join("release");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    executable_support::install(&signer, &format!("#!/bin/sh\nread line < '{}'\n", fifo.display()));
    let hung = with_context(&root, &w.dir, &[], json!({}), |c| commit_loki(c, &root, Some(1_000)));
    // Release the signer (it is the FIFO's only reader).
    std::fs::write(&fifo, "go\n").unwrap();
    assert_eq!(
        hung,
        Err("git commit failed: timed out after 1 s (a hook or a signing prompt?)".to_string())
    );
    assert_eq!(
        git(&root, &w.dir, &["status", "--porcelain", "--", ".loki"]),
        "?? .loki/"
    );
    assert!(
        !root.join(".git/index.lock").exists(),
        "git was stopped with SIGTERM, so it removed its lock"
    );
}

#[test]
fn loki_auto_setup_creates_only_loki_commits_it_and_a_second_session_changes_nothing() {
    let w = work();
    let root = repository(&w.dir, "repo", &w.dir);
    std::fs::write(root.join("a.txt"), "staged before setup\n").unwrap();
    git(&root, &w.dir, &["add", "a.txt"]);
    let first = with_context(&root, &w.dir, &[], json!({}), setup);
    assert_eq!(first, Setup::Commit(root.clone()));
    let note = with_context(&root, &w.dir, &[], json!({}), |c| commit_notice(c, &root));
    let sha = git(&root, &w.dir, &["rev-parse", "--short", "HEAD"]);
    assert_eq!(
        note,
        format!("Loki set up and committed ({sha}): .loki/ holds the guardrail engine and policy.")
    );
    let mut files: Vec<String> = std::fs::read_dir(root.join(".loki"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(files, ["loki.json", "loki.py"]);
    for other in [
        ".claude",
        ".codex",
        ".factory",
        ".pi",
        ".omp",
        ".github",
        ".ruff.toml",
        ".oxlintrc.json",
    ] {
        assert!(!root.join(other).exists(), "{other}");
    }
    assert_eq!(
        std::fs::read(root.join(".loki/loki.py")).unwrap(),
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/ultron/loki-engine/loki.py")
        )
        .unwrap()
    );
    assert_eq!(git(&root, &w.dir, &["diff", "--cached", "--name-only"]), "a.txt");
    // A second session uses the committed .loki/ and changes nothing.
    assert_eq!(with_context(&root, &w.dir, &[], json!({}), setup), Setup::Quiet);
    assert_eq!(git(&root, &w.dir, &["rev-parse", "--short", "HEAD"]), sha);
}

#[test]
fn loki_auto_setup_notes_without_a_commit_and_stays_quiet_where_it_installs_nothing() {
    let w = work();
    let root = repository(&w.dir, "repo", &w.dir);
    let skipped = with_context(&root, &w.dir, &[("ULTRON_LOKI_AUTOCOMMIT", "off")], json!({}), setup);
    assert_eq!(
        skipped,
        Setup::Notice(
            "Loki set up in .loki/ (not committed: auto-commit is off (ULTRON_LOKI_AUTOCOMMIT)). Commit it to enable \
             post-write checks: git add .loki && git commit -m \"Add Loki guardrails\" -- .loki"
                .into()
        )
    );
    let other = repository(&w.dir, "other", &w.dir);
    let off = with_context(&other, &w.dir, &[("ULTRON_LOKI_AUTOINIT", "off")], json!({}), setup);
    assert_eq!(off, Setup::Quiet);
    assert!(!other.join(".loki").exists());
    let plain = w.dir.join("plain");
    std::fs::create_dir(&plain).unwrap();
    assert_eq!(with_context(&plain, &w.dir, &[], json!({}), setup), Setup::Quiet);
    assert!(!plain.join(".loki").exists());
}

#[test]
fn loki_auto_setup_an_install_that_fails_is_the_note() {
    let w = work();
    let root = repository(&w.dir, "repo", &w.dir);
    // `.loki` is a file: the directory cannot be created.
    std::fs::write(root.join(".loki"), "not a directory\n").unwrap();
    let Setup::Notice(note) = with_context(&root, &w.dir, &[], json!({}), setup) else {
        panic!("no note");
    };
    assert!(
        note.starts_with("Loki could not create .loki/: loki: [Errno "),
        "{note}"
    );
    assert!(note.ends_with(". It runs with its default policy."), "{note}");
}
