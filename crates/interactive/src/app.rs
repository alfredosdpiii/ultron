//! The client TUI (`ExperimentalClientTui` in experimental/client-tui.ts): the startup header and transcript above
//! a dock of pending messages, status, the editor (or a selector in its place) and the footer, laid out full
//! screen. Keys follow Pi's: Enter sends (steers while a turn runs), Alt+Enter queues a follow-up, Escape aborts the
//! turn, Ctrl+C clears and exits on a second press, Ctrl+D exits on an empty editor.
//!
//! The app only holds view state. Everything that touches the session goes through [`LaneClient`] and comes back as
//! [`LaneEvent`]s, so handling a key or painting a frame never waits on the network or the disk.

use std::path::Path;
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
use crate::lane::{
    AutoreviewRequest, BashResult, LaneClient, LaneEvent, ModelInfo, PromptImage, QueueKind, Queued, SessionListing,
};
use crate::model_hub::{ModelHub, ModelHubEvent};
use crate::qol::{GitSource, NotifyFn, QolSettings, code_highlighter, rlm_highlighter};
use crate::rlm_cell::Highlighter;
use crate::selectors::{SelectorAction, UserMessageSelector};
use crate::session_selector::{SessionAction, SessionSelector};
use crate::settings_selector::{SettingsEvent, SettingsInput, SettingsSelector, format_http_idle};
use crate::transcript_picker::{PickerEvent, PickerMode, PickerOptions, TranscriptPicker};
use crate::tree_selector::{FilterMode, TreeAction, TreeSelector};

/// The error a navigation answers when its branch summary was aborted (session-core `BRANCH_SUMMARY_ABORTED`).
const BRANCH_SUMMARY_ABORTED: &str = "Branch summarization cancelled";
/// The error a navigation answers when an extension cancelled it (session-core `NAVIGATION_CANCELLED`).
const NAVIGATION_CANCELLED: &str = "Navigation cancelled";
/// The last choice of `/autoreview on`'s account picker.
const ALL_ACCOUNTS: &str = "All logged-in accounts";

/// Pi exits on a second Ctrl-C within this window.
const CTRL_C_EXIT_WINDOW: Duration = Duration::from_millis(500);
/// Pi's double-Esc window (`DOUBLE_ESCAPE_WINDOW_MS`).
const DOUBLE_ESCAPE_WINDOW: Duration = Duration::from_millis(500);
/// The RLM pane opens beside the chat from this terminal width; narrower, the full-screen graph opens.
pub const RLM_PANE_MIN_COLUMNS: usize = 120;
/// RLM polls while an RLM view (or the live wave summary) shows, and while nothing does (`RLM_POLL_*_MS`).
const RLM_POLL_VISIBLE: Duration = Duration::from_millis(1000);
const RLM_POLL_HIDDEN: Duration = Duration::from_millis(5000);
/// Frame summaries fetched per poll (`RLM_FRAME_LIMIT`).
const RLM_FRAME_LIMIT: u64 = 200;

/// Suspends the process until it is continued (`suspendTui`); `Err` is shown in the status line.
pub type SuspendHook = Box<dyn FnMut() -> Result<(), String>>;

/// `rlmPaneWidth`: about 40% of the terminal, at least 44 columns (border excluded).
pub fn rlm_pane_width(columns: usize) -> usize {
    44usize.max((columns as f64 * 0.4).round() as usize)
}

/// Whether `ULTRON_SPLASH` / `ULTRON_RLM_PANE_AUTO` turn a default-on feature off (`off`, `0`, `false`).
pub fn env_off(value: Option<&str>) -> bool {
    matches!(
        value.unwrap_or("").trim().to_lowercase().as_str(),
        "off" | "0" | "false"
    )
}

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
    /// `/mode`: the always-on skills the status names after a choice.
    Mode(Vec<String>),
    /// `/autoreview on`: the `gh` accounts, `(login, host)`, to review as.
    AutoreviewAccount(Vec<(String, String)>),
    /// "Summarize branch?" before navigating to this entry.
    Summarize(String),
    /// "Select authentication method": the options it chooses among (`None`: every provider).
    LoginMethod(Vec<crate::selectors::AuthProvider>, bool),
    /// An extension's `select` (or, `confirm`, its Yes/No) dialog: the request id.
    ExtensionSelect {
        id: String,
        confirm: bool,
    },
    /// "Switch to it?" after `/import`: the imported session's file, what was imported, and the Pi file it came from.
    SwitchImported {
        session_file: Option<String>,
        imported: String,
        source: String,
    },
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
    /// Pi's picker of its own sessions (`/import` without a path); a selection is imported, never changed.
    PiSessions(Box<SessionSelector>),
    Settings(Box<SettingsSelector>),
    /// The full-screen RLM graph (`RlmGraphFocus`) in the editor's place.
    RlmFocus(Box<crate::rlm::RlmGraphFocus>),
    Logout(Box<crate::selectors::AuthSelector>),
    /// `/trust`: the project trust selector.
    Trust(Box<crate::trust_selector::TrustSelector>),
    /// The `/login` provider selector; `Some(auth type)` when a method was chosen first (Esc goes back to it).
    Login(
        Box<crate::selectors::AuthSelector>,
        Vec<crate::selectors::AuthProvider>,
        Option<String>,
    ),
    /// `/scoped-models`: the Ctrl+P scope selector. `changed`: the user changed the selection since it opened;
    /// `session_scope`: it opened on a session scope (not `enabledModels`).
    ScopedModels {
        selector: Box<crate::scoped_models::ScopedModelsSelector>,
        changed: bool,
        session_scope: bool,
    },
    /// The login dialog of a subscription (OAuth) login running in the client.
    OAuthLogin(Box<crate::login_dialog::LoginDialog>),
    /// The login dialog's prompt for an API key.
    LoginPrompt {
        provider: crate::selectors::AuthProvider,
        method: String,
        input: Box<tui::input::Input>,
    },
    /// An extension's `input` dialog (`#showTextInput`): the request id.
    ExtensionInput {
        id: String,
        dialog: Box<crate::extension_dialogs::ExtensionInputDialog>,
    },
    /// The multi-line editor dialog (`#showTextEditor`): an extension's `editor` request, or the custom branch
    /// summary instructions.
    TextEditor {
        purpose: EditorPurpose,
        dialog: Box<crate::extension_dialogs::ExtensionEditorDialog>,
    },
    /// `/share`'s "Creating gist..." loader; Esc cancels the upload.
    ShareLoader(Box<crate::extension_dialogs::BorderedLoader>),
    /// `/bug`'s dialogs and loaders.
    BugReport(Box<crate::bug_report::BugFlow>),
}

/// What the editor dialog's text is for.
enum EditorPurpose {
    /// An extension's `editor` request id.
    Extension(String),
    /// Custom summarization instructions for navigating to this entry.
    Instructions(String),
}

/// The RLM inspections polled besides the subagent listing (`#refreshRlm`).
#[derive(Default)]
struct RlmPoll {
    frames: Vec<crate::rlm::visualizer::RlmFrame>,
    pool: Option<crate::rlm::visualizer::RlmPool>,
    context: Option<crate::rlm::visualizer::RlmContextState>,
    retained: std::collections::HashSet<String>,
    last: Option<Instant>,
    /// `agents.status {graph: true}`: the session's tasks, usage, limits, jobs, tool calls and workflows.
    status: Option<crate::rlm::visualizer::AgentsStatus>,
    /// The last failure of `agents.status` (cleared by the next answer).
    error: Option<String>,
    /// The worker rejected `agents.status {graph: true}` (`#graphListing` false): the plain listing is polled.
    plain_listing: bool,
    /// `progress.assess` of running tasks, polled only while the docked panel or the graph shows.
    progress: std::collections::HashMap<String, crate::rlm::visualizer::RlmProgress>,
    /// The assessment round in flight: its number, the answers so far and how many are still out.
    assessing: (
        u64,
        std::collections::HashMap<String, crate::rlm::visualizer::RlmProgress>,
        usize,
    ),
}

/// Running tasks assessed per poll (`RLM_MAX_ASSESSED`).
const RLM_MAX_ASSESSED: usize = 8;

/// `nextModel` (client-tui-actions.ts): the next model in the `enabledModels` scope when it selects any
/// (`resolveModelScopeFromModels`: globs, fuzzy ids and names, `:<thinking>` suffixes accepted and not applied,
/// Claude Code CLI models only by their full reference), kept in the available order; else among all available
/// models.
pub fn next_model<'a>(
    available: &'a [ModelInfo],
    current: &ModelInfo,
    direction: isize,
    enabled: &[String],
) -> Result<&'a ModelInfo, &'static str> {
    let mut candidates: Vec<&ModelInfo> = available.iter().collect();
    let mut scoped = false;
    if !enabled.is_empty() {
        let (in_scope, _) = model_scope::resolve_model_scope(enabled, available);
        if !in_scope.is_empty() {
            scoped = true;
            candidates.retain(|m| in_scope.iter().any(|s| model_scope::same(s.model, *m)));
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
    /// A compaction is running (its indicator says Esc cancels it).
    compacting: bool,
    /// The run is waiting to retry its request (`auto_retry_start`): Esc stops the retry.
    retrying: bool,
    selection: Option<Selection>,
    /// What `/mode` or `/engineering` does once [`LaneEvent::Mode`] reports the settings' mode.
    mode_request: Option<crate::modes::ModeRequest>,
    session_id: Option<String>,
    /// Package updates found before the first session opened (opening clears the transcript), shown with it.
    early_package_updates: Vec<String>,
    /// Startup notices that arrived before the first session opened, shown with it.
    early_startup_notices: Vec<crate::lane::StartupNotice>,
    /// `anthropicSubscriptionWarningShown`.
    subscription_warning_shown: bool,
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
    /// `/arminsayshi` portraits still animating: the portrait, when it started, and the frames it played.
    armin: Vec<(u64, Rc<std::cell::RefCell<crate::easter_eggs::Armin>>, Instant, u64)>,
    /// The running `!` command (Pi's pending `BashExecutionComponent`).
    bash: Option<BashExecution>,
    /// Transcript messages before this index are compacted away (the context starts after them).
    context_from: usize,
    compacted: bool,
    panel: Option<Panel>,
    /// `enabledModels` (Ctrl+P's scope).
    enabled_models: Vec<String>,
    /// The session-only Ctrl+P scope from `/scoped-models` (`provider/id` references); `None` follows
    /// `enabled_models`, an empty list is every model.
    scoped_ids: Option<Vec<String>>,
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
    /// The entry a navigation with a branch summary goes to, while the summary runs: Esc cancels it.
    summarizing_branch: Option<String>,
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
    /// The connection to a Radius-hosted server is being recovered (client-tui.ts `#busy` during recovery): input
    /// waits, except Ctrl+C and Ctrl+D on an empty editor.
    recovering: bool,
    /// The width of the last frame (transcript search renders the transcript at it).
    cols: usize,
    /// The RLM panel shows (`app.rlm.toggle`).
    rlm_visible: bool,
    /// Where `fd` is, for the editor's path completion (kept to rebuild the slash commands).
    fd_path: Option<std::path::PathBuf>,
    /// The session's extension commands that are slash commands of their own.
    extension_commands: Vec<String>,
    /// The extensions' keyboard shortcuts in force (crate::extension_shortcuts).
    extension_shortcuts: Vec<crate::extension_shortcuts::ExtensionShortcut>,
    /// Their argument completions, as they arrive.
    completions: crate::extension_commands::Completions,
    /// The last subagent listing (`rlm.list_subagents`).
    rlm_agents: Value,
    rlm_clock: crate::rlm::RlmClock,
    /// Extension dialogs not shown yet: they share the selector, so they are answered in order.
    extension_dialogs: std::collections::VecDeque<Value>,
    /// An extension dialog is open.
    extension_dialog_open: bool,
    /// The editor's keybindings (dialogs with an editor of their own).
    editor_keys: KeybindingsManager,
    /// The clock (tests drive double taps, countdowns and polls with their own).
    clock: Box<dyn Fn() -> Instant>,
    /// The theme setting, its live application and previews.
    theme_controller: Option<crate::theme_setup::ThemeController>,
    /// The terminal's color palette notifications are on (`CSI ? 2031 h` sent, not yet turned off).
    scheme_notifications: bool,
    /// `showHardwareCursor`: the painter shows the terminal's cursor at the editor's.
    pub show_hardware_cursor: bool,
    /// `terminal.clearOnShrink`: the split-viewport painter repaints a shrunk viewport whole.
    pub clear_on_shrink: bool,
    /// `doubleEscapeAction`: `tree`, `fork` or `none`.
    double_escape_action: String,
    last_escape: Option<Instant>,
    /// `ULTRON_SPLASH` allows the splash; `quietStartup` hides it.
    splash_allowed: bool,
    /// Ctrl+Z asked to suspend (`app.suspend`): the run loop gives the terminal back and stops the process.
    suspend_request: bool,
    suspend_hook: Option<SuspendHook>,
    /// Success statuses of copies waiting for the clipboard's answer, oldest first; `None` is a fullscreen
    /// selection copy, which answers with a flash.
    pending_copies: std::collections::VecDeque<Option<String>>,
    /// The fullscreen transcript viewport (`TuiAltScreen`): scroll position, selection, scrollbar, flashes.
    viewport: tui::fullscreen::FullscreenViewport,
    /// The mouse tracking the alternate screen turns on (`None`: none, as in tests that paint frames).
    mouse_tracking: Option<&'static str>,
    /// Where a pasted clipboard image is saved (`tmpdir()`).
    temp_dir: std::path::PathBuf,
    /// Extension statuses (`setStatus`) on the footer's status line.
    extension_statuses: Vec<(String, String)>,
    /// Extension widgets (`setWidget`): lines, and whether below the editor; by key.
    widgets: std::collections::BTreeMap<String, (Vec<String>, bool)>,
    /// The RLM pane (`app.rlm.pane`): open beside the chat on wide terminals; focused takes the keys.
    rlm_pane: crate::rlm::pane::RlmPane,
    rlm_pane_open: bool,
    rlm_pane_focused: bool,
    /// Runs the pane already opened for (or showed while open), and `op:<id>` for turns in which the user closed it.
    rlm_pane_handled: std::collections::HashSet<String>,
    /// `rlmPaneAutoOpen` (and not `ULTRON_RLM_PANE_AUTO=off`).
    rlm_pane_auto: bool,
    rlm_poll: RlmPoll,
    /// The wave summary showed at the last poll (it polls at the visible rate while it does).
    wave_active: bool,
    /// The running operation's start, ms since the epoch (the pane's auto-open counts runs from it).
    operation_started_ms: Option<f64>,
    /// The external editor edits the editor dialog's text, not the main editor's.
    edit_dialog: bool,
    /// A multi-field API-key login in progress: its flow and provider.
    api_login: Option<(crate::api_key_login::ApiKeyLogin, crate::selectors::AuthProvider)>,
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

/// The editor's slash commands: the client TUI's, with each extension command after `/extension` (Pi exposes each
/// extension command as its own slash command; built-ins keep their names) and `/model`'s completions.
fn slash_commands(
    models: &[ModelInfo],
    extension: &[(String, Option<String>)],
    completions: &crate::extension_commands::Completions,
    qol: &QolSettings,
) -> Vec<SlashCommand> {
    let direct: Vec<String> = crate::extension_commands::direct_extension_commands(extension)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let mut commands: Vec<SlashCommand> = crate::extension_commands::ordered(extension)
        .into_iter()
        .map(|(name, description, hint)| SlashCommand {
            argument_completions: direct.contains(&name).then(|| completions.for_command(&name)),
            name,
            description: Some(description),
            argument_hint: hint,
        })
        .collect();
    if qol.overlays {
        commands.extend(crate::commands::QOL_COMMANDS.iter().map(|c| SlashCommand {
            name: c.name.to_string(),
            description: Some(c.description.to_string()),
            argument_hint: (!c.hint.is_empty()).then(|| c.hint.to_string()),
            argument_completions: None,
        }));
    }
    let models = models.to_vec();
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
    // `/engineering` and `/autoreview`: their fixed arguments, by prefix (client-tui-pi-commands.ts).
    for (name, values) in [("engineering", ["true", "false"]), ("autoreview", ["on", "off"])] {
        if let Some(command) = commands.iter_mut().find(|c| c.name == name) {
            command.argument_completions = Some(Box::new(move |prefix: &str| {
                Some(
                    values
                        .iter()
                        .filter(|value| value.starts_with(prefix))
                        .map(|value| AutocompleteItem {
                            value: value.to_string(),
                            label: value.to_string(),
                            description: None,
                        })
                        .collect(),
                )
            }));
        }
    }
    commands
}

impl App {
    /// The session's extension commands arrived: each becomes a slash command of its own.
    fn set_extension_commands(&mut self, commands: Vec<(String, Option<String>)>) {
        self.extension_commands = crate::extension_commands::direct_extension_commands(&commands)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        self.editor.set_autocomplete_provider(crate::qol::autocomplete_provider(
            slash_commands(&self.models, &commands, &self.completions, &self.qol),
            &self.cwd,
            self.fd_path.clone(),
            &self.models,
            &self.qol,
        ));
    }

    pub fn new(options: AppOptions, client: Box<dyn LaneClient>) -> Self {
        let theme = options.theme;
        let editor_keys = options.editor_keys.clone();
        let mut editor = Editor::new(
            options.editor_keys,
            Default::default(),
            thinking_border(&theme, &options.thinking_level),
            1,
        );
        editor.set_select_list_theme(select_theme(&theme, "muted", "muted"));
        let qol = options.qol;
        let commands = slash_commands(&options.models, &[], &Default::default(), &qol);
        editor.set_autocomplete_provider(crate::qol::autocomplete_provider(
            commands,
            &options.cwd,
            options.fd_path.clone(),
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
            compacting: false,
            retrying: false,
            selection: None,
            mode_request: None,
            session_id: None,
            early_package_updates: Vec::new(),
            early_startup_notices: Vec::new(),
            subscription_warning_shown: false,
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
            armin: Vec::new(),
            bash: None,
            context_from: 0,
            compacted: false,
            panel: None,
            extension_dialogs: std::collections::VecDeque::new(),
            extension_dialog_open: false,
            enabled_models: Vec::new(),
            scoped_ids: None,
            overlay: None,
            escapes: Default::default(),
            rewind_pending: false,
            rewind_ids: Vec::new(),
            recent_models: Vec::new(),
            reset_display: false,
            tree: None,
            tree_filter: FilterMode::Default,
            skip_summary_prompt: false,
            summarizing_branch: None,
            path_usage: Value::Null,
            auto_compact: true,
            external_editor: None,
            edit_request: None,
            input_gate: Arc::default(),
            recovering: false,
            cols: 80,
            rlm_visible: false,
            fd_path: options.fd_path,
            extension_commands: Vec::new(),
            extension_shortcuts: Vec::new(),
            completions: Default::default(),
            rlm_agents: Value::Null,
            rlm_clock: crate::rlm::RlmClock::new(),
            clock: Box::new(Instant::now),
            editor_keys,
            theme_controller: None,
            scheme_notifications: false,
            show_hardware_cursor: false,
            clear_on_shrink: false,
            double_escape_action: "tree".into(),
            last_escape: None,
            splash_allowed: options.splash,
            suspend_request: false,
            suspend_hook: None,
            pending_copies: std::collections::VecDeque::new(),
            viewport: {
                let mut viewport = tui::fullscreen::FullscreenViewport::default();
                // The frames keep the OSC 133 marks the TUI always painted (the qol_parity byte guard); TS strips
                // them, which no terminal shows differently.
                viewport.keep_zone_marks = true;
                viewport
            },
            mouse_tracking: None,
            temp_dir: std::env::temp_dir(),
            extension_statuses: Vec::new(),
            widgets: std::collections::BTreeMap::new(),
            rlm_pane: crate::rlm::pane::RlmPane::new(),
            rlm_pane_open: false,
            rlm_pane_focused: false,
            rlm_pane_handled: std::collections::HashSet::new(),
            rlm_pane_auto: true,
            rlm_poll: RlmPoll::default(),
            wave_active: false,
            operation_started_ms: None,
            edit_dialog: false,
            api_login: None,
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
        if std::mem::take(&mut self.edit_dialog) {
            match (edited, self.panel.as_mut()) {
                (Some(text), Some(Panel::TextEditor { dialog, .. })) => dialog.set_text(&text),
                (Some(text), Some(Panel::BugReport(flow))) => {
                    if let Some(dialog) = flow.editor() {
                        dialog.set_text(&text);
                    }
                }
                _ => {}
            }
            return;
        }
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

    /// The clock the app reads for double taps, countdowns and RLM polls (tests pass their own).
    pub fn set_clock(&mut self, clock: Box<dyn Fn() -> Instant>) {
        self.clock = clock;
    }

    fn now(&self) -> Instant {
        (self.clock)()
    }

    fn editor_keys(&self) -> KeybindingsManager {
        self.editor_keys.clone()
    }

    /// Where a pasted clipboard image is saved (default: the system temp directory).
    pub fn set_temp_dir(&mut self, dir: std::path::PathBuf) {
        self.temp_dir = dir;
    }

    /// The presentation settings the TUI applies to itself at startup (`runClientTui`): the hardware cursor and
    /// clear-on-shrink, the double-Esc action, `quietStartup` (no splash), `rlmPaneAutoOpen` (with
    /// `ULTRON_RLM_PANE_AUTO`); and an explicitly set `editorPaddingX` and `autocompleteMaxVisible`.
    pub fn apply_startup_settings(&mut self, settings: &Value, var: &dyn Fn(&str) -> Option<String>) {
        self.show_hardware_cursor = settings["showHardwareCursor"] == true;
        self.clear_on_shrink = settings["terminal"]["clearOnShrink"] == true;
        if let Some(action) = settings["doubleEscapeAction"].as_str() {
            self.double_escape_action = action.to_string();
        }
        self.header.splash = self.splash_allowed && settings["quietStartup"] != true;
        self.rlm_pane_auto = settings["rlmPaneAutoOpen"] != false && !env_off(var("ULTRON_RLM_PANE_AUTO").as_deref());
        if let Some(padding) = settings["editorPaddingX"].as_f64() {
            self.apply_local_setting("editorPaddingX", &Value::from(padding));
        }
        if let Some(visible) = settings["autocompleteMaxVisible"].as_f64() {
            self.apply_local_setting("autocompleteMaxVisible", &Value::from(visible));
        }
    }

    /// `#applySettingLocally` (and what reads its setting on use): a presentation setting changed in `/settings`.
    fn apply_local_setting(&mut self, key: &str, value: &Value) {
        match key {
            "hideThinkingBlock" => self.chat.set_hide_thinking(*value == true),
            "theme" => {
                if let Some(setting) = value.as_str()
                    && let Some(controller) = self.theme_controller.as_mut()
                {
                    let applied = controller.set_setting(setting);
                    self.adopt_theme(&applied.theme);
                    self.sync_scheme_notifications(applied.query_scheme);
                    if let Some(error) = applied.error {
                        self.show_status(&format!("Error: {error}"));
                    }
                }
            }
            "showHardwareCursor" => self.show_hardware_cursor = *value == true,
            "terminal.clearOnShrink" => self.clear_on_shrink = *value == true,
            // `setPaddingX`: a finite number, floored, at least 0.
            "editorPaddingX" => {
                if let Some(padding) = value.as_f64().filter(|p| p.is_finite()) {
                    self.editor.set_padding_x(padding.max(0.0).floor() as usize);
                }
            }
            // `setAutocompleteMaxVisible`: floored, 3..=20.
            "autocompleteMaxVisible" => {
                if let Some(visible) = value.as_f64().filter(|v| v.is_finite()) {
                    self.editor
                        .set_autocomplete_max_visible(visible.clamp(3.0, 20.0).floor() as usize);
                }
            }
            "compaction.enabled" => self.auto_compact = *value != false,
            "doubleEscapeAction" => self.double_escape_action = value.as_str().unwrap_or("tree").to_string(),
            "quietStartup" => self.header.splash = self.splash_allowed && *value != true,
            "rlmPaneAutoOpen" => self.rlm_pane_auto = *value != false,
            _ => {}
        }
    }

    /// The theme setting's controller: the theme it selects applies now (`applyFromSettings`); a confidently detected
    /// appearance is saved as the setting, a theme that fails to load says so.
    pub fn set_theme_controller(&mut self, mut controller: crate::theme_setup::ThemeController) {
        let applied = controller.apply_from_settings();
        self.theme_controller = Some(controller);
        self.adopt_theme(&applied.theme);
        self.sync_scheme_notifications(applied.query_scheme);
        if let Some(save) = applied.save {
            self.client.save_setting("theme", Value::String(save));
        }
        if let Some(error) = applied.error {
            self.show_status(&format!("Error: {error}"));
        }
    }

    /// `setAutoSync` / `queryTerminalColorScheme`: the palette notifications follow the controller's auto sync, and an
    /// applied pair asks for the current scheme; the answer arrives as input ([`App::handle_input`]).
    fn sync_scheme_notifications(&mut self, query: bool) {
        let want = self.theme_controller.as_ref().is_some_and(|c| c.auto_sync());
        if want != self.scheme_notifications {
            self.scheme_notifications = want;
            self.terminal_writes
                .push(if want { "\x1b[?2031h" } else { "\x1b[?2031l" }.to_string());
        }
        if query {
            self.terminal_writes.push("\x1b[?996n".to_string());
        }
    }

    /// The writes that leave the terminal as it was (`TUI.stop`): the palette notifications off.
    pub fn stop_terminal_notifications(&mut self) {
        if std::mem::take(&mut self.scheme_notifications) {
            self.terminal_writes.push("\x1b[?2031l".to_string());
        }
    }

    /// Draw with `theme` from now on (`refreshTheme`): every view shares the app's theme; the highlighters and the
    /// transcript's rendered rows are rebuilt and the terminal repainted.
    fn adopt_theme(&mut self, theme: &Theme) {
        self.theme.adopt(theme);
        let mut style = self.chat.style();
        if self.qol.highlight {
            style.highlight = rlm_highlighter(&self.theme);
            style.code_highlight = Some(code_highlighter(&self.theme));
        }
        self.chat.set_style(style);
        self.update_editor_border();
        self.reset_display = true;
        self.committed.clear();
    }

    /// Whether Ctrl+Z asked to suspend; clears the request.
    pub fn take_suspend_request(&mut self) -> bool {
        std::mem::take(&mut self.suspend_request)
    }

    /// How Ctrl+Z suspends the process (the product stops it with SIGTSTP; returns once it continues). Without one,
    /// Ctrl+Z does nothing.
    pub fn set_suspend_hook(&mut self, hook: SuspendHook) {
        self.suspend_hook = Some(hook);
    }

    /// Run the suspend hook (the terminal is already given back); a failure shows in the status line.
    pub fn suspend(&mut self) -> bool {
        let Some(hook) = self.suspend_hook.as_mut() else {
            return false;
        };
        if let Err(error) = hook() {
            self.show_status(&error);
        }
        true
    }

    pub fn can_suspend(&self) -> bool {
        self.suspend_hook.is_some()
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
                ClipboardReply::Copied(result) => match (self.pending_copies.pop_front().flatten(), result) {
                    (Some(done), Ok(osc52)) => {
                        self.terminal_writes.extend(osc52);
                        self.show_status(&done);
                    }
                    (Some(_), Err(error)) => self.show_status(&format!("Error: {error}")),
                    // `copySelection`: "Copied!", or the clipboard's error, as a flash.
                    (None, Ok(osc52)) => {
                        self.terminal_writes.extend(osc52);
                        self.viewport.copied(Ok(()), Instant::now());
                    }
                    (None, Err(error)) => self.viewport.copied(Err(error), Instant::now()),
                },
                ClipboardReply::Image(data) if self.qol.attachments => {
                    let placeholder = self.attachments.add_image(data, "image/png", None);
                    self.editor.insert_text_at_cursor(&format!("{placeholder} "));
                }
                ClipboardReply::Image(data) => {
                    if let Some(path) = self.save_clipboard_image(&data) {
                        self.editor.insert_text_at_cursor(&path);
                    }
                }
                ClipboardReply::Text(text) if self.qol.attachments => {
                    self.editor.handle_input(&format!("{PASTE_START}{text}{PASTE_END}"));
                }
                ClipboardReply::Text(text) => {
                    if !text.is_empty() {
                        self.editor.insert_text_at_cursor(&text);
                    }
                }
                ClipboardReply::Error(e) if self.qol.attachments => self.show_status(&format!("Error: {e}")),
                // Clipboard access can be unavailable (no display, no permission): Pi pastes nothing.
                ClipboardReply::Error(_) => {}
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

    /// A clipboard image (base64 PNG) saved as `<tmp>/ultron-clipboard-<id>.png`; its path.
    fn save_clipboard_image(&self, data: &str) -> Option<String> {
        let bytes = tui::clipboard::image_bytes(data)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = self
            .temp_dir
            .join(format!("ultron-clipboard-{}-{nanos:x}.png", std::process::id()));
        std::fs::write(&path, bytes).ok()?;
        Some(path.display().to_string())
    }

    /// Copy `text` to the clipboard (`copyToClipboard`): native writers and clipboard tools before OSC 52, through
    /// the clipboard service when there is one (`done` shows once it worked); without one, OSC 52.
    fn copy_to_clipboard(&mut self, text: &str, done: &str) {
        match &self.clipboard {
            Some(clipboard) => {
                self.pending_copies.push_back(Some(done.to_string()));
                clipboard.copy_checked(text);
            }
            None => {
                self.terminal_writes
                    .push(format!("\x1b]52;c;{}\x07", base64(text.as_bytes())));
                self.show_status(done);
            }
        }
    }

    /// Copy a finished fullscreen selection (`copySelection` of tui-renderer.ts: `copyToClipboard`, then a flash):
    /// through the clipboard service (its tools come from the injected PATH), else OSC 52.
    fn copy_selection(&mut self, text: &str) {
        match &self.clipboard {
            Some(clipboard) => {
                self.pending_copies.push_back(None);
                clipboard.copy_checked(text);
            }
            None => {
                self.terminal_writes
                    .push(format!("\x1b]52;c;{}\x07", base64(text.as_bytes())));
                self.viewport.copied(Ok(()), Instant::now());
            }
        }
    }

    /// The mouse tracking the alternate screen turns on ([`tui::fullscreen::mouse_tracking`] of the terminal's
    /// environment); the product sets it, frames painted in tests leave the mouse alone.
    pub fn set_mouse_tracking(&mut self, mouse: Option<&'static str>) {
        self.mouse_tracking = mouse;
    }

    pub fn mouse_tracking(&self) -> Option<&'static str> {
        self.mouse_tracking
    }

    /// The fullscreen transcript viewport.
    pub fn viewport(&self) -> &tui::fullscreen::FullscreenViewport {
        &self.viewport
    }

    /// The alternate screen was entered again: the viewport forgets its gestures and layout.
    pub fn reset_viewport(&mut self) {
        self.viewport.reset();
    }

    /// The fullscreen viewport's timers (scrollbar hide, flashes, a held drag's auto-scroll) due by `now`; true
    /// when the frame changed.
    pub fn poll_viewport(&mut self, now: Instant) -> bool {
        self.viewport.poll(now)
    }

    /// When the fullscreen viewport next has a timer due.
    pub fn viewport_deadline(&self) -> Option<Instant> {
        self.viewport.deadline()
    }

    /// `handleViewportInput` in fullscreen: focus reports are the viewport's; while a full-frame overlay shows it
    /// gets the keys and the mouse (`shouldDeferViewportInputToOverlay`; the search and the pickers read SGR
    /// reports, the others ignore them); otherwise the viewport takes the mouse and its scroll keys. True when the
    /// input was taken.
    fn fullscreen_input(&mut self, data: &str) -> bool {
        if self.qol.viewport_split {
            return false;
        }
        if self.overlay.is_some() && !tui::fullscreen::is_focus_report(data) {
            return false;
        }
        match self.viewport.handle_input(data, &self.keys, Instant::now()) {
            tui::fullscreen::ViewportInput::Pass => false,
            tui::fullscreen::ViewportInput::Consumed => true,
            tui::fullscreen::ViewportInput::Copy(text) => {
                self.copy_selection(&text);
                true
            }
        }
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

    /// Whether something animates (the working spinner, a loader, a dialog's countdown).
    pub fn animating(&self) -> bool {
        self.chat.is_working()
            || self.bash.is_some()
            || matches!(self.panel, Some(Panel::ShareLoader(_)))
            || matches!(&self.panel, Some(Panel::BugReport(flow)) if flow.animating())
            || !self.armin.is_empty()
            || matches!(&self.panel, Some(Panel::ExtensionInput { dialog, .. }) if dialog.counting())
    }

    pub fn tick(&mut self) {
        self.ticks += 1;
        self.chat.tick();
        if let Some(Panel::ShareLoader(loader)) = self.panel.as_mut() {
            loader.tick();
        }
        if let Some(Panel::BugReport(flow)) = self.panel.as_mut() {
            flow.tick();
        }
        // The portraits play their frames at their own rate (`setInterval(…, 1000 / fps)`), whatever the tick rate.
        let now = self.now();
        let chat = &mut self.chat;
        self.armin.retain_mut(|(id, armin, started, played)| {
            let mut armin = armin.borrow_mut();
            let due = now.saturating_duration_since(*started).as_millis() as u64 * armin.fps() / 1000;
            let before = *played;
            while *played < due && !armin.done() {
                armin.tick();
                *played += 1;
            }
            if *played != before || armin.done() {
                chat.refresh_live(*id, armin.done());
            }
            !armin.done()
        });
        self.expire_dialogs();
        if let Some(bash) = self.bash.as_mut() {
            bash.tick();
        }
    }

    /// Called off the UI thread when an autocomplete answer computed there (`@` file search) is ready; the run
    /// loop then applies it with [`App::poll_autocomplete`].
    pub fn set_autocomplete_waker(&mut self, waker: impl Fn() + Send + Sync + 'static) {
        self.editor.set_autocomplete_waker(waker);
    }

    /// Apply autocomplete answers computed off the UI thread; true when the editor changed. Never blocks.
    pub fn poll_autocomplete(&mut self) -> bool {
        self.editor.poll_autocomplete()
    }

    /// Whether a debounced autocomplete request waits.
    pub fn fire_autocomplete_pending(&self) -> bool {
        self.editor.has_pending_autocomplete()
    }

    /// Fire the editor's debounced autocomplete request, when one waits.
    pub fn fire_autocomplete(&mut self) -> bool {
        if self.editor.has_pending_autocomplete() {
            self.editor.fire_autocomplete_debounce();
            self.ask_completions();
            return true;
        }
        false
    }

    /// Completions the editor asked for and nothing answered yet are fetched off the UI thread.
    fn ask_completions(&mut self) {
        for (name, prefix) in self.completions.take_wanted() {
            self.client.extension_completions(&name, &prefix);
        }
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

    /// One of interactive-mode.ts's startup notices (crate::startup_notices).
    fn show_startup_notice(&mut self, notice: crate::lane::StartupNotice) {
        use crate::lane::StartupNotice;
        use crate::startup_notices::{
            ANTHROPIC_SUBSCRIPTION_AUTH_WARNING, NewVersionNotice, WhatsNewNotice, warning_text,
        };
        match notice {
            StartupNotice::WhatsNew {
                markdown,
                collapse,
                version,
            } => self.chat.append_notice(Box::new(WhatsNewNotice {
                theme: Rc::clone(&self.theme),
                markdown,
                collapse,
                version,
                code_block_indent: None,
                hyperlinks: crate::render_settings::hyperlinks(),
            })),
            StartupNotice::NewVersion(version) => self.chat.append_notice(Box::new(NewVersionNotice {
                theme: Rc::clone(&self.theme),
                version,
                hyperlinks: crate::render_settings::hyperlinks(),
            })),
            StartupNotice::SubscriptionAuth => {
                if !self.subscription_warning_shown {
                    self.subscription_warning_shown = true;
                    let text = warning_text(&self.theme, ANTHROPIC_SUBSCRIPTION_AUTH_WARNING);
                    self.notice(&text);
                }
            }
        }
    }

    /// `maybeWarnAboutAnthropicSubscriptionAuth`: only an `anthropic` model is checked, and only until it warned.
    fn check_subscription_auth(&mut self) {
        if !self.subscription_warning_shown && self.model.provider == "anthropic" {
            self.client.subscription_auth_warning(&self.model.provider);
        }
    }

    /// The "Package Updates Available" notice, when there are any.
    fn show_package_updates(&mut self, packages: Vec<String>) {
        if !packages.is_empty() {
            self.chat
                .append_notice(Box::new(crate::package_updates::PackageUpdateNotice {
                    theme: Rc::clone(&self.theme),
                    packages,
                }));
        }
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
                    // `init`: the changelog of a session without messages, then the subscription-auth check.
                    if self.transcript.is_empty() {
                        self.client.startup_changelog();
                    }
                    let packages = std::mem::take(&mut self.early_package_updates);
                    self.show_package_updates(packages);
                    for notice in std::mem::take(&mut self.early_startup_notices) {
                        self.show_startup_notice(notice);
                    }
                    self.check_subscription_auth();
                }
                self.update_title();
                // `setupExtensionShortcuts`: the session's extensions under the effective keybindings.
                self.extension_shortcuts.clear();
                self.client
                    .extension_shortcuts(crate::extension_shortcuts::effective_keybindings(&self.keys));
            }
            LaneEvent::Entries(entries) => self.load_entries(&entries),
            LaneEvent::OperationStarted(id) => {
                self.operation_started_ms = Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0.0, |d| d.as_millis() as f64),
                );
                self.operation = Some(id);
                self.aborting = false;
                self.turn_started = Some(Instant::now());
                self.chat.set_working(true);
            }
            LaneEvent::Streaming(message) => self.chat.set_streaming(&message),
            LaneEvent::Message(message) => {
                self.chat.add_message(&message);
                // A Claude turn that switched to API credits (or back) says so once, under its answer.
                if let Some(notice) = crate::lane::route_notice(&message) {
                    self.notice(&fg(&self.theme, "warning", &notice));
                }
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
                if std::mem::take(&mut self.retrying) {
                    self.chat.clear_retry_indicator();
                }
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
                // A model change (selector, cycling, /model) checks the new model's credential.
                if self.session_id.is_some() {
                    self.check_subscription_auth();
                }
            }
            LaneEvent::Thinking(level) => {
                self.thinking_level = level;
                self.update_editor_border();
            }
            LaneEvent::Notice(text) => self.notice(&text),
            LaneEvent::ExtensionCommands(commands) => self.set_extension_commands(commands),
            LaneEvent::ExtensionShortcuts { shortcuts, diagnostics } => {
                self.extension_shortcuts = shortcuts;
                if let Some(issues) = crate::extension_shortcuts::issues_block(&self.theme, &diagnostics) {
                    self.notice(&issues);
                }
            }
            LaneEvent::ExtensionCompletions { name, prefix, items } => {
                self.completions.answer(&name, &prefix, items);
                self.editor.refresh_autocomplete();
                self.ask_completions();
            }
            LaneEvent::GoalReply(text) => self.notice(&fg(&self.theme, "dim", &text)),
            LaneEvent::BashDone(result) => self.bash_done(result),
            LaneEvent::BugSummary(result) => self.bug_summary_done(result),
            LaneEvent::BugReport(outcome) => self.bug_report_done(outcome),
            // interactive-mode.ts `handleBashCommand`: the runner already reported the error; nothing ran.
            LaneEvent::BashFailedClosed => self.bash = None,
            // `showExtensionError`, without the stack (it does not cross the extension process boundary).
            LaneEvent::ExtensionError { path, error } => {
                let text = fg(&self.theme, "error", &format!("Extension \"{path}\" error: {error}"));
                self.chat.append_text(&text);
            }
            LaneEvent::CompactionStarted { reason } => {
                self.compacting = true;
                self.chat.set_compacting_for(&reason);
            }
            LaneEvent::CompactionEnded { status, error } => self.auto_compaction_ended(&status, error),
            LaneEvent::Compacted(result) => self.compacted(result),
            // interactive-mode.ts: the error in the transcript and the retry countdown; the summary's own indicator
            // again when the retry starts; the countdown cleared when the retries end.
            LaneEvent::SummarizationRetryScheduled {
                attempt,
                max_attempts,
                delay_ms,
                error,
            } => {
                self.notice(&fg(&self.theme, "error", &format!("Error: {error}")));
                self.chat.set_retry_indicator(attempt, max_attempts, delay_ms);
            }
            LaneEvent::SummarizationRetryStarted { branch_summary, reason } => {
                self.chat.clear_retry_indicator();
                if branch_summary {
                    self.chat.set_branch_summary_indicator();
                } else {
                    self.chat.set_compacting_for(&reason);
                }
            }
            LaneEvent::SummarizationRetryFinished => self.chat.clear_retry_indicator(),
            // interactive-mode.ts `auto_retry_start` / `auto_retry_end`: the countdown, Esc stops the retry; the
            // error only when the last attempt failed too.
            LaneEvent::AutoRetryStart {
                attempt,
                max_attempts,
                delay_ms,
            } => {
                self.retrying = true;
                self.chat.set_retry_indicator(attempt, max_attempts, delay_ms);
            }
            LaneEvent::AutoRetryEnd { attempt, failed } => {
                self.retrying = false;
                self.chat.clear_retry_indicator();
                if let Some(error) = failed {
                    let text = format!("Error: Retry failed after {attempt} attempts: {error}");
                    self.notice(&fg(&self.theme, "error", &text));
                }
            }
            LaneEvent::Tree { tree, leaf } => self.show_tree(tree, leaf, None),
            LaneEvent::RlmStatus(listed) => {
                self.rlm_agents = listed;
                self.auto_open_rlm_pane();
                // The wave summary is live: poll at the visible rate while it shows.
                self.wave_active = !self.wave_summary(200).is_empty();
            }
            LaneEvent::RlmInspected { tag, result } => self.rlm_inspected(&tag, result),
            LaneEvent::ShareLoader(true) => {
                let loader = crate::extension_dialogs::BorderedLoader::new(
                    Rc::clone(&self.theme),
                    self.keys.clone(),
                    "Creating gist...",
                );
                self.editor.focused = false;
                self.panel = Some(Panel::ShareLoader(Box::new(loader)));
            }
            LaneEvent::ShareLoader(false) => {
                if matches!(self.panel, Some(Panel::ShareLoader(_))) {
                    self.panel = None;
                }
            }
            LaneEvent::AllSessions(sessions) => {
                if let Some(Panel::Sessions(selector)) = self.panel.as_mut() {
                    selector.set_all_sessions(sessions);
                }
            }
            LaneEvent::PiAllSessions(sessions) => {
                if let Some(Panel::PiSessions(selector)) = self.panel.as_mut() {
                    selector.set_all_sessions(sessions);
                }
            }
            LaneEvent::Mode {
                engineering,
                always_skills,
            } => self.mode_read(engineering, always_skills),
            LaneEvent::AutoreviewAccounts(accounts) => self.autoreview_accounts(accounts),
            LaneEvent::Settings {
                settings,
                env_overrides,
                themes,
                terminal_theme,
            } => {
                // The client's own theme registry and terminal appearance, when it has them (`getAvailableThemes`).
                let (themes, terminal_theme) = match &self.theme_controller {
                    Some(controller) => (controller.available(), controller.terminal_theme().to_string()),
                    None => (themes, terminal_theme),
                };
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
            LaneEvent::Login(step) => self.login_step(step),
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
            LaneEvent::CatalogRefreshed(failed) => self.catalog_refreshed(failed),
            LaneEvent::PackageUpdates(packages) => match self.session_id {
                Some(_) => self.show_package_updates(packages),
                None => self.early_package_updates = packages,
            },
            LaneEvent::StartupNotice(notice) => match self.session_id {
                Some(_) => self.show_startup_notice(notice),
                None => self.early_startup_notices.push(notice),
            },
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
                self.chat.set_navigating(false);
                let summarized = self.summarizing_branch.take();
                match result {
                    // An extension's `session_before_tree` cancelled it (interactive-mode.ts).
                    Err(error) if error == NAVIGATION_CANCELLED => self.show_status(NAVIGATION_CANCELLED),
                    // The summary was cancelled: the tree selector again, on the same entry (interactive-mode.ts).
                    Err(error) if error == BRANCH_SUMMARY_ABORTED => {
                        self.show_status(BRANCH_SUMMARY_ABORTED);
                        if let (Some(entry), Some((tree, leaf))) = (summarized, self.tree.clone()) {
                            self.show_tree(tree, leaf, Some(&entry));
                        }
                    }
                    Ok(text) => {
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
            LaneEvent::AgentTranscript { id, messages } => {
                if let Some(Overlay::AgentHub(hub)) = self.overlay.as_mut() {
                    let entries = messages
                        .into_iter()
                        .enumerate()
                        .map(|(i, message)| {
                            let at = message["timestamp"].as_i64().map(|t| t.to_string()).unwrap_or_default();
                            crate::copy_targets::TranscriptEntry::new(&format!("{id}#{i}"), &at, message)
                        })
                        .collect();
                    hub.set_transcript(&id, entries);
                }
            }
            LaneEvent::AgentNotice(notice) => match self.overlay.as_mut() {
                Some(Overlay::AgentHub(hub)) => hub.set_notice(Some(notice)),
                _ => self.show_status(&notice),
            },
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
            LaneEvent::PiImported {
                imported,
                source,
                session_file,
                current,
            } => {
                if current {
                    self.show_status(&format!("{imported} (the current session)"));
                    return;
                }
                let items = ["Switch to the imported session", "Stay here"]
                    .into_iter()
                    .map(|choice| SelectItem {
                        value: choice.into(),
                        label: choice.into(),
                        description: None,
                    })
                    .collect();
                let title = format!("{imported}. Switch to it?");
                self.open_selection(
                    &title,
                    items,
                    None,
                    Purpose::SwitchImported {
                        session_file,
                        imported,
                        source,
                    },
                );
            }
            LaneEvent::Sessions(sessions) => self.show_resume(sessions),
            LaneEvent::PiSessions(sessions) => self.show_pi_sessions(sessions),
            LaneEvent::Status(text) => self.show_status(&text),
            LaneEvent::Recovery { busy, status } => {
                self.recovering = busy;
                self.show_status(&status);
            }
            LaneEvent::Error(text) => self.show_status(&format!("Error: {text}")),
            LaneEvent::ExtensionUi(request) => self.extension_ui(request),
            LaneEvent::Trust(view) => {
                let selector =
                    crate::trust_selector::TrustSelector::new(Rc::clone(&self.theme), self.keys.clone(), view);
                self.panel = Some(Panel::Trust(Box::new(selector)));
                self.editor.focused = false;
            }
            LaneEvent::DebugLogWritten { path, summary } => {
                let notice = crate::debug_log::DebugNotice {
                    theme: Rc::clone(&self.theme),
                    path,
                    summary,
                };
                self.chat.append_notice(Box::new(notice));
            }
        }
    }

    /// `#serveExtensionUI`: dialogs open the selector or a text input (one at a time, in order), `notify` shows in
    /// the status line, `set_editor_text` fills the editor.
    fn extension_ui(&mut self, request: Value) {
        match request["method"].as_str().unwrap_or_default() {
            "select" | "confirm" | "input" | "editor" => {
                self.extension_dialogs.push_back(request);
                self.next_extension_dialog();
            }
            "notify" => self.show_status(request["message"].as_str().unwrap_or_default()),
            // Pi's footer status line (`FooterDataProvider.setExtensionStatus`): no text removes the key's status.
            "setStatus" => {
                let key = request["statusKey"].as_str().unwrap_or_default().to_string();
                self.extension_statuses.retain(|(k, _)| *k != key);
                if let Some(text) = request["statusText"].as_str() {
                    self.extension_statuses.push((key, text.to_string()));
                }
            }
            // `#setWidget`: lines above the editor, or below it (`belowEditor`); no lines removes the widget.
            "setWidget" => {
                let key = request["widgetKey"].as_str().unwrap_or_default().to_string();
                match request["widgetLines"].as_array() {
                    Some(lines) => {
                        let lines = lines
                            .iter()
                            .map(|l| l.as_str().unwrap_or_default().to_string())
                            .collect();
                        let below = request["widgetPlacement"] == "belowEditor";
                        self.widgets.insert(key, (lines, below));
                    }
                    None => {
                        self.widgets.remove(&key);
                    }
                }
            }
            "setTitle" => {
                let title = request["title"].as_str().unwrap_or_default();
                self.terminal_writes.push(format!("\x1b]0;{title}\x07"));
            }
            "set_editor_text" => {
                self.editor.set_text(request["text"].as_str().unwrap_or_default());
                self.update_editor_border();
            }
            _ => {}
        }
    }

    fn next_extension_dialog(&mut self) {
        if self.extension_dialog_open {
            return;
        }
        let Some(request) = self.extension_dialogs.pop_front() else {
            return;
        };
        self.extension_dialog_open = true;
        let id = request["id"].as_str().unwrap_or_default().to_string();
        let title = request["title"].as_str().unwrap_or_default();
        match request["method"].as_str().unwrap_or_default() {
            "select" => {
                let items = request["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(|o| SelectItem {
                        value: o.into(),
                        label: o.into(),
                        description: None,
                    })
                    .collect();
                self.open_selection(title, items, None, Purpose::ExtensionSelect { id, confirm: false });
            }
            "confirm" => {
                let title = match request["message"].as_str().filter(|m| !m.is_empty()) {
                    Some(message) => format!("{title}: {message}"),
                    None => title.to_string(),
                };
                let item = |value: &str, label: &str| SelectItem {
                    value: value.into(),
                    label: label.into(),
                    description: None,
                };
                let items = vec![item("yes", "Yes"), item("no", "No")];
                self.open_selection(&title, items, None, Purpose::ExtensionSelect { id, confirm: true });
            }
            "editor" => {
                let dialog = crate::extension_dialogs::ExtensionEditorDialog::new(
                    Rc::clone(&self.theme),
                    self.keys.clone(),
                    self.editor_keys(),
                    title,
                    request["prefill"].as_str(),
                );
                self.editor.focused = false;
                self.panel = Some(Panel::TextEditor {
                    purpose: EditorPurpose::Extension(id),
                    dialog: Box::new(dialog),
                });
            }
            _ => {
                let dialog = crate::extension_dialogs::ExtensionInputDialog::new(
                    Rc::clone(&self.theme),
                    self.keys.clone(),
                    title,
                    request["timeout"].as_u64(),
                    None,
                    self.now(),
                );
                self.editor.focused = false;
                self.panel = Some(Panel::ExtensionInput {
                    id,
                    dialog: Box::new(dialog),
                });
            }
        }
    }

    /// An input dialog whose countdown ran out cancels (`CountdownTimer` `onExpire`).
    pub fn expire_dialogs(&mut self) {
        let now = self.now();
        if let Some(Panel::ExtensionInput { id, dialog }) = &self.panel
            && dialog.expired(now)
        {
            let id = id.clone();
            self.panel = None;
            self.answer_extension_dialog(&id, serde_json::json!({ "cancelled": true }));
        }
    }

    /// Answer the open extension dialog and show the next one.
    fn answer_extension_dialog(&mut self, id: &str, answer: Value) {
        self.client.extension_ui_respond(id, answer);
        self.extension_dialog_open = false;
        self.next_extension_dialog();
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
        // `consumeTerminalColorSchemeReport`: a scheme report is the terminal's, never a key.
        if let Some(scheme) = crate::theme_setup::parse_color_scheme_report(data) {
            if let Some(theme) = self
                .theme_controller
                .as_mut()
                .and_then(|c| c.terminal_scheme_changed(scheme))
            {
                self.adopt_theme(&theme);
            }
            self.sync_scheme_notifications(false);
            return;
        }
        self.handle_key(data);
        self.ask_completions();
    }

    fn handle_key(&mut self, data: &str) {
        // `TuiAltScreen`'s input listener runs before the focused component.
        if self.fullscreen_input(data) {
            return;
        }
        // `handleInput` while `#busy`: only Ctrl+C, and Ctrl+D on an empty editor, are read.
        if self.recovering {
            if self.keys.matches(data, "app.clear") {
                self.handle_ctrl_c();
            } else if self.editor.text().is_empty() && self.keys.matches(data, "app.exit") {
                self.exit = true;
            }
            return;
        }
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
        // The focused RLM pane takes the keys; its toggle moves the focus back to the chat.
        if self.rlm_pane_focused && self.rlm_pane_shown() {
            if self.keys.matches(data, "app.rlm.pane") {
                self.rlm_pane();
                return;
            }
            let snapshot = self.rlm_snapshot();
            let height = self.rows;
            if self.rlm_pane.handle_input(&self.keys, data, &snapshot, height) == crate::rlm::pane::PaneEvent::Close {
                self.close_rlm_pane();
            }
            return;
        }
        // custom-editor.ts: extension shortcuts before any built-in binding; the handler runs in the session.
        if let Some(shortcut) = crate::extension_shortcuts::matching(&self.extension_shortcuts, data) {
            let key = shortcut.key.clone();
            self.client
                .extension_shortcut(&key, crate::extension_shortcuts::effective_keybindings(&self.keys));
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
        // Pi's Ctrl+V (`clipboardPasteText`): a clipboard image is saved to a temp file and its path inserted, else the
        // clipboard's text (with the attachments, an image becomes an attachment instead).
        if k("app.clipboard.pasteImage") {
            if let Some(clipboard) = &self.clipboard {
                clipboard.request(ClipboardRead::ImageOrText);
            }
            return;
        }
        // Pi's Ctrl+Z (`suspendTui`): the run loop gives the terminal back and suspends the process.
        if k("app.suspend") {
            self.suspend_request = true;
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
        // A run waiting to retry its request: Esc stops the retry (interactive-mode.ts `abortRetry`).
        if self.retrying {
            self.retrying = false;
            self.client.abort_retry();
            return;
        }
        // A branch summary in progress: Esc cancels it (interactive-mode.ts `abortBranchSummary`).
        if self.summarizing_branch.is_some() {
            self.client.abort();
            return;
        }
        // A manual compaction (no run): Esc cancels it (interactive-mode.ts `abortCompaction`). During a run the
        // abort below cancels the run's compaction with the run.
        if self.compacting && self.operation.is_none() {
            self.status = "Cancelling compaction…".into();
            self.client.abort();
            return;
        }
        if let Some(operation) = self.operation.clone() {
            self.restore_queued_to_editor();
            self.aborting = true;
            self.status = format!("Aborting {operation}…");
            self.client.abort();
        }
        if self.operation.is_some() || !self.editor.text().trim().is_empty() {
            return;
        }
        let now = self.now();
        // With the overlays, idle on an empty editor a second Esc opens the rewind selector (omp's Esc-Esc).
        if self.qol.overlays {
            if self.escapes.tap(now) {
                self.rewind_pending = true;
                self.client.fork_messages();
            }
            return;
        }
        // Pi's `doubleEscapeAction`: the tree or fork selector on a second Esc within the window.
        if self.double_escape_action == "none" {
            return;
        }
        if self
            .last_escape
            .is_some_and(|t| now.saturating_duration_since(t) < DOUBLE_ESCAPE_WINDOW)
        {
            self.last_escape = None;
            if self.double_escape_action == "tree" {
                self.client.tree();
            } else {
                self.client.fork_messages();
            }
        } else {
            self.last_escape = Some(now);
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
        let qol_command = self.qol.overlays && crate::commands::QOL_COMMANDS.iter().any(|c| c.name == name);
        if !COMMANDS.iter().any(|c| c.name == name)
            && !qol_command
            && !crate::commands::PI_COMMANDS.contains(&name)
            && !self.extension_commands.iter().any(|c| c == name)
        {
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
                    markdown: hotkeys_markdown(&self.keys)
                        + &crate::extension_shortcuts::hotkeys_section(&self.extension_shortcuts),
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
            "scoped-models" => self.open_scoped_models(),
            "model" if self.qol.overlays && args.is_empty() => self.open_model_hub(),
            "model" => self.model_command(args),
            "resume" => self.client.list_sessions(),
            "import" => match path_argument(args) {
                Some(path) => {
                    let path = if Path::new(&path).is_absolute() {
                        path
                    } else {
                        Path::new(&self.cwd).join(path).display().to_string()
                    };
                    self.show_status(&format!("Importing {path}…"));
                    self.client.import_pi(&path);
                }
                // Pi's picker of its own sessions for this project (client-tui-sessions.ts `showPiSessionPicker`).
                None => self.client.list_pi_sessions(),
            },
            "thinking" => self.thinking_command(args),
            "mode" => match args.trim() {
                "" => self.request_mode(crate::modes::ModeRequest::Picker),
                name => match crate::modes::find(name) {
                    Some(mode) => self.request_mode(crate::modes::ModeRequest::Set(mode.engineering)),
                    None => self.show_status(&format!(
                        "Error: Unknown mode \"{name}\". Available modes: {}.",
                        crate::modes::MODES.iter().map(|m| m.id).collect::<Vec<_>>().join(", ")
                    )),
                },
            },
            "engineering" => match crate::modes::engineering_arg(args) {
                Some(None) => self.request_mode(crate::modes::ModeRequest::Status),
                Some(Some(enabled)) => self.request_mode(crate::modes::ModeRequest::Set(enabled)),
                None => self.show_status("Usage: /engineering true|false"),
            },
            "autoreview" => match args.trim().to_lowercase().as_str() {
                "" => self.client.autoreview(AutoreviewRequest::Status),
                "on" => self.client.autoreview(AutoreviewRequest::Accounts),
                "off" => self.client.autoreview(AutoreviewRequest::Off),
                _ => self.show_status("Usage: /autoreview on|off"),
            },
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
            "trust" => self.client.read_trust(),
            "debug" => {
                // `host.renderedLines()`: the frame at the terminal's size.
                let (width, height) = (self.cols.max(1), self.rows.max(1));
                let lines = self.render(width, height);
                self.client.write_debug_log(crate::debug_log::DebugCapture {
                    width,
                    height,
                    lines,
                    messages: self.transcript.clone(),
                    session_id: self.session_id.clone(),
                });
            }
            "reload" => {
                self.show_status("Reloading plugins…");
                self.client.reload_plugins();
            }
            // Extension commands run in the session's worker; their dialogs and notices come back as extension UI.
            "mcp" => self.client.extension_command("mcp", args.trim()),
            // `reviewCommand` (slash-commands-provider.ts): sent as typed; the worker's agent controller expands it
            // (session_core::review_command), as it does for queued messages.
            "review" => {
                self.show_status("Reviewing changes…");
                let message = format!("/review {args}").trim().to_string();
                if self.operation.is_some() {
                    let id = self.client.steer(&message);
                    self.status = format!("Queued {id}.");
                } else {
                    self.client.prompt(&message);
                }
            }
            "bug" => self.open_bug_report(args),
            // `handleArminSaysHi` and `handleDementedDelves` (only the bare commands are easter eggs).
            "arminsayshi" if args.is_empty() => {
                let armin = Rc::new(std::cell::RefCell::new(crate::easter_eggs::Armin::new(
                    Rc::clone(&self.theme),
                    Box::new(crate::easter_eggs::seeded_random(self.now())),
                )));
                let id = self.ticks ^ (self.armin.len() as u64) << 48;
                self.chat
                    .append_live_notice(id, Box::new(crate::easter_eggs::Shared(Rc::clone(&armin))));
                let now = self.now();
                self.armin.push((id, armin, now, 0));
            }
            "dementedelves" if args.is_empty() => {
                let announcement = crate::easter_eggs::EarendilAnnouncement::new(Rc::clone(&self.theme));
                self.chat.append_notice(Box::new(announcement));
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
            "goal" => self.client.goal(args),
            // With the overlays, `/agents` opens the agent hub (the inspection text is the worker's, not ported).
            "agents" if self.qol.overlays && args.is_empty() => self.client.agents(),
            // The inspection commands: one read-only host request each, answered as indented lines.
            name if crate::inspection::INSPECTORS.contains(&name) => {
                match crate::inspection::inspection_request(name, args.trim()) {
                    Some(Ok((request, payload))) => {
                        let title = format!("/{name} {args}");
                        self.client.inspect(title.trim(), &request, payload);
                    }
                    Some(Err(usage)) => self.show_status(&format!("Error: {usage}")),
                    None => {}
                }
            }
            // `legacyExtensionCommand` of slash-commands-provider.ts.
            "extension" => {
                let (name, rest) = args.split_once(' ').unwrap_or((args, ""));
                if name.is_empty() {
                    self.show_status("Error: Usage: /extension <name> [args]");
                } else {
                    self.client.extension_command(name, rest);
                }
            }
            name if self.extension_commands.iter().any(|c| c == name) => self.client.extension_command(name, args),
            other => self.show_status(&format!("Error: /{other} is not available in the Rust build yet")),
        }
    }

    /// `showError`: `Error: <message>` in the transcript.
    fn show_error(&mut self, message: &str) {
        let text = fg(&self.theme, "error", &format!("Error: {message}"));
        self.notice(&text);
    }

    /// `/bug [description]` (`handleBugCommand`): the report's dialogs in the editor's place.
    fn open_bug_report(&mut self, args: &str) {
        let hint = args.trim();
        let model = (!self.model.name.is_empty()).then(|| (self.model.name.clone(), self.model.provider.clone()));
        let flow = crate::bug_report::BugFlow::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            self.editor_keys(),
            (!hint.is_empty()).then_some(hint),
            model,
            self.client.bug_report_target(),
        );
        self.editor.focused = false;
        self.panel = Some(Panel::BugReport(Box::new(flow)));
    }

    fn bug_cancelled(&mut self) {
        self.panel = None;
        self.show_status("Bug report cancelled");
    }

    /// What a key in the `/bug` flow asks for.
    fn bug_action(&mut self, action: crate::bug_report::Action) {
        use crate::bug_report::Action;
        let Some(Panel::BugReport(flow)) = self.panel.as_mut() else {
            return;
        };
        match action {
            Action::None => {}
            Action::Cancelled => self.bug_cancelled(),
            Action::Offline => {
                self.panel = None;
                self.show_error("Uploading bug reports requires online mode. Use Export as Zip instead.");
            }
            Action::Summarize => {
                let hint = flow.choices.hint.clone();
                self.client.bug_report_summary(hint.as_deref());
            }
            Action::CancelSummary => {
                self.client.abort_bug_report_summary();
                self.bug_cancelled();
            }
            Action::Deliver => {
                let choices = flow.choices.clone();
                let request = crate::lane::BugReportRequest {
                    hint: choices.hint,
                    include_session: choices.include_session,
                    summary: flow.summary.clone(),
                    upload: choices.upload,
                };
                // An export writes at once (no loader); an upload shows its loader until it answers.
                if !request.upload {
                    self.panel = None;
                }
                self.client.bug_report(request);
            }
            Action::CancelUpload => {
                self.client.cancel_bug_report();
                self.bug_cancelled();
            }
            Action::ExportAfterFailure => {
                self.panel = None;
                self.client.bug_report_export();
            }
            Action::External(text) => {
                if self.external_editor.is_some() {
                    self.edit_dialog = true;
                    self.edit_request = Some(text);
                } else {
                    self.show_status("No editor configured. Set $VISUAL or $EDITOR environment variable.");
                }
            }
        }
    }

    /// The summary arrived; a flow that was cancelled meanwhile ignores it.
    fn bug_summary_done(&mut self, result: Result<String, String>) {
        let Some(Panel::BugReport(flow)) = self.panel.as_mut() else {
            return;
        };
        if !matches!(flow.step, crate::bug_report::Step::Summarizing(_)) {
            return;
        }
        match result {
            Ok(summary) => {
                let action = flow.summarized(summary);
                self.bug_action(action);
            }
            Err(error) => {
                self.panel = None;
                self.show_error(&format!("Failed to write bug report summary: {error}"));
            }
        }
    }

    fn bug_report_done(&mut self, outcome: crate::lane::BugReportOutcome) {
        use crate::lane::BugReportOutcome;
        let uploading = matches!(
            &self.panel,
            Some(Panel::BugReport(flow)) if matches!(flow.step, crate::bug_report::Step::Uploading(_))
        );
        match outcome {
            BugReportOutcome::UploadFailed(failure) => {
                if let (true, Some(Panel::BugReport(flow))) = (uploading, self.panel.as_mut()) {
                    flow.upload_failed(&failure);
                }
            }
            outcome => {
                if uploading {
                    self.panel = None;
                }
                match outcome {
                    BugReportOutcome::Uploaded(id) => {
                        self.show_status(&format!("Bug report uploaded. Report ID: {id}"));
                    }
                    BugReportOutcome::Exported { path, id } => {
                        self.show_status(&format!("Bug report exported to: {path}\nReport ID: {id}"));
                    }
                    BugReportOutcome::Failed(error) => self.show_error(&error),
                    BugReportOutcome::UploadFailed(_) => {}
                }
            }
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

    /// After a compaction the run it happened in (if any) is still working.
    fn compaction_over(&mut self) {
        self.compacting = false;
        self.chat.set_compacting(false);
        if self.operation.is_some() {
            self.chat.set_working(true);
        }
    }

    /// An automatic compaction ended without a compaction entry (interactive-mode.ts `compaction_end`): a failure is
    /// shown in the transcript, a cancellation in the status line; a declined one (trimmed instead) shows nothing.
    fn auto_compaction_ended(&mut self, status: &str, error: Option<String>) {
        self.compaction_over();
        match status {
            "aborted" => self.status = "Auto-compaction cancelled".into(),
            "failed" => {
                let message = error.unwrap_or_else(|| "Auto-compaction failed".into());
                self.notice(&fg(&self.theme, "error", &message));
            }
            _ => {}
        }
    }

    fn compacted(&mut self, result: Result<Value, String>) {
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
                self.status = if error == "Compaction cancelled" {
                    error
                } else {
                    format!("Operation rejected: {error}")
                };
            }
        }
        // After the transcript restarted (clearing it drops the indicator).
        self.compaction_over();
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

    /// `/mode` and `/engineering` act on the settings' mode, which the client reads.
    fn request_mode(&mut self, request: crate::modes::ModeRequest) {
        self.mode_request = Some(request);
        self.client.read_mode();
    }

    fn mode_read(&mut self, engineering: bool, always_skills: Vec<String>) {
        use crate::modes::{MODES, ModeRequest, current};
        match self.mode_request.take() {
            Some(ModeRequest::Picker) => {
                let selected = current(engineering);
                let items = MODES
                    .iter()
                    .map(|mode| SelectItem {
                        value: mode.id.to_string(),
                        label: if mode.id == selected.id {
                            format!("{} (selected)", mode.label)
                        } else {
                            mode.label.to_string()
                        },
                        description: Some(mode.description.to_string()),
                    })
                    .collect();
                self.open_selection("Select mode:", items, Some(selected.id), Purpose::Mode(always_skills));
            }
            Some(ModeRequest::Status) => self.show_status(&crate::modes::status(engineering, &always_skills)),
            Some(ModeRequest::Set(enabled)) => self.set_engineering(enabled, &always_skills),
            None => {}
        }
    }

    /// `/autoreview on`: one account is reviewed as; with several, the user picks one or all of them.
    fn autoreview_accounts(&mut self, accounts: Vec<(String, String)>) {
        match accounts.as_slice() {
            [] => self.show_status("No gh account is logged in: run gh auth login, then /autoreview on"),
            [(login, host)] => self.client.autoreview(AutoreviewRequest::On {
                accounts: None,
                reviewing_as: format!("{host}/{login}"),
            }),
            _ => {
                let mut items: Vec<SelectItem> = accounts
                    .iter()
                    .enumerate()
                    .map(|(index, (login, host))| SelectItem {
                        value: index.to_string(),
                        label: format!("{login} ({host})"),
                        description: None,
                    })
                    .collect();
                items.push(SelectItem {
                    value: ALL_ACCOUNTS.into(),
                    label: ALL_ACCOUNTS.into(),
                    description: None,
                });
                self.open_selection(
                    "Review pull requests as",
                    items,
                    None,
                    Purpose::AutoreviewAccount(accounts),
                );
            }
        }
    }

    /// Write the `engineering` setting; the session's next request renders its prompt in that mode.
    fn set_engineering(&mut self, enabled: bool, always_skills: &[String]) {
        self.client.save_setting("engineering", Value::Bool(enabled));
        self.show_status(&crate::modes::changed(enabled, always_skills));
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
        let scope = self.scoped_ids.clone().unwrap_or_else(|| self.enabled_models.clone());
        let next = match next_model(&self.models, &self.model, direction, &scope) {
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
                    // `loadTrace`: the frame's trace (`rlm.frames {id}`), answered as an inspection.
                    crate::rlm::focus::FocusEvent::LoadTrace { key, trace_id } => self.client.rlm_inspect(
                        &format!("trace:{key}"),
                        "rlm.frames",
                        serde_json::json!({ "id": trace_id }),
                    ),
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
                Some(SessionAction::LoadAll) => self.client.list_all_sessions(),
                Some(SessionAction::Rename(path, name)) => {
                    if Some(&path) == self.session_file.as_ref() {
                        self.session_name = Some(name.clone());
                        self.update_title();
                    }
                    self.client.rename_session(&path, &name);
                }
                None => {}
            },
            Some(Panel::PiSessions(selector)) => match selector.handle_input(data) {
                Some(SessionAction::Selected(path)) => {
                    self.panel = None;
                    self.show_status(&format!("Importing {path}…"));
                    self.client.import_pi(&path);
                }
                Some(SessionAction::Cancelled) => self.panel = None,
                Some(SessionAction::LoadAll) => self.client.list_all_pi_sessions(),
                Some(SessionAction::Delete(_) | SessionAction::Rename(..)) => {
                    selector.set_status(true, "Pi sessions are read-only here", Duration::from_secs(3));
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
            Some(Panel::ScopedModels { selector, changed, .. }) => match selector.handle_input(data) {
                crate::scoped_models::ScopedAction::Changed(enabled) => {
                    *changed = true;
                    self.update_session_models(enabled);
                }
                crate::scoped_models::ScopedAction::Persist(enabled) => self.persist_scope(enabled),
                crate::scoped_models::ScopedAction::Cancel => self.panel = None,
                crate::scoped_models::ScopedAction::None => {}
            },
            Some(Panel::OAuthLogin(dialog)) => match dialog.handle_input(data) {
                crate::login_dialog::DialogAction::Answer(answer) => match self.api_login.as_mut() {
                    Some((login, _)) => {
                        let steps = login.answer(answer);
                        self.api_login_steps(steps);
                    }
                    None => self.client.login_reply(Some(answer)),
                },
                crate::login_dialog::DialogAction::Cancel => {
                    // Esc stops the login and closes its dialog silently (`Login cancelled`).
                    self.panel = None;
                    if self.api_login.take().is_none() {
                        self.client.login_reply(None);
                    }
                }
                crate::login_dialog::DialogAction::None => {}
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
            Some(Panel::Trust(selector)) => match selector.handle_input(data) {
                Some(crate::trust_selector::TrustAction::Selected { trusted, updates }) => {
                    self.panel = None;
                    self.client.set_project_trust(trusted, updates);
                }
                Some(crate::trust_selector::TrustAction::Cancelled) => self.panel = None,
                None => {}
            },
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
                // `onThemePreview`: the theme shows while the user browses; leaving restores the setting's.
                Some(SettingsEvent::Preview(setting)) => {
                    if let Some(theme) = self.theme_controller.as_mut().and_then(|c| c.preview(&setting)) {
                        self.adopt_theme(&theme);
                    }
                }
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
                    Some(text) => self.copy_to_clipboard(&text, "Copied selected message to clipboard"),
                },
                Some(TreeAction::Label(entry, label)) => self.client.set_label(&entry, label.as_deref()),
                None => {}
            },
            Some(Panel::ExtensionInput { id, dialog }) => {
                use crate::extension_dialogs::DialogOutcome;
                let answer = match dialog.handle_input(data) {
                    DialogOutcome::Submit(value) => serde_json::json!({ "value": value }),
                    DialogOutcome::Cancel => serde_json::json!({ "cancelled": true }),
                    _ => return,
                };
                let id = id.clone();
                self.panel = None;
                self.answer_extension_dialog(&id, answer);
            }
            Some(Panel::TextEditor { dialog, .. }) => {
                use crate::extension_dialogs::DialogOutcome;
                let outcome = dialog.handle_input(data);
                if let DialogOutcome::External(text) = outcome {
                    if self.external_editor.is_some() {
                        self.edit_dialog = true;
                        self.edit_request = Some(text);
                    } else {
                        self.show_status("No editor configured. Set $VISUAL or $EDITOR environment variable.");
                    }
                    return;
                }
                let submitted = match outcome {
                    DialogOutcome::Submit(text) => Some(text),
                    DialogOutcome::Cancel => None,
                    _ => return,
                };
                let Some(Panel::TextEditor { purpose, .. }) = self.panel.take() else {
                    return;
                };
                match (purpose, submitted) {
                    (EditorPurpose::Extension(id), Some(value)) => {
                        self.answer_extension_dialog(&id, serde_json::json!({ "value": value }));
                    }
                    (EditorPurpose::Extension(id), None) => {
                        self.answer_extension_dialog(&id, serde_json::json!({ "cancelled": true }));
                    }
                    (EditorPurpose::Instructions(entry), Some(text)) => self.navigate(&entry, true, Some(&text)),
                    // Back to the summary choice.
                    (EditorPurpose::Instructions(entry), None) => self.ask_summary(entry),
                }
            }
            Some(Panel::BugReport(flow)) => {
                let action = flow.handle_input(data);
                self.bug_action(action);
            }
            Some(Panel::ShareLoader(loader)) => {
                // `loader.onAbort`: the upload stops and the loader closes.
                if loader.handle_input(data) {
                    self.panel = None;
                    self.client.cancel_share();
                    self.show_status("Share cancelled");
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

    /// The `enabledModels` patterns as the selector shows them (`configuredEnabledIds`): the models they select, then
    /// patterns that select none (shown as unavailable); `None` without patterns.
    fn configured_enabled_ids(&self) -> Option<Vec<String>> {
        if self.enabled_models.is_empty() {
            return None;
        }
        let (scoped, diagnostics) = model_scope::resolve_model_scope(&self.enabled_models, &self.models);
        let mut ids: Vec<String> = scoped
            .iter()
            .map(|s| format!("{}/{}", s.model.provider, s.model.id))
            .collect();
        for diagnostic in diagnostics {
            if diagnostic.code == "no-match" && !ids.contains(&diagnostic.pattern) {
                ids.push(diagnostic.pattern);
            }
        }
        Some(ids)
    }

    /// Pi's `/scoped-models` (`showScopedModelsSelector`): the session scope, else `enabledModels`; the catalogs refresh
    /// while it is open.
    fn open_scoped_models(&mut self) {
        let session_scope = self.scoped_ids.as_ref().is_some_and(|ids| !ids.is_empty());
        let enabled = if session_scope {
            self.scoped_ids.clone()
        } else {
            self.configured_enabled_ids()
        };
        let selector = crate::scoped_models::ScopedModelsSelector::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            &self.models,
            enabled,
            Some("Refreshing model catalogs…"),
        );
        self.panel = Some(Panel::ScopedModels {
            selector: Box::new(selector),
            changed: false,
            session_scope,
        });
        self.editor.focused = false;
        self.client.refresh_catalog();
    }

    /// The catalogs refreshed (or timed out, or failed): the open selector shows the catalog it has now.
    fn catalog_refreshed(&mut self, refresh: crate::lane::CatalogRefresh) {
        use crate::lane::CatalogRefresh;
        let configured = self.configured_enabled_ids();
        let models = self.models.clone();
        if let Some(Panel::ScopedModels {
            selector,
            changed,
            session_scope,
        }) = self.panel.as_mut()
        {
            let enabled = (!*changed && !*session_scope).then_some(configured);
            selector.update_models(&models, enabled);
            match refresh {
                CatalogRefresh::TimedOut => {
                    selector.set_refresh_status("Model refresh timed out; showing cached models.", "warning");
                }
                CatalogRefresh::Failed(error) => {
                    selector.set_refresh_status(&format!("Could not refresh model catalogs: {error}"), "warning");
                }
                CatalogRefresh::Done(failed) if !failed.is_empty() => selector.set_refresh_status(
                    &format!("Could not refresh {}; showing cached models.", failed.join(", ")),
                    "warning",
                ),
                CatalogRefresh::Done(_) => selector.set_refresh_status("Model catalogs refreshed.", "success"),
            }
        }
    }

    /// `updateSessionModels`: the session scope Ctrl+P uses; every model when the selection covers them all or none
    /// is available.
    fn update_session_models(&mut self, enabled: Option<Vec<String>>) {
        let available: Vec<String> = self.models.iter().map(|m| format!("{}/{}", m.provider, m.id)).collect();
        self.scoped_ids = Some(match enabled {
            Some(ids)
                if ids.iter().any(|id| available.contains(id)) && !available.iter().all(|id| ids.contains(id)) =>
            {
                ids
            }
            _ => Vec::new(),
        });
    }

    /// `onPersist`: the selection saved to `enabledModels` (removed when it is every model).
    fn persist_scope(&mut self, enabled: Option<Vec<String>>) {
        let available: Vec<String> = self.models.iter().map(|m| format!("{}/{}", m.provider, m.id)).collect();
        let patterns =
            enabled.filter(|ids| !(ids.len() == available.len() && ids.iter().all(|id| available.contains(id))));
        let value = patterns
            .as_ref()
            .map_or(serde_json::Value::Null, |p| serde_json::json!(p));
        self.client.save_setting("enabledModels", value);
        self.enabled_models = patterns.unwrap_or_default();
        self.show_status("Model selection saved to settings");
    }

    /// A step of the subscription login running in the client (`runProviderLogin`'s dialog).
    fn login_step(&mut self, step: crate::lane::LoginStep) {
        use crate::lane::LoginStep;
        match step {
            LoginStep::Started { name, reach } => {
                let dialog = crate::login_dialog::LoginDialog::new(self.keys.clone(), &name, &reach);
                self.panel = Some(Panel::OAuthLogin(Box::new(dialog)));
                self.editor.focused = false;
            }
            LoginStep::Event(event) => {
                if let Some(Panel::OAuthLogin(dialog)) = self.panel.as_mut() {
                    dialog.show_event(&self.theme, &event);
                }
            }
            LoginStep::Prompt(prompt) => {
                if let Some(Panel::OAuthLogin(dialog)) = self.panel.as_mut() {
                    dialog.show_prompt(&self.theme, &prompt);
                }
            }
            LoginStep::Finished => {
                if matches!(self.panel, Some(Panel::OAuthLogin(_))) {
                    self.panel = None;
                }
            }
        }
    }

    /// `startProviderLogin`: the login dialog (a subscription login runs in the client; an API key is prompted for).
    fn start_login(&mut self, option: crate::selectors::AuthProvider) {
        if option.auth_type == "oauth" {
            // The flow runs in the client; its dialog opens with its first step (`LoginStep::Started`).
            if self.client.oauth_login(&option.id, &option.name) {
                return;
            }
            self.show_status(&format!(
                "Error: Failed to login to {}: signing in with an account is not available in the Rust build yet; use an API key",
                option.name
            ));
            return;
        }
        // A provider whose key prompt has a choice or several fields (`apiKey.login`): its flow runs in the dialog.
        if let Some((login, steps)) = crate::api_key_login::ApiKeyLogin::start(&option.id) {
            let dialog = crate::login_dialog::LoginDialog::new(self.keys.clone(), &option.name, "local");
            self.panel = Some(Panel::OAuthLogin(Box::new(dialog)));
            self.editor.focused = false;
            self.api_login = Some((login, option));
            self.api_login_steps(steps);
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

    /// The multi-field API-key login's next steps: notices and the next prompt go to the dialog; the credential is
    /// saved (`runLoginDialog`'s outcome statuses come from the client), a failure says so.
    fn api_login_steps(&mut self, steps: Vec<crate::api_key_login::Step>) {
        use crate::api_key_login::Step;
        for step in steps {
            match step {
                Step::Event(event) => {
                    if let Some(Panel::OAuthLogin(dialog)) = self.panel.as_mut() {
                        dialog.show_event(&self.theme, &event);
                    }
                }
                Step::Prompt(prompt) => {
                    if let Some(Panel::OAuthLogin(dialog)) = self.panel.as_mut() {
                        dialog.show_prompt(&self.theme, &prompt);
                    }
                }
                Step::Done(credential) => {
                    self.panel = None;
                    if let Some((login, _)) = self.api_login.take() {
                        self.client.save_credential(login.provider(), credential);
                    }
                }
                Step::Failed(error) => {
                    self.panel = None;
                    if let Some((_, option)) = self.api_login.take() {
                        self.show_status(&format!("Error: Failed to save API key for {}: {error}", option.name));
                    }
                }
            }
        }
    }

    /// `changeSetting` (client-tui-settings.ts): save, apply the live ones here, report the outcome.
    fn setting_changed(&mut self, key: &str, value: Value) {
        const UNUSED_BY_ULTRON: [&str; 17] = [
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
            "treeFilterMode" => self.tree_filter = FilterMode::parse(value.as_str().unwrap_or("default")),
            "enabledModels" => {
                self.enabled_models = value
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
            }
            _ => self.apply_local_setting(key, &value),
        }
        if key == "tuiMode" {
            // The Rust TUI reads tuiMode when it starts (fullscreen, or the scrollback split).
            self.show_status(&format!(
                "Saved tuiMode: {}. It takes effect when Ultron restarts.",
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

    /// `#refreshRlm`: poll the read-only inspections the RLM views draw from (the subagents, recent frames, the
    /// kernel pool, the context state, retained instances); answers arrive as lane events.
    fn refresh_rlm(&mut self) {
        self.rlm_poll.last = Some(self.now());
        // The `rlm.spawn` subagents: the Rust worker keeps them in a task journal of their own, which
        // `agents.status` does not list (TypeScript's one task host lists both).
        self.client.rlm_status();
        self.inspect_agents_status();
        self.client
            .rlm_inspect("frames", "rlm.frames", serde_json::json!({ "limit": RLM_FRAME_LIMIT }));
        self.client.rlm_inspect("pool", "rlm.pool", serde_json::json!({}));
        self.client.rlm_inspect("context", "ctx.state", serde_json::json!({}));
        self.client
            .rlm_inspect("instances", "instances.list", serde_json::json!({}));
    }

    /// `#scheduleRlmPolling`: poll every second while an RLM view or the live wave summary shows, else every five
    /// seconds. The run loop calls this on every pass; true when it polled.
    pub fn poll_rlm(&mut self) -> bool {
        let live = self.rlm_visible
            || matches!(self.panel, Some(Panel::RlmFocus(_)))
            || self.rlm_pane_open
            || self.wave_active;
        let interval = if live { RLM_POLL_VISIBLE } else { RLM_POLL_HIDDEN };
        let now = self.now();
        if self.session_id.is_none()
            || self
                .rlm_poll
                .last
                .is_some_and(|last| now.saturating_duration_since(last) < interval)
        {
            return false;
        }
        self.refresh_rlm();
        true
    }

    /// `agents.status {graph: true}`, or the plain listing once the worker rejected the graph field.
    fn inspect_agents_status(&mut self) {
        let payload = if self.rlm_poll.plain_listing {
            serde_json::json!({})
        } else {
            serde_json::json!({ "graph": true })
        };
        self.client.rlm_inspect("agents", "agents.status", payload);
    }

    /// `agents.status`'s answer (`#refreshRlm`): a worker that rejects `{graph: true}` is asked for the plain
    /// listing at once; then the running tasks' progress is assessed.
    fn agents_status_inspected(&mut self, result: Result<Value, String>) {
        match result {
            Ok(value) => {
                self.rlm_poll.status = Some(crate::rlm::visualizer::parse_agents_status(&value));
                self.rlm_poll.error = None;
            }
            Err(error) => {
                if !self.rlm_poll.plain_listing && error.contains("Unknown payload field") {
                    self.rlm_poll.plain_listing = true;
                    self.inspect_agents_status();
                }
                self.rlm_poll.error = Some(error);
            }
        }
        self.assess_progress();
        self.auto_open_rlm_pane();
        self.wave_active = !self.wave_summary(200).is_empty();
    }

    /// "Progress assessments are cheap host reads; only fetch them for a few running tasks while visible": while the
    /// docked panel or the graph shows, `progress.assess` of the first [`RLM_MAX_ASSESSED`] running tasks; else none.
    fn assess_progress(&mut self) {
        let round = self.rlm_poll.assessing.0 + 1;
        self.rlm_poll.assessing = (round, std::collections::HashMap::new(), 0);
        if !(self.rlm_visible || matches!(self.panel, Some(Panel::RlmFocus(_)))) {
            self.rlm_poll.progress.clear();
            return;
        }
        let running: Vec<String> = self
            .rlm_status()
            .tasks
            .into_iter()
            .filter(|task| crate::rlm::visualizer::is_active_state(&task.state))
            .take(RLM_MAX_ASSESSED)
            .map(|task| task.id)
            .collect();
        if running.is_empty() {
            self.rlm_poll.progress.clear();
            return;
        }
        self.rlm_poll.assessing.2 = running.len();
        for id in running {
            self.client.rlm_inspect(
                &format!("progress:{round}:{id}"),
                "progress.assess",
                serde_json::json!({ "task_id": id }),
            );
        }
    }

    /// One `progress.assess` answer of round `round`; the round's assessments replace the shown ones when the last
    /// answer arrives (a failed or unreadable one is left out).
    fn progress_inspected(&mut self, round: u64, id: &str, result: Result<Value, String>) {
        let (current, answers, left) = &mut self.rlm_poll.assessing;
        if *current != round || *left == 0 {
            return;
        }
        if let Some(progress) = result
            .ok()
            .and_then(|value| crate::rlm::visualizer::parse_progress(&value))
        {
            answers.insert(id.to_string(), progress);
        }
        *left -= 1;
        if *left == 0 {
            self.rlm_poll.progress = std::mem::take(answers);
        }
    }

    /// The session's work as the RLM views list it: `agents.status`, then the subagents it does not list.
    fn rlm_status(&self) -> crate::rlm::visualizer::AgentsStatus {
        use crate::rlm::visualizer::parse_agents_status;
        let mut status = self.rlm_poll.status.clone().unwrap_or_default();
        let subagents = parse_agents_status(&serde_json::json!({"tasks": self.rlm_agents["subagents"]}));
        for task in subagents.tasks {
            if !status.tasks.iter().any(|known| known.id == task.id) {
                status.tasks.push(task);
            }
        }
        status
    }

    /// An RLM inspection's answer (`#refreshRlm`): a failed one leaves the pool and context unknown.
    fn rlm_inspected(&mut self, tag: &str, result: Result<Value, String>) {
        use crate::rlm::visualizer::{parse_context_state, parse_frames, parse_pool, parse_retained};
        if tag == "agents" {
            return self.agents_status_inspected(result);
        }
        if let Some((round, id)) = tag.strip_prefix("progress:").and_then(|rest| rest.split_once(':')) {
            let round = round.parse().unwrap_or_default();
            let id = id.to_string();
            return self.progress_inspected(round, &id, result);
        }
        if let Some(key) = tag.strip_prefix("trace:") {
            if let Some(Panel::RlmFocus(focus)) = self.panel.as_mut() {
                match &result {
                    Ok(trace) => focus.set_trace(key, Ok(trace)),
                    Err(error) => focus.set_trace(key, Err(error)),
                }
            }
            return;
        }
        match (tag, result) {
            ("frames", Ok(value)) => self.rlm_poll.frames = parse_frames(&value),
            ("pool", result) => self.rlm_poll.pool = result.ok().and_then(|v| parse_pool(&v)),
            ("context", result) => self.rlm_poll.context = result.ok().and_then(|v| parse_context_state(&v)),
            ("instances", Ok(value)) => self.rlm_poll.retained = parse_retained(&value),
            _ => {}
        }
    }

    /// `#toggleRlm`: the docked RLM panel under the status.
    fn toggle_rlm(&mut self) {
        self.rlm_visible = !self.rlm_visible;
        self.refresh_rlm();
    }

    /// Whether the RLM pane shows beside the chat (open, and the terminal wide enough). The split-viewport painter
    /// keeps finished rows in the terminal's scrollback, which a side column cannot share, so it has no pane.
    fn rlm_pane_shown(&self) -> bool {
        self.rlm_pane_open && self.cols >= RLM_PANE_MIN_COLUMNS && !self.qol.viewport_split
    }

    /// `#toggleRlmPane`: open the RLM pane with the focus; again, the focus moves between the pane and the chat (the
    /// pane stays open). On a narrow terminal the full-screen graph opens instead.
    fn rlm_pane(&mut self) {
        if self.cols < RLM_PANE_MIN_COLUMNS || self.qol.viewport_split {
            self.rlm_pane_open = false;
            self.rlm_pane_focused = false;
            self.open_rlm_focus();
            return;
        }
        if !self.rlm_pane_open {
            self.rlm_pane_open = true;
            self.rlm_pane_focused = true;
        } else {
            self.rlm_pane_focused = !self.rlm_pane_focused;
        }
        self.refresh_rlm();
    }

    /// `#closeRlmPane`: closed during a turn, that turn does not auto-open it again (a later turn may).
    fn close_rlm_pane(&mut self) {
        if let Some(operation) = &self.operation {
            self.rlm_pane_handled.insert(format!("op:{operation}"));
        }
        self.rlm_pane_open = false;
        self.rlm_pane_focused = false;
    }

    /// `#autoOpenRlmPane`: work the pane shows (a child, a background task, a workflow node, a map fan-out) that
    /// starts in a run the pane has not opened for opens it beside the chat without the focus. Only on a terminal
    /// wide enough to split, on the chat screen with the graph closed; a pane the user closed stays closed for the
    /// rest of that turn and for the runs it already showed. It never closes the pane.
    fn auto_open_rlm_pane(&mut self) {
        use crate::rlm::pane::{RunKind, build_dag_runs, pane_work_runs};
        let snapshot = self.rlm_snapshot();
        let runs = build_dag_runs(&snapshot);
        let ids: Vec<String> = pane_work_runs(&runs, self.operation_started_ms)
            .into_iter()
            .map(|run| match run.kind {
                RunKind::Turn => format!(
                    "turn:{}",
                    self.operation
                        .clone()
                        .or_else(|| run.started_at.map(|s| s.to_string()))
                        .unwrap_or_else(|| run.id.clone())
                ),
                _ => run.key.clone(),
            })
            .collect();
        if !ids.iter().any(|id| !self.rlm_pane_handled.contains(id)) {
            return;
        }
        if self.rlm_pane_open {
            self.rlm_pane_handled.extend(ids);
            return;
        }
        if !self.rlm_pane_auto || self.cols < RLM_PANE_MIN_COLUMNS || self.qol.viewport_split {
            return;
        }
        if self.selection.is_some() || self.panel.is_some() || self.overlay.is_some() {
            return;
        }
        self.rlm_pane_handled.extend(ids);
        if let Some(operation) = &self.operation
            && self.rlm_pane_handled.contains(&format!("op:{operation}"))
        {
            return;
        }
        self.rlm_pane_open = true;
        self.rlm_pane_focused = false;
    }

    /// `#renderWaveSummary`: the live wave summary above the editor, hidden while the docked panel or the graph
    /// shows.
    fn wave_summary(&mut self, width: usize) -> Vec<String> {
        if self.rlm_visible || matches!(self.panel, Some(Panel::RlmFocus(_))) {
            return Vec::new();
        }
        let snapshot = self.rlm_snapshot();
        let runs = crate::rlm::pane::build_dag_runs(&snapshot);
        let style = crate::rlm::RlmStyle::themed(Rc::clone(&self.theme));
        crate::rlm::pane::render_wave_summary(&runs, width.saturating_sub(2).max(1), &style, snapshot.now, 4)
            .into_iter()
            .map(|line| format!(" {line}"))
            .collect()
    }

    /// `#openRlmFocus`: the full-screen RLM graph in the editor's place; Esc returns to the chat.
    fn open_rlm_focus(&mut self) {
        if matches!(self.panel, Some(Panel::RlmFocus(_))) {
            return;
        }
        self.panel = Some(Panel::RlmFocus(Box::new(crate::rlm::RlmGraphFocus::new())));
        self.editor.focused = false;
        self.refresh_rlm();
    }

    /// The focus view's height: the screen above the footer (`#openRlmFocus`).
    fn rlm_focus_height(&self) -> usize {
        let footer = render_footer(&self.theme, &self.footer_view(), self.cols.max(1)).len();
        self.rows.saturating_sub(footer + 1).max(10)
    }

    /// `#buildRlmSnapshot`: the session's subagents (polled) and the root turn's `rlm` cells (the transcript).
    fn rlm_snapshot(&mut self) -> crate::rlm::RlmSnapshot {
        use crate::rlm::visualizer::{RunningTool, TranscriptView, extract_root_cell, extract_turn};
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
        let status = self.rlm_status();
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
            frames: self.rlm_poll.frames.clone(),
            pool: self.rlm_poll.pool.clone(),
            context: self.rlm_poll.context.clone(),
            retained: self.rlm_poll.retained.clone(),
            progress: self.rlm_poll.progress.clone(),
            error: self.rlm_poll.error.clone(),
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
        self.summarizing_branch = summarize.then(|| entry_id.to_string());
        self.show_status(if summarize {
            "Summarizing branch… (Esc to cancel)"
        } else {
            "Navigating…"
        });
        // The lane's navigation operation shows its indicator until the session answers.
        self.chat.set_navigating(true);
        // The session resolves the selection (AgentSession `navigateTree`) and answers the editor's text.
        self.client.navigate(Some(entry_id), summarize, instructions);
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
                    self.copy_to_clipboard(&content, &format!("Copied {label} to clipboard"));
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
                HubEvent::OpenTranscript(id) => {
                    hub.set_transcript(&id, Vec::new());
                    self.client.agent_transcript(&id);
                }
                HubEvent::Revive(id) => {
                    self.client.revive_agent(&id);
                    self.client.agents();
                }
                HubEvent::Steer { id, text } => {
                    // A parked (interrupted) agent is revived first; its brief runs again as a new subagent.
                    let parked = hub
                        .rows()
                        .iter()
                        .any(|r| r.id == id && r.status == crate::agent_hub::AgentStatus::Parked);
                    if parked {
                        self.client.revive_agent(&id);
                        self.client.agents();
                    } else {
                        self.client.steer_agent(&id, &text);
                        self.client.agent_transcript(&id);
                    }
                }
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

    /// `/import` without a path: Pi's session selector over Pi's sessions of this project (`showPiSessionPicker`).
    /// Nothing in it is the attached session, and Pi's files are read-only here.
    fn show_pi_sessions(&mut self, sessions: Vec<SessionListing>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let mut selector = SessionSelector::new(
            Rc::clone(&self.theme),
            self.keys.clone(),
            sessions,
            None,
            self.home.as_deref().unwrap_or(""),
            now,
        );
        selector.set_show_rename_hint(false);
        self.panel = Some(Panel::PiSessions(Box::new(selector)));
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
                            let dialog = crate::extension_dialogs::ExtensionEditorDialog::new(
                                Rc::clone(&self.theme),
                                self.keys.clone(),
                                self.editor_keys(),
                                "Custom summarization instructions",
                                None,
                            );
                            self.editor.focused = false;
                            self.panel = Some(Panel::TextEditor {
                                purpose: EditorPurpose::Instructions(entry),
                                dialog: Box::new(dialog),
                            });
                        }
                        choice => self.navigate(&entry, choice == "Summarize", None),
                    },
                    Purpose::SwitchImported {
                        session_file,
                        imported,
                        source,
                    } => match (item.value.as_str(), session_file) {
                        ("Switch to the imported session", Some(file)) => {
                            self.client.resume(&file);
                            self.show_status(&format!("Session imported from: {source}"));
                        }
                        _ => self.show_status(&format!("{imported}. Use /resume to open it.")),
                    },
                    Purpose::Thinking => {
                        let level = item.value.clone();
                        self.set_thinking(&level);
                        self.status = format!("Thinking level: {level}.");
                    }
                    Purpose::Mode(always_skills) => {
                        if let Some(mode) = crate::modes::find(&item.value) {
                            self.set_engineering(mode.engineering, &always_skills);
                        }
                    }
                    Purpose::AutoreviewAccount(accounts) => {
                        let chosen = item.value.parse::<usize>().ok().and_then(|index| accounts.get(index));
                        self.client.autoreview(match chosen {
                            Some((login, host)) => AutoreviewRequest::On {
                                accounts: Some(vec![format!("{host}/{login}")]),
                                reviewing_as: format!("{host}/{login}"),
                            },
                            None => AutoreviewRequest::On {
                                accounts: None,
                                reviewing_as: "every logged-in account".into(),
                            },
                        });
                    }
                    Purpose::ExtensionSelect { id, confirm } => {
                        let answer = if confirm {
                            serde_json::json!({ "confirmed": item.value == "yes" })
                        } else {
                            serde_json::json!({ "value": item.value })
                        };
                        self.answer_extension_dialog(&id, answer);
                    }
                }
            }
            Some(SelectEvent::Cancelled) => match self.selection.take() {
                // Esc on the summary choice returns to the tree with the same selection.
                Some(Selection {
                    purpose: Purpose::Summarize(entry),
                    ..
                }) => {
                    if let Some((tree, leaf)) = self.tree.clone() {
                        self.show_tree(tree, leaf, Some(&entry));
                    }
                }
                Some(Selection {
                    purpose: Purpose::SwitchImported { imported, .. },
                    ..
                }) => self.show_status(&format!("{imported}. Use /resume to open it.")),
                Some(Selection {
                    purpose: Purpose::ExtensionSelect { id, .. },
                    ..
                }) => self.answer_extension_dialog(&id, serde_json::json!({ "cancelled": true })),
                Some(Selection {
                    purpose: Purpose::AutoreviewAccount(_),
                    ..
                }) => self.show_status("Autoreview left as it was"),
                _ => {}
            },
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
            Some(text) => self.copy_to_clipboard(&text, "Copied last agent message to clipboard"),
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
        v.statuses = self.extension_statuses.clone();
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
        // Extension widgets above and below the editor, by key (`#setWidget`).
        let widget_lines = |below: bool| -> Vec<String> {
            self.widgets
                .values()
                .filter(|(_, b)| *b == below)
                .flat_map(|(lines, _)| Text::new(&lines.join("\n"), 1, 0, None).render(width))
                .collect()
        };
        let (widgets_above, widgets_below) = (widget_lines(false), widget_lines(true));
        let mut status = widgets_above;
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
        status.extend(self.wave_summary(width));
        // `#renderRuntimeLine`: one RLM line under the footer while the panel and the graph are hidden.
        let runtime_line = match &snapshot {
            Some(snapshot) if !self.rlm_visible && !rlm_focus => {
                crate::rlm::render_rlm_footer(snapshot, width.max(1), &options)
            }
            _ => None,
        };
        let mut footer = widgets_below;
        footer.extend(render_footer(&self.theme, &self.footer_view(), width));
        footer.extend(runtime_line);
        let focus_height = self.rows.saturating_sub(footer.len() + 1).max(10);
        let now = self.now();
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
            (None, Some(Panel::Trust(selector))) => selector.render(width),
            (None, Some(Panel::Login(selector, ..))) => selector.render(width),
            (None, Some(Panel::OAuthLogin(dialog))) => dialog.render(&self.theme, width),
            (None, Some(Panel::ScopedModels { selector, .. })) => selector.render(width),
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
            (None, Some(Panel::Sessions(selector) | Panel::PiSessions(selector))) => selector.render(width),
            (None, Some(Panel::ExtensionInput { dialog, .. })) => dialog.render(width, now),
            (None, Some(Panel::TextEditor { dialog, .. })) => dialog.render(width, self.rows),
            (None, Some(Panel::ShareLoader(loader))) => loader.render(width),
            (None, Some(Panel::BugReport(flow))) => flow.render(width, self.rows),
            (None, None) => {
                // `#chatInput.focused = !#busy && !(rlm pane focused and shown)`: no cursor while recovering.
                self.editor.focused = !self.recovering && !(self.rlm_pane_focused && self.rlm_pane_shown());
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

    /// One full-screen frame of `height` rows: the chat, with the RLM pane beside it while it shows (`SplitView`).
    pub fn render(&mut self, width: usize, height: usize) -> Vec<String> {
        self.rows = height;
        self.cols = width;
        if let Some(frame) = self.overlay_frame(width, height) {
            self.viewport.hide_layout();
            return frame;
        }
        let frame = self.render_columns(width, height);
        self.viewport.finish_frame(frame, width, height)
    }

    /// The chat column and the RLM pane beside it.
    fn render_columns(&mut self, width: usize, height: usize) -> Vec<String> {
        let (main_width, side_width) =
            tui::split_view::split_widths(width, self.rlm_pane_shown(), rlm_pane_width(width));
        let main = self.render_main(main_width, height);
        if side_width == 0 {
            return main;
        }
        let snapshot = self.rlm_snapshot();
        let style = crate::rlm::RlmStyle::themed(Rc::clone(&self.theme));
        let toggle = self.keys.get_keys("app.rlm.pane").first().cloned();
        let mut side = self.rlm_pane.render(
            &self.keys,
            &snapshot,
            side_width,
            height,
            self.rlm_pane_focused,
            toggle.as_deref(),
            &style,
        );
        side.truncate(height);
        let border = fg(
            &self.theme,
            if self.rlm_pane_focused {
                "borderAccent"
            } else {
                "border"
            },
            "│",
        );
        let mut main = main;
        main.resize(height, String::new());
        tui::split_view::render_split(&main, &side, main_width, side_width, &border)
    }

    /// The chat column: transcript above the dock, `height` rows.
    fn render_main(&mut self, width: usize, height: usize) -> Vec<String> {
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
        self.style_viewport();
        let mut lines = self.viewport.layout(
            document,
            tui::scroll_view::Rect {
                x: 0,
                y: 0,
                width,
                height: sizes[0],
            },
        );
        lines.extend(dock_lines);
        lines
    }

    /// The transcript scrollbar's theme colors and the jump-to-end label (tui-renderer.ts `scrollToEndIndicator`).
    fn style_viewport(&mut self) {
        let (track, thumb) = (Rc::clone(&self.theme), Rc::clone(&self.theme));
        self.viewport.view.track_style = Box::new(move |t| fg(&track, "scrollbarTrack", t));
        self.viewport.view.thumb_style = Box::new(move |t| fg(&thumb, "scrollbarThumb", t));
        let shortcut = key_display_text(&self.keys, "tui.altScreen.bottom");
        let label = if shortcut.is_empty() {
            " ↓ Jump to latest message ".to_string()
        } else {
            format!(" ↓ Jump to latest message · {shortcut} ")
        };
        self.viewport.indicator = Some(
            self.theme
                .bg("selectedBg", &fg(&self.theme, "text", &label))
                .unwrap_or(label),
        );
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
