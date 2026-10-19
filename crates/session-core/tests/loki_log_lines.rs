//! The ULTRON_LOKI_LOG line (loki.ts `lokiLog`): literal expectations from the TS semantics.

use host::file_hooks::GuardRecord;
use serde_json::{Value, json};
use session_core::guard_log::{log_line, loki_log};

#[test]
fn session_core_loki_log_lines_are_json_with_at_first() {
    let record = GuardRecord {
        guard: "loki".into(),
        phase: "before_write",
        outcome: "blocked",
        ms: 4,
        paths: vec!["a.txt".into()],
        lane: "main".into(),
        detail: Some("[loki] no".into()),
    };
    assert_eq!(
        log_line(123, &record),
        "{\"at\":123,\"guard\":\"loki\",\"phase\":\"before_write\",\"outcome\":\"blocked\",\"ms\":4,\"paths\":[\"a.txt\"],\"lane\":\"main\",\"detail\":\"[loki] no\"}\n"
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
