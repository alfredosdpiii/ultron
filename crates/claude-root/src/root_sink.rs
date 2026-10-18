//! The root lane's completion events of a root driven from outside, as the async dispatcher hands them to its
//! `rootSink` (async-events.ts `publish` and `#flush` with session-worker.ts's `rootSink: externalRoot.sink`):
//! the first event of a batch opens a 500 ms window, the events published within it join the batch, and the whole
//! batch reaches the root's inbox when the window closes; an event after that opens the next batch. Until then it is
//! pending (`pendingFor("main")`, the `events` of `async.pending`) and not yet in the next cell result or prompt.
//!
//! The windows are kept against an injected clock and closed whenever the sink is read or written, so delivery
//! happens at the same moments TS's timer delivers, as seen by everyone who reads the inbox.

use std::sync::{Arc, Mutex, PoisonError};

/// `DEFAULT_COALESCE_MS`.
pub const COALESCE_MS: u64 = 500;

/// Milliseconds on some fixed scale (the system clock in the product, a test's own in tests).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

#[derive(Default)]
struct State {
    batch: Vec<String>,
    /// When the open batch is delivered.
    due: Option<u64>,
    delivered: Vec<String>,
}

pub struct RootSink {
    clock: Clock,
    coalesce_ms: u64,
    state: Mutex<State>,
}

impl RootSink {
    /// A sink on `clock` with the dispatcher's 500 ms window.
    pub fn new(clock: Clock) -> RootSink {
        RootSink {
            clock,
            coalesce_ms: COALESCE_MS,
            state: Mutex::default(),
        }
    }

    /// A sink on the system clock.
    pub fn system() -> RootSink {
        RootSink::new(Arc::new(|| session_core::lane::now_ms().max(0) as u64))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let now = (self.clock)();
        if state.due.is_some_and(|due| due <= now) {
            let batch = std::mem::take(&mut state.batch);
            state.delivered.extend(batch);
            state.due = None;
        }
        state
    }

    /// `publish`: one event's `<runtime_event>` line.
    pub fn publish(&self, line: String) {
        let now = (self.clock)();
        let mut state = self.state();
        if state.due.is_none() {
            state.due = Some(now + self.coalesce_ms);
        }
        state.batch.push(line);
    }

    /// The lines delivered so far (and clears them): what the sink handed the root's inbox.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.state().delivered)
    }

    /// Events published but not yet delivered (their window is still open).
    pub fn pending(&self) -> usize {
        self.state().batch.len()
    }
}
