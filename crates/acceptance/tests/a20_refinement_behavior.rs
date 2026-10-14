//! A20: a versioned refinement changes later behavior and can roll back. Port of
//! packages/coding-agent/test/ultron-a20-refinement-behavior.test.ts with its literal expectations: the Rust
//! typed-agent host (`delegation::agents`) with `services::instruction_hook` as its refinement hook (the TS
//! `refinements` option: the active `instruction:<definition>` refinement) and the local-services module answering
//! `refinements.*` through the same request path; the lane records each prompt and completes with "done".

mod services_support;

use serde_json::{Value, json};
use services::instruction_hook;
use services::local::{LocalOptions, LocalServices, LocalServicesModule};
use services_support::{Documents, Host, Setup, build};
use std::sync::Arc;

fn run(host: &Host) -> String {
    let task = host
        .call(
            "agents.spawn",
            json!({ "definition": "rlm-child@1", "input": { "prompt": "task" } }),
        )
        .unwrap();
    let result = host.call("agents.result", json!({ "id": task["id"] })).unwrap();
    assert_eq!(result["status"], "succeeded", "{result}");
    host.last_prompt()
}

fn propose(host: &Host, content: &str, base_version: u64) -> Value {
    host.call(
        "refinements.propose",
        json!({
            "kind": "instruction", "target": "instruction:rlm-child", "baseVersion": base_version,
            "content": content, "evidence": [{ "failure": "missed edge case" }],
        }),
    )
    .unwrap()["id"]
        .clone()
}

#[test]
fn acceptance_a20_activation_reaches_later_model_runs_a_new_version_supersedes_it_and_rollback_removes_it() {
    let services = Arc::new(LocalServices::new(
        Box::new(Documents::default()),
        LocalOptions::default(),
    ));
    let host = build(Setup {
        script: Some(Arc::new(|_, _, _| Ok("done".into()))),
        refinements: Some(instruction_hook(services.clone())),
        modules: vec![Arc::new(LocalServicesModule { services })],
        ..Setup::default()
    });

    assert_eq!(run(&host), "task");

    let first = propose(&host, "Always check empty inputs.", 0);
    assert!(!run(&host).contains("empty inputs")); // proposed only, not active
    host.call("refinements.activate", json!({ "id": first })).unwrap();
    let activated = run(&host);
    assert!(activated.contains("Always check empty inputs."));
    // The refinement is appended to the brief with its id and version.
    assert_eq!(
        activated,
        format!(
            "task\n\nActive refinement {} (version 1):\nAlways check empty inputs.",
            first.as_str().unwrap()
        )
    );

    let second = propose(&host, "Always check empty and huge inputs.", 1);
    host.call("refinements.activate", json!({ "id": second })).unwrap();
    let after_second = run(&host);
    assert!(after_second.contains("empty and huge inputs"));
    assert!(!after_second.contains("Always check empty inputs."));

    host.call("refinements.rollback", json!({ "id": second })).unwrap();
    let after_rollback = run(&host);
    assert!(!after_rollback.contains("huge inputs"));
    host.close();
}
