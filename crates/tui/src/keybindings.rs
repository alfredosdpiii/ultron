//! Keybinding registry (port of packages/tui/src/keybindings.ts) and the default editor and app bindings
//! (`TUI_KEYBINDINGS`, and `KEYBINDINGS` from packages/coding-agent/src/core/keybindings.ts).

use crate::keys::{KeyContext, matches_key};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeybindingDefinition {
    pub id: &'static str,
    pub default_keys: Vec<String>,
    pub description: &'static str,
}

fn def(id: &'static str, keys: &[&str], description: &'static str) -> KeybindingDefinition {
    KeybindingDefinition {
        id,
        default_keys: keys.iter().map(|k| (*k).to_string()).collect(),
        description,
    }
}

/// Platform facts the TS module reads from `process.platform` and the WSL environment.
#[derive(Clone, Copy, Debug, Default)]
pub struct Platform {
    pub win32: bool,
    pub darwin: bool,
    /// `useWindowsKeybindings()`: win32, or linux under WSL.
    pub windows_keybindings: bool,
}

impl Platform {
    pub fn new(platform: &str, wsl: bool) -> Self {
        Platform {
            win32: platform == "win32",
            darwin: platform == "darwin",
            windows_keybindings: platform == "win32" || (platform == "linux" && wsl),
        }
    }
}

/// `TUI_KEYBINDINGS`, in declaration order (the default editor bindings).
pub fn tui_keybindings() -> Vec<KeybindingDefinition> {
    vec![
        def("tui.editor.cursorUp", &["up"], "Move cursor up"),
        def("tui.editor.cursorDown", &["down"], "Move cursor down"),
        def(
            "tui.editor.historyPrevious",
            &[],
            "Select previous prompt history entry",
        ),
        def("tui.editor.historyNext", &[], "Select next prompt history entry"),
        def("tui.editor.cursorLeft", &["left", "ctrl+b"], "Move cursor left"),
        def("tui.editor.cursorRight", &["right", "ctrl+f"], "Move cursor right"),
        def(
            "tui.editor.cursorWordLeft",
            &["alt+left", "ctrl+left", "alt+b"],
            "Move cursor word left",
        ),
        def(
            "tui.editor.cursorWordRight",
            &["alt+right", "ctrl+right", "alt+f"],
            "Move cursor word right",
        ),
        def(
            "tui.editor.cursorLineStart",
            &["home", "ctrl+home", "ctrl+a"],
            "Move to line start",
        ),
        def(
            "tui.editor.cursorLineEnd",
            &["end", "ctrl+end", "ctrl+e"],
            "Move to line end",
        ),
        def("tui.editor.jumpForward", &["ctrl+]"], "Jump forward to character"),
        def("tui.editor.jumpBackward", &["ctrl+alt+]"], "Jump backward to character"),
        def("tui.editor.pageUp", &["pageUp", "ctrl+pageUp"], "Page up"),
        def("tui.editor.pageDown", &["pageDown", "ctrl+pageDown"], "Page down"),
        def(
            "tui.editor.deleteCharBackward",
            &["backspace"],
            "Delete character backward",
        ),
        def(
            "tui.editor.deleteCharForward",
            &["delete", "ctrl+d"],
            "Delete character forward",
        ),
        def(
            "tui.editor.deleteWordBackward",
            &["ctrl+w", "alt+backspace"],
            "Delete word backward",
        ),
        def(
            "tui.editor.deleteWordForward",
            &["alt+d", "alt+delete"],
            "Delete word forward",
        ),
        def("tui.editor.deleteToLineStart", &["ctrl+u"], "Delete to line start"),
        def("tui.editor.deleteToLineEnd", &["ctrl+k"], "Delete to line end"),
        def("tui.editor.yank", &["ctrl+y"], "Yank"),
        def("tui.editor.yankPop", &["alt+y"], "Yank pop"),
        def("tui.editor.undo", &["ctrl+-"], "Undo"),
        def("tui.input.newLine", &["shift+enter", "ctrl+j"], "Insert newline"),
        def("tui.input.submit", &["enter"], "Submit input"),
        def("tui.input.tab", &["tab"], "Tab / autocomplete"),
        def("tui.input.copy", &["ctrl+c"], "Copy selection"),
        def("tui.select.up", &["up"], "Move selection up"),
        def("tui.select.down", &["down"], "Move selection down"),
        def("tui.select.pageUp", &["pageUp"], "Selection page up"),
        def("tui.select.pageDown", &["pageDown"], "Selection page down"),
        def("tui.select.confirm", &["enter"], "Confirm selection"),
        def("tui.select.cancel", &["escape", "ctrl+c"], "Cancel selection"),
        def("tui.altScreen.pageUp", &["pageUp"], "Scroll viewport up one page"),
        def("tui.altScreen.pageDown", &["pageDown"], "Scroll viewport down one page"),
        def("tui.altScreen.halfPageUp", &[], "Scroll viewport up half a page"),
        def("tui.altScreen.halfPageDown", &[], "Scroll viewport down half a page"),
        def("tui.altScreen.lineUp", &[], "Scroll viewport up one line"),
        def("tui.altScreen.lineDown", &[], "Scroll viewport down one line"),
        def(
            "tui.altScreen.previousPrompt",
            &["ctrl+shift+up", "ctrl+up"],
            "Jump to previous semantic prompt",
        ),
        def(
            "tui.altScreen.nextPrompt",
            &["ctrl+shift+down", "ctrl+down"],
            "Jump to next semantic prompt",
        ),
        def(
            "tui.altScreen.search",
            &["ctrl+shift+f"],
            "Search the primary scroll view",
        ),
        def(
            "tui.altScreen.searchNext",
            &["enter", "ctrl+g"],
            "Select the next search match",
        ),
        def(
            "tui.altScreen.searchPrevious",
            &["shift+enter", "ctrl+shift+g"],
            "Select the previous search match",
        ),
        def("tui.altScreen.searchClose", &["escape"], "Close transcript search"),
        def("tui.altScreen.top", &["home"], "Scroll viewport to top"),
        def("tui.altScreen.bottom", &["end"], "Scroll viewport to bottom"),
    ]
}

/// `KEYBINDINGS` for a platform: the TUI bindings (some overridden in place), then the app bindings.
pub fn app_keybindings(p: Platform) -> Vec<KeybindingDefinition> {
    let overrides: [(&str, &[&str]); 4] = [
        (
            "tui.editor.undo",
            if p.win32 {
                &["ctrl+z"]
            } else if p.windows_keybindings {
                &["alt+z"]
            } else {
                &["ctrl+-"]
            },
        ),
        (
            "tui.altScreen.previousPrompt",
            if p.windows_keybindings {
                &["ctrl+up"]
            } else {
                &["ctrl+shift+up", "ctrl+up"]
            },
        ),
        (
            "tui.altScreen.nextPrompt",
            if p.windows_keybindings {
                &["ctrl+down"]
            } else {
                &["ctrl+shift+down", "ctrl+down"]
            },
        ),
        (
            "tui.altScreen.search",
            if p.windows_keybindings {
                &["ctrl+f"]
            } else {
                &["ctrl+shift+f"]
            },
        ),
    ];
    let mut defs = tui_keybindings();
    for (id, keys) in overrides {
        if let Some(d) = defs.iter_mut().find(|d| d.id == id) {
            d.default_keys = keys.iter().map(|k| (*k).to_string()).collect();
        }
    }
    defs.extend([
        def("app.interrupt", &["escape"], "Cancel or abort"),
        def("app.clear", &["ctrl+c"], "Clear editor"),
        def("app.exit", &["ctrl+d"], "Exit when editor is empty"),
        def(
            "app.suspend",
            if p.win32 { &[] } else { &["ctrl+z"] },
            "Suspend to background",
        ),
        def("app.thinking.cycle", &["shift+tab"], "Cycle thinking level"),
        def("app.thinking.save", &["ctrl+s"], "Save thinking level"),
        def("app.model.cycleForward", &["ctrl+p"], "Cycle to next model"),
        def(
            "app.model.cycleBackward",
            if p.windows_keybindings {
                &["alt+p"]
            } else {
                &["shift+ctrl+p"]
            },
            "Cycle to previous model",
        ),
        def("app.model.select", &["ctrl+l"], "Open model selector"),
        def("app.tools.expand", &["ctrl+o"], "Toggle tool output"),
        def("app.thinking.toggle", &["ctrl+t"], "Toggle thinking blocks"),
        def("app.rlm.toggle", &["ctrl+r"], "Toggle the live RLM panel (native TUI)"),
        def(
            "app.rlm.focus",
            &["alt+g"],
            "Open the full-screen RLM graph (native TUI)",
        ),
        def("app.rlm.graph.up", &["up", "k"], "RLM graph: select the previous node"),
        def("app.rlm.graph.down", &["down", "j"], "RLM graph: select the next node"),
        def("app.rlm.graph.pageUp", &["pageUp"], "RLM graph: page up"),
        def("app.rlm.graph.pageDown", &["pageDown"], "RLM graph: page down"),
        def(
            "app.rlm.graph.details",
            &["enter"],
            "RLM graph: show or hide the node's details",
        ),
        def(
            "app.rlm.graph.collapse",
            &["c", "space"],
            "RLM graph: collapse or expand the subtree",
        ),
        def(
            "app.rlm.graph.exit",
            &["escape", "q"],
            "RLM graph: leave the focus view",
        ),
        def(
            "app.rlm.pane",
            &["alt+w"],
            "Open the RLM pane, or move focus between it and the chat (native TUI)",
        ),
        def("app.rlm.pane.next", &["tab", "n"], "RLM pane: select the next node"),
        def(
            "app.rlm.pane.prev",
            &["shift+tab", "p"],
            "RLM pane: select the previous node",
        ),
        def(
            "app.rlm.pane.fold",
            &["space", "enter"],
            "RLM pane: fold or unfold the node's box",
        ),
        def(
            "app.rlm.pane.details",
            &["d"],
            "RLM pane: show or hide the node detail cards",
        ),
        def("app.rlm.pane.scrollUp", &["up"], "RLM pane: scroll up"),
        def("app.rlm.pane.scrollDown", &["down"], "RLM pane: scroll down"),
        def("app.rlm.pane.pageUp", &["pageUp"], "RLM pane: page up"),
        def("app.rlm.pane.pageDown", &["pageDown"], "RLM pane: page down"),
        def("app.rlm.pane.prevRun", &["left"], "RLM pane: previous run"),
        def("app.rlm.pane.nextRun", &["right"], "RLM pane: next run"),
        def("app.rlm.pane.close", &["q", "escape"], "RLM pane: close the pane"),
        def(
            "app.history.search",
            &["alt+r"],
            "Reverse-search prompt history (native TUI)",
        ),
        def(
            "app.session.toggleNamedFilter",
            &["ctrl+n"],
            "Toggle named session filter",
        ),
        def("app.editor.external", &["ctrl+g"], "Open external editor"),
        def(
            "app.message.copy",
            &["ctrl+x"],
            "Copy selection or last assistant message",
        ),
        def(
            "app.message.followUp",
            if p.windows_keybindings {
                &["ctrl+q"]
            } else {
                &["alt+enter"]
            },
            "Queue follow-up message",
        ),
        def(
            "app.message.dequeue",
            if p.windows_keybindings { &["alt+q"] } else { &["alt+up"] },
            "Restore queued messages",
        ),
        def(
            "app.clipboard.pasteImage",
            if p.windows_keybindings { &["alt+v"] } else { &["ctrl+v"] },
            "Paste image from clipboard (text fallback)",
        ),
        def("app.session.new", &[], "Start a new session"),
        def("app.session.tree", &[], "Open session tree"),
        def("app.session.fork", &[], "Fork current session"),
        def("app.session.resume", &[], "Resume a session"),
        def(
            "app.tree.foldOrUp",
            if p.darwin {
                &["alt+left", "ctrl+left"]
            } else {
                &["ctrl+left", "alt+left"]
            },
            "Fold tree branch or move up",
        ),
        def(
            "app.tree.unfoldOrDown",
            if p.darwin {
                &["alt+right", "ctrl+right"]
            } else {
                &["ctrl+right", "alt+right"]
            },
            "Unfold tree branch or move down",
        ),
        def("app.tree.editLabel", &["shift+l"], "Edit tree label"),
        def(
            "app.tree.toggleLabelTimestamp",
            &["shift+t"],
            "Toggle tree label timestamps",
        ),
        def("app.session.togglePath", &["ctrl+p"], "Toggle session path display"),
        def("app.session.toggleSort", &["ctrl+s"], "Toggle session sort mode"),
        def("app.session.rename", &["ctrl+r"], "Rename session"),
        def("app.session.delete", &["ctrl+d"], "Delete session"),
        def(
            "app.session.deleteNoninvasive",
            &["ctrl+backspace"],
            "Delete session when query is empty",
        ),
        def("app.models.save", &["ctrl+s"], "Save model selection"),
        def("app.models.enableAll", &["ctrl+a"], "Enable all models"),
        def("app.models.clearAll", &["ctrl+x"], "Clear all models"),
        def(
            "app.models.toggleProvider",
            &["ctrl+p"],
            "Toggle all models for provider",
        ),
        def("app.models.reorderUp", &["alt+up"], "Move model up in order"),
        def("app.models.reorderDown", &["alt+down"], "Move model down in order"),
        def("app.tree.filter.default", &["ctrl+d"], "Tree filter: default view"),
        def("app.tree.filter.noTools", &["ctrl+t"], "Tree filter: hide tool results"),
        def(
            "app.tree.filter.userOnly",
            &["ctrl+u"],
            "Tree filter: user messages only",
        ),
        def(
            "app.tree.filter.labeledOnly",
            &["ctrl+l"],
            "Tree filter: labeled entries only",
        ),
        def("app.tree.filter.all", &["ctrl+a"], "Tree filter: show all entries"),
        def(
            "app.tree.filter.cycleForward",
            &["ctrl+o"],
            "Tree filter: cycle forward",
        ),
        def(
            "app.tree.filter.cycleBackward",
            &["shift+ctrl+o"],
            "Tree filter: cycle backward",
        ),
    ]);
    defs
}

/// A key claimed by more than one keybinding in the user config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeybindingConflict {
    pub key: String,
    pub keybindings: Vec<String>,
}

fn dedup(keys: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for k in keys {
        if !out.contains(k) {
            out.push(k.clone());
        }
    }
    out
}

/// `KeybindingsManager`: resolves user overrides over definitions and matches input against bindings.
/// User bindings are ordered `(id, keys)`; `None` keys mean "use the default" (TS `undefined`).
#[derive(Clone)]
pub struct KeybindingsManager {
    definitions: Vec<KeybindingDefinition>,
    user: Vec<(String, Option<Vec<String>>)>,
    keys_by_id: Vec<(&'static str, Vec<String>)>,
    conflicts: Vec<KeybindingConflict>,
    ctx: KeyContext,
}

impl KeybindingsManager {
    pub fn new(
        definitions: Vec<KeybindingDefinition>,
        user: Vec<(String, Option<Vec<String>>)>,
        ctx: KeyContext,
    ) -> Self {
        let mut m = KeybindingsManager {
            definitions,
            user,
            keys_by_id: Vec::new(),
            conflicts: Vec::new(),
            ctx,
        };
        m.rebuild();
        m
    }

    fn rebuild(&mut self) {
        let mut claims: Vec<(String, Vec<String>)> = Vec::new();
        for (id, keys) in &self.user {
            if !self.definitions.iter().any(|d| d.id == id) {
                continue;
            }
            for key in dedup(keys.as_deref().unwrap_or_default()) {
                match claims.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, ids)) if !ids.contains(id) => ids.push(id.clone()),
                    Some(_) => {}
                    None => claims.push((key, vec![id.clone()])),
                }
            }
        }
        self.conflicts = claims
            .into_iter()
            .filter(|(_, ids)| ids.len() > 1)
            .map(|(key, keybindings)| KeybindingConflict { key, keybindings })
            .collect();
        self.keys_by_id = self
            .definitions
            .iter()
            .map(|d| {
                let user = self
                    .user
                    .iter()
                    .find(|(id, _)| id == d.id)
                    .and_then(|(_, k)| k.as_ref());
                (d.id, dedup(user.unwrap_or(&d.default_keys)))
            })
            .collect();
    }

    pub fn set_kitty_active(&mut self, active: bool) {
        self.ctx.kitty_active = active;
    }

    /// The key-matching state this manager matches with.
    pub fn ctx(&self) -> KeyContext {
        self.ctx
    }

    pub fn matches(&self, data: &str, keybinding: &str) -> bool {
        self.get_keys(keybinding).iter().any(|k| matches_key(data, k, self.ctx))
    }

    pub fn get_keys(&self, keybinding: &str) -> Vec<String> {
        self.keys_by_id
            .iter()
            .find(|(id, _)| *id == keybinding)
            .map(|(_, k)| k.clone())
            .unwrap_or_default()
    }

    pub fn get_definition(&self, keybinding: &str) -> Option<&KeybindingDefinition> {
        self.definitions.iter().find(|d| d.id == keybinding)
    }

    pub fn get_conflicts(&self) -> Vec<KeybindingConflict> {
        self.conflicts.clone()
    }

    pub fn set_user_bindings(&mut self, user: Vec<(String, Option<Vec<String>>)>) {
        self.user = user;
        self.rebuild();
    }

    /// `getResolvedBindings`: every definition's keys, in definition order.
    pub fn get_resolved_bindings(&self) -> Vec<(&'static str, Vec<String>)> {
        self.keys_by_id.clone()
    }
}
