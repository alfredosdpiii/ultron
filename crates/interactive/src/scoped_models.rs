//! Pi's `/scoped-models` selector (`ScopedModelsSelectorComponent`, modes/interactive/components/
//! scoped-models-selector.ts, as client-tui-pi-commands.ts `showScopedModelsSelector` drives it): the models Ctrl+P
//! cycles through, session-only until Ctrl+S saves them to `enabledModels`. `None` enables every model; a list is
//! the enabled models in cycling order.

use crate::components::{bold, fg, format_key_text, strikethrough};
use crate::lane::ModelInfo;
use std::rc::Rc;
use tui::components::{Component, Text};
use tui::keybindings::KeybindingsManager;
use tui::theme::Theme;

type EnabledIds = Option<Vec<String>>;

fn is_enabled(enabled: &EnabledIds, id: &str) -> bool {
    enabled.as_ref().is_none_or(|ids| ids.iter().any(|e| e == id))
}

/// `normalizeEnabled`: an explicit list covering every model is all of them.
fn normalize(result: Vec<String>, all: &[String]) -> EnabledIds {
    if result.len() == all.len() && result.iter().all(|id| all.contains(id)) {
        None
    } else {
        Some(result)
    }
}

fn toggle(enabled: &EnabledIds, all: &[String], id: &str) -> EnabledIds {
    match enabled {
        None => Some(all.iter().filter(|m| *m != id).cloned().collect()),
        Some(ids) => match ids.iter().position(|e| e == id) {
            Some(index) => {
                let mut ids = ids.clone();
                ids.remove(index);
                Some(ids)
            }
            None => {
                let mut ids = ids.clone();
                ids.push(id.to_string());
                normalize(ids, all)
            }
        },
    }
}

fn enable_all(enabled: &EnabledIds, all: &[String], targets: Option<&[String]>) -> EnabledIds {
    let ids = enabled.as_ref()?;
    let mut result = ids.clone();
    for id in targets.unwrap_or(all) {
        if !result.contains(id) {
            result.push(id.clone());
        }
    }
    normalize(result, all)
}

fn clear_all(enabled: &EnabledIds, all: &[String], targets: Option<&[String]>) -> EnabledIds {
    match enabled {
        None => Some(match targets {
            Some(targets) => all.iter().filter(|id| !targets.contains(id)).cloned().collect(),
            None => Vec::new(),
        }),
        Some(ids) => {
            let targets = targets.unwrap_or(ids);
            Some(ids.iter().filter(|id| !targets.contains(id)).cloned().collect())
        }
    }
}

/// `getSortedIds`: the enabled models first, in their order, then the rest.
fn sorted_ids(enabled: &EnabledIds, all: &[String]) -> Vec<String> {
    match enabled {
        None => all.to_vec(),
        Some(ids) => ids
            .iter()
            .cloned()
            .chain(all.iter().filter(|id| !ids.contains(id)).cloned())
            .collect(),
    }
}

/// `getModelSearchText`.
fn search_text(model: &ModelInfo) -> String {
    let name = if model.name.is_empty() {
        String::new()
    } else {
        format!(" {}", model.name)
    };
    let (id, provider) = (&model.id, &model.provider);
    format!("{id} {provider} {provider}/{id} {provider} {id}{name}")
}

#[derive(Clone)]
struct Item {
    full_id: String,
    model: Option<ModelInfo>,
    enabled: bool,
}

/// What a key did.
#[derive(Debug, PartialEq)]
pub enum ScopedAction {
    None,
    /// The enabled set or order changed (session-only).
    Changed(Option<Vec<String>>),
    /// Ctrl+S: save the selection to settings.
    Persist(Option<Vec<String>>),
    Cancel,
}

pub struct ScopedModelsSelector {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    models: Vec<(String, ModelInfo)>,
    all_ids: Vec<String>,
    enabled: EnabledIds,
    filtered: Vec<Item>,
    selected: usize,
    search: tui::input::Input,
    dirty: bool,
    /// The catalog refresh's line and its color.
    refresh: Option<(String, &'static str)>,
}

const MAX_VISIBLE: usize = 8;

impl ScopedModelsSelector {
    pub fn new(
        theme: Rc<Theme>,
        keys: KeybindingsManager,
        models: &[ModelInfo],
        enabled: Option<Vec<String>>,
        refresh_status: Option<&str>,
    ) -> Self {
        let mut search = tui::input::Input::new(keys.clone());
        search.focused = true;
        let mut selector = ScopedModelsSelector {
            theme,
            keys,
            models: Vec::new(),
            all_ids: Vec::new(),
            enabled,
            filtered: Vec::new(),
            selected: 0,
            search,
            dirty: false,
            refresh: refresh_status.map(|s| (s.to_string(), "muted")),
        };
        selector.set_models(models);
        selector.filtered = selector.items();
        selector
    }

    fn set_models(&mut self, models: &[ModelInfo]) {
        self.models = models
            .iter()
            .map(|m| (format!("{}/{}", m.provider, m.id), m.clone()))
            .collect();
        self.all_ids = self.models.iter().map(|(id, _)| id.clone()).collect();
    }

    fn model(&self, id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|(full, _)| full == id).map(|(_, m)| m)
    }

    /// `updateModels`: a refreshed catalog (and, when given, the enabled models), keeping the selected row.
    pub fn update_models(&mut self, models: &[ModelInfo], enabled: Option<Option<Vec<String>>>) {
        let selected = self.filtered.get(self.selected).map(|item| item.full_id.clone());
        if let Some(enabled) = enabled {
            self.enabled = enabled;
        }
        self.set_models(models);
        self.refresh();
        if let Some(index) = selected.and_then(|id| self.filtered.iter().position(|item| item.full_id == id)) {
            self.selected = index;
        }
    }

    /// `setRefreshStatus`.
    pub fn set_refresh_status(&mut self, message: &str, kind: &'static str) {
        self.refresh = Some((message.to_string(), kind));
    }

    fn items(&self) -> Vec<Item> {
        sorted_ids(&self.enabled, &self.all_ids)
            .into_iter()
            .map(|id| Item {
                model: self.model(&id).cloned(),
                enabled: is_enabled(&self.enabled, &id),
                full_id: id,
            })
            .collect()
    }

    fn refresh(&mut self) {
        let query = self.search.value().to_string();
        let items = self.items();
        self.filtered = if query.is_empty() {
            items
        } else {
            tui::fuzzy::fuzzy_filter(&items, &query, |item| match &item.model {
                Some(model) => search_text(model),
                None => item.full_id.clone(),
            })
        };
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    fn key(&self, keybinding: &str) -> String {
        format_key_text(&self.keys.get_keys(keybinding).join("/"), true, false)
    }

    fn footer(&self) -> String {
        let t = &self.theme;
        let count = match &self.enabled {
            None => "all enabled".to_string(),
            Some(ids) => {
                let available = ids.iter().filter(|id| self.model(id).is_some()).count();
                let unavailable = ids.len() - available;
                let extra = if unavailable > 0 {
                    format!(" · {unavailable} unavailable")
                } else {
                    String::new()
                };
                format!("{available}/{} enabled{extra}", self.all_ids.len())
            }
        };
        let parts = [
            format!("{} toggle", self.key("tui.select.confirm")),
            format!("{} all", self.key("app.models.enableAll")),
            format!("{} clear", self.key("app.models.clearAll")),
            format!("{} provider", self.key("app.models.toggleProvider")),
            format!(
                "{}/{} reorder",
                self.key("app.models.reorderUp"),
                self.key("app.models.reorderDown")
            ),
            format!("{} save", self.key("app.models.save")),
            count,
        ]
        .join(" · ");
        if self.dirty {
            format!(
                "{}{}",
                fg(t, "dim", &format!("  {parts} ")),
                fg(t, "warning", "(unsaved)")
            )
        } else {
            fg(t, "dim", &format!("  {parts}"))
        }
    }

    fn changed(&mut self) -> ScopedAction {
        self.dirty = true;
        self.refresh();
        ScopedAction::Changed(self.enabled.clone())
    }

    pub fn handle_input(&mut self, data: &str) -> ScopedAction {
        let keys = self.keys.clone();
        let count = self.filtered.len();
        if keys.matches(data, "tui.select.up") {
            if count > 0 {
                self.selected = if self.selected == 0 {
                    count - 1
                } else {
                    self.selected - 1
                };
            }
            return ScopedAction::None;
        }
        if keys.matches(data, "tui.select.down") {
            if count > 0 {
                self.selected = if self.selected + 1 >= count {
                    0
                } else {
                    self.selected + 1
                };
            }
            return ScopedAction::None;
        }
        let up = keys.matches(data, "app.models.reorderUp");
        if up || keys.matches(data, "app.models.reorderDown") {
            let Some(ids) = self.enabled.clone() else {
                return ScopedAction::None;
            };
            let Some(item) = self.filtered.get(self.selected).cloned() else {
                return ScopedAction::None;
            };
            let Some(index) = ids.iter().position(|id| *id == item.full_id) else {
                return ScopedAction::None;
            };
            let target = if up { index.checked_sub(1) } else { Some(index + 1) };
            if let Some(target) = target.filter(|t| *t < ids.len()) {
                let mut ids = ids;
                ids.swap(index, target);
                self.enabled = Some(ids);
                self.selected = if up {
                    self.selected.saturating_sub(1)
                } else {
                    self.selected + 1
                };
                return self.changed();
            }
            return ScopedAction::None;
        }
        if keys.matches(data, "tui.select.confirm") {
            let Some(item) = self.filtered.get(self.selected).cloned() else {
                return ScopedAction::None;
            };
            self.enabled = toggle(&self.enabled, &self.all_ids, &item.full_id);
            return self.changed();
        }
        let targets: Option<Vec<String>> =
            (!self.search.value().is_empty()).then(|| self.filtered.iter().map(|i| i.full_id.clone()).collect());
        if keys.matches(data, "app.models.enableAll") {
            self.enabled = enable_all(&self.enabled, &self.all_ids, targets.as_deref());
            return self.changed();
        }
        if keys.matches(data, "app.models.clearAll") {
            self.enabled = clear_all(&self.enabled, &self.all_ids, targets.as_deref());
            return self.changed();
        }
        if keys.matches(data, "app.models.toggleProvider") {
            let Some(provider) = self
                .filtered
                .get(self.selected)
                .and_then(|item| item.model.as_ref())
                .map(|m| m.provider.clone())
            else {
                return ScopedAction::None;
            };
            let ids: Vec<String> = self
                .models
                .iter()
                .filter(|(_, m)| m.provider == provider)
                .map(|(id, _)| id.clone())
                .collect();
            let all_enabled = ids.iter().all(|id| is_enabled(&self.enabled, id));
            self.enabled = if all_enabled {
                clear_all(&self.enabled, &self.all_ids, Some(&ids))
            } else {
                enable_all(&self.enabled, &self.all_ids, Some(&ids))
            };
            return self.changed();
        }
        if keys.matches(data, "app.models.save") {
            self.dirty = false;
            return ScopedAction::Persist(self.enabled.clone());
        }
        if data == "\x03" {
            if self.search.value().is_empty() {
                return ScopedAction::Cancel;
            }
            self.search.set_value("");
            self.refresh();
            return ScopedAction::None;
        }
        if data == "\x1b" {
            return ScopedAction::Cancel;
        }
        self.search.handle_input(data);
        self.refresh();
        ScopedAction::None
    }

    pub fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let text = |s: &str| Text::new(s, 0, 0, None).render(width);
        let border = fg(&t, "border", &"─".repeat(width.max(1)));
        let mut lines = vec![border.clone(), String::new()];
        lines.extend(text(&fg(&t, "accent", &bold("Model Configuration"))));
        lines.extend(text(&fg(
            &t,
            "muted",
            &format!("Session-only. {} to save to settings.", self.key("app.models.save")),
        )));
        lines.push(String::new());
        lines.extend(self.search.render(width));
        lines.push(String::new());
        if self.filtered.is_empty() {
            lines.extend(text(&fg(&t, "muted", "  No matching models")));
        } else {
            let count = self.filtered.len();
            let start = self
                .selected
                .saturating_sub(MAX_VISIBLE / 2)
                .min(count.saturating_sub(MAX_VISIBLE));
            let end = (start + MAX_VISIBLE).min(count);
            for (index, item) in self.filtered[start..end].iter().enumerate() {
                let selected = start + index == self.selected;
                let prefix = if selected {
                    fg(&t, "accent", "→ ")
                } else {
                    "  ".into()
                };
                let id = item.model.as_ref().map_or(item.full_id.clone(), |m| m.id.clone());
                let styled = if item.model.is_some() { id } else { strikethrough(&id) };
                let model_text = if selected { fg(&t, "accent", &styled) } else { styled };
                let badge = fg(
                    &t,
                    "muted",
                    &item
                        .model
                        .as_ref()
                        .map_or(" [unavailable]".to_string(), |m| format!(" [{}]", m.provider)),
                );
                let status = if item.model.is_some() && item.enabled {
                    fg(&t, "accent", "✓ ")
                } else {
                    "  ".into()
                };
                lines.extend(text(&format!("{prefix}{status}{model_text}{badge}")));
            }
            if start > 0 || end < count {
                lines.extend(text(&fg(&t, "muted", &format!("  ({}/{count})", self.selected + 1))));
            }
            let selected = &self.filtered[self.selected];
            lines.push(String::new());
            let name = match &selected.model {
                Some(model) => format!("  Model Name: {}", model.name),
                None => "  Model unavailable".into(),
            };
            lines.extend(text(&fg(&t, "muted", &name)));
        }
        lines.push(String::new());
        if let Some((message, kind)) = &self.refresh {
            lines.extend(text(&fg(&t, kind, &format!("  {message}"))));
        }
        lines.extend(text(&self.footer()));
        lines.push(border);
        lines
    }
}
