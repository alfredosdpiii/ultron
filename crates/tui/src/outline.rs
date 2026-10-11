//! The fullscreen transcript-selector engine shared by `/copy`, Esc-Esc rewind and the agent transcript viewer
//! (oh-my-pi packages/tui/src/chat/transcript-outline.ts, chat/transcript-browser.ts and the row arithmetic of
//! components/scroll-viewport.ts / scroll-view.ts).
//!
//! The owner replays the transcript into per-item rows ("children"), maps each turn to the half-open child range
//! it rendered ([`OutlineTarget`], built by [`append_outline_entries`]), and composes one column of gutter-prefixed
//! rows with a dotted outline around the selected turn ([`compose_outline_column`]). [`FrameBrowser`] lays a
//! header, the scrolled body and a footer over the whole screen and keeps the selection revealed.

use crate::utils::{is_whitespace_char, truncate_to_width, visible_width};

/// One selectable transcript item: a turn plus its rendered child range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlineTarget {
    /// Entry the selection resolves to; extended over folded tool results.
    pub entry_id: String,
    /// Entry that opened the turn.
    pub turn_id: String,
    /// A real user prompt (drives user-turn jumps).
    pub is_user_turn: bool,
    /// First child rendered by the turn.
    pub start: usize,
    /// One past its last child.
    pub end: usize,
    /// Indices (into the owner's entry list) of the entries the target spans.
    pub entries: Vec<usize>,
}

/// What [`append_outline_entries`] needs to know about one entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutlineEntry {
    pub id: String,
    /// A `toolResult` message: folds into the turn above.
    pub is_tool_result: bool,
    pub is_user_turn: bool,
}

/// The transcript builder the targets are measured against.
pub trait OutlineBuilder {
    /// Children rendered so far.
    fn child_count(&self) -> usize;
    /// Append entry `index`.
    fn append(&mut self, index: usize);
    /// A usage row flushed at the head of an append (attributed to the turn above).
    fn is_usage_row(&self, _child: usize) -> bool {
        false
    }
}

/// `appendOutlineEntries`: append `entries` (indices `first..`) and return the targets they produce. Tool results
/// fold into the previous target; entries that render nothing are skipped.
pub fn append_outline_entries(
    builder: &mut dyn OutlineBuilder,
    entries: &[OutlineEntry],
    first: usize,
) -> Vec<OutlineTarget> {
    let mut targets: Vec<OutlineTarget> = Vec::new();
    for (offset, entry) in entries.iter().enumerate() {
        let index = first + offset;
        let before = builder.child_count();
        builder.append(index);
        let after = builder.child_count();
        let mut start = before;
        while start < after && builder.is_usage_row(start) {
            if let Some(previous) = targets.last_mut()
                && previous.end == start
            {
                previous.end = start + 1;
            }
            start += 1;
        }
        if entry.is_tool_result
            && let Some(previous) = targets.last_mut()
        {
            previous.entry_id = entry.id.clone();
            previous.entries.push(index);
            if after > previous.end {
                previous.end = after;
            }
            continue;
        }
        if start >= after {
            continue;
        }
        targets.push(OutlineTarget {
            entry_id: entry.id.clone(),
            turn_id: entry.id.clone(),
            is_user_turn: entry.is_user_turn,
            start,
            end: after,
            entries: vec![index],
        });
    }
    targets
}

/// JS `/\S/.test(row)`.
pub fn has_non_blank(row: &str) -> bool {
    row.chars().any(|c| !is_whitespace_char(c))
}

/// `outlineVisibility`: per target, whether it renders at least one non-blank row.
pub fn outline_visibility(child_rows: &[Vec<String>], targets: &[OutlineTarget]) -> Vec<bool> {
    targets
        .iter()
        .map(|t| (t.start..t.end).any(|i| child_rows[i].iter().any(|r| has_non_blank(r))))
        .collect()
}

/// `stripPromptZones`: OSC 133 prompt marks must not latch onto alternate-screen rows.
pub fn strip_prompt_zones(rows: &[String]) -> Vec<String> {
    rows.iter()
        .map(|row| {
            if !row.contains("\x1b]133;") {
                return row.clone();
            }
            let mut out = String::with_capacity(row.len());
            let mut rest = row.as_str();
            while let Some(at) = rest.find("\x1b]133;") {
                let tail = &rest[at + 6..];
                // `[^\x07\x1b]*(?:\x07|\x1b\\)`: no terminator means no match; keep the text as is.
                let stop = tail.find(['\x07', '\x1b']);
                match stop {
                    Some(i) if tail.as_bytes()[i] == 0x07 => {
                        out.push_str(&rest[..at]);
                        rest = &tail[i + 1..];
                    }
                    Some(i) if tail[i..].starts_with("\x1b\\") => {
                        out.push_str(&rest[..at]);
                        rest = &tail[i + 2..];
                    }
                    _ => {
                        out.push_str(&rest[..at + 6]);
                        rest = tail;
                    }
                }
            }
            out.push_str(rest);
            out
        })
        .collect()
}

/// Theme access for the outline: `fg(color, text)` and `bold(text)`.
pub struct OutlinePaint<'a> {
    pub fg: &'a dyn Fn(&str, &str) -> String,
    pub bold: &'a dyn Fn(&str) -> String,
}

/// Stroke color and an optional caption inset into the top rule.
#[derive(Clone, Debug, Default)]
pub struct OutlineStyle {
    /// Theme color; `accent` when unset.
    pub color: Option<String>,
    pub caption: Option<String>,
}

pub const DOTTED_HORIZONTAL: &str = "┄";
pub const DOTTED_VERTICAL: &str = "┆";

/// omp `padToWidth`: pad to exactly `width` cells, truncating with `…` when wider.
pub fn pad_to_width(text: &str, width: usize) -> String {
    if width == 0 {
        return text.to_string();
    }
    let w = visible_width(text);
    if w == width {
        return text.to_string();
    }
    let fitted = if w < width {
        format!("{text}{}", " ".repeat(width - w))
    } else {
        truncate_to_width(text, width, "…", false)
    };
    let drift = width.saturating_sub(visible_width(&fitted));
    format!("{fitted}{}", " ".repeat(drift))
}

/// `outlineRule`: a dotted rule with rounded corners spanning the outline.
pub fn outline_rule(
    paint: &OutlinePaint,
    left: &str,
    right: &str,
    inner_width: usize,
    color: &str,
    caption: Option<&str>,
) -> String {
    let label = caption.map(|c| format!(" {c} ")).unwrap_or_default();
    let fill = (inner_width + 2).saturating_sub(visible_width(&label));
    format!(
        "{}{}{}",
        (paint.fg)(color, &format!("{left}{}", DOTTED_HORIZONTAL.repeat(fill))),
        (paint.bold)(&(paint.fg)(color, &label)),
        (paint.fg)(color, right)
    )
}

/// `outlineRows`: `rows` inside the dotted outline (rules above and below, `┆` sides).
pub fn outline_rows(paint: &OutlinePaint, rows: &[String], inner_width: usize, style: &OutlineStyle) -> Vec<String> {
    let color = style.color.as_deref().unwrap_or("accent");
    let vertical = (paint.fg)(color, DOTTED_VERTICAL);
    let mut lines = vec![outline_rule(
        paint,
        "╭",
        "╮",
        inner_width,
        color,
        style.caption.as_deref(),
    )];
    for row in rows {
        let body = if inner_width > 0 {
            pad_to_width(row, inner_width)
        } else {
            String::new()
        };
        lines.push(format!("{vertical} {body} {vertical}"));
    }
    lines.push(outline_rule(paint, "╰", "╯", inner_width, color, None));
    lines
}

/// One composed column and the outline's line range within it (`None` when nothing is outlined).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ComposedColumn {
    pub lines: Vec<String>,
    pub selection: Option<(usize, usize)>,
}

/// `composeOutlineColumn`: gutter-prefixed rows of `child_rows[from..to]` with a dotted outline around
/// `targets[selected]` (only its non-blank core; edge spacers stay outside).
#[allow(clippy::too_many_arguments)]
pub fn compose_outline_column(
    paint: &OutlinePaint,
    child_rows: &[Vec<String>],
    from: usize,
    to: usize,
    targets: &[OutlineTarget],
    selected: Option<usize>,
    column_width: usize,
    header: Option<&[String]>,
    style: &OutlineStyle,
) -> ComposedColumn {
    let inner = column_width.saturating_sub(4).max(10);
    let mut lines: Vec<String> = header.map(<[String]>::to_vec).unwrap_or_default();
    let mut selection = None;
    let target = selected.and_then(|s| targets.get(s));
    let mut index = from;
    while index < to {
        if let Some(t) = target
            && index == t.start
            && t.end <= to
        {
            let segment: Vec<String> = (t.start..t.end).flat_map(|c| child_rows[c].iter().cloned()).collect();
            let mut head = 0;
            let mut tail = segment.len();
            while head < tail && !has_non_blank(&segment[head]) {
                head += 1;
            }
            while tail > head && !has_non_blank(&segment[tail - 1]) {
                tail -= 1;
            }
            lines.extend(std::iter::repeat_n(String::new(), head));
            let sel_start = lines.len();
            lines.extend(outline_rows(paint, &segment[head..tail], inner, style));
            selection = Some((sel_start, lines.len()));
            lines.extend(std::iter::repeat_n(String::new(), segment.len() - tail));
            index = t.end;
            continue;
        }
        for row in &child_rows[index] {
            lines.push(if row.is_empty() {
                String::new()
            } else {
                format!("  {row}")
            });
        }
        index += 1;
    }
    ComposedColumn { lines, selection }
}

/// `clampScrollOffset`.
pub fn clamp_scroll_offset(offset: i64, total: usize, height: usize) -> usize {
    let max = total.saturating_sub(height) as i64;
    offset.clamp(0, max.max(0)) as usize
}

/// Where a revealed row lands in the viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Nearest,
    Start,
    Center,
    End,
}

/// `scrollOffsetForRow`: the offset that keeps `row` visible.
pub fn scroll_offset_for_row(current: i64, row: i64, total: usize, height: usize, align: Align) -> usize {
    if total == 0 || height == 0 {
        return 0;
    }
    let selected = row.clamp(0, total as i64 - 1);
    let current = clamp_scroll_offset(current, total, height) as i64;
    let h = height as i64;
    let next = match align {
        Align::Start => selected,
        Align::Center => selected - h / 2,
        Align::End => selected - h + 1,
        Align::Nearest if selected < current => selected,
        Align::Nearest if selected >= current + h => selected - h + 1,
        Align::Nearest => current,
    };
    clamp_scroll_offset(next, total, height)
}

/// A keyed range the body reveals whenever its geometry changes (`ScrollRangeAnchor`, nearest alignment, margin 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Anchor {
    pub id: String,
    pub start: usize,
    pub end: usize,
}

/// A scrolled row window (`ScrollView` without the scrollbar).
#[derive(Clone, Debug, Default)]
pub struct ScrollBody {
    pub offset: usize,
    total: usize,
    height: usize,
    /// Anchor, height and total of the last reveal.
    revealed: Option<(Anchor, usize, usize)>,
}

impl ScrollBody {
    pub fn set_geometry(&mut self, total: usize, height: usize) {
        self.total = total;
        self.height = height;
        self.offset = clamp_scroll_offset(self.offset as i64, total, height);
    }

    pub fn max_offset(&self) -> usize {
        self.total.saturating_sub(self.height)
    }

    /// Scroll by `delta` rows; true when the offset moved.
    pub fn scroll(&mut self, delta: i64) -> bool {
        let before = self.offset;
        self.offset = clamp_scroll_offset(self.offset as i64 + delta, self.total, self.height);
        self.offset != before
    }

    pub fn page(&mut self, pages: i64) -> bool {
        let step = self.height.saturating_sub(1).max(1) as i64;
        self.scroll(step * pages)
    }

    pub fn to_top(&mut self) -> bool {
        let before = self.offset;
        self.offset = 0;
        self.offset != before
    }

    pub fn to_bottom(&mut self) -> bool {
        let before = self.offset;
        self.offset = self.max_offset();
        self.offset != before
    }

    /// `revealRange` (selection mode, nearest, margin 1): a manual scroll survives renders of an unchanged anchor.
    pub fn reveal(&mut self, anchor: Option<&Anchor>) -> bool {
        let Some(anchor) = anchor else {
            self.revealed = None;
            return false;
        };
        if anchor.end <= anchor.start {
            return false;
        }
        if let Some((prev, height, total)) = &self.revealed
            && prev == anchor
            && *height == self.height
            && *total == self.total
        {
            return false;
        }
        let top = anchor.start.saturating_sub(1);
        let bottom = (anchor.end + 1).min(self.total);
        let visible_end = (self.offset + self.height).min(self.total);
        if top < self.offset {
            self.offset = scroll_offset_for_row(self.offset as i64, top as i64, self.total, self.height, Align::Start);
        } else if bottom > visible_end {
            let row = bottom.saturating_sub(1).max(top);
            self.offset = scroll_offset_for_row(self.offset as i64, row as i64, self.total, self.height, Align::End);
        }
        self.revealed = Some((anchor.clone(), self.height, self.total));
        true
    }

    /// Whether `id` was revealed last.
    pub fn has_revealed(&self, id: &str) -> bool {
        self.revealed.as_ref().is_some_and(|(a, _, _)| a.id == id)
    }
}

/// One screen of a fullscreen browser: chrome above and below a scrolled body.
#[derive(Clone, Debug, Default)]
pub struct BrowserFrame {
    pub header: Vec<String>,
    pub body: Vec<String>,
    pub anchor: Option<Anchor>,
    pub footer: Vec<String>,
}

/// `TranscriptBrowser`: paints a [`BrowserFrame`] over `height` rows and maps screen points into the body.
#[derive(Clone, Debug, Default)]
pub struct FrameBrowser {
    pub body: ScrollBody,
    header_rows: usize,
    body_rows: usize,
}

impl FrameBrowser {
    /// Exactly `height` rows, each cut to `width`.
    pub fn render(&mut self, frame: &BrowserFrame, width: usize, height: usize) -> Vec<String> {
        let height = height.max(1);
        let header: Vec<&String> = frame.header.iter().take(height).collect();
        let room = height - header.len();
        let footer: Vec<&String> = frame.footer.iter().take(room).collect();
        let body_rows = room - footer.len();
        self.header_rows = header.len();
        self.body_rows = body_rows;
        self.body.set_geometry(frame.body.len(), body_rows);
        self.body.reveal(frame.anchor.as_ref());
        let mut out: Vec<String> = header.into_iter().cloned().collect();
        let start = self.body.offset;
        for i in 0..body_rows {
            out.push(frame.body.get(start + i).cloned().unwrap_or_default());
        }
        out.extend(footer.into_iter().cloned());
        out.into_iter()
            .map(|l| {
                if visible_width(&l) > width {
                    truncate_to_width(&l, width, "", false)
                } else {
                    l
                }
            })
            .collect()
    }

    /// A 0-based screen point to a body (line, column), when it lands in the body.
    pub fn to_body_point(&self, row: i64, col: i64) -> Option<(usize, usize)> {
        let local = row - self.header_rows as i64;
        if row < 0 || col < 0 || local < 0 || local >= self.body_rows as i64 {
            return None;
        }
        Some((self.body.offset + local as usize, col as usize))
    }

    pub fn body_rows(&self) -> usize {
        self.body_rows
    }
}

/// `nearestVisibleOutlineTarget`: the selection, else the nearest visible target above, then below.
pub fn nearest_visible_target(visible: &[bool], selected: Option<usize>) -> Option<usize> {
    let Some(selected) = selected.filter(|s| *s < visible.len()) else {
        return visible.iter().position(|v| *v);
    };
    if visible[selected] {
        return Some(selected);
    }
    (0..selected)
        .rev()
        .chain(selected + 1..visible.len())
        .find(|&i| visible[i])
        .or(Some(selected))
}

/// Key hint text for a key id: arrows and Enter as glyphs (`→`, `↵`), Escape as `esc`, the rest as written.
pub fn key_glyph(key: &str) -> String {
    match key {
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "enter" | "return" => "↵".into(),
        "escape" => "esc".into(),
        "pageUp" => "PgUp".into(),
        "pageDown" => "PgDn".into(),
        other => other.to_string(),
    }
}
