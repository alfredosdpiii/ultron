//! Port of `kernel-pool.ts`: bounds live REPL kernels (lanes) per worker. Idle kernels are evicted by TTL or to make
//! room, never while a cell runs (an unreleased lease) or while a holder pins the lane; with a snapshot path, state is
//! saved first and a failed snapshot keeps the kernel. A root opens more lanes by acquiring more lane names.
//!
//! Synchronous: the pool lives behind the host's mutex, so admission is serialized by construction.

use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The part of a kernel the pool needs. `snapshot` returns the names it could not save, or the error text.
pub trait PoolableKernel {
    fn snapshot(&mut self, path: &str) -> Result<Vec<String>, String>;
    fn shutdown(&mut self);
}

/// A real kernel in the pool: its snapshot is the host-signed one of [`kernel::Kernel::snapshot`], and a call that
/// fails (the kernel died, was shut down) is a failed snapshot, which refuses the eviction.
impl PoolableKernel for kernel::Kernel {
    fn snapshot(&mut self, path: &str) -> Result<Vec<String>, String> {
        let reply =
            kernel::Kernel::snapshot(self, Some(std::path::Path::new(path))).map_err(|error| error.to_string())?;
        snapshot_outcome(&serde_json::to_value(&reply).map_err(|error| error.to_string())?)
    }

    fn shutdown(&mut self) {
        kernel::Kernel::shutdown(self);
    }
}

/// Where a lane's snapshot goes before eviction; `None` evicts without one.
pub type SnapshotPath = Box<dyn Fn(&str) -> Option<String> + Send>;

pub struct KernelPoolOptions<K> {
    pub create: Box<dyn FnMut(&str) -> K + Send>,
    pub max_live: usize,
    pub idle_ttl_ms: f64,
    pub max_pinned: Option<usize>,
    pub snapshot_path: Option<SnapshotPath>,
    pub now: Box<dyn Fn() -> f64 + Send>,
}

struct Entry<K> {
    kernel: K,
    running: usize,
    last_used: f64,
}

/// A lease: `release` it (via [`KernelPool::release`]) when the cell ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lease {
    id: u64,
}

const MAX_EVICTION_RECORDS: usize = 256;

/// A clock reading as JSON: whole milliseconds as integers, like JavaScript numbers print.
fn ms(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9e15 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

/// A kernel's snapshot reply (`KernelExecutionResult`: `{status, error?: {evalue}, snapshot?: {skipped}}`) as the
/// pool's outcome: the skipped names, or why it failed.
pub fn snapshot_outcome(reply: &Value) -> Result<Vec<String>, String> {
    if reply["status"] == "error" {
        return Err(reply["error"]["evalue"]
            .as_str()
            .unwrap_or("snapshot failed")
            .to_string());
    }
    let Some(skipped) = reply["snapshot"]["skipped"].as_array() else {
        return Err("snapshot returned no report".into());
    };
    Ok(skipped.iter().filter_map(|s| s.as_str().map(String::from)).collect())
}

pub fn capacity_error(live: usize, max: usize) -> String {
    format!("RLM kernel capacity exhausted: {live}/{max} kernels are running or pinned")
}

pub struct KernelPool<K: PoolableKernel> {
    entries: Vec<(String, Entry<K>)>,
    pins: BTreeMap<String, Vec<String>>,
    leases: BTreeMap<u64, String>,
    next_lease: u64,
    records: Vec<Value>,
    closed: bool,
    options: KernelPoolOptions<K>,
    max_pinned: usize,
}

impl<K: PoolableKernel> KernelPool<K> {
    pub fn new(options: KernelPoolOptions<K>) -> Result<Self, String> {
        if options.max_live < 1 {
            return Err("maxLive must be a positive integer".into());
        }
        if !options.idle_ttl_ms.is_finite() || options.idle_ttl_ms < 0.0 {
            return Err("idleTtlMs must be a nonnegative number".into());
        }
        let max_pinned = options.max_pinned.unwrap_or(options.max_live);
        if max_pinned > options.max_live {
            return Err("maxPinned must be an integer between 0 and maxLive".into());
        }
        Ok(KernelPool {
            entries: Vec::new(),
            pins: BTreeMap::new(),
            leases: BTreeMap::new(),
            next_lease: 0,
            records: Vec::new(),
            closed: false,
            options,
            max_pinned,
        })
    }

    fn entry(&mut self, lane: &str) -> Option<&mut Entry<K>> {
        self.entries.iter_mut().find(|(l, _)| l == lane).map(|(_, e)| e)
    }

    /// Lease a lane's kernel, creating it (and evicting idle lanes to make room) when needed.
    pub fn acquire(&mut self, lane: &str) -> Result<Lease, String> {
        self.acquire_inner(lane, None)
    }

    /// [`KernelPool::acquire`] whose new kernel comes from `create` (which may fail: nothing is admitted then)
    /// instead of the pool's own factory.
    pub fn acquire_with(&mut self, lane: &str, create: impl FnOnce() -> Result<K, String>) -> Result<Lease, String> {
        self.acquire_inner(lane, Some(Box::new(create)))
    }

    #[allow(clippy::type_complexity)]
    fn acquire_inner(
        &mut self,
        lane: &str,
        create: Option<Box<dyn FnOnce() -> Result<K, String> + '_>>,
    ) -> Result<Lease, String> {
        if self.closed {
            return Err("RLM kernel pool is closed".into());
        }
        if self.entry(lane).is_none() {
            while self.entries.len() >= self.options.max_live {
                let Some(victim) = self.victims().into_iter().next() else {
                    return Err(capacity_error(self.entries.len(), self.options.max_live));
                };
                if self.evict(&victim, "capacity")["evicted"] != true {
                    return Err(capacity_error(self.entries.len(), self.options.max_live));
                }
            }
            let kernel = match create {
                Some(create) => create()?,
                None => (self.options.create)(lane),
            };
            let last_used = (self.options.now)();
            self.entries.push((
                lane.to_string(),
                Entry {
                    kernel,
                    running: 0,
                    last_used,
                },
            ));
        }
        let now = (self.options.now)();
        let entry = self.entry(lane).expect("admitted above");
        entry.running += 1;
        entry.last_used = now;
        self.next_lease += 1;
        self.leases.insert(self.next_lease, lane.to_string());
        Ok(Lease { id: self.next_lease })
    }

    /// The leased kernel.
    pub fn kernel(&mut self, lease: Lease) -> Option<&mut K> {
        let lane = self.leases.get(&lease.id)?.clone();
        self.entry(&lane).map(|e| &mut e.kernel)
    }

    /// End a lease; releasing twice is a no-op.
    pub fn release(&mut self, lease: Lease) {
        let Some(lane) = self.leases.remove(&lease.id) else {
            return;
        };
        let now = (self.options.now)();
        if let Some(entry) = self.entry(&lane) {
            entry.running -= 1;
            entry.last_used = now;
        }
    }

    /// Idle, unpinned lanes, least recently used first.
    fn victims(&self) -> Vec<String> {
        let mut idle: Vec<&(String, Entry<K>)> = self
            .entries
            .iter()
            .filter(|(lane, e)| e.running == 0 && !self.is_pinned(lane))
            .collect();
        idle.sort_by(|a, b| a.1.last_used.total_cmp(&b.1.last_used));
        idle.into_iter().map(|(lane, _)| lane.clone()).collect()
    }

    fn is_pinned(&self, lane: &str) -> bool {
        self.pins.get(lane).is_some_and(|h| !h.is_empty())
    }

    /// The lane's live kernel without leasing it.
    pub fn live(&mut self, lane: &str) -> Option<&mut K> {
        self.entry(lane).map(|e| &mut e.kernel)
    }

    pub fn pin(&mut self, lane: &str, holder: &str) -> Result<(), String> {
        if holder.is_empty() {
            return Err("pin holder must be nonempty".into());
        }
        let pinned = self.pins.len();
        let holders = self.pins.entry(lane.to_string()).or_default();
        if holders.is_empty() && pinned >= self.max_pinned {
            self.pins.retain(|_, h| !h.is_empty());
            return Err(capacity_error(pinned, self.max_pinned));
        }
        if !holders.iter().any(|h| h == holder) {
            holders.push(holder.to_string());
        }
        Ok(())
    }

    pub fn unpin(&mut self, lane: &str, holder: &str) {
        if let Some(holders) = self.pins.get_mut(lane) {
            holders.retain(|h| h != holder);
            if holders.is_empty() {
                self.pins.remove(lane);
            }
        }
    }

    /// Evict one lane (`reason`: idle, capacity, explicit, close). Refusals are recorded too.
    pub fn evict(&mut self, lane: &str, reason: &str) -> Value {
        let at = ms((self.options.now)());
        let refuse = |refused: &str| {
            json!({ "lane": lane, "reason": reason, "at": at, "evicted": false, "refused": refused,
                    "restorable": "none", "nonRestorable": [] })
        };
        let Some(index) = self.entries.iter().position(|(l, _)| l == lane) else {
            return self.record(refuse("absent"));
        };
        if reason != "close" {
            if self.entries[index].1.running > 0 {
                return self.record(refuse("running"));
            }
            if self.is_pinned(lane) {
                return self.record(refuse("pinned"));
            }
        }
        // Only close reaches a running kernel; its snapshot would wait behind the cell.
        let busy = self.entries[index].1.running > 0;
        let path = if busy {
            None
        } else {
            self.options.snapshot_path.as_ref().and_then(|f| f(lane))
        };
        let mut result = json!({ "lane": lane, "reason": reason, "at": at, "evicted": true, "restorable": "none", "nonRestorable": [] });
        if busy {
            result["error"] = json!("cell running at close; not snapshotted");
        }
        if let Some(path) = path {
            match self.entries[index].1.kernel.snapshot(&path) {
                Ok(skipped) => {
                    result["snapshotPath"] = json!(path);
                    result["restorable"] = json!(if skipped.is_empty() { "full" } else { "partial" });
                    result["nonRestorable"] = json!(skipped);
                }
                Err(error) => {
                    result["snapshotPath"] = json!(path);
                    result["error"] = json!(error);
                    if reason != "close" {
                        result["evicted"] = json!(false);
                        result["refused"] = json!("snapshot-failed");
                        return self.record(result);
                    }
                }
            }
        }
        let (_, mut entry) = self.entries.remove(index);
        entry.kernel.shutdown();
        self.record(result)
    }

    /// Drop a lane's kernel without a snapshot (its lane is closed for good), pins and all; false when it had none.
    pub fn discard(&mut self, lane: &str) -> bool {
        self.pins.remove(lane);
        let Some(index) = self.entries.iter().position(|(l, _)| l == lane) else {
            return false;
        };
        let (_, mut entry) = self.entries.remove(index);
        entry.kernel.shutdown();
        true
    }

    /// Evict every idle, unpinned kernel whose idle time reached the TTL.
    pub fn sweep(&mut self) -> Vec<Value> {
        let now = (self.options.now)();
        let ttl = self.options.idle_ttl_ms;
        let expired: Vec<String> = self
            .victims()
            .into_iter()
            .filter(|lane| self.entries.iter().any(|(l, e)| l == lane && now - e.last_used >= ttl))
            .collect();
        expired.iter().map(|lane| self.evict(lane, "idle")).collect()
    }

    pub fn stats(&self) -> Value {
        let now = (self.options.now)();
        let lanes: Vec<Value> = self
            .entries
            .iter()
            .map(|(lane, e)| {
                let pinned: Vec<&String> = self.pins.get(lane).map(|h| h.iter().collect()).unwrap_or_default();
                let idle = if e.running > 0 { 0.0 } else { now - e.last_used };
                json!({ "lane": lane, "running": e.running, "pinnedBy": pinned, "idleMs": ms(idle) })
            })
            .collect();
        json!({ "live": self.entries.len(), "maxLive": self.options.max_live, "lanes": lanes })
    }

    /// Eviction history, refusals included, oldest first (bounded).
    pub fn evictions(&self) -> &[Value] {
        &self.records
    }

    fn record(&mut self, record: Value) -> Value {
        self.records.push(record.clone());
        if self.records.len() > MAX_EVICTION_RECORDS {
            self.records.remove(0);
        }
        record
    }

    /// Shut down every kernel, snapshotting idle ones first. Pins do not block close.
    pub fn close(&mut self) {
        self.closed = true;
        let lanes: Vec<String> = self.entries.iter().map(|(l, _)| l.clone()).collect();
        for lane in lanes {
            self.evict(&lane, "close");
        }
    }
}
