//! A one-shot latch for tests that must hold a fake (a slow clipboard, a blocked sink) until the test releases it,
//! instead of sleeping and hoping the caller wins the race. The wait is bounded only by a hang guard.

#![allow(dead_code)]

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Deadline for anything a test waits on: a hang guard, never a timing assumption.
pub const HANG_GUARD: Duration = Duration::from_secs(120);

#[derive(Clone, Default)]
pub struct Latch(Arc<(Mutex<bool>, Condvar)>);

impl Latch {
    pub fn open(&self) {
        *self.0.0.lock().unwrap() = true;
        self.0.1.notify_all();
    }

    /// Block until opened; panics once the hang guard expires.
    pub fn wait(&self) {
        let (open, changed) = &*self.0;
        let open = open.lock().unwrap();
        let (open, _) = changed.wait_timeout_while(open, HANG_GUARD, |open| !*open).unwrap();
        assert!(*open, "latch never opened");
    }
}
