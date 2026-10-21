//! Which lanes the host wakes when their work ends, and what follows from it. One rule for every mode: the root lane
//! (`main`) is woken only when its composer says so (the native session's event dispatcher with completion events
//! on, `ultron claude`'s wait-events hook); every other lane (subagents, other root conversations) and every root
//! nothing wakes (`ULTRON_ASYNC_EVENTS=off`, an `ultron mcp` root without `--wake`, a Claude child) waits in its cell.
//!
//! - A woken lane's `rlm.collect` never waits ([`CollectMode::Host`]): the host waits between turns and each end
//!   continues the run as a `child_done` event.
//! - Any other lane's `rlm.collect` (and `rlm.merge`'s wait) blocks the cell until the children end
//!   ([`CollectMode::InCell`]), cancelled with the cell and never parked: a parked cell is detached and its
//!   `cell_done` would reach nobody.
//!
//! The cell hints, the detached-cell note and the runtime guide word each lane's waits to match.

use delegation::host::CollectMode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Whether the session's root lane is woken by the ends of its work; clones share it. Off until the composer turns it
/// on.
#[derive(Clone, Default)]
pub struct RootWake(Arc<AtomicBool>);

impl RootWake {
    /// The root lane is (`true`) or is not woken from now on.
    pub fn set(&self, woken: bool) {
        self.0.store(woken, Ordering::SeqCst);
    }

    /// Whether the root lane is woken.
    pub fn root_woken(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Whether the host wakes `lane` when its work ends: only the root lane, and only while the root is woken.
    pub fn woken(&self, lane: &str) -> bool {
        lane == crate::lane::MAIN && self.root_woken()
    }

    /// How `lane`'s `rlm.collect` treats running children.
    pub fn collect_mode(&self, lane: &str) -> CollectMode {
        if self.woken(lane) {
            CollectMode::Host
        } else {
            CollectMode::InCell
        }
    }
}
