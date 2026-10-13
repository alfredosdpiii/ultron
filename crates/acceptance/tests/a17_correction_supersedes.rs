//! A17 (verified): a correction supersedes stale claims; hypotheses remain distinguishable.
//! Ported from packages/coding-agent/test/ultron-a17-correction-supersedes.test.ts over the Rust `MemoryService`
//! with one durable journal reopened by each fresh session, through the real Hindsight adapter
//! (`hindsight::HindsightBackend`) over the fake Hindsight of `memory_support`, as the TS fake sits behind `createHindsightBackend`.

mod memory_support;

use kernel_apis::MemoryDocumentStore;
use memory_support::*;
use serde_json::{Value, json};
use std::sync::Arc;

const STALE: &str = "The Pi launcher is /usr/local/bin/pi.";
const CORRECTED: &str = "The Pi launcher is ~/.local/bin/pi (a wrapper script), not /usr/local/bin/pi.";
const NODE: &str = "The project pins Node 22.";

fn column(results: &Value, keys: &[&str]) -> Vec<Value> {
    results
        .as_array()
        .unwrap()
        .iter()
        .map(|item| Value::Array(keys.iter().map(|k| item[*k].clone()).collect()))
        .collect()
}

#[test]
fn acceptance_a17_corrected_claim_is_current_and_stale_claim_superseded_in_fresh_sessions() {
    let hindsight = FakeHindsight::new();
    let store = durable_store();

    // Session 1: the assistant guesses the launcher path; a tool observes the Node version.
    let first = open(&hindsight, &store, keep_gate());
    let guess = first
        .propose(
            &p(json!({"text": STALE, "evidence": [{"ref": "assistant:session-1:turn-3"}], "scope": "project"})),
            &sig(),
        )
        .unwrap();
    let observed = first
        .propose(
            &p(
                json!({"text": NODE, "evidence": [{"ref": "tool:node --version", "sha256": sha256("v22.4.0")}],
                "scope": "project", "evidenceClass": "tool_evidence"}),
            ),
            &sig(),
        )
        .unwrap();
    assert_match(
        &guess,
        &json!({"evidenceClass": "hypothesis", "gate": {"action": "keep"}}),
    );
    assert_eq!(observed["evidenceClass"], "tool_evidence");
    first.get(&p(json!({"id": guess["memoryId"]})), &sig()).unwrap();
    first.get(&p(json!({"id": observed["memoryId"]})), &sig()).unwrap();

    // Session 2: the hypothesis is recalled but visibly marked as unconfirmed.
    let second = open(&hindsight, &store, keep_gate());
    let before = second
        .prepare(
            &p(json!({"query": "where is the Pi launcher", "scope": "project", "taskId": "task-2"})),
            &sig(),
        )
        .unwrap();
    assert_eq!(
        column(&before["results"], &["memoryId", "evidenceClass"]),
        vec![
            json!([guess["memoryId"], "hypothesis"]),
            json!([observed["memoryId"], "tool_evidence"])
        ]
    );
    let context = before["context"].as_str().unwrap();
    assert!(context.contains(&format!("[unconfirmed hypothesis] {STALE}")));
    assert!(context.contains(&format!("[tool evidence] {NODE}")));

    // A user correction is a user statement, not verified tool evidence.
    let correction = second
        .correct(
            &p(json!({"id": guess["memoryId"], "text": CORRECTED, "evidence": [{"ref": "user:session-2:message-7"}]})),
            &sig(),
        )
        .unwrap();
    assert_match(
        &correction,
        &json!({"kind": "correct", "memoryId": guess["memoryId"], "evidenceClass": "user_statement",
            "supersedes": guess["id"], "state": "accepted"}),
    );
    let read = second.get(&p(json!({"id": guess["memoryId"]})), &sig()).unwrap();
    assert_match(&read, &json!({"state": "stored", "content": CORRECTED}));

    // The backend index still returns the replaced unit. It must not win.
    hindsight.lock().stale.push(json!({
        "id": "stale-unit", "text": STALE, "tags": tags("project"), "type": "world", "document_id": guess["memoryId"],
        "metadata": {"ultron_operation": guess["id"], "ultron_evidence_class": "hypothesis"},
    }));

    // Session 3: prepare, why and list all present the correction as current.
    let third = open(&hindsight, &store, keep_gate());
    let after = third
        .prepare(
            &p(json!({"query": "where is the Pi launcher", "scope": "project", "taskId": "task-3"})),
            &sig(),
        )
        .unwrap();
    assert_eq!(
        column(&after["results"], &["memoryId", "evidenceClass", "text"]),
        vec![
            json!([guess["memoryId"], "user_statement", CORRECTED]),
            json!([observed["memoryId"], "tool_evidence", NODE])
        ]
    );
    let context = after["context"].as_str().unwrap();
    assert!(context.contains(&format!("[user statement] {CORRECTED}")));
    assert!(!context.lines().any(|line| line.ends_with(STALE)));

    let why = third.why(&json!("task-3")).unwrap()[0].clone();
    let references = why["references"].as_array().unwrap();
    assert_eq!(references.len(), 2);
    assert!(references.iter().all(|r| r["id"].is_string()));
    assert_eq!(
        references
            .iter()
            .map(|r| json!([
                r["textHash"],
                r["memoryId"],
                r["evidenceClass"],
                r.as_object().unwrap().len()
            ]))
            .collect::<Vec<_>>(),
        vec![
            json!([sha256(CORRECTED), guess["memoryId"], "user_statement", 4]),
            json!([sha256(NODE), observed["memoryId"], "tool_evidence", 4]),
        ]
    );
    assert_eq!(
        why["excluded"],
        json!([{"id": "stale-unit", "memoryId": guess["memoryId"], "reason": "superseded"}])
    );

    let list = third.list().unwrap();
    let claims: Vec<&Value> = list
        .as_array()
        .unwrap()
        .iter()
        .filter(|op| op["memoryId"] == guess["memoryId"] && op["phase"] == "backend")
        .collect();
    let propose = claims.iter().find(|op| op["kind"] == "propose").unwrap();
    let correct = claims.iter().find(|op| op["kind"] == "correct").unwrap();
    // History is linked, not rewritten: the stale claim keeps its own hash and class.
    assert_match(
        propose,
        &json!({"evidenceClass": "hypothesis", "textHash": sha256(STALE), "state": "stored", "supersededBy": correct["id"]}),
    );
    assert_match(
        correct,
        &json!({"evidenceClass": "user_statement", "textHash": sha256(CORRECTED), "supersedes": propose["id"], "state": "stored"}),
    );
    assert!(correct.get("supersededBy").is_none());
    let observed_op = list
        .as_array()
        .unwrap()
        .iter()
        .find(|op| op["id"] == observed["id"])
        .unwrap();
    assert!(observed_op.get("supersededBy").is_none());

    // Derived supersession is never written into the durable journal.
    let journal = stored(&store).to_string();
    assert!(!journal.contains("supersededBy"));
    assert!(!journal.contains("launcher"));
}

#[test]
fn acceptance_a17_evidence_classes_stay_distinct_and_forged_journals_are_refused() {
    let hindsight = FakeHindsight::new();
    let store = durable_store();
    let memory = open(&hindsight, &store, keep_gate());
    let classes = ["hypothesis", "user_statement", "tool_evidence", "verified"];
    for class in classes {
        memory
            .propose(
                &p(json!({"text": format!("claim {class}"), "evidence": [{"ref": format!("e:{class}")}], "evidenceClass": class})),
                &sig(),
            )
            .unwrap();
    }
    let unmarked = memory
        .propose(
            &p(json!({"text": "claim unmarked", "evidence": [{"ref": "e:none"}]})),
            &sig(),
        )
        .unwrap();
    assert_eq!(unmarked["evidenceClass"], "hypothesis");

    // The class travels with the write.
    let metadata: Vec<Value> = hindsight
        .retains()
        .iter()
        .map(|b| b["items"][0]["metadata"].clone())
        .collect();
    let expected: Vec<&str> = classes.iter().copied().chain(["hypothesis"]).collect();
    assert_eq!(metadata.len(), expected.len());
    for (metadata, class) in metadata.iter().zip(expected) {
        assert_eq!(metadata.as_object().unwrap().len(), 2);
        assert!(metadata["ultron_operation"].is_string());
        assert_eq!(metadata["ultron_evidence_class"], class);
    }
    let prepared = open(&hindsight, &store, keep_gate())
        .prepare(&p(json!({"query": "claims", "taskId": "task"})), &sig())
        .unwrap();
    let labels: Vec<&str> = prepared["context"].as_str().unwrap().split('\n').skip(1).collect();
    assert_eq!(
        labels,
        [
            "1. [unconfirmed hypothesis] claim hypothesis",
            "2. [user statement] claim user_statement",
            "3. [tool evidence] claim tool_evidence",
            "4. [verified conclusion] claim verified",
            "5. [unconfirmed hypothesis] claim unmarked",
        ]
    );
    let error = memory
        .propose(
            &p(json!({"text": "x", "evidence": [{"ref": "e"}], "evidenceClass": "confirmed"})),
            &sig(),
        )
        .unwrap_err();
    assert_eq!(error.code, "INVALID_INPUT");

    // A supersession link to another document's claim is rejected on reopen rather than trusted.
    let journal = stored(&store);
    let operations = journal["operations"].as_array().unwrap();
    let first = operations
        .iter()
        .find(|op| op["kind"] == "propose" && op["phase"] == "backend")
        .unwrap();
    let target = operations.iter().find(|op| op["id"] == unmarked["id"]).unwrap();
    let with_correction = |supersedes: &Value| {
        let mut added = target.clone();
        added["id"] = "added-correction".into();
        added["kind"] = "correct".into();
        added["supersedes"] = supersedes.clone();
        let mut document = journal.clone();
        document["operations"].as_array_mut().unwrap().push(added);
        let reopened = Arc::new(MemoryDocumentStore::new(Some(document)));
        open(&hindsight, &reopened, keep_gate()).list()
    };
    // Control: a link to the same document's earlier claim loads.
    assert!(with_correction(&target["id"]).is_ok());
    assert_eq!(with_correction(&first["id"]).unwrap_err().code, "INVALID_JOURNAL");
}
