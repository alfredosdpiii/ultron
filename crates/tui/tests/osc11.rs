//! The OSC 11 background reply: `isOsc11BackgroundColorResponse` / `parseOsc11BackgroundColor` replayed from
//! fixtures/osc11-parity.json (recorded from packages/tui/src/terminal-colors.ts by generate-osc11.mjs), and the
//! reader that takes the reply out of the input stream, fed through the stdin buffer as the terminal thread feeds it.

use tui::stdin_buffer::{InputEvent, StdinBuffer};
use tui::terminal_colors::{
    OSC11_BACKGROUND_QUERY, Osc11Read, Osc11Reader, is_osc11_background_color_response, parse_osc11_background_color,
};

fn fixture() -> serde_json::Value {
    serde_json::from_str(include_str!("../fixtures/osc11-parity.json")).expect("fixture")
}

#[test]
fn tui_osc11_reply_parsing_matches_typescript() {
    let fx = fixture();
    let cases = fx["cases"].as_array().expect("cases");
    assert!(cases.len() >= 80);
    for c in cases {
        let data = c["data"].as_str().expect("data");
        assert_eq!(is_osc11_background_color_response(data), c["is"] == true, "{data:?}");
        let want = c["rgb"].as_object().map(|o| {
            let n = |k: &str| o[k].as_u64().expect("channel") as u8;
            (n("r"), n("g"), n("b"))
        });
        assert_eq!(parse_osc11_background_color(data), want, "{data:?}");
    }
}

#[test]
fn tui_osc11_reply_literals() {
    assert_eq!(OSC11_BACKGROUND_QUERY, "\x1b]11;?\x07");
    // 4-digit and 2-digit channels, BEL and ST.
    assert_eq!(
        parse_osc11_background_color("\x1b]11;rgb:fafa/fafa/fafa\x07"),
        Some((250, 250, 250))
    );
    assert_eq!(
        parse_osc11_background_color("\x1b]11;rgb:1e/1e/2e\x1b\\"),
        Some((30, 30, 46))
    );
    // 1- and 3-digit channels scale from their own range.
    assert_eq!(
        parse_osc11_background_color("\x1b]11;rgb:8/0/f\x07"),
        Some((136, 0, 255))
    );
    assert_eq!(
        parse_osc11_background_color("\x1b]11;rgb:800/000/fff\x07"),
        Some((128, 0, 255))
    );
    assert_eq!(parse_osc11_background_color("\x1b]11;#102030\x07"), Some((16, 32, 48)));
    assert_eq!(parse_osc11_background_color("\x1b]11;rgb:zz/00/00\x07"), None);
    assert!(is_osc11_background_color_response("\x1b]11;garbage\x07"));
    assert!(!is_osc11_background_color_response("\x1b]11;rgb:ff/ff/ff"));
}

fn sequences(buffer: &mut StdinBuffer, chunk: &str) -> Vec<String> {
    buffer
        .process(chunk)
        .into_iter()
        .map(|e| match e {
            InputEvent::Data(d) => d,
            InputEvent::Paste(p) => panic!("paste {p:?}"),
        })
        .collect()
}

/// The terminal thread's path: each read through the stdin buffer, a timed-out partial flushed between reads, every
/// sequence through the reader. Returns the replies and the forwarded input.
type Rgb = (u8, u8, u8);

fn feed(reads: &[&str], expecting: bool) -> (Vec<Option<Rgb>>, Vec<String>) {
    let mut buffer = StdinBuffer::new();
    let mut reader = Osc11Reader::new();
    let (mut replies, mut forward) = (Vec::new(), Vec::new());
    for read in reads {
        let mut seqs = sequences(&mut buffer, read);
        // The next read comes after the stdin buffer's timeout: what is buffered is flushed as one sequence.
        seqs.extend(buffer.flush());
        for seq in seqs {
            let Osc11Read { reply, forward: f } = reader.input(&seq, expecting);
            replies.extend(reply);
            forward.extend(f);
        }
    }
    assert_eq!(reader.held(), "", "nothing left held");
    (replies, forward)
}

#[test]
fn tui_osc11_reader_takes_whole_replies_among_keys() {
    let (replies, forward) = feed(&["ab\x1b]11;rgb:ffff/ffff/ffff\x07cd"], true);
    assert_eq!(replies, [Some((255, 255, 255))]);
    assert_eq!(forward, ["a", "b", "c", "d"]);
    let (replies, forward) = feed(&["\x1b]11;rgb:00/00/00\x1b\\x"], true);
    assert_eq!(replies, [Some((0, 0, 0))]);
    assert_eq!(forward, ["x"]);
    // A complete reply is never typed text, even when no query is outstanding (it came late).
    let (replies, forward) = feed(&["\x1b]11;rgb:ffff/ffff/ffff\x07"], false);
    assert_eq!(replies, [Some((255, 255, 255))]);
    assert!(forward.is_empty());
    // Garbage in a well-formed reply is taken and reports no color.
    let (replies, forward) = feed(&["\x1b]11;nonsense\x07"], true);
    assert_eq!(replies, [None]);
    assert!(forward.is_empty());
}

#[test]
fn tui_osc11_reader_joins_replies_split_across_reads() {
    for reads in [
        &["\x1b]11;rgb:fa", "fa/fafa/fafa\x07"][..],
        &["\x1b]1", "1;rgb:fafa/fafa/fafa\x07"],
        &["\x1b]11;", "rgb:fafa/", "fafa/fafa", "\x07"],
        &["\x1b]11;rgb:fafa/fafa/fafa", "\x1b\\"],
        &["\x1b]11;rgb:fafa/fafa/fafa\x1b", "\\"],
        &["\x1b]11;rgb:fafa/fafa/fafa", "\x1b", "\\"],
    ] {
        let (replies, forward) = feed(reads, true);
        assert_eq!(replies, [Some((250, 250, 250))], "{reads:?}");
        assert!(forward.is_empty(), "{reads:?}: {forward:?}");
    }
    let (replies, forward) = feed(&["\x1b]11;rgb:1e/1e", "/2e\x07q"], true);
    assert_eq!(replies, [Some((30, 30, 46))]);
    assert_eq!(forward, ["q"]);
}

#[test]
fn tui_osc11_reader_drops_broken_fragments_and_keeps_keys() {
    // A fragment that began `ESC ] 11 ;` and stopped being a reply is dropped; the key after it is kept.
    let (replies, forward) = feed(&["\x1b]11;rgb:ff", "\x7f"], true);
    assert!(replies.is_empty());
    assert_eq!(forward, ["\x7f"]);
    // Shorter fragments may be keys (Alt+]): handed on with what followed.
    let (replies, forward) = feed(&["\x1b]", "x"], true);
    assert!(replies.is_empty());
    assert_eq!(forward, ["\x1b]", "x"]);
    // Nothing is held while no query is outstanding.
    let mut reader = Osc11Reader::new();
    assert_eq!(reader.input("\x1b]11;rgb:ff", false).forward, ["\x1b]11;rgb:ff"]);
    assert_eq!(reader.held(), "");
    // A lone ESC is the Escape key, never held.
    assert_eq!(reader.input("\x1b", true).forward, ["\x1b"]);
    // A body that grows past any real reply is let go.
    let mut reader = Osc11Reader::new();
    assert!(reader.input("\x1b]11;", true).forward.is_empty());
    for _ in 0..64 {
        assert!(reader.input("f", true).forward.is_empty());
    }
    assert!(reader.input("f", true).forward == ["f"]);
    assert_eq!(reader.held(), "");
}
