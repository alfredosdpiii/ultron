//! Working-tree checkpoints of an external root's turns (native Ultron records one per root user turn, see
//! session_core::core's checkpoints): each Claude Code turn records the working tree as an unreferenced commit
//! (delegation::checkpoints) under the turn's id (session_journal::checkpoints), before the turn's cells run.
//!
//! The same settings as native Ultron: `checkpoints.enabled` (default true; only Git or jj working trees record
//! anything), `checkpoints.maxFiles`, `checkpoints.maxFileBytes`; none while the shell sandbox is on (its Git would
//! run outside the sandbox). A tree that cannot be recorded keeps a note instead and never fails the turn.

use delegation::checkpoints::{self as vcs, Capture, Checkpoint, Limits};
use serde_json::{Value, json};
use session_core::durable_store::{SharedSession, lock};
use session_journal::checkpoints as stored;
use std::path::PathBuf;

/// What a turn's checkpoint needs: where, under which settings, with which `jj` (from the session's PATH).
pub struct TurnCheckpoints {
    pub session: SharedSession,
    pub cwd: PathBuf,
    /// The merged settings' `checkpoints` object.
    pub settings: Value,
    pub sandboxed: bool,
    pub jj: Option<PathBuf>,
}

impl TurnCheckpoints {
    fn limits(&self) -> Limits {
        let defaults = Limits::default();
        Limits {
            max_files: self.settings["maxFiles"]
                .as_u64()
                .map_or(defaults.max_files, |n| n as usize),
            max_file_bytes: self.settings["maxFileBytes"]
                .as_u64()
                .unwrap_or(defaults.max_file_bytes),
        }
    }

    /// Record the working tree for the root turn `turn`, before it runs.
    pub fn turn_started(&self, turn: &str) {
        if self.settings["enabled"] == false || self.sandboxed || turn.is_empty() {
            return;
        }
        let previous = stored::latest_taken(&lock(&self.session)).and_then(|v| Checkpoint::from_json(&v));
        let captured = vcs::capture(&self.cwd, previous.as_ref(), &self.limits(), self.jj.as_deref());
        let mut value = match captured {
            Ok(Capture::Outside) => return,
            Ok(Capture::Taken(checkpoint)) => checkpoint.to_json(),
            Ok(Capture::Unchanged(checkpoint)) => {
                let mut value = checkpoint.to_json();
                value["unchanged"] = json!(true);
                value
            }
            Ok(Capture::Skipped(note)) | Err(note) => json!({"skipped": note}),
        };
        value["kind"] = json!("turn");
        value["turn"] = json!(turn);
        value["createdAt"] = json!(session_core::lane::now_ms());
        let _ = stored::put(&mut lock(&self.session), turn, value);
    }
}

/// The first executable `jj` on the session's `path`.
pub fn jj_on(path: Option<&str>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path?)
        .map(|dir| dir.join("jj"))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}
