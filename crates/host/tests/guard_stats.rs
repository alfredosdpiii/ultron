//! File-hook guard statistics and records (file-hooks.ts `stats()`, `beginTurn()`, `onRecord`), ported from
//! packages/coding-agent/test/ultron-file-hooks.test.ts with its literal expectations, plus the TS semantics of
//! `GuardRecord` (phase, outcome, ms, display paths, lane, detail), `enabled()` and per-call time limits.

use host::file_hooks::{
    AfterCellReport, AfterOutcome, CellChanges, FileHooks, FileWriteGuard, GuardRecord, GuardStats, Snapshot,
    Snapshotter, Verdict,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(root.join("app.py"), "VALUE = 1\n").unwrap();
    std::fs::write(root.join("notes.txt"), "alpha\n").unwrap();
    (dir, root)
}

/// A clock that advances 10 ms each time it is read: guard times are deterministic.
fn ticking(hooks: &FileHooks) {
    let now = Arc::new(AtomicU64::new(0));
    hooks.set_clock(Arc::new(move || now.fetch_add(10, Ordering::SeqCst)));
}

fn recorded(hooks: &FileHooks) -> Arc<Mutex<Vec<Value>>> {
    let records = Arc::new(Mutex::new(Vec::new()));
    let sink = records.clone();
    hooks.on_record(Arc::new(move |record: &GuardRecord| {
        sink.lock().unwrap().push(record.to_json())
    }));
    records
}

type Seen = Arc<Mutex<Vec<(PathBuf, String, String)>>>;

/// Blocks content with "forbidden", otherwise lets it through with "looks fine".
struct Fake(Seen);

impl FileWriteGuard for Fake {
    fn name(&self) -> &str {
        "Fake"
    }
    fn before_write_on(&self, path: &Path, content: &str, _: &Path, lane: &str) -> Result<Verdict, String> {
        self.0
            .lock()
            .unwrap()
            .push((path.to_path_buf(), content.to_string(), lane.to_string()));
        Ok(if content.contains("forbidden") {
            Verdict::Block("app.py:1: fake/rule: forbidden text".into())
        } else {
            Verdict::Allow(Some("looks fine".into()))
        })
    }
    fn checks_shell(&self) -> bool {
        false
    }
}

fn stats_of(hooks: &FileHooks, index: usize) -> GuardStats {
    hooks.stats()[index].clone()
}

#[test]
fn host_guard_stats_count_checks_and_blocks_like_file_hooks_ts() {
    let (_dir, root) = dir();
    let seen: Seen = Arc::default();
    let mut hooks = FileHooks::new(&root);
    hooks.add(Box::new(Fake(seen.clone())));
    let blocked = hooks.before_write_on("main", "app.py", "VALUE = \"forbidden\"\n");
    assert_eq!(
        blocked,
        json!({"blocked": true, "reason": "[Fake] app.py:1: fake/rule: forbidden text", "notes": []})
    );
    assert_eq!(
        seen.lock().unwrap()[0],
        (
            root.join("app.py"),
            "VALUE = \"forbidden\"\n".to_string(),
            "main".to_string()
        )
    );
    assert_eq!(
        hooks.before_write_on("lane-2", "pkg/new.py", "forbidden = True\n")["blocked"],
        true
    );
    assert_eq!(seen.lock().unwrap()[1].2, "lane-2");
    assert_eq!(
        hooks.before_write_on("main", "app.py", "VALUE = 2\n"),
        json!({"blocked": false, "notes": ["[Fake] looks fine"]})
    );
    hooks.before_write_on("main", "a.txt", "one");
    hooks.before_write_on("main", "b.txt", "two");
    let stats = stats_of(&hooks, 0);
    assert_eq!(
        (stats.name.as_str(), stats.checks, stats.blocked, stats.unchecked),
        ("Fake", 5, 2, 0)
    );
}

/// Blocks shell commands that touch app.py and records what it saw.
struct ShellFake(Arc<Mutex<Vec<(String, PathBuf)>>>);

impl FileWriteGuard for ShellFake {
    fn name(&self) -> &str {
        "Fake"
    }
    fn checks_writes(&self) -> bool {
        false
    }
    fn before_shell(&self, command: &str, cwd: &Path) -> Result<Verdict, String> {
        self.0.lock().unwrap().push((command.to_string(), cwd.to_path_buf()));
        Ok(if command.contains("app.py") {
            Verdict::Block("loki: `redirect` into app.py".into())
        } else {
            Verdict::Allow(None)
        })
    }
}

#[test]
fn host_guard_stats_count_shell_checks_and_skip_commands_that_write_nothing() {
    let (_dir, root) = dir();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut hooks = FileHooks::new(&root);
    hooks.add(Box::new(ShellFake(seen.clone())));
    assert_eq!(
        hooks.before_shell_on("main", "echo VALUE = 2 > app.py").as_deref(),
        Some("[Fake] loki: `redirect` into app.py")
    );
    assert_eq!(
        hooks.before_shell_on("main", "echo beta >> notes.txt && cat notes.txt"),
        None
    );
    assert_eq!(
        hooks.before_shell_on("main", "grep -c VALUE app.py 2>/dev/null | tail -1"),
        None
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("echo VALUE = 2 > app.py".to_string(), root.clone()),
            ("echo beta >> notes.txt && cat notes.txt".to_string(), root.clone()),
        ]
    );
    let stats = stats_of(&hooks, 0);
    assert_eq!((stats.checks, stats.blocked), (2, 1));
}

/// Never answers (until the test ends): its time limit decides.
struct Hung(Mutex<Receiver<()>>, Duration);

impl FileWriteGuard for Hung {
    fn name(&self) -> &str {
        "Slow"
    }
    fn timeout(&self) -> Option<Duration> {
        Some(self.1)
    }
    fn before_write(&self, _: &Path, _: &str, _: &Path) -> Result<Verdict, String> {
        let _ = self.0.lock().unwrap().recv();
        Ok(Verdict::Block("too late".into()))
    }
    fn before_shell(&self, _: &str, _: &Path) -> Result<Verdict, String> {
        let _ = self.0.lock().unwrap().recv();
        Ok(Verdict::Block("too late".into()))
    }
}

struct Broken;

impl FileWriteGuard for Broken {
    fn name(&self) -> &str {
        "Broken"
    }
    fn before_write(&self, _: &Path, _: &str, _: &Path) -> Result<Verdict, String> {
        Err("checker crashed".into())
    }
}

#[test]
fn host_guard_that_does_not_answer_in_time_lets_the_command_run() {
    let (_dir, root) = dir();
    let (_release, wait) = channel::<()>();
    let mut hooks = FileHooks::new(&root);
    hooks.add(Box::new(Hung(Mutex::new(wait), Duration::from_millis(200))));
    assert_eq!(hooks.before_shell_on("main", "echo VALUE = 3 > app.py"), None);
    let stats = stats_of(&hooks, 0);
    assert_eq!((stats.checks, stats.unchecked, stats.blocked), (1, 1, 0));
}

#[test]
fn host_guard_that_does_not_answer_in_time_lets_the_write_proceed_with_a_note() {
    let (_dir, root) = dir();
    let (_release, wait) = channel::<()>();
    let mut hooks = FileHooks::new(&root);
    hooks.add(Box::new(Hung(Mutex::new(wait), Duration::from_millis(200))));
    hooks.add(Box::new(Broken));
    assert_eq!(
        hooks.before_write_on("main", "out.txt", "hello"),
        json!({"blocked": false, "notes": [
            "[Slow] Slow did not check this write to out.txt (no answer within 0.2 s); it was written unchecked.",
            "[Broken] Broken did not check this write to out.txt (checker crashed); it was written unchecked."
        ]})
    );
    assert_eq!(
        hooks.stats().iter().map(|s| s.unchecked).collect::<Vec<_>>(),
        vec![1, 1]
    );
    // `Math.round(250 / 100) / 10`: 0.3 s.
    let (_release, wait) = channel::<()>();
    let mut rounded = FileHooks::new(&root);
    rounded.add(Box::new(Hung(Mutex::new(wait), Duration::from_millis(250))));
    assert_eq!(
        rounded.before_write_on("main", "/elsewhere/x.txt", "hello")["notes"][0],
        "[Slow] Slow did not check this write to /elsewhere/x.txt (no answer within 0.3 s); it was written unchecked."
    );
}

/// Disabled, or without a before-write check: never called, never counted, still listed.
struct Off(Arc<AtomicU64>, bool);

impl FileWriteGuard for Off {
    fn name(&self) -> &str {
        "Off"
    }
    fn enabled(&self) -> bool {
        self.1
    }
    fn checks_writes(&self) -> bool {
        !self.1
    }
    fn before_write(&self, _: &Path, _: &str, _: &Path) -> Result<Verdict, String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Verdict::Block("should not run".into()))
    }
}

#[test]
fn host_disabled_guards_are_not_asked_or_counted() {
    let (_dir, root) = dir();
    let calls = Arc::new(AtomicU64::new(0));
    let mut hooks = FileHooks::new(&root);
    hooks.add(Box::new(Off(calls.clone(), false)));
    hooks.add(Box::new(Off(calls.clone(), true)));
    assert_eq!(
        hooks.before_write_on("main", "a.txt", "x"),
        json!({"blocked": false, "notes": []})
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let stats: Vec<(String, u64)> = hooks.stats().into_iter().map(|s| (s.name, s.checks)).collect();
    assert_eq!(stats, vec![("Off".into(), 0), ("Off".into(), 0)]);
}

#[test]
fn host_guard_records_and_turn_times_follow_file_hooks_ts() {
    let (_dir, root) = dir();
    let mut hooks = FileHooks::new(&root);
    hooks.add(Box::new(Fake(Arc::default())));
    hooks.add(Box::new(ShellFake(Arc::default())));
    ticking(&hooks);
    let records = recorded(&hooks);
    hooks.before_write_on("main", "app.py", "forbidden");
    hooks.before_write_on("worker", "/abs/out.txt", "fine");
    let long = format!("echo {} > app.py", "x".repeat(90));
    hooks.before_shell_on("main", &long);
    hooks.before_shell_on("main", "echo ok > notes.txt");
    let shown = format!("{}...", &long[..77]);
    assert_eq!(
        *records.lock().unwrap(),
        vec![
            json!({"guard": "Fake", "phase": "before_write", "outcome": "blocked", "ms": 10, "paths": ["app.py"],
                "lane": "main", "detail": "[Fake] app.py:1: fake/rule: forbidden text"}),
            json!({"guard": "Fake", "phase": "before_write", "outcome": "allowed", "ms": 10,
                "paths": ["/abs/out.txt"], "lane": "worker"}),
            json!({"guard": "Fake", "phase": "before_shell", "outcome": "blocked", "ms": 10, "paths": [shown],
                "lane": "main", "detail": "[Fake] loki: `redirect` into app.py"}),
            json!({"guard": "Fake", "phase": "before_shell", "outcome": "allowed", "ms": 10,
                "paths": ["echo ok > notes.txt"], "lane": "main"}),
        ]
    );
    let times: Vec<(u64, u64)> = hooks.stats().iter().map(|s| (s.total_ms, s.turn_ms)).collect();
    assert_eq!(times, vec![(20, 20), (20, 20)]);
    hooks.begin_turn();
    hooks.before_write_on("main", "a.txt", "x");
    let times: Vec<(u64, u64)> = hooks.stats().iter().map(|s| (s.total_ms, s.turn_ms)).collect();
    assert_eq!(times, vec![(30, 10), (20, 0)]);
    assert_eq!(
        hooks.stats()[0].to_json(),
        json!({"name": "Fake", "checks": 3, "blocked": 1, "unchecked": 0, "afterChecks": 0, "afterFindings": 0,
            "totalMs": 30, "turnMs": 10})
    );
}

/// Every file under `dir` with its content as the signature.
fn snapshotter() -> Snapshotter {
    Arc::new(|dir: &Path| {
        let mut entries = BTreeMap::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            if entry.path().is_file() {
                let name = entry.file_name().to_string_lossy().into_owned();
                entries.insert(name, format!("{:?}", std::fs::read(entry.path()).unwrap()));
            }
        }
        Ok(Snapshot {
            root: dir.to_path_buf(),
            entries,
            complete: true,
        })
    })
}

/// Reports with a fixed outcome.
struct After(&'static str, Result<Option<AfterCellReport>, String>);

impl FileWriteGuard for After {
    fn name(&self) -> &str {
        self.0
    }
    fn checks_writes(&self) -> bool {
        false
    }
    fn watches_cells(&self) -> bool {
        true
    }
    fn after_cell_changes(&self, _: &CellChanges, _: &Path, _: &str) -> Result<Option<AfterCellReport>, String> {
        self.1.clone()
    }
}

#[test]
fn host_after_cell_stats_count_findings_and_failures_not_advisories() {
    let (_dir, root) = dir();
    let mut hooks = FileHooks::with_snapshots(&root, snapshotter());
    let report = |message: &str, outcome| {
        Ok(Some(AfterCellReport {
            message: Some(message.into()),
            outcome,
        }))
    };
    hooks.add(Box::new(After(
        "Finds",
        report("app.py:1: bad", AfterOutcome::Findings),
    )));
    hooks.add(Box::new(After("Advises", report("fyi", AfterOutcome::Advisory))));
    hooks.add(Box::new(After("Broken", Err("engine missing".into()))));
    hooks.add(Box::new(After("Quiet", Ok(None))));
    ticking(&hooks);
    let records = recorded(&hooks);
    hooks.cell_started("main");
    let _ = hooks.before_write_on("main", "app.py", "VALUE = 2\n");
    std::fs::write(root.join("app.py"), "VALUE = 2\n").unwrap();
    std::fs::write(root.join("notes.txt"), "beta\n").unwrap();
    hooks.cell_ended("main");
    hooks.settled();
    let after: Vec<(String, u64, u64)> = hooks
        .stats()
        .into_iter()
        .map(|s| (s.name, s.after_checks, s.after_findings))
        .collect();
    assert_eq!(
        after,
        vec![
            ("Finds".into(), 1, 1),
            ("Advises".into(), 1, 0),
            ("Broken".into(), 1, 1),
            ("Quiet".into(), 1, 0),
        ]
    );
    // Unchecked files first, then checked ones (relative to the session directory).
    let paths = json!(["notes.txt", "app.py"]);
    assert_eq!(
        *records.lock().unwrap(),
        vec![
            json!({"guard": "Finds", "phase": "after_cell", "outcome": "findings", "ms": 40, "paths": paths,
                "lane": "main", "detail": "app.py:1: bad"}),
            json!({"guard": "Advises", "phase": "after_cell", "outcome": "advisory", "ms": 40, "paths": paths,
                "lane": "main", "detail": "fyi"}),
            json!({"guard": "Broken", "phase": "after_cell", "outcome": "unchecked", "ms": 40, "paths": paths,
                "lane": "main", "detail": "Broken could not check the files this cell changed: engine missing"}),
            json!({"guard": "Quiet", "phase": "after_cell", "outcome": "clean", "ms": 40, "paths": paths,
                "lane": "main"}),
        ]
    );
    assert_eq!(
        hooks.take_pending("main").as_deref(),
        Some(
            "[Finds] app.py:1: bad\n[Advises] fyi\n[Broken] Broken could not check the files this cell changed: engine missing"
        )
    );
}

/// Allows everything and records the lane of each write it sees.
struct Lanes(Arc<Mutex<Vec<String>>>);

impl FileWriteGuard for Lanes {
    fn name(&self) -> &str {
        "Lanes"
    }
    fn before_write_on(&self, _: &Path, _: &str, _: &Path, lane: &str) -> Result<Verdict, String> {
        self.0.lock().unwrap().push(lane.to_string());
        Ok(Verdict::Allow(None))
    }
    fn before_shell(&self, command: &str, _: &Path) -> Result<Verdict, String> {
        Ok(Verdict::Block(format!("no: {command}")))
    }
}

/// A kernel request of a lane other than the root's: its writes reach the guards as that lane's, and the records
/// of its writes and shell commands carry it (session-worker.ts passes the requesting lane to `beforeWrite`).
#[test]
fn host_requests_check_writes_and_commands_as_the_requesting_lane() {
    let (dir, root) = dir();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let mut hooks = FileHooks::new(&root);
    hooks.add(Box::new(Lanes(seen.clone())));
    let records = recorded(&hooks);
    let host = host::Host::new(
        &root,
        dir.path().join("jobs"),
        dir.path().join("spill"),
        Box::new(hooks.clone()),
    );
    let payload = |value: Value| value.as_object().unwrap().clone();
    let signal = kernel::Signal::new();
    let answer = host
        .handle_lane(
            "worker-1",
            "files.before_write",
            payload(json!({"writes": [{"path": "a.txt", "content": "x"}]})),
            &signal,
        )
        .unwrap();
    assert_eq!(answer, json!({"results": [{"blocked": false, "notes": []}]}));
    let refused = host.handle_lane(
        "worker-1",
        "bash",
        payload(json!({"command": "echo x > a.txt"})),
        &signal,
    );
    assert_eq!(
        refused,
        Err("bash command was not run: [Lanes] no: echo x > a.txt".to_string())
    );
    assert_eq!(*seen.lock().unwrap(), vec!["worker-1".to_string()]);
    let lanes: Vec<Value> = records.lock().unwrap().iter().map(|r| r["lane"].clone()).collect();
    assert_eq!(lanes, vec![json!("worker-1"), json!("worker-1")]);
    assert_eq!(host.guard_stats()[0]["checks"], 2);
}
