//! `/login` with an account for the built-in providers, in a real terminal (tmux): each test replays a scenario of
//! `fixtures/oauth_login_tui.json`, recorded from the TypeScript client TUI by `fixtures/oauth_login_tui.mjs`, up to
//! the flow's first question and then Esc: Anthropic (auth URL and paste prompt), OpenAI Codex (method choice),
//! GitHub Copilot (enterprise domain prompt) and OpenRouter (auth URL on its own callback port, paste prompt). No step
//! reaches a provider; browser logins listen on a system-chosen port (`ULTRON_OAUTH_CALLBACK_PORT=0`) and the
//! browser opener is a stub that only logs the address. Screens are normalized as the recording's are.

mod tui_harness;

use serde_json::Value;
use tui_harness::{TempDir, Tui, normalize};

fn fixture() -> &'static Value {
    static FIXTURE: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    FIXTURE.get_or_init(|| serde_json::from_str(include_str!("../fixtures/oauth_login_tui.json")).unwrap())
}

/// `maskUrl` of the generator: the authorize URL's lines as `<url>`.
fn mask_url(screen: &str) -> String {
    let mut lines: Vec<String> = screen.split('\n').map(str::to_string).collect();
    let Some(start) = lines.iter().position(|l| l.contains(" Login to ")) else {
        return screen.to_string();
    };
    let Some(end) = lines
        .iter()
        .enumerate()
        .position(|(i, l)| i > start && l.contains("Ctrl+click to open"))
    else {
        return screen.to_string();
    };
    for line in &mut lines[start + 1..end] {
        if !line.trim().is_empty() {
            *line = "<url>".into();
        }
    }
    lines.join("\n")
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
    let bin = tmp.0.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let opened = tmp.0.join("opened.log");
    let opener = bin.join("xdg-open");
    std::fs::write(&opener, format!("#!/bin/sh\necho \"$1\" >> '{}'\n", opened.display())).unwrap();
    std::fs::set_permissions(&opener, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let tui = Tui::start_at(
        &tmp.0,
        9,
        fixture["cols"].as_u64().unwrap(),
        fixture["rows"].as_u64().unwrap(),
        &["--model", "anthropic/claude-sonnet-4-5"],
        &[
            ("PATH", &path),
            ("ULTRON_AUTO_NAME", "off"),
            ("ULTRON_TUI_PARITY", "1"),
            ("ULTRON_SPLASH", "off"),
            ("PI_OFFLINE", "1"),
            ("ULTRON_OAUTH_CALLBACK_PORT", "0"),
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
        screens.push(mask_url(&normalize(&screen, &root)));
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
    let count = std::fs::read_to_string(&opened).map_or(0, |t| t.lines().count());
    assert_eq!(
        count as u64,
        scenario["opened"].as_u64().unwrap(),
        "{name}: browser opened"
    );
}

#[test]
fn cli_anthropic_account_login_dialog_like_ts() {
    replay("anthropic");
}

#[test]
fn cli_openai_codex_account_login_dialog_like_ts() {
    replay("openai-codex");
}

#[test]
fn cli_github_copilot_account_login_dialog_like_ts() {
    replay("github-copilot");
}

#[test]
fn cli_openrouter_account_login_dialog_like_ts() {
    replay("openrouter");
}
