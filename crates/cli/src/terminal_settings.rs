//! The terminal settings the TUIs apply before they paint (interactive-mode.ts and startup-ui.ts call
//! `setCapabilityOverrides(settingsManager.getTerminalCapabilityOverrides())`): the terminal's capabilities with
//! `terminal.trueColor`, `terminal.hyperlinks` and `terminal.images` forced over the detected ones, and the platform
//! facts the keybindings and clipboard read from the run's environment.

use config::settings::SettingsManager;
use tui::capability_overrides::{CapabilityOverrides, capabilities as with_overrides};
use tui::image::{Capabilities, ImageProtocol};

use crate::Env;

/// The settings' overrides in the TUI's terms.
pub fn overrides(settings: &SettingsManager) -> CapabilityOverrides {
    let o = settings.terminal_capability_overrides();
    CapabilityOverrides {
        images: o.images.map(|protocol| match protocol {
            Some("kitty") => Some(ImageProtocol::Kitty),
            Some("iterm2") => Some(ImageProtocol::Iterm2),
            _ => None,
        }),
        true_color: o.true_color,
        hyperlinks: o.hyperlinks,
    }
}

/// `getCapabilities()` for this run: its environment, the tmux probe ([`tmux_forwards_hyperlinks`], run only
/// inside tmux when neither `PI_HYPERLINKS` nor `terminal.hyperlinks` decides), and the settings' overrides.
pub fn capabilities(env: &Env, settings: &SettingsManager) -> Capabilities {
    capabilities_with(env, settings, &tmux_forwards_hyperlinks)
}

/// [`capabilities`] with `probe` as the tmux probe.
pub fn capabilities_with(env: &Env, settings: &SettingsManager, probe: &dyn Fn(&Env) -> bool) -> Capabilities {
    let overrides = overrides(settings);
    let var = |k: &str| (env.var)(k);
    let in_tmux =
        var("TMUX").is_some_and(|v| !v.is_empty()) || var("TERM").is_some_and(|t| t.to_lowercase().starts_with("tmux"));
    let decided = matches!(var("PI_HYPERLINKS").as_deref(), Some("1" | "0")) || overrides.hyperlinks.is_some();
    let tmux = in_tmux && !decided && probe(env);
    with_overrides(&var, false, tmux, &overrides)
}

/// What `probeTmuxHyperlinks` reads: whether `client_termfeatures` lists `hyperlinks`.
pub fn termfeatures_list_hyperlinks(termfeatures: &str) -> bool {
    termfeatures.split(',').any(|feature| feature.trim() == "hyperlinks")
}

/// The variables the tmux probe passes on (the client finds its server through `TMUX` / `TMUX_TMPDIR`).
const TMUX_PROBE_ENV: [&str; 8] = [
    "PATH",
    "HOME",
    "TMUX",
    "TMUX_TMPDIR",
    "TMPDIR",
    "TERM",
    "LANG",
    "LC_ALL",
];
/// `execSync(..., { timeout: 250 })`.
const TMUX_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// `probeTmuxHyperlinks`: `tmux display-message -p '#{client_termfeatures}'` (tmux from the run's PATH only, with
/// a cleared environment but the variables above), at most 250 ms; any failure is `false`.
pub fn tmux_forwards_hyperlinks(env: &Env) -> bool {
    tmux_forwards_hyperlinks_within(env, TMUX_PROBE_TIMEOUT)
}

/// [`tmux_forwards_hyperlinks`] with its time limit.
pub fn tmux_forwards_hyperlinks_within(env: &Env, timeout: std::time::Duration) -> bool {
    use std::process::{Command, Stdio};
    let Some(path) = (env.var)("PATH") else {
        return false;
    };
    let Some(tmux) = std::env::split_paths(&path)
        .map(|dir| dir.join("tmux"))
        .find(|p| p.is_file())
    else {
        return false;
    };
    let mut command = Command::new(tmux);
    command
        .args(["display-message", "-p", "#{client_termfeatures}"])
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for name in TMUX_PROBE_ENV {
        if let Some(value) = (env.var)(name) {
            command.env(name, value);
        }
    }
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut text);
        let _ = tx.send(text);
    });
    let Ok(text) = rx.recv_timeout(timeout) else {
        // Our own child only.
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    match child.wait() {
        Ok(status) if status.success() => termfeatures_list_hyperlinks(&text),
        _ => false,
    }
}

/// `useWindowsKeybindings()` on Linux: WSL, named by `WSL_DISTRO_NAME` or `WSL_INTEROP` (core/keybindings.ts).
pub fn wsl_keybindings(env: &Env) -> bool {
    let set = |name: &str| (env.var)(name).is_some_and(|v| !v.is_empty());
    set("WSL_DISTRO_NAME") || set("WSL_INTEROP")
}

/// `isWindowsTerminalSession()` (keys.ts): `WT_SESSION` set and none of `SSH_CONNECTION`, `SSH_CLIENT`, `SSH_TTY`;
/// a raw BS byte is then Ctrl+Backspace.
pub fn windows_terminal_session(env: &Env) -> bool {
    let set = |name: &str| (env.var)(name).is_some_and(|v| !v.is_empty());
    set("WT_SESSION") && !set("SSH_CONNECTION") && !set("SSH_CLIENT") && !set("SSH_TTY")
}

/// The key-matching state the TUI starts with: Kitty off until the terminal confirms it, Windows Terminal from the
/// environment.
pub fn key_context(env: &Env) -> tui::keys::KeyContext {
    tui::keys::KeyContext {
        kitty_active: false,
        windows_terminal: windows_terminal_session(env),
    }
}

/// The keybinding platform of this run.
pub fn platform(env: &Env) -> tui::keybindings::Platform {
    tui::keybindings::Platform::new("linux", wsl_keybindings(env))
}
