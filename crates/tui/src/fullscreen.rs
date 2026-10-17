//! The fullscreen transcript viewport (the viewport half of `TuiAltScreen` in packages/tui/src/tui-alt-screen.ts):
//! keyboard scrolling (page, half page, line, top/bottom, previous/next OSC 133 prompt), mouse-wheel scrolling,
//! the auto scrollbar (hover reveal, track jump, thumb drag), the jump-to-end indicator, application-owned text
//! selection (drag, double-click words, triple-click lines, auto-scroll at the edge) with copy-on-select, and the
//! flash stack ("Copied!").
//!
//! The application lays the transcript out with [`FullscreenViewport::layout`] (its rows at a rectangle of the
//! screen), composes the rest of the frame, and finishes it with [`FullscreenViewport::finish_frame`]. Input goes
//! through [`FullscreenViewport::handle_input`] first; a finished selection comes back as [`ViewportInput::Copy`]
//! for the application to copy (through its injected clipboard), which then reports with
//! [`FullscreenViewport::copied`]. Time is passed in: timers are deadlines that [`FullscreenViewport::poll`] expires.

use std::sync::Arc;
use std::time::{Duration, Instant};

use unicode_segmentation::UnicodeSegmentation;

use crate::keybindings::KeybindingsManager;
use crate::keys::{KeyEventType, key_event_type};
use crate::overlay::composite_line;
use crate::renderer::is_image_line;
use crate::scroll_view::{
    Rect, ScrollView, Scrollbar, ScrollbarGeometry, grapheme_cell_range, paint_scrollbar, scrollbar_geometry,
    slice_by_column,
};
use crate::sgr_mouse::parse_sgr_mouse;
use crate::utils::{
    ansi_len, extract_ansi_code, grapheme_width, js_trim_end, strip_terminal_sequences, truncate_to_width,
    visible_width,
};

/// Rows a page scroll keeps from the previous page (`PAGE_SCROLL_OVERLAP`).
pub const PAGE_SCROLL_OVERLAP: usize = 4;
/// `ALT_WHEEL_SCROLL_MULTIPLIER`.
pub const ALT_WHEEL_SCROLL_MULTIPLIER: i64 = 5;
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
const FLASH_DURATION: Duration = Duration::from_millis(1000);
const COPY_ERROR_FLASH_DURATION: Duration = Duration::from_millis(5000);
const AUTO_SCROLL_INTERVAL: Duration = Duration::from_millis(50);
const OSC133_PROMPT: [&str; 2] = ["\x1b]133;A\x07", "\x1b]133;A\x1b\\"];

/// `ENABLE_BUTTON_MOTION_MOUSE`: clicks, wheel, drags and focus reports (inside terminal multiplexers).
pub const ENABLE_BUTTON_MOTION_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1004h\x1b[?1006h";
/// `ENABLE_ALL_MOTION_MOUSE`: also pointer motion without a button (scrollbar hover).
pub const ENABLE_ALL_MOTION_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1004h\x1b[?1006h";
/// `DISABLE_MOUSE`.
pub const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1004l\x1b[?1003l\x1b[?1002l\x1b[?1000l";
const FOCUS_IN: &str = "\x1b[I";
const FOCUS_OUT: &str = "\x1b[O";

/// The mouse tracking `beforeTerminalStart` turns on: multiplexers (tmux, zellij, screen) lag when every pointer
/// movement is forwarded, so they get button-motion tracking. `var` reads the environment.
pub fn mouse_tracking(var: &dyn Fn(&str) -> Option<String>) -> &'static str {
    let term = var("TERM").unwrap_or_default().to_lowercase();
    if var("TMUX").is_some()
        || var("ZELLIJ").is_some()
        || var("STY").is_some()
        || term.starts_with("tmux")
        || term.starts_with("screen")
    {
        ENABLE_BUTTON_MOTION_MOUSE
    } else {
        ENABLE_ALL_MOTION_MOUSE
    }
}

/// Strip the leading OSC 133 zone markers of a row (`OSC133_ZONE_PREFIX`).
pub fn strip_zone_prefix(line: &str) -> &str {
    let mut rest = line;
    loop {
        let Some(after) = rest.strip_prefix("\x1b]133;") else {
            return rest;
        };
        let mut chars = after.chars();
        if !matches!(chars.next(), Some('A' | 'B' | 'C')) {
            return rest;
        }
        let tail = chars.as_str();
        if let Some(t) = tail.strip_prefix('\x07') {
            rest = t;
        } else if let Some(t) = tail.strip_prefix("\x1b\\") {
            rest = t;
        } else {
            return rest;
        }
    }
}

/// Opens a clicked OSC 8 hyperlink (`openUrl`); the application injects it (its launcher comes from the session's
/// PATH only).
pub type UrlOpener = Arc<dyn Fn(&str) + Send + Sync>;

/// `getOsc8LinkAtColumn`: the OSC 8 hyperlink covering visible column `column` of `line` (tabs are 3 columns).
pub fn osc8_link_at_column(line: &str, column: usize) -> Option<String> {
    let mut active: Option<&str> = None;
    let mut col = 0;
    let mut i = 0;
    while i < line.len() {
        if let Some(len) = ansi_len(line, i) {
            if let Some(url) = osc8_url(&line[i..i + len]) {
                active = (!url.is_empty()).then_some(url);
            }
            i += len;
            continue;
        }
        let mut end = i;
        while end < line.len() && ansi_len(line, end).is_none() {
            end += line[end..].chars().next().map_or(1, char::len_utf8);
        }
        for g in line[i..end].graphemes(true) {
            let width = if g == "\t" { 3 } else { grapheme_width(g) };
            if column >= col && column < col + width {
                return active.map(str::to_string);
            }
            col += width;
        }
        i = end;
    }
    None
}

/// `/^\x1b\]8;[^;]*;([^\x07\x1b]*)(?:\x07|\x1b\\)$/`: the URL of an OSC 8 sequence (empty closes the link).
fn osc8_url(code: &str) -> Option<&str> {
    let body = code.strip_prefix("\x1b]8;")?;
    let body = body.strip_suffix('\x07').or_else(|| body.strip_suffix("\x1b\\"))?;
    let (params, url) = body.split_once(';')?;
    (!params.contains(';') && !url.contains(['\x07', '\x1b'])).then_some(url)
}

/// A viewport key's effect.
type ViewportAction<'a> = dyn Fn(&mut FullscreenViewport) + 'a;

/// What the viewport did with an input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewportInput {
    /// Not the viewport's: the application handles it.
    Pass,
    /// Handled (paint again).
    Consumed,
    /// A selection finished with copy-on-select: copy this text, then report with [`FullscreenViewport::copied`].
    Copy(String),
    /// A right-button press with [`FullscreenViewport::right_click_paste`] on: paste the clipboard's text
    /// ([`crate::right_click_paste`]).
    RightClickPaste,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Granularity {
    Character,
    Word,
    Line,
}

/// `SelectionPoint`: a content row of the transcript (`scroll`) or a screen row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Point {
    row: usize,
    col: usize,
    scroll: bool,
    /// Between cells rather than on one.
    boundary: bool,
}

#[derive(Clone, Copy, Debug)]
struct Range {
    start: Point,
    end: Point,
}

#[derive(Clone, Copy, Debug)]
struct Click {
    at: Instant,
    count: u32,
    row: usize,
    scroll: bool,
    word_start: usize,
    word_end: usize,
}

/// A selection bound in screen coordinates (rows may lie above the screen).
#[derive(Clone, Copy, Debug)]
struct ScreenPoint {
    row: i64,
    col: usize,
    boundary: bool,
}

/// The parsed parts of an SGR mouse report (0-based cell).
#[derive(Clone, Copy, Debug)]
struct Sgr {
    button: u32,
    x: i64,
    y: i64,
    release: bool,
}

struct Flash {
    message: String,
    until: Instant,
}

/// The transcript viewport of a fullscreen frame.
pub struct FullscreenViewport {
    pub view: ScrollView,
    /// The transcript's rectangle and rows in the last frame (`currentLayout`).
    rect: Option<Rect>,
    content: Vec<String>,
    /// The last frame's rows (`previousScreen`).
    screen: Vec<String>,
    cols: usize,
    rows: usize,
    anchor: Option<Point>,
    focus: Option<Point>,
    granularity: Granularity,
    initial: Option<Range>,
    last_click: Option<Click>,
    drag_pointer: Option<(i64, i64)>,
    auto_direction: i64,
    auto_next: Option<Instant>,
    press_active: bool,
    dragged: bool,
    scrollbar_drag: Option<i64>,
    scrollbar_hover: bool,
    indicator_rect: Option<(usize, usize, usize)>,
    flashes: Vec<Flash>,
    /// Rows moved per wheel notch (`wheelScrollLines`).
    pub wheel_lines: i64,
    /// Copy a finished selection (`copyOnSelect`, default on).
    pub copy_on_select: bool,
    /// The jump-to-end label (`scrollToEndIndicator`); `None` shows none.
    pub indicator: Option<String>,
    /// Paint rows with their OSC 133 zone marks (TypeScript strips them; they are invisible either way).
    pub keep_zone_marks: bool,
    /// A right-button press pastes instead of selecting ([`crate::right_click_paste::right_click_paste_enabled`];
    /// off by default).
    pub right_click_paste: bool,
    /// Opens an OSC 8 hyperlink clicked with the primary button (`openUrl`); `None` opens nothing.
    pub open_url: Option<UrlOpener>,
    /// The hyperlink under the press, opened when the release ends a click (`pressedUrl`).
    pressed_url: Option<String>,
}

impl Default for FullscreenViewport {
    fn default() -> Self {
        Self::new(ScrollView::new(true, Scrollbar::Auto))
    }
}

impl FullscreenViewport {
    /// A viewport over `view` (the client TUI's transcript: follow-end, auto scrollbar).
    pub fn new(view: ScrollView) -> Self {
        Self {
            view,
            rect: None,
            content: Vec::new(),
            screen: Vec::new(),
            cols: 0,
            rows: 0,
            anchor: None,
            focus: None,
            granularity: Granularity::Character,
            initial: None,
            last_click: None,
            drag_pointer: None,
            auto_direction: 0,
            auto_next: None,
            press_active: false,
            dragged: false,
            scrollbar_drag: None,
            scrollbar_hover: false,
            indicator_rect: None,
            flashes: Vec::new(),
            wheel_lines: 1,
            copy_on_select: true,
            indicator: None,
            keep_zone_marks: false,
            right_click_paste: false,
            open_url: None,
            pressed_url: None,
        }
    }

    /// The terminal size input is measured against (`terminal.columns`, `terminal.rows`).
    pub fn set_size(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
    }

    /// `viewportTop`.
    pub fn viewport_top(&self) -> usize {
        self.view.scroll_top()
    }

    /// `isFollowingOutput`.
    pub fn is_following_output(&self) -> bool {
        self.view.is_following_end()
    }

    /// Forget the layout and gestures (`beforeTerminalStart`: a new alternate screen).
    pub fn reset(&mut self) {
        self.stop_auto_scroll();
        self.press_active = false;
        self.set_hover(false);
        self.scrollbar_drag = None;
        self.flashes.clear();
        self.anchor = None;
        self.focus = None;
        self.granularity = Granularity::Character;
        self.initial = None;
        self.last_click = None;
        self.pressed_url = None;
        self.dragged = false;
        self.rect = None;
        self.screen.clear();
    }

    /// Lay the transcript out: `content` (rendered at `view.content_width(rect.width)`) shown in `rect`, scrolled
    /// to the view's position. The rect's rows of the frame, with the scrollbar painted.
    pub fn layout(&mut self, content: Vec<String>, rect: Rect) -> Vec<String> {
        self.view.update_layout(content.len(), rect.height);
        let top = self.view.scroll_top();
        let mut rows: Vec<String> = (0..rect.height)
            .map(|i| {
                content.get(top + i).map_or_else(String::new, |l| {
                    if self.keep_zone_marks {
                        l.clone()
                    } else {
                        strip_zone_prefix(l).to_string()
                    }
                })
            })
            .collect();
        let local = Rect { x: 0, y: 0, ..rect };
        paint_scrollbar(&self.view, local, content.len(), &mut rows, rect.width);
        self.content = content;
        self.rect = Some(rect);
        rows
    }

    /// The layout is not shown (an overlay owns the frame): mouse input then has no transcript to hit.
    pub fn hide_layout(&mut self) {
        self.rect = None;
    }

    /// Finish a `width` x `height` frame (`doRender` after the layout): the jump-to-end indicator, the selection
    /// highlight and the flashes.
    pub fn finish_frame(&mut self, screen: Vec<String>, width: usize, height: usize) -> Vec<String> {
        self.cols = width;
        self.rows = height;
        let mut screen = self.composite_indicator(screen, width);
        if screen.len() > height {
            screen.drain(..screen.len() - height);
        }
        let screen = self.apply_selection(screen);
        let screen = self.composite_flashes(screen, width, height);
        self.screen = screen
            .iter()
            .map(|l| {
                if is_image_line(l) || visible_width(l) <= width {
                    l.clone()
                } else {
                    slice_by_column(l, 0, width, true)
                }
            })
            .collect();
        screen
    }

    /// `flash`: a transient message at the top right.
    pub fn flash(&mut self, message: &str, duration: Duration, now: Instant) {
        self.flashes.push(Flash {
            message: message.to_string(),
            until: now + duration,
        });
    }

    /// The application copied a selection: "Copied!", or the error for five seconds.
    pub fn copied(&mut self, result: Result<(), String>, now: Instant) {
        match result {
            Ok(()) => self.flash("Copied!", FLASH_DURATION, now),
            Err(e) => {
                let message = if e.is_empty() { "Copy failed".to_string() } else { e };
                self.flash(&message, COPY_ERROR_FLASH_DURATION, now);
            }
        }
    }

    /// Timers due by `now`: the scrollbar hides, flashes expire, a held drag auto-scrolls. True when the frame changed.
    pub fn poll(&mut self, now: Instant) -> bool {
        let mut changed = self.view.poll(now);
        let before = self.flashes.len();
        self.flashes.retain(|f| f.until > now);
        changed |= self.flashes.len() != before;
        // `setInterval`: one tick when due, however late (missed ticks are not caught up).
        if self.auto_next.is_some_and(|at| at <= now) {
            self.auto_next = Some(now + AUTO_SCROLL_INTERVAL);
            changed |= self.auto_scroll_tick();
        }
        changed
    }

    /// The next instant [`Self::poll`] has work.
    pub fn deadline(&self) -> Option<Instant> {
        [
            self.view.deadline(),
            self.auto_next,
            self.flashes.iter().map(|f| f.until).min(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Whether a non-empty selection is active (`hasActiveSelection`).
    pub fn has_active_selection(&self) -> bool {
        self.selection_text().is_some()
    }

    /// `getActiveSelectionText`.
    pub fn selection_text(&self) -> Option<String> {
        let (start, end) = self.bounds()?;
        let source: &[String] = if start.scroll {
            self.rect?;
            &self.content
        } else {
            &self.screen
        };
        let mut lines = Vec::new();
        for row in start.row..=end.row {
            let line = source.get(row).map_or("", String::as_str);
            let (a, b) = selection_columns(
                line,
                row as i64,
                to_screen(start, 0),
                to_screen(end, 0),
                0,
                visible_width(line),
            );
            let text = slice_by_column(line, a, b.saturating_sub(a), true);
            lines.push(js_trim_end(&strip_terminal_sequences(&text)).to_string());
        }
        let text = lines.join("\n");
        (!text.is_empty()).then_some(text)
    }

    /// Route `data` (`handleViewportInput`): focus reports, wheel, mouse, then the viewport keys.
    pub fn handle_input(&mut self, data: &str, keys: &KeybindingsManager, now: Instant) -> ViewportInput {
        if data == FOCUS_OUT {
            let had_active = self.press_active;
            self.press_active = false;
            self.stop_auto_scroll();
            self.set_hover(false);
            self.scrollbar_drag = None;
            self.pressed_url = None;
            self.dragged = false;
            if had_active {
                self.anchor = None;
                self.focus = None;
                self.granularity = Granularity::Character;
                self.initial = None;
            }
            self.last_click = None;
            return ViewportInput::Consumed;
        }
        if data == FOCUS_IN {
            return ViewportInput::Consumed;
        }
        if let Some((direction, x, y, button)) = parse_wheel(data) {
            self.view.scroll_by(direction * self.wheel_scroll_lines(button));
            self.update_hover(x, y);
            return ViewportInput::Consumed;
        }
        if let Some(m) = parse_sgr_mouse(data) {
            return self.handle_mouse(
                Sgr {
                    button: m.button,
                    x: m.col,
                    y: m.row,
                    release: m.release,
                },
                now,
            );
        }
        if is_mouse_sequence(data) {
            return ViewportInput::Consumed;
        }
        let release = !data.contains("\x1b[200~") && key_event_type(data) == Some(KeyEventType::Release);
        let height = self.view.viewport_height();
        let page = height.saturating_sub(PAGE_SCROLL_OVERLAP).max(1) as i64;
        let half = (height / 2).max(1) as i64;
        let actions: [(&str, &ViewportAction); 10] = [
            ("tui.altScreen.pageUp", &|v| v.scroll(-page)),
            ("tui.altScreen.pageDown", &|v| v.scroll(page)),
            ("tui.altScreen.halfPageUp", &|v| v.scroll(-half)),
            ("tui.altScreen.halfPageDown", &|v| v.scroll(half)),
            ("tui.altScreen.lineUp", &|v| v.scroll(-1)),
            ("tui.altScreen.lineDown", &|v| v.scroll(1)),
            ("tui.altScreen.previousPrompt", &|v| v.scroll_to_prompt(-1)),
            ("tui.altScreen.nextPrompt", &|v| v.scroll_to_prompt(1)),
            ("tui.altScreen.top", &|v| v.view.scroll_to_start()),
            ("tui.altScreen.bottom", &|v| v.view.scroll_to_end()),
        ];
        for (binding, action) in actions {
            if keys.matches(data, binding) {
                if !release {
                    action(self);
                }
                return ViewportInput::Consumed;
            }
        }
        ViewportInput::Pass
    }

    fn scroll(&mut self, lines: i64) {
        self.view.scroll_by(lines);
    }

    /// `scrollToPrompt`: the nearest OSC 133 prompt start above (-1) or below (1) the viewport top.
    fn scroll_to_prompt(&mut self, direction: i64) {
        if self.rect.is_none() {
            return;
        }
        let mut row = self.view.scroll_top() as i64 + direction;
        while row >= 0 && (row as usize) < self.content.len() {
            let line = &self.content[row as usize];
            if OSC133_PROMPT.iter().any(|p| line.starts_with(p)) {
                self.view.scroll_to(row, false);
                return;
            }
            row += direction;
        }
    }

    fn wheel_scroll_lines(&self, button: u32) -> i64 {
        if button & 8 != 0 {
            self.wheel_lines * ALT_WHEEL_SCROLL_MULTIPLIER
        } else {
            self.wheel_lines
        }
    }

    /// `handleMouseEvent` for an application without mouse-aware components.
    fn handle_mouse(&mut self, ev: Sgr, now: Instant) -> ViewportInput {
        if self.indicator_click(ev) {
            return ViewportInput::Consumed;
        }
        let handled = self.scrollbar_mouse(ev);
        if self.scrollbar_drag.is_none() {
            self.update_hover(ev.x, ev.y);
        }
        if handled {
            return ViewportInput::Consumed;
        }
        if self.right_click_paste && crate::right_click_paste::is_right_click_press(ev.button, ev.release) {
            return ViewportInput::RightClickPaste;
        }
        self.selection_mouse(ev, now)
    }

    fn indicator_click(&mut self, ev: Sgr) -> bool {
        let Some((row, column, width)) = self.indicator_rect else {
            return false;
        };
        if ev.release || ev.button & 32 != 0 || ev.button & 3 != 0 {
            return false;
        }
        if ev.y != row as i64 || ev.x < column as i64 || ev.x >= (column + width) as i64 {
            return false;
        }
        self.view.scroll_to_end();
        true
    }

    fn scrollbar_target(&self, x: i64, y: i64, include_hidden: bool) -> Option<ScrollbarGeometry> {
        let rect = self.rect?;
        if !rect.contains(x, y) {
            return None;
        }
        let g = scrollbar_geometry(&self.view, rect, self.content.len(), include_hidden)?;
        (x == g.column as i64 && y >= g.track_top as i64 && y < (g.track_top + g.track_height) as i64).then_some(g)
    }

    fn set_hover(&mut self, hover: bool) {
        if hover == self.scrollbar_hover {
            return;
        }
        self.scrollbar_hover = hover;
        self.view.set_scrollbar_active(hover);
    }

    fn update_hover(&mut self, x: i64, y: i64) {
        let hover = self.scrollbar_target(x, y, true).is_some();
        self.set_hover(hover);
    }

    fn scroll_to_pointer(&mut self, g: ScrollbarGeometry, pointer_y: i64, grab: i64) {
        let max_thumb = (g.track_height - g.thumb_height) as i64;
        let offset = (pointer_y - g.track_top as i64 - grab).clamp(0, max_thumb.max(0));
        let top = if max_thumb == 0 {
            0
        } else {
            (offset as f64 / max_thumb as f64 * g.max_scroll_top as f64 + 0.5).floor() as i64
        };
        self.view.scroll_to(top, false);
    }

    fn scrollbar_mouse(&mut self, ev: Sgr) -> bool {
        if let Some(grab) = self.scrollbar_drag {
            if ev.release {
                self.scrollbar_drag = None;
                return true;
            }
            if let Some(rect) = self.rect
                && let Some(g) = scrollbar_geometry(&self.view, rect, self.content.len(), false)
            {
                self.scroll_to_pointer(g, ev.y, grab);
            }
            return true;
        }
        if ev.release || ev.button & 32 != 0 || ev.button & 3 != 0 {
            return false;
        }
        let Some(g) = self.scrollbar_target(ev.x, ev.y, false) else {
            return false;
        };
        self.stop_auto_scroll();
        self.press_active = false;
        self.anchor = None;
        self.focus = None;
        self.granularity = Granularity::Character;
        self.initial = None;
        self.last_click = None;
        self.pressed_url = None;
        self.dragged = false;
        self.set_hover(true);
        let on_thumb = ev.y >= g.thumb_top as i64 && ev.y < (g.thumb_top + g.thumb_height) as i64;
        let grab = if on_thumb {
            ev.y - g.thumb_top as i64
        } else {
            (g.thumb_height / 2) as i64
        };
        if !on_thumb {
            self.scroll_to_pointer(g, ev.y, grab);
        }
        self.scrollbar_drag = Some(grab);
        true
    }

    /// `getScrollSelectionPoint`.
    fn scroll_point(&self, x: i64, y: i64) -> Option<Point> {
        let rect = self.rect?;
        if rect.height == 0 {
            return None;
        }
        let top = rect.y as i64;
        let bottom = (self.rows as i64 - 1).min((rect.y + rect.height) as i64 - 1);
        if bottom < top {
            return None;
        }
        let pointer = y.clamp(top, bottom);
        let max_row = self.content.len().max(1) as i64 - 1;
        Some(Point {
            row: (self.view.scroll_top() as i64 + pointer - top).clamp(0, max_row) as usize,
            col: (x - rect.x as i64).clamp(0, rect.width as i64 - 1) as usize,
            scroll: true,
            boundary: false,
        })
    }

    /// `getSelectionPoint`.
    fn point(&self, x: i64, y: i64, scroll: bool) -> Point {
        if scroll && let Some(p) = self.scroll_point(x, y) {
            return p;
        }
        Point {
            row: y.clamp(0, (self.rows as i64 - 1).max(0)) as usize,
            col: x.clamp(0, (self.cols as i64 - 1).max(0)) as usize,
            scroll: false,
            boundary: false,
        }
    }

    fn source_line(&self, p: Point) -> &str {
        let lines = if p.scroll && self.rect.is_some() {
            &self.content
        } else {
            &self.screen
        };
        lines.get(p.row).map_or("", String::as_str)
    }

    /// `getWordSelection`: the word (paths and kebab-case kept whole) under `p`.
    fn word(&self, p: Point) -> Option<Range> {
        let (from, to) = word_selection(&strip_terminal_sequences(self.source_line(p)), p.col)?;
        Some(Range {
            start: Point { col: from, ..p },
            end: Point {
                col: to,
                boundary: true,
                ..p
            },
        })
    }

    /// `getLineSelection`.
    fn line(&self, p: Point) -> Range {
        Range {
            start: Point { col: 0, ..p },
            end: Point {
                col: visible_width(self.source_line(p)),
                boundary: true,
                ..p
            },
        }
    }

    /// `updateSelectionFocus`.
    fn update_focus(&mut self, p: Point) {
        let Some(initial) = self.initial.filter(|_| self.granularity != Granularity::Character) else {
            self.focus = Some(p);
            return;
        };
        let range = if self.granularity == Granularity::Word {
            self.word(p)
        } else {
            Some(self.line(p))
        };
        let Some(range) = range else {
            return;
        };
        let before = range.start.row < initial.start.row
            || (range.start.row == initial.start.row && range.start.col < initial.start.col);
        if before {
            self.anchor = Some(initial.end);
            self.focus = Some(range.start);
        } else {
            self.anchor = Some(initial.start);
            self.focus = Some(range.end);
        }
    }

    /// `getClickCount`.
    fn click_count(&mut self, p: Point, word: Option<Range>, now: Instant) -> u32 {
        let count = match (word, self.last_click) {
            (Some(w), Some(prev))
                if now.saturating_duration_since(prev.at) <= DOUBLE_CLICK_INTERVAL
                    && prev.row == p.row
                    && prev.scroll == p.scroll
                    && prev.word_start == w.start.col
                    && prev.word_end == w.end.col =>
            {
                prev.count % 3 + 1
            }
            _ => 1,
        };
        self.last_click = word.map(|w| Click {
            at: now,
            count,
            row: p.row,
            scroll: p.scroll,
            word_start: w.start.col,
            word_end: w.end.col,
        });
        count
    }

    /// `updateSelectionAutoScroll`.
    fn update_auto_scroll(&mut self, ev: Sgr, now: Instant) {
        let scroll = self.anchor.is_some_and(|a| a.scroll);
        let Some(rect) = self.rect.filter(|r| scroll && r.height > 0) else {
            self.stop_auto_scroll();
            return;
        };
        let top = rect.y as i64;
        let bottom = (self.rows as i64 - 1).min((rect.y + rect.height) as i64 - 1);
        self.drag_pointer = Some((ev.x, ev.y));
        self.auto_direction = if ev.y <= top {
            -1
        } else if ev.y >= bottom {
            1
        } else {
            0
        };
        if self.auto_direction == 0 {
            self.stop_auto_scroll();
            return;
        }
        if self.auto_next.is_none() {
            self.auto_next = Some(now + AUTO_SCROLL_INTERVAL);
        }
    }

    /// One tick of the held drag's auto-scroll (`autoScrollSelection`). True when it scrolled.
    pub fn auto_scroll_tick(&mut self) -> bool {
        let scroll = self.anchor.is_some_and(|a| a.scroll);
        let (Some((x, y)), true) = (self.drag_pointer, scroll) else {
            self.stop_auto_scroll();
            return false;
        };
        let direction = self.auto_direction;
        if direction == 0 {
            self.stop_auto_scroll();
            return false;
        }
        if self.view.scroll_by(direction) == direction {
            self.stop_auto_scroll();
            return false;
        }
        if let Some(p) = self.scroll_point(x, y) {
            self.update_focus(p);
        }
        true
    }

    /// A held drag is auto-scrolling (its interval runs).
    pub fn is_auto_scrolling(&self) -> bool {
        self.auto_next.is_some()
    }

    fn stop_auto_scroll(&mut self) {
        self.auto_next = None;
        self.auto_direction = 0;
        self.drag_pointer = None;
    }

    /// `handleSelectionMouseEvent`.
    fn selection_mouse(&mut self, ev: Sgr, now: Instant) -> ViewportInput {
        let button = ev.button & 3;
        if button != 0 && !(ev.release && button == 3) {
            return ViewportInput::Consumed;
        }
        let anchor_scroll = self.anchor.is_some_and(|a| a.scroll);
        let point = self.point(ev.x, ev.y, anchor_scroll);
        if ev.release {
            if !self.press_active {
                return ViewportInput::Consumed;
            }
            self.press_active = false;
            self.stop_auto_scroll();
            if self.anchor.is_none() {
                return ViewportInput::Consumed;
            }
            self.update_focus(point);
            let is_click = !self.dragged
                && self
                    .anchor
                    .is_some_and(|a| a.scroll == point.scroll && a.row == point.row && a.col == point.col);
            let clicked = self.pressed_url.take().filter(|_| is_click);
            if let (Some(url), Some(open)) = (clicked, self.open_url.clone()) {
                self.anchor = None;
                self.focus = None;
                open(&url);
                return ViewportInput::Consumed;
            }
            if self.copy_on_select
                && let Some(text) = self.selection_text()
            {
                return ViewportInput::Copy(text);
            }
            return ViewportInput::Consumed;
        }
        if ev.button & 32 != 0 {
            if !self.press_active || self.anchor.is_none() {
                return ViewportInput::Consumed;
            }
            self.dragged = true;
            self.last_click = None;
            self.pressed_url = None;
            self.update_focus(point);
            self.update_auto_scroll(ev, now);
            return ViewportInput::Consumed;
        }
        self.stop_auto_scroll();
        self.press_active = true;
        let in_view = self.rect.is_some_and(|r| r.contains(ev.x, ev.y));
        let anchor = self.point(ev.x, ev.y, in_view);
        let word = self.word(anchor);
        let count = self.click_count(anchor, word, now);
        let range = match count {
            2 => word,
            3 => Some(self.line(anchor)),
            _ => None,
        };
        self.granularity = match (range, count) {
            (Some(_), 2) => Granularity::Word,
            (Some(_), _) => Granularity::Line,
            _ => Granularity::Character,
        };
        self.initial = range;
        self.anchor = Some(range.map_or(anchor, |r| r.start));
        self.focus = Some(range.map_or(anchor, |r| r.end));
        self.dragged = false;
        self.pressed_url = if range.is_some() {
            None
        } else {
            let row = ev.y.clamp(0, (self.rows as i64 - 1).max(0)) as usize;
            let col = ev.x.clamp(0, (self.cols as i64 - 1).max(0)) as usize;
            self.screen.get(row).and_then(|line| osc8_link_at_column(line, col))
        };
        ViewportInput::Consumed
    }

    /// `getSelectionBounds`: ordered, or `None` when empty or across views.
    fn bounds(&self) -> Option<(Point, Point)> {
        let (a, f) = (self.anchor?, self.focus?);
        if a.scroll != f.scroll || (a.row == f.row && a.col == f.col) {
            return None;
        }
        let a_first = a.row < f.row || (a.row == f.row && a.col < f.col);
        Some(if a_first { (a, f) } else { (f, a) })
    }

    /// `applySelection`.
    fn apply_selection(&self, screen: Vec<String>) -> Vec<String> {
        let Some((start, end)) = self.bounds() else {
            return screen;
        };
        let (mut min_row, mut max_row) = (0i64, screen.len() as i64 - 1);
        let (mut min_col, mut max_col) = (0, self.cols);
        let (mut s, mut e) = (to_screen(start, 0), to_screen(end, 0));
        if start.scroll {
            let Some(rect) = self.rect else {
                return screen;
            };
            min_row = rect.y as i64;
            max_row = max_row.min((rect.y + rect.height) as i64 - 1);
            min_col = rect.x;
            max_col = self.cols.min(rect.x + rect.width);
            let shift = rect.y as i64 - self.view.scroll_top() as i64;
            s = ScreenPoint {
                row: start.row as i64 + shift,
                col: rect.x + start.col,
                boundary: start.boundary,
            };
            e = ScreenPoint {
                row: end.row as i64 + shift,
                col: rect.x + end.col,
                boundary: end.boundary,
            };
        }
        screen
            .into_iter()
            .enumerate()
            .map(|(row, line)| {
                let r = row as i64;
                if r < min_row || r > max_row || r < s.row || r > e.row || is_image_line(&line) {
                    return line;
                }
                let width = visible_width(&line);
                let (a, b) = selection_columns(&line, r, s, e, min_col, max_col);
                if b <= a {
                    return line;
                }
                let before = slice_by_column(&line, 0, a, true);
                let selected = slice_by_column(&line, a, b - a, true);
                let after = slice_by_column(&line, b, width.saturating_sub(b), true);
                format!("{before}{}{after}", selection_highlight(&selected))
            })
            .collect()
    }

    /// `compositeScrollToEndIndicator`.
    fn composite_indicator(&mut self, mut screen: Vec<String>, width: usize) -> Vec<String> {
        self.indicator_rect = None;
        let (Some(label), Some(rect)) = (self.indicator.as_deref(), self.rect) else {
            return screen;
        };
        if !self.view.follows_end() || self.view.is_following_end() || rect.width == 0 || rect.height == 0 {
            return screen;
        }
        let row = rect.y + rect.height - 1;
        if row >= screen.len() || is_image_line(&screen[row]) {
            return screen;
        }
        let scrollbar = scrollbar_geometry(&self.view, rect, self.content.len(), false).map(|g| g.column);
        let label = truncate_to_width(label, rect.width, "", false);
        let column = rect.x + (rect.width - visible_width(&label)) / 2;
        let right = scrollbar.unwrap_or(rect.x + rect.width);
        let text = truncate_to_width(&label, right.saturating_sub(column), "", false);
        let text_width = visible_width(&text);
        if text_width == 0 {
            return screen;
        }
        screen[row] = composite_line(&screen[row], &text, column, text_width, width);
        self.indicator_rect = Some((row, column, text_width));
        screen
    }

    /// `compositeFlashes`: the newest `height` flashes at the top right.
    fn composite_flashes(&self, mut screen: Vec<String>, width: usize, height: usize) -> Vec<String> {
        let skip = self.flashes.len().saturating_sub(height);
        let lines: Vec<String> = self.flashes[skip..]
            .iter()
            .map(|f| {
                format!(
                    "\x1b[7m{}\x1b[27m",
                    truncate_to_width(&format!(" {} ", f.message), width, "", false)
                )
            })
            .collect();
        if lines.is_empty() {
            return screen;
        }
        if screen.len() < height {
            screen.resize(height, String::new());
        }
        for (row, line) in lines.iter().enumerate() {
            let w = visible_width(line);
            if w == 0 {
                continue;
            }
            screen[row] = composite_line(&screen[row], line, width - w, w, width);
        }
        screen
    }
}

/// The script of a character that Intl.Segmenter splits with ICU's CJ dictionary: Han, Hiragana or Katakana
/// (word-break Katakana: the prolonged sound mark, the vertical kana repeat marks and the half-width forms too).
#[derive(Clone, Copy, PartialEq, Eq)]
enum CjScript {
    Han,
    Hiragana,
    Katakana,
}

fn cj_script(c: char) -> Option<CjScript> {
    match u32::from(c) {
        0x2E80..=0x2FDF
        | 0x3005..=0x3007
        | 0x3021..=0x3029
        | 0x3038..=0x303B
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xF900..=0xFAFF
        | 0x20000..=0x3FFFF => Some(CjScript::Han),
        0x3041..=0x3096 | 0x309D..=0x309F | 0x1B001..=0x1B11F | 0x1F200 => Some(CjScript::Hiragana),
        0x3031..=0x3035
        | 0x309B..=0x309C
        | 0x30A1..=0x30FA
        | 0x30FC..=0x30FF
        | 0x31F0..=0x31FF
        | 0x32D0..=0x32FE
        | 0x3300..=0x3357
        | 0xFF66..=0xFF9D
        | 0x1AFF0..=0x1AFFE
        | 0x1B000 => Some(CjScript::Katakana),
        _ => None,
    }
}

/// `wordSegmenter.segment(line)` with `isWordLike`: UAX #29 word bounds, where a run of one CJ script is one word.
/// Node's ICU splits such runs with its Chinese/Japanese dictionary; without one, a script change is the boundary
/// (日本語|の|テキスト as ICU; 中文分词 stays whole where ICU splits 中文|分词).
fn word_segments(line: &str) -> Vec<(&str, bool)> {
    let mut segments: Vec<(&str, bool, Option<CjScript>)> = Vec::new();
    let mut at = 0;
    for segment in line.split_word_bounds() {
        let script = segment.chars().next().and_then(cj_script);
        let word_like = script.is_some() || segment.chars().any(char::is_alphanumeric);
        match segments.last_mut() {
            Some(last) if script.is_some() && last.2 == script => {
                last.0 = &line[at - last.0.len()..at + segment.len()];
            }
            _ => segments.push((segment, word_like, script)),
        }
        at += segment.len();
    }
    segments.into_iter().map(|(s, w, _)| (s, w)).collect()
}

/// `getWordSelection` on a plain `line`: the columns `[start, end)` of the word under column `col` (`/` and `-`
/// join selectable neighbours, so paths and kebab-case stay whole), or `None` past the line's end.
pub fn word_selection(line: &str, col: usize) -> Option<(usize, usize)> {
    let mut segments: Vec<(usize, usize, bool, bool)> = Vec::new();
    let mut start = 0;
    for (segment, word_like) in word_segments(line) {
        let end = start + visible_width(segment);
        let joiner = segment == "/" || segment == "-";
        segments.push((start, end, word_like || joiner, joiner));
        start = end;
    }
    let clicked = segments.iter().position(|s| col >= s.0 && col < s.1)?;
    let join = |l: (usize, usize, bool, bool), r: (usize, usize, bool, bool)| l.2 && r.2 && (l.3 || r.3);
    let (mut from, mut to) = (segments[clicked].0, segments[clicked].1);
    let mut i = clicked;
    while i > 0 && join(segments[i - 1], segments[i]) {
        from = segments[i - 1].0;
        i -= 1;
    }
    let mut i = clicked;
    while i + 1 < segments.len() && join(segments[i], segments[i + 1]) {
        to = segments[i + 1].1;
        i += 1;
    }
    Some((from, to))
}

fn to_screen(p: Point, shift: i64) -> ScreenPoint {
    ScreenPoint {
        row: p.row as i64 + shift,
        col: p.col,
        boundary: p.boundary,
    }
}

/// `getSelectionColumns`: the columns of `line` (screen or content row `row`) inside the selection `s`..`e`.
fn selection_columns(
    line: &str,
    row: i64,
    s: ScreenPoint,
    e: ScreenPoint,
    min_col: usize,
    max_col: usize,
) -> (usize, usize) {
    let width = visible_width(line);
    let mut start = min_col;
    let mut end = width.min(max_col);
    if row == s.row {
        start = grapheme_cell_range(line, s.col).map_or(s.col.min(width), |r| r.0);
    }
    if row == e.row {
        end = if e.boundary {
            e.col.min(width)
        } else {
            grapheme_cell_range(line, e.col).map_or((e.col + 1).min(width), |r| r.1)
        };
    }
    (start.max(min_col), end.min(max_col))
}

/// `applySelectionHighlight`: inverse video that survives the text's own SGR codes.
fn selection_highlight(text: &str) -> String {
    let mut out = String::from("\x1b[7m");
    let mut i = 0;
    while i < text.len() {
        match extract_ansi_code(text, i) {
            Some(code) => {
                out.push_str(code);
                if code.ends_with('m') {
                    out.push_str("\x1b[7m");
                }
                i += code.len();
            }
            None => {
                let c = text[i..].chars().next().unwrap_or('\0');
                out.push(c);
                i += c.len_utf8().max(1);
            }
        }
    }
    out.push_str("\x1b[27m");
    out
}

/// `parseWheelEvent`: (direction, x, y, button) of a vertical wheel report (SGR or X10).
fn parse_wheel(data: &str) -> Option<(i64, i64, i64, u32)> {
    if let Some(m) = parse_sgr_mouse(data) {
        if m.button & 64 == 0 {
            return None;
        }
        return match m.button & 3 {
            0 => Some((-1, m.col, m.row, m.button)),
            1 => Some((1, m.col, m.row, m.button)),
            _ => None,
        };
    }
    let chars: Vec<char> = data.chars().collect();
    if chars.len() == 6 && data.starts_with("\x1b[M") {
        let button = (chars[3] as u32).wrapping_sub(32);
        if button & 64 == 0 {
            return None;
        }
        let direction = match button & 3 {
            0 => -1,
            1 => 1,
            _ => return None,
        };
        return Some((direction, chars[4] as i64 - 33, chars[5] as i64 - 33, button));
    }
    None
}

/// `isMouseSequence`.
pub fn is_mouse_sequence(data: &str) -> bool {
    parse_sgr_mouse(data).is_some() || (data.chars().count() == 6 && data.starts_with("\x1b[M"))
}

/// Whether `data` is a terminal focus report (`\x1b[I` / `\x1b[O`).
pub fn is_focus_report(data: &str) -> bool {
    data == FOCUS_IN || data == FOCUS_OUT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::ansi_len;

    #[test]
    fn zone_prefix_strips_every_leading_marker() {
        assert_eq!(strip_zone_prefix("\x1b]133;A\x07\x1b]133;B\x1b\\x"), "x");
        assert_eq!(strip_zone_prefix("\x1b]133;D\x07x"), "\x1b]133;D\x07x");
        assert_eq!(strip_zone_prefix("plain"), "plain");
    }

    #[test]
    fn ansi_len_is_used_for_highlight() {
        assert_eq!(ansi_len("\x1b[1mx", 0), Some(4));
        assert_eq!(selection_highlight("\x1b[1mab"), "\x1b[7m\x1b[1m\x1b[7mab\x1b[27m");
    }
}
