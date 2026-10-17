//! Rust-only additions to the interactive TUI (rewriteplan "TUI (omp-inspired)"): turn-end notifications, the
//! terminal writer thread with frame pacing and the scrollback/live split, the status line, Vim mode, image paste
//! and attachments, syntect highlighting, and the extra autocomplete sources. Each is a [`QolSettings`] field read
//! from the merged settings; `ULTRON_TUI_PARITY=1` turns all of them off, so the TUI paints exactly what the
//! TypeScript client TUI paints (the tmux parity runs set it).
//!
//! Settings keys (all optional):
//! - `notifications`: bool or `"on"`/`"off"` (default on; `ULTRON_NOTIFICATIONS=off` still silences delivery).
//! - `tuiMode`: the alternate screen (fullscreen) by default, like the TypeScript native client, which always runs
//!   fullscreen; only an explicit `"regular"` uses the scrollback/live split.
//! - `terminal.writerThread`, `terminal.frameGate`: bool (default on).
//! - `statusLine`: bool, or `{enabled, preset, left, right, separator, symbols: "unicode"|"nerd"|"ascii", pr}`
//!   (default on, preset `default`, unicode symbols, no `gh` PR lookups).
//! - `vimMode`: bool (default off).
//! - `attachments`: bool (default on; `images.blockImages: true` turns it off).
//! - `markdown.syntaxHighlight`: bool (default on).
//! - `autocomplete.emoji`, `autocomplete.modelMentions` (default on), `autocomplete.githubRefs` (default off).
//! - `overlays`: bool (default on): the omp-inspired overlays (F5 retry, Alt+L display reset, Alt+A agent hub, the
//!   `/copy` picker, the Esc-Esc rewind selector and the model hub for `/model` and Ctrl+L).

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tui::autocomplete::{AutocompleteProvider, CombinedAutocompleteProvider, SlashCommand};
use tui::autocomplete_sources::{
    ChainProvider, EmojiProvider, GhRefTitles, GithubRefProvider, MentionModel, ModelMentionProvider,
};
use tui::highlight::{HighlightColors, highlight_lines, supports_language};
use tui::notify::{Message, NotificationService, Notifier, ProcessSink};
use tui::status_git::{GhPrLookup, GitSnapshot, GitWatcher, PrLookup};
use tui::status_line::{Preset, SegmentId, SeparatorStyle, StatusLineSettings, Symbols};
use tui::theme::Theme;

use crate::app::App;
use crate::components::fg;
use crate::lane::ModelInfo;
use crate::rlm_cell::Highlighter;

/// The environment variable that turns every Rust-only addition off.
pub const PARITY_ENV: &str = "ULTRON_TUI_PARITY";
/// The model mention trigger (omp's): `@` stays file completion.
pub const MODEL_MENTION_TRIGGER: char = '^';

/// Glyph set of the status line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StatusSymbols {
    #[default]
    Unicode,
    Nerd,
    Ascii,
}

impl StatusSymbols {
    pub fn symbols(self) -> Symbols {
        match self {
            Self::Unicode => Symbols::unicode(),
            Self::Nerd => Symbols::nerd(),
            Self::Ascii => Symbols::ascii(),
        }
    }
}

/// The `tuiMode` value that selects the scrollback/live split; every other value (and none) is fullscreen.
pub const SPLIT_TUI_MODE: &str = "regular";

/// Which Rust-only additions are on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QolSettings {
    /// A terminal notification when a turn ends (not for a turn the user aborted).
    pub notifications: bool,
    /// Frames go through the terminal writer thread (used by [`crate::run::run_stdout`]).
    pub tty_writer: bool,
    /// Frame pacing with output backpressure ([`tui::viewport::RenderGate`]).
    pub render_gate: bool,
    /// Finished transcript goes to the terminal's scrollback once; only the live area and the dock are repainted
    /// (main screen). Off (the default): the alternate-screen layout.
    pub viewport_split: bool,
    /// A status line above the editor.
    pub status_line: bool,
    pub status_layout: StatusLineSettings,
    pub status_symbols: StatusSymbols,
    /// `gh pr view` lookups for the `pr` segment (network, on the git watcher thread).
    pub status_pr: bool,
    /// Vim modal editing (starts in Insert mode).
    pub vim: bool,
    /// Pasted image paths and clipboard images become `[Image #N]` attachments, shown as chips.
    pub attachments: bool,
    /// syntect highlighting of `rlm` cells and fenced code blocks.
    pub highlight: bool,
    /// `:emoji` completion.
    pub autocomplete_emoji: bool,
    /// `^model` completion.
    pub autocomplete_models: bool,
    /// `#123` GitHub reference completion (titles through `gh`).
    pub autocomplete_github: bool,
    /// The omp-inspired overlays and keys (retry, display reset, agent hub, copy picker, rewind, model hub).
    pub overlays: bool,
}

impl Default for QolSettings {
    fn default() -> Self {
        Self {
            notifications: true,
            tty_writer: true,
            render_gate: true,
            viewport_split: false,
            status_line: true,
            status_layout: StatusLineSettings::default(),
            status_symbols: StatusSymbols::Unicode,
            status_pr: false,
            vim: false,
            attachments: true,
            highlight: true,
            autocomplete_emoji: true,
            autocomplete_models: true,
            autocomplete_github: false,
            overlays: true,
        }
    }
}

fn flag(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "on" | "true" | "1" | "yes" => Some(true),
            "off" | "false" | "0" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn segments(v: &Value) -> Option<Vec<SegmentId>> {
    v.as_array().map(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .filter_map(SegmentId::parse)
            .collect()
    })
}

impl QolSettings {
    /// Every addition off: the TypeScript client TUI's behavior.
    pub fn parity() -> Self {
        Self {
            notifications: false,
            tty_writer: false,
            render_gate: false,
            viewport_split: false,
            status_line: false,
            status_layout: StatusLineSettings::default(),
            status_symbols: StatusSymbols::Unicode,
            status_pr: false,
            vim: false,
            attachments: false,
            highlight: false,
            autocomplete_emoji: false,
            autocomplete_models: false,
            autocomplete_github: false,
            overlays: false,
        }
    }

    /// Whether anything is on (a parity build has nothing to start).
    pub fn any(&self) -> bool {
        *self != Self::parity()
    }
}

/// The additions from the merged settings JSON and the environment (`var`).
pub fn from_settings(settings: &Value, var: &dyn Fn(&str) -> Option<String>) -> QolSettings {
    QolSettings::from_settings(settings, var)
}

impl QolSettings {
    /// See [`from_settings`].
    pub fn from_settings(settings: &Value, var: &dyn Fn(&str) -> Option<String>) -> Self {
        if var(PARITY_ENV).is_some_and(|v| v.trim() == "1") {
            return Self::parity();
        }
        let mut q = Self::default();
        let set = |field: &mut bool, v: &Value| {
            if let Some(b) = flag(v) {
                *field = b;
            }
        };
        set(&mut q.notifications, &settings["notifications"]);
        q.viewport_split = settings["tuiMode"].as_str() == Some(SPLIT_TUI_MODE);
        set(&mut q.tty_writer, &settings["terminal"]["writerThread"]);
        set(&mut q.render_gate, &settings["terminal"]["frameGate"]);
        let status = &settings["statusLine"];
        set(&mut q.status_line, status);
        if status.is_object() {
            set(&mut q.status_line, &status["enabled"]);
            set(&mut q.status_pr, &status["pr"]);
            let layout = &mut q.status_layout;
            layout.preset = status["preset"].as_str().and_then(Preset::parse);
            layout.left = segments(&status["left"]);
            layout.right = segments(&status["right"]);
            layout.separator = status["separator"].as_str().map(SeparatorStyle::parse);
            q.status_symbols = match status["symbols"].as_str() {
                Some("nerd") => StatusSymbols::Nerd,
                Some("ascii") => StatusSymbols::Ascii,
                _ => StatusSymbols::Unicode,
            };
        }
        set(&mut q.vim, &settings["vimMode"]);
        set(&mut q.attachments, &settings["attachments"]);
        if settings["images"]["blockImages"] == true {
            q.attachments = false;
        }
        set(&mut q.highlight, &settings["markdown"]["syntaxHighlight"]);
        set(&mut q.autocomplete_emoji, &settings["autocomplete"]["emoji"]);
        set(&mut q.autocomplete_models, &settings["autocomplete"]["modelMentions"]);
        set(&mut q.autocomplete_github, &settings["autocomplete"]["githubRefs"]);
        set(&mut q.overlays, &settings["overlays"]);
        q
    }
}

// ---- highlighting ----

/// A fenced code block highlighter: `(code, language)` to one styled string per line.
pub type CodeHighlight = Rc<dyn Fn(&str, Option<&str>) -> Vec<String>>;

/// The `rlm` cell highlighter: Python through syntect with the theme's syntax colors.
pub fn rlm_highlighter(theme: &Theme) -> Highlighter {
    let colors = HighlightColors::from_theme(theme);
    Rc::new(move |code: &str| highlight_lines(code, Some("python"), &colors))
}

/// Fenced code blocks: known languages through syntect, others in `mdCodeBlock` as before.
pub fn code_highlighter(theme: &Rc<Theme>) -> CodeHighlight {
    let colors = HighlightColors::from_theme(theme);
    let t = Rc::clone(theme);
    Rc::new(
        move |code: &str, lang: Option<&str>| match lang.filter(|l| supports_language(l)) {
            Some(lang) => highlight_lines(code, Some(lang), &colors),
            None => code.split('\n').map(|l| fg(&t, "mdCodeBlock", l)).collect(),
        },
    )
}

// ---- autocomplete ----

/// The editor's completion: slash commands and files, preceded by the enabled extra sources (each answers only
/// for its own syntax, so `/` and `@` still reach the command and file provider).
pub fn autocomplete_provider(
    commands: Vec<SlashCommand>,
    cwd: &str,
    fd_path: Option<PathBuf>,
    models: &[ModelInfo],
    qol: &QolSettings,
) -> Box<dyn AutocompleteProvider> {
    let combined = Box::new(CombinedAutocompleteProvider::new(commands, cwd, fd_path));
    if !(qol.autocomplete_emoji || qol.autocomplete_models || qol.autocomplete_github) {
        return combined;
    }
    let mut chain: Vec<Box<dyn AutocompleteProvider>> = Vec::new();
    if qol.autocomplete_models {
        chain.push(Box::new(ModelMentionProvider {
            models: models
                .iter()
                .map(|m| MentionModel {
                    selector: format!("{}/{}", m.provider, m.id),
                    name: m.name.clone(),
                })
                .collect(),
            trigger: MODEL_MENTION_TRIGGER,
        }));
    }
    if qol.autocomplete_github {
        chain.push(Box::new(GithubRefProvider {
            titles: Some(Box::new(GhRefTitles::gh(PathBuf::from(cwd), || {}))),
        }));
    }
    if qol.autocomplete_emoji {
        chain.push(Box::new(EmojiProvider));
    }
    chain.push(combined);
    Box::new(ChainProvider::new(chain))
}

// ---- background services ----

/// Where the status line's git fields come from (never blocks).
pub trait GitSource {
    fn snapshot(&self) -> GitSnapshot;
    /// Ask for a refresh (after a turn).
    fn refresh(&self) {}
}

impl GitSource for GitWatcher {
    fn snapshot(&self) -> GitSnapshot {
        GitWatcher::snapshot(self)
    }
    fn refresh(&self) {
        GitWatcher::refresh(self);
    }
}

/// Receives the notification for each finished turn.
pub type NotifyFn = Box<dyn FnMut(Message)>;

/// No PR lookups (`statusLine.pr` off).
struct NoPr;

impl PrLookup for NoPr {
    fn lookup(&mut self, _: &std::path::Path, _: &str) -> Option<(u64, String)> {
        None
    }
}

/// Start what the enabled additions need on their own threads: the desktop clipboard (attachments, Vim yanks), the
/// git watcher (status line), the terminal notifier (its bytes reach the terminal through the paint loop), and
/// grammar warm-up (highlighting). Call once at startup, before the paint loop.
pub fn install_services(app: &mut App, path_var: Option<String>) {
    let qol = app.qol().clone();
    // Pi's Ctrl+V and copies use the clipboard in every mode (its tools come from `path_var`).
    app.set_clipboard(tui::clipboard::ClipboardService::spawn(
        tui::clipboard::SystemClipboard::with_env(path_var.clone(), std::env::vars().collect()),
    ));
    // Clicked hyperlinks open with the launcher on `path_var` only (`openBrowser`).
    app.set_url_opener(crate::url_opener::url_opener_with(
        path_var.clone(),
        std::env::var("HOME").ok(),
        crate::url_opener::DESKTOP_VARS
            .iter()
            .filter_map(|name| std::env::var(name).ok().map(|value| (name.to_string(), value)))
            .collect(),
    ));
    // Pi's footer branch: read and watched on its own thread (`git` from `path_var` only).
    app.set_footer_branch(crate::footer_data::FooterBranch::spawn(
        PathBuf::from(app.cwd()),
        path_var.clone(),
        || {},
    ));
    if qol.status_line {
        let cwd = PathBuf::from(app.cwd());
        let watcher = if qol.status_pr {
            GitWatcher::spawn(cwd, GhPrLookup, || {})
        } else {
            GitWatcher::spawn(cwd, NoPr, || {})
        };
        app.set_git_source(Box::new(watcher));
    }
    if qol.notifications {
        let out: Arc<Mutex<Vec<String>>> = app.side_writes();
        let sink = ProcessSink::new(
            move |data: &str| out.lock().unwrap_or_else(|e| e.into_inner()).push(data.to_string()),
            path_var,
        );
        let service = NotificationService::spawn(Notifier::detect(), sink);
        app.set_notify_sink(Box::new(move |m| service.notify(m)));
    }
    if qol.highlight {
        std::thread::spawn(tui::highlight::warm);
    }
}
