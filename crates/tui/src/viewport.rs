//! History/viewport split (omp docs/tui-core-renderer.md, packages/tui/src/tui.ts frame plans).
//!
//! The application decides finality. Finished transcript blocks are rendered once into an append-only
//! [`HistoryBatch`], written to terminal scrollback exactly once, acknowledged by id, and then dropped from the
//! per-frame work (their render caches released). Only the mutable viewport (live blocks plus chrome such as the
//! editor and status line) is rendered and diffed each frame, so frame cost is bounded by the live area, not by the
//! transcript length.
//!
//! Kitty images are drawn as the TypeScript main screen (tui-main-screen.ts) draws them: an image line's reserved
//! rows are made (and cleared) before the image is placed on its first row, an image whose rows change is deleted
//! by id before it is drawn again, and a full repaint deletes the images it repaints. Images written to scrollback
//! scroll with it; a replay (which clears scrollback) deletes them too.
//!
//! [`RenderGate`] paces frames (30 fps cadence, adaptive duty cycle) and skips frames while the terminal writer's
//! backlog exceeds [`crate::tty_writer::MAX_PENDING_OUTPUT_BYTES`]; the retry paints the latest state only.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use crate::components::Component;
use crate::image::delete_kitty_image;
use crate::renderer::{CURSOR_MARKER, is_image_line};
use crate::tty_writer::MAX_PENDING_OUTPUT_BYTES;
use crate::utils::{normalize_terminal_output, truncate_to_width, visible_width};

const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";
const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";

/// How a history batch reaches the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HistoryKind {
    /// Finalized rows appended below the retained history.
    Append,
    /// The complete ledger after a destructive reset (clears screen and scrollback first).
    Replay,
}

/// Rows written to scrollback exactly once, identified by a monotonic id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryBatch {
    pub id: u64,
    pub rows: Vec<String>,
    pub kind: HistoryKind,
}

/// One frame: optional history to append, and the complete mutable viewport.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FramePlan {
    pub history: Option<HistoryBatch>,
    pub viewport: Vec<String>,
}

/// Bytes for one frame and the history id the caller must acknowledge.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Frame {
    pub bytes: String,
    pub acknowledged: Option<u64>,
}

/// Paints [`FramePlan`]s: history is appended once above the viewport, viewport rows are diffed.
#[derive(Debug, Default)]
pub struct ViewportRenderer {
    prev: Vec<String>,
    /// Hardware cursor row relative to the viewport's top row.
    cursor_row: usize,
    /// Rows that exist below the anchor (reachable with cursor-down without scrolling).
    drawn: usize,
    width: usize,
    height: usize,
    accepted_history: u64,
    painted: bool,
    force_repaint: bool,
    pub show_hardware_cursor: bool,
    /// `setClearOnShrink`: a viewport that shrinks below the most rows it has drawn since its last full paint is
    /// painted whole (TUI main-screen `clearOnShrink`) instead of diffed.
    pub clear_on_shrink: bool,
    /// The most viewport rows painted since the last full paint (`maxLinesRendered`).
    max_rows: usize,
    /// Viewport rows written over the renderer's lifetime (diff efficiency, tests).
    pub rows_written: usize,
    pub history_rows_written: usize,
    /// Kitty image ids written to scrollback (deleted when a replay clears it).
    history_images: BTreeSet<u32>,
}

/// `parseKittyImageHeader`: the image ids (`i=`) and cell rows (`r=`, default 1) of a line's first Kitty command.
fn kitty_header(line: &str) -> Option<(Vec<u32>, usize)> {
    let start = line.find("\x1b_G")? + 3;
    let end = start + line[start..].find(';')?;
    let mut ids = Vec::new();
    let mut rows = 1;
    for param in line[start..end].split(',') {
        let Some((key, value)) = param.split_once('=') else {
            continue;
        };
        // `Number(value)`: an integer in 1..=0xffffffff.
        let Some(n) = value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| (1..=0xffff_ffff).contains(n))
        else {
            continue;
        };
        match key {
            "i" => ids.push(n as u32),
            "r" => rows = n as usize,
            _ => {}
        }
    }
    Some((ids, rows))
}

fn kitty_ids(line: &str) -> Vec<u32> {
    kitty_header(line).map(|(ids, _)| ids).unwrap_or_default()
}

/// `getKittyImageReservedRows`: the rows an image line at `index` keeps (its `r=`, through the blank rows after it,
/// at most to `max_index`).
fn reserved_rows(lines: &[String], index: usize, max_index: usize) -> usize {
    let rows = kitty_header(&lines[index]).map_or(1, |(_, rows)| rows);
    if rows <= 1 {
        return 1;
    }
    let max = rows.min((max_index + 1).saturating_sub(index)).min(lines.len() - index);
    let mut reserved = 1;
    while reserved < max {
        let line = &lines[index + reserved];
        if is_image_line(line) || visible_width(line) > 0 {
            break;
        }
        reserved += 1;
    }
    reserved
}

/// The rows of `rows` to rewrite: those that differ from `prev`, widened to whole images
/// (`expandChangedRangeForKittyImages`): an image (shown before or now) with a changed row, or a row past the new
/// end, is redrawn whole.
fn changed_rows(prev: &[String], rows: &[String]) -> Vec<bool> {
    let n = rows.len();
    let original: Vec<bool> = (0..n).map(|i| prev.get(i) != Some(&rows[i])).collect();
    let mut changed = original.clone();
    for lines in [prev, rows] {
        for i in 0..lines.len() {
            if kitty_ids(&lines[i]).is_empty() {
                continue;
            }
            let end = i + reserved_rows(lines, i, lines.len() - 1);
            if (i..end).any(|row| original.get(row).is_none_or(|c| *c)) {
                for flag in changed.iter_mut().take(end).skip(i) {
                    *flag = true;
                }
            }
        }
    }
    changed
}

/// `fullRender`'s rows: an image line taller than one row makes its rows first (`\r\n` each), goes back up, is
/// placed, and goes down again, so placing it never scrolls; one taller than `height` is placed on its first row.
/// Every row is followed by `\r\n` with `trailing`, else only separated by it.
fn write_rows(out: &mut String, rows: &[String], height: usize, trailing: bool) {
    let mut i = 0;
    while i < rows.len() {
        if i > 0 && !trailing {
            out.push_str("\r\n");
        }
        let line = &rows[i];
        let reserved = if is_image_line(line) {
            reserved_rows(rows, i, rows.len() - 1)
        } else {
            1
        };
        if reserved > 1 && reserved <= height {
            let up = reserved - 1;
            out.push_str(&"\r\n".repeat(up));
            out.push_str(&format!("\x1b[{up}A{line}\x1b[{up}B"));
            i += reserved;
        } else {
            out.push_str(line);
            i += 1;
        }
        if trailing {
            out.push_str("\r\n");
        }
    }
}

impl ViewportRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Repaint the whole viewport on the next frame (overlay close, theme change).
    pub fn invalidate(&mut self) {
        self.force_repaint = true;
    }

    /// Delete every Kitty image this renderer drew, in scrollback and in the viewport (before the terminal is
    /// cleared by someone else, as a display reset clears it).
    pub fn delete_images(&mut self) -> String {
        let mut ids: BTreeSet<u32> = self.prev.iter().flat_map(|l| kitty_ids(l)).collect();
        ids.append(&mut self.history_images);
        ids.into_iter().map(delete_kitty_image).collect()
    }

    /// Rows of the last painted viewport (prepared: normalized, clamped, reset-terminated).
    pub fn viewport(&self) -> &[String] {
        &self.prev
    }

    fn prepare(line: &str, width: usize) -> String {
        if is_image_line(line) {
            return line.to_string();
        }
        let line = normalize_terminal_output(line);
        let line = if visible_width(&line) > width {
            truncate_to_width(&line, width, "", false)
        } else {
            line
        };
        line + SEGMENT_RESET
    }

    fn goto(&mut self, out: &mut String, row: usize) {
        let last = self.drawn.max(1) - 1;
        if row <= last {
            let diff = row as i64 - self.cursor_row as i64;
            if diff > 0 {
                out.push_str(&format!("\x1b[{diff}B"));
            } else if diff < 0 {
                out.push_str(&format!("\x1b[{}A", -diff));
            }
        } else {
            self.goto(out, last);
            out.push_str(&"\r\n".repeat(row - last));
            self.drawn = row + 1;
        }
        self.cursor_row = row;
    }

    /// Paint `plan` on a `width` x `height` terminal.
    pub fn render(&mut self, plan: &FramePlan, width: usize, height: usize) -> Frame {
        let height = height.max(1);
        let skip = plan.viewport.len().saturating_sub(height);
        let mut rows: Vec<String> = plan.viewport[skip..].to_vec();
        // An image cut by the top of the screen shows its visible rows on the first row.
        crate::screen::crop_images(&plan.viewport, skip, &mut rows);
        let cursor = extract_cursor(&mut rows);
        let rows: Vec<String> = rows.iter().map(|r| Self::prepare(r, width)).collect();

        let history = plan.history.as_ref().filter(|h| h.id > self.accepted_history);
        let stale_ack = plan
            .history
            .as_ref()
            .filter(|h| h.id <= self.accepted_history)
            .map(|h| h.id);
        let geometry_changed = self.painted && (width != self.width || height != self.height);
        let replay = history.is_some_and(|h| h.kind == HistoryKind::Replay);

        let shrunk = self.painted && self.clear_on_shrink && rows.len() < self.max_rows;
        let mut out = String::from(SYNC_BEGIN);
        if !self.painted || history.is_some() || geometry_changed || self.force_repaint || shrunk {
            // The images being repainted go first (`deleteKittyImages(previousKittyImageIds)`); a replay clears
            // scrollback, so the images written there go too.
            let mut deleted: BTreeSet<u32> = self.prev.iter().flat_map(|l| kitty_ids(l)).collect();
            if replay {
                deleted.append(&mut self.history_images);
            }
            for id in deleted {
                out.push_str(&delete_kitty_image(id));
            }
            if replay {
                out.push_str("\x1b[H\x1b[2J\x1b[3J");
            } else if self.painted {
                let up = self.cursor_row.min(height - 1);
                if up > 0 {
                    out.push_str(&format!("\x1b[{up}A"));
                }
                out.push_str("\r\x1b[J");
            }
            if let Some(h) = history {
                let prepared: Vec<String> = h.rows.iter().map(|row| Self::prepare(row, width)).collect();
                write_rows(&mut out, &prepared, height, true);
                self.history_images.extend(prepared.iter().flat_map(|l| kitty_ids(l)));
                self.history_rows_written += h.rows.len();
                self.accepted_history = h.id;
            }
            write_rows(&mut out, &rows, height, false);
            self.rows_written += rows.len();
            self.cursor_row = rows.len().saturating_sub(1);
            self.drawn = rows.len().max(1);
            self.max_rows = rows.len();
        } else {
            self.max_rows = self.max_rows.max(rows.len());
            let changed = changed_rows(&self.prev, &rows);
            // `deleteChangedKittyImages`: the images shown on changed rows (or rows now gone) are deleted before
            // anything is drawn.
            let deleted: BTreeSet<u32> = self
                .prev
                .iter()
                .enumerate()
                .filter(|(i, _)| changed.get(*i).is_none_or(|c| *c))
                .flat_map(|(_, l)| kitty_ids(l))
                .collect();
            for id in deleted {
                out.push_str(&delete_kitty_image(id));
            }
            let mut i = 0;
            while i < rows.len() {
                if !changed[i] {
                    i += 1;
                    continue;
                }
                let row = &rows[i];
                let reserved = if is_image_line(row) {
                    reserved_rows(&rows, i, rows.len() - 1)
                } else {
                    1
                };
                self.goto(&mut out, i);
                if reserved > 1 {
                    // Clear the reserved rows first, then place the image on its first row.
                    let up = reserved - 1;
                    out.push_str("\r\x1b[2K");
                    for k in 1..reserved {
                        self.goto(&mut out, i + k);
                        out.push_str("\r\x1b[2K");
                    }
                    out.push_str(&format!("\x1b[{up}A{row}\x1b[{up}B"));
                    self.rows_written += reserved;
                    i += reserved;
                    continue;
                }
                out.push_str("\r\x1b[2K");
                out.push_str(row);
                self.rows_written += 1;
                i += 1;
            }
            if rows.len() < self.prev.len() {
                self.goto(&mut out, rows.len());
                out.push_str("\r\x1b[J");
            }
        }
        match cursor.filter(|_| !rows.is_empty()) {
            Some((row, col)) => {
                self.goto(&mut out, row.min(rows.len() - 1));
                out.push_str(&format!("\x1b[{}G", col + 1));
                out.push_str(if self.show_hardware_cursor {
                    "\x1b[?25h"
                } else {
                    "\x1b[?25l"
                });
            }
            None => out.push_str("\x1b[?25l"),
        }
        out.push_str(SYNC_END);

        self.prev = rows;
        self.width = width;
        self.height = height;
        self.painted = true;
        self.force_repaint = false;
        Frame {
            bytes: out,
            acknowledged: history.map(|h| h.id).or(stale_ack),
        }
    }
}

/// Strip the last [`CURSOR_MARKER`] and return its (row, column).
fn extract_cursor(rows: &mut [String]) -> Option<(usize, usize)> {
    for (row, line) in rows.iter_mut().enumerate().rev() {
        if let Some(idx) = line.find(CURSOR_MARKER) {
            let col = visible_width(&line[..idx]);
            line.replace_range(idx..idx + CURSOR_MARKER.len(), "");
            return Some((row, col));
        }
    }
    None
}

/// Identifies a block in a [`Transcript`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockId(u64);

struct LiveBlock {
    id: BlockId,
    component: Box<dyn Component>,
    finished: bool,
}

/// Transcript ledger: live blocks render every frame; finished blocks at the head retire into history once.
pub struct Transcript {
    live: Vec<LiveBlock>,
    retired: Vec<Box<dyn Component>>,
    pending: Option<HistoryBatch>,
    next_history_id: u64,
    next_block: u64,
    /// Keep retired components (caches released) so a reset or resize can replay them.
    pub keep_retired: bool,
    /// Component renders performed by frames (work counter for tests and diagnostics).
    pub renders: usize,
}

impl Default for Transcript {
    fn default() -> Self {
        Self {
            live: Vec::new(),
            retired: Vec::new(),
            pending: None,
            next_history_id: 1,
            next_block: 0,
            keep_retired: true,
            renders: 0,
        }
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a mutable block.
    pub fn push_live(&mut self, component: Box<dyn Component>) -> BlockId {
        self.push(component, false)
    }

    /// Append a block that is already final (it retires as soon as every block above it has).
    pub fn push_final(&mut self, component: Box<dyn Component>) -> BlockId {
        self.push(component, true)
    }

    fn push(&mut self, component: Box<dyn Component>, finished: bool) -> BlockId {
        let id = BlockId(self.next_block);
        self.next_block += 1;
        self.live.push(LiveBlock {
            id,
            component,
            finished,
        });
        id
    }

    /// A live block, for updates; `None` once it retired.
    pub fn block_mut(&mut self, id: BlockId) -> Option<&mut Box<dyn Component>> {
        self.live.iter_mut().find(|b| b.id == id).map(|b| &mut b.component)
    }

    /// Declare a block final. Retirement keeps transcript order: it waits for earlier blocks.
    pub fn finish(&mut self, id: BlockId) {
        if let Some(b) = self.live.iter_mut().find(|b| b.id == id) {
            b.finished = true;
        }
    }

    pub fn live_len(&self) -> usize {
        self.live.len()
    }

    pub fn retired_len(&self) -> usize {
        self.retired.len()
    }

    /// Plan a frame: an unacknowledged batch is offered again; otherwise the finished head retires into a new
    /// batch. The viewport is the live blocks followed by `chrome` (editor, status line).
    pub fn frame(&mut self, width: usize, chrome: &[String]) -> FramePlan {
        if self.pending.is_none() {
            let done = self.live.iter().take_while(|b| b.finished).count();
            let mut rows = Vec::new();
            for mut block in self.live.drain(..done) {
                rows.extend(block.component.render(width));
                self.renders += 1;
                block.component.release_cache();
                if self.keep_retired {
                    self.retired.push(block.component);
                }
            }
            if !rows.is_empty() {
                self.pending = Some(HistoryBatch {
                    id: self.next_history_id,
                    rows,
                    kind: HistoryKind::Append,
                });
                self.next_history_id += 1;
            }
        }
        let mut viewport = Vec::new();
        for block in &mut self.live {
            viewport.extend(block.component.render(width));
            self.renders += 1;
        }
        viewport.extend(chrome.iter().cloned());
        FramePlan {
            history: self.pending.clone(),
            viewport,
        }
    }

    /// The renderer wrote batch `id`.
    pub fn acknowledge(&mut self, id: u64) {
        if self.pending.as_ref().is_some_and(|p| p.id == id) {
            self.pending = None;
        }
    }

    /// Every transcript row at `width`: retired (scrollback) blocks then live ones. For on-demand views such as
    /// transcript search; retired caches are released again afterwards.
    pub fn all_rows(&mut self, width: usize) -> Vec<String> {
        let mut rows = Vec::new();
        for c in &mut self.retired {
            rows.extend(c.render(width));
            c.release_cache();
        }
        for block in &mut self.live {
            rows.extend(block.component.render(width));
        }
        rows
    }

    /// A destructive replay of every retired block at `width` (resize rebuild, display reset). Consumes a fresh id.
    pub fn replay(&mut self, width: usize, chrome: &[String]) -> FramePlan {
        let mut rows = Vec::new();
        for c in &mut self.retired {
            rows.extend(c.render(width));
            c.release_cache();
        }
        let id = self.next_history_id;
        self.next_history_id += 1;
        self.pending = Some(HistoryBatch {
            id,
            rows,
            kind: HistoryKind::Replay,
        });
        self.frame(width, chrome)
    }
}

/// What the render loop should do now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GateDecision {
    /// Nothing requested.
    Idle,
    /// Compose and paint the latest state now.
    Render,
    /// Check again after this long (cadence, or the output backlog is too deep).
    Wait(Duration),
}

/// Frame pacing with output backpressure (omp `#scheduleRender` / `#deferRenderForOutputBacklog`).
#[derive(Clone, Debug)]
pub struct RenderGate {
    requested: bool,
    last_render: Option<Instant>,
    last_cost: Duration,
    pub min_interval: Duration,
    pub max_adaptive: Duration,
    pub max_pending: usize,
    pub backlog_retry: Duration,
    /// Frames deferred because of the output backlog.
    pub skipped: usize,
}

impl Default for RenderGate {
    fn default() -> Self {
        Self {
            requested: false,
            last_render: None,
            last_cost: Duration::ZERO,
            min_interval: Duration::from_micros(33_333),
            max_adaptive: Duration::from_millis(200),
            max_pending: MAX_PENDING_OUTPUT_BYTES,
            backlog_retry: Duration::from_millis(10),
            skipped: 0,
        }
    }
}

impl RenderGate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request(&mut self) {
        self.requested = true;
    }

    pub fn requested(&self) -> bool {
        self.requested
    }

    /// Decide at `now`; `pending` is the writer's backlog (`None` for writers that do not report one).
    pub fn poll(&mut self, now: Instant, pending: Option<usize>) -> GateDecision {
        if !self.requested {
            return GateDecision::Idle;
        }
        if pending.is_some_and(|p| p > self.max_pending) {
            self.skipped += 1;
            return GateDecision::Wait(self.backlog_retry);
        }
        if let Some(last) = self.last_render {
            let elapsed = now.saturating_duration_since(last);
            let floor = self.min_interval.max(self.max_adaptive.min(self.last_cost * 2));
            if elapsed < floor {
                return GateDecision::Wait(floor - elapsed);
            }
        }
        self.requested = false;
        self.last_render = Some(now);
        GateDecision::Render
    }

    /// Record how long the frame that `poll` allowed took to compose and enqueue.
    pub fn rendered(&mut self, cost: Duration) {
        self.last_cost = cost;
    }
}
