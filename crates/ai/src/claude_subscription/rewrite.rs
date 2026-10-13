//! What the admission relay may change in the one Messages request it forwards (`admission.py`): restore Ultron's
//! queried tool-result frame when native's copy matches it unambiguously (native appends per-request context such as
//! the account-email reminder to the last result, which must not masquerade as tool output), keep the single message
//! cache breakpoint on content the next request replays, and put Ultron's first turn ahead of the context block native
//! prepends to it on a conversation's first request. Content is never dropped or invented; anything that does not
//! parse or match forwards exactly as native built it.

use serde_json::{Map, Value};

/// The queried frame could not be reconciled with native's request; the payload forwards unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueriedTurnMismatch;

pub const MISMATCH: &str = "Native request does not uniquely match Ultron tool results";

impl std::fmt::Display for QueriedTurnMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(MISMATCH)
    }
}

const UNCACHEABLE: &[&str] = &["thinking", "redacted_thinking"];

fn plain(block: &Value) -> Value {
    match block {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(k, _)| *k != "cache_control")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn kind(block: &Value) -> Option<&str> {
    block.as_object()?.get("type")?.as_str()
}

fn role(message: &Value) -> Option<&str> {
    message.as_object()?.get("role")?.as_str()
}

fn dump(body: &Value) -> Vec<u8> {
    serde_json::to_vec(body).unwrap_or_default()
}

/// Put Ultron's first user turn ahead of the plain-text context native prepends to it (only on a conversation's first
/// request, only when the queried frame occurs exactly once after nothing but text). Returns whether it moved.
fn frame_first(messages: &mut [Value], queried: &[Value]) -> bool {
    if messages.iter().any(|m| role(m) == Some("assistant")) {
        return false;
    }
    let Some(first) = messages.iter_mut().find(|m| role(m) == Some("user")) else {
        return false;
    };
    let Some(content) = first.get_mut("content").and_then(Value::as_array_mut) else {
        return false;
    };
    if queried.is_empty() || content.len() <= queried.len() {
        return false;
    }
    let want: Vec<Value> = queried.iter().map(plain).collect();
    let starts: Vec<usize> = (0..=content.len() - queried.len())
        .filter(|&k| content[k..k + queried.len()].iter().map(plain).eq(want.iter().cloned()))
        .collect();
    let [k] = starts[..] else { return false };
    if k == 0 || !content[..k].iter().all(|b| b.is_object() && kind(b) == Some("text")) {
        return false;
    }
    let mut reordered: Vec<Value> = content[k..k + queried.len()].to_vec();
    reordered.extend_from_slice(&content[..k]);
    reordered.extend_from_slice(&content[k + queried.len()..]);
    *content = reordered;
    true
}

/// Keep the one message `cache_control` on content the next request replays unchanged (`pin_message_breakpoint`):
/// everything through the last assistant message plus the leading blocks of the newest turn equal to the queried
/// frame recur; marks after that span fold into one on its last cacheable block. System and tools marks are never
/// touched, the breakpoint never moves later and the count never grows.
pub fn pin_message_breakpoint(payload: &[u8], queried: Option<&[Value]>) -> Vec<u8> {
    let Some(queried) = queried.filter(|q| !q.is_empty()) else {
        return payload.to_vec();
    };
    pin(payload, queried).unwrap_or_else(|| payload.to_vec())
}

fn pin(payload: &[u8], queried: &[Value]) -> Option<Vec<u8>> {
    let mut body: Value = serde_json::from_slice(payload).ok()?;
    let messages = body.get_mut("messages")?.as_array_mut()?;
    if messages.iter().any(|m| !m.is_object()) {
        return None;
    }
    let reordered = frame_first(messages, queried);
    let mut blocks: Vec<(usize, usize)> = Vec::new();
    for (i, message) in messages.iter().enumerate() {
        if let Some(content) = message.get("content").and_then(Value::as_array) {
            blocks.extend((0..content.len()).map(|j| (i, j)));
        }
    }
    let block = |messages: &[Value], (i, j): (usize, usize)| messages[i]["content"][j].clone();
    let marked: Vec<(usize, usize)> = blocks
        .iter()
        .copied()
        .filter(|&at| block(messages, at).get("cache_control").is_some())
        .collect();
    if marked.is_empty() {
        return Some(payload.to_vec());
    }
    let last = messages.iter().rposition(|m| role(m) == Some("assistant"));
    let next = last.map_or(0, |l| l + 1);
    let mut stable: Vec<(usize, usize)> = blocks
        .iter()
        .copied()
        .filter(|(i, _)| last.is_some_and(|l| *i <= l))
        .collect();
    if let Some(newest) = messages.get(next)
        && role(newest) == Some("user")
        && let Some(content) = newest.get("content").and_then(Value::as_array)
    {
        let prefix = content
            .iter()
            .zip(queried)
            .take_while(|(sent, host)| plain(sent) == plain(host))
            .count();
        let rest = &content[prefix..];
        if !rest.first().is_some_and(|b| kind(b) == Some("tool_result")) {
            stable.extend((0..prefix).map(|j| (next, j)));
        }
    }
    let target = stable.iter().rev().copied().find(|&at| {
        let b = block(messages, at);
        b.is_object() && !kind(&b).is_some_and(|k| UNCACHEABLE.contains(&k))
    });
    let after: Vec<(usize, usize)> = match target {
        Some(target) => marked.iter().copied().filter(|&at| at > target).collect(),
        None => Vec::new(),
    };
    if let (Some((ti, tj)), false) = (target, after.is_empty()) {
        let mut moved = None;
        for (i, j) in after {
            let removed = messages[i]["content"][j].as_object_mut()?.shift_remove("cache_control");
            moved = moved.or(removed);
        }
        let target = messages[ti]["content"][tj].as_object_mut()?;
        if !target.contains_key("cache_control") {
            target.insert("cache_control".into(), moved?);
        }
    } else if !reordered {
        return Some(payload.to_vec());
    }
    Some(dump(&body))
}

fn bare_text(block: &Value) -> bool {
    block.as_object().is_some_and(|map| {
        map.len() == 2 && map.get("type") == Some(&Value::from("text")) && map.get("text").is_some_and(Value::is_string)
    })
}

fn joined(blocks: &[Value]) -> String {
    blocks.iter().filter_map(|b| b["text"].as_str()).collect()
}

fn restore_content(sent: &Value, host: &Value) -> Result<Value, QueriedTurnMismatch> {
    if host.is_string() || sent.is_string() {
        let text = |value: &Value| match value {
            Value::String(s) => Some(s.clone()),
            Value::Array(items) if items.iter().all(bare_text) => Some(joined(items)),
            _ => None,
        };
        return match (text(sent), text(host)) {
            (Some(left), Some(right)) if left.starts_with(&right) => Ok(host.clone()),
            _ => Err(QueriedTurnMismatch),
        };
    }
    if let (Value::Array(sent), Value::Array(host)) = (sent, host) {
        return restore_sequence(sent, host).map(Value::Array);
    }
    if sent != host {
        return Err(QueriedTurnMismatch);
    }
    Ok(host.clone())
}

fn without(map: &Map<String, Value>, ignored: &[&str]) -> Map<String, Value> {
    map.iter()
        .filter(|(k, _)| !ignored.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn restore_block(sent: &Value, host: &Value) -> Result<Value, QueriedTurnMismatch> {
    let (Some(sent_map), Some(host_map)) = (sent.as_object(), host.as_object()) else {
        return Err(QueriedTurnMismatch);
    };
    let mut restored = host_map.clone();
    if kind(host) == Some("tool_result") {
        // Missing means false; numeric 0, null or "false" are not protocol booleans.
        let flag = |map: &Map<String, Value>| map.get("is_error").cloned().unwrap_or(Value::Bool(false));
        match (flag(sent_map), flag(host_map)) {
            (Value::Bool(left), Value::Bool(right)) if left == right => {}
            _ => return Err(QueriedTurnMismatch),
        }
        let ignored = ["content", "is_error", "cache_control"];
        if without(sent_map, &ignored) != without(host_map, &ignored)
            || sent_map.contains_key("content") != host_map.contains_key("content")
        {
            return Err(QueriedTurnMismatch);
        }
        if let (Some(sent_content), Some(host_content)) = (sent_map.get("content"), host_map.get("content")) {
            restored.insert("content".into(), restore_content(sent_content, host_content)?);
        }
    } else if plain(sent) != plain(host) {
        return Err(QueriedTurnMismatch);
    }
    if let Some(marker) = sent_map.get("cache_control") {
        if host_map.get("cache_control").is_some_and(|own| own != marker) {
            return Err(QueriedTurnMismatch);
        }
        restored.insert("cache_control".into(), marker.clone());
    }
    Ok(Value::Object(restored))
}

fn restore_sequence(sent: &[Value], host: &[Value]) -> Result<Vec<Value>, QueriedTurnMismatch> {
    let mut restored = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < host.len() {
        if j >= sent.len() {
            return Err(QueriedTurnMismatch);
        }
        if bare_text(&host[i]) && bare_text(&sent[j]) {
            let (mut host_end, mut sent_end) = (i + 1, j + 1);
            while host_end < host.len() && bare_text(&host[host_end]) {
                host_end += 1;
            }
            while sent_end < sent.len() && bare_text(&sent[sent_end]) {
                sent_end += 1;
            }
            let original = joined(&host[i..host_end]);
            let actual = joined(&sent[j..sent_end]);
            // Extensions before a protected host block are insertions, not suffixes.
            let ok = if host_end == host.len() {
                actual.starts_with(&original)
            } else {
                actual == original
            };
            if !ok {
                return Err(QueriedTurnMismatch);
            }
            restored.extend_from_slice(&host[i..host_end]);
            (i, j) = (host_end, sent_end);
        } else {
            restored.push(restore_block(&sent[j], &host[i])?);
            i += 1;
            j += 1;
        }
    }
    if !sent[j..].iter().all(bare_text) {
        return Err(QueriedTurnMismatch);
    }
    Ok(restored)
}

/// Restore the queried tool-result frame (`restore_queried_turn`): anchored on Ultron's unique ordered result ids,
/// only unmarked plain text native appended is dropped, equivalent text encodings restore the host's own shape.
/// An ordinary user-only frame is outside its scope and forwards byte for byte.
pub fn restore_queried_turn(payload: &[u8], queried: Option<&[Value]>) -> Result<Vec<u8>, QueriedTurnMismatch> {
    let Some(queried) = queried.filter(|q| !q.is_empty()) else {
        return Ok(payload.to_vec());
    };
    if !queried.iter().any(|b| b.is_object() && kind(b) == Some("tool_result")) {
        return Ok(payload.to_vec());
    }
    let mut body: Value = serde_json::from_slice(payload).map_err(|_| QueriedTurnMismatch)?;
    let messages = body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or(QueriedTurnMismatch)?;
    if messages.iter().any(|m| !m.is_object()) {
        return Err(QueriedTurnMismatch);
    }
    let next = messages
        .iter()
        .rposition(|m| role(m) == Some("assistant"))
        .map_or(0, |l| l + 1);
    let newest = messages.get_mut(next).ok_or(QueriedTurnMismatch)?;
    if role(newest) != Some("user") {
        return Err(QueriedTurnMismatch);
    }
    let native = newest
        .get("content")
        .and_then(Value::as_array)
        .ok_or(QueriedTurnMismatch)?
        .clone();
    let result_ids: Vec<&Value> = queried
        .iter()
        .filter(|b| b.is_object() && kind(b) == Some("tool_result"))
        .map(|b| b.get("tool_use_id").unwrap_or(&Value::Null))
        .collect();
    let mut unique = std::collections::HashSet::new();
    if result_ids
        .iter()
        .any(|id| !id.as_str().is_some_and(|s| !s.is_empty()) || !unique.insert(id.as_str()))
    {
        return Err(QueriedTurnMismatch);
    }
    let native_ids: Vec<&Value> = native
        .iter()
        .filter(|b| b.is_object() && kind(b) == Some("tool_result"))
        .map(|b| b.get("tool_use_id").unwrap_or(&Value::Null))
        .collect();
    if result_ids != native_ids {
        return Err(QueriedTurnMismatch);
    }
    let restored = restore_sequence(&native, queried)?;
    if native == restored {
        return Ok(payload.to_vec());
    }
    newest["content"] = Value::Array(restored);
    Ok(dump(&body))
}
