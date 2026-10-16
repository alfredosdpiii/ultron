//! Extensions' `after_cell_changes` event as an after-cell file hook (legacy-extension-adapter.ts `fileGuards`): the
//! files a cell changed by other means than checked writes go to every extension that handles the event, and their
//! messages reach the model with the lane's next cell result. Active only while an extension handles it.

use extensions::ExtensionHost;
use host::file_hooks::{AfterCellReport, AfterOutcome, CellChanges, FileWriteGuard};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

/// `emitAfterCellChanges` over one event.
type Emit = Box<dyn Fn(&Value) -> Option<Value> + Send + Sync>;

pub struct ExtensionAfterCell {
    /// Whether an extension handles `after_cell_changes` now.
    handled: Box<dyn Fn() -> bool + Send + Sync>,
    /// `emitAfterCellChanges`: every handler runs; their messages are joined (`{message}`).
    emit: Emit,
}

impl ExtensionAfterCell {
    /// The session's extensions.
    pub fn new(host: Arc<ExtensionHost>) -> Self {
        let emitter = host.clone();
        ExtensionAfterCell::with(
            move || host.router().has_handlers("after_cell_changes"),
            move |event| emitter.router().emit_after_cell_changes(event),
        )
    }

    /// Over any dispatcher (tests).
    pub fn with(
        handled: impl Fn() -> bool + Send + Sync + 'static,
        emit: impl Fn(&Value) -> Option<Value> + Send + Sync + 'static,
    ) -> Self {
        ExtensionAfterCell {
            handled: Box::new(handled),
            emit: Box::new(emit),
        }
    }
}

fn paths(list: &[std::path::PathBuf]) -> Vec<String> {
    list.iter().map(|p| p.to_string_lossy().into_owned()).collect()
}

impl FileWriteGuard for ExtensionAfterCell {
    fn name(&self) -> &str {
        "extension"
    }

    fn watches_cells(&self) -> bool {
        (self.handled)()
    }

    fn after_cell_changes(
        &self,
        changes: &CellChanges,
        cwd: &Path,
        lane: &str,
    ) -> Result<Option<AfterCellReport>, String> {
        let event = json!({
            "type": "after_cell_changes",
            "files": paths(&changes.files),
            "checked": paths(&changes.checked),
            "deleted": paths(&changes.deleted),
            "complete": changes.complete,
            "cwd": cwd.to_string_lossy(),
            "lane": lane,
        });
        // A string report is findings (`AfterCellReport` from a plain message).
        let message = (self.emit)(&event)
            .and_then(|reply| reply.get("message").and_then(Value::as_str).map(str::to_string))
            .filter(|message| !message.is_empty());
        Ok(message.map(|message| AfterCellReport {
            message: Some(message),
            outcome: AfterOutcome::Findings,
        }))
    }
}
