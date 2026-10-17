//! Replays fixtures/fullscreen-clicks-parity.json (packages/tui/src/tui-alt-screen.ts and utils.ts, recorded by
//! fixtures/generate-fullscreen-clicks.mjs) against `tui::fullscreen`: OSC 8 hyperlinks under a column, the URLs a
//! click opens (and the clicks, drags, buttons and focus changes that open none), and double-click word selection
//! on CJK, Korean, Thai/Lao/Khmer/Burmese and mixed text, which matches TypeScript (Node's ICU dictionaries) on
//! every line and column.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tui::fullscreen::{FullscreenViewport, ViewportInput, osc8_link_at_column, word_selection};
use tui::keybindings::{KeybindingsManager, tui_keybindings};
use tui::keys::KeyContext;
use tui::scroll_view::{Rect, ScrollView, Scrollbar, slice_by_column};
use tui::utils::{normalize_terminal_output, visible_width};

const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

/// `doRender` of a TuiAltScreen whose children sit in its implicit transcript (`addChild`: a follow-end scroll view
/// over the whole screen), then line resets and rows cut to the terminal width.
fn painted(view: &mut FullscreenViewport, lines: &[String], cols: usize, rows: usize) -> Vec<String> {
    let rect = Rect {
        x: 0,
        y: 0,
        width: cols,
        height: rows,
    };
    let screen = view.layout(lines.to_vec(), rect);
    view.finish_frame(screen, cols, rows)
        .into_iter()
        .map(|l| {
            let l = normalize_terminal_output(&l) + SEGMENT_RESET;
            if visible_width(&l) > cols {
                slice_by_column(&l, 0, cols, true)
            } else {
                l
            }
        })
        .collect()
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/fullscreen-clicks-parity.json")).unwrap()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn tui_fullscreen_osc8_link_under_a_column_matches_ts() {
    for case in fixture()["osc8"].as_array().unwrap() {
        let line = case["line"].as_str().unwrap();
        for (column, expected) in case["links"].as_array().unwrap().iter().enumerate() {
            assert_eq!(
                osc8_link_at_column(line, column).as_deref(),
                expected.as_str(),
                "{line:?} column {column}"
            );
        }
    }
}

/// `openUrl` on a primary click (any release code, any modifier) of an OSC 8 link; not after a drag, a double
/// click, another button, or a press the focus loss ended. Every step's opened URLs, copies and screen match.
#[test]
fn tui_fullscreen_click_opens_osc8_links_like_ts() {
    let f = fixture();
    let links = &f["links"];
    let (cols, rows) = (
        links["cols"].as_u64().unwrap() as usize,
        links["rows"].as_u64().unwrap() as usize,
    );
    let lines = strings(&links["lines"]);
    let keys = KeybindingsManager::new(tui_keybindings(), Default::default(), KeyContext::default());
    let opened: Arc<Mutex<Vec<String>>> = Arc::default();
    let mut view = FullscreenViewport::new(ScrollView::new(true, Scrollbar::Hidden));
    let sink = Arc::clone(&opened);
    view.open_url = Some(Arc::new(move |url: &str| sink.lock().unwrap().push(url.to_string())));
    view.set_size(cols, rows);
    let start = Instant::now();
    let mut screen = painted(&mut view, &lines, cols, rows);
    let mut copies: Vec<String> = Vec::new();
    for (i, frame) in links["frames"].as_array().unwrap().iter().enumerate() {
        let now = start + Duration::from_secs(2 * (i as u64 + 1));
        for input in frame["inputs"].as_array().unwrap() {
            if let ViewportInput::Copy(text) = view.handle_input(input.as_str().unwrap(), &keys, now) {
                copies.push(text);
                view.copied(Ok(()), now);
            }
            screen = painted(&mut view, &lines, cols, rows);
        }
        let at = frame["name"].as_str().unwrap();
        assert_eq!(*opened.lock().unwrap(), strings(&frame["opened"]), "{at}");
        assert_eq!(copies, strings(&frame["copies"]), "{at}");
        assert_eq!(screen, strings(&frame["screen"]), "{at}");
    }
}

/// Without an opener a click on a link is an ordinary click: nothing opens, nothing is selected.
#[test]
fn tui_fullscreen_click_on_a_link_without_an_opener_only_clicks() {
    let f = fixture();
    let mut base = strings(&f["links"]["lines"]);
    base.resize(8, String::new());
    let keys = KeybindingsManager::new(tui_keybindings(), Default::default(), KeyContext::default());
    let mut view = FullscreenViewport::default();
    view.set_size(24, 8);
    view.finish_frame(base.clone(), 24, 8);
    let now = Instant::now();
    assert_eq!(view.handle_input("\x1b[<0;2;1M", &keys, now), ViewportInput::Consumed);
    assert_eq!(view.handle_input("\x1b[<0;2;1m", &keys, now), ViewportInput::Consumed);
    assert!(!view.has_active_selection());
    assert_eq!(view.finish_frame(base.clone(), 24, 8), base);
}

#[test]
fn tui_fullscreen_double_click_word_selection_on_cjk_matches_ts() {
    let words = fixture()["words"].as_array().unwrap().clone();
    assert_eq!(words.len(), 50);
    for case in &words {
        let line = case["line"].as_str().unwrap();
        let ts: Vec<Option<(usize, usize)>> = case["selections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                r.as_array()
                    .map(|r| (r[0].as_u64().unwrap() as usize, r[1].as_u64().unwrap() as usize))
            })
            .collect();
        let rust: Vec<Option<(usize, usize)>> = (0..ts.len()).map(|col| word_selection(line, col)).collect();
        assert_eq!(rust, ts, "{line}");
    }
}

/// A double click inside a Chinese run selects (and copies) the dictionary word under it, as ICU splits 中文分词测试.
#[test]
fn tui_fullscreen_double_click_copies_a_chinese_dictionary_word() {
    let keys = KeybindingsManager::new(tui_keybindings(), Default::default(), KeyContext::default());
    let mut view = FullscreenViewport::default();
    let screen = vec!["中文分词测试很重要".to_string(), String::new()];
    view.set_size(20, 2);
    view.finish_frame(screen.clone(), 20, 2);
    let now = Instant::now();
    let mut copied = Vec::new();
    for data in ["\x1b[<0;6;1M", "\x1b[<0;6;1m", "\x1b[<0;6;1M", "\x1b[<0;6;1m"] {
        if let ViewportInput::Copy(text) = view.handle_input(data, &keys, now) {
            copied.push(text);
        }
        view.finish_frame(screen.clone(), 20, 2);
    }
    assert_eq!(copied, ["分词"]);
}

/// A double click on Japanese text selects (and copies) the word under it, as ICU does for 日本語のテキスト.
#[test]
fn tui_fullscreen_double_click_copies_a_japanese_word() {
    let keys = KeybindingsManager::new(tui_keybindings(), Default::default(), KeyContext::default());
    let mut view = FullscreenViewport::default();
    let screen = vec!["日本語のテキスト".to_string(), String::new()];
    view.set_size(20, 2);
    view.finish_frame(screen.clone(), 20, 2);
    let now = Instant::now();
    let mut copied = Vec::new();
    for data in ["\x1b[<0;10;1M", "\x1b[<0;10;1m", "\x1b[<0;10;1M", "\x1b[<0;10;1m"] {
        if let ViewportInput::Copy(text) = view.handle_input(data, &keys, now) {
            copied.push(text);
        }
        view.finish_frame(screen.clone(), 20, 2);
    }
    assert_eq!(copied, ["テキスト"]);
}
