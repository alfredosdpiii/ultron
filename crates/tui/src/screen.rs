//! Full-screen (alternate screen) presentation: stack size allocation (`allocateStackSizes` of
//! packages/tui/src/components/stack.ts), a transcript viewport that follows its end (`ScrollView` with
//! `follow: "end"`), and the alternate-screen painter of `TuiAltScreen` that rewrites only changed rows.

use crate::renderer::CURSOR_MARKER;
use crate::utils::{truncate_to_width, visible_width};

/// One child of a vertical stack.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StackEntry {
    /// `None` is `"auto"` (the rendered height).
    pub basis: Option<usize>,
    pub grow: usize,
    /// `None` is the default weight 1.
    pub shrink: Option<usize>,
    pub min_size: usize,
    pub max_size: Option<usize>,
}

fn clamp_size(size: usize, e: &StackEntry) -> usize {
    let max = e.max_size.unwrap_or(usize::MAX).max(e.min_size);
    size.clamp(e.min_size, max)
}

fn distribute(sizes: &mut [usize], entries: &[StackEntry], amount: usize, grow: bool) {
    let mut remaining = amount;
    while remaining > 0 {
        let candidates: Vec<usize> = (0..entries.len())
            .filter(|&i| {
                let e = &entries[i];
                if grow {
                    e.grow > 0 && sizes[i] < e.max_size.unwrap_or(usize::MAX)
                } else {
                    e.shrink.unwrap_or(1) > 0 && sizes[i] > e.min_size
                }
            })
            .collect();
        if candidates.is_empty() {
            return;
        }
        let weights: Vec<usize> = candidates
            .iter()
            .map(|&i| {
                if grow {
                    entries[i].grow
                } else {
                    entries[i].shrink.unwrap_or(1) * sizes[i].max(1)
                }
            })
            .collect();
        let total: usize = weights.iter().sum();
        let mut distributed = 0;
        for (&i, &weight) in candidates.iter().zip(&weights) {
            if remaining == 0 {
                break;
            }
            let proposed = (remaining * weight / total).max(1);
            let capacity = if grow {
                entries[i].max_size.unwrap_or(usize::MAX) - sizes[i]
            } else {
                sizes[i] - entries[i].min_size
            };
            let delta = remaining.min(proposed).min(capacity);
            if delta == 0 {
                continue;
            }
            if grow {
                sizes[i] += delta;
            } else {
                sizes[i] -= delta;
            }
            remaining -= delta;
            distributed += delta;
        }
        if distributed == 0 {
            return;
        }
    }
}

/// `allocateStackSizes` with no gap: each entry's size from its basis (or `intrinsic`), grown or shrunk to fill
/// `available`.
pub fn allocate_stack_sizes(entries: &[StackEntry], intrinsic: &[usize], available: Option<usize>) -> Vec<usize> {
    let mut sizes: Vec<usize> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| clamp_size(e.basis.unwrap_or(intrinsic.get(i).copied().unwrap_or(0)), e))
        .collect();
    let Some(available) = available else {
        return sizes;
    };
    let total: usize = sizes.iter().sum();
    if total < available {
        distribute(&mut sizes, entries, available - total, true);
    } else if total > available {
        distribute(&mut sizes, entries, total - available, false);
    }
    sizes
}

/// Lay out `children` (already rendered) in `height` rows: each gets its allocated rows, cut or padded with blank
/// lines. `scroll_end[i]` keeps the last rows of an overflowing child (a viewport following its end) instead of
/// the first.
pub fn stack_lines(
    children: Vec<Vec<String>>,
    entries: &[StackEntry],
    scroll_end: &[bool],
    height: usize,
) -> Vec<String> {
    let intrinsic: Vec<usize> = children.iter().map(Vec::len).collect();
    let sizes = allocate_stack_sizes(entries, &intrinsic, Some(height));
    let mut lines = Vec::with_capacity(height);
    for (i, (child, size)) in children.into_iter().zip(sizes).enumerate() {
        let skip = if scroll_end.get(i).copied().unwrap_or(false) {
            child.len().saturating_sub(size)
        } else {
            0
        };
        let shown: Vec<String> = child.into_iter().skip(skip).take(size).collect();
        let pad = size - shown.len();
        lines.extend(shown);
        lines.extend(std::iter::repeat_n(String::new(), pad));
    }
    lines
}

const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
const EXIT_ALT_SCREEN: &str = "\x1b[?1049l";
const DISABLE_AUTOWRAP: &str = "\x1b[?7l";
const ENABLE_AUTOWRAP: &str = "\x1b[?7h";
const BEGIN_SYNC: &str = "\x1b[?2026h";
const END_SYNC: &str = "\x1b[?2026l";
const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

/// The alternate-screen painter: [`AltScreen::enter`], then [`AltScreen::paint`] per frame (only rows that changed
/// are rewritten), then [`AltScreen::exit`] which leaves the final document on the main screen.
#[derive(Default)]
pub struct AltScreen {
    previous: Vec<String>,
    width: usize,
    height: usize,
    pub show_hardware_cursor: bool,
    pub full_redraws: usize,
}

impl AltScreen {
    pub fn new() -> Self {
        Self::default()
    }

    /// Switch to the alternate screen, without autowrap, cleared, cursor hidden.
    pub fn enter(&mut self) -> String {
        self.enter_with_mouse("")
    }

    /// [`AltScreen::enter`] that also turns on `mouse` tracking (`beforeTerminalStart` with `mouse: true`; see
    /// [`crate::fullscreen::mouse_tracking`]).
    pub fn enter_with_mouse(&mut self, mouse: &str) -> String {
        self.previous.clear();
        format!("{ENTER_ALT_SCREEN}{DISABLE_AUTOWRAP}{mouse}\x1b[2J\x1b[H\x1b[?25l")
    }

    /// `beforeTerminalStop` with mouse tracking on: tracking off and autowrap back on, before [`AltScreen::exit`] or
    /// a handover of the terminal.
    pub fn release_mouse() -> String {
        format!(
            "{BEGIN_SYNC}{}{ENABLE_AUTOWRAP}{END_SYNC}",
            crate::fullscreen::DISABLE_MOUSE
        )
    }

    /// One frame of exactly `height` rows (extra rows are dropped, missing rows blank); a cursor marker places the
    /// hardware cursor.
    pub fn paint(&mut self, lines: &[String], width: usize, height: usize) -> String {
        let mut screen: Vec<String> = Vec::with_capacity(height);
        let mut cursor = None;
        for (row, line) in lines.iter().take(height).enumerate() {
            let mut line = line.clone();
            if let Some(idx) = line.find(CURSOR_MARKER) {
                cursor = Some((row, visible_width(&line[..idx])));
                line.replace_range(idx..idx + CURSOR_MARKER.len(), "");
            }
            if visible_width(&line) > width {
                line = truncate_to_width(&line, width, "", false);
            }
            screen.push(format!("{line}{SEGMENT_RESET}"));
        }
        screen.resize(height, SEGMENT_RESET.to_string());
        let full = self.width != width || self.height != height || self.previous.is_empty();
        let mut out = String::from(BEGIN_SYNC);
        if full {
            self.full_redraws += 1;
            out.push_str("\x1b[2J");
        }
        for (row, line) in screen.iter().enumerate() {
            if !full && self.previous.get(row) == Some(line) {
                continue;
            }
            out.push_str(&format!("\x1b[{};1H\x1b[2K{line}", row + 1));
        }
        match cursor {
            Some((row, col)) => {
                out.push_str(&format!("\x1b[{};{}H", row + 1, col.min(width) + 1));
                out.push_str(if self.show_hardware_cursor {
                    "\x1b[?25h"
                } else {
                    "\x1b[?25l"
                });
            }
            None => out.push_str("\x1b[?25l"),
        }
        out.push_str(END_SYNC);
        self.previous = screen;
        self.width = width;
        self.height = height;
        out
    }

    /// Leave the alternate screen and print `document` (the whole component render, cursor markers removed) on the
    /// main screen, as `TuiAltScreen.afterTerminalStop` does.
    pub fn exit(&mut self, document: &[String], width: usize) -> String {
        let mut out = format!("{BEGIN_SYNC}{EXIT_ALT_SCREEN}{DISABLE_AUTOWRAP}");
        for (row, line) in document.iter().enumerate() {
            let mut line = line.replace(CURSOR_MARKER, "");
            if visible_width(&line) > width {
                line = truncate_to_width(&line, width, "", false);
            }
            if row > 0 {
                out.push_str("\r\n");
            }
            out.push_str(&format!("\r\x1b[2K{line}{SEGMENT_RESET}"));
        }
        out.push_str(&format!("\x1b[0m{ENABLE_AUTOWRAP}\r\n\x1b[?25h{END_SYNC}"));
        self.previous.clear();
        out
    }
}
