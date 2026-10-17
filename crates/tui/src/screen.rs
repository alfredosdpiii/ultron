//! Full-screen (alternate screen) presentation: stack size allocation (`allocateStackSizes` of
//! packages/tui/src/components/stack.ts), a transcript viewport that follows its end (`ScrollView` with
//! `follow: "end"`), and the alternate-screen painter of `TuiAltScreen` that rewrites only changed rows.

use crate::image::{
    DELETE_ALL_KITTY_IMAGES, DELETE_ALL_KITTY_PLACEMENTS, ImageProtocol, delete_kitty_image, kitty_image_placement,
};
use crate::renderer::{CURSOR_MARKER, is_image_line};
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
        let mut shown: Vec<String> = child.iter().skip(skip).take(size).cloned().collect();
        crop_images(&child, skip, &mut shown);
        let pad = size - shown.len();
        lines.extend(shown);
        lines.extend(std::iter::repeat_n(String::new(), pad));
    }
    lines
}

/// `paintBox` for Kitty images: an image running past the bottom of its rows is cropped to the rows it has, and
/// one whose first line scrolled above them is cropped to its visible rows and placed on the first row.
pub(crate) fn crop_images(child: &[String], skip: usize, shown: &mut [String]) {
    use crate::image::{crop_kitty_image_line, kitty_image_metadata};
    let height = shown.len() as i64;
    for (row, line) in shown.iter_mut().enumerate() {
        if let Some(metadata) = kitty_image_metadata(line) {
            let visible = i64::from(metadata.rows).min(height - row as i64);
            if visible < i64::from(metadata.rows) {
                *line = crop_kitty_image_line(line, 0, visible);
            }
        }
    }
    if skip == 0 || shown.is_empty() {
        return;
    }
    for image_row in (0..skip).rev() {
        let line = &child[image_row];
        if let Some(metadata) = kitty_image_metadata(line) {
            let hidden = (skip - image_row) as i64;
            let rows = i64::from(metadata.rows);
            if hidden < rows {
                shown[0] = crop_kitty_image_line(line, hidden, height.min(rows - hidden));
            }
            break;
        }
        if !line.is_empty() {
            break;
        }
    }
}

const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
const EXIT_ALT_SCREEN: &str = "\x1b[?1049l";
const DISABLE_AUTOWRAP: &str = "\x1b[?7l";
const ENABLE_AUTOWRAP: &str = "\x1b[?7h";
const BEGIN_SYNC: &str = "\x1b[?2026h";
const END_SYNC: &str = "\x1b[?2026l";
const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

/// `MAX_CACHED_OFFSCREEN_KITTY_*`: how many uploaded images off screen are kept for placement-only reuse.
const MAX_CACHED_OFFSCREEN_IMAGES: usize = 16;
const MAX_CACHED_OFFSCREEN_TRANSMISSION_BYTES: usize = 32 * 1024 * 1024;
const MAX_CACHED_OFFSCREEN_DECODED_BYTES: u64 = 64 * 1024 * 1024;

/// An image the terminal holds (`uploadedKittyImages`).
#[derive(Clone, Copy, Debug)]
struct Uploaded {
    generation: u64,
    transmission_bytes: usize,
    decoded_bytes: u64,
}

/// The alternate-screen painter: [`AltScreen::enter`], then [`AltScreen::paint`] per frame (only rows that changed
/// are rewritten), then [`AltScreen::exit`] which leaves the final document on the main screen. Image lines
/// (`TuiAltScreen.doRender`) are written as they are; a frame whose image rows changed is redrawn whole, with Kitty
/// images already uploaded placed again instead of transmitted.
#[derive(Default)]
pub struct AltScreen {
    previous: Vec<String>,
    width: usize,
    height: usize,
    pub show_hardware_cursor: bool,
    pub full_redraws: usize,
    /// The terminal's image protocol (`getCapabilities().images` at start).
    pub image_protocol: Option<ImageProtocol>,
    /// WezTerm (`WEZTERM_PANE` or `TERM_PROGRAM=wezterm`): rows are cleared before Kitty images are drawn.
    pub wezterm: bool,
    /// Uploaded Kitty images, least recently shown first.
    uploaded: Vec<(u32, Uploaded)>,
}

impl AltScreen {
    pub fn new() -> Self {
        Self::default()
    }

    fn kitty(&self) -> bool {
        self.image_protocol == Some(ImageProtocol::Kitty)
    }

    /// `deleteKittyImages`.
    fn delete_images(&self) -> &'static str {
        if self.kitty() { DELETE_ALL_KITTY_IMAGES } else { "" }
    }

    /// Switch to the alternate screen, without autowrap, cleared, cursor hidden.
    pub fn enter(&mut self) -> String {
        self.enter_with_mouse("")
    }

    /// [`AltScreen::enter`] that also turns on `mouse` tracking (`beforeTerminalStart` with `mouse: true`; see
    /// [`crate::fullscreen::mouse_tracking`]).
    pub fn enter_with_mouse(&mut self, mouse: &str) -> String {
        self.previous.clear();
        self.uploaded.clear();
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

    /// `prepareKittyScreen`: lines of images the terminal holds in the same transmission become placements; images
    /// off screen beyond the cache limits are deleted, least recently shown first.
    fn prepare_kitty(&mut self, screen: &[String]) -> (Vec<String>, String) {
        let mut visible: Vec<u32> = Vec::new();
        let lines = screen
            .iter()
            .map(|line| {
                let Some(placement) = kitty_image_placement(line) else {
                    return line.clone();
                };
                visible.push(placement.image_id);
                let cached = self
                    .uploaded
                    .iter()
                    .position(|(id, _)| *id == placement.image_id)
                    .map(|i| self.uploaded.remove(i).1);
                self.uploaded.push((
                    placement.image_id,
                    Uploaded {
                        generation: placement.transmission_generation,
                        transmission_bytes: placement.transmission_bytes,
                        decoded_bytes: placement.estimated_decoded_bytes,
                    },
                ));
                if cached.is_some_and(|c| c.generation == placement.transmission_generation) {
                    placement.replacement_line
                } else {
                    line.clone()
                }
            })
            .collect();
        let offscreen = |u: &[(u32, Uploaded)]| {
            u.iter()
                .filter(|(id, _)| !visible.contains(id))
                .fold((0usize, 0usize, 0u64), |(n, t, d), (_, c)| {
                    (n + 1, t + c.transmission_bytes, d + c.decoded_bytes)
                })
        };
        let (mut count, mut transmitted, mut decoded) = offscreen(&self.uploaded);
        let mut deletion = String::new();
        let mut i = 0;
        while i < self.uploaded.len() {
            if count <= MAX_CACHED_OFFSCREEN_IMAGES
                && transmitted <= MAX_CACHED_OFFSCREEN_TRANSMISSION_BYTES
                && decoded <= MAX_CACHED_OFFSCREEN_DECODED_BYTES
            {
                break;
            }
            let (id, cached) = self.uploaded[i];
            if visible.contains(&id) {
                i += 1;
                continue;
            }
            deletion.push_str(&delete_kitty_image(id));
            self.uploaded.remove(i);
            count -= 1;
            transmitted -= cached.transmission_bytes;
            decoded -= cached.decoded_bytes;
        }
        (lines, deletion)
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
            if is_image_line(&line) {
                screen.push(line);
                continue;
            }
            if visible_width(&line) > width {
                line = truncate_to_width(&line, width, "", false);
            }
            screen.push(format!("{line}{SEGMENT_RESET}"));
        }
        screen.resize(height, SEGMENT_RESET.to_string());
        let full = self.width != width || self.height != height || self.previous.is_empty();
        let images_changed = screen.iter().enumerate().any(|(row, line)| {
            let before = self.previous.get(row);
            before != Some(line) && (is_image_line(line) || before.is_some_and(|b| is_image_line(b)))
        });
        let redraw_images = full || images_changed;
        let had_uploaded = !self.uploaded.is_empty();
        let (prepared, evicted) = if redraw_images && self.kitty() {
            self.prepare_kitty(&screen)
        } else {
            (screen.clone(), String::new())
        };
        let mut out = String::from(BEGIN_SYNC);
        if full {
            self.full_redraws += 1;
            out.push_str(if self.kitty() && had_uploaded {
                DELETE_ALL_KITTY_PLACEMENTS
            } else {
                self.delete_images()
            });
            out.push_str("\x1b[2J");
        } else if images_changed {
            match self.image_protocol {
                Some(ImageProtocol::Iterm2) => out.push_str("\x1b[2J"),
                Some(ImageProtocol::Kitty) => out.push_str(DELETE_ALL_KITTY_PLACEMENTS),
                None => {}
            }
        }
        out.push_str(&evicted);
        let unchanged = |row: usize| !redraw_images && self.previous.get(row) == Some(&screen[row]);
        // WezTerm erases a Kitty image's cells when a later EL clears a row it covers.
        let clear_first = redraw_images && self.kitty() && self.wezterm && screen.iter().any(|l| is_image_line(l));
        if clear_first {
            for row in 0..height {
                if !unchanged(row) {
                    out.push_str(&format!("\x1b[{};1H\x1b[2K", row + 1));
                }
            }
        }
        for (row, line) in prepared.iter().enumerate().take(height) {
            if unchanged(row) {
                continue;
            }
            let clear = if clear_first { "" } else { "\x1b[2K" };
            out.push_str(&format!("\x1b[{};1H{clear}{line}", row + 1));
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
        // `beforeTerminalStop`: the alternate screen's Kitty images are deleted.
        let mut out = if self.kitty() {
            format!("{BEGIN_SYNC}{DELETE_ALL_KITTY_IMAGES}{END_SYNC}")
        } else {
            String::new()
        };
        self.uploaded.clear();
        out.push_str(&format!("{BEGIN_SYNC}{EXIT_ALT_SCREEN}{DISABLE_AUTOWRAP}"));
        for (row, line) in document.iter().enumerate() {
            let mut line = line.replace(CURSOR_MARKER, "");
            let image = is_image_line(&line);
            if !image && visible_width(&line) > width {
                line = truncate_to_width(&line, width, "", false);
            }
            if row > 0 {
                out.push_str("\r\n");
            }
            out.push_str(&format!("\r\x1b[2K{line}{}", if image { "" } else { SEGMENT_RESET }));
        }
        out.push_str(&format!("\x1b[0m{ENABLE_AUTOWRAP}\r\n\x1b[?25h{END_SYNC}"));
        self.previous.clear();
        out
    }
}
