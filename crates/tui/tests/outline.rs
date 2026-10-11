//! Replays fixtures/outline-parity.json (omp transcript-outline.ts, scroll-viewport.ts scrollOffsetForRow and
//! mouse.ts parseSgrMouse, recorded by fixtures/generate-outline.mjs) against `tui::outline` and
//! `tui::sgr_mouse`, plus behavior tests of the scroll body and frame browser.

use serde_json::Value;
use tui::outline::{
    Align, Anchor, BrowserFrame, FrameBrowser, OutlineBuilder, OutlineEntry, OutlinePaint, OutlineStyle, OutlineTarget,
    ScrollBody, append_outline_entries, compose_outline_column, outline_rows, outline_rule, outline_visibility,
    scroll_offset_for_row, strip_prompt_zones,
};
use tui::sgr_mouse::parse_sgr_mouse;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/outline-parity.json")).unwrap()
}

fn fg(color: &str, s: &str) -> String {
    let code = match color {
        "accent" => 39,
        "success" => 34,
        "dim" => 245,
        "muted" => 244,
        "warning" => 214,
        "error" => 196,
        _ => 15,
    };
    format!("\x1b[38;5;{code}m{s}\x1b[39m")
}

fn bold(s: &str) -> String {
    format!("\x1b[1m{s}\x1b[22m")
}

fn paint() -> OutlinePaint<'static> {
    OutlinePaint { fg: &fg, bold: &bold }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn style(v: &Value) -> OutlineStyle {
    OutlineStyle {
        color: v["color"].as_str().map(str::to_string),
        caption: v["caption"].as_str().map(str::to_string),
    }
}

fn targets(v: &Value) -> Vec<OutlineTarget> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|t| OutlineTarget {
            entry_id: t["entryId"].as_str().unwrap().into(),
            turn_id: t["turnId"].as_str().unwrap().into(),
            is_user_turn: t["isUserTurn"].as_bool().unwrap(),
            start: t["start"].as_u64().unwrap() as usize,
            end: t["end"].as_u64().unwrap() as usize,
            entries: Vec::new(),
        })
        .collect()
}

#[test]
fn tui_outline_rules_and_rows_match_omp() {
    let f = fixture();
    for case in f["rules"].as_array().unwrap() {
        let got = outline_rule(
            &paint(),
            "╭",
            "╮",
            case["inner"].as_u64().unwrap() as usize,
            case["color"].as_str().unwrap(),
            case["caption"].as_str(),
        );
        assert_eq!(got, case["out"].as_str().unwrap(), "{case}");
    }
    for case in f["outlined"].as_array().unwrap() {
        let got = outline_rows(
            &paint(),
            &strings(&case["rows"]),
            case["inner"].as_u64().unwrap() as usize,
            &style(&case["style"]),
        );
        assert_eq!(got, strings(&case["out"]), "{case}");
    }
}

#[test]
fn tui_outline_compose_column_matches_omp() {
    let f = fixture();
    let children: Vec<Vec<String>> = f["children"].as_array().unwrap().iter().map(strings).collect();
    let targets = targets(&f["targets"]);
    let cases = f["columns"].as_array().unwrap();
    assert!(cases.len() > 300);
    for case in cases {
        let selected = case["selected"].as_i64().unwrap();
        let header = (!case["header"].is_null()).then(|| strings(&case["header"]));
        let got = compose_outline_column(
            &paint(),
            &children,
            case["from"].as_u64().unwrap() as usize,
            case["to"].as_u64().unwrap() as usize,
            &targets,
            usize::try_from(selected).ok(),
            case["width"].as_u64().unwrap() as usize,
            header.as_deref(),
            &style(&case["style"]),
        );
        assert_eq!(got.lines, strings(&case["lines"]), "{case}");
        let expected = match (case["selStart"].as_i64().unwrap(), case["selEnd"].as_i64().unwrap()) {
            (-1, -1) => None,
            (s, e) => Some((s as usize, e as usize)),
        };
        assert_eq!(got.selection, expected, "{case}");
    }
    let visibility: Vec<bool> = f["visibility"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b.as_bool().unwrap())
        .collect();
    assert_eq!(outline_visibility(&children, &targets), visibility);
}

#[test]
fn tui_outline_strips_prompt_zones_like_omp() {
    let f = fixture();
    assert_eq!(strip_prompt_zones(&strings(&f["zones"])), strings(&f["stripped"]));
}

struct FakeBuilder {
    /// Per entry: usage rows, then rendered rows.
    steps: Vec<(usize, usize)>,
    usage: Vec<bool>,
}

impl OutlineBuilder for FakeBuilder {
    fn child_count(&self) -> usize {
        self.usage.len()
    }
    fn append(&mut self, index: usize) {
        let (usage, rows) = self.steps[index];
        self.usage.extend(std::iter::repeat_n(true, usage));
        self.usage.extend(std::iter::repeat_n(false, rows));
    }
    fn is_usage_row(&self, child: usize) -> bool {
        self.usage[child]
    }
}

#[test]
fn tui_outline_append_entries_folds_tool_results_like_omp() {
    let f = fixture();
    for script in f["appends"].as_array().unwrap() {
        let entries = script["entries"].as_array().unwrap();
        let mut builder = FakeBuilder {
            steps: entries
                .iter()
                .map(|e| {
                    (
                        e["usage"].as_u64().unwrap() as usize,
                        e["rows"].as_u64().unwrap() as usize,
                    )
                })
                .collect(),
            usage: Vec::new(),
        };
        let outline: Vec<OutlineEntry> = entries
            .iter()
            .map(|e| OutlineEntry {
                id: e["id"].as_str().unwrap().into(),
                is_tool_result: e["role"] == "toolResult",
                is_user_turn: e["isUserTurn"].as_bool().unwrap(),
            })
            .collect();
        let got = append_outline_entries(&mut builder, &outline, 0);
        let expected = script["targets"].as_array().unwrap();
        assert_eq!(got.len(), expected.len(), "{script}");
        for (g, e) in got.iter().zip(expected) {
            assert_eq!(g.entry_id, e["entryId"].as_str().unwrap());
            assert_eq!(g.turn_id, e["turnId"].as_str().unwrap());
            assert_eq!(g.is_user_turn, e["isUserTurn"].as_bool().unwrap());
            assert_eq!(
                (g.start, g.end),
                (
                    e["start"].as_u64().unwrap() as usize,
                    e["end"].as_u64().unwrap() as usize
                )
            );
            let ids: Vec<&str> = g.entries.iter().map(|i| outline[*i].id.as_str()).collect();
            assert_eq!(ids, strings(&e["entries"]));
        }
    }
}

#[test]
fn tui_outline_scroll_offset_for_row_matches_omp() {
    let f = fixture();
    for case in f["scroll"].as_array().unwrap() {
        let c = case.as_array().unwrap();
        let align = match c[4].as_str().unwrap() {
            "start" => Align::Start,
            "center" => Align::Center,
            "end" => Align::End,
            _ => Align::Nearest,
        };
        let got = scroll_offset_for_row(
            c[0].as_i64().unwrap(),
            c[1].as_i64().unwrap(),
            c[2].as_u64().unwrap() as usize,
            c[3].as_u64().unwrap() as usize,
            align,
        );
        assert_eq!(got as u64, c[5].as_u64().unwrap(), "{case}");
    }
}

#[test]
fn tui_sgr_mouse_parses_like_omp() {
    let f = fixture();
    for case in f["mice"].as_array().unwrap() {
        let data = case[0].as_str().unwrap();
        let got = parse_sgr_mouse(data);
        let want = &case[1];
        match got {
            None => assert!(want.is_null(), "{data:?}"),
            Some(m) => {
                assert_eq!(m.button as u64, want["button"].as_u64().unwrap(), "{data:?}");
                assert_eq!(m.col, want["col"].as_i64().unwrap());
                assert_eq!(m.row, want["row"].as_i64().unwrap());
                assert_eq!(m.release, want["release"].as_bool().unwrap());
                assert_eq!(m.wheel, want["wheel"].as_i64());
                assert_eq!(m.motion, want["motion"].as_bool().unwrap());
                assert_eq!(m.left_click, want["leftClick"].as_bool().unwrap());
            }
        }
    }
}

#[test]
fn tui_outline_scroll_body_reveals_changed_anchor_only() {
    let mut body = ScrollBody::default();
    body.set_geometry(100, 10);
    let a = Anchor {
        id: "t5".into(),
        start: 40,
        end: 44,
    };
    assert!(body.reveal(Some(&a)));
    // Below the window: the range's last row (plus margin) lands on the bottom edge.
    assert_eq!(body.offset, 35);
    // A manual scroll survives renders of the same anchor.
    body.scroll(-20);
    assert!(!body.reveal(Some(&a)));
    assert_eq!(body.offset, 15);
    // Above the window: the range's first row (minus margin) lands on the top edge.
    let b = Anchor {
        id: "t1".into(),
        start: 3,
        end: 5,
    };
    assert!(body.reveal(Some(&b)));
    assert_eq!(body.offset, 2);
    assert!(body.has_revealed("t1"));
    // A visible range does not move the window.
    let c = Anchor {
        id: "t2".into(),
        start: 6,
        end: 8,
    };
    body.reveal(Some(&c));
    assert_eq!(body.offset, 2);
    assert!(body.page(1));
    assert_eq!(body.offset, 11);
    assert!(body.to_bottom());
    assert_eq!(body.offset, 90);
    assert!(!body.scroll(5));
}

#[test]
fn tui_outline_frame_browser_lays_out_and_maps_points() {
    let mut browser = FrameBrowser::default();
    let frame = BrowserFrame {
        header: vec!["HEAD".into()],
        body: (0..20).map(|i| format!("row {i}")).collect(),
        anchor: Some(Anchor {
            id: "x".into(),
            start: 15,
            end: 16,
        }),
        footer: vec!["FOOT".into()],
    };
    let out = browser.render(&frame, 6, 7);
    assert_eq!(
        out,
        vec!["HEAD", "row 12", "row 13", "row 14", "row 15", "row 16", "FOOT"]
    );
    assert_eq!(browser.body_rows(), 5);
    assert_eq!(browser.body.offset, 12);
    assert_eq!(browser.to_body_point(1, 3), Some((12, 3)));
    assert_eq!(browser.to_body_point(5, 0), Some((16, 0)));
    assert_eq!(browser.to_body_point(0, 0), None);
    assert_eq!(browser.to_body_point(6, 0), None);
}
