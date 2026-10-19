//! The files a cell wrote through the checked `write()`/`edit()` (`files.before_write`), with their text before and
//! after, so the cell's result can carry a diff of what it changed (the interactive mode's expanded rlm cell). The
//! text before is read when the write is proposed, while the file still holds it; at the cell's end a write counts
//! only when the file holds what was proposed. Files changed by other means (`bash`) are not seen here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// Files above this size (before or after) are named without their texts.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;
/// Files one cell reports.
pub const MAX_FILES: usize = 32;

/// One file a cell wrote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CellWrite {
    pub path: PathBuf,
    /// The file did not exist before the cell.
    pub created: bool,
    /// The text before (empty for a new file) and after; None when either is too large or not UTF-8.
    pub texts: Option<(String, String)>,
}

/// Off until the composition root turns it on ([`crate::Host::recording_cell_writes`]).
#[derive(Default)]
pub struct CellWrites {
    enabled: AtomicBool,
    lanes: Mutex<HashMap<String, Vec<CellWrite>>>,
}

fn small_text(path: &Path) -> Result<Option<String>, ()> {
    match std::fs::metadata(path) {
        Err(_) => Ok(None),
        Ok(meta) if !meta.is_file() || meta.len() as usize > MAX_TEXT_BYTES => Err(()),
        Ok(_) => std::fs::read(path)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .map(Some)
            .ok_or(()),
    }
}

impl CellWrites {
    fn lanes(&self) -> std::sync::MutexGuard<'_, HashMap<String, Vec<CellWrite>>> {
        self.lanes.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::SeqCst);
    }

    /// A cell on `lane` starts: forget what an earlier one left (a cell that never ended).
    pub fn begin(&self, lane: &str) {
        self.lanes().remove(lane);
    }

    /// `lane` proposed writing `content` to `path` (absolute) and no hook blocked it. A file written twice keeps its
    /// first text before.
    pub fn record(&self, lane: &str, path: &Path, content: &str) {
        if !self.enabled.load(Ordering::SeqCst) {
            return;
        }
        let mut lanes = self.lanes();
        let writes = lanes.entry(lane.to_string()).or_default();
        if let Some(write) = writes.iter_mut().find(|w| w.path == path) {
            write.texts = match write.texts.take() {
                Some((before, _)) if content.len() <= MAX_TEXT_BYTES => Some((before, content.to_string())),
                _ => None,
            };
            return;
        }
        if writes.len() >= MAX_FILES {
            return;
        }
        let before = small_text(path);
        let created = matches!(before, Ok(None));
        let texts = match before {
            Ok(before) if content.len() <= MAX_TEXT_BYTES => Some((before.unwrap_or_default(), content.to_string())),
            _ => None,
        };
        writes.push(CellWrite {
            path: path.to_path_buf(),
            created,
            texts,
        });
    }

    /// What `lane`'s cell wrote, in order (and forget it): the writes whose file now holds the proposed text, minus
    /// those that changed nothing.
    pub fn take(&self, lane: &str) -> Vec<CellWrite> {
        let writes = self.lanes().remove(lane).unwrap_or_default();
        writes
            .into_iter()
            .filter(|write| match &write.texts {
                Some((before, after)) => {
                    (write.created || before != after)
                        && std::fs::read(&write.path).is_ok_and(|now| now == after.as_bytes())
                }
                None => write.path.exists(),
            })
            .collect()
    }
}
