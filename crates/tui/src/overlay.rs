//! Overlay layout and line compositing: ports of `resolveOverlayLayout` and `compositeTuiLine`
//! (packages/tui/src/tui.ts) and `extractSegments` (packages/tui/src/utils.ts). Focus and the overlay stack stay
//! with the owner: it passes visible overlays in focus order (lowest first).

use unicode_segmentation::UnicodeSegmentation;

use crate::renderer::is_image_line;
use crate::utils::{AnsiCodeTracker, ansi_len, grapheme_width, slice_with_width, visible_width};

const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

/// Absolute columns/rows or a percentage of the reference size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SizeValue {
    Abs(i64),
    Percent(f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Anchor {
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverlayOptions {
    pub width: Option<SizeValue>,
    pub min_width: Option<i64>,
    pub max_height: Option<SizeValue>,
    pub anchor: Anchor,
    pub offset_x: i64,
    pub offset_y: i64,
    pub row: Option<SizeValue>,
    pub col: Option<SizeValue>,
    /// top, right, bottom, left
    pub margin: [i64; 4],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayLayout {
    pub width: i64,
    pub row: i64,
    pub col: i64,
    pub max_height: Option<i64>,
}

fn parse_size(v: Option<SizeValue>, reference: i64) -> Option<i64> {
    v.map(|v| match v {
        SizeValue::Abs(n) => n,
        SizeValue::Percent(p) => ((reference as f64 * p) / 100.0).floor() as i64,
    })
}

fn anchor_row(anchor: Anchor, height: i64, avail: i64, top: i64) -> i64 {
    use Anchor::*;
    match anchor {
        TopLeft | TopCenter | TopRight => top,
        BottomLeft | BottomCenter | BottomRight => top + avail - height,
        LeftCenter | Center | RightCenter => top + (avail - height).div_euclid(2),
    }
}

fn anchor_col(anchor: Anchor, width: i64, avail: i64, left: i64) -> i64 {
    use Anchor::*;
    match anchor {
        TopLeft | LeftCenter | BottomLeft => left,
        TopRight | RightCenter | BottomRight => left + avail - width,
        TopCenter | Center | BottomCenter => left + (avail - width).div_euclid(2),
    }
}

/// Width, position and height cap of an overlay of `height` rows on a `term_w` x `term_h` terminal.
pub fn resolve_overlay_layout(opt: &OverlayOptions, height: i64, term_w: i64, term_h: i64) -> OverlayLayout {
    let [top, right, bottom, left] = opt.margin.map(|m| m.max(0));
    let avail_w = (term_w - left - right).max(1);
    let avail_h = (term_h - top - bottom).max(1);
    let mut width = parse_size(opt.width, term_w).unwrap_or(80.min(avail_w));
    if let Some(min) = opt.min_width {
        width = width.max(min);
    }
    let width = width.min(avail_w).max(1);
    let max_height = parse_size(opt.max_height, term_h).map(|m| m.min(avail_h).max(1));
    let eff_h = max_height.map_or(height, |m| height.min(m));
    let mut row = match opt.row {
        Some(SizeValue::Abs(r)) => r,
        Some(SizeValue::Percent(p)) => top + ((avail_h - eff_h).max(0) as f64 * (p / 100.0)).floor() as i64,
        None => anchor_row(opt.anchor, eff_h, avail_h, top),
    };
    let mut col = match opt.col {
        Some(SizeValue::Abs(c)) => c,
        Some(SizeValue::Percent(p)) => left + ((avail_w - width).max(0) as f64 * (p / 100.0)).floor() as i64,
        None => anchor_col(opt.anchor, width, avail_w, left),
    };
    row += opt.offset_y;
    col += opt.offset_x;
    row = row.min(term_h - bottom - eff_h).max(top);
    col = col.min(term_w - right - width).max(left);
    OverlayLayout {
        width,
        row,
        col,
        max_height,
    }
}

fn run_end(s: &str, mut i: usize) -> usize {
    while i < s.len() && ansi_len(s, i).is_none() {
        i += 1;
    }
    i
}

/// Content before column `before_end` and from `after_start` for `after_len` columns; the after part inherits the
/// styling active at its start.
pub fn extract_segments(
    line: &str,
    before_end: usize,
    after_start: usize,
    after_len: usize,
    strict_after: bool,
) -> (String, usize, String, usize) {
    let (mut before, mut before_w, mut after, mut after_w) = (String::new(), 0, String::new(), 0);
    let (mut col, mut i) = (0, 0);
    let mut pending = String::new();
    let mut after_started = false;
    let after_end = after_start + after_len;
    let mut tracker = AnsiCodeTracker::default();
    let done = |col: usize| {
        if after_len == 0 {
            col >= before_end
        } else {
            col >= after_end
        }
    };
    while i < line.len() {
        if let Some(len) = ansi_len(line, i) {
            let code = &line[i..i + len];
            tracker.process(code);
            if col < before_end {
                pending.push_str(code);
            } else if col >= after_start && col < after_end && after_started {
                after.push_str(code);
            }
            i += len;
            continue;
        }
        let end = run_end(line, i);
        for g in line[i..end].graphemes(true) {
            let w = grapheme_width(g);
            if col < before_end && col + w <= before_end {
                before.push_str(&std::mem::take(&mut pending));
                before.push_str(g);
                before_w += w;
            } else if col >= after_start && col < after_end && (!strict_after || col + w <= after_end) {
                if !after_started {
                    after.push_str(&tracker.active_codes());
                    after_started = true;
                }
                after.push_str(g);
                after_w += w;
            }
            col += w;
            if done(col) {
                break;
            }
        }
        i = end;
        if done(col) {
            break;
        }
    }
    (before, before_w, after, after_w)
}

/// `overlay_line` drawn over `base_line` at `start_col`, `overlay_width` columns wide, in a `total_width` line.
pub fn composite_line(
    base_line: &str,
    overlay_line: &str,
    start_col: usize,
    overlay_width: usize,
    total_width: usize,
) -> String {
    if is_image_line(base_line) {
        return base_line.to_string();
    }
    let after_start = start_col + overlay_width;
    let (before, before_w, after, after_w) = extract_segments(
        base_line,
        start_col,
        after_start,
        total_width.saturating_sub(after_start),
        true,
    );
    let (ov, ov_w) = slice_with_width(overlay_line, 0, overlay_width, true);
    let actual_before = start_col.max(before_w);
    let actual_ov = overlay_width.max(ov_w);
    let after_target = total_width.saturating_sub(actual_before + actual_ov);
    let result = format!(
        "{before}{}{SEGMENT_RESET}{ov}{}{SEGMENT_RESET}{after}{}",
        " ".repeat(start_col.saturating_sub(before_w)),
        " ".repeat(overlay_width.saturating_sub(ov_w)),
        " ".repeat(after_target.saturating_sub(after_w)),
    );
    if visible_width(&result) <= total_width {
        result
    } else {
        slice_with_width(&result, 0, total_width, true).0
    }
}
