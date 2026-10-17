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

fn env_of(pairs: &[(&str, &str)], home: PathBuf) -> Env {
    env_with(
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        home,
    )
}

/// keys.ts `isWindowsTerminalSession`: `WT_SESSION` set and no SSH variable; the TUI's key context carries it.
#[test]
fn cli_terminal_settings_windows_terminal_session_needs_wt_session_without_ssh() {
    let home = PathBuf::from("/nonexistent");
    let wt = |pairs: &[(&str, &str)]| cli::terminal_settings::key_context(&env_of(pairs, home.clone()));
    assert!(wt(&[("WT_SESSION", "abc")]).windows_terminal);
    assert!(!wt(&[("WT_SESSION", "abc")]).kitty_active);
    assert!(!wt(&[]).windows_terminal);
    assert!(!wt(&[("WT_SESSION", "")]).windows_terminal);
    for ssh in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"] {
        assert!(!wt(&[("WT_SESSION", "abc"), (ssh, "x")]).windows_terminal, "{ssh}");
        assert!(wt(&[("WT_SESSION", "abc"), (ssh, "")]).windows_terminal, "{ssh}");
    }
}

/// A fake `tmux` in its own directory: prints `features`, records its arguments and environment.
fn fake_tmux(dir: &std::path::Path, script_body: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let tmux = bin.join("tmux");
    std::fs::write(&tmux, format!("#!/bin/sh\n{script_body}\n")).unwrap();
    std::fs::set_permissions(&tmux, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    bin
}

/// terminal-image.ts `probeTmuxHyperlinks`: `tmux display-message -p '#{client_termfeatures}'` lists `hyperlinks`;
/// tmux comes from the run's PATH only; failures and a missing tmux are `false`.
#[test]
fn cli_terminal_settings_tmux_probe_reads_client_termfeatures() {
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("record");
    let bin = fake_tmux(
        dir.path(),
        &format!(
            "printf '%s|' \"$@\" > {r}; printf 'TMUX=%s DISPLAY=%s' \"$TMUX\" \"$DISPLAY\" >> {r}; echo '256,RGB, hyperlinks ,title'",
            r = record.display()
        ),
    );
    let path = bin.display().to_string();
    let env = env_of(
        &[("PATH", &path), ("TMUX", "/tmp/fake-socket,1,0"), ("DISPLAY", ":9")],
        dir.path().to_path_buf(),
    );
    let hang = Duration::from_secs(120);
    assert!(cli::terminal_settings::tmux_forwards_hyperlinks_within(&env, hang));
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        "display-message|-p|#{client_termfeatures}|TMUX=/tmp/fake-socket,1,0 DISPLAY="
    );
    let bin = fake_tmux(dir.path(), "echo '256,RGB,title'");
    let env = env_of(&[("PATH", &bin.display().to_string())], dir.path().to_path_buf());
    assert!(!cli::terminal_settings::tmux_forwards_hyperlinks_within(&env, hang));
    let bin = fake_tmux(dir.path(), "echo hyperlinks; exit 1");
    let env = env_of(&[("PATH", &bin.display().to_string())], dir.path().to_path_buf());
    assert!(!cli::terminal_settings::tmux_forwards_hyperlinks_within(&env, hang));
    // No tmux on the run's PATH.
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let env = env_of(&[("PATH", &empty.display().to_string())], dir.path().to_path_buf());
    assert!(!cli::terminal_settings::tmux_forwards_hyperlinks_within(&env, hang));
    assert!(!cli::terminal_settings::tmux_forwards_hyperlinks_within(
        &env_of(&[], dir.path().to_path_buf()),
        hang
    ));
    // A tmux that never answers is given up on (`timeout`): `exec` keeps the child the process we spawned.
    let bin = fake_tmux(dir.path(), "exec sleep 600");
    let env = env_of(
        &[("PATH", &format!("{}:/usr/bin:/bin", bin.display()))],
        dir.path().to_path_buf(),
    );
    assert!(!cli::terminal_settings::tmux_forwards_hyperlinks_within(
        &env,
        Duration::from_millis(50)
    ));
    assert!(cli::terminal_settings::termfeatures_list_hyperlinks("hyperlinks"));
    assert!(!cli::terminal_settings::termfeatures_list_hyperlinks("hyperlinks2,RGB"));
}

/// `detectCapabilities`: the tmux probe runs only inside tmux (`TMUX`, or `TERM` starting with tmux) when neither
/// `PI_HYPERLINKS` nor `terminal.hyperlinks` decides, and its answer is the hyperlinks capability.
#[test]
fn cli_terminal_settings_capabilities_probe_tmux_only_when_undecided() {
    let dir = tempfile::tempdir().unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let probes = std::cell::Cell::new(0);
    let probe = |_: &Env| {
        probes.set(probes.get() + 1);
        true
    };
    let caps = |pairs: &[(&str, &str)], settings: Value| {
        std::fs::write(agent.join("settings.json"), settings.to_string()).unwrap();
        let manager = SettingsManager::create(dir.path(), &agent, false);
        cli::terminal_settings::capabilities_with(&env_of(pairs, dir.path().to_path_buf()), &manager, &probe)
    };
    assert!(caps(&[("TMUX", "/s,1,0")], json!({})).hyperlinks);
    assert_eq!(probes.get(), 1);
    assert!(caps(&[("TERM", "tmux-256color")], json!({})).hyperlinks);
    assert_eq!(probes.get(), 2);
    assert!(!caps(&[("TMUX", "/s,1,0"), ("PI_HYPERLINKS", "0")], json!({})).hyperlinks);
    assert!(caps(&[("TMUX", "/s,1,0"), ("PI_HYPERLINKS", "1")], json!({})).hyperlinks);
    assert!(!caps(&[("TMUX", "/s,1,0")], json!({"terminal": {"hyperlinks": false}})).hyperlinks);
    assert!(!caps(&[("TERM", "xterm-256color")], json!({})).hyperlinks);
    assert_eq!(probes.get(), 2);
}
