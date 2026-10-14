//! The built-in inbox (spec §6): queued submissions wait in `pi.inbox` until a boundary places them. Port of upstream
//! `harness/inbox.ts`.

use chord_delta::{Draft, Seg};
use durable::{ConversationId, EntryDraft, EntryHead, EntryId, SubmissionId, SubmissionSettlement, Tx};
use serde_json::{Value, json};

use super::docs::INBOX;
use super::util::{d, owner};
use crate::scheduler::{HarnessError, Result};

/// How a boundary selects queued items of one mode: the first one, or all of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QueueMode {
    #[default]
    OneAtATime,
    All,
}

/// The settings a boundary reads, on the Session line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueModes {
    pub steering: QueueMode,
    pub follow_up: QueueMode,
    /// At `final`, follow-ups wait while steers are placed (see `Settings::follow_up_after_steer`).
    pub follow_up_after_steer: bool,
}

/// Where a boundary runs: after a tool round, or at the end of a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum At {
    PostTools,
    Final,
}

/// What a boundary read before the commit's first table write, and the newest head it has seen so far.
pub struct Boundary {
    pub conversation_id: ConversationId,
    pub inbox: Draft,
    pub modes: QueueModes,
    /// Start of the active range, the newest head marker's `head`; advanced by heads written in this commit.
    pub head: Option<EntryId>,
}

/// Selected user items, in ID order, and whether a `head: "self"` write (a reset) was placed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoundaryResult {
    pub users: Vec<SubmissionId>,
    pub reset: bool,
}

fn items_path() -> Vec<Seg> {
    vec![Seg::from("items")]
}

/// The queued items of an inbox draft.
pub(crate) fn items(inbox: &Draft) -> Result<Vec<Value>> {
    Ok(match d(inbox.get(&items_path()))? {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    })
}

fn item_id(item: &Value) -> SubmissionId {
    SubmissionId(item["id"].as_u64().unwrap_or_default())
}

/// The `EntryDraft` a queued write stores as plain JSON.
pub fn entry_draft(entry: &Value) -> Result<EntryDraft> {
    let kind = entry["kind"]
        .as_str()
        .ok_or_else(|| HarnessError::msg("A write entry needs a kind"))?;
    let head = match entry.get("head") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text == "self" => Some(EntryHead::SelfId),
        Some(value) => {
            Some(EntryHead::Entry(EntryId(value.as_u64().ok_or_else(|| {
                HarnessError::msg("A write entry's head is an entry ID or \"self\"")
            })?)))
        }
    };
    let list = |key: &str| -> Option<Vec<Value>> {
        match entry.get(key) {
            Some(Value::Array(values)) => Some(values.clone()),
            _ => None,
        }
    };
    Ok(EntryDraft {
        kind: kind.to_owned(),
        model: list("model"),
        data: entry.get("data").cloned(),
        head,
        edits: list("edits"),
    })
}

/// Read what a boundary needs. Table reads must precede the commit's first table write, so callers prepare the
/// boundary at the start of their commit.
pub fn prepare_boundary(tx: &mut Tx<'_>, conversation_id: ConversationId, modes: QueueModes) -> Result<Boundary> {
    let head = tx.latest_head_marker(conversation_id)?.and_then(|marker| marker.head);
    let inbox = tx.doc(&INBOX, owner(conversation_id))?;
    Ok(Boundary {
        conversation_id,
        inbox,
        modes,
        head,
    })
}

/// Whether a head write targets an entry before the active range, so placing it would bring back cut history.
pub fn is_stale(boundary: &Boundary, entry: &EntryDraft) -> bool {
    matches!((entry.head, boundary.head), (Some(EntryHead::Entry(target)), Some(head)) if target < head)
}

/// A user entry for queued or submitted input.
pub(crate) fn user_entry(content: &Value, now: u64) -> EntryDraft {
    EntryDraft {
        model: Some(vec![json!({ "role": "user", "content": content, "timestamp": now })]),
        ..EntryDraft::new(super::entries::USER)
    }
}

/// Place the queued items a boundary selects (spec §6): every write, the first or all steers, and at `final` the
/// first or all follow-ups. A selected reset turns a `postTools` boundary into `final`. Writes are placed first and
/// user items after them, each in ID order; a stale write settles `unanswered` with `stale`. Selected and stale items
/// are removed positionally.
pub fn apply_boundary(tx: &mut Tx<'_>, boundary: &mut Boundary, at: At, now: u64) -> Result<BoundaryResult> {
    let items = items(&boundary.inbox)?;
    let mode = |item: &Value| item["mode"].as_str().unwrap_or_default().to_owned();
    let reset = items
        .iter()
        .any(|item| mode(item) == "write" && item["entry"]["head"] == "self");
    let last = at == At::Final || reset;
    let pick = |wanted: &str, queue: QueueMode| -> Vec<usize> {
        let indexes: Vec<usize> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| mode(item) == wanted)
            .map(|(index, _)| index)
            .collect();
        match queue {
            QueueMode::All => indexes,
            QueueMode::OneAtATime => indexes.into_iter().take(1).collect(),
        }
    };
    let writes: Vec<usize> = items
        .iter()
        .enumerate()
        .filter(|(_, item)| mode(item) == "write")
        .map(|(index, _)| index)
        .collect();
    let mut users = pick("steer", boundary.modes.steering);
    if last && !(boundary.modes.follow_up_after_steer && !users.is_empty()) {
        users.extend(pick("followUp", boundary.modes.follow_up));
    }
    users.sort_unstable();

    for &index in &writes {
        let item = &items[index];
        let draft = entry_draft(&item["entry"])?;
        if is_stale(boundary, &draft) {
            tx.settle_submission(
                item_id(item),
                SubmissionSettlement::Unanswered {
                    reason: "stale".into(),
                    detail: None,
                },
            );
            continue;
        }
        let head = draft.head;
        let entry = tx.append_entry(boundary.conversation_id, draft)?;
        if let Some(head) = head {
            boundary.head = Some(match head {
                EntryHead::SelfId => entry.id,
                EntryHead::Entry(target) => target,
            });
        }
        tx.place_submission(item_id(item), entry.id);
    }
    let mut placed = Vec::new();
    for &index in &users {
        let item = &items[index];
        let entry = tx.append_entry(boundary.conversation_id, user_entry(&item["content"], now))?;
        tx.place_submission(item_id(item), entry.id);
        placed.push(item_id(item));
    }
    let mut removed: Vec<usize> = writes.into_iter().chain(users).collect();
    removed.sort_unstable_by(|a, b| b.cmp(a));
    for index in removed {
        d(boundary.inbox.splice(&items_path(), index, 1, Vec::new()))?;
    }
    Ok(BoundaryResult { users: placed, reset })
}

/// Remove a withdrawn submission's item; the caller settles the submission.
pub fn remove_inbox_item(tx: &mut Tx<'_>, conversation_id: ConversationId, id: SubmissionId) -> Result<()> {
    let inbox = tx.doc(&INBOX, owner(conversation_id))?;
    if let Some(index) = items(&inbox)?.iter().position(|item| item_id(item) == id) {
        d(inbox.splice(&items_path(), index, 1, Vec::new()))?;
    }
    Ok(())
}

/// Withdraw every queued input of a conversation, as `Conversation::abort` and abort cascades do: each settles
/// `unanswered` with `aborted` and leaves the inbox; queued writes stay for later placement.
pub fn withdraw_queued_inputs(tx: &mut Tx<'_>, conversation_id: ConversationId) -> Result<()> {
    let inbox = tx.doc(&INBOX, owner(conversation_id))?;
    let items = items(&inbox)?;
    for index in (0..items.len()).rev() {
        let item = &items[index];
        if item["mode"] == "write" {
            continue;
        }
        tx.settle_submission(
            item_id(item),
            SubmissionSettlement::Unanswered {
                reason: "aborted".into(),
                detail: None,
            },
        );
        d(inbox.splice(&items_path(), index, 1, Vec::new()))?;
    }
    Ok(())
}
