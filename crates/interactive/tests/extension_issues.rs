//! Extensions that did not load show in the startup `[Extension issues]` block, before the shortcut conflicts, as
//! interactive-mode.ts `showLoadedResources` lists `resourceLoader.getExtensions().errors` (type `error`, in the error
//! color) ahead of the runner's shortcut diagnostics (warnings). Literal expectations from `formatDiagnostics`.

use interactive::app::{App, AppOptions};
use interactive::extension_shortcuts::{ShortcutDiagnostic, issues_block};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
use std::rc::Rc;
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, Theme, builtin_theme_json, create_theme};

struct Quiet;

impl LaneClient for Quiet {
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

fn theme() -> Rc<Theme> {
    Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap())
}

fn issues() -> Vec<ShortcutDiagnostic> {
    vec![
        ShortcutDiagnostic {
            message: "Invalid extension manifest: \"command\" must be a non-empty string".into(),
            path: "/p/.pi/extensions/broken/extension.json".into(),
            error: true,
        },
        ShortcutDiagnostic {
            message:
                "Extension shortcut 'ctrl+c' from /x/ext/extension.json conflicts with built-in shortcut. Skipping."
                    .into(),
            path: "/x/ext/extension.json".into(),
            error: false,
        },
    ]
}

#[test]
fn interactive_extension_issues_color_load_errors_as_errors() {
    let theme = theme();
    let fg = |color: &str, text: &str| theme.fg(color, text).unwrap();
    assert_eq!(issues_block(&theme, &[]), None);
    assert_eq!(
        issues_block(&theme, &issues()).unwrap(),
        [
            fg("warning", "[Extension issues]"),
            fg("error", "  /p/.pi/extensions/broken/extension.json"),
            fg(
                "error",
                "    Invalid extension manifest: \"command\" must be a non-empty string"
            ),
            fg("warning", "  /x/ext/extension.json"),
            fg(
                "warning",
                "    Extension shortcut 'ctrl+c' from /x/ext/extension.json conflicts with built-in shortcut. Skipping."
            ),
        ]
        .join("\n")
    );
    assert_ne!(
        fg("error", "x"),
        fg("warning", "x"),
        "the two colors differ in this theme"
    );
}

#[test]
fn interactive_extension_load_errors_show_at_startup() {
    let keys = || {
        KeybindingsManager::new(
            native_app_keybindings(Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        )
    };
    let model = ModelInfo {
        provider: "p".into(),
        id: "m".into(),
        name: "m".into(),
        reasoning: false,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    let mut app = App::new(
        AppOptions {
            theme: theme(),
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
        Box::new(Quiet),
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: None,
        name: None,
        messages: Vec::new(),
    }));
    app.apply(LaneEvent::ExtensionShortcuts {
        shortcuts: Vec::new(),
        diagnostics: issues()[..1].to_vec(),
    });
    let shown: Vec<String> = app
        .render(100, 30)
        .iter()
        .map(|l| tui::utils::strip_terminal_sequences(l).trim_end().to_string())
        .collect();
    let at = shown
        .iter()
        .position(|l| l == " [Extension issues]")
        .unwrap_or_else(|| panic!("{}", shown.join("\n")));
    // A notice is drawn one column in.
    assert_eq!(shown[at + 1], "   /p/.pi/extensions/broken/extension.json");
    assert_eq!(
        shown[at + 2],
        "     Invalid extension manifest: \"command\" must be a non-empty string"
    );
}
