//! Throttled commits of in-flight progress (a generation partial, a cell's running output): at most one commit per
//! interval, with one in flight; stopping waits for that commit and drops what is still pending, so no stale progress
//! lands after the outcome.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::scheduler::cancel::lock;

struct State<T> {
    pending: Option<T>,
    stopped: bool,
}

type Shared<T> = Arc<(Mutex<State<T>>, Condvar)>;

pub(crate) struct Throttle<T: Send + 'static> {
    shared: Shared<T>,
    worker: Option<JoinHandle<()>>,
}

impl<T: Send + 'static> Throttle<T> {
    /// Commit the newest marked value at most every `interval`; `commit` runs on the throttle's own thread.
    pub(crate) fn new(interval: Duration, mut commit: impl FnMut(T) + Send + 'static) -> Throttle<T> {
        let shared: Shared<T> = Arc::new((
            Mutex::new(State {
                pending: None,
                stopped: false,
            }),
            Condvar::new(),
        ));
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("durable-throttle".into())
            .spawn(move || {
                let (state, changed) = &*worker_shared;
                loop {
                    let mut guard = lock(state);
                    while guard.pending.is_none() && !guard.stopped {
                        guard = changed.wait(guard).unwrap_or_else(|poisoned| poisoned.into_inner());
                    }
                    if guard.stopped {
                        return;
                    }
                    // The first mark of a window waits out the interval; later marks replace the value.
                    let deadline = Instant::now() + interval;
                    loop {
                        let now = Instant::now();
                        if guard.stopped || now >= deadline {
                            break;
                        }
                        guard = changed
                            .wait_timeout(guard, deadline - now)
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .0;
                    }
                    if guard.stopped {
                        return;
                    }
                    let Some(value) = guard.pending.take() else { continue };
                    drop(guard);
                    commit(value);
                }
            })
            .ok();
        Throttle { shared, worker }
    }

    /// Replace the pending value; a commit follows within the interval.
    pub(crate) fn mark(&self, value: T) {
        let (state, changed) = &*self.shared;
        lock(state).pending = Some(value);
        changed.notify_all();
    }

    /// Stop: wait for a commit in flight and drop what is pending.
    pub(crate) fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        let (state, changed) = &*self.shared;
        {
            let mut guard = lock(state);
            guard.stopped = true;
            guard.pending = None;
        }
        changed.notify_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl<T: Send + 'static> Drop for Throttle<T> {
    fn drop(&mut self) {
        self.halt();
    }
}
