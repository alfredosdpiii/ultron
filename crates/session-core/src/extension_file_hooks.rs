//! Extensions' file events as file hooks (legacy-extension-adapter.ts `fileGuards`), each active only while an
//! extension handles its event:
//! - `before_file_write` ([`ExtensionBeforeWrite`]): a proposed `edit()`/`write()` goes to the handlers
//!   `{path, content, cwd, lane}`; the first block refuses the write, messages become a note, and a handler that
//!   fails or does not answer in 5 s lets the write through with a note (the session's `FileHooks` decide);
//! - `after_cell_changes` ([`ExtensionAfterCell`]): the files a cell changed by other means than checked writes go
//!   to every extension that handles the event, and their messages reach the model with the lane's next cell result.

use extensions::ExtensionHost;
use host::file_hooks::{AfterCellReport, AfterOutcome, CellChanges, FileWriteGuard, Verdict};
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

    fn checks_writes(&self) -> bool {
        false
    }

    fn checks_shell(&self) -> bool {
        false
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

/// `emitBeforeFileWrite` over one event: `Ok(None)` when no handler said anything, `Err` when one failed.
type EmitWrite = Box<dyn Fn(&Value) -> Result<Option<Value>, String> + Send + Sync>;

/// The `before_file_write` guard (name "extension").
pub struct ExtensionBeforeWrite {
    handled: Box<dyn Fn() -> bool + Send + Sync>,
    emit: EmitWrite,
}

impl ExtensionBeforeWrite {
    /// The session's extensions.
    pub fn new(host: Arc<ExtensionHost>) -> Self {
        let emitter = host.clone();
        ExtensionBeforeWrite::with(
            move || host.router().has_handlers("before_file_write"),
            move |event| {
                emitter
                    .router()
                    .emit_before_file_write(event)
                    .map_err(|error| error.error)
            },
        )
    }

    /// Over any dispatcher (tests).
    pub fn with(
        handled: impl Fn() -> bool + Send + Sync + 'static,
        emit: impl Fn(&Value) -> Result<Option<Value>, String> + Send + Sync + 'static,
    ) -> Self {
        ExtensionBeforeWrite {
            handled: Box::new(handled),
            emit: Box::new(emit),
        }
    }
}

/// `String(value)` of a block reason.
fn js_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

impl FileWriteGuard for ExtensionBeforeWrite {
    fn name(&self) -> &str {
        "extension"
    }

    fn enabled(&self) -> bool {
        (self.handled)()
    }

    fn checks_shell(&self) -> bool {
        false
    }

    fn before_write_on(&self, path: &Path, content: &str, cwd: &Path, lane: &str) -> Result<Verdict, String> {
        let event = json!({
            "type": "before_file_write",
            "path": path.to_string_lossy(),
            "content": content,
            "cwd": cwd.to_string_lossy(),
            "lane": lane,
        });
        Ok(match (self.emit)(&event)? {
            Some(verdict) if verdict["block"] == true => Verdict::Block(js_text(&verdict["reason"])),
            Some(verdict) => Verdict::Allow(verdict["message"].as_str().map(str::to_string)),
            None => Verdict::Allow(None),
        })
    }
}
