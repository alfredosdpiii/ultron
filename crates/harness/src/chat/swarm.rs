//! The session's shared world for many agents (rewriteplan "Swarm design items"), as durable Session documents behind
//! host operations:
//!
//! - `ultron.agents`: a small registry of what each agent holds (topics, files, recent work), so a question routes to
//!   the agent with the context instead of through the root (`swarm.register`, `swarm.agents`, `swarm.route`).
//! - `ultron.mail`: durable peer messages between agents of the session (`swarm.send`, `swarm.inbox`); a message to a
//!   persistent child can also wake it as a follow-up input.
//! - `ultron.board`: the session's task board (`board.add`, `board.claim`, `board.update`, `board.list`).
//! - `ultron.notes`: the session's notes document, with optimistic versions (`notes.read`, `notes.append`,
//!   `notes.write`).
//!
//! Every change is one commit, so concurrent agents never lose each other's writes and everything survives restarts.

use std::sync::LazyLock;

use chord_delta::Seg;
use durable::{ConversationId, Doc, DocDefinition, DocSemantics, Owner, Tx};
use serde_json::{Map, Value, json};

use super::util::d;
use crate::scheduler::{HarnessError, Result};

/// Recent work an agent keeps in the registry.
pub const MAX_RECENT: usize = 10;
/// Messages kept per mailbox; the oldest read ones go first.
pub const MAX_MAIL: usize = 200;

fn session_doc(kind: &str, initial: Value) -> Doc {
    DocDefinition::new(kind, 1, DocSemantics::Session, move |_| initial.clone())
        .define()
        .expect("valid swarm document")
}

pub static AGENTS: LazyLock<Doc> = LazyLock::new(|| session_doc("ultron.agents", json!({ "agents": {} })));
pub static MAIL: LazyLock<Doc> = LazyLock::new(|| session_doc("ultron.mail", json!({ "boxes": {}, "next": 1 })));
pub static BOARD: LazyLock<Doc> = LazyLock::new(|| session_doc("ultron.board", json!({ "tasks": [], "next": 1 })));
pub static NOTES: LazyLock<Doc> = LazyLock::new(|| session_doc("ultron.notes", json!({ "text": "", "version": 0 })));

fn path(segments: &[&str]) -> Vec<Seg> {
    segments.iter().map(|segment| Seg::from(*segment)).collect()
}

fn msg(text: impl Into<String>) -> HarnessError {
    HarnessError::msg(text)
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// An agent's registry entry, merged: topics and files are sets, recent work is the newest [`MAX_RECENT`] items.
pub fn register(
    tx: &mut Tx<'_>,
    name: &str,
    conversation: Option<ConversationId>,
    child: Option<String>,
    payload: &Map<String, Value>,
    now: u64,
) -> Result<Value> {
    if name.trim().is_empty() {
        return Err(msg("An agent needs a non-empty name"));
    }
    let doc = tx.doc(&AGENTS, Owner::Session)?;
    let current =
        d(doc.get(&path(&["agents", name])))?.unwrap_or_else(|| json!({ "topics": [], "files": [], "recent": [] }));
    let mut entry = current.clone();
    let merge = |field: &str, entry: &mut Value| {
        let mut values = string_list(entry.get(field));
        for value in string_list(payload.get(field)) {
            if !values.contains(&value) {
                values.push(value);
            }
        }
        entry[field] = json!(values);
    };
    merge("topics", &mut entry);
    merge("files", &mut entry);
    let mut recent = string_list(entry.get("recent"));
    if let Some(work) = payload
        .get("work")
        .and_then(Value::as_str)
        .filter(|work| !work.is_empty())
    {
        recent.push(work.to_owned());
    }
    let start = recent.len().saturating_sub(MAX_RECENT);
    entry["recent"] = json!(recent[start..]);
    if let Some(conversation) = conversation {
        entry["conversation"] = json!(conversation.0);
    }
    if let Some(child) = child {
        entry["child"] = json!(child);
    }
    if let Some(summary) = payload.get("summary").and_then(Value::as_str) {
        entry["summary"] = json!(summary);
    }
    entry["updatedAt"] = json!(now);
    if entry != current {
        d(doc.set(&path(&["agents", name]), entry.clone()))?;
    }
    Ok(json!({ "name": name, "agent": entry }))
}

/// Every registered agent, by name.
pub fn agents(tx: &mut Tx<'_>) -> Result<Value> {
    let doc = tx.doc(&AGENTS, Owner::Session)?;
    Ok(d(doc.get(&path(&["agents"])))?.unwrap_or(json!({})))
}

/// The name an agent registered for `conversation`, if any.
pub fn name_of(tx: &mut Tx<'_>, conversation: ConversationId) -> Result<Option<String>> {
    let all = agents(tx)?;
    Ok(all.as_object().and_then(|agents| {
        agents
            .iter()
            .find(|(_, entry)| entry["conversation"].as_u64() == Some(conversation.0))
            .map(|(name, _)| name.clone())
    }))
}

/// Words too common to route by.
const STOPWORDS: [&str; 24] = [
    "the", "and", "for", "with", "this", "that", "from", "into", "what", "who", "why", "does", "how", "are", "was",
    "has", "have", "not", "but", "you", "your", "can", "its", "any",
];

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.' && c != '/')
        .map(|word| word.trim_matches('.').to_lowercase())
        .filter(|word| word.chars().count() >= 3 && !STOPWORDS.contains(&word.as_str()))
        .collect()
}

/// Agents ranked for `query`: one point per query word found in an agent's topics or files (weight 2) or recent work
/// and summary (weight 1). Agents with no match are left out; ties keep registry order.
pub fn route(tx: &mut Tx<'_>, query: &str) -> Result<Value> {
    let all = agents(tx)?;
    let query = words(query);
    let mut ranked: Vec<(u64, String, Value)> = Vec::new();
    for (name, entry) in all.as_object().into_iter().flatten() {
        let strong: Vec<String> = string_list(entry.get("topics"))
            .into_iter()
            .chain(string_list(entry.get("files")))
            .flat_map(|item| words(&item))
            .collect();
        let weak: Vec<String> = string_list(entry.get("recent"))
            .into_iter()
            .chain(entry["summary"].as_str().map(str::to_owned))
            .flat_map(|item| words(&item))
            .collect();
        let score: u64 = query
            .iter()
            .map(|word| 2 * u64::from(strong.contains(word)) + u64::from(weak.contains(word)))
            .sum();
        if score > 0 {
            ranked.push((score, name.clone(), entry.clone()));
        }
    }
    ranked.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    Ok(json!(
        ranked
            .into_iter()
            .map(|(score, name, entry)| json!({ "name": name, "score": score, "agent": entry }))
            .collect::<Vec<_>>()
    ))
}

/// Deliver a message to `to`'s mailbox; returns its ID.
pub fn send(tx: &mut Tx<'_>, from: &str, to: &str, text: &str, now: u64) -> Result<u64> {
    if to.trim().is_empty() || text.is_empty() {
        return Err(msg("swarm.send needs a recipient and text"));
    }
    let doc = tx.doc(&MAIL, Owner::Session)?;
    let id = d(doc.get(&path(&["next"])))?
        .and_then(|next| next.as_u64())
        .unwrap_or(1);
    d(doc.set(&path(&["next"]), json!(id + 1)))?;
    if d(doc.get(&path(&["boxes", to])))?.is_none() {
        d(doc.set(&path(&["boxes", to]), json!([])))?;
    }
    let message = json!({ "id": id, "from": from, "text": text, "at": now, "read": false });
    d(doc.push(&path(&["boxes", to]), vec![message]))?;
    // Bound the box: drop the oldest read messages first, then the oldest.
    let messages = d(doc.get(&path(&["boxes", to])))?.unwrap_or(json!([]));
    let count = messages.as_array().map_or(0, Vec::len);
    if count > MAX_MAIL {
        let drop = messages
            .as_array()
            .unwrap()
            .iter()
            .position(|message| message["read"] == true)
            .unwrap_or(0);
        d(doc.splice(&path(&["boxes", to]), drop, 1, Vec::new()))?;
    }
    Ok(id)
}

/// `name`'s messages (only unread ones unless `all`); with `mark`, they become read in the same commit.
pub fn inbox(tx: &mut Tx<'_>, name: &str, all: bool, mark: bool) -> Result<Value> {
    let doc = tx.doc(&MAIL, Owner::Session)?;
    let messages = d(doc.get(&path(&["boxes", name])))?.unwrap_or(json!([]));
    let mut out = Vec::new();
    for (index, message) in messages.as_array().into_iter().flatten().enumerate() {
        let unread = message["read"] == false;
        if all || unread {
            out.push(message.clone());
        }
        if mark && unread {
            d(doc.set(
                &[Seg::from("boxes"), Seg::from(name), Seg::from(index), Seg::from("read")],
                json!(true),
            ))?;
        }
    }
    Ok(json!(out))
}

fn board_index(tx: &mut Tx<'_>, id: u64) -> Result<(chord_delta::Draft, usize, Value)> {
    let doc = tx.doc(&BOARD, Owner::Session)?;
    let tasks = d(doc.get(&path(&["tasks"])))?.unwrap_or(json!([]));
    let (index, task) = tasks
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .find(|(_, task)| task["id"].as_u64() == Some(id))
        .map(|(index, task)| (index, task.clone()))
        .ok_or_else(|| msg(format!("No board task {id}")))?;
    Ok((doc, index, task))
}

fn task_path(index: usize, field: &str) -> Vec<Seg> {
    vec![Seg::from("tasks"), Seg::from(index), Seg::from(field)]
}

/// Add a task to the board; returns it.
pub fn board_add(tx: &mut Tx<'_>, by: &str, title: &str, detail: Option<&str>, now: u64) -> Result<Value> {
    if title.trim().is_empty() {
        return Err(msg("A board task needs a title"));
    }
    let doc = tx.doc(&BOARD, Owner::Session)?;
    let id = d(doc.get(&path(&["next"])))?
        .and_then(|next| next.as_u64())
        .unwrap_or(1);
    d(doc.set(&path(&["next"]), json!(id + 1)))?;
    let mut task = json!({ "id": id, "title": title, "status": "open", "by": by, "notes": [], "updatedAt": now });
    if let Some(detail) = detail {
        task["detail"] = json!(detail);
    }
    d(doc.push(&path(&["tasks"]), vec![task.clone()]))?;
    Ok(task)
}

/// Claim an open task for `owner`; a task someone else claimed is refused.
pub fn board_claim(tx: &mut Tx<'_>, owner: &str, id: u64, now: u64) -> Result<Value> {
    let (doc, index, mut task) = board_index(tx, id)?;
    match task["status"].as_str() {
        Some("open") => {}
        Some("claimed") if task["owner"] == owner => return Ok(task),
        Some("claimed") => {
            return Err(msg(format!(
                "Board task {id} is claimed by {}",
                task["owner"].as_str().unwrap_or("another agent")
            )));
        }
        _ => {
            return Err(msg(format!(
                "Board task {id} is {}",
                task["status"].as_str().unwrap_or("closed")
            )));
        }
    }
    d(doc.set(&task_path(index, "status"), json!("claimed")))?;
    d(doc.set(&task_path(index, "owner"), json!(owner)))?;
    d(doc.set(&task_path(index, "updatedAt"), json!(now)))?;
    task["status"] = json!("claimed");
    task["owner"] = json!(owner);
    task["updatedAt"] = json!(now);
    Ok(task)
}

/// Change a task's status (`open`, `claimed`, `done`, `dropped`) and/or append a note.
pub fn board_update(
    tx: &mut Tx<'_>,
    by: &str,
    id: u64,
    status: Option<&str>,
    note: Option<&str>,
    now: u64,
) -> Result<Value> {
    let (doc, index, mut task) = board_index(tx, id)?;
    if let Some(status) = status {
        if !["open", "claimed", "done", "dropped"].contains(&status) {
            return Err(msg(format!("Unknown board status {status}")));
        }
        d(doc.set(&task_path(index, "status"), json!(status)))?;
        task["status"] = json!(status);
        if status == "open" {
            d(doc.delete(&task_path(index, "owner")))?;
            task.as_object_mut().map(|task| task.remove("owner"));
        }
    }
    if let Some(note) = note {
        let entry = json!({ "by": by, "text": note, "at": now });
        d(doc.push(&task_path(index, "notes"), vec![entry.clone()]))?;
        if let Some(notes) = task["notes"].as_array_mut() {
            notes.push(entry);
        }
    }
    d(doc.set(&task_path(index, "updatedAt"), json!(now)))?;
    task["updatedAt"] = json!(now);
    Ok(task)
}

pub fn board_list(tx: &mut Tx<'_>, status: Option<&str>) -> Result<Value> {
    let doc = tx.doc(&BOARD, Owner::Session)?;
    let tasks = d(doc.get(&path(&["tasks"])))?.unwrap_or(json!([]));
    Ok(json!(
        tasks
            .as_array()
            .into_iter()
            .flatten()
            .filter(|task| status.is_none_or(|status| task["status"] == status))
            .cloned()
            .collect::<Vec<_>>()
    ))
}

pub fn notes_read(tx: &mut Tx<'_>) -> Result<Value> {
    let doc = tx.doc(&NOTES, Owner::Session)?;
    Ok(json!({
        "text": d(doc.get(&path(&["text"])))?.unwrap_or(json!("")),
        "version": d(doc.get(&path(&["version"])))?.unwrap_or(json!(0)),
    }))
}

/// Append a line to the notes (never conflicts).
pub fn notes_append(tx: &mut Tx<'_>, by: &str, text: &str) -> Result<Value> {
    let doc = tx.doc(&NOTES, Owner::Session)?;
    let current = d(doc.get(&path(&["text"])))?
        .and_then(|text| text.as_str().map(str::to_owned))
        .unwrap_or_default();
    let version = d(doc.get(&path(&["version"])))?
        .and_then(|version| version.as_u64())
        .unwrap_or(0)
        + 1;
    let separator = if current.is_empty() || current.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let line = format!("{current}{separator}[{by}] {text}\n");
    d(doc.set(&path(&["text"]), json!(line)))?;
    d(doc.set(&path(&["version"]), json!(version)))?;
    Ok(json!({ "version": version }))
}

/// Replace the notes; with `expect`, only when the current version is `expect` (a stale writer is refused).
pub fn notes_write(tx: &mut Tx<'_>, text: &str, expect: Option<u64>) -> Result<Value> {
    let doc = tx.doc(&NOTES, Owner::Session)?;
    let version = d(doc.get(&path(&["version"])))?
        .and_then(|version| version.as_u64())
        .unwrap_or(0);
    if let Some(expect) = expect
        && expect != version
    {
        return Err(msg(format!(
            "The notes changed (version {version}, expected {expect}); read them again"
        )));
    }
    d(doc.set(&path(&["text"]), json!(text)))?;
    d(doc.set(&path(&["version"]), json!(version + 1)))?;
    Ok(json!({ "version": version + 1 }))
}
