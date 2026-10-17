//! Kitty keyboard protocol negotiation, ported from packages/tui/test/terminal.test.ts ("ProcessTerminal Kitty
//! keyboard protocol negotiation") with its literal bytes, through `tui::keyboard_protocol` and `tui::terminal`.
//! Time is passed in, so the fragment timeout is tested without waiting.

use std::time::{Duration, Instant};

use tui::keyboard_protocol::{KeyboardProtocol, NegotiationSequence, parse_negotiation_sequence};
use tui::terminal::Terminal;

/// The harness: a protocol after `queryAndEnableKittyProtocol`, its writes and the forwarded input.
struct Harness {
    protocol: KeyboardProtocol,
    writes: Vec<String>,
    input: Vec<String>,
    kitty: bool,
    now: Instant,
}

impl Harness {
    fn new() -> Self {
        let mut protocol = KeyboardProtocol::new();
        let query = protocol.query().to_string();
        Harness {
            protocol,
            writes: vec![query],
            input: Vec::new(),
            kitty: false,
            now: Instant::now(),
        }
    }

    fn send(&mut self, data: &str) {
        let n = self.protocol.input(data, self.now);
        if !n.write.is_empty() {
            self.writes.push(n.write);
        }
        self.kitty |= n.kitty_activated;
        self.input.extend(n.forward);
    }

    fn tick(&mut self, ms: u64) {
        self.now += Duration::from_millis(ms);
        if let Some(fragment) = self.protocol.poll(self.now) {
            self.input.push(fragment);
        }
    }

    fn cleanup(&mut self) {
        let bytes = self.protocol.stop();
        // `stop()` writes each sequence on its own.
        for part in ["\x1b[<u", "\x1b[>4;0m"] {
            if bytes.contains(part) {
                self.writes.push(part.into());
            }
        }
    }

    fn count(&self, write: &str) -> usize {
        self.writes.iter().filter(|w| *w == write).count()
    }
}

#[test]
fn tui_keyboard_protocol_queries_kitty_mode_before_enabling_modify_other_keys() {
    let mut h = Harness::new();
    assert_eq!(h.writes[0], "\x1b[>7u\x1b[?u\x1b[c");
    assert_eq!(h.count("\x1b[>4;2m"), 0);
    assert!(!h.protocol.kitty_active());
    h.cleanup();
}

#[test]
fn tui_keyboard_protocol_activates_kitty_for_non_zero_flags() {
    let mut h = Harness::new();
    h.send("\x1b[?7u");
    assert!(h.input.is_empty());
    assert!(h.protocol.kitty_active() && h.kitty);
    assert_eq!(h.count("\x1b[>4;2m"), 0);
    assert_eq!(h.count("\x1b[>4;0m"), 0);
    // A second confirmation does not activate again.
    h.kitty = false;
    h.send("\x1b[?7u");
    assert!(!h.kitty);
    h.cleanup();
    assert_eq!(h.count("\x1b[<u"), 1);
    assert_eq!(h.count("\x1b[>4;0m"), 0);
    // Stopping again writes nothing more.
    assert_eq!(h.protocol.stop(), "");
}

#[test]
fn tui_keyboard_protocol_falls_back_to_modify_other_keys_for_zero_flags() {
    let mut h = Harness::new();
    h.send("\x1b[?0u");
    assert!(h.input.is_empty());
    assert!(!h.protocol.kitty_active());
    assert_eq!(h.count("\x1b[>4;2m"), 1);
    // A DA after the zero reply does not enable it twice.
    h.send("\x1b[?62;c");
    assert_eq!(h.count("\x1b[>4;2m"), 1);
    h.cleanup();
    assert_eq!(h.count("\x1b[>4;0m"), 1);
}

#[test]
fn tui_keyboard_protocol_falls_back_for_device_attributes_without_kitty_flags() {
    let mut h = Harness::new();
    h.send("\x1b[?62;4;52c");
    assert!(h.input.is_empty());
    assert!(!h.protocol.kitty_active());
    assert_eq!(h.count("\x1b[>4;2m"), 1);
    // Kitty confirmed late turns modifyOtherKeys off again.
    h.send("\x1b[?1u");
    assert!(h.protocol.kitty_active());
    assert_eq!(h.count("\x1b[>4;0m"), 1);
    assert!(!h.protocol.modify_other_keys_active());
    h.cleanup();
}

#[test]
fn tui_keyboard_protocol_device_attributes_after_kitty_keep_kitty() {
    let mut h = Harness::new();
    h.send("\x1b[?7u");
    h.send("\x1b[?62;4c");
    assert!(h.protocol.kitty_active());
    assert_eq!(h.count("\x1b[>4;2m"), 0);
    h.cleanup();
}

#[test]
fn tui_keyboard_protocol_forwards_normal_input_while_waiting() {
    let mut h = Harness::new();
    h.send("a");
    assert_eq!(h.input, ["a"]);
    assert!(!h.protocol.kitty_active());
    h.cleanup();
}

#[test]
fn tui_keyboard_protocol_tracks_split_kitty_confirmation() {
    let mut h = Harness::new();
    h.send("\x1b[?7");
    h.tick(10);
    assert!(h.input.is_empty());
    h.send("u");
    assert!(h.protocol.kitty_active());
    assert_eq!(h.count("\x1b[>4;2m"), 0);
    assert_eq!(h.protocol.flush_deadline(), None);
    h.cleanup();
}

#[test]
fn tui_keyboard_protocol_replays_a_buffered_csi_prefix_that_is_not_a_reply() {
    let mut h = Harness::new();
    h.send("\x1b[");
    h.tick(50);
    assert!(h.input.is_empty());
    h.tick(100);
    assert_eq!(h.input, ["\x1b["]);
    assert_eq!(h.protocol.flush_deadline(), None);
    // A held fragment followed by a key: the fragment is input, then the key.
    let mut h = Harness::new();
    h.send("\x1b[?");
    h.send("x");
    assert_eq!(h.input, ["\x1b[?", "x"]);
    // Each fragment restarts the 150 ms.
    let mut h = Harness::new();
    h.send("\x1b[");
    h.tick(100);
    h.send("?");
    h.tick(100);
    assert!(h.input.is_empty());
    h.tick(50);
    assert_eq!(h.input, ["\x1b[?"]);
}

#[test]
fn tui_keyboard_protocol_parses_replies_like_ts() {
    assert_eq!(
        parse_negotiation_sequence("\x1b[?7u"),
        Some(NegotiationSequence::KittyFlags(7))
    );
    assert_eq!(
        parse_negotiation_sequence("\x1b[?0u"),
        Some(NegotiationSequence::KittyFlags(0))
    );
    assert_eq!(
        parse_negotiation_sequence("\x1b[?31u"),
        Some(NegotiationSequence::KittyFlags(31))
    );
    assert_eq!(parse_negotiation_sequence("\x1b[?u"), None);
    assert_eq!(parse_negotiation_sequence("\x1b[?1;2u"), None);
    assert_eq!(parse_negotiation_sequence("\x1b[7u"), None);
    assert_eq!(
        parse_negotiation_sequence("\x1b[?62;22c"),
        Some(NegotiationSequence::DeviceAttributes)
    );
    assert_eq!(
        parse_negotiation_sequence("\x1b[?c"),
        Some(NegotiationSequence::DeviceAttributes)
    );
    assert_eq!(parse_negotiation_sequence("\x1b[62c"), None);
    assert_eq!(parse_negotiation_sequence("\x1b[?6x2c"), None);
}

/// `start` ends with the query; `stop` disables bracketed paste, pops the flags and resets modifyOtherKeys.
#[test]
fn tui_keyboard_protocol_terminal_start_and_stop_write_exact_bytes() {
    let mut terminal = Terminal::new(Vec::<u8>::new());
    terminal.start(false).unwrap();
    terminal.start_keyboard_protocol().unwrap();
    let n = terminal.keyboard.input("\x1b[?62;c", Instant::now());
    terminal.write(&n.write).unwrap();
    terminal.stop().unwrap();
    assert_eq!(
        String::from_utf8(terminal.out).unwrap(),
        "\x1b[?2004h\x1b[>7u\x1b[?u\x1b[c\x1b[>4;2m\x1b[?2004l\x1b[<u\x1b[>4;0m"
    );
    // A terminal that never queried pops nothing.
    let mut plain = Terminal::new(Vec::<u8>::new());
    plain.start(false).unwrap();
    plain.stop().unwrap();
    assert_eq!(String::from_utf8(plain.out).unwrap(), "\x1b[?2004h\x1b[?2004l");
}
