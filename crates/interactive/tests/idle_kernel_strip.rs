//! The kernel strip of an idle session (no cell yet) reads `kernels ················ 0/16`, as the TypeScript
//! footer draws it from the session's `rlm.pool` (crates/delegation/fixtures/idle-pool.json, recorded from
//! `createUltronRlmTool(...).poolStats()` and `renderKernelStrip`). The pool reporting 0 live kernels is checked in
//! crates/delegation/tests/idle_pool.rs and crates/session-core/tests/idle_kernel_count.rs.

use interactive::rlm::graph::render_kernel_strip;
use interactive::rlm::visualizer::{RlmSnapshot, RlmStyle, parse_pool};
use serde_json::Value;

#[test]
fn interactive_idle_session_kernel_strip_shows_no_live_kernel_like_ts() {
    let fixture: Value = serde_json::from_str(include_str!("../../delegation/fixtures/idle-pool.json")).unwrap();
    let idle = &fixture["idle"];
    let snapshot = RlmSnapshot {
        pool: parse_pool(&idle["stats"]),
        ..RlmSnapshot::default()
    };
    assert_eq!(
        render_kernel_strip(&snapshot, 80, &RlmStyle::plain()).as_deref(),
        idle["strip"].as_str()
    );
}
