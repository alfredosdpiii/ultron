//! A55 (the registry cases): a real Python kernel whose host requests go to the Rust typed-agent host (definition
//! registry, journal, validation) with scripted model lanes. Ports of the invocation, bounded-preview, bad-return,
//! subclass-role, durable-field, snapshot and generation cases of packages/coding-agent/test/ultron-agent-classes.test.ts.

mod agents_support;
mod common;

use agents_support::{Fixture, Options, fixture, lock};
use common::{null_handler, runtime};
use kernel::{ExecutionResult, Kernel, KernelOptions, Status};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;

type Reply = Arc<dyn Fn(&str, &Value) -> String + Send + Sync>;

/// Scripted lane: answers by the method named in the derived instructions.
fn lane_script(reply: Reply) -> agents_support::Script {
    Arc::new(move |_, prompt, _, _| {
        let method = prompt
            .split("You implement the method `")
            .nth(1)
            .and_then(|rest| rest.split('(').next())
            .and_then(|name| name.split('.').nth(1))
            .ok_or_else(|| format!("Unscripted prompt: {}", &prompt[..prompt.len().min(120)]))?;
        let input = prompt
            .split("\nInput data:\n")
            .nth(1)
            .and_then(|rest| rest.split("\n\nOutput contract:\n").next())
            .ok_or("no input")?;
        Ok(reply(method, &serde_json::from_str(input).unwrap()))
    })
}

struct Setup {
    fixture: Fixture,
    spill: tempfile::TempDir,
    snapshots: tempfile::TempDir,
}

fn setup(reply: Reply) -> Setup {
    let fixture = fixture(Options {
        script: Some(lane_script(reply)),
        ..Options::default()
    });
    Setup {
        fixture,
        spill: tempfile::tempdir().unwrap(),
        snapshots: tempfile::tempdir().unwrap(),
    }
}

impl Setup {
    fn kernel(&self) -> Kernel {
        let mut options = KernelOptions::new(env!("CARGO_MANIFEST_DIR"), runtime());
        options.snapshot_key = Some(vec![5; 32]);
        options
            .env
            .push(("ULTRON_RLM_SPILL_DIR".into(), self.spill.path().display().to_string()));
        Kernel::new(options, Arc::new(self.fixture.agents.handler("main", null_handler()))).unwrap()
    }
    fn snapshot_path(&self) -> PathBuf {
        self.snapshots.path().join("main.snapshot")
    }
    fn prompts(&self) -> Vec<String> {
        lock(&self.fixture.prompts).iter().map(|(_, p)| p.clone()).collect()
    }
}

fn execute(kernel: &Kernel, code: &str) -> ExecutionResult {
    kernel.execute(code, None, None).unwrap()
}

fn run(kernel: &Kernel, code: &str) -> String {
    let result = execute(kernel, code);
    assert_eq!(result.status, Status::Ok, "cell failed: {:?}\n{code}", result.error);
    result.stdout
}

fn evalue(kernel: &Kernel, code: &str) -> String {
    execute(kernel, code).error.expect("an error").evalue
}

const TRIAGE: &str = r#"
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
"#;

fn label_reply() -> Reply {
    Arc::new(|method, input| {
        assert_eq!(method, "classify");
        let report = input["args"]["report"].as_str().unwrap_or_default();
        json!({ "kind": if report.contains("crash") { "bug" } else { "feature" }, "confidence": 0.9 }).to_string()
    })
}

#[test]
fn acceptance_a55_a_model_driven_method_is_one_typed_agents_invoke_docstrings_are_the_prompt_arguments_and_fields_are_data()
 {
    let s = setup(label_reply());
    let k = s.kernel();
    let out = run(
        &k,
        &format!(
            "{TRIAGE}\nt = Triage(\"main\")\nlabel = await t.classify(\"the app crashes on start\")\nprint(type(label).__name__, label.kind, label.confidence, t.record(label), Triage.describe()[\"generation\"])"
        ),
    );
    assert_eq!(out, "Label bug 0.9 1 1\n");
    let prompts = s.prompts();
    assert_eq!(prompts.len(), 1);
    let prompt = &prompts[0];
    for part in [
        "TRIAGE ROLE: you label incoming reports.",
        "You implement the method `Triage.classify(report: str) -> Label`.",
        "Label the report as a bug or a feature.",
        "Input data:\n{\"args\":{\"report\":\"the app crashes on start\"},\"state\":{\"seen\":0,\"labels\":[]}}",
        "One JSON value of type {kind: \"bug\" | \"feature\", confidence: number}",
    ] {
        assert!(prompt.contains(part), "{part}\n{prompt}");
    }
    // The instructions carry no argument values: arguments stay in the data channel.
    assert!(!prompt.split("Input data:").next().unwrap().contains("crashes"));
    let tasks = s.fixture.journal();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["definition"], "triage--classify@1");
    assert_eq!(tasks[0]["state"], "completed");
    assert_eq!(
        tasks[0]["result"],
        json!({ "status": "succeeded", "value": { "kind": "bug", "confidence": 0.9 }, "verification": "unverified" })
    );
    let listed = s.fixture.call("agents.list", json!({})).unwrap();
    let derived = listed
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == "triage--classify")
        .unwrap();
    assert_eq!(
        (derived["version"].clone(), derived["strategy"].clone()),
        (json!("1"), json!("rlm"))
    );
    assert!(
        derived["inputDescription"]
            .as_str()
            .unwrap()
            .contains("args: {report: string")
    );
    k.shutdown();
    s.fixture.agents.close();
}

#[test]
fn acceptance_a55_large_arguments_reach_the_model_as_bounded_previews_with_a_file_holding_the_full_value() {
    let s = setup(Arc::new(|_, input| {
        let doc = &input["args"]["doc"];
        let path = doc["path"].as_str().unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        json!(format!("{}:{}", doc["bytes"], text.encode_utf16().count())).to_string()
    }));
    let k = s.kernel();
    let out = run(
        &k,
        "@agent\nclass Reader:\n    \"\"\"Summarize documents.\"\"\"\n    async def summarize(self, doc: str) -> str:\n        \"\"\"Summarize the document.\"\"\"\n        ...\nbig = \"x\" * 200_000\nprint(await Reader().summarize(big))",
    );
    assert_eq!(out, "200002:200002\n");
    let prompt = &s.prompts()[0];
    assert!(prompt.encode_utf16().count() < 8_000);
    assert!(prompt.contains("\"$preview\":{\"type\":\"builtins.str\",\"length\":200000"));
    assert!(prompt.contains(&s.spill.path().display().to_string()));
    k.shutdown();
    s.fixture.agents.close();
}

#[test]
fn acceptance_a55_a_bad_return_is_rejected_with_the_schema_error_and_never_recorded_as_success() {
    let reply = Arc::new(std::sync::Mutex::new(
        json!({ "kind": "urgent", "confidence": 0.5 }).to_string(),
    ));
    let current = reply.clone();
    let s = setup(Arc::new(move |_, _| lock(&current).clone()));
    let k = s.kernel();
    run(&k, TRIAGE);
    let bad = execute(&k, "t = Triage(\"main\")\nawait t.classify(\"crash\")");
    assert_eq!(bad.status, Status::Error);
    let error = bad.error.unwrap();
    assert_eq!(error.ename, "AgentCallError");
    assert_eq!(
        error.evalue,
        "Triage.classify (triage--classify@1) failed: triage--classify@1 output does not match its schema: $.kind must be equal to one of the allowed values (\"bug\", \"feature\")"
    );
    *lock(&reply) = json!({ "kind": "bug" }).to_string();
    assert!(
        evalue(&k, "await Triage(\"main\").classify(\"crash\")").contains("$ must have required properties confidence")
    );
    *lock(&reply) = "Looks like a bug to me.".into();
    let message = evalue(&k, "await Triage(\"main\").classify(\"crash\")");
    assert!(
        message.starts_with("Triage.classify (triage--classify@1) failed: ") && message.contains("JSON"),
        "{message}"
    );
    // The error keeps the host's result for code that wants to inspect it; state is untouched.
    assert_eq!(
        run(
            &k,
            "try:\n    await Triage(\"main\").classify(\"crash\")\nexcept AgentCallError as error:\n    print(error.result[\"status\"], error.result[\"verification\"], error.definition, Triage(\"main\").seen)"
        ),
        "failed unverified triage--classify@1 0\n"
    );
    let tasks = s.fixture.journal();
    assert_eq!(tasks.len(), 4);
    for task in &tasks {
        assert_eq!(task["state"], "failed");
        assert_eq!(
            (task["result"]["status"].clone(), task["result"]["verification"].clone()),
            (json!("failed"), json!("unverified"))
        );
        assert!(task["result"].get("value").is_none());
    }
    // Input contracts are checked in the kernel before any task is admitted.
    let wrong = execute(&k, "await Triage(\"main\").classify(42)").error.unwrap();
    assert_eq!(
        (wrong.ename.as_str(), wrong.evalue.as_str()),
        ("TypeError", "Triage.classify(report) must be string, got int")
    );
    assert_eq!(s.fixture.journal().len(), 4);
    k.shutdown();
    s.fixture.agents.close();
}

#[test]
fn acceptance_a55_real_bodies_run_in_the_kernel_subclasses_rebuild_inherited_model_driven_methods_with_their_own_role()
{
    let s = setup(Arc::new(|method, _| {
        if method == "respond" {
            json!(["concern"])
        } else {
            json!(0)
        }
        .to_string()
    }));
    let k = s.kernel();
    let out = run(
        &k,
        "import os\n@agent\nclass Role:\n    \"\"\"Generic reviewer.\"\"\"\n    async def respond(self, change: str) -> list[str]:\n        \"\"\"Review the change.\"\"\"\n        ...\n    def pid(self) -> int:\n        return os.getpid()\n\nclass Security(Role):\n    \"\"\"SECURITY ROLE: review authentication and data exposure.\"\"\"\n\ns = Security(\"s1\")\nprint(await s.respond(\"add login\"), s.pid() == os.getpid(), Security.describe()[\"methods\"][\"respond\"][\"definition\"])",
    );
    assert_eq!(out, "['concern'] True security--respond\n");
    let prompt = &s.prompts()[0];
    assert!(prompt.contains("SECURITY ROLE"));
    assert!(!prompt.contains("Generic reviewer"));
    k.shutdown();
    s.fixture.agents.close();
}

#[test]
fn acceptance_a55_fields_persist_across_cells_and_reset_scratch_assignments_are_type_checked() {
    let s = setup(label_reply());
    let k = s.kernel();
    run(
        &k,
        &format!("{TRIAGE}\nt = Triage(\"main\")\nt.record(await t.classify(\"crash\"))\nt.labels.append(\"manual\")"),
    );
    assert_eq!(
        run(&k, "print(t.seen, t.labels, Triage.instances())"),
        "1 ['bug', 'manual'] ['main']\n"
    );
    assert_eq!(evalue(&k, "t.seen = \"many\""), "Triage.seen must be integer, got str");
    k.reset_scratch().unwrap();
    // Scratch is fresh (the instance variable is gone), but the class is defined again and the state reattaches.
    assert_eq!(
        run(
            &k,
            "print('t' in globals(), 'Label' in globals())\nt = Triage(\"main\")\nprint(t.seen, t.labels, Triage(\"other\").seen)"
        ),
        "False True\n1 ['bug', 'manual'] 0\n"
    );
    assert_eq!(
        run(&k, "t.record(await t.classify(\"feature request\"))\nprint(t.fields())"),
        "{'seen': 2, 'labels': ['bug', 'manual', 'feature']}\n"
    );
    assert_eq!(execute(&k, "Triage.load(\"missing\")").error.unwrap().ename, "KeyError");
    assert_eq!(
        evalue(&k, "@agent\nclass Needs:\n    goal: str\nNeeds(\"x\")"),
        "Needs('x') is missing required field(s): goal"
    );
    assert_eq!(
        run(&k, "n = Needs(\"x\", goal=\"ship\")\nprint(Needs.load(\"x\").goal)"),
        "ship\n"
    );
    k.shutdown();
    s.fixture.agents.close();
}

#[test]
fn acceptance_a55_the_class_its_generation_and_instance_fields_survive_a_snapshot_and_restore_into_a_new_kernel() {
    let s = setup(label_reply());
    let first = s.kernel();
    run(
        &first,
        &format!("{TRIAGE}\nt = Triage(\"main\")\nt.record(await t.classify(\"crash\"))\nt.labels.append(\"kept\")"),
    );
    let saved = first.snapshot(Some(&s.snapshot_path())).unwrap();
    assert_eq!(saved.result.status, Status::Ok);
    let report = saved.snapshot.unwrap();
    assert!(report.saved.contains(&"state".to_string()));
    for name in ["Label", "Triage", "t"] {
        assert!(
            report.skipped.contains(&name.to_string()),
            "{name}: {:?}",
            report.skipped
        );
    }
    first.shutdown();

    let second = s.kernel();
    assert_eq!(
        second.restore(Some(&s.snapshot_path())).unwrap().result.status,
        Status::Ok
    );
    assert_eq!(
        run(
            &second,
            "t = Triage(\"main\")\nprint(t.seen, t.labels, agent.classes()[\"triage\"][\"generation\"])"
        ),
        "1 ['bug', 'kept'] 1\n"
    );
    assert_eq!(
        run(
            &second,
            "print(t.record(await t.classify(\"new feature\")), Triage.describe()[\"generation\"])"
        ),
        "2 1\n"
    );
    // The restored class reused its registered generation instead of minting a new one.
    let listed = s.fixture.call("agents.list", json!({})).unwrap();
    let versions: Vec<&Value> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["id"] == "triage--classify")
        .map(|d| &d["version"])
        .collect();
    assert_eq!(versions, [&json!("1")]);
    assert!(
        s.prompts()
            .last()
            .unwrap()
            .contains("\"state\":{\"seen\":1,\"labels\":[\"bug\",\"kept\"]}")
    );
    second.shutdown();
    s.fixture.agents.close();
}

#[test]
fn acceptance_a55_redefinition_makes_a_new_generation_older_generations_stay_invokable_and_roll_back() {
    let s = setup(label_reply());
    let k = s.kernel();
    run(&k, &format!("{TRIAGE}\nawait Triage(\"main\").classify(\"crash\")"));
    // The identical cell again: same descriptors, same generation.
    run(&k, &format!("{TRIAGE}\nawait Triage(\"main\").classify(\"crash\")"));
    let v2 = TRIAGE.replace("Label the report as a bug or a feature.", "GEN2: label strictly.");
    run(&k, &format!("{v2}\nawait Triage(\"main\").classify(\"crash\")"));
    assert_eq!(
        run(&k, "print(Triage.describe()[\"generation\"], Triage.generations())"),
        "2 {1: {'current': False, 'restorable': True}, 2: {'current': True, 'restorable': True}}\n"
    );
    assert_eq!(
        run(
            &k,
            "Old = Triage.at(1)\nprint(await Old(\"main\").classify(\"crash\"), Triage.describe()[\"generation\"])"
        ),
        "Label(kind='bug', confidence=0.9) 2\n"
    );
    let last = s.prompts().last().unwrap().clone();
    assert!(last.contains("Label the report as a bug or a feature.") && !last.contains("GEN2"));
    assert_eq!(
        run(
            &k,
            "await Triage.rollback(1)\nawait Triage(\"main\").classify(\"crash\")\nprint(Triage.generations())"
        ),
        "{1: {'current': True, 'restorable': True}, 2: {'current': False, 'restorable': True}}\n"
    );
    assert!(!s.prompts().last().unwrap().contains("GEN2"));
    let definitions: Vec<Value> = s.fixture.journal().iter().map(|t| t["definition"].clone()).collect();
    assert_eq!(
        definitions,
        [
            "triage--classify@1",
            "triage--classify@1",
            "triage--classify@2",
            "triage--classify@1",
            "triage--classify@1"
        ]
    );
    // A generation that was never registered is refused by the host, not invented.
    assert!(
        evalue(&k, "await Triage.at(9)(\"main\").classify(\"crash\")")
            .contains("Unknown Ultron agent definition: triage--classify@9")
    );
    k.shutdown();
    s.fixture.agents.close();
}
