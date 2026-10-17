//! Document states: a publication-only replicated state over one committed document incarnation.
//!
//! Port of `Session.documentState` (packages/durable/src/session/session.ts) with the parts of Chord it uses:
//! `replicatedState(source)` (`AttachedReplicatedStateImpl`, `ReplicatedStatePublisher` and `StateSubscriber` of
//! chord/src/services/state.ts) over the Session's `CommittedStateSource` (session/observation.ts).
//!
//! A state is hydrated when it is attached and follows every later commit of its incarnation: [`DocumentState::value`]
//! is the newest published value as soon as the commit is published (`None` once the incarnation is retired; a
//! recreated document is a new incarnation it does not follow). Subscribers get a `hydrate` delivery with the state's
//! current sequence, then one `update` per publication, serialized per subscriber. At most 100 deliveries wait behind
//! a running callback; one more keeps only the newest (and a hydration not yet started). Disposal and Session close
//! stop publication; the last value stays readable.
//!
//! Differences from upstream: each subscriber delivers on its own thread (as watches do), so a callback may block or
//! commit without holding up the committer; a callback cannot fail (Chord reports a throw and continues, a plain
//! Session's `report` ignores it); dropping a [`DocumentState`] disposes it; there is no delivery `Context`.

use crate::DocumentId;
use crate::observation::Observed;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// `StateSubscriber`'s bound on deliveries waiting behind a running callback.
const MAX_PENDING_DELIVERIES: usize = 100;

/// `ReplicatedStateDelivery.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryKind {
    Hydrate,
    Update,
}

impl DeliveryKind {
    /// The upstream `kind` string.
    pub fn as_str(self) -> &'static str {
        match self {
            DeliveryKind::Hydrate => "hydrate",
            DeliveryKind::Update => "update",
        }
    }
}

/// `ReplicatedStateDelivery`: which publication a callback sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Delivery {
    pub kind: DeliveryKind,
    pub sequence: u64,
}

type StateListener = Box<dyn FnMut(&Observed, Delivery) + Send>;

#[derive(Clone)]
struct Pending {
    value: Observed,
    delivery: Delivery,
}

struct SubscriberState {
    pending: VecDeque<Pending>,
    /// A delivery has begun (`#started`): an overflow no longer keeps the hydration.
    started: bool,
    running: bool,
    /// Unsubscribed: pending deliveries are discarded.
    closed: bool,
    /// The state publishes nothing more: the thread ends once the queue is empty.
    source_ended: bool,
}

struct Subscriber {
    state: Mutex<SubscriberState>,
    changed: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Subscriber {
    fn new(source_ended: bool) -> Arc<Subscriber> {
        Arc::new(Subscriber {
            state: Mutex::new(SubscriberState {
                pending: VecDeque::new(),
                started: false,
                running: false,
                closed: false,
                source_ended,
            }),
            changed: Condvar::new(),
        })
    }

    /// `StateSubscriber.push`.
    fn push(&self, frame: Pending) {
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        if state.pending.len() == MAX_PENDING_DELIVERIES {
            // A hydration that has not started yet survives the overflow.
            let hydration = if state.started {
                None
            } else {
                state.pending.front().cloned()
            };
            state.pending.clear();
            state.pending.extend(hydration);
        }
        state.pending.push_back(frame);
        self.changed.notify_all();
    }

    fn end_source(&self) {
        lock(&self.state).source_ended = true;
        self.changed.notify_all();
    }

    fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        state.pending.clear();
        self.changed.notify_all();
    }

    fn deliver(self: Arc<Self>, mut listener: StateListener) {
        loop {
            let frame = {
                let mut state = lock(&self.state);
                loop {
                    if state.closed {
                        return;
                    }
                    if let Some(frame) = state.pending.pop_front() {
                        state.started = true;
                        state.running = true;
                        break frame;
                    }
                    if state.source_ended {
                        return;
                    }
                    state = self
                        .changed
                        .wait(state)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
            };
            listener(&frame.value, frame.delivery);
            lock(&self.state).running = false;
            self.changed.notify_all();
        }
    }
}

struct Inner {
    value: Observed,
    sequence: u64,
    /// Still receiving the source's frames: not disposed, not retired, Session open.
    attached: bool,
    subscribers: Vec<Arc<Subscriber>>,
}

pub(crate) struct StateShared {
    pub(crate) record: DocumentId,
    inner: Mutex<Inner>,
}

impl StateShared {
    pub(crate) fn new(record: DocumentId, value: Arc<serde_json::Value>) -> Arc<StateShared> {
        Arc::new(StateShared {
            record,
            inner: Mutex::new(Inner {
                value: Some(value),
                sequence: 0,
                attached: true,
                subscribers: Vec::new(),
            }),
        })
    }

    pub(crate) fn is_ended(&self) -> bool {
        !lock(&self.inner).attached
    }

    /// `CommittedStateSource.advance` then `ReplicatedStatePublisher.publish`: the value is current at once; every
    /// subscriber queues the update.
    pub(crate) fn advance(&self, value: Observed) {
        let mut inner = lock(&self.inner);
        if !inner.attached {
            return;
        }
        inner.sequence += 1;
        // A retired incarnation publishes nothing more (a recreated document is another incarnation).
        inner.attached = value.is_some();
        inner.value = value.clone();
        let delivery = Delivery {
            kind: DeliveryKind::Update,
            sequence: inner.sequence,
        };
        for subscriber in &inner.subscribers {
            subscriber.push(Pending {
                value: value.clone(),
                delivery,
            });
            if !inner.attached {
                subscriber.end_source();
            }
        }
    }

    /// Stop publishing (`dispose`, `closeSession`); the value stays readable.
    pub(crate) fn detach(&self) {
        let mut inner = lock(&self.inner);
        inner.attached = false;
        for subscriber in &inner.subscribers {
            subscriber.end_source();
        }
    }
}

/// A hydrated, read-only replicated state of one committed document (`DocumentState`).
pub struct DocumentState {
    pub(crate) shared: Arc<StateShared>,
}

impl DocumentState {
    /// The newest published value; `None` once the incarnation is retired.
    pub fn value(&self) -> Observed {
        lock(&self.shared.inner).value.clone()
    }

    /// Subscribe: a `hydrate` delivery of the current value and sequence, then every later publication, in order,
    /// on the subscription's own thread.
    pub fn subscribe(&self, listener: impl FnMut(&Observed, Delivery) + Send + 'static) -> StateSubscription {
        let mut inner = lock(&self.shared.inner);
        let subscriber = Subscriber::new(!inner.attached);
        subscriber.push(Pending {
            value: inner.value.clone(),
            delivery: Delivery {
                kind: DeliveryKind::Hydrate,
                sequence: inner.sequence,
            },
        });
        inner.subscribers.push(subscriber.clone());
        drop(inner);
        let delivering = subscriber.clone();
        std::thread::spawn(move || delivering.deliver(Box::new(listener)));
        StateSubscription {
            state: Arc::downgrade(&self.shared),
            subscriber,
        }
    }

    /// Idempotently stop following commits; the last published value remains readable.
    pub fn dispose(&self) {
        self.shared.detach();
    }
}

impl Drop for DocumentState {
    fn drop(&mut self) {
        self.shared.detach();
    }
}

/// One subscription of a [`DocumentState`].
pub struct StateSubscription {
    state: std::sync::Weak<StateShared>,
    subscriber: Arc<Subscriber>,
}

impl StateSubscription {
    /// Discard pending deliveries and receive no more; a callback already running is not interrupted.
    pub fn unsubscribe(&self) {
        self.subscriber.close();
        if let Some(state) = self.state.upgrade() {
            lock(&state.inner)
                .subscribers
                .retain(|subscriber| !Arc::ptr_eq(subscriber, &self.subscriber));
        }
    }

    /// Wait up to `timeout` until every queued delivery has run (or the subscription ended); false on timeout.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        let state = lock(&self.subscriber.state);
        let (state, _) = self
            .subscriber
            .changed
            .wait_timeout_while(state, timeout, |state| {
                !state.closed && (state.running || !state.pending.is_empty())
            })
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.closed || !(state.running || !state.pending.is_empty())
    }
}
