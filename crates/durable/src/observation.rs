//! Document watches: serialized, exact-frame observation of one committed document incarnation.
//!
//! Port of `packages/durable/src/session/observation.ts` (`CommittedWatch`). Upstream delivers frames from the
//! microtask queue; here each started watch delivers on its own thread, so a listener may block or commit to the
//! Session without holding up the committer. A commit never waits for a listener.

use crate::DocumentId;
use crate::session::SessionError;
use chord_delta::Op;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// Maximum exact committed frames retained behind one unavailable listener; one more collapses them into a root
/// replacement.
const MAX_PENDING_WATCH_FRAMES: usize = 100;

/// A document value as observed: `None` once the incarnation is retired.
pub type Observed = Option<Arc<Value>>;

/// Terminal result of one watch.
#[derive(Clone, Debug, PartialEq)]
pub enum WatchEnd {
    Stopped,
    Cancelled,
    SessionClosed,
    Retired,
    /// The listener returned this error.
    ListenerError(String),
    /// The Session failed; this is what failed it.
    SessionFailed(SessionError),
}

impl WatchEnd {
    /// The upstream `reason` string.
    pub fn reason(&self) -> &'static str {
        match self {
            WatchEnd::Stopped => "stopped",
            WatchEnd::Cancelled => "cancelled",
            WatchEnd::SessionClosed => "session_closed",
            WatchEnd::Retired => "retired",
            WatchEnd::ListenerError(_) => "listener_error",
            WatchEnd::SessionFailed(_) => "session_failed",
        }
    }
}

type Listener = Box<dyn FnMut(&Observed, &[Op]) -> Result<(), String> + Send>;

struct Frame {
    value: Observed,
    ops: Vec<Op>,
}

struct State {
    value: Observed,
    pending: VecDeque<Frame>,
    started: bool,
    retired: bool,
    end: Option<WatchEnd>,
}

pub(crate) struct WatchShared {
    pub(crate) record: DocumentId,
    state: Mutex<State>,
    changed: Condvar,
}

fn lock(shared: &WatchShared) -> MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl WatchShared {
    pub(crate) fn new(record: DocumentId, value: Arc<Value>) -> Arc<WatchShared> {
        Arc::new(WatchShared {
            record,
            state: Mutex::new(State {
                value: Some(value),
                pending: VecDeque::new(),
                started: false,
                retired: false,
                end: None,
            }),
            changed: Condvar::new(),
        })
    }

    pub(crate) fn is_ended(&self) -> bool {
        lock(self).end.is_some()
    }

    /// Queue one committed frame; past the bound, every pending frame collapses into one root replacement.
    pub(crate) fn advance(&self, value: Observed, ops: Vec<Op>) {
        let mut state = lock(self);
        if state.end.is_some() || state.retired {
            return;
        }
        if value.is_none() {
            state.retired = true;
        }
        if state.pending.len() >= MAX_PENDING_WATCH_FRAMES {
            state.pending.clear();
            let root = value.as_deref().cloned().unwrap_or(Value::Null);
            state.pending.push_back(Frame {
                value,
                ops: vec![Op::Replace(root)],
            });
        } else {
            state.pending.push_back(Frame { value, ops });
        }
        self.changed.notify_all();
    }

    /// End once: drop pending frames and settle. A callback already running keeps running.
    pub(crate) fn terminate(&self, end: WatchEnd) {
        let mut state = lock(self);
        if state.end.is_none() {
            state.end = Some(end);
            state.pending.clear();
            self.changed.notify_all();
        }
    }

    fn deliver(self: Arc<Self>, mut listener: Listener) {
        loop {
            let frame = {
                let mut state = lock(&self);
                loop {
                    if state.end.is_some() {
                        return;
                    }
                    if let Some(frame) = state.pending.pop_front() {
                        state.value = frame.value.clone();
                        break frame;
                    }
                    state = self
                        .changed
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
            };
            if let Err(error) = listener(&frame.value, &frame.ops) {
                self.terminate(WatchEnd::ListenerError(error));
                return;
            }
            if frame.value.is_none() {
                self.terminate(WatchEnd::Retired);
                return;
            }
        }
    }
}

/// Handle of one document watch: the acquisition revision until frames are delivered, then the latest delivered one.
pub struct Watch {
    pub(crate) shared: Arc<WatchShared>,
}

impl Watch {
    /// Acquisition revision before start; the latest delivered revision afterward; `None` after retirement.
    pub fn value(&self) -> Observed {
        lock(&self.shared).value.clone()
    }

    /// Install the sole listener; frames are delivered in commit order on the watch's own thread, one at a time.
    /// A listener error ends the watch with [`WatchEnd::ListenerError`].
    pub fn start(
        &self,
        listener: impl FnMut(&Observed, &[Op]) -> Result<(), String> + Send + 'static,
    ) -> Result<(), SessionError> {
        {
            let mut state = lock(&self.shared);
            if state.started {
                return Err(SessionError::Error("Watch is already started".into()));
            }
            if state.end.is_some() {
                return Err(SessionError::Error("Watch is stopped".into()));
            }
            state.started = true;
        }
        let shared = self.shared.clone();
        std::thread::spawn(move || shared.deliver(Box::new(listener)));
        Ok(())
    }

    /// Idempotently stop future callbacks and return this watch's terminal result.
    pub fn stop(&self) -> WatchEnd {
        self.shared.terminate(WatchEnd::Stopped);
        self.end().expect("terminated")
    }

    /// Cancel future delivery; a callback already running is not interrupted.
    pub fn cancel(&self) {
        self.shared.terminate(WatchEnd::Cancelled);
    }

    /// The terminal result, once the watch has ended.
    pub fn end(&self) -> Option<WatchEnd> {
        lock(&self.shared).end.clone()
    }

    /// Wait up to `timeout` for the watch to end.
    pub fn closed(&self, timeout: Duration) -> Option<WatchEnd> {
        let state = lock(&self.shared);
        let (state, _) = self
            .shared
            .changed
            .wait_timeout_while(state, timeout, |state| state.end.is_none())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.end.clone()
    }
}
