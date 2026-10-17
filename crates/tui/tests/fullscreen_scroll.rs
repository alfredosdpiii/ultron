//! Replays fixtures/fullscreen-scroll-parity.json (packages/tui/src/tui-alt-screen.ts driving the client TUI's
//! fullscreen layout, recorded by fixtures/generate-fullscreen-scroll.mjs) against `tui::fullscreen`: every step's
//! viewport top, follow state, painted frame (after line resets) and copied text must match.

use std::time::{Duration, Instant};

use serde_json::Value;
use tui::fullscreen::{FullscreenViewport, ViewportInput};
use tui::keybindings::{KeybindingsManager, tui_keybindings};
use tui::keys::KeyContext;
use tui::screen::{AltScreen, StackEntry, allocate_stack_sizes, stack_lines};
use tui::scroll_view::{Rect, ScrollView, Scrollbar, slice_by_column};
use tui::utils::{normalize_terminal_output, visible_width};

const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";
const PROMPT: &str = "\x1b]133;A\x07";
const INDICATOR: &str = "\x1b[7m ↓ Jump to latest message · End \x1b[27m";

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/fullscreen-scroll-parity.json")).unwrap()
}

fn keys(user: &Value) -> KeybindingsManager {
    let user = user
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), Some(vec![v.as_str().unwrap().to_string()])))
        .collect();
    KeybindingsManager::new(tui_keybindings(), user, KeyContext::default())
}

/// The generator's `documentLines`.
fn document_line(n: usize) -> String {
    match n {
        _ if n % 10 == 5 => format!("{PROMPT}> prompt {n}"),
        7 => "path /usr/local-bin/x.sh end".into(),
        8 => "\x1b[31mred\x1b[39m and \x1b[1mbold\x1b[22m text".into(),
        9 => "日本語 テキスト wide".into(),
        12 => "a very long line that runs well past the terminal width".into(),
        _ => format!("line {n}"),
    }
}

/// The client TUI's layout (`createChatViewport`) over the generator's dock, finished like `doRender`.
fn frame(view: &mut FullscreenViewport, doc: &[String], cols: usize, rows: usize) -> Vec<String> {
    let dock: Vec<Vec<String>> = vec![
        vec![],
        vec!["status".into()],
        vec!["────────".into(), "> editor".into(), "────────".into()],
        vec!["footer".into()],
    ];
    let dock_entries: Vec<StackEntry> = [0, 0, 3, 0]
        .iter()
        .map(|&min| StackEntry {
            min_size: min,
            ..StackEntry::default()
        })
        .collect();
    let dock_height: usize = allocate_stack_sizes(&dock_entries, &dock.iter().map(Vec::len).collect::<Vec<_>>(), None)
        .iter()
        .sum();
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
    let sizes = allocate_stack_sizes(&root, &[doc.len(), dock_height], Some(rows));
    let mut screen = view.layout(
        doc.to_vec(),
        Rect {
            x: 0,
            y: 0,
            width: cols,
            height: sizes[0],
        },
    );
    screen.extend(stack_lines(dock, &dock_entries, &[], sizes[1]));
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

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn replay(scenario: &Value, keys: &KeybindingsManager) {
    let name = scenario["name"].as_str().unwrap();
    let (mut cols, mut rows) = (
        scenario["cols"].as_u64().unwrap() as usize,
        scenario["rows"].as_u64().unwrap() as usize,
    );
    let mut view = FullscreenViewport::new(ScrollView::new(true, Scrollbar::Auto));
    view.copy_on_select = scenario["copyOnSelect"].as_bool().unwrap();
    if scenario["indicator"].as_bool().unwrap() {
        view.indicator = Some(INDICATOR.into());
    }
    if name == "wheel" {
        view.wheel_lines = 3;
    }
    let copy_result = match &scenario["copyResult"] {
        Value::Bool(true) => Ok(()),
        Value::String(s) => Err(s.clone()),
        other => panic!("{other}"),
    };
    let inject = scenario["injectCopy"].as_bool().unwrap();
    let mut doc: Vec<String> = (1..=scenario["doc"].as_u64().unwrap() as usize)
        .map(document_line)
        .collect();
    let base = Instant::now();
    let mut clock = Duration::ZERO;
    let mut copies: Vec<String> = Vec::new();
    let mut osc52: Vec<String> = Vec::new();
    for (i, expected) in scenario["frames"].as_array().unwrap().iter().enumerate() {
        let step = &expected["step"];
        if let Some(ms) = step["at"].as_u64() {
            clock += Duration::from_millis(ms);
        }
        let now = base + clock;
        if let Some(size) = step["resize"].as_array() {
            cols = size[0].as_u64().unwrap() as usize;
            rows = size[1].as_u64().unwrap() as usize;
            view.set_size(cols, rows);
        }
        if let Some(n) = step["grow"].as_u64() {
            let len = doc.len();
            doc.extend((len + 1..=len + n as usize).map(document_line));
        }
        if let Some(data) = step["input"].as_str()
            && let ViewportInput::Copy(text) = view.handle_input(data, keys, now)
        {
            if inject {
                copies.push(text);
                view.copied(copy_result.clone(), now);
            } else {
                osc52.push(format!(
                    "\x1b]52;c;{}\x07",
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &text)
                ));
                view.copied(Ok(()), now);
            }
        }
        if let Some(n) = step["growAfter"].as_u64() {
            let len = doc.len();
            doc.extend((len + 1..=len + n as usize).map(document_line));
        }
        if step["autoscroll"].as_bool() == Some(true) && view.is_auto_scrolling() {
            view.auto_scroll_tick();
        }
        if step["expire"].as_bool() == Some(true) {
            view.poll(now + Duration::from_secs(3600));
        }
        let screen = frame(&mut view, &doc, cols, rows);
        let at = format!("{name} frame {i} {step}");
        assert_eq!(screen, strings(&expected["screen"]), "{at}");
        assert_eq!(view.viewport_top() as u64, expected["top"].as_u64().unwrap(), "{at}");
        assert_eq!(
            view.is_following_output(),
            expected["following"].as_bool().unwrap(),
            "{at}"
        );
        assert_eq!(copies, strings(&expected["copies"]), "{at}");
        assert_eq!(osc52, strings(&expected["osc52"]), "{at}");
    }
}

macro_rules! scenario_test {
    ($fn_name:ident, $name:literal) => {
        #[test]
        fn $fn_name() {
            let f = fixture();
            let keys = keys(&f["userBindings"]);
            let scenario = f["scenarios"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["name"] == $name)
                .unwrap();
            replay(scenario, &keys);
        }
    };
}

scenario_test!(tui_fullscreen_page_half_page_line_top_bottom_keys_match_ts, "keys");
scenario_test!(tui_fullscreen_previous_next_prompt_jumps_match_ts, "prompts");
scenario_test!(tui_fullscreen_wheel_scrolling_matches_ts, "wheel");
scenario_test!(tui_fullscreen_follow_end_on_growth_matches_ts, "follow");
scenario_test!(tui_fullscreen_scrollbar_hover_jump_and_drag_match_ts, "scrollbar");
scenario_test!(tui_fullscreen_jump_to_end_indicator_matches_ts, "indicator");
scenario_test!(tui_fullscreen_drag_selection_copies_on_release_like_ts, "select-drag");
scenario_test!(tui_fullscreen_word_and_line_clicks_match_ts, "select-words");
scenario_test!(tui_fullscreen_edge_drag_auto_scrolls_like_ts, "autoscroll");
scenario_test!(
    tui_fullscreen_copy_on_select_off_keeps_the_selection_like_ts,
    "copy-off"
);
scenario_test!(tui_fullscreen_copy_failure_flashes_the_error_like_ts, "copy-fails");
scenario_test!(tui_fullscreen_copy_without_clipboard_writes_osc52_like_ts, "copy-osc52");
scenario_test!(tui_fullscreen_focus_loss_clears_a_held_selection_like_ts, "focus-loss");
scenario_test!(tui_fullscreen_short_transcript_does_not_scroll_like_ts, "short");
scenario_test!(tui_fullscreen_scrollbar_hides_when_the_content_fits_like_ts, "fits");

/// `beforeTerminalStart`/`beforeTerminalStop`: button-motion tracking inside multiplexers, all-motion otherwise;
/// leaving turns tracking off and autowrap back on.
#[test]
fn tui_fullscreen_mouse_tracking_sequences_match_ts() {
    for case in fixture()["startStop"].as_array().unwrap() {
        let env = case["env"].as_object().unwrap().clone();
        let var = |name: &str| env.get(name).and_then(Value::as_str).map(str::to_string);
        let mouse = tui::fullscreen::mouse_tracking(&var);
        assert_eq!(
            AltScreen::new().enter_with_mouse(mouse),
            case["enter"].as_str().unwrap(),
            "{case}"
        );
        let exit = case["exit"].as_str().unwrap();
        assert!(exit.starts_with(&AltScreen::release_mouse()), "{case}");
    }
}
