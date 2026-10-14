//! `/scoped-models` and Ctrl+P over the session scope it sets, in a real terminal (tmux): replays
//! `fixtures/scoped_models.json`, recorded from the TypeScript client TUI by `fixtures/scoped_models.mjs` (open,
//! toggle, clear all, filter and enable the filtered models, close, cycle twice inside the scope, reopen, save to
//! settings). Every stable screen and the settings.json it leaves must match. Offline (PI_OFFLINE=1).

mod tui_harness;

use serde_json::Value;
use tui_harness::{TempDir, Tui, normalize};

#[test]
fn cli_scoped_models_selector_and_cycling_like_ts() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/scoped_models.json")).unwrap();
    let scenario = &fixture["scenarios"][0];
    let tmp = TempDir::new();
    let tui = Tui::start_at(
        &tmp.0,
        9,
        fixture["cols"].as_u64().unwrap(),
        fixture["rows"].as_u64().unwrap(),
        &["--model", "anthropic/claude-sonnet-4-5"],
        &[
            ("ULTRON_AUTO_NAME", "off"),
            ("ULTRON_TUI_PARITY", "1"),
            ("ULTRON_SPLASH", "off"),
            ("PI_OFFLINE", "1"),
        ],
    );
    let root = tmp.0.display().to_string();
    let mut screens = Vec::new();
    for step in scenario["steps"].as_array().unwrap() {
        if let Some(text) = step["type"].as_str() {
            tui.send_text(text);
        }
        if let Some(keys) = step["keys"].as_array() {
            let keys: Vec<&str> = keys.iter().map(|k| k.as_str().unwrap()).collect();
            tui.send_keys(&keys);
        }
        let mut screen = tui.wait(step["wait"].as_str().unwrap());
        if let Some(gone) = step["gone"].as_str() {
            while screen.contains(gone) {
                screen = tui.wait_change(&screen);
            }
        }
        screens.push(normalize(&screen, &root));
    }
    let expected: Vec<&str> = scenario["captures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert_eq!(screens.len(), expected.len());
    for (index, (got, want)) in screens.iter().zip(&expected).enumerate() {
        assert_eq!(got, want, "step {}", index + 1);
    }
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(tmp.0.join("agent/settings.json")).unwrap()).unwrap();
    let want: Value = serde_json::from_str(scenario["artifacts"]["agent/settings.json"].as_str().unwrap()).unwrap();
    assert_eq!(settings, want, "settings.json");
}
