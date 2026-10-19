//! The pure half of `ultron acp`: ACP prompt content blocks become one user message, the RPC event stream of
//! [`crate::rpc`] (Pi's wire events, as `--mode rpc` writes them) becomes ACP `session/update`s, a stored conversation
//! becomes its replay.
//!
//! Every `rlm` cell is one ACP tool call: `kind: "execute"` (`"edit"` once it changed files), its code as the first
//! content block and `rawInput.code`, its output as the last content block when it ends. Files a cell changed come as
//! `locations` (and `diff` content where the text before it is known), so editors reload and follow them.

use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The ACP protocol version spoken (schema v1).
pub const PROTOCOL_VERSION: u64 = 1;
/// The default refusal text of the Anthropic provider (`ai::anthropic`), the only refusal the stream can tell apart.
pub const REFUSAL_TEXT: &str = "The model refused to complete the request";
/// Diff text larger than this is not sent (the location is).
pub const MAX_DIFF_BYTES: usize = 256 * 1024;
/// A tool call title is the cell's first code line, cut to this many characters.
const MAX_TITLE: usize = 80;

/// One ACP prompt (`PromptRequest.prompt`) as the session's user message: its text and Pi image blocks.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UserMessage {
    pub text: String,
    pub images: Vec<Value>,
}

/// `ContentBlock[]` -> the user message. Text and resource links run on inline (Zed splits a line around a mention);
/// consecutive text blocks (CodeCompanion and avante send one block per message part) are separated by a blank line;
/// embedded text resources follow as `<context ref="uri">` sections; images become Pi image blocks.
pub fn user_message(blocks: &[Value]) -> Result<UserMessage, String> {
    let mut message = UserMessage::default();
    let mut contexts = Vec::new();
    let mut last_was_text = false;
    for block in blocks {
        let kind = block["type"].as_str().unwrap_or_default();
        match kind {
            "text" => {
                let text = block["text"].as_str().unwrap_or_default();
                if last_was_text && !message.text.is_empty() {
                    message.text.push_str("\n\n");
                }
                message.text.push_str(text);
                last_was_text = true;
                continue;
            }
            "resource_link" => {
                let uri = block["uri"].as_str().unwrap_or_default();
                let name = block["name"].as_str().filter(|n| !n.is_empty()).unwrap_or(uri);
                message.text.push_str(&format!("[@{name}]({uri})"));
            }
            "image" => match (block["data"].as_str(), block["mimeType"].as_str()) {
                (Some(data), Some(mime)) => message
                    .images
                    .push(json!({"type": "image", "data": data, "mimeType": mime})),
                _ => return Err("An image content block needs data and mimeType".into()),
            },
            "resource" => {
                let resource = &block["resource"];
                let uri = resource["uri"].as_str().unwrap_or_default();
                let mime = resource["mimeType"].as_str().unwrap_or_default();
                if let Some(text) = resource["text"].as_str() {
                    contexts.push(format!("<context ref=\"{uri}\">\n{text}\n</context>"));
                } else if let (Some(blob), true) = (resource["blob"].as_str(), mime.starts_with("image/")) {
                    message
                        .images
                        .push(json!({"type": "image", "data": blob, "mimeType": mime}));
                } else {
                    contexts.push(format!("[binary resource {uri} omitted]"));
                }
            }
            "audio" => contexts.push("[audio omitted: not supported]".into()),
            other => return Err(format!("Unsupported content block type: {other}")),
        }
        last_was_text = false;
    }
    for context in contexts {
        if !message.text.is_empty() {
            message.text.push('\n');
        }
        message.text.push_str(&context);
    }
    if message.text.trim().is_empty() && message.images.is_empty() {
        return Err("The prompt has no content".into());
    }
    Ok(message)
}

/// `{sessionUpdate, content: {type: "text", text}}`.
pub fn chunk(kind: &str, text: &str) -> Value {
    json!({"sessionUpdate": kind, "content": {"type": "text", "text": text}})
}

fn text_content(text: &str) -> Value {
    json!({"type": "content", "content": {"type": "text", "text": text}})
}

/// The code of a cell as a fenced block.
fn code_content(code: &str) -> Value {
    text_content(&format!("```python\n{}\n```", code.trim_end()))
}

/// A tool call's title: the cell's first non-blank code line in backticks (CodeCompanion shows `Execute: <line>`).
pub fn tool_title(name: &str, args: &Value) -> String {
    let Some(code) = args["code"].as_str() else {
        return name.to_string();
    };
    let line = code.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.is_empty() {
        return name.to_string();
    }
    let mut title: String = line.chars().take(MAX_TITLE).collect();
    if line.chars().count() > MAX_TITLE {
        title.push_str("...");
    }
    format!("`{}`", title.replace('`', "'"))
}

/// The text of a tool result (`{content: [...]}`): its text blocks joined.
pub fn result_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `tool_call` that opens a cell.
pub fn tool_call(id: &str, name: &str, args: &Value) -> Value {
    let mut content = Vec::new();
    if let Some(code) = args["code"].as_str() {
        content.push(code_content(code));
    }
    json!({
        "sessionUpdate": "tool_call", "toolCallId": id, "title": tool_title(name, args),
        "kind": if name == "rlm" { "execute" } else { "other" }, "status": "in_progress",
        "content": content, "rawInput": args,
    })
}

/// What a cell changed on disk (absolute paths), and the diffs that can be shown.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileChanges {
    pub changed: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    /// `{type: "diff", path, oldText, newText}` blocks.
    pub diffs: Vec<Value>,
}

impl FileChanges {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.deleted.is_empty()
    }
}

/// The `tool_call_update` of a cell's progress (`partial`: Pi's partial result) or its end.
pub fn tool_update(id: &str, args: &Value, output: &Value, end: Option<(bool, &FileChanges)>) -> Value {
    let mut content = Vec::new();
    if let Some(code) = args["code"].as_str() {
        content.push(code_content(code));
    }
    let text = result_text(output);
    if !text.is_empty() {
        content.push(text_content(&format!("```\n{}\n```", text.trim_end())));
    }
    let mut update = Map::new();
    update.insert("sessionUpdate".into(), "tool_call_update".into());
    update.insert("toolCallId".into(), id.into());
    let Some((is_error, changes)) = end else {
        update.insert("status".into(), "in_progress".into());
        update.insert("content".into(), Value::Array(content));
        return Value::Object(update);
    };
    update.insert("status".into(), if is_error { "failed" } else { "completed" }.into());
    if !changes.is_empty() {
        update.insert("kind".into(), "edit".into());
        let locations: Vec<Value> = changes
            .changed
            .iter()
            .map(|path| json!({"path": path.to_string_lossy()}))
            .collect();
        update.insert("locations".into(), Value::Array(locations));
        content.extend(changes.diffs.iter().cloned());
        if !changes.deleted.is_empty() {
            let deleted: Vec<String> = changes
                .deleted
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            content.push(text_content(&format!("Deleted: {}", deleted.join(", "))));
        }
    }
    update.insert("content".into(), Value::Array(content));
    update.insert("rawOutput".into(), json!({"output": text, "isError": is_error}));
    Value::Object(update)
}

/// How a prompt turn ended.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnEnd {
    /// `PromptResponse.stopReason`.
    Stop(&'static str),
    /// The run failed: the error response's message.
    Failed(String),
}

/// The turn's end from `agent_end`'s messages: `cancelled` when the client cancelled (whatever the run reports), an
/// aborted run as cancelled, a length stop as `max_tokens`, the provider's refusal as `refusal`, any other error as a
/// failed request.
pub fn turn_end(messages: &[Value], cancelled: bool) -> TurnEnd {
    if cancelled {
        return TurnEnd::Stop("cancelled");
    }
    let last = messages.iter().rev().find(|m| m["role"] == "assistant");
    let Some(last) = last else {
        return TurnEnd::Stop("end_turn");
    };
    match last["stopReason"].as_str().unwrap_or_default() {
        "aborted" => TurnEnd::Stop("cancelled"),
        "length" => TurnEnd::Stop("max_tokens"),
        "error" => {
            let message = last["errorMessage"].as_str().unwrap_or("The model request failed");
            if message == REFUSAL_TEXT || message == "Provider stopped with: sensitive" {
                TurnEnd::Stop("refusal")
            } else {
                TurnEnd::Failed(message.to_string())
            }
        }
        _ => TurnEnd::Stop("end_turn"),
    }
}

/// `usage_update` after a turn: the last answer's context use against the model's window.
pub fn usage_update(messages: &[Value], context_window: Option<u64>) -> Option<Value> {
    let size = context_window.filter(|w| *w > 0)?;
    let usage = &messages.iter().rev().find(|m| m["role"] == "assistant")?["usage"];
    let used = usage["totalTokens"].as_u64().filter(|t| *t > 0).unwrap_or_else(|| {
        ["input", "output", "cacheRead", "cacheWrite"]
            .iter()
            .filter_map(|k| usage[*k].as_u64())
            .sum()
    });
    let mut update = json!({"sessionUpdate": "usage_update", "used": used, "size": size});
    if let Some(total) = usage["cost"]["total"].as_f64().filter(|c| *c > 0.0) {
        update["cost"] = json!({"amount": total, "currency": "USD"});
    }
    Some(update)
}

/// What one RPC event line asks of the ACP side.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A `session/update` of the session.
    Update(Value),
    /// A cell started: take the picture its changes are measured against, unless one is held.
    CellStarted,
    /// A cell ended: measure what it changed, then send this update builder's result.
    CellEnded {
        id: String,
        args: Value,
        result: Value,
        is_error: bool,
    },
    /// The run ended (`agent_end`): its messages.
    RunEnded(Vec<Value>),
    /// A dialog or notification of the worker (`/mcp`, the MCP gateway).
    ExtensionUi(Value),
}

/// Follows one session's RPC event stream (`rpc::events`' output) and says what each line means for ACP.
#[derive(Default)]
pub struct Translator {
    /// Arguments of the cells in progress, by tool call id.
    args: HashMap<String, Value>,
}

impl Translator {
    pub fn translate(&mut self, line: &Value) -> Vec<Action> {
        match line["type"].as_str().unwrap_or_default() {
            "message_update" => {
                let event = &line["assistantMessageEvent"];
                let kind = match event["type"].as_str() {
                    Some("text_delta") => "agent_message_chunk",
                    Some("thinking_delta") => "agent_thought_chunk",
                    _ => return Vec::new(),
                };
                match event["delta"].as_str().filter(|d| !d.is_empty()) {
                    Some(delta) => vec![Action::Update(chunk(kind, delta))],
                    None => Vec::new(),
                }
            }
            "tool_execution_start" => {
                let id = line["toolCallId"].as_str().unwrap_or_default().to_string();
                let name = line["toolName"].as_str().unwrap_or_default();
                let args = line["args"].clone();
                let update = tool_call(&id, name, &args);
                self.args.insert(id, args);
                vec![Action::CellStarted, Action::Update(update)]
            }
            "tool_execution_update" => {
                let id = line["toolCallId"].as_str().unwrap_or_default();
                let args = self.args.get(id).cloned().unwrap_or_else(|| line["args"].clone());
                vec![Action::Update(tool_update(id, &args, &line["partialResult"], None))]
            }
            "tool_execution_end" => {
                let id = line["toolCallId"].as_str().unwrap_or_default().to_string();
                let args = self.args.remove(&id).unwrap_or(Value::Null);
                vec![Action::CellEnded {
                    id,
                    args,
                    result: line["result"].clone(),
                    is_error: line["isError"] == true,
                }]
            }
            "agent_end" => {
                self.args.clear();
                vec![Action::RunEnded(
                    line["messages"].as_array().cloned().unwrap_or_default(),
                )]
            }
            "session_info_changed" => {
                vec![Action::Update(
                    json!({"sessionUpdate": "session_info_update", "title": line.get("name").cloned().unwrap_or(Value::Null)}),
                )]
            }
            "extension_ui_request" => vec![Action::ExtensionUi(line.clone())],
            _ => Vec::new(),
        }
    }
}

/// `session/load`'s replay of a stored conversation (Pi messages): user and agent text, thinking, and each cell as a
/// finished tool call (`failed` when it has no result).
pub fn replay(messages: &[Value]) -> Vec<Value> {
    let mut updates = Vec::new();
    let mut open: Vec<(String, String, Value)> = Vec::new();
    for message in messages {
        match message["role"].as_str().unwrap_or_default() {
            "user" => match &message["content"] {
                Value::String(text) => updates.push(chunk("user_message_chunk", text)),
                Value::Array(blocks) => {
                    for block in blocks {
                        match block["type"].as_str() {
                            Some("text") => {
                                updates.push(chunk("user_message_chunk", block["text"].as_str().unwrap_or_default()))
                            }
                            Some("image") => updates.push(json!({"sessionUpdate": "user_message_chunk",
                                "content": {"type": "image", "data": block["data"], "mimeType": block["mimeType"]}})),
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            "assistant" => {
                for block in message["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            let text = block["text"].as_str().unwrap_or_default();
                            if !text.is_empty() {
                                updates.push(chunk("agent_message_chunk", text));
                            }
                        }
                        Some("thinking") => {
                            let text = block["thinking"].as_str().unwrap_or_default();
                            if !text.is_empty() {
                                updates.push(chunk("agent_thought_chunk", text));
                            }
                        }
                        Some("toolCall") => {
                            let id = block["id"].as_str().unwrap_or_default().to_string();
                            let name = block["name"].as_str().unwrap_or_default().to_string();
                            updates.push(tool_call(&id, &name, &block["arguments"]));
                            open.push((id, name, block["arguments"].clone()));
                        }
                        _ => {}
                    }
                }
            }
            "toolResult" => {
                let id = message["toolCallId"].as_str().unwrap_or_default();
                let args = match open.iter().position(|(known, _, _)| known == id) {
                    Some(index) => open.remove(index).2,
                    None => continue,
                };
                let is_error = message["isError"] == true;
                updates.push(tool_update(
                    id,
                    &args,
                    message,
                    Some((is_error, &FileChanges::default())),
                ));
            }
            _ => {}
        }
    }
    for (id, _, args) in open {
        updates.push(tool_update(
            &id,
            &args,
            &Value::Null,
            Some((true, &FileChanges::default())),
        ));
    }
    updates
}

/// A diff block for `path` (absolute).
pub fn diff(path: &Path, old: Option<&str>, new: &str) -> Value {
    json!({"type": "diff", "path": path.to_string_lossy(), "oldText": old, "newText": new})
}
