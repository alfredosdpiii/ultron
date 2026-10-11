//! `/copy` and Esc-Esc rewind over the real transcript (behavior of omp copy-selector.ts / rewind-selector.ts):
//! the row -> entry map, the outlined turn, stepping into blocks with keyboard and mouse controls, user-turn jumps
//! and the word filter, all through keybinding ids.

use std::rc::Rc;

use interactive::chat::ChatStyle;
use interactive::copy_targets::TranscriptEntry;
use interactive::transcript_picker::{PickerEvent, PickerMode, PickerOptions, TranscriptPicker};
use serde_json::json;
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};
use tui::utils::{strip_terminal_sequences, visible_width};

const W: usize = 72;
const H: usize = 60;

fn keys(user: Vec<(String, Option<Vec<String>>)>) -> KeybindingsManager {
    KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        user,
        Default::default(),
    )
}

fn entries() -> Vec<TranscriptEntry> {
    vec![
        TranscriptEntry::new("u1", "t", json!({"role": "user", "content": "Show me the build"})),
        TranscriptEntry::new(
            "a1",
            "t",
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "Run this:\n```sh\ncargo build\n```\nDocs at https://doc.rust-lang.org/cargo"},
                {"type": "toolCall", "id": "c1", "name": "rlm", "arguments": {"code": "print(1)"}}
            ], "stopReason": "toolUse"}),
        ),
        TranscriptEntry::new(
            "r1",
            "t",
            json!({"role": "toolResult", "toolName": "rlm", "toolCallId": "c1", "content": [{"type": "text", "text": "1"}]}),
        ),
        TranscriptEntry::new("u2", "t", json!({"role": "user", "content": "and the tests?"})),
        TranscriptEntry::new(
            "a2",
            "t",
            json!({"role": "assistant", "content": [{"type": "text", "text": "Use cargo test."}]}),
        ),
    ]
}

fn picker_with(mode: PickerMode, keys: KeybindingsManager) -> TranscriptPicker {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let chat = ChatStyle {
        theme: Rc::clone(&theme),
        highlight: Rc::new(|code: &str| code.lines().map(str::to_string).collect()),
        expand_key: "ctrl+o".into(),
        dequeue_key: "Alt+Up".into(),
        interrupt_key: "escape".into(),
        code_highlight: None,
    };
    TranscriptPicker::new(
        entries(),
        PickerOptions {
            mode,
            theme,
            keys,
            chat,
            title: None,
            action_label: None,
        },
    )
}

fn picker(mode: PickerMode) -> TranscriptPicker {
    picker_with(mode, keys(Vec::new()))
}

fn plain(lines: &[String]) -> Vec<String> {
    lines.iter().map(|l| strip_terminal_sequences(l)).collect()
}

const UP: &str = "\x1b[A";
const DOWN: &str = "\x1b[B";
const LEFT: &str = "\x1b[D";
const RIGHT: &str = "\x1b[C";
const ESC: &str = "\x1b";

#[test]
fn interactive_picker_maps_rendered_items_to_turns() {
    let mut p = picker(PickerMode::Copy);
    let t = p.targets();
    let ids: Vec<(&str, &str, bool)> = t
        .iter()
        .map(|t| (t.turn_id.as_str(), t.entry_id.as_str(), t.is_user_turn))
        .collect();
    assert_eq!(
        ids,
        [
            ("u1", "u1", true),
            ("a1", "r1", false),
            ("u2", "u2", true),
            ("a2", "a2", false)
        ]
    );
    assert_eq!(t[1].entries, vec![1, 2], "the rlm result folds into the call's turn");
    assert_eq!(p.selected(), 3, "opens on the newest turn");
    let screen = plain(&p.render(W, H));
    assert_eq!(screen.len(), H);
    assert!(
        screen[0].starts_with("⧉ Copy · pick what to put on the clipboard"),
        "{:?}",
        screen[0]
    );
    let top = screen.iter().position(|l| l.trim_start().starts_with('╭')).unwrap();
    assert!(
        screen[top + 1].contains("Use cargo test."),
        "the outline wraps the selected turn"
    );
    assert!(
        screen.last().unwrap().starts_with("4/4  ↑/↓ step"),
        "{:?}",
        screen.last()
    );
}

#[test]
fn interactive_picker_copies_the_outlined_turn() {
    let mut p = picker(PickerMode::Copy);
    p.render(W, H);
    assert_eq!(p.handle_input(UP), PickerEvent::Render);
    assert_eq!(p.handle_input(UP), PickerEvent::Render);
    let screen = plain(&p.render(W, H));
    let rule = screen.iter().find(|l| l.contains('╭')).unwrap();
    assert!(rule.contains(" 4 blocks → "), "caption advertises the blocks: {rule}");
    assert_eq!(
        p.handle_input("\r"),
        PickerEvent::Copy {
            content: "Run this:\n```sh\ncargo build\n```\nDocs at https://doc.rust-lang.org/cargo".into(),
            label: "assistant message".into(),
            entry_id: "a1".into(),
            block: None,
        }
    );
    assert_eq!(p.handle_input(UP), PickerEvent::Render);
    assert_eq!(
        p.handle_input(UP),
        PickerEvent::Ignored,
        "the first turn stops the outline"
    );
    assert_eq!(p.selected(), 0);
}

#[test]
fn interactive_picker_steps_into_blocks_with_copy_and_open_controls() {
    let mut p = picker(PickerMode::Copy);
    p.render(W, H);
    p.handle_input(UP);
    p.handle_input(UP);
    assert_eq!(p.handle_input(RIGHT), PickerEvent::Render);
    let (blocks, selected) = p.blocks().unwrap();
    let labels: Vec<&str> = blocks.iter().map(|b| b.label.as_str()).collect();
    assert_eq!(labels, ["sh code", "link", "rlm code", "rlm result"]);
    assert_eq!(selected, 0);
    let screen = plain(&p.render(W, H));
    assert!(
        screen.iter().any(|l| l.contains("1/4 · sh code · 1 line  ⧉ copy")),
        "{screen:#?}"
    );
    assert!(screen.iter().any(|l| l.contains("2/4 · link · 1 line  ⧉ copy  ↗ open")));
    assert!(screen.last().unwrap().starts_with("1/4  ↑/↓ block"));
    assert_eq!(
        p.handle_input("\r"),
        PickerEvent::Copy {
            content: "cargo build".into(),
            label: "sh code".into(),
            entry_id: "a1".into(),
            block: Some(0),
        }
    );
    assert_eq!(p.handle_input("o"), PickerEvent::Ignored, "no link on a code block");
    p.handle_input(DOWN);
    assert_eq!(
        p.handle_input("o"),
        PickerEvent::Open {
            href: "https://doc.rust-lang.org/cargo".into(),
            label: "link".into(),
        }
    );
    // Click the copy control of block 4 (the folded tool result).
    let screen = plain(&p.render(W, H));
    let row = screen.iter().position(|l| l.contains("4/4 · rlm result")).unwrap();
    let line = &screen[row];
    let col = visible_width(&line[..line.find('⧉').unwrap()]);
    let click = format!("\x1b[<0;{};{}M", col + 1, row + 1);
    assert_eq!(
        p.handle_input(&click),
        PickerEvent::Copy {
            content: "1".into(),
            label: "rlm result".into(),
            entry_id: "r1".into(),
            block: Some(3),
        }
    );
    // A click beside the control does nothing.
    assert_eq!(
        p.handle_input(&format!("\x1b[<0;{};{}M", 1, row + 1)),
        PickerEvent::Ignored
    );
    // Left and Escape leave the blocks; Escape again closes.
    assert_eq!(p.handle_input(LEFT), PickerEvent::Render);
    assert!(p.blocks().is_none());
    p.handle_input(RIGHT);
    assert_eq!(p.handle_input(ESC), PickerEvent::Render);
    assert_eq!(p.handle_input(ESC), PickerEvent::Cancel);
}

#[test]
fn interactive_picker_keys_follow_user_bindings() {
    let mut p = picker_with(
        PickerMode::Copy,
        keys(vec![("app.copy.blocks".into(), Some(vec!["tab".into()]))]),
    );
    p.render(W, H);
    p.handle_input(UP);
    p.handle_input(UP);
    assert_eq!(p.handle_input(RIGHT), PickerEvent::Ignored);
    assert_eq!(p.handle_input("\t"), PickerEvent::Render);
    assert!(p.blocks().is_some());
    let screen = plain(&p.render(W, H));
    assert!(screen.iter().any(|l| l.contains("1/4 · sh code")));
}

#[test]
fn interactive_picker_wheel_scrolls_and_selection_stays_revealed() {
    let mut p = picker(PickerMode::Copy);
    let first = plain(&p.render(W, 8));
    assert!(
        first.iter().any(|l| l.contains("Use cargo test.")),
        "the outlined newest turn is in view"
    );
    assert_eq!(p.handle_input("\x1b[<64;5;3M"), PickerEvent::Render, "wheel up");
    let scrolled = plain(&p.render(W, 8));
    assert_ne!(first, scrolled);
    // An unchanged outline keeps the manual scroll; moving it reveals it again.
    assert_eq!(plain(&p.render(W, 8)), scrolled);
    p.handle_input(UP);
    p.render(W, 8);
    p.handle_input(DOWN);
    let back = plain(&p.render(W, 8));
    assert!(back.iter().any(|l| l.contains("Use cargo test.")));
}

#[test]
fn interactive_rewind_jumps_user_turns_and_rewinds_to_the_outlined_entry() {
    let mut p = picker(PickerMode::Rewind);
    let screen = plain(&p.render(W, H));
    assert!(screen[0].starts_with("↶ Rewind · pick the point to continue from"));
    assert!(screen.last().unwrap().contains("←/→ user turns  f filter"));
    assert_eq!(p.handle_input(LEFT), PickerEvent::Render);
    assert_eq!(p.selected(), 2);
    p.handle_input(LEFT);
    assert_eq!(p.selected(), 0);
    assert_eq!(p.handle_input(LEFT), PickerEvent::Ignored);
    p.handle_input(RIGHT);
    assert_eq!(p.selected(), 2);
    p.handle_input(UP);
    assert_eq!(p.selected(), 1);
    assert_eq!(
        p.handle_input("\r"),
        PickerEvent::Rewind { entry_id: "r1".into() },
        "a turn with folded tool output rewinds past its results"
    );
    assert_eq!(p.handle_input(ESC), PickerEvent::Cancel);
}

#[test]
fn interactive_rewind_filter_matches_whole_words_of_rendered_rows() {
    let mut p = picker(PickerMode::Rewind);
    p.render(W, H);
    assert_eq!(p.handle_input("f"), PickerEvent::Render);
    for ch in "tests".chars() {
        p.handle_input(&ch.to_string());
    }
    assert_eq!(p.filter_matches(), vec![2]);
    assert_eq!(p.selected(), 2, "the selection rests on the newest match above it");
    let screen = plain(&p.render(W, H));
    assert!(screen.last().unwrap().contains("1/1"), "{:?}", screen.last());
    assert!(
        !screen.iter().any(|l| l.contains("Use cargo test.")),
        "non-matching turns are hidden"
    );
    // `test` is a whole word: it does not match `tests`, nor `cargo test.`'s neighbor turns twice.
    for _ in 0.."tests".len() {
        p.handle_input("\x7f");
    }
    for ch in "test".chars() {
        p.handle_input(&ch.to_string());
    }
    assert_eq!(p.filter_matches(), vec![3]);
    for _ in 0.."test".len() {
        p.handle_input("\x7f");
    }
    for ch in "cargo".chars() {
        p.handle_input(&ch.to_string());
    }
    assert_eq!(p.filter_matches(), vec![1, 3]);
    p.render(W, H);
    assert_eq!(p.handle_input(UP), PickerEvent::Render);
    assert_eq!(p.selected(), 1);
    assert_eq!(p.handle_input(UP), PickerEvent::Ignored, "steps stay within matches");
    assert_eq!(p.handle_input("\r"), PickerEvent::Rewind { entry_id: "r1".into() });
    // Esc leaves the filter keeping the selection; Backspace on an empty filter does too.
    assert_eq!(p.handle_input(ESC), PickerEvent::Render);
    assert_eq!(p.filter_query(), None);
    assert_eq!(p.selected(), 1);
    p.handle_input("f");
    assert_eq!(p.handle_input("\x7f"), PickerEvent::Render);
    assert_eq!(p.filter_query(), None);
}
