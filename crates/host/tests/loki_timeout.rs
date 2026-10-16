//! ULTRON_LOKI_TIMEOUT_MS (loki.ts `lokiTimeoutMs`): how long Loki's before-write check may take before the write
//! proceeds unchecked with a note (file-hooks.ts `#timed`). The ported cases of ultron-loki.test.ts, then the limit
//! applied to a real check: a Python file's check runs the CPython parser helper, which no process starts within
//! 1 ms, so a 1 ms limit always lets that write through unchecked, and no limit always lets the check decide.

use host::file_hooks::FileHooks;
use host::loki_guard::{LokiGuard, LokiMode, loki_timeout};
use std::time::Duration;

/// `lokiTimeoutMs({ULTRON_LOKI_TIMEOUT_MS: "250"})` is 250 and an invalid value is ignored; unset (TypeScript: the
/// 5 s default) is no limit for the in-process engine.
#[test]
fn host_loki_timeout_reads_ultron_loki_timeout_ms_like_typescript() {
    assert_eq!(loki_timeout(Some("250")), Some(Duration::from_millis(250)));
    assert_eq!(loki_timeout(Some("soon")), None);
    assert_eq!(loki_timeout(None), None);
    // `Number(value)` then `Number.isSafeInteger(value) && value > 0`.
    assert_eq!(loki_timeout(Some(" 1200 ")), Some(Duration::from_millis(1200)));
    assert_eq!(loki_timeout(Some("1e3")), Some(Duration::from_secs(1)));
    for invalid in ["", "0", "-5", "1.5", "Infinity", "9007199254740992"] {
        assert_eq!(loki_timeout(Some(invalid)), None, "{invalid:?}");
    }
}

/// `before_write_on` of a new Python file under a Loki guard with `limit`.
fn check(limit: Option<Duration>, file: &str, content: &str) -> (serde_json::Value, u64) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut hooks = FileHooks::new(&root);
    hooks.add(Box::new(LokiGuard::new(&root, LokiMode::On).with_timeout(limit)));
    let verdict = hooks.before_write_on("main", file, content);
    (verdict, hooks.stats()[0].unchecked)
}

#[test]
fn host_loki_check_past_ultron_loki_timeout_ms_writes_unchecked() {
    // A text no other test parses, so the parser helper runs for it.
    let (verdict, unchecked) = check(
        loki_timeout(Some("1")),
        "limited.py",
        "LIMITED_BY_ULTRON_LOKI_TIMEOUT_MS = 1\n",
    );
    assert_eq!(verdict["blocked"], false, "{verdict}");
    assert_eq!(
        verdict["notes"],
        serde_json::json!([
            "[Loki] Loki did not check this write to limited.py (no answer within 0 s); it was written unchecked."
        ])
    );
    assert_eq!(unchecked, 1);
    // Without a limit the same kind of write is checked: nothing to say, nothing unchecked.
    let (verdict, unchecked) = check(None, "unlimited.py", "UNLIMITED_LOKI_CHECK = 2\n");
    assert_eq!(verdict, serde_json::json!({"blocked": false, "notes": []}));
    assert_eq!(unchecked, 0);
}
