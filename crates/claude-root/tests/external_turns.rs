//! The external root's turn plumbing, ported from packages/coding-agent/test/ultron-claude-host.test.ts ("the
//! external root": a user turn opens a root turn with Loki's note, Stop closes it; the research brake and the wait
//! nudge are appended to results without promising a wake-up; a spent budget refuses the cell) with the literal
//! texts of external-root.ts and tool-round-nudge.ts; and "a root that is itself a subagent spawns only within its
//! allowance and relays its verdict" / "harness-lane children get the configured child model when the spawn names
//! none" on the delegation host (`rootSpawn`, `rootFinish`, `childModel`).

use ai::AbortSignal;
use claude_root::external_root::{CellRunner, EventSource, ExternalRoot, RootHost};
use delegation::host::{ChildRun, Delegation, DelegationOptions};
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};

type Log = Arc<Mutex<Vec<String>>>;

struct FakeHost {
    events: Log,
    running: u64,
    exhausted: Option<String>,
}

impl RootHost for FakeHost {
    fn begin_root_turn(&self, turn: &str) {
        self.events.lock().unwrap().push(format!("begin {turn}"));
    }
    fn end_root_turn(&self, turn: &str) {
        self.events.lock().unwrap().push(format!("end {turn}"));
    }
    fn pending_root_notifications(&self) -> u64 {
        self.running
    }
    fn turn_budget_exhausted(&self, root: &str) -> Option<String> {
        self.events.lock().unwrap().push(format!("budget {root}"));
        self.exhausted.clone()
    }
    fn run_ended(&self, lane: &str) {
        self.events.lock().unwrap().push(format!("hints {lane}"));
    }
    fn begin_file_turn(&self) {
        self.events.lock().unwrap().push("files".into());
    }
}

const EVENT: &str = r#"<runtime_event kind="job_done" id="job-1" status="completed" summary="exit 0" fetch="await rlm.job(&quot;job-1&quot;)" />"#;

struct Fixture {
    root: ExternalRoot,
    events: Log,
    calls: Log,
    waiting: Log,
}

fn controller(running: u64, exhausted: Option<&str>, nudge: u64, skill: u64) -> Fixture {
    let (events, calls, waiting): (Log, Log, Log) = Default::default();
    let source = waiting.clone();
    let event_source: EventSource = Arc::new(move || std::mem::take(&mut *source.lock().unwrap()));
    let seen = calls.clone();
    let runner: CellRunner = Arc::new(move |code: &str, _: &AbortSignal| {
        seen.lock().unwrap().push(code.to_string());
        if code == "raise" {
            return Err("ZeroDivisionError: division by zero".into());
        }
        Ok(vec![json!({"type": "text", "text": format!("ran {code}")})])
    });
    let root = ExternalRoot::new(event_source, runner, Some("LOKI policy".into()))
        .with_host(Arc::new(FakeHost {
            events: events.clone(),
            running,
            exhausted: exhausted.map(str::to_string),
        }))
        .with_nudges(nudge, skill, true)
        .with_loki_notice(Some("Loki created .loki/ and committed it.".into()));
    Fixture {
        root,
        events,
        calls,
        waiting,
    }
}

fn text(result: &claude_root::external_root::CellResult) -> String {
    result.content[0]["text"].as_str().unwrap().to_string()
}

#[test]
fn claude_root_external_user_turn_opens_a_root_turn_and_stop_closes_it() {
    let f = controller(0, None, 0, 0);
    // The policy note and, once, the setup note.
    assert_eq!(
        f.root.session_context().as_deref(),
        Some("LOKI policy\n\nLoki created .loki/ and committed it.")
    );
    assert_eq!(f.root.session_context().as_deref(), Some("LOKI policy"));
    f.waiting.lock().unwrap().push(EVENT.into());
    let context = f.root.begin_turn().unwrap();
    // The setup note was already shown with the session context.
    assert_eq!(context, format!("Runtime events since your last rlm call:\n{EVENT}"));
    let turn = f.root.turn().unwrap();
    f.root.end_turn();
    assert_eq!(f.root.turn(), None);
    assert_eq!(
        *f.events.lock().unwrap(),
        vec![
            format!("begin {turn}"),
            "files".into(),
            format!("end {turn}"),
            "hints main".into()
        ]
    );
}

#[test]
fn claude_root_external_setup_note_comes_with_the_first_user_turn_when_the_session_did_not_show_it() {
    let f = controller(0, None, 0, 0);
    assert_eq!(
        f.root.begin_turn().as_deref(),
        Some("Loki created .loki/ and committed it.")
    );
    f.waiting.lock().unwrap().push(EVENT.into());
    // A second prompt while the first turn is open ends it first; the note is not repeated.
    let first = f.root.turn().unwrap();
    assert_eq!(
        f.root.begin_turn(),
        Some(format!("Runtime events since your last rlm call:\n{EVENT}"))
    );
    let second = f.root.turn().unwrap();
    assert_eq!(
        *f.events.lock().unwrap(),
        vec![
            format!("begin {first}"),
            "files".into(),
            format!("end {first}"),
            "hints main".into(),
            format!("begin {second}"),
            "files".into()
        ]
    );
}

#[test]
fn claude_root_external_wait_nudge_is_appended_without_promising_a_wake_up() {
    let f = controller(1, None, 2, 0);
    assert_eq!(text(&f.root.run_cell("a", &AbortSignal::new())), "ran a");
    // The second round reaches the brake while a subagent still runs: the wait nudge comes with that result.
    let second = text(&f.root.run_cell("b", &AbortSignal::new()));
    assert_eq!(
        second,
        "ran b\n[Ultron] 1 subagent or task you started is still running; each end is reported as a `child_done` (or `task_done`) event at the top of a later rlm result. Do only separate work of your own and do not check on them through their files, logs or progress. If nothing of your own is left, wait for free: `await rlm.collect(handles)` (nothing wakes you after you reply)."
    );
    assert!(!second.contains("events wake you"));
}

#[test]
fn claude_root_external_research_brake_counts_cells_of_a_turn_and_starts_over_with_the_next() {
    let f = controller(0, None, 2, 0);
    let run = |code: &str| text(&f.root.run_cell(code, &AbortSignal::new()));
    assert_eq!(run("a"), "ran a");
    assert_eq!(
        run("b"),
        "ran b\n[Ultron] You have used 2 rounds of tool calls in this turn without answering. Unless one more specific call is essential, stop gathering and answer now with what you have, noting what remains unknown."
    );
    assert_eq!(run("c"), "ran c");
    assert_eq!(
        run("d"),
        "ran d\n[Ultron] You have now used 4 rounds of tool calls in this turn. Stop calling tools and write your answer now from what you already have. List anything still unknown instead of searching further."
    );
    // Stop ends the turn: the next turn's count starts from zero.
    f.root.end_turn();
    assert_eq!(run("e"), "ran e");
}

#[test]
fn claude_root_external_skill_hint_follows_successful_cells_once() {
    let f = controller(0, None, 0, 2);
    let run = |code: &str| text(&f.root.run_cell(code, &AbortSignal::new()));
    assert_eq!(run("a"), "ran a");
    assert!(f.root.run_cell("raise", &AbortSignal::new()).is_error);
    assert_eq!(run("b"), "ran b");
    assert_eq!(
        run("c"),
        "ran c\n[Ultron] 2 tool rounds in a row succeeded. If this procedure will recur, once it works save it as a tested skill with `await skills.propose_code(name, source, test_source, evidence)`; otherwise carry on."
    );
    assert_eq!(run("d"), "ran d");
    assert_eq!(run("e"), "ran e");
}

#[test]
fn claude_root_external_spent_budget_refuses_the_cell() {
    let f = controller(0, Some("token budget of turn:x exhausted"), 0, 0);
    f.waiting.lock().unwrap().push(EVENT.into());
    let result = f.root.run_cell("x", &AbortSignal::new());
    assert!(result.is_error);
    assert_eq!(
        text(&result),
        format!("{EVENT}\n[Ultron] token budget of turn:x exhausted; answer with what you have.")
    );
    assert!(f.calls.lock().unwrap().is_empty());
    // The budget asked about is the open turn's root.
    let turn = f.root.turn().unwrap();
    assert!(f.events.lock().unwrap().contains(&format!("budget turn:{turn}")));
    assert_eq!(f.root.cells(), 1);
}

/// Each child run: its name, level, allowance and model.
type Runs = Arc<Mutex<Vec<(String, u32, u32, Option<String>)>>>;

/// A delegation host whose children are recorded and answered at once.
fn host(runs: Runs) -> Delegation {
    let runner = Arc::new(move |run: ChildRun| {
        runs.lock()
            .unwrap()
            .push((run.name.clone(), run.level, run.allowance, run.model.clone()));
        Ok("ok".to_string())
    });
    Delegation::new(DelegationOptions::new(runner)).0
}

fn spawn(host: &Delegation, kwargs: Value) -> Result<Value, String> {
    host.handle("rlm.spawn", json!({"prompt": "x", "kwargs": kwargs}), "main")
}

#[test]
fn claude_root_subagent_root_spawns_only_within_its_allowance_and_relays_its_verdict() {
    let runs: Runs = Arc::default();
    let no_depth = host(Arc::clone(&runs));
    no_depth.set_root_spawn(1, Some(0));
    let relayed: Arc<Mutex<Vec<Map<String, Value>>>> = Arc::default();
    let seen = relayed.clone();
    no_depth.set_root_finish(Arc::new(move |payload| {
        seen.lock().unwrap().push(payload);
        Ok(json!({"recorded": true}))
    }));
    assert!(
        spawn(&no_depth, json!({"name": "x"}))
            .unwrap_err()
            .contains("started with depth=0")
    );
    assert_eq!(
        no_depth
            .handle(
                "rlm.finish",
                json!({"status": "blocked", "summary": "no network"}),
                "main"
            )
            .unwrap(),
        json!({"recorded": true})
    );
    assert_eq!(
        *relayed.lock().unwrap(),
        vec![
            json!({"status": "blocked", "summary": "no network"})
                .as_object()
                .unwrap()
                .clone()
        ]
    );
    let with_depth = host(Arc::clone(&runs));
    with_depth.set_root_spawn(1, Some(2));
    assert!(
        spawn(&with_depth, json!({"name": "x", "depth": 2}))
            .unwrap_err()
            .contains("too deep here: at most 1")
    );
    assert_eq!(
        spawn(&with_depth, json!({"name": "x", "depth": 1})).unwrap()["name"],
        "x"
    );
    // The child is two levels down (one above the root) and may create one more.
    let id = spawn(&with_depth, json!({"name": "y", "depth": 1})).unwrap()["rlm_child_id"].clone();
    with_depth.wait(&[id.as_str().unwrap().to_string()], None);
    assert!(
        runs.lock().unwrap().contains(&("y".into(), 2, 1, None)),
        "{:?}",
        runs.lock().unwrap()
    );
    // Levels above the root count toward ULTRON_SPAWN_DEPTH (default 3): a root three levels down spawns nothing.
    let deep = host(Arc::clone(&runs));
    deep.set_root_spawn(3, None);
    assert!(
        spawn(&deep, json!({"name": "z"}))
            .unwrap_err()
            .contains("subagents nest at most 3 level(s) deep")
    );
    // Without a relay a root's verdict is refused as before.
    assert!(
        host(runs)
            .handle("rlm.finish", json!({"status": "passed", "summary": "s"}), "main")
            .unwrap_err()
            .contains("rlm.finish is for subagents started with rlm.spawn")
    );
}

#[test]
fn claude_root_harness_lane_children_get_the_configured_child_model() {
    let runs: Runs = Arc::default();
    let host = host(Arc::clone(&runs));
    host.set_child_model(Some("cliproxyapi/glm-5.3-flash".into()));
    assert_eq!(
        spawn(&host, json!({"name": "x"})).unwrap()["model"],
        "cliproxyapi/glm-5.3-flash"
    );
    assert_eq!(
        spawn(&host, json!({"name": "y", "model": "other/m"})).unwrap()["model"],
        "other/m"
    );
}
