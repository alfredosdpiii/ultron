//! Configurable status line: segments, presets, separators and width fitting (port of omp
//! packages/tui/src/status-line/{segments,presets,separators,schema,metrics}.ts, chrome/context-thresholds.ts and
//! pi-utils format.ts).
//!
//! Rendering is pure: [`StatusData`] is a snapshot the owner keeps current (git and PR come from
//! [`crate::status_git::GitWatcher`] off the paint path), [`StatusStyle`] maps semantic roles to colors, and
//! [`render_status_line`] lays both groups out for a width, truncating the session name, dropping right segments,
//! shrinking the path, then dropping left segments, the way omp does.

use crate::utils::{truncate_to_width, visible_width};

/// Segment identifiers (omp `STATUS_LINE_SEGMENT_IDS`, plus Ultron's `state`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SegmentId {
    Pi,
    Status,
    Model,
    Path,
    Git,
    Pr,
    Subagents,
    TokenIn,
    TokenOut,
    TokenTotal,
    TokenRate,
    Cost,
    ContextPct,
    ContextTotal,
    TimeSpent,
    Time,
    Session,
    Hostname,
    CacheRead,
    CacheWrite,
    CacheHit,
    SessionName,
    Vim,
    /// Stream state of the current turn (Ultron): thinking, streaming, running a cell, waiting.
    State,
}

const SEGMENT_NAMES: [(SegmentId, &str); 24] = [
    (SegmentId::Pi, "pi"),
    (SegmentId::Status, "status"),
    (SegmentId::Model, "model"),
    (SegmentId::Path, "path"),
    (SegmentId::Git, "git"),
    (SegmentId::Pr, "pr"),
    (SegmentId::Subagents, "subagents"),
    (SegmentId::TokenIn, "token_in"),
    (SegmentId::TokenOut, "token_out"),
    (SegmentId::TokenTotal, "token_total"),
    (SegmentId::TokenRate, "token_rate"),
    (SegmentId::Cost, "cost"),
    (SegmentId::ContextPct, "context_pct"),
    (SegmentId::ContextTotal, "context_total"),
    (SegmentId::TimeSpent, "time_spent"),
    (SegmentId::Time, "time"),
    (SegmentId::Session, "session"),
    (SegmentId::Hostname, "hostname"),
    (SegmentId::CacheRead, "cache_read"),
    (SegmentId::CacheWrite, "cache_write"),
    (SegmentId::CacheHit, "cache_hit"),
    (SegmentId::SessionName, "session_name"),
    (SegmentId::Vim, "vim"),
    (SegmentId::State, "state"),
];

impl SegmentId {
    pub fn parse(name: &str) -> Option<Self> {
        SEGMENT_NAMES.iter().find(|(_, n)| *n == name).map(|(id, _)| *id)
    }

    pub fn name(self) -> &'static str {
        SEGMENT_NAMES.iter().find(|(id, _)| *id == self).map_or("", |(_, n)| n)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    Default,
    Minimal,
    Compact,
    Full,
    Nerd,
    Ascii,
    Custom,
}

impl Preset {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "default" => Self::Default,
            "minimal" => Self::Minimal,
            "compact" => Self::Compact,
            "full" => Self::Full,
            "nerd" => Self::Nerd,
            "ascii" => Self::Ascii,
            "custom" => Self::Custom,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeparatorStyle {
    Powerline,
    PowerlineThin,
    Slash,
    Pipe,
    Block,
    None,
    Ascii,
}

impl SeparatorStyle {
    /// Unknown names fall back to `powerline-thin`, as in omp.
    pub fn parse(name: &str) -> Self {
        match name {
            "powerline" => Self::Powerline,
            "slash" => Self::Slash,
            "pipe" => Self::Pipe,
            "block" => Self::Block,
            "none" => Self::None,
            "ascii" => Self::Ascii,
            _ => Self::PowerlineThin,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathOptions {
    pub abbreviate: bool,
    pub max_length: usize,
    pub strip_work_prefix: bool,
}

impl Default for PathOptions {
    fn default() -> Self {
        Self {
            abbreviate: true,
            max_length: 40,
            strip_work_prefix: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitOptions {
    pub show_branch: bool,
    pub show_staged: bool,
    pub show_unstaged: bool,
    pub show_untracked: bool,
}

impl Default for GitOptions {
    fn default() -> Self {
        Self {
            show_branch: true,
            show_staged: true,
            show_unstaged: true,
            show_untracked: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TimeOptions {
    pub twelve_hour: bool,
    pub show_seconds: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentOptions {
    pub show_thinking_level: bool,
    pub path: PathOptions,
    pub git: GitOptions,
    pub time: TimeOptions,
}

impl Default for SegmentOptions {
    fn default() -> Self {
        Self {
            show_thinking_level: true,
            path: PathOptions::default(),
            git: GitOptions::default(),
            time: TimeOptions::default(),
        }
    }
}

/// A preset's segments, separator and options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub left: Vec<SegmentId>,
    pub right: Vec<SegmentId>,
    pub separator: SeparatorStyle,
    pub options: SegmentOptions,
}

fn ids(names: &[&str]) -> Vec<SegmentId> {
    names.iter().filter_map(|n| SegmentId::parse(n)).collect()
}

fn git(staged: bool, unstaged: bool, untracked: bool) -> GitOptions {
    GitOptions {
        show_branch: true,
        show_staged: staged,
        show_unstaged: unstaged,
        show_untracked: untracked,
    }
}

fn path(max_length: usize, strip: bool) -> PathOptions {
    PathOptions {
        abbreviate: true,
        max_length,
        strip_work_prefix: strip,
    }
}

/// omp `STATUS_LINE_PRESETS` (segment ids omp has and Ultron does not, `mode`, `collab`, `stream`, `usage`, are
/// left out).
pub fn preset(p: Preset) -> Layout {
    let opts = |show_thinking_level, path, git, time| SegmentOptions {
        show_thinking_level,
        path,
        git,
        time,
    };
    match p {
        Preset::Default => Layout {
            left: ids(&["pi", "vim", "model", "path", "git", "pr", "context_pct", "cost"]),
            right: ids(&["session_name"]),
            separator: SeparatorStyle::PowerlineThin,
            options: opts(true, path(40, true), git(true, true, true), TimeOptions::default()),
        },
        Preset::Minimal => Layout {
            left: ids(&["vim", "path", "git"]),
            right: ids(&["session_name", "context_pct"]),
            separator: SeparatorStyle::Slash,
            options: opts(true, path(30, true), git(false, false, false), TimeOptions::default()),
        },
        Preset::Compact => Layout {
            left: ids(&["vim", "model", "git", "pr"]),
            right: ids(&["session_name", "cost", "context_pct"]),
            separator: SeparatorStyle::PowerlineThin,
            options: opts(
                false,
                PathOptions::default(),
                git(true, true, false),
                TimeOptions::default(),
            ),
        },
        Preset::Full => Layout {
            left: ids(&["pi", "vim", "hostname", "model", "path", "git", "pr", "subagents"]),
            right: ids(&[
                "session_name",
                "cache_hit",
                "token_in",
                "token_out",
                "token_rate",
                "cache_read",
                "cost",
                "context_pct",
                "time_spent",
                "time",
            ]),
            separator: SeparatorStyle::Powerline,
            options: opts(true, path(50, true), git(true, true, true), TimeOptions::default()),
        },
        Preset::Nerd => Layout {
            left: ids(&[
                "pi",
                "vim",
                "hostname",
                "model",
                "path",
                "git",
                "pr",
                "session",
                "subagents",
            ]),
            right: ids(&[
                "session_name",
                "token_in",
                "token_out",
                "cache_read",
                "cache_write",
                "token_rate",
                "cost",
                "context_pct",
                "context_total",
                "time_spent",
                "time",
            ]),
            separator: SeparatorStyle::Powerline,
            options: opts(
                true,
                path(60, true),
                git(true, true, true),
                TimeOptions {
                    twelve_hour: false,
                    show_seconds: true,
                },
            ),
        },
        Preset::Ascii => Layout {
            left: ids(&["vim", "model", "path", "git", "pr"]),
            right: ids(&["session_name", "token_total", "cost", "context_pct"]),
            separator: SeparatorStyle::Ascii,
            options: opts(true, path(40, true), git(true, true, true), TimeOptions::default()),
        },
        Preset::Custom => Layout {
            left: ids(&["vim", "model", "path", "git", "pr"]),
            right: ids(&["session_name", "token_total", "cost", "context_pct"]),
            separator: SeparatorStyle::PowerlineThin,
            options: SegmentOptions::default(),
        },
    }
}

/// User settings over a preset.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusLineSettings {
    pub preset: Option<Preset>,
    pub left: Option<Vec<SegmentId>>,
    pub right: Option<Vec<SegmentId>>,
    pub separator: Option<SeparatorStyle>,
    pub options: Option<SegmentOptions>,
}

impl StatusLineSettings {
    /// The preset with each explicitly set field replacing it.
    pub fn effective(&self) -> Layout {
        let mut layout = preset(self.preset.unwrap_or(Preset::Default));
        if let Some(l) = &self.left {
            layout.left = l.clone();
        }
        if let Some(r) = &self.right {
            layout.right = r.clone();
        }
        if let Some(s) = self.separator {
            layout.separator = s;
        }
        if let Some(o) = &self.options {
            layout.options = o.clone();
        }
        layout
    }
}

/// Glyph set (omp symbol presets: unicode, nerd, ascii).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbols {
    pub model: &'static str,
    pub folder: &'static str,
    pub git: &'static str,
    pub branch: &'static str,
    pub pr: &'static str,
    pub tokens: &'static str,
    pub context: &'static str,
    pub time: &'static str,
    pub agents: &'static str,
    pub cache: &'static str,
    pub input: &'static str,
    pub output: &'static str,
    pub throughput: &'static str,
    pub host: &'static str,
    pub session: &'static str,
    pub auto: &'static str,
    pub brand: &'static str,
    pub disabled: &'static str,
    pub vim: [&'static str; 5],
    /// minimal, low, medium, high, xhigh, max
    pub thinking: [&'static str; 6],
    pub auto_pending: &'static str,
    pub dot: &'static str,
    pub powerline: (&'static str, &'static str),
    pub powerline_thin: (&'static str, &'static str),
    pub slash: &'static str,
    pub pipe: &'static str,
    pub block: &'static str,
    pub ascii: (&'static str, &'static str),
    pub spinner: &'static [&'static str],
}

const BRAILLE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

impl Symbols {
    pub fn unicode() -> Self {
        Self {
            model: "⬢",
            folder: "📁",
            git: "⎇",
            branch: "⑂",
            pr: "⤴",
            tokens: "🪙",
            context: "◫",
            time: "⏱",
            agents: "👥",
            cache: "💾",
            input: "⤵",
            output: "⤴",
            throughput: "⚡",
            host: "🖥",
            session: "🆔",
            auto: "⟲",
            brand: "π",
            disabled: "⦸",
            vim: ["▎", "■", "◉", "≡", "▁"],
            thinking: ["○ min", "◔ low", "◑ med", "◒ high", "◕ xhigh", "◉ max"],
            auto_pending: "⟳",
            dot: " · ",
            powerline: ("▶", "◀"),
            powerline_thin: (">", "<"),
            slash: " / ",
            pipe: " │ ",
            block: "▌",
            ascii: (">", "<"),
            spinner: BRAILLE,
        }
    }

    pub fn nerd() -> Self {
        Self {
            model: "\u{ec19}",
            folder: "\u{f115}",
            git: "\u{f1d3}",
            branch: "\u{f126}",
            pr: "\u{ea64}",
            tokens: "\u{e26b}",
            context: "\u{e70f}",
            time: "\u{f017}",
            agents: "\u{f0c0}",
            cache: "\u{f1c0}",
            input: "\u{f090}",
            output: "\u{f08b}",
            throughput: "\u{f0e4}",
            host: "\u{f109}",
            session: "\u{f0051}",
            auto: "\u{f0068}",
            brand: "π",
            disabled: "\u{f05e}",
            vim: ["\u{f040}", "\u{f0c8}", "\u{f06e}", "\u{f0c9}", "\u{f0ec}"],
            thinking: [
                "\u{F0A9E} min",
                "\u{F0A9F} low",
                "\u{F0AA1} med",
                "\u{F0AA3} high",
                "\u{F0AA5} xhi",
                "\u{F06D} max",
            ],
            auto_pending: "\u{F074}",
            dot: " · ",
            powerline: ("\u{e0b0}", "\u{e0b2}"),
            powerline_thin: ("\u{e0b1}", "\u{e0b3}"),
            slash: "\u{e0bb}",
            pipe: "\u{e0b3}",
            block: "█",
            ascii: (">", "<"),
            spinner: BRAILLE,
        }
    }

    pub fn ascii() -> Self {
        Self {
            model: "[M]",
            folder: "[D]",
            git: "git:",
            branch: "@",
            pr: "PR",
            tokens: "tok:",
            context: "ctx:",
            time: "t:",
            agents: "AG",
            cache: "cache",
            input: "in:",
            output: "out:",
            throughput: "tok/s:",
            host: "host",
            session: "id",
            auto: "[A]",
            brand: "",
            disabled: "[ ]",
            vim: ["I", "N", "V", "L", "R"],
            thinking: ["[min]", "[low]", "[med]", "[high]", "[xhi]", "[max]"],
            auto_pending: "[~]",
            dot: " - ",
            powerline: (">", "<"),
            powerline_thin: (">", "<"),
            slash: " / ",
            pipe: " | ",
            block: "#",
            ascii: (">", "<"),
            spinner: &["-", "\\", "|", "/"],
        }
    }

    /// (left-group separator, right-group separator) for a style.
    pub fn separator(&self, style: SeparatorStyle) -> (&'static str, &'static str) {
        match style {
            SeparatorStyle::Powerline => self.powerline,
            SeparatorStyle::PowerlineThin => self.powerline_thin,
            SeparatorStyle::Slash => (self.slash.trim(), self.slash.trim()),
            SeparatorStyle::Pipe => (self.pipe.trim(), self.pipe.trim()),
            SeparatorStyle::Block => (self.block, self.block),
            SeparatorStyle::None => (" ", " "),
            SeparatorStyle::Ascii => self.ascii,
        }
    }
}

/// Semantic color of a span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Accent,
    Muted,
    Dim,
    Success,
    Warning,
    Error,
    /// Context usage between the warning and error levels (omp `thinkingHigh`).
    Purple,
    Model,
    Path,
    GitClean,
    GitDirty,
    Dirty,
    Staged,
    Untracked,
    Context,
    Spend,
    Output,
    Cost,
    Subagents,
    Separator,
}

/// Maps roles to terminal styling.
pub trait StatusStyle {
    fn fg(&self, role: Role, text: &str) -> String;
}

/// No styling: segment text only.
pub struct Plain;

impl StatusStyle for Plain {
    fn fg(&self, _role: Role, text: &str) -> String {
        text.to_string()
    }
}

/// Styling from a [`crate::theme::Theme`]: roles map to existing theme colors.
pub struct ThemeStyle<'a>(pub &'a crate::theme::Theme);

impl StatusStyle for ThemeStyle<'_> {
    fn fg(&self, role: Role, text: &str) -> String {
        let color = match role {
            Role::Accent | Role::Model | Role::Context => "accent",
            Role::Muted | Role::Path | Role::Spend | Role::Cost | Role::Subagents => "muted",
            Role::Dim | Role::Separator => "dim",
            Role::Success | Role::GitClean | Role::Staged => "success",
            Role::Warning | Role::GitDirty | Role::Dirty => "warning",
            Role::Error => "error",
            Role::Purple => "thinkingHigh",
            Role::Untracked | Role::Output => "text",
        };
        self.0.fg(color, text).unwrap_or_else(|_| text.to_string())
    }
}

/// Vim modal state shown by the `vim` segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimMode {
    Insert,
    Normal,
    Visual,
    VisualLine,
    Replace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VimDisplay {
    Text,
    Icon,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VimStatus {
    pub mode: VimMode,
    pub pending: String,
    pub selected_lines: usize,
    pub display: VimDisplay,
}

/// What the current turn is doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StreamState {
    #[default]
    Idle,
    Thinking,
    Streaming,
    /// A REPL cell is running.
    Running,
    /// Waiting on the provider (retry or rate limit).
    Waiting,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    /// Portion of `cost` carried by completed subagent results.
    pub subagent_cost: f64,
    pub tokens_per_second: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GitCounts {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
}

/// Snapshot the segments render from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatusData {
    pub model_name: Option<String>,
    /// `None` when the model has no thinking; else the level word (`off`, `low`, ..., `max`, or `auto`).
    pub thinking_level: Option<String>,
    pub cwd: String,
    pub home: String,
    pub git_branch: Option<String>,
    pub git_status: Option<GitCounts>,
    pub pr: Option<(u64, String)>,
    pub hyperlinks: bool,
    pub context_percent: Option<f64>,
    pub context_tokens: u64,
    pub context_window: u64,
    pub auto_compact: bool,
    pub usage: Usage,
    pub using_subscription: bool,
    pub active_ms: u64,
    pub turn_elapsed_ms: Option<u64>,
    /// Wall clock as (hours, minutes, seconds), local time.
    pub clock: (u32, u32, u32),
    /// Milliseconds on the shared animation clock (spinner frame).
    pub now_ms: u64,
    pub hostname: String,
    pub session_id: Option<String>,
    pub session_name: Option<String>,
    pub subagent_count: usize,
    pub stream: StreamState,
    pub vim: Option<VimStatus>,
    /// Extension status texts, key-sorted.
    pub hook_statuses: Vec<String>,
}

/// omp `formatNumber`: `999`, `1K`, `1.5K`, `25K`, `1M`, `1.5M`, `25M`, `1.5B`.
pub fn format_number(n: u64) -> String {
    fn trim1(x: f64) -> String {
        let s = format!("{x:.1}");
        s.strip_suffix(".0").map_or(s.clone(), str::to_string)
    }
    let f = n as f64;
    match n {
        0..1_000 => n.to_string(),
        1_000..10_000 => format!("{}K", trim1(f / 1e3)),
        10_000..1_000_000 => format!("{}K", (f / 1e3).round()),
        1_000_000..10_000_000 => format!("{}M", trim1(f / 1e6)),
        10_000_000..1_000_000_000 => format!("{}M", (f / 1e6).round()),
        1_000_000_000..10_000_000_000 => format!("{}B", trim1(f / 1e9)),
        _ => format!("{}B", (f / 1e9).round()),
    }
}

/// omp `formatDuration`: `123ms`, `1.5s`, `30m15s`, `2h30m`, `3d2h`.
pub fn format_duration(ms: u64) -> String {
    const SEC: u64 = 1000;
    const MIN: u64 = 60 * SEC;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    let pair = |a: u64, ua: &str, b: u64, ub: &str| {
        if b > 0 {
            format!("{a}{ua}{b}{ub}")
        } else {
            format!("{a}{ua}")
        }
    };
    match ms {
        0 => "0ms".into(),
        1..SEC => format!("{ms}ms"),
        SEC..MIN => {
            let tenths = (ms as f64 / 100.0).round() as u64;
            if tenths < 600 {
                format!("{:.1}s", tenths as f64 / 10.0)
            } else {
                "1m".into()
            }
        }
        MIN..HOUR => pair(ms / MIN, "m", (ms % MIN) / SEC, "s"),
        HOUR..DAY => pair(ms / HOUR, "h", (ms % HOUR) / MIN, "m"),
        _ => pair(ms / DAY, "d", (ms % DAY) / HOUR, "h"),
    }
}

/// Context usage level from percent and window (omp context-thresholds.ts).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextLevel {
    Normal,
    Warning,
    Purple,
    Error,
}

pub fn context_level(percent: f64, window: u64) -> ContextLevel {
    let reaches = |pct_threshold: f64, token_threshold: f64| {
        if !percent.is_finite() || percent <= 0.0 {
            return false;
        }
        if window == 0 {
            return percent >= pct_threshold;
        }
        percent >= pct_threshold.min(token_threshold / window as f64 * 100.0)
    };
    if reaches(90.0, 500_000.0) {
        ContextLevel::Error
    } else if reaches(70.0, 270_000.0) {
        ContextLevel::Purple
    } else if reaches(50.0, 150_000.0) {
        ContextLevel::Warning
    } else {
        ContextLevel::Normal
    }
}

/// `<percent>%/<window>`, or `<tokens>/?` for an unknown window.
pub fn format_context_usage(percent: Option<f64>, window: u64, used_tokens: u64) -> String {
    if window == 0 {
        return format!("{}/?", format_number(used_tokens));
    }
    let pct = percent.map_or("?".to_string(), |p| format!("{p:.1}%"));
    format!("{pct}/{}", format_number(window))
}

/// Billing summary for the `cost` segment (omp `formatBillingSummary`, two decimals, no advisor).
pub fn format_billing(cost: f64, subagent_cost: f64, using_subscription: bool) -> Option<String> {
    if cost == 0.0 && subagent_cost == 0.0 && !using_subscription {
        return None;
    }
    let mut parts = Vec::new();
    if cost != 0.0 || subagent_cost != 0.0 {
        parts.push(if using_subscription {
            format!("S{cost:.2}")
        } else {
            format!("${cost:.2}")
        });
    } else {
        parts.push("(sub)".to_string());
    }
    if subagent_cost != 0.0 {
        parts.push(format!("(+{subagent_cost:.2})"));
    }
    Some(parts.join(" "))
}

/// Left-truncate to `max` chars with a leading ellipsis.
fn clamp_path(p: &str, max: usize) -> String {
    let n = p.chars().count();
    if n <= max {
        return p.to_string();
    }
    let keep = max.saturating_sub(1);
    format!("…{}", p.chars().skip(n - keep).collect::<String>())
}

fn relative_within(root: &str, path: &str) -> Option<String> {
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return None;
    }
    if path == root {
        return Some(String::new());
    }
    path.strip_prefix(root)?.strip_prefix('/').map(str::to_string)
}

/// The path segment's text: scratch roots and `~/Projects`, `~/repos`, `/work` stripped when `strip`, `~` for
/// home when `abbreviate`, left-clamped to `max`.
pub fn display_path(cwd: &str, home: &str, opts: &PathOptions) -> String {
    let mut p = cwd.to_string();
    if opts.strip_work_prefix {
        let scratch = ["/tmp".to_string(), "/var/tmp".to_string(), format!("{home}/tmp")];
        let work = [format!("{home}/Projects"), format!("{home}/repos"), "/work".to_string()];
        if let Some(rel) = scratch.iter().find_map(|r| relative_within(r, cwd)) {
            if !rel.is_empty() {
                p = rel;
            }
        } else if let Some(rel) = work
            .iter()
            .find_map(|r| relative_within(r, cwd).filter(|r| !r.is_empty()))
        {
            p = rel;
        }
    }
    if opts.abbreviate
        && !home.is_empty()
        && let Some(rel) = relative_within(home, &p)
    {
        p = if rel.is_empty() { "~".into() } else { format!("~/{rel}") };
    }
    clamp_path(&p, opts.max_length)
}

/// Single-line status text: control characters become spaces, runs of spaces collapse, ends trimmed.
pub fn sanitize_status_text(text: &str) -> String {
    let flat: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    flat.split(' ').filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ")
}

fn with_icon(icon: &str, text: &str) -> String {
    if icon.is_empty() {
        text.to_string()
    } else {
        format!("{icon} {text}")
    }
}

fn thinking_display(level: &str, sym: &Symbols) -> String {
    let i = match level {
        "minimal" => 0,
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        "xhigh" => 4,
        "max" => 5,
        "off" => return format!("{} off", sym.disabled),
        "auto" => return format!("{} auto", sym.auto_pending),
        other => return other.to_string(),
    };
    sym.thinking[i].to_string()
}

/// Render one segment; `None` when hidden.
pub fn render_segment(
    id: SegmentId,
    d: &StatusData,
    opts: &SegmentOptions,
    sym: &Symbols,
    style: &dyn StatusStyle,
) -> Option<String> {
    let u = &d.usage;
    let stat = |value: u64, icon: &str, role: Role| {
        (value > 0).then(|| style.fg(role, &with_icon(icon, &format_number(value))))
    };
    match id {
        SegmentId::Pi => {
            let content = match d.turn_elapsed_ms {
                Some(ms) => {
                    let frame = sym.spinner[(d.now_ms / 80) as usize % sym.spinner.len()];
                    let s = ms / 1000;
                    let timer = if s < 60 {
                        format!("{s}s")
                    } else if s < 3600 {
                        format!("{}m", s / 60)
                    } else {
                        format!("{}h", (s / 3600).min(99))
                    };
                    format!("{frame} {timer}")
                }
                None => sym.brand.to_string(),
            };
            (!content.is_empty()).then(|| style.fg(Role::Dim, &content))
        }
        SegmentId::Status => {
            let text: Vec<String> = d
                .hook_statuses
                .iter()
                .map(|s| sanitize_status_text(s))
                .filter(|s| !s.is_empty())
                .collect();
            (!text.is_empty()).then(|| style.fg(Role::Accent, &text.join(sym.dot)))
        }
        SegmentId::Model => {
            let name = d.model_name.as_deref().filter(|n| !n.is_empty()).unwrap_or("no-model");
            let name = name.strip_prefix("Claude ").unwrap_or(name);
            let mut content = style.fg(Role::Model, &with_icon(sym.model, name));
            if opts.show_thinking_level
                && let Some(level) = &d.thinking_level
            {
                content += &style.fg(Role::Model, &format!("{}{}", sym.dot, thinking_display(level, sym)));
            }
            Some(content)
        }
        SegmentId::Path => Some(style.fg(
            Role::Path,
            &with_icon(sym.folder, &display_path(&d.cwd, &d.home, &opts.path)),
        )),
        SegmentId::Git => {
            let g = &opts.git;
            if d.git_branch.is_none() && d.git_status.is_none() {
                return None;
            }
            let mut content = match (&d.git_branch, g.show_branch) {
                (Some(b), true) => with_icon(sym.branch, b),
                _ => String::new(),
            };
            let mut dirty = false;
            if let Some(s) = d.git_status {
                dirty = s.staged > 0 || s.unstaged > 0 || s.untracked > 0;
                let mut ind = Vec::new();
                if g.show_unstaged && s.unstaged > 0 {
                    ind.push(style.fg(Role::Dirty, &format!("*{}", s.unstaged)));
                }
                if g.show_staged && s.staged > 0 {
                    ind.push(style.fg(Role::Staged, &format!("+{}", s.staged)));
                }
                if g.show_untracked && s.untracked > 0 {
                    ind.push(style.fg(Role::Untracked, &format!("?{}", s.untracked)));
                }
                if !ind.is_empty() {
                    let text = ind.join(" ");
                    if content.is_empty() && !g.show_branch {
                        content = with_icon(sym.git, &text);
                    } else if content.is_empty() {
                        content = text;
                    } else {
                        content = format!("{content} {text}");
                    }
                }
            }
            (!content.is_empty()).then(|| style.fg(if dirty { Role::GitDirty } else { Role::GitClean }, &content))
        }
        SegmentId::Pr => d.pr.as_ref().map(|(number, url)| {
            let label = with_icon(sym.pr, &format!("#{number}"));
            let content = if d.hyperlinks {
                format!("\x1b]8;;{url}\x07{label}\x1b]8;;\x07")
            } else {
                label
            };
            style.fg(Role::Accent, &content)
        }),
        SegmentId::Subagents => (d.subagent_count > 0)
            .then(|| style.fg(Role::Subagents, &with_icon(sym.agents, &d.subagent_count.to_string()))),
        SegmentId::TokenIn => stat(u.input, sym.input, Role::Spend),
        SegmentId::TokenOut => stat(u.output, sym.output, Role::Output),
        SegmentId::CacheRead => stat(u.cache_read, sym.cache, Role::Spend),
        SegmentId::CacheWrite => stat(u.cache_write, sym.cache, Role::Output),
        SegmentId::TokenTotal => stat(u.input + u.output + u.cache_write, sym.tokens, Role::Spend),
        SegmentId::TokenRate => u
            .tokens_per_second
            .filter(|r| *r > 0.0)
            .map(|r| style.fg(Role::Output, &with_icon(sym.throughput, &format!("{r:.1} tok/s")))),
        SegmentId::Cost => {
            let own = (u.cost - u.subagent_cost).max(0.0);
            format_billing(own, u.subagent_cost, d.using_subscription).map(|b| style.fg(Role::Cost, &b))
        }
        SegmentId::ContextPct => {
            let role = match context_level(d.context_percent.unwrap_or(0.0), d.context_window) {
                ContextLevel::Normal => Role::Context,
                ContextLevel::Warning => Role::Warning,
                ContextLevel::Purple => Role::Purple,
                ContextLevel::Error => Role::Error,
            };
            let text = if d.context_percent.is_none() && d.context_window > 0 {
                format_number(d.context_window)
            } else {
                format_context_usage(d.context_percent, d.context_window, d.context_tokens)
            };
            let mut text = style.fg(role, &text);
            if d.auto_compact && !sym.auto.is_empty() {
                text = format!("{text} {}", style.fg(role, sym.auto));
            }
            Some(with_icon(sym.context, &text))
        }
        SegmentId::ContextTotal => (d.context_window > 0)
            .then(|| style.fg(Role::Context, &with_icon(sym.context, &format_number(d.context_window)))),
        SegmentId::TimeSpent => (d.active_ms >= 1000).then(|| with_icon(sym.time, &format_duration(d.active_ms))),
        SegmentId::Time => {
            let (mut h, m, s) = d.clock;
            let mut suffix = "";
            if opts.time.twelve_hour {
                suffix = if h >= 12 { "pm" } else { "am" };
                h = if h % 12 == 0 { 12 } else { h % 12 };
            }
            let mut t = format!("{h}:{m:02}");
            if opts.time.show_seconds {
                t += &format!(":{s:02}");
            }
            Some(with_icon(sym.time, &(t + suffix)))
        }
        SegmentId::Session => {
            let id: String = d.session_id.as_deref().unwrap_or("").chars().take(8).collect();
            Some(with_icon(sym.session, if id.is_empty() { "new" } else { &id }))
        }
        SegmentId::Hostname => Some(with_icon(sym.host, d.hostname.split('.').next().unwrap_or(""))),
        SegmentId::CacheHit => (u.cache_read > 0).then(|| {
            let rate = u.cache_read as f64 / (u.cache_read + u.cache_write + u.input) as f64 * 100.0;
            format!("{} {}", sym.cache, style.fg(Role::Spend, &format!("{rate:.2}%")))
        }),
        SegmentId::SessionName => d
            .session_name
            .as_deref()
            .map(sanitize_status_text)
            .filter(|s| !s.is_empty())
            .map(|s| style.fg(Role::Accent, &s)),
        SegmentId::Vim => {
            let v = d.vim.as_ref().filter(|v| v.display != VimDisplay::None)?;
            let (idx, label, role) = match v.mode {
                VimMode::Insert => (0, "INSERT", Role::Success),
                VimMode::Normal => (1, "NORMAL", Role::Accent),
                VimMode::Visual => (2, "VISUAL", Role::Warning),
                VimMode::VisualLine => (3, "V-LINE", Role::Warning),
                VimMode::Replace => (4, "REPLACE", Role::Accent),
            };
            let mut label = if v.display == VimDisplay::Icon {
                sym.vim[idx].to_string()
            } else {
                label.to_string()
            };
            if v.selected_lines > 1 {
                label += &format!(" {}L", v.selected_lines);
            }
            let pending = if v.pending.is_empty() {
                String::new()
            } else {
                style.fg(Role::Muted, &format!(" {}", v.pending))
            };
            Some(format!("{}{pending}", style.fg(role, &label)))
        }
        SegmentId::State => {
            let (text, role) = match d.stream {
                StreamState::Idle => return None,
                StreamState::Thinking => ("thinking", Role::Purple),
                StreamState::Streaming => ("streaming", Role::Accent),
                StreamState::Running => ("running", Role::Success),
                StreamState::Waiting => ("waiting", Role::Warning),
            };
            let frame = sym.spinner[(d.now_ms / 80) as usize % sym.spinner.len()];
            Some(style.fg(role, &format!("{frame} {text}")))
        }
    }
}

/// Lay out the status line in `width` columns (omp `#renderStatusLine`, transparent/plain layout: no fill, no caps).
pub fn render_status_line(
    layout: &Layout,
    d: &StatusData,
    sym: &Symbols,
    style: &dyn StatusStyle,
    width: usize,
) -> String {
    let render_side = |ids: &[SegmentId]| -> Vec<(SegmentId, String)> {
        ids.iter()
            .filter_map(|id| render_segment(*id, d, &layout.options, sym, style).map(|s| (*id, s)))
            .collect()
    };
    let mut left = render_side(&layout.left);
    let mut right = render_side(&layout.right);
    let (lsep, rsep) = sym.separator(layout.separator);
    let (lsep_w, rsep_w) = (visible_width(lsep), visible_width(rsep));
    let group_width = |parts: &[(SegmentId, String)], sep_w: usize| -> usize {
        if parts.is_empty() {
            return 0;
        }
        parts.iter().map(|(_, s)| visible_width(s)).sum::<usize>() + (parts.len() - 1) * (sep_w + 2) + 2
    };
    let total = |l: &[(SegmentId, String)], r: &[(SegmentId, String)]| {
        group_width(l, lsep_w) + group_width(r, rsep_w) + usize::from(!l.is_empty() && !r.is_empty())
    };

    if width > 0 {
        if let Some(i) = right.iter().position(|(id, _)| *id == SegmentId::SessionName)
            && total(&left, &right) > width
        {
            let cur = visible_width(&right[i].1);
            let shrink = cur.saturating_sub(8).min(total(&left, &right) - width);
            if shrink > 0 {
                right[i].1 = truncate_to_width(&right[i].1, cur - shrink, "…", false);
            }
        }
        while total(&left, &right) > width && !right.is_empty() {
            right.pop();
        }
        if let Some(i) = left.iter().position(|(id, _)| *id == SegmentId::Path)
            && total(&left, &right) > width
        {
            let over = total(&left, &right) - width;
            let cur = visible_width(&left[i].1);
            let mut max = layout.options.path.max_length.min(cur).saturating_sub(over).max(4);
            let mut opts = layout.options.clone();
            loop {
                opts.path.max_length = max;
                let s = render_segment(SegmentId::Path, d, &opts, sym, style).unwrap_or_default();
                let saved = cur.saturating_sub(visible_width(&s));
                left[i].1 = s;
                if saved >= over || max == 4 {
                    break;
                }
                max = max.saturating_sub(over - saved).max(4);
            }
        }
        while total(&left, &right) > width && !left.is_empty() {
            let idx = left
                .iter()
                .rposition(|(id, _)| *id != SegmentId::Path)
                .unwrap_or(left.len() - 1);
            left.remove(idx);
        }
    }

    let group = |parts: &[(SegmentId, String)], sep: &str| -> String {
        if parts.is_empty() {
            return String::new();
        }
        let sep = format!(" {} ", style.fg(Role::Separator, sep));
        format!(
            " {} ",
            parts.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join(&sep)
        )
    };
    let (lg, rg) = (group(&left, lsep), group(&right, rsep));
    if lg.is_empty() && rg.is_empty() {
        return String::new();
    }
    if width == 0 || lg.is_empty() || rg.is_empty() {
        let line = format!("{lg}{}{rg}", if !lg.is_empty() && !rg.is_empty() { " " } else { "" });
        return if width > 0 && rg.is_empty() {
            line
        } else if width > 0 {
            format!("{}{rg}", " ".repeat(width.saturating_sub(visible_width(&rg))))
        } else {
            line
        };
    }
    let gap = width.saturating_sub(visible_width(&lg) + visible_width(&rg)).max(1);
    format!("{lg}{}{rg}", " ".repeat(gap))
}
