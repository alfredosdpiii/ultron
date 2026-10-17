//! A child lane's compaction summary, as the TypeScript harness makes it (`compactWithRequest` of
//! packages/agent/src/harness/compaction/compaction.ts, replayed from fixtures/child-compaction.json written by
//! fixtures/child-compaction.mjs): the update prompt over a previous summary, the split turn's prefix summary, the
//! "No prior history." of a split turn with nothing before it, custom instructions, reasoning, and the file lists.

use ai::{Message, SystemContent, UserContent, UserMessageContent};
use compaction::{CompactOptions, FileOperations};
use harness::chat::child_summary::{SummaryPlan, compose_summary, summary_requests};
use serde_json::Value;

const NOW: i64 = 1_760_000_000_000;

fn fixture() -> Vec<Value> {
    serde_json::from_str(include_str!("../fixtures/child-compaction.json")).unwrap()
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

fn system_text(messages: &[Message]) -> String {
    messages
        .iter()
        .find_map(|m| match m {
            Message::System(system) => Some(match &system.content {
                SystemContent::Text(text) => text.clone(),
                other => serde_json::to_value(other).unwrap().to_string(),
            }),
            _ => None,
        })
        .expect("a system prompt")
}

fn user_text(messages: &[Message]) -> String {
    messages
        .iter()
        .find_map(|m| match m {
            Message::User(user) => Some(match &user.content {
                UserMessageContent::Text(text) => text.clone(),
                UserMessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| match b {
                        UserContent::Text(t) => Some(t.text.clone()),
                        _ => None,
                    })
                    .collect(),
            }),
            _ => None,
        })
        .expect("a user message")
}

fn model(value: &Value) -> ai::Model {
    let mut model: ai::Model = serde_json::from_value(value.clone()).expect("a model");
    model.max_tokens = value["maxTokens"].as_u64().unwrap();
    model
}

#[test]
fn harness_child_compaction_requests_and_summary_match_the_typescript_harness() {
    let cases = fixture();
    assert_eq!(cases.len(), 5);
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let mut file_ops = FileOperations::default();
        file_ops.read.extend(strings(&case["fileOps"]["read"]));
        file_ops.written.extend(strings(&case["fileOps"]["written"]));
        file_ops.edited.extend(strings(&case["fileOps"]["edited"]));
        let plan = SummaryPlan {
            history: case["history"].as_array().unwrap().clone(),
            prefix: case["prefix"].as_array().unwrap().clone(),
            split: case["splitTurn"] == true,
            previous_summary: case["previousSummary"].as_str().map(str::to_string),
            file_ops,
        };
        let model = model(&case["model"]);
        let options = CompactOptions {
            model: &model,
            custom_instructions: case["instructions"].as_str(),
            thinking_level: case["thinking"].as_str(),
            session_id: None,
            now_ms: NOW,
        };
        let requests = summary_requests(&plan, case["reserveTokens"].as_u64().unwrap(), &options);
        let expected = case["requests"].as_array().unwrap();
        assert_eq!(requests.len(), expected.len(), "{name}");
        for (request, want) in requests.iter().zip(expected) {
            assert_eq!(
                system_text(&request.context.messages),
                want["systemPrompt"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(
                user_text(&request.context.messages),
                want["text"].as_str().unwrap(),
                "{name}"
            );
            assert_eq!(request.max_tokens, want["maxTokens"].as_u64().unwrap(), "{name}");
            assert_eq!(request.reasoning.as_deref(), want["reasoning"].as_str(), "{name}");
        }
        let texts: Vec<String> = (1..=requests.len()).map(|n| format!("RESPONSE-{n}")).collect();
        let (summary, read, modified) = compose_summary(&plan, &texts);
        assert_eq!(summary, case["summary"].as_str().unwrap(), "{name}");
        assert_eq!(read, strings(&case["details"]["readFiles"]), "{name}");
        assert_eq!(modified, strings(&case["details"]["modifiedFiles"]), "{name}");
    }
}
