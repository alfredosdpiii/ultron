//! Differential parity against packages/tui (TypeScript). Fixture: fixtures/tui-parity.json, produced by
//! `node crates/tui/fixtures/generate.mjs`.

use serde_json::Value;
use tui::components::{BgFn, Component, Panel, Text, TruncatedText};
use tui::keys::{KeyContext, decode_printable_key, parse_key};
use tui::renderer::Renderer;
use tui::stdin_buffer::{InputEvent, StdinBuffer};
use tui::utils::{slice_with_width, truncate_to_width, visible_width, wrap_text_with_ansi};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/tui-parity.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn rows<'a>(f: &'a Value, key: &str) -> &'a Vec<Value> {
    f[key].as_array().expect(key)
}

fn s(v: &Value) -> &str {
    v.as_str().expect("string")
}

fn n(v: &Value) -> usize {
    v.as_u64().expect("number") as usize
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().expect("array").iter().map(|x| s(x).to_string()).collect()
}

fn report(failures: Vec<String>, total: usize) {
    assert!(
        failures.is_empty(),
        "{} of {total} cases differ; first: {:#?}",
        failures.len(),
        &failures[..failures.len().min(5)]
    );
}

#[test]
fn tui_visible_width_of_every_code_point_matches_ts() {
    let f = fixture();
    let runs = rows(&f, "singleWidths");
    let mut failures = Vec::new();
    for (i, run) in runs.iter().enumerate() {
        let start = n(&run[0]) as u32;
        let end = runs.get(i + 1).map_or(0x110000, |r| n(&r[0]) as u32);
        for cp in start..end {
            if let Some(c) = char::from_u32(cp)
                && visible_width(&c.to_string()) != n(&run[1])
            {
                failures.push(format!("U+{cp:04X}: want {}", run[1]));
            }
        }
    }
    report(failures, 0x10f800);
}

#[test]
fn tui_visible_width_of_samples_matches_ts() {
    let f = fixture();
    let cases = rows(&f, "widths");
    let failures: Vec<String> = cases
        .iter()
        .filter(|c| visible_width(s(&c[0])) != n(&c[1]))
        .map(|c| format!("{:?}: want {} got {}", s(&c[0]), c[1], visible_width(s(&c[0]))))
        .collect();
    report(failures, cases.len());
}

#[test]
fn tui_truncate_to_width_matches_ts() {
    let f = fixture();
    let cases = rows(&f, "truncate");
    let mut failures = Vec::new();
    for c in cases {
        let got = truncate_to_width(s(&c[0]), n(&c[1]), s(&c[2]), c[3].as_bool().unwrap_or(false));
        if got != s(&c[4]) {
            failures.push(format!(
                "{:?} w={} e={:?} pad={}: want {:?} got {got:?}",
                s(&c[0]),
                c[1],
                s(&c[2]),
                c[3],
                s(&c[4])
            ));
        }
    }
    report(failures, cases.len());
}

#[test]
fn tui_wrap_text_with_ansi_matches_ts() {
    let f = fixture();
    let cases = rows(&f, "wrap");
    let mut failures = Vec::new();
    for c in cases {
        let got = wrap_text_with_ansi(s(&c[0]), n(&c[1]));
        if got != strings(&c[2]) {
            failures.push(format!(
                "{:?} w={}: want {:?} got {got:?}",
                s(&c[0]),
                c[1],
                strings(&c[2])
            ));
        }
    }
    report(failures, cases.len());
}

#[test]
fn tui_slice_with_width_matches_ts() {
    let f = fixture();
    let cases = rows(&f, "slice");
    let mut failures = Vec::new();
    for c in cases {
        let got = slice_with_width(s(&c[0]), n(&c[1]), n(&c[2]), c[3].as_bool().unwrap_or(false));
        if got != (s(&c[4]).to_string(), n(&c[5])) {
            failures.push(format!(
                "{:?} start={} len={} strict={}: want {:?} got {got:?}",
                s(&c[0]),
                c[1],
                c[2],
                c[3],
                (&c[4], &c[5])
            ));
        }
    }
    report(failures, cases.len());
}

#[test]
fn tui_parse_key_matches_ts() {
    let f = fixture();
    let cases = rows(&f, "parseKey");
    let mut failures = Vec::new();
    for c in cases {
        let ctx = KeyContext {
            kitty_active: c[1].as_bool().unwrap_or(false),
            windows_terminal: false,
        };
        let got = parse_key(s(&c[0]), ctx);
        if got.as_deref() != c[2].as_str() {
            failures.push(format!("{:?} kitty={}: want {} got {got:?}", s(&c[0]), c[1], c[2]));
        }
    }
    report(failures, cases.len());
}

#[test]
fn tui_decode_printable_key_matches_ts() {
    let f = fixture();
    let cases = rows(&f, "printable");
    let mut failures = Vec::new();
    for c in cases {
        let got = decode_printable_key(s(&c[0]));
        if got.as_deref() != c[1].as_str() {
            failures.push(format!("{:?}: want {} got {got:?}", s(&c[0]), c[1]));
        }
    }
    report(failures, cases.len());
}

#[test]
fn tui_stdin_buffer_splits_like_ts() {
    let f = fixture();
    let cases = rows(&f, "stdin");
    let mut failures = Vec::new();
    for c in cases {
        let mut buf = StdinBuffer::new();
        let mut events = Vec::new();
        for chunk in strings(&c[0]) {
            events.extend(buf.process(&chunk));
        }
        let want: Vec<InputEvent> = c[1]
            .as_array()
            .expect("events")
            .iter()
            .map(|e| match s(&e[0]) {
                "paste" => InputEvent::Paste(s(&e[1]).to_string()),
                _ => InputEvent::Data(s(&e[1]).to_string()),
            })
            .collect();
        let flushed = buf.flush();
        if events != want || flushed != strings(&c[2]) {
            failures.push(format!(
                "{:?}: want {want:?} + {:?} got {events:?} + {flushed:?}",
                c[0], c[2]
            ));
        }
    }
    report(failures, cases.len());
}

#[test]
fn tui_text_components_render_like_ts() {
    let f = fixture();
    let cases = rows(&f, "components");
    let mut failures = Vec::new();
    for c in cases {
        let (text, width, px, py) = (s(&c[1]), n(&c[2]), n(&c[3]), n(&c[4]));
        let got = match s(&c[0]) {
            "text" if c[5].as_bool() == Some(true) => {
                Text::new(text, px, py, Some(Box::new(|l: &str| format!("\x1b[44m{l}\x1b[49m")))).render(width)
            }
            "text" => Text::new(text, px, py, None).render(width),
            "box" => {
                let bg: Option<BgFn> =
                    (c[5].as_bool() == Some(true)).then(|| Box::new(|l: &str| format!("\x1b[44m{l}\x1b[49m")) as BgFn);
                let children: Vec<Box<dyn Component>> = vec![
                    Box::new(Text::new(text, 1, 0, None)),
                    Box::new(TruncatedText {
                        text: text.to_string(),
                        padding_x: 0,
                        padding_y: 0,
                    }),
                ];
                Panel {
                    children,
                    padding_x: px,
                    padding_y: py,
                    bg,
                }
                .render(width)
            }
            _ => TruncatedText {
                text: text.to_string(),
                padding_x: px,
                padding_y: py,
            }
            .render(width),
        };
        if got != strings(&c[6]) {
            failures.push(format!(
                "{} {text:?} w={width}: want {:?} got {got:?}",
                s(&c[0]),
                strings(&c[6])
            ));
        }
    }
    report(failures, cases.len());
}

#[test]
fn tui_renderer_frames_match_ts_escape_output() {
    let f = fixture();
    let scripts = rows(&f, "renders");
    let mut failures = Vec::new();
    for script in scripts {
        let mut r = Renderer::new();
        r.show_hardware_cursor = script["showHardwareCursor"].as_bool().unwrap_or(false);
        r.clear_on_shrink = script["clearOnShrink"].as_bool().unwrap_or(false);
        for (i, frame) in script["frames"].as_array().expect("frames").iter().enumerate() {
            let got = r
                .render(strings(&frame[0]), n(&frame[1]), n(&frame[2]))
                .expect("frame fits");
            if got != s(&frame[3]) {
                failures.push(format!(
                    "{} frame {i}: want {:?} got {got:?}",
                    script["name"],
                    s(&frame[3])
                ));
            }
        }
    }
    report(failures, scripts.len());
}
