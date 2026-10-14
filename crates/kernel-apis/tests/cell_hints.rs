//! Situational hints, ported from packages/coding-agent/test/ultron-hints.test.ts ("situational hints") with its
//! literal expectations, over `kernel_apis::cell_hints::CellHints` and the `hints.*` module it shares mutes and
//! counts with. The A56 cases are job-detached, blocked-on-job and poll-loop.

use kernel_apis::cell_hints::{
    CellHints, CellHintsOptions, CellOutcome, hint_max_per_tag, hints_enabled, read_handle_bytes,
};
use kernel_apis::{HintsModule, MemoryDocumentStore};
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

const TRUNCATED: &str = "head\n[... 5000 bytes truncated ...]\ntail";

struct Fixture {
    clock: Arc<AtomicI64>,
    store: Arc<MemoryDocumentStore>,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture {
            clock: Arc::new(AtomicI64::new(1_000_000)),
            store: Arc::new(MemoryDocumentStore::default()),
        }
    }

    fn options(&self) -> CellHintsOptions {
        let clock = self.clock.clone();
        CellHintsOptions {
            now: Arc::new(move || clock.load(Ordering::SeqCst)),
            ..CellHintsOptions::default()
        }
    }

    fn make_with(&self, options: CellHintsOptions) -> CellHints {
        CellHints::new(Arc::new(HintsModule::new(self.store.clone())), options)
    }

    fn make(&self) -> CellHints {
        self.make_with(self.options())
    }

    /// One "cell": the observed host requests (type, payload, result, took ms), then its outcome; the hint line.
    fn cell(
        &self,
        hints: &CellHints,
        code: &str,
        requests: &[(&str, Value, Value, i64)],
        outcome: (&str, Option<&str>),
    ) -> Option<String> {
        self.cell_on(hints, "main", code, requests, outcome)
    }

    fn cell_on(
        &self,
        hints: &CellHints,
        lane: &str,
        code: &str,
        requests: &[(&str, Value, Value, i64)],
        (text, ename): (&str, Option<&str>),
    ) -> Option<String> {
        hints.begin_cell(lane, code);
        for (kind, payload, result, took) in requests {
            let started = self.clock.load(Ordering::SeqCst);
            self.clock.fetch_add(*took, Ordering::SeqCst);
            hints.observe(lane, kind, &object(payload), Some(result), started);
        }
        hints.end_cell(lane, &CellOutcome { text, ename })
    }
}

fn object(value: &Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

const OK: (&str, Option<&str>) = ("", None);

fn module_request(module: &HintsModule, kind: &str, tags: &[&str], lane: &str) -> Result<Value, String> {
    let payload = object(&json!({"tags": tags}));
    module.handle(kind, &payload, lane)
}

fn starts(hint: &Option<String>, prefix: &str) -> bool {
    hint.as_deref().is_some_and(|h| h.starts_with(prefix))
}

#[test]
fn kernel_apis_hints_job_detached_a_plain_bash_that_came_back_running() {
    let fx = Fixture::new();
    let hints = fx.make();
    let hint = fx.cell(
        &hints,
        "out = await bash('pytest')",
        &[(
            "bash",
            json!({"command": "pytest", "yield_after": 30}),
            json!({"running": true, "job": {"id": "job-ab12"}}),
            10,
        )],
        OK,
    );
    assert!(
        starts(
            &hint,
            "[hint:job-detached] The command was still running after 30 s, so it continues as job job-ab12."
        ),
        "{hint:?}"
    );
    let hint = hint.unwrap();
    assert!(hint.contains("<runtime_event>"));
    assert!(hint.contains("await hints.mute(\"job-detached\")"));
    assert_eq!(hint.split('\n').count(), 1);
    // A command that finished inside the window is not a detach.
    assert_eq!(
        fx.cell(
            &hints,
            "await bash('ls')",
            &[(
                "bash",
                json!({"yield_after": 30}),
                json!({"running": false, "job": null}),
                10
            )],
            OK
        ),
        None
    );
    // With events off the hint says how to wait instead.
    let quiet = fx.make_with(CellHintsOptions {
        async_events: false,
        ..fx.options()
    });
    let off = fx
        .cell(
            &quiet,
            "x",
            &[(
                "bash",
                json!({"yield_after": 30}),
                json!({"running": true, "job": {"id": "job-1"}}),
                10,
            )],
            OK,
        )
        .unwrap();
    assert!(off.contains("`await <result>.job.result()` waits for it"));
    assert!(!off.contains("runtime_event"));
}

#[test]
fn kernel_apis_hints_blocked_on_job_over_60_s_awaiting_job_result_while_events_are_on() {
    let fx = Fixture::new();
    let hints = fx.make();
    assert_eq!(
        fx.cell(
            &hints,
            "await job.result()",
            &[(
                "shell.result",
                json!({"id": "job-1"}),
                json!({"running": false}),
                59_000
            )],
            OK
        ),
        None
    );
    let hint = fx.cell(
        &hints,
        "await job.result()",
        &[
            (
                "shell.result",
                json!({"id": "job-2"}),
                json!({"running": false}),
                40_000,
            ),
            ("background.result", json!({"id": "t-1"}), json!({}), 25_000),
        ],
        OK,
    );
    assert!(
        starts(
            &hint,
            "[hint:blocked-on-job] This cell spent 65 s waiting in job.result(), background.result()."
        ),
        "{hint:?}"
    );
    assert!(hint.unwrap().contains("You could have ended your turn"));
    // Waiting for subagents is what the guide asks once the root has nothing of its own left: no hint.
    let fx2 = Fixture::new();
    let collecting = fx2.make();
    assert_eq!(
        fx2.cell(
            &collecting,
            "await rlm.collect(hs)",
            &[
                ("rlm.collect", json!({"selectors": []}), json!({"results": []}), 120_000),
                ("agents.result", json!({"id": "t-2"}), json!({}), 90_000),
            ],
            OK
        ),
        None
    );
    // Concurrent waits (asyncio.gather) count once.
    let fx3 = Fixture::new();
    let gathered = fx3.make();
    gathered.begin_cell("main", "await asyncio.gather(a.result(), b.result())");
    let start = fx3.clock.load(Ordering::SeqCst);
    fx3.clock.fetch_add(40_000, Ordering::SeqCst);
    gathered.observe(
        "main",
        "shell.result",
        &object(&json!({"id": "a"})),
        Some(&json!({"running": false})),
        start,
    );
    gathered.observe(
        "main",
        "shell.result",
        &object(&json!({"id": "b"})),
        Some(&json!({"running": false})),
        start,
    );
    assert_eq!(gathered.end_cell("main", &CellOutcome { text: "", ename: None }), None);
    // A wait inside another counts once (`unionMs` skips intervals that end before the covered span does).
    let fx5 = Fixture::new();
    let nested = fx5.make();
    nested.begin_cell("main", "await asyncio.gather(a.result(), b.result())");
    let start = fx5.clock.load(Ordering::SeqCst);
    fx5.clock.fetch_add(20_000, Ordering::SeqCst);
    nested.observe(
        "main",
        "shell.result",
        &object(&json!({"id": "b"})),
        Some(&json!({"running": false})),
        start + 10_000,
    );
    fx5.clock.fetch_add(50_000, Ordering::SeqCst);
    nested.observe(
        "main",
        "shell.result",
        &object(&json!({"id": "a"})),
        Some(&json!({"running": false})),
        start,
    );
    let hint = nested.end_cell("main", &CellOutcome { text: "", ename: None });
    assert!(
        starts(
            &hint,
            "[hint:blocked-on-job] This cell spent 70 s waiting in job.result()."
        ),
        "{hint:?}"
    );
    // Without completion events waiting is the only way, so no hint.
    let fx4 = Fixture::new();
    let quiet = fx4.make_with(CellHintsOptions {
        async_events: false,
        ..fx4.options()
    });
    assert_eq!(
        fx4.cell(
            &quiet,
            "x",
            &[("shell.result", json!({"id": "j"}), json!({"running": false}), 120_000)],
            OK
        ),
        None
    );
}

#[test]
fn kernel_apis_hints_poll_loop_repeated_status_calls_or_sleeping_in_a_loop_around_a_check() {
    let fx = Fixture::new();
    let hints = fx.make_with(CellHintsOptions {
        max_per_tag: 10,
        ..fx.options()
    });
    let running = json!({"running": true});
    let within = fx.cell(
        &hints,
        "x",
        &[
            ("shell.get", json!({"id": "job-1"}), running.clone(), 10),
            ("shell.get", json!({"id": "job-1"}), running.clone(), 10),
            ("shell.get", json!({"id": "job-1"}), running.clone(), 10),
        ],
        OK,
    );
    assert!(
        starts(
            &within,
            "[hint:poll-loop] Repeated status checks (shell.get job-1) look like polling."
        ),
        "{within:?}"
    );
    assert!(
        within
            .unwrap()
            .contains("Completions arrive as <runtime_event> messages on their own")
    );
    // Consecutive cells asking the same status.
    assert_eq!(
        fx.cell(
            &hints,
            "await agents.status()",
            &[("agents.status", json!({}), json!({}), 10)],
            OK
        ),
        None
    );
    assert!(
        fx.cell(
            &hints,
            "await agents.status()",
            &[("agents.status", json!({}), json!({}), 10)],
            OK
        )
        .unwrap()
        .contains("[hint:poll-loop]")
    );
    // A result fetch of finished work is not a poll.
    assert_eq!(
        fx.cell(
            &hints,
            "a",
            &[("shell.result", json!({"id": "j"}), json!({"running": false}), 10)],
            OK
        ),
        None
    );
    assert_eq!(
        fx.cell(
            &hints,
            "b",
            &[("shell.result", json!({"id": "j"}), json!({"running": false}), 10)],
            OK
        ),
        None
    );
    // Sleep in a loop with a status check.
    let looped = "while True:\n    s = await rlm.job(j)\n    if not s.running: break\n    await asyncio.sleep(5)";
    assert!(starts(
        &fx.cell(&hints, looped, &[], OK),
        "[hint:poll-loop] Sleeping in a loop around a status check"
    ));
    assert_eq!(
        fx.cell(
            &hints,
            "for f in files:\n    await asyncio.sleep(0.1)\n    print(f)",
            &[],
            OK
        ),
        None
    );
}

#[test]
fn kernel_apis_hints_output_truncated_and_large_read() {
    let fx = Fixture::new();
    let hints = fx.make();
    assert!(starts(
        &fx.cell(&hints, "print(data)", &[], (TRUNCATED, None)),
        "[hint:output-truncated] The output was cut"
    ));
    assert!(starts(
        &fx.cell(&hints, "print(open('big.log').read())", &[], (TRUNCATED, None)),
        "[hint:large-read] This cell read a large file into a string. `await read(path)` returns a ContextHandle for files over 256 KiB"
    ));
    let shown = "<str: 900,000 chars, 12,000 lines, sha256 0123456789ab>\nhead: 'x'";
    assert!(
        fx.cell(&hints, "Path('big.log').read_text()", &[], (shown, None))
            .unwrap()
            .contains("[hint:large-read]")
    );
    assert_eq!(fx.cell(&hints, "'x' * 900000", &[], (shown, None)), None);
    assert_eq!(
        fx.cell(
            &hints,
            "Path('small').read_text()",
            &[],
            ("<str: 3,000 chars, 1 lines", None)
        ),
        None
    );
}

#[test]
fn kernel_apis_hints_repeated_failure_the_same_exception_three_cells_in_a_row() {
    let fx = Fixture::new();
    let hints = fx.make();
    // The stuck-loop hint covers failure streaks first; muted, this one fires.
    let module = HintsModule::new(fx.store.clone());
    module_request(&module, "hints.mute", &["stuck-loop"], "main").unwrap();
    let key = Some("KeyError");
    assert_eq!(fx.cell(&hints, "a", &[], ("", key)), None);
    assert_eq!(fx.cell(&hints, "b", &[], ("", key)), None);
    assert!(starts(
        &fx.cell(&hints, "c", &[], ("", key)),
        "[hint:repeated-failure] KeyError 3 cells in a row. Read the traceback carefully"
    ));
    let value = Some("ValueError");
    assert_eq!(fx.cell(&hints, "d", &[], ("", value)), None);
    assert_eq!(fx.cell(&hints, "e", &[], ("", value)), None);
    assert_eq!(fx.cell(&hints, "f", &[], OK), None);
    assert_eq!(fx.cell(&hints, "g", &[], ("", value)), None);
}

#[test]
fn kernel_apis_hints_repeated_cell_three_times_in_a_row_and_again_at_six() {
    let fx = Fixture::new();
    let hints = fx.make();
    let code = "print(len(rows))";
    assert_eq!(fx.cell(&hints, code, &[], OK), None);
    assert_eq!(fx.cell(&hints, &format!("{code}\n"), &[], OK), None);
    assert!(starts(
        &fx.cell(&hints, code, &[], OK),
        "[hint:repeated-cell] This exact cell has now run 3 times in a row"
    ));
    assert_eq!(fx.cell(&hints, code, &[], OK), None);
    assert_eq!(fx.cell(&hints, code, &[], OK), None);
    assert!(fx.cell(&hints, code, &[], OK).unwrap().contains("run 6 times in a row"));
    let paging = Fixture::new();
    let p = paging.make();
    for i in 0..6 {
        assert_eq!(
            paging.cell(&p, &format!("print(rows[{}:{}])", i * 10, i * 10 + 10), &[], OK),
            None
        );
    }
    let broken = Fixture::new();
    let b = broken.make();
    broken.cell(&b, code, &[], OK);
    broken.cell(&b, code, &[], ("", Some("KeyError")));
    assert_eq!(broken.cell(&b, code, &[], OK), None);
}

#[test]
fn kernel_apis_hints_truncation_marker_naming_the_file_is_recognized() {
    let fx = Fixture::new();
    let hints = fx.make();
    assert!(starts(
        &fx.cell(
            &hints,
            "print(data)",
            &[],
            (
                "head\n[... 5000 bytes truncated; full stdout in /tmp/x/1-2-stdout.txt ...]\ntail",
                None
            )
        ),
        "[hint:output-truncated]"
    ));
}

#[test]
fn kernel_apis_hints_one_hint_per_cell_most_specific_first_a_spent_tag_gives_way() {
    let fx = Fixture::new();
    let hints = fx.make_with(CellHintsOptions {
        max_per_tag: 1,
        ..fx.options()
    });
    let detach = || {
        (
            "bash",
            json!({"yield_after": 30}),
            json!({"running": true, "job": {"id": "job-9"}}),
            10,
        )
    };
    let first = fx.cell(&hints, "x1", &[detach()], (TRUNCATED, None));
    assert!(starts(&first, "[hint:job-detached]"));
    assert!(!first.unwrap().contains("output-truncated"));
    assert!(starts(
        &fx.cell(&hints, "x2", &[detach()], (TRUNCATED, None)),
        "[hint:output-truncated]"
    ));
    assert_eq!(fx.cell(&hints, "x3", &[detach()], (TRUNCATED, None)), None);
}

#[test]
fn kernel_apis_hints_mutes_are_per_lane_and_persist_each_tag_fires_at_most_n_times() {
    let fx = Fixture::new();
    let module = Arc::new(HintsModule::new(fx.store.clone()));
    let hints = CellHints::new(module.clone(), fx.options());
    assert_eq!(
        module_request(&module, "hints.mute", &["output-truncated", "poll-loop"], "main").unwrap(),
        json!({"muted": ["output-truncated", "poll-loop"]})
    );
    assert_eq!(fx.cell(&hints, "x", &[], (TRUNCATED, None)), None);
    // Another lane (a subagent) is unaffected.
    assert!(
        fx.cell_on(&hints, "ultron.rlm-child.t1", "x", &[], (TRUNCATED, None))
            .unwrap()
            .contains("[hint:output-truncated]")
    );
    assert_eq!(
        module_request(&module, "hints.unmute", &["output-truncated"], "main").unwrap(),
        json!({"muted": ["poll-loop"]})
    );
    assert_eq!(
        module_request(&module, "hints.muted", &[], "main").unwrap(),
        json!({"muted": ["poll-loop"]})
    );
    let fired: Vec<Option<String>> = (0..5)
        .map(|i| fx.cell(&hints, &format!("x{i}"), &[], (TRUNCATED, None)))
        .collect();
    assert_eq!(fired.iter().filter(|h| h.is_some()).count(), 3);
    // Mutes and counts are a session value: a restarted worker keeps them.
    let again_module = Arc::new(HintsModule::new(fx.store.clone()));
    let again = CellHints::new(again_module.clone(), fx.options());
    assert_eq!(
        module_request(&again_module, "hints.muted", &[], "main").unwrap(),
        json!({"muted": ["poll-loop"]})
    );
    assert_eq!(fx.cell(&again, "x", &[], (TRUNCATED, None)), None);
    let stored = kernel_apis::DocumentStore::read(fx.store.as_ref()).unwrap().unwrap();
    assert_eq!(stored["version"], 1);
    assert_eq!(stored["lanes"]["main"]["fired"]["output-truncated"], 3);
    // Disabled: nothing fires.
    let fx2 = Fixture::new();
    let off = fx2.make_with(CellHintsOptions {
        enabled: false,
        ..fx2.options()
    });
    assert_eq!(fx2.cell(&off, "x", &[], (TRUNCATED, None)), None);
    assert!(!hints_enabled(Some("off")));
    assert!(!hints_enabled(None));
    assert!(hints_enabled(Some("on")));
    assert_eq!(hint_max_per_tag(Some("1")), 1);
    assert_eq!(hint_max_per_tag(None), 3);
    assert_eq!(read_handle_bytes(None), 256 * 1024);
}
