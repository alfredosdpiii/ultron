//! A56: completion is delivered without polling, in the real CLI. Ported from
//! packages/coding-agent/test/ultron-async-cli.test.ts against the `ultron` binary in RPC mode (and `-p`) with a
//! scripted provider: a detached shell job's end arrives as a `job_done` `<runtime_event>` custom message that
//! re-invokes the idle root once; print mode follows that run; a completion during a running turn is steered to the
//! next boundary without a second run; a spent turn budget turns re-invocation into quiet delivery with the next
//! prompt; Esc on the turn stops its job and nothing restarts the model; and each root request extends the previous
//! one byte for byte. The detach case runs with `ULTRON_HINTS=on` and checks the `[hint:job-detached]` line; the
//! collapse-on-return case is in a56_collapse_cli.rs.
//!
//! Where the TypeScript test sleeps until a job's `sleep N` is over, these jobs wait for a file the test writes, and
//! "nothing else happens" is checked after the root lane reports no pending work (`async.pending` through
//! `get_state`-driven settles), never on a timer alone.

mod common;

use common::{HANG, Install, Provider, Reply, Request};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// `<runtime_event kind="job_done" id="job-…" status="…" summary="…" fetch="…" />` in a text: (id, status, summary,
/// fetch).
fn job_event(text: &str) -> Option<(String, String, String, String)> {
    let at = text.find("<runtime_event kind=\"job_done\"")?;
    let line = text[at..].lines().next()?;
    let field = |name: &str| -> Option<String> {
        let start = line.find(&format!("{name}=\""))? + name.len() + 2;
        let end = line[start..].find('"')? + start;
        Some(line[start..end].to_string())
    };
    Some((field("id")?, field("status")?, field("summary")?, field("fetch")?))
}

/// The last user message's text (custom event messages reach the model as user messages).
fn last_user(request: &Request) -> String {
    request.users().last().cloned().unwrap_or_default()
}

/// The text of the last message when it is a tool result.
fn last_tool_result(request: &Request) -> Option<String> {
    let messages = request.body["messages"].as_array()?;
    let last = messages.last()?;
    (last["role"] == "tool").then(|| request.tool_results().last().cloned().unwrap_or_default())
}

/// Model turns already in the request (its assistant messages).
fn turn(request: &Request) -> usize {
    request.messages("assistant").len()
}

/// The first user message that is not a clock line.
fn first_user(request: &Request) -> String {
    request
        .users()
        .into_iter()
        .find(|u| !u.starts_with("[clock]"))
        .unwrap_or_default()
}

const PROMPT: &str = "ASYNC: run the slow test suite, and tell me when it passes";

/// Starts a job that runs until the test writes `go`, answers at once, and acts on the completion event.
fn job_script(request: &Request) -> Reply {
    if first_user(request) != PROMPT {
        return Reply::Text("unexpected".into());
    }
    if let Some((id, ..)) = job_event(&last_user(request))
        && last_tool_result(request).is_none()
    {
        return Reply::Rlm(format!(
            "j = await rlm.job(\"{id}\")\nprint(\"RESULT\", j.exit_code, j.text.strip())"
        ));
    }
    if let Some(result) = last_tool_result(request).filter(|r| r.starts_with("RESULT")) {
        return Reply::Text(format!("TESTS PASSED: {}", result.trim()));
    }
    if turn(request) == 0 {
        return Reply::Rlm(
            "job = await bash('''while [ ! -e go ]; do sleep 0.05; done; touch job.done; echo '5 passed in 5.0s' ''', yield_after=0)\nprint('STARTED', job.running)".into(),
        );
    }
    if last_user(request) == "SECOND: thanks" {
        return Reply::Text("YOU ARE WELCOME".into());
    }
    Reply::Text("STARTED THE TESTS; I will report when they finish.".into())
}

fn count(client: &cli::rpc::client::RpcClient, kind: &str) -> usize {
    client.transcript.iter().filter(|l| l["type"] == kind).count()
}

/// Read RPC lines until `n` events of `kind` arrived in total.
fn wait_events(client: &mut cli::rpc::client::RpcClient, kind: &str, n: usize) {
    while count(client, kind) < n {
        client.mark();
        client
            .wait_for(&|l| l["type"] == kind, HANG)
            .unwrap_or_else(|e| panic!("waiting for {kind} #{n}: {e}"));
    }
}

/// The root lane reports no root-owned work left (`async.pending` of the session worker): nothing can start
/// another run.
fn settled(install: &Install, client: &mut cli::rpc::client::RpcClient) {
    let _ = install;
    common::wait_until("the session has no pending work", || {
        client
            .get_state()
            .is_ok_and(|s| s["isStreaming"] == false && s["pendingMessageCount"] == 0)
    });
}

#[test]
fn acceptance_a56_a_slow_job_does_not_block_the_turn_and_its_completion_event_reinvokes_the_model() {
    let provider = Provider::start(job_script);
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[]);
    client.prompt_and_wait(PROMPT, HANG).unwrap();
    // The turn ended while the job was still running: it has not finished.
    assert!(!install.project().join("job.done").exists());
    assert!(
        client
            .get_last_assistant_text()
            .unwrap()
            .unwrap_or_default()
            .contains("STARTED THE TESTS")
    );
    let requests = provider.requests();
    assert_eq!(last_tool_result(&requests[0]), None);
    assert!(last_tool_result(&requests[1]).unwrap().contains("STARTED True"));
    std::fs::write(install.project().join("go"), "").unwrap();
    // The completion starts exactly one more run, and the model fetches the full result.
    wait_events(&mut client, "agent_end", 2);
    assert_eq!(
        client.get_last_assistant_text().unwrap().as_deref(),
        Some("TESTS PASSED: RESULT 0 5 passed in 5.0s")
    );
    let requests = provider.requests();
    let event = requests
        .iter()
        .find_map(|r| job_event(&last_user(r)))
        .expect("a request carries the job's event");
    let (id, status, summary, fetch) = event;
    assert_eq!(
        (status.as_str(), summary.as_str()),
        ("completed", "exit 0; 5 passed in 5.0s")
    );
    assert_eq!(fetch, format!("await rlm.job(&quot;{id}&quot;)"));
    // RPC clients see the event as a normal Pi custom message in the transcript.
    let messages = client.get_messages().unwrap();
    assert!(
        messages
            .iter()
            .any(|m| m["role"] == "custom" && m["customType"] == "ultron-runtime-event" && m["display"] == true),
        "{messages:#?}"
    );
    // Nothing loops: no further runs after the model answered the event.
    settled(&install, &mut client);
    assert_eq!(count(&client, "agent_start"), 2);
    assert_eq!(provider.requests().len(), 4);
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}

#[test]
fn acceptance_a56_a_plain_bash_past_the_yield_window_detaches_and_its_completion_event_reinvokes_the_model() {
    const DETACH: &str = "DETACH: run the slow tests";
    let provider = Provider::start(|request: &Request| {
        if first_user(request) != DETACH {
            return Reply::Text("unexpected".into());
        }
        if let Some((id, ..)) = job_event(&last_user(request))
            && last_tool_result(request).is_none()
        {
            return Reply::Rlm(format!(
                "j = await rlm.job(\"{id}\")\nprint(\"RESULT\", j.exit_code, j.text.strip())"
            ));
        }
        if let Some(result) = last_tool_result(request).filter(|r| r.starts_with("RESULT")) {
            return Reply::Text(format!("DONE: {}", result.trim()));
        }
        if turn(request) == 0 {
            return Reply::Rlm("out = await bash('''while [ ! -e go ]; do sleep 0.05; done; touch job.done; echo '3 passed' ''')\nprint('RUNNING', out.running, out.exit_code)\nout".into());
        }
        Reply::Text("THE TESTS ARE RUNNING; I will report when they finish.".into())
    });
    let install = Install::new(&provider);
    // Situational hints are off by default; this row checks the job-detached hint.
    let mut client = install.rpc(
        &["--no-session"],
        &[("ULTRON_BASH_YIELD_AFTER", "1"), ("ULTRON_HINTS", "on")],
    );
    client.prompt_and_wait(DETACH, HANG).unwrap();
    assert!(!install.project().join("job.done").exists());
    let cell = last_tool_result(&provider.requests()[1]).unwrap();
    assert!(cell.starts_with("RUNNING True None\n"), "{cell}");
    assert!(
        cell.contains("[still running as job job-")
            && cell.contains(" after 1 s; its completion will arrive as a runtime event"),
        "{cell}"
    );
    // The runtime's one-line hint ends the result.
    let hint = cell.lines().last().unwrap_or_default();
    let job = hint
        .strip_prefix("[hint:job-detached] The command was still running after 1 s, so it continues as job job-")
        .unwrap_or_else(|| panic!("{cell}"));
    let (hex, rest) = job.split_once('.').unwrap();
    assert!(!hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()), "{hint}");
    assert!(rest.starts_with(" Do not wait for it"), "{hint}");
    std::fs::write(install.project().join("go"), "").unwrap();
    wait_events(&mut client, "agent_end", 2);
    assert_eq!(
        client.get_last_assistant_text().unwrap().as_deref(),
        Some("DONE: RESULT 0 3 passed")
    );
    let event = provider
        .requests()
        .iter()
        .find_map(|r| job_event(&last_user(r)))
        .unwrap();
    assert_eq!((event.1.as_str(), event.2.as_str()), ("completed", "exit 0; 3 passed"));
    settled(&install, &mut client);
    assert_eq!(count(&client, "agent_start"), 2);
    assert_eq!(provider.requests().len(), 4);
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}

#[test]
fn acceptance_a56_print_mode_follows_the_run_a_completion_event_starts_and_prints_its_answer() {
    let provider = Provider::start(job_script);
    let install = Install::new(&provider);
    let go = install.project().join("go");
    // The job may end only after the first turn's answer was requested: the test releases it then.
    let watcher = {
        let requests = provider.requests.clone();
        let go = go.clone();
        std::thread::spawn(move || {
            common::wait_until("the first turn answered", || requests.lock().unwrap().len() >= 2);
            std::fs::write(go, "").unwrap();
        })
    };
    let output = install.prompt(PROMPT, &[]);
    watcher.join().unwrap();
    assert_eq!(
        common::stdout_of(&output).trim(),
        "TESTS PASSED: RESULT 0 5 passed in 5.0s"
    );
    assert_eq!(provider.requests().len(), 4);
}

#[test]
fn acceptance_a56_each_root_request_extends_the_previous_one_byte_for_byte_events_included() {
    let provider = Provider::start(job_script);
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[]);
    client.prompt_and_wait(PROMPT, HANG).unwrap();
    std::fs::write(install.project().join("go"), "").unwrap();
    wait_events(&mut client, "agent_end", 2);
    settled(&install, &mut client);
    client.prompt_and_wait("SECOND: thanks", HANG).unwrap();
    assert_eq!(
        client.get_last_assistant_text().unwrap().as_deref(),
        Some("YOU ARE WELCOME")
    );
    let requests = provider.requests();
    // Two tool rounds, the event run's two requests, and the follow-up prompt.
    assert_eq!(requests.len(), 5);
    let bodies: Vec<&Value> = requests.iter().map(|r| &r.body).collect();
    for index in 1..bodies.len() {
        let (previous, current) = (bodies[index - 1], bodies[index]);
        assert_eq!(current["tools"].to_string(), previous["tools"].to_string());
        let (old, new) = (
            previous["messages"].as_array().unwrap(),
            current["messages"].as_array().unwrap(),
        );
        assert!(new.len() > old.len(), "request {index} does not extend the previous");
        let prefix: Vec<String> = new[..old.len()].iter().map(Value::to_string).collect();
        let before: Vec<String> = old.iter().map(Value::to_string).collect();
        assert_eq!(prefix, before, "request {index} rewrote earlier messages");
    }
    // The serialized request itself shares the prefix up to the end of the previous request's messages.
    let last_previous = bodies[3]["messages"].as_array().unwrap().last().unwrap().to_string();
    let (raw3, raw4) = (&requests[3].raw, &requests[4].raw);
    let at4 = raw4
        .find(&last_previous)
        .expect("the previous message is in the next request");
    let at3 = raw3.find(&last_previous).unwrap();
    assert!(at4 > 0);
    assert_eq!(&raw4[..at4], &raw3[..at3]);
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}

#[test]
fn acceptance_a56_a_completion_during_a_running_turn_is_delivered_at_the_next_boundary_without_a_second_run() {
    let provider = Provider::start(|request: &Request| {
        if let Some((_, status, summary, _)) = job_event(&last_user(request)) {
            return Reply::Text(format!("SAW {status} {summary}"));
        }
        match turn(request) {
            // The job ends only once the next cell runs, so it is detached (not reported inline).
            0 => Reply::Rlm("job = await bash('''while [ ! -e go ]; do sleep 0.05; done; echo quick-done''', yield_after=0)".into()),
            // Other work that lasts until the job's completion was steered into the run (the test sees the queue).
            1 => Reply::Rlm("import os, asyncio\nopen('go', 'w').close()\nwhile not os.path.exists('steered'):\n    await asyncio.sleep(0.05)\nprint('other work done')".into()),
            _ => Reply::Text("NO EVENT SEEN".into()),
        }
    });
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[]);
    client.mark();
    client
        .send(serde_json::json!({"type": "prompt", "message": "STEER: start a job, then do other work"}))
        .unwrap();
    client
        .wait_for(
            &|l| {
                l["type"] == "queue_update"
                    && l["steering"]
                        .as_array()
                        .is_some_and(|s| s.iter().any(|m| m.to_string().contains("job_done")))
            },
            HANG,
        )
        .unwrap();
    std::fs::write(install.project().join("steered"), "").unwrap();
    wait_events(&mut client, "agent_end", 1);
    assert_eq!(
        client.get_last_assistant_text().unwrap().as_deref(),
        Some("SAW completed exit 0; quick-done")
    );
    // The event followed the second cell's result in the same run.
    let requests = provider.requests();
    let last = requests.last().unwrap().body["messages"].as_array().unwrap().clone();
    assert_eq!(last[last.len() - 2]["role"], "tool", "{last:#?}");
    settled(&install, &mut client);
    assert_eq!(count(&client, "agent_start"), 1);
    assert_eq!(provider.requests().len(), 3);
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}

#[test]
fn acceptance_a56_a_spent_turn_budget_turns_the_reinvocation_into_quiet_delivery_with_the_next_prompt() {
    let provider = Provider::start(|request: &Request| {
        if job_event(&last_user(request)).is_some() {
            return Reply::Text("SHOULD NOT BE RE-INVOKED".into());
        }
        if last_user(request) == "NEXT: anything new?" {
            return Reply::Text(if request.raw.contains("exit 0; budget-done") {
                "EVENT WAITED FOR ME".into()
            } else {
                "NO EVENT".into()
            });
        }
        if turn(request) == 0 {
            return Reply::Rlm(
                "job = await bash('''while [ ! -e go ]; do sleep 0.05; done; echo budget-done''', yield_after=0)"
                    .into(),
            );
        }
        Reply::Text("STARTED".into())
    });
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[("ULTRON_MAX_TOTAL_TURNS", "2")]);
    client.prompt_and_wait("BUDGET: start a job", HANG).unwrap();
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("STARTED"));
    std::fs::write(install.project().join("go"), "").unwrap();
    // Two model turns spent the root's budget of two: the completion does not start a run; it waits in the
    // transcript as the runtime event message.
    common::wait_until("the event is kept for the next run", || {
        client
            .get_messages()
            .is_ok_and(|m| m.iter().any(|m| m["customType"] == "ultron-runtime-event"))
    });
    settled(&install, &mut client);
    assert_eq!(count(&client, "agent_start"), 1);
    assert_eq!(provider.requests().len(), 2);
    client.prompt_and_wait("NEXT: anything new?", HANG).unwrap();
    assert_eq!(
        client.get_last_assistant_text().unwrap().as_deref(),
        Some("EVENT WAITED FOR ME")
    );
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
        && !std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| {
            s.rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        })
}

#[test]
fn acceptance_a56_esc_on_the_turn_stops_its_job_and_the_completion_never_restarts_the_model() {
    let provider = Provider::start(|request: &Request| match turn(request) {
        // A sleep this test can find by its argument (Loki refuses a shell redirect into a file).
        0 => Reply::Rlm("job = await bash('''exec sleep 3017''', yield_after=0.5)".into()),
        1 => Reply::Rlm("import asyncio\nopen('waiting', 'w').write('1')\nawait asyncio.sleep(30)".into()),
        _ => Reply::Text("SHOULD NOT RUN".into()),
    });
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[]);
    client.mark();
    client
        .send(serde_json::json!({"type": "prompt", "message": "ABORT: start a long job then wait"}))
        .unwrap();
    let waiting = install.project().join("waiting");
    let deadline = std::time::Instant::now() + HANG;
    while !waiting.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the second cell never waited: {:#?}",
            provider.requests().iter().map(|r| r.body.clone()).collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let pid = std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()))
        .find(|pid| std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| c == b"sleep\x003017\x00"))
        .expect("the job's process");
    assert!(alive(pid));
    client.send(serde_json::json!({"type": "abort"})).unwrap();
    wait_events(&mut client, "agent_end", 1);
    common::wait_until("the job's process is gone", || !alive(pid));
    settled(&install, &mut client);
    assert_eq!(count(&client, "agent_start"), 1);
    assert_eq!(provider.requests().len(), 2);
    let (_, code) = client.finish(HANG);
    assert_eq!(code, Some(0));
}
