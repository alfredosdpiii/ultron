//! The terminal side of `ultron setup` (cli/setup/tui.ts): a transcript of what each step found and did, with the
//! current question below it, painted on the main screen (the transcript stays in the scrollback). Questions use
//! Pi's components (the `/login` provider picker and login dialog, bordered selectors) and a masked input for
//! secrets. Esc answers a question with "skip/back"; Ctrl+C quits the wizard. Also the startup selector the
//! first-run offer asks with (cli/startup-ui.ts `showStartupSelector`).

use crate::Env;
use crate::setup_wizard::{Choice, InputOptions, LoginOutcome, SetupQuit, SetupUi, Tone};
use ai::auth::{AuthEvent, AuthInteraction, AuthPrompt};
use ai::http::{AbortSignal, BoxFuture};
use interactive::lane::{LoginEvent, LoginPrompt};
use interactive::login_dialog::{DialogAction, LoginDialog};
use interactive::selectors::{AuthProvider, AuthSelector};
use interactive::{bold, key_hint, key_text};
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;
use tui::components::{Component, Text};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::renderer::Renderer;
use tui::select::{Loader, SelectEvent, SelectItem, SelectList, SelectListTheme};
use tui::theme::{ColorMode, Theme, builtin_theme_json, create_theme};

const MAX_VISIBLE: usize = 10;

fn fg(theme: &Theme, color: &str, text: &str) -> String {
    theme.fg(color, text).unwrap_or_else(|_| text.to_string())
}
const CTRL_C: &str = "\x03";

/// What the UI thread waits on.
enum Ev {
    Input(String),
    Closed,
    Login(LoginMsg),
}

enum LoginMsg {
    Event(LoginEvent),
    Prompt(LoginPrompt, tokio::sync::oneshot::Sender<Option<String>>),
    Done(Result<(), String>),
}

static INPUT_SINK: std::sync::Mutex<Option<Sender<Ev>>> = std::sync::Mutex::new(None);
static INPUT_PUMP: std::sync::Once = std::sync::Once::new();
static INPUT_PAUSED: std::sync::LazyLock<Arc<std::sync::atomic::AtomicBool>> =
    std::sync::LazyLock::new(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));

/// The terminal: raw mode, the main-screen renderer, and the hardware cursor row it leaves (for `stop`).
struct Screen {
    out: tui::terminal::Terminal<std::io::Stdout>,
    renderer: Renderer,
    /// Where the hardware cursor is, as a row of the document (`hardwareCursorRow`).
    cursor_row: i64,
    events: Receiver<Ev>,
    sender: Sender<Ev>,
    theme: Rc<Theme>,
    keys: KeybindingsManager,
}

fn keys() -> KeybindingsManager {
    KeybindingsManager::new(
        tui::qol_keys::native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

fn theme(env: &Env) -> Rc<Theme> {
    let settings = config::settings::SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    let caps = tui::image::detect_capabilities(&|k| (env.var)(k), false, false);
    let name = settings.theme().unwrap_or_else(|| "dark".into());
    let json = builtin_theme_json(&name)
        .or_else(|| builtin_theme_json("dark"))
        .expect("the dark theme is built in");
    Rc::new(create_theme(&json, ColorMode::from_capabilities(&caps), None).expect("the built-in theme compiles"))
}

/// Advance `row` over the cursor moves of renderer output (`\x1b[nA`, `\x1b[nB`, CRLF, a clear to home).
fn track(row: &mut i64, out: &str) {
    let bytes = out.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            *row += 1;
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"\x1b[2J\x1b[H") {
            *row = 0;
            i += 7;
            continue;
        }
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 2 && j < bytes.len() {
                let n: i64 = out[i + 2..j].parse().unwrap_or(0);
                match bytes[j] {
                    b'A' => *row -= n,
                    b'B' => *row += n,
                    _ => {}
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
}

impl Screen {
    fn start(env: &Env) -> std::io::Result<Screen> {
        let (sender, events) = mpsc::channel();
        // One stdin reader for the process, feeding the screen started last: the first-run selector's screen and
        // the wizard's that follows it must not both read the terminal (the first would swallow the wizard's keys).
        // It stops reading while no setup screen runs (`stop`), so the session TUI that may follow owns stdin.
        *INPUT_SINK.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender.clone());
        INPUT_PAUSED.store(false, std::sync::atomic::Ordering::SeqCst);
        INPUT_PUMP.call_once(|| {
            let (tx, rx) = mpsc::channel();
            interactive::run::spawn_gated_input(tx, Arc::clone(&INPUT_PAUSED));
            std::thread::spawn(move || {
                let send = |ev: Ev| {
                    if let Some(sink) = INPUT_SINK.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                        let _ = sink.send(ev);
                    }
                };
                for event in rx {
                    match event {
                        interactive::run::UiEvent::Input(data) => send(Ev::Input(data)),
                        interactive::run::UiEvent::InputClosed => send(Ev::Closed),
                        interactive::run::UiEvent::Lane(_) => {}
                    }
                }
            });
        });
        let mut out = tui::terminal::Terminal::new(std::io::stdout());
        out.start(true)?;
        out.hide_cursor()?;
        Ok(Screen {
            out,
            renderer: Renderer::new(),
            cursor_row: 0,
            events,
            sender,
            theme: theme(env),
            keys: keys(),
        })
    }

    fn size() -> (usize, usize) {
        let (w, h) = tui::terminal::size();
        (w.max(1) as usize, h.max(1) as usize)
    }

    fn paint(&mut self, lines: Vec<String>) {
        let (width, height) = Self::size();
        if let Ok(out) = self.renderer.render(lines, width, height) {
            track(&mut self.cursor_row, &out);
            let _ = self.out.write(&out);
        }
    }

    fn next(&self, timeout: Duration) -> Option<Ev> {
        match self.events.recv_timeout(timeout) {
            Ok(ev) => Some(ev),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Ev::Closed),
        }
    }

    /// `TuiMainScreen.stop`: below the last line, cursor shown, raw mode off.
    fn stop(&mut self) {
        INPUT_PAUSED.store(true, std::sync::atomic::Ordering::SeqCst);
        let len = self.renderer.previous_lines().len() as i64;
        if len > 0 {
            let mut out = String::from(" ");
            let diff = len - self.cursor_row;
            if diff > 0 {
                out.push_str(&format!("\x1b[{diff}B"));
            } else if diff < 0 {
                out.push_str(&format!("\x1b[{}A", -diff));
            }
            out.push_str("\r\n");
            let _ = self.out.write(&out);
        }
        let _ = self.out.show_cursor();
        let _ = self.out.stop();
        let _ = std::io::stdout().flush();
    }
}

fn hint(theme: &Theme, keys: &KeybindingsManager, binding: &str, description: &str) -> String {
    key_hint(theme, &key_text(keys, binding, false), description)
}

fn border(theme: &Theme, width: usize) -> String {
    fg(theme, "border", &"─".repeat(width.max(1)))
}

fn select_theme(theme: &Rc<Theme>) -> SelectListTheme {
    let style = |color: &'static str| {
        let t = Rc::clone(theme);
        Box::new(move |s: &str| fg(&t, color, s)) as Box<dyn Fn(&str) -> String>
    };
    SelectListTheme {
        selected_prefix: style("accent"),
        selected_text: style("accent"),
        description: style("muted"),
        scroll_info: style("muted"),
        no_match: style("muted"),
    }
}

/// A bordered list; with more than a screenful of choices, typing filters it (`SetupSelect`).
struct SetupSelect {
    title: String,
    description: Option<String>,
    items: Vec<SelectItem>,
    search: Option<tui::input::Input>,
    list: SelectList,
}

impl SetupSelect {
    fn new(screen: &Screen, title: &str, choices: &[Choice], description: Option<&str>) -> Self {
        let items: Vec<SelectItem> = choices
            .iter()
            .enumerate()
            .map(|(i, c)| SelectItem {
                value: i.to_string(),
                label: c.label.clone(),
                description: c.description.clone(),
            })
            .collect();
        let search = (items.len() > MAX_VISIBLE).then(|| {
            let mut input =
                tui::input::Input::new(screen.keys.clone()).with_options(Some("Search: "), None, None, None);
            input.focused = true;
            input
        });
        SetupSelect {
            title: title.into(),
            description: description.map(str::to_string),
            list: Self::list(screen, items.clone()),
            items,
            search,
        }
    }

    fn list(screen: &Screen, items: Vec<SelectItem>) -> SelectList {
        let visible = MAX_VISIBLE.min(items.len().max(1));
        let mut list = SelectList::new(items, visible, select_theme(&screen.theme));
        list.min_primary_column_width = Some(16);
        list.max_primary_column_width = Some(48);
        list
    }

    /// `Some(answer)` once answered (`None` inside: Esc).
    fn handle(&mut self, screen: &Screen, data: &str) -> Option<Option<usize>> {
        let k = |id: &str| screen.keys.matches(data, id);
        let navigation =
            k("tui.select.up") || k("tui.select.down") || k("tui.select.confirm") || k("tui.select.cancel");
        if navigation || self.search.is_none() {
            return match self.list.handle_input(data) {
                Some(SelectEvent::Selected(item)) => Some(item.value.parse().ok()),
                Some(SelectEvent::Cancelled) => Some(None),
                _ => None,
            };
        }
        let search = self.search.as_mut().expect("checked above");
        search.handle_input(data);
        let query = search.value().to_string();
        let filtered = if query.is_empty() {
            self.items.clone()
        } else {
            tui::fuzzy::fuzzy_filter(&self.items, &query, |item| {
                format!("{} {}", item.label, item.description.clone().unwrap_or_default())
            })
        };
        self.list = Self::list(screen, filtered);
        None
    }

    fn render(&mut self, screen: &Screen, width: usize) -> Vec<String> {
        let t = &screen.theme;
        let mut lines = vec![border(t, width)];
        lines.extend(Text::new(&fg(t, "accent", &bold(&self.title)), 1, 0, None).render(width));
        if let Some(description) = &self.description {
            lines.extend(Text::new(&fg(t, "text", description), 1, 0, None).render(width));
        }
        lines.push(String::new());
        if let Some(search) = self.search.as_mut() {
            lines.extend(search.render(width));
        }
        lines.extend(self.list.render(width));
        lines.push(String::new());
        let hints = format!(
            "{}  {}  {}  {}",
            key_hint(t, "↑↓", "navigate"),
            hint(t, &screen.keys, "tui.select.confirm", "select"),
            key_hint(t, "esc", "skip/back"),
            key_hint(t, "ctrl+c", "quit setup")
        );
        lines.extend(Text::new(&hints, 1, 0, None).render(width));
        lines.push(border(t, width));
        lines
    }
}

/// A bordered one-line question; secrets are masked and validation errors show under the input (`SetupInput`).
struct SetupInput {
    title: String,
    options: InputOptions,
    input: tui::input::Input,
    error: String,
}

impl SetupInput {
    fn new(screen: &Screen, title: &str, options: InputOptions) -> Self {
        let mut input = if options.secret {
            tui::input::Input::new(screen.keys.clone()).with_options(None, None, None, Some("•"))
        } else {
            tui::input::Input::new(screen.keys.clone())
        };
        input.focused = true;
        if let Some(initial) = options.initial.as_deref().filter(|i| !i.is_empty()) {
            input.set_value(initial);
            input.move_cursor_to_end();
        }
        SetupInput {
            title: title.into(),
            options,
            input,
            error: String::new(),
        }
    }

    fn handle(&mut self, screen: &Screen, data: &str) -> Option<Option<String>> {
        if screen.keys.matches(data, "tui.select.cancel") {
            return Some(None);
        }
        self.error.clear();
        match self.input.handle_input(data) {
            Some(tui::input::InputEvent::Submit(value)) => {
                if let Some(problem) = self.options.validate.as_ref().and_then(|v| v(&value)) {
                    self.error = fg(&screen.theme, "error", &problem);
                    return None;
                }
                Some(Some(value))
            }
            Some(tui::input::InputEvent::Escape) => Some(None),
            None => None,
        }
    }

    fn render(&mut self, screen: &Screen, width: usize) -> Vec<String> {
        let t = &screen.theme;
        let mut lines = vec![border(t, width)];
        lines.extend(Text::new(&fg(t, "accent", &bold(&self.title)), 1, 0, None).render(width));
        if let Some(description) = &self.options.description {
            lines.extend(Text::new(&fg(t, "text", description), 1, 0, None).render(width));
        }
        if self.options.secret {
            lines.extend(
                Text::new(
                    &fg(t, "dim", "Input is hidden. Paste the value and press Enter."),
                    1,
                    0,
                    None,
                )
                .render(width),
            );
        }
        lines.push(String::new());
        lines.extend(self.input.render(width));
        lines.extend(Text::new(&self.error, 1, 0, None).render(width));
        let hints = format!(
            "{}  {}  {}",
            hint(t, &screen.keys, "tui.select.confirm", "submit"),
            key_hint(t, "esc", "skip/back"),
            key_hint(t, "ctrl+c", "quit setup")
        );
        lines.extend(Text::new(&hints, 1, 0, None).render(width));
        lines.push(border(t, width));
        lines
    }
}

enum Entry {
    Heading(String),
    Note(String, Tone),
}

/// [`SetupUi`] on the terminal: notes go into a transcript, the current question sits in a slot below it.
pub struct TuiSetupUi {
    screen: Screen,
    transcript: Vec<Entry>,
}

enum Slot<'a> {
    Empty,
    Select(&'a mut SetupSelect),
    Input(&'a mut SetupInput),
    Auth(&'a mut AuthSelector),
    Dialog(&'a mut LoginDialog),
    Loader(&'a mut Loader),
}

impl TuiSetupUi {
    pub fn start(env: &Env) -> std::io::Result<TuiSetupUi> {
        let mut ui = TuiSetupUi {
            screen: Screen::start(env)?,
            transcript: Vec::new(),
        };
        ui.paint(Slot::Empty);
        Ok(ui)
    }

    /// Paint the last frame and leave the terminal (`ui.stop()`); the transcript stays in the scrollback.
    pub fn stop(mut self) {
        self.paint(Slot::Empty);
        self.screen.stop();
    }

    fn paint(&mut self, slot: Slot) {
        let (width, _) = Screen::size();
        let t = Rc::clone(&self.screen.theme);
        let mut lines = Text::new(&fg(&t, "accent", &bold("Ultron setup")), 1, 0, None).render(width);
        for entry in &self.transcript {
            match entry {
                Entry::Heading(title) => {
                    lines.push(String::new());
                    lines.extend(Text::new(&fg(&t, "accent", &bold(title)), 1, 0, None).render(width));
                }
                Entry::Note(text, tone) => {
                    let color = match tone {
                        Tone::Success => "success",
                        Tone::Warning => "warning",
                        Tone::Error => "error",
                        Tone::Dim => "dim",
                        Tone::Info => "text",
                    };
                    lines.extend(Text::new(&fg(&t, color, text), 2, 0, None).render(width));
                }
            }
        }
        let screen = &self.screen;
        lines.extend(match slot {
            Slot::Empty => Vec::new(),
            Slot::Select(select) => select.render(screen, width),
            Slot::Input(input) => input.render(screen, width),
            Slot::Auth(selector) => selector.render(width),
            Slot::Dialog(dialog) => dialog.render(&t, width),
            Slot::Loader(loader) => loader.render(width),
        });
        self.screen.paint(lines);
    }

    /// The next key, or `Err` for Ctrl+C or a closed input (the frame is repainted while waiting).
    fn key(&mut self) -> Result<String, SetupQuit> {
        match self.screen.next(Duration::from_millis(100)) {
            Some(Ev::Input(data)) if data == CTRL_C => Err(SetupQuit),
            Some(Ev::Input(data)) => Ok(data),
            Some(Ev::Closed) => Err(SetupQuit),
            Some(Ev::Login(_)) | None => Ok(String::new()),
        }
    }
}

impl SetupUi for TuiSetupUi {
    fn heading(&mut self, title: &str) {
        self.transcript.push(Entry::Heading(title.into()));
        // Painted with the next frame: TS coalesces notes into one render.
    }

    fn note(&mut self, text: &str, tone: Tone) {
        self.transcript.push(Entry::Note(text.into(), tone));
        // Painted with the next frame: TS coalesces notes into one render.
    }

    fn select(
        &mut self,
        title: &str,
        choices: &[Choice],
        description: Option<&str>,
    ) -> Result<Option<usize>, SetupQuit> {
        let mut select = SetupSelect::new(&self.screen, title, choices, description);
        loop {
            self.paint(Slot::Select(&mut select));
            let data = self.key();
            let data = match data {
                Ok(data) => data,
                Err(quit) => {
                    // Ctrl+C: TS paints the cleared slot and what follows in one frame.
                    return Err(quit);
                }
            };
            if data.is_empty() {
                continue;
            }
            if let Some(answer) = select.handle(&self.screen, &data) {
                self.paint(Slot::Empty);
                return Ok(answer);
            }
        }
    }

    fn input(&mut self, title: &str, options: InputOptions) -> Result<Option<String>, SetupQuit> {
        let mut input = SetupInput::new(&self.screen, title, options);
        loop {
            self.paint(Slot::Input(&mut input));
            let data = match self.key() {
                Ok(data) => data,
                Err(quit) => {
                    // Ctrl+C: TS paints the cleared slot and what follows in one frame.
                    return Err(quit);
                }
            };
            if data.is_empty() {
                continue;
            }
            if let Some(answer) = input.handle(&self.screen, &data) {
                self.paint(Slot::Empty);
                return Ok(answer);
            }
        }
    }

    fn pick_login_provider(&mut self, providers: &[AuthProvider], _title: &str) -> Result<Option<usize>, SetupQuit> {
        let mut selector = AuthSelector::new(
            Rc::clone(&self.screen.theme),
            self.screen.keys.clone(),
            true,
            providers.to_vec(),
            None,
        );
        loop {
            self.paint(Slot::Auth(&mut selector));
            let data = match self.key() {
                Ok(data) => data,
                Err(quit) => {
                    // Ctrl+C: TS paints the cleared slot and what follows in one frame.
                    return Err(quit);
                }
            };
            if data.is_empty() {
                continue;
            }
            match selector.handle_input(&data) {
                Some(Ok((id, kind))) => {
                    self.paint(Slot::Empty);
                    return Ok(providers.iter().position(|p| p.id == id && p.auth_type == kind));
                }
                Some(Err(())) => {
                    self.paint(Slot::Empty);
                    return Ok(None);
                }
                None => {}
            }
        }
    }

    fn login(&mut self, env: &Env, option: &AuthProvider) -> Result<LoginOutcome, SetupQuit> {
        let reach = crate::tui_login::browser_reach(env);
        let mut dialog = LoginDialog::new(self.screen.keys.clone(), &option.name, &reach);
        let t = Rc::clone(&self.screen.theme);
        if option.auth_type != "oauth" {
            let method = match &option.api_key_method {
                Some((method, true)) if option.generic_login => method.clone(),
                _ => {
                    return Ok(LoginOutcome::Error(
                        "this provider's sign-in is not available in the Rust build yet".into(),
                    ));
                }
            };
            dialog.show_prompt(
                &t,
                &LoginPrompt::Text {
                    message: format!("Enter {method}"),
                    placeholder: None,
                    secret: true,
                },
            );
            loop {
                self.paint(Slot::Dialog(&mut dialog));
                // Ctrl+C stops the login as Esc does (`quit` cancels the dialog).
                let data = match self.screen.next(Duration::from_millis(100)) {
                    Some(Ev::Input(data)) => data,
                    Some(Ev::Closed) => return Err(SetupQuit),
                    _ => continue,
                };
                let action = if data == CTRL_C {
                    DialogAction::Cancel
                } else {
                    dialog.handle_input(&data)
                };
                match action {
                    DialogAction::Cancel => {
                        self.paint(Slot::Empty);
                        return Ok(LoginOutcome::Cancelled);
                    }
                    DialogAction::Answer(key) => {
                        self.paint(Slot::Empty);
                        let auth = config::paths::AgentPaths::new(env.agent_dir()).auth();
                        return Ok(
                            match config::auth::AuthStorage::create(auth)
                                .modify(&option.id, |_| Some(serde_json::json!({"type": "api_key", "key": key})))
                            {
                                Ok(_) => LoginOutcome::Ok,
                                Err(error) => LoginOutcome::Error(error),
                            },
                        );
                    }
                    DialogAction::None => {}
                }
            }
        }
        if !crate::tui_login::supported(env, &option.id) {
            return Ok(LoginOutcome::Error(
                "signing in with an account is not available in the Rust build yet; use an API key".into(),
            ));
        }
        let signal = AbortSignal::new();
        start_oauth(
            env.clone(),
            option.id.clone(),
            reach,
            signal.clone(),
            self.screen.sender.clone(),
        );
        let mut pending: Option<tokio::sync::oneshot::Sender<Option<String>>> = None;
        loop {
            self.paint(Slot::Dialog(&mut dialog));
            match self.screen.next(Duration::from_millis(100)) {
                Some(Ev::Login(LoginMsg::Event(event))) => dialog.show_event(&t, &event),
                Some(Ev::Login(LoginMsg::Prompt(prompt, reply))) => {
                    dialog.show_prompt(&t, &prompt);
                    pending = Some(reply);
                }
                Some(Ev::Login(LoginMsg::Done(result))) => {
                    self.paint(Slot::Empty);
                    return Ok(match result {
                        Ok(()) => LoginOutcome::Ok,
                        Err(error) if error == CANCELLED || signal.aborted() => LoginOutcome::Cancelled,
                        Err(error) => LoginOutcome::Error(error),
                    });
                }
                Some(Ev::Input(data)) => {
                    let action = if data == CTRL_C {
                        DialogAction::Cancel
                    } else {
                        dialog.handle_input(&data)
                    };
                    match action {
                        DialogAction::Cancel => {
                            signal.abort();
                            if let Some(reply) = pending.take() {
                                let _ = reply.send(None);
                            }
                        }
                        DialogAction::Answer(answer) => {
                            if let Some(reply) = pending.take() {
                                let _ = reply.send(Some(answer));
                            }
                        }
                        DialogAction::None => {}
                    }
                }
                Some(Ev::Closed) => {
                    signal.abort();
                    return Err(SetupQuit);
                }
                None => {}
            }
        }
    }

    fn busy<T: Send + 'static>(
        &mut self,
        message: &str,
        task: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, SetupQuit> {
        let t = Rc::clone(&self.screen.theme);
        let (spin, muted) = (Rc::clone(&t), Rc::clone(&t));
        let mut loader = Loader::new(
            Box::new(move |s: &str| fg(&spin, "accent", s)),
            Box::new(move |s: &str| fg(&muted, "muted", s)),
            message,
            None,
        );
        let (done, result) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = done.send(task());
        });
        loop {
            self.paint(Slot::Loader(&mut loader));
            if let Ok(value) = result.try_recv() {
                self.paint(Slot::Empty);
                return Ok(value);
            }
            match self.screen.next(Duration::from_millis(loader.interval_ms())) {
                Some(Ev::Input(data)) if data == CTRL_C => {
                    self.paint(Slot::Empty);
                    return Err(SetupQuit);
                }
                Some(Ev::Closed) => return Err(SetupQuit),
                None => loader.tick(),
                _ => {}
            }
        }
    }
}

/// `CANCELLED`.
const CANCELLED: &str = "Login cancelled";

struct Interaction {
    send: Sender<Ev>,
    signal: AbortSignal,
    reach: String,
    env: Env,
}

/// `openBrowser` on the run's own PATH; never waits on the opener.
fn open_browser(env: &Env, url: &str) {
    let name = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let Some(path) = (env.var)("PATH") else {
        return;
    };
    let Some(opener) = std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
    else {
        return;
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .env("PATH", &path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

impl AuthInteraction for Interaction {
    fn prompt<'a>(&'a self, prompt: AuthPrompt, cancel: Option<AbortSignal>) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let view = match prompt {
                AuthPrompt::Select { message, options } => LoginPrompt::Select {
                    message,
                    options: options.into_iter().map(|o| (o.id, o.label)).collect(),
                },
                AuthPrompt::ManualCode { message, .. } => LoginPrompt::ManualCode { message },
                AuthPrompt::Text { message, placeholder } => LoginPrompt::Text {
                    message,
                    placeholder,
                    secret: false,
                },
                AuthPrompt::Secret { message, placeholder } => LoginPrompt::Text {
                    message,
                    placeholder,
                    secret: true,
                },
            };
            if self.signal.aborted() || cancel.as_ref().is_some_and(AbortSignal::aborted) {
                return Err(CANCELLED.into());
            }
            let (sender, receiver) = tokio::sync::oneshot::channel();
            let _ = self.send.send(Ev::Login(LoginMsg::Prompt(view, sender)));
            let cancelled = async {
                match &cancel {
                    Some(cancel) => cancel.cancelled().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                answer = receiver => match answer {
                    Ok(Some(answer)) => Ok(answer),
                    _ => Err(CANCELLED.into()),
                },
                () = self.signal.cancelled() => Err(CANCELLED.into()),
                () = cancelled => Err(CANCELLED.into()),
            }
        })
    }

    fn notify(&self, event: AuthEvent) {
        let view = match event {
            AuthEvent::AuthUrl { url, instructions } => {
                if self.reach == "local" {
                    open_browser(&self.env, &url);
                }
                LoginEvent::AuthUrl { url, instructions }
            }
            AuthEvent::DeviceCode {
                user_code,
                verification_uri,
                ..
            } => LoginEvent::DeviceCode {
                user_code,
                verification_uri,
            },
            AuthEvent::Info { message } => LoginEvent::Info {
                message,
                links: Vec::new(),
            },
            AuthEvent::Progress { message } => LoginEvent::Progress(message),
        };
        let _ = self.send.send(Ev::Login(LoginMsg::Event(view)));
    }

    fn signal(&self) -> Option<AbortSignal> {
        Some(self.signal.clone())
    }
}

/// `runtime.login(provider, "oauth")` on its own thread; the credential is saved to auth.json.
fn start_oauth(env: Env, provider: String, reach: String, signal: AbortSignal, send: Sender<Ev>) {
    std::thread::spawn(move || {
        let interaction = Interaction {
            send: send.clone(),
            signal: signal.clone(),
            reach,
            env: env.clone(),
        };
        let outcome = (|| -> Result<(), String> {
            let options = crate::tui_login::options(&env);
            let oauth = session_core::stored_auth::oauth_handler_with(&env, &provider, &options)
                .ok_or_else(|| format!("{provider} does not support oauth login"))?;
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?;
            let credential = runtime.block_on(oauth.login(&interaction))?;
            if signal.aborted() {
                return Err(CANCELLED.into());
            }
            let auth = config::paths::AgentPaths::new(env.agent_dir()).auth();
            config::auth::AuthStorage::create(auth)
                .modify(&provider, |_| Some(credential.to_json()))
                .map(|_| ())
                .map_err(|e| e.to_string())
        })();
        let _ = send.send(Ev::Login(LoginMsg::Done(outcome)));
    });
}

/// `showStartupSelector` (`ExtensionSelectorComponent` on a startup TUI): the picked option's index, `None` for
/// Esc. The selector is cleared before the terminal is handed back.
pub fn show_startup_selector(env: &Env, title: &str, options: &[&str]) -> std::io::Result<Option<usize>> {
    let mut screen = Screen::start(env)?;
    let mut index = 0usize;
    let answer = loop {
        let (width, _) = Screen::size();
        let t = Rc::clone(&screen.theme);
        let mut lines = vec![border(&t, width), String::new()];
        lines.extend(Text::new(&fg(&t, "accent", &bold(title)), 1, 0, None).render(width));
        lines.push(String::new());
        for (i, option) in options.iter().enumerate() {
            let row = if i == index {
                format!("{}{}", fg(&t, "accent", "→ "), fg(&t, "accent", option))
            } else {
                format!("  {}", fg(&t, "text", option))
            };
            lines.extend(Text::new(&row, 1, 0, None).render(width));
        }
        lines.push(String::new());
        let hints = format!(
            "{}  {}  {}",
            key_hint(&t, "↑↓", "navigate"),
            hint(&t, &screen.keys, "tui.select.confirm", "select"),
            hint(&t, &screen.keys, "tui.select.cancel", "cancel")
        );
        lines.extend(Text::new(&hints, 1, 0, None).render(width));
        lines.push(String::new());
        lines.push(border(&t, width));
        screen.paint(lines);
        let data = match screen.next(Duration::from_millis(100)) {
            Some(Ev::Input(data)) => data,
            Some(Ev::Closed) => break None,
            _ => continue,
        };
        let k = |id: &str| screen.keys.matches(&data, id);
        if k("tui.select.up") || data == "k" {
            index = index.saturating_sub(1);
        } else if k("tui.select.down") || data == "j" {
            index = (index + 1).min(options.len().saturating_sub(1));
        } else if k("tui.select.confirm") || data == "\n" {
            break Some(index);
        } else if k("tui.select.cancel") {
            break None;
        }
    };
    // `clearStartupTui`: an empty frame, then stop.
    screen.paint(Vec::new());
    screen.stop();
    Ok(answer)
}
