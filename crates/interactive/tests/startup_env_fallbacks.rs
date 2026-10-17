//! `runClientTui`'s hardware cursor and clear-on-shrink (client-tui.ts): `settingsManager.getShowHardwareCursor()`
//! and `getClearOnShrink()`, whose unset settings fall back to `PI_HARDWARE_CURSOR=1` / `PI_CLEAR_ON_SHRINK=1`
//! (settings-manager.ts; the getters' TypeScript values are recorded in crates/config/fixtures/worker-settings.json).
//! Literal expectations through `App::apply_startup_settings`, which the interactive command calls at startup.

use std::rc::Rc;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, ModelInfo};
use interactive::qol::QolSettings;
use serde_json::{Value, json};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

struct Silent;

impl LaneClient for Silent {
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
    fn list_all_sessions(&self) {}
    fn list_pi_sessions(&self) {}
    fn list_all_pi_sessions(&self) {}
    fn set_name(&self, _: &str) {}
    fn select_model(&self, _: &str, _: &str) {}
    fn tree(&self) {}
    fn fork_messages(&self) {}
    fn save_setting(&self, _: &str, _: Value) {}
    fn share(&self, _: Option<&str>) {}
    fn cancel_share(&self) {}
    fn save_credential(&self, _: &str, _: Value) {}
    fn login_reply(&self, _: Option<String>) {}
    fn rlm_status(&self) {}
    fn rlm_inspect(&self, _: &str, _: &str, _: Value) {}
    fn read_settings(&self) {}
}

fn app() -> App {
    let keys = || {
        KeybindingsManager::new(
            native_app_keybindings(Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        )
    };
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "a".into(),
        name: "Model a".into(),
        reasoning: true,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    App::new(
        AppOptions {
            theme: Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap()),
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
            qol: QolSettings::parity(),
        },
        Box::new(Silent),
    )
}

fn applied(settings: Value, env: &[(&str, &str)]) -> (bool, bool) {
    let mut app = app();
    let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    app.apply_startup_settings(&settings, &|key| {
        env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    });
    (app.show_hardware_cursor, app.clear_on_shrink)
}

#[test]
fn interactive_startup_hardware_cursor_and_clear_on_shrink_fall_back_to_the_pi_env_vars() {
    let on = [("PI_HARDWARE_CURSOR", "1"), ("PI_CLEAR_ON_SHRINK", "1")];
    assert_eq!(applied(json!({}), &[]), (false, false));
    assert_eq!(applied(json!({}), &on), (true, true));
    // Only "1" turns them on.
    assert_eq!(
        applied(
            json!({}),
            &[("PI_HARDWARE_CURSOR", "true"), ("PI_CLEAR_ON_SHRINK", "yes")]
        ),
        (false, false)
    );
    assert_eq!(applied(json!({}), &[("PI_HARDWARE_CURSOR", "1")]), (true, false));
    assert_eq!(applied(json!({}), &[("PI_CLEAR_ON_SHRINK", "1")]), (false, true));
    // A setting wins over the environment, either way.
    let off = json!({"showHardwareCursor": false, "terminal": {"clearOnShrink": false}});
    assert_eq!(applied(off, &on), (false, false));
    let set = json!({"showHardwareCursor": true, "terminal": {"clearOnShrink": true}});
    assert_eq!(applied(set, &[]), (true, true));
}
