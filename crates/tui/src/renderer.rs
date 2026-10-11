//! Main-screen differential renderer (port of packages/tui/src/tui-main-screen.ts `doRender`): repaints only
//! the changed line range inside synchronized output and tracks the hardware cursor row.

use crate::utils::{normalize_terminal_output, visible_width};

/// Marks the hardware cursor position inside a rendered line (APC, zero width).
pub const CURSOR_MARKER: &str = "\x1b_pi:c\x07";
const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";

pub fn is_image_line(line: &str) -> bool {
    line.contains("\x1b_G") || line.contains("\x1b]1337;File=")
}

/// A rendered frame overflowed the terminal width.
#[derive(Debug, PartialEq, Eq)]
pub struct LineTooWide {
    pub line: usize,
    pub width: usize,
    pub terminal_width: usize,
}

/// Renderer state between frames. `render` returns the bytes to write to the terminal.
#[derive(Default)]
pub struct Renderer {
    previous_lines: Vec<String>,
    previous_width: usize,
    previous_height: usize,
    cursor_row: usize,
    hardware_cursor_row: i64,
    max_lines_rendered: usize,
    previous_viewport_top: i64,
    pub show_hardware_cursor: bool,
    pub clear_on_shrink: bool,
    pub full_redraws: usize,
}

fn mv(out: &mut String, diff: i64) {
    if diff > 0 {
        out.push_str(&format!("\x1b[{diff}B"));
    } else if diff < 0 {
        out.push_str(&format!("\x1b[{}A", -diff));
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lines of the last frame as written (after cursor extraction and line resets).
    pub fn previous_lines(&self) -> &[String] {
        &self.previous_lines
    }

    /// Paint one frame of `lines` on a `width` x `height` terminal.
    pub fn render(&mut self, mut lines: Vec<String>, width: usize, height: usize) -> Result<String, LineTooWide> {
        let mut out = String::new();
        let width_changed = self.previous_width != 0 && self.previous_width != width;
        let height_changed = self.previous_height != 0 && self.previous_height != height;
        let h = height as i64;
        let previous_buffer_len = if self.previous_height > 0 {
            self.previous_viewport_top + self.previous_height as i64
        } else {
            h
        };
        let mut prev_viewport_top = if height_changed {
            (previous_buffer_len - h).max(0)
        } else {
            self.previous_viewport_top
        };
        let mut viewport_top = prev_viewport_top;
        let mut hardware_cursor_row = self.hardware_cursor_row;

        let cursor = extract_cursor_position(&mut lines, height);
        for line in &mut lines {
            if !is_image_line(line) {
                *line = normalize_terminal_output(line) + SEGMENT_RESET;
            }
        }

        if (self.previous_lines.is_empty() && !width_changed && !height_changed)
            || width_changed
            || height_changed
            || (self.clear_on_shrink && lines.len() < self.max_lines_rendered)
        {
            let clear = !(self.previous_lines.is_empty() && !width_changed && !height_changed);
            self.full_render(&mut out, lines, clear, cursor, width, height);
            return Ok(out);
        }

        let prev_len = self.previous_lines.len();
        let mut first: Option<usize> = None;
        let mut last = 0;
        for i in 0..lines.len().max(prev_len) {
            let old = self.previous_lines.get(i).map_or("", String::as_str);
            let new = lines.get(i).map_or("", String::as_str);
            if old != new {
                first.get_or_insert(i);
                last = i;
            }
        }
        let appended = lines.len() > prev_len;
        if appended {
            first.get_or_insert(prev_len);
            last = lines.len() - 1;
        }
        let Some(first) = first else {
            self.position_cursor(&mut out, cursor, lines.len());
            self.previous_viewport_top = prev_viewport_top;
            self.previous_height = height;
            return Ok(out);
        };
        let append_start = appended && first == prev_len && first > 0;

        if first >= lines.len() {
            if prev_len > lines.len() {
                let mut buf = String::from("\x1b[?2026h");
                let target_row = lines.len().saturating_sub(1) as i64;
                if target_row < prev_viewport_top {
                    self.full_render(&mut out, lines, true, cursor, width, height);
                    return Ok(out);
                }
                mv(
                    &mut buf,
                    (target_row - viewport_top) - (hardware_cursor_row - prev_viewport_top),
                );
                buf.push('\r');
                let extra = prev_len - lines.len();
                if extra > height {
                    self.full_render(&mut out, lines, true, cursor, width, height);
                    return Ok(out);
                }
                let clear_start = usize::from(!lines.is_empty());
                if extra > 0 && clear_start > 0 {
                    buf.push_str(&format!("\x1b[{clear_start}B"));
                }
                for i in 0..extra {
                    buf.push_str("\r\x1b[2K");
                    if i + 1 < extra {
                        buf.push_str("\x1b[1B");
                    }
                }
                let move_back = (extra + clear_start).saturating_sub(1);
                if move_back > 0 {
                    buf.push_str(&format!("\x1b[{move_back}A"));
                }
                buf.push_str("\x1b[?2026l");
                out.push_str(&buf);
                self.cursor_row = target_row as usize;
                self.hardware_cursor_row = target_row;
            }
            self.position_cursor(&mut out, cursor, lines.len());
            self.previous_lines = lines;
            self.previous_width = width;
            self.previous_height = height;
            self.previous_viewport_top = prev_viewport_top;
            return Ok(out);
        }

        if (first as i64) < prev_viewport_top {
            self.full_render(&mut out, lines, true, cursor, width, height);
            return Ok(out);
        }

        let mut buf = String::from("\x1b[?2026h");
        let prev_viewport_bottom = prev_viewport_top + h - 1;
        let move_target = if append_start { first - 1 } else { first } as i64;
        if move_target > prev_viewport_bottom {
            let current_screen_row = (hardware_cursor_row - prev_viewport_top).clamp(0, h - 1);
            let to_bottom = h - 1 - current_screen_row;
            if to_bottom > 0 {
                buf.push_str(&format!("\x1b[{to_bottom}B"));
            }
            let scroll = move_target - prev_viewport_bottom;
            buf.push_str(&"\r\n".repeat(scroll as usize));
            prev_viewport_top += scroll;
            viewport_top += scroll;
            hardware_cursor_row = move_target;
        }
        mv(
            &mut buf,
            (move_target - viewport_top) - (hardware_cursor_row - prev_viewport_top),
        );
        buf.push_str(if append_start { "\r\n" } else { "\r" });

        let render_end = last.min(lines.len() - 1);
        for (i, line) in lines.iter().enumerate().take(render_end + 1).skip(first) {
            if i > first {
                buf.push_str("\r\n");
            }
            buf.push_str("\x1b[2K");
            if !is_image_line(line) && visible_width(line) > width {
                return Err(LineTooWide {
                    line: i,
                    width: visible_width(line),
                    terminal_width: width,
                });
            }
            buf.push_str(line);
        }
        let mut final_row = render_end;
        if prev_len > lines.len() {
            if render_end < lines.len() - 1 {
                buf.push_str(&format!("\x1b[{}B", lines.len() - 1 - render_end));
                final_row = lines.len() - 1;
            }
            let extra = prev_len - lines.len();
            buf.push_str(&"\r\n\x1b[2K".repeat(extra));
            buf.push_str(&format!("\x1b[{extra}A"));
        }
        buf.push_str("\x1b[?2026l");
        out.push_str(&buf);

        self.cursor_row = lines.len().saturating_sub(1);
        self.hardware_cursor_row = final_row as i64;
        self.max_lines_rendered = self.max_lines_rendered.max(lines.len());
        self.previous_viewport_top = prev_viewport_top.max(final_row as i64 - h + 1);
        self.position_cursor(&mut out, cursor, lines.len());
        self.previous_lines = lines;
        self.previous_width = width;
        self.previous_height = height;
        Ok(out)
    }

    fn full_render(
        &mut self,
        out: &mut String,
        lines: Vec<String>,
        clear: bool,
        cursor: Option<(usize, usize)>,
        width: usize,
        height: usize,
    ) {
        self.full_redraws += 1;
        out.push_str("\x1b[?2026h");
        if clear {
            out.push_str("\x1b[2J\x1b[H\x1b[3J");
        }
        out.push_str(&lines.join("\r\n"));
        out.push_str("\x1b[?2026l");
        self.cursor_row = lines.len().saturating_sub(1);
        self.hardware_cursor_row = self.cursor_row as i64;
        self.max_lines_rendered = if clear {
            lines.len()
        } else {
            self.max_lines_rendered.max(lines.len())
        };
        self.previous_viewport_top = (height.max(lines.len()) - height) as i64;
        self.position_cursor(out, cursor, lines.len());
        self.previous_lines = lines;
        self.previous_width = width;
        self.previous_height = height;
    }

    fn position_cursor(&mut self, out: &mut String, cursor: Option<(usize, usize)>, total: usize) {
        let Some((row, col)) = cursor.filter(|_| total > 0) else {
            out.push_str(HIDE_CURSOR);
            return;
        };
        let target = row.min(total - 1) as i64;
        mv(out, target - self.hardware_cursor_row);
        out.push_str(&format!("\x1b[{}G", col + 1));
        self.hardware_cursor_row = target;
        out.push_str(if self.show_hardware_cursor {
            SHOW_CURSOR
        } else {
            HIDE_CURSOR
        });
    }
}

/// Find, strip and locate the last `CURSOR_MARKER` within the bottom `height` lines.
fn extract_cursor_position(lines: &mut [String], height: usize) -> Option<(usize, usize)> {
    let top = lines.len().saturating_sub(height);
    for row in (top..lines.len()).rev() {
        if let Some(idx) = lines[row].find(CURSOR_MARKER) {
            let col = visible_width(&lines[row][..idx]);
            lines[row].replace_range(idx..idx + CURSOR_MARKER.len(), "");
            return Some((row, col));
        }
    }
    None
}
