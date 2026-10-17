//! The resume check (format-version.ts `assertSessionFormatsReadable`, ported from ultron-format-versions.test.ts
//! "resume check"): opening a session whose Ultron values were written in a format this build does not know fails
//! up front, naming the value and its version, and leaves the session file as it was.

mod p_session_support;
mod reopen_support;

use p_session_support::text;
use reopen_support::Home;
use serde_json::{Value, json};
use session_journal::{SessionRepo, SystemClock, Write};

/// The session with `values` set (`namespace|key` addresses, as the TypeScript fake session names them).
fn home_with(values: &[(&str, Value)]) -> Home {
    let home = Home::new(None);
    let repo = SessionRepo::new(&home.sessions(), SystemClock::shared());
    let metadata = repo.list(None).into_iter().next().unwrap();
    let mut session = repo.open(&metadata).unwrap();
    let writes: Vec<Write> = values
        .iter()
        .map(|(address, value)| {
            let (namespace, key) = address.split_once('|').unwrap();
            Write::set(namespace, key, value.clone())
        })
        .collect();
    session.commit(&writes).unwrap();
    home
}

fn session_file(home: &Home) -> Vec<Value> {
    home.writes()
}

fn open_error(home: &Home) -> Option<String> {
    match home.try_open(Box::new(|_, _| text("done")), &[], None) {
        Ok(worker) => {
            worker.close();
            None
        }
        Err(error) => Some(error),
    }
}

#[test]
fn session_core_format_version_resumes_current_and_unversioned_values() {
    let home = home_with(&[
        ("ultron.tasks|root", json!({"version": 1, "tasks": []})),
        ("ultron.usage|root", json!({"roots": {}})),
        ("ultron.module|grants", json!({"version": 1, "grants": []})),
        ("ultron.module|skills", json!({"format": 1})),
        (
            "ultron.local|ultron.refinements",
            json!({"formatVersion": 1, "events": []}),
        ),
    ]);
    assert_eq!(open_error(&home), None);
}

#[test]
fn session_core_format_version_newer_values_stop_the_resume_like_typescript() {
    let cases = [
        (
            "ultron.tasks|root",
            json!({"version": 2, "tasks": []}),
            "ultron.tasks/root has format version 2",
        ),
        (
            "ultron.memory.state|root",
            json!({"version": 7}),
            "ultron.memory.state/root has format version 7",
        ),
        (
            "ultron.module|progress",
            json!({"version": 2}),
            "ultron.module/progress has format version 2",
        ),
        (
            "ultron.module|skills",
            json!({"format": 2}),
            "ultron.module/skills has format version 2",
        ),
        (
            "ultron.local|ultron.experiments",
            json!({"formatVersion": 2}),
            "ultron.local/ultron.experiments has format version 2",
        ),
        (
            "ultron.definitions|root",
            json!({"version": 3}),
            "ultron.definitions/root has format version 3",
        ),
        (
            "ultron.module|stats",
            json!({"version": "1"}),
            "ultron.module/stats has format version \"1\"",
        ),
        (
            "ultron.local|ultron.artifact-refs",
            json!({"formatVersion": 2, "refs": {}}),
            "ultron.local/ultron.artifact-refs has format version 2",
        ),
    ];
    for (address, document, message) in cases {
        let home = home_with(&[(address, document)]);
        let before = session_file(&home);
        let error = open_error(&home).unwrap_or_else(|| panic!("{address} resumed"));
        assert!(error.contains(message), "{address}: {error}");
        // Nothing was settled or rewritten.
        assert_eq!(session_file(&home), before, "{address}");
    }
}

/// The full UnsupportedFormatVersionError text.
#[test]
fn session_core_format_version_error_text_matches_typescript() {
    let home = home_with(&[("ultron.module|goal", json!({"version": 2, "goal": null}))]);
    assert_eq!(
        open_error(&home).unwrap(),
        "Cannot resume: session value ultron.module/goal has format version 2, but this Ultron reads only format \
         version 1. It was left unchanged; open the session with the Ultron version that wrote it, or upgrade."
    );
}
