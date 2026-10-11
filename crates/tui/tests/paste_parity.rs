//! Replays fixtures/paste-parity.json (recorded from omp TypeScript by generate-paste.mjs): bracketed paste
//! assembly, tmux control re-encoding, the OSC 5522 typed-paste controller and pasted-path detection.

use serde_json::{Value, json};
use tui::attachments::*;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/paste-parity.json")).unwrap()
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn result_json(r: &PasteResult) -> Value {
    match r {
        PasteResult::NotHandled => json!({"handled": false}),
        PasteResult::Handled {
            content: Some(c),
            remaining,
        } => {
            json!({"handled": true, "pasteContent": c, "remaining": remaining})
        }
        PasteResult::Handled {
            content: None,
            remaining,
        } => json!({"handled": true, "remaining": remaining}),
    }
}

#[test]
fn tui_paste_bracketed_handler_matches_omp() {
    let f = fixture();
    let cases = f["bracketed"]
        .as_array()
        .unwrap()
        .iter()
        .chain(f["bracketedLimit"].as_array().unwrap());
    let mut n = 0;
    for case in cases {
        let mut h = match case["limit"].as_u64() {
            Some(l) => BracketedPaste::with_byte_limit(l as usize),
            None => BracketedPaste::new(),
        };
        let got: Vec<Value> = strs(&case["chunks"])
            .iter()
            .map(|c| result_json(&h.process(c)))
            .collect();
        assert_eq!(Value::from(got), case["results"], "{:?}", case["chunks"]);
        n += 1;
    }
    assert_eq!(n, 13);
}

#[test]
fn tui_paste_reencoded_controls_match_omp() {
    for case in fixture()["reencoded"].as_array().unwrap() {
        assert_eq!(
            decode_reencoded_paste_controls(case["input"].as_str().unwrap()),
            case["output"].as_str().unwrap()
        );
    }
}

#[test]
fn tui_paste_osc5522_controller_matches_omp() {
    let f = fixture();
    let streams = f["enhanced"].as_object().unwrap();
    assert_eq!(streams.len(), 12);
    for (name, steps) in streams {
        let mut c = EnhancedPaste::new();
        for step in steps.as_array().unwrap() {
            let got = c.handle_input(step["input"].as_str().unwrap());
            assert_eq!(got.is_some(), step["handled"].as_bool().unwrap(), "{name}");
            let events: Vec<Value> = got
                .unwrap_or_default()
                .into_iter()
                .map(|e| match e {
                    PasteEvent::Write(d) => json!(["write", d]),
                    PasteEvent::Text(t) => json!(["text", t]),
                    PasteEvent::Image { data_b64, mime } => json!(["image", data_b64, mime]),
                    PasteEvent::Status(m) => json!(["status", m]),
                })
                .collect();
            assert_eq!(Value::from(events), step["events"], "{name} {:?}", step["input"]);
        }
    }
    assert_eq!(EnhancedPaste::enable(), "\x1b[?5522h");
    assert_eq!(EnhancedPaste::new().disable(), "\x1b[?5522l");
}

#[test]
fn tui_paste_path_detection_matches_omp() {
    let opt = |v: Option<Vec<String>>| v.map_or(Value::Null, |p| json!(p));
    for case in fixture()["paths"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let wrapped = format!("{PASTE_START}{input}{PASTE_END}");
        let got = json!({
            "input": input,
            "paths": opt(extract_paste_paths_from_text(input)),
            "imagePaths": opt(extract_image_paste_paths_from_text(input)),
            "imagePath": extract_image_path_from_text(input),
            "bracketed": opt(extract_bracketed_paste_paths(&wrapped)),
            "bracketedImages": opt(extract_bracketed_image_paste_paths(&wrapped)),
        });
        assert_eq!(&got, case, "{input:?}");
    }
}
