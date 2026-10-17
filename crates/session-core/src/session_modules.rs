//! The session's host-module documents as values of the session itself, as the TypeScript worker keeps them
//! (`createSessionModuleStore(session, name)` at `ultron.module/<name>`: hints, stats, goal, jobs, tool-calls, family,
//! progress, schedules, instances, grants, release-gates, skills; the local services at `ultron.local/<key>`; the
//! memory journal at `ultron.memory.state/root`). Resuming the session restores them, a tree fork copies them, a
//! branch fork starts without them (session-journal's fork projection), and refinements follow the main lane's
//! branch (`mainBranch`).

use crate::durable_store::SharedSession;
use services::{Branch, BranchSource, SessionDocument, SessionValues};
use std::collections::HashSet;
use std::sync::Arc;

/// `createSessionModuleStore`'s namespace.
pub const MODULE_NAMESPACE: &str = "ultron.module";

/// A module's document as a session value; a write that changes nothing is not journaled (opening a session
/// re-persists its shell jobs, and the session's place in the resume list follows its last write).
pub struct ModuleDocument(pub SessionDocument);

impl kernel_apis::DocumentStore for ModuleDocument {
    fn read(&self) -> Result<Option<serde_json::Value>, String> {
        kernel_apis::DocumentStore::read(&self.0)
    }

    fn write(&self, document: &serde_json::Value) -> Result<(), String> {
        if kernel_apis::DocumentStore::read(&self.0)?.as_ref() == Some(document) {
            return Ok(());
        }
        kernel_apis::DocumentStore::write(&self.0, document)
    }
}

/// The shell jobs' journal as the session value `ultron.module/jobs`.
pub struct ModuleJobJournal(pub ModuleDocument);

impl host::journal::JobJournal for ModuleJobJournal {
    fn read(&self) -> Result<Option<serde_json::Value>, String> {
        kernel_apis::DocumentStore::read(&self.0)
    }

    fn write(&self, document: &serde_json::Value) -> Result<(), String> {
        // A session that never ran a job keeps no journal.
        let empty = document["jobs"].as_array().is_some_and(Vec::is_empty);
        if empty && kernel_apis::DocumentStore::read(&self.0)?.is_none() {
            return Ok(());
        }
        kernel_apis::DocumentStore::write(&self.0, document)
    }
}

/// The session's values (the actor's open session; writes stop once it is handed to a successor).
pub fn session_values(session: &SharedSession) -> SessionValues {
    SessionValues::shared(session.clone())
}

/// `mainBranch`: the main lane's tip is the anchor of a change, and an anchor applies while it is on the main
/// lane's path.
pub fn main_branch(values: &SessionValues) -> BranchSource {
    let session = values.session().clone();
    Arc::new(move || {
        let session = crate::durable_store::lock(&session);
        let anchor = session.branch_tip(crate::lane::MAIN).flatten();
        let on_path: HashSet<String> = session
            .branch_entries(crate::lane::MAIN)
            .unwrap_or_default()
            .iter()
            .filter_map(|entry| entry["id"].as_str().map(str::to_string))
            .collect();
        Branch {
            anchor,
            on_branch: Some(Arc::new(move |anchor: &str| on_path.contains(anchor))),
        }
    })
}
