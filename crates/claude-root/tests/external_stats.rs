//! An external root's turns in the session counters, ported from packages/coding-agent/test/ultron-session-stats.test.ts
//! ("an external root's turns: how many, and how long they were open", and the ExternalRootController wiring of
//! `beginRootTurn`/`endRootTurn` to `externalTurnStarted`/`externalTurnEnded`), with literal expected values; plus
//! the resumed-session rule that the stored document takes the recorder's root ("the session is now driven by
//! Claude Code").

use claude_root::external_root::{CellRunner, EventSource, ExternalRoot};
use serde_json::{Value, json};
use session_core::session_stats::{SessionStats, StatsStore};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Memory(Mutex<Option<Value>>);

impl StatsStore for Memory {
    fn read(&self) -> Result<Option<Value>, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn write(&self, document: &Value) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(document.clone());
        Ok(())
    }
}

fn recorder(root: &str, now: &Arc<AtomicU64>) -> SessionStats {
    let clock = now.clone();
    SessionStats::new(
        root,
        Arc::new(move || clock.load(Ordering::SeqCst)),
        Duration::from_secs(1_000_000),
    )
}

fn controller(stats: Arc<SessionStats>) -> ExternalRoot {
    let events: EventSource = Arc::new(Vec::new);
    let runner: CellRunner = Arc::new(|code: &str, _| Ok(vec![json!({"type": "text", "text": format!("ran {code}")})]));
    ExternalRoot::new(events, runner, None).with_stats(stats)
}

#[test]
fn claude_root_external_turns_are_counted_with_their_open_time() {
    let now = Arc::new(AtomicU64::new(0));
    let stats = Arc::new(recorder("external", &now));
    stats.attach(Arc::new(Memory(Mutex::new(None))));
    let root = controller(stats.clone());
    // UserPromptSubmit opens t1 at 0.
    root.begin_turn();
    now.store(4000, Ordering::SeqCst);
    // Stop closes it at 4000; a second Stop does nothing.
    root.end_turn();
    root.end_turn();
    // A cell with no turn open opens t2 at 4000; the next UserPromptSubmit ends it at 4500 and opens t3.
    root.run_cell("a", &ai::AbortSignal::new());
    now.store(4500, Ordering::SeqCst);
    root.begin_turn();
    now.store(5000, Ordering::SeqCst);
    // A turn still open when the session closes is closed with it.
    stats.close();
    assert_eq!(stats.snapshot()["externalTurns"], json!({"count": 3, "wallMs": 5000}));
}

#[test]
fn claude_root_without_counters_runs_cells_as_before() {
    let events: EventSource = Arc::new(Vec::new);
    let runner: CellRunner = Arc::new(|_: &str, _| Ok(vec![json!({"type": "text", "text": "ok"})]));
    let root = ExternalRoot::new(events, runner, None);
    root.begin_turn();
    assert_eq!(
        root.run_cell("x", &ai::AbortSignal::new()).content,
        vec![json!({"type": "text", "text": "ok"})]
    );
    root.end_turn();
    assert_eq!(root.turn(), None);
}

#[test]
fn claude_root_resumed_session_is_now_driven_by_claude_code() {
    let now = Arc::new(AtomicU64::new(1000));
    let first = recorder("lane", &now);
    let store = Arc::new(Memory(Mutex::new(None)));
    first.attach(store.clone());
    first.cell("main", "x = 1", false, 0);
    first.usage_limit_block();
    first.close();
    now.store(9000, Ordering::SeqCst);
    // The MCP server's stack starts as a lane root; it is made external before the stored counters are read.
    let second = recorder("lane", &now);
    second.set_root("external");
    second.attach(store.clone());
    second.cell("main", "y = 2", true, 0);
    let snapshot = second.snapshot();
    assert_eq!(snapshot["since"], json!(1000));
    assert_eq!(snapshot["updatedAt"], json!(9000));
    assert_eq!(snapshot["root"], json!("external"));
    assert_eq!(snapshot["cells"]["root"]["count"], json!(2));
    assert_eq!(snapshot["cells"]["root"]["failed"], json!(1));
    assert_eq!(snapshot["usageLimitBlocks"], json!(1));
    second.close();
    assert_eq!(store.0.lock().unwrap().clone(), Some(snapshot));
}
