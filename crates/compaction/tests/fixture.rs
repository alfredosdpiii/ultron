//! Replays crates/compaction/fixtures/compaction.json, recorded from the TypeScript compaction module by
//! fixtures/generate.mjs, and requires identical decisions, requests and results.

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use ai::transcript::ContextUsageEstimate;
use ai::{AssistantMessage, Model, Usage};
use compaction::{
    BranchSummaryError, BranchSummaryOptions, CompactOptions, CompactionSettings, FileOperations, SummaryRequest,
    collect_entries_for_branch_summary, compact, compute_file_lists, convert_to_llm, estimate_context_tokens,
    estimate_projected_context_tokens, estimate_tokens, find_cut_point, find_turn_start_index, format_file_operations,
    generate_branch_summary, prepare_branch_entries, prepare_compaction, serialize_conversation, should_compact,
};
use serde_json::{Value, json};
use session_file::{Env, Leaf, SessionManager, build_session_projection};

fn fixture() -> Value {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/compaction.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

/// Polls a future whose model calls complete immediately.
fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

fn u(value: &Value) -> u64 {
    value.as_u64().unwrap()
}

fn index(value: &Value) -> Option<usize> {
    (value.as_i64().unwrap() >= 0).then(|| value.as_u64().unwrap() as usize)
}

fn estimate_json(estimate: &ContextUsageEstimate) -> Value {
    json!({
        "tokens": estimate.tokens,
        "usageTokens": estimate.usage_tokens,
        "trailingTokens": estimate.trailing_tokens,
        "lastUsageIndex": estimate.last_usage_index,
    })
}

fn ops_json(ops: &FileOperations) -> Value {
    json!({ "read": ops.read, "written": ops.written, "edited": ops.edited })
}

fn entries_of(case: &Value) -> Vec<Value> {
    case["entries"].as_array().unwrap().clone()
}

#[test]
fn compaction_token_estimates_match_typescript() {
    for (name, case) in fixture()["decisions"].as_object().unwrap() {
        let entries = entries_of(case);
        let projection = build_session_projection(&entries, Leaf::Last);
        let tokens: Vec<u64> = projection.messages.iter().map(estimate_tokens).collect();
        assert_eq!(json!(tokens), case["messageTokens"], "{name} projected message tokens");
        let raw: Vec<Value> = entries
            .iter()
            .filter(|e| e["type"] == "message")
            .map(|e| e["message"].clone())
            .collect();
        let tokens: Vec<u64> = raw.iter().map(estimate_tokens).collect();
        assert_eq!(json!(tokens), case["rawMessageTokens"], "{name} raw message tokens");
        assert_eq!(
            estimate_json(&estimate_context_tokens(&projection.messages)),
            case["contextEstimate"],
            "{name} context estimate"
        );
        assert_eq!(
            estimate_json(&estimate_projected_context_tokens(&projection, &entries)),
            case["projectedEstimate"],
            "{name} projected estimate"
        );
    }
}

#[test]
fn compaction_serialized_conversations_match_typescript() {
    for (name, case) in fixture()["decisions"].as_object().unwrap() {
        let messages: Vec<Value> = entries_of(case)
            .iter()
            .filter(|e| e["type"] == "message")
            .map(|e| e["message"].clone())
            .collect();
        assert_eq!(
            serialize_conversation(&convert_to_llm(&messages)),
            case["serialized"].as_str().unwrap(),
            "{name}"
        );
    }
}

#[test]
fn compaction_cut_points_match_typescript() {
    for (name, case) in fixture()["decisions"].as_object().unwrap() {
        let entries = entries_of(case);
        for cut in case["cutPoints"].as_array().unwrap() {
            let keep = u(&cut["keep"]);
            let got = find_cut_point(&entries, 0, entries.len(), keep);
            let want = &cut["result"];
            assert_eq!(
                got.first_kept_entry_index as u64,
                u(&want["firstKeptEntryIndex"]),
                "{name} keep {keep}"
            );
            assert_eq!(
                got.turn_start_index,
                index(&want["turnStartIndex"]),
                "{name} keep {keep}"
            );
            assert_eq!(got.is_split_turn, want["isSplitTurn"], "{name} keep {keep}");
        }
        let turn_starts: Vec<Option<usize>> = case["turnStarts"].as_array().unwrap().iter().map(index).collect();
        let got: Vec<Option<usize>> = (0..entries.len())
            .map(|i| find_turn_start_index(&entries, i, 0))
            .collect();
        assert_eq!(got, turn_starts, "{name} turn starts");
    }
}

#[test]
fn compaction_preparations_match_typescript() {
    for (name, case) in fixture()["decisions"].as_object().unwrap() {
        let entries = entries_of(case);
        for prep in case["preparations"].as_array().unwrap() {
            let keep = u(&prep["keep"]);
            let settings = CompactionSettings {
                enabled: true,
                reserve_tokens: 16384,
                keep_recent_tokens: keep,
            };
            let got = prepare_compaction(&entries, settings).map(|p| {
                json!({
                    "firstKeptEntryId": p.first_kept_entry_id,
                    "messagesToSummarize": p.messages_to_summarize,
                    "turnPrefixMessages": p.turn_prefix_messages,
                    "isSplitTurn": p.is_split_turn,
                    "tokensBefore": p.tokens_before,
                    "previousSummary": p.previous_summary,
                    "fileOps": ops_json(&p.file_ops),
                })
            });
            assert_eq!(got.unwrap_or(Value::Null), prep["prep"], "{name} keep {keep}");
        }
    }
}

#[test]
fn compaction_branch_preparations_match_typescript() {
    for (name, case) in fixture()["decisions"].as_object().unwrap() {
        let entries = entries_of(case);
        for want in case["branchPrep"].as_array().unwrap() {
            let budget = want["budget"].as_i64().unwrap();
            let got = prepare_branch_entries(&entries, budget);
            let got = json!({
                "budget": budget,
                "messages": got.messages,
                "totalTokens": got.total_tokens,
                "fileOps": ops_json(&got.file_ops),
            });
            assert_eq!(&got, want, "{name} budget {budget}");
        }
    }
}

#[test]
fn compaction_branch_budget_keeps_summaries_under_ninety_percent_like_typescript() {
    for case in fixture()["summaryBudget"].as_array().unwrap() {
        let budget = case["budget"].as_i64().unwrap();
        let got = prepare_branch_entries(&entries_of(case), budget);
        assert_eq!(json!(got.messages), case["messages"], "budget {budget}");
    }
}

#[test]
fn compaction_should_compact_and_file_lists_match_typescript() {
    let fixture = fixture();
    for case in fixture["shouldCompact"].as_array().unwrap() {
        let settings = CompactionSettings {
            enabled: case["enabled"].as_bool().unwrap(),
            reserve_tokens: u(&case["reserve"]),
            keep_recent_tokens: 1,
        };
        assert_eq!(
            should_compact(u(&case["tokens"]), u(&case["window"]), &settings),
            case["result"].as_bool().unwrap(),
            "{case}"
        );
    }
    for case in fixture["fileLists"].as_array().unwrap() {
        let set = |key: &str| {
            case["ops"][key]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        let ops = FileOperations {
            read: set("read"),
            written: set("written"),
            edited: set("edited"),
        };
        let (read, modified) = compute_file_lists(&ops);
        assert_eq!(json!({ "readFiles": read, "modifiedFiles": modified }), case["lists"]);
        assert_eq!(
            format_file_operations(&read, &modified),
            case["formatted"].as_str().unwrap()
        );
    }
}

/// A scripted model: records each request as upstream's `streamFn` saw it and replays the recorded responses.
fn scripted(responses: &[Value], requests: &mut Vec<Value>) -> impl AsyncFnMut(SummaryRequest) -> AssistantMessage {
    let responses: Vec<AssistantMessage> = responses
        .iter()
        .map(|r| serde_json::from_value(r.clone()).unwrap())
        .collect();
    async move |request: SummaryRequest| {
        let mut options = json!({ "maxTokens": request.max_tokens });
        if let Some(reasoning) = &request.reasoning {
            options["reasoning"] = json!(reasoning);
        }
        options["apiKey"] = json!("key");
        options["sessionId"] = json!(request.session_id.as_deref().unwrap_or("<fresh>"));
        options["cacheRetention"] = json!("none");
        requests.push(json!({ "context": request.context, "options": options }));
        responses[(requests.len() - 1).min(responses.len() - 1)].clone()
    }
}

/// Upstream requests without the model id, with option keys in a fixed order.
fn recorded_requests(case: &Value) -> Vec<Value> {
    case["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let o = &r["options"];
            let mut options = json!({ "maxTokens": o["maxTokens"] });
            if let Some(reasoning) = o.get("reasoning") {
                options["reasoning"] = reasoning.clone();
            }
            for key in ["apiKey", "sessionId", "cacheRetention"] {
                options[key] = o[key].clone();
            }
            assert_eq!(r["model"], "sum-1");
            json!({ "context": r["context"], "options": options })
        })
        .collect()
}

fn usage_of(value: &Value) -> Usage {
    serde_json::from_value(value.clone()).unwrap()
}

#[test]
fn compaction_compact_requests_and_results_match_typescript() {
    let fixture = fixture();
    let now = fixture["now"].as_i64().unwrap();
    for case in fixture["compactCases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let entries = entries_of(&fixture["decisions"][case["transcript"].as_str().unwrap()]);
        let settings = CompactionSettings {
            enabled: true,
            reserve_tokens: u(&case["reserveTokens"]),
            keep_recent_tokens: u(&case["keep"]),
        };
        let prep = prepare_compaction(&entries, settings).unwrap();
        let model: Model = serde_json::from_value(case["model"].clone()).unwrap();
        let options = CompactOptions {
            model: &model,
            custom_instructions: case["customInstructions"].as_str(),
            thinking_level: case["thinkingLevel"].as_str(),
            session_id: case["sessionId"].as_str(),
            now_ms: now,
        };
        let mut requests = Vec::new();
        let result = block_on(compact(
            &prep,
            &options,
            scripted(case["responses"].as_array().unwrap(), &mut requests),
        ));
        assert_eq!(requests, recorded_requests(case), "{name} requests");
        match (result, case["outcome"].get("ok")) {
            (Ok(got), Some(want)) => {
                assert_eq!(got.summary, want["summary"].as_str().unwrap(), "{name}");
                assert_eq!(
                    got.first_kept_entry_id,
                    want["firstKeptEntryId"].as_str().unwrap(),
                    "{name}"
                );
                assert_eq!(got.tokens_before, u(&want["tokensBefore"]), "{name}");
                assert_eq!(got.usage, usage_of(&want["usage"]), "{name}");
                assert_eq!(json!(got.read_files), want["details"]["readFiles"], "{name}");
                assert_eq!(json!(got.modified_files), want["details"]["modifiedFiles"], "{name}");
            }
            (Err(error), None) => assert_eq!(error, case["outcome"]["error"].as_str().unwrap(), "{name}"),
            (got, _) => panic!("{name}: outcome {got:?} differs from {}", case["outcome"]),
        }
    }
}

struct FixedEnv;

impl Env for FixedEnv {
    fn now_ms(&mut self) -> i64 {
        0
    }
    fn random_uuid(&mut self) -> String {
        "00000000-0000-4000-8000-000000000000".to_string()
    }
    fn session_id(&mut self) -> String {
        "session".to_string()
    }
    fn agent_dir(&self) -> String {
        "/nonexistent-agent-dir".to_string()
    }
}

#[test]
fn compaction_branch_collection_matches_typescript() {
    let fixture = fixture();
    let mut file = vec![
        json!({ "type": "session", "version": 3, "id": "s", "timestamp": "2025-01-01T00:00:00.000Z", "cwd": "/w" }),
    ];
    file.extend(fixture["tree"].as_array().unwrap().iter().cloned());
    let session = SessionManager::in_memory(Box::new(FixedEnv), "/w", None, Some(file)).unwrap();
    for case in fixture["collect"].as_array().unwrap() {
        let (entries, ancestor) =
            collect_entries_for_branch_summary(&session, case["from"].as_str(), case["to"].as_str().unwrap());
        assert_eq!(json!(entries), case["result"]["entries"], "{case}");
        assert_eq!(json!(ancestor), case["result"]["commonAncestorId"], "{case}");
    }
}

#[test]
fn compaction_branch_summary_requests_and_results_match_typescript() {
    let fixture = fixture();
    let now = fixture["now"].as_i64().unwrap();
    for case in fixture["branchCases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let model: Model = serde_json::from_value(case["model"].clone()).unwrap();
        let options = BranchSummaryOptions {
            model: &model,
            custom_instructions: case["customInstructions"].as_str(),
            replace_instructions: case["replaceInstructions"].as_bool().unwrap_or(false),
            reserve_tokens: case["reserveTokens"].as_u64().unwrap_or(16384),
            now_ms: now,
        };
        let mut requests = Vec::new();
        let entries = entries_of(case);
        let result = block_on(generate_branch_summary(
            &entries,
            &options,
            scripted(case["responses"].as_array().unwrap(), &mut requests),
        ));
        assert_eq!(requests, recorded_requests(case), "{name} requests");
        let want = &case["outcome"];
        match result {
            Ok(got) => {
                assert_eq!(got.summary, want["summary"].as_str().unwrap(), "{name}");
                assert_eq!(got.usage, want.get("usage").map(usage_of), "{name}");
                assert_eq!(
                    json!(got.read_files),
                    *want.get("readFiles").unwrap_or(&json!([])),
                    "{name}"
                );
                assert_eq!(
                    json!(got.modified_files),
                    *want.get("modifiedFiles").unwrap_or(&json!([])),
                    "{name}"
                );
            }
            Err(BranchSummaryError::Aborted) => assert_eq!(want["aborted"], true, "{name}"),
            Err(BranchSummaryError::Failed(error)) => assert_eq!(want["error"], error, "{name}"),
        }
    }
}
