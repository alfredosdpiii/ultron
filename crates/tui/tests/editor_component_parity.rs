//! Editor component parity: replays crates/tui/fixtures/editor-component-parity.json, recorded from
//! packages/tui/src/components/editor.ts by fixtures/generate-editor-component.mjs.

use serde_json::Value;
use tui::editor::{Editor, is_key_release, word_wrap_line};
use tui::keybindings::{KeybindingsManager, tui_keybindings};
use tui::keys::KeyContext;

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/editor-component-parity.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("array")
        .iter()
        .map(|s| s.as_str().expect("string").to_string())
        .collect()
}

fn editor(padding_x: usize) -> Editor {
    let keys = KeybindingsManager::new(tui_keybindings(), Vec::new(), KeyContext::default());
    let mut e = Editor::new(keys, KeyContext::default(), Box::new(|s| format!("<{s}>")), padding_x);
    e.terminal_rows = 20;
    e
}

fn u16_col(line: &str, byte_col: usize) -> usize {
    line[..byte_col].encode_utf16().count()
}

#[test]
fn tui_editor_component_scripted_sequences_match_ts() {
    let fx = fixture();
    let mut failures = Vec::new();
    let records = fx["records"].as_array().expect("records");
    for r in records {
        let width = r["width"].as_u64().expect("width") as usize;
        let mut e = editor(r["paddingX"].as_u64().expect("padding") as usize);
        e.focused = r["focused"].as_bool().expect("focused");
        for h in strs(&r["history"]).iter().rev() {
            e.add_to_history(h);
        }
        e.render(width);
        let mut submits = Vec::new();
        for (i, step) in r["steps"].as_array().expect("steps").iter().enumerate() {
            let key = step["key"].as_str().expect("key");
            submits.extend(e.handle_input(key));
            let (line, col) = e.cursor();
            let got = (
                e.lines().to_vec(),
                line,
                u16_col(&e.lines()[line], col),
                e.expanded_text(),
                e.render(width),
            );
            let want = (
                strs(&step["lines"]),
                step["line"].as_u64().expect("line") as usize,
                step["col"].as_u64().expect("col") as usize,
                step["expanded"].as_str().expect("expanded").to_string(),
                strs(&step["render"]),
            );
            if got != want {
                failures.push(format!(
                    "{} w={width} focused={} step {i} key {key:?}:\n got {got:?}\nwant {want:?}",
                    r["name"], r["focused"]
                ));
                break;
            }
        }
        if submits != strs(&r["submits"]) {
            failures.push(format!("{} w={width}: submits {submits:?}", r["name"]));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} records differ:\n{}",
        failures.len(),
        records.len(),
        failures.join("\n")
    );
}

#[test]
fn tui_editor_word_wrap_line_matches_ts() {
    let fx = fixture();
    for c in fx["wrapCases"].as_array().expect("wrapCases") {
        let line = c["line"].as_str().expect("line");
        let width = c["width"].as_u64().expect("width") as usize;
        let got: Vec<(String, usize, usize)> = word_wrap_line(line, width)
            .into_iter()
            .map(|ch| (ch.text.clone(), u16_col(line, ch.start), u16_col(line, ch.end)))
            .collect();
        let want: Vec<(String, usize, usize)> = c["chunks"]
            .as_array()
            .expect("chunks")
            .iter()
            .map(|ch| {
                let n = |k: &str| ch[k].as_u64().expect("index") as usize;
                (
                    ch["text"].as_str().expect("text").to_string(),
                    n("startIndex"),
                    n("endIndex"),
                )
            })
            .collect();
        assert_eq!(got, want, "wordWrapLine({line:?}, {width})");
    }
}

#[test]
fn tui_editor_key_release_matches_ts() {
    let fx = fixture();
    for c in fx["events"].as_array().expect("events") {
        let data = c["data"].as_str().expect("data");
        assert_eq!(
            is_key_release(data),
            c["release"].as_bool().expect("release"),
            "isKeyRelease({data:?})"
        );
    }
}
