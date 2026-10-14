//! Model-owned context (`ctx.*`) on every lane, as the TypeScript worker serves it: one `ContextControl` whose
//! host module answers `ctx.*` from any lane's kernel, each lane editing its own conversation branch
//! (`#lane(caller.lane)` in ultron/context-control.ts). The root lane's branch is the session's `main` branch
//! (core::context_lane); a subagent's lane (`ultron.rlm-child.*`, a typed agent's lane, another root
//! conversation's `ultron.conversation.<id>`) is its child conversation in the durable runtime.
//!
//! On a child conversation the branch is the conversation's active entries as Pi entries: each message an entry
//! whose id is the durable entry id (`<id>.<n>` when an entry holds several), a compaction or reset head marker a
//! `compaction` entry, and the context entries `ctx` appends (custom entries) under their own ids. An appended
//! custom entry is a durable write (`ultron.custom`): placed at once when the conversation is idle, else at its
//! running turn's next boundary (TypeScript's `appendCustomEntry` with a `write` inbox item). A note carries its
//! projected message as the entry's model message (`CONTEXT_ENTRY_PROJECTORS`, then `convertToLlm`), and a context
//! edit carries the durable edits (`omit` / `replace` of the targeted entries) that make the conversation's
//! derived context equal `applyContextEdits` of the branch, so the lane's next request, and nothing else, sees it.
//! Collapse on return stays on the root lane, as in TypeScript; the extension event of an edit
//! (`ultron:context_edit`) fires once, when the edit lands on the child's branch.

use super::context_lane::RootBranches;
use crate::durable_store::{SharedSession, lock as lock_session};
use crate::subagents::{CONVERSATION_LANE_PREFIX, DurableChildren};
use durable::{ConversationId, EntryRecord, SubmissionId};
use extensions::ExtensionHost;
use harness::chat::SubmissionDraft;
use kernel_apis::ContextControl;
use kernel_apis::context_control::{
    CONTEXT_EDIT_CUSTOM_TYPE, CONTEXT_EDIT_EVENT, CONTEXT_NOTE_CUSTOM_TYPE, CONTEXT_OMITTED_CUSTOM_TYPE,
    ContextBranches, apply_context_edits, project_note,
};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

/// The durable entry kind of a custom entry `ctx` appended to a child conversation.
pub const CUSTOM_KIND: &str = "ultron.custom";

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The custom entry an `ultron.custom` durable entry stores (as JSON text: a write drops null fields, and an
/// omission is `replacement: null`), with the lane it was appended on.
fn stored_custom(entry: &EntryRecord) -> Option<(String, Value)> {
    if entry.kind != CUSTOM_KIND {
        return None;
    }
    let data = entry.data.as_ref()?;
    let custom: Value = serde_json::from_str(data["entry"].as_str()?).ok()?;
    Some((data["lane"].as_str()?.to_string(), custom))
}

/// A child conversation's visible entries as the Pi branch `ContextControl` reads, in append order:
/// `(entry, durable entry id)`. A compaction (or reset) is a `compaction` entry where it was appended, so the window
/// `ctx` edits starts after it and the notes and pins before it are what a compaction cut off.
pub(crate) fn pi_entries(entries: &[EntryRecord]) -> Vec<(Value, u64)> {
    let mut out = Vec::new();
    for entry in entries {
        let id = entry.id.0;
        if let Some((_, custom)) = stored_custom(entry) {
            out.push((custom, id));
            continue;
        }
        if entry.head.is_some() {
            out.push((json!({"id": id.to_string(), "type": "compaction"}), id));
            continue;
        }
        if entry.kind == harness::chat::entries::SYSTEM {
            continue;
        }
        let messages = entry.model.clone().unwrap_or_default();
        let several = messages.len() > 1;
        for (index, message) in messages.into_iter().enumerate() {
            let pi_id = if several {
                format!("{id}.{index}")
            } else {
                id.to_string()
            };
            let timestamp = message["timestamp"].clone();
            out.push((
                json!({"id": pi_id, "parentId": null, "type": "message", "timestamp": timestamp, "message": message}),
                id,
            ));
        }
    }
    out
}

/// What a new context edit changes, as durable edits: for every placed entry it changes (`durable[i]` the durable
/// entry of `path[i]`), `omit` when nothing of it stays visible, else `replace` with what does, as the model sees it;
/// and for each note still waiting for its turn's boundary (`durable[i]` `None`) that it changes, the index in `path`
/// and the messages the note must carry instead (none: hidden).
pub(crate) struct Translated {
    pub edits: Vec<Value>,
    pub waiting: Vec<(usize, Vec<Value>)>,
}

/// [`Translated`] of `path` (the branch with the new edit appended): `applyContextEdits` over the window after the
/// newest compaction.
pub(crate) fn durable_edits(path: &[Value], durable: &[Option<u64>]) -> Translated {
    let start = path
        .iter()
        .rposition(|entry| entry["type"] == "compaction")
        .map_or(0, |i| i + 1);
    let applied = apply_context_edits(&path[start..]);
    let mut groups: Vec<(u64, bool, Vec<Value>)> = Vec::new();
    let mut waiting = Vec::new();
    for (offset, applied) in applied.iter().enumerate() {
        let omitted = applied.entry["customType"] == CONTEXT_OMITTED_CUSTOM_TYPE;
        let visible: Vec<Value> = match applied.entry["type"].as_str() {
            _ if omitted => Vec::new(),
            Some("message") => vec![applied.entry["message"].clone()],
            Some("custom") if applied.entry["customType"] == CONTEXT_NOTE_CUSTOM_TYPE => project_note(&applied.entry),
            _ => Vec::new(),
        };
        let Some(target) = durable[start + offset] else {
            let note = path[start + offset]["customType"] == CONTEXT_NOTE_CUSTOM_TYPE;
            if note && applied.changed {
                waiting.push((start + offset, session_journal::context::convert_to_llm(&visible)));
            }
            continue;
        };
        match groups.last_mut() {
            Some((id, changed, messages)) if *id == target => {
                *changed |= applied.changed;
                messages.extend(visible);
            }
            _ => groups.push((target, applied.changed, visible)),
        }
    }
    let edits = groups
        .into_iter()
        .filter(|(_, changed, _)| *changed)
        .map(|(target, _, messages)| {
            if messages.is_empty() {
                json!({"target": target, "action": "omit"})
            } else {
                json!({"target": target, "action": "replace",
                    "messages": session_journal::context::convert_to_llm(&messages)})
            }
        })
        .collect();
    Translated { edits, waiting }
}

/// A custom entry appended while its lane's turn ran: its lane, Pi entry and write submission.
struct Queued {
    lane: String,
    entry: Value,
    /// `None`: placed by the harness itself (a trim declining a compaction).
    submission: Option<SubmissionId>,
}

/// The durable write of the custom entry `custom` appended on `lane`, carrying `model` (a note's message).
fn custom_write(lane: &str, custom: &Value, model: Option<Vec<Value>>) -> Value {
    let mut stored = json!({
        "kind": CUSTOM_KIND,
        "data": {"lane": lane, "entry": custom.to_string()},
    });
    if let Some(model) = model.filter(|m| !m.is_empty()) {
        stored["model"] = model.into();
    }
    stored
}

/// `ctx` on child conversations.
pub(crate) struct ChildContext {
    /// Weak: the runtime's commit observer holds this, and the children hold the runtime.
    children: Weak<DurableChildren>,
    session: SharedSession,
    /// Custom entries appended while their lane's turn ran, until its branch shows them.
    queued: Mutex<Vec<Queued>>,
    /// Where a landed context edit is reported: the module and the extensions' event bus.
    observers: OnceLock<(Weak<ContextControl>, Option<Arc<ExtensionHost>>)>,
    /// While `before_compaction` runs for a lane: the writes its entries become, for the harness to place.
    capture: Mutex<Option<(String, Vec<Value>)>>,
}

impl ChildContext {
    pub(crate) fn new(children: Arc<DurableChildren>, session: SharedSession) -> ChildContext {
        ChildContext {
            children: Arc::downgrade(&children),
            session,
            queued: Mutex::new(Vec::new()),
            observers: OnceLock::new(),
            capture: Mutex::new(None),
        }
    }

    /// Report landed entries to `context` and publish its edit events to `extensions`.
    pub(crate) fn observe(&self, context: &Arc<ContextControl>, extensions: Option<Arc<ExtensionHost>>) {
        let _ = self.observers.set((Arc::downgrade(context), extensions));
    }

    /// A durable entry committed: a context entry of a child lane landed on its branch (`entry_added`).
    pub(crate) fn committed(&self, entry: &EntryRecord) {
        let Some((lane, custom)) = stored_custom(entry) else {
            return;
        };
        let Some((context, extensions)) = self.observers.get() else {
            return;
        };
        let Some(context) = context.upgrade() else {
            return;
        };
        if let Some(edit) = context.entry_added(&lane, &custom)
            && let Some(host) = extensions
        {
            host.publish(CONTEXT_EDIT_EVENT, &edit);
        }
    }

    fn context(&self) -> Option<Arc<ContextControl>> {
        self.observers.get()?.0.upgrade()
    }

    /// The lane of a child conversation.
    fn lane_of(&self, conversation: ConversationId) -> Option<String> {
        self.children.upgrade()?.lane_of(conversation)
    }

    /// Harness `before_compaction` of a child conversation's blocking threshold compaction (TS `before_compaction`
    /// with reason `threshold`): the writes of the trim that declines it, or none to compact.
    pub(crate) fn before_compaction(&self, conversation: ConversationId, tokens: u64, threshold: u64) -> Vec<Value> {
        let (Some(context), Some(lane)) = (self.context(), self.lane_of(conversation)) else {
            return Vec::new();
        };
        *lock(&self.capture) = Some((lane.clone(), Vec::new()));
        let tokens = i64::try_from(tokens).unwrap_or(i64::MAX);
        let threshold = i64::try_from(threshold).unwrap_or(i64::MAX);
        let declined = context.before_compaction(&lane, "threshold", tokens, Some(threshold), 0);
        let captured = lock(&self.capture).take().map(|(_, writes)| writes).unwrap_or_default();
        if declined == Ok(true) { captured } else { Vec::new() }
    }

    /// Harness `transform_context` of a child conversation's request: after a compaction summary, the notes and
    /// pinned items it cut off (lanes that used ctx only), as the user message `convertToLlm` makes of the custom one.
    pub(crate) fn transform_request(&self, conversation: ConversationId, messages: &mut Vec<ai::Message>) {
        let (Some(context), Some(lane)) = (self.context(), self.lane_of(conversation)) else {
            return;
        };
        let Some(summary) = self.summary_of(conversation) else {
            return;
        };
        let Some(index) = messages.iter().position(|m| *m == summary) else {
            return;
        };
        let marker = [json!({"role": "compactionSummary"})];
        let Ok(Some(with)) = context.transform_context(&lane, &marker) else {
            return;
        };
        let Some(kept) = with.get(1) else {
            return;
        };
        let converted = session_journal::context::convert_to_llm(std::slice::from_ref(kept));
        if let Some(message) = converted
            .into_iter()
            .find_map(|m| serde_json::from_value::<ai::Message>(m).ok())
        {
            messages.insert(index + 1, message);
        }
    }

    /// The summary message of a conversation's newest compaction, when its context starts at one.
    fn summary_of(&self, conversation: ConversationId) -> Option<ai::Message> {
        let chat = self.children.upgrade()?.chat()?;
        let view = chat.conversation(conversation).ok()??.context(None).ok()?;
        let head = view.head?;
        if head.kind != harness::chat::entries::COMPACTION {
            return None;
        }
        let message = head.model?.into_iter().next()?;
        serde_json::from_value(message).ok()
    }

    fn conversation(&self, lane: &str) -> Result<harness::chat::Conversation, String> {
        let children = self.children.upgrade().ok_or("ctx: the session is closed")?;
        let chat = children.chat().ok_or("ctx: the durable runtime is not open")?;
        let id = match lane.strip_prefix(CONVERSATION_LANE_PREFIX) {
            Some(id) => id.parse().ok().map(ConversationId),
            None => children.conversation_of_lane(lane),
        }
        .ok_or_else(|| format!("ctx: lane {lane} has no conversation"))?;
        chat.conversation(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("ctx: lane {lane} has no conversation"))
    }

    /// The lane's branch with the durable entry of each, dropping queued entries that landed.
    fn branch(&self, lane: &str) -> Result<(harness::chat::Conversation, Vec<(Value, u64)>), String> {
        let conversation = self.conversation(lane)?;
        let entries = pi_entries(&conversation.entries().map_err(|e| e.to_string())?);
        let landed: HashSet<&str> = entries.iter().filter_map(|(entry, _)| entry["id"].as_str()).collect();
        lock(&self.queued).retain(|q| !q.entry["id"].as_str().is_some_and(|id| landed.contains(id)));
        Ok((conversation, entries))
    }

    fn entries(&self, lane: &str) -> Result<Vec<Value>, String> {
        Ok(self.branch(lane)?.1.into_iter().map(|(entry, _)| entry).collect())
    }

    fn append_custom(&self, lane: &str, custom_type: &str, data: &Value) -> Result<String, String> {
        let (id, timestamp) = {
            let session = lock_session(&self.session);
            let id = session_journal::lock_clock(&session.clock()).next_id();
            (id, crate::lane::now_ms())
        };
        let custom = json!({"id": id, "parentId": null, "type": "custom", "customType": custom_type, "data": data,
            "timestamp": timestamp});
        // An edit of a note still waiting for the boundary withdraws that note's write and queues it again carrying
        // what stays visible of it; one that landed meanwhile is edited as placed, on the next attempt.
        let mut attempts = 0;
        let (conversation, stored) = loop {
            attempts += 1;
            let (conversation, entries) = self.branch(lane)?;
            let mut stored = custom_write(lane, &custom, None);
            if custom_type == CONTEXT_NOTE_CUSTOM_TYPE {
                stored = custom_write(
                    lane,
                    &custom,
                    Some(session_journal::context::convert_to_llm(&project_note(&custom))),
                );
            }
            if custom_type != CONTEXT_EDIT_CUSTOM_TYPE {
                break (conversation, stored);
            }
            let mut path: Vec<Value> = Vec::new();
            let mut durable: Vec<Option<u64>> = Vec::new();
            let mut submissions: Vec<Option<SubmissionId>> = Vec::new();
            for (entry, target) in entries {
                path.push(entry);
                durable.push(Some(target));
                submissions.push(None);
            }
            for queued in lock(&self.queued).iter().filter(|q| q.lane == lane) {
                path.push(queued.entry.clone());
                durable.push(None);
                submissions.push(queued.submission);
            }
            path.push(custom.clone());
            durable.push(None);
            submissions.push(None);
            let translated = durable_edits(&path, &durable);
            if !translated.edits.is_empty() {
                stored["edits"] = translated.edits.into();
            }
            let mut withdrawn = true;
            for (index, model) in translated.waiting {
                let Some(submission) = submissions[index] else {
                    continue;
                };
                let chat = conversation.chat();
                let aborted = match chat.submission(submission).map_err(|e| e.to_string())? {
                    Some(handle) => handle.abort().map_err(|e| e.to_string())?,
                    None => harness::chat::SubmissionAbort::NotFound,
                };
                if aborted != harness::chat::SubmissionAbort::Aborted {
                    withdrawn = false;
                    continue;
                }
                let note = path[index].clone();
                let again = conversation
                    .submit(SubmissionDraft::write(custom_write(lane, &note, Some(model))))
                    .map_err(|e| e.to_string())?;
                if let Some(queued) = lock(&self.queued).iter_mut().find(|q| q.submission == Some(submission)) {
                    queued.submission = Some(again.id);
                }
            }
            if withdrawn || attempts == 3 {
                break (conversation, stored);
            }
        };
        // Held across the write: a write admitted at once lands (and the next read drops it) only after the push.
        let mut queued = lock(&self.queued);
        let submission = match lock(&self.capture).as_mut() {
            // A trim declining a compaction: the harness places the entry with the request it prepares.
            Some((capturing, captured)) if capturing == lane => {
                captured.push(stored);
                None
            }
            _ => Some(
                conversation
                    .submit(SubmissionDraft::write(stored))
                    .map_err(|e| e.to_string())?
                    .id,
            ),
        };
        queued.push(Queued {
            lane: lane.to_string(),
            entry: custom,
            submission,
        });
        Ok(id)
    }

    fn tip_id(&self, lane: &str) -> Result<Option<String>, String> {
        Ok(self
            .branch(lane)?
            .1
            .last()
            .and_then(|(entry, _)| entry["id"].as_str().map(str::to_string)))
    }
}

/// The branches `ctx` edits: the root lane's session branch, every other lane's child conversation.
pub(crate) struct LaneBranches {
    pub root: Arc<RootBranches>,
    pub children: Arc<ChildContext>,
}

impl ContextBranches for LaneBranches {
    fn entries(&self, lane: &str) -> Result<Vec<Value>, String> {
        if lane == super::LANE {
            self.root.entries(lane)
        } else {
            self.children.entries(lane)
        }
    }

    fn append_custom(&self, lane: &str, custom_type: &str, data: &Value) -> Result<String, String> {
        if lane == super::LANE {
            self.root.append_custom(lane, custom_type, data)
        } else {
            self.children.append_custom(lane, custom_type, data)
        }
    }

    fn tip_id(&self, lane: &str) -> Result<Option<String>, String> {
        if lane == super::LANE {
            self.root.tip_id(lane)
        } else {
            self.children.tip_id(lane)
        }
    }
}
