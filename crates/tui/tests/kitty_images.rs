//! Inline Kitty images on the alternate screen, ported from packages/tui/test/tui-alt-screen.test.ts (the Kitty and
//! iTerm2 cases), layout.test.ts ("crops Kitty images at a scroll view's lower boundary") and terminal-image.test.ts
//! (Kitty cursor movement), with their literal expectations. A TypeScript scroll is a transcript showing other rows
//! here: the frames are what the TypeScript screen shows after each scroll. Each test registers its own image ids
//! (the registry is the process's, as TypeScript's module map is).

use tui::image::{
    Capabilities, DELETE_ALL_KITTY_IMAGES, DELETE_ALL_KITTY_PLACEMENTS, Dimensions, Image, ImageProtocol,
    KittyImageMetadata, RenderOptions, allocate_image_id, crop_kitty_image_line, encode_kitty, kitty_image_metadata,
    kitty_image_placement, register_kitty_image_metadata, render_image,
};
use tui::screen::{AltScreen, StackEntry, stack_lines};

fn kitty_line(id: u32, columns: u32, rows: u32, width_px: u32, height_px: u32) -> String {
    let line = encode_kitty("AAAA", Some(columns), Some(rows), Some(id), false);
    register_kitty_image_metadata(KittyImageMetadata {
        image_id: id,
        columns,
        rows,
        width_px,
        height_px,
    });
    line
}

fn rows(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn kitty_screen() -> AltScreen {
    let mut screen = AltScreen::new();
    screen.image_protocol = Some(ImageProtocol::Kitty);
    screen.enter();
    screen
}

const GROW: StackEntry = StackEntry {
    basis: Some(0),
    grow: 1,
    shrink: None,
    min_size: 0,
    max_size: None,
};

#[test]
fn tui_kitty_images_crop_an_image_whose_first_line_is_above_the_viewport() {
    let image = kitty_line(123, 2, 3, 100, 100);
    let content = rows(&["before", &image, "", "", "after", "end"]);
    let shown = stack_lines(vec![content], &[GROW], &[true], 3);
    assert!(
        shown[0].contains("i=123") && shown[0].contains("y=66,h=34,r=1"),
        "{shown:?}"
    );
    assert_eq!(&shown[1..], ["after", "end"]);
    // The cropped placement is what the screen writes.
    let mut screen = kitty_screen();
    let frame = screen.paint(&shown, 20, 3);
    assert!(frame.contains("i=123") && frame.contains("y=66,h=34,r=1"));
}

#[test]
fn tui_kitty_images_crop_at_a_scroll_views_lower_boundary() {
    let image = kitty_line(124, 2, 3, 100, 100);
    let transcript = rows(&["one", "two", &image, "", ""]);
    let dock = rows(&["dock"]);
    let frame = stack_lines(
        vec![transcript, dock],
        &[GROW, StackEntry::default()],
        &[false, false],
        4,
    );
    assert!(frame[2].contains("y=0,h=34,r=1"), "{frame:?}");
    // A whole image is left as it is.
    let whole = stack_lines(vec![rows(&[&image, "", ""])], &[GROW], &[false], 3);
    assert_eq!(whole[0], image);
}

#[test]
fn tui_kitty_images_retain_recently_offscreen_images_for_placement_only_reuse() {
    let image = kitty_line(321, 2, 1, 100, 50);
    let mut screen = kitty_screen();
    let first = screen.paint(&rows(&[&image]), 20, 1);
    assert!(first.contains("\x1b_Ga=T"));
    // Scrolled to "after", then back.
    let mut reentry = screen.paint(&rows(&["after"]), 20, 1);
    reentry.push_str(&screen.paint(&rows(&[&image]), 20, 1));
    assert!(reentry.contains("\x1b_Ga=p,q=2"), "{reentry:?}");
    assert!(!reentry.contains("\x1b_Ga=T"));
    assert!(!reentry.contains("\x1b_Ga=d,d=I,i=321,q=2\x1b\\"));
    // The placement keeps the image's own controls (id, cells, no cursor movement).
    assert!(reentry.contains("\x1b_Ga=p,q=2,C=1,c=2,r=1,i=321\x1b\\"), "{reentry:?}");
}

#[test]
fn tui_kitty_images_evict_the_least_recently_visible_image_when_the_cache_is_full() {
    let images: Vec<String> = (0..18).map(|i| kitty_line(500 + i, 2, 1, 100, 50)).collect();
    let mut screen = kitty_screen();
    let mut writes = String::new();
    for image in &images {
        writes.push_str(&screen.paint(std::slice::from_ref(image), 20, 1));
    }
    assert!(writes.contains("\x1b_Ga=d,d=I,i=500,q=2\x1b\\"));
    assert!(!writes.contains("\x1b_Ga=d,d=I,i=501,q=2\x1b\\"));
    let reentry = screen.paint(&images[..1], 20, 1);
    assert!(reentry.contains("\x1b_Ga=T"), "an evicted image is transmitted again");
}

#[test]
fn tui_kitty_images_evict_offscreen_images_past_the_decoded_memory_quota() {
    let images: Vec<String> = (0..4).map(|i| kitty_line(600 + i, 2, 1, 3840, 2160)).collect();
    let mut screen = kitty_screen();
    let mut writes = String::new();
    for image in &images {
        writes.push_str(&screen.paint(std::slice::from_ref(image), 20, 1));
    }
    assert!(writes.contains("\x1b_Ga=d,d=I,i=600,q=2\x1b\\"));
}

#[test]
fn tui_kitty_images_moved_images_are_placed_not_transmitted() {
    // "reuses moved Kitty images without dropping HStack siblings": the image moves down a row and a sibling row
    // changes; the frame clears placements, places the uploaded image and writes the changed text.
    let caps = Capabilities {
        images: Some(ImageProtocol::Kitty),
        true_color: true,
        hyperlinks: true,
    };
    let mut image = Image::new(&"A".repeat(8192), "image/png", Box::new(|s: &str| s.to_string()), caps);
    image.dimensions = Dimensions {
        width_px: 100,
        height_px: 100,
    };
    image.image_id = Some(777);
    let lines = tui::components::Component::render(&mut image, 10);
    let mut first = rows(&["header"]);
    first.extend(lines.iter().cloned());
    first.push("left".into());
    let mut screen = kitty_screen();
    assert!(screen.paint(&first, 20, 12).contains("\x1b_Ga=T"));
    let mut second = rows(&["header", "second"]);
    second.extend(lines.iter().cloned());
    second.push("changed".into());
    let redraw = screen.paint(&second, 20, 12);
    assert!(redraw.contains(DELETE_ALL_KITTY_PLACEMENTS));
    assert!(redraw.contains("\x1b_Ga=p,q=2"));
    assert!(!redraw.contains("\x1b_Ga=T"));
    assert!(
        redraw.len() < 2000,
        "expected placement-only redraw, got {} bytes",
        redraw.len()
    );
    assert!(redraw.contains("changed"));
}

#[test]
fn tui_kitty_images_full_redraws_and_exit_clear_images() {
    let image = kitty_line(901, 2, 1, 10, 10);
    let mut screen = kitty_screen();
    // A first frame with nothing uploaded deletes every image; a later full redraw only the placements.
    let first = screen.paint(&rows(&[&image, "x"]), 20, 2);
    assert!(
        first.contains(&format!("{DELETE_ALL_KITTY_IMAGES}\x1b[2J")),
        "{first:?}"
    );
    // Image rows are written whole, never cut to the width.
    // Written as they are: no reset appended (`applyLineResets` skips image lines).
    assert!(
        first.contains(&format!("\x1b[1;1H\x1b[2K{image}\x1b[2;1H")),
        "{first:?}"
    );
    let resized = screen.paint(&rows(&[&image, "x"]), 21, 2);
    assert!(
        resized.contains(&format!("{DELETE_ALL_KITTY_PLACEMENTS}\x1b[2J")),
        "{resized:?}"
    );
    // The uploaded image is placed again, not transmitted.
    assert!(resized.contains("\x1b_Ga=p,q=2,C=1,c=2,r=1,i=901\x1b\\") && !resized.contains("a=T"));
    // An unchanged frame writes no row.
    let same = screen.paint(&rows(&[&image, "x"]), 21, 2);
    assert!(!same.contains("\x1b[1;1H") && !same.contains("\x1b[2;1H"));
    let exit = screen.exit(&rows(&["done"]), 21);
    assert!(
        exit.starts_with(&format!("\x1b[?2026h{DELETE_ALL_KITTY_IMAGES}\x1b[?2026l")),
        "{exit:?}"
    );
    // Without images nothing is deleted.
    let mut plain = AltScreen::new();
    plain.enter();
    assert!(!plain.paint(&rows(&["a"]), 5, 1).contains("\x1b_G"));
    assert!(!plain.exit(&rows(&["a"]), 5).contains("\x1b_G"));
}

#[test]
fn tui_kitty_images_wezterm_clears_rows_before_drawing_images() {
    let image = kitty_line(902, 2, 1, 10, 10);
    let mut screen = kitty_screen();
    screen.wezterm = true;
    let frame = screen.paint(&rows(&[&image, "x"]), 20, 2);
    let clears = frame
        .find("\x1b[1;1H\x1b[2K\x1b[2;1H\x1b[2K")
        .expect("rows cleared first");
    let image_at = frame
        .find(&format!("\x1b[1;1H{image}"))
        .expect("image row without its own clear");
    assert!(clears < image_at);
}

#[test]
fn tui_kitty_images_clear_stale_iterm2_images_when_they_leave_the_viewport() {
    let image = "\x1b]1337;File=inline=1;width=2;height=auto:AAAA\x07";
    let mut screen = AltScreen::new();
    screen.image_protocol = Some(ImageProtocol::Iterm2);
    screen.enter();
    screen.paint(&rows(&[image, "", ""]), 20, 3);
    let scrolled = screen.paint(&rows(&["", "", "after"]), 20, 3);
    assert!(scrolled.contains("\x1b[2J"));
}

#[test]
fn tui_kitty_images_placement_and_crop_controls() {
    let line = kitty_line(903, 4, 4, 40, 80);
    assert_eq!(
        kitty_image_metadata(&line),
        Some(KittyImageMetadata {
            image_id: 903,
            columns: 4,
            rows: 4,
            width_px: 40,
            height_px: 80
        })
    );
    let placement = kitty_image_placement(&format!("pre{line}post")).unwrap();
    assert_eq!(placement.sequence, "\x1b_Ga=p,q=2,C=1,c=4,r=4,i=903\x1b\\");
    assert_eq!(placement.replacement_line, format!("pre{}post", placement.sequence));
    assert_eq!(placement.transmission_bytes, line.len());
    assert_eq!(placement.estimated_decoded_bytes, 40 * 80 * 4);
    // A chunked transmission is replaced whole.
    let chunked = encode_kitty(&"B".repeat(9000), Some(4), Some(4), Some(903), false);
    let placement = kitty_image_placement(&chunked).unwrap();
    assert_eq!(placement.transmission_bytes, chunked.len());
    assert_eq!(placement.replacement_line, placement.sequence);
    // Crops: rows 1..3 of 4 at 80 px.
    assert!(crop_kitty_image_line(&line, 1, 2).contains(",y=20,h=40,r=2;"));
    assert_eq!(crop_kitty_image_line(&line, 0, 4), line);
    assert_eq!(crop_kitty_image_line(&line, 4, 1), line);
    assert_eq!(crop_kitty_image_line(&line, 0, 0), line);
    // An unregistered id is no image to crop or place.
    let unknown = encode_kitty("AAAA", Some(2), Some(2), Some(904), false);
    assert_eq!(kitty_image_metadata(&unknown), None);
    assert_eq!(kitty_image_placement(&unknown), None);
    assert_eq!(crop_kitty_image_line(&unknown, 1, 1), unknown);
}

#[test]
fn tui_kitty_images_render_image_cursor_movement_and_registration() {
    let caps = Capabilities {
        images: Some(ImageProtocol::Kitty),
        true_color: true,
        hyperlinks: true,
    };
    let cell = Dimensions {
        width_px: 10,
        height_px: 10,
    };
    let image = Dimensions {
        width_px: 20,
        height_px: 20,
    };
    let options = |move_cursor, image_id| RenderOptions {
        max_width: Some(2),
        max_height: None,
        image_id,
        move_cursor,
    };
    let moving = render_image("AAAA", image, &caps, cell, options(true, None)).unwrap();
    assert!(!moving.sequence.contains(",C=1,"));
    assert_eq!(moving.rows, 2);
    let still = render_image("AAAA", image, &caps, cell, options(false, Some(905))).unwrap();
    assert!(still.sequence.contains(",C=1,"));
    assert_eq!(
        kitty_image_metadata(&still.sequence).map(|m| (m.rows, m.width_px)),
        Some((2, 20))
    );
    for _ in 0..100 {
        let id = allocate_image_id();
        assert!((1..=0xffff_fffe).contains(&id));
    }
}
