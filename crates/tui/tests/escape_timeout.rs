//! The lone-ESC wait: replays fixtures/escape-timeout.json (packages/tui/src/terminal.ts `resolveEscapeTimeoutMs`,
//! recorded by fixtures/generate-escape-timeout.mjs) and checks the input buffer waits that long.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;
use tui::stdin_buffer::StdinBuffer;
use tui::terminal::resolve_escape_timeout;

#[test]
fn tui_escape_timeout_matches_ts_resolve_escape_timeout_ms() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/escape-timeout.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let env: HashMap<String, String> = case["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        let got = resolve_escape_timeout(&|k| env.get(k).cloned());
        let want = Duration::from_secs_f64(case["ms"].as_f64().unwrap() / 1000.0);
        assert_eq!(got, want, "{case}");
    }
}

/// `new StdinBuffer({ escapeTimeout })`: a lone ESC waits the configured time; other partial sequences keep 50 ms.
#[test]
fn tui_stdin_buffer_waits_the_configured_escape_timeout() {
    let mut buf = StdinBuffer::new().with_escape_timeout(Duration::from_millis(100));
    assert!(buf.process("\x1b").is_empty());
    assert_eq!(buf.flush_after(), Some(Duration::from_millis(100)));
    assert_eq!(buf.flush(), vec!["\x1b".to_string()]);
    assert!(buf.process("\x1b[").is_empty());
    assert_eq!(buf.flush_after(), Some(Duration::from_millis(50)));
    let mut plain = StdinBuffer::new();
    assert!(plain.process("\x1b").is_empty());
    assert_eq!(plain.flush_after(), Some(Duration::from_millis(10)));
}
