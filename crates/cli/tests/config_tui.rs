//! `ultron config` in a real terminal (tmux): each test replays a scenario of `fixtures/config_tui.json`, recorded
//! from the TypeScript resource selector by `fixtures/config_tui.mjs`, and compares every screen and the settings
//! files the run leaves (local resources only: Pi packages are not supported by ultron-rs).

mod tui_harness;

use serde_json::Value;
use tui_harness::{TempDir, Tui};

fn fixture() -> &'static Value {
    static FIXTURE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    FIXTURE.get_or_init(|| serde_json::from_str(include_str!("../fixtures/config_tui.json")).unwrap())
}

fn replay(name: &str) {
    let fixture = fixture();
    let scenario = fixture["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == name)
        .unwrap();
    let tmp = TempDir::new();
    for (rel, text) in scenario["files"].as_object().unwrap() {
        let path = tmp.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text.as_str().unwrap()).unwrap();
    }
    let args: Vec<&str> = scenario["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    let tui = Tui::start_at(
        &tmp.0,
        9,
        fixture["cols"].as_u64().unwrap(),
        fixture["rows"].as_u64().unwrap(),
        &args,
        &[("PATH", "/usr/bin:/bin"), ("PI_OFFLINE", "1")],
    );
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
        let want = scenario["captures"][screens.len()].as_str().unwrap_or_default();
        screens.push(tui.wait_screen(screen, want));
    }
    let expected: Vec<&str> = scenario["captures"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    assert_eq!(screens.len(), expected.len(), "{name}");
    for (index, (got, want)) in screens.iter().zip(&expected).enumerate() {
        assert_eq!(got, want, "{name} step {}", index + 1);
    }
    let root = tmp.0.display().to_string();
    for (rel, want) in scenario["artifacts"].as_object().unwrap() {
        let got = std::fs::read_to_string(tmp.0.join(rel))
            .ok()
            .map(|t| t.replace(&root, "<TMP>"));
        assert_eq!(got.as_deref(), want.as_str(), "{name}: {rel}");
    }
}

#[test]
fn cli_config_selector_global_mode_like_ts() {
    replay("global");
}

#[test]
fn cli_config_selector_project_mode_like_ts() {
    replay("project");
}

#[test]
fn cli_config_selector_untrusted_local_is_refused_like_ts() {
    replay("untrusted local");
}
