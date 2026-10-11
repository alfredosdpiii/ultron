//! `experimental/pi-session-view.ts`: Pi's read-side views (tree, context messages, last assistant text, stats)
//! over a format-4 session converted to Pi entries ([`super::pi_view`]), with the shapes and numbers Pi reports.

/// The text blocks of a message joined (a string content as it is).
pub fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}
use serde_json::{Map, Value, json};
use session_file::SessionManager;

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// The entries, without the header, in session order.
pub fn entries(manager: &SessionManager) -> Vec<Value> {
    manager.entries().into_iter().cloned().collect()
}

/// Pi's `session.messages`: the model context at the leaf.
pub fn messages(manager: &SessionManager) -> Vec<Value> {
    manager.build_session_context().messages
}

/// Pi's `getTree()`: `{entry, children, label?, labelTimestamp?}` nodes.
pub fn tree(manager: &SessionManager) -> Value {
    let tree = manager.tree();
    fn node(tree: &session_file::SessionTree<'_>, index: usize) -> Value {
        let n = &tree.nodes[index];
        let mut out = Map::new();
        out.insert("entry".into(), n.entry.clone());
        out.insert(
            "children".into(),
            Value::Array(n.children.iter().map(|c| node(tree, *c)).collect()),
        );
        if let Some(label) = n.label {
            out.insert("label".into(), label.clone());
        }
        if let Some(timestamp) = n.label_timestamp {
            out.insert("labelTimestamp".into(), timestamp.clone());
        }
        Value::Object(out)
    }
    Value::Array(tree.roots.iter().map(|r| node(&tree, *r)).collect())
}

/// `getLastAssistantText()`: the newest assistant message that is not an empty abort, its text blocks joined.
pub fn last_assistant_text(manager: &SessionManager) -> Option<String> {
    let messages = messages(manager);
    let last = messages.iter().rev().find(|m| {
        str_of(m, "role") == Some("assistant")
            && !(str_of(m, "stopReason") == Some("aborted") && m["content"].as_array().is_none_or(Vec::is_empty))
    })?;
    Some(
        last["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect()
            })
            .unwrap_or_default(),
    )
}

/// `get_fork_messages`: every user message entry (any branch) with its text, when it has text.
pub fn fork_messages(manager: &SessionManager) -> Vec<Value> {
    manager
        .entries()
        .into_iter()
        .filter(|e| str_of(e, "type") == Some("message") && str_of(&e["message"], "role") == Some("user"))
        .filter_map(|e| {
            let text = message_text(&e["message"]);
            (!text.is_empty()).then(|| json!({"entryId": e["id"], "text": text}))
        })
        .collect()
}

#[derive(Default)]
struct Totals {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    cost: f64,
}

impl Totals {
    /// `addUsageToTotals`.
    fn add(&mut self, usage: &Value) {
        let n = |v: &Value| v.as_f64().unwrap_or(0.0);
        self.input += n(&usage["input"]);
        self.output += n(&usage["output"]);
        self.cache_read += n(&usage["cacheRead"]);
        self.cache_write += n(&usage["cacheWrite"]);
        self.cost += n(&usage["cost"]["total"]);
    }
}

/// A JSON number as JavaScript holds it: integral values print without a fraction.
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.007_199_254_740_992e15 {
        Value::from(value as i64)
    } else {
        Value::from(value)
    }
}

/// `getSessionStats()`: totals over every entry, compacted and abandoned history included.
pub fn stats(manager: &SessionManager, session_file: Option<&str>, session_id: &str, context_window: u64) -> Value {
    let (mut users, mut assistants, mut tool_results, mut total, mut tool_calls) = (0, 0, 0, 0, 0);
    let mut totals = Totals::default();
    for entry in manager.entries() {
        match str_of(entry, "type") {
            Some("usage") => totals.add(&entry["usage"]),
            Some("branch_summary" | "compaction") if entry.get("usage").is_some_and(|u| !u.is_null()) => {
                totals.add(&entry["usage"])
            }
            _ => {}
        }
        if str_of(entry, "type") != Some("message") {
            continue;
        }
        total += 1;
        let message = &entry["message"];
        match str_of(message, "role") {
            Some("user") => users += 1,
            Some("toolResult") => {
                tool_results += 1;
                if let Some(usage) = message.get("usage").filter(|u| !u.is_null()) {
                    totals.add(usage);
                }
            }
            Some("assistant") => {
                assistants += 1;
                tool_calls += message["content"]
                    .as_array()
                    .map_or(0, |c| c.iter().filter(|b| b["type"] == "toolCall").count());
                totals.add(&message["usage"]);
            }
            _ => {}
        }
    }
    let mut out = Map::new();
    if let Some(file) = session_file {
        out.insert("sessionFile".into(), file.into());
    }
    out.insert("sessionId".into(), session_id.into());
    out.insert("userMessages".into(), users.into());
    out.insert("assistantMessages".into(), assistants.into());
    out.insert("toolCalls".into(), tool_calls.into());
    out.insert("toolResults".into(), tool_results.into());
    out.insert("totalMessages".into(), total.into());
    out.insert(
        "tokens".into(),
        json!({
            "input": number(totals.input), "output": number(totals.output),
            "cacheRead": number(totals.cache_read), "cacheWrite": number(totals.cache_write),
            "total": number(totals.input + totals.output + totals.cache_read + totals.cache_write),
        }),
    );
    out.insert("cost".into(), number(totals.cost));
    if let Some(usage) = context_usage(manager, context_window) {
        out.insert("contextUsage".into(), usage);
    }
    Value::Object(out)
}

/// `getContextUsage()`: unknown (`null`) right after a compaction until a later response reports usage.
fn context_usage(manager: &SessionManager, context_window: u64) -> Option<Value> {
    if context_window == 0 {
        return None;
    }
    let projection = manager.build_session_projection();
    let branch: Vec<Value> = manager.branch_entries(None).into_iter().cloned().collect();
    if let Some(compaction) = branch.iter().rposition(|e| str_of(e, "type") == Some("compaction")) {
        let projected_assistants: Vec<&Value> = projection
            .entries
            .iter()
            .filter(|entry| {
                entry.messages.iter().any(|m| {
                    str_of(m, "role") == Some("assistant")
                        && !matches!(str_of(m, "stopReason"), Some("aborted" | "error"))
                        && compaction::calculate_context_tokens(&m["usage"]) > 0
                })
            })
            .filter_map(|entry| entry.source.get("id"))
            .collect();
        let has_usage_after = branch[compaction + 1..]
            .iter()
            .any(|e| e.get("id").is_some_and(|id| projected_assistants.contains(&id)));
        if !has_usage_after {
            return Some(json!({"tokens": null, "contextWindow": context_window, "percent": null}));
        }
    }
    let estimate = compaction::estimate_projected_context_tokens(&projection, &branch);
    let percent = estimate.tokens as f64 / context_window as f64 * 100.0;
    Some(json!({"tokens": estimate.tokens, "contextWindow": context_window, "percent": number(percent)}))
}
