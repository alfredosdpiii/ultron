//! Model-owned context on the root lane (`ctx.*` and collapse on return, `kernel_apis::context_control`): the
//! branch it reads and appends to is the session's `main` branch.
//!
//! An entry appended while a run is in progress waits for the run's next boundary, as TypeScript's
//! `lane.appendCustomEntry` does (a pending entry with a `write` inbox item): a note placed between a tool call and
//! its result would break the pair the provider needs. It waits durably, as a session value keyed by its entry id (a
//! UUIDv7, so keys sort in append order), and is placed with the next generation's `assistant.ready` transaction or
//! at the run's end; a worker that died with entries waiting places them when the session opens again. Outside a
//! run an entry joins the branch at once.

use super::*;
use kernel_apis::context_control::{CONTEXT_NOTE_CUSTOM_TYPE, ContextBranches, project_note};
use session_journal::context::EntryProjector;

/// Custom entries waiting for the running turn's next boundary: `{customType, data}` by entry id.
pub const CONTEXT_PENDING: &str = "ultron.context.pending";

/// The custom-entry projectors of the root lane's context (`CONTEXT_ENTRY_PROJECTORS`): notes.
pub(super) const PROJECTORS: &[(&str, EntryProjector)] = &[(CONTEXT_NOTE_CUSTOM_TYPE, project_note)];

pub(super) struct RootBranches {
    session: SharedSession,
    /// A run is in progress: appends wait for its next boundary. Held while an append or a placement commits, so
    /// an append never lands between the placed entries.
    running: Mutex<bool>,
}

fn root_only(lane: &str) -> Result<(), String> {
    if lane == LANE {
        Ok(())
    } else {
        Err(format!("ctx edits the root lane's context only (lane {lane})"))
    }
}

fn custom_entry(id: &str, parent: Option<&str>, waiting: &Value) -> Write {
    let mut entry = Map::new();
    entry.insert("id".into(), id.into());
    entry.insert("parentId".into(), parent.map_or(Value::Null, Value::from));
    entry.insert("type".into(), "custom".into());
    entry.insert("customType".into(), waiting["customType"].clone());
    if let Some(data) = waiting.get("data") {
        entry.insert("data".into(), data.clone());
    }
    Write::Entry(entry)
}

/// The waiting entries as writes that place them on the branch after `tip` (and drop them from the waiting
/// values), and the tip after them.
pub(super) fn placement(session: &session_journal::Session, tip: Option<String>) -> (Vec<Write>, Option<String>) {
    let mut writes = Vec::new();
    let mut tip = tip;
    for waiting in session.state().scan_values(CONTEXT_PENDING, "") {
        writes.push(custom_entry(&waiting.key, tip.as_deref(), &waiting.value));
        writes.push(Write::delete(CONTEXT_PENDING, &waiting.key));
        tip = Some(waiting.key.clone());
    }
    (writes, tip)
}

/// How many entries wait for the run's next boundary.
pub(super) fn pending_count(session: &session_journal::Session) -> usize {
    session.state().scan_values(CONTEXT_PENDING, "").len()
}

impl RootBranches {
    pub(super) fn new(session: SharedSession) -> Self {
        RootBranches {
            session,
            running: Mutex::new(false),
        }
    }

    /// A run starts: appends wait for its boundaries.
    pub(super) fn run_started(&self) {
        *lock(&self.running) = true;
    }

    /// Place every waiting entry at the branch's tip now. `ended`: the run is over, later appends join at once.
    pub(super) fn place(&self, ended: bool) -> Result<(), String> {
        let mut running = lock(&self.running);
        if ended {
            *running = false;
        }
        let mut session = lock_session(&self.session);
        let tip = session.branch_tip(LANE).flatten();
        let (mut writes, placed) = placement(&session, tip.clone());
        if writes.is_empty() {
            return Ok(());
        }
        if let Some(placed) = placed.filter(|placed| Some(placed) != tip.as_ref()) {
            writes.push(Write::set(BRANCH_TIP, LANE, placed.into()));
        }
        session.commit(&writes).map(drop)
    }
}

impl ContextBranches for RootBranches {
    fn entries(&self, lane: &str) -> Result<Vec<Value>, String> {
        root_only(lane)?;
        lock_session(&self.session).branch_entries(LANE)
    }

    fn append_custom(&self, lane: &str, custom_type: &str, data: &Value) -> Result<String, String> {
        root_only(lane)?;
        let running = lock(&self.running);
        let mut session = lock_session(&self.session);
        let id = session_journal::lock_clock(&session.clock()).next_id();
        let waiting = json!({"customType": custom_type, "data": data});
        if *running {
            session.commit(&[Write::set(CONTEXT_PENDING, &id, waiting)])?;
        } else {
            let tip = session.branch_tip(LANE).flatten();
            session.commit(&[
                custom_entry(&id, tip.as_deref(), &waiting),
                Write::set(BRANCH_TIP, LANE, id.clone().into()),
            ])?;
        }
        Ok(id)
    }

    fn tip_id(&self, lane: &str) -> Result<Option<String>, String> {
        root_only(lane)?;
        Ok(lock_session(&self.session).branch_tip(LANE).flatten())
    }
}
