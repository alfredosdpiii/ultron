//! What each cell changed on disk, for the editor: a metadata picture of the session's directory (the worker's own
//! after-cell snapshot, `delegation::workspace`) taken when a prompt starts and after each cell, diffed when the next
//! cell ends. The text of files this process has seen is kept (bounded) so a later change shows as a diff against it;
//! a file it never saw is reported by location only, and a new file as a diff from nothing.

use super::translate::{FileChanges, MAX_DIFF_BYTES, diff};
use delegation::workspace::{DEFAULT_MAX_SNAPSHOT_ENTRIES, WorkspaceSnapshot, diff_snapshots, snapshot_workspace};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Texts kept for later diffs, in all.
const MAX_CACHED_BYTES: usize = 8 * 1024 * 1024;

#[derive(Default)]
pub struct Tracker {
    baseline: Option<WorkspaceSnapshot>,
    texts: HashMap<PathBuf, String>,
    cached: usize,
}

/// The text of a file small enough to show.
fn small_text(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() as usize > MAX_DIFF_BYTES {
        return None;
    }
    String::from_utf8(std::fs::read(path).ok()?).ok()
}

impl Tracker {
    fn picture(dir: &Path) -> WorkspaceSnapshot {
        snapshot_workspace(dir, DEFAULT_MAX_SNAPSHOT_ENTRIES, true)
    }

    /// The picture changes are measured against (a prompt starts, or a cell starts with none held).
    pub fn begin(&mut self, dir: &Path) {
        if self.baseline.as_ref().is_none_or(|b| b.root != dir) {
            self.baseline = Some(Self::picture(dir));
        }
    }

    /// The run ended: the next one starts from a new picture.
    pub fn end(&mut self) {
        self.baseline = None;
    }

    fn remember(&mut self, path: &Path, text: &str) {
        if let Some(old) = self.texts.remove(path) {
            self.cached -= old.len();
        }
        if self.cached + text.len() <= MAX_CACHED_BYTES {
            self.cached += text.len();
            self.texts.insert(path.to_path_buf(), text.to_string());
        }
    }

    /// What changed since the baseline; the picture now becomes the next baseline.
    pub fn cell_ended(&mut self, dir: &Path) -> FileChanges {
        let after = Self::picture(dir);
        let Some(before) = self.baseline.replace(after.clone()) else {
            return FileChanges::default();
        };
        let Ok(found) = diff_snapshots(&before, &after) else {
            return FileChanges::default();
        };
        let mut changes = FileChanges::default();
        for (relative, added) in found
            .added
            .iter()
            .map(|r| (r, true))
            .chain(found.modified.iter().map(|r| (r, false)))
        {
            let path = after.root.join(relative);
            if let Some(text) = small_text(&path) {
                let old = if added { None } else { self.texts.get(&path).cloned() };
                if added || old.is_some() {
                    changes.diffs.push(diff(&path, old.as_deref(), &text));
                }
                self.remember(&path, &text);
            }
            changes.changed.push(path);
        }
        for relative in &found.deleted {
            let path = after.root.join(relative);
            if let Some(old) = self.texts.remove(&path) {
                self.cached -= old.len();
            }
            changes.deleted.push(path);
        }
        changes
    }
}
