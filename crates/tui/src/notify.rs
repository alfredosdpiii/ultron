//! Turn-complete desktop notifications (port of omp packages/tui/src/desktop-notify.ts, terminal-capabilities.ts
//! notification parts, terminal-multiplexer.ts and multiplexers/*.ts).
//!
//! Delivery order for one notification: a multiplexer notifier (innermost tier first: a Herdr pane, a cmux surface,
//! then the tmux/Zellij in-band rewrites), else the terminal's own sequence (OSC 99, OSC 9, OSC 777 or BEL). A BEL
//! terminal on a Linux desktop session also fans out to `notify-send`, else `gdbus`.
//!
//! [`NotificationService`] runs all of this on its own thread: `notify` only queues, so neither PATH probing nor
//! process spawning ever happens on the paint path. Terminal bytes go through the caller's writer (the terminal
//! writer thread), never straight to stdout, so a notification cannot split a frame's escape sequence.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

/// Default notification title and application name.
pub const APP_NAME: &str = "ultron";
/// `off`, `0` or `false` suppresses every notification.
pub const NOTIFICATIONS_ENV: &str = "ULTRON_NOTIFICATIONS";
/// `1` opts out of the D-Bus fallback only.
pub const NO_DESKTOP_NOTIFY_ENV: &str = "ULTRON_NO_DESKTOP_NOTIFY";

/// Environment snapshot; every check reads from it instead of the process environment so tests stay hermetic.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Env(BTreeMap<String, String>);

impl Env {
    pub fn from_process() -> Self {
        Self(std::env::vars().collect())
    }

    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Self(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    /// JavaScript truthiness of `env[key]`: set and non-empty.
    pub fn truthy(&self, key: &str) -> bool {
        self.get(key).is_some_and(|v| !v.is_empty())
    }

    pub fn set(&mut self, key: &str, value: &str) {
        self.0.insert(key.to_string(), value.to_string());
    }

    pub fn remove(&mut self, key: &str) {
        self.0.remove(key);
    }
}

/// In-band notification protocol of a terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyProtocol {
    Bell,
    /// `ESC ] 9 ; message ST` (iTerm2, Ghostty, WezTerm, Warp).
    Osc9,
    /// `ESC ] 99 ; metadata ; payload ST` (Kitty desktop notifications).
    Osc99,
    /// `ESC ] 777 ; notify ; title ; body ST` (foot, rxvt-unicode).
    Osc777,
}

/// Terminal identity from environment markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalId {
    Kitty,
    Ghostty,
    Wezterm,
    Iterm2,
    Vscode,
    Alacritty,
    Warp,
    Orca,
    Otty,
    Rio,
    Tern,
    Monstar,
    Foot,
    Base,
    TrueColor,
}

impl TerminalId {
    pub fn notify_protocol(self) -> NotifyProtocol {
        match self {
            Self::Kitty | Self::Otty | Self::Tern => NotifyProtocol::Osc99,
            Self::Ghostty | Self::Wezterm | Self::Iterm2 | Self::Warp | Self::Monstar => NotifyProtocol::Osc9,
            Self::Foot => NotifyProtocol::Osc777,
            Self::Vscode | Self::Alacritty | Self::Orca | Self::Rio | Self::Base | Self::TrueColor => {
                NotifyProtocol::Bell
            }
        }
    }

    fn from_program(program: &str) -> Option<Self> {
        Some(match program.to_ascii_lowercase().as_str() {
            "kitty" => Self::Kitty,
            "ghostty" => Self::Ghostty,
            "wezterm" => Self::Wezterm,
            "iterm.app" | "iterm2" => Self::Iterm2,
            "vscode" => Self::Vscode,
            "alacritty" => Self::Alacritty,
            "warpterminal" => Self::Warp,
            "orca" => Self::Orca,
            "otty" => Self::Otty,
            "rio" => Self::Rio,
            "tern" => Self::Tern,
            "monstar" => Self::Monstar,
            "foot" => Self::Foot,
            _ => return None,
        })
    }
}

/// Resolve terminal identity (`detectTerminalId`). `tmux_client` is the attached client's terminal name from
/// [`query_tmux_client_terminal_name`], consulted after `TERM_PROGRAM` because tmux overwrites pane identity.
pub fn detect_terminal_id(env: &Env, tmux_client: Option<&str>) -> TerminalId {
    for (key, id) in [
        ("KITTY_WINDOW_ID", TerminalId::Kitty),
        ("GHOSTTY_RESOURCES_DIR", TerminalId::Ghostty),
        ("WEZTERM_PANE", TerminalId::Wezterm),
        ("ITERM_SESSION_ID", TerminalId::Iterm2),
        ("VSCODE_PID", TerminalId::Vscode),
        ("ALACRITTY_WINDOW_ID", TerminalId::Alacritty),
    ] {
        if env.truthy(key) {
            return id;
        }
    }
    if let Some(id) = env.get("TERM_PROGRAM").and_then(TerminalId::from_program) {
        return id;
    }
    if let Some(id) = tmux_client.and_then(TerminalId::from_program) {
        return id;
    }
    let term = env.get("TERM").unwrap_or("").to_ascii_lowercase();
    if term.contains("ghostty") {
        return TerminalId::Ghostty;
    }
    if term == "monstar" {
        return TerminalId::Monstar;
    }
    if term == "foot" || term.starts_with("foot-") {
        return TerminalId::Foot;
    }
    match env.get("COLORTERM").map(str::to_ascii_lowercase).as_deref() {
        Some("truecolor" | "24bit") => TerminalId::TrueColor,
        _ => TerminalId::Base,
    }
}

/// First word of tmux's `#{client_termtype}` reply (`xterm-kitty(0.35.2)` style replies included).
pub fn parse_client_termtype(reply: &str) -> Option<String> {
    let reply = reply.trim();
    let first = reply.chars().next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let end = reply
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-')))
        .map_or(reply.len(), |(i, _)| i);
    let rest = &reply[end..];
    (rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('('))
        .then(|| reply[..end].to_string())
}

/// Ask the tmux server for the attached client's terminal type (bounded at 500 ms). Call once at startup.
pub fn query_tmux_client_terminal_name(env: &Env) -> Option<String> {
    if !Multiplexer::Tmux.is_inside(env) {
        return None;
    }
    let mut child = Command::new("tmux")
        .args(["display-message", "-p", "#{client_termtype}"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => thread::sleep(Duration::from_millis(5)),
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    parse_client_termtype(&out)
}

/// Notification urgency.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

/// Structured notification. Rich fields reach only a confirmed OSC 99 terminal; other sinks get `title: body`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notification {
    pub title: Option<String>,
    pub body: Option<String>,
    pub id: Option<String>,
    pub kinds: Vec<String>,
    pub urgency: Option<Urgency>,
    pub icon_name: Option<String>,
    pub sound: Option<String>,
    /// `focus`, `report`, `focus-report` or `none`.
    pub actions: Option<String>,
    pub expires_ms: Option<i64>,
}

/// How a turn ended, for [`Message::turn`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    Complete,
    Error,
    Ask,
}

/// A plain text notification or a structured one (TS `string | TerminalNotification`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    Text(String),
    Structured(Notification),
}

impl Message {
    /// The notification for a finished turn: titled by the session name, focusing the terminal when clicked.
    pub fn turn(session_name: Option<&str>, outcome: TurnOutcome) -> Self {
        let (body, kind, urgency) = match outcome {
            TurnOutcome::Complete => ("Complete", "completion", Urgency::Normal),
            TurnOutcome::Error => ("Stopped with error", "error", Urgency::Critical),
            TurnOutcome::Ask => ("Waiting for input", "ask", Urgency::Normal),
        };
        Self::Structured(Notification {
            title: session_name.filter(|s| !s.trim().is_empty()).map(str::to_string),
            body: Some(body.to_string()),
            kinds: vec![kind.to_string()],
            urgency: Some(urgency),
            actions: Some("focus".to_string()),
            ..Notification::default()
        })
    }

    fn title_and_body(&self, app_name: &str) -> (String, String) {
        match self {
            Self::Text(s) => (app_name.to_string(), s.clone()),
            Self::Structured(n) => (
                n.title
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .unwrap_or(app_name)
                    .to_string(),
                n.body.clone().unwrap_or_default(),
            ),
        }
    }

    fn urgency(&self) -> Urgency {
        match self {
            Self::Text(_) => Urgency::Normal,
            Self::Structured(n) => n.urgency.unwrap_or_default(),
        }
    }
}

/// `title: body`, or whichever is present.
fn notification_to_line(n: &Notification) -> String {
    match (&n.title, &n.body) {
        (Some(t), Some(b)) if !t.is_empty() && !b.is_empty() => format!("{t}: {b}"),
        (Some(t), _) => t.clone(),
        (None, Some(b)) => b.clone(),
        (None, None) => String::new(),
    }
}

/// Control characters would end or corrupt an OSC payload; they become spaces (OSC 99 base64-encodes instead).
fn osc_safe(s: &str) -> String {
    s.chars().map(|c| if is_osc_unsafe(c) { ' ' } else { c }).collect()
}

fn is_osc_unsafe(c: char) -> bool {
    matches!(c as u32, 0x00..=0x1f | 0x7f..=0x9f)
}

const OSC99_MAX_PAYLOAD_BYTES: usize = 2048;

fn sanitize_osc99_id(id: Option<&str>) -> String {
    let safe: String = id
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.'))
        .collect();
    if safe == "0" { String::new() } else { safe }
}

fn chunk_utf8(payload: &str) -> Vec<&str> {
    if payload.is_empty() {
        return vec![""];
    }
    let mut chunks = Vec::new();
    let (mut start, mut bytes) = (0, 0);
    for (i, c) in payload.char_indices() {
        let n = c.len_utf8();
        if bytes > 0 && bytes + n > OSC99_MAX_PAYLOAD_BYTES {
            chunks.push(&payload[start..i]);
            start = i;
            bytes = 0;
        }
        bytes += n;
    }
    chunks.push(&payload[start..]);
    chunks
}

fn osc99_chunk(meta: &[String], payload: &str) -> String {
    if payload.chars().any(is_osc_unsafe) {
        let mut meta = meta.to_vec();
        meta.push("e=1".into());
        return format!("\x1b]99;{};{}\x1b\\", meta.join(":"), B64.encode(payload));
    }
    format!("\x1b]99;{};{payload}\x1b\\", meta.join(":"))
}

fn osc99_payload(meta: &[String], payload: &str, hold: bool) -> String {
    let chunks = chunk_utf8(payload);
    let last = chunks.len() - 1;
    chunks
        .iter()
        .enumerate()
        .map(|(i, chunk)| {
            let mut m = meta.to_vec();
            if hold || i < last {
                m.push("d=0".into());
            }
            osc99_chunk(&m, chunk)
        })
        .collect()
}

/// Terminal protocol, app name and OSC 99 state; formats and routes one notification.
#[derive(Clone, Debug)]
pub struct Notifier {
    pub terminal: TerminalId,
    pub protocol: NotifyProtocol,
    pub app_name: String,
    pub env: Env,
    /// Linux with a session bus: a BEL terminal also gets a D-Bus toast.
    pub desktop_session: bool,
    /// Set once the terminal answered the OSC 99 `p=?` probe.
    pub osc99_confirmed: bool,
    next_osc99_id: u64,
}

impl Notifier {
    pub fn new(terminal: TerminalId, env: Env, desktop_session: bool) -> Self {
        Self {
            terminal,
            protocol: terminal.notify_protocol(),
            app_name: APP_NAME.to_string(),
            env,
            desktop_session,
            osc99_confirmed: false,
            next_osc99_id: 1,
        }
    }

    /// From the process environment (runs `tmux display-message` inside tmux; call at startup, not while painting).
    pub fn detect() -> Self {
        let env = Env::from_process();
        let tmux_client = query_tmux_client_terminal_name(&env);
        let terminal = detect_terminal_id(&env, tmux_client.as_deref());
        let session = has_linux_desktop_session(std::env::consts::OS, &env, &|p| Path::new(p).exists());
        Self::new(terminal, env, session)
    }

    pub fn suppressed(&self) -> bool {
        matches!(self.env.get(NOTIFICATIONS_ENV), Some("off" | "0" | "false"))
    }

    /// The terminal's in-band sequence for `message` (`formatNotification`).
    pub fn format(&mut self, message: &Message) -> String {
        let (prefix, text) = match (self.protocol, message) {
            (NotifyProtocol::Bell, _) => return "\x07".into(),
            (NotifyProtocol::Osc777, m) => {
                let (title, body) = m.title_and_body(&self.app_name);
                return format!(
                    "\x1b]777;notify;{};{}\x1b\\",
                    osc_safe(&title).replace(';', ","),
                    osc_safe(&body)
                );
            }
            (NotifyProtocol::Osc99, Message::Structured(n)) if self.osc99_confirmed => {
                return self.format_osc99(n);
            }
            (NotifyProtocol::Osc99, Message::Structured(n)) => ("\x1b]99;;", notification_to_line(n)),
            (NotifyProtocol::Osc99, Message::Text(s)) => ("\x1b]99;;", s.clone()),
            (NotifyProtocol::Osc9, Message::Structured(n)) => ("\x1b]9;", notification_to_line(n)),
            (NotifyProtocol::Osc9, Message::Text(s)) => ("\x1b]9;", s.clone()),
        };
        format!("{prefix}{}\x1b\\", osc_safe(&text))
    }

    fn format_osc99(&mut self, n: &Notification) -> String {
        let mut id = sanitize_osc99_id(n.id.as_deref());
        if id.is_empty() {
            id = format!("{}-{}", self.app_name, self.next_osc99_id);
            self.next_osc99_id += 1;
        }
        let mut meta = vec![format!("i={id}"), format!("f={}", B64.encode(&self.app_name))];
        let actions = match n.actions.as_deref() {
            Some("focus") => Some("focus"),
            Some("report") => Some("report"),
            Some("focus-report") => Some("focus,report"),
            Some("none") => Some("-focus"),
            _ => None,
        };
        if let Some(a) = actions {
            meta.push(format!("a={a}"));
        }
        if let Some(u) = n.urgency {
            meta.push(format!(
                "u={}",
                match u {
                    Urgency::Low => 0,
                    Urgency::Normal => 1,
                    Urgency::Critical => 2,
                }
            ));
        }
        meta.extend(n.kinds.iter().map(|t| format!("t={}", B64.encode(t))));
        if let Some(icon) = n.icon_name.as_deref().filter(|s| !s.is_empty()) {
            meta.push(format!("n={}", B64.encode(icon)));
        }
        if let Some(sound) = n.sound.as_deref().filter(|s| !s.is_empty()) {
            meta.push(format!("s={}", B64.encode(sound)));
        }
        if let Some(w) = n.expires_ms {
            meta.push(format!("w={}", w.max(-1)));
        }
        let title = n.title.as_deref().or(n.body.as_deref()).unwrap_or("");
        let body = n
            .title
            .as_deref()
            .filter(|t| !t.is_empty())
            .and(n.body.as_deref())
            .filter(|b| !b.is_empty());
        match body {
            Some(body) => {
                osc99_payload(&meta, title, true) + &osc99_payload(&[format!("i={id}"), "p=body".into()], body, false)
            }
            None => osc99_payload(&meta, title, false),
        }
    }

    /// Route `message` (`sendNotification`): multiplexer notifiers first, then the terminal sequence, then D-Bus
    /// for BEL terminals. Never fails; a failed spawn falls through to the next sink.
    pub fn deliver(&mut self, message: &Message, sink: &mut dyn NotifySink) {
        if self.suppressed() {
            return;
        }
        let formatted = self.format(message);
        let sequence = (self.protocol != NotifyProtocol::Bell).then_some(formatted.as_str());
        if route_multiplexer_notification(message, &self.env, sequence, &self.app_name, sink) {
            return;
        }
        sink.write(&formatted);
        if self.protocol == NotifyProtocol::Bell
            && self.desktop_session
            && self.env.get(NO_DESKTOP_NOTIFY_ENV) != Some("1")
            && let Some(notifier) = sink.desktop_notifier()
        {
            sink.spawn(&build_desktop_notify_command(&notifier, message, &self.app_name));
        }
    }
}

/// Side effects of delivery: terminal writes, detached process spawns and the PATH probe for D-Bus notifiers.
pub trait NotifySink {
    fn write(&mut self, data: &str);
    /// Start `argv` detached; false when it could not be spawned.
    fn spawn(&mut self, argv: &[String]) -> bool;
    /// `notify-send` or `gdbus` on PATH (cached by the implementation).
    fn desktop_notifier(&mut self) -> Option<DesktopNotifier>;
}

/// Terminal multiplexers, in classification order within each precedence tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Multiplexer {
    Herdr,
    Tmux,
    Screen,
    Zellij,
    Cmux,
    Wmux,
    Orca,
}

/// Notification delivery tier, innermost first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum NotifierTier {
    Pane,
    Surface,
    InBand,
}

const REGISTRY: [Multiplexer; 7] = [
    Multiplexer::Herdr,
    Multiplexer::Tmux,
    Multiplexer::Screen,
    Multiplexer::Zellij,
    Multiplexer::Cmux,
    Multiplexer::Wmux,
    Multiplexer::Orca,
];

impl Multiplexer {
    /// Whether this multiplexer's own session markers are present (TERM is not consulted).
    pub fn is_inside(self, env: &Env) -> bool {
        match self {
            Self::Herdr => {
                env.get("HERDR_ENV") == Some("1")
                    || env.truthy("HERDR_PANE_ID")
                    || env.truthy("HERDR_TAB_ID")
                    || env.truthy("HERDR_WORKSPACE_ID")
            }
            Self::Tmux => env.truthy("TMUX"),
            Self::Screen => env.truthy("STY"),
            Self::Zellij => env.truthy("ZELLIJ"),
            Self::Cmux => {
                env.truthy("CMUX_WORKSPACE_ID") || env.truthy("CMUX_SURFACE_ID") || env.truthy("CMUX_REMOTE_TRANSPORT")
            }
            Self::Wmux => env.get("WMUX") == Some("1") || env.truthy("WMUX_SURFACE_ID"),
            Self::Orca => {
                let nonblank = |k| env.get(k).is_some_and(|v: &str| !v.trim().is_empty());
                nonblank("ORCA_PANE_KEY") && nonblank("ORCA_WORKTREE_ID")
            }
        }
    }

    fn outer_app(self) -> bool {
        self == Self::Orca
    }

    fn term_prefix(self) -> Option<&'static str> {
        match self {
            Self::Tmux => Some("tmux"),
            Self::Screen => Some("screen"),
            _ => None,
        }
    }

    pub fn owns_screen_grid(self) -> bool {
        self != Self::Orca
    }

    pub fn notifier_tier(self) -> Option<NotifierTier> {
        match self {
            Self::Herdr => Some(NotifierTier::Pane),
            Self::Cmux => Some(NotifierTier::Surface),
            Self::Tmux | Self::Zellij => Some(NotifierTier::InBand),
            _ => None,
        }
    }

    /// Deliver through this multiplexer; false leaves the notification to the next notifier.
    fn send(
        self,
        message: &Message,
        env: &Env,
        sequence: Option<&str>,
        app_name: &str,
        sink: &mut dyn NotifySink,
    ) -> bool {
        match self {
            Self::Tmux => sequence.is_some_and(|s| {
                sink.write(&format!("{}\x07", wrap_tmux_passthrough(s)));
                true
            }),
            Self::Zellij => sequence.is_some_and(|s| {
                sink.write(&format!("{s}\x07"));
                true
            }),
            Self::Cmux => {
                let Some(surface) = env.get("CMUX_SURFACE_ID").map(str::trim).filter(|s| is_uuid(s)) else {
                    return false;
                };
                let (title, body) = message.title_and_body(app_name);
                sink.spawn(&argv(&[
                    "cmux",
                    "notify",
                    "--surface",
                    surface,
                    "--title",
                    &title,
                    "--body",
                    &body,
                ]))
            }
            Self::Herdr => {
                let valid_pane = env.get("HERDR_PANE_ID").map(str::trim).is_some_and(|p| {
                    (1..=64).contains(&p.len())
                        && p.chars()
                            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'))
                });
                if !valid_pane {
                    return false;
                }
                let (mut title, body) = message.title_and_body(app_name);
                if matches!(title.as_str(), "help" | "--help" | "-h") {
                    title = app_name.to_string();
                }
                let kinds: &[String] = match message {
                    Message::Structured(n) => &n.kinds,
                    Message::Text(_) => &[],
                };
                let has = |k: &str| kinds.iter().any(|x| x == k);
                let sound = if has("ask") || has("error") {
                    "request"
                } else if has("completion") {
                    "done"
                } else {
                    "none"
                };
                sink.spawn(&argv(&[
                    "herdr",
                    "notification",
                    "show",
                    &title,
                    "--body",
                    &body,
                    "--sound",
                    sound,
                ]))
            }
            Self::Screen | Self::Wmux | Self::Orca => false,
        }
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// The multiplexer hosting this process: session markers, then a `tmux`/`screen` TERM prefix, then outer apps.
pub fn classify_multiplexer(env: &Env) -> Option<Multiplexer> {
    let term = env.get("TERM").unwrap_or("").to_ascii_lowercase();
    REGISTRY
        .into_iter()
        .find(|m| !m.outer_app() && m.is_inside(env))
        .or_else(|| {
            REGISTRY
                .into_iter()
                .find(|m| m.term_prefix().is_some_and(|p| term.starts_with(p)))
        })
        .or_else(|| REGISTRY.into_iter().find(|m| m.outer_app() && m.is_inside(env)))
}

/// Wrap a control sequence in tmux's DCS passthrough envelope.
pub fn wrap_tmux_passthrough(payload: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", payload.replace('\x1b', "\x1b\x1b"))
}

/// Offer `message` to every active session's notifier, innermost tier first; true when one delivered it.
pub fn route_multiplexer_notification(
    message: &Message,
    env: &Env,
    sequence: Option<&str>,
    app_name: &str,
    sink: &mut dyn NotifySink,
) -> bool {
    let mut notifiers: Vec<(NotifierTier, Multiplexer)> = REGISTRY
        .into_iter()
        .filter_map(|m| m.notifier_tier().map(|t| (t, m)))
        .collect();
    notifiers.sort_by_key(|(tier, _)| *tier);
    notifiers
        .into_iter()
        .any(|(_, m)| m.is_inside(env) && m.send(message, env, sequence, app_name, sink))
}

/// Resolved D-Bus notification binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DesktopNotifierKind {
    NotifySend,
    Gdbus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopNotifier {
    pub kind: DesktopNotifierKind,
    pub path: String,
}

/// Linux plus a session bus address or the systemd user-bus socket at `$XDG_RUNTIME_DIR/bus`.
pub fn has_linux_desktop_session(platform: &str, env: &Env, file_exists: &dyn Fn(&str) -> bool) -> bool {
    if platform != "linux" {
        return false;
    }
    if env.truthy("DBUS_SESSION_BUS_ADDRESS") {
        return true;
    }
    env.get("XDG_RUNTIME_DIR")
        .filter(|d| !d.is_empty())
        .is_some_and(|dir| file_exists(&format!("{}/bus", dir.trim_end_matches('/'))))
}

/// Prefer `notify-send`, else `gdbus`.
pub fn resolve_desktop_notifier(which: &dyn Fn(&str) -> Option<String>) -> Option<DesktopNotifier> {
    which("notify-send")
        .map(|path| DesktopNotifier {
            kind: DesktopNotifierKind::NotifySend,
            path,
        })
        .or_else(|| {
            which("gdbus").map(|path| DesktopNotifier {
                kind: DesktopNotifierKind::Gdbus,
                path,
            })
        })
}

/// The argv delivering `message` through `notifier`.
pub fn build_desktop_notify_command(notifier: &DesktopNotifier, message: &Message, app_name: &str) -> Vec<String> {
    let (title, body) = message.title_and_body(app_name);
    let (name, byte) = match message.urgency() {
        Urgency::Low => ("low", 0),
        Urgency::Normal => ("normal", 1),
        Urgency::Critical => ("critical", 2),
    };
    match notifier.kind {
        DesktopNotifierKind::NotifySend => argv(&[
            &notifier.path,
            "--app-name",
            app_name,
            &format!("--urgency={name}"),
            "--expire-time=5000",
            &title,
            &body,
        ]),
        DesktopNotifierKind::Gdbus => argv(&[
            &notifier.path,
            "call",
            "--session",
            "--dest",
            "org.freedesktop.Notifications",
            "--object-path",
            "/org/freedesktop/Notifications",
            "--method",
            "org.freedesktop.Notifications.Notify",
            app_name,
            "0",
            "",
            &title,
            &body,
            "[]",
            &format!("{{\"urgency\": <byte {byte}>}}"),
            "5000",
        ]),
    }
}

/// First executable `name` on `PATH`.
pub fn which(name: &str, path_var: Option<&str>) -> Option<String> {
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(name))
        .find_map(|p| {
            let meta = std::fs::metadata(&p).ok()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 == 0 {
                    return None;
                }
            }
            meta.is_file().then(|| p.to_string_lossy().into_owned())
        })
}

/// Real sink: writes through `write`, spawns children detached and reaps them on a thread.
pub struct ProcessSink<W: FnMut(&str) + Send> {
    write: W,
    path_var: Option<String>,
    notifier: Option<Option<DesktopNotifier>>,
}

impl<W: FnMut(&str) + Send> ProcessSink<W> {
    pub fn new(write: W, path_var: Option<String>) -> Self {
        Self {
            write,
            path_var,
            notifier: None,
        }
    }
}

impl<W: FnMut(&str) + Send> NotifySink for ProcessSink<W> {
    fn write(&mut self, data: &str) {
        (self.write)(data);
    }

    fn spawn(&mut self, argv: &[String]) -> bool {
        let Some((program, args)) = argv.split_first() else {
            return false;
        };
        match Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                thread::spawn(move || child.wait());
                true
            }
            Err(_) => false,
        }
    }

    fn desktop_notifier(&mut self) -> Option<DesktopNotifier> {
        let path_var = self.path_var.clone();
        self.notifier
            .get_or_insert_with(|| resolve_desktop_notifier(&|name| which(name, path_var.as_deref())))
            .clone()
    }
}

enum Job {
    Notify(Message),
    ConfirmOsc99(bool),
}

/// Fire-and-forget notifications: a thread owns the [`Notifier`] and its sink; `notify` only queues.
pub struct NotificationService {
    tx: Option<Sender<Job>>,
    worker: Option<JoinHandle<()>>,
}

impl NotificationService {
    pub fn spawn<S: NotifySink + Send + 'static>(mut notifier: Notifier, mut sink: S) -> Self {
        let (tx, rx) = mpsc::channel::<Job>();
        let worker = thread::Builder::new()
            .name("notify".into())
            .spawn(move || {
                for job in rx {
                    match job {
                        Job::Notify(m) => notifier.deliver(&m, &mut sink),
                        Job::ConfirmOsc99(ok) => notifier.osc99_confirmed = ok,
                    }
                }
            })
            .ok();
        Self { tx: Some(tx), worker }
    }

    /// Queue a notification; returns immediately.
    pub fn notify(&self, message: Message) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job::Notify(message));
        }
    }

    /// Record the OSC 99 `p=?` probe result.
    pub fn set_osc99_supported(&self, supported: bool) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(Job::ConfirmOsc99(supported));
        }
    }

    /// Deliver everything queued, then stop the worker.
    pub fn shutdown(mut self) {
        self.tx.take();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl Drop for NotificationService {
    fn drop(&mut self) {
        self.tx.take();
    }
}
