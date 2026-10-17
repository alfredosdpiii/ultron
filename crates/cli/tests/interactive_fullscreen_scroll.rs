//! The fullscreen transcript in a real terminal (tmux, private socket; `ULTRON_TUI_PARITY=1` runs the alternate
//! screen like the TypeScript client TUI): Page Up and the wheel scroll the transcript back with the jump-to-end
//! label shown, End and a click on the label follow the end again, Ctrl+Up jumps to a message, and a mouse drag
//! selects text that copy-on-select puts on the clipboard (OSC 52 here, no display) with the "Copied!" flash.
//! The viewport's behavior is replayed against tui-alt-screen.ts in crates/tui/tests/fullscreen_scroll.rs.

mod tui_harness;

use tui_harness::{Mock, Tui, text_reply};

const MODEL: [&str; 2] = ["--model", "anthropic/claude-sonnet-4-5"];
const ENV: [(&str, &str); 3] = [
    ("ULTRON_AUTO_NAME", "off"),
    ("ULTRON_SPLASH", "off"),
    ("ULTRON_TUI_PARITY", "1"),
];

fn long_answer() -> String {
    (1..=40)
        .map(|i| format!("- item {i:02}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn start() -> (Mock, Tui) {
    let mock = Mock::start(vec![text_reply(&long_answer())], None);
    let tui = Tui::start(mock.port, 80, 24, &MODEL, &ENV);
    tui.wait("Session:");
    tui.send_text("show me the list");
    tui.send_keys(&["Enter"]);
    tui.wait("item 40");
    (mock, tui)
}

#[test]
fn cli_fullscreen_page_up_and_end_scroll_the_transcript() {
    let (_mock, tui) = start();
    tui.send_keys(&["PPage"]);
    let screen = tui.wait("Jump to latest message");
    assert!(!screen.contains("item 40"), "{screen}");
    assert!(screen.contains("item 2"), "{screen}");
    tui.send_keys(&["End"]);
    let screen = tui.wait("item 40");
    assert!(!screen.contains("Jump to latest message"), "{screen}");
}

#[test]
fn cli_fullscreen_wheel_scrolls_and_the_label_click_follows_the_end() {
    let (_mock, tui) = start();
    for _ in 0..5 {
        tui.send_text("\x1b[<64;10;5M");
    }
    let screen = tui.wait("Jump to latest message");
    assert!(!screen.contains("item 40"), "{screen}");
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("Jump to latest message"))
        .unwrap();
    let col = line.chars().position(|c| c == '↓').unwrap();
    tui.send_text(&format!("\x1b[<0;{};{}M", col + 1, row + 1));
    tui.send_text(&format!("\x1b[<0;{};{}m", col + 1, row + 1));
    let screen = tui.wait("item 40");
    assert!(!screen.contains("Jump to latest message"), "{screen}");
}

#[test]
fn cli_fullscreen_ctrl_up_jumps_to_the_previous_message() {
    let (_mock, tui) = start();
    tui.send_keys(&["C-Up"]);
    let screen = tui.wait("Jump to latest message");
    // The answer's zone starts at its top: its first item sits on the second row.
    assert!(screen.lines().nth(1).is_some_and(|l| l.contains("item 01")), "{screen}");
}

#[test]
fn cli_fullscreen_drag_selection_copies_with_a_flash() {
    let (_mock, tui) = start();
    tui.tmux(&["set-option", "-s", "set-clipboard", "on"]);
    let screen = tui.capture();
    let (row, line) = screen.lines().enumerate().find(|(_, l)| l.contains("item 39")).unwrap();
    let col = line[..line.find("item 39").unwrap()].chars().count();
    tui.send_text(&format!("\x1b[<0;{};{}M", col + 1, row + 1));
    tui.send_text(&format!("\x1b[<32;{};{}M", col + 7, row + 1));
    tui.send_text(&format!("\x1b[<0;{};{}m", col + 7, row + 1));
    tui.wait("Copied!");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while tui.tmux(&["show-buffer"]) != "item 39" {
        assert!(std::time::Instant::now() < deadline, "{:?}", tui.tmux(&["show-buffer"]));
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
