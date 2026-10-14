//! A32: skill proposals require evidence, validation, and applicable approval; rejection and rollback hold. Every
//! case of packages/coding-agent/test/ultron-a32-skill-proposals.test.ts (over `services::LocalServices` with the
//! test's procedure validator as the activation check) and of ultron-a32-production-validation.test.ts (the worker's
//! service, `LocalServices::worker`, over the session's own values in a real format-4 session journal, as
//! `createWorkerServices` keeps them under `ultron.local`), with their literal expectations.

mod services_support;

use serde_json::{Value, json};
use services::SessionValues;
use services::local::{LocalOptions, LocalServices, Validator};
use services_support::{Documents, rejects};
use std::sync::Arc;

/// The test's activation validator: a skill proposal must be a complete procedure.
fn validate_skill() -> Validator {
    Arc::new(|kind: &str, content: &Value| {
        if kind != "skill" {
            return Ok(true);
        }
        let Some(map) = content.as_object() else {
            return Ok(false);
        };
        Ok(["problem", "procedure", "input", "output", "tests"]
            .iter()
            .all(|key| map.get(*key).is_some_and(|v| v.is_string() || v.is_array())))
    })
}

fn procedure() -> Value {
    json!({
        "problem": "Flaky vitest runs are misreported as regressions.",
        "procedure": "Re-run the failing file three times in isolation before bisecting.",
        "input": "failing test file path",
        "output": "flaky | deterministic verdict with run logs",
        "tests": ["test/flaky-triage.test.ts"],
    })
}

/// The repeated-procedure evidence: several independent runs where the same procedure succeeded.
fn repeated_runs() -> Value {
    json!([
        { "run": "run-101", "outcome": "passed", "procedure": "rerun-isolated" },
        { "run": "run-117", "outcome": "passed", "procedure": "rerun-isolated" },
        { "run": "run-130", "outcome": "passed", "procedure": "rerun-isolated" },
    ])
}

struct Services(LocalServices);

impl Services {
    fn call(&self, kind: &str, payload: Value) -> Result<Value, String> {
        self.0.handle(kind, &payload, None)
    }
}

fn services(store: &Documents, require_approval: bool) -> Services {
    Services(LocalServices::new(
        Box::new(store.clone()),
        LocalOptions {
            validate: Some(validate_skill()),
            require_approval,
            ..LocalOptions::default()
        },
    ))
}

fn skill(overrides: Value) -> Value {
    let mut out = json!({
        "kind": "skill", "target": "skill:flaky-test-triage", "baseVersion": 0, "content": procedure(),
        "evidence": repeated_runs(), "scope": "project",
    });
    for (key, value) in overrides.as_object().unwrap() {
        out[key] = value.clone();
    }
    out
}

fn assert_has(got: &Value, want: Value) {
    for (key, value) in want.as_object().unwrap() {
        assert_eq!(&got[key], value, "{key} of {got}");
    }
}

fn revised() -> Value {
    let mut content = procedure();
    content["procedure"] = json!("Re-run five times.");
    content
}

fn history(record: &Value) -> Vec<&str> {
    record["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["state"].as_str().unwrap())
        .collect()
}

#[test]
fn acceptance_a32_a_repeated_procedure_proposal_activates_only_with_evidence_and_records_that_approval_is_disabled() {
    let store = Documents::default();
    let call = services(&store, false);
    let unsupported = call
        .call("refinements.propose", skill(json!({ "evidence": [] })))
        .unwrap();
    rejects(
        call.call("refinements.activate", json!({ "id": unsupported["id"] })),
        "nonempty evidence",
    );
    call.call("refinements.reject", json!({ "id": unsupported["id"] }))
        .unwrap();

    let proposed = call.call("refinements.propose", skill(json!({}))).unwrap();
    assert_has(
        &proposed,
        json!({ "state": "proposed", "approval": null, "evidence": repeated_runs() }),
    );
    let active = call
        .call("refinements.activate", json!({ "id": proposed["id"] }))
        .unwrap();
    // Approval is off by default (no-friction amendment); the record says so explicitly.
    assert_has(
        &active,
        json!({ "state": "active", "version": 1, "approval": "not_required" }),
    );

    let reopened = services(&store, false);
    let current = reopened
        .call(
            "refinements.current",
            json!({ "kind": "skill", "target": "skill:flaky-test-triage" }),
        )
        .unwrap();
    assert_has(
        &current,
        json!({ "id": proposed["id"], "approval": "not_required", "version": 1 }),
    );
}

#[test]
fn acceptance_a32_an_equivalent_pending_or_active_proposal_is_a_duplicate_a_real_change_is_not() {
    let call = services(&Documents::default(), false);
    let first = call.call("refinements.propose", skill(json!({}))).unwrap();
    rejects(
        call.call("refinements.propose", skill(json!({}))),
        &format!("Duplicate refinement proposal: {}", first["id"].as_str().unwrap()),
    );
    // Key order and fresh evidence do not make the same procedure new while it is pending.
    let mut reordered = serde_json::Map::new();
    for (key, value) in procedure().as_object().unwrap().iter().rev() {
        reordered.insert(key.clone(), value.clone());
    }
    rejects(
        call.call(
            "refinements.propose",
            skill(json!({ "content": reordered, "evidence": [{ "run": "run-200" }] })),
        ),
        "Duplicate refinement proposal",
    );
    call.call("refinements.activate", json!({ "id": first["id"] })).unwrap();
    rejects(call.call("refinements.propose", skill(json!({}))), "(active)");

    // A changed procedure against the active version, and the same one in another scope, are not duplicates.
    let changed = call
        .call(
            "refinements.propose",
            skill(json!({ "baseVersion": 1, "content": revised() })),
        )
        .unwrap();
    assert_eq!(changed["state"], "proposed");
    let other_scope = call
        .call("refinements.propose", skill(json!({ "scope": "session" })))
        .unwrap();
    assert_eq!(other_scope["state"], "proposed");
}

#[test]
fn acceptance_a32_invalid_skill_content_fails_validation_and_enabled_approval_cannot_override_it() {
    let store = Documents::default();
    let call = services(&store, true);
    let invalid = call
        .call(
            "refinements.propose",
            skill(json!({ "content": { "problem": "only a problem statement" } })),
        )
        .unwrap();
    rejects(
        call.call("refinements.activate", json!({ "id": invalid["id"] })),
        "Activation requires approval",
    );
    let approved = call
        .call("refinements.approve", json!({ "id": invalid["id"] }))
        .unwrap();
    assert_eq!(approved["approval"], "approved");
    rejects(
        call.call("refinements.activate", json!({ "id": invalid["id"] })),
        "validation failed",
    );
    assert_has(
        &call.call("refinements.get", json!({ "id": invalid["id"] })).unwrap(),
        json!({ "state": "proposed", "version": null }),
    );

    let valid = call.call("refinements.propose", skill(json!({}))).unwrap();
    rejects(
        call.call("refinements.activate", json!({ "id": valid["id"] })),
        "Activation requires approval",
    );
    call.call("refinements.approve", json!({ "id": valid["id"] })).unwrap();
    assert_has(
        &call.call("refinements.activate", json!({ "id": valid["id"] })).unwrap(),
        json!({ "state": "active", "approval": "approved" }),
    );
    // Approval is only for proposed records and never re-opens a decided one.
    rejects(
        call.call("refinements.approve", json!({ "id": valid["id"] })),
        "Only a proposed",
    );
    // The approval is durable: a restart with the control on still sees it.
    let restarted = services(&store, true);
    assert_eq!(
        restarted.call("refinements.get", json!({ "id": valid["id"] })).unwrap()["approval"],
        "approved"
    );
}

#[test]
fn acceptance_a32_rejection_and_rollback_hold_across_restart_a_declined_proposal_needs_new_evidence_to_resurface() {
    let store = Documents::default();
    let mut call = services(&store, false);
    let declined = call
        .call("refinements.propose", skill(json!({ "target": "skill:auto-rebase" })))
        .unwrap();
    call.call("refinements.reject", json!({ "id": declined["id"] }))
        .unwrap();

    let v1 = call.call("refinements.propose", skill(json!({}))).unwrap();
    call.call("refinements.activate", json!({ "id": v1["id"] })).unwrap();
    let v2 = call
        .call(
            "refinements.propose",
            skill(json!({ "baseVersion": 1, "content": revised() })),
        )
        .unwrap();
    call.call("refinements.activate", json!({ "id": v2["id"] })).unwrap();
    call.call("refinements.rollback", json!({ "id": v2["id"] })).unwrap();

    call = services(&store, false); // restart
    assert_eq!(
        call.call("refinements.get", json!({ "id": declined["id"] })).unwrap()["state"],
        "rejected"
    );
    rejects(
        call.call("refinements.activate", json!({ "id": declined["id"] })),
        "Only a proposed",
    );
    rejects(
        call.call("refinements.propose", skill(json!({ "target": "skill:auto-rebase" }))),
        "(rejected)",
    );
    // New supporting evidence may resurface it as a new, separately tracked proposal.
    let mut more = repeated_runs();
    more.as_array_mut()
        .unwrap()
        .push(json!({ "run": "run-150", "outcome": "passed" }));
    let resurfaced = call
        .call(
            "refinements.propose",
            skill(json!({ "target": "skill:auto-rebase", "evidence": more })),
        )
        .unwrap();
    assert_ne!(resurfaced["id"], declined["id"]);

    let current = call
        .call(
            "refinements.current",
            json!({ "kind": "skill", "target": "skill:flaky-test-triage" }),
        )
        .unwrap();
    assert_has(&current, json!({ "id": v1["id"], "version": 1, "state": "active" }));
    let rolled_back = call.call("refinements.get", json!({ "id": v2["id"] })).unwrap();
    assert_has(&rolled_back, json!({ "state": "rolled_back", "version": 2 }));
    assert_eq!(history(&rolled_back), ["proposed", "active", "rolled_back"]);
    assert_eq!(
        history(&call.call("refinements.get", json!({ "id": v1["id"] })).unwrap()),
        ["proposed", "active", "superseded", "active"]
    );
    rejects(
        call.call("refinements.activate", json!({ "id": v2["id"] })),
        "Only a proposed",
    );
    rejects(
        call.call("refinements.rollback", json!({ "id": v2["id"] })),
        "latest active",
    );
}

#[test]
fn acceptance_a32_the_workers_refinement_service_refuses_malformed_skills_and_accepts_a_loadable_skill_md() {
    let temp = tempfile::tempdir().unwrap();
    let repo = session_journal::SessionRepo::new(&temp.path().join("sessions"), session_journal::SystemClock::shared());
    let session = repo
        .create(&temp.path().display().to_string(), Some("s"), None)
        .unwrap();
    let values = SessionValues::new(session);
    let services = LocalServices::worker(Box::new(values.keyed("ultron.local")));
    let propose = |content: &str, target: &str| -> Value {
        services
            .handle(
                "refinements.propose",
                &json!({ "kind": "skill", "target": target, "baseVersion": 0, "content": content,
                         "evidence": [{ "repeated": 3 }] }),
                None,
            )
            .unwrap()["id"]
            .clone()
    };
    let activate = |id: Value| services.handle("refinements.activate", &json!({ "id": id }), None);

    rejects(
        activate(propose("just prose, no frontmatter", "skill:bad-one")),
        "frontmatter",
    );
    rejects(
        activate(propose(
            "---\nname: Bad Name\ndescription: x\n---\nbody",
            "skill:bad-two",
        )),
        "kebab-case",
    );
    rejects(
        activate(propose(
            "---\nname: empty-body\ndescription: x\n---\n   ",
            "skill:bad-three",
        )),
        "body is empty",
    );
    let accepted = activate(propose(
        "---\nname: release-notes\ndescription: Draft release notes from merged changes\n---\nCollect merged PRs, then group them.",
        "skill:release-notes",
    ))
    .unwrap();
    assert_has(&accepted, json!({ "state": "active", "approval": "not_required" }));
    // The ledger is a value of the session itself.
    assert!(values.get("ultron.local", "ultron.refinements").is_some());
}
