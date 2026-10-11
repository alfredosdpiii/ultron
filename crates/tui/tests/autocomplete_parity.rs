//! Autocomplete parity: replays crates/tui/fixtures/autocomplete-parity.json, recorded from
//! packages/tui/src/autocomplete.ts by fixtures/generate-autocomplete.mjs.

use serde_json::Value;
use tui::autocomplete::{
    AutocompleteItem, AutocompleteProvider, CombinedAutocompleteProvider, SlashCommand, apply_completion,
};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/autocomplete-parity.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn item(v: &Value) -> AutocompleteItem {
    AutocompleteItem {
        value: v["value"].as_str().expect("value").into(),
        label: v["label"].as_str().expect("label").into(),
        description: v["description"].as_str().map(str::to_string),
    }
}

fn command(name: &str, description: Option<&str>, hint: Option<&str>) -> SlashCommand {
    SlashCommand {
        name: name.into(),
        description: description.map(str::to_string),
        argument_hint: hint.map(str::to_string),
        argument_completions: None,
    }
}

/// The command list of generate-autocomplete.mjs.
fn provider() -> CombinedAutocompleteProvider {
    let mut model = command("model", Some("Switch model"), Some("<name>"));
    model.argument_completions = Some(Box::new(|p: &str| {
        Some(
            ["alpha", "beta", "gamma"]
                .into_iter()
                .filter(|v| v.starts_with(p))
                .map(AutocompleteItem::new)
                .collect(),
        )
    }));
    CombinedAutocompleteProvider::new(
        vec![
            model,
            command("settings", Some("Open settings"), None),
            command("skill:review", Some("Review code"), None),
            command("session", None, Some("[id]")),
            command("quit", Some("Exit"), None),
            command("export", None, None),
        ],
        "/nonexistent-base",
        None,
    )
}

fn byte_col(line: &str, u16_col: usize) -> usize {
    let mut acc = 0;
    for (b, c) in line.char_indices() {
        if acc >= u16_col {
            return b;
        }
        acc += c.len_utf16();
    }
    line.len()
}

#[test]
fn tui_autocomplete_slash_suggestions_match_ts() {
    let fx = fixture();
    let p = provider();
    for c in fx["suggestions"].as_array().expect("suggestions") {
        let text = c["text"].as_str().expect("text");
        let got = p.get_suggestions(&[text.to_string()], 0, text.len(), false);
        let want = (!c["result"].is_null()).then(|| {
            let items: Vec<AutocompleteItem> = c["result"]["items"]
                .as_array()
                .expect("items")
                .iter()
                .map(item)
                .collect();
            (items, c["result"]["prefix"].as_str().expect("prefix").to_string())
        });
        assert_eq!(got.map(|s| (s.items, s.prefix)), want, "getSuggestions({text:?})");
    }
    // Forced, `/x` is a path: the base dir does not exist, so `xyz` finds nothing under `/`.
    assert_eq!(
        p.get_suggestions(&["/nonexistent-dir/x".to_string()], 0, 18, true),
        None
    );
}

#[test]
fn tui_autocomplete_apply_completion_matches_ts() {
    let fx = fixture();
    for c in fx["applied"].as_array().expect("applied") {
        let line = c["line"].as_str().expect("line");
        let lines = vec!["before".to_string(), line.to_string(), "after".to_string()];
        let col = byte_col(line, c["col"].as_u64().expect("col") as usize);
        let got = apply_completion(&lines, 1, col, &item(&c["item"]), c["prefix"].as_str().expect("prefix"));
        let r = &c["result"];
        let want_lines: Vec<String> = r["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .map(|l| l.as_str().expect("l").to_string())
            .collect();
        let want_col = byte_col(&want_lines[1], r["cursorCol"].as_u64().expect("cursorCol") as usize);
        assert_eq!(
            (got.lines, got.cursor_line, got.cursor_col),
            (want_lines, 1, want_col),
            "applyCompletion({line:?})"
        );
    }
}
