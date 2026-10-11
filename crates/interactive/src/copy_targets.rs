//! What `/copy` can copy out of a transcript turn (oh-my-pi packages/tui/src/overlays/copy-targets.ts and the
//! block/label helpers of overlays/copy-selector.ts): fenced code, `>` quotes, http(s) links, bash/eval/rlm
//! commands and tool output, plus the whole-turn payload and its one-line summary.
//!
//! Ultron's model has one tool, `rlm`, whose `code` argument is Python; its cells count as commands next to omp's
//! `bash` and `eval`.

use serde_json::Value;
use tui::markdown::Token;
use tui::marked::lex;

/// A persisted transcript entry as the client receives it: id, ISO timestamp and the agent message.
#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptEntry {
    pub id: String,
    pub timestamp: String,
    pub message: Value,
}

impl TranscriptEntry {
    pub fn new(id: &str, timestamp: &str, message: Value) -> Self {
        TranscriptEntry {
            id: id.into(),
            timestamp: timestamp.into(),
            message,
        }
    }

    pub fn role(&self) -> &str {
        self.message["role"].as_str().unwrap_or("")
    }
}

/// A drillable block of markdown, in document order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageBlock {
    /// Fenced code: info string (trimmed) and body without the trailing newline.
    Code { lang: String, code: String },
    /// A run of `>` lines with each marker (and one optional space) removed.
    Quote { text: String },
}

/// `extractBlocks`: fenced code and `>`-quoted runs. Fences mask their bodies; an unclosed fence is text.
pub fn extract_blocks(text: &str) -> Vec<MessageBlock> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut blocks = Vec::new();
    let mut quote: Option<Vec<String>> = None;
    let flush = |quote: &mut Option<Vec<String>>, blocks: &mut Vec<MessageBlock>| {
        if let Some(q) = quote.take() {
            blocks.push(MessageBlock::Quote { text: q.join("\n") });
        }
    };
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(info) = line.strip_prefix("```")
            && let Some(close) = (i + 1..lines.len()).find(|&k| lines[k].starts_with("```"))
        {
            flush(&mut quote, &mut blocks);
            blocks.push(MessageBlock::Code {
                lang: info.trim().to_string(),
                code: lines[i + 1..close].join("\n"),
            });
            i = close + 1;
            continue;
        }
        if let Some(rest) = line.strip_prefix('>') {
            quote
                .get_or_insert_with(Vec::new)
                .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        } else {
            flush(&mut quote, &mut blocks);
        }
        i += 1;
    }
    flush(&mut quote, &mut blocks);
    blocks
}

/// `(lang, code)` of each fenced block.
pub fn extract_code_blocks(text: &str) -> Vec<(String, String)> {
    extract_blocks(text)
        .into_iter()
        .filter_map(|b| match b {
            MessageBlock::Code { lang, code } => Some((lang, code)),
            MessageBlock::Quote { .. } => None,
        })
        .collect()
}

/// The text of each quote run.
pub fn extract_quote_blocks(text: &str) -> Vec<String> {
    extract_blocks(text)
        .into_iter()
        .filter_map(|b| match b {
            MessageBlock::Quote { text } => Some(text),
            MessageBlock::Code { .. } => None,
        })
        .collect()
}

/// A hyperlink the renderer draws: its flattened label (the href when empty) and target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkTarget {
    pub text: String,
    pub href: String,
}

fn plain_inline(tokens: &[Token]) -> String {
    let mut out = String::new();
    for token in tokens {
        match token {
            Token::Text { tokens: Some(t), .. } if !t.is_empty() => out.push_str(&plain_inline(t)),
            Token::Text { text, .. } => out.push_str(text),
            Token::Strong { tokens } | Token::Em { tokens } | Token::Del { tokens } | Token::Link { tokens, .. } => {
                out.push_str(&plain_inline(tokens))
            }
            Token::Codespan { text } | Token::Escape { text, .. } => out.push_str(text),
            Token::Br => out.push('\n'),
            Token::Html { raw } => out.push_str(raw),
            Token::Other { text: Some(text), .. } => out.push_str(text),
            _ => {}
        }
    }
    out
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn walk_links(tokens: &[Token], links: &mut Vec<(String, String)>) {
    for token in tokens {
        match token {
            Token::Link { href, tokens, .. } => {
                if !href.is_empty() {
                    let label = collapse_whitespace(&plain_inline(tokens));
                    links.push((if label.is_empty() { href.clone() } else { label }, href.clone()));
                }
            }
            Token::Heading { tokens, .. }
            | Token::Paragraph { tokens }
            | Token::Blockquote { tokens }
            | Token::Strong { tokens }
            | Token::Em { tokens }
            | Token::Del { tokens }
            | Token::Text {
                tokens: Some(tokens), ..
            } => walk_links(tokens, links),
            Token::List { items, .. } => {
                for item in items {
                    walk_links(&item.tokens, links);
                }
            }
            Token::Table { header, rows, .. } => {
                for cell in header {
                    walk_links(cell, links);
                }
                for row in rows {
                    for cell in row {
                        walk_links(cell, links);
                    }
                }
            }
            _ => {}
        }
    }
}

/// `extractLinks`: http(s) links the renderer would draw, in order, deduplicated by href. The renderer's own lexer
/// decides, so code spans, fences, escapes, reference definitions and GFM autolinks agree with the screen.
pub fn extract_links(text: &str) -> Vec<LinkTarget> {
    let mut found = Vec::new();
    walk_links(&lex(text), &mut found);
    let mut links: Vec<LinkTarget> = Vec::new();
    for (label, href) in found {
        let lower = href.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) || links.iter().any(|l| l.href == href) {
            continue;
        }
        links.push(LinkTarget { text: label, href });
    }
    links
}

/// Command kinds a tool call can carry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    Bash,
    Eval,
    /// Ultron's `rlm` cell.
    Rlm,
}

/// A runnable command found in the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastCommand {
    pub kind: CommandKind,
    pub code: String,
    /// Highlight language.
    pub language: String,
}

fn eval_code(args: &Value) -> Option<(String, String)> {
    let cells: Vec<&Value> = match (&args["cells"], &args["code"]) {
        (Value::Array(cells), _) => cells.iter().collect(),
        (_, Value::String(_)) => vec![args],
        _ => return None,
    };
    let mut blocks = Vec::new();
    let mut language: Option<&str> = None;
    for cell in cells {
        let Some(code) = cell["code"].as_str().filter(|c| !c.is_empty()) else {
            continue;
        };
        blocks.push(code);
        if language.is_none() {
            language = Some(if cell["language"] == "js" {
                "javascript"
            } else {
                "python"
            });
        }
    }
    (!blocks.is_empty()).then(|| (blocks.join("\n\n"), language.unwrap_or("python").to_string()))
}

/// `commandFromToolCall`.
pub fn command_from_tool_call(name: &str, args: &Value) -> Option<LastCommand> {
    match name {
        "bash" => args["command"].as_str().map(|c| LastCommand {
            kind: CommandKind::Bash,
            code: c.to_string(),
            language: "bash".into(),
        }),
        "eval" => eval_code(args).map(|(code, language)| LastCommand {
            kind: CommandKind::Eval,
            code,
            language,
        }),
        "rlm" => args["code"].as_str().filter(|c| !c.is_empty()).map(|c| LastCommand {
            kind: CommandKind::Rlm,
            code: c.to_string(),
            language: "python".into(),
        }),
        _ => None,
    }
}

fn tool_calls(message: &Value) -> impl Iterator<Item = &Value> {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "toolCall")
}

/// `assistantText`: concatenated text blocks of an assistant message, verbatim; `None` when blank.
pub fn assistant_text(message: &Value) -> Option<String> {
    if message["role"] != "assistant" {
        return None;
    }
    let text: String = message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    (!text.trim().is_empty()).then_some(text)
}

/// `extractLastCommand`: the newest bash/eval/rlm command.
pub fn extract_last_command(messages: &[Value]) -> Option<LastCommand> {
    messages
        .iter()
        .rev()
        .filter(|m| m["role"] == "assistant")
        .find_map(|m| {
            let calls: Vec<&Value> = tool_calls(m).collect();
            calls
                .iter()
                .rev()
                .find_map(|c| command_from_tool_call(c["name"].as_str().unwrap_or(""), &c["arguments"]))
        })
}

/// `extractLastLink`: the last link of the newest assistant message that has one.
pub fn extract_last_link(messages: &[Value]) -> Option<LinkTarget> {
    messages
        .iter()
        .rev()
        .filter_map(assistant_text)
        .find_map(|text| extract_links(&text).pop())
}

/// `extractLastCodeBlock`.
pub fn extract_last_code_block(messages: &[Value]) -> Option<(String, String)> {
    messages
        .iter()
        .rev()
        .filter_map(assistant_text)
        .find_map(|text| extract_code_blocks(&text).pop())
}

pub const SEP_DOT: &str = " · ";

/// The kind of a copy block, when it is not plain output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Code,
    Quote,
    Command,
}

/// One copyable inner block of a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyBlock {
    /// Short label (`ts code`, `bash command`, `read result`, `link · docs`).
    pub label: String,
    /// Exact clipboard text.
    pub content: String,
    /// Index of the entry that produced it.
    pub entry: usize,
    pub kind: Option<BlockKind>,
    pub language: Option<String>,
    /// Link blocks: the URL `o` opens (`content` is the same URL).
    pub href: Option<String>,
}

fn text_blocks(content: &Value, sep: &str) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .map(|b| b["text"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join(sep),
        _ => String::new(),
    }
}

fn assistant_visible(message: &Value) -> String {
    text_blocks(&message["content"], "").trim().to_string()
}

fn tool_result_text(message: &Value) -> String {
    text_blocks(&message["content"], "\n").trim().to_string()
}

fn push_markdown_blocks(blocks: &mut Vec<CopyBlock>, text: &str, entry: usize) {
    for block in extract_blocks(text) {
        blocks.push(match block {
            MessageBlock::Code { lang, code } => CopyBlock {
                label: if lang.is_empty() {
                    "code".into()
                } else {
                    format!("{lang} code")
                },
                content: code,
                entry,
                kind: Some(BlockKind::Code),
                language: (!lang.is_empty()).then_some(lang),
                href: None,
            },
            MessageBlock::Quote { text } => CopyBlock {
                label: "quote".into(),
                content: text,
                entry,
                kind: Some(BlockKind::Quote),
                language: None,
                href: None,
            },
        });
    }
    for link in extract_links(text) {
        blocks.push(CopyBlock {
            label: if link.text != link.href {
                format!("link{SEP_DOT}{}", link.text)
            } else {
                "link".into()
            },
            content: link.href.clone(),
            entry,
            kind: None,
            language: None,
            href: Some(link.href),
        });
    }
}

fn plain_block(label: &str, content: &str, entry: usize, language: Option<&str>) -> CopyBlock {
    CopyBlock {
        label: label.into(),
        content: content.into(),
        entry,
        kind: None,
        language: language.map(str::to_string),
        href: None,
    }
}

/// `collectBlocks`: markdown code/quotes/links, commands and tool output of the entries `indices` of `entries`.
pub fn collect_blocks(entries: &[TranscriptEntry], indices: &[usize]) -> Vec<CopyBlock> {
    let mut blocks = Vec::new();
    for &index in indices {
        let message = &entries[index].message;
        match message["role"].as_str() {
            Some("user") => push_markdown_blocks(&mut blocks, &text_blocks(&message["content"], "\n"), index),
            Some("assistant") => {
                push_markdown_blocks(&mut blocks, &assistant_visible(message), index);
                for call in tool_calls(message) {
                    if let Some(command) =
                        command_from_tool_call(call["name"].as_str().unwrap_or(""), &call["arguments"])
                    {
                        let label = match command.kind {
                            CommandKind::Bash => "bash command",
                            CommandKind::Eval => "eval code",
                            CommandKind::Rlm => "rlm code",
                        };
                        blocks.push(CopyBlock {
                            label: label.into(),
                            content: command.code,
                            entry: index,
                            kind: Some(BlockKind::Command),
                            language: Some(command.language),
                            href: None,
                        });
                    }
                }
            }
            Some("toolResult") => {
                let text = tool_result_text(message);
                if !text.is_empty() {
                    let label = format!("{} result", message["toolName"].as_str().unwrap_or(""));
                    blocks.push(plain_block(&label, &text, index, None));
                }
            }
            Some("bashExecution") => {
                blocks.push(plain_block(
                    "command",
                    message["command"].as_str().unwrap_or(""),
                    index,
                    Some("bash"),
                ));
                let output = message["output"].as_str().unwrap_or("");
                if !output.trim().is_empty() {
                    blocks.push(plain_block("output", output, index, None));
                }
            }
            Some("pythonExecution") => {
                blocks.push(plain_block(
                    "eval code",
                    message["code"].as_str().unwrap_or(""),
                    index,
                    Some("python"),
                ));
                let output = message["output"].as_str().unwrap_or("");
                if !output.trim().is_empty() {
                    blocks.push(plain_block("output", output, index, None));
                }
            }
            _ => {}
        }
    }
    blocks
}

/// `targetCopy`: the clipboard payload of a whole turn (opened by `entries[first]`), falling back to its blocks.
pub fn target_copy(entries: &[TranscriptEntry], first: usize, blocks: &[CopyBlock]) -> (String, String) {
    let message = &entries[first].message;
    let joined = |a: &str, b: &str| {
        [a, b]
            .iter()
            .filter(|p| !p.trim().is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join("\n")
    };
    match message["role"].as_str() {
        Some("user") => return (text_blocks(&message["content"], "\n"), "user message".into()),
        Some("assistant") => {
            let text = assistant_visible(message);
            if !text.is_empty() {
                return (text, "assistant message".into());
            }
        }
        Some("toolResult") => {
            let text = tool_result_text(message);
            if !text.is_empty() {
                return (text, format!("{} result", message["toolName"].as_str().unwrap_or("")));
            }
        }
        Some("bashExecution") => {
            let s = |k: &str| message[k].as_str().unwrap_or("").to_string();
            return (joined(&s("command"), &s("output")), "bash execution".into());
        }
        Some("pythonExecution") => {
            let s = |k: &str| message[k].as_str().unwrap_or("").to_string();
            return (joined(&s("code"), &s("output")), "eval execution".into());
        }
        Some("compactionSummary") | Some("branchSummary") => {
            return (message["summary"].as_str().unwrap_or("").to_string(), "summary".into());
        }
        Some("custom") | Some("hookMessage") => {
            let content = text_blocks(&message["content"], "\n");
            if !content.trim().is_empty() {
                return (content, "message".into());
            }
        }
        _ => {}
    }
    (
        blocks
            .iter()
            .map(|b| b.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        "turn content".into(),
    )
}

/// First non-blank line, whitespace collapsed.
pub fn first_line(source: &str) -> String {
    source
        .split('\n')
        .find(|l| !l.trim().is_empty())
        .map(collapse_whitespace)
        .unwrap_or_default()
}

/// `turnSummary`: one-line label of the turn an entry opens.
pub fn turn_summary(entry: &TranscriptEntry) -> String {
    let message = &entry.message;
    match message["role"].as_str() {
        Some("user") => collapse_whitespace(&text_blocks(&message["content"], " ")),
        Some("assistant") => {
            let prose: String = text_blocks(&message["content"], "");
            let tools: Vec<&str> = tool_calls(message).filter_map(|c| c["name"].as_str()).collect();
            let line = first_line(&prose);
            if !line.is_empty() {
                line
            } else if !tools.is_empty() {
                tools.join(", ")
            } else {
                "thinking".into()
            }
        }
        Some("toolResult") => format!("{} result", message["toolName"].as_str().unwrap_or("")),
        Some("bashExecution") => format!("$ {}", first_line(message["command"].as_str().unwrap_or(""))),
        Some("pythonExecution") => first_line(message["code"].as_str().unwrap_or("")),
        Some("compactionSummary") => "Compaction summary".into(),
        Some("branchSummary") => "Branch summary".into(),
        Some("custom") | Some("hookMessage") => {
            let line = first_line(&text_blocks(&message["content"], " "));
            if line.is_empty() {
                message["customType"].as_str().unwrap_or("").to_string()
            } else {
                line
            }
        }
        _ => entry.id.clone(),
    }
}

/// A user message with prompt text (`userMessageHasText`): a turn rewind can re-edit.
pub fn is_user_turn(message: &Value) -> bool {
    if message["role"] != "user" {
        return false;
    }
    match &message["content"] {
        Value::String(s) => !s.trim().is_empty(),
        Value::Array(blocks) => blocks
            .iter()
            .any(|b| b["type"] == "text" && !b["text"].as_str().unwrap_or("").trim().is_empty()),
        _ => false,
    }
}

/// Argument keys naming what a tool call works on, in preference order.
const TOOL_TARGET_KEYS: [&str; 7] = ["command", "pattern", "path", "file_path", "query", "url", "code"];

/// `toolCallLabel`: a tool call's name and main argument (`grep ack`, `bash ls -la`).
pub fn tool_call_label(name: &str, args: &Value) -> String {
    TOOL_TARGET_KEYS
        .iter()
        .find_map(|k| args[*k].as_str().filter(|v| !v.trim().is_empty()))
        .map_or_else(|| name.to_string(), |v| format!("{name} {}", first_line(v)))
}
