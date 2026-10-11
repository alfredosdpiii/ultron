//! The client TUI (`ExperimentalClientTui` in experimental/client-tui.ts): the startup header and transcript above
//! a dock of pending messages, status, the editor (or a selector in its place) and the footer, laid out full
//! screen. Keys follow Pi's: Enter sends (steers while a turn runs), Alt+Enter queues a follow-up, Escape aborts the
//! turn, Ctrl+C clears and exits on a second press, Ctrl+D exits on an empty editor.
//!
//! The app only holds view state. Everything that touches the session goes through [`LaneClient`] and comes back as
//! [`LaneEvent`]s, so handling a key or painting a frame never waits on the network or the disk.

use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tui::attachments::{Attachments, PASTE_END, PASTE_START, PasteAction, chip_band, classify_paste, draft_chips};
use tui::autocomplete::{AutocompleteItem, SlashCommand};
use tui::clipboard::{ClipboardRead, ClipboardReply, ClipboardService};
use tui::components::{Component, Text};
use tui::editor::Editor;
use tui::keybindings::KeybindingsManager;
use tui::screen::{StackEntry, stack_lines};
use tui::select::{SelectEvent, SelectItem, SelectList, SelectListTheme};
use tui::status_git::GitSnapshot;
use tui::status_line::{StatusData, StreamState, ThemeStyle, Usage, VimDisplay, VimStatus, render_status_line};
use tui::theme::Theme;
use tui::utils::truncate_to_width;
use tui::viewport::{FramePlan, HistoryBatch, HistoryKind};
use tui::vim::VimMode;
use tui::vim_editor::VimEditor;

use crate::chat::{ChatStyle, ChatView, user_message_text};
use crate::commands::{COMMANDS, MarkdownNotice, SessionStats, hotkeys_markdown, key_display_text, session_info};
use crate::components::{FooterModel, FooterView, bold, fg, key_text, render_footer};
use crate::header::StartupHeader;
use crate::history::{PromptHistoryStore, TimedPrompt};
use crate::lane::{LaneClient, LaneEvent, ModelInfo, PromptImage, QueueKind, Queued, SessionListing};
use crate::qol::{GitSource, NotifyFn, QolSettings, code_highlighter, rlm_highlighter};
use crate::rlm_cell::Highlighter;

/// Pi exits on a second Ctrl-C within this window.
const CTRL_C_EXIT_WINDOW: Duration = Duration::from_millis(500);

/// What the app was started with.
pub struct AppOptions {
    pub theme: Rc<Theme>,
    /// Two managers over the same bindings: one for the editor, one for the app's own keys.
    pub keys: KeybindingsManager,
    pub editor_keys: KeybindingsManager,
    pub version: String,
    pub cwd: String,
    pub home: Option<String>,
    /// Shown as the header's `Server:` line.
    pub server_id: String,
    pub models: Vec<ModelInfo>,
    pub model: ModelInfo,
    pub thinking_level: String,
    pub splash: bool,
    pub history: PromptHistoryStore,
    pub fd_path: Option<std::path::PathBuf>,
    /// The `rlm` cell highlighter; replaced by syntect Python when `qol.highlight` is on.
    pub highlight: Highlighter,
    /// The Rust-only additions ([`QolSettings::parity`] paints exactly what the TypeScript client TUI paints).
    pub qol: QolSettings,
}

enum Purpose {
    Model,
    Resume(Vec<SessionListing>),
}

struct Selection {
    title: String,
    list: SelectList,
    purpose: Purpose,
}

pub struct App {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    editor: Editor,
    chat: ChatView,
    header: StartupHeader,
    client: Box<dyn LaneClient>,
    history: PromptHistoryStore,
    cwd: String,
    home: Option<String>,
    server_id: String,
    models: Vec<ModelInfo>,
    model: ModelInfo,
    thinking_level: String,
    status: String,
    /// The running operation's id.
    operation: Option<String>,
    selection: Option<Selection>,
    session_id: Option<String>,
    session_file: Option<String>,
    session_name: Option<String>,
    transcript: Vec<Value>,
    last_ctrl_c: Option<Instant>,
    /// The app asked to exit.
    pub exit: bool,
    /// Escape sequences for the terminal (clipboard), written by the run loop.
    pub terminal_writes: Vec<String>,
    /// Terminal rows (the editor shows at most 30% of them; the splash needs room).
    pub rows: usize,
    qol: QolSettings,
    vim: Option<VimEditor>,
    attachments: Attachments,
    clipboard: Option<ClipboardService>,
    git: Option<Box<dyn GitSource>>,
    git_snapshot: GitSnapshot,
    notify: Option<NotifyFn>,
    /// Terminal bytes from service threads (notifications), moved into `terminal_writes` by `poll_services`.
    side_writes: Arc<Mutex<Vec<String>>>,
    /// Transcript rows already in the terminal's scrollback (the scrollback/live split).
    committed: Vec<String>,
    history_id: u64,
    /// The user aborted the running turn (no notification for it).
    aborting: bool,
    turn_started: Option<Instant>,
    active_ms: u64,
    ticks: u64,
}

fn select_theme(t: &Rc<Theme>, scroll: &'static str, no_match: &'static str) -> SelectListTheme {
    let style = |color: &'static str| {
        let t = Rc::clone(t);
        Box::new(move |s: &str| fg(&t, color, s)) as Box<dyn Fn(&str) -> String>
    };
    SelectListTheme {
        selected_prefix: style("accent"),
        selected_text: style("accent"),
        description: style("muted"),
        scroll_info: style(scroll),
        no_match: style(no_match),
    }
}

fn thinking_border(t: &Rc<Theme>, level: &str) -> Box<dyn Fn(&str) -> String> {
    let t = Rc::clone(t);
    let level = level.to_string();
    Box::new(move |s| t.thinking_border(&level, s))
}

/// `calculateContextTokens`.
fn context_tokens(usage: &Value) -> u64 {
    let n = |k: &str| usage[k].as_u64().unwrap_or(0);
    match n("totalTokens") {
        0 => n("input") + n("output") + n("cacheRead") + n("cacheWrite"),
        total => total,
    }
}

/// `estimateTokens`: characters / 4, rounded up.
fn estimate_tokens(message: &Value) -> u64 {
    let text_chars = |content: &Value| -> usize {
        match content {
            Value::String(s) => s.encode_utf16().count(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b["text"].as_str())
                .map(|t| t.encode_utf16().count())
                .sum(),
            _ => 0,
        }
    };
    let chars = match message["role"].as_str() {
        Some("assistant") => message["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .map(|b| match b["type"].as_str() {
                        Some("text") => b["text"].as_str().unwrap_or("").encode_utf16().count(),
                        Some("thinking") => b["thinking"].as_str().unwrap_or("").encode_utf16().count(),
                        Some("toolCall") => {
                            b["name"].as_str().unwrap_or("").encode_utf16().count()
                                + b["arguments"].to_string().encode_utf16().count()
                        }
                        _ => 0,
                    })
                    .sum()
            })
            .unwrap_or(0),
        Some(_) => text_chars(&message["content"]),
        None => 0,
    };
    (chars as u64).div_ceil(4)
}

/// `laneContextUsage`: the last valid assistant usage plus estimates for the messages after it; `None` without a
/// context window.
pub fn context_usage(transcript: &[Value], window: u64) -> Option<(u64, f64)> {
    if window == 0 {
        return None;
    }
    let last = transcript.iter().rposition(|m| {
        m["role"] == "assistant"
            && m["stopReason"] != "aborted"
            && m["stopReason"] != "error"
            && context_tokens(&m["usage"]) > 0
    });
    let tokens = match last {
        Some(i) => {
            context_tokens(&transcript[i]["usage"]) + transcript[i + 1..].iter().map(estimate_tokens).sum::<u64>()
        }
        None => transcript.iter().map(estimate_tokens).sum(),
    };
    Some((tokens, tokens as f64 / window as f64 * 100.0))
}

impl App {
    pub fn new(options: AppOptions, client: Box<dyn LaneClient>) -> Self {
        let theme = options.theme;
        let mut editor = Editor::new(
            options.editor_keys,
            Default::default(),
            thinking_border(&theme, &options.thinking_level),
            1,
        );
        editor.set_select_list_theme(select_theme(&theme, "muted", "muted"));
        let commands = COMMANDS
            .iter()
            .map(|c| SlashCommand {
                name: c.name.to_string(),
                description: Some(c.description.to_string()),
                argument_hint: (!c.hint.is_empty()).then(|| c.hint.to_string()),
                argument_completions: None,
            })
            .collect::<Vec<_>>();
        let mut commands = commands;
        let qol = options.qol;
        let models = options.models.clone();
        if let Some(model) = commands.iter_mut().find(|c| c.name == "model") {
            model.argument_completions = Some(Box::new(move |prefix: &str| {
                let normalized = prefix.to_lowercase();
                Some(
                    models
                        .iter()
                        .filter(|m| {
                            format!("{}/{} {}", m.provider, m.id, m.name)
                                .to_lowercase()
                                .contains(&normalized)
                        })
                        .map(|m| AutocompleteItem {
                            value: format!("{}/{}", m.provider, m.id),
                            label: m.id.clone(),
                            description: Some(m.provider.clone()),
                        })
                        .collect(),
                )
            }));
        }
        editor.set_autocomplete_provider(crate::qol::autocomplete_provider(
            commands,
            &options.cwd,
            options.fd_path,
            &options.models,
            &qol,
        ));
        let mut history = options.history;
        let entries = history.load();
        editor.set_history(&entries.iter().map(String::as_str).collect::<Vec<_>>());
        let keys = options.keys;
        let style = ChatStyle {
            theme: Rc::clone(&theme),
            highlight: if qol.highlight {
                rlm_highlighter(&theme)
            } else {
                options.highlight
            },
            code_highlight: qol.highlight.then(|| code_highlighter(&theme)),
            expand_key: key_text(&keys, "app.tools.expand", false),
            dequeue_key: key_display_text(&keys, "app.message.dequeue"),
            interrupt_key: key_text(&keys, "app.interrupt", false),
        };
        App {
            header: StartupHeader {
                theme: Rc::clone(&theme),
                version: options.version,
                expanded: false,
                details: String::new(),
                splash: options.splash,
                rows: None,
            },
            chat: ChatView::new(style),
            theme,
            keys,
            editor,
            client,
            history,
            cwd: options.cwd,
            home: options.home,
            server_id: options.server_id,
            models: options.models,
            model: options.model,
            thinking_level: options.thinking_level,
            status: "Starting Session…".into(),
            operation: None,
            selection: None,
            session_id: None,
            session_file: None,
            session_name: None,
            transcript: Vec::new(),
            last_ctrl_c: None,
            exit: false,
            terminal_writes: Vec::new(),
            rows: 24,
            vim: qol.vim.then(|| VimEditor::new(Default::default())),
            qol,
            attachments: Attachments::default(),
            clipboard: None,
            git: None,
            git_snapshot: GitSnapshot::default(),
            notify: None,
            side_writes: Arc::default(),
            committed: Vec::new(),
            history_id: 0,
            aborting: false,
            turn_started: None,
            active_ms: 0,
            ticks: 0,
        }
    }

    // ---- Rust-only additions (see crate::qol) ----

    pub fn qol(&self) -> &QolSettings {
        &self.qol
    }

    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// The clipboard for image paste and Vim yanks.
    pub fn set_clipboard(&mut self, clipboard: ClipboardService) {
        if let Some(vim) = self.vim.as_mut() {
            vim.on_yank = Some(clipboard.yank_sink());
        }
        self.clipboard = Some(clipboard);
    }

    /// The status line's git fields.
    pub fn set_git_source(&mut self, source: Box<dyn GitSource>) {
        self.git = Some(source);
    }

    /// Where turn-end notifications go (with `qol.notifications`).
    pub fn set_notify_sink(&mut self, sink: NotifyFn) {
        self.notify = Some(sink);
    }

    /// A queue service threads push terminal bytes into; the paint loop writes them.
    pub fn side_writes(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.side_writes)
    }

    /// Images staged for the draft.
    pub fn attachments(&self) -> &Attachments {
        &self.attachments
    }

    /// The Vim mode, when Vim mode is on.
    pub fn vim_mode(&self) -> Option<VimMode> {
        self.vim.as_ref().map(VimEditor::mode)
    }

    /// The editor's text.
    pub fn editor_text(&self) -> String {
        self.editor.text()
    }

    /// Apply what background services produced (clipboard reads, git changes, notification bytes); true when the
    /// screen may have changed. Never blocks.
    pub fn poll_services(&mut self) -> bool {
        let mut changed = false;
        let replies: Vec<ClipboardReply> = self
            .clipboard
            .as_ref()
            .map(|c| std::iter::from_fn(|| c.try_reply()).collect())
            .unwrap_or_default();
        for reply in replies {
            match reply {
                ClipboardReply::Image(data) => {
                    let placeholder = self.attachments.add_image(data, "image/png", None);
                    self.editor.insert_text_at_cursor(&format!("{placeholder} "));
                }
                ClipboardReply::Text(text) => {
                    self.editor.handle_input(&format!("{PASTE_START}{text}{PASTE_END}"));
                }
                ClipboardReply::Error(e) => self.show_status(&format!("Error: {e}")),
            }
            self.update_editor_border();
            changed = true;
        }
        if let Some(git) = &self.git {
            let snapshot = git.snapshot();
            if snapshot != self.git_snapshot {
                self.git_snapshot = snapshot;
                changed = true;
            }
        }
        let writes = std::mem::take(&mut *self.side_writes.lock().unwrap_or_else(|e| e.into_inner()));
        if !writes.is_empty() {
            self.terminal_writes.extend(writes);
            changed = true;
        }
        changed
    }

    /// A paste whose every segment names a local image or video becomes attachments (`[Image #N, WxH]`).
    fn paste_attachments(&mut self, data: &str) -> bool {
        let Some(content) = data.strip_prefix(PASTE_START).and_then(|d| d.strip_suffix(PASTE_END)) else {
            return false;
        };
        let PasteAction::Attach(paths) = classify_paste(content) else {
            return false;
        };
        let mut placeholders = Vec::new();
        for path in &paths {
            match self.attachments.add_image_file(path) {
                Ok(placeholder) => placeholders.push(placeholder),
                Err(_) => return false,
            }
        }
        self.editor
            .insert_text_at_cursor(&format!("{} ", placeholders.join(" ")));
        true
    }

    /// The staged images `prompt` still references; the registry is emptied (the draft was sent).
    fn take_images(&mut self, prompt: &str) -> Vec<PromptImage> {
        let images = self
            .attachments
            .referenced(prompt)
            .into_iter()
            .map(|(_, image)| PromptImage {
                data: image.data_b64.clone(),
                mime_type: image.mime.clone(),
            })
            .collect();
        self.attachments.clear();
        images
    }

    fn status_data(&self) -> StatusData {
        let footer = self.footer_view();
        let context = context_usage(&self.transcript, self.model.context_window);
        let stream = if self.chat.tool_running() {
            StreamState::Running
        } else if self.chat.is_working() {
            StreamState::Streaming
        } else {
            StreamState::Idle
        };
        let vim = self.vim.as_ref().map(|v| {
            let indicator = v.indicator(&self.editor);
            VimStatus {
                mode: match indicator.mode {
                    VimMode::Insert => tui::status_line::VimMode::Insert,
                    VimMode::Normal => tui::status_line::VimMode::Normal,
                    VimMode::Visual => tui::status_line::VimMode::Visual,
                    VimMode::VisualLine => tui::status_line::VimMode::VisualLine,
                    VimMode::Replace => tui::status_line::VimMode::Replace,
                },
                pending: indicator.pending,
                selected_lines: indicator.selected_lines,
                display: VimDisplay::Text,
            }
        });
        StatusData {
            model_name: Some(self.model.name.clone()),
            thinking_level: self.model.reasoning.then(|| self.thinking_level.clone()),
            cwd: self.cwd.clone(),
            home: self.home.clone().unwrap_or_default(),
            git_branch: self.git_snapshot.branch.clone(),
            git_status: self.git_snapshot.status,
            pr: self.git_snapshot.pr.clone(),
            context_percent: context.map(|(_, percent)| percent),
            context_tokens: context.map_or(0, |(tokens, _)| tokens),
            context_window: self.model.context_window,
            auto_compact: true,
            usage: Usage {
                input: footer.input,
                output: footer.output,
                cache_read: footer.cache_read,
                cache_write: footer.cache_write,
                cost: footer.cost,
                ..Usage::default()
            },
            active_ms: self.active_ms,
            turn_elapsed_ms: self.turn_started.map(|t| t.elapsed().as_millis() as u64),
            now_ms: self.ticks * 80,
            session_id: self.session_id.clone(),
            session_name: self.session_name.clone(),
            stream,
            vim,
            ..StatusData::default()
        }
    }

    /// The status line row (with `qol.status_line`).
    pub fn status_line(&self, width: usize) -> Option<String> {
        if !self.qol.status_line {
            return None;
        }
        let line = render_status_line(
            &self.qol.status_layout.effective(),
            &self.status_data(),
            &self.qol.status_symbols.symbols(),
            &ThemeStyle(&self.theme),
            width,
        );
        (!line.is_empty()).then(|| truncate_to_width(&line, width, "", false))
    }

    /// Rows of `settled` not yet in scrollback: appended when the committed rows are still its head, else the whole
    /// of it replayed (a toggle, a resize or a session switch changed rows already written).
    fn commit(&mut self, settled: &[String]) -> Option<HistoryBatch> {
        let (rows, kind) = if settled.starts_with(&self.committed) {
            if settled.len() == self.committed.len() {
                return None;
            }
            (settled[self.committed.len()..].to_vec(), HistoryKind::Append)
        } else {
            (settled.to_vec(), HistoryKind::Replay)
        };
        self.committed = settled.to_vec();
        self.history_id += 1;
        Some(HistoryBatch {
            id: self.history_id,
            rows,
            kind,
        })
    }

    /// One frame of the scrollback/live split: finished transcript rows go to history once; the viewport is the
    /// live transcript (a streaming message, running cells) above the dock. The painter keeps the viewport's last
    /// `height` rows, so the editor and footer stay on screen. The batch it returns must be painted.
    pub fn frame_plan(&mut self, width: usize, height: usize) -> FramePlan {
        self.rows = height;
        let header = self.header_lines(width);
        let transcript = self.chat.render_transcript(width);
        let settled = if self.session_id.is_none() {
            0
        } else if self.operation.is_none() && !self.chat.is_working() {
            header.len() + transcript.len()
        } else {
            header.len() + self.chat.settled_lines()
        };
        let mut document = header;
        document.extend(transcript);
        let history = self.commit(&document[..settled]);
        let mut viewport = document.split_off(settled);
        for part in self.dock(width) {
            viewport.extend(part);
        }
        FramePlan { history, viewport }
    }

    /// The last frame of the split: the whole transcript in scrollback, the dock below it.
    pub fn exit_plan(&mut self, width: usize) -> FramePlan {
        let document = self.document(width);
        let history = self.commit(&document);
        FramePlan {
            history,
            viewport: self.dock(width).concat(),
        }
    }

    pub fn session_name(&self) -> Option<&str> {
        self.session_name.as_deref()
    }

    /// Whether something animates (the working spinner).
    pub fn animating(&self) -> bool {
        self.chat.is_working()
    }

    pub fn tick(&mut self) {
        self.ticks += 1;
        self.chat.tick();
    }

    /// Whether a debounced autocomplete request waits.
    pub fn fire_autocomplete_pending(&self) -> bool {
        self.editor.has_pending_autocomplete()
    }

    /// Fire the editor's debounced autocomplete request, when one waits.
    pub fn fire_autocomplete(&mut self) -> bool {
        if self.editor.has_pending_autocomplete() {
            self.editor.fire_autocomplete_debounce();
            return true;
        }
        false
    }

    fn update_editor_border(&mut self) {
        let bash = self.editor.text().trim_start().starts_with('!');
        self.editor.border_color = if bash {
            let t = Rc::clone(&self.theme);
            Box::new(move |s| t.bash_mode_border(s))
        } else {
            thinking_border(&self.theme, &self.thinking_level)
        };
    }

    /// Pi's terminal title: app, session name, cwd.
    fn update_title(&mut self) {
        let cwd = std::path::Path::new(&self.cwd)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let title = match &self.session_name {
            Some(name) => format!("ultron - {name} - {cwd}"),
            None => format!("ultron - {cwd}"),
        };
        self.terminal_writes.push(format!("\x1b]0;{title}\x07"));
    }

    // ---- lane events ----

    pub fn apply(&mut self, event: LaneEvent) {
        match event {
            LaneEvent::Opened(opened) => {
                let switched = self.session_id.is_some();
                self.chat.clear();
                self.transcript.clear();
                for message in &opened.messages {
                    self.chat.add_message(message);
                }
                self.transcript = opened.messages;
                self.session_id = Some(opened.session_id);
                self.session_file = opened.session_file;
                self.session_name = opened.name;
                self.operation = None;
                // A resumed session's own prompts join this project's history by time.
                let prompts: Vec<TimedPrompt> = self
                    .transcript
                    .iter()
                    .filter(|m| m["role"] == "user")
                    .map(|m| TimedPrompt {
                        text: user_message_text(m).trim().to_string(),
                        timestamp: m["timestamp"].as_i64().unwrap_or(0),
                    })
                    .filter(|p| !p.text.is_empty())
                    .collect();
                let history = self.history.history(&prompts);
                self.editor
                    .set_history(&history.iter().map(String::as_str).collect::<Vec<_>>());
                if !switched {
                    self.status.clear();
                }
                self.update_title();
            }
            LaneEvent::OperationStarted(id) => {
                self.operation = Some(id);
                self.aborting = false;
                self.turn_started = Some(Instant::now());
                self.chat.set_working(true);
            }
            LaneEvent::Streaming(message) => self.chat.set_streaming(&message),
            LaneEvent::Message(message) => {
                self.chat.add_message(&message);
                self.transcript.push(message);
            }
            LaneEvent::ToolStarted { id, name, args } => self.chat.tool_started(&name, &id, args),
            LaneEvent::ToolEnded { .. } => {}
            LaneEvent::Queues(queues) => self.chat.set_queues(queues),
            LaneEvent::OperationEnded { error } => {
                self.turn_ended(error.is_some());
                self.operation = None;
                self.chat.set_working(false);
                self.chat.set_queues(Vec::new());
                self.status = error.map(|e| format!("Operation failed: {e}")).unwrap_or_default();
            }
            LaneEvent::Name(name) => {
                self.session_name = name;
                self.update_title();
            }
            LaneEvent::Model(model) => {
                self.model = model;
            }
            LaneEvent::Sessions(sessions) => self.show_resume(sessions),
            LaneEvent::Status(text) => self.show_status(&text),
            LaneEvent::Error(text) => self.show_status(&format!("Error: {text}")),
        }
    }

    /// Turn bookkeeping of the additions: active time, the git refresh and the turn-end notification.
    fn turn_ended(&mut self, failed: bool) {
        if let Some(started) = self.turn_started.take() {
            self.active_ms += started.elapsed().as_millis() as u64;
        }
        if let Some(git) = &self.git {
            git.refresh();
        }
        let aborted = std::mem::take(&mut self.aborting);
        if !self.qol.notifications || aborted || self.operation.is_none() {
            return;
        }
        let errored = failed
            || self
                .transcript
                .iter()
                .rev()
                .find(|m| m["role"] == "assistant")
                .is_some_and(|m| m["stopReason"] == "error");
        let outcome = if errored {
            tui::notify::TurnOutcome::Error
        } else {
            tui::notify::TurnOutcome::Complete
        };
        let message = tui::notify::Message::turn(self.session_name.as_deref(), outcome);
        if let Some(notify) = self.notify.as_mut() {
            notify(message);
        }
    }

    /// `showStatus`: short text in the status line; more than three lines go to the transcript.
    fn show_status(&mut self, text: &str) {
        if text.split('\n').count() > 3 {
            self.chat.append_notice_text(text);
            self.status.clear();
        } else {
            self.status = text.to_string();
        }
    }

    fn notice(&mut self, text: &str) {
        self.chat.append_notice_text(text);
    }

    // ---- input ----

    pub fn handle_input(&mut self, data: &str) {
        if self.selection.is_some() {
            self.handle_select_input(data);
            return;
        }
        if self.qol.attachments && data.starts_with(PASTE_START) && self.paste_attachments(data) {
            return;
        }
        let k = |id: &str| self.keys.matches(data, id);
        if k("app.clipboard.pasteImage") {
            if self.qol.attachments
                && let Some(clipboard) = &self.clipboard
            {
                clipboard.request(ClipboardRead::ImageOrText);
            }
            return;
        }
        if k("app.interrupt") {
            if let Some(vim) = self.vim.as_mut()
                && vim.consumes_escape()
                && !self.editor.is_showing_autocomplete()
            {
                vim.handle_input(&mut self.editor, data);
                return;
            }
            if !self.editor.is_showing_autocomplete() {
                self.handle_escape();
                return;
            }
            self.feed_editor(data);
            return;
        }
        if k("app.exit") && self.editor.text().is_empty() {
            self.exit = true;
            return;
        }
        if !k("app.exit") && !k("tui.editor.historyPrevious") && !k("tui.editor.historyNext") {
            if k("app.clear") {
                self.handle_ctrl_c();
                return;
            }
            if k("app.model.select") {
                self.execute_slash_command("model", "");
                return;
            }
            if k("app.message.followUp") {
                let text = self.editor.expanded_text().trim().to_string();
                if !text.is_empty() {
                    self.editor.set_text("");
                    self.record_history(&text);
                    self.status = "Queueing follow-up…".into();
                    let id = self.client.follow_up(&text);
                    self.status = format!("Queued {id}.");
                }
                return;
            }
            if k("app.message.dequeue") {
                let restored = self.restore_queued_to_editor();
                self.show_status(&match restored {
                    0 => "No queued messages to restore".to_string(),
                    1 => "Restored 1 queued message to editor".to_string(),
                    n => format!("Restored {n} queued messages to editor"),
                });
                return;
            }
            if k("app.tools.expand") {
                let expanded = !self.chat.tools_expanded;
                self.chat.set_tools_expanded(expanded);
                self.header.expanded = expanded;
                return;
            }
            if k("app.message.copy") {
                self.copy_last_message();
                return;
            }
        }
        self.feed_editor(data);
    }

    fn feed_editor(&mut self, data: &str) {
        let vim = match self.vim.as_mut() {
            Some(vim) => vim.handle_input(&mut self.editor, data),
            None => false,
        };
        if !vim && let Some(text) = self.editor.handle_input(data) {
            self.run_prompt(&text);
        }
        self.update_editor_border();
    }

    fn handle_escape(&mut self) {
        if let Some(operation) = self.operation.clone() {
            self.restore_queued_to_editor();
            self.aborting = true;
            self.status = format!("Aborting {operation}…");
            self.client.abort();
        }
        // ponytail: Pi's double-Esc tree/fork selector is not ported.
    }

    fn handle_ctrl_c(&mut self) {
        let now = Instant::now();
        if self
            .last_ctrl_c
            .is_some_and(|t| now.duration_since(t) < CTRL_C_EXIT_WINDOW)
        {
            self.last_ctrl_c = None;
            self.exit = true;
            return;
        }
        self.last_ctrl_c = Some(now);
        self.editor.set_text("");
    }

    /// Queued user messages back into the editor (steering first); returns how many.
    fn restore_queued_to_editor(&mut self) -> usize {
        let queued: Vec<Queued> = self.chat.queues().to_vec();
        if queued.is_empty() {
            return 0;
        }
        let ordered: Vec<&Queued> = queued
            .iter()
            .filter(|q| q.kind == QueueKind::Steer)
            .chain(queued.iter().filter(|q| q.kind != QueueKind::Steer))
            .collect();
        let text = ordered.iter().map(|q| q.text.as_str()).collect::<Vec<_>>().join("\n\n");
        let current = self.editor.text();
        let combined = [text.as_str(), current.as_str()]
            .into_iter()
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.editor.set_text(&combined);
        for q in &ordered {
            self.client.cancel_queued(&q.entry_id);
        }
        self.chat.set_queues(Vec::new());
        ordered.len()
    }

    fn record_history(&mut self, text: &str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.history.append(text, now);
        self.editor.add_to_history(text);
    }

    fn run_prompt(&mut self, text: &str) {
        let prompt = text.trim().to_string();
        if prompt.is_empty() {
            return;
        }
        self.record_history(&prompt);
        if let Some(rest) = prompt.strip_prefix('!') {
            let command = rest.strip_prefix('!').unwrap_or(rest).trim();
            if !command.is_empty() {
                // ponytail: `!command` needs the session worker's bash; not ported yet.
                self.show_status("Error: ! commands are not available in the Rust build yet");
                return;
            }
        }
        if let Some(rest) = prompt.strip_prefix('/') {
            let (name, args) = match rest.split_once(' ') {
                Some((n, a)) => (n.to_string(), a.trim().to_string()),
                None => (rest.to_string(), String::new()),
            };
            self.execute_slash_command(&name, &args);
            return;
        }
        self.editor.set_text("");
        let images = self.take_images(&prompt);
        if self.operation.is_some() {
            self.status = "Queueing steering message…".into();
            let id = self.client.steer(&prompt);
            self.status = format!("Queued {id}.");
        } else {
            self.status = "Running turn…".into();
            if images.is_empty() {
                self.client.prompt(&prompt);
            } else {
                self.client.prompt_with_images(&prompt, images);
            }
        }
    }

    fn execute_slash_command(&mut self, name: &str, args: &str) {
        self.editor.set_text("");
        if !COMMANDS.iter().any(|c| c.name == name) {
            self.status = format!("Unknown slash command: /{name}");
            return;
        }
        match name {
            "name" => {
                let name = args.trim();
                if name.is_empty() {
                    match self.session_name.clone() {
                        Some(current) => self.notice(&fg(&self.theme, "dim", &format!("Session name: {current}"))),
                        None => self.show_status("Usage: /name <name>"),
                    }
                    return;
                }
                self.client.set_name(name);
                self.session_name = Some(name.to_string());
                self.update_title();
                self.notice(&fg(&self.theme, "dim", &format!("Session name set: {name}")));
            }
            "session" => {
                let Some(id) = self.session_id.clone() else {
                    return;
                };
                let stats = self.session_stats(id);
                let text = session_info(&self.theme, &stats);
                self.notice(&text);
            }
            "copy" => self.copy_last_message(),
            "hotkeys" => {
                let notice = MarkdownNotice {
                    theme: Rc::clone(&self.theme),
                    title: "Keyboard Shortcuts".into(),
                    markdown: hotkeys_markdown(&self.keys),
                };
                self.chat.append_notice(Box::new(notice));
            }
            "quit" => self.exit = true,
            "new" => {
                if self.operation.is_some() {
                    self.client.abort();
                }
                self.client.new_session();
            }
            "model" => self.model_command(args),
            "resume" => self.client.list_sessions(),
            other => self.show_status(&format!("Error: /{other} is not available in the Rust build yet")),
        }
    }

    fn model_command(&mut self, args: &str) {
        let exact = |query: &str| -> Option<ModelInfo> {
            let q = query.to_lowercase();
            let matches: Vec<&ModelInfo> = self
                .models
                .iter()
                .filter(|m| format!("{}/{}", m.provider, m.id).to_lowercase() == q || m.id.to_lowercase() == q)
                .collect();
            (matches.len() == 1).then(|| matches[0].clone())
        };
        if !args.is_empty() {
            match exact(args) {
                Some(model) => self.select_model(model),
                None => self.status = format!("Error: Unknown model: {args}"),
            }
            return;
        }
        let items: Vec<SelectItem> = self
            .models
            .iter()
            .map(|m| SelectItem {
                value: format!("{}/{}", m.provider, m.id),
                label: if m.provider == self.model.provider && m.id == self.model.id {
                    format!("{} (selected)", m.name)
                } else {
                    m.name.clone()
                },
                description: Some(format!("{}/{}", m.provider, m.id)),
            })
            .collect();
        let selected = format!("{}/{}", self.model.provider, self.model.id);
        self.open_selection("Select model:", items, Some(&selected), Purpose::Model);
    }

    fn select_model(&mut self, model: ModelInfo) {
        self.client.select_model(&model.provider, &model.id);
        self.status = format!("Selected {}/{}.", model.provider, model.id);
        self.model = model;
    }

    fn show_resume(&mut self, sessions: Vec<SessionListing>) {
        // The picker lists this working directory's sessions only (no all-projects toggle), with their names.
        let items = sessions
            .iter()
            .map(|s| SelectItem {
                value: s.path.clone(),
                label: s.name.clone().unwrap_or_else(|| {
                    if s.first_message.is_empty() {
                        "(no messages)".into()
                    } else {
                        s.first_message.split_whitespace().collect::<Vec<_>>().join(" ")
                    }
                }),
                description: Some(format!(
                    "{} message{}",
                    s.message_count,
                    if s.message_count == 1 { "" } else { "s" }
                )),
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            self.show_status("No sessions in this directory");
            return;
        }
        let current = self.session_file.clone();
        self.open_selection("Resume Session", items, current.as_deref(), Purpose::Resume(sessions));
    }

    fn open_selection(&mut self, title: &str, items: Vec<SelectItem>, selected: Option<&str>, purpose: Purpose) {
        let index = selected.and_then(|v| items.iter().position(|i| i.value == v));
        let mut list = SelectList::new(
            items.clone(),
            items.len().clamp(1, 12),
            select_theme(&self.theme, "dim", "warning"),
        );
        if let Some(index) = index {
            list.set_selected_index(index);
        }
        self.editor.focused = false;
        self.selection = Some(Selection {
            title: title.to_string(),
            list,
            purpose,
        });
    }

    fn handle_select_input(&mut self, data: &str) {
        let Some(selection) = self.selection.as_mut() else {
            return;
        };
        match selection.list.handle_input(data) {
            Some(SelectEvent::Selected(item)) => {
                let selection = self.selection.take().expect("selection is open");
                match selection.purpose {
                    Purpose::Model => {
                        if let Some(model) = self
                            .models
                            .iter()
                            .find(|m| format!("{}/{}", m.provider, m.id) == item.value)
                            .cloned()
                        {
                            self.select_model(model);
                        }
                    }
                    Purpose::Resume(sessions) => {
                        let Some(session) = sessions.iter().find(|s| s.path == item.value) else {
                            return;
                        };
                        if Some(&session.id) == self.session_id.as_ref() {
                            self.show_status("Already in this session");
                        } else {
                            self.client.resume(&session.path);
                            self.status = "Resumed session".into();
                        }
                    }
                }
            }
            Some(SelectEvent::Cancelled) => self.selection = None,
            _ => {}
        }
    }

    fn last_assistant_text(&self) -> Option<String> {
        self.transcript
            .iter()
            .rev()
            .filter(|m| m["role"] == "assistant")
            .find(|m| !(m["stopReason"] == "aborted" && m["content"].as_array().is_none_or(Vec::is_empty)))
            .map(|m| {
                m["content"]
                    .as_array()
                    .map(|blocks| {
                        blocks
                            .iter()
                            .filter(|b| b["type"] == "text")
                            .filter_map(|b| b["text"].as_str())
                            .collect::<String>()
                    })
                    .unwrap_or_default()
            })
    }

    fn copy_last_message(&mut self) {
        match self.last_assistant_text().filter(|t| !t.is_empty()) {
            None => self.show_status("Error: No agent messages to copy yet."),
            Some(text) => {
                // OSC 52: the terminal copies (works over SSH and in tmux with set-clipboard).
                self.terminal_writes
                    .push(format!("\x1b]52;c;{}\x07", base64(text.as_bytes())));
                self.show_status("Copied last agent message to clipboard");
            }
        }
    }

    fn session_stats(&self, session_id: String) -> SessionStats {
        let mut s = SessionStats {
            session_file: self.session_file.clone(),
            session_id,
            name: self.session_name.clone(),
            ..SessionStats::default()
        };
        for m in &self.transcript {
            match m["role"].as_str() {
                Some("user") => s.user_messages += 1,
                Some("assistant") => {
                    s.assistant_messages += 1;
                    let u = &m["usage"];
                    s.input += u["input"].as_u64().unwrap_or(0);
                    s.output += u["output"].as_u64().unwrap_or(0);
                    s.cache_read += u["cacheRead"].as_u64().unwrap_or(0);
                    s.cache_write += u["cacheWrite"].as_u64().unwrap_or(0);
                    s.cost += u["cost"]["total"].as_f64().unwrap_or(0.0);
                    s.tool_calls += m["content"]
                        .as_array()
                        .map_or(0, |b| b.iter().filter(|b| b["type"] == "toolCall").count());
                }
                Some("toolResult") => s.tool_results += 1,
                _ => {}
            }
        }
        s.context = context_usage(&self.transcript, self.model.context_window)
            .map(|(tokens, percent)| (Some(tokens), self.model.context_window, Some(percent)));
        s
    }

    // ---- rendering ----

    fn footer_view(&self) -> FooterView {
        let mut v = FooterView {
            cwd: self.cwd.clone(),
            home: self.home.clone(),
            session_name: self.session_name.clone(),
            thinking_level: self.thinking_level.clone(),
            auto_compact: true,
            model: Some(FooterModel {
                id: self.model.id.clone(),
                provider: self.model.provider.clone(),
                reasoning: self.model.reasoning,
                context_window: self.model.context_window,
            }),
            ..FooterView::default()
        };
        let mut providers: Vec<&str> = self.models.iter().map(|m| m.provider.as_str()).collect();
        providers.sort_unstable();
        providers.dedup();
        v.provider_count = providers.len();
        for m in self.transcript.iter().filter(|m| m["role"] == "assistant") {
            let u = &m["usage"];
            v.input += u["input"].as_u64().unwrap_or(0);
            v.output += u["output"].as_u64().unwrap_or(0);
            v.cache_read += u["cacheRead"].as_u64().unwrap_or(0);
            v.cache_write += u["cacheWrite"].as_u64().unwrap_or(0);
            v.cost += u["cost"]["total"].as_f64().unwrap_or(0.0);
        }
        if let Some(last) = self.transcript.iter().rev().find(|m| m["role"] == "assistant") {
            let u = &last["usage"];
            let read = u["cacheRead"].as_f64().unwrap_or(0.0);
            let prompt = u["input"].as_f64().unwrap_or(0.0) + read + u["cacheWrite"].as_f64().unwrap_or(0.0);
            if prompt > 0.0 {
                v.latest_cache_hit_rate = Some(read / prompt * 100.0);
            }
        }
        if let Some((_, percent)) = context_usage(&self.transcript, self.model.context_window) {
            v.context_percent = Some(Some(percent));
            v.context_window = Some(self.model.context_window);
        }
        v
    }

    /// The transcript document: the startup header, then the conversation.
    fn document(&mut self, width: usize) -> Vec<String> {
        let mut lines = self.header_lines(width);
        lines.extend(self.chat.render_transcript(width));
        lines
    }

    fn header_lines(&mut self, width: usize) -> Vec<String> {
        self.header.details = match &self.session_id {
            Some(id) => fg(
                &self.theme,
                "dim",
                &format!("Server: {}\nSession: {id}", self.server_id),
            ),
            None => String::new(),
        };
        self.header.rows = Some(self.rows);
        self.header.render(&self.keys, width)
    }

    /// The dock under the transcript: pending messages, status, editor (or selector) and footer.
    fn dock(&mut self, width: usize) -> Vec<Vec<String>> {
        let pending = self.chat.render_pending(width);
        let mut status = Vec::new();
        if !self.status.is_empty() {
            status.extend(Text::new(&fg(&self.theme, "dim", &self.status), 1, 0, None).render(width));
        }
        status.extend(self.chat.render_status(width));
        let editor = match &mut self.selection {
            Some(selection) => {
                let mut lines = Text::new(&bold(&selection.title), 1, 1, None).render(width);
                lines.extend(selection.list.render(width));
                lines
            }
            None => {
                self.editor.focused = true;
                self.editor.terminal_rows = self.rows;
                self.editor.render(width)
            }
        };
        let footer = render_footer(&self.theme, &self.footer_view(), width);
        let mut parts = vec![pending, status];
        if let Some(line) = self.status_line(width) {
            parts.push(vec![line]);
        }
        if self.qol.attachments && self.selection.is_none() {
            let text = self.editor.text();
            let chips = chip_band(&draft_chips(&text, self.editor.pastes(), &self.attachments), width);
            if !chips.is_empty() {
                parts.push(chips);
            }
        }
        parts.push(editor);
        parts.push(footer);
        parts
    }

    /// One full-screen frame of `height` rows.
    pub fn render(&mut self, width: usize, height: usize) -> Vec<String> {
        self.rows = height;
        let document = self.document(width);
        let dock = self.dock(width);
        // The editor (second to last) keeps at least three rows.
        let editor = dock.len() - 2;
        let dock_entries: Vec<StackEntry> = (0..dock.len())
            .map(|i| StackEntry {
                min_size: if i == editor { 3 } else { 0 },
                ..StackEntry::default()
            })
            .collect();
        let dock_height: usize =
            tui::screen::allocate_stack_sizes(&dock_entries, &dock.iter().map(Vec::len).collect::<Vec<_>>(), None)
                .iter()
                .sum();
        let root = [
            StackEntry {
                basis: Some(0),
                grow: 1,
                min_size: 1,
                ..StackEntry::default()
            },
            StackEntry {
                min_size: 1,
                ..StackEntry::default()
            },
        ];
        let sizes = tui::screen::allocate_stack_sizes(&root, &[document.len(), dock_height], Some(height));
        let dock_lines = stack_lines(dock, &dock_entries, &[], sizes[1]);
        stack_lines(vec![document, dock_lines], &root, &[true, false], height)
    }

    /// What stays on the main screen after exit: the layout rendered without a height, where the transcript gets its
    /// minimum of one row (the header's first line) and the dock its natural height.
    pub fn exit_document(&mut self, width: usize) -> Vec<String> {
        let mut lines: Vec<String> = self.document(width).into_iter().take(1).collect();
        for part in self.dock(width) {
            lines.extend(part);
        }
        lines
    }
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
