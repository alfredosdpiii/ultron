//! The root lane's completion events of a Claude Code root are coalesced as the TS dispatcher coalesces them for
//! its `rootSink` (async-events.ts `publish`/`#flush`, `DEFAULT_COALESCE_MS` = 500): a batch reaches the root's
//! inbox when the window its first event opened closes, then leads the next cell result or comes with the next user
//! prompt. On an injected clock: no sleeps, every step is a clock value.

use claude_root::external_root::{CellRunner, EventSource, ExternalRoot};
use claude_root::root_sink::{COALESCE_MS, RootSink};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    now: Arc<AtomicU64>,
    sink: Arc<RootSink>,
    root: ExternalRoot,
}

impl Fixture {
    fn new() -> Fixture {
        let now = Arc::new(AtomicU64::new(10_000));
        let clock = now.clone();
        let sink = Arc::new(RootSink::new(Arc::new(move || clock.load(Ordering::SeqCst))));
        let source = sink.clone();
        let events: EventSource = Arc::new(move || source.take());
        let runner: CellRunner =
            Arc::new(|code: &str, _| Ok(vec![json!({"type": "text", "text": format!("ran {code}")})]));
        let root = ExternalRoot::new(events, runner, None);
        Fixture { now, sink, root }
    }

    fn at(&self, ms: u64) {
        self.now.store(10_000 + ms, Ordering::SeqCst);
    }

    fn cell(&self, code: &str) -> String {
        let result = self.root.run_cell(code, &ai::AbortSignal::new());
        result.content[0]["text"].as_str().unwrap().to_string()
    }
}

#[test]
fn claude_root_sink_window_is_the_dispatchers_500_ms() {
    assert_eq!(COALESCE_MS, 500);
}

#[test]
fn claude_root_sink_delivers_a_batch_when_its_first_events_window_closes() {
    let f = Fixture::new();
    f.at(0);
    f.sink.publish("e1".into());
    f.at(400);
    f.sink.publish("e2".into());
    // Still in the window: pending (`async.pending` events), not in the inbox, not in the next result.
    f.at(499);
    assert_eq!(f.sink.pending(), 2);
    assert_eq!(f.root.pending_events(), 0);
    assert_eq!(f.cell("a"), "ran a");
    // The window closes 500 ms after the first event: both lead the next result, as one batch.
    f.at(500);
    assert_eq!(f.sink.pending(), 0);
    assert_eq!(f.root.pending_events(), 2);
    assert_eq!(f.cell("b"), "e1\ne2\nran b");
    assert_eq!(f.cell("c"), "ran c");
}

#[test]
fn claude_root_sink_an_event_after_the_window_opens_the_next_batch() {
    let f = Fixture::new();
    f.at(0);
    f.sink.publish("e1".into());
    // Published after the first window closed (nobody read the sink in between): e1 was delivered at 500, e2 waits
    // for its own window, until 1100.
    f.at(600);
    f.sink.publish("e2".into());
    assert_eq!(f.sink.pending(), 1);
    assert_eq!(
        f.root.begin_turn(),
        Some("Runtime events since your last rlm call:\ne1".into())
    );
    f.at(1099);
    assert_eq!(f.cell("a"), "ran a");
    f.at(1100);
    assert_eq!(
        f.root.begin_turn(),
        Some("Runtime events since your last rlm call:\ne2".into())
    );
}
