//! Replays fixtures/bug_summary.json (recorded from `generateBugReportSummary` of core/bug-report.ts by
//! fixtures/bug_summary.mjs): the same conversation, hint, model and thinking level make the same summary request
//! (system prompt, prompt text, token cap, reasoning, routing session), and the same response gives the same report
//! text or the same error.

use ai::{AssistantMessage, Model};
use compaction::{bug_summary_request, bug_summary_text};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/bug_summary.json")).unwrap()
}

#[test]
fn compaction_bug_summary_requests_and_outcomes_match_typescript() {
    let fixture = fixture();
    let now = fixture["now"].as_i64().unwrap();
    let cases = fixture["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 15);
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let model: Model = serde_json::from_value(case["model"].clone()).unwrap();
        let request = bug_summary_request(
            case["messages"].as_array().unwrap(),
            case["hint"].as_str(),
            &model,
            case["thinkingLevel"].as_str(),
            case["sessionId"].as_str(),
            now,
        );
        // What completeSummarization sends: the caller's options, no cache writes, a fresh routing id without one.
        let mut options = json!({"maxTokens": request.max_tokens, "apiKey": "key",
            "sessionId": request.session_id.as_deref().unwrap_or("<fresh>"), "cacheRetention": "none"});
        if let Some(reasoning) = &request.reasoning {
            options["reasoning"] = json!(reasoning);
        }
        let recorded = &case["requests"][0];
        assert_eq!(case["requests"].as_array().unwrap().len(), 1, "{name}");
        assert_eq!(
            serde_json::to_value(&request.context).unwrap(),
            recorded["context"],
            "{name} context"
        );
        assert_eq!(options, recorded["options"], "{name} options");
        let response: AssistantMessage = serde_json::from_value(case["response"].clone()).unwrap();
        let outcome = match bug_summary_text(&response) {
            Ok(text) => json!({"ok": text}),
            Err(error) => json!({"error": error}),
        };
        assert_eq!(outcome, case["outcome"], "{name} outcome");
    }
}

/// `selectMessages` keeps the newest message even when it alone is over the budget, and notes what it left out.
#[test]
fn compaction_bug_summary_keeps_the_newest_message_and_notes_the_rest() {
    let fixture = fixture();
    let case = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "newest-only")
        .unwrap();
    let prompt = case["requests"][0]["context"]["messages"][1]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        prompt.starts_with("Note: only the last 1 of 3 messages are shown.\n\n<conversation>\n[User]: now it hangs")
    );
}
