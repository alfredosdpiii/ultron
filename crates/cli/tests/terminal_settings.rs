//! The terminal settings the TUIs apply: replays crates/tui/fixtures/capability-overrides.json (terminal-image.ts
//! `getCapabilities()` after `setCapabilityOverrides(getTerminalCapabilityOverrides())`, recorded by
//! crates/tui/fixtures/generate-capability-overrides.mjs) through a settings file and `cli::terminal_settings`, and
//! `useWindowsKeybindings()`'s WSL detection.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use cli::Env;
use config::settings::SettingsManager;
use serde_json::{Value, json};
use tui::image::ImageProtocol;

fn env_with(vars: HashMap<String, String>, home: PathBuf) -> Env {
    Env {
        cwd: home.clone(),
        home,
        var: Arc::new(move |k| vars.get(k).cloned()),
        docs: PathBuf::new(),
    }
}

#[test]
fn cli_terminal_settings_override_detected_capabilities_like_ts() {
    let fixture: Value = serde_json::from_str(include_str!("../../tui/fixtures/capability-overrides.json")).unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("settings.json"), case["settings"].to_string()).unwrap();
        let settings = SettingsManager::create(dir.path(), &agent, false);
        let vars: HashMap<String, String> = case["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
            .collect();
        let caps = cli::terminal_settings::capabilities(&env_with(vars, dir.path().to_path_buf()), &settings);
        let images = match caps.images {
            Some(ImageProtocol::Kitty) => json!("kitty"),
            Some(ImageProtocol::Iterm2) => json!("iterm2"),
            None => Value::Null,
        };
        assert_eq!(
            json!({"images": images, "trueColor": caps.true_color, "hyperlinks": caps.hyperlinks}),
            case["capabilities"],
            "{case}"
        );
    }
}

/// core/keybindings.ts `useWindowsKeybindings`: on Linux, `WSL_DISTRO_NAME` or `WSL_INTEROP` (non-empty) selects
/// the Windows-style defaults (e.g. `alt+z` undo, `alt+v` image paste).
#[test]
fn cli_terminal_settings_wsl_interop_selects_windows_keybindings() {
    let home = PathBuf::from("/nonexistent");
    let env = |pairs: &[(&str, &str)]| {
        env_with(
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            home.clone(),
        )
    };
    assert!(!cli::terminal_settings::wsl_keybindings(&env(&[])));
    assert!(!cli::terminal_settings::wsl_keybindings(&env(&[("WSL_INTEROP", "")])));
    assert!(cli::terminal_settings::wsl_keybindings(&env(&[(
        "WSL_INTEROP",
        "/run/WSL/1_interop"
    )])));
    assert!(cli::terminal_settings::wsl_keybindings(&env(&[(
        "WSL_DISTRO_NAME",
        "Ubuntu"
    )])));
    // WSLENV alone is the clipboard's WSL test (utils/wsl.ts), not the keybindings'.
    assert!(!cli::terminal_settings::wsl_keybindings(&env(&[("WSLENV", "x")])));
    let paste = |platform| {
        tui::keybindings::KeybindingsManager::new(
            tui::qol_keys::native_app_keybindings(platform),
            Vec::new(),
            Default::default(),
        )
        .get_keys("app.clipboard.pasteImage")
    };
    assert_eq!(
        paste(cli::terminal_settings::platform(&env(&[(
            "WSL_INTEROP",
            "/run/WSL/1_interop"
        )]))),
        vec!["alt+v".to_string()]
    );
    assert_eq!(
        paste(cli::terminal_settings::platform(&env(&[]))),
        vec!["ctrl+v".to_string()]
    );
}
