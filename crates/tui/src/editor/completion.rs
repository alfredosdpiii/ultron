//! The editor's autocomplete picker: when typing triggers a request (`/` at the start of the message, `@`, `#` and
//! provider trigger characters at token boundaries, word characters inside those contexts), the debounce for
//! attachment-style triggers, Tab/Enter/Escape/arrows while the picker is open, and mouse picks.
//! TS runs provider requests asynchronously; here a request runs at once, or, when TS would debounce it, waits in
//! `pending_request` until the owner calls `fire_autocomplete_debounce` after `AUTOCOMPLETE_DEBOUNCE_MS`.

use super::*;
use crate::autocomplete::{AutocompleteItem, AutocompleteSuggestions, is_autocomplete_separator};
use crate::select::{SelectAction, SelectEvent, SelectItem};

pub(super) const DEFAULT_TRIGGER_CHARACTERS: [char; 2] = ['@', '#'];

/// Delay before an `@`/trigger-character request runs (TS `ATTACHMENT_AUTOCOMPLETE_DEBOUNCE_MS`).
pub const AUTOCOMPLETE_DEBOUNCE_MS: u64 = 20;

pub(super) struct Picker {
    pub(super) list: SelectList,
    prefix: String,
    force: bool,
}

/// Whether the text ends in a token that starts at a boundary (start or separator) with `head` and continues with
/// non-separators, or with `@"` followed by anything but a quote. `quoted_at` enables the `@"` form.
fn ends_with_trigger_token(text: &str, heads: &[char], quoted_at: bool) -> bool {
    let boundaries = std::iter::once(0).chain(
        text.char_indices()
            .filter(|&(_, c)| is_autocomplete_separator(c))
            .map(|(i, c)| i + c.len_utf8()),
    );
    for start in boundaries {
        let rest = &text[start..];
        if quoted_at && rest.strip_prefix("@\"").is_some_and(|q| !q.contains('"')) {
            return true;
        }
        let mut chars = rest.chars();
        if let Some(c) = chars.next()
            && heads.contains(&c)
            && !chars.as_str().chars().any(is_autocomplete_separator)
        {
            return true;
        }
    }
    false
}

fn trimmed(s: &str) -> &str {
    s.trim_matches(is_whitespace_char)
}

fn to_select_item(item: &AutocompleteItem) -> SelectItem {
    SelectItem {
        value: item.value.clone(),
        label: item.label.clone(),
        description: item.description.clone(),
    }
}

impl Editor {
    /// `setAutocompleteProvider`: also adds the provider's single-character triggers to `@` and `#`.
    pub fn set_autocomplete_provider(&mut self, provider: Box<dyn AutocompleteProvider>) {
        self.cancel_autocomplete();
        let mut chars = DEFAULT_TRIGGER_CHARACTERS.to_vec();
        for t in provider.trigger_characters() {
            let mut it = t.chars();
            let (Some(c), None) = (it.next(), it.next()) else {
                continue;
            };
            if c.len_utf16() != 1 || c == '/' || is_whitespace_char(c) || chars.contains(&c) {
                continue;
            }
            chars.push(c);
        }
        self.trigger_chars = chars;
        self.provider = Some(provider);
    }

    pub fn autocomplete_max_visible(&self) -> usize {
        self.autocomplete_max_visible
    }

    /// Clamped to 3..=20.
    pub fn set_autocomplete_max_visible(&mut self, max_visible: usize) {
        self.autocomplete_max_visible = max_visible.clamp(3, 20);
    }

    pub fn is_showing_autocomplete(&self) -> bool {
        self.picker.is_some()
    }

    /// Whether a debounced request waits for `fire_autocomplete_debounce`.
    pub fn has_pending_autocomplete(&self) -> bool {
        self.pending_request.is_some()
    }

    /// Run the debounced request (the TS timer firing).
    pub fn fire_autocomplete_debounce(&mut self) {
        if let Some((force, explicit_tab)) = self.pending_request.take() {
            self.run_autocomplete_request(force, explicit_tab);
        }
    }

    fn before_cursor(&self) -> &str {
        &self.line()[..self.cursor_col]
    }

    fn trigger_pattern(&self, text: &str) -> bool {
        ends_with_trigger_token(text, &self.trigger_chars, true)
    }

    fn debounce_pattern(&self, text: &str) -> bool {
        let without_at: Vec<char> = self.trigger_chars.iter().copied().filter(|&c| c != '@').collect();
        ends_with_trigger_token(text, &without_at, false) || ends_with_trigger_token(text, &['@'], true)
    }

    fn is_at_start_of_message(&self) -> bool {
        self.cursor_line == 0 && matches!(trimmed(self.before_cursor()), "" | "/")
    }

    fn is_in_slash_command_context(&self, before: &str) -> bool {
        self.cursor_line == 0 && before.trim_start_matches(is_whitespace_char).starts_with('/')
    }

    /// The trigger checks at the end of `insertCharacter`.
    pub(super) fn after_insert_character(&mut self, ch: &str) {
        if self.picker.is_some() {
            self.update_autocomplete();
            return;
        }
        let before = self.before_cursor().to_string();
        let single = {
            let mut it = ch.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        };
        if ch == "/" && self.is_at_start_of_message() {
            self.request_autocomplete(false, false);
        } else if single.is_some_and(|c| self.trigger_chars.contains(&c)) {
            if self.trigger_pattern(&before) {
                self.request_autocomplete(false, false);
            }
        } else if ch
            .chars()
            .any(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') || is_cjk_break(c))
            && (self.is_in_slash_command_context(&before) || self.trigger_pattern(&before))
        {
            self.request_autocomplete(false, false);
        }
    }

    /// The re-trigger checks at the end of backspace and forward delete.
    pub(super) fn after_delete(&mut self) {
        if self.picker.is_some() {
            self.update_autocomplete();
            return;
        }
        let before = self.before_cursor().to_string();
        if self.is_in_slash_command_context(&before) || self.trigger_pattern(&before) {
            self.request_autocomplete(false, false);
        }
    }

    pub(super) fn update_autocomplete(&mut self) {
        let Some(picker) = &self.picker else { return };
        if self.provider.is_none() {
            return;
        }
        let force = picker.force;
        self.request_autocomplete(force, false);
    }

    pub(super) fn handle_tab_completion(&mut self) {
        if self.provider.is_none() {
            return;
        }
        let before = self.before_cursor().to_string();
        if self.is_in_slash_command_context(&before) && !before.trim_start_matches(is_whitespace_char).contains(' ') {
            self.request_autocomplete(false, true);
        } else {
            self.request_autocomplete(true, true);
        }
    }

    fn request_autocomplete(&mut self, force: bool, explicit_tab: bool) {
        let Some(provider) = &self.provider else { return };
        if force && !provider.should_trigger_file_completion(&self.lines, self.cursor_line, self.cursor_col) {
            return;
        }
        self.pending_request = None;
        if !explicit_tab && !force && self.debounce_pattern(self.before_cursor()) {
            self.pending_request = Some((force, explicit_tab));
            return;
        }
        self.run_autocomplete_request(force, explicit_tab);
    }

    fn run_autocomplete_request(&mut self, force: bool, explicit_tab: bool) {
        let Some(provider) = &self.provider else { return };
        let suggestions = provider.get_suggestions(&self.lines, self.cursor_line, self.cursor_col, force);
        let Some(suggestions) = suggestions.filter(|s| !s.items.is_empty()) else {
            self.cancel_autocomplete();
            return;
        };
        if force && explicit_tab && suggestions.items.len() == 1 {
            let item = suggestions.items[0].clone();
            self.apply_item(&item, &suggestions.prefix);
            self.changed();
            return;
        }
        self.show_suggestions(suggestions, force);
    }

    fn show_suggestions(&mut self, suggestions: AutocompleteSuggestions, force: bool) {
        let items: Vec<SelectItem> = suggestions.items.iter().map(to_select_item).collect();
        let mut list = SelectList::new(items, self.autocomplete_max_visible, Rc::clone(&self.select_list_theme));
        if suggestions.prefix.starts_with('/') {
            list.min_primary_column_width = Some(12);
            list.max_primary_column_width = Some(32);
        }
        // Exact value match wins, else the first value starting with the prefix (case-sensitive).
        if !suggestions.prefix.is_empty() {
            let items = &suggestions.items;
            let best = items
                .iter()
                .position(|i| i.value == suggestions.prefix)
                .or_else(|| items.iter().position(|i| i.value.starts_with(&suggestions.prefix)));
            if let Some(i) = best {
                list.set_selected_index(i);
            }
        }
        self.picker = Some(Picker {
            list,
            prefix: suggestions.prefix,
            force,
        });
    }

    /// Apply `item` over `prefix` as one undo step and report the change.
    fn apply_item(&mut self, item: &AutocompleteItem, prefix: &str) {
        let Some(provider) = &self.provider else { return };
        let result = provider.apply_completion(&self.lines, self.cursor_line, self.cursor_col, item, prefix);
        self.push_undo();
        self.last_action = None;
        self.lines = result.lines;
        self.cursor_line = result.cursor_line;
        self.set_cursor_col(result.cursor_col);
    }

    /// Apply the picker's selected item; false when nothing is selected.
    fn apply_selected(&mut self) -> bool {
        let Some(picker) = &self.picker else { return false };
        let Some(selected) = picker.list.selected_item() else {
            return false;
        };
        let item = AutocompleteItem {
            value: selected.value.clone(),
            label: selected.label.clone(),
            description: selected.description.clone(),
        };
        let prefix = picker.prefix.clone();
        self.apply_item(&item, &prefix);
        true
    }

    /// Keys while the picker is open; true when the key should go on to normal handling.
    pub(super) fn handle_picker_key(&mut self, data: &str) -> bool {
        let kb = |id: &str| self.keys.matches(data, id);
        if kb("tui.select.cancel") {
            self.cancel_autocomplete();
            return false;
        }
        if kb("tui.select.up") || kb("tui.select.down") {
            let action = if kb("tui.select.up") {
                SelectAction::Up
            } else {
                SelectAction::Down
            };
            if let Some(picker) = &mut self.picker {
                picker.list.apply_action(action);
            }
            return false;
        }
        if kb("tui.input.tab") {
            if self.apply_selected() {
                self.cancel_autocomplete();
                self.changed();
            }
            return false;
        }
        if kb("tui.select.confirm") {
            let slash = self.picker.as_ref().is_some_and(|p| p.prefix.starts_with('/'));
            if self.apply_selected() {
                self.cancel_autocomplete();
                if slash {
                    // Enter on a slash command submits it.
                    return true;
                }
                self.changed();
                return false;
            }
        }
        true
    }

    pub(super) fn cancel_autocomplete(&mut self) {
        self.pending_request = None;
        self.picker = None;
    }

    /// `handleMouse`: picker rows pick completions; a left click on a text row moves the cursor there.
    pub fn handle_mouse(&mut self, event: &MouseEvent) -> Option<MouseResult> {
        let max_padding = event.width.saturating_sub(1) / 2;
        let padding_x = self.padding_x.min(max_padding);
        let picker_start = self.rendered_visible_lines as i64 + 2;
        if self.picker.is_some()
            && event.y >= picker_start
            && event.y < picker_start + self.rendered_picker_height as i64
        {
            let local = MouseEvent {
                x: event.x - padding_x as i64,
                y: event.y - picker_start,
                width: event.width.saturating_sub(padding_x * 2).max(1),
                height: self.rendered_picker_height,
                ..*event
            };
            let mut events = Vec::new();
            let result = self.picker.as_mut()?.list.handle_mouse(&local, &mut events);
            for e in events {
                if let SelectEvent::Selected(_) = e
                    && self.apply_selected()
                {
                    self.cancel_autocomplete();
                    self.changed();
                }
            }
            return result.map(|r| MouseResult { focus: Some(true), ..r });
        }
        if event.kind != MouseEventType::Click || event.button != MouseButton::Left {
            return None;
        }
        let focus = MouseResult {
            handled: Some(true),
            focus: Some(true),
            ..MouseResult::default()
        };
        if event.y <= 0 || event.y > self.rendered_visible_lines as i64 {
            return Some(focus);
        }
        let vls = self.build_visual_line_map(self.last_width);
        let index = self.scroll_offset + event.y as usize - 1;
        let Some(&vl) = vls.get(index) else {
            return Some(focus);
        };
        let line = self.lines[vl.logical].clone();
        let chunk = &line[vl.start..vl.start + vl.len];
        let target = (event.x - padding_x as i64).max(0) as usize;
        let mut column = 0;
        let mut target_index = chunk.len();
        let mut last_index = 0;
        for (i, g) in self.graphemes(chunk) {
            let next = column + visible_width(g);
            last_index = i;
            if target < next {
                target_index = i;
                break;
            }
            column = next;
        }
        let is_last = index == vls.len() - 1 || vls[index + 1].logical != vl.logical;
        if !is_last && target_index == chunk.len() && !chunk.is_empty() {
            target_index = last_index;
        }
        self.cursor_line = vl.logical;
        self.set_cursor_col(vl.start + target_index);
        self.last_action = None;
        self.exit_history_browsing();
        if self.picker.is_some() {
            self.update_autocomplete();
        }
        Some(focus)
    }
}
