//! The refinement hook a typed-agent host applies to its prompts (TS `NativeHostOptions.refinements` as the worker
//! builds it: the active `instruction:<definition>` refinement as `{id, version, text: String(content)}`), and the
//! session-value stores (`ultron.local` keyed documents scanned by prefix and namespace, whole documents replaced,
//! both restored when the session is reopened).

use kernel_apis::DocumentStore;
use kernel_apis::artifacts::{KeyedDocuments, MemoryKeyedDocuments};
use serde_json::{Value, json};
use services::local::{LocalOptions, LocalServices};
use services::{SessionValues, instruction_hook};
use session_journal::{SessionRepo, SystemClock};
use std::sync::Arc;

#[test]
fn services_instruction_hook_yields_the_active_instruction_of_a_definition_and_nothing_else() {
    let services = Arc::new(LocalServices::new(
        Box::new(MemoryKeyedDocuments::default()),
        LocalOptions::default(),
    ));
    let hook = instruction_hook(services.clone());
    assert_eq!(hook("rlm-child").unwrap(), Vec::<Value>::new());
    let propose = |target: &str, content: Value| {
        let id = services
            .handle(
                "refinements.propose",
                &json!({ "kind": "instruction", "target": target, "baseVersion": 0, "content": content,
                         "evidence": [{ "run": "r" }] }),
                None,
            )
            .unwrap()["id"]
            .clone();
        services
            .handle("refinements.activate", &json!({ "id": id }), None)
            .unwrap();
        id
    };
    let text = propose("instruction:rlm-child", json!("Check empty inputs."));
    let object = propose("instruction:correctness-reviewer", json!({ "rule": "x" }));
    let list = propose("instruction:tests-reviewer", json!(["a", null, 3]));
    assert_eq!(
        hook("rlm-child").unwrap(),
        vec![json!({ "id": text, "version": 1, "text": "Check empty inputs." })]
    );
    assert_eq!(hook("correctness-reviewer").unwrap()[0]["text"], "[object Object]");
    assert_eq!(hook("correctness-reviewer").unwrap()[0]["id"], object);
    assert_eq!(hook("tests-reviewer").unwrap()[0]["text"], "a,,3");
    assert_eq!(hook("tests-reviewer").unwrap()[0]["id"], list);
    assert_eq!(hook("style-reviewer").unwrap(), Vec::<Value>::new());
    services
        .handle("refinements.rollback", &json!({ "id": text }), None)
        .unwrap();
    assert_eq!(hook("rlm-child").unwrap(), Vec::<Value>::new());
}

#[test]
fn services_session_values_keep_documents_per_namespace_and_survive_a_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let repo = SessionRepo::new(&temp.path().join("sessions"), SystemClock::shared());
    let session = repo
        .create(&temp.path().display().to_string(), Some("v"), None)
        .unwrap();
    let values = SessionValues::new(session);
    let local = values.keyed("ultron.local");
    local
        .set("ultron.refinements", &json!({ "formatVersion": 1, "events": [] }))
        .unwrap();
    local.set("ultron.artifacts/b", &json!(2)).unwrap();
    local.set("ultron.artifacts/a", &json!(1)).unwrap();
    values
        .keyed("ultron.other")
        .set("ultron.artifacts/z", &json!(9))
        .unwrap();
    let module = values.document("ultron.module", "release-gates");
    module.write(&json!({ "version": 1 })).unwrap();
    let listed = |keyed: &dyn KeyedDocuments| keyed.list("ultron.artifacts/").unwrap();
    assert_eq!(
        listed(&local),
        vec![
            ("ultron.artifacts/a".to_string(), json!(1)),
            ("ultron.artifacts/b".to_string(), json!(2))
        ]
    );
    assert_eq!(local.get("missing").unwrap(), None);

    let metadata = repo.list(None).into_iter().find(|m| m.id == "v").unwrap();
    drop(values);
    let reopened = SessionValues::new(repo.open(&metadata).unwrap());
    let local = reopened.keyed("ultron.local");
    assert_eq!(listed(&local).len(), 2);
    assert_eq!(
        local.get("ultron.refinements").unwrap(),
        Some(json!({ "formatVersion": 1, "events": [] }))
    );
    assert_eq!(
        reopened.document("ultron.module", "release-gates").read().unwrap(),
        Some(json!({ "version": 1 }))
    );
    assert_eq!(reopened.document("ultron.module", "other").read().unwrap(), None);
}
