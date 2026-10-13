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

use crate::agent_hub::{AgentHub, HubEvent};
use crate::chat::{ChatStyle, ChatView, user_message_text};
use crate::commands::{COMMANDS, MarkdownNotice, SessionStats, hotkeys_markdown, key_display_text, session_info};
use crate::components::{BashExecution, FooterModel, FooterView, bold, fg, key_text, render_footer};
use crate::copy_targets::TranscriptEntry;
use crate::header::StartupHeader;
use crate::history::{PromptHistoryStore, TimedPrompt};
use crate::history_search::{HistorySearchEvent, PromptHistorySearch};
use crate::lane::{BashResult, LaneClient, LaneEvent, ModelInfo, PromptImage, QueueKind, Queued, SessionListing};
use crate::model_hub::{ModelHub, ModelHubEvent};
use crate::qol::{GitSource, NotifyFn, QolSettings, code_highlighter, rlm_highlighter};
use crate::rlm_cell::Highlighter;
use crate::selectors::{SelectorAction, UserMessageSelector};
use crate::session_selector::{SessionAction, SessionSelector};
use crate::settings_selector::{SettingsEvent, SettingsInput, SettingsSelector, format_http_idle};
use crate::transcript_picker::{PickerEvent, PickerMode, PickerOptions, TranscriptPicker};
use crate::tree_selector::{FilterMode, TreeAction, TreeSelector};

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
    Thinking,
    /// "Summarize branch?" before navigating to this entry.
    Summarize(String),
    /// "Select authentication method": the options it chooses among (`None`: every provider).
    LoginMethod(Vec<crate::selectors::AuthProvider>, bool),
}

/// `THINKING_DESCRIPTIONS` of slash-commands-provider.ts.
fn thinking_description(level: &str) -> Option<&'static str> {
    Some(match level {
        "off" => "No reasoning",
        "minimal" => "Very brief reasoning",
        "low" => "Light reasoning",
        "medium" => "Moderate reasoning",
        "high" => "Deep reasoning",
        "xhigh" => "Extra-high reasoning",
        "max" => "Maximum reasoning",
        _ => return None,
    })
}

/// A full-screen overlay (the omp-inspired additions, [`QolSettings::overlays`]).
enum Overlay {
    Search(Box<tui::transcript_search::TranscriptSearch>),
    Picker(Box<TranscriptPicker>),
    ModelHub(Box<ModelHub>),
    AgentHub(Box<AgentHub>),
}

/// A component shown in the editor's place (`#showComponent`).
enum Panel {
    Fork(UserMessageSelector),
    HistorySearch(Box<PromptHistorySearch>),
    Tree(Box<TreeSelector>),
    Sessions(Box<SessionSelector>),
    Settings(Box<SettingsSelector>),
    /// The full-screen RLM graph (`RlmGraphFocus`) in the editor's place.
    RlmFocus(Box<crate::rlm::RlmGraphFocus>),
    Logout(Box<crate::selectors::AuthSelector>),
    /// The `/login` provider selector; `Some(auth type)` when a method was chosen first (Esc goes back to it).
    Login(
        Box<crate::selectors::AuthSelector>,
        Vec<crate::selectors::AuthProvider>,
        Option<String>,
    ),
    /// The login dialog's prompt for an API key.
    LoginPrompt {
        provider: crate::selectors::AuthProvider,
        method: String,
        input: Box<tui::input::Input>,
    },
    /// A one-line text prompt (`#showTextEditor`): the custom branch summary instructions for `entry`.
    Instructions {
        entry: String,
        input: Box<tui::input::Input>,
    },
}

/// `nextModel` (client-tui-actions.ts): the next model in the `enabledModels` scope when it matches any, else among
/// all available models.
pub fn next_model<'a>(
    available: &'a [ModelInfo],
    current: &ModelInfo,
    direction: isize,
    enabled: &[String],
) -> Result<&'a ModelInfo, &'static str> {
    let mut candidates: Vec<&ModelInfo> = available.iter().collect();
    let mut scoped = false;
    if !enabled.is_empty() {
        let in_scope: Vec<&ModelInfo> = available
            .iter()
            .filter(|m| enabled.iter().any(|p| model_matches(p, m)))
            .collect();
        if !in_scope.is_empty() {
            scoped = true;
            candidates = in_scope;
        }
    }
    if candidates.len() <= 1 {
        return Err(if scoped {
            "Only one model in scope"
        } else {
            "Only one model available"
        });
    }
    let index = candidates
        .iter()
        .position(|m| m.provider == current.provider && m.id == current.id)
        .map_or(-1, |i| i as isize);
    let n = candidates.len() as isize;
    Ok(candidates[((index + direction + n) % n) as usize])
}

/// An `enabledModels` pattern against a model: `provider/id` or `id`, with `*` globs, case-insensitive.
fn model_matches(pattern: &str, model: &ModelInfo) -> bool {
    let pattern = pattern.to_lowercase();
    let pattern = pattern.split(':').next().unwrap_or("");
    let full = format!("{}/{}", model.provider, model.id).to_lowercase();
    let id = model.id.to_lowercase();
    glob(pattern, &full) || glob(pattern, &id)
}

fn glob(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, rest)) => {
            let Some(tail) = text.strip_prefix(head) else {
                return false;
            };
            (0..=tail.len())
                .filter(|i| tail.is_char_boundary(*i))
                .any(|i| glob(rest, &tail[i..]))
        }
    }
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
    /// The running `!` command (Pi's pending `BashExecutionComponent`).
    bash: Option<BashExecution>,
    /// Transcript messages before this index are compacted away (the context starts after them).
    context_from: usize,
    compacted: bool,
    panel: Option<Panel>,
    /// `enabledModels` (Ctrl+P's scope).
    enabled_models: Vec<String>,
    overlay: Option<Overlay>,
    /// Esc-Esc on an idle, empty editor (the rewind selector).
    escapes: tui::qol_keys::DoubleTap,
    /// Rewind waits for the session's user-message entries.
    rewind_pending: bool,
    /// The rewind selector's turns: their ids and whether each is a session user entry.
    rewind_ids: Vec<(String, bool)>,
    /// Models picked this session, newest first (the model hub's Recent scope).
    recent_models: Vec<String>,
    /// The terminal display should be reset (Alt+L): the run loop clears it and paints everything again.
    pub reset_display: bool,
    /// The last tree `/tree` showed (to reopen it after a cancelled summary choice).
    tree: Option<(Value, Option<String>)>,
    tree_filter: FilterMode,
    /// `branchSummary.skipPrompt`: navigate without asking about a summary.
    skip_summary_prompt: bool,
    /// The text a navigation puts back into an empty editor (a selected user message).
    navigation_text: Option<String>,
    /// Usage of the path's branch summaries (`{input, output, cacheRead, cacheWrite, cost}`), in the footer's totals.
    path_usage: Value,
    /// `compaction.enabled` (the footer's `(auto)`).
    auto_compact: bool,
    /// The external editor command (`externalEditor`, `$VISUAL`, `$EDITOR`).
    external_editor: Option<String>,
    /// Ctrl+G asked for the external editor over this text; the run loop runs it.
    edit_request: Option<String>,
    /// Set while something else owns the terminal: the input pump stops reading.
    input_gate: Arc<std::sync::atomic::AtomicBool>,
    /// The width of the last frame (transcript search renders the transcript at it).
    cols: usize,
    /// The RLM panel shows (`app.rlm.toggle`).
    rlm_visible: bool,
    /// The last subagent listing (`rlm.list_subagents`).
    rlm_agents: Value,
    rlm_clock: crate::rlm::RlmClock,
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
            bash: None,
            context_from: 0,
            compacted: false,
            panel: None,
            enabled_models: Vec::new(),
            overlay: None,
            escapes: Default::default(),
            rewind_pending: false,
            rewind_ids: Vec::new(),
            recent_models: Vec::new(),
            reset_display: false,
            tree: None,
            tree_filter: FilterMode::Default,
            skip_summary_prompt: false,
            navigation_text: None,
            path_usage: Value::Null,
            auto_compact: true,
            external_editor: None,
            edit_request: None,
            input_gate: Arc::default(),
            cols: 80,
            rlm_visible: false,
            rlm_agents: Value::Null,
            rlm_clock: crate::rlm::RlmClock::new(),
        }
        .with_cancel_key()
    }

    fn with_cancel_key(mut self) -> Self {
        self.chat.cancel_key = key_text(&self.keys, "tui.select.cancel", false);
        self
    }

    /// Pi's `hideThinkingBlock` setting.
    pub fn set_hide_thinking(&mut self, hide: bool) {
        self.chat.set_hide_thinking(hide);
    }

    /// The external editor command (`getExternalEditorCommand`).
    pub fn set_external_editor(&mut self, command: Option<String>) {
        self.external_editor = command.filter(|c| !c.trim().is_empty());
    }

    /// The flag that pauses the input pump ([`crate::run::spawn_gated_input`]).
    pub fn input_gate(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.input_gate)
    }

    /// The pending Ctrl+G request: `(command, text)`.
    pub fn take_edit_request(&mut self) -> Option<(String, String)> {
        let content = self.edit_request.take()?;
        Some((self.external_editor.clone()?, content))
    }

    /// The editor's result: the new editor text, or `None` (the editor failed: the text stays).
    pub fn finish_edit(&mut self, edited: Option<String>) {
        if let Some(text) = edited {
            self.editor.set_text(&text);
            self.update_editor_border();
        }
    }

    /// `compaction.enabled`.
    pub fn set_auto_compact(&mut self, enabled: bool) {
        self.auto_compact = enabled;
    }

    /// Pi's `treeFilterMode` and `branchSummary.skipPrompt` settings.
    pub fn set_tree_settings(&mut self, filter: &str, skip_summary_prompt: bool) {
        self.tree_filter = FilterMode::parse(filter);
        self.skip_summary_prompt = skip_summary_prompt;
    }

    /// Pi's `enabledModels` setting: the models Ctrl+P cycles through.
    pub fn set_enabled_models(&mut self, patterns: Vec<String>) {
        self.enabled_models = patterns;
    }

    /// The context use the footer shows: `(tokens, percent)`, both unknown right after a compaction
    /// (`laneContextUsage`); `None` without a context window.
    fn context(&self) -> Option<(Option<u64>, Option<f64>)> {
        let window = self.model.context_window;
        let after = &self.transcript[self.context_from.min(self.transcript.len())..];
        let measured = after.iter().any(|m| {
            m["role"] == "assistant"
                && m["stopReason"] != "aborted"
                && m["stopReason"] != "error"
                && context_tokens(&m["usage"]) > 0
        });
        if window > 0 && self.compacted && !measured {
            return Some((None, None));
        }
        context_usage(after, window).map(|(tokens, percent)| (Some(tokens), Some(percent)))
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
        let context = self.context();
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
            context_percent: context.and_then(|(_, percent)| percent),
            context_tokens: context.and_then(|(tokens, _)| tokens).unwrap_or(0),
            context_window: self.model.context_window,
            auto_compact: self.auto_compact,
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
        self.cols = width;
        if let Some(frame) = self.overlay_frame(width, height) {
            return FramePlan {
                history: None,
                viewport: frame,
            };
        }
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
        self.chat.is_working() || self.bash.is_some()
    }

    pub fn tick(&mut self) {
        self.ticks += 1;
        self.chat.tick();
        if let Some(bash) = self.bash.as_mut() {
            bash.tick();
        }
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
                self.context_from = 0;
                self.compacted = false;
                self.panel = None;
                self.path_usage = Value::Null;
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
            LaneEvent::Entries(entries) => self.load_entries(&entries),
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
            LaneEvent::ToolStarted { id, name, args } => {
                self.chat.tool_started(&name, &id, args);
                self.refresh_rlm();
            }
            // The task tree changes on tool boundaries (`#refreshRlm` on tool_start / tool_end).
            LaneEvent::ToolEnded { .. } => self.refresh_rlm(),
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
            LaneEvent::Thinking(level) => {
                self.thinking_level = level;
                self.update_editor_border();
            }
            LaneEvent::Notice(text) => self.notice(&text),
            LaneEvent::BashDone(result) => self.bash_done(result),
            LaneEvent::CompactionStarted => self.chat.set_compacting(true),
            LaneEvent::Compacted(result) => self.compacted(result),
            LaneEvent::Tree { tree, leaf } => self.show_tree(tree, leaf, None),
            LaneEvent::RlmStatus(listed) => self.rlm_agents = listed,
            LaneEvent::Settings {
                settings,
                env_overrides,
                themes,
                terminal_theme,
            } => {
                let input = SettingsInput {
                    settings,
                    env_overrides,
                    models: self.models.clone(),
                    session: (!self.model.id.is_empty()).then(|| self.model.clone()),
                    themes,
                    terminal_theme,
                    supports_images: false,
                    follow_up_key: key_display_text(&self.keys, "app.message.followUp"),
                    cycle_thinking_key: key_display_text(&self.keys, "app.thinking.cycle"),
                };
                let selector = SettingsSelector::new(Rc::clone(&self.theme), self.keys.clone(), input);
                self.panel = Some(Panel::Settings(Box::new(selector)));
                self.editor.focused = false;
            }
            LaneEvent::LoginProviders { providers, query } => self.login(providers, &query),
            LaneEvent::Changelog(tokens) => {
                let notice = crate::commands::LexedNotice {
                    theme: Rc::clone(&self.theme),
                    title: "What's New".into(),
                    tokens,
                };
                self.chat.append_notice(Box::new(notice));
            }
            LaneEvent::Credentials(stored) => {
                if stored.is_empty() {
                    self.show_status(
                        "No stored credentials to remove. /logout only removes credentials saved by /login; environment variables and models.json config are unchanged.",
                    );
                    return;
                }
                let mut providers: Vec<crate::selectors::AuthProvider> = stored
                    .into_iter()
                    .map(|(id, kind)| crate::selectors::AuthProvider {
                        name: crate::selectors::provider_name(&id),
                        status: Some((kind.clone(), Some("stored credential".into()))),
                        id,
                        auth_type: kind,
                        api_key_method: None,
                        login_label: None,
                        generic_login: false,
                    })
                    .collect();
                providers.sort_by(|a, b| crate::selectors::locale_compare(&a.name, &b.name));
                let selector = crate::selectors::AuthSelector::new(
                    Rc::clone(&self.theme),
                    self.keys.clone(),
                    false,
                    providers,
                    None,
                );
                self.panel = Some(Panel::Logout(Box::new(selector)));
                self.editor.focused = false;
            }
            LaneEvent::Catalog(models) => {
                if let Some(current) = models
                    .iter()
                    .find(|m| m.provider == self.model.provider && m.id == self.model.id)
                {
                    self.model = current.clone();
                }
                self.models = models;
            }
            LaneEvent::SessionDeleted(result) => {
                if let Some(Panel::Sessions(selector)) = self.panel.as_mut() {
                    match result {
                        // The server removed the file, so Pi's trash step finds it gone and reports the trash.
                        Ok(()) => selector.set_status(false, "Session moved to trash", Duration::from_secs(2)),
                        Err(error) => selector.set_status(true, &error, Duration::from_secs(3)),
                    }
                }
            }
            LaneEvent::Navigated(result) => {
                let text = self.navigation_text.take();
                match result {
                    Ok(()) => {
                        if let Some(text) = text.filter(|t| !t.is_empty())
                            && self.editor.text().trim().is_empty()
                        {
                            self.editor.set_text(&text);
                            self.update_editor_border();
                        }
                        self.show_status("Navigated to selected point");
                    }
                    Err(error) => self.show_status(&format!("Error: {error}")),
                }
            }
            LaneEvent::Agents(agents) => {
                if let Some(Overlay::AgentHub(hub)) = self.overlay.as_mut() {
                    hub.set_agents(agents, true);
                    return;
                }
                let style = self.chat.style();
                let theme = Rc::clone(&style.theme);
                let (highlight, code, expand, dequeue, interrupt) = (
                    style.highlight,
                    style.code_highlight,
                    style.expand_key,
                    style.dequeue_key,
                    style.interrupt_key,
                );
                let mut hub = AgentHub::new(
                    Rc::clone(&self.theme),
                    self.keys.clone(),
                    Rc::new(move || ChatStyle {
                        theme: Rc::clone(&theme),
                        highlight: Rc::clone(&highlight),
                        code_highlight: code.clone(),
                        expand_key: expand.clone(),
                        dequeue_key: dequeue.clone(),
                        interrupt_key: interrupt.clone(),
                    }),
                );
                hub.set_agents(agents, true);
                self.overlay = Some(Overlay::AgentHub(Box::new(hub)));
            }
            LaneEvent::ForkMessages(messages) if self.rewind_pending => {
                self.rewind_pending = false;
                self.open_rewind(messages);
            }
            LaneEvent::ForkMessages(messages) => {
                if messages.is_empty() {
                    self.show_status("No messages to fork from");
                    return;
                }
                let last = messages.last().map(|m| m.entry_id.clone());
                self.panel = Some(Panel::Fork(UserMessageSelector::new(
                    Rc::clone(&self.theme),
                    messages,
                    last.as_deref(),
                )));
                self.editor.focused = false;
            }
            LaneEvent::Forked { editor_text, status } => {
                self.editor.set_text(&editor_text);
                self.update_editor_border();
                self.show_status(&status);
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
        if self.overlay.is_some() {
            self.handle_overlay_input(data);
            return;
        }
        if self.panel.is_some() {
            self.handle_panel_input(data);
            return;
        }
        if self.qol.overlays && self.handle_overlay_keys(data) {
            return;
        }
        // A quick Esc-Esc can arrive as one read (Alt+Escape): the rewind selector, like two taps.
        if self.qol.overlays && data == "\x1b\x1b" && self.operation.is_none() && self.editor.text().trim().is_empty() {
            self.rewind_pending = true;
            self.client.fork_messages();
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
                if self.qol.overlays {
                    self.open_model_hub();
                } else {
                    self.execute_slash_command("model", "");
                }
                return;
            }
            if k("app.message.followUp") {
                let text = self.editor.expanded_text().trim().to_string();
                if !text.is_empty() {
                    self.editor.set_text("");
                    self.record_history(&text);
                    self.status = "Queueing follow-up…".into();
                    let images = self.take_images(&text);
                    let id = if images.is_empty() {
                        self.client.follow_up(&text)
                    } else {
                        self.client.follow_up_with_images(&text, images)
                    };
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
                if let Some(bash) = self.bash.as_mut() {
                    bash.expanded = expanded;
                }
                self.header.expanded = expanded;
                self.show_status(&format!(
                    "Tool output: {}",
                    if expanded { "expanded" } else { "collapsed" }
                ));
                return;
            }
            if k("app.thinking.toggle") {
                let hide = !self.chat.hide_thinking;
                self.chat.set_hide_thinking(hide);
                self.client.save_setting("hideThinkingBlock", Value::Bool(hide));
                self.show_status(&format!("Thinking blocks: {}", if hide { "hidden" } else { "visible" }));
                return;
            }
            if k("app.thinking.cycle") {
                self.cycle_thinking();
                return;
            }
            if k("app.model.cycleForward") {
                self.cycle_model(1);
                return;
            }
            if k("app.model.cycleBackward") {
                self.cycle_model(-1);
                return;
            }
            if k("app.editor.external") {
                if self.external_editor.is_none() {
                    self.show_status("No editor configured. Set $VISUAL or $EDITOR environment variable.");
                } else {
                    self.edit_request = Some(self.editor.expanded_text());
                }
                return;
            }
            if k("app.rlm.toggle") {
                self.toggle_rlm();
                return;
            }
            if k("app.rlm.focus") {
                self.open_rlm_focus();
                return;
            }
            if k("app.rlm.pane") {
                self.rlm_pane();
                return;
            }
            if k("app.history.search") {
                self.show_history_search();
                return;
            }
            if k("app.session.tree") {
                self.execute_slash_command("tree", "");
                return;
            }
            if k("app.session.fork") {
                self.execute_slash_command("fork", "");
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
        self.escapes.reset();
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
        if self.bash.is_some() {
            self.client.abort_bash();
            return;
        }
        if let Some(operation) = self.operation.clone() {
            self.restore_queued_to_editor();
            self.aborting = true;
            self.status = format!("Aborting {operation}…");
            self.client.abort();
        }
        // Idle on an empty editor, a second Esc opens the rewind selector (omp's Esc-Esc; Pi's tree selector is not
        // ported).
        if self.qol.overlays
            && self.operation.is_none()
            && self.editor.text().trim().is_empty()
            && self.escapes.tap(Instant::now())
        {
            self.rewind_pending = true;
            self.client.fork_messages();
        }
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
        // Pi's `!command` runs in the Session cwd and is recorded for the model; `!!command` is not.
        if let Some(rest) = prompt.strip_prefix('!') {
            let excluded = rest.starts_with('!');
            let command = rest.strip_prefix('!').unwrap_or(rest).trim();
            if !command.is_empty() {
                self.run_bash(command, excluded);
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
            let id = if images.is_empty() {
                self.client.steer(&prompt)
            } else {
                self.client.steer_with_images(&prompt, images)
            };
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
            "copy" if self.qol.overlays => self.open_copy_picker(),
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
            "model" if self.qol.overlays && args.is_empty() => self.open_model_hub(),
            "model" => self.model_command(args),
            "resume" => self.client.list_sessions(),
            "thinking" => self.thinking_command(args),
            "compact" => {
                self.show_status("Compacting…");
                self.client.compact((!args.is_empty()).then_some(args));
            }
            "fork" => self.client.fork_messages(),
            "tree" => self.client.tree(),
            "rlm" => match args.trim() {
                "focus" => self.open_rlm_focus(),
                "pane" => self.rlm_pane(),
                _ => self.toggle_rlm(),
            },
            "settings" => self.client.read_settings(),
            "review" => {
                self.show_status("Reviewing changes…");
                if self.operation.is_some() {
                    let id = self.client.steer(&crate::commands::review_prompt(args));
                    self.status = format!("Queued {id}.");
                } else {
                    self.client.prompt(&crate::commands::review_prompt(args));
                }
            }
            "logout" => self.client.credentials(),
            "login" => self.client.login_providers(args),
            "changelog" => self.client.changelog(),
            "share" => self.client.share(),
            "clone" => {
                if self.operation.is_some() {
                    self.client.abort();
                }
                self.client.clone_session();
            }
            "export" => self.client.export(path_argument(args).as_deref()),
            "usage" => self.client.usage_report(),
            // With the overlays, `/agents` opens the agent hub (the inspection text is the worker's, not ported).
            "agents" if self.qol.overlays && args.is_empty() => self.client.agents(),
            other => self.show_status(&format!("Error: /{other} is not available in the Rust build yet")),
        }
    }

    /// Pi's `!` command: output shows while it runs; the recorded result joins the transcript.
    fn run_bash(&mut self, command: &str, excluded: bool) {
        self.editor.set_text("");
        if self.bash.is_some() {
            self.show_status("A bash command is already running. Press Esc to cancel it first.");
            self.editor
                .set_text(&format!("{}{command}", if excluded { "!!" } else { "!" }));
            self.update_editor_border();
            return;
        }
        self.bash = Some(self.chat.bash_component(command, excluded));
        self.client.bash(command, excluded);
        self.update_editor_border();
    }

    fn bash_done(&mut self, result: Result<BashResult, String>) {
        // The worker records the command in the session, where the transcript draws it.
        self.bash = None;
        if let Err(error) = result {
            self.status = format!("Error: {error}");
        }
    }

    fn compacted(&mut self, result: Result<Value, String>) {
        self.chat.set_compacting(false);
        match result {
            Ok(entry) => {
                // The lane's transcript restarts at the compaction entry (`#syncTranscript` diverged).
                self.chat.clear();
                self.chat.add_entry(&entry);
                self.context_from = self.transcript.len();
                self.compacted = true;
                self.status.clear();
            }
            Err(error) => {
                // The native lane's rejection (lane.ts `NothingToCompact`), whatever the session's own wording.
                let error = if error.starts_with("Nothing to compact") || error == "Already compacted" {
                    "Lane \"main\" has nothing to compact".to_string()
                } else {
                    error
                };
                self.status = format!("Operation rejected: {error}");
            }
        }
    }

    /// Redraw the transcript from the lane's entries (`#syncTranscript` of a fresh snapshot): compactions with their
    /// retained tails, branch summaries; the context restarts after the last compaction, and branch summaries'
    /// usage joins the footer's totals as the lane's usage rows do.
    fn load_entries(&mut self, entries: &[Value]) {
        if entries.is_empty() {
            return;
        }
        // The lane's transcript starts at its last compaction (`scanBranch` stops there).
        let start = entries.iter().rposition(|e| e["type"] == "compaction").unwrap_or(0);
        let entries = &entries[start..];
        self.chat.clear();
        let mut messages = Vec::new();
        let mut compacted_at = None;
        self.path_usage = Value::Null;
        let mut usage = serde_json::Map::new();
        for entry in entries {
            self.chat.add_entry(entry);
            match entry["type"].as_str() {
                Some("compaction") => {
                    messages.extend(entry["retainedTail"].as_array().cloned().unwrap_or_default());
                    compacted_at = Some(messages.len());
                }
                Some("message") => messages.push(entry["message"].clone()),
                Some("branch_summary") => {
                    for key in ["input", "output", "cacheRead", "cacheWrite"] {
                        let add = entry["usage"][key].as_u64().unwrap_or(0);
                        let sum = usage.get(key).and_then(Value::as_u64).unwrap_or(0) + add;
                        usage.insert(key.into(), sum.into());
                    }
                    let cost = usage.get("cost").and_then(Value::as_f64).unwrap_or(0.0)
                        + entry["usage"]["cost"]["total"].as_f64().unwrap_or(0.0);
                    usage.insert("cost".into(), cost.into());
                }
                _ => {}
            }
        }
        if !usage.is_empty() {
            self.path_usage = Value::Object(usage);
        }
        self.transcript = messages;
        self.compacted = compacted_at.is_some();
        self.context_from = compacted_at.unwrap_or(0);
    }

    /// `/thinking [level]`: the selector without a level.
    fn thinking_command(&mut self, args: &str) {
        let levels = self.model.levels();
        if !args.is_empty() {
            match levels.iter().find(|l| **l == args.to_lowercase()) {
                Some(level) => {
                    let level = level.clone();
                    self.set_thinking(&level);
                    self.status = format!("Thinking level: {level}.");
                }
                None => {
                    self.status = format!(
                        "Error: Unknown thinking level \"{args}\". Available levels: {}.",
                        levels.join(", ")
                    );
                }
            }
            return;
        }
        let items = levels
            .iter()
            .map(|level| SelectItem {
                value: level.clone(),
                label: if *level == self.thinking_level {
                    format!("{level} (selected)")
                } else {
                    level.clone()
                },
                description: thinking_description(level).map(str::to_string),
            })
            .collect();
        let current = self.thinking_level.clone();
        self.open_selection("Select thinking level:", items, Some(&current), Purpose::Thinking);
    }

    fn set_thinking(&mut self, level: &str) {
        self.client.set_thinking(level);
        self.thinking_level = level.to_string();
        self.update_editor_border();
    }

    /// Pi's Shift+Tab.
    fn cycle_thinking(&mut self) {
        if !self.model.reasoning {
            self.show_status("Current model does not support thinking");
            return;
        }
        let levels = self.model.levels();
        if levels.len() < 2 {
            return;
        }
        let index = levels.iter().position(|l| *l == self.thinking_level);
        let next = levels[index.map_or(0, |i| (i + 1) % levels.len())].clone();
        self.set_thinking(&next);
        self.show_status(&format!("Thinking level: {next}"));
    }

    /// Pi's Ctrl+P / Shift+Ctrl+P.
    fn cycle_model(&mut self, direction: isize) {
        let next = match next_model(&self.models, &self.model, direction, &self.enabled_models) {
            Ok(model) => model.clone(),
            Err(error) => {
                self.show_status(error);
                return;
            }
        };
        self.client.select_model(&next.provider, &next.id);
        let suffix = if next.reasoning && self.thinking_level != "off" {
            format!(" (thinking: {})", self.thinking_level)
        } else {
            String::new()
        };
        let name = if next.name.is_empty() { &next.id } else { &next.name };
        self.show_status(&format!("Switched to {name}{suffix}"));
        self.model = next;
    }

    fn handle_panel_input(&mut self, data: &str) {
        match self.panel.as_mut() {
            Some(Panel::Fork(selector)) => match selector.handle_input(&self.keys, data) {
                Some(SelectorAction::Selected(entry_id)) => {
                    self.panel = None;
                    if self.operation.is_some() {
                        self.client.abort();
                    }
                    self.client.fork(&entry_id);
                }
                Some(SelectorAction::Cancelled) => self.panel = None,
                None => {}
            },
            Some(Panel::RlmFocus(_)) => {
                let snapshot = self.rlm_snapshot();
                let height = self.rlm_focus_height();
                let Some(Panel::RlmFocus(focus)) = self.panel.as_mut() else {
                    return;
                };
                match focus.handle_input(&self.keys, data, &snapshot, height) {
                    crate::rlm::focus::FocusEvent::Exit => self.panel = None,
                    crate::rlm::focus::FocusEvent::LoadTrace { key, .. } => {
                        focus.set_trace(&key, Err("frame traces are not available in the Rust build yet"));
                    }
                    _ => {}
                }
            }
            Some(Panel::Sessions(selector)) => match selector.handle_input(data) {
                Some(SessionAction::Selected(path)) => {
                    self.panel = None;
                    if Some(&path) == self.session_file.as_ref() {
                        self.show_status("Already in this session");
                    } else {
                        // "Resumed session" once the switch is done (the link reports it).
                        self.client.resume(&path);
                    }
                }
                Some(SessionAction::Cancelled) => self.panel = None,
                Some(SessionAction::Delete(path)) => self.client.delete_session(&path),
                Some(SessionAction::Rename(path, name)) => {
                    if Some(&path) == self.session_file.as_ref() {
                        self.session_name = Some(name.clone());
                        self.update_title();
                    }
                    self.client.rename_session(&path, &name);
                }
                None => {}
            },
            Some(Panel::Login(selector, options, auth_type)) => match selector.handle_input(data) {
                Some(Ok((id, kind))) => {
                    let option = options.iter().find(|o| o.id == id && o.auth_type == kind).cloned();
                    self.panel = None;
                    if let Some(option) = option {
                        self.start_login(option);
                    }
                }
                Some(Err(())) => {
                    let (options, typed) = (options.clone(), auth_type.is_some());
                    self.panel = None;
                    if typed {
                        self.ask_login_method(options, false);
                    }
                }
                None => {}
            },
            Some(Panel::LoginPrompt { provider, input, .. }) => {
                if self.keys.matches(data, "tui.select.cancel") {
                    // Esc closes the login silently.
                    self.panel = None;
                } else if self.keys.matches(data, "tui.select.confirm") {
                    let key = input.value().to_string();
                    let id = provider.id.clone();
                    self.panel = None;
                    self.client.save_api_key(&id, &key);
                } else {
                    input.handle_input(data);
                }
            }
            Some(Panel::Logout(selector)) => match selector.handle_input(data) {
                Some(Ok((id, kind))) => {
                    self.panel = None;
                    let name = crate::selectors::provider_name(&id);
                    let done = if kind == "oauth" {
                        format!("Logged out of {name}")
                    } else {
                        format!(
                            "Removed stored API key for {name}. Environment variables and models.json config are unchanged"
                        )
                    };
                    self.client.logout(&id, &done);
                }
                Some(Err(())) => self.panel = None,
                None => {}
            },
            Some(Panel::Settings(selector)) => match selector.handle_input(data) {
                Some(SettingsEvent::Cancelled) => self.panel = None,
                Some(SettingsEvent::Change(key, value)) => self.setting_changed(&key, value),
                Some(SettingsEvent::SessionModel(selector)) => {
                    if let Some(model) = self
                        .models
                        .iter()
                        .find(|m| format!("{}/{}", m.provider, m.id) == selector)
                        .cloned()
                    {
                        self.client.select_model(&model.provider, &model.id);
                        self.client
                            .save_setting("defaultProvider", Value::String(model.provider.clone()));
                        self.client
                            .save_setting("defaultModel", Value::String(model.id.clone()));
                        self.model = model;
                        self.show_status(&format!("Session model: {selector} (saved as default)"));
                    }
                }
                Some(SettingsEvent::ModelSetting { key, label, value }) => {
                    let stored = if value.is_empty() {
                        Value::Null
                    } else {
                        Value::String(value.clone())
                    };
                    self.client.save_setting(&key, stored);
                    let next = match key.as_str() {
                        "rlm.frameModel" | "rlm.frameThinking" => Some("the next frame"),
                        "review.model" => Some("the next /review"),
                        "rlm.childModel" => Some("the next sub-agent"),
                        _ => None,
                    };
                    let when = match next {
                        Some(next) => format!("applies to {next}"),
                        None if key.starts_with("claudeCode.") => "applies when ultron claude next starts".to_string(),
                        None => "applies to the running session".to_string(),
                    };
                    let shown = if value.is_empty() { "default".to_string() } else { value };
                    self.show_status(&format!("{label}: {shown} ({when})"));
                }
                None => {}
            },
            Some(Panel::Tree(tree)) => match tree.handle_input(data) {
                Some(TreeAction::Selected(entry)) => {
                    self.panel = None;
                    self.tree_selected(entry);
                }
                Some(TreeAction::Cancelled) => self.panel = None,
                Some(TreeAction::Copy(text)) => match text {
                    None => self.show_status("Error: Selected entry has no text to copy"),
                    Some(text) => {
                        self.terminal_writes
                            .push(format!("\x1b]52;c;{}\x07", base64(text.as_bytes())));
                        self.show_status("Copied selected message to clipboard");
                    }
                },
                Some(TreeAction::Label(entry, label)) => self.client.set_label(&entry, label.as_deref()),
                None => {}
            },
            Some(Panel::Instructions { entry, input }) => {
                if self.keys.matches(data, "tui.select.confirm") {
                    let text = input.value().to_string();
                    let entry = entry.clone();
                    self.panel = None;
                    self.navigate(&entry, true, Some(&text));
                } else if self.keys.matches(data, "tui.select.cancel") {
                    // Back to the summary choice.
                    let entry = entry.clone();
                    self.panel = None;
                    self.ask_summary(entry);
                } else {
                    input.handle_input(data);
                }
            }
            Some(Panel::HistorySearch(search)) => match search.handle_input(data) {
                HistorySearchEvent::Accept(text) => {
                    self.panel = None;
                    self.editor.set_text(&text);
                    self.update_editor_border();
                }
                HistorySearchEvent::Cancel => self.panel = None,
                HistorySearchEvent::None => {}
            },
            None => {}
        }
    }

    /// `handleLogin`: a provider named in the command signs in directly; otherwise the method, then the provider.
    fn login(&mut self, providers: Vec<crate::selectors::AuthProvider>, query: &str) {
        let reference = query.trim().to_lowercase();
        if reference.is_empty() {
            self.ask_login_method(providers, true);
            return;
        }
        let matches: Vec<crate::selectors::AuthProvider> = providers
            .iter()
            .filter(|p| p.id.to_lowercase() == reference || p.name.to_lowercase() == reference)
            .cloned()
            .collect();
        if matches.len() == 1 {
            self.start_login(matches[0].clone());
            return;
        }
        let ids: std::collections::HashSet<&str> = matches.iter().map(|p| p.id.as_str()).collect();
        if matches.len() > 1 && ids.len() == 1 {
            self.ask_login_method(matches, false);
            return;
        }
        self.show_login_providers(providers, None, query.trim());
    }

    /// `showAuthTypeSelector`: account (subscription) or API key; `all` when it chooses among every provider.
    fn ask_login_method(&mut self, options: Vec<crate::selectors::AuthProvider>, all: bool) {
        let oauth_label = (!all)
            .then(|| {
                options
                    .iter()
                    .find(|o| o.auth_type == "oauth")
                    .and_then(|o| o.login_label.clone())
            })
            .flatten();
        let subscription = oauth_label.unwrap_or_else(|| "Sign in with an account".into());
        let has = |kind: &str| all || options.iter().any(|o| o.auth_type == kind);
        let mut labels = Vec::new();
        if has("oauth") {
            labels.push(subscription.clone());
        }
        if has("api_key") {
            labels.push("Sign in with an API key".to_string());
        }
        if labels.is_empty() {
            self.show_status("No login methods available.");
            return;
        }
        if !all && labels.len() == 1 {
            self.start_login(options[0].clone());
            return;
        }
        let title = if all {
            "Select authentication method:".to_string()
        } else {
            format!("Select authentication method for {}:", options[0].name)
        };
        let items = labels
            .iter()
            .map(|l| SelectItem {
                value: if *l == subscription {
                    "oauth".into()
                } else {
                    "api_key".into()
                },
                label: l.clone(),
                description: None,
            })
            .collect();
        self.open_selection(&title, items, None, Purpose::LoginMethod(options, all));
    }

    /// `showProviderSelector`.
    fn show_login_providers(
        &mut self,
        all: Vec<crate::selectors::AuthProvider>,
        auth_type: Option<&str>,
        search: &str,
    ) {
        let options: Vec<crate::selectors::AuthProvider> = all
            .iter()
            .filter(|p| auth_type.is_none_or(|t| p.auth_type == t))
            .cloned()
            .collect();
        if options.is_empty() {
            self.show_status(match auth_type {
                Some("oauth") => "No subscription providers available.",
                Some("api_key") => "No API key providers available.",
                _ => "No login providers available.",
            });
            return;
        }
        let selector = crate::selectors::AuthSelector::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            true,
            options,
            (!search.is_empty()).then_some(search),
        );
        self.panel = Some(Panel::Login(Box::new(selector), all, auth_type.map(str::to_string)));
        self.editor.focused = false;
    }

    /// `startProviderLogin`: the login dialog's API-key prompt (subscription flows are not ported).
    fn start_login(&mut self, option: crate::selectors::AuthProvider) {
        if option.auth_type == "oauth" {
            self.show_status(&format!(
                "Error: Failed to login to {}: signing in with an account is not available in the Rust build yet; use an API key",
                option.name
            ));
            return;
        }
        match option.api_key_method.clone() {
            Some((method, true)) if option.generic_login => {
                let mut input = tui::input::Input::new(self.keys.clone()).with_options(None, None, None, Some("•"));
                input.focused = true;
                self.panel = Some(Panel::LoginPrompt {
                    provider: option,
                    method,
                    input: Box::new(input),
                });
                self.editor.focused = false;
            }
            Some((method, false)) => self.show_status(&format!("{method} is configured outside Ultron.")),
            _ => self.show_status(&format!(
                "Error: Failed to save API key for {}: this provider's sign-in is not available in the Rust build yet",
                option.name
            )),
        }
    }

    /// `changeSetting` (client-tui-settings.ts): save, apply the live ones here, report the outcome.
    fn setting_changed(&mut self, key: &str, value: Value) {
        const UNUSED_BY_ULTRON: [&str; 18] = [
            "terminal.showImages",
            "terminal.imageWidthCells",
            "images.autoResize",
            "images.blockImages",
            "enableSkillCommands",
            "markdown.mermaid",
            "showCacheMissNotices",
            "collapseChangelog",
            "enableInstallTelemetry",
            "quietStartup",
            "outputPad",
            "terminal.showTerminalProgress",
            "tuiMode",
            "fullscreenExitOutput",
            "fullscreenScrollbar",
            "fullscreenCopyOnSelect",
            "warnings",
            "cacheWarming",
        ];
        self.client.save_setting(key, value.clone());
        // A per-model thinking level of the current model applies to the session now.
        if key == "modelThinkingLevel"
            && value["provider"] == self.model.provider.as_str()
            && value["modelId"] == self.model.id.as_str()
        {
            let level = value["level"].as_str().unwrap_or("medium").to_string();
            self.set_thinking(&level);
        }
        match key {
            "hideThinkingBlock" => self.chat.set_hide_thinking(value == true),
            "compaction.enabled" => self.auto_compact = value != false,
            "treeFilterMode" => self.tree_filter = FilterMode::parse(value.as_str().unwrap_or("default")),
            "enabledModels" => {
                self.enabled_models = value
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
            }
            _ => {}
        }
        if key == "tuiMode" {
            self.show_status(&format!(
                "Saved tuiMode: {} for Pi. Ultron's native TUI always runs fullscreen.",
                value.as_str().unwrap_or("")
            ));
        } else if matches!(key, "defaultThinkingLevel" | "defaultProjectTrust") {
            self.show_status(&format!(
                "Saved {key}. It takes effect when the Session worker restarts."
            ));
        } else if UNUSED_BY_ULTRON.contains(&key) {
            self.show_status(&format!(
                "Saved {key} to settings. Ultron's native TUI and Session worker do not use it."
            ));
        } else if key == "httpIdleTimeoutMs"
            && let Some(ms) = value.as_u64()
        {
            self.show_status(&format!("HTTP idle timeout: {}", format_http_idle(ms)));
        }
    }

    /// Poll the subagents while an RLM view shows.
    fn refresh_rlm(&self) {
        if self.rlm_visible || matches!(self.panel, Some(Panel::RlmFocus(_))) {
            self.client.rlm_status();
        }
    }

    /// `#toggleRlm`: the docked RLM panel under the status.
    fn toggle_rlm(&mut self) {
        self.rlm_visible = !self.rlm_visible;
        self.client.rlm_status();
    }

    /// `#toggleRlmPane`: below RLM_PANE_MIN_COLUMNS (120) the full-screen graph opens instead of the pane; the
    /// side pane itself is not wired yet, so wide terminals get the graph too.
    fn rlm_pane(&mut self) {
        self.open_rlm_focus();
    }

    /// `#openRlmFocus`: the full-screen RLM graph in the editor's place; Esc returns to the chat.
    fn open_rlm_focus(&mut self) {
        if matches!(self.panel, Some(Panel::RlmFocus(_))) {
            return;
        }
        self.client.rlm_status();
        self.panel = Some(Panel::RlmFocus(Box::new(crate::rlm::RlmGraphFocus::new())));
        self.editor.focused = false;
    }

    /// The focus view's height: the screen above the footer (`#openRlmFocus`).
    fn rlm_focus_height(&self) -> usize {
        let footer = render_footer(&self.theme, &self.footer_view(), self.cols.max(1)).len();
        self.rows.saturating_sub(footer + 1).max(10)
    }

    /// `#buildRlmSnapshot`: the session's subagents (polled) and the root turn's `rlm` cells (the transcript).
    fn rlm_snapshot(&mut self) -> crate::rlm::RlmSnapshot {
        use crate::rlm::visualizer::{
            RunningTool, TranscriptView, extract_root_cell, extract_turn, parse_agents_status,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_millis() as f64);
        // The open operation's tools: the calls of the last assistant message that have no result yet.
        let running = self.operation.as_ref().map(|_| {
            let done: std::collections::HashSet<&str> = self
                .transcript
                .iter()
                .filter(|m| m["role"] == "toolResult")
                .filter_map(|m| m["toolCallId"].as_str())
                .collect();
            self.transcript
                .iter()
                .rev()
                .find(|m| m["role"] == "assistant")
                .and_then(|m| m["content"].as_array())
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "toolCall")
                .filter(|b| !done.contains(b["id"].as_str().unwrap_or("")))
                .map(|b| RunningTool {
                    status: "running".into(),
                    tool_call_id: b["id"].as_str().unwrap_or("").to_string(),
                    tool_name: b["name"].as_str().unwrap_or("").to_string(),
                    args: b["arguments"].clone(),
                    ..RunningTool::default()
                })
                .collect()
        });
        let view = TranscriptView::from_messages(
            &self.transcript[self.context_from.min(self.transcript.len())..],
            running,
        );
        let (turn, cells) = extract_turn(Some(&view), &mut self.rlm_clock, now, 30);
        let root_cell = extract_root_cell(Some(&view), &mut self.rlm_clock, now);
        let status = parse_agents_status(&serde_json::json!({"tasks": self.rlm_agents["subagents"]}));
        let timing = self.rlm_clock.timings(&status.tasks, status.usage.as_ref(), now);
        crate::rlm::RlmSnapshot {
            now,
            tasks: status.tasks,
            usage: status.usage,
            limits: status.limits,
            jobs: status.jobs,
            tool_calls: status.tool_calls,
            workflows: status.workflows,
            truncated_tasks: status.truncated_tasks,
            timing,
            root_cell,
            cells,
            turn,
            ..crate::rlm::RlmSnapshot::default()
        }
    }

    fn rlm_options(&self) -> crate::rlm::graph::RlmViewOptions {
        crate::rlm::graph::RlmViewOptions {
            style: crate::rlm::RlmStyle::themed(Rc::clone(&self.theme)),
            spinner_frame: (self.ticks * 80 / 100) as i64,
            focus_key: self.keys.get_keys("app.rlm.focus").first().cloned(),
            ..Default::default()
        }
    }

    /// Pi's `/tree` selector over every branch.
    fn show_tree(&mut self, tree: Value, leaf: Option<String>, initial: Option<&str>) {
        if tree.as_array().is_none_or(Vec::is_empty) {
            self.show_status("No entries in session");
            return;
        }
        let selector = TreeSelector::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            &tree,
            (leaf.as_deref(), initial),
            self.rows,
            self.tree_filter,
            self.home.as_deref().unwrap_or(""),
        );
        self.tree = Some((tree, leaf));
        self.panel = Some(Panel::Tree(Box::new(selector)));
        self.editor.focused = false;
    }

    fn tree_entry(&self, id: &str) -> Option<Value> {
        let (tree, _) = self.tree.as_ref()?;
        let mut stack: Vec<&Value> = tree.as_array()?.iter().collect();
        while let Some(node) = stack.pop() {
            if node["entry"]["id"] == id {
                return Some(node["entry"].clone());
            }
            stack.extend(node["children"].as_array().into_iter().flatten());
        }
        None
    }

    /// `#navigateTree`: the summary choice, then the move.
    fn tree_selected(&mut self, entry: String) {
        let leaf = self.tree.as_ref().and_then(|(_, leaf)| leaf.clone());
        if Some(&entry) == leaf.as_ref() {
            self.show_status("Already at this point");
            return;
        }
        if self.skip_summary_prompt {
            self.navigate(&entry, false, None);
        } else {
            self.ask_summary(entry);
        }
    }

    fn ask_summary(&mut self, entry: String) {
        let items = ["No summary", "Summarize", "Summarize with custom prompt"]
            .iter()
            .map(|v| SelectItem {
                value: (*v).to_string(),
                label: (*v).to_string(),
                description: None,
            })
            .collect();
        self.open_selection("Summarize branch?", items, None, Purpose::Summarize(entry));
    }

    fn navigate(&mut self, entry_id: &str, summarize: bool, instructions: Option<&str>) {
        if self.operation.is_some() {
            self.client.abort();
        }
        // A selected user (or custom) message goes back into the editor; the session continues from before it.
        let entry = self.tree_entry(entry_id);
        let (target, text) = match &entry {
            Some(e) if e["type"] == "message" && e["message"]["role"] == "user" => (
                e["parentId"].as_str().map(str::to_string),
                Some(user_message_text(&e["message"])),
            ),
            Some(e) if e["type"] == "custom_message" => (
                e["parentId"].as_str().map(str::to_string),
                Some(match &e["content"] {
                    Value::String(s) => s.clone(),
                    other => other
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|p| p["type"] == "text")
                        .filter_map(|p| p["text"].as_str())
                        .collect(),
                }),
            ),
            _ => (Some(entry_id.to_string()), None),
        };
        self.navigation_text = text;
        self.show_status(if summarize {
            "Summarizing branch… (Esc to cancel)"
        } else {
            "Navigating…"
        });
        self.client.navigate(target.as_deref(), summarize, instructions);
    }

    /// Pi's prompt-history search (`app.history.search`, Alt+R).
    fn show_history_search(&mut self) {
        let history = self.editor.history().to_vec();
        if history.is_empty() {
            self.show_status("No prompt history yet");
            return;
        }
        let mut search = PromptHistorySearch::new(history, Rc::clone(&self.theme), self.keys.clone(), "");
        search.set_focused(true);
        self.panel = Some(Panel::HistorySearch(Box::new(search)));
        self.editor.focused = false;
    }

    /// The overlays' global keys: F5 retry, Alt+L display reset, Alt+A agent hub.
    fn handle_overlay_keys(&mut self, data: &str) -> bool {
        if self.keys.matches(data, "app.retry") {
            self.retry();
            return true;
        }
        if self.keys.matches(data, "app.display.reset") {
            self.reset_display = true;
            self.committed.clear();
            return true;
        }
        if self.keys.matches(data, "app.agents.hub") {
            self.client.agents();
            return true;
        }
        if self.keys.matches(data, "tui.altScreen.search") {
            // Every transcript row (scrollback and viewport) at the current width, rendered once.
            let width = self.cols.max(1);
            let rows = self.document(width);
            let mut search = tui::transcript_search::TranscriptSearch::new(rows, self.keys.clone(), false);
            let (t1, t2) = (Rc::clone(&self.theme), Rc::clone(&self.theme));
            search.set_styles(
                Box::new(move |s| format!("\x1b[4m{}\x1b[24m", fg(&t1, "accent", s))),
                Box::new(move |s| format!("\x1b[7m{}\x1b[27m", fg(&t2, "accent", s))),
            );
            self.overlay = Some(Overlay::Search(Box::new(search)));
            return true;
        }
        false
    }

    /// F5: send the failed turn's prompt again (the session keeps the failed attempt).
    fn retry(&mut self) {
        if self.operation.is_some() || !crate::retry::failed_turn_tail(&self.transcript) {
            self.show_status("Nothing to retry");
            return;
        }
        let Some(prompt) = self
            .transcript
            .iter()
            .rev()
            .find(|m| m["role"] == "user")
            .map(|m| user_message_text(m).trim().to_string())
            .filter(|t| !t.is_empty())
        else {
            self.show_status("Nothing to retry");
            return;
        };
        self.status = "Retrying…".into();
        self.client.prompt(&prompt);
    }

    /// Whether the display reset was asked for; clears the request.
    pub fn take_reset_display(&mut self) -> bool {
        std::mem::take(&mut self.reset_display)
    }

    fn transcript_entries(&self) -> Vec<TranscriptEntry> {
        self.transcript
            .iter()
            .enumerate()
            .map(|(i, m)| TranscriptEntry::new(&format!("m{i}"), "", m.clone()))
            .collect()
    }

    fn picker(&self, mode: PickerMode, entries: Vec<TranscriptEntry>) -> TranscriptPicker {
        TranscriptPicker::new(
            entries,
            PickerOptions {
                mode,
                theme: Rc::clone(&self.theme),
                keys: self.keys.clone(),
                chat: self.chat.style(),
                title: None,
                action_label: None,
            },
        )
    }

    /// `/copy` with the overlays: pick a turn (or a block of it) over the transcript.
    fn open_copy_picker(&mut self) {
        if self.last_assistant_text().is_none_or(|t| t.is_empty()) {
            self.show_status("Error: No agent messages to copy yet.");
            return;
        }
        let picker = self.picker(PickerMode::Copy, self.transcript_entries());
        self.overlay = Some(Overlay::Picker(Box::new(picker)));
    }

    /// Esc-Esc: pick the user turn to continue from; the session forks before it (user messages carry their
    /// session entry ids, matched in order by text).
    fn open_rewind(&mut self, forkable: Vec<crate::lane::ForkMessage>) {
        let mut ids = forkable.into_iter();
        let entries: Vec<TranscriptEntry> = self
            .transcript
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let id = if m["role"] == "user" {
                    let text = user_message_text(m);
                    ids.by_ref()
                        .find(|f| f.text == text)
                        .map_or(format!("m{i}"), |f| f.entry_id)
                } else {
                    format!("m{i}")
                };
                TranscriptEntry::new(&id, "", m.clone())
            })
            .collect();
        self.rewind_ids = entries
            .iter()
            .map(|e| (e.id.clone(), e.role() == "user" && !e.id.starts_with('m')))
            .collect();
        if !self.rewind_ids.iter().any(|(_, user)| *user) {
            self.show_status("No messages to rewind to");
            return;
        }
        let picker = self.picker(PickerMode::Rewind, entries);
        self.overlay = Some(Overlay::Picker(Box::new(picker)));
    }

    fn open_model_hub(&mut self) {
        let current = Some((
            format!("{}/{}", self.model.provider, self.model.id),
            self.thinking_level.clone(),
        ));
        let hub = ModelHub::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            self.models.clone(),
            self.recent_models.clone(),
            current,
        );
        self.overlay = Some(Overlay::ModelHub(Box::new(hub)));
    }

    fn handle_overlay_input(&mut self, data: &str) {
        self.overlay_input(data);
        if self.overlay.is_none() {
            // The overlay painted over the transcript: paint everything again.
            self.reset_display = true;
            self.committed.clear();
        }
    }

    fn overlay_input(&mut self, data: &str) {
        let Some(overlay) = self.overlay.as_mut() else {
            return;
        };
        match overlay {
            Overlay::Search(search) => {
                if matches!(search.handle_input(data), tui::transcript_search::SearchOutcome::Close) {
                    self.overlay = None;
                }
            }
            Overlay::Picker(picker) => match picker.handle_input(data) {
                PickerEvent::Copy { content, label, .. } => {
                    self.overlay = None;
                    self.terminal_writes
                        .push(format!("\x1b]52;c;{}\x07", base64(content.as_bytes())));
                    self.show_status(&format!("Copied {label} to clipboard"));
                }
                PickerEvent::Open { href, .. } => {
                    self.overlay = None;
                    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
                    let spawned = std::process::Command::new(opener)
                        .arg(&href)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .spawn();
                    self.show_status(&match spawned {
                        Ok(_) => format!("Opened {href}"),
                        Err(e) => format!("Error: {e}"),
                    });
                }
                PickerEvent::Rewind { entry_id } => {
                    self.overlay = None;
                    // A user turn: fork before it (its prompt returns to the editor). Any other turn: keep it, so
                    // fork before the next user turn; after the last one there is nothing to drop.
                    let at = self.rewind_ids.iter().position(|(id, _)| *id == entry_id);
                    let target = at.and_then(|at| {
                        let (_, user) = &self.rewind_ids[at];
                        let from = if *user { at } else { at + 1 };
                        self.rewind_ids[from..]
                            .iter()
                            .find(|(_, user)| *user)
                            .map(|(id, _)| id.clone())
                    });
                    let Some(entry_id) = target else {
                        self.show_status("Already at this point");
                        return;
                    };
                    if self.operation.is_some() {
                        self.client.abort();
                    }
                    self.client.fork(&entry_id);
                }
                PickerEvent::Cancel => self.overlay = None,
                PickerEvent::Ignored | PickerEvent::Render => {}
            },
            Overlay::ModelHub(hub) => match hub.handle_input(data) {
                ModelHubEvent::Use { selector, thinking } => {
                    self.overlay = None;
                    let Some(model) = self
                        .models
                        .iter()
                        .find(|m| format!("{}/{}", m.provider, m.id) == selector)
                        .cloned()
                    else {
                        self.show_status(&format!("Error: Unknown model: {selector}"));
                        return;
                    };
                    self.recent_models.retain(|s| *s != selector);
                    self.recent_models.insert(0, selector);
                    self.select_model(model);
                    if let Some(level) = thinking {
                        self.set_thinking(&level);
                    }
                }
                ModelHubEvent::Cancel => self.overlay = None,
                ModelHubEvent::Ignored | ModelHubEvent::Render => {}
            },
            Overlay::AgentHub(hub) => match hub.handle_input(data) {
                HubEvent::Close => self.overlay = None,
                HubEvent::Kill(id) => {
                    self.client.kill_agent(&id);
                    self.client.agents();
                }
                HubEvent::OpenTranscript(id) => hub.set_transcript(&id, Vec::new()),
                HubEvent::Revive(_) | HubEvent::Steer { .. } => hub.set_notice(Some(
                    "Steering and reviving subagents are not available in the Rust build yet".into(),
                )),
                HubEvent::Ignored | HubEvent::Render => {}
            },
        }
    }

    /// The open overlay's frame, when one is open.
    fn overlay_frame(&mut self, width: usize, height: usize) -> Option<Vec<String>> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        Some(match self.overlay.as_mut()? {
            Overlay::Search(search) => search.render(width, height),
            Overlay::Picker(picker) => picker.render(width, height),
            Overlay::ModelHub(hub) => hub.render(width, height),
            Overlay::AgentHub(hub) => hub.render(width, height, now),
        })
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

    /// `/resume`: Pi's session selector over this directory's sessions; a fresh listing updates an open one.
    fn show_resume(&mut self, sessions: Vec<SessionListing>) {
        if let Some(Panel::Sessions(selector)) = self.panel.as_mut() {
            selector.set_sessions(sessions);
            return;
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let selector = SessionSelector::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            sessions,
            self.session_file.clone(),
            self.home.as_deref().unwrap_or(""),
            now,
        );
        self.panel = Some(Panel::Sessions(Box::new(selector)));
        self.editor.focused = false;
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
                    Purpose::LoginMethod(options, all) => {
                        let kind = item.value.clone();
                        if all {
                            self.show_login_providers(options, Some(&kind), "");
                        } else if let Some(option) = options.iter().find(|o| o.auth_type == kind).cloned() {
                            self.start_login(option);
                        }
                    }
                    Purpose::Summarize(entry) => match item.value.as_str() {
                        "Summarize with custom prompt" => {
                            self.panel = Some(Panel::Instructions {
                                entry,
                                input: Box::new(tui::input::Input::new(self.keys.clone())),
                            });
                        }
                        choice => self.navigate(&entry, choice == "Summarize", None),
                    },
                    Purpose::Thinking => {
                        let level = item.value.clone();
                        self.set_thinking(&level);
                        self.status = format!("Thinking level: {level}.");
                    }
                }
            }
            Some(SelectEvent::Cancelled) => {
                // Esc on the summary choice returns to the tree with the same selection.
                if let Some(Selection {
                    purpose: Purpose::Summarize(entry),
                    ..
                }) = self.selection.take()
                    && let Some((tree, leaf)) = self.tree.clone()
                {
                    self.show_tree(tree, leaf, Some(&entry));
                }
            }
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
        s.context = self
            .context()
            .map(|(tokens, percent)| (tokens, self.model.context_window, percent));
        s
    }

    // ---- rendering ----

    fn footer_view(&self) -> FooterView {
        let mut v = FooterView {
            cwd: self.cwd.clone(),
            home: self.home.clone(),
            session_name: self.session_name.clone(),
            thinking_level: self.thinking_level.clone(),
            auto_compact: self.auto_compact,
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
        let p = &self.path_usage;
        v.input += p["input"].as_u64().unwrap_or(0);
        v.output += p["output"].as_u64().unwrap_or(0);
        v.cache_read += p["cacheRead"].as_u64().unwrap_or(0);
        v.cache_write += p["cacheWrite"].as_u64().unwrap_or(0);
        v.cost += p["cost"].as_f64().unwrap_or(0.0);
        if let Some(last) = self.transcript.iter().rev().find(|m| m["role"] == "assistant") {
            let u = &last["usage"];
            let read = u["cacheRead"].as_f64().unwrap_or(0.0);
            let prompt = u["input"].as_f64().unwrap_or(0.0) + read + u["cacheWrite"].as_f64().unwrap_or(0.0);
            if prompt > 0.0 {
                v.latest_cache_hit_rate = Some(read / prompt * 100.0);
            }
        }
        if let Some((_, percent)) = self.context() {
            v.context_percent = Some(percent);
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
        let mut pending = self.chat.render_pending(width);
        if let Some(bash) = self.bash.as_mut() {
            pending.extend(bash.render(width));
        }
        let mut status = Vec::new();
        if !self.status.is_empty() {
            status.extend(Text::new(&fg(&self.theme, "dim", &self.status), 1, 0, None).render(width));
        }
        // `#renderRlm(width, "panel")`: the docked RLM panel, one column in, hidden while the graph shows.
        let rlm_focus = matches!(self.panel, Some(Panel::RlmFocus(_)));
        let need_snapshot = self.rlm_visible || !rlm_focus;
        let snapshot = need_snapshot.then(|| self.rlm_snapshot());
        let options = self.rlm_options();
        if self.rlm_visible
            && !rlm_focus
            && let Some(snapshot) = &snapshot
        {
            let inner = width.saturating_sub(2).max(1);
            status.extend(
                crate::rlm::render_rlm_dock(snapshot, inner, &options, usize::MAX)
                    .into_iter()
                    .map(|l| format!(" {l}")),
            );
        }
        status.extend(self.chat.render_status(width));
        // `#renderRuntimeLine`: one RLM line under the footer while the panel and the graph are hidden.
        let runtime_line = match &snapshot {
            Some(snapshot) if !self.rlm_visible && !rlm_focus => {
                crate::rlm::render_rlm_footer(snapshot, width.max(1), &options)
            }
            _ => None,
        };
        let mut footer = render_footer(&self.theme, &self.footer_view(), width);
        footer.extend(runtime_line);
        let focus_height = self.rows.saturating_sub(footer.len() + 1).max(10);
        let focus_snapshot = rlm_focus.then(|| self.rlm_snapshot());
        let editor = match (&mut self.selection, &mut self.panel) {
            (None, Some(Panel::RlmFocus(focus))) => focus.render(
                &self.keys,
                focus_snapshot.as_ref().expect("snapshot for the focus view"),
                width,
                focus_height,
                &options.style,
                options.spinner_frame,
            ),
            (Some(selection), _) => {
                let mut lines = Text::new(&bold(&selection.title), 1, 1, None).render(width);
                lines.extend(selection.list.render(width));
                lines
            }
            (None, Some(Panel::Fork(selector))) => selector.render(width),
            (None, Some(Panel::HistorySearch(search))) => search.render(width),
            (None, Some(Panel::Tree(tree))) => tree.render(width),
            (None, Some(Panel::Settings(selector))) => selector.render(width),
            (None, Some(Panel::Logout(selector))) => selector.render(width),
            (None, Some(Panel::Login(selector, ..))) => selector.render(width),
            (
                None,
                Some(Panel::LoginPrompt {
                    provider,
                    method,
                    input,
                }),
            ) => {
                // `LoginDialogComponent` with its prompt (`showPrompt`).
                let t = Rc::clone(&self.theme);
                let border = fg(&t, "border", &"─".repeat(width.max(1)));
                let mut lines = vec![border.clone()];
                lines.extend(
                    Text::new(
                        &fg(&t, "accent", &bold(&format!("Login to {}", provider.name))),
                        1,
                        0,
                        None,
                    )
                    .render(width),
                );
                lines.push(String::new());
                lines.extend(Text::new(&fg(&t, "text", &format!("Enter {method}")), 1, 0, None).render(width));
                lines.extend(input.render(width));
                let hint = |id: &str, d: &str| crate::components::key_hint(&t, &key_text(&self.keys, id, false), d);
                lines.extend(
                    Text::new(
                        &format!(
                            "({} {})",
                            hint("tui.select.cancel", "to cancel,"),
                            hint("tui.select.confirm", "to submit")
                        ),
                        1,
                        0,
                        None,
                    )
                    .render(width),
                );
                lines.push(border);
                lines
            }
            (None, Some(Panel::Sessions(selector))) => selector.render(width),
            (None, Some(Panel::Instructions { input, .. })) => {
                let mut lines = Text::new(&bold("Custom summarization instructions"), 1, 1, None).render(width);
                lines.extend(
                    input
                        .render(width.saturating_sub(1))
                        .into_iter()
                        .map(|l| format!(" {l}")),
                );
                lines
            }
            (None, None) => {
                self.editor.focused = true;
                self.editor.terminal_rows = self.rows;
                self.editor.render(width)
            }
        };
        let mut parts = vec![pending, status];
        if let Some(line) = self.status_line(width) {
            parts.push(vec![line]);
        }
        if self.qol.attachments && self.selection.is_none() && self.panel.is_none() {
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
        self.cols = width;
        if let Some(frame) = self.overlay_frame(width, height) {
            return frame;
        }
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

/// Pi's path argument (`pathArgument`): the first word, or a quoted string.
pub fn path_argument(args: &str) -> Option<String> {
    let text = args.trim_start();
    let quote = text.chars().next()?;
    if quote == '"' || quote == '\'' {
        let rest = &text[1..];
        return rest.find(quote).map(|end| rest[..end].to_string());
    }
    Some(match text.find(char::is_whitespace) {
        Some(space) => text[..space].to_string(),
        None => text.to_string(),
    })
}
