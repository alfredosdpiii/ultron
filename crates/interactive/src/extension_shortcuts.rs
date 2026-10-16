//! Keyboard shortcuts extensions register (`pi.registerShortcut`), as interactive-mode.ts binds them: the session
//! resolves them under the TUI's effective keybindings (runner.ts `getShortcuts`: a reserved action's key is refused,
//! another built-in key or a key two extensions register goes to the later one, with a warning), the editor checks
//! them before any built-in binding (custom-editor.ts `handleInput`: `onExtensionShortcut` first), and a match runs the
//! handler in the session without blocking input; a failure shows `Shortcut handler error: <message>`. `/hotkeys`
//! lists them under **Extensions**, and the conflict warnings show as `[Extension issues]`.

use tui::keybindings::KeybindingsManager;
use tui::keys::KeyContext;
use tui::keys::matching::matches_key;

/// One shortcut in force: the key as registered, its description and the extension that registered it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionShortcut {
    pub key: String,
    pub description: Option<String>,
    pub extension_path: String,
}

/// An extension issue (`ResourceDiagnostic`): its message and the extension's path; `error` for an extension that
/// did not load (shown in the error color), else a warning such as a shortcut conflict.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShortcutDiagnostic {
    pub message: String,
    pub path: String,
    pub error: bool,
}

/// The effective keybindings (`getEffectiveConfig`): every action with its keys.
pub fn effective_keybindings(keys: &KeybindingsManager) -> Vec<(String, Vec<String>)> {
    keys.get_resolved_bindings()
        .into_iter()
        .map(|(action, keys)| (action.to_string(), keys))
        .collect()
}

/// The shortcut `data` is (`matchesKey` against each in order), if any.
pub fn matching<'a>(shortcuts: &'a [ExtensionShortcut], data: &str) -> Option<&'a ExtensionShortcut> {
    shortcuts
        .iter()
        .find(|s| matches_key(data, &s.key, KeyContext::default()))
}

/// The **Extensions** section `/hotkeys` appends (nothing without shortcuts): each key as `formatKeyText(key,
/// { capitalize: true })` with its description, else the extension's path.
pub fn hotkeys_section(shortcuts: &[ExtensionShortcut]) -> String {
    if shortcuts.is_empty() {
        return String::new();
    }
    let mut out = "\n**Extensions**\n| Key | Action |\n|-----|--------|\n".to_string();
    for shortcut in shortcuts {
        let description = shortcut.description.as_deref().unwrap_or(&shortcut.extension_path);
        let key = crate::components::format_key_text(&shortcut.key, true, false);
        out.push_str(&format!("| `{key}` | {description} |\n"));
    }
    out
}

/// The `[Extension issues]` block (interactive-mode.ts `showLoadedResources`) in `theme`: the heading in the warning
/// color, each issue's path and message (`formatDiagnostics`) in the error color for a load error, else the warning
/// color. `None` without any.
pub fn issues_block(theme: &tui::theme::Theme, diagnostics: &[ShortcutDiagnostic]) -> Option<String> {
    if diagnostics.is_empty() {
        return None;
    }
    let mut lines = vec![crate::components::fg(theme, "warning", "[Extension issues]")];
    for d in diagnostics {
        let color = if d.error { "error" } else { "warning" };
        lines.push(crate::components::fg(theme, color, &format!("  {}", d.path)));
        lines.push(crate::components::fg(theme, color, &format!("    {}", d.message)));
    }
    Some(lines.join("\n"))
}

/// The `[Extension issues]` block for the warnings (`formatDiagnostics`: the path, then the message under it), or
/// `None` without any. Lines are uncolored; the caller colors them as warnings.
pub fn issues(diagnostics: &[ShortcutDiagnostic]) -> Option<String> {
    if diagnostics.is_empty() {
        return None;
    }
    let mut lines = vec!["[Extension issues]".to_string()];
    for d in diagnostics {
        lines.push(format!("  {}", d.path));
        lines.push(format!("    {}", d.message));
    }
    Some(lines.join("\n"))
}
