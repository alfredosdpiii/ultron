//! Session names: the listing a picker shows (cwd-scoped, newest first, with name and first message), `--resume
//! <name|id>` resolution, and automatic names.
//!
//! A name is the session value `pi.session.name` (what TypeScript's `setName` writes; a v3 file's `session_info`).
//! An automatic name is also recorded in `ultron.module/session-name` as `{ version: 1, automatic: <name> }`; the
//! name is automatic exactly while the two agree, so a name set later by any writer (`-n`, `/name`, the TypeScript
//! product) is the user's and is never replaced.

use crate::repo::{Metadata, SessionRepo, absolute, read_metadata};
use crate::session::{SESSION_NAME, Session};
use crate::state::Write;
use serde_json::{Value, json};
use session_file::js;
use std::path::Path;

pub const AUTO_NAME_NAMESPACE: &str = "ultron.module";
pub const AUTO_NAME_KEY: &str = "session-name";
/// Longest automatic name from a prompt, in characters (before the ellipsis).
pub const AUTO_NAME_MAX: usize = 50;
/// Longest model-made title, in characters.
pub const TITLE_MAX: usize = 60;

/// `normalizeSessionName`: trimmed (JavaScript whitespace), `None` when empty.
pub fn normalize_session_name(value: &str) -> Option<String> {
    let name = js::trim(value);
    (!name.is_empty()).then(|| name.to_string())
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Cut `text` to at most `max` characters at a word boundary; `ellipsis` is appended when it was cut.
fn cut_at_word(text: &str, max: usize, ellipsis: bool) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max + 1).collect();
    let cut = match head.rfind(' ') {
        Some(space) if space > 0 => head[..space].to_string(),
        _ => text.chars().take(max).collect(),
    };
    let cut = cut.trim_end().to_string();
    if ellipsis { format!("{cut}…") } else { cut }
}

fn strip_markdown(line: &str) -> String {
    let mut text = line.trim_start();
    // Block markers: headings, quotes, list bullets, numbered items.
    loop {
        let before = text;
        text = text.trim_start_matches(['#', '>']).trim_start();
        for bullet in ["- ", "* ", "+ "] {
            if let Some(rest) = text.strip_prefix(bullet) {
                text = rest.trim_start();
            }
        }
        let digits = text.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && (text[digits..].starts_with(". ") || text[digits..].starts_with(") ")) {
            text = text[digits + 2..].trim_start();
        }
        if text == before {
            break;
        }
    }
    text.chars().filter(|c| !matches!(c, '*' | '_' | '`' | '~')).collect()
}

/// The deterministic automatic name of a first prompt: its first non-empty line without leading `/command` and
/// `@file` tokens and markdown markers, whitespace collapsed, cut at a word boundary to 50 characters ("…" when
/// cut), normalized. `None` when nothing is left.
pub fn auto_name(prompt: &str) -> Option<String> {
    let line = prompt.lines().find(|l| !l.trim().is_empty())?;
    let text = strip_markdown(line);
    let words: Vec<&str> = text.split_whitespace().collect();
    let skip = words
        .iter()
        .take_while(|w| w.starts_with('/') || w.starts_with('@'))
        .count();
    let text = collapse_whitespace(&strip_markdown(&words[skip..].join(" ")));
    normalize_session_name(&cut_at_word(&text, AUTO_NAME_MAX, true))
}

/// A model reply made into a title: its first non-empty line without markdown, quotes and trailing punctuation,
/// whitespace collapsed, at most 60 characters (cut at a word boundary), normalized.
pub fn clean_title(reply: &str) -> Option<String> {
    let line = reply.lines().find(|l| !l.trim().is_empty())?;
    let mut text = collapse_whitespace(&strip_markdown(line));
    if let Some(rest) = text.strip_prefix("Title:").or_else(|| text.strip_prefix("title:")) {
        text = rest.trim().to_string();
    }
    let quotes = ['"', '\'', '“', '”', '‘', '’', '«', '»'];
    let text = text
        .trim_matches(|c: char| quotes.contains(&c) || c.is_whitespace())
        .trim_end_matches(['.', '!', '?', ':', ';', ','])
        .trim_matches(|c: char| quotes.contains(&c) || c.is_whitespace());
    normalize_session_name(&cut_at_word(text, TITLE_MAX, false))
}

/// The automatic name currently recorded, if any.
fn recorded_automatic(session: &Session) -> Option<String> {
    session
        .value(AUTO_NAME_NAMESPACE, AUTO_NAME_KEY)
        .and_then(|v| v.get("automatic"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Whether the session's name is an automatic one.
pub fn is_automatic(session: &Session) -> bool {
    let name = session.name();
    name.is_some() && name == recorded_automatic(session)
}

/// Rename the session (`-n`, `/name`): the user's name always replaces an automatic one. `None` or an empty name
/// removes it.
pub fn set_user_name(session: &mut Session, name: Option<&str>) -> Result<Option<String>, String> {
    let name = name.and_then(normalize_session_name);
    let mut writes = vec![match &name {
        Some(name) => Write::set(SESSION_NAME, "", name.clone().into()),
        None => Write::delete(SESSION_NAME, ""),
    }];
    if session.value(AUTO_NAME_NAMESPACE, AUTO_NAME_KEY).is_some() {
        writes.push(Write::delete(AUTO_NAME_NAMESPACE, AUTO_NAME_KEY));
    }
    session.commit(&writes)?;
    Ok(name)
}

fn write_automatic(session: &mut Session, name: &str) -> Result<(), String> {
    session.commit(&[
        Write::set(SESSION_NAME, "", name.into()),
        Write::set(
            AUTO_NAME_NAMESPACE,
            AUTO_NAME_KEY,
            json!({"version": 1, "automatic": name}),
        ),
    ])?;
    Ok(())
}

/// Name a session from its first prompt, before that prompt is committed: only a session with no name and no
/// message yet is named. Returns the name given.
pub fn apply_auto_name(session: &mut Session, prompt: &str) -> Result<Option<String>, String> {
    if session.name().is_some() || session.state().entries().iter().any(|e| e["type"] == "message") {
        return Ok(None);
    }
    let Some(name) = auto_name(prompt) else {
        return Ok(None);
    };
    write_automatic(session, &name)?;
    Ok(Some(name))
}

/// Replace an automatic name by a better one (the model's title); a name the user set meanwhile wins.
pub fn replace_auto_name(session: &mut Session, title: &str) -> Result<bool, String> {
    let Some(title) = normalize_session_name(title) else {
        return Ok(false);
    };
    if !is_automatic(session) {
        return Ok(false);
    }
    write_automatic(session, &title)?;
    Ok(true)
}

/// One session as a picker lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionListing {
    pub metadata: Metadata,
    pub name: Option<String>,
    pub automatic_name: bool,
    /// Text of the first user message (text parts joined by spaces).
    pub first_message: String,
    /// `SessionInfo.allMessagesText`: every user and assistant message's text, joined by spaces (what the
    /// selector searches).
    pub all_messages_text: String,
    pub message_count: usize,
}

/// What a session file says about itself (`nativeSessionDetails`, plus `allMessagesText`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionDetails {
    pub name: Option<String>,
    /// The name is the automatic one (the user never set it).
    pub automatic_name: bool,
    pub first_message: String,
    pub all_messages_text: String,
    pub message_count: usize,
}

/// `extractTextContent` of session-manager.ts's `buildSessionInfo`: a string content, or an array's `text` blocks
/// joined by spaces (a block's `text` as `Array.join` renders it). Content upstream would throw on (not a string
/// or array, or an array holding `null`) contributes nothing; upstream drops the whole file from its list.
fn search_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) if !blocks.iter().any(Value::is_null) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .map(|b| js::join_string(b.get("text")))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .map(|p| match &p["text"] {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => js::stringify(other),
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// `nativeSessionDetails` of a file's text (format 4 or v3), plus whether the name is automatic and
/// `allMessagesText`.
pub fn session_details(text: &str) -> SessionDetails {
    let mut name: Option<String> = None;
    let mut automatic: Option<String> = None;
    let mut first = String::new();
    let mut all = Vec::new();
    let mut count = 0;
    for line in text.split('\n').filter(|l| !l.trim().is_empty()) {
        let Ok(parsed) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let mutations = match parsed {
            Value::Array(items) => items,
            other => vec![other],
        };
        for m in mutations.iter().filter(|m| m.is_object()) {
            let kind = m.get("kind").and_then(Value::as_str);
            if kind == Some("value") && m["namespace"] == SESSION_NAME {
                name = (m["op"] == "set")
                    .then(|| m["value"].as_str().map(str::to_string))
                    .flatten();
                continue;
            }
            if kind == Some("value") && m["namespace"] == AUTO_NAME_NAMESPACE && m["key"] == AUTO_NAME_KEY {
                automatic = (m["op"] == "set")
                    .then(|| m["value"]["automatic"].as_str().map(str::to_string))
                    .flatten();
                continue;
            }
            if m["type"] == "session_info" {
                name = m["name"]
                    .as_str()
                    .map(js::trim)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string);
                continue;
            }
            if (kind == Some("entry") || kind.is_none()) && m["type"] == "message" {
                let role = &m["message"]["role"];
                if role != "user" && role != "assistant" {
                    continue;
                }
                count += 1;
                if role == "user" && first.is_empty() {
                    first = content_text(&m["message"]["content"]);
                }
                // `isMessageWithContent`: only a message with a `content` field has text; empty text is skipped.
                if let Some(content) = m["message"].get("content") {
                    let text = search_text(content);
                    if !text.is_empty() {
                        all.push(text);
                    }
                }
            }
        }
    }
    SessionDetails {
        automatic_name: name.is_some() && name == automatic,
        name,
        first_message: first,
        all_messages_text: all.join(" "),
        message_count: count,
    }
}

/// The listing of one session file.
pub fn listing(metadata: Metadata) -> SessionListing {
    let text = std::fs::read_to_string(&metadata.path).unwrap_or_default();
    let details = session_details(&text);
    SessionListing {
        metadata,
        name: details.name,
        automatic_name: details.automatic_name,
        first_message: details.first_message,
        all_messages_text: details.all_messages_text,
        message_count: details.message_count,
    }
}

/// The sessions of `cwd` under `root`, most recently modified first (then newest created, then id).
pub fn list_sessions(root: &Path, cwd: &str) -> Vec<SessionListing> {
    let cwd = absolute(cwd);
    let mut found: Vec<SessionListing> = Vec::new();
    let Ok(directories) = std::fs::read_dir(root) else {
        return found;
    };
    for directory in directories
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
    {
        let Ok(files) = std::fs::read_dir(directory.path()) else {
            continue;
        };
        for file in files.flatten() {
            if !file.file_type().is_ok_and(|t| t.is_file()) || !file.file_name().to_string_lossy().ends_with(".jsonl") {
                continue;
            }
            if let Some(metadata) = read_metadata(&file.path())
                && metadata.cwd == cwd
            {
                found.push(listing(metadata));
            }
        }
    }
    sort_newest(&mut found);
    found
}

fn sort_newest(sessions: &mut [SessionListing]) {
    sessions.sort_by(|l, r| {
        r.metadata
            .modified_at
            .total_cmp(&l.metadata.modified_at)
            .then(r.metadata.created_at.cmp(&l.metadata.created_at))
            .then_with(|| l.metadata.id.cmp(&r.metadata.id))
    });
}

/// Every session under `root` (any cwd), most recently modified first.
pub fn list_all_sessions(repo: &SessionRepo) -> Vec<SessionListing> {
    let mut found: Vec<SessionListing> = repo.list(None).into_iter().map(listing).collect();
    sort_newest(&mut found);
    found
}

fn describe(session: &SessionListing) -> String {
    match &session.name {
        Some(name) => format!("{} ({name})", session.metadata.id),
        None => session.metadata.id.clone(),
    }
}

/// `--resume <selector>` among `sessions` (newest first): an exact id; else an exact name (case-insensitive; the
/// most recent of several); else the one session whose id or name starts with the selector (names
/// case-insensitively). Errors name the candidates.
pub fn resolve_resume<'a>(sessions: &'a [SessionListing], selector: &str) -> Result<&'a SessionListing, String> {
    if let Some(exact) = sessions.iter().find(|s| s.metadata.id == selector) {
        return Ok(exact);
    }
    let wanted = selector.trim().to_lowercase();
    if wanted.is_empty() {
        return Err("Session selector is empty".into());
    }
    let name_of = |s: &SessionListing| s.name.as_deref().map(str::to_lowercase);
    if let Some(named) = sessions.iter().find(|s| name_of(s).as_deref() == Some(wanted.as_str())) {
        return Ok(named);
    }
    let matches: Vec<&SessionListing> = sessions
        .iter()
        .filter(|s| s.metadata.id.starts_with(selector) || name_of(s).is_some_and(|n| n.starts_with(&wanted)))
        .collect();
    match matches[..] {
        [one] => Ok(one),
        [] => {
            let mut message = format!("No session found matching '{selector}' in this directory.");
            let named: Vec<String> = sessions.iter().filter(|s| s.name.is_some()).map(describe).collect();
            if !named.is_empty() {
                message.push_str(&format!(" Named sessions: {}", named.join(", ")));
            }
            Err(message)
        }
        _ => Err(format!(
            "Session selector '{selector}' is ambiguous; matches: {}",
            matches.iter().map(|s| describe(s)).collect::<Vec<_>>().join(", ")
        )),
    }
}
