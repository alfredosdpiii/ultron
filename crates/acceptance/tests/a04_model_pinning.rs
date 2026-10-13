//! A04: an explicit child model reaches only the child lane; the root lane's model is never changed, and a malformed
//! explicit model is refused rather than silently replaced.
//! Port of packages/coding-agent/test/ultron-a04-model-pinning.test.ts over the Rust typed-agent host: a lane's model
//! is set only when the request (or its definition) names one (`LaneRun.model`), which the fixture records as the TS
//! fake records `setModel`. The `rlm.spawn(model=...)` refusal is covered by crates/delegation/tests/spawn_model.rs.

mod agents_support;

use agents_support::{Options, fixture, lock};
use serde_json::json;
use std::sync::Arc;

#[test]
fn acceptance_a04_an_explicit_child_model_reaches_only_the_child_lane_the_root_lanes_model_is_never_changed() {
    let fixture = fixture(Options {
        script: Some(Arc::new(|_, _, _, _| Ok("done".into()))),
        ..Options::default()
    });
    let pinned = fixture
        .call(
            "agents.spawn",
            json!({ "definition": "rlm-child@1", "input": { "prompt": "explicit" }, "model": "cliproxyapi/gpt-5.6-sol" }),
        )
        .unwrap();
    fixture.call("agents.result", json!({ "id": pinned["id"] })).unwrap();
    let inherited = fixture
        .call(
            "agents.spawn",
            json!({ "definition": "rlm-child@1", "input": { "prompt": "inherits" } }),
        )
        .unwrap();
    fixture.call("agents.result", json!({ "id": inherited["id"] })).unwrap();
    assert_eq!(
        *lock(&fixture.models),
        vec![(
            format!("ultron.rlm-child.{}", pinned["id"].as_str().unwrap()),
            "cliproxyapi/gpt-5.6-sol".to_string()
        )]
    );
    // No model was set on any other lane, the root ("main") included.
    assert!(lock(&fixture.models).iter().all(|(lane, _)| lane != "main"));
    // A malformed explicit model is refused rather than silently replaced.
    let refused = fixture
        .call(
            "agents.spawn",
            json!({ "definition": "rlm-child@1", "input": { "prompt": "x" }, "model": "no-slash" }),
        )
        .unwrap_err();
    assert!(refused.contains("provider/model"), "{refused}");
    fixture.agents.close();
}
