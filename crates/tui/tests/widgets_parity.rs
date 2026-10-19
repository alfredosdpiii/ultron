//! Differential parity of the widget ports against packages/tui (TypeScript). Fixture: fixtures/widgets-parity.json,
//! produced by `node crates/tui/fixtures/generate-widgets.mjs`.

use serde_json::Value;

mod support;
use support::latch::HANG_GUARD;
use tui::components::Component;
use tui::image::{
    Capabilities, Dimensions, Image, ImageProtocol, calculate_image_cell_size, detect_capabilities, encode_iterm2,
    encode_kitty, image_dimensions,
};
use tui::overlay::{Anchor, OverlayOptions, SizeValue, composite_line, resolve_overlay_layout};
use tui::select::{
    Loader, SelectItem, SelectList, SelectListTheme, SettingItem, SettingsList, SettingsListTheme, StyleFn,
};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/widgets-parity.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn s(v: &Value) -> &str {
    v.as_str().expect("string")
}

fn n(v: &Value) -> i64 {
    v.as_i64().expect("number")
}

fn opt_s(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().expect("array").iter().map(|x| s(x).to_string()).collect()
}

fn sty(tag: u32) -> StyleFn {
    Box::new(move |t| format!("\x1b[{tag}m{t}\x1b[0m"))
}

fn widths(f: &Value) -> Vec<usize> {
    f["widths"].as_array().unwrap().iter().map(|w| n(w) as usize).collect()
}

fn report(failures: Vec<String>, total: usize) {
    assert!(
        failures.is_empty(),
        "{} of {total} cases differ; first: {:#?}",
        failures.len(),
        &failures[..failures.len().min(3)]
    );
}

fn check_frames(name: &str, c: &mut dyn Component, widths: &[usize], want: &Value, failures: &mut Vec<String>) {
    for (w, lines) in widths.iter().zip(want.as_array().unwrap()) {
        let got = c.render(*w);
        if got != strings(lines) {
            failures.push(format!("{name} width {w}: got {got:?} want {lines}"));
        }
    }
}

#[test]
fn tui_select_list_renders_and_navigates_like_ts() {
    let f = fixture();
    let ws = widths(&f);
    let items: Vec<SelectItem> = f["selectItems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| SelectItem {
            value: s(&i["value"]).into(),
            label: s(&i["label"]).into(),
            description: opt_s(&i["description"]),
        })
        .collect();
    let mut failures = Vec::new();
    let mut total = 0;
    for (si, script) in f["select"].as_array().unwrap().iter().enumerate() {
        let theme = SelectListTheme {
            selected_prefix: sty(36),
            selected_text: sty(1),
            description: sty(2),
            scroll_info: sty(90),
            no_match: sty(33),
        };
        let mut list = SelectList::new(items.clone(), n(&script["maxVisible"]) as usize, theme);
        list.min_primary_column_width = script["layout"]["minPrimaryColumnWidth"].as_u64().map(|v| v as usize);
        list.max_primary_column_width = script["layout"]["maxPrimaryColumnWidth"].as_u64().map(|v| v as usize);
        if let Some(filter) = script["filter"].as_str() {
            list.set_filter(filter);
        }
        let frames = script["frames"].as_array().unwrap();
        let mut events = Vec::new();
        check_frames(
            &format!("script {si} frame 0"),
            &mut list,
            &ws,
            &frames[0],
            &mut failures,
        );
        for (k, key) in script["keys"].as_array().unwrap().iter().enumerate() {
            if let Some(e) = list.handle_input(s(&f["keys"][s(key)])) {
                events.push(match e {
                    tui::select::SelectEvent::SelectionChanged(i) => serde_json::json!(["changed", i.value]),
                    tui::select::SelectEvent::Selected(i) => serde_json::json!(["selected", i.value]),
                    tui::select::SelectEvent::Cancelled => serde_json::json!(["cancelled"]),
                });
            }
            check_frames(
                &format!("script {si} frame {}", k + 1),
                &mut list,
                &ws,
                &frames[k + 1],
                &mut failures,
            );
            total += ws.len();
        }
        if Value::from(events.clone()) != script["events"] {
            failures.push(format!("script {si} events {events:?} want {}", script["events"]));
        }
    }
    report(failures, total);
}

#[test]
fn tui_select_list_width_sweep_matches_ts() {
    let f = fixture();
    let sweep: Vec<usize> = f["sweep"].as_array().unwrap().iter().map(|w| n(w) as usize).collect();
    let items: Vec<SelectItem> = f["selectItems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| SelectItem {
            value: s(&i["value"]).into(),
            label: s(&i["label"]).into(),
            description: opt_s(&i["description"]),
        })
        .collect();
    let mut failures = Vec::new();
    for case in f["selectSweep"].as_array().unwrap() {
        let theme = SelectListTheme {
            selected_prefix: sty(36),
            selected_text: sty(1),
            description: sty(2),
            scroll_info: sty(90),
            no_match: sty(33),
        };
        let mut list = SelectList::new(items.clone(), 7, theme);
        list.set_selected_index(n(&case[0]) as usize);
        check_frames(
            &format!("selected {}", case[0]),
            &mut list,
            &sweep,
            &case[1],
            &mut failures,
        );
    }
    report(failures, sweep.len() * 3);
}

#[test]
fn tui_settings_list_renders_and_cycles_like_ts() {
    let f = fixture();
    let ws = widths(&f);
    let mut failures = Vec::new();
    let mut total = 0;
    let items = || {
        f["settingsItems"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| SettingItem {
                id: s(&i["id"]).into(),
                label: s(&i["label"]).into(),
                current_value: s(&i["currentValue"]).into(),
                values: i["values"]
                    .as_array()
                    .map(|_| strings(&i["values"]))
                    .unwrap_or_default(),
                description: opt_s(&i["description"]),
            })
            .collect::<Vec<_>>()
    };
    for (si, script) in f["settings"].as_array().unwrap().iter().enumerate() {
        let theme = SettingsListTheme {
            label: Box::new(|t, sel| {
                if sel {
                    format!("\x1b[1m{t}\x1b[0m")
                } else {
                    t.to_string()
                }
            }),
            value: Box::new(|t, sel| {
                if sel {
                    format!("\x1b[36m{t}\x1b[0m")
                } else {
                    format!("\x1b[2m{t}\x1b[0m")
                }
            }),
            description: sty(3),
            cursor: "→ ".into(),
            hint: sty(90),
        };
        let list_items = if script["items"].as_bool().unwrap() {
            items()
        } else {
            Vec::new()
        };
        let mut list = SettingsList::new(list_items, n(&script["maxVisible"]) as usize, theme);
        let frames = script["frames"].as_array().unwrap();
        let mut events = Vec::new();
        check_frames(
            &format!("settings {si} frame 0"),
            &mut list,
            &ws,
            &frames[0],
            &mut failures,
        );
        for (k, key) in script["keys"].as_array().unwrap().iter().enumerate() {
            match list.handle_input(s(&f["keys"][s(key)])) {
                Some(tui::select::SettingsEvent::Changed { id, value }) => {
                    events.push(serde_json::json!(["changed", id, value]))
                }
                Some(tui::select::SettingsEvent::Cancelled) => events.push(serde_json::json!(["cancelled"])),
                None => {}
            }
            check_frames(
                &format!("settings {si} frame {}", k + 1),
                &mut list,
                &ws,
                &frames[k + 1],
                &mut failures,
            );
            total += ws.len();
        }
        if Value::from(events.clone()) != script["events"] {
            failures.push(format!("settings {si} events {events:?} want {}", script["events"]));
        }
    }
    report(failures, total);
}

#[test]
fn tui_loader_frames_match_ts() {
    let f = fixture();
    let ws = widths(&f);
    let mut failures = Vec::new();
    for (ci, c) in f["loaders"].as_array().unwrap().iter().enumerate() {
        let indicator = if c["indicator"].is_null() {
            None
        } else {
            Some((
                strings(&c["indicator"]["frames"]),
                c["indicator"]["intervalMs"].as_i64().unwrap_or(0).max(0) as u64,
            ))
        };
        let mut l = Loader::new(sty(36), sty(2), s(&c["message"]), indicator);
        if l.interval_ms() != n(&c["intervalMs"]) as u64 || l.is_animated() != c["animated"].as_bool().unwrap() {
            failures.push(format!("loader {ci}: interval/animated differ"));
        }
        for (t, frame) in c["frames"].as_array().unwrap().iter().enumerate() {
            check_frames(&format!("loader {ci} tick {t}"), &mut l, &ws, frame, &mut failures);
            l.tick();
        }
    }
    for row in f["cancellable"].as_array().unwrap() {
        let mut l = Loader::new(sty(36), sty(2), "Cancel me", None);
        let fired = l.handle_input(s(&row[0]));
        if fired != row[1].as_bool().unwrap() || l.aborted() != row[2].as_bool().unwrap() {
            failures.push(format!("cancel {:?}", row[0]));
        }
    }
    report(failures, 0);
}

fn size(v: &Value) -> Option<SizeValue> {
    match v {
        Value::Null => None,
        Value::String(p) => Some(SizeValue::Percent(p.trim_end_matches('%').parse().unwrap())),
        v => Some(SizeValue::Abs(n(v))),
    }
}

fn overlay_options(o: &Value) -> OverlayOptions {
    let anchor = match o["anchor"].as_str() {
        None | Some("center") => Anchor::Center,
        Some("top-left") => Anchor::TopLeft,
        Some("top-right") => Anchor::TopRight,
        Some("bottom-left") => Anchor::BottomLeft,
        Some("bottom-right") => Anchor::BottomRight,
        Some("top-center") => Anchor::TopCenter,
        Some("bottom-center") => Anchor::BottomCenter,
        Some("left-center") => Anchor::LeftCenter,
        Some(_) => Anchor::RightCenter,
    };
    let m = &o["margin"];
    let margin = match m {
        Value::Null => [0; 4],
        Value::Number(_) => [n(m); 4],
        _ => ["top", "right", "bottom", "left"].map(|k| m[k].as_i64().unwrap_or(0)),
    };
    OverlayOptions {
        width: size(&o["width"]),
        min_width: o["minWidth"].as_i64(),
        max_height: size(&o["maxHeight"]),
        anchor,
        offset_x: o["offsetX"].as_i64().unwrap_or(0),
        offset_y: o["offsetY"].as_i64().unwrap_or(0),
        row: size(&o["row"]),
        col: size(&o["col"]),
        margin,
    }
}

#[test]
fn tui_overlay_layout_matches_ts() {
    let f = fixture();
    let cases = f["layoutCases"].as_array().unwrap();
    let mut failures = Vec::new();
    for c in cases {
        let l = resolve_overlay_layout(&overlay_options(&c[0]), n(&c[1]), n(&c[2]), n(&c[3]));
        let want = &c[4];
        if l.width != n(&want["width"])
            || l.row != n(&want["row"])
            || l.col != n(&want["col"])
            || l.max_height != want["maxHeight"].as_i64()
        {
            failures.push(format!("{c}: got {l:?}"));
        }
    }
    report(failures, cases.len());
}

#[test]
fn tui_overlay_composite_line_matches_ts() {
    let f = fixture();
    let cases = f["composite"].as_array().unwrap();
    let mut failures = Vec::new();
    for c in cases {
        let got = composite_line(
            s(&c[0]),
            s(&c[1]),
            n(&c[2]) as usize,
            n(&c[3]) as usize,
            n(&c[4]) as usize,
        );
        if got != s(&c[5]) {
            failures.push(format!("{c}: got {got:?}"));
        }
    }
    report(failures, cases.len());
}

fn dims(v: &Value) -> Option<Dimensions> {
    (!v.is_null()).then(|| Dimensions {
        width_px: n(&v["widthPx"]) as u32,
        height_px: n(&v["heightPx"]) as u32,
    })
}

fn caps(v: &Value) -> Capabilities {
    Capabilities {
        images: match v["images"].as_str() {
            Some("kitty") => Some(ImageProtocol::Kitty),
            Some("iterm2") => Some(ImageProtocol::Iterm2),
            _ => None,
        },
        true_color: v["trueColor"].as_bool().unwrap(),
        hyperlinks: v["hyperlinks"].as_bool().unwrap(),
    }
}

#[test]
fn tui_image_header_dimensions_match_ts() {
    let f = fixture();
    let mut failures = Vec::new();
    for c in f["dims"].as_array().unwrap() {
        let got = image_dimensions(s(&c[1]), s(&c[0]));
        if got != dims(&c[2]) {
            failures.push(format!("{c}: got {got:?}"));
        }
    }
    report(failures, 0);
}

#[test]
fn tui_image_kitty_and_iterm2_encoding_match_ts() {
    let f = fixture();
    let mut failures = Vec::new();
    for c in f["kitty"].as_array().unwrap() {
        let o = &c[1];
        let u = |k: &str| o[k].as_u64().map(|v| v as u32);
        let got = encode_kitty(
            s(&c[0]),
            u("columns"),
            u("rows"),
            u("imageId"),
            o["moveCursor"].as_bool() != Some(false),
        );
        if got != s(&c[2]) {
            failures.push(format!("kitty {o}: differs"));
        }
    }
    for c in f["iterm"].as_array().unwrap() {
        let o = &c[1];
        let width = o["width"].as_u64().map(|v| v.to_string());
        let got = encode_iterm2(
            s(&c[0]),
            width.as_deref(),
            o["height"].as_str(),
            o["name"].as_str(),
            o["preserveAspectRatio"].as_bool() != Some(false),
            o["inline"].as_bool() != Some(false),
        );
        if got != s(&c[2]) {
            failures.push(format!("iterm {c}: got {got:?}"));
        }
    }
    report(failures, 0);
}

#[test]
fn tui_image_cell_size_matches_ts() {
    let f = fixture();
    let cases = f["cellSizes"].as_array().unwrap();
    let mut failures = Vec::new();
    for c in cases {
        let got = calculate_image_cell_size(
            dims(&c[0]).unwrap(),
            n(&c[1]) as u32,
            c[2].as_u64().map(|v| v as u32),
            dims(&c[3]).unwrap(),
        );
        if got != (n(&c[4]["columns"]) as u32, n(&c[4]["rows"]) as u32) {
            failures.push(format!("{c}: got {got:?}"));
        }
    }
    report(failures, cases.len());
}

#[test]
fn tui_image_component_lines_match_ts() {
    let f = fixture();
    let mut failures = Vec::new();
    for c in f["imageComponents"].as_array().unwrap() {
        let o = &c[3];
        let mut img = Image::new(s(&c[2]), s(&c[1]), sty(33), caps(&c[0]));
        img.image_id = o["imageId"].as_u64().map(|v| v as u32);
        img.max_width_cells = o["maxWidthCells"].as_u64().map(|v| v as u32);
        img.max_height_cells = o["maxHeightCells"].as_u64().map(|v| v as u32);
        img.filename = opt_s(&o["filename"]);
        let got = img.render(n(&c[4]) as usize);
        if got != strings(&c[5]) {
            failures.push(format!("{}: got {got:?} want {}", c[0], c[5]));
        }
    }
    report(failures, 0);
}

#[test]
fn tui_image_capability_detection_matches_ts() {
    let f = fixture();
    let mut failures = Vec::new();
    for c in f["detect"].as_array().unwrap() {
        let env = c[0].clone();
        let got = detect_capabilities(&|k| env[k].as_str().map(str::to_string), false, c[1].as_bool().unwrap());
        if got != caps(&c[2]) {
            failures.push(format!("{c}: got {got:?}"));
        }
    }
    report(failures, 0);
}

#[test]
fn tui_terminal_escape_writes_match_ts() {
    let f = fixture();
    let mut failures = Vec::new();
    for c in f["terminalWrites"].as_array().unwrap() {
        let mut t = tui::terminal::Terminal::new(Vec::new());
        match s(&c[0]) {
            // The Rust product writes these sequences itself (the title in interactive's `update_title`).
            "moveBy" | "clearLine" | "clearFromCursor" | "clearScreen" | "setTitle" => continue,
            "hideCursor" => t.hide_cursor(),
            "showCursor" => t.show_cursor(),
            _ => t.write(s(&c[1])),
        }
        .unwrap();
        if String::from_utf8(t.out).unwrap() != s(&c[2]) {
            failures.push(format!("{c}"));
        }
    }
    report(failures, 0);
}

/// A reader that yields scripted chunks and records when each `read` was entered and returned. A chunk marked
/// `gated` is held until the test releases it, so the lone-escape flush timeout fires with no input racing it.
struct Chunks {
    chunks: Vec<(&'static [u8], bool)>,
    gate: std::sync::mpsc::Receiver<()>,
    reads: std::sync::Arc<std::sync::Mutex<Vec<(std::time::Instant, std::time::Instant)>>>,
}

impl std::io::Read for Chunks {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let entered = std::time::Instant::now();
        let n = if self.chunks.is_empty() {
            0
        } else {
            let (bytes, gated) = self.chunks.remove(0);
            if gated {
                let _ = self.gate.recv_timeout(HANG_GUARD);
            }
            buf[..bytes.len()].copy_from_slice(bytes);
            bytes.len()
        };
        self.reads.lock().unwrap().push((entered, std::time::Instant::now()));
        Ok(n)
    }
}

/// The pump flushes an incomplete sequence after 50 ms without input (10 ms for a lone ESC). The scripted chunks
/// up to the lone ESC must reach the pump within that window or a split sequence is flushed early: correct product
/// behaviour for input that really paused. When load stretches the gap, the run is discarded and repeated.
#[test]
fn tui_terminal_input_pump_splits_sequences_pastes_and_lone_escape() {
    let want = [
        "a",
        "b",
        "\x1b[A",
        "\x1b[1;5C",
        "\x1b[200~paste\x1b[201~",
        "x",
        "\x1b",
        "y",
        "\x1b[",
    ];
    let hang = std::time::Instant::now() + HANG_GUARD;
    loop {
        let (release, gate) = std::sync::mpsc::channel();
        let reads = std::sync::Arc::default();
        let reader = Chunks {
            chunks: vec![
                (b"ab\x1b[A\x1b[1;5", false),
                (b"C\x1b[200~pa", false),
                (b"ste\x1b[201~x", false),
                (b"\x1b", false),
                (b"y", true),
                (b"\x1b[", false),
            ],
            gate,
            reads: std::sync::Arc::clone(&reads),
        };
        let rx = tui::terminal::spawn_input_pump(reader);
        let mut got = Vec::new();
        while let Ok(event) = rx.recv_timeout(HANG_GUARD) {
            if event == "\x1b" {
                let _ = release.send(());
            }
            got.push(event);
        }
        // Each chunk before the ESC was sent after the previous read returned and before the next read began, so
        // the gap between consecutive sends is below the time from the first read's return to the gated read's entry.
        let reads = reads.lock().unwrap().clone();
        let burst = reads[4].0 - reads[0].1;
        if burst >= std::time::Duration::from_millis(45) && std::time::Instant::now() < hang {
            eprintln!("chunks took {burst:?} to arrive; flush timeouts may have fired, repeating");
            continue;
        }
        assert_eq!(got, want);
        return;
    }
}
