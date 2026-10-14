//! The stuck-loop detector and its hint, ported from packages/coding-agent/test/ultron-loop-detector.test.ts
//! ("loop detector" and "stuck-loop hint") with its literal expectations, over `kernel_apis::cell_hints`.

use kernel_apis::cell_hints::{
    CellHints, CellHintsOptions, CellOutcome, LoopDetector, LoopLimits, LoopTrip, alternation_length, bash_failed,
    loop_hint_text, normalize_cell,
};
use kernel_apis::{HintsModule, MemoryDocumentStore};
use serde_json::{Map, Value, json};
use std::sync::Arc;

fn trip(level: u8, reason: &'static str, count: usize, ename: Option<&str>) -> Option<LoopTrip> {
    Some(LoopTrip {
        level,
        reason,
        count,
        ename: ename.map(str::to_string),
    })
}

#[test]
fn kernel_apis_loop_three_failures_trip_once_three_more_escalate_once_then_quiet() {
    let mut loops = LoopDetector::default();
    let mut fail = |code: &str, ename: &str| loops.cell_ended("main", code, true, Some(ename));
    assert_eq!(fail("a", "KeyError"), None);
    assert_eq!(fail("b", "KeyError"), None);
    assert_eq!(fail("c", "KeyError"), trip(1, "consecutive", 3, Some("KeyError")));
    assert_eq!(fail("d", "ValueError"), None);
    assert_eq!(fail("e", "ValueError"), None);
    // Mixed exception types: the escalation names no single one.
    assert_eq!(fail("f", "KeyError"), trip(2, "consecutive", 6, None));
    for code in ["g", "h", "i", "j", "k", "l"] {
        assert_eq!(fail(code, "KeyError"), None);
    }
    assert_eq!(loops.level("main"), 2);
}

#[test]
fn kernel_apis_loop_a_success_resets_the_streak_and_the_phase() {
    let mut loops = LoopDetector::default();
    loops.cell_ended("main", "a", true, None);
    loops.cell_ended("main", "b", true, None);
    assert_eq!(loops.cell_ended("main", "c", false, None), None);
    assert_eq!(loops.cell_ended("main", "d", true, None), None);
    assert_eq!(loops.cell_ended("main", "e", true, None), None);
    assert_eq!(loops.cell_ended("main", "f", true, None).map(|t| t.level), Some(1));
    assert_eq!(loops.cell_ended("main", "g", false, None), None);
    assert_eq!(loops.level("main"), 0);
    loops.cell_ended("main", "h", true, None);
    loops.cell_ended("main", "i", true, None);
    let third = loops.cell_ended("main", "j", true, None).unwrap();
    assert_eq!((third.level, third.reason), (1, "consecutive"));
}

#[test]
fn kernel_apis_loop_alternation_of_near_identical_cells_trips_and_escalates_at_twice_the_length() {
    let mut loops = LoopDetector::default();
    let a = |loops: &mut LoopDetector, n: usize| {
        loops.cell_ended(
            "main",
            &format!("x = parse(data, retries={n})\nprint(x)"),
            true,
            Some("ValueError"),
        )
    };
    let b = |loops: &mut LoopDetector| loops.cell_ended("main", "print(  data[:100] )", false, None);
    assert_eq!(a(&mut loops, 1), None);
    assert_eq!(b(&mut loops), None);
    assert_eq!(a(&mut loops, 2), None);
    assert_eq!(b(&mut loops), trip(1, "alternation", 4, None));
    for i in 3..5 {
        assert_eq!(a(&mut loops, i), None);
        if i < 4 {
            assert_eq!(b(&mut loops), None);
        }
    }
    assert_eq!(b(&mut loops), trip(2, "alternation", 8, None));
    assert_eq!(a(&mut loops, 9), None);
    assert_eq!(loops.cell_ended("main", "something_else()", false, None), None);
    assert_eq!(loops.level("main"), 0);
}

#[test]
fn kernel_apis_loop_repeated_successful_cells_never_trip() {
    let mut loops = LoopDetector::default();
    for i in 0..20 {
        assert_eq!(
            loops.cell_ended("main", &format!("print(step({i}))"), false, None),
            None
        );
    }
    for i in 0..20 {
        let code = if i % 2 == 1 { "run_tests()" } else { "apply_patch()" };
        assert_eq!(loops.cell_ended("main", code, false, None), None);
    }
}

#[test]
fn kernel_apis_loop_total_failures_trip_once_and_escalate_once() {
    let mut loops = LoopDetector::new(LoopLimits {
        total: 4,
        ..LoopLimits::default()
    });
    let mut trips = Vec::new();
    for i in 0..12 {
        if let Some(trip) = loops.cell_ended("main", &format!("attempt_{}()", "x".repeat(i)), true, None) {
            trips.push(trip);
        }
        loops.cell_ended("main", &format!("ok_{}()", "y".repeat(i)), false, None);
    }
    assert_eq!(
        trips,
        [trip(1, "total", 4, None).unwrap(), trip(2, "total", 8, None).unwrap()]
    );
}

#[test]
fn kernel_apis_loop_lanes_are_separate_and_a_finished_run_starts_clean() {
    let mut loops = LoopDetector::default();
    loops.cell_ended("main", "a", true, None);
    loops.cell_ended("main", "b", true, None);
    assert_eq!(loops.cell_ended("child-1", "a", true, None), None);
    loops.reset("main");
    assert_eq!(loops.cell_ended("main", "c", true, None), None);
}

#[test]
fn kernel_apis_loop_normalization_and_alternation_length() {
    assert_eq!(normalize_cell("x = f( 1 )\n"), normalize_cell("x=f(22)"));
    assert_ne!(normalize_cell("x = f(a)"), normalize_cell("x = f(b)"));
    assert_eq!(alternation_length(&["a", "b", "a", "b"]), 4);
    assert_eq!(alternation_length(&["c", "a", "b", "a", "b", "a"]), 5);
    assert_eq!(alternation_length(&["a", "a"]), 0);
    assert_eq!(alternation_length(&["a", "b", "c"]), 2);
}

#[test]
fn kernel_apis_loop_hint_text_and_bash_failures() {
    let first = loop_hint_text(&trip(1, "consecutive", 3, Some("KeyError")).unwrap());
    assert!(first.contains("The last 3 cells failed (KeyError each time)"));
    assert!(first.contains("three different hypotheses"));
    let second = loop_hint_text(&trip(2, "alternation", 8, None).unwrap());
    assert!(second.contains("alternate between two near-identical versions"));
    assert!(second.contains("Stop repeating this approach"));
    assert!(bash_failed("make", &json!(2), false));
    assert!(bash_failed("pytest -x", &json!(1), false));
    assert!(!bash_failed("grep -n foo src", &json!(1), false));
    assert!(bash_failed("rg foo", &json!(2), false));
    assert!(!bash_failed("ls", &json!(0), false));
    assert!(bash_failed("sleep 99", &Value::Null, true));
}

fn hints() -> (CellHints, Arc<HintsModule>) {
    let module = Arc::new(HintsModule::new(Arc::new(MemoryDocumentStore::default())));
    (CellHints::new(module.clone(), CellHintsOptions::default()), module)
}

fn cell(hints: &CellHints, code: &str, ename: Option<&str>, bash: Option<(&str, i64)>) -> Option<String> {
    hints.begin_cell("main", code);
    if let Some((command, exit_code)) = bash {
        let payload: Map<String, Value> = json!({"command": command}).as_object().cloned().unwrap();
        let result = json!({"command": command, "exit_code": exit_code, "running": false});
        hints.observe("main", "bash", &payload, Some(&result), 0);
    }
    hints.end_cell("main", &CellOutcome { text: "", ename })
}

#[test]
fn kernel_apis_stuck_loop_fires_once_per_phase_escalates_once_and_replaces_repeated_failure() {
    let (hints, _) = hints();
    let results: Vec<Option<String>> = (0..9)
        .map(|i| cell(&hints, &format!("step_{}()", "i".repeat(i)), Some("KeyError"), None))
        .collect();
    assert_eq!(results.iter().filter(|h| h.is_some()).count(), 2);
    let third = results[2].as_deref().unwrap();
    assert!(
        third.starts_with("[hint:stuck-loop] The last 3 cells failed (KeyError each time); you look stuck."),
        "{third}"
    );
    assert!(third.contains("await hints.mute(\"stuck-loop\")"));
    assert!(
        results[5]
            .as_deref()
            .unwrap()
            .starts_with("[hint:stuck-loop] The last 6 cells failed")
    );
    assert!(results.iter().flatten().all(|h| !h.contains("repeated-failure")));
}

#[test]
fn kernel_apis_stuck_loop_a_failed_bash_fails_the_cell_and_a_muted_tag_stays_silent() {
    let (hints_a, _) = hints();
    assert_eq!(
        cell(&hints_a, "await bash('npm test')", None, Some(("npm test", 1))),
        None
    );
    assert_eq!(
        cell(
            &hints_a,
            "await bash('npm test -- --verbose')",
            None,
            Some(("npm test", 1))
        ),
        None
    );
    assert!(
        cell(&hints_a, "await bash('npm test -- -x')", None, Some(("npm test", 1)))
            .unwrap()
            .starts_with("[hint:stuck-loop]")
    );
    let (muted, module) = hints();
    let payload: Map<String, Value> = json!({"tags": ["stuck-loop"]}).as_object().cloned().unwrap();
    module.handle("hints.mute", &payload, "main").unwrap();
    let results: Vec<Option<String>> = (0..6)
        .map(|i| cell(&muted, &format!("f{}()", "x".repeat(i)), Some("OSError"), None))
        .collect();
    // Muted, the narrower repeated-failure hint is what remains.
    assert!(results.iter().any(Option::is_some));
    assert!(
        results
            .iter()
            .flatten()
            .all(|h| h.starts_with("[hint:repeated-failure]"))
    );
}

#[test]
fn kernel_apis_stuck_loop_grep_finding_nothing_is_not_a_failure_and_run_end_starts_over() {
    let (hints_a, _) = hints();
    for i in 0..5 {
        let command = format!("grep -rn x{i} .");
        assert_eq!(
            cell(&hints_a, &format!("await bash('{command}')"), None, Some((&command, 1))),
            None
        );
    }
    let (hints_b, _) = hints();
    cell(&hints_b, "a()", Some("E"), None);
    cell(&hints_b, "b()", Some("E"), None);
    hints_b.run_ended("main");
    assert_eq!(cell(&hints_b, "c()", Some("E"), None), None);
}
