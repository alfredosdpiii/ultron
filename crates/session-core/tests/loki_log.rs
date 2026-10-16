//! `ULTRON_LOKI_LOG` (loki.ts `lokiLog`) gets the session's Loki setup record before any guard outcome, as
//! `setupLoki` writes it: `{at, phase: "setup", outcome: "on", mode, engine, reports, projectPython, ms}`
//! (ultron-loki.test.ts: `{phase: "setup", engine: "bundled", reports: "json"}`, and `projectPython` naming
//! ULTRON_LOKI_PROJECT_PYTHON). With Loki off nothing is logged.

mod p_session_support;

use p_session_support::{Setup, actor, in_order, text, tool};
use serde_json::Value;

fn records(vars: Vec<(&'static str, String)>, log: &std::path::Path) -> Vec<Value> {
    let a = actor(
        in_order(vec![
            tool("toolu_w", "print(await write('notes.md', 'hi'))"),
            text("done"),
        ]),
        Setup {
            vars,
            ..Setup::default()
        },
    );
    assert!(a.prompt("go")["error"].is_null());
    drop(a);
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn session_core_loki_log_starts_with_the_setup_record_like_typescript() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("loki.jsonl");
    let lines = records(vec![("ULTRON_LOKI_LOG", log.display().to_string())], &log);
    let setup = &lines[0];
    let keys: Vec<&str> = setup.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "at",
            "phase",
            "outcome",
            "mode",
            "engine",
            "reports",
            "projectPython",
            "ms"
        ]
    );
    assert_eq!(
        (&setup["phase"], &setup["outcome"], &setup["mode"]),
        (&"setup".into(), &"on".into(), &"on".into())
    );
    assert_eq!(
        (&setup["engine"], &setup["reports"]),
        (&"bundled".into(), &"json".into())
    );
    assert_eq!(setup["projectPython"], Value::Null);
    assert!(setup["at"].as_u64().unwrap() > 0 && setup["ms"].is_u64());
    // Then the guard outcomes.
    assert_eq!(lines[1]["phase"], "before_write", "{lines:?}");
    assert_eq!(lines[1]["guard"], "Loki");
}

#[test]
fn session_core_loki_log_setup_names_the_project_python_and_advise_mode() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("loki.jsonl");
    let missing = dir.path().join("no-such-python").display().to_string();
    let lines = records(
        vec![
            ("ULTRON_LOKI_LOG", log.display().to_string()),
            ("ULTRON_LOKI_PROJECT_PYTHON", missing.clone()),
            ("ULTRON_LOKI", "advise".into()),
        ],
        &log,
    );
    assert_eq!(lines[0]["projectPython"], missing);
    assert_eq!(lines[0]["mode"], "advise");
}

#[test]
fn session_core_loki_log_is_empty_with_loki_off() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("loki.jsonl");
    let lines = records(
        vec![
            ("ULTRON_LOKI_LOG", log.display().to_string()),
            ("ULTRON_LOKI", "off".into()),
        ],
        &log,
    );
    assert!(lines.is_empty(), "{lines:?}");
}

/// ULTRON_LOKI_TIMEOUT_MS reaches the session's Loki guard: a Python file's check (the CPython parser helper never
/// starts within 1 ms) lets the write through unchecked, logged as `unchecked`.
#[test]
fn session_core_loki_timeout_ms_reaches_the_session_guard() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("loki.jsonl");
    let lines = {
        let a = actor(
            in_order(vec![
                tool(
                    "toolu_w",
                    "print(await write('timed.py', 'SESSION_LOKI_TIMEOUT_CHECK = 1\\n'))",
                ),
                text("done"),
            ]),
            Setup {
                vars: vec![
                    ("ULTRON_LOKI_LOG", log.display().to_string()),
                    ("ULTRON_LOKI_TIMEOUT_MS", "1".into()),
                ],
                ..Setup::default()
            },
        );
        assert!(a.prompt("go")["error"].is_null());
        let results = p_session_support::tool_results(a.requests().last().unwrap());
        assert!(
            results[0]
                .to_string()
                .contains("Loki did not check this write to timed.py (no answer within 0 s)"),
            "{results:?}"
        );
        drop(a);
        std::fs::read_to_string(&log).unwrap()
    };
    let outcome: Value = serde_json::from_str(lines.lines().nth(1).unwrap()).unwrap();
    assert_eq!(
        (&outcome["phase"], &outcome["outcome"]),
        (&"before_write".into(), &"unchecked".into())
    );
}
