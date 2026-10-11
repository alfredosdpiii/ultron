//! Prompt-history search: replays fixtures/history-search-parity.json (experimental/prompt-history-search.ts and
//! searchPromptHistory, recorded by fixtures/generate-history-search.mjs) key by key, in both themes and three widths.

use std::rc::Rc;

use interactive::history_search::{HistorySearchEvent, PromptHistorySearch, search_prompt_history};
use serde_json::Value;
use tui::components::Component;
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/history-search-parity.json")).unwrap()
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

fn key(name: &str) -> &str {
    match name {
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "enter" => "\r",
        "tab" => "\t",
        "esc" => "\x1b",
        "ctrlc" => "\x03",
        "ctrlr" => "\x12",
        "ctrls" => "\x13",
        "altr" => "\x1br",
        "bs" => "\x7f",
        other => other,
    }
}

#[test]
fn interactive_history_search_filters_like_ts() {
    let f = fixture();
    let history = strings(&f["history"]);
    for case in f["searches"].as_array().unwrap() {
        assert_eq!(
            search_prompt_history(&history, case[0].as_str().unwrap()),
            strings(&case[1]),
            "{case}"
        );
    }
}

#[test]
fn interactive_history_search_keys_and_rows_match_ts() {
    let f = fixture();
    let history = strings(&f["history"]);
    let runs = f["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 42);
    for run in runs {
        let (name, mode) = match run["theme"].as_str().unwrap() {
            "dark-truecolor" => ("dark", ColorMode::Truecolor),
            _ => ("light", ColorMode::Ansi256),
        };
        let theme = Rc::new(create_theme(&builtin_theme_json(name).unwrap(), mode, None).unwrap());
        let width = run["width"].as_u64().unwrap() as usize;
        let mut search = PromptHistorySearch::new(history.clone(), theme, keys(), "");
        search.set_focused(true);
        let mut outcome = Value::Null;
        for step in run["steps"].as_array().unwrap() {
            if let Some(k) = step["key"].as_str() {
                match search.handle_input(key(k)) {
                    HistorySearchEvent::Accept(text) => outcome = serde_json::json!({"accept": text}),
                    HistorySearchEvent::Cancel => outcome = serde_json::json!({"cancel": true}),
                    HistorySearchEvent::None => {}
                }
                assert_eq!(outcome, step["outcome"], "{} {width} {step}", run["theme"]);
            }
            assert_eq!(
                search.render(width),
                strings(&step["lines"]),
                "{} {width} {step}",
                run["theme"]
            );
            assert_eq!(search.query(), step["query"].as_str().unwrap());
            assert_eq!(search.matches().len() as u64, step["matches"].as_u64().unwrap());
            assert_eq!(search.selected(), step["selected"].as_str());
        }
    }
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let mut seeded = PromptHistorySearch::new(history, theme, keys(), "parser");
    assert_eq!(seeded.matches(), strings(&f["seeded"]["matches"]));
    assert_eq!(seeded.render(60), strings(&f["seeded"]["lines"]));
}
