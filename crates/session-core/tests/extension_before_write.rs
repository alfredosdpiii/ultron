//! Extensions' `before_file_write` as a file guard (legacy-extension-adapter.ts `fileGuards`, file-hooks.ts
//! `beforeWrite`) and the ULTRON_LOKI_LOG line (loki.ts `lokiLog`): literal expectations from the TS semantics.

use host::Guard;
use host::file_hooks::{FileHooks, FileWriteGuard, GuardRecord, Verdict};
use serde_json::{Value, json};
use session_core::extension_file_hooks::ExtensionBeforeWrite;
use session_core::guard_log::{log_line, loki_log};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[test]
fn session_core_extension_before_write_maps_the_event_answer_to_a_verdict() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let answer: Arc<Mutex<Result<Option<Value>, String>>> = Arc::new(Mutex::new(Ok(None)));
    let (seen, reply) = (events.clone(), answer.clone());
    let guard = ExtensionBeforeWrite::with(
        || true,
        move |event| {
            seen.lock().unwrap().push(event.clone());
            reply.lock().unwrap().clone()
        },
    );
    let check = |value: Result<Option<Value>, String>| {
        *answer.lock().unwrap() = value;
        guard.before_write_on(Path::new("/w/a.txt"), "hi", Path::new("/w"), "worker")
    };
    assert_eq!(
        check(Ok(Some(json!({"block": true, "reason": "probe says no"})))),
        Ok(Verdict::Block("probe says no".into()))
    );
    assert_eq!(
        events.lock().unwrap()[0],
        json!({"type": "before_file_write", "path": "/w/a.txt", "content": "hi", "cwd": "/w", "lane": "worker"})
    );
    assert_eq!(
        check(Ok(Some(json!({"message": "first\nsecond"})))),
        Ok(Verdict::Allow(Some("first\nsecond".into())))
    );
    assert_eq!(check(Ok(None)), Ok(Verdict::Allow(None)));
    assert_eq!(check(Err("handler crashed".into())), Err("handler crashed".to_string()));
    assert_eq!(guard.name(), "extension");
    assert!(!guard.checks_shell() && guard.checks_writes() && !guard.watches_cells());
}

/// Through the session's file hooks: blocks and notes are `[extension] ...`; a handler error writes unchecked with
/// a note; no handler means the guard is not asked and its checks are not counted.
#[test]
fn session_core_extension_before_write_through_file_hooks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let handled = Arc::new(AtomicBool::new(true));
    let flag = handled.clone();
    let mut hooks = FileHooks::new(&root);
    hooks.add(Box::new(ExtensionBeforeWrite::with(
        move || flag.load(Ordering::SeqCst),
        |event| match event["content"].as_str() {
            Some("forbidden") => Ok(Some(json!({"block": true, "reason": "blocked by an extension"}))),
            Some("crash") => Err("Error: boom".into()),
            _ => Ok(Some(json!({"message": "looks fine"}))),
        },
    )));
    assert_eq!(
        hooks.before_write_on("main", "a.txt", "forbidden"),
        json!({"blocked": true, "reason": "[extension] blocked by an extension", "notes": []})
    );
    assert_eq!(
        hooks.before_write_on("main", "a.txt", "ok"),
        json!({"blocked": false, "notes": ["[extension] looks fine"]})
    );
    assert_eq!(
        hooks.before_write_on("main", "a.txt", "crash"),
        json!({"blocked": false, "notes": [
            "[extension] extension did not check this write to a.txt (Error: boom); it was written unchecked."
        ]})
    );
    handled.store(false, Ordering::SeqCst);
    assert_eq!(
        hooks.before_write_on("main", "a.txt", "forbidden"),
        json!({"blocked": false, "notes": []})
    );
    let stats = &hooks.guard_stats()[0];
    assert_eq!(
        (&stats["name"], &stats["checks"], &stats["blocked"], &stats["unchecked"]),
        (&json!("extension"), &json!(3), &json!(1), &json!(1))
    );
}

#[test]
fn session_core_loki_log_lines_are_json_with_at_first() {
    let record = GuardRecord {
        guard: "extension".into(),
        phase: "before_write",
        outcome: "blocked",
        ms: 4,
        paths: vec!["a.txt".into()],
        lane: "main".into(),
        detail: Some("[extension] no".into()),
    };
    assert_eq!(
        log_line(123, &record),
        "{\"at\":123,\"guard\":\"extension\",\"phase\":\"before_write\",\"outcome\":\"blocked\",\"ms\":4,\"paths\":[\"a.txt\"],\"lane\":\"main\",\"detail\":\"[extension] no\"}\n"
    );
    let quiet = GuardRecord {
        detail: None,
        outcome: "allowed",
        ..record.clone()
    };
    assert!(!log_line(1, &quiet).contains("detail"));

    assert!(loki_log(None).is_none());
    assert!(loki_log(Some("  ")).is_none());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("logs/nested/loki.jsonl");
    let log = loki_log(Some(&format!("  {}  ", path.display()))).unwrap();
    log(&record);
    log(&quiet);
    let lines: Vec<Value> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 2);
    assert_eq!(
        (&lines[0]["outcome"], &lines[1]["outcome"]),
        (&json!("blocked"), &json!("allowed"))
    );
    assert!(lines[0]["at"].as_u64().unwrap() > 1_600_000_000_000);
}
