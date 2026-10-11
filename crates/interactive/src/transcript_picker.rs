//! The `/copy` picker and the Esc-Esc rewind selector drawn over the real transcript (oh-my-pi
//! packages/tui/src/overlays/copy-selector.ts and rewind-selector.ts).
//!
//! Both replay the session's entries through a [`ChatView`] on the alternate screen, map every rendered item back
//! to the turn that produced it ([`OutlineTarget`]: the row -> entry map) and move a dotted outline over those
//! turns. `/copy` copies the outlined turn; Right steps into its fenced code, quotes, bash/eval/rlm commands, tool
//! output and links, each captioned with clickable `⧉ copy` / `↗ open` controls. Rewind picks the turn to continue
//! from: Left/Right jump between user turns and `f` filters turns by words.
//!
//! The picker owns no session: it returns [`PickerEvent`]s and the app copies, opens or rewinds.

use std::collections::HashMap;
use std::rc::Rc;

use fancy_regex::Regex;
use tui::components::Component;
use tui::highlight::{HighlightColors, highlight_lines, supports_language, warmed};
use tui::input::Input;
use tui::keybindings::KeybindingsManager;
use tui::outline::{
    Anchor, BrowserFrame, ComposedColumn, FrameBrowser, OutlineBuilder, OutlineEntry, OutlinePaint, OutlineStyle,
    OutlineTarget, append_outline_entries, compose_outline_column, key_glyph, nearest_visible_target, outline_rows,
    outline_visibility, strip_prompt_zones,
};
use tui::sgr_mouse::parse_sgr_mouse;
use tui::theme::Theme;
use tui::utils::{strip_terminal_sequences, truncate_to_width, visible_width};

use crate::chat::{ChatStyle, ChatView};
use crate::components::{bold, fg};
use crate::copy_targets::{CopyBlock, SEP_DOT, TranscriptEntry, collect_blocks, is_user_turn, target_copy};

/// Preview rows per block in the descended view; copy always takes the full text.
pub const BLOCK_PREVIEW_LINES: usize = 12;
/// The copy picker's outline is green, distinct from rewind's accent.
const COPY_OUTLINE: &str = "success";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerMode {
    Copy,
    Rewind,
}

/// What the owner should do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PickerEvent {
    /// The key is not the picker's.
    Ignored,
    /// State changed; paint.
    Render,
    /// Put `content` on the clipboard. `block` is the inner block index when descended.
    Copy {
        content: String,
        label: String,
        entry_id: String,
        block: Option<usize>,
    },
    /// Open `href` with the system opener.
    Open {
        href: String,
        label: String,
    },
    /// Rewind the session to continue from `entry_id`.
    Rewind {
        entry_id: String,
    },
    Cancel,
}

/// A clickable caption control: `(copy|open, block, start column, end column)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Copy,
    Open,
}

/// Replays entries into a [`ChatView`]; each item is one outline child.
struct Replay<'a> {
    chat: &'a mut ChatView,
    entries: &'a [TranscriptEntry],
}

/// Add one entry to the chat the way the live transcript shows it. Entries the chat has no item for (bash
/// executions, summaries, custom notices) become notices; empty ones render nothing and are skipped.
fn replay_entry(chat: &mut ChatView, entry: &TranscriptEntry) {
    let m = &entry.message;
    let s = |k: &str| m[k].as_str().unwrap_or("").to_string();
    match entry.role() {
        "user" | "assistant" | "toolResult" => chat.add_message(m),
        "bashExecution" => {
            let output = s("output");
            let body = if output.trim().is_empty() {
                format!("$ {}", s("command"))
            } else {
                format!("$ {}\n{}", s("command"), output.trim_end())
            };
            chat.append_notice_text(&body);
        }
        "compactionSummary" => chat.append_notice_text(&format!("Compaction summary\n{}", s("summary"))),
        "branchSummary" => chat.append_notice_text(&format!("Branch summary\n{}", s("summary"))),
        "custom" => {
            let text = crate::chat::user_message_text(m);
            if !text.trim().is_empty() {
                chat.append_notice_text(&text);
            }
        }
        _ => {}
    }
}

impl OutlineBuilder for Replay<'_> {
    fn child_count(&self) -> usize {
        self.chat.item_count()
    }
    fn append(&mut self, index: usize) {
        replay_entry(self.chat, &self.entries[index]);
    }
}

pub struct PickerOptions {
    pub mode: PickerMode,
    pub theme: Rc<Theme>,
    /// Resolved app bindings (`native_app_keybindings`).
    pub keys: KeybindingsManager,
    pub chat: ChatStyle,
    /// Replaces the `Copy` / `Rewind` header.
    pub title: Option<String>,
    /// Verb of the pick action (default `copy`).
    pub action_label: Option<String>,
}

pub struct TranscriptPicker {
    mode: PickerMode,
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    title: Option<String>,
    action: String,
    entries: Vec<TranscriptEntry>,
    chat: ChatView,
    targets: Vec<OutlineTarget>,
    selected: usize,
    visible: Vec<bool>,
    expanded: bool,
    blocks: Option<Vec<CopyBlock>>,
    block_selected: usize,
    block_cache: HashMap<String, Vec<CopyBlock>>,
    /// Click targets of the last frame by body line: `(control, block, start col, end col)`.
    controls: HashMap<usize, Vec<(Control, usize, usize, usize)>>,
    browser: FrameBrowser,
    filter: Option<Input>,
    /// Rendered child rows of the last frame (the rewind filter matches what is on screen).
    child_rows: Vec<Vec<String>>,
    width: usize,
}

impl TranscriptPicker {
    pub fn new(entries: Vec<TranscriptEntry>, options: PickerOptions) -> Self {
        let mut chat = ChatView::new(options.chat);
        let outline: Vec<OutlineEntry> = entries
            .iter()
            .map(|e| OutlineEntry {
                id: e.id.clone(),
                is_tool_result: e.role() == "toolResult",
                is_user_turn: is_user_turn(&e.message),
            })
            .collect();
        let targets = append_outline_entries(
            &mut Replay {
                chat: &mut chat,
                entries: &entries,
            },
            &outline,
            0,
        );
        let selected = targets.len().saturating_sub(1);
        TranscriptPicker {
            mode: options.mode,
            theme: options.theme,
            keys: options.keys,
            title: options.title,
            action: options.action_label.unwrap_or_else(|| "copy".into()),
            entries,
            chat,
            targets,
            selected,
            visible: Vec::new(),
            expanded: false,
            blocks: None,
            block_selected: 0,
            block_cache: HashMap::new(),
            controls: HashMap::new(),
            browser: FrameBrowser::default(),
            filter: None,
            child_rows: Vec::new(),
            width: 0,
        }
    }

    /// Selectable turns; the app does not open the picker when this is zero.
    pub fn target_count(&self) -> usize {
        self.targets.len()
    }

    pub fn targets(&self) -> &[OutlineTarget] {
        &self.targets
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The descended view's blocks and selection.
    pub fn blocks(&self) -> Option<(&[CopyBlock], usize)> {
        self.blocks.as_deref().map(|b| (b, self.block_selected))
    }

    pub fn filter_query(&self) -> Option<&str> {
        self.filter.as_ref().map(Input::value)
    }

    fn blocks_for(&mut self, target: usize) -> Vec<CopyBlock> {
        let t = &self.targets[target];
        if let Some(cached) = self.block_cache.get(&t.turn_id) {
            return cached.clone();
        }
        let blocks = collect_blocks(&self.entries, &t.entries);
        self.block_cache.insert(t.turn_id.clone(), blocks.clone());
        blocks
    }

    fn k(&self, data: &str, id: &str) -> bool {
        self.keys.matches(data, id)
    }

    pub fn handle_input(&mut self, data: &str) -> PickerEvent {
        if let Some(mouse) = parse_sgr_mouse(data) {
            if let Some(wheel) = mouse.wheel {
                return if self.browser.body.scroll(wheel * 3) {
                    PickerEvent::Render
                } else {
                    PickerEvent::Ignored
                };
            }
            if mouse.left_click && self.mode == PickerMode::Copy {
                return self.click(mouse.row, mouse.col);
            }
            return PickerEvent::Ignored;
        }
        if self.filter.is_some() {
            return self.handle_filter_input(data);
        }
        if self.k(data, "tui.select.cancel") {
            if self.blocks.is_some() {
                return self.ascend();
            }
            return PickerEvent::Cancel;
        }
        if self.k(data, "app.tools.expand") {
            self.expanded = !self.expanded;
            self.chat.set_tools_expanded(self.expanded);
            return PickerEvent::Render;
        }
        if self.k(data, "tui.select.up") {
            return self.move_vertical(-1);
        }
        if self.k(data, "tui.select.down") {
            return self.move_vertical(1);
        }
        match self.mode {
            PickerMode::Copy => {
                if self.k(data, "app.copy.blocks") {
                    return self.descend();
                }
                if self.k(data, "app.copy.back") {
                    return if self.blocks.is_some() {
                        self.ascend()
                    } else {
                        PickerEvent::Ignored
                    };
                }
                if self.k(data, "app.copy.open") {
                    return self.open_selected_block();
                }
                if self.k(data, "tui.select.confirm") {
                    return self.pick_outlined();
                }
            }
            PickerMode::Rewind => {
                if self.k(data, "app.rewind.filter") {
                    self.open_filter();
                    return PickerEvent::Render;
                }
                if self.k(data, "app.rewind.prevUser") {
                    return self.move_to(-1, true);
                }
                if self.k(data, "app.rewind.nextUser") {
                    return self.move_to(1, true);
                }
                if self.k(data, "tui.select.confirm") {
                    return match self.targets.get(self.selected) {
                        Some(t) => PickerEvent::Rewind {
                            entry_id: t.entry_id.clone(),
                        },
                        None => PickerEvent::Ignored,
                    };
                }
            }
        }
        self.scroll_key(data)
    }

    /// Page/home/end/shift+arrow scroll without moving the selection.
    fn scroll_key(&mut self, data: &str) -> PickerEvent {
        let body = &mut self.browser.body;
        let moved = if self.keys.matches(data, "tui.altScreen.pageUp") {
            body.page(-1)
        } else if self.keys.matches(data, "tui.altScreen.pageDown") {
            body.page(1)
        } else if self.keys.matches(data, "tui.altScreen.top") {
            body.to_top()
        } else if self.keys.matches(data, "tui.altScreen.bottom") {
            body.to_bottom()
        } else if self.keys.matches(data, "app.transcript.scrollUp") {
            body.scroll(-5)
        } else if self.keys.matches(data, "app.transcript.scrollDown") {
            body.scroll(5)
        } else {
            return PickerEvent::Ignored;
        };
        if moved {
            PickerEvent::Render
        } else {
            PickerEvent::Ignored
        }
    }

    fn move_vertical(&mut self, delta: i64) -> PickerEvent {
        if let Some(blocks) = &self.blocks {
            let next = self.block_selected as i64 + delta;
            if next >= 0 && (next as usize) < blocks.len() {
                self.block_selected = next as usize;
                return PickerEvent::Render;
            }
            return PickerEvent::Ignored;
        }
        self.move_to(delta, false)
    }

    /// Step to the nearest visible target (a user turn when `user_only`).
    fn move_to(&mut self, delta: i64, user_only: bool) -> PickerEvent {
        let mut index = self.selected as i64 + delta;
        while index >= 0 && (index as usize) < self.targets.len() {
            let i = index as usize;
            if self.visible.get(i).copied().unwrap_or(true) && (!user_only || self.targets[i].is_user_turn) {
                self.selected = i;
                return PickerEvent::Render;
            }
            index += delta;
        }
        PickerEvent::Ignored
    }

    fn descend(&mut self) -> PickerEvent {
        if self.blocks.is_some() || self.targets.is_empty() {
            return PickerEvent::Ignored;
        }
        let blocks = self.blocks_for(self.selected);
        if blocks.is_empty() {
            return PickerEvent::Ignored;
        }
        self.blocks = Some(blocks);
        self.block_selected = 0;
        PickerEvent::Render
    }

    fn ascend(&mut self) -> PickerEvent {
        self.blocks = None;
        self.block_selected = 0;
        PickerEvent::Render
    }

    fn block_event(&self, block: &CopyBlock, index: usize) -> PickerEvent {
        PickerEvent::Copy {
            content: block.content.clone(),
            label: block.label.clone(),
            entry_id: self.entries[block.entry].id.clone(),
            block: Some(index),
        }
    }

    fn pick_outlined(&mut self) -> PickerEvent {
        if let Some(blocks) = &self.blocks {
            return match blocks.get(self.block_selected) {
                Some(b) => self.block_event(b, self.block_selected),
                None => PickerEvent::Ignored,
            };
        }
        let Some(target) = self.targets.get(self.selected) else {
            return PickerEvent::Ignored;
        };
        let first = target.entries[0];
        let blocks = self.blocks_for(self.selected);
        let (content, label) = target_copy(&self.entries, first, &blocks);
        PickerEvent::Copy {
            content,
            label,
            entry_id: self.entries[first].id.clone(),
            block: None,
        }
    }

    fn open_selected_block(&self) -> PickerEvent {
        match self.blocks.as_ref().and_then(|b| b.get(self.block_selected)) {
            Some(CopyBlock {
                href: Some(href),
                label,
                ..
            }) => PickerEvent::Open {
                href: href.clone(),
                label: label.clone(),
            },
            _ => PickerEvent::Ignored,
        }
    }

    fn click(&mut self, row: i64, col: i64) -> PickerEvent {
        if self.blocks.is_none() {
            return PickerEvent::Ignored;
        }
        let Some((line, col)) = self.browser.to_body_point(row, col) else {
            return PickerEvent::Ignored;
        };
        let Some(hit) = self
            .controls
            .get(&line)
            .and_then(|regions| regions.iter().find(|r| col >= r.2 && col < r.3).copied())
        else {
            return PickerEvent::Ignored;
        };
        self.block_selected = hit.1;
        let block = self.blocks.as_ref().and_then(|b| b.get(hit.1)).cloned();
        match (hit.0, block) {
            (
                Control::Open,
                Some(CopyBlock {
                    href: Some(href),
                    label,
                    ..
                }),
            ) => PickerEvent::Open { href, label },
            (Control::Copy, Some(b)) => self.block_event(&b, hit.1),
            _ => PickerEvent::Ignored,
        }
    }

    // Rewind filter.

    fn open_filter(&mut self) {
        let input = Input::new(self.keys.clone()).with_options(
            Some(&format!("{} ", fg(&self.theme, "accent", "filter:"))),
            Some("words…"),
            None,
            None,
        );
        let mut input = input;
        input.focused = true;
        self.filter = Some(input);
    }

    fn handle_filter_input(&mut self, data: &str) -> PickerEvent {
        if self.k(data, "tui.select.cancel") {
            self.filter = None;
            return PickerEvent::Render;
        }
        if self.k(data, "tui.select.confirm") {
            let matches = self.filter_matches();
            return match self
                .targets
                .get(self.selected)
                .filter(|_| matches.contains(&self.selected))
            {
                Some(t) => PickerEvent::Rewind {
                    entry_id: t.entry_id.clone(),
                },
                None => PickerEvent::Ignored,
            };
        }
        if self.k(data, "app.tools.expand") {
            self.expanded = !self.expanded;
            self.chat.set_tools_expanded(self.expanded);
            return PickerEvent::Render;
        }
        for (id, delta, user_only) in [
            ("tui.select.up", -1, false),
            ("app.rewind.prevUser", -1, true),
            ("tui.select.down", 1, false),
            ("app.rewind.nextUser", 1, true),
        ] {
            if self.k(data, id) {
                return self.step_filtered(delta, user_only);
            }
        }
        let Some(input) = self.filter.as_mut() else {
            return PickerEvent::Ignored;
        };
        let before = input.value().to_string();
        if before.is_empty() && self.keys.matches(data, "tui.editor.deleteCharBackward") {
            self.filter = None;
            return PickerEvent::Render;
        }
        input.handle_input(data);
        if input.value() != before {
            let matches = self.filter_matches();
            if !matches.contains(&self.selected) {
                self.selected = matches
                    .iter()
                    .rev()
                    .find(|&&i| i < self.selected)
                    .or(matches.last())
                    .copied()
                    .unwrap_or(self.selected);
            }
        }
        PickerEvent::Render
    }

    fn step_filtered(&mut self, delta: i64, user_only: bool) -> PickerEvent {
        let matches: Vec<usize> = self
            .filter_matches()
            .into_iter()
            .filter(|&i| !user_only || self.targets[i].is_user_turn)
            .collect();
        let next = if delta < 0 {
            matches.iter().rev().find(|&&i| i < self.selected)
        } else {
            matches.iter().find(|&&i| i > self.selected)
        };
        match next {
            Some(&i) => {
                self.selected = i;
                PickerEvent::Render
            }
            None => PickerEvent::Ignored,
        }
    }

    /// Visible targets whose rendered rows contain every query word: Latin words match whole words
    /// (case-insensitive: `ls` matches `ls -la`, not `tools`), other scripts match substrings.
    pub fn filter_matches(&self) -> Vec<usize> {
        let query = self
            .filter
            .as_ref()
            .map(|f| f.value().to_lowercase())
            .unwrap_or_default();
        let patterns: Vec<Regex> = query
            .split_whitespace()
            .filter_map(|word| {
                let escaped = fancy_regex::escape(word);
                let latin = word.chars().all(|c| c == '_' || c.is_numeric() || is_latin(c));
                let source = if latin {
                    format!(r"(?<![\p{{L}}\p{{N}}_]){escaped}(?![\p{{L}}\p{{N}}_])")
                } else {
                    escaped.into_owned()
                };
                Regex::new(&source).ok()
            })
            .collect();
        (0..self.targets.len())
            .filter(|&i| self.visible.get(i).copied().unwrap_or(true))
            .filter(|&i| {
                let t = &self.targets[i];
                patterns.iter().all(|p| {
                    (t.start..t.end).any(|c| {
                        self.child_rows.get(c).is_some_and(|rows| {
                            p.is_match(&strip_terminal_sequences(&rows.join("\n")).to_lowercase())
                                .unwrap_or(false)
                        })
                    })
                })
            })
            .collect()
    }

    // Render.

    fn painter(&self) -> impl Fn(&str, &str) -> String + 'static {
        let theme = Rc::clone(&self.theme);
        move |c: &str, s: &str| fg(&theme, c, s)
    }

    fn first_key(&self, id: &str) -> String {
        self.keys.get_keys(id).first().map(|k| key_glyph(k)).unwrap_or_default()
    }

    fn keys_pair(&self, a: &str, b: &str) -> String {
        format!("{}/{}", self.first_key(a), self.first_key(b))
    }

    /// The full screen at `width` x `height`.
    pub fn render(&mut self, width: usize, height: usize) -> Vec<String> {
        let width = width.max(1);
        self.width = width;
        let rows: Vec<Vec<String>> = self
            .chat
            .render_item_rows(width)
            .iter()
            .map(|r| strip_prompt_zones(r))
            .collect();
        self.visible = outline_visibility(&rows, &self.targets);
        if let Some(s) = nearest_visible_target(&self.visible, Some(self.selected))
            && s != self.selected
        {
            self.selected = s;
            self.blocks = None;
            self.block_selected = 0;
        }
        self.child_rows = rows;
        let frame = match (self.mode, self.filter.is_some()) {
            (PickerMode::Rewind, true) => self.filter_frame(width),
            (PickerMode::Rewind, false) => self.rewind_frame(width),
            (PickerMode::Copy, _) => self.copy_frame(width),
        };
        self.browser.render(&frame, width, height)
    }

    fn header(&self) -> String {
        let t = &self.theme;
        match (&self.title, self.mode) {
            (Some(title), _) => bold(title),
            (None, PickerMode::Copy) => format!(
                "⧉ {}{}",
                bold("Copy"),
                fg(t, "dim", &format!("{SEP_DOT}pick what to put on the clipboard"))
            ),
            (None, PickerMode::Rewind) => format!(
                "↶ {}{}",
                bold("Rewind"),
                fg(t, "dim", &format!("{SEP_DOT}pick the point to continue from"))
            ),
        }
    }

    fn anchor(&self, column: &ComposedColumn, id: String) -> Option<Anchor> {
        column.selection.map(|(start, end)| Anchor { id, start, end })
    }

    fn copy_frame(&mut self, width: usize) -> BrowserFrame {
        self.controls.clear();
        let blocks = if self.targets.is_empty() {
            Vec::new()
        } else {
            self.blocks_for(self.selected)
        };
        let target = self.targets.get(self.selected).cloned();
        let fg_fn = self.painter();
        let bold_fn = |s: &str| bold(s);
        let paint = OutlinePaint {
            fg: &fg_fn,
            bold: &bold_fn,
        };
        let n = self.child_rows.len();
        let stack = self.blocks.clone();
        let (column, anchor_id) = match (&stack, &target) {
            (Some(stack), Some(t)) => {
                let before = compose_outline_column(
                    &paint,
                    &self.child_rows,
                    0,
                    t.start,
                    &[],
                    None,
                    width,
                    None,
                    &OutlineStyle::default(),
                );
                let (stack_lines, selection) = self.compose_blocks(&paint, stack, width, before.lines.len());
                let after = compose_outline_column(
                    &paint,
                    &self.child_rows,
                    t.end,
                    n,
                    &[],
                    None,
                    width,
                    None,
                    &OutlineStyle::default(),
                );
                let offset = before.lines.len();
                let mut lines = before.lines;
                lines.extend(stack_lines);
                lines.extend(after.lines);
                let column = ComposedColumn {
                    lines,
                    selection: selection.map(|(s, e)| (offset + s, offset + e)),
                };
                (column, format!("copy:{}:block:{}", t.turn_id, self.block_selected))
            }
            _ => {
                let caption = (!blocks.is_empty()).then(|| {
                    format!(
                        "{} block{} {}",
                        blocks.len(),
                        if blocks.len() == 1 { "" } else { "s" },
                        self.first_key("app.copy.blocks")
                    )
                });
                let style = OutlineStyle {
                    color: Some(COPY_OUTLINE.into()),
                    caption,
                };
                let column = compose_outline_column(
                    &paint,
                    &self.child_rows,
                    0,
                    n,
                    &self.targets,
                    target.is_some().then_some(self.selected),
                    width,
                    None,
                    &style,
                );
                let id = target
                    .as_ref()
                    .map(|t| format!("copy:{}", t.turn_id))
                    .unwrap_or_default();
                (column, id)
            }
        };
        let up_down = self.keys_pair("tui.select.up", "tui.select.down");
        let enter = self.first_key("tui.select.confirm");
        let cancel = self.first_key("tui.select.cancel");
        let hint = match &self.blocks {
            Some(stack) => {
                let open = stack
                    .get(self.block_selected)
                    .filter(|b| b.href.is_some())
                    .map(|_| format!("  {} open", self.first_key("app.copy.open")))
                    .unwrap_or_default();
                format!(
                    "{}/{}  {up_down} block  {}/{cancel} back  {enter} {}{open}  click ⧉/↗",
                    self.block_selected + 1,
                    stack.len(),
                    self.first_key("app.copy.back"),
                    self.action
                )
            }
            None => {
                let position = if self.targets.is_empty() {
                    String::new()
                } else {
                    format!("{}/{}  ", self.selected + 1, self.targets.len())
                };
                let right = if blocks.is_empty() {
                    String::new()
                } else {
                    format!("{} blocks  ", self.first_key("app.copy.blocks"))
                };
                format!(
                    "{position}{up_down} step  {right}{enter} {}  {} expand  {cancel} close",
                    self.action,
                    self.first_key("app.tools.expand")
                )
            }
        };
        let anchor = self.anchor(&column, anchor_id);
        BrowserFrame {
            header: vec![self.header()],
            body: column.lines,
            anchor,
            footer: vec![fg(&self.theme, "dim", &hint)],
        }
    }

    /// The selected turn exploded into captioned block previews, the selected block outlined. Caption controls are
    /// recorded by body line (`line_offset` + local line).
    fn compose_blocks(
        &mut self,
        paint: &OutlinePaint,
        blocks: &[CopyBlock],
        width: usize,
        line_offset: usize,
    ) -> (Vec<String>, Option<(usize, usize)>) {
        let inner = width.saturating_sub(4).max(10);
        let colors = HighlightColors::from_theme(&self.theme);
        let mut lines = Vec::new();
        let mut selection = None;
        for (index, block) in blocks.iter().enumerate() {
            let raw: Vec<&str> = block.content.split('\n').collect();
            let shown = &raw[..raw.len().min(BLOCK_PREVIEW_LINES)];
            let styled: Vec<String> = match block.language.as_deref() {
                Some(lang) if warmed() && supports_language(lang) => {
                    highlight_lines(&shown.join("\n"), Some(lang), &colors)
                }
                _ => shown.iter().map(|s| s.to_string()).collect(),
            };
            let mut rows: Vec<String> = styled
                .iter()
                .map(|r| truncate_to_width(&r.replace('\t', "   "), inner, "…", false))
                .collect();
            if raw.len() > shown.len() {
                rows.push(fg(
                    &self.theme,
                    "dim",
                    &format!("… +{} more lines", raw.len() - shown.len()),
                ));
            }
            let selected = index == self.block_selected;
            let caption_color = if selected { COPY_OUTLINE } else { "dim" };
            let mut controls = vec![(Control::Copy, format!("⧉ {}", self.action))];
            if block.href.is_some() {
                controls.push((Control::Open, "↗ open".to_string()));
            }
            let controls_width: usize = controls.iter().map(|c| visible_width(&c.1) + 2).sum();
            let count = raw.len();
            let summary = truncate_to_width(
                &format!(
                    "{}/{}{SEP_DOT}{}{SEP_DOT}{count} line{}",
                    index + 1,
                    blocks.len(),
                    block.label,
                    if count == 1 { "" } else { "s" }
                ),
                inner.saturating_sub(controls_width).max(4),
                "…",
                false,
            );
            let mut caption = fg(&self.theme, caption_color, &summary);
            let mut cursor = 2 + visible_width(&summary);
            let mut regions = Vec::new();
            for (kind, text) in &controls {
                cursor += 2;
                regions.push((*kind, index, cursor, cursor + visible_width(text)));
                caption.push_str(&format!("  {}", fg(&self.theme, "accent", text)));
                cursor += visible_width(text);
            }
            lines.push(String::new());
            self.controls.insert(line_offset + lines.len(), regions);
            if selected {
                let start = lines.len();
                lines.push(format!("  {caption}"));
                let style = OutlineStyle {
                    color: Some(COPY_OUTLINE.into()),
                    caption: None,
                };
                lines.extend(outline_rows(paint, &rows, inner, &style));
                selection = Some((start, lines.len()));
            } else {
                lines.push(format!("  {caption}"));
                lines.extend(
                    rows.into_iter()
                        .map(|r| if r.is_empty() { r } else { format!("  {r}") }),
                );
            }
        }
        lines.push(String::new());
        (lines, selection)
    }

    fn rewind_frame(&mut self, width: usize) -> BrowserFrame {
        let fg_fn = self.painter();
        let bold_fn = |s: &str| bold(s);
        let paint = OutlinePaint {
            fg: &fg_fn,
            bold: &bold_fn,
        };
        let has = !self.targets.is_empty();
        let column = compose_outline_column(
            &paint,
            &self.child_rows,
            0,
            self.child_rows.len(),
            &self.targets,
            has.then_some(self.selected),
            width,
            None,
            &OutlineStyle::default(),
        );
        let position = if has {
            format!("{}/{}  ", self.selected + 1, self.targets.len())
        } else {
            String::new()
        };
        let hint = format!(
            "{position}{} step  {} user turns  {} filter  {} rewind  {} expand  {} cancel",
            self.keys_pair("tui.select.up", "tui.select.down"),
            self.keys_pair("app.rewind.prevUser", "app.rewind.nextUser"),
            self.first_key("app.rewind.filter"),
            self.first_key("tui.select.confirm"),
            self.first_key("app.tools.expand"),
            self.first_key("tui.select.cancel"),
        );
        let id = self
            .targets
            .get(self.selected)
            .map(|t| format!("rewind:main:{}", t.turn_id));
        BrowserFrame {
            header: vec![self.header()],
            anchor: id.and_then(|id| self.anchor(&column, id)),
            body: column.lines,
            footer: vec![fg(&self.theme, "dim", &hint)],
        }
    }

    /// Only the matching turns, concatenated.
    fn filter_frame(&mut self, width: usize) -> BrowserFrame {
        let matches = self.filter_matches();
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut targets = Vec::new();
        for &i in &matches {
            let t = &self.targets[i];
            let start = rows.len();
            rows.extend(self.child_rows[t.start..t.end].iter().cloned());
            targets.push(OutlineTarget {
                start,
                end: rows.len(),
                ..t.clone()
            });
        }
        let selected = matches.iter().position(|&i| i == self.selected);
        let query = self.filter.as_ref().map(|f| f.value().to_string()).unwrap_or_default();
        let fg_fn = self.painter();
        let bold_fn = |s: &str| bold(s);
        let paint = OutlinePaint {
            fg: &fg_fn,
            bold: &bold_fn,
        };
        let column = compose_outline_column(
            &paint,
            &rows,
            0,
            rows.len(),
            &targets,
            selected,
            width,
            None,
            &OutlineStyle::default(),
        );
        let t = Rc::clone(&self.theme);
        let lines = if matches.is_empty() {
            vec![fg(&t, "muted", &format!("  No items match \"{query}\""))]
        } else {
            column.lines.clone()
        };
        let count = if matches.is_empty() {
            fg(&t, "error", "no matches")
        } else {
            fg(
                &t,
                "dim",
                &format!(
                    "{}/{}",
                    selected.map_or("-".to_string(), |s| (s + 1).to_string()),
                    matches.len()
                ),
            )
        };
        let hint = format!(
            "{} step  {} user turns  {} rewind  {} show all",
            self.keys_pair("tui.select.up", "tui.select.down"),
            self.keys_pair("app.rewind.prevUser", "app.rewind.nextUser"),
            self.first_key("tui.select.confirm"),
            self.first_key("tui.select.cancel"),
        );
        let input_width = visible_width(&format!("filter: {query}")) + 1;
        let field = self
            .filter
            .as_mut()
            .map(|f| f.render(input_width).into_iter().next().unwrap_or_default())
            .unwrap_or_default();
        let id = format!(
            "rewind:filter:{query}:{}",
            self.targets
                .get(self.selected)
                .map_or(String::new(), |t| t.turn_id.clone())
        );
        BrowserFrame {
            header: vec![self.header()],
            anchor: self.anchor(&column, id),
            body: lines,
            footer: vec![format!("{field}  {count}  {}", fg(&t, "dim", &hint))],
        }
    }
}

fn is_latin(c: char) -> bool {
    // `\p{Script=Latin}`: ASCII letters and the Latin-1/Extended blocks.
    c.is_ascii_alphabetic()
        || matches!(c as u32, 0xAA | 0xBA | 0xC0..=0xD6 | 0xD8..=0xF6 | 0xF8..=0x24F | 0x1E00..=0x1EFF | 0x2C60..=0x2C7F | 0xA720..=0xA7FF | 0xFF21..=0xFF3A | 0xFF41..=0xFF5A)
}
