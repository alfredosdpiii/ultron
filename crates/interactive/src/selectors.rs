//! Selectors the client TUI shows in the editor's place: Pi's `UserMessageSelectorComponent` (`/fork`,
//! modes/interactive/components/user-message-selector.ts).

use std::rc::Rc;

use tui::components::{Component, Text};
use tui::keybindings::KeybindingsManager;
use tui::theme::Theme;
use tui::utils::truncate_to_width;

use crate::components::{bold, fg};
use crate::lane::ForkMessage;

/// What a key did to a selector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectorAction {
    /// Picked: the entry id.
    Selected(String),
    Cancelled,
}

/// `UserMessageSelectorComponent`: the user messages, newest selected, two lines each.
pub struct UserMessageSelector {
    theme: Rc<Theme>,
    messages: Vec<ForkMessage>,
    selected: usize,
    max_visible: usize,
}

impl UserMessageSelector {
    /// `initial`: the entry to select first (else the most recent).
    pub fn new(theme: Rc<Theme>, messages: Vec<ForkMessage>, initial: Option<&str>) -> Self {
        let selected = initial
            .and_then(|id| messages.iter().position(|m| m.entry_id == id))
            .unwrap_or(messages.len().saturating_sub(1));
        UserMessageSelector {
            theme,
            messages,
            selected,
            max_visible: 10,
        }
    }

    pub fn selected(&self) -> Option<&ForkMessage> {
        self.messages.get(self.selected)
    }

    /// `UserMessageList.handleInput`: up and down wrap.
    pub fn handle_input(&mut self, keys: &KeybindingsManager, data: &str) -> Option<SelectorAction> {
        let n = self.messages.len();
        if keys.matches(data, "tui.select.up") {
            if n > 0 {
                self.selected = if self.selected == 0 { n - 1 } else { self.selected - 1 };
            }
        } else if keys.matches(data, "tui.select.down") {
            if n > 0 {
                self.selected = if self.selected + 1 >= n { 0 } else { self.selected + 1 };
            }
        } else if keys.matches(data, "tui.select.confirm") {
            return self.selected().map(|m| SelectorAction::Selected(m.entry_id.clone()));
        } else if keys.matches(data, "tui.select.cancel") {
            return Some(SelectorAction::Cancelled);
        }
        None
    }

    fn list(&self, width: usize) -> Vec<String> {
        let t = &self.theme;
        if self.messages.is_empty() {
            return vec![fg(t, "muted", "  No user messages found")];
        }
        let n = self.messages.len() as isize;
        let max = self.max_visible as isize;
        let start = (self.selected as isize - max / 2).min(n - max).max(0) as usize;
        let end = (start + self.max_visible).min(self.messages.len());
        let mut lines = Vec::new();
        for i in start..end {
            let selected = i == self.selected;
            let normalized = self.messages[i].text.replace('\n', " ");
            let truncated = truncate_to_width(normalized.trim(), width.saturating_sub(2), "...", false);
            let cursor = if selected { fg(t, "accent", "› ") } else { "  ".into() };
            let text = if selected { bold(&truncated) } else { truncated };
            lines.push(format!("{cursor}{text}"));
            lines.push(fg(
                t,
                "muted",
                &format!("  Message {} of {}", i + 1, self.messages.len()),
            ));
            lines.push(String::new());
        }
        if start > 0 || end < self.messages.len() {
            lines.push(fg(
                t,
                "muted",
                &format!("  ({}/{})", self.selected + 1, self.messages.len()),
            ));
        }
        lines
    }
}

impl Component for UserMessageSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let border = fg(&t, "border", &"─".repeat(width.max(1)));
        let mut lines = vec![String::new()];
        lines.extend(Text::new(&bold("Fork from Message"), 1, 0, None).render(width));
        lines.extend(
            Text::new(
                &fg(
                    &t,
                    "muted",
                    "Select a user message to copy the active path up to that point into a new session",
                ),
                1,
                0,
                None,
            )
            .render(width),
        );
        lines.push(String::new());
        lines.push(border.clone());
        lines.push(String::new());
        lines.extend(self.list(width));
        lines.push(String::new());
        lines.push(border);
        lines
    }
}

fn provider_table() -> &'static serde_json::Map<String, serde_json::Value> {
    static TABLE: std::sync::OnceLock<serde_json::Map<String, serde_json::Value>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| serde_json::from_str(include_str!("../fixtures/provider_names.json")).unwrap_or_default())
}

/// The built-in providers' display names (`provider.name`), recorded from the TypeScript registry by
/// fixtures/provider_names.mjs.
pub fn provider_name(id: &str) -> String {
    provider_table()
        .get(id)
        .and_then(|p| p["name"].as_str())
        .unwrap_or(id)
        .to_string()
}

/// How a built-in provider signs in (`provider.auth`).
#[derive(Clone, Debug, PartialEq)]
pub struct ProviderLogin {
    pub id: String,
    pub name: String,
    /// The API-key method's name and whether Ultron can save a key for it (`apiKey.login`).
    pub api_key: Option<(String, bool)>,
    /// The subscription method's login label.
    pub oauth: Option<Option<String>>,
}

/// Every built-in provider's sign-in methods, by id.
pub fn provider_logins() -> Vec<ProviderLogin> {
    provider_table()
        .iter()
        .map(|(id, p)| ProviderLogin {
            id: id.clone(),
            name: p["name"].as_str().unwrap_or(id).to_string(),
            api_key: p
                .get("apiKey")
                .map(|k| (k["name"].as_str().unwrap_or("").to_string(), k["login"] == true)),
            oauth: p.get("oauth").map(|o| o["loginLabel"].as_str().map(str::to_string)),
        })
        .collect()
}

/// `String.localeCompare` for provider names: case-insensitive first, then by case.
pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| b.cmp(a))
}

/// One provider row of the auth selector (`AuthSelectorProvider`).
#[derive(Clone, Debug, PartialEq)]
pub struct AuthProvider {
    pub id: String,
    pub name: String,
    /// `oauth` or `api_key`.
    pub auth_type: String,
    /// What is configured (`AuthCheck`): `(type, source)`; `None` when nothing is.
    pub status: Option<(String, Option<String>)>,
    /// The API-key method's name and whether a key can be saved for it (`apiKey.login`).
    pub api_key_method: Option<(String, bool)>,
    /// The subscription method's login label.
    pub login_label: Option<String>,
    /// The key prompt is Pi's generic one (`envApiKeyAuth`: one secret, "Enter <method>").
    pub generic_login: bool,
}

/// Pi's `OAuthSelectorComponent` (`/login` and `/logout`): search, the providers with their status, Enter picks.
pub struct AuthSelector {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    login: bool,
    all: Vec<AuthProvider>,
    filtered: Vec<AuthProvider>,
    selected: usize,
    search: tui::input::Input,
    type_labels: bool,
}

impl AuthSelector {
    pub fn new(
        theme: Rc<Theme>,
        keys: KeybindingsManager,
        login: bool,
        providers: Vec<AuthProvider>,
        search: Option<&str>,
    ) -> Self {
        let mut input = tui::input::Input::new(keys.clone());
        input.focused = true;
        if let Some(s) = search {
            input.set_value(s);
        }
        let types: std::collections::HashSet<&str> = providers.iter().map(|p| p.auth_type.as_str()).collect();
        let mut selector = AuthSelector {
            theme,
            keys,
            login,
            type_labels: types.len() > 1,
            filtered: providers.clone(),
            all: providers,
            selected: 0,
            search: input,
        };
        selector.filter();
        selector
    }

    fn filter(&mut self) {
        let query = self.search.value().to_string();
        self.filtered = if query.is_empty() {
            self.all.clone()
        } else {
            tui::fuzzy::fuzzy_filter(&self.all, &query, |p| format!("{} {} {}", p.name, p.id, p.auth_type))
        };
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    /// The picked `(provider id, auth type)`, or `Cancelled`.
    pub fn handle_input(&mut self, data: &str) -> Option<Result<(String, String), ()>> {
        let k = |id: &str| self.keys.matches(data, id);
        if k("tui.select.up") {
            self.selected = self.selected.saturating_sub(1);
        } else if k("tui.select.down") {
            if !self.filtered.is_empty() {
                self.selected = (self.selected + 1).min(self.filtered.len() - 1);
            }
        } else if k("tui.select.confirm") {
            return self
                .filtered
                .get(self.selected)
                .map(|p| Ok((p.id.clone(), p.auth_type.clone())));
        } else if k("tui.select.cancel") {
            return Some(Err(()));
        } else {
            self.search.handle_input(data);
            self.filter();
        }
        None
    }

    fn status(&self, p: &AuthProvider) -> String {
        let t = &self.theme;
        let Some((kind, source)) = &p.status else {
            return fg(t, "muted", " • unconfigured");
        };
        if *kind != p.auth_type {
            let label = if kind == "oauth" {
                "subscription configured"
            } else {
                "API key configured"
            };
            return fg(t, "muted", " • ") + &fg(t, "warning", label);
        }
        match source.as_deref() {
            None | Some("OAuth" | "stored credential") => fg(t, "success", " ✓ configured"),
            Some(source) => {
                let env = !source.is_empty()
                    && source.split(", ").all(|s| {
                        let mut c = s.chars();
                        c.next().is_some_and(|f| f.is_ascii_uppercase())
                            && c.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                    });
                let shown = if env {
                    format!("env: {source}")
                } else {
                    source.to_string()
                };
                fg(t, "success", &format!(" ✓ {shown}"))
            }
        }
    }
}

impl Component for AuthSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let border = fg(&t, "border", &"─".repeat(width.max(1)));
        let truncated = |s: String| {
            tui::components::TruncatedText {
                text: s,
                padding_x: 1,
                padding_y: 0,
            }
            .render(width)
        };
        let mut lines = vec![border.clone(), String::new()];
        let title = if self.login {
            "Select provider to configure:"
        } else {
            "Select provider to logout:"
        };
        lines.extend(truncated(fg(&t, "accent", &bold(title))));
        lines.push(String::new());
        lines.extend(self.search.render(width));
        lines.push(String::new());
        let max = 8isize;
        let n = self.filtered.len() as isize;
        let start = (self.selected as isize - max / 2).min(n - max).max(0) as usize;
        let end = (start + 8).min(self.filtered.len());
        for i in start..end {
            let p = &self.filtered[i];
            let label = if self.type_labels {
                let kind = if p.auth_type == "oauth" {
                    "subscription"
                } else {
                    "API key"
                };
                fg(&t, "muted", &format!(" [{kind}]"))
            } else {
                String::new()
            };
            let line = if i == self.selected {
                format!(
                    "{}{}{label}{}",
                    fg(&t, "accent", "→ "),
                    fg(&t, "accent", &p.name),
                    self.status(p)
                )
            } else {
                format!("  {}{label}{}", fg(&t, "text", &p.name), self.status(p))
            };
            lines.extend(truncated(line));
        }
        if start > 0 || end < self.filtered.len() {
            lines.extend(truncated(fg(
                &t,
                "muted",
                &format!("  ({}/{})", self.selected + 1, self.filtered.len()),
            )));
        }
        if self.filtered.is_empty() {
            let message = if !self.all.is_empty() {
                "No matching providers"
            } else if self.login {
                "No providers available"
            } else {
                "No providers logged in. Use /login first."
            };
            lines.extend(truncated(fg(&t, "muted", &format!("  {message}"))));
        }
        lines.push(String::new());
        lines.push(border);
        lines
    }
}
