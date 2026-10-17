//! A vertical scroll view over rendered rows (`ScrollView` of packages/tui/src/components/scroll-view.ts) and its
//! scrollbar (`getScrollbarGeometry`, `paintScrollbar` and `composeScrollbarCell` of packages/tui/src/layout.ts).
//!
//! The view keeps the scroll position, whether it follows the content's end, and the transient visibility of an
//! `auto` scrollbar. TypeScript hides that scrollbar with a timer; here the deadline is kept and
//! [`ScrollView::poll`] expires it, so the owner decides when time passes (the run loop, or a test).

use std::time::{Duration, Instant};

use crate::utils::{ansi_len, get_active_background_ansi, grapheme_width, slice_with_width, visible_width};
use unicode_segmentation::UnicodeSegmentation;

/// `ScrollViewScrollbar`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scrollbar {
    #[default]
    Hidden,
    Auto,
    Always,
}

impl Scrollbar {
    /// The setting value (`"hidden"`, `"auto"`, `"always"`); anything else is `None`.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "hidden" => Some(Self::Hidden),
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            _ => None,
        }
    }
}

/// Styles a scrollbar glyph.
pub type GlyphStyle = Box<dyn Fn(&str) -> String>;

/// `ScrollView` state.
pub struct ScrollView {
    follow_end: bool,
    scrollbar: Scrollbar,
    scroll_top: usize,
    content_height: usize,
    viewport_height: usize,
    following_end: bool,
    follow_suppressed_at_end: bool,
    transient_visible: bool,
    active: bool,
    hide_at: Option<Instant>,
    hide_delay: Duration,
    pub track_style: GlyphStyle,
    pub thumb_style: GlyphStyle,
}

impl ScrollView {
    /// `new ScrollView(child, { follow, scrollbar })` with the default styles and the 1000 ms hide delay.
    pub fn new(follow_end: bool, scrollbar: Scrollbar) -> Self {
        Self {
            follow_end,
            scrollbar,
            scroll_top: 0,
            content_height: 0,
            viewport_height: 0,
            following_end: follow_end,
            follow_suppressed_at_end: false,
            transient_visible: false,
            active: false,
            hide_at: None,
            hide_delay: Duration::from_millis(1000),
            track_style: Box::new(|t| format!("\x1b[90m{t}\x1b[39m")),
            thumb_style: Box::new(|t| format!("\x1b[37m{t}\x1b[39m")),
        }
    }

    pub fn scroll_top(&self) -> usize {
        self.scroll_top
    }

    pub fn is_following_end(&self) -> bool {
        self.following_end
    }

    pub fn follows_end(&self) -> bool {
        self.follow_end
    }

    pub fn viewport_height(&self) -> usize {
        self.viewport_height
    }

    pub fn content_height(&self) -> usize {
        self.content_height
    }

    pub fn scrollbar(&self) -> Scrollbar {
        self.scrollbar
    }

    fn max_scroll_top(&self) -> usize {
        self.content_height.saturating_sub(self.viewport_height)
    }

    /// `isScrollbarVisible`.
    pub fn is_scrollbar_visible(&self) -> bool {
        match self.scrollbar {
            Scrollbar::Always => self.viewport_height > 0,
            Scrollbar::Auto => self.content_height > self.viewport_height && self.transient_visible,
            Scrollbar::Hidden => false,
        }
    }

    /// `getContentWidth`: an always-visible scrollbar keeps the last column.
    pub fn content_width(&self, width: usize) -> usize {
        if self.scrollbar == Scrollbar::Always && width > 1 {
            width - 1
        } else {
            width
        }
    }

    fn mark_activity(&mut self) {
        if self.scrollbar != Scrollbar::Auto || self.content_height <= self.viewport_height {
            return;
        }
        self.transient_visible = true;
        self.hide_at = None;
        if self.active {
            return;
        }
        self.hide_at = Some(Instant::now() + self.hide_delay);
    }

    fn hide_transient(&mut self) {
        self.transient_visible = false;
        self.hide_at = None;
    }

    /// The hide timer fired by `now`: the transient scrollbar hides. True when that changed what shows.
    pub fn poll(&mut self, now: Instant) -> bool {
        match self.hide_at {
            Some(at) if at <= now => {
                self.hide_at = None;
                self.transient_visible = false;
                true
            }
            _ => false,
        }
    }

    /// When the transient scrollbar hides, if it is due to.
    pub fn deadline(&self) -> Option<Instant> {
        self.hide_at
    }

    /// `setScrollbarActive` (the pointer is over the track).
    pub fn set_scrollbar_active(&mut self, active: bool) {
        if active == self.active {
            return;
        }
        self.active = active;
        self.mark_activity();
    }

    /// `scrollTo`; `disable_follow` keeps follow-end off even at the content end.
    pub fn scroll_to(&mut self, scroll_top: i64, disable_follow: bool) {
        let max = self.max_scroll_top();
        let next = scroll_top.clamp(0, max as i64) as usize;
        let suppressed = disable_follow && next == max;
        let following = !suppressed && self.follow_end && next == max;
        if next == self.scroll_top && following == self.following_end && suppressed == self.follow_suppressed_at_end {
            return;
        }
        let moved = next != self.scroll_top;
        self.scroll_top = next;
        self.following_end = following;
        self.follow_suppressed_at_end = suppressed;
        if moved {
            self.mark_activity();
        }
    }

    /// `scrollBy`: the lines that could not be scrolled.
    pub fn scroll_by(&mut self, lines: i64) -> i64 {
        if lines == 0 {
            return 0;
        }
        let max = self.max_scroll_top() as i64;
        let start = if self.following_end {
            max
        } else {
            self.scroll_top as i64
        };
        let next = (start + lines).clamp(0, max);
        let moved = next - start;
        self.scroll_top = next as usize;
        self.following_end = self.follow_end && next == max;
        self.follow_suppressed_at_end = false;
        if moved != 0 {
            self.mark_activity();
        }
        lines - moved
    }

    /// `scrollToStart`.
    pub fn scroll_to_start(&mut self) {
        let following = self.follow_end && self.content_height <= self.viewport_height;
        let changed = self.scroll_top != 0 || self.following_end != following;
        self.scroll_top = 0;
        self.following_end = following;
        self.follow_suppressed_at_end = false;
        if changed {
            self.mark_activity();
        }
    }

    /// `scrollToEnd`.
    pub fn scroll_to_end(&mut self) {
        let next = self.max_scroll_top();
        let changed = self.scroll_top != next || self.following_end != self.follow_end;
        self.scroll_top = next;
        self.following_end = self.follow_end;
        self.follow_suppressed_at_end = false;
        if changed {
            self.mark_activity();
        }
    }

    /// `updateLayout`: the content and viewport heights of this frame.
    pub fn update_layout(&mut self, content_height: usize, viewport_height: usize) {
        self.content_height = content_height;
        self.viewport_height = viewport_height;
        let max = self.max_scroll_top();
        self.scroll_top = if self.following_end {
            max
        } else {
            self.scroll_top.min(max)
        };
        if self.scroll_top < max {
            self.follow_suppressed_at_end = false;
        }
        if self.follow_end && self.scroll_top == max && !self.follow_suppressed_at_end {
            self.following_end = true;
        }
        if self.content_height <= self.viewport_height {
            self.hide_transient();
        }
    }
}

/// Where a view's rows sit on the screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl Rect {
    pub fn contains(&self, x: i64, y: i64) -> bool {
        x >= self.x as i64
            && x < (self.x + self.width) as i64
            && y >= self.y as i64
            && y < (self.y + self.height) as i64
    }
}

/// `ScrollbarGeometry`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScrollbarGeometry {
    pub column: usize,
    pub track_top: usize,
    pub track_height: usize,
    pub thumb_top: usize,
    pub thumb_height: usize,
    pub max_scroll_top: usize,
}

/// JavaScript `Math.round` for non-negative values.
fn js_round(x: f64) -> usize {
    (x + 0.5).floor() as usize
}

/// `getScrollbarGeometry` of a view laid out at `rect` over `content_height` rows; `include_hidden_auto` also
/// answers for an `auto` scrollbar that is hidden (hover reveals it).
pub fn scrollbar_geometry(
    view: &ScrollView,
    rect: Rect,
    content_height: usize,
    include_hidden_auto: bool,
) -> Option<ScrollbarGeometry> {
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    let track = rect.height;
    let reveal = include_hidden_auto && view.scrollbar == Scrollbar::Auto && content_height > track;
    if !view.is_scrollbar_visible() && !reveal {
        return None;
    }
    let min_thumb = track.min(2);
    let thumb = if content_height == 0 {
        track
    } else {
        min_thumb.max(track.min(js_round((track * track) as f64 / content_height as f64)))
    };
    let max_scroll_top = content_height.saturating_sub(track);
    let max_thumb_top = track - thumb;
    let offset = if max_scroll_top == 0 {
        0
    } else {
        js_round(view.scroll_top as f64 / max_scroll_top as f64 * max_thumb_top as f64)
    };
    Some(ScrollbarGeometry {
        column: rect.x + rect.width - 1,
        track_top: rect.y,
        track_height: track,
        thumb_top: rect.y + offset,
        thumb_height: thumb,
        max_scroll_top,
    })
}

/// `getGraphemeCellRange`: the columns of the visible grapheme covering `column`.
pub fn grapheme_cell_range(line: &str, column: usize) -> Option<(usize, usize)> {
    let mut col = 0;
    let mut i = 0;
    while i < line.len() {
        if let Some(len) = ansi_len(line, i) {
            i += len;
            continue;
        }
        let mut end = i;
        while end < line.len() && ansi_len(line, end).is_none() {
            end += line[end..].chars().next().map_or(1, char::len_utf8);
        }
        for g in line[i..end].graphemes(true) {
            let w = grapheme_width(g);
            if w > 0 && column >= col && column < col + w {
                return Some((col, col + w));
            }
            col += w;
        }
        i = end;
    }
    None
}

/// `sliceByColumn`.
pub fn slice_by_column(line: &str, start: usize, length: usize, strict: bool) -> String {
    slice_with_width(line, start, length, strict).0
}

/// `composeScrollbarCell`: `replacement` in `column` of `line` (`total_width` columns), styles reset before it;
/// `preserve_background` keeps the background the replaced cell had.
pub fn compose_scrollbar_cell(
    line: &str,
    column: usize,
    total_width: usize,
    replacement: &str,
    preserve_background: bool,
) -> String {
    let (start, end) = grapheme_cell_range(line, column).unwrap_or((column, column + 1));
    let before = slice_by_column(line, 0, start, true);
    let target = slice_by_column(line, start, end - start, true);
    let after = slice_by_column(line, end, total_width.saturating_sub(end), true);
    let mut prefix_end = 0;
    while let Some(len) = ansi_len(&target, prefix_end) {
        prefix_end += len;
    }
    let target_prefix = &target[..prefix_end];
    let before_padding = " ".repeat(start.saturating_sub(visible_width(&before)));
    let cell_before = " ".repeat(column.saturating_sub(start));
    let cell_after = " ".repeat((end - column).saturating_sub(1));
    let background = if preserve_background {
        get_active_background_ansi(target_prefix)
    } else {
        String::new()
    };
    format!("{before}{before_padding}\x1b[0m\x1b]8;;\x07{background}{cell_before}{replacement}{cell_after}{after}")
}

/// `paintScrollbar`: the track and thumb of `view` (laid out at `rect` over `content_height` rows) over `screen`.
pub fn paint_scrollbar(
    view: &ScrollView,
    rect: Rect,
    content_height: usize,
    screen: &mut [String],
    total_width: usize,
) {
    let Some(g) = scrollbar_geometry(view, rect, content_height, false) else {
        return;
    };
    for row in g.track_top..g.track_top + g.track_height {
        if row >= screen.len() {
            continue;
        }
        let thumb = row >= g.thumb_top && row < g.thumb_top + g.thumb_height;
        let replacement = if thumb {
            (view.thumb_style)(if view.active { "█" } else { "┃" })
        } else {
            (view.track_style)("│")
        };
        screen[row] = compose_scrollbar_cell(
            &screen[row],
            g.column,
            total_width,
            &replacement,
            view.scrollbar != Scrollbar::Always,
        );
    }
}
