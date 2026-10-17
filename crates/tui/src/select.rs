//! SelectList, SettingsList (no search, no submenus) and Loader/CancellableLoader: ports of
//! packages/tui/src/components/{select-list,settings-list,loader,cancellable-loader}.ts.

use std::rc::Rc;

use crate::components::{Component, Text};
use crate::keys::{KeyContext, parse_key};
use crate::mouse::{MouseButton, MouseEvent, MouseEventType, MouseResult};
use crate::utils::{truncate_to_width, visible_width, wrap_text_with_ansi};

/// Default `tui.select.*` keybindings (packages/tui/src/keybindings.ts).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectAction {
    Up,
    Down,
    Confirm,
    Cancel,
}

/// The select action for one input sequence under the default keybindings.
pub fn select_action(data: &str) -> Option<SelectAction> {
    match parse_key(data, KeyContext::default())?.as_str() {
        "up" => Some(SelectAction::Up),
        "down" => Some(SelectAction::Down),
        "enter" => Some(SelectAction::Confirm),
        "escape" | "ctrl+c" => Some(SelectAction::Cancel),
        _ => None,
    }
}

pub type StyleFn = Box<dyn Fn(&str) -> String>;
pub type SelectedStyleFn = Box<dyn Fn(&str, bool) -> String>;

/// JS `width - n` passed to truncateToWidth: negative widths behave like 0.
fn sub(a: usize, b: usize) -> usize {
    a.saturating_sub(b)
}

fn visible_range(selected: usize, len: usize, max_visible: usize) -> (usize, usize) {
    let start = (selected as i64 - (max_visible / 2) as i64)
        .min(len as i64 - max_visible as i64)
        .max(0) as usize;
    (start, (start + max_visible).min(len))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectItem {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

pub struct SelectListTheme {
    pub selected_prefix: StyleFn,
    pub selected_text: StyleFn,
    pub description: StyleFn,
    pub scroll_info: StyleFn,
    pub no_match: StyleFn,
}

/// What a custom primary-column truncation sees (TS `SelectListTruncatePrimaryContext`).
pub struct TruncatePrimaryContext<'a> {
    pub text: &'a str,
    pub max_width: usize,
    pub column_width: usize,
    pub item: &'a SelectItem,
    pub is_selected: bool,
}

pub type TruncatePrimaryFn = Box<dyn Fn(&TruncatePrimaryContext) -> String>;

const DEFAULT_PRIMARY_COLUMN_WIDTH: usize = 32;
const PRIMARY_COLUMN_GAP: usize = 2;
const MIN_DESCRIPTION_WIDTH: i64 = 10;

/// Scrolling single-choice list with an aligned description column.
pub struct SelectList {
    items: Vec<SelectItem>,
    filtered: Vec<SelectItem>,
    selected: usize,
    max_visible: usize,
    theme: Rc<SelectListTheme>,
    pressed: Option<usize>,
    pub min_primary_column_width: Option<usize>,
    pub max_primary_column_width: Option<usize>,
    pub truncate_primary: Option<TruncatePrimaryFn>,
}

/// Outcome of one input to a list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectEvent {
    SelectionChanged(SelectItem),
    Selected(SelectItem),
    Cancelled,
}

impl SelectList {
    pub fn new(items: Vec<SelectItem>, max_visible: usize, theme: impl Into<Rc<SelectListTheme>>) -> Self {
        Self {
            filtered: items.clone(),
            items,
            selected: 0,
            max_visible,
            theme: theme.into(),
            pressed: None,
            min_primary_column_width: None,
            max_primary_column_width: None,
            truncate_primary: None,
        }
    }

    pub fn set_filter(&mut self, filter: &str) {
        let f = filter.to_lowercase();
        self.filtered = self
            .items
            .iter()
            .filter(|i| i.value.to_lowercase().starts_with(&f))
            .cloned()
            .collect();
        self.selected = 0;
    }

    pub fn set_selected_index(&mut self, index: usize) {
        self.selected = index.min(self.filtered.len().saturating_sub(1));
    }

    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.filtered.get(self.selected)
    }

    pub fn handle_input(&mut self, data: &str) -> Option<SelectEvent> {
        self.apply_action(select_action(data)?)
    }

    /// One `tui.select.*` action, for owners that match keys with their own keybindings.
    pub fn apply_action(&mut self, action: SelectAction) -> Option<SelectEvent> {
        let len = self.filtered.len();
        match action {
            SelectAction::Up => {
                self.selected = if self.selected == 0 {
                    len.saturating_sub(1)
                } else {
                    self.selected - 1
                };
                self.selected_item().cloned().map(SelectEvent::SelectionChanged)
            }
            SelectAction::Down => {
                self.selected = if self.selected + 1 >= len { 0 } else { self.selected + 1 };
                self.selected_item().cloned().map(SelectEvent::SelectionChanged)
            }
            SelectAction::Confirm => self.selected_item().cloned().map(SelectEvent::Selected),
            SelectAction::Cancel => Some(SelectEvent::Cancelled),
        }
    }

    /// `handleMouse`: the wheel moves the selection, a left press selects the row under it and the click that
    /// follows confirms the pressed row. Selection events go to `events`.
    pub fn handle_mouse(&mut self, event: &MouseEvent, events: &mut Vec<SelectEvent>) -> Option<MouseResult> {
        let len = self.filtered.len();
        if len == 0 {
            return None;
        }
        if event.kind == MouseEventType::Wheel
            && let Some(delta) = event.wheel_delta.filter(|d| *d != 0)
        {
            let previous = self.selected;
            self.selected = if delta < 0 {
                self.selected.saturating_sub(1)
            } else {
                (self.selected + 1).min(len - 1)
            };
            let changed = self.selected != previous;
            if changed {
                events.extend(self.selected_item().cloned().map(SelectEvent::SelectionChanged));
            }
            return Some(MouseResult {
                handled: Some(true),
                render: Some(changed),
                ..MouseResult::default()
            });
        }
        if event.button != MouseButton::Left || !matches!(event.kind, MouseEventType::Press | MouseEventType::Click) {
            return None;
        }
        let (start, end) = visible_range(self.selected, len, self.max_visible);
        let index = start as i64 + event.y;
        if index < start as i64 || index >= end as i64 {
            return None;
        }
        let index = index as usize;
        if event.kind == MouseEventType::Press {
            self.pressed = Some(index);
            if self.selected != index {
                self.selected = index;
                events.extend(self.selected_item().cloned().map(SelectEvent::SelectionChanged));
            }
            return Some(MouseResult {
                handled: Some(true),
                focus: Some(true),
                ..MouseResult::default()
            });
        }
        let clicked = self.pressed.take().unwrap_or(index);
        let changed = self.selected != clicked;
        self.selected = clicked;
        if changed {
            events.extend(self.selected_item().cloned().map(SelectEvent::SelectionChanged));
        }
        events.extend(self.selected_item().cloned().map(SelectEvent::Selected));
        Some(MouseResult {
            handled: Some(true),
            ..MouseResult::default()
        })
    }

    fn truncate_primary(&self, item: &SelectItem, is_selected: bool, max_width: usize, column_width: usize) -> String {
        let text = Self::display_value(item);
        let truncated = match &self.truncate_primary {
            Some(f) => f(&TruncatePrimaryContext {
                text,
                max_width,
                column_width,
                item,
                is_selected,
            }),
            None => truncate_to_width(text, max_width, "", false),
        };
        truncate_to_width(&truncated, max_width, "", false)
    }

    fn display_value(item: &SelectItem) -> &str {
        if item.label.is_empty() {
            &item.value
        } else {
            &item.label
        }
    }

    fn primary_column_width(&self) -> usize {
        let raw_min = self
            .min_primary_column_width
            .or(self.max_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let raw_max = self
            .max_primary_column_width
            .or(self.min_primary_column_width)
            .unwrap_or(DEFAULT_PRIMARY_COLUMN_WIDTH);
        let (min, max) = (raw_min.min(raw_max).max(1), raw_min.max(raw_max).max(1));
        let widest = self
            .filtered
            .iter()
            .map(|i| visible_width(Self::display_value(i)) + PRIMARY_COLUMN_GAP)
            .max()
            .unwrap_or(0);
        widest.clamp(min, max)
    }

    fn render_item(&self, item: &SelectItem, is_selected: bool, width: usize, primary: usize) -> String {
        let prefix = if is_selected { "→ " } else { "  " };
        let prefix_width = visible_width(prefix);
        let desc = item.description.as_deref().map(|d| {
            d.split(['\r', '\n'])
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        });
        let desc = desc.map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
        if let Some(desc) = desc
            && width > 40
        {
            let effective = primary.min(width - prefix_width - 4).max(1);
            let max_primary = effective.saturating_sub(PRIMARY_COLUMN_GAP).max(1);
            let tv = self.truncate_primary(item, is_selected, max_primary, effective);
            let tvw = visible_width(&tv);
            let spacing = " ".repeat(effective.saturating_sub(tvw).max(1));
            let remaining = width as i64 - (prefix_width + tvw + spacing.len()) as i64 - 2;
            if remaining > MIN_DESCRIPTION_WIDTH {
                let td = truncate_to_width(&desc, remaining as usize, "", false);
                if is_selected {
                    return (self.theme.selected_text)(&format!("{prefix}{tv}{spacing}{td}"));
                }
                return format!("{prefix}{tv}{}", (self.theme.description)(&format!("{spacing}{td}")));
            }
        }
        let max_width = sub(width, prefix_width + 2);
        let tv = self.truncate_primary(item, is_selected, max_width, max_width);
        if is_selected {
            return (self.theme.selected_text)(&format!("{prefix}{tv}"));
        }
        format!("{prefix}{tv}")
    }
}

impl Component for SelectList {
    fn render(&mut self, width: usize) -> Vec<String> {
        if self.filtered.is_empty() {
            return vec![(self.theme.no_match)("  No matching commands")];
        }
        let primary = self.primary_column_width();
        let (start, end) = visible_range(self.selected, self.filtered.len(), self.max_visible);
        let mut lines: Vec<String> = (start..end)
            .map(|i| self.render_item(&self.filtered[i], i == self.selected, width, primary))
            .collect();
        if start > 0 || end < self.filtered.len() {
            let text = format!("  ({}/{})", self.selected + 1, self.filtered.len());
            lines.push((self.theme.scroll_info)(&truncate_to_width(
                &text,
                sub(width, 2),
                "",
                false,
            )));
        }
        lines
    }
}

pub struct SettingItem {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    pub current_value: String,
    pub values: Vec<String>,
}

pub struct SettingsListTheme {
    pub label: SelectedStyleFn,
    pub value: SelectedStyleFn,
    pub description: StyleFn,
    pub cursor: String,
    pub hint: StyleFn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SettingsEvent {
    Changed { id: String, value: String },
    Cancelled,
}

/// Label/value settings list; Enter or Space cycles an item's values.
pub struct SettingsList {
    pub items: Vec<SettingItem>,
    selected: usize,
    max_visible: usize,
    theme: SettingsListTheme,
}

impl SettingsList {
    pub fn new(items: Vec<SettingItem>, max_visible: usize, theme: SettingsListTheme) -> Self {
        Self {
            items,
            selected: 0,
            max_visible,
            theme,
        }
    }

    pub fn handle_input(&mut self, data: &str) -> Option<SettingsEvent> {
        let len = self.items.len();
        let action = if data == " " {
            Some(SelectAction::Confirm)
        } else {
            select_action(data)
        };
        match action? {
            SelectAction::Up if len > 0 => self.selected = if self.selected == 0 { len - 1 } else { self.selected - 1 },
            SelectAction::Down if len > 0 => {
                self.selected = if self.selected + 1 == len { 0 } else { self.selected + 1 }
            }
            SelectAction::Confirm => {
                let item = self.items.get_mut(self.selected)?;
                if item.values.is_empty() {
                    return None;
                }
                let next = item
                    .values
                    .iter()
                    .position(|v| *v == item.current_value)
                    .map_or(0, |i| (i + 1) % item.values.len());
                item.current_value = item.values[next].clone();
                return Some(SettingsEvent::Changed {
                    id: item.id.clone(),
                    value: item.current_value.clone(),
                });
            }
            SelectAction::Cancel => return Some(SettingsEvent::Cancelled),
            _ => {}
        }
        None
    }

    fn add_hint_line(&self, lines: &mut Vec<String>, width: usize) {
        lines.push(String::new());
        lines.push(truncate_to_width(
            &(self.theme.hint)("  Enter/Space to change · Esc to cancel"),
            width,
            "...",
            false,
        ));
    }
}

impl Component for SettingsList {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        if self.items.is_empty() {
            lines.push((self.theme.hint)("  No settings available"));
            return lines;
        }
        let (start, end) = visible_range(self.selected, self.items.len(), self.max_visible);
        let max_label = self
            .items
            .iter()
            .map(|i| visible_width(&i.label))
            .max()
            .unwrap_or(0)
            .min(36);
        for i in start..end {
            let item = &self.items[i];
            let is_selected = i == self.selected;
            let prefix = if is_selected { self.theme.cursor.as_str() } else { "  " };
            let padded = format!(
                "{}{}",
                item.label,
                " ".repeat(max_label.saturating_sub(visible_width(&item.label)))
            );
            let label = (self.theme.label)(&padded, is_selected);
            let value_max = sub(width, visible_width(prefix) + max_label + 2 + 2);
            let value = (self.theme.value)(
                &truncate_to_width(&item.current_value, value_max, "", false),
                is_selected,
            );
            lines.push(truncate_to_width(
                &format!("{prefix}{label}  {value}"),
                width,
                "...",
                false,
            ));
        }
        if start > 0 || end < self.items.len() {
            let text = format!("  ({}/{})", self.selected + 1, self.items.len());
            lines.push((self.theme.hint)(&truncate_to_width(&text, sub(width, 2), "", false)));
        }
        if let Some(desc) = self.items.get(self.selected).and_then(|i| i.description.as_deref())
            && !desc.is_empty()
        {
            lines.push(String::new());
            for line in wrap_text_with_ansi(desc, sub(width, 4)) {
                lines.push((self.theme.description)(&format!("  {line}")));
            }
        }
        self.add_hint_line(&mut lines, width);
        lines
    }
}

const DEFAULT_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
pub const DEFAULT_INTERVAL_MS: u64 = 80;

/// Spinner plus message. The owner drives the animation: call `tick()` every `interval_ms()` while `is_animated()`
/// and re-render (TS uses setInterval and ui.requestRender()).
pub struct Loader {
    text: Text,
    frames: Vec<String>,
    interval_ms: u64,
    current: usize,
    verbatim: bool,
    spinner: StyleFn,
    message_style: StyleFn,
    message: String,
    aborted: bool,
}

impl Loader {
    /// `indicator`: custom frames (shown verbatim, empty hides) and interval; `None` uses the colored braille spinner.
    pub fn new(spinner: StyleFn, message_style: StyleFn, message: &str, indicator: Option<(Vec<String>, u64)>) -> Self {
        let mut l = Self {
            text: Text::new("", 1, 0, None),
            frames: Vec::new(),
            interval_ms: DEFAULT_INTERVAL_MS,
            current: 0,
            verbatim: false,
            spinner,
            message_style,
            message: message.to_string(),
            aborted: false,
        };
        l.set_indicator(indicator);
        l
    }

    pub fn set_indicator(&mut self, indicator: Option<(Vec<String>, u64)>) {
        self.verbatim = indicator.is_some();
        let (frames, interval) =
            indicator.unwrap_or_else(|| (DEFAULT_FRAMES.iter().map(|s| s.to_string()).collect(), 0));
        self.frames = frames;
        self.interval_ms = if interval > 0 { interval } else { DEFAULT_INTERVAL_MS };
        self.current = 0;
        self.update();
    }

    pub fn set_message(&mut self, message: &str) {
        self.message = message.to_string();
        self.update();
    }

    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    pub fn interval_ms(&self) -> u64 {
        self.interval_ms
    }

    pub fn tick(&mut self) {
        if self.is_animated() {
            self.current = (self.current + 1) % self.frames.len();
            self.update();
        }
    }

    /// CancellableLoader: Escape or Ctrl+C aborts; returns true when this input aborted.
    pub fn handle_input(&mut self, data: &str) -> bool {
        if select_action(data) == Some(SelectAction::Cancel) {
            self.aborted = true;
            return true;
        }
        false
    }

    pub fn aborted(&self) -> bool {
        self.aborted
    }

    fn update(&mut self) {
        let frame = self.frames.get(self.current).map_or("", String::as_str);
        let rendered = if self.verbatim {
            frame.to_string()
        } else {
            (self.spinner)(frame)
        };
        let indicator = if rendered.is_empty() {
            String::new()
        } else {
            format!("{rendered} ")
        };
        self.text
            .set_text(&format!("{indicator}{}", (self.message_style)(&self.message)));
    }
}

impl Component for Loader {
    fn render(&mut self, width: usize) -> Vec<String> {
        let mut out = vec![String::new()];
        out.extend(self.text.render(width));
        out
    }
}
