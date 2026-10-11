//! Port of `packages/agent/src/harness/execution/effect-gate.ts`: the per-drive admission gate that makes abort
//! two-phase. `begin_abort` refuses new effects at once (with the pending cancellation, so the refused party can
//! wait for it to commit); `signal_abort`, called once the abort request is durable, aborts work already admitted.
//! `close` refuses everything permanently and signals admitted work.

use std::sync::{Arc, Mutex};

use crate::supervisor::CancelSignal;

/// Refusal of an effect while an abort is being committed; carries that pending cancellation.
#[derive(Clone, Debug, PartialEq)]
pub struct AbortRequested<C> {
    pub cancellation: C,
}

/// Why the gate refused an effect.
#[derive(Clone, Debug, PartialEq)]
pub enum Refusal<C, E> {
    Aborting(AbortRequested<C>),
    Closed(E),
}

#[derive(Clone, Debug)]
enum State<C, E> {
    Open,
    Aborting(C),
    Closed(E),
}

#[derive(Debug)]
struct Inner<C, E> {
    state: State<C, E>,
    signal: CancelSignal,
}

/// Procedure-facing view: admit effects, observe the abort signal.
#[derive(Clone, Debug)]
pub struct Gate<C, E>(Arc<Mutex<Inner<C, E>>>);

/// Owner-facing lifecycle controls of the same gate.
#[derive(Clone, Debug)]
pub struct GateControl<C, E>(Arc<Mutex<Inner<C, E>>>);

/// A gate and its controls (`createGate`).
pub fn create_gate<C: Clone, E: Clone>() -> (Gate<C, E>, GateControl<C, E>) {
    let inner = Arc::new(Mutex::new(Inner {
        state: State::Open,
        signal: CancelSignal::default(),
    }));
    (Gate(inner.clone()), GateControl(inner))
}

fn lock<C, E>(inner: &Mutex<Inner<C, E>>) -> std::sync::MutexGuard<'_, Inner<C, E>> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

impl<C: Clone, E: Clone> Gate<C, E> {
    /// Runs `invoke` only while the gate is open.
    pub fn admit<T>(&self, invoke: impl FnOnce() -> T) -> Result<T, Refusal<C, E>> {
        match &lock(&self.0).state {
            State::Open => {}
            State::Aborting(cancellation) => {
                return Err(Refusal::Aborting(AbortRequested {
                    cancellation: cancellation.clone(),
                }));
            }
            State::Closed(error) => return Err(Refusal::Closed(error.clone())),
        }
        Ok(invoke())
    }

    /// Aborted once the abort is durable or the gate closed.
    pub fn signal(&self) -> CancelSignal {
        lock(&self.0).signal.clone()
    }
}

impl<C: Clone, E: Clone> GateControl<C, E> {
    /// Refuses new effects with `cancellation`; only from the open state.
    pub fn begin_abort(&self, cancellation: C) {
        let mut inner = lock(&self.0);
        if matches!(inner.state, State::Open) {
            inner.state = State::Aborting(cancellation);
        }
    }

    /// Aborts admitted work; only while aborting and not yet signalled.
    pub fn signal_abort(&self) {
        let inner = lock(&self.0);
        if matches!(inner.state, State::Aborting(_)) && !inner.signal.is_cancelled() {
            inner.signal.cancel("Abort requested");
        }
    }

    /// Refuses every later effect with `error` and signals admitted work.
    pub fn close(&self, error: E, reason: &str) {
        let mut inner = lock(&self.0);
        if matches!(inner.state, State::Closed(_)) {
            return;
        }
        inner.state = State::Closed(error);
        inner.signal.cancel(reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from packages/agent/test/harness/execution-primitives.test.ts ("createGate").

    #[test]
    fn harness_gate_refuses_synchronously_and_signals_only_after_commit() {
        let (gate, control) = create_gate::<&str, String>();
        control.begin_abort("cancellation-1");
        assert_eq!(
            gate.admit(|| ()),
            Err(Refusal::Aborting(AbortRequested {
                cancellation: "cancellation-1"
            }))
        );
        assert!(!gate.signal().is_cancelled());
        control.signal_abort();
        assert!(gate.signal().is_cancelled());
        assert_eq!(gate.signal().reason().as_deref(), Some("Abort requested"));
    }

    #[test]
    fn harness_gate_close_is_permanent_and_signals() {
        let (gate, control) = create_gate::<&str, String>();
        control.close("closed".into(), "closed");
        assert_eq!(gate.admit(|| ()), Err(Refusal::Closed("closed".into())));
        assert!(gate.signal().is_cancelled());
    }

    // Transitions from effect-gate.ts beyond the ported cases.

    #[test]
    fn harness_gate_admits_while_open_and_ignores_signal_without_abort() {
        let (gate, control) = create_gate::<&str, String>();
        assert_eq!(gate.admit(|| 7), Ok(7));
        control.signal_abort();
        assert!(!gate.signal().is_cancelled());
        assert_eq!(gate.admit(|| 8), Ok(8));
    }

    #[test]
    fn harness_gate_first_abort_and_first_close_win() {
        let (gate, control) = create_gate::<&str, String>();
        control.begin_abort("first");
        control.begin_abort("second");
        assert_eq!(
            gate.admit(|| ()),
            Err(Refusal::Aborting(AbortRequested { cancellation: "first" }))
        );
        control.close("closed".into(), "closed");
        control.begin_abort("third");
        control.close("again".into(), "again");
        assert_eq!(gate.admit(|| ()), Err(Refusal::Closed("closed".into())));
        // Closing while aborting signals admitted work with the close reason.
        assert_eq!(gate.signal().reason().as_deref(), Some("closed"));
        control.signal_abort();
        assert_eq!(gate.signal().reason().as_deref(), Some("closed"));
    }

    #[test]
    fn harness_gate_close_after_signal_keeps_the_abort() {
        let (gate, control) = create_gate::<&str, String>();
        control.begin_abort("c");
        control.signal_abort();
        control.close("closed".into(), "closed");
        assert_eq!(gate.signal().reason().as_deref(), Some("Abort requested"));
        assert_eq!(gate.admit(|| ()), Err(Refusal::Closed("closed".into())));
    }
}
