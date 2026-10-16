//! `getShortcuts` of runner.ts: which extension shortcuts the TUI binds, given the resolved keybindings.
//!
//! A key bound to one of the reserved editor-global actions (interrupt, clear, exit, submit, ...) is never given to an
//! extension. A key bound to another built-in action is, with a warning. When two extensions register the same key the
//! later one wins, with a warning. Keys compare lowercased.

/// Built-in actions extensions may never take a key from (`RESERVED_KEYBINDINGS_FOR_EXTENSION_CONFLICTS`).
pub const RESERVED: [&str; 18] = [
    "app.interrupt",
    "app.clear",
    "app.exit",
    "app.suspend",
    "app.thinking.cycle",
    "app.model.cycleForward",
    "app.model.cycleBackward",
    "app.model.select",
    "app.tools.expand",
    "app.thinking.toggle",
    "app.editor.external",
    "app.message.copy",
    "app.message.followUp",
    "tui.input.submit",
    "tui.select.confirm",
    "tui.select.cancel",
    "tui.input.copy",
    "tui.editor.deleteToLineEnd",
];

/// One extension shortcut: the key as registered, its description and the extension that registered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shortcut {
    pub key: String,
    pub description: Option<String>,
    pub extension_path: String,
}

/// The shortcuts in force, keyed by the lowercased key in first-registration order, and the conflict warnings
/// (`getShortcutDiagnostics`) as (message, extension path).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shortcuts {
    pub shortcuts: Vec<(String, Shortcut)>,
    pub diagnostics: Vec<(String, String)>,
}

impl Shortcuts {
    pub fn get(&self, key: &str) -> Option<&Shortcut> {
        let key = key.to_lowercase();
        self.shortcuts.iter().find(|(k, _)| *k == key).map(|(_, s)| s)
    }
}

/// `buildBuiltinKeybindings`: each bound key's action, a reserved action winning a key several actions share.
fn builtin(keybindings: &[(String, Vec<String>)]) -> Vec<(String, String, bool)> {
    let mut out: Vec<(String, String, bool)> = Vec::new();
    for (action, keys) in keybindings {
        let restrict = RESERVED.contains(&action.as_str());
        for key in keys {
            let key = key.to_lowercase();
            match out.iter_mut().find(|(k, _, _)| *k == key) {
                Some(existing) if existing.2 && !restrict => {}
                Some(existing) => {
                    existing.1 = action.clone();
                    existing.2 = restrict;
                }
                None => out.push((key, action.clone(), restrict)),
            }
        }
    }
    out
}

/// One extension's shortcuts as it registered them: (extension path, [(key, description)]).
pub type Registered = (String, Vec<(String, Option<String>)>);

/// `getShortcuts`: `extensions` are (extension path, shortcuts as registered) in load order; `keybindings` are the
/// resolved keybindings (action, keys) in the configuration's order. A shortcut an extension lists twice keeps its
/// first place with its last description (`Map.set`).
pub fn resolve(extensions: &[Registered], keybindings: &[(String, Vec<String>)]) -> Shortcuts {
    let builtin = builtin(keybindings);
    let mut out = Shortcuts::default();
    for (path, registered) in extensions {
        let mut own: Vec<(String, Option<String>)> = Vec::new();
        for (key, description) in registered {
            match own.iter_mut().find(|(k, _)| k == key) {
                Some(slot) => slot.1 = description.clone(),
                None => own.push((key.clone(), description.clone())),
            }
        }
        for (key, description) in own {
            let normalized = key.to_lowercase();
            let shortcut = Shortcut {
                key: key.clone(),
                description,
                extension_path: path.clone(),
            };
            let mut warn = |message: String| out.diagnostics.push((message, path.clone()));
            match builtin.iter().find(|(k, _, _)| *k == normalized) {
                Some((_, _, true)) => {
                    warn(format!(
                        "Extension shortcut '{key}' from {path} conflicts with built-in shortcut. Skipping."
                    ));
                    continue;
                }
                Some((_, action, false)) => warn(format!(
                    "Extension shortcut conflict: '{key}' is built-in shortcut for {action} and {path}. Using {path}."
                )),
                None => {}
            }
            match out.shortcuts.iter_mut().find(|(k, _)| *k == normalized) {
                Some(slot) => {
                    let previous = slot.1.extension_path.clone();
                    slot.1 = shortcut;
                    out.diagnostics.push((
                        format!(
                            "Extension shortcut conflict: '{key}' registered by both {previous} and {path}. Using {path}."
                        ),
                        path.clone(),
                    ));
                }
                None => out.shortcuts.push((normalized, shortcut)),
            }
        }
    }
    out
}
