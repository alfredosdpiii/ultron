//! Pi's dialogs in the editor's place: the extension text input (`ExtensionInputComponent`, with its countdown), the
//! multi-line extension editor (`ExtensionEditorComponent`, Ctrl+G opens the external editor) and the bordered
//! loader (`BorderedLoader`, Esc cancels) of modes/interactive/components.

use std::rc::Rc;
use std::time::Instant;

use tui::components::{Component, Text};
use tui::editor::Editor;
use tui::input::Input;
use tui::keybindings::KeybindingsManager;
use tui::select::Loader;
use tui::theme::Theme;

use crate::components::{fg, key_hint, key_text};

/// What a key did to a dialog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialogOutcome {
    None,
    Submit(String),
    Cancel,
    /// Ctrl+G in the editor dialog: edit this text in the external editor.
    External(String),
}

fn border(theme: &Theme, width: usize) -> String {
    fg(theme, "border", &"─".repeat(width.max(1)))
}

fn hint(theme: &Theme, keys: &KeybindingsManager, id: &str, description: &str) -> String {
    key_hint(theme, &key_text(keys, id, false), description)
}

/// `ExtensionInputComponent`: a one-line input under an accent title; with a timeout the title counts the seconds
/// down and the dialog cancels itself when they run out (`CountdownTimer`).
pub struct ExtensionInputDialog {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    title: String,
    input: Input,
    /// `(seconds, started)`: `Math.ceil(timeout / 1000)` and when the countdown began.
    countdown: Option<(u64, Instant)>,
}

impl ExtensionInputDialog {
    /// `placeholder` is accepted and not shown, as in TypeScript; `timeout_ms` > 0 starts the countdown at `now`.
    pub fn new(
        theme: Rc<Theme>,
        keys: KeybindingsManager,
        title: &str,
        timeout_ms: Option<u64>,
        initial: Option<&str>,
        now: Instant,
    ) -> Self {
        let mut input = Input::new(keys.clone());
        input.focused = true;
        if let Some(value) = initial.filter(|v| !v.is_empty()) {
            input.set_value(value);
        }
        ExtensionInputDialog {
            theme,
            keys,
            title: title.to_string(),
            input,
            countdown: timeout_ms.filter(|ms| *ms > 0).map(|ms| (ms.div_ceil(1000), now)),
        }
    }

    /// Seconds left at `now` (`remainingSeconds`), when counting down.
    pub fn remaining(&self, now: Instant) -> Option<u64> {
        self.countdown
            .map(|(seconds, started)| seconds.saturating_sub(now.saturating_duration_since(started).as_secs()))
    }

    /// Whether the countdown ran out at `now` (the dialog then cancels).
    pub fn expired(&self, now: Instant) -> bool {
        self.remaining(now) == Some(0)
    }

    pub fn counting(&self) -> bool {
        self.countdown.is_some()
    }

    pub fn handle_input(&mut self, data: &str) -> DialogOutcome {
        if self.keys.matches(data, "tui.select.confirm") || data == "\n" {
            return DialogOutcome::Submit(self.input.value().to_string());
        }
        if self.keys.matches(data, "tui.select.cancel") {
            return DialogOutcome::Cancel;
        }
        self.input.handle_input(data);
        DialogOutcome::None
    }

    /// `ExtensionInputComponent.handleMouse` (a `Container`): a press on the input's row moves its cursor.
    pub fn handle_mouse(&mut self, event: &tui::mouse::MouseEvent, now: Instant) -> Option<tui::mouse::MouseResult> {
        let title = match self.remaining(now) {
            Some(seconds) => format!("{} ({seconds}s)", self.title),
            None => self.title.clone(),
        };
        let t = Rc::clone(&self.theme);
        let top = 2
            + Text::new(&fg(&t, "accent", &title), 1, 0, None)
                .render(event.width)
                .len() as i64
            + 1;
        let rows = self.input.render(event.width).len() as i64;
        if event.y < top || event.y >= top + rows {
            return None;
        }
        let local = tui::mouse::MouseEvent {
            y: event.y - top,
            ..*event
        };
        self.input.handle_mouse(&local)
    }

    pub fn render(&mut self, width: usize, now: Instant) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let title = match self.remaining(now) {
            Some(seconds) => format!("{} ({seconds}s)", self.title),
            None => self.title.clone(),
        };
        let mut lines = vec![border(&t, width), String::new()];
        lines.extend(Text::new(&fg(&t, "accent", &title), 1, 0, None).render(width));
        lines.push(String::new());
        lines.extend(self.input.render(width));
        lines.push(String::new());
        let hints = format!(
            "{}  {}",
            hint(&t, &self.keys, "tui.select.confirm", "submit"),
            hint(&t, &self.keys, "tui.select.cancel", "cancel")
        );
        lines.extend(Text::new(&hints, 1, 0, None).render(width));
        lines.push(String::new());
        lines.push(border(&t, width));
        lines
    }
}

/// `ExtensionEditorComponent`: a multi-line editor under an accent title; Enter submits, Shift+Enter adds a line,
/// Esc cancels, Ctrl+G edits the text in the external editor.
pub struct ExtensionEditorDialog {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    title: String,
    /// `options.description`, under the title.
    description: Option<String>,
    editor: Editor,
}

impl ExtensionEditorDialog {
    /// `editor_keys` drive the editor; `keys` the dialog's own (`tui.select.cancel`, `app.editor.external`).
    pub fn new(
        theme: Rc<Theme>,
        keys: KeybindingsManager,
        editor_keys: KeybindingsManager,
        title: &str,
        prefill: Option<&str>,
    ) -> Self {
        let t = Rc::clone(&theme);
        let mut editor = Editor::new(
            editor_keys,
            Default::default(),
            Box::new(move |s| fg(&t, "borderMuted", s)),
            0,
        );
        editor.focused = true;
        if let Some(prefill) = prefill.filter(|p| !p.is_empty()) {
            editor.set_text(prefill);
        }
        ExtensionEditorDialog {
            theme,
            keys,
            title: title.to_string(),
            description: None,
            editor,
        }
    }

    /// The dialog with `options.description` under its title (`/bug`'s consent text).
    pub fn with_description(mut self, description: &str) -> Self {
        self.description = (!description.is_empty()).then(|| description.to_string());
        self
    }

    pub fn text(&self) -> String {
        self.editor.text()
    }

    /// The external editor's result replaces the text.
    pub fn set_text(&mut self, text: &str) {
        self.editor.set_text(text);
    }

    pub fn handle_input(&mut self, data: &str) -> DialogOutcome {
        if self.keys.matches(data, "tui.select.cancel") {
            return DialogOutcome::Cancel;
        }
        if self.keys.matches(data, "app.editor.external") {
            return DialogOutcome::External(self.editor.text());
        }
        match self.editor.handle_input(data) {
            Some(text) => DialogOutcome::Submit(text),
            None => DialogOutcome::None,
        }
    }

    /// `ExtensionEditorComponent.handleMouse` (a `Container`): the editor's rows go to the `Editor` (a click moves
    /// its cursor, its autocomplete rows pick).
    pub fn handle_mouse(&mut self, event: &tui::mouse::MouseEvent, rows: usize) -> Option<tui::mouse::MouseResult> {
        let t = Rc::clone(&self.theme);
        let width = event.width;
        let mut top = 2 + Text::new(&fg(&t, "accent", &self.title), 1, 0, None)
            .render(width)
            .len() as i64;
        if let Some(description) = &self.description {
            top += 1 + Text::new(&fg(&t, "text", description), 1, 0, None).render(width).len() as i64;
        }
        top += 1;
        self.editor.terminal_rows = rows;
        let height = self.editor.render(width).len();
        if event.y < top || event.y >= top + height as i64 {
            return None;
        }
        let local = tui::mouse::MouseEvent {
            y: event.y - top,
            height,
            ..*event
        };
        self.editor.handle_mouse(&local)
    }

    pub fn render(&mut self, width: usize, rows: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let mut lines = vec![border(&t, width), String::new()];
        lines.extend(Text::new(&fg(&t, "accent", &self.title), 1, 0, None).render(width));
        if let Some(description) = &self.description {
            lines.push(String::new());
            lines.extend(Text::new(&fg(&t, "text", description), 1, 0, None).render(width));
        }
        lines.push(String::new());
        self.editor.terminal_rows = rows;
        lines.extend(self.editor.render(width));
        lines.push(String::new());
        let hints = format!(
            "{}  {}  {}  {}",
            hint(&t, &self.keys, "tui.select.confirm", "submit"),
            hint(&t, &self.keys, "tui.input.newLine", "newline"),
            hint(&t, &self.keys, "tui.select.cancel", "cancel"),
            hint(&t, &self.keys, "app.editor.external", "external editor")
        );
        lines.extend(Text::new(&hints, 1, 0, None).render(width));
        lines.push(String::new());
        lines.push(border(&t, width));
        lines
    }
}

/// `BorderedLoader` (cancellable): a spinner and message between borders, with the cancel key's hint.
pub struct BorderedLoader {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    loader: Loader,
}

impl BorderedLoader {
    pub fn new(theme: Rc<Theme>, keys: KeybindingsManager, message: &str) -> Self {
        let (t1, t2) = (Rc::clone(&theme), Rc::clone(&theme));
        let loader = Loader::new(
            Box::new(move |s| fg(&t1, "accent", s)),
            Box::new(move |s| fg(&t2, "muted", s)),
            message,
            None,
        );
        BorderedLoader { theme, keys, loader }
    }

    pub fn tick(&mut self) {
        self.loader.tick();
    }

    /// Whether this key aborts (`CancellableLoader`: the cancel key).
    pub fn handle_input(&mut self, data: &str) -> bool {
        self.keys.matches(data, "tui.select.cancel")
    }

    pub fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let mut lines = vec![border(&t, width)];
        lines.extend(self.loader.render(width));
        lines.push(String::new());
        lines.extend(Text::new(&hint(&t, &self.keys, "tui.select.cancel", "cancel"), 1, 0, None).render(width));
        lines.push(String::new());
        lines.push(border(&t, width));
        lines
    }
}
