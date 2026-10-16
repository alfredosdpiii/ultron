//! `schedules.*` and `goal.*` reach every lane's REPL, as session-worker.ts registers them for the whole host; the
//! goal module itself admits only the root and the goal's job (goal.ts `#handle`: "Only the goal's job or the root
//! agent works the session goal; subagents report to it"). The goal's job works it from its own REPL lane:
//! `goal.check`, `goal.blocked` and `goal.complete` from the job's cells.

mod p_session_support;

use delegation::lanes::CellOutcome;
use p_session_support::{Setup, actor, text, tool};
use serde_json::{Value, json};
use session_core::RunEnv;
use session_core::compose::{Runtime, Services};
use session_core::core::Op;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const HANG: Duration = Duration::from_secs(120);

/// One cell on `lane`; its output, or its error.
fn cell(stack: &session_core::compose::Stack, lane: &str, code: &str) -> String {
    stack.lanes.open(lane).unwrap();
    match stack.lanes.run(lane, code, HANG).unwrap() {
        CellOutcome::Done(Ok(result)) => format!(
            "{}{}{}",
            result.stdout,
            result.result.unwrap_or_default(),
            result
                .error
                .map(|e| format!("{}: {}", e.ename, e.evalue))
                .unwrap_or_default()
        ),
        other => panic!("cell did not end: {other:?}"),
    }
}

#[test]
fn session_core_schedules_and_goal_reach_every_lane_and_the_goal_admits_only_the_root_and_its_job() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let agent_dir = home.join(".ultron/agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({"sandbox": {"mode": "off"}}).to_string(),
    )
    .unwrap();
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent_dir.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
    ];
    let env = RunEnv {
        cwd,
        home,
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let runtime = Runtime::new(&env, "print", root.join("sessions"), Services::default()).unwrap();
    let model = ai::models::builtin_model("anthropic", "claude-sonnet-4-5")
        .unwrap()
        .clone();
    let stack = runtime.stack(&model, None, "goal-lanes").unwrap();
    // A subagent's lane lists the schedules and reads the goal.
    let listed = cell(
        &stack,
        "lane-x",
        "import json\nprint(json.dumps(await rlm.host_request('schedules.list', {})))",
    );
    assert_eq!(listed.trim(), "[]");
    assert_eq!(cell(&stack, "lane-x", "print(await goal.get())").trim(), "None");
    // The goal (its job cannot start without the session's runtime: the goal pauses).
    stack
        .goal
        .command("Make the tests pass", &kernel::Signal::new())
        .unwrap();
    let complete = "try:\n    await goal.complete(1, 'done', ['ok'])\nexcept Exception as e:\n    print(e)";
    assert_eq!(
        cell(&stack, "lane-x", complete).trim(),
        "Only the goal's job or the root agent works the session goal; subagents report to it"
    );
    // The root passes the gate (and gets the goal's own answer).
    let from_root = cell(&stack, "main", complete);
    assert!(from_root.contains("The session goal is"), "{from_root}");
    stack.close();
}

/// The text of every user message of a request.
fn body_text(body: &Value) -> String {
    body["messages"].to_string()
}

#[test]
fn session_core_the_goal_job_checks_and_completes_the_goal_from_its_own_repl() {
    let job_cell = [
        "g = await goal.get()",
        "try:\n    await goal.check()\nexcept Exception as e:\n    print('check:', e)",
        "r = await goal.complete(g['revision'], 'done', ['the tests pass'])",
        "print('complete:', r['complete'], r['status'])",
    ]
    .join("\n");
    let script = Box::new(move |body: &Value, _| {
        let messages = body_text(body);
        if messages.contains("GOAL-OBJECTIVE-7") && !messages.contains("tool_result") {
            tool("toolu_job", &job_cell)
        } else {
            text("job done")
        }
    });
    let a = actor(script, Setup::default());
    let set = a.call(Op::Goal {
        args: "GOAL-OBJECTIVE-7: make the tests pass".into(),
    });
    assert!(set.is_ok(), "{set:?}");
    // The job ran its cell on its own lane: the next request carries the cell's result.
    let end = Instant::now() + HANG;
    let result = loop {
        let found = a
            .requests()
            .iter()
            .flat_map(p_session_support::tool_results)
            .find(|r| r.to_string().contains("complete:"));
        if let Some(found) = found {
            break found;
        }
        assert!(Instant::now() < end, "the job's cell did not run: {:?}", a.requests());
        std::thread::sleep(Duration::from_millis(20));
    };
    let shown = result.to_string();
    // Past the lane gate: the goal's own answers.
    assert!(shown.contains("check: This goal has no check command"), "{shown}");
    assert!(shown.contains("complete: True complete"), "{shown}");
    let status = a.call(Op::Goal { args: "status".into() }).unwrap().unwrap();
    assert!(status.to_string().contains("complete"), "{status}");
}
