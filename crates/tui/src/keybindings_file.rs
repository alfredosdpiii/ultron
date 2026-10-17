//! `<agentDir>/keybindings.json` (packages/coding-agent/src/core/keybindings.ts): `KeybindingsManager.create()` reads
//! it, migrates legacy names (`exit` -> `app.exit`; a legacy name loses to its new name when both are present),
//! orders it (definitions first, the rest sorted) and keeps only string and string-array bindings; `reload()` reads it
//! again. A missing, unreadable or malformed file is no user bindings.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::keybindings::KeybindingDefinition;

/// The file name under the agent directory.
pub const KEYBINDINGS_FILE: &str = "keybindings.json";

/// User bindings as [`crate::keybindings::KeybindingsManager`] takes them.
pub type UserBindings = Vec<(String, Option<Vec<String>>)>;

/// `KEYBINDING_NAME_MIGRATIONS`: legacy name -> keybinding id.
pub const KEYBINDING_NAME_MIGRATIONS: [(&str, &str); 59] = [
    ("cursorUp", "tui.editor.cursorUp"),
    ("cursorDown", "tui.editor.cursorDown"),
    ("cursorLeft", "tui.editor.cursorLeft"),
    ("cursorRight", "tui.editor.cursorRight"),
    ("cursorWordLeft", "tui.editor.cursorWordLeft"),
    ("cursorWordRight", "tui.editor.cursorWordRight"),
    ("cursorLineStart", "tui.editor.cursorLineStart"),
    ("cursorLineEnd", "tui.editor.cursorLineEnd"),
    ("jumpForward", "tui.editor.jumpForward"),
    ("jumpBackward", "tui.editor.jumpBackward"),
    ("pageUp", "tui.editor.pageUp"),
    ("pageDown", "tui.editor.pageDown"),
    ("deleteCharBackward", "tui.editor.deleteCharBackward"),
    ("deleteCharForward", "tui.editor.deleteCharForward"),
    ("deleteWordBackward", "tui.editor.deleteWordBackward"),
    ("deleteWordForward", "tui.editor.deleteWordForward"),
    ("deleteToLineStart", "tui.editor.deleteToLineStart"),
    ("deleteToLineEnd", "tui.editor.deleteToLineEnd"),
    ("yank", "tui.editor.yank"),
    ("yankPop", "tui.editor.yankPop"),
    ("undo", "tui.editor.undo"),
    ("newLine", "tui.input.newLine"),
    ("submit", "tui.input.submit"),
    ("tab", "tui.input.tab"),
    ("copy", "tui.input.copy"),
    ("selectUp", "tui.select.up"),
    ("selectDown", "tui.select.down"),
    ("selectPageUp", "tui.select.pageUp"),
    ("selectPageDown", "tui.select.pageDown"),
    ("selectConfirm", "tui.select.confirm"),
    ("selectCancel", "tui.select.cancel"),
    ("interrupt", "app.interrupt"),
    ("clear", "app.clear"),
    ("exit", "app.exit"),
    ("suspend", "app.suspend"),
    ("cycleThinkingLevel", "app.thinking.cycle"),
    ("cycleModelForward", "app.model.cycleForward"),
    ("cycleModelBackward", "app.model.cycleBackward"),
    ("selectModel", "app.model.select"),
    ("expandTools", "app.tools.expand"),
    ("toggleThinking", "app.thinking.toggle"),
    ("toggleSessionNamedFilter", "app.session.toggleNamedFilter"),
    ("externalEditor", "app.editor.external"),
    ("followUp", "app.message.followUp"),
    ("dequeue", "app.message.dequeue"),
    ("pasteImage", "app.clipboard.pasteImage"),
    ("newSession", "app.session.new"),
    ("tree", "app.session.tree"),
    ("fork", "app.session.fork"),
    ("resume", "app.session.resume"),
    ("treeFoldOrUp", "app.tree.foldOrUp"),
    ("treeUnfoldOrDown", "app.tree.unfoldOrDown"),
    ("treeEditLabel", "app.tree.editLabel"),
    ("treeToggleLabelTimestamp", "app.tree.toggleLabelTimestamp"),
    ("toggleSessionPath", "app.session.togglePath"),
    ("toggleSessionSort", "app.session.toggleSort"),
    ("renameSession", "app.session.rename"),
    ("deleteSession", "app.session.delete"),
    ("deleteSessionNoninvasive", "app.session.deleteNoninvasive"),
];

fn migrated_name(key: &str) -> &str {
    KEYBINDING_NAME_MIGRATIONS
        .iter()
        .find(|(legacy, _)| *legacy == key)
        .map_or(key, |(_, id)| id)
}

/// `migrateKeybindingsConfig`: legacy names renamed (dropped when the new name is also present), ordered by
/// `order` (the definitions' ids) and then the remaining keys sorted; `true` when anything was renamed or dropped.
pub fn migrate_keybindings_config(raw: &Map<String, Value>, order: &[&str]) -> (Map<String, Value>, bool) {
    let mut config = Map::new();
    let mut migrated = false;
    for (key, value) in raw {
        let next = migrated_name(key);
        if next != key {
            migrated = true;
            if raw.contains_key(next) {
                continue;
            }
        }
        config.insert(next.to_string(), value.clone());
    }
    let mut ordered = Map::new();
    for id in order {
        if let Some(value) = config.get(*id) {
            ordered.insert((*id).to_string(), value.clone());
        }
    }
    let mut extras: Vec<&String> = config.keys().filter(|k| !ordered.contains_key(*k)).collect();
    extras.sort();
    for key in extras {
        ordered.insert(key.clone(), config[key].clone());
    }
    (ordered, migrated)
}

/// `toKeybindingsConfig`: a string binding is one key, an all-string array its keys; anything else is dropped.
pub fn to_user_bindings(config: &Map<String, Value>) -> UserBindings {
    config
        .iter()
        .filter_map(|(id, binding)| {
            let keys = match binding {
                Value::String(key) => vec![key.clone()],
                Value::Array(keys) => keys
                    .iter()
                    .map(|k| k.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()?,
                _ => return None,
            };
            Some((id.clone(), Some(keys)))
        })
        .collect()
}

/// `loadRawConfig`: the parsed file (BOM stripped) when it is a JSON object or array (an array's entries are keyed
/// by index, as `Object.entries` keys them); otherwise nothing.
fn load_raw(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(text.strip_prefix('\u{feff}').unwrap_or(&text)).ok()? {
        Value::Object(map) => Some(map),
        Value::Array(items) => Some(items.into_iter().enumerate().map(|(i, v)| (i.to_string(), v)).collect()),
        _ => None,
    }
}

/// `KeybindingsManager.loadFromFile`: the user bindings `path` holds, migrated and ordered by `definitions`.
pub fn load_user_bindings(path: &Path, definitions: &[KeybindingDefinition]) -> UserBindings {
    let Some(raw) = load_raw(path) else {
        return Vec::new();
    };
    let order: Vec<&str> = definitions.iter().map(|d| d.id).collect();
    to_user_bindings(&migrate_keybindings_config(&raw, &order).0)
}

/// `<agentDir>/keybindings.json`.
pub fn keybindings_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(KEYBINDINGS_FILE)
}
