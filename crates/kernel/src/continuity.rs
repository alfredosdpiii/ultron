//! Kernel state continuity (Rust-only; kernel.ts has none): the REPL's state survives a kernel crash or restart.
//!
//! With a [`ContinuityPolicy`] the supervisor snapshots the namespace after every cell (debounced by
//! `min_interval`, with a flush once idle), so a crash loses at most the cell that was running. When the kernel
//! dies it is restarted at once and restored from the latest checkpoint, and the next cell result reports exactly
//! which names came back and which were lost, with the reason for each (A08: state that cannot be restored is
//! reported, never silently replaced).

use crate::snapshot::{RestoreReport, SnapshotReport};
use serde::Serialize;
use std::time::Duration;

/// How the supervisor keeps the REPL state recoverable. Requires `KernelOptions::snapshot_path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContinuityPolicy {
    /// Least time between two checkpoints. Cells finishing sooner are coalesced into one snapshot taken once the
    /// interval has passed (the idle flush). Zero snapshots after every cell.
    pub min_interval: Duration,
}

impl Default for ContinuityPolicy {
    fn default() -> Self {
        Self {
            min_interval: Duration::ZERO,
        }
    }
}

/// A name that did not come back, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LostName {
    pub name: String,
    pub reason: String,
}

/// What a recovery restored and lost. Attached to the first cell result after the kernel restarted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuityReport {
    /// Why the kernel restarted: the failure the previous kernel ended with.
    pub reason: String,
    pub snapshot_path: String,
    /// Names restored from the checkpoint, sorted.
    pub restored: Vec<String>,
    /// Names the checkpoint knew that did not come back, sorted by name.
    pub lost: Vec<LostName>,
    /// A cell was running when the kernel died; whatever it changed is gone.
    pub interrupted_cell: bool,
    /// Cells that completed after the checkpoint; their changes are gone too (non-zero only with a debounce).
    pub unsaved_cells: u64,
    /// Set when the checkpoint could not be restored (refused, unreadable, or the kernel died restoring it); the
    /// kernel then starts with an empty namespace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore_error: Option<String>,
}

/// The failure a recovery reports, recorded when the kernel dies.
#[derive(Debug, Clone)]
pub(crate) struct Crash {
    pub reason: String,
    pub interrupted_cell: bool,
    pub unsaved_cells: u64,
    pub restore_error: Option<String>,
}

fn lost_from(names: &[String], reason: impl Fn(&str) -> Option<String>) -> Vec<LostName> {
    names
        .iter()
        .map(|name| LostName {
            name: name.clone(),
            reason: reason(name).unwrap_or_else(|| "not restorable".into()),
        })
        .collect()
}

/// What a recovery restored and lost. `restore` is the kernel's report, or why restoring failed; `checkpoint` is the
/// latest snapshot this supervisor wrote, which names what existed when restoring is impossible.
pub(crate) fn recovery_report(
    crash: Crash,
    snapshot_path: String,
    restore: Result<Option<RestoreReport>, String>,
    checkpoint: Option<&SnapshotReport>,
) -> ContinuityReport {
    let all_checkpoint_names = |why: &str| -> Vec<LostName> {
        let Some(checkpoint) = checkpoint else {
            return Vec::new();
        };
        let mut lost = lost_from(&checkpoint.saved, |_| Some(why.to_string()));
        lost.extend(lost_from(&checkpoint.skipped, |name| {
            checkpoint.reason(name).map(String::from)
        }));
        lost
    };
    let (restored, mut lost, restore_error) = match restore {
        Ok(Some(report)) if !report.missing => {
            let mut lost = lost_from(&report.skipped, |name| report.reason(name).map(String::from));
            // A name the checkpoint saved but the restore did not bring back (another writer replaced the file).
            if let Some(checkpoint) = checkpoint {
                for name in &checkpoint.saved {
                    if !report.restored.contains(name) && !report.skipped.contains(name) {
                        lost.push(LostName {
                            name: name.clone(),
                            reason: "absent from the restored snapshot".into(),
                        });
                    }
                }
            }
            (report.restored, lost, crash.restore_error)
        }
        Ok(_) => (
            Vec::new(),
            all_checkpoint_names("snapshot file is missing"),
            crash.restore_error,
        ),
        Err(error) => (
            Vec::new(),
            all_checkpoint_names(&format!("snapshot not restored: {error}")),
            Some(error),
        ),
    };
    lost.sort_by(|left, right| left.name.cmp(&right.name));
    lost.dedup_by(|left, right| left.name == right.name);
    ContinuityReport {
        reason: crash.reason,
        snapshot_path,
        restored,
        lost,
        interrupted_cell: crash.interrupted_cell,
        unsaved_cells: crash.unsaved_cells,
        restore_error,
    }
}

impl ContinuityReport {
    /// The notice the model reads at the start of the cell's stderr.
    pub fn render(&self) -> String {
        let mut text = format!("[RLM kernel restarted after: {}]\n", self.reason);
        match &self.restore_error {
            Some(error) => text.push_str(&format!(
                "[State not restored ({error}); the namespace started empty]\n"
            )),
            None if self.restored.is_empty() => text.push_str("[No state restored]\n"),
            None => text.push_str(&format!("[Restored: {}]\n", self.restored.join(", "))),
        }
        if !self.lost.is_empty() {
            let lost: Vec<String> = self
                .lost
                .iter()
                .map(|lost| format!("{} ({})", lost.name, lost.reason))
                .collect();
            text.push_str(&format!("[Lost: {}]\n", lost.join("; ")));
        }
        if self.interrupted_cell {
            text.push_str("[The cell running at the failure did not finish; its changes to Python state are lost]\n");
        }
        if self.unsaved_cells > 0 {
            text.push_str(&format!(
                "[{} completed cell(s) after the last checkpoint are lost; restored values predate them]\n",
                self.unsaved_cells
            ));
        }
        text
    }
}
