//! Cancellation and one-shot settlement for the thread-based task runtime: [`Cancel`] (upstream `AbortSignal` /
//! `AbortController`), [`Ctx`] (upstream `Context`, reduced to its abort signal), [`Slot`] (a promise that settles
//! once) and [`Waiters`] (upstream `harness/util.ts` `Waiters`).

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use super::HarnessError;

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

type Callback = Box<dyn FnOnce(&HarnessError) + Send>;

#[derive(Default)]
struct CancelState {
    reason: Option<HarnessError>,
    next: u64,
    callbacks: Vec<(u64, Callback)>,
}

#[derive(Default)]
struct CancelInner {
    state: Mutex<CancelState>,
    changed: Condvar,
}

/// A cancellation signal and its controller in one: the first [`Cancel::cancel`] wins, wakes every waiter, and runs
/// every registered callback once. Clones share the signal.
#[derive(Clone, Default)]
pub struct Cancel(Arc<CancelInner>);

impl std::fmt::Debug for Cancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cancel").field("reason", &self.reason()).finish()
    }
}

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }

    /// Cancel with `reason`; later calls do nothing. Callbacks run on this thread after the state is released.
    pub fn cancel(&self, reason: HarnessError) {
        let callbacks = {
            let mut state = lock(&self.0.state);
            if state.reason.is_some() {
                return;
            }
            state.reason = Some(reason.clone());
            std::mem::take(&mut state.callbacks)
        };
        self.0.changed.notify_all();
        for (_, callback) in callbacks {
            callback(&reason);
        }
    }

    /// `AbortController.abort()` without a reason: an `AbortError`.
    pub fn abort(&self) {
        self.cancel(HarnessError::Aborted);
    }

    pub fn reason(&self) -> Option<HarnessError> {
        lock(&self.0.state).reason.clone()
    }

    pub fn is_cancelled(&self) -> bool {
        lock(&self.0.state).reason.is_some()
    }

    /// `throwIfAborted()`.
    pub fn check(&self) -> Result<(), HarnessError> {
        match self.reason() {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// Block until cancelled; returns the reason.
    pub fn wait(&self) -> HarnessError {
        let state = lock(&self.0.state);
        let state = self
            .0
            .changed
            .wait_while(state, |state| state.reason.is_none())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.reason.clone().expect("cancelled")
    }

    /// Wait up to `timeout`; the reason once cancelled.
    pub fn wait_timeout(&self, timeout: Duration) -> Option<HarnessError> {
        let state = lock(&self.0.state);
        let (state, _) = self
            .0
            .changed
            .wait_timeout_while(state, timeout, |state| state.reason.is_none())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.reason.clone()
    }

    /// Register `callback` for the cancellation; `None` (and the callback dropped) when already cancelled.
    pub(crate) fn on_cancel(&self, callback: impl FnOnce(&HarnessError) + Send + 'static) -> Option<u64> {
        let mut state = lock(&self.0.state);
        if state.reason.is_some() {
            return None;
        }
        state.next += 1;
        let id = state.next;
        state.callbacks.push((id, Box::new(callback)));
        Some(id)
    }

    pub(crate) fn off(&self, id: u64) {
        lock(&self.0.state).callbacks.retain(|(each, _)| *each != id);
    }

    /// `AbortSignal.any(signals)`: cancelled with the first reason of any of them. The links drop with the result.
    pub fn any(signals: &[&Cancel]) -> Linked {
        let cancel = Cancel::new();
        let mut links = Vec::new();
        for signal in signals {
            if let Some(reason) = signal.reason() {
                cancel.cancel(reason);
                break;
            }
            let target = cancel.clone();
            if let Some(id) = signal.on_cancel(move |reason| target.cancel(reason.clone())) {
                links.push(((*signal).clone(), id));
            } else if let Some(reason) = signal.reason() {
                cancel.cancel(reason);
                break;
            }
        }
        Linked { cancel, links }
    }
}

/// A [`Cancel`] linked to other signals; dropping it unregisters the links.
pub struct Linked {
    cancel: Cancel,
    links: Vec<(Cancel, u64)>,
}

impl Linked {
    pub fn signal(&self) -> &Cancel {
        &self.cancel
    }
}

impl Drop for Linked {
    fn drop(&mut self) {
        for (signal, id) in &self.links {
            signal.off(*id);
        }
    }
}

/// What an operation runs under: an optional abort signal (upstream `Context`; no values are carried).
#[derive(Clone, Debug, Default)]
pub struct Ctx {
    signal: Option<Cancel>,
}

impl Ctx {
    /// No cancellation (upstream `BACKGROUND_CONTEXT`).
    pub fn background() -> Ctx {
        Ctx::default()
    }

    pub fn with_signal(signal: &Cancel) -> Ctx {
        Ctx {
            signal: Some(signal.clone()),
        }
    }

    pub fn signal(&self) -> Option<&Cancel> {
        self.signal.as_ref()
    }

    pub fn check(&self) -> Result<(), HarnessError> {
        match &self.signal {
            Some(signal) => signal.check(),
            None => Ok(()),
        }
    }
}

/// A value that settles once; waiting blocks until it has.
type SlotState<T> = (Mutex<Option<Result<T, HarnessError>>>, Condvar);

pub(crate) struct Slot<T>(Arc<SlotState<T>>);

impl<T> Clone for Slot<T> {
    fn clone(&self) -> Self {
        Slot(self.0.clone())
    }
}

impl<T: Clone + Send + 'static> Slot<T> {
    pub(crate) fn new() -> Slot<T> {
        Slot(Arc::new((Mutex::new(None), Condvar::new())))
    }

    pub(crate) fn settled(value: Result<T, HarnessError>) -> Slot<T> {
        Slot(Arc::new((Mutex::new(Some(value)), Condvar::new())))
    }

    /// First settlement wins.
    pub(crate) fn settle(&self, value: Result<T, HarnessError>) {
        let mut slot = lock(&self.0.0);
        if slot.is_none() {
            *slot = Some(value);
            self.0.1.notify_all();
        }
    }

    /// Wait for the settlement; a cancellation of `ctx` settles it with the cancellation's reason first.
    pub(crate) fn wait(&self, ctx: &Ctx) -> Result<T, HarnessError> {
        let registration = ctx.signal().and_then(|signal| {
            let slot = self.clone();
            match signal.on_cancel(move |reason| slot.settle(Err(reason.clone()))) {
                Some(id) => Some((signal.clone(), id)),
                None => {
                    self.settle(Err(signal.reason().expect("cancelled")));
                    None
                }
            }
        });
        let result = {
            let slot = lock(&self.0.0);
            let slot = self
                .0
                .1
                .wait_while(slot, |slot| slot.is_none())
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            slot.clone().expect("settled")
        };
        if let Some((signal, id)) = registration {
            signal.off(id);
        }
        result
    }
}

/// Pending waits by key. Each settles once: through `resolve`, `reject_all`, or cancellation of its context.
pub(crate) struct Waiters<K, T>(Mutex<HashMap<K, Vec<Slot<T>>>>);

impl<K: Eq + Hash + Clone, T: Clone + Send + 'static> Waiters<K, T> {
    pub(crate) fn new() -> Self {
        Waiters(Mutex::new(HashMap::new()))
    }

    /// A pending wait; rejected at once when `ctx` is already cancelled.
    pub(crate) fn add(&self, key: K, ctx: &Ctx) -> Result<Slot<T>, HarnessError> {
        ctx.check()?;
        let slot = Slot::new();
        lock(&self.0).entry(key).or_default().push(slot.clone());
        Ok(slot)
    }

    /// Keys with a wait that has not settled.
    pub(crate) fn keys(&self) -> Vec<K> {
        let mut sets = lock(&self.0);
        sets.retain(|_, slots| {
            slots.retain(|slot| lock(&slot.0.0).is_none());
            !slots.is_empty()
        });
        sets.keys().cloned().collect()
    }

    pub(crate) fn resolve(&self, key: &K, value: T) {
        let slots = lock(&self.0).remove(key).unwrap_or_default();
        for slot in slots {
            slot.settle(Ok(value.clone()));
        }
    }

    pub(crate) fn reject_all(&self, error: HarnessError) {
        let sets = std::mem::take(&mut *lock(&self.0));
        for slot in sets.into_values().flatten() {
            slot.settle(Err(error.clone()));
        }
    }
}

/// Wait until any of `signals` is cancelled; returns its index and reason.
pub(crate) fn wait_any(signals: &[&Cancel]) -> (usize, HarnessError) {
    let slot: Slot<(usize, HarnessError)> = Slot::new();
    let mut registrations = Vec::new();
    for (index, signal) in signals.iter().enumerate() {
        let target = slot.clone();
        match signal.on_cancel(move |reason| target.settle(Ok((index, reason.clone())))) {
            Some(id) => registrations.push(((*signal).clone(), id)),
            None => slot.settle(Ok((index, signal.reason().expect("cancelled")))),
        }
    }
    let result = slot.wait(&Ctx::background()).expect("never rejected");
    for (signal, id) in registrations {
        signal.off(id);
    }
    result
}
