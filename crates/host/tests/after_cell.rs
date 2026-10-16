//! After-cell file hooks (file-hooks.ts `cellStarted` / `cellEnded` / `takePending`, loki.ts `afterCellChanges`),
//! ported from packages/coding-agent/test/ultron-file-hooks.test.ts and ultron-loki.test.ts with their literal
//! expectations: files a cell changed by other means than checked writes reach the guards in the background and
//! their findings arrive with the lane's next cell; Loki's report tiers, its "said once" memory, and the Rust engine
//! catching a secret a shell write introduced.

use host::Guard;
use host::file_hooks::{AfterCellReport, AfterOutcome, CellChanges, FileHooks, FileWriteGuard, Snapshot, Snapshotter};
use host::loki_guard::{LokiGuard, LokiMode, LokiReport, classify_loki_text, parse_loki_report};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// A picture of `dir`: every file under it (not `.git`) with its content as the signature.
fn snapshotter() -> Snapshotter {
    fn walk(root: &Path, dir: &Path, entries: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if entry.file_name() == ".git" {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, entries);
            } else {
                let relative = path.strip_prefix(root).unwrap().to_string_lossy().into_owned();
                entries.insert(relative, format!("{:?}", std::fs::read(&path).unwrap_or_default()));
            }
        }
    }
    Arc::new(|dir: &Path| {
        let mut entries = BTreeMap::new();
        walk(dir, dir, &mut entries);
        Ok(Snapshot {
            root: dir.to_path_buf(),
            entries,
            complete: true,
        })
    })
}

/// Records each cell's changes and reports the unchecked files.
struct Watch(Arc<Mutex<Vec<CellChanges>>>, PathBuf);

impl FileWriteGuard for Watch {
    fn name(&self) -> &str {
        "Watch"
    }
    fn watches_cells(&self) -> bool {
        true
    }
    fn after_cell_changes(&self, changes: &CellChanges, _: &Path, _: &str) -> Result<Option<AfterCellReport>, String> {
        self.0.lock().unwrap().push(changes.clone());
        if changes.files.is_empty() {
            return Ok(None);
        }
        let names: Vec<String> = changes
            .files
            .iter()
            .map(|p| p.strip_prefix(&self.1).unwrap().to_string_lossy().into_owned())
            .collect();
        Ok(Some(AfterCellReport {
            message: Some(format!("unchecked writes: {}", names.join(", "))),
            outcome: AfterOutcome::Findings,
        }))
    }
}

#[test]
fn host_after_cell_changes_reach_the_guards_and_findings_come_with_the_next_cell() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("app.py"), "VALUE = 1\n").unwrap();
    std::fs::write(root.join("notes.txt"), "alpha\n").unwrap();
    let reports = Arc::new(Mutex::new(Vec::new()));
    let mut hooks = FileHooks::with_snapshots(&root, snapshotter());
    hooks.add(Box::new(Watch(reports.clone(), root.clone())));
    // Cell 1: a shell write and a checked edit.
    hooks.cell_started("main");
    std::fs::write(root.join("notes.txt"), "beta\n").unwrap();
    let verdict = hooks.before_write("app.py", "VALUE = 3\n");
    assert_eq!(verdict["blocked"], false);
    std::fs::write(root.join("app.py"), "VALUE = 3\n").unwrap();
    hooks.cell_ended("main");
    hooks.settled();
    {
        let reports = reports.lock().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].files, vec![root.join("notes.txt")]);
        assert_eq!(reports[0].checked, vec![root.join("app.py")]);
    }
    // What the guard found arrives once, with the lane's next cell; other lanes have nothing.
    assert_eq!(hooks.take_pending("other"), None);
    assert_eq!(
        hooks.take_pending("main").as_deref(),
        Some("[Watch] unchecked writes: notes.txt")
    );
    assert_eq!(hooks.take_pending("main"), None);
    // Cell 2 changes nothing: nothing more is reported.
    hooks.cell_started("main");
    hooks.cell_ended("main");
    hooks.settled();
    assert_eq!(reports.lock().unwrap().len(), 1);
    // A checked write changed again afterwards is an unchecked file; a deletion is reported as deleted.
    hooks.cell_started("main");
    let _ = hooks.before_write("app.py", "VALUE = 4\n");
    std::fs::write(root.join("app.py"), "VALUE = 5\n").unwrap();
    std::fs::remove_file(root.join("notes.txt")).unwrap();
    hooks.cell_ended("main");
    hooks.settled();
    let last = reports.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last.files, vec![root.join("app.py")]);
    assert_eq!(last.deleted, vec![root.join("notes.txt")]);
    assert!(last.checked.is_empty() && last.complete);
}

/// A guard that cannot check, and one that gives up.
struct Broken;

impl FileWriteGuard for Broken {
    fn name(&self) -> &str {
        "Broken"
    }
    fn watches_cells(&self) -> bool {
        true
    }
    fn after_cell_changes(&self, _: &CellChanges, _: &Path, _: &str) -> Result<Option<AfterCellReport>, String> {
        Err("engine missing".into())
    }
}

struct Slow(Mutex<std::sync::mpsc::Receiver<()>>);

impl FileWriteGuard for Slow {
    fn name(&self) -> &str {
        "Slow"
    }
    fn watches_cells(&self) -> bool {
        true
    }
    fn after_cell_changes(&self, _: &CellChanges, _: &Path, _: &str) -> Result<Option<AfterCellReport>, String> {
        // Answers only when the test lets it, long after its time limit.
        let _ = self.0.lock().unwrap().recv();
        Ok(None)
    }
    fn after_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(1)
    }
}

#[test]
fn host_after_cell_guards_that_fail_or_time_out_say_so() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    let mut hooks = FileHooks::with_snapshots(&root, snapshotter());
    hooks.add(Box::new(Broken));
    hooks.add(Box::new(Slow(Mutex::new(wait))));
    hooks.cell_started("main");
    std::fs::write(root.join("x.txt"), "x").unwrap();
    hooks.cell_ended("main");
    hooks.settled();
    let _ = release.send(());
    assert_eq!(
        hooks.take_pending("main").as_deref(),
        Some(
            "[Broken] Broken could not check the files this cell changed: engine missing\n[Slow] Slow did not finish checking the files this cell changed."
        )
    );
}

#[test]
fn host_after_cell_needs_a_watching_guard_and_a_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let reports = Arc::new(Mutex::new(Vec::new()));
    // Without snapshots nothing is watched.
    let mut plain = FileHooks::new(&root);
    plain.add(Box::new(Watch(reports.clone(), root.clone())));
    assert!(!plain.watches_cells());
    plain.cell_started("main");
    std::fs::write(root.join("a.txt"), "a").unwrap();
    plain.cell_ended("main");
    plain.settled();
    assert!(reports.lock().unwrap().is_empty());
    // A cell that ends without a baseline only takes one: its changes cannot be told apart.
    let mut late = FileHooks::with_snapshots(&root, snapshotter());
    late.add(Box::new(Watch(reports.clone(), root.clone())));
    std::fs::write(root.join("b.txt"), "b").unwrap();
    late.cell_ended("main");
    late.settled();
    assert!(reports.lock().unwrap().is_empty());
    std::fs::write(root.join("c.txt"), "c").unwrap();
    late.cell_ended("main");
    late.settled();
    assert_eq!(reports.lock().unwrap()[0].files, vec![root.join("c.txt")]);
}

#[test]
fn host_after_cell_findings_are_capped_at_6000_characters() {
    struct Loud;
    impl FileWriteGuard for Loud {
        fn name(&self) -> &str {
            "Loud"
        }
        fn watches_cells(&self) -> bool {
            true
        }
        fn after_cell_changes(&self, _: &CellChanges, _: &Path, _: &str) -> Result<Option<AfterCellReport>, String> {
            Ok(Some(AfterCellReport {
                message: Some("é".repeat(7000)),
                outcome: AfterOutcome::Findings,
            }))
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut hooks = FileHooks::with_snapshots(&root, snapshotter());
    hooks.add(Box::new(Loud));
    hooks.cell_started("main");
    std::fs::write(root.join("x"), "x").unwrap();
    hooks.cell_ended("main");
    hooks.settled();
    let text = hooks.take_pending("main").unwrap();
    // "[Loud] " + 7000 characters = 7007; the first 6000 are kept.
    assert_eq!(
        text,
        format!("[Loud] {}\n… (1007 more characters of findings)", "é".repeat(5993))
    );
}

#[test]
fn host_loki_reports_keep_their_tiers() {
    let report = parse_loki_report(
        &serde_json::json!({
            "loki": "0.1.3", "command": "hook", "status": "blocked",
            "blocking": [{"text": "app.py:3: python-ast: stub body in todo", "path": "app.py", "line": 3}],
            "advisory": [{"text": "app.py:9: loki/slop-complexity: f cyclomatic complexity 9 -> 11 (> 10)"}, {"line": 1}],
            "not_checked": ["NOT CHECKED python: missing ruff"],
        })
        .to_string(),
    );
    assert_eq!(
        report,
        Some(LokiReport {
            blocking: vec!["app.py:3: python-ast: stub body in todo".into()],
            advisory: vec!["app.py:9: loki/slop-complexity: f cyclomatic complexity 9 -> 11 (> 10)".into()],
            not_checked: vec!["NOT CHECKED python: missing ruff".into()],
            error: None,
        })
    );
    assert_eq!(
        parse_loki_report(r#"{"status": "error", "blocking": [], "error": "invalid hook input: x"}"#),
        Some(LokiReport {
            error: Some("invalid hook input: x".into()),
            ..LokiReport::default()
        })
    );
    for not_a_report in [
        r#"{"hookSpecificOutput": {}}"#,
        "[]",
        "loki: post-write check failed",
        "",
    ] {
        assert_eq!(parse_loki_report(not_a_report), None);
    }
    assert_eq!(
        classify_loki_text(
            "loki: post-write check failed; files are already changed.\napp.py:3: python-ast: stub body in todo\napp.py:9: loki/slop-complexity: f cyclomatic complexity 9 -> 11 (> 10); split it into smaller functions\nNOT CHECKED python: missing ruff\n",
            true
        ),
        LokiReport {
            blocking: vec!["app.py:3: python-ast: stub body in todo".into()],
            advisory: vec![
                "app.py:9: loki/slop-complexity: f cyclomatic complexity 9 -> 11 (> 10); split it into smaller functions"
                    .into()
            ],
            not_checked: vec!["NOT CHECKED python: missing ruff".into()],
            error: None,
        }
    );
    assert_eq!(
        classify_loki_text(
            "loki: post-write check failed; files are already changed.\na.ts:1: loki/xss: x\nloki: advisory (not blocking):\na.ts:2: something new\nNOT CHECKED typescript: missing oxlint",
            true
        ),
        LokiReport {
            blocking: vec!["a.ts:1: loki/xss: x".into()],
            advisory: vec!["a.ts:2: something new".into()],
            not_checked: vec!["NOT CHECKED typescript: missing oxlint".into()],
            error: None,
        }
    );
    assert_eq!(
        classify_loki_text(
            "copy.py:1: loki/slop-duplication: lines 1-10 duplicate lib.py:1-10",
            false
        ),
        LokiReport {
            advisory: vec!["copy.py:1: loki/slop-duplication: lines 1-10 duplicate lib.py:1-10".into()],
            ..LokiReport::default()
        }
    );
    assert_eq!(
        classify_loki_text("loki: invalid hook input: bad path", true)
            .error
            .as_deref(),
        Some("loki: invalid hook input: bad path")
    );
}

/// The stand-in engine of ultron-loki.test.ts: each cell writes `file` and the engine answers the scripted report.
struct StandIn {
    root: PathBuf,
    hooks: FileHooks,
    answer: Arc<Mutex<loki::run::Outcome>>,
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    cell: usize,
}

const STUB: &str = "app.py:12: python-ast: stub body in todo";
const HOTSPOT: &str =
    "app.py:20: loki/slop-complexity: f cyclomatic complexity 9 -> 11 (> 10); split it into smaller functions";
const NO_RUFF: &str = "NOT CHECKED python: missing ruff";

impl StandIn {
    fn new(dir: &Path) -> StandIn {
        let answer = Arc::new(Mutex::new(loki::run::Outcome {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        }));
        let calls: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
        let (scripted, seen) = (answer.clone(), calls.clone());
        let guard = LokiGuard::new(dir, LokiMode::On).with_hook(Arc::new(move |_: &Path, files: &[String]| {
            seen.lock().unwrap().push(files.to_vec());
            scripted.lock().unwrap().clone()
        }));
        let mut hooks = FileHooks::with_snapshots(dir, snapshotter());
        hooks.add(Box::new(guard));
        StandIn {
            root: dir.to_path_buf(),
            hooks,
            answer,
            calls,
            cell: 0,
        }
    }

    fn json(&mut self, blocking: &[&str], advisory: &[&str], not_checked: &[&str], file: &str) -> Option<String> {
        let text = |list: &[&str]| {
            list.iter()
                .map(|t| serde_json::json!({ "text": t }))
                .collect::<Vec<_>>()
        };
        let report = serde_json::json!({
            "status": if blocking.is_empty() { "passed" } else { "blocked" },
            "blocking": text(blocking), "advisory": text(advisory), "not_checked": not_checked,
        });
        self.run(
            if blocking.is_empty() { 0 } else { 2 },
            report.to_string(),
            String::new(),
            file,
        )
    }

    fn run(&mut self, status: u8, stdout: String, stderr: String, file: &str) -> Option<String> {
        *self.answer.lock().unwrap() = loki::run::Outcome { status, stdout, stderr };
        self.hooks.cell_started("main");
        self.cell += 1;
        std::fs::write(self.root.join(file), format!("VALUE = {}\n", self.cell)).unwrap();
        self.hooks.cell_ended("main");
        self.hooks.settled();
        self.hooks.take_pending("main")
    }
}

fn stand_in() -> (tempfile::TempDir, StandIn) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let session = StandIn::new(&root);
    (dir, session)
}

#[test]
fn host_loki_after_cell_new_findings_say_fix_these_and_advisories_are_fyi() {
    let (_dir, mut session) = stand_in();
    let told = session.json(&[STUB], &[HOTSPOT], &[NO_RUFF], "app.py");
    assert_eq!(
        told.as_deref(),
        Some(
            [
                "[Loki] 1 new finding from this cell's changes; fix these:",
                STUB,
                "FYI (advisory, not blocking; no change required):",
                HOTSPOT,
                "not checked in this session (said once): python: missing ruff",
            ]
            .join("\n")
            .as_str()
        )
    );
    assert_eq!(session.calls.lock().unwrap()[0], vec!["app.py".to_string()]);
}

#[test]
fn host_loki_after_cell_what_was_said_is_not_said_again() {
    let (_dir, mut session) = stand_in();
    session.json(&[STUB], &[HOTSPOT], &[NO_RUFF], "app.py");
    let moved_stub = STUB.replace(":12:", ":14:");
    let moved_hotspot = HOTSPOT.replace(":20:", ":22:");
    assert_eq!(
        session
            .json(&[&moved_stub], &[&moved_hotspot], &[NO_RUFF], "app.py")
            .as_deref(),
        Some("[Loki] 1 finding reported earlier is still open in app.py.")
    );
    let second = "app.py:30: python-ast: stub body in later";
    assert_eq!(
        session.json(&[STUB, second], &[], &[NO_RUFF], "app.py"),
        Some(format!(
            "[Loki] 1 new finding from this cell's changes; fix these:\n{second}\n1 finding reported earlier is still open in app.py."
        ))
    );
    assert_eq!(session.json(&[], &[], &[NO_RUFF], "app.py"), None);
    assert!(
        session
            .json(&[STUB], &[], &[], "app.py")
            .unwrap()
            .contains("1 new finding from this cell's changes; fix these:")
    );
}

#[test]
fn host_loki_after_cell_advisories_never_say_fix_and_notes_are_said_once() {
    let (_dir, mut session) = stand_in();
    assert_eq!(
        session.json(&[], &[HOTSPOT], &[], "app.py"),
        Some(format!(
            "[Loki] FYI (advisory, not blocking; no change required):\n{HOTSPOT}"
        ))
    );
    assert_eq!(
        session.json(&[], &[], &[NO_RUFF], "other.py").as_deref(),
        Some("[Loki] not checked in this session (said once): python: missing ruff")
    );
    let interpreter = "NOT CHECKED python imports: no project interpreter found (set LOKI_PYTHON or pass --python)";
    assert_eq!(
        session.json(&[], &[], &[NO_RUFF, interpreter], "other.py").as_deref(),
        Some(
            "[Loki] not checked in this session (said once): python imports: no project interpreter found (set LOKI_PYTHON or pass --python)"
        )
    );
    assert_eq!(session.json(&[], &[], &[NO_RUFF], "other.py"), None);
}

#[test]
fn host_loki_after_cell_text_reports_and_engine_errors() {
    let (_dir, mut session) = stand_in();
    let told = session.run(
        2,
        String::new(),
        format!("loki: post-write check failed; files are already changed.\n{STUB}\n{HOTSPOT}\n{NO_RUFF}\n"),
        "app.py",
    );
    assert_eq!(
        told.as_deref(),
        Some(
            [
                "[Loki] 1 new finding from this cell's changes; fix these:",
                STUB,
                "FYI (advisory, not blocking; no change required):",
                HOTSPOT,
                "not checked in this session (said once): python: missing ruff",
            ]
            .join("\n")
            .as_str()
        )
    );
    assert_eq!(session.run(0, String::new(), format!("{NO_RUFF}\n"), "app.py"), None);
    assert_eq!(
        session
            .run(
                2,
                String::new(),
                "loki: invalid hook input: git: cannot resolve HEAD\n".into(),
                "app.py"
            )
            .as_deref(),
        Some(
            "[Loki] Loki could not check the files this cell changed: loki: invalid hook input: git: cannot resolve HEAD"
        )
    );
    // An uncommitted .loki/ pauses post-write checks, said once.
    let paused = session.json(&[".loki/loki.json: protected content changed"], &[], &[], "app.py");
    assert!(
        paused
            .unwrap()
            .starts_with("[Loki] post-write checks are paused: .loki/ has uncommitted changes")
    );
    assert_eq!(
        session.json(&[".loki/loki.json: protected content changed"], &[], &[], "app.py"),
        None
    );
}

#[test]
fn host_loki_after_cell_checks_at_most_40_files_inside_the_repository() {
    let (_dir, session) = stand_in();
    let outside = tempfile::tempdir().unwrap();
    session.hooks.cell_started("main");
    for n in 0..45 {
        std::fs::write(session.root.join(format!("f{n:02}.py")), "x = 1\n").unwrap();
    }
    std::fs::write(outside.path().join("elsewhere.py"), "x").unwrap();
    *session.answer.lock().unwrap() = loki::run::Outcome {
        status: 0,
        stdout: r#"{"status": "passed", "blocking": [], "advisory": [], "not_checked": []}"#.into(),
        stderr: String::new(),
    };
    session.hooks.cell_ended("main");
    session.hooks.settled();
    assert_eq!(
        session.hooks.take_pending("main").as_deref(),
        Some("[Loki] (5 more changed files were not checked)")
    );
    let calls = session.calls.lock().unwrap();
    assert_eq!(calls[0].len(), 40);
    assert_eq!(calls[0][0], "f00.py");
}

fn git(cwd: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

#[test]
fn host_loki_after_cell_rust_engine_catches_a_secret_a_shell_write_introduced() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["config", "user.email", "t@example.com"]);
    git(&root, &["config", "user.name", "T"]);
    loki::scan::install_minimal(root.to_str().unwrap(), false).unwrap();
    std::fs::write(root.join("settings.py"), "DEBUG = False\n").unwrap();
    git(&root, &["add", "-A"]);
    git(&root, &["-c", "commit.gpgsign=false", "commit", "-qm", "init"]);
    let mut hooks = FileHooks::with_snapshots(&root, snapshotter());
    hooks.add(Box::new(LokiGuard::new(&root, LokiMode::On)));
    // A literal that looks like a provider key, written by "bash": only the after-cell check sees it.
    let key = format!("sk-{}", "a1B2c3D4e5F6g7H8i9J0".repeat(2));
    hooks.cell_started("main");
    std::fs::write(
        root.join("settings.py"),
        format!("DEBUG = False\nAPI_KEY = \"{key}\"\n"),
    )
    .unwrap();
    hooks.cell_ended("main");
    hooks.settled();
    let findings = hooks.take_pending("main").expect("Loki reported the secret");
    assert!(
        findings.starts_with("[Loki] 1 new finding from this cell's changes; fix these:"),
        "{findings}"
    );
    assert!(findings.contains("loki/secret"), "{findings}");
    assert!(!findings.contains(&key), "the secret itself is never echoed");
}

#[test]
fn host_loki_outside_a_repository_runs_only_before_write_checks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let ran = Arc::new(Mutex::new(0));
    let counter = ran.clone();
    let guard = LokiGuard::new(&root, LokiMode::On)
        .with_post_write(false)
        .with_hook(Arc::new(move |_: &Path, _: &[String]| {
            *counter.lock().unwrap() += 1;
            loki::run::Outcome {
                status: 2,
                stdout: String::new(),
                stderr: "x.py:1: rule: finding\n".into(),
            }
        }));
    let mut hooks = FileHooks::with_snapshots(&root, snapshotter());
    hooks.add(Box::new(guard));
    assert!(!hooks.watches_cells());
    hooks.cell_started("main");
    std::fs::write(root.join("x.py"), "x = 1\n").unwrap();
    hooks.cell_ended("main");
    hooks.settled();
    assert_eq!(hooks.take_pending("main"), None);
    assert_eq!(*ran.lock().unwrap(), 0);
}
