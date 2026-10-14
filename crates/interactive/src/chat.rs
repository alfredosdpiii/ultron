//! The transcript of the interactive TUI (`ExperimentalChatView` in experimental/client-tui-chat.ts): user and
//! assistant messages, tool calls drawn by their renderers (the `rlm` cell renderer for `rlm`), notices from
//! commands, the queued-message display and the working indicator, driven by [`LaneEvent`]s.

use std::rc::Rc;

use serde_json::Value;
use tui::components::{Component, Spacer, Text, TruncatedText};
use tui::select::Loader;
use tui::theme::Theme;

use crate::components::{
    AssistantBlock, AssistantMessage, AssistantView, BashExecution, ToolContent, ToolExecution, ToolResultView,
    UserMessage, fg,
};
use crate::lane::{QueueKind, Queued};
use crate::rlm_cell::{Highlighter, rlm_renderers};

/// Text of a user message's text blocks.
pub fn user_message_text(message: &Value) -> String {
    match &message["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}

/// What `AssistantMessageComponent` reads from an assistant message.
pub fn assistant_view(message: &Value) -> AssistantView {
    let content = message["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| match b["type"].as_str() {
                    Some("text") => Some(AssistantBlock::Text(b["text"].as_str().unwrap_or("").to_string())),
                    Some("thinking") => Some(AssistantBlock::Thinking(
                        b["thinking"].as_str().unwrap_or("").to_string(),
                    )),
                    Some("toolCall") => Some(AssistantBlock::ToolCall),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    AssistantView {
        content,
        stop_reason: message["stopReason"].as_str().unwrap_or("stop").to_string(),
        error_message: message["errorMessage"].as_str().map(str::to_string),
    }
}

fn tool_result_view(message: &Value) -> ToolResultView {
    let content = message["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| match b["type"].as_str() {
                    Some("text") => Some(ToolContent::Text(b["text"].as_str().unwrap_or("").to_string())),
                    Some("image") => Some(ToolContent::Image {
                        data: b["data"].as_str().unwrap_or("").to_string(),
                        mime_type: b["mimeType"].as_str().map(str::to_string),
                    }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    ToolResultView {
        content,
        is_error: message["isError"] == true,
    }
}

struct Tool {
    name: String,
    id: String,
    args: Value,
    result: Option<ToolResultView>,
    partial: bool,
    started: bool,
    args_complete: bool,
}

enum Kind {
    Spacer,
    User(String),
    Assistant(AssistantView),
    Tool(Tool),
    /// A `Text` with horizontal padding 1 (notices and command output).
    Text(String),
    Component(Box<dyn Component>),
    /// A recorded `!` command (`bashExecution` message); follows the tool expansion toggle.
    Bash(Box<BashExecution>),
}

struct Item {
    kind: Kind,
    cache: Option<(usize, Vec<String>)>,
}

/// The rendering context components need.
pub struct ChatStyle {
    pub theme: Rc<Theme>,
    pub highlight: Highlighter,
    /// Fenced code highlighting (Rust-only addition).
    pub code_highlight: Option<crate::qol::CodeHighlight>,
    /// Formatted `app.tools.expand` key.
    pub expand_key: String,
    /// Formatted `app.message.dequeue` key, capitalized (`keyDisplayText`).
    pub dequeue_key: String,
    /// Formatted `app.interrupt` key.
    pub interrupt_key: String,
}

pub struct ChatView {
    style: ChatStyle,
    items: Vec<Item>,
    /// The item index of the streaming assistant message.
    streaming: Option<usize>,
    queues: Vec<Queued>,
    indicator: Option<Loader>,
    /// The indicator is a summary's retry countdown (`RetryStatusIndicator`): its message and when it started.
    retry: Option<RetryCountdown>,
    pub tools_expanded: bool,
    pub hide_thinking: bool,
    /// Formatted `tui.select.cancel` key (the running `!` command's hint).
    pub cancel_key: String,
}

impl ChatView {
    pub fn new(style: ChatStyle) -> Self {
        ChatView {
            style,
            items: Vec::new(),
            streaming: None,
            queues: Vec::new(),
            indicator: None,
            retry: None,
            tools_expanded: false,
            hide_thinking: false,
            cancel_key: "escape".into(),
        }
    }

    /// A copy of the rendering context (for overlays that replay the transcript).
    pub fn style(&self) -> ChatStyle {
        let s = &self.style;
        ChatStyle {
            theme: Rc::clone(&s.theme),
            highlight: Rc::clone(&s.highlight),
            code_highlight: s.code_highlight.clone(),
            expand_key: s.expand_key.clone(),
            dequeue_key: s.dequeue_key.clone(),
            interrupt_key: s.interrupt_key.clone(),
        }
    }

    /// A `!` command's component (`BashExecutionComponent`), running.
    pub fn bash_component(&self, command: &str, exclude: bool) -> BashExecution {
        let mut bash = BashExecution::new(
            Rc::clone(&self.style.theme),
            command,
            exclude,
            &self.style.expand_key,
            &self.cancel_key,
        );
        bash.expanded = self.tools_expanded;
        bash
    }

    /// A transcript entry as the client TUI draws it (`#addEntry`): compactions with their retained tail, branch
    /// summaries, custom entries, messages.
    pub fn add_entry(&mut self, entry: &Value) {
        let t = Rc::clone(&self.style.theme);
        match entry["type"].as_str() {
            Some("compaction") => {
                let tokens = match &entry["tokensBefore"] {
                    Value::Null => "undefined".to_string(),
                    v => v.to_string(),
                };
                self.append_notice_text(&fg(
                    &t,
                    "muted",
                    &format!("[compaction] compacted from {tokens} tokens"),
                ));
                for retained in entry["retainedTail"].as_array().into_iter().flatten() {
                    self.add_message(retained);
                }
            }
            Some("branch_summary") => {
                self.append_notice_text(&fg(&t, "muted", "[branch summary]"));
                self.append_notice_text(entry["summary"].as_str().unwrap_or(""));
            }
            Some("custom") => {
                let kind = entry["customType"].as_str().unwrap_or("");
                self.append_notice_text(&fg(&t, "muted", &format!("[{kind}]")));
            }
            _ => self.add_message(&entry["message"]),
        }
    }

    fn push(&mut self, kind: Kind) -> usize {
        self.items.push(Item { kind, cache: None });
        self.items.len() - 1
    }

    /// Append a message that joined the transcript.
    pub fn add_message(&mut self, message: &Value) {
        match message["role"].as_str() {
            Some("user") => {
                self.push(Kind::Spacer);
                self.push(Kind::User(user_message_text(message)));
            }
            Some("assistant") => {
                let view = assistant_view(message);
                let index = match self.streaming.take() {
                    Some(i) => i,
                    None => self.push(Kind::Assistant(view.clone())),
                };
                self.items[index].kind = Kind::Assistant(view);
                self.items[index].cache = None;
                for call in tool_calls(message) {
                    let i = self.tool(&call.0, &call.1, Some(call.2));
                    if let Kind::Tool(t) = &mut self.items[i].kind {
                        t.args_complete = true;
                    }
                    self.items[i].cache = None;
                }
            }
            Some("toolResult") => {
                let name = message["toolName"].as_str().unwrap_or("");
                let id = message["toolCallId"].as_str().unwrap_or("");
                let i = self.tool(name, id, None);
                if let Kind::Tool(t) = &mut self.items[i].kind {
                    t.result = Some(tool_result_view(message));
                    t.partial = false;
                }
                self.items[i].cache = None;
            }
            Some("bashExecution") => {
                let mut bash = self.bash_component(
                    message["command"].as_str().unwrap_or(""),
                    message["excludeFromContext"] == true,
                );
                if let Some(output) = message["output"].as_str().filter(|o| !o.is_empty()) {
                    bash.append_output(output);
                }
                bash.set_complete(
                    message["exitCode"].as_i64(),
                    message["cancelled"] == true,
                    message["truncated"] == true,
                    message["fullOutputPath"].as_str(),
                );
                self.push(Kind::Bash(Box::new(bash)));
            }
            // Other injected context is shown muted, so what reached the model is visible.
            Some("custom") if message["display"] == true => {
                let text = match &message["content"] {
                    Value::String(s) => s.clone(),
                    Value::Array(parts) => parts
                        .iter()
                        .map(|p| {
                            if p["type"] == "text" {
                                p["text"].as_str().unwrap_or("")
                            } else {
                                ""
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => String::new(),
                };
                let kind = message["customType"].as_str().unwrap_or("");
                let t = Rc::clone(&self.style.theme);
                self.append_notice_text(&fg(&t, "muted", &format!("[{kind}]\n{text}")));
            }
            _ => {}
        }
    }

    /// The streaming assistant message so far.
    pub fn set_streaming(&mut self, message: &Value) {
        let view = assistant_view(message);
        let index = match self.streaming {
            Some(i) => i,
            None => {
                let i = self.push(Kind::Assistant(view.clone()));
                self.streaming = Some(i);
                i
            }
        };
        self.items[index].kind = Kind::Assistant(view);
        self.items[index].cache = None;
        for call in tool_calls(message) {
            self.tool(&call.0, &call.1, Some(call.2));
        }
    }

    pub fn tool_started(&mut self, name: &str, id: &str, args: Value) {
        let i = self.tool(name, id, Some(args));
        if let Kind::Tool(t) = &mut self.items[i].kind {
            t.started = true;
        }
        self.items[i].cache = None;
    }

    /// The tool item for `id`, created after the transcript so far when new; `args` replace its arguments.
    fn tool(&mut self, name: &str, id: &str, args: Option<Value>) -> usize {
        if let Some(i) = self
            .items
            .iter()
            .position(|item| matches!(&item.kind, Kind::Tool(t) if t.id == id))
        {
            if let (Some(args), Kind::Tool(t)) = (args, &mut self.items[i].kind) {
                t.args = args;
                self.items[i].cache = None;
            }
            return i;
        }
        self.push(Kind::Tool(Tool {
            name: name.to_string(),
            id: id.to_string(),
            args: args.unwrap_or_else(|| Value::Object(Default::default())),
            result: None,
            partial: true,
            started: false,
            args_complete: false,
        }))
    }

    /// A client-side notice (command output) after the transcript so far.
    pub fn append_notice_text(&mut self, text: &str) {
        self.push(Kind::Spacer);
        self.push(Kind::Text(text.to_string()));
    }

    pub fn append_notice(&mut self, component: Box<dyn Component>) {
        self.push(Kind::Spacer);
        self.push(Kind::Component(component));
    }

    pub fn set_queues(&mut self, queues: Vec<Queued>) {
        self.queues = queues;
    }

    pub fn queues(&self) -> &[Queued] {
        &self.queues
    }

    /// Show or hide the working indicator (`Working... (escape to abort)`).
    pub fn set_working(&mut self, working: bool) {
        if !working {
            self.indicator = None;
            self.retry = None;
            return;
        }
        if self.indicator.is_none() {
            let t = Rc::clone(&self.style.theme);
            let t2 = Rc::clone(&self.style.theme);
            self.indicator = Some(Loader::new(
                Box::new(move |s| fg(&t, "accent", s)),
                Box::new(move |s| fg(&t2, "muted", s)),
                &format!("Working... ({} to abort)", self.style.interrupt_key),
                None,
            ));
        }
    }

    /// A summary's retry countdown (`RetryStatusIndicator`): `Retrying (attempt/maxAttempts) in Ns... (escape to
    /// cancel)`, counting down each second from `ceil(delayMs / 1000)` (`CountdownTimer`).
    pub fn set_retry_indicator(&mut self, attempt: u64, max_attempts: u64, delay_ms: u64) {
        let countdown = RetryCountdown {
            attempt,
            max_attempts,
            seconds: delay_ms.div_ceil(1000),
            started: std::time::Instant::now(),
            interrupt_key: self.style.interrupt_key.clone(),
        };
        let t = Rc::clone(&self.style.theme);
        let t2 = Rc::clone(&self.style.theme);
        self.indicator = Some(Loader::new(
            Box::new(move |s| fg(&t, "warning", s)),
            Box::new(move |s| fg(&t2, "muted", s)),
            &countdown.message(countdown.seconds),
            None,
        ));
        self.retry = Some(countdown);
    }

    /// `clearStatusIndicator("retry")`: only a retry countdown is cleared.
    pub fn clear_retry_indicator(&mut self) {
        if self.retry.take().is_some() {
            self.indicator = None;
            self.retry = None;
        }
    }

    /// `BranchSummaryStatusIndicator`: `Summarizing branch... (escape to cancel)`.
    pub fn set_branch_summary_indicator(&mut self) {
        let label = format!("Summarizing branch... ({} to cancel)", self.style.interrupt_key);
        let t = Rc::clone(&self.style.theme);
        let t2 = Rc::clone(&self.style.theme);
        self.retry = None;
        self.indicator = Some(Loader::new(
            Box::new(move |s| fg(&t, "accent", s)),
            Box::new(move |s| fg(&t2, "muted", s)),
            &label,
            None,
        ));
    }

    /// Show or hide the compaction indicator (`Compacting context... (escape to cancel)`).
    pub fn set_compacting(&mut self, compacting: bool) {
        if compacting {
            self.set_compacting_for("manual");
        } else {
            self.indicator = None;
        }
    }

    /// Show the indicator of a compaction for `reason` (`CompactionStatusIndicator`): `Compacting context...` for
    /// a manual one, `Auto-compacting...` for a threshold one, `Context overflow detected, Auto-compacting...` for
    /// an overflow one.
    pub fn set_compacting_for(&mut self, reason: &str) {
        let cancel = format!("({} to cancel)", self.style.interrupt_key);
        let label = match reason {
            "manual" => format!("Compacting context... {cancel}"),
            "overflow" => format!("Context overflow detected, Auto-compacting... {cancel}"),
            _ => format!("Auto-compacting... {cancel}"),
        };
        let t = Rc::clone(&self.style.theme);
        let t2 = Rc::clone(&self.style.theme);
        self.retry = None;
        self.indicator = Some(Loader::new(
            Box::new(move |s| fg(&t, "accent", s)),
            Box::new(move |s| fg(&t2, "muted", s)),
            &label,
            None,
        ));
    }

    /// Whether a started tool (an `rlm` cell) has no result yet.
    pub fn tool_running(&self) -> bool {
        self.items
            .iter()
            .any(|item| matches!(&item.kind, Kind::Tool(t) if t.started && t.result.is_none()))
    }

    /// Lines of the transcript's final head, as of the last [`ChatView::render_transcript`]: items before the
    /// streaming message or the first tool without a result. Those no longer change (until a toggle or resize
    /// re-renders everything).
    pub fn settled_lines(&self) -> usize {
        let mut lines = 0;
        for (i, item) in self.items.iter().enumerate() {
            let live =
                self.streaming == Some(i) || matches!(&item.kind, Kind::Tool(t) if t.result.is_none() || t.partial);
            let Some((_, cached)) = item.cache.as_ref().filter(|_| !live) else {
                break;
            };
            lines += cached.len();
        }
        lines
    }

    pub fn is_working(&self) -> bool {
        self.indicator.is_some()
    }

    /// Advance the spinner one frame.
    pub fn tick(&mut self) {
        if let Some(i) = &mut self.indicator {
            i.tick();
            if let Some(retry) = &self.retry {
                let elapsed = retry.started.elapsed().as_secs();
                i.set_message(&retry.message(retry.seconds.saturating_sub(elapsed)));
            }
        }
    }

    pub fn set_tools_expanded(&mut self, expanded: bool) {
        self.tools_expanded = expanded;
        for item in &mut self.items {
            if let Kind::Bash(bash) = &mut item.kind {
                bash.expanded = expanded;
            }
            item.cache = None;
        }
    }

    /// Pi's Ctrl+T: show or hide thinking blocks in every assistant message.
    pub fn set_hide_thinking(&mut self, hide: bool) {
        self.hide_thinking = hide;
        for item in &mut self.items {
            item.cache = None;
        }
    }

    /// Drop everything (a new or switched session).
    pub fn clear(&mut self) {
        self.items.clear();
        self.streaming = None;
        self.queues.clear();
        self.indicator = None;
        self.retry = None;
    }

    pub fn render_transcript(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        for i in 0..self.items.len() {
            if let Some((w, cached)) = &self.items[i].cache
                && *w == width
            {
                lines.extend(cached.iter().cloned());
                continue;
            }
            let rendered = self.render_item(i, width);
            lines.extend(rendered.iter().cloned());
            self.items[i].cache = Some((width, rendered));
        }
        lines
    }

    /// Transcript items so far: selectors map each entry to the items it added.
    pub fn item_count(&self) -> usize {
        self.items.len()
    }

    /// Rows of every item at `width` (cached like [`ChatView::render_transcript`]).
    pub fn render_item_rows(&mut self, width: usize) -> Vec<Vec<String>> {
        (0..self.items.len())
            .map(|i| {
                if let Some((w, cached)) = &self.items[i].cache
                    && *w == width
                {
                    return cached.clone();
                }
                let rendered = self.render_item(i, width);
                self.items[i].cache = Some((width, rendered.clone()));
                rendered
            })
            .collect()
    }

    fn render_item(&mut self, i: usize, width: usize) -> Vec<String> {
        let theme = Rc::clone(&self.style.theme);
        let expanded = self.tools_expanded;
        let hide_thinking = self.hide_thinking;
        let style = &self.style;
        match &mut self.items[i].kind {
            Kind::Spacer => Spacer(1).render(width),
            Kind::User(text) => UserMessage {
                theme,
                text: text.clone(),
                output_pad: 1,
            }
            .render(width),
            Kind::Assistant(view) => AssistantMessage {
                theme,
                message: view.clone(),
                hide_thinking,
                hidden_thinking_label: "Thinking...".into(),
                output_pad: 1,
                thinking_overrides: Vec::new(),
                code_highlight: style.code_highlight.clone(),
            }
            .render(width),
            Kind::Tool(t) => {
                let renderers =
                    (t.name == "rlm").then(|| rlm_renderers(&theme, Rc::clone(&style.highlight), &style.expand_key));
                ToolExecution {
                    theme,
                    tool_name: t.name.clone(),
                    args: t.args.clone(),
                    result: t.result.clone(),
                    is_partial: t.partial,
                    expanded,
                    show_images: false,
                    execution_started: t.started,
                    args_complete: t.args_complete,
                    renderers,
                    expand_key: style.expand_key.clone(),
                }
                .render(width)
            }
            Kind::Text(text) => Text::new(text, 1, 0, None).render(width),
            Kind::Component(c) => c.render(width),
            Kind::Bash(bash) => bash.render(width),
        }
    }

    /// Pending messages: steering first, then follow-ups, then the dequeue hint.
    pub fn render_pending(&self, width: usize) -> Vec<String> {
        if self.queues.is_empty() {
            return Vec::new();
        }
        let t = &self.style.theme;
        let mut lines = vec![String::new()];
        let ordered = self
            .queues
            .iter()
            .filter(|q| q.kind == QueueKind::Steer)
            .chain(self.queues.iter().filter(|q| q.kind != QueueKind::Steer));
        for q in ordered {
            let label = match q.kind {
                QueueKind::Steer => "Steering",
                QueueKind::FollowUp => "Follow-up",
            };
            let text: Vec<&str> = q.text.split_whitespace().collect();
            let line = fg(t, "dim", &format!("{label}: {}", text.join(" ")));
            lines.extend(truncated(line, width));
        }
        let hint = format!("↳ {} to edit all queued messages", self.style.dequeue_key);
        lines.extend(truncated(fg(t, "dim", &hint), width));
        lines
    }

    pub fn render_status(&mut self, width: usize) -> Vec<String> {
        self.indicator.as_mut().map(|i| i.render(width)).unwrap_or_default()
    }
}

/// A summary retry's countdown (`RetryStatusIndicator` with its `CountdownTimer`).
struct RetryCountdown {
    attempt: u64,
    max_attempts: u64,
    seconds: u64,
    started: std::time::Instant,
    interrupt_key: String,
}

impl RetryCountdown {
    fn message(&self, seconds: u64) -> String {
        format!(
            "Retrying ({}/{}) in {seconds}s... ({} to cancel)",
            self.attempt, self.max_attempts, self.interrupt_key
        )
    }
}

fn truncated(text: String, width: usize) -> Vec<String> {
    TruncatedText {
        text,
        padding_x: 1,
        padding_y: 0,
    }
    .render(width)
}

/// `(name, id, arguments)` of each tool call in an assistant message.
fn tool_calls(message: &Value) -> Vec<(String, String, Value)> {
    message["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b["type"] == "toolCall")
                .map(|b| {
                    (
                        b["name"].as_str().unwrap_or("").to_string(),
                        b["id"].as_str().unwrap_or("").to_string(),
                        b["arguments"].clone(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}
