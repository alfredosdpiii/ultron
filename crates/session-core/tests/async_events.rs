//! Completion events (the dispatcher's delivery rules): every case of
//! packages/coding-agent/test/ultron-async-events.test.ts, against a fake lane that follows the harness's queue
//! semantics as the TypeScript one does: a steer is consumed by a running run or left in the inbox once the run has
//! ended, `cancel_queued` reports which, and an abort drops queued steers. `drain` delivers at once where the
//! TypeScript test waits out the coalescing window.

use serde_json::{Value, json};
use session_core::async_events::{
    Accepted, Cancelled, Delivery, Dispatcher, DispatcherOptions, EventLanes, Execution, RUNTIME_EVENT_MESSAGE_TYPE,
    RuntimeEvent, async_events_enabled, max_event_runs_from_env, runtime_event_text,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct LaneState {
    current: Option<String>,
    /// entry id -> (kind, message), in insertion order.
    inbox: Vec<(String, &'static str, Value)>,
    consumed: Vec<String>,
    runs: Vec<(String, Value)>,
    next: usize,
    /// The lane runs another operation (a compaction, a navigation): events wait and are retried.
    other: bool,
}

#[derive(Default)]
struct Fake {
    lanes: Mutex<HashMap<String, LaneState>>,
    aliases: Mutex<HashMap<String, String>>,
}

impl Fake {
    fn with<T>(&self, lane: &str, f: impl FnOnce(&mut LaneState) -> T) -> T {
        let mut lanes = self.lanes.lock().unwrap();
        f(lanes.entry(lane.to_string()).or_default())
    }

    /// Start a run of `lane` by hand (a user prompt); its operation id.
    fn start(&self, lane: &str, message: Value) -> String {
        self.with(lane, |state| {
            state.next += 1;
            let id = format!("run-{lane}-{}", state.next);
            state.current = Some(id.clone());
            state.runs.push((id.clone(), message));
            id
        })
    }

    /// The running run places every queued steer at its boundary.
    fn boundary(&self, lane: &str) {
        self.with(lane, |state| {
            let steers: Vec<String> = state
                .inbox
                .iter()
                .filter(|(_, kind, _)| *kind == "steer")
                .map(|(id, _, _)| id.clone())
                .collect();
            state.inbox.retain(|(_, kind, _)| *kind != "steer");
            state.consumed.extend(steers);
        });
    }

    /// End the current run; an abort drops queued steers, as the harness does. Emits `run_end`.
    fn end(&self, lane: &str, aborted: bool, dispatcher: &Dispatcher) {
        let run = self.with(lane, |state| {
            if aborted {
                state.inbox.retain(|(_, kind, _)| *kind != "steer");
            }
            state.current.take().unwrap_or_else(|| "none".into())
        });
        dispatcher.run_ended(lane, &run, aborted);
    }

    fn queued(&self, lane: &str, kind: &str) -> Vec<String> {
        self.with(lane, |state| {
            state
                .inbox
                .iter()
                .filter(|(_, k, _)| *k == kind)
                .map(|(_, _, m)| text(m))
                .collect()
        })
    }

    fn runs(&self, lane: &str) -> Vec<(String, String)> {
        self.with(lane, |state| {
            state.runs.iter().map(|(id, m)| (id.clone(), text(m))).collect()
        })
    }

    fn inbox_len(&self, lane: &str) -> usize {
        self.with(lane, |state| state.inbox.len())
    }
}

impl EventLanes for Fake {
    fn execution(&self, lane: &str) -> Execution {
        self.with(lane, |state| {
            if state.other {
                Execution::Other
            } else if state.current.is_some() {
                Execution::Run
            } else {
                Execution::Idle
            }
        })
    }

    fn accept(&self, lane: &str, message: &Value, root: &str) -> Accepted {
        if self.with(lane, |state| state.current.is_some()) {
            return Accepted::Busy;
        }
        let id = self.start(lane, message.clone());
        self.aliases.lock().unwrap().insert(id.clone(), root.to_string());
        Accepted::Run(id)
    }

    fn steer(&self, lane: &str, message: &Value) -> Result<String, String> {
        Ok(self.with(lane, |state| {
            state.next += 1;
            let entry = format!("e{}", state.next);
            state.inbox.push((entry.clone(), "steer", message.clone()));
            entry
        }))
    }

    fn next_run(&self, lane: &str, message: &Value) -> Result<String, String> {
        Ok(self.with(lane, |state| {
            state.next += 1;
            let entry = format!("e{}", state.next);
            state.inbox.push((entry.clone(), "nextRun", message.clone()));
            entry
        }))
    }

    fn cancel_queued(&self, lane: &str, entry: &str) -> Result<Cancelled, String> {
        Ok(self.with(lane, |state| {
            if let Some(index) = state.inbox.iter().position(|(id, _, _)| id == entry) {
                state.inbox.remove(index);
                Cancelled::Cancelled
            } else if state.consumed.iter().any(|id| id == entry) {
                Cancelled::AlreadyConsumed
            } else {
                Cancelled::NotFound
            }
        }))
    }
}

fn text(message: &Value) -> String {
    message["content"].as_str().unwrap_or_default().to_string()
}

struct Setup {
    fake: Arc<Fake>,
    dispatcher: Arc<Dispatcher>,
    aborted: Arc<Mutex<Vec<String>>>,
    deliveries: Arc<Mutex<Vec<String>>>,
    messages: Arc<Mutex<Vec<Value>>>,
}

fn setup(configure: impl FnOnce(&mut DispatcherOptions)) -> Setup {
    let fake = Arc::new(Fake::default());
    let aliases = fake.clone();
    let mut options = DispatcherOptions::new(Arc::new(move |run: &str| {
        aliases
            .aliases
            .lock()
            .unwrap()
            .get(run)
            .cloned()
            .unwrap_or_else(|| format!("turn:{run}"))
    }));
    options.coalesce = Duration::from_millis(30);
    let aborted = Arc::new(Mutex::new(Vec::new()));
    let seen = aborted.clone();
    options.on_root_aborted = Some(Arc::new(move |root: &str| seen.lock().unwrap().push(root.to_string())));
    let deliveries = Arc::new(Mutex::new(Vec::new()));
    let messages = Arc::new(Mutex::new(Vec::new()));
    let (log, kept) = (deliveries.clone(), messages.clone());
    options.on_delivery = Some(Arc::new(move |delivery: &Delivery| {
        log.lock().unwrap().push(format!(
            "{}:{}",
            delivery.mode,
            delivery.reason.clone().unwrap_or_default()
        ));
        kept.lock()
            .unwrap()
            .push(json!(delivery.events.iter().map(|e| e.id.clone()).collect::<Vec<_>>()));
    }));
    configure(&mut options);
    let dispatcher = Dispatcher::new(options, fake.clone());
    Setup {
        fake,
        dispatcher,
        aborted,
        deliveries,
        messages,
    }
}

fn event(id: &str) -> RuntimeEvent {
    RuntimeEvent {
        kind: "job_done".into(),
        id: id.into(),
        status: "completed".into(),
        summary: format!("exit 0; {id} passed"),
        fetch: format!("await rlm.job(\"{id}\")"),
        lane: "main".into(),
        root_id: Some("turn:user-1".into()),
    }
}

#[test]
fn session_core_async_events_an_idle_root_is_reinvoked_once_per_coalesced_batch_charged_to_its_root() {
    let s = setup(|_| {});
    s.dispatcher.publish(event("job-a"));
    s.dispatcher.publish(RuntimeEvent {
        kind: "child_done".into(),
        fetch: "await rlm.collect([\"t1\"])".into(),
        ..event("job-b")
    });
    s.dispatcher.publish(event("job-c"));
    s.dispatcher.drain();
    let runs = s.fake.runs("main");
    assert_eq!(runs.len(), 1);
    let message = s.fake.with("main", |state| state.runs[0].1.clone());
    assert_eq!(message["role"], "custom");
    assert_eq!(message["customType"], RUNTIME_EVENT_MESSAGE_TYPE);
    assert_eq!(message["display"], true);
    assert_eq!(runs[0].1.split('\n').count(), 3);
    assert!(
        runs[0]
            .1
            .contains("<runtime_event kind=\"job_done\" id=\"job-a\" status=\"completed\""),
        "{}",
        runs[0].1
    );
    assert_eq!(s.fake.aliases.lock().unwrap()[&runs[0].0], "turn:user-1");
    s.fake.end("main", false, &s.dispatcher);
    // A later completion is its own batch and its own run, still charged to the same root.
    s.dispatcher.publish(event("job-d"));
    s.dispatcher.drain();
    let runs = s.fake.runs("main");
    assert_eq!(runs.len(), 2);
    assert_eq!(s.fake.aliases.lock().unwrap()[&runs[1].0], "turn:user-1");
}

/// An event that finds the lane busy with another operation waits to be retried, and is counted pending all the
/// while: the session's activity check reads the count, and a worker with no client retires when it is 0, which
/// dropped the event before it was ever delivered.
#[test]
fn session_core_async_events_an_event_waiting_for_a_retry_still_counts_as_pending() {
    let s = setup(|options| options.retry_after = Duration::from_millis(50));
    s.fake.with("main", |state| state.other = true);
    s.dispatcher.publish(event("job-a"));
    s.dispatcher.drain();
    assert!(s.fake.runs("main").is_empty());
    assert_eq!(s.dispatcher.pending_for("main"), 1);
    s.fake.with("main", |state| state.other = false);
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        // Read the count first: 0 only once the delivery (counted in flight until it returns) has run.
        let pending = s.dispatcher.pending_for("main");
        if !s.fake.runs("main").is_empty() {
            break;
        }
        assert_eq!(pending, 1, "pending until delivered");
        assert!(
            std::time::Instant::now() < deadline,
            "the retried event was never delivered"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(s.fake.runs("main").len(), 1);
    assert_eq!(s.dispatcher.pending_for("main"), 0);
}

#[test]
fn session_core_async_events_coalesce_within_the_window_without_a_drain() {
    let s = setup(|options| options.coalesce = Duration::from_millis(200));
    s.dispatcher.publish(event("job-a"));
    s.dispatcher.publish(event("job-b"));
    assert_eq!(s.dispatcher.pending_for("main"), 2);
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    while s.fake.runs("main").is_empty() {
        assert!(std::time::Instant::now() < deadline, "the batch was never delivered");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(s.fake.runs("main").len(), 1);
    assert_eq!(s.fake.runs("main")[0].1.split('\n').count(), 2);
    assert_eq!(s.dispatcher.pending_for("main"), 0);
}

#[test]
fn session_core_async_events_a_mid_turn_event_is_steered_to_the_next_boundary_and_never_starts_a_second_run() {
    let s = setup(|_| {});
    s.fake.start("main", json!({"role": "user", "content": "work"}));
    s.dispatcher.publish(event("job-a"));
    s.dispatcher.drain();
    assert_eq!(s.fake.queued("main", "steer").len(), 1);
    assert_eq!(s.dispatcher.pending_for("main"), 1);
    s.fake.boundary("main");
    s.fake.end("main", false, &s.dispatcher);
    s.dispatcher.drain();
    assert_eq!(s.fake.runs("main").len(), 1);
    assert_eq!(s.fake.inbox_len("main"), 0);
    assert_eq!(s.dispatcher.pending_for("main"), 0);
}

#[test]
fn session_core_async_events_an_unconsumed_steer_is_taken_back_and_reinvokes_the_idle_root() {
    let s = setup(|_| {});
    s.fake.start("main", json!({"role": "user", "content": "work"}));
    s.dispatcher.publish(event("job-a"));
    s.dispatcher.drain();
    s.fake.end("main", false, &s.dispatcher);
    s.dispatcher.drain();
    assert_eq!(s.fake.inbox_len("main"), 0);
    let runs = s.fake.runs("main");
    assert_eq!(runs.len(), 2);
    assert!(runs[1].1.contains("id=\"job-a\""), "{}", runs[1].1);
}

#[test]
fn session_core_async_events_turn_budget_cost_cap_and_chain_cap_turn_reinvocation_into_quiet_delivery() {
    let refused = setup(|options| {
        options.refuse = Some(Arc::new(|_: &str| {
            Some("Usage turn limit reached for root turn:user-1".into())
        }))
    });
    refused.dispatcher.publish(event("job-a"));
    refused.dispatcher.drain();
    assert!(refused.fake.runs("main").is_empty());
    assert_eq!(refused.fake.queued("main", "nextRun").len(), 1);

    let capped = setup(|options| options.max_runs = 1);
    capped.dispatcher.publish(event("job-a"));
    capped.dispatcher.drain();
    capped.fake.end("main", false, &capped.dispatcher);
    capped.dispatcher.publish(event("job-b"));
    capped.dispatcher.drain();
    assert_eq!(capped.fake.runs("main").len(), 1);
    assert_eq!(capped.fake.queued("main", "nextRun").len(), 1);
    assert_eq!(
        *capped.deliveries.lock().unwrap(),
        ["run:", "nextRun:1 of 1 automatic runs used for turn:user-1"]
    );
}

#[test]
fn session_core_async_events_after_an_abort_the_root_never_restarts_and_later_completions_wait_quietly() {
    let s = setup(|_| {});
    let run = s.fake.start("main", json!({"role": "user", "content": "work"}));
    let root = format!("turn:{run}");
    // A child's completion is steered into the run, then the user presses Esc: the harness drops the steer.
    s.dispatcher.publish(RuntimeEvent {
        kind: "child_done".into(),
        root_id: Some(root.clone()),
        ..event("t1")
    });
    s.dispatcher.publish(RuntimeEvent {
        status: "cancelled".into(),
        root_id: Some(root.clone()),
        ..event("job-x")
    });
    s.dispatcher.drain();
    assert_eq!(s.fake.queued("main", "steer").len(), 1);
    s.fake.end("main", true, &s.dispatcher);
    s.dispatcher.drain();
    assert_eq!(s.aborted.lock().unwrap().as_slice(), std::slice::from_ref(&root));
    assert!(s.dispatcher.root_aborted(Some(&root)));
    // No new run; the child's result waits for the next user turn, the cancelled job is not announced.
    assert_eq!(s.fake.runs("main").len(), 1);
    let waiting = s.fake.queued("main", "nextRun");
    assert_eq!(waiting.len(), 1);
    assert!(waiting[0].contains("id=\"t1\""), "{}", waiting[0]);
    assert!(!waiting[0].contains("job-x"), "{}", waiting[0]);
    s.dispatcher.publish(RuntimeEvent {
        kind: "task_done".into(),
        root_id: Some(root),
        ..event("t2")
    });
    s.dispatcher.drain();
    assert_eq!(s.fake.runs("main").len(), 1);
    assert_eq!(s.fake.queued("main", "nextRun").len(), 2);
}

#[test]
fn session_core_async_events_childrens_events_go_to_their_own_lane_steered_when_running_never_reinvoked() {
    let s = setup(|_| {});
    let child = "ultron.rlm-child.t1";
    s.dispatcher.publish(RuntimeEvent {
        lane: child.into(),
        ..event("job-c1")
    });
    s.dispatcher.drain();
    assert!(s.fake.runs(child).is_empty());
    assert_eq!(s.fake.queued(child, "nextRun").len(), 1);
    s.fake.start(child, json!({"role": "user", "content": "child work"}));
    s.dispatcher.publish(RuntimeEvent {
        lane: child.into(),
        ..event("job-c2")
    });
    s.dispatcher.drain();
    assert_eq!(s.fake.queued(child, "steer").len(), 1);
    assert!(s.fake.runs("main").is_empty());
    assert_eq!(s.fake.inbox_len("main"), 0);
}

#[test]
fn session_core_async_events_off_disables_events() {
    let s = setup(|options| options.enabled = async_events_enabled(Some("off")));
    s.dispatcher.publish(event("job-a"));
    s.dispatcher.drain();
    assert!(s.fake.runs("main").is_empty());
    assert_eq!(s.fake.inbox_len("main"), 0);
    assert!(s.messages.lock().unwrap().is_empty());
    assert!(async_events_enabled(None));
    assert!(async_events_enabled(Some("on")));
    assert!(!async_events_enabled(Some(" No ")));
    assert_eq!(max_event_runs_from_env(Some("3")), 3);
    assert_eq!(max_event_runs_from_env(Some("x")), 16);
    assert_eq!(max_event_runs_from_env(None), 16);
}

#[test]
fn session_core_async_events_the_event_text_is_one_bounded_escaped_line_per_completion() {
    let line = runtime_event_text(&[RuntimeEvent {
        summary: format!("a \"quoted\" <b>\n{}", "x".repeat(1000)),
        ..event("job-a")
    }]);
    let head =
        "<runtime_event kind=\"job_done\" id=\"job-a\" status=\"completed\" summary=\"a &quot;quoted&quot; &lt;b&gt; ";
    let tail = "…\" fetch=\"await rlm.job(&quot;job-a&quot;)\" />";
    assert!(line.starts_with(head) && line.ends_with(tail), "{line}");
    let xs = &line[head.len()..line.len() - tail.len()];
    assert!(!xs.is_empty() && xs.chars().all(|c| c == 'x'), "{line}");
    assert!(line.chars().count() < 400);
}
