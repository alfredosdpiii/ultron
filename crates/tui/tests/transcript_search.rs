//! Replays fixtures/transcript-search-parity.json (packages/tui/src/alt-screen-search.ts, recorded by
//! fixtures/generate-transcript-search.mjs) against `tui::transcript_search`, and tests the selection, highlight
//! and fullscreen view behavior ported from tui-alt-screen.ts over the scrollback/viewport split.

use serde_json::Value;
use tui::components::Component;
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::transcript_search::{
    SearchBar, SearchIndex, SearchMatch, SearchOutcome, SearchSegment, SelectionMode, TranscriptSearch, highlight_row,
    match_key, select_match,
};
use tui::viewport::Transcript;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/transcript-search-parity.json")).unwrap()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn keys() -> KeybindingsManager {
    KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

fn seg(row: usize, start_col: usize, end_col: usize) -> SearchSegment {
    SearchSegment {
        row,
        start_col,
        end_col,
    }
}

#[test]
fn tui_transcript_search_matches_like_ts() {
    let f = fixture();
    let cases = f["matches"].as_array().unwrap();
    assert!(cases.len() > 200);
    for case in cases {
        // `findAltScreenSearchMatches` through the index the search bar uses.
        let got = SearchIndex::default()
            .search(&strings(&case["lines"]), case["query"].as_str().unwrap())
            .0
            .to_vec();
        let want: Vec<SearchMatch> = case["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| SearchMatch {
                segments: m["segments"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| {
                        seg(
                            s["row"].as_u64().unwrap() as usize,
                            s["startCol"].as_u64().unwrap() as usize,
                            s["endCol"].as_u64().unwrap() as usize,
                        )
                    })
                    .collect(),
            })
            .collect();
        assert_eq!(got, want, "{case}");
    }
}

#[test]
fn tui_transcript_search_index_reports_changes_like_ts() {
    let f = fixture();
    let mut index = SearchIndex::default();
    for step in f["indexSteps"].as_array().unwrap() {
        let (matches, changed) = index.search(&strings(&step["lines"]), step["query"].as_str().unwrap());
        assert_eq!(changed, step["changed"].as_bool().unwrap(), "{step}");
        let got: Vec<String> = matches.iter().map(match_key).collect();
        assert_eq!(got, strings(&step["keys"]), "{step}");
    }
}

#[test]
fn tui_transcript_search_bar_renders_and_hit_tests_like_ts() {
    let f = fixture();
    let k = keys();
    for case in f["bars"].as_array().unwrap() {
        let mut bar = SearchBar::new(
            &k,
            false,
            Box::new(|t, hovered| if hovered { format!("[{t}]") } else { t.to_string() }),
        );
        for ch in case["query"].as_str().unwrap().chars() {
            bar.handle_input(&ch.to_string());
        }
        let index = case["index"].as_i64().unwrap();
        bar.set_result(usize::try_from(index).ok(), case["count"].as_u64().unwrap() as usize);
        let hovered = case["hovered"].as_i64().unwrap();
        bar.set_hovered((hovered != 0).then_some(hovered as i8));
        let width = case["width"].as_u64().unwrap() as usize;
        assert_eq!(bar.render(width), strings(&case["lines"]), "{case}");
        let hits: Vec<i64> = (0..width)
            .map(|col| bar.navigation_at(2, col).map_or(0, i64::from))
            .collect();
        let want: Vec<i64> = case["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h.as_i64().unwrap())
            .collect();
        assert_eq!(hits, want, "{case}");
        assert_eq!(
            bar.navigation_at(1, 3).map_or(0, i64::from),
            case["row1"].as_i64().unwrap()
        );
    }
}

fn m(row: usize) -> SearchMatch {
    SearchMatch {
        segments: vec![seg(row, 0, 1)],
    }
}

#[test]
fn tui_transcript_search_selection_follows_alt_screen_rules() {
    let matches = vec![m(2), m(5), m(9)];
    // A new query selects the first match at or below the anchor row, wrapping to the first.
    assert_eq!(
        select_match(&matches, SelectionMode::Query, true, 0, None, None),
        Some(0)
    );
    assert_eq!(
        select_match(&matches, SelectionMode::Query, true, 5, None, None),
        Some(1)
    );
    assert_eq!(
        select_match(&matches, SelectionMode::Query, true, 6, None, None),
        Some(2)
    );
    assert_eq!(
        select_match(&matches, SelectionMode::Query, true, 10, None, None),
        Some(0)
    );
    // Next/previous wrap.
    assert_eq!(
        select_match(&matches, SelectionMode::Next, false, 0, Some(2), None),
        Some(0)
    );
    assert_eq!(
        select_match(&matches, SelectionMode::Previous, false, 0, Some(0), None),
        Some(2)
    );
    assert_eq!(
        select_match(&matches, SelectionMode::Next, false, 0, None, None),
        Some(0)
    );
    assert_eq!(
        select_match(&matches, SelectionMode::Previous, false, 0, None, None),
        Some(2)
    );
    // Changed rows: the previous match is found again by key.
    let key = match_key(&m(5));
    assert_eq!(
        select_match(&matches, SelectionMode::Retain, true, 0, Some(0), Some(&key)),
        Some(1)
    );
    assert_eq!(
        select_match(&matches, SelectionMode::Next, true, 0, Some(0), Some(&key)),
        Some(2)
    );
    // A vanished key retains the clamped index.
    assert_eq!(
        select_match(&matches, SelectionMode::Retain, true, 0, Some(7), Some("x")),
        Some(2)
    );
    assert_eq!(select_match(&[], SelectionMode::Next, true, 0, Some(1), None), None);
}

#[test]
fn tui_transcript_search_highlights_rows_like_ts() {
    let f = fixture();
    let u = |t: &str| format!("<{t}>");
    let c = |t: &str| format!("[{t}]");
    for case in f["highlights"].as_array().unwrap() {
        let ranges: Vec<(usize, usize, bool)> = case["ranges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["startCol"].as_u64().unwrap() as usize,
                    r["endCol"].as_u64().unwrap() as usize,
                    r["current"].as_bool().unwrap(),
                )
            })
            .collect();
        let got = highlight_row(case["line"].as_str().unwrap(), &ranges, &u, &c);
        assert_eq!(got, case["out"].as_str().unwrap(), "{case}");
    }
}

#[test]
fn tui_transcript_search_view_searches_scrollback_and_viewport() {
    let mut transcript = Transcript::new();
    for i in 0..30 {
        let id = transcript.push_live(Box::new(tui::components::Text::new(
            &format!("line {i} alpha"),
            0,
            0,
            None,
        )));
        transcript.finish(id);
    }
    transcript.push_live(Box::new(tui::components::Text::new("live alpha tail", 0, 0, None)));
    let plan = transcript.frame(40, &[]);
    transcript.acknowledge(plan.history.unwrap().id);
    assert_eq!(transcript.retired_len(), 30);
    let rows = transcript.all_rows(40);
    assert_eq!(rows.len(), 31, "scrollback rows then the live row");

    let mut view = TranscriptSearch::new(rows, keys(), false);
    view.set_styles(Box::new(|t| format!("<{t}>")), Box::new(|t| format!("[{t}]")));
    let first = view.render(40, 13);
    assert_eq!(first.len(), 13);
    assert!(
        first[9].starts_with("live alpha tail"),
        "opens at the bottom: {first:?}"
    );
    for ch in "alpha".chars() {
        assert_eq!(view.handle_input(&ch.to_string()), SearchOutcome::Render);
    }
    let out = view.render(40, 13);
    assert_eq!(view.matches().len(), 31);
    // Anchor: the top row of the window when the query started (row 21) -> its match is current.
    assert_eq!(view.selected(), Some(21));
    assert!(out[0].contains("line 21 [alpha]"), "{out:?}");
    assert!(out[1].contains("line 22 <alpha>"));
    assert!(out[11].contains("22/31"), "{:?}", out[11]);
    // Next wraps past the live row back to the first scrollback row and scrolls it a third from the top.
    for _ in 0..10 {
        view.handle_input("\r");
        view.render(40, 13);
    }
    assert_eq!(view.selected(), Some(0));
    assert_eq!(view.scroll_offset(), 0);
    view.handle_input("\x1b[13;2u");
    view.render(40, 13);
    assert_eq!(view.selected(), Some(30), "shift+enter steps back and wraps");
    assert_eq!(view.handle_input("\x1b"), SearchOutcome::Close);
}
