//! Replays fixtures/reconcile.json (recorded by fixtures/generate-reconcile.mjs from reconcile.ts over the records
//! of the A24 runs and synthetic documents): `reconcile_records` must return the same reconciliation (as JSON text)
//! or the same error, and `inspection_discrepancies` the same list for the recorded `agents.status`.

use delegation::reconcile::{ReconcileInput, inspection_discrepancies, reconcile_records};
use serde_json::{Value, json};
use std::path::Path;

fn fixture() -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/reconcile.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn delegation_reconcile_matches_reconcile_records() {
    for case in fixture().as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let input = &case["input"];
        let input = ReconcileInput {
            tasks: input.get("tasks"),
            usage: input.get("usage"),
            progress: input.get("progress"),
            schedules: input.get("schedules"),
            root_id: input.get("rootId").and_then(Value::as_str),
        };
        match reconcile_records(&input) {
            Err(error) => assert_eq!(json!(error), case["error"], "{name}"),
            Ok(reconciliation) => {
                assert!(case.get("error").is_none(), "{name}: expected {}", case["error"]);
                assert_eq!(reconciliation.to_string(), case["reconciliation"].to_string(), "{name}");
                if let Some(status) = case.get("status") {
                    let found = inspection_discrepancies(&reconciliation, status).unwrap();
                    assert_eq!(json!(found), case["inspection"], "{name}: inspection");
                }
            }
        }
    }
}
