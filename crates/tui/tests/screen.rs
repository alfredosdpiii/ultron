//! Ported from packages/tui/test/layout.test.ts ("viewport layout"): vertical stack allocation and a transcript
//! following its end, with the same literal expectations; plus the alternate-screen painter's row diffing.

use tui::screen::{AltScreen, StackEntry, allocate_stack_sizes, stack_lines};

fn lines(text: &str) -> Vec<String> {
    text.split('\n').map(str::to_string).collect()
}

#[test]
fn tui_screen_allocates_vertical_grow_space_deterministically() {
    let entries = [
        StackEntry {
            basis: Some(1),
            shrink: Some(0),
            ..StackEntry::default()
        },
        StackEntry {
            basis: Some(0),
            grow: 1,
            ..StackEntry::default()
        },
    ];
    assert_eq!(allocate_stack_sizes(&entries, &[1, 1], Some(4)), [1, 3]);
    assert_eq!(
        stack_lines(vec![lines("top"), lines("body")], &entries, &[], 4),
        ["top", "body", "", ""]
    );
}

#[test]
fn tui_screen_shrinks_entries_to_their_minimum_sizes() {
    let entries = [
        StackEntry {
            shrink: Some(1),
            min_size: 1,
            ..StackEntry::default()
        },
        StackEntry {
            shrink: Some(0),
            ..StackEntry::default()
        },
    ];
    assert_eq!(allocate_stack_sizes(&entries, &[3, 3], Some(4)), [1, 3]);
    assert_eq!(
        stack_lines(vec![lines("a1\na2\na3"), lines("b1\nb2\nb3")], &entries, &[], 4),
        ["a1", "b1", "b2", "b3"]
    );
}

#[test]
fn tui_screen_includes_nested_minimum_sizes_in_intrinsic_measurement() {
    let dock_entries = [
        StackEntry::default(),
        StackEntry {
            min_size: 3,
            ..StackEntry::default()
        },
        StackEntry::default(),
        StackEntry {
            min_size: 1,
            ..StackEntry::default()
        },
    ];
    let dock = vec![
        lines("top1\ntop2\ntop3"),
        lines("selector"),
        lines("below"),
        lines("footer"),
    ];
    let intrinsic: usize = allocate_stack_sizes(&dock_entries, &[3, 1, 1, 1], None).iter().sum();
    let root = [
        StackEntry {
            basis: Some(0),
            grow: 1,
            min_size: 1,
            ..StackEntry::default()
        },
        StackEntry {
            min_size: 1,
            ..StackEntry::default()
        },
    ];
    let sizes = allocate_stack_sizes(&root, &[1, intrinsic], Some(9));
    let dock = stack_lines(dock, &dock_entries, &[], sizes[1]);
    assert_eq!(
        stack_lines(vec![lines("body"), dock], &root, &[], 9),
        ["body", "top1", "top2", "top3", "selector", "", "", "below", "footer"]
    );
}

#[test]
fn tui_screen_follow_end_shows_the_last_rows() {
    // "paints only clipped rows from very large scroll content", at a size a test can hold.
    let mut content: Vec<String> = (0..1000).map(|i| format!("line {i}")).collect();
    content.extend(["visible 1".into(), "visible 2".into(), "visible 3".into()]);
    let entries = [StackEntry {
        basis: Some(0),
        grow: 1,
        ..StackEntry::default()
    }];
    assert_eq!(
        stack_lines(vec![content], &entries, &[true], 3),
        ["visible 1", "visible 2", "visible 3"]
    );
}

#[test]
fn tui_screen_alt_screen_rewrites_only_changed_rows() {
    let mut screen = AltScreen::new();
    assert!(screen.enter().starts_with("\x1b[?1049h"));
    let first = screen.paint(&lines("a\nb\nc"), 10, 3);
    assert!(first.contains("\x1b[2J") && first.contains("\x1b[1;1H") && first.contains("\x1b[3;1H"));
    let second = screen.paint(&lines("a\nB\nc"), 10, 3);
    assert!(!second.contains("\x1b[2J"));
    assert!(second.contains("\x1b[2;1H\x1b[2KB"));
    assert!(!second.contains("\x1b[1;1H") && !second.contains("\x1b[3;1H"));
    // A resize repaints everything.
    assert!(screen.paint(&lines("a\nB\nc"), 12, 3).contains("\x1b[2J"));
    let exit = screen.exit(&lines("x\ny"), 12);
    assert!(exit.contains("\x1b[?1049l") && exit.contains("x") && exit.ends_with("\x1b[?2026l"));
}
