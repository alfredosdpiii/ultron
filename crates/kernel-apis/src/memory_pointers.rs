//! File-linked memory as pointers: when a cell writes a file (a checked `write()`/`edit()` or any other change the
//! after-cell snapshot finds) that matches a typed record's `pathGlobs`, the lane's next cell result ends with one
//! line per record, `[memory] <id> <type>: <situation>`, never the record's content: the model reads it with
//! `memory.get(id)` when it wants it. At most [`MAX_POINTERS`] lines per cell; each record is pointed at once per
//! session. The setting `memory.filePointers: false` turns this off.

use crate::memory_records::{Glob, IndexEntry, RecordIndex};
use crate::util::lock;
use host::file_hooks::{CellChanges, CellObserver, inside};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Pointer lines per cell.
pub const MAX_POINTERS: usize = 3;

/// Whether the settings leave file pointers on (`memory.filePointers`, default true).
pub fn file_pointers_enabled(settings: &Value) -> bool {
    settings["memory"]["filePointers"] != false
}

/// A session's pointer observer, or `None` when the settings turn pointers off (what the composition root registers
/// with the session's file hooks).
pub fn session_pointers(
    settings: &Value,
    index: Arc<RecordIndex>,
    filters: Map<String, Value>,
) -> Option<Arc<dyn CellObserver>> {
    file_pointers_enabled(settings).then(|| Arc::new(MemoryPointers::new(index, filters)) as Arc<dyn CellObserver>)
}

/// One pointer line.
pub fn pointer_line(entry: &IndexEntry) -> String {
    format!("[memory] {} {}: {}", entry.id, entry.typed.kind, entry.typed.situation)
}

/// The pointers of one session, over the shared record index.
pub struct MemoryPointers {
    index: Arc<RecordIndex>,
    /// Scope → identity tags of the session: only its own, its project's and the machine's records apply.
    filters: Map<String, Value>,
    /// Records already pointed at in this session.
    delivered: Mutex<HashSet<String>>,
}

impl MemoryPointers {
    pub fn new(index: Arc<RecordIndex>, filters: Map<String, Value>) -> MemoryPointers {
        MemoryPointers {
            index,
            filters,
            delivered: Mutex::new(HashSet::new()),
        }
    }

    /// The pointer lines for a cell that wrote `paths` (relative to the session directory, with `/`).
    pub fn pointers_for(&self, paths: &[String]) -> Vec<String> {
        if paths.is_empty() {
            return Vec::new();
        }
        let Ok(entries) = self.index.entries() else {
            return Vec::new();
        };
        let mut delivered = lock(&self.delivered);
        let mut lines = Vec::new();
        for entry in entries {
            if lines.len() == MAX_POINTERS {
                break;
            }
            if entry.typed.path_globs.is_empty() || delivered.contains(&entry.id) || !entry.visible(&self.filters) {
                continue;
            }
            let globs: Vec<Glob> = entry
                .typed
                .path_globs
                .iter()
                .filter_map(|pattern| Glob::new(pattern).ok())
                .collect();
            if paths.iter().any(|path| globs.iter().any(|glob| glob.matches(path))) {
                delivered.insert(entry.id.clone());
                lines.push(pointer_line(&entry));
            }
        }
        lines
    }
}

impl CellObserver for MemoryPointers {
    fn cell_changed(&self, changes: &CellChanges, cwd: &Path, _lane: &str) -> Vec<String> {
        let paths: Vec<String> = changes
            .files
            .iter()
            .chain(&changes.checked)
            .filter_map(|path| inside(cwd, path))
            .map(|relative| relative.replace('\\', "/"))
            .collect();
        self.pointers_for(&paths)
    }
}
