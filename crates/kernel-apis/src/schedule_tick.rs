//! The schedule tick: fires due schedules on its own, without anything else waking the session (the TS
//! `ScheduleModule` interval timer, `tickIntervalMs`).
//!
//! A [`ScheduleTicker`] thread sleeps until the earliest next fire time ([`SchedulesModule::next_fire`]) or until it
//! is woken (a committed schedule change, [`ScheduleTicker::wake`]), then runs [`SchedulesModule::tick`]. The next
//! fire time is derived from durable schedule fields, so a reopened ticker recomputes it: slots missed while no
//! worker ran are due at once and coalesce into one run of the latest slot (TS semantics). Each fire is admitted as a
//! task whose key is its slot (`schedule:<id>:<slot>`) after the slot is claimed durably, so a crash between firing
//! and delivery resumes the claimed slot with the same key and the slot runs exactly once.
//!
//! Time comes from a [`TickClock`]: [`SystemTickClock`] in the product, [`ManualTickClock`] in tests, which never
//! sleeps on real time (its waits end only when woken) so tests advance it and wake the ticker.

use crate::schedules::SchedulesModule;
use crate::tasks::TaskHost;
use crate::util::lock;
use kernel::Signal;
use serde_json::Value;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// After a tick that failed (a store write failed) or settled nothing, wait this long before trying again.
pub const RETRY_MS: i64 = 1000;

/// The ticker's time source.
pub trait TickClock: Send + Sync {
    /// Now, in ms on the schedules' clock.
    fn now(&self) -> i64;
    /// How long to block, at most, before `deadline` may have passed; `None` blocks until woken.
    fn wait_limit(&self, deadline: i64) -> Option<Duration>;
}

/// Wall-clock time; waits end at the deadline.
pub struct SystemTickClock;

impl TickClock for SystemTickClock {
    fn now(&self) -> i64 {
        (crate::util::system_clock())()
    }

    fn wait_limit(&self, deadline: i64) -> Option<Duration> {
        Some(Duration::from_millis(deadline.saturating_sub(self.now()).max(0) as u64))
    }
}

/// A clock that moves only when told to; a ticker on it waits until woken.
pub struct ManualTickClock {
    now: AtomicI64,
}

impl ManualTickClock {
    pub fn new(now: i64) -> Arc<ManualTickClock> {
        Arc::new(ManualTickClock {
            now: AtomicI64::new(now),
        })
    }

    pub fn set(&self, now: i64) {
        self.now.store(now, Ordering::SeqCst);
    }

    pub fn advance(&self, ms: i64) -> i64 {
        self.now.fetch_add(ms, Ordering::SeqCst) + ms
    }

    /// This clock as the schedules module's clock.
    pub fn as_clock(self: &Arc<Self>) -> crate::util::Clock {
        let clock = self.clone();
        Arc::new(move || clock.now())
    }
}

impl TickClock for ManualTickClock {
    fn now(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }

    fn wait_limit(&self, _deadline: i64) -> Option<Duration> {
        None
    }
}

/// Observes every tick's report (`{fired, failed}`) or error.
pub type TickObserver = Arc<dyn Fn(&Result<Value, String>) + Send + Sync>;

#[derive(Default)]
struct Flags {
    woken: bool,
    closed: bool,
}

struct Shared {
    flags: Mutex<Flags>,
    bell: Condvar,
}

impl Shared {
    fn wake(&self) {
        lock(&self.flags).woken = true;
        self.bell.notify_all();
    }
}

pub struct ScheduleTicker {
    shared: Arc<Shared>,
    module: Arc<SchedulesModule>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl ScheduleTicker {
    /// Starts the tick thread over `module` (already started on `host`), admitting firings through `host`.
    /// `observer` sees each tick's report.
    pub fn start(
        module: Arc<SchedulesModule>,
        host: Arc<dyn TaskHost>,
        clock: Arc<dyn TickClock>,
        observer: Option<TickObserver>,
    ) -> ScheduleTicker {
        let shared = Arc::new(Shared {
            flags: Mutex::default(),
            bell: Condvar::new(),
        });
        let waker = shared.clone();
        module.on_change(Some(Arc::new(move || waker.wake())));
        let (thread_shared, thread_module) = (shared.clone(), module.clone());
        let thread = std::thread::Builder::new()
            .name("ultron-schedule-tick".into())
            .spawn(move || run(&thread_shared, &thread_module, host.as_ref(), clock.as_ref(), observer))
            .ok();
        ScheduleTicker {
            shared,
            module,
            thread: Mutex::new(thread),
        }
    }

    /// Re-read the next fire time now (the clock moved, or something outside the module changed).
    pub fn wake(&self) {
        self.shared.wake();
    }

    /// Stops the ticker and waits for its thread; a tick in progress finishes first. Idempotent.
    pub fn close(&self) {
        lock(&self.shared.flags).closed = true;
        self.shared.bell.notify_all();
        self.module.on_change(None);
        let thread = lock(&self.thread).take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
    }
}

impl Drop for ScheduleTicker {
    fn drop(&mut self) {
        self.close();
    }
}

fn run(
    shared: &Shared,
    module: &SchedulesModule,
    host: &dyn TaskHost,
    clock: &dyn TickClock,
    observer: Option<TickObserver>,
) {
    loop {
        if lock(&shared.flags).closed {
            return;
        }
        let deadline = match module.next_fire() {
            Ok(Some(next)) if next <= clock.now() => {
                // Firings outlive whatever woke the ticker.
                let report = module.tick(host, &Signal::new());
                // A tick that settled nothing (or failed) must not be repeated at once: wait for a change or retry.
                let settled = report.as_ref().is_ok_and(|report| {
                    ["fired", "failed"]
                        .iter()
                        .any(|key| report[*key].as_array().is_some_and(|items| !items.is_empty()))
                });
                if let Some(observer) = &observer {
                    observer(&report);
                }
                if settled {
                    continue;
                }
                Some(clock.now().saturating_add(RETRY_MS))
            }
            Ok(next) => next,
            Err(_) => Some(clock.now().saturating_add(RETRY_MS)),
        };
        let mut flags = lock(&shared.flags);
        while !flags.woken && !flags.closed {
            let limit = deadline.map(|deadline| clock.wait_limit(deadline));
            match limit {
                // No schedule fires and none is due: sleep until something changes.
                None | Some(None) => {
                    flags = shared
                        .bell
                        .wait(flags)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                Some(Some(limit)) if limit.is_zero() => break,
                Some(Some(limit)) => {
                    flags = shared
                        .bell
                        .wait_timeout(flags, limit)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0;
                }
            }
        }
        flags.woken = false;
    }
}
