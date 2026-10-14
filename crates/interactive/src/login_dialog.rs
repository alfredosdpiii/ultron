//! Pi's login dialog (`LoginDialogComponent`, modes/interactive/components/login-dialog.ts) and the select prompt a
//! login shows over it (`ExtensionSelectorComponent`), as client-tui-auth.ts `runProviderLogin` drives them: the
//! auth URL or device code a provider's flow reports, its waiting and progress lines, and the text, secret or
//! paste-the-code prompts it asks. The flow itself runs in the client process (the composition root); the dialog
//! only shows its steps and returns what the user answered.

use crate::components::{bold, fg, key_hint, key_text};
use crate::lane::{LoginEvent, LoginPrompt};
use std::rc::Rc;
use tui::components::{Component, Text};
use tui::keybindings::KeybindingsManager;
use tui::theme::Theme;

/// `UNREACHABLE_CALLBACK_STEP`.
const UNREACHABLE_CALLBACK_STEP: &str = "copy that page's full address from the address bar and paste it here.";

/// `unreachableCallbackHint`: what to do when the browser cannot load the login's localhost callback page.
pub fn unreachable_callback_hint(reach: &str) -> String {
    if reach == "local" {
        return format!(
            "If the browser ends on a page that can't be reached (localhost refused to connect), {UNREACHABLE_CALLBACK_STEP}"
        );
    }
    let place = if reach == "container" {
        "in a container"
    } else {
        "over SSH"
    };
    format!(
        "Ultron is running {place}, so the browser on your computer may not reach this login's callback. If it ends on a page that can't be reached (localhost refused to connect) after you sign in, that is expected: {UNREACHABLE_CALLBACK_STEP}"
    )
}

/// A hyperlinked label (OSC 8 with BEL, as the TypeScript writes it).
fn link(url: &str, label: &str) -> String {
    format!("\x1b]8;;{url}\x07{label}\x1b]8;;\x07")
}

enum Item {
    Spacer,
    /// A styled line with one column of padding (`new Text(text, 1, 0)`).
    Line(String),
    /// The input, where the prompt put it.
    Input,
    /// A submitted answer in the input's place (`> value`, no padding).
    Submitted(String),
}

struct Select {
    message: String,
    options: Vec<(String, String)>,
    index: usize,
}

/// What a key did to the dialog.
#[derive(Debug, PartialEq)]
pub enum DialogAction {
    None,
    /// The prompt's answer (an option id for a select prompt).
    Answer(String),
    /// Esc: the login stops (`Login cancelled`).
    Cancel,
}

pub struct LoginDialog {
    title: String,
    items: Vec<Item>,
    input: tui::input::Input,
    keys: KeybindingsManager,
    secret: bool,
    /// The prompt has no meaningful empty answer (paste-the-code): Enter on an empty input is ignored.
    require_value: bool,
    /// A text prompt waits for its answer.
    waiting: bool,
    select: Option<Select>,
    /// `browserReach()`: `local`, `container` or `ssh`.
    reach: String,
}

impl LoginDialog {
    /// The dialog for signing in to `name` (`Login to <name>`).
    pub fn new(keys: KeybindingsManager, name: &str, reach: &str) -> Self {
        let mut input = tui::input::Input::new(keys.clone());
        input.focused = true;
        LoginDialog {
            title: format!("Login to {name}"),
            items: Vec::new(),
            input,
            keys,
            secret: false,
            require_value: false,
            waiting: false,
            select: None,
            reach: reach.to_string(),
        }
    }

    fn line(&mut self, text: String) {
        self.items.push(Item::Line(text));
    }

    /// A new input, masked for secrets (`createInput`).
    fn reset_input(&mut self, secret: bool) {
        let mut input = if secret {
            tui::input::Input::new(self.keys.clone()).with_options(None, None, None, Some("•"))
        } else {
            tui::input::Input::new(self.keys.clone())
        };
        input.focused = true;
        self.input = input;
        self.secret = secret;
    }

    /// One step the provider's flow reported (`notify`).
    pub fn show_event(&mut self, theme: &Theme, event: &LoginEvent) {
        match event {
            LoginEvent::AuthUrl { url, instructions } => {
                self.items.clear();
                self.items.push(Item::Spacer);
                self.line(fg(theme, "accent", &link(url, url)));
                self.line(fg(theme, "dim", &link(url, "Ctrl+click to open")));
                if self.reach != "local" {
                    self.line(fg(theme, "warning", "Open this address in a browser on your computer."));
                }
                if let Some(instructions) = instructions.as_deref().filter(|_| self.reach == "local") {
                    self.items.push(Item::Spacer);
                    self.line(fg(theme, "warning", instructions));
                }
            }
            LoginEvent::DeviceCode {
                user_code,
                verification_uri,
            } => {
                self.items.clear();
                self.items.push(Item::Spacer);
                self.line(fg(theme, "accent", &link(verification_uri, verification_uri)));
                self.line(fg(theme, "dim", &link(verification_uri, "Ctrl+click to open")));
                self.items.push(Item::Spacer);
                self.line(fg(theme, "warning", &format!("Enter code: {user_code}")));
                // `showWaiting("Waiting for authentication...")` follows every device code.
                self.items.push(Item::Spacer);
                self.line(fg(theme, "dim", "Waiting for authentication..."));
                let hint = self.hint(theme, "tui.select.cancel", "to cancel");
                self.line(format!("({hint})"));
            }
            LoginEvent::Info { message, links } => {
                self.items.push(Item::Spacer);
                self.line(fg(theme, "text", message));
                for (label, url) in links {
                    let text = match label {
                        Some(label) => format!("{label}: {url}"),
                        None => url.clone(),
                    };
                    self.line(fg(theme, "accent", &link(url, &text)));
                }
            }
            LoginEvent::Progress(message) => self.line(fg(theme, "dim", message)),
        }
    }

    fn hint(&self, theme: &Theme, keybinding: &str, description: &str) -> String {
        key_hint(theme, &key_text(&self.keys, keybinding, false), description)
    }

    /// A question the provider's flow asks (`prompt`).
    pub fn show_prompt(&mut self, theme: &Theme, prompt: &LoginPrompt) {
        match prompt {
            LoginPrompt::Select { message, options } => {
                self.select = Some(Select {
                    message: message.clone(),
                    options: options.clone(),
                    index: 0,
                });
            }
            LoginPrompt::ManualCode { message } => {
                // A pasted code or redirect URL is not a secret, whatever an earlier prompt asked for.
                if self.secret {
                    self.reset_input(false);
                }
                self.require_value = true;
                self.input.set_value("");
                self.items.push(Item::Spacer);
                self.line(fg(theme, "dim", message));
                let color = if self.reach == "local" { "dim" } else { "warning" };
                self.line(fg(theme, color, &unreachable_callback_hint(&self.reach)));
                self.items.push(Item::Input);
                let hint = self.hint(theme, "tui.select.cancel", "to cancel");
                self.line(format!("({hint})"));
                self.waiting = true;
            }
            LoginPrompt::Text {
                message,
                placeholder,
                secret,
            } => {
                if *secret != self.secret {
                    self.reset_input(*secret);
                }
                self.require_value = false;
                self.items.push(Item::Spacer);
                self.line(fg(theme, "text", message));
                if let Some(placeholder) = placeholder {
                    self.line(fg(theme, "dim", &format!("e.g., {placeholder}")));
                }
                self.items.push(Item::Input);
                let cancel = self.hint(theme, "tui.select.cancel", "to cancel,");
                let confirm = self.hint(theme, "tui.select.confirm", "to submit");
                self.line(format!("({cancel} {confirm})"));
                self.input.set_value("");
                self.waiting = true;
            }
        }
    }

    pub fn handle_input(&mut self, data: &str) -> DialogAction {
        if let Some(select) = self.select.as_mut() {
            let keys = &self.keys;
            if keys.matches(data, "tui.select.up") || data == "k" {
                select.index = select.index.saturating_sub(1);
            } else if keys.matches(data, "tui.select.down") || data == "j" {
                select.index = (select.index + 1).min(select.options.len().saturating_sub(1));
            } else if keys.matches(data, "tui.select.confirm") || data == "\n" {
                if let Some((id, _)) = select.options.get(select.index) {
                    let id = id.clone();
                    self.select = None;
                    return DialogAction::Answer(id);
                }
            } else if keys.matches(data, "tui.select.cancel") {
                self.select = None;
                return DialogAction::Cancel;
            }
            return DialogAction::None;
        }
        if self.keys.matches(data, "tui.select.cancel") {
            return DialogAction::Cancel;
        }
        if self.waiting && self.keys.matches(data, "tui.select.confirm") {
            let value = self.input.value().to_string();
            if self.require_value && value.trim().is_empty() {
                return DialogAction::None;
            }
            let shown = if self.secret {
                "•".repeat(value.chars().count().min(12))
            } else {
                value.clone()
            };
            for item in self.items.iter_mut() {
                if matches!(item, Item::Input) {
                    *item = Item::Submitted(shown.clone());
                }
            }
            self.waiting = false;
            return DialogAction::Answer(value);
        }
        self.input.handle_input(data);
        DialogAction::None
    }

    pub fn render(&mut self, theme: &Rc<Theme>, width: usize) -> Vec<String> {
        let border = fg(theme, "border", &"─".repeat(width.max(1)));
        let text = |s: &str| Text::new(s, 1, 0, None).render(width);
        let mut lines = vec![border.clone()];
        if let Some(select) = &self.select {
            // `ExtensionSelectorComponent` in the dialog's slot.
            lines.push(String::new());
            lines.extend(text(&fg(theme, "accent", &bold(&select.message))));
            lines.push(String::new());
            for (index, (_, label)) in select.options.iter().enumerate() {
                let row = if index == select.index {
                    format!("{}{}", fg(theme, "accent", "→ "), fg(theme, "accent", label))
                } else {
                    format!("  {}", fg(theme, "text", label))
                };
                lines.extend(text(&row));
            }
            lines.push(String::new());
            let navigate = key_hint(theme, "↑↓", "navigate");
            let select_hint = self.hint(theme, "tui.select.confirm", "select");
            let cancel = self.hint(theme, "tui.select.cancel", "cancel");
            lines.extend(text(&format!("{navigate}  {select_hint}  {cancel}")));
            lines.push(String::new());
            lines.push(border);
            return lines;
        }
        lines.extend(text(&fg(theme, "accent", &bold(&self.title))));
        for item in &self.items {
            match item {
                Item::Spacer => lines.push(String::new()),
                Item::Line(line) => lines.extend(text(line)),
                Item::Input => lines.extend(self.input.render(width)),
                Item::Submitted(value) => lines.extend(Text::new(&format!("> {value}"), 0, 0, None).render(width)),
            }
        }
        lines.push(border);
        lines
    }
}
