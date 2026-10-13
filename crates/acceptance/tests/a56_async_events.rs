//! A56 (partial): completion is delivered without polling.
//! Ported from packages/coding-agent/test/ultron-async-events.test.ts (the event text and the off switch) and from
//! packages/coding-agent/test/ultron-async-cli.test.ts (a subagent's completion re-invokes the idle root), end to
//! end through the `ultron` binary in RPC mode against a scripted provider.
//!
//! The `child_done` summary leads with the task definition and the verdict tag, as session-worker.ts `taskEvent`
//! writes it (`rlm-child@1: [unverified] child found 3 files`, the TS test's literal). Not ported: every job-based case (no `job_done` events: `host::Host` reports no job ends),
//! the dispatcher's turn budget, cost cap and chain cap (`ULTRON_MAX_TOTAL_TURNS`, max event runs), steering a
//! running turn and reclaiming an unconsumed steer, abort semantics for jobs, child-lane event routing, the event as
//! an `ultron-runtime-event` custom message (Rust appends a user message), and the prompt-cache prefix and collapse
//! cases.

mod common;

use common::{Install, Provider, Reply, Request};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

#[test]
fn acceptance_a56_event_text_is_one_bounded_escaped_line() {
    let summary = format!("a \"quoted\" <b>\n{}", "x".repeat(1000));
    let line = session_core::compose::runtime_event_line(
        "job_done",
        "job-a",
        "completed",
        &summary,
        "await rlm.job(\"job-a\")",
    );
    let head =
        "<runtime_event kind=\"job_done\" id=\"job-a\" status=\"completed\" summary=\"a &quot;quoted&quot; &lt;b&gt; ";
    let tail = "…\" fetch=\"await rlm.job(&quot;job-a&quot;)\" />";
    assert!(line.starts_with(head) && line.ends_with(tail), "{line}");
    let xs = &line[head.len()..line.len() - tail.len()];
    assert!(!xs.is_empty() && xs.chars().all(|c| c == 'x'), "{line}");
    assert!(!line.contains('\n'));
    assert!(line.chars().count() < 400);
}

/// User messages without the `[clock]` context line the host adds before each prompt.
fn prompts(request: &Request) -> Vec<String> {
    request
        .users()
        .into_iter()
        .filter(|u| !u.starts_with("[clock] "))
        .collect()
}

/// Orders the scripted provider: the child's reply is held until the root's own turn has ended (its DELEGATED
/// reply served), so the child always ends while the root is idle, however slow the machine.
#[derive(Default)]
struct Order {
    delegated: Mutex<bool>,
    changed: Condvar,
    child_replied: AtomicUsize,
}

impl Order {
    fn delegate(&self) {
        *self.delegated.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait_delegated(&self) {
        let (delegated, _) = self
            .changed
            .wait_timeout_while(self.delegated.lock().unwrap(), common::HANG, |d| !*d)
            .unwrap();
        assert!(*delegated, "the root never ended its turn");
    }
}

fn child_script(order: &Arc<Order>) -> impl Fn(&Request) -> Reply + Send + Sync + 'static {
    let order = order.clone();
    let served = AtomicBool::new(false);
    move |request| {
        let reply = script(&order, request);
        if matches!(&reply, Reply::Text(t) if t == "DELEGATED") && !served.swap(true, Ordering::SeqCst) {
            order.delegate();
        }
        reply
    }
}

fn script(order: &Order, request: &Request) -> Reply {
    let first = prompts(request).first().cloned().unwrap_or_default();
    if first.starts_with("CHILD:") {
        order.wait_delegated();
        order.child_replied.fetch_add(1, Ordering::SeqCst);
        return Reply::Text("child found 3 files".into());
    }
    let last = request.users().last().cloned().unwrap_or_default();
    if let Some(at) = last.find("<runtime_event kind=\"child_done\"") {
        let summary = last[at..]
            .split("summary=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .unwrap_or("");
        return Reply::Text(format!("ROOT GOT {summary}"));
    }
    if request.tool_results().is_empty() {
        return Reply::Rlm("h = await rlm.spawn(\"CHILD: count the files\", name=\"counter\")".into());
    }
    Reply::Text("DELEGATED".into())
}

fn agent_events(events: &[serde_json::Value], kind: &str) -> usize {
    events.iter().filter(|e| e["type"] == kind).count()
}

#[test]
fn acceptance_a56_child_done_event_reinvokes_the_idle_root_ported_part() {
    let order = Arc::new(Order::default());
    let provider = Provider::start(child_script(&order));
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[]);
    let events = client
        .prompt_and_wait("PARENT: delegate the count", common::HANG)
        .unwrap();
    // The child's end continued the run: the root answered the event after its own turn ended with DELEGATED.
    assert_eq!(
        client.get_last_assistant_text().unwrap().as_deref(),
        Some("ROOT GOT rlm-child@1: [unverified] child found 3 files")
    );
    let roots: Vec<Request> = provider
        .requests()
        .into_iter()
        .filter(|r| prompts(r).first().is_some_and(|u| u.starts_with("PARENT")))
        .collect();
    assert_eq!(roots.len(), 3);
    assert_eq!(roots[1].tool_results().len(), 1);
    let event = roots[2].users().last().cloned().unwrap();
    let id = event
        .split("id=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap()
        .to_string();
    assert!(event.contains("kind=\"child_done\""), "{event}");
    assert!(event.contains("status=\"succeeded\""), "{event}");
    assert!(
        event.contains(&format!("fetch=\"await rlm.collect([&quot;{id}&quot;])\"")),
        "{event}"
    );
    // Nothing loops: one settled run, no further requests (a short settle: a slow machine can only miss a loop).
    assert_eq!(agent_events(&events, "agent_settled"), 1);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(provider.requests().len(), 4);
    let (_, code) = client.finish(common::HANG);
    assert_eq!(code, Some(0));
}

#[test]
fn acceptance_a56_async_events_off_disables_events_ported_part() {
    let order = Arc::new(Order::default());
    let provider = Provider::start(child_script(&order));
    let install = Install::new(&provider);
    let mut client = install.rpc(&["--no-session"], &[("ULTRON_ASYNC_EVENTS", "off")]);
    client
        .prompt_and_wait("PARENT: delegate the count", common::HANG)
        .unwrap();
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("DELEGATED"));
    // The child still runs and ends, but its completion re-invokes nothing: wait for its reply to be served, then
    // a short settle (a slow machine can only miss a wrongful re-invocation, never fail the test).
    common::wait_until("the child's reply", || order.child_replied.load(Ordering::SeqCst) == 1);
    std::thread::sleep(Duration::from_millis(3000));
    let requests = provider.requests();
    let child = |r: &&Request| prompts(r).first().is_some_and(|u| u.starts_with("CHILD:"));
    assert_eq!(requests.iter().filter(child).count(), 1);
    assert!(
        !requests
            .iter()
            .any(|r| r.users().iter().any(|u| u.contains("<runtime_event kind=")))
    );
    assert_eq!(client.get_last_assistant_text().unwrap().as_deref(), Some("DELEGATED"));
    let (_, code) = client.finish(common::HANG);
    assert_eq!(code, Some(0));
}
