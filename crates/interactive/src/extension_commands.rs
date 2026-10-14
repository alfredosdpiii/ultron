//! Extension commands as slash commands of their own (`directExtensionCommand` of slash-commands-provider.ts): each
//! command the session's extensions registered, unless its name is taken by a built-in or is not a valid slash
//! command name (`/extension <name>` still runs it), described as it registered, else "Extension command".

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use tui::autocomplete::{ArgumentCompletions, AutocompleteItem};

use crate::commands::COMMANDS;

/// `SlashCommandRegistry` name validation: `^[a-z0-9][a-z0-9:-]*$`.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == ':' || c == '-')
}

/// The extension commands (invocation name, description) that become slash commands, with their descriptions, in
/// order.
pub fn direct_extension_commands(commands: &[(String, Option<String>)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, description) in commands {
        if !valid_name(name) || COMMANDS.iter().any(|c| c.name == name) || out.iter().any(|(n, _)| n == name) {
            continue;
        }
        let description = description.clone().unwrap_or_else(|| "Extension command".into());
        out.push((name.clone(), description));
    }
    out
}

/// Every slash command in the editor's order as `(name, description, argument hint)`: the client TUI's commands,
/// with the extension commands that are slash commands of their own right after `/extension` (the order
/// slash-commands-provider.ts registers them in, which puts them before the built-in MCP extension's `/mcp`).
pub fn ordered(extension: &[(String, Option<String>)]) -> Vec<(String, String, Option<String>)> {
    let mut out: Vec<(String, String, Option<String>)> = COMMANDS
        .iter()
        .map(|c| {
            (
                c.name.to_string(),
                c.description.to_string(),
                (!c.hint.is_empty()).then(|| c.hint.to_string()),
            )
        })
        .collect();
    let at = out
        .iter()
        .position(|(name, _, _)| name == "extension")
        .map_or(out.len(), |i| i + 1);
    let direct = direct_extension_commands(extension)
        .into_iter()
        .map(|(name, description)| (name, description, None));
    out.splice(at..at, direct);
    out
}

/// Extension command argument completions (`commands/complete`), fetched off the UI thread: the editor asks
/// synchronously, so a prefix not answered yet shows nothing and is queued; its answer is kept, and the editor asks
/// again ([`tui::editor::Editor::refresh_autocomplete`]).
#[derive(Clone, Default)]
pub struct Completions(Rc<RefCell<CompletionState>>);

#[derive(Default)]
struct CompletionState {
    answered: HashMap<(String, String), Option<Vec<AutocompleteItem>>>,
    wanted: Vec<(String, String)>,
}

impl Completions {
    /// The editor's completion callback of `/name`.
    pub fn for_command(&self, name: &str) -> ArgumentCompletions {
        let (state, name) = (self.0.clone(), name.to_string());
        Box::new(move |prefix: &str| {
            let key = (name.clone(), prefix.to_string());
            let mut state = state.borrow_mut();
            if let Some(answer) = state.answered.get(&key) {
                return answer.clone();
            }
            if !state.wanted.contains(&key) {
                state.wanted.push(key);
            }
            None
        })
    }

    /// The `(name, prefix)` pairs to ask for, each once.
    pub fn take_wanted(&self) -> Vec<(String, String)> {
        let mut state = self.0.borrow_mut();
        let wanted = std::mem::take(&mut state.wanted);
        for key in &wanted {
            // Asked: a repeat waits for this answer instead of asking again.
            state.answered.entry(key.clone()).or_insert(None);
        }
        wanted
    }

    /// An answer arrived.
    pub fn answer(&self, name: &str, prefix: &str, items: Option<Vec<(String, String, Option<String>)>>) {
        let items = items.map(|items| {
            items
                .into_iter()
                .map(|(value, label, description)| AutocompleteItem {
                    value,
                    label,
                    description,
                })
                .collect()
        });
        self.0
            .borrow_mut()
            .answered
            .insert((name.to_string(), prefix.to_string()), items);
    }
}
