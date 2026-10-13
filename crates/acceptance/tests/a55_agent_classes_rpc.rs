//! A55: the RPC end-to-end case of packages/coding-agent/test/ultron-a55-agent-classes.test.ts. The real binary runs
//! in RPC mode against a scripted provider: the root model's cells and the class method's model answers are fixed,
//! while the kernel, host, definition registry, task journal and lanes are the shipped session stack's
//! (session-core's composition root wires the typed-agent host into every cell's requests).

mod common;

use common::{Install, Provider, Reply, Request};
use serde_json::{Value, json};

const DEFINE_AND_CALL: &str = r#"import json
from dataclasses import dataclass
from typing import Literal

@dataclass
class Label:
    kind: Literal["bug", "feature"]
    confidence: float

@agent
class Triage:
    """TRIAGE ROLE: you label incoming reports."""
    seen: int = 0
    labels: list[str] = []

    async def classify(self, report: str) -> Label:
        """Label the report as a bug or a feature."""
        ...

    def record(self, label: Label) -> int:
        self.seen += 1
        self.labels.append(label.kind)
        return self.seen

t = Triage("main")
label = await t.classify("the app crashes on start")
t.record(label)
print('RESULT ' + json.dumps({'type': type(label).__name__, 'kind': label.kind, 'confidence': label.confidence, 'seen': t.seen, 'generation': Triage.describe()['generation']}))"#;

const NEXT_CELL: &str = r#"import json
t = Triage("main")
label = await t.classify("please add dark mode")
t.record(label)
print('RESULT ' + json.dumps({'kind': label.kind, 'seen': t.seen, 'labels': t.labels}))"#;

const BAD_RETURN: &str = r#"import json
try:
    await Triage("main").classify("BAD report")
    outcome = {'raised': False}
except AgentCallError as error:
    outcome = {'raised': True, 'message': str(error), 'status': error.result['status'], 'verification': error.result['verification']}
tasks = [{'definition': task['definition'], 'state': task['state'], 'result': task.get('result')} for task in (await agents.tasks())['tasks']]
print('RESULT ' + json.dumps({'outcome': outcome, 'seen': Triage("main").seen, 'tasks': tasks}))"#;

/// The first user message that is not a clock line.
fn first_user(request: &Request) -> String {
    request
        .users()
        .into_iter()
        .find(|u| !u.starts_with("[clock]"))
        .unwrap_or_default()
}

fn script(request: &Request) -> Reply {
    let first = first_user(request);
    if first.starts_with("TRIAGE ROLE") {
        if first.contains("BAD") {
            return Reply::Text(json!({"kind": "urgent", "confidence": 0.4}).to_string());
        }
        let kind = if first.contains("crash") { "bug" } else { "feature" };
        return Reply::Text(json!({"kind": kind, "confidence": 0.9}).to_string());
    }
    if first.starts_with("A55") {
        let steps = [DEFINE_AND_CALL, NEXT_CELL, BAD_RETURN];
        return match steps.get(request.tool_results().len()) {
            Some(code) => Reply::Rlm((*code).into()),
            None => Reply::Text("A55 COMPLETE".into()),
        };
    }
    panic!("Unscripted request: {first}");
}

fn results(messages: &[Value]) -> Vec<Value> {
    common::rpc_tool_results(messages)
        .iter()
        .flat_map(|text| text.lines().map(str::to_string).collect::<Vec<_>>())
        .filter_map(|line| {
            line.strip_prefix("RESULT ")
                .map(|json| serde_json::from_str(json).unwrap())
        })
        .collect()
}

#[test]
fn acceptance_a55_a_class_defined_in_a_cell_is_invokable_typed_end_to_end_keeps_state_and_rejects_a_bad_return() {
    let provider = Provider::start(script);
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[("ULTRON_HINDSIGHT_URL", "off")]);
    client
        .prompt_and_wait("A55: triage two reports with an agent class", common::HANG)
        .unwrap();
    assert_eq!(
        client.get_last_assistant_text().unwrap().as_deref(),
        Some("A55 COMPLETE")
    );
    let results = results(&client.get_messages().unwrap());
    assert_eq!(results.len(), 3, "{results:?}");
    let (defined, next, bad) = (&results[0], &results[1], &results[2]);

    // Typed end to end: the method returned a dataclass built from the validated value.
    assert_eq!(
        defined,
        &json!({"type": "Label", "kind": "bug", "confidence": 0.9, "seen": 1, "generation": 1})
    );
    // State from the first cell is live in the next one, and was shown to the model as data.
    assert_eq!(
        next,
        &json!({"kind": "feature", "seen": 2, "labels": ["bug", "feature"]})
    );
    let requests = provider.requests();
    let methods: Vec<String> = requests
        .iter()
        .map(first_user)
        .filter(|u| u.starts_with("TRIAGE ROLE"))
        .collect();
    assert!(methods.len() >= 3, "{methods:?}");
    assert!(
        methods
            .iter()
            .any(|m| m.contains(r#""args":{"report":"please add dark mode"},"state":{"seen":1,"labels":["bug"]}"#)),
        "{methods:?}"
    );
    // The root model is told about agent classes.
    let root = requests.iter().find(|r| first_user(r).starts_with("A55")).unwrap();
    assert!(root.raw.contains("`@agent` classes"));

    // A bad return raises with the schema error; state is unchanged; the journal records a failure.
    assert_eq!(
        bad["outcome"],
        json!({
            "raised": true,
            "message": "Triage.classify (triage--classify@1) failed: triage--classify@1 output does not match its schema: $.kind must be equal to one of the allowed values (\"bug\", \"feature\")",
            "status": "failed",
            "verification": "unverified",
        })
    );
    assert_eq!(bad["seen"], 2);
    let tasks = bad["tasks"].as_array().unwrap();
    let states: Vec<(String, String)> = tasks
        .iter()
        .map(|t| {
            (
                t["definition"].as_str().unwrap().into(),
                t["state"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        states,
        [
            ("triage--classify@1".to_string(), "completed".to_string()),
            ("triage--classify@1".into(), "completed".into()),
            ("triage--classify@1".into(), "failed".into()),
        ]
    );
    for task in tasks {
        assert_eq!(task["result"]["verification"], "unverified");
    }
    assert!(tasks[2]["result"].get("value").is_none());
    let (_, code) = client.finish(common::HANG);
    assert_eq!(code, Some(0));
}
