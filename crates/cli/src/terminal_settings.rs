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

/// `getCapabilities()` for this run: its environment, the tmux probe answered "no" (the Rust TUI does not probe
/// tmux), and the settings' overrides.
pub fn capabilities(env: &Env, settings: &SettingsManager) -> Capabilities {
    with_overrides(&|k| (env.var)(k), false, false, &overrides(settings))
}

/// `useWindowsKeybindings()` on Linux: WSL, named by `WSL_DISTRO_NAME` or `WSL_INTEROP` (core/keybindings.ts).
pub fn wsl_keybindings(env: &Env) -> bool {
    let set = |name: &str| (env.var)(name).is_some_and(|v| !v.is_empty());
    set("WSL_DISTRO_NAME") || set("WSL_INTEROP")
}

/// The keybinding platform of this run.
pub fn platform(env: &Env) -> tui::keybindings::Platform {
    tui::keybindings::Platform::new("linux", wsl_keybindings(env))
}
