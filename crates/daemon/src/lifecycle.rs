//! `WorkerLifecycle` (experimental/session-worker.ts): a worker's reconciliation of server demand and activity. It
//! retires (once) when demand was initialized, nothing holds retirement, no operation is active and no attachment
//! demands it. A crashed owner's demand lapses after the orphan grace; an owner that never demanded lapses too.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

struct Demand {
    server: String,
    /// Set while a departed server's demand waits out the orphan grace.
    lapsing: Option<u64>,
}

struct Inner {
    owner: Option<String>,
    demands: HashMap<String, Demand>,
    active: HashSet<String>,
    holds: usize,
    initialized: bool,
    /// The pending initial (or orphan) grace timer, by token.
    initial_timer: Option<u64>,
    next_token: u64,
    retiring: bool,
}

pub struct Lifecycle {
    inner: Mutex<Inner>,
    orphan_grace: Duration,
    on_retire: Box<dyn Fn() + Send + Sync>,
}

fn key(server: &str, attachment: &str) -> String {
    format!("{server}\0{attachment}")
}

impl Lifecycle {
    pub fn new(
        owner: Option<String>,
        initial_grace: Duration,
        orphan_grace: Duration,
        on_retire: Box<dyn Fn() + Send + Sync>,
    ) -> Arc<Lifecycle> {
        let lifecycle = Arc::new(Lifecycle {
            inner: Mutex::new(Inner {
                owner,
                demands: HashMap::new(),
                active: HashSet::new(),
                holds: 0,
                initialized: false,
                initial_timer: None,
                next_token: 0,
                retiring: false,
            }),
            orphan_grace,
            on_retire,
        });
        lifecycle.arm_initial(initial_grace);
        lifecycle
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn token(inner: &mut Inner) -> u64 {
        inner.next_token += 1;
        inner.next_token
    }

    fn arm_initial(self: &Arc<Self>, delay: Duration) {
        let token = {
            let mut inner = self.lock();
            let token = Self::token(&mut inner);
            inner.initial_timer = Some(token);
            token
        };
        let weak = Arc::downgrade(self);
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            if let Some(this) = weak.upgrade() {
                let mut inner = this.lock();
                if inner.initial_timer == Some(token) {
                    inner.initial_timer = None;
                    inner.initialized = true;
                    this.reconcile(inner);
                }
            }
        });
    }

    pub fn owner(&self) -> Option<String> {
        self.lock().owner.clone()
    }

    pub fn retiring(&self) -> bool {
        self.lock().retiring
    }

    pub fn busy(&self) -> bool {
        !self.lock().active.is_empty()
    }

    pub fn server_connected(&self, server: &str) {
        let mut inner = self.lock();
        inner.owner = Some(server.to_string());
        for demand in inner.demands.values_mut() {
            if demand.server == server {
                demand.lapsing = None;
            }
        }
    }

    pub fn server_disconnected(self: &Arc<Self>, server: &str) {
        let mut inner = self.lock();
        let rearm = inner.owner.as_deref() == Some(server) && inner.initial_timer.is_some();
        if inner.owner.as_deref() == Some(server) {
            inner.owner = None;
        }
        let mut lapsing = Vec::new();
        let mut next = inner.next_token;
        for (key, demand) in inner.demands.iter_mut() {
            if demand.server == server && demand.lapsing.is_none() {
                next += 1;
                demand.lapsing = Some(next);
                lapsing.push((key.clone(), next));
            }
        }
        inner.next_token = next;
        drop(inner);
        if rearm {
            self.arm_initial(self.orphan_grace);
        }
        for (key, token) in lapsing {
            let weak: Weak<Lifecycle> = Arc::downgrade(self);
            let delay = self.orphan_grace;
            std::thread::spawn(move || {
                std::thread::sleep(delay);
                if let Some(this) = weak.upgrade() {
                    let mut inner = this.lock();
                    if inner.demands.get(&key).and_then(|d| d.lapsing) == Some(token) {
                        inner.demands.remove(&key);
                        this.reconcile(inner);
                    }
                }
            });
        }
    }

    /// Admit a request from `server` on `attachment`; the guard holds retirement until dropped.
    pub fn begin_request(self: &Arc<Self>, server: &str, attachment: &str) -> Result<Hold, String> {
        let inner = self.lock();
        if inner.retiring {
            return Err("Session worker is retiring".into());
        }
        if inner.owner.as_deref() != Some(server) {
            return Err("Session worker received a request from a stale server generation".into());
        }
        match inner.demands.get(&key(server, attachment)) {
            Some(demand) if demand.lapsing.is_none() => {}
            _ => return Err("Session worker request does not match the active attachment".into()),
        }
        drop(inner);
        Ok(self.hold_retirement())
    }

    pub fn hold_retirement(self: &Arc<Self>) -> Hold {
        self.lock().holds += 1;
        Hold {
            lifecycle: Some(self.clone()),
        }
    }

    pub fn set_demand(&self, server: &str, attachment: &str, attached: bool) -> Result<(), String> {
        let mut inner = self.lock();
        if inner.retiring {
            return Err("Session worker is retiring".into());
        }
        if inner.owner.as_deref() != Some(server) {
            return Err("Session worker received demand from a stale server generation".into());
        }
        inner.initialized = true;
        inner.initial_timer = None;
        let key = key(server, attachment);
        if attached {
            inner.demands.insert(
                key,
                Demand {
                    server: server.into(),
                    lapsing: None,
                },
            );
        } else {
            inner.demands.remove(&key);
        }
        self.reconcile(inner);
        Ok(())
    }

    pub fn release_server(&self, server: &str) {
        let mut inner = self.lock();
        if inner.owner.as_deref() == Some(server) {
            inner.owner = None;
        }
        inner.demands.retain(|_, d| d.server != server);
        inner.initialized = true;
        inner.initial_timer = None;
        self.reconcile(inner);
    }

    /// A lane operation (`run`, `compaction`, `navigation`) started.
    pub fn operation_started(&self, kind: &str, lane: &str, operation: &str) {
        self.lock().active.insert(format!("{kind}\0{lane}\0{operation}"));
    }

    pub fn operation_stopped(&self, kind: &str, lane: &str, operation: &str) {
        let mut inner = self.lock();
        inner.active.remove(&format!("{kind}\0{lane}\0{operation}"));
        self.reconcile(inner);
    }

    /// Cancel pending timers and drop every demand.
    pub fn close(&self) {
        let mut inner = self.lock();
        inner.initial_timer = None;
        inner.demands.clear();
    }

    fn reconcile(&self, mut inner: std::sync::MutexGuard<'_, Inner>) {
        if inner.retiring
            || !inner.initialized
            || inner.holds != 0
            || !inner.active.is_empty()
            || !inner.demands.is_empty()
        {
            return;
        }
        inner.retiring = true;
        drop(inner);
        (self.on_retire)();
    }

    fn release(&self) {
        let mut inner = self.lock();
        inner.holds -= 1;
        self.reconcile(inner);
    }
}

/// A retirement hold, released on drop.
pub struct Hold {
    lifecycle: Option<Arc<Lifecycle>>,
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Some(lifecycle) = self.lifecycle.take() {
            lifecycle.release();
        }
    }
}
