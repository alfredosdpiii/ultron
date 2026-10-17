//! Right-click paste in fullscreen on Windows terminals (`handleRightClickPaste` of packages/tui/src/tui-alt-screen.ts
//! and of modes/interactive/interactive-mode.ts). Fullscreen captures the mouse, so a Windows console's own
//! right-click paste never happens; a right-button press the viewport would otherwise give to text selection pastes
//! the clipboard's text instead, as a bracketed paste into the focused editor. VS Code's terminal (`TERM_PROGRAM`
//! `vscode`, any case) keeps its own context menu, and every other platform keeps selection.

/// Whether right-click paste is on (`process.platform === "win32"` and `TERM_PROGRAM` not `vscode`); `platform` is
/// the Node platform name ([`crate::clipboard_copy::platform`]) and `var` reads the environment.
pub fn right_click_paste_enabled(platform: &str, var: &dyn Fn(&str) -> Option<String>) -> bool {
    platform == "win32" && var("TERM_PROGRAM").is_none_or(|program| program.to_lowercase() != "vscode")
}

/// An SGR mouse report that pastes: a press (not a release) of exactly button 2, so no modifier, motion or wheel
/// bits.
pub fn is_right_click_press(button: u32, release: bool) -> bool {
    !release && button == 2
}
