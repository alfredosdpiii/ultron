//! The mouse in the fullscreen surfaces whose TypeScript components read it (tui-alt-screen.ts `handleMouseEvent`
//! through `Container.handleMouse` to settings-list.ts, select-list.ts, input.ts and editor.ts): settings submenus
//! (`SettingsList.handleMouse` hands the event to `submenuComponent`), the login dialog's and an extension's input,
//! an extension's editor dialog and the session selector's rename input. Surfaces built from text rows only take no
//! mouse in TypeScript (`TreeList`, `SessionList`, `UserMessageList`, `RlmPane`, `RlmGraphFocus`), nor here.
//! Literal expectations from the TypeScript sources.

use std::rc::Rc;
use std::sync::{Arc, Mutex};

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionListing, SessionOpened};
use interactive::qol::QolSettings;
use serde_json::{Value, json};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};
use tui::utils::strip_terminal_sequences;

const W: usize = 80;
const H: usize = 40;

struct Recorder(Arc<Mutex<Vec<String>>>);

impl Recorder {
    fn push(&self, call: String) {
        self.0.lock().unwrap().push(call);
    }
}

impl LaneClient for Recorder {
    fn prompt(&self, _: &str) {}
    fn steer(&self, _: &str) -> String {
        String::new()
    }
    fn follow_up(&self, _: &str) -> String {
        String::new()
    }
    fn cancel_queued(&self, _: &str) {}
    fn abort(&self) {}
    fn new_session(&self) {}
    fn resume(&self, _: &str) {}
    fn list_sessions(&self) {}
    fn set_name(&self, _: &str) {}
    fn select_model(&self, _: &str, _: &str) {}
    fn save_setting(&self, key: &str, value: Value) {
        self.push(format!("{key}={value}"));
    }
    fn login_providers(&self, _: &str) {}
    fn save_api_key(&self, provider: &str, key: &str) {
        self.push(format!("key {provider} {key}"));
    }
    fn extension_ui_respond(&self, id: &str, answer: Value) {
        self.push(format!("respond {id} {answer}"));
    }
    fn rename_session(&self, path: &str, name: &str) {
        self.push(format!("rename {path} {name}"));
    }
}

fn keys() -> KeybindingsManager {
    KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

fn app() -> (App, Arc<Mutex<Vec<String>>>) {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "Model a".into(),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut app = App::new(
        AppOptions {
            theme,
            keys: keys(),
            editor_keys: keys(),
            version: "1.2.3".into(),
            cwd: "/p".into(),
            home: None,
            server_id: "server".into(),
            models: vec![model.clone()],
            model,
            thinking_level: "off".into(),
            splash: false,
            history: PromptHistoryStore::new(None, "/p"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol: QolSettings::parity(),
        },
        Box::new(Recorder(Arc::clone(&calls))),
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "cur".into(),
        session_file: Some("/s/cur".into()),
        name: None,
        messages: Vec::new(),
    }));
    (app, calls)
}

fn screen(app: &mut App) -> Vec<String> {
    app.render(W, H)
        .iter()
        .map(|l| strip_terminal_sequences(l).trim_end().to_string())
        .collect()
}

fn shown(app: &mut App) -> String {
    screen(app).join("\n")
}

/// The screen cell (column, row) where `text` starts, on the last row showing it.
fn cell_of(app: &mut App, text: &str) -> (usize, usize) {
    let s = screen(app);
    let row = s
        .iter()
        .rposition(|l| l.contains(text))
        .unwrap_or_else(|| panic!("{text}: {s:#?}"));
    let byte = s[row].find(text).unwrap();
    (tui::utils::visible_width(&s[row][..byte]), row)
}

fn press(app: &mut App, (x, y): (usize, usize)) {
    app.handle_input(&format!("\x1b[<0;{};{}M", x + 1, y + 1));
}

fn release(app: &mut App, (x, y): (usize, usize)) {
    app.handle_input(&format!("\x1b[<0;{};{}m", x + 1, y + 1));
}

/// A left press and release on one cell (SGR, 1-based).
fn click(app: &mut App, at: (usize, usize)) {
    press(app, at);
    release(app, at);
}

fn typed(app: &mut App, text: &str) {
    for c in text.chars() {
        app.handle_input(&c.to_string());
    }
}

fn calls(c: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    c.lock().unwrap().clone()
}

fn open_settings(app: &mut App) {
    typed(app, "/settings");
    app.handle_input("\r");
    app.apply(LaneEvent::Settings {
        settings: json!({}),
        env_overrides: Default::default(),
        themes: vec!["dark".into(), "light".into()],
        terminal_theme: "dark".into(),
    });
}

/// settings-list.ts: a click on a row with a submenu opens it, and the submenu (`SelectSubmenu`, a `Container`)
/// takes the next click: its `SelectList` row is pressed (highlighted, previewed) and the click picks it.
#[test]
fn interactive_mouse_settings_select_submenu_row_picks_it() {
    let (mut app, c) = app();
    open_settings(&mut app);
    typed(&mut app, "theme");
    let at = cell_of(&mut app, "Theme");
    click(&mut app, at);
    let s = shown(&mut app);
    assert!(s.contains("Enter to select · Esc to go back"), "the theme submenu: {s}");
    let at = cell_of(&mut app, "light");
    click(&mut app, at);
    assert!(
        calls(&c).contains(&"theme=\"light\"".to_string()),
        "{:?}\n{s}",
        calls(&c)
    );
    assert!(
        !shown(&mut app).contains("Enter to select · Esc to go back"),
        "picking closes it"
    );
}

/// The wheel over a submenu list moves its selection (`SelectList.handleMouse` wheel); a click on its title row is
/// nobody's (`Text`).
#[test]
fn interactive_mouse_settings_submenu_wheel_moves_and_title_is_inert() {
    let (mut app, c) = app();
    open_settings(&mut app);
    typed(&mut app, "theme");
    let at = cell_of(&mut app, "Theme");
    click(&mut app, at);
    let before = shown(&mut app);
    let title = before
        .lines()
        .position(|l| l.trim() == "Theme")
        .unwrap_or_else(|| panic!("{before}"));
    click(&mut app, (2, title));
    assert_eq!(shown(&mut app), before, "the title takes no click");
    let at = cell_of(&mut app, "light");
    app.handle_input(&format!("\x1b[<64;{};{}M", at.0 + 1, at.1 + 1));
    app.handle_input(&format!("\x1b[<65;{};{}M", at.0 + 1, at.1 + 1));
    app.handle_input(&format!("\x1b[<65;{};{}M", at.0 + 1, at.1 + 1));
    // From the current theme (dark): up to Automatic, then down twice to light; Enter picks it.
    app.handle_input("\r");
    assert_eq!(calls(&c), ["theme=\"light\""], "Enter picks the row the wheel moved to");
}

/// input.ts through the login dialog (`LoginDialogComponent`'s prompt): a press on the key input moves its cursor
/// (a masked input counts its bullets).
#[test]
fn interactive_mouse_press_in_the_login_key_input_moves_its_cursor() {
    let (mut app, c) = app();
    typed(&mut app, "/login");
    app.handle_input("\r");
    app.apply(LaneEvent::LoginProviders {
        providers: vec![interactive::selectors::AuthProvider {
            id: "groq".into(),
            name: "Groq".into(),
            auth_type: "api_key".into(),
            status: None,
            api_key_method: Some(("Groq API key".into(), true)),
            login_label: None,
            generic_login: true,
        }],
        query: "groq".into(),
    });
    assert!(shown(&mut app).contains("Enter Groq API key"));
    typed(&mut app, "secret");
    let (x, y) = cell_of(&mut app, "> ••••••");
    // Column 3 is after "> •".
    press(&mut app, (x + 3, y));
    typed(&mut app, "X");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "key groq sXecret");
}

/// input.ts through `ExtensionInputComponent`.
#[test]
fn interactive_mouse_press_in_an_extension_input_moves_its_cursor() {
    let (mut app, c) = app();
    app.apply(LaneEvent::ExtensionUi(
        json!({"type": "extension_ui_request", "id": "p1", "method": "input", "title": "Name?"}),
    ));
    typed(&mut app, "abcd");
    let (x, y) = cell_of(&mut app, "> abcd");
    press(&mut app, (x + 4, y));
    typed(&mut app, "X");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "respond p1 {\"value\":\"abXcd\"}");
}

fn listing(path: &str, name: Option<&str>, first: &str, modified: i64) -> SessionListing {
    SessionListing {
        path: path.into(),
        id: path.into(),
        name: name.map(str::to_string),
        modified,
        message_count: 2,
        first_message: first.into(),
        all_messages_text: first.into(),
        cwd: "/p".into(),
    }
}

/// session-selector.ts: the rename panel's `Input` takes a press; the list (`SessionList`) takes no click.
#[test]
fn interactive_mouse_session_selector_rename_input_takes_a_press_and_the_list_none() {
    let (mut app, c) = app();
    typed(&mut app, "/resume");
    app.handle_input("\r");
    app.apply(LaneEvent::Sessions(vec![
        listing("/s/old", Some("Named one"), "old ask", 1),
        listing("/s/cur", None, "current ask", 3),
    ]));
    let before = shown(&mut app);
    assert!(before.contains("› current ask"), "{before}");
    let at = cell_of(&mut app, "Named one");
    click(&mut app, at);
    assert_eq!(shown(&mut app), before, "a click on a session row selects nothing");
    app.handle_input("\x12");
    assert!(shown(&mut app).contains("Rename Session"));
    typed(&mut app, "abc");
    let (x, y) = cell_of(&mut app, "> abc");
    press(&mut app, (x + 3, y));
    typed(&mut app, "X");
    app.handle_input("\r");
    assert_eq!(calls(&c).last().unwrap(), "rename /s/cur aXbc");
}

/// A list submenu (the automatic theme's `SettingsList` under its header, in the theme submenu's place): its rows
/// take clicks like the main list.
#[test]
fn interactive_mouse_settings_list_submenu_row_activates() {
    let (mut app, c) = app();
    open_settings(&mut app);
    typed(&mut app, "theme");
    let at = cell_of(&mut app, "Theme");
    click(&mut app, at);
    let at = cell_of(&mut app, "Automatic");
    click(&mut app, at);
    assert!(shown(&mut app).contains("Automatic Theme"));
    // A row that opens a select submenu above the list's header (the stack's enclosing headers stay shown).
    let at = cell_of(&mut app, "Light theme");
    click(&mut app, at);
    let s = shown(&mut app);
    assert!(
        s.contains("Automatic Theme") && s.contains("Enter to select · Esc to go back"),
        "{s}"
    );
    let at = cell_of(&mut app, "light");
    click(&mut app, at);
    let s = shown(&mut app);
    assert!(s.contains("Light theme  light"), "the pick went back to the list: {s}");
    let at = cell_of(&mut app, "Apply");
    click(&mut app, at);
    assert_eq!(calls(&c), ["theme=\"light/dark\""]);
}

/// input.ts through the `/login` provider selector (`OAuthSelectorComponent`) and the `/scoped-models` selector:
/// a press on their search line moves its cursor; their rows are text and take no click.
#[test]
fn interactive_mouse_press_in_selector_search_lines_moves_their_cursor() {
    let (mut app, _) = app();
    typed(&mut app, "/login");
    app.handle_input("\r");
    let provider = |kind: &str| interactive::selectors::AuthProvider {
        id: "groq".into(),
        name: "Groq".into(),
        auth_type: kind.into(),
        status: None,
        api_key_method: (kind == "api_key").then(|| ("Groq API key".to_string(), true)),
        login_label: None,
        generic_login: true,
    };
    app.apply(LaneEvent::LoginProviders {
        providers: vec![provider("oauth"), provider("api_key")],
        query: String::new(),
    });
    app.handle_input("\x1b[B");
    app.handle_input("\r");
    assert!(shown(&mut app).contains("Select provider to configure:"));
    typed(&mut app, "gro");
    let (x, y) = cell_of(&mut app, "> gro");
    press(&mut app, (x + 3, y));
    typed(&mut app, "X");
    assert!(shown(&mut app).contains("> gXro"), "{}", shown(&mut app));

    let (mut app, _) = self::app();
    typed(&mut app, "/scoped-models");
    app.handle_input("\r");
    assert!(shown(&mut app).contains("Model Configuration"), "{}", shown(&mut app));
    typed(&mut app, "abc");
    let (x, y) = cell_of(&mut app, "> abc");
    press(&mut app, (x + 2, y));
    typed(&mut app, "X");
    assert!(shown(&mut app).contains("> Xabc"), "{}", shown(&mut app));
}

/// `/bug`'s first step is an `ExtensionEditorComponent`: a click on its text moves the editor's cursor; its later
/// steps (text rows) take none.
#[test]
fn interactive_mouse_click_in_the_bug_report_editor_moves_its_cursor() {
    use tui::mouse::{MouseButton, MouseEvent, MouseEventType};
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let mut flow = interactive::bug_report::BugFlow::new(
        theme,
        keys(),
        keys(),
        Some("first line"),
        None,
        ("radius.pi.dev".into(), true),
    );
    let rows: Vec<String> = flow.render(W, H).iter().map(|l| strip_terminal_sequences(l)).collect();
    let y = rows.iter().position(|l| l.contains("first line")).expect("the hint");
    let x = rows[y].find("first line").unwrap() + 5;
    let click = MouseEvent {
        kind: MouseEventType::Click,
        button: MouseButton::Left,
        x: x as i64,
        y: y as i64,
        screen_x: x as i64,
        screen_y: y as i64,
        width: W,
        height: rows.len(),
        shift: false,
        alt: false,
        ctrl: false,
        wheel_delta: None,
        click_count: Some(1),
    };
    assert!(flow.handle_mouse(&click, H).is_some());
    flow.handle_input("X");
    let rows: Vec<String> = flow.render(W, H).iter().map(|l| strip_terminal_sequences(l)).collect();
    assert!(rows.iter().any(|l| l.contains("firstX line")), "{rows:#?}");
    let above = MouseEvent { y: 0, ..click };
    assert_eq!(flow.handle_mouse(&above, H), None, "the border is nobody's");
}
