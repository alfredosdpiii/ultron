//! The host modules' documents are values of the session (session-worker.ts `createSessionModuleStore(session,
//! name)` at `ultron.module/<name>`, the local services at `ultron.local/<key>`): resuming restores them, a tree
//! fork copies them, a branch fork starts without them (fork-policy.ts `projectForkCurrentStateWrite`), and nothing
//! is kept beside the session. The shell jobs' journal (`ultron.module/jobs`) is the observable one here.

mod p_session_support;
mod reopen_support;

use p_session_support::{text, tool, tool_results};
use reopen_support::Home;
use serde_json::Value;
use session_core::core::Op;
use session_journal::{ForkOptions, SessionRepo, SystemClock};
use std::path::PathBuf;

const START_JOB: &str = "j = await bash(\"echo kept-output\", yield_after=0)\nj = await j.result(wait=60)\nprint(\"STARTED\", j.id, j.status)";
const LIST_JOBS: &str = "print(\"LISTED\", [(j.id, j.status) for j in await rlm.jobs()])";

fn prompt(w: &reopen_support::Worker, text: &str) -> Value {
    w.call(Op::PromptAndWait {
        message: text.into(),
        images: Vec::new(),
    })
    .unwrap()
    .unwrap()
}

/// A script that runs `cell` once per prompt (a resumed session's requests carry the earlier turns), then answers.
fn once(cell: &'static str) -> p_session_support::Script {
    Box::new(move |request, _| {
        let last = request["messages"]
            .as_array()
            .and_then(|m| m.last())
            .cloned()
            .unwrap_or_default();
        let answered_cell = last["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"));
        if answered_cell {
            text("done")
        } else {
            tool("toolu_1", cell)
        }
    })
}

fn last_result(w: &reopen_support::Worker) -> String {
    let requests = w.requests();
    tool_results(requests.last().unwrap())
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

fn sessions(home: &Home) -> PathBuf {
    home.root.join("agent").join("experimental").join("sessions")
}

/// Fork the only session with `options`, then keep only the fork (the support opens the one session it finds).
fn fork_only(home: &Home, options: &ForkOptions) -> String {
    let repo = SessionRepo::new(&sessions(home), SystemClock::shared());
    let source = repo.list(None).into_iter().next().unwrap();
    let fork = repo.fork(&source, options, None).unwrap();
    let id = fork.id().to_string();
    drop(fork);
    std::fs::remove_file(&source.path).unwrap();
    id
}

fn started_job(home: &Home) -> String {
    let w = home.open(once(START_JOB), &[], None);
    prompt(&w, "start a job");
    let result = last_result(&w);
    w.close();
    let start = result.find("STARTED ").expect("the cell started a job") + "STARTED ".len();
    let id = result[start..].split(' ').next().unwrap().to_string();
    assert!(result.contains(&format!("{id} completed")), "{result}");
    id
}

#[test]
fn session_core_module_values_jobs_journal_is_a_session_value_that_resume_restores() {
    let home = Home::new(None);
    let id = started_job(&home);
    // The journal is the session value `ultron.module/jobs`; no module file is kept beside the session.
    let jobs: Vec<Value> = home
        .writes()
        .into_iter()
        .filter(|w| w["namespace"] == "ultron.module" && w["key"] == "jobs" && w["op"] == "set")
        .map(|w| w["value"].clone())
        .collect();
    assert!(jobs.last().unwrap().to_string().contains(&id), "{jobs:?}");
    let files: Vec<_> = walk(&home.root);
    assert!(files.iter().all(|f| !f.ends_with("jobs.json")), "{files:?}");
    // A resumed session lists the job.
    let w = home.open(once(LIST_JOBS), &[], None);
    prompt(&w, "list jobs");
    let listed = last_result(&w);
    assert!(listed.contains(&format!("('{id}', 'completed')")), "{listed}");
}

#[test]
fn session_core_module_values_a_tree_fork_copies_them_and_a_branch_fork_does_not() {
    let home = Home::new(None);
    let id = started_job(&home);
    fork_only(&home, &ForkOptions::Tree);
    let w = home.open(once(LIST_JOBS), &[], None);
    prompt(&w, "list jobs");
    assert!(
        last_result(&w).contains(&format!("('{id}', 'completed')")),
        "{}",
        last_result(&w)
    );
    w.close();
    // A branch fork projects only the branch's state: the fork's module values start empty.
    fork_only(
        &home,
        &ForkOptions::Branch {
            branch: "main".into(),
            entry_id: None,
            any_tree_entry: false,
            before: false,
        },
    );
    let w = home.open(once(LIST_JOBS), &[], None);
    prompt(&w, "list jobs");
    let listed = last_result(&w);
    assert!(listed.contains("LISTED []"), "{listed}");
}

fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}
