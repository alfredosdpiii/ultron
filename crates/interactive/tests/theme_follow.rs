//! Live theme follow (theme-controller.ts auto-sync, tui.ts color palette notifications): an automatic `light/dark`
//! pair turns on `CSI ? 2031 h`, asks `CSI ? 996 n`, and each `CSI ? 997 ; 1|2 n` report switches the theme, without
//! waiting for any answer. Ported from test/theme-controller.test.ts and packages/tui/test/terminal-colors.test.ts
//! (`parseTerminalColorSchemeReport`), with literal expectations.

use std::path::PathBuf;
use std::rc::Rc;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, ModelInfo};
use interactive::theme_setup::{ThemeController, parse_color_scheme_report};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::registry::ThemeRegistry;
use tui::theme::{ColorMode, TerminalTheme, Theme, builtin_theme_json, create_theme};

#[test]
fn interactive_theme_color_scheme_reports_parse_like_ts() {
    assert_eq!(parse_color_scheme_report("\x1b[?997;1n"), Some(TerminalTheme::Dark));
    assert_eq!(parse_color_scheme_report("\x1b[?997;2n"), Some(TerminalTheme::Light));
    assert_eq!(
        parse_color_scheme_report("\x1b[?997;2n\x1b[?997;1n\x1b[?997;1n"),
        Some(TerminalTheme::Dark)
    );
    assert_eq!(
        parse_color_scheme_report("\x1b[?997;1n\x1b[?997;2n\x1b[?997;2n"),
        Some(TerminalTheme::Light)
    );
    assert_eq!(parse_color_scheme_report("\x1b[?997;3n"), None);
    assert_eq!(parse_color_scheme_report("\x1b[?996n"), None);
    assert_eq!(parse_color_scheme_report("x\x1b[?997;1n"), None);
    assert_eq!(parse_color_scheme_report(""), None);
}

fn registry() -> ThemeRegistry {
    ThemeRegistry::new(
        PathBuf::new(),
        PathBuf::from("/nonexistent/themes"),
        ColorMode::Truecolor,
    )
}

fn builtin(name: &str) -> Theme {
    create_theme(&builtin_theme_json(name).unwrap(), ColorMode::Truecolor, None).unwrap()
}

/// Which built-in `theme` draws like.
fn which(theme: &Theme) -> &'static str {
    let probe = |t: &Theme| t.fg("accent", "x").unwrap();
    if probe(theme) == probe(&builtin("light")) {
        "light"
    } else if probe(theme) == probe(&builtin("dark")) {
        "dark"
    } else {
        "other"
    }
}

/// "resolves a theme pair and follows terminal appearance changes".
#[test]
fn interactive_theme_pair_follows_terminal_appearance_changes() {
    let mut controller = ThemeController::new(registry(), Some("light/dark".into()), Some("15;0"));
    let applied = controller.apply_from_settings();
    assert_eq!(which(&applied.theme), "dark", "COLORFGBG 15;0 is a dark background");
    assert!(applied.query_scheme);
    assert!(controller.auto_sync());
    let light = controller
        .terminal_scheme_changed(TerminalTheme::Light)
        .expect("switches");
    assert_eq!(which(&light), "light");
    assert_eq!(controller.terminal_theme(), "light");
    assert!(
        controller.terminal_scheme_changed(TerminalTheme::Light).is_none(),
        "the same side changes nothing"
    );
    let dark = controller
        .terminal_scheme_changed(TerminalTheme::Dark)
        .expect("switches back");
    assert_eq!(which(&dark), "dark");
}

/// A named theme does not follow; selecting a pair later does ("detects the current terminal appearance when
/// selecting a theme pair").
#[test]
fn interactive_theme_only_a_pair_syncs() {
    let mut controller = ThemeController::new(registry(), Some("dark".into()), None);
    let applied = controller.apply_from_settings();
    assert!(!applied.query_scheme);
    assert!(!controller.auto_sync());
    assert!(controller.terminal_scheme_changed(TerminalTheme::Light).is_none());
    let applied = controller.set_setting("light/dark");
    assert!(applied.query_scheme, "a pair asks for the current scheme");
    assert!(controller.auto_sync());
    let light = controller.terminal_scheme_changed(TerminalTheme::Light).unwrap();
    assert_eq!(which(&light), "light");
    let applied = controller.set_setting("dark");
    assert!(!controller.auto_sync());
    assert_eq!(which(&applied.theme), "dark");
    assert!(controller.terminal_scheme_changed(TerminalTheme::Light).is_none());
}

struct Lane;

impl LaneClient for Lane {
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
}

fn keys() -> KeybindingsManager {
    KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

fn app(theme: Rc<Theme>) -> App {
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "A".into(),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    App::new(
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
            thinking_level: "medium".into(),
            splash: false,
            history: PromptHistoryStore::new(None, "/p"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol: interactive::qol::QolSettings::parity(),
        },
        Box::new(Lane),
    )
}

/// The app's side: notifications on with the query, reports switch the shared theme and never reach the editor,
/// and the notifications go off when the setting stops being a pair and when the TUI stops ("disables terminal
/// appearance updates when disposed").
#[test]
fn interactive_theme_reports_switch_the_running_app() {
    let theme = Rc::new(builtin("dark"));
    let mut app = app(Rc::clone(&theme));
    app.set_theme_controller(ThemeController::new(
        registry(),
        Some("light/dark".into()),
        Some("15;0"),
    ));
    assert_eq!(std::mem::take(&mut app.terminal_writes), ["\x1b[?2031h", "\x1b[?996n"]);
    assert_eq!(which(&theme), "dark");
    app.handle_input("\x1b[?997;2n");
    assert_eq!(which(&theme), "light");
    app.handle_input("\x1b[?997;1n");
    assert_eq!(which(&theme), "dark");
    assert!(app.terminal_writes.is_empty(), "{:?}", app.terminal_writes);
    let editor: String = app
        .render(80, 30)
        .iter()
        .map(|l| tui::utils::strip_terminal_sequences(l))
        .collect();
    assert!(!editor.contains("997"), "a report is not typed into the editor");
    app.stop_terminal_notifications();
    assert_eq!(std::mem::take(&mut app.terminal_writes), ["\x1b[?2031l"]);
    app.stop_terminal_notifications();
    assert!(app.terminal_writes.is_empty(), "turned off once");
}

/// Without a pair the terminal is never asked and reports change nothing.
#[test]
fn interactive_theme_named_setting_sends_nothing() {
    let theme = Rc::new(builtin("dark"));
    let mut app = app(Rc::clone(&theme));
    app.set_theme_controller(ThemeController::new(registry(), Some("dark".into()), None));
    assert!(app.terminal_writes.is_empty(), "{:?}", app.terminal_writes);
    app.handle_input("\x1b[?997;2n");
    assert_eq!(which(&theme), "dark");
    app.stop_terminal_notifications();
    assert!(app.terminal_writes.is_empty());
}

/// The paint loop leaves the terminal as it found it: the notifications go off on exit (`TUI.stop`).
#[test]
fn interactive_theme_exit_turns_the_notifications_off() {
    let theme = Rc::new(builtin("dark"));
    let mut app = app(Rc::clone(&theme));
    app.set_theme_controller(ThemeController::new(
        registry(),
        Some("light/dark".into()),
        Some("15;0"),
    ));
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(interactive::run::UiEvent::InputClosed).unwrap();
    let mut written = String::new();
    interactive::run::paint_loop(
        &mut app,
        &rx,
        &mut |data| {
            written.push_str(data);
            Ok(())
        },
        &|| (80, 24),
        None,
    )
    .unwrap();
    let on = written.find("\x1b[?2031h").expect("turned on");
    let off = written.rfind("\x1b[?2031l").expect("turned off");
    assert!(on < off);
    assert_eq!(written.matches("\x1b[?2031l").count(), 1);
}

/// The startup check's answer (`showPackageUpdateNotification` only when packages came back). An answer that comes
/// before the first session opened is shown once it has (opening clears the transcript).
#[test]
fn interactive_package_updates_show_only_when_there_are_any() {
    let text = |app: &mut App| -> String {
        app.render(80, 30)
            .iter()
            .map(|l| tui::utils::strip_terminal_sequences(l))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let opened = || {
        interactive::lane::LaneEvent::Opened(interactive::lane::SessionOpened {
            session_id: "sess".into(),
            session_file: None,
            name: None,
            messages: Vec::new(),
        })
    };
    let mut late = app(Rc::new(builtin("dark")));
    late.apply(opened());
    late.apply(interactive::lane::LaneEvent::PackageUpdates(Vec::new()));
    assert!(!text(&mut late).contains("Package Updates Available"));
    late.apply(interactive::lane::LaneEvent::PackageUpdates(vec!["foo".into()]));
    let screen = text(&mut late);
    assert!(screen.contains("Package Updates Available"), "{screen}");
    assert!(screen.contains("- foo"), "{screen}");
    // Before the session opened: held, then shown with it.
    let mut early = app(Rc::new(builtin("dark")));
    early.apply(interactive::lane::LaneEvent::PackageUpdates(vec!["foo".into()]));
    early.apply(opened());
    let screen = text(&mut early);
    assert!(screen.contains("Package Updates Available"), "{screen}");
    assert!(screen.contains("- foo"), "{screen}");
}
