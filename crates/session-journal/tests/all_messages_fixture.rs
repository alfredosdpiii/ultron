//! Replays `fixtures/all-messages.json` (recorded by `fixtures/all-messages.mjs` from TypeScript's
//! `SessionManager.list` / `buildSessionInfo` and `nativeSessionDetails`): the text the session selector searches is
//! every user and assistant message's text, for a format-4 journal and a Pi v3 file alike.

use serde_json::Value;
use session_journal::names::{self, session_details};
use session_journal::repo::read_metadata;

fn fixture() -> Vec<Value> {
    serde_json::from_str(include_str!("../fixtures/all-messages.json")).unwrap()
}

#[test]
fn session_journal_all_messages_text_matches_typescript() {
    let cases = fixture();
    assert_eq!(cases.len(), 8);
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let expected = case["allMessagesText"].as_str().unwrap();
        for (format, text, details) in [
            ("native", &case["nativeText"], &case["nativeDetails"]),
            ("pi", &case["piText"], &case["piDetails"]),
        ] {
            let got = session_details(text.as_str().unwrap());
            assert_eq!(got.all_messages_text, expected, "{name} ({format})");
            assert_eq!(
                got.first_message,
                details["firstMessage"].as_str().unwrap(),
                "{name} ({format})"
            );
            assert_eq!(
                got.message_count as u64,
                details["messageCount"].as_u64().unwrap(),
                "{name} ({format})"
            );
            assert_eq!(got.name.as_deref(), details["name"].as_str(), "{name} ({format})");
        }
        // Where upstream lists the file, Rust's text is exactly upstream's.
        if let Some(info) = case["sessionInfo"].as_object() {
            assert_eq!(info["allMessagesText"].as_str().unwrap(), expected, "{name}");
        }
    }
    // The case upstream drops: Rust keeps the session and searches the messages it can read.
    let dropped: Vec<&str> = cases
        .iter()
        .filter(|c| c["sessionInfo"].is_null())
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(dropped, ["content-upstream-throws-on"]);
}

/// The listing a picker gets (`list_sessions`, what `Op::ListSessions` and the daemon's `describe` send) carries
/// the text of later messages, not only the first.
#[test]
fn session_journal_listing_carries_all_messages_text() {
    let case = fixture()
        .into_iter()
        .find(|c| c["name"] == "later-messages-are-searched")
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("--work-project--");
    std::fs::create_dir_all(&folder).unwrap();
    let path = folder.join("2026-09-21T14-13-20-000Z_11111111-aaaa.jsonl");
    std::fs::write(&path, case["nativeText"].as_str().unwrap()).unwrap();
    let listed = names::list_sessions(dir.path(), "/work/project");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].first_message, "fix the parser");
    assert_eq!(
        listed[0].all_messages_text,
        "fix the parser Done. now the kestrel lexer Kestrel lexer fixed."
    );
    assert_eq!(names::listing(read_metadata(&path).unwrap()), listed[0]);
}
