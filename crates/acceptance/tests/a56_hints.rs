//! A56: situational hints point the model at completion events instead of waiting or polling, in the real CLI.
//! The detection is ported with literal expectations in crates/kernel-apis/tests/cell_hints.rs (from
//! ultron-hints.test.ts); this checks the live root lane: with `ULTRON_HINTS=on` the `rlm` tool result of a cell
//! whose plain `bash` came back running (past `ULTRON_BASH_YIELD_AFTER`) ends with the `job-detached` line, the next status check of that job in a
//! following cell earns `poll-loop`, and without `ULTRON_HINTS` nothing is added.

mod common;

use common::{HANG, Install, Provider, Reply, Request};

const PROMPT: &str = "HINTS: start the slow tests";

fn last_tool_result(request: &Request) -> Option<String> {
    let last = request.body["messages"].as_array()?.last()?.clone();
    (last["role"] == "tool").then(|| request.tool_results().last().cloned().unwrap_or_default())
}

fn script(request: &Request) -> Reply {
    match request.messages("assistant").len() {
        0 => Reply::Rlm(
            "out = await bash('''while [ ! -e go ]; do sleep 0.05; done; echo done''')\nprint('STARTED', out.running)"
                .into(),
        ),
        1 => Reply::Rlm("print((await rlm.job(out.job.id)).running)".into()),
        2 => Reply::Rlm("print((await rlm.job(out.job.id)).running)".into()),
        _ => Reply::Text("WAITING FOR THE EVENT".into()),
    }
}

fn run(env: &[(&str, &str)]) -> Vec<String> {
    let provider = Provider::start(script);
    let install = Install::new(&provider);
    // A plain bash still running after a second continues as a job (`ULTRON_BASH_YIELD_AFTER`).
    let mut env = env.to_vec();
    env.push(("ULTRON_BASH_YIELD_AFTER", "1"));
    let mut client = install.rpc(&["--no-session"], &env);
    client.prompt_and_wait(PROMPT, HANG).unwrap();
    let results: Vec<String> = provider.requests().iter().filter_map(last_tool_result).collect();
    // Let the job end so nothing outlives the test; its event re-invokes the root once.
    std::fs::write(install.project().join("go"), "").unwrap();
    while client.transcript.iter().filter(|l| l["type"] == "agent_end").count() < 2 {
        client.mark();
        client.wait_for(&|l| l["type"] == "agent_end", HANG).unwrap();
    }
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
    results
}

#[test]
fn acceptance_a56_hints_job_detached_and_poll_loop_end_the_live_cell_results() {
    let results = run(&[("ULTRON_HINTS", "on")]);
    assert_eq!(results.len(), 3, "{results:#?}");
    let first = &results[0];
    assert!(first.starts_with("STARTED True\n"), "{first}");
    let hint = first.lines().last().unwrap();
    assert!(
        hint.starts_with("[hint:job-detached] The command was still running after 1 s, so it continues as job job-"),
        "{first}"
    );
    assert!(hint.contains("its completion arrives as a <runtime_event> that starts your next turn"));
    assert!(hint.ends_with("(Mute this hint: `await hints.mute(\"job-detached\")`.)"));
    // One status check is not a poll; the same check again in the next cell is.
    assert_eq!(results[1], "True");
    let second = &results[2];
    assert!(
        second.starts_with("True\n[hint:poll-loop] Repeated status checks (shell.get job-"),
        "{second}"
    );
}

#[test]
fn acceptance_a56_hints_are_off_unless_asked_for() {
    let results = run(&[]);
    assert_eq!(results.len(), 3, "{results:#?}");
    assert_eq!(results[0], "STARTED True");
    assert!(results.iter().all(|r| !r.contains("[hint:")), "{results:#?}");
}
