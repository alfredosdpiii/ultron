//! Working-tree checkpoints of the root lane's turns and the file-restoring rewind (Rust-only feature; see
//! docs/rust-rewrite/features.md): each root user turn records the working tree as an unreferenced commit
//! (crates/delegation `checkpoints`) under its prompt entry (crates/session-journal `checkpoints`), before the turn
//! runs; [`Op::RestoreFiles`](super::Op::RestoreFiles) writes a checkpoint's files back after saving the current
//! ones as another checkpoint.
//!
//! Settings: `checkpoints.enabled` (default true; only Git or jj working trees record anything),
//! `checkpoints.maxFiles`, `checkpoints.maxFileBytes`. Off while the shell sandbox is on (`sandbox.mode` other than
//! `off`): the checkpoint's Git runs on the host, outside the sandbox, and would obey hooks, filters and the
//! fsmonitor that sandboxed code can write into the repository's config (Mirror disables checkpoints in a sandbox
//! for the same reason).

use super::Core;
use crate::durable_store::lock as lock_session;
use crate::lane::now_ms;
use delegation::checkpoints::{self as vcs, Capture, Checkpoint, Limits};
use serde_json::{Value, json};
use session_journal::checkpoints::{self as stored, RESTORE_PREFIX};
use std::path::PathBuf;

/// A restore while a run streams.
pub const RESTORE_WHILE_STREAMING: &str = "Wait for the current response to finish before restoring files.";
/// A restore inside the shell sandbox.
pub const RESTORE_IN_SANDBOX: &str =
    "Checkpoints are off while the shell sandbox is on (sandbox.mode): their Git would run outside the sandbox.";

/// The first executable `name` on `path` (the session's PATH, never the process's).
pub fn on_path(path: Option<&str>, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            std::fs::metadata(candidate).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        })
}

impl Core {
    fn checkpoint_settings(&self) -> &Value {
        &self.runtime.config.settings["checkpoints"]
    }

    fn checkpoint_limits(&self) -> Limits {
        let settings = self.checkpoint_settings();
        let defaults = Limits::default();
        Limits {
            max_files: settings["maxFiles"].as_u64().map_or(defaults.max_files, |n| n as usize),
            max_file_bytes: settings["maxFileBytes"].as_u64().unwrap_or(defaults.max_file_bytes),
        }
    }

    /// Whether the shell sandbox is on.
    fn sandboxed(&self) -> bool {
        self.lane
            .stack
            .host
            .launcher()
            .is_some_and(|launcher| launcher.mode != sandbox::settings::Mode::Off)
    }

    fn jj(&self) -> Option<PathBuf> {
        on_path((self.env.var)("PATH").as_deref(), "jj")
    }

    /// Record the working tree for the root turn whose prompt is `prompt_entry` (before the turn runs). Never
    /// fails the turn: a tree that could not be recorded keeps a note instead.
    pub(super) fn checkpoint_turn(&mut self, prompt_entry: &str) {
        if self.checkpoint_settings()["enabled"] == false || self.sandboxed() || prompt_entry.is_empty() {
            return;
        }
        let previous = stored::latest_taken(&lock_session(&self.session)).and_then(|v| Checkpoint::from_json(&v));
        let captured = vcs::capture(
            &self.env.cwd,
            previous.as_ref(),
            &self.checkpoint_limits(),
            self.jj().as_deref(),
        );
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
        value["entryId"] = json!(prompt_entry);
        value["createdAt"] = json!(now_ms());
        // A checkpoint that cannot be stored costs the turn nothing.
        let _ = stored::put(&mut lock_session(&self.session), prompt_entry, value);
    }

    /// [`Op::Checkpoints`](super::Op::Checkpoints): `{checkpoints: [{key, kind, entryId, commit, createdAt,
    /// skipped?, unchanged?}]}`, oldest first, and whether new ones are recorded.
    pub(super) fn list_checkpoints(&self) -> Value {
        let listed: Vec<Value> = stored::list(&lock_session(&self.session))
            .into_iter()
            .map(|(key, value)| {
                let mut item = json!({
                    "key": key, "kind": value["kind"], "entryId": value["entryId"], "commit": value["commit"],
                    "createdAt": value["createdAt"],
                });
                for extra in ["skipped", "unchanged", "restoredTo"] {
                    if !value[extra].is_null() {
                        item[extra] = value[extra].clone();
                    }
                }
                item
            })
            .collect();
        let enabled = self.checkpoint_settings()["enabled"] != false && !self.sandboxed();
        json!({"checkpoints": listed, "enabled": enabled})
    }

    /// [`Op::RestoreFiles`](super::Op::RestoreFiles): write the files of checkpoint `key`, else of the checkpoint
    /// that describes entry `entry_id` (its turn's start), back into the working tree. The current files are saved
    /// first as a `restore:` checkpoint on the branch tip, so the restore can be undone the same way.
    pub(super) fn restore_files(&mut self, entry_id: Option<String>, key: Option<String>) -> Result<Value, String> {
        if self.run.is_some() {
            return Err(RESTORE_WHILE_STREAMING.into());
        }
        if self.sandboxed() {
            return Err(RESTORE_IN_SANDBOX.into());
        }
        let found = {
            let session = lock_session(&self.session);
            match (&key, &entry_id) {
                (Some(key), _) => session
                    .value(stored::CHECKPOINTS, key)
                    .cloned()
                    .map(|value| (key.clone(), value)),
                (None, Some(entry)) => stored::for_entry(&session, entry),
                (None, None) => return Err("Name a checkpoint or an entry to restore files to".into()),
            }
        };
        let Some((key, value)) = found else {
            return Err(
                "No checkpoint of the files at this point (checkpoints start with the first turn in a Git repository)"
                    .into(),
            );
        };
        if let Some(note) = value["skipped"].as_str() {
            return Err(format!("The files at this point were not recorded: {note}"));
        }
        let target = Checkpoint::from_json(&value).ok_or("The checkpoint record is incomplete")?;
        let restored = vcs::restore(&self.env.cwd, &target, &self.checkpoint_limits(), self.jj().as_deref())?;
        let short = |commit: &str| commit[..commit.len().min(12)].to_string();
        let already = target.vcs == "git" && restored.safety.tree == target.tree;
        if already {
            return Ok(json!({
                "checkpoint": key, "commit": target.commit, "written": 0, "deleted": 0, "safety": null,
                "message": format!("Files already match checkpoint {}", short(&target.commit)),
            }));
        }
        let tip = self.tip();
        let safety_key = format!(
            "{RESTORE_PREFIX}{}",
            session_journal::lock_clock(&lock_session(&self.session).clock()).next_id()
        );
        let mut safety = restored.safety.to_json();
        safety["kind"] = json!("restore");
        safety["entryId"] = tip.map_or(Value::Null, Value::from);
        safety["restoredTo"] = json!(key);
        safety["createdAt"] = json!(now_ms());
        stored::put(&mut lock_session(&self.session), &safety_key, safety)?;
        let counts = if target.vcs == "git" {
            format!(
                " ({} written, {} deleted)",
                restored.written.len(),
                restored.deleted.len()
            )
        } else {
            String::new()
        };
        Ok(json!({
            "checkpoint": key, "commit": target.commit, "written": restored.written.len(),
            "deleted": restored.deleted.len(), "safety": {"key": safety_key, "commit": restored.safety.commit},
            "message": format!(
                "Restored files to checkpoint {}{counts}. The files from before are checkpoint {}: restore the current entry in /tree to undo.",
                short(&target.commit),
                short(&restored.safety.commit)
            ),
        }))
    }
}
