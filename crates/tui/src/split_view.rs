//! Two columns side by side with a vertical border (`SplitView` of packages/tui components/split-view.ts): the main
//! column and a side pane on the right. [`split_widths`] sizes the columns; [`render_split`] merges them line by
//! line as `SplitView.render` does (`compositeTuiLine` into a blank line).

use crate::overlay::composite_line;

/// `widths`: `(main, side)` for a total width; the side is 0 while the pane is closed. `side_width` is the pane's
/// wanted width (without the border).
pub fn split_widths(width: usize, open: bool, side_width: usize) -> (usize, usize) {
    let total = width.max(1);
    if !open {
        return (total, 0);
    }
    let side = side_width.min(total.saturating_sub(2)).max(1);
    (total.saturating_sub(side + 1), side)
}

/// `render`: `main` (at most `main_width` columns) and `side` (at most `side_width`) beside each other with `border`
/// (one column) between, as many rows as the taller column. A side width of 0 returns `main` unchanged.
pub fn render_split(
    main: &[String],
    side: &[String],
    main_width: usize,
    side_width: usize,
    border: &str,
) -> Vec<String> {
    if side_width == 0 {
        return main.to_vec();
    }
    let total = main_width + 1 + side_width;
    let height = main.len().max(side.len());
    (0..height)
        .map(|row| {
            let line = composite_line("", main.get(row).map_or("", String::as_str), 0, main_width, total);
            let line = composite_line(&line, border, main_width, 1, total);
            composite_line(
                &line,
                side.get(row).map_or("", String::as_str),
                main_width + 1,
                side_width,
                total,
            )
        })
        .collect()
}
