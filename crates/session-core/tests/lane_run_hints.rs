//! Every lane's run end starts its stuck-loop detection over (session-worker.ts: `cellHints.runEnded(event.lane)`
//! on every lane's `run_end`, not only the root's): a typed agent's retained lane runs again without the failure
//! streak of its last run. Literal expectations from kernel_apis' loop detector (3 failed cells in a row trip it).

use delegation::agents::{LaneReply, LaneRun, LaneRunner};
use kernel_apis::cell_hints::{CellHints, CellHintsOptions, CellOutcome};
use kernel_apis::{HintsModule, MemoryDocumentStore};
use std::sync::Arc;

const LANE: &str = "agent-retained";

fn fail(hints: &CellHints, code: &str) -> Option<String> {
    hints.begin_cell(LANE, code);
    hints.end_cell(
        LANE,
        &CellOutcome {
            text: "",
            ename: Some("KeyError"),
        },
    )
}

fn run(runner: &LaneRunner) {
    runner(LaneRun {
        task_id: "t1".into(),
        lane: LANE.into(),
        reuse: true,
        prompt: "again".into(),
        model: None,
        timeout_ms: 1000,
        deadline_at: None,
        definition: "d".into(),
        signal: kernel::Signal::new(),
    })
    .unwrap();
}

/// Two failures, a run of the lane ends, one more failure: no stuck-loop hint through the session's lane runner;
/// a runner that does not report the end trips it.
#[test]
fn session_core_lane_run_end_resets_the_lanes_loop_hints() {
    let module = || Arc::new(HintsModule::new(Arc::new(MemoryDocumentStore::default())));
    let plain: LaneRunner = Arc::new(|_| Ok(LaneReply::default()));
    let hints = Arc::new(CellHints::new(module(), CellHintsOptions::default()));
    let ended = hints.clone();
    let runner = session_core::compose::hinted_lanes(plain.clone(), Arc::new(move |lane: &str| ended.run_ended(lane)));
    fail(&hints, "a()");
    fail(&hints, "b()");
    run(&runner);
    assert_eq!(fail(&hints, "c()"), None);

    let unreported = CellHints::new(module(), CellHintsOptions::default());
    fail(&unreported, "a()");
    fail(&unreported, "b()");
    run(&plain);
    let hint = fail(&unreported, "c()").unwrap();
    assert!(hint.starts_with("[hint:stuck-loop] The last 3 cells failed"), "{hint}");
}
