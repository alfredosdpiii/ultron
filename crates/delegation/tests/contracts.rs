//! Delegation briefs and contracts (rewriteplan "Workflow design items", from Sakana MASS) over the typed-agent host:
//! `agents.invoke` / `agents.spawn` and workflow nodes take a `brief` (role, instruction, contract), the host stamps
//! its hop, puts role and instruction ahead of an `rlm` lane's prompt, and verifies every contract check on the result
//! before it is committed: a result that misses a check is a failure naming it, and the evidence (each check with
//! `ok`) is in the result's `check.contract`. New in Rust: no TypeScript counterpart; literal expectations.

use delegation::agents::{AdapterRequest, Agents, AgentsOptions, LaneReply, LaneRun};
use delegation::contract::{Brief, Check};
use delegation::task_store::MemoryStore;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

struct Host {
    agents: Agents,
    prompts: Arc<Mutex<Vec<(String, String)>>>,
}

/// Deterministic `answer` returns its input; `rlm` lanes reply the JSON at the end of their prompt's input line, or
/// "ok".
fn host() -> Host {
    let prompts: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let mut options = AgentsOptions::new(Box::new(MemoryStore::default()));
    options.definition_store = Some(Box::new(MemoryStore::default()));
    options.deterministic = Some(Arc::new(|request: AdapterRequest<'_>| Ok(request.input.clone())));
    let said = prompts.clone();
    options.lanes = Some(Arc::new(move |run: LaneRun| {
        said.lock().unwrap().push((run.lane.clone(), run.prompt.clone()));
        let text = match run.prompt.find("Input data:\n") {
            Some(at) => run.prompt[at + 12..].lines().next().unwrap_or_default().to_string(),
            None => "ok".to_string(),
        };
        Ok(LaneReply {
            text,
            ..LaneReply::default()
        })
    }));
    let (agents, _) = Agents::new(options);
    let any = json!({});
    let register = |id: &str, strategy: &str| {
        let definition = json!({
            "id": id, "version": "1", "strategy": strategy, "instructions": format!("Run {id}."),
            "inputSchema": any, "outputSchema": any, "maxRepairs": 0, "inputDescription": "any",
            "outputDescription": "any",
        });
        agents
            .handle("agents.register", json!({"definition": definition}), "main", None)
            .unwrap();
    };
    register("answer", "deterministic");
    register("worker", "rlm");
    let strict = json!({
        "id": "strict", "version": "1", "strategy": "deterministic", "instructions": "Return z.",
        "inputSchema": any, "outputSchema": {"type": "object", "required": ["z"]}, "maxRepairs": 0,
        "inputDescription": "any", "outputDescription": "{z}",
    });
    agents
        .handle("agents.register", json!({"definition": strict}), "main", None)
        .unwrap();
    Host { agents, prompts }
}

impl Host {
    fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        self.agents.handle(kind, payload, "main", None)
    }
}

fn brief(contract: Value) -> Value {
    json!({"role": "checker", "instruction": "Answer exactly.", "contract": contract})
}

#[test]
fn delegation_contract_checks_hold_on_the_dotted_field_they_name() {
    let result = json!({"status": "succeeded", "value": {"passed": true, "items": ["a", "b"], "note": "all good",
        "empty": "", "none": null}, "verdict": {"status": "passed"}});
    let check = |value: Value| Check::parse(&value, 0).unwrap();
    let cases = [
        (json!({"check": "equals", "path": "value.passed", "value": true}), true),
        (
            json!({"check": "equals", "path": "value.passed", "value": false}),
            false,
        ),
        (
            json!({"check": "equals", "path": "verdict.status", "value": "passed"}),
            true,
        ),
        (
            json!({"check": "equals", "path": "value.missing", "value": null}),
            false,
        ),
        (json!({"check": "present", "path": "value.items"}), true),
        (json!({"check": "present", "path": "value.none"}), false),
        (json!({"check": "present", "path": "value.missing"}), false),
        (json!({"check": "nonempty", "path": "value.items"}), true),
        (json!({"check": "nonempty", "path": "value.empty"}), false),
        (json!({"check": "nonempty", "path": "value.passed"}), false),
        (
            json!({"check": "contains", "path": "value.note", "value": "good"}),
            true,
        ),
        (
            json!({"check": "contains", "path": "value.note", "value": "bad"}),
            false,
        ),
        (json!({"check": "contains", "path": "value.items", "value": "b"}), true),
        (json!({"check": "contains", "path": "value.items", "value": "c"}), false),
        (json!({"check": "equals", "path": "value.items.1", "value": "b"}), true),
        (json!({"check": "equals", "path": "value.items.2", "value": "b"}), false),
    ];
    for (written, holds) in cases {
        assert_eq!(check(written.clone()).holds(&result), holds, "{written}");
    }
}

#[test]
fn delegation_contract_briefs_are_validated_and_the_hop_is_the_hosts() {
    let refused = |value: Value, hop: Option<u32>, text: &str| {
        let error = Brief::parse(&value, hop).unwrap_err();
        assert!(error.contains(text), "{error} does not contain {text}");
    };
    refused(json!("x"), Some(1), "brief must be an object");
    refused(json!({"goal": "x"}), Some(1), "Unknown brief field: goal");
    refused(json!({"role": " "}), Some(1), "brief.role must be a nonempty string");
    refused(
        json!({"instruction": 3}),
        Some(1),
        "brief.instruction must be a nonempty string",
    );
    refused(json!({"contract": {}}), Some(1), "brief.contract must be an array");
    refused(
        json!({"contract": [{"check": "maybe", "path": "value"}]}),
        Some(1),
        "contract[0].check must be one of",
    );
    refused(
        json!({"contract": [{"check": "equals", "path": "value"}]}),
        Some(1),
        "contract[0].value is required",
    );
    refused(
        json!({"contract": [{"check": "present", "path": "a..b"}]}),
        Some(1),
        "contract[0].path must be a dotted path",
    );
    refused(
        json!({"contract": [{"check": "present", "path": "value", "value": 1}]}),
        Some(1),
        "Unknown contract check field: value",
    );
    let many: Vec<Value> = (0..33).map(|_| json!({"check": "present", "path": "value"})).collect();
    refused(json!({"contract": many}), Some(1), "at most 32 checks");
    refused(json!({"hop": 1}), Some(2), "brief.hop is 2 here; it is set by the host");
    let parsed = Brief::parse(&json!({"role": "r", "hop": 2}), Some(2)).unwrap();
    assert_eq!((parsed.role.as_deref(), parsed.hop), (Some("r"), 2));
}

#[test]
fn delegation_contract_a_result_that_meets_its_contract_carries_the_evidence() {
    let h = host();
    let result = h
        .call(
            "agents.invoke",
            json!({"definition": "answer@1", "input": {"passed": true, "files": ["a.rs"]},
                   "brief": brief(json!([{"check": "equals", "path": "value.passed", "value": true},
                                         {"check": "contains", "path": "value.files", "value": "a.rs"}]))}),
        )
        .unwrap();
    assert_eq!(
        result,
        json!({
            "status": "succeeded", "value": {"passed": true, "files": ["a.rs"]}, "verification": "unverified",
            "check": {"contract": {"role": "checker", "instruction": "Answer exactly.", "hop": 1, "passed": true,
                "checks": [
                    {"check": "equals", "path": "value.passed", "value": true, "ok": true},
                    {"check": "contains", "path": "value.files", "value": "a.rs", "ok": true},
                ]}},
        })
    );
}

#[test]
fn delegation_contract_a_result_that_misses_a_check_fails_naming_it_and_the_journal_keeps_the_failure() {
    let h = host();
    let result = h
        .call(
            "agents.invoke",
            json!({"definition": "answer@1", "input": {"passed": false},
                   "brief": brief(json!([{"check": "present", "path": "value.passed"},
                                         {"check": "equals", "path": "value.passed", "value": true}]))}),
        )
        .unwrap();
    assert_eq!(result["status"], "failed");
    assert_eq!(
        result["error"],
        r#"Contract not met: {"check":"equals","path":"value.passed","value":true}"#
    );
    assert!(result.get("value").is_none());
    assert_eq!(result["check"]["contract"]["passed"], false);
    assert_eq!(
        result["check"]["contract"]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["ok"].clone())
            .collect::<Vec<_>>(),
        [json!(true), json!(false)]
    );
    let tasks = h.call("agents.tasks", json!({})).unwrap();
    assert_eq!(tasks["tasks"][0]["state"], "failed");
    // A failed run is not checked: there is nothing it claimed.
    let invalid = h
        .call(
            "agents.invoke",
            json!({"definition": "strict@1", "input": {"x": 1},
                   "brief": brief(json!([{"check": "present", "path": "value"}]))}),
        )
        .unwrap();
    assert_eq!(invalid["status"], "failed");
    assert!(invalid.get("check").is_none(), "{invalid}");
}

#[test]
fn delegation_contract_role_and_instruction_lead_the_rlm_prompt_and_the_hop_grows_with_nesting() {
    let h = host();
    let outer = h
        .call(
            "agents.invoke",
            json!({"definition": "worker@1", "input": {"n": 1},
                   "brief": brief(json!([{"check": "equals", "path": "value.n", "value": 1}]))}),
        )
        .unwrap();
    assert_eq!(outer["status"], "succeeded", "{outer}");
    let (lane, prompt) = h.prompts.lock().unwrap()[0].clone();
    assert!(
        prompt.starts_with(
            "Role: checker\nInstruction: Answer exactly.\nContract (the host checks your result against these): {\"check\":\"equals\",\"path\":\"value.n\",\"value\":1}\n\nRun worker.\n\nInput data:\n{\"n\":1}"
        ),
        "{prompt}"
    );
    // A task started from that task's lane is one hop further down; a brief claiming otherwise is refused.
    let nested = h
        .agents
        .handle(
            "agents.invoke",
            json!({"definition": "answer@1", "input": {"ok": true}, "brief": {"role": "inner"}}),
            &lane,
            None,
        )
        .unwrap();
    assert_eq!(nested["check"]["contract"]["hop"], 2);
    let claimed = h
        .agents
        .handle(
            "agents.invoke",
            json!({"definition": "answer@1", "input": {}, "brief": {"hop": 1}}),
            &lane,
            None,
        )
        .unwrap_err();
    assert_eq!(claimed, "brief.hop is 2 here; it is set by the host");
}

#[test]
fn delegation_contract_a_retried_key_with_another_contract_is_another_task() {
    let h = host();
    let first = json!({"definition": "answer@1", "input": {"a": 1}, "key": "k",
                       "brief": brief(json!([{"check": "present", "path": "value.a"}]))});
    h.call("agents.invoke", first.clone()).unwrap();
    assert_eq!(h.call("agents.invoke", first).unwrap()["status"], "succeeded");
    let other = json!({"definition": "answer@1", "input": {"a": 1}, "key": "k",
                       "brief": brief(json!([{"check": "present", "path": "value.b"}]))});
    assert_eq!(
        h.call("agents.invoke", other).unwrap_err(),
        "Idempotency key reused for a different task"
    );
}

#[test]
fn delegation_contract_workflow_nodes_carry_briefs_and_a_missed_check_skips_the_dependents() {
    let h = host();
    let run = h
        .call(
            "workflows.run",
            json!({"nodes": [
                {"id": "a", "definition": "answer@1", "input": {"score": 3},
                 "brief": brief(json!([{"check": "equals", "path": "value.score", "value": 3}]))},
                {"id": "b", "definition": "answer@1", "input": {"score": 1},
                 "brief": brief(json!([{"check": "equals", "path": "value.score", "value": 3}]))},
                {"id": "c", "definition": "answer@1", "dependsOn": ["b"], "inputFrom": "b"},
            ]}),
        )
        .unwrap();
    let nodes = &run;
    assert_eq!(nodes["a"]["status"], "succeeded", "{run}");
    assert_eq!(nodes["a"]["check"]["contract"]["passed"], true);
    assert_eq!(nodes["b"]["status"], "failed");
    assert_eq!(
        nodes["b"]["error"],
        r#"Contract not met: {"check":"equals","path":"value.score","value":3}"#
    );
    assert_ne!(nodes["c"]["status"], "succeeded");
    // An invalid node brief is a validation error before anything runs.
    let invalid = h
        .call(
            "workflows.run",
            json!({"nodes": [{"id": "a", "definition": "answer@1", "input": {}, "brief": {"contract": [{"check": "x"}]}}]}),
        )
        .unwrap_err();
    assert!(
        invalid.starts_with("Workflow node a: brief.contract[0].check"),
        "{invalid}"
    );
    // a and b ran; c was skipped without admission; the invalid workflow admitted nothing.
    assert_eq!(
        h.call("agents.tasks", json!({})).unwrap()["tasks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
