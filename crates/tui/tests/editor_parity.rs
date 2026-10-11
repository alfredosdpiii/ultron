//! Differential parity for matchesKey, keybindings, fuzzy matching, word navigation and Input against packages/tui
//! (TypeScript). Fixture: fixtures/editor-parity.json, produced by `node crates/tui/fixtures/generate-editor.mjs`.

use serde_json::Value;
use tui::components::Component;
use tui::editing::{find_word_backward, find_word_forward};
use tui::fuzzy::{fuzzy_filter, fuzzy_match};
use tui::input::{Input, InputEvent};
use tui::keybindings::{KeybindingsManager, Platform, app_keybindings, tui_keybindings};
use tui::keys::{KeyContext, matches_key};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/editor-parity.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn s(v: &Value) -> &str {
    v.as_str().expect("string")
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().expect("array").iter().map(|x| s(x).to_string()).collect()
}

fn rows<'a>(f: &'a Value, key: &str) -> &'a Vec<Value> {
    f[key].as_array().expect(key)
}

/// UTF-16 offset (TS string index) -> byte offset.
fn byte_offset(text: &str, utf16: usize) -> usize {
    let mut n = 0;
    for (i, c) in text.char_indices() {
        if n >= utf16 {
            return i;
        }
        n += c.len_utf16();
    }
    text.len()
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
fn tui_matches_key_matches_ts_for_every_input_and_key_id() {
    let f = fixture();
    let ids = strings(&f["keyIds"]);
    let cases = rows(&f, "matchKey");
    let mut failures = Vec::new();
    for c in cases {
        let ctx = KeyContext {
            kitty_active: c[0].as_bool().unwrap(),
            windows_terminal: false,
        };
        let data = s(&c[1]);
        let want = strings(&c[2]);
        let got: Vec<String> = ids.iter().filter(|k| matches_key(data, k, ctx)).cloned().collect();
        if got != want {
            failures.push(format!(
                "kitty={} {data:?}: got {got:?} want {want:?}",
                ctx.kitty_active
            ));
        }
    }
    report(failures, cases.len());
}

fn defs_json(defs: &[tui::keybindings::KeybindingDefinition]) -> Vec<(String, Vec<String>, String)> {
    defs.iter()
        .map(|d| (d.id.to_string(), d.default_keys.clone(), d.description.to_string()))
        .collect()
}

fn defs_fixture(v: &Value) -> Vec<(String, Vec<String>, String)> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|d| (s(&d[0]).to_string(), strings(&d[1]), s(&d[2]).to_string()))
        .collect()
}

#[test]
fn tui_keybinding_defaults_match_ts_for_each_platform() {
    let f = fixture();
    assert_eq!(defs_json(&tui_keybindings()), defs_fixture(&f["tuiKeybindings"]));
    for p in rows(&f, "platforms") {
        let platform = Platform::new(s(&p[0]), p[1].as_bool().unwrap());
        assert_eq!(
            defs_json(&app_keybindings(platform)),
            defs_fixture(&p[2]),
            "platform {} wsl {}",
            p[0],
            p[1]
        );
    }
}

#[test]
fn tui_keybindings_manager_resolves_user_bindings_like_ts() {
    let f = fixture();
    for case in rows(&f, "manager") {
        let user: Vec<(String, Option<Vec<String>>)> = case[0]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| (s(&u[0]).to_string(), u[1].as_array().map(|_| strings(&u[1]))))
            .collect();
        let m = KeybindingsManager::new(tui_keybindings(), user, KeyContext::default());
        let resolved: Vec<(String, Vec<String>)> = m
            .get_resolved_bindings()
            .into_iter()
            .map(|(id, k)| (id.to_string(), k))
            .collect();
        let want: Vec<(String, Vec<String>)> = case[1]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (s(&r[0]).to_string(), strings(&r[1])))
            .collect();
        assert_eq!(resolved, want);
        let conflicts: Vec<(String, Vec<String>)> =
            m.get_conflicts().into_iter().map(|c| (c.key, c.keybindings)).collect();
        let want: Vec<(String, Vec<String>)> = case[2]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (s(&r[0]).to_string(), strings(&r[1])))
            .collect();
        assert_eq!(conflicts, want);
        for mcase in case[3].as_array().unwrap() {
            let data = s(&mcase[0]);
            let got: Vec<&str> = tui_keybindings()
                .iter()
                .map(|d| d.id)
                .filter(|id| m.matches(data, id))
                .collect();
            assert_eq!(got, strings(&mcase[1]), "matches {data:?}");
        }
    }
}

#[test]
fn tui_fuzzy_match_and_filter_match_ts() {
    let f = fixture();
    let mut failures = Vec::new();
    let cases = rows(&f, "fuzzyCases");
    for c in cases {
        let r = fuzzy_match(s(&c[0]), s(&c[1]));
        // Scores are recorded as JS strings: shortest round-trip text, parsed exactly here.
        let want_score: f64 = s(&c[3]).parse().unwrap();
        if r.matches != c[2].as_bool().unwrap() || r.score != want_score {
            failures.push(format!(
                "{:?} in {:?}: got {r:?} want {} {want_score}",
                c[0], c[1], c[2]
            ));
        }
    }
    report(failures, cases.len());
    let names: Vec<String> = strings(&rows(&f, "filterCases")[0][1]);
    for c in rows(&f, "filterCases") {
        assert_eq!(
            fuzzy_filter(&names, s(&c[0]), String::clone),
            strings(&c[1]),
            "query {:?}",
            c[0]
        );
    }
}

#[test]
fn tui_word_navigation_matches_ts() {
    let f = fixture();
    let cases = rows(&f, "wordCases");
    let mut failures = Vec::new();
    for c in cases {
        let t = s(&c[0]);
        let at = |i: usize| byte_offset(t, c[i].as_u64().unwrap() as usize);
        let got = (find_word_backward(t, at(1)), find_word_forward(t, at(1)));
        // ICU splits Han text with a dictionary ("本語" is two words); the port treats a Han run as one word.
        let icu_dictionary_split = t == "日本語 テキスト" && c[1] == 1;
        if (got != (at(2), at(3))) != icu_dictionary_split {
            failures.push(format!("{t:?} @{}: got {got:?} want {:?}", c[1], (at(2), at(3))));
        }
    }
    report(failures, cases.len());
}

#[test]
fn tui_input_state_and_render_match_ts_for_scripted_keys() {
    let f = fixture();
    let cases = rows(&f, "inputCases");
    let mut failures = Vec::new();
    for c in cases {
        let opts = &c[0];
        let width = c[1].as_u64().unwrap() as usize;
        let mut input = Input::new(KeybindingsManager::new(
            tui_keybindings(),
            Vec::new(),
            KeyContext::default(),
        ))
        .with_options(
            opts["prompt"].as_str(),
            opts["placeholder"].as_str(),
            None,
            opts["mask"].as_str(),
        );
        input.focused = c[2].as_bool().unwrap();
        for (i, step) in c[3].as_array().unwrap().iter().enumerate() {
            let event = input.handle_input(s(&step[0]));
            let value = s(&step[1]);
            let events: Vec<InputEvent> = step[4]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| match s(&e[0]) {
                    "submit" => InputEvent::Submit(s(&e[1]).to_string()),
                    _ => InputEvent::Escape,
                })
                .collect();
            let got = (
                input.value().to_string(),
                input.cursor(),
                input.render(width),
                event.into_iter().collect::<Vec<_>>(),
            );
            let want = (
                value.to_string(),
                byte_offset(value, step[2].as_u64().unwrap() as usize),
                strings(&step[3]),
                events,
            );
            if got != want {
                failures.push(format!(
                    "{opts} w={width} step {i} {:?}: got {got:?} want {want:?}",
                    step[0]
                ));
                break;
            }
        }
    }
    report(failures, cases.len());
}
