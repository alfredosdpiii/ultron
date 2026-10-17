//! Kitty images in the split viewport (scrollback plus a repainted live area), ported from the "TUI Kitty image
//! cleanup" cases of packages/tui/test/tui-render.test.ts (TypeScript's main screen, `TuiMainScreen`): reserved rows
//! are made and cleared before an image is placed on its first row, an image with a changed row is deleted by id
//! before it is drawn again, and full repaints delete the images they repaint. Their literal expectations are
//! written as the bytes this renderer emits for the same frames (it diffs per row and appends history above the
//! viewport where TypeScript rewrites a contiguous range of one document).

use tui::image::{
    KittyImageMetadata, crop_kitty_image_line, delete_kitty_image, encode_kitty, register_kitty_image_metadata,
};
use tui::viewport::{FramePlan, HistoryBatch, HistoryKind, ViewportRenderer};

const R: &str = "\x1b[0m\x1b]8;;\x07";
const BEGIN: &str = "\x1b[?2026h";
const END: &str = "\x1b[?25l\x1b[?2026l";

fn plan(viewport: &[&str]) -> FramePlan {
    FramePlan {
        history: None,
        viewport: viewport.iter().map(|s| s.to_string()).collect(),
    }
}

fn history(id: u64, kind: HistoryKind, rows: &[&str], viewport: &[&str]) -> FramePlan {
    FramePlan {
        history: Some(HistoryBatch {
            id,
            rows: rows.iter().map(|s| s.to_string()).collect(),
            kind,
        }),
        ..plan(viewport)
    }
}

/// `encodeKitty("AAAA", { columns, rows, imageId, moveCursor: false })`.
fn image(id: u32, columns: u32, rows: u32) -> String {
    encode_kitty("AAAA", Some(columns), Some(rows), Some(id), false)
}

#[test]
fn tui_split_kitty_reserved_rows_are_cleared_before_an_appended_image_is_placed() {
    let mut r = ViewportRenderer::new();
    r.render(&plan(&["before"]), 40, 10);
    let img = image(31, 2, 2);
    let f = r.render(&plan(&["before", &img, "", "after"]), 40, 10);
    // The image's two rows are made and cleared, then it is placed on the first; nothing clears after it.
    assert_eq!(
        f.bytes,
        format!("{BEGIN}\r\n\r\x1b[2K\r\n\r\x1b[2K\x1b[1A{img}\x1b[1B\r\n\r\x1b[2Kafter{R}{END}")
    );
    assert!(!f.bytes.contains(&format!("{img}\r\n\r\x1b[2K\x1b[1A")));
}

#[test]
fn tui_split_kitty_full_paint_reserves_image_rows_before_placing() {
    let mut r = ViewportRenderer::new();
    let img = image(32, 3, 3);
    let f = r.render(&plan(&["l0", &img, "", "", "after"]), 40, 5);
    assert_eq!(
        f.bytes,
        format!("{BEGIN}l0{R}\r\n\r\n\r\n\x1b[2A{img}\x1b[2B\r\nafter{R}{END}")
    );
    // Not padding rows written after the placement.
    assert!(!f.bytes.contains(&format!("{img}\r\n{R}")));
}

#[test]
fn tui_split_kitty_history_rows_reserve_image_rows_in_scrollback() {
    let mut r = ViewportRenderer::new();
    r.render(&plan(&["editor"]), 40, 10);
    let img = image(33, 2, 2);
    let f = r.render(&history(1, HistoryKind::Append, &["a", &img, ""], &["editor"]), 40, 10);
    assert_eq!(
        f.bytes,
        format!("{BEGIN}\r\x1b[Ja{R}\r\n\r\n\x1b[1A{img}\x1b[1B\r\neditor{R}{END}")
    );
    // A replay clears scrollback: the image written there is deleted before the clear.
    let f = r.render(&history(2, HistoryKind::Replay, &["a"], &["editor"]), 40, 10);
    assert_eq!(
        f.bytes,
        format!(
            "{BEGIN}{}\x1b[H\x1b[2J\x1b[3Ja{R}\r\neditor{R}{END}",
            delete_kitty_image(33)
        )
    );
    // Once deleted it is not deleted again.
    let f = r.render(&history(3, HistoryKind::Replay, &["a"], &["editor"]), 40, 10);
    assert!(!f.bytes.contains("\x1b_Ga=d"), "{:?}", f.bytes);
}

/// TS #4461: an image taller than the screen is placed on its first row, without the cursor-up placement. The live
/// viewport never is (it keeps the screen's last rows); scrollback can be.
#[test]
fn tui_split_kitty_image_taller_than_the_screen_is_placed_on_its_first_row() {
    let mut r = ViewportRenderer::new();
    let img = image(34, 6, 6);
    let rows: Vec<String> = std::iter::once(img.clone())
        .chain(std::iter::repeat_n(String::new(), 5))
        .collect();
    let f = r.render(
        &FramePlan {
            history: None,
            viewport: rows,
        },
        40,
        6,
    );
    assert_eq!(f.bytes, format!("{BEGIN}\r\n\r\n\r\n\r\n\r\n\x1b[5A{img}\x1b[5B{END}"));
    let mut r = ViewportRenderer::new();
    r.render(&plan(&["editor"]), 40, 6);
    let tall = image(35, 6, 7);
    let mut rows = vec![tall.as_str()];
    rows.extend(std::iter::repeat_n("", 6));
    let f = r.render(&history(1, HistoryKind::Append, &rows, &["editor"]), 40, 6);
    let blank = format!("{R}\r\n");
    assert_eq!(
        f.bytes,
        format!("{BEGIN}\r\x1b[J{tall}\r\n{}editor{R}{END}", blank.repeat(6))
    );
}

/// An image whose first row is above the screen shows its visible rows on the first row (`paintBox` cropping).
#[test]
fn tui_split_kitty_image_cut_by_the_top_of_the_screen_is_cropped_onto_the_first_row() {
    register_kitty_image_metadata(KittyImageMetadata {
        image_id: 36,
        columns: 4,
        rows: 4,
        width_px: 40,
        height_px: 40,
    });
    let img = image(36, 4, 4);
    let mut r = ViewportRenderer::new();
    let f = r.render(&plan(&["top", &img, "", "", "", "editor"]), 40, 4);
    // Its first of four rows is above the screen: the other three show from the first row.
    let cropped = crop_kitty_image_line(&img, 1, 3);
    assert!(cropped.contains(",y=10,h=30,r=3;"), "{cropped:?}");
    assert_eq!(
        f.bytes,
        format!("{BEGIN}\r\n\r\n\x1b[2A{cropped}\x1b[2B\r\neditor{R}{END}")
    );
}

#[test]
fn tui_split_kitty_changed_image_is_deleted_before_the_moved_placement_is_drawn() {
    let mut r = ViewportRenderer::new();
    let old = image(42, 2, 2);
    r.render(&plan(&["top", &old]), 40, 10);
    let new = image(42, 2, 1);
    let f = r.render(&plan(&[&new, ""]), 40, 10);
    let delete = f.bytes.find(&delete_kitty_image(42)).expect("the old image is deleted");
    let draw = f.bytes.find(&new).expect("the new image is drawn");
    assert!(delete < draw);
}

#[test]
fn tui_split_kitty_image_is_redrawn_when_one_of_its_reserved_rows_changes() {
    let mut r = ViewportRenderer::new();
    let img = image(88, 2, 2);
    r.render(&plan(&[&img, "", "status"]), 40, 10);
    let f = r.render(&plan(&[&img, "covered", "status"]), 40, 10);
    assert_eq!(
        f.bytes,
        format!(
            "{BEGIN}{}\x1b[2A\r\x1b[2K{img}\x1b[1B\r\x1b[2Kcovered{R}{END}",
            delete_kitty_image(88)
        )
    );
    assert!(!f.bytes.contains("\x1b[2J"));
}

/// Per-row diffing: a change below an image leaves the image alone (TypeScript rewrites from the first changed
/// row down and so redraws any image after it; here nothing above or below the changed row moves).
#[test]
fn tui_split_kitty_image_untouched_by_an_unrelated_row_change() {
    let mut r = ViewportRenderer::new();
    let img = image(89, 2, 2);
    r.render(&plan(&[&img, "", "status 1"]), 40, 10);
    let f = r.render(&plan(&[&img, "", "status 2"]), 40, 10);
    assert_eq!(f.bytes, format!("{BEGIN}\r\x1b[2Kstatus 2{R}{END}"));
}

#[test]
fn tui_split_kitty_full_repaint_deletes_the_images_it_repaints_first() {
    let mut r = ViewportRenderer::new();
    r.render(&plan(&[&image(77, 2, 2), ""]), 40, 10);
    r.invalidate();
    let f = r.render(&plan(&["plain text"]), 40, 10);
    assert_eq!(
        f.bytes,
        format!("{BEGIN}{}\x1b[1A\r\x1b[Jplain text{R}{END}", delete_kitty_image(77))
    );
    // A shrink past the image's rows erases them and deletes the image.
    let mut r = ViewportRenderer::new();
    r.render(&plan(&["a", &image(78, 2, 2), ""]), 40, 10);
    let f = r.render(&plan(&["a"]), 40, 10);
    assert_eq!(
        f.bytes,
        format!("{BEGIN}{}\x1b[1A\r\x1b[J{END}", delete_kitty_image(78))
    );
}
