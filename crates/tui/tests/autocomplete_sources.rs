//! Replays fixtures/autocomplete-sources-parity.json (recorded from omp emoji-autocomplete.ts and
//! github-ref-autocomplete.ts), plus ported omp literals for model mentions, the off-thread title source and
//! provider chaining.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tui::autocomplete::{AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions};
use tui::autocomplete_sources::*;

mod support;
use support::latch::{HANG_GUARD, Latch};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/autocomplete-sources-parity.json")).unwrap()
}

fn suggestions_json(s: &Option<AutocompleteSuggestions>) -> Value {
    match s {
        None => Value::Null,
        Some(s) => json!({
            "items": s.items.iter().map(|i| {
                let mut o = json!({"value": i.value, "label": i.label});
                if let Some(d) = &i.description { o["description"] = json!(d); }
                o
            }).collect::<Vec<_>>(),
            "prefix": s.prefix,
        }),
    }
}

#[test]
fn tui_autocomplete_sources_emoji_matches_omp() {
    let f = fixture();
    for case in f["emoji"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let got = emoji_suggestions(input);
        assert_eq!(suggestions_json(&got), case["result"], "{input:?}");
        if let Some(s) = got {
            let line = format!("{input}|after");
            let c = EmojiProvider.apply_completion(&[line], 0, input.len(), &s.items[0], &s.prefix);
            assert_eq!(
                json!({"lines": c.lines, "cursorCol": c.cursor_col}),
                case["applied"],
                "{input:?}"
            );
        }
    }
    for case in f["inline"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let got = emoji_inline_replace(input).map(|(n, s)| json!({"replaceLen": n, "insert": s}));
        assert_eq!(got.unwrap_or(Value::Null), case["result"], "{input:?}");
    }
    for case in f["expand"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        assert_eq!(expand_emoticons(input), case["output"].as_str().unwrap(), "{input:?}");
    }
}

#[test]
fn tui_autocomplete_sources_github_refs_match_omp() {
    for case in fixture()["github"].as_array().unwrap() {
        let input = case["input"].as_str().unwrap();
        let ctx = github_ref_context(input).map(|c| {
            json!({
                "prefix": c.prefix,
                "qualifier": match c.qualifier { Some(RefKind::Pr) => json!("pr"), Some(RefKind::Issue) => json!("issue"), None => Value::Null },
                "number": c.number,
            })
        });
        assert_eq!(ctx.unwrap_or(Value::Null), case["context"], "{input:?}");
        assert_eq!(
            suggestions_json(&github_ref_suggestions(input, None)),
            case["suggestions"],
            "{input:?}"
        );
    }
    let p = GithubRefProvider { titles: None };
    let lines = vec!["see #12 now".to_string()];
    let s = p.get_suggestions(&lines, 0, 7, false).unwrap();
    let c = p.apply_completion(&lines, 0, 7, &s.items[1], &s.prefix);
    assert_eq!(c.lines, ["see issue://12  now"]);
    assert_eq!(c.cursor_col, 15);
}

/// Titles arrive from a slow lookup without blocking suggestions; each reference is fetched once.
#[test]
fn tui_autocomplete_sources_github_titles_off_thread() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let (tx, rx) = mpsc::channel();
    // The lookup blocks until the test releases it: suggestions that waited on it would hang, not merely be slow.
    let slow = Latch::default();
    let release = slow.clone();
    let titles = GhRefTitles::spawn(
        move |kind, n| {
            slow.wait();
            seen.lock().unwrap().push((kind, n));
            (kind == RefKind::Pr).then(|| format!("Fix thing {n}"))
        },
        move || {
            let _ = tx.send(());
        },
    );
    let first = github_ref_suggestions("#7", Some(&titles)).unwrap();
    assert_eq!(first.items[0].description.as_deref(), Some("GitHub pull request"));
    github_ref_suggestions("#7", Some(&titles));
    release.open();
    rx.recv_timeout(HANG_GUARD).unwrap();
    rx.recv_timeout(HANG_GUARD).unwrap();
    let later = github_ref_suggestions("#7", Some(&titles)).unwrap();
    assert_eq!(later.items[0].description.as_deref(), Some("Fix thing 7"));
    assert_eq!(later.items[1].description.as_deref(), Some("GitHub issue"));
    assert!(
        rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "each reference is fetched once"
    );
    assert_eq!(*calls.lock().unwrap(), [(RefKind::Pr, 7), (RefKind::Issue, 7)]);
}

fn models() -> ModelMentionProvider {
    let m = |s: &str, n: &str| MentionModel {
        selector: s.into(),
        name: n.into(),
    };
    ModelMentionProvider {
        models: vec![
            m("anthropic/claude-sonnet-4-5", "Claude Sonnet 4.5"),
            m("openai/gpt-5", "GPT-5"),
            m("anthropic/claude-opus-4-1", "Claude \"Opus\" 4.1"),
        ],
        trigger: '@',
    }
}

#[test]
fn tui_autocomplete_sources_model_mentions() {
    let p = models();
    let at = |text: &str| p.get_suggestions(&[text.to_string()], 0, text.len(), false);
    assert_eq!(mention_prefix("ask @cla", '@'), Some("@cla"));
    assert_eq!(mention_prefix("mail me@x", '@'), None);
    assert_eq!(mention_prefix("@gpt", '^'), None);
    assert!(at("@").is_none(), "a bare trigger leaves @ to file completion");
    assert!(at("@src/").is_none(), "no model matches: falls through");
    let s = at("ask @opus").unwrap();
    assert_eq!(s.prefix, "@opus");
    assert_eq!(s.items[0].label, "anthropic/claude-opus-4-1");
    assert_eq!(s.items[0].description.as_deref(), Some("Claude Opus 4.1"));
    let s = at("@gpt").unwrap();
    assert_eq!(s.items.len(), 1);
    let lines = vec!["ask @gp please".to_string()];
    let c = p.apply_completion(&lines, 0, 7, &s.items[0], "@gp");
    assert_eq!(c.lines, ["ask @openai/gpt-5  please"]);
    assert_eq!(c.cursor_col, "ask @openai/gpt-5 ".len());
}

struct Files;

impl AutocompleteProvider for Files {
    fn trigger_characters(&self) -> Vec<String> {
        vec!["@".into()]
    }
    fn get_suggestions(&self, lines: &[String], line: usize, col: usize, _f: bool) -> Option<AutocompleteSuggestions> {
        let before = &lines[line][..col];
        let at = before.rfind('@')?;
        Some(AutocompleteSuggestions {
            items: vec![AutocompleteItem::new("@src/main.rs")],
            prefix: before[at..].to_string(),
        })
    }
}

#[test]
fn tui_autocomplete_sources_chain_falls_through() {
    let chain = ChainProvider::new(vec![Box::new(models()), Box::new(EmojiProvider), Box::new(Files)]);
    assert_eq!(chain.trigger_characters(), ["@", ":"]);
    let lines = vec!["@gpt".to_string()];
    let s = chain.get_suggestions(&lines, 0, 4, false).unwrap();
    assert_eq!(s.items[0].value, "openai/gpt-5");
    let c = chain.apply_completion(&lines, 0, 4, &s.items[0], &s.prefix);
    assert_eq!(c.lines, ["@openai/gpt-5 "]);
    let lines = vec!["@src/".to_string()];
    let s = chain.get_suggestions(&lines, 0, 5, false).unwrap();
    assert_eq!(
        s.items[0].value, "@src/main.rs",
        "no model matched: the file provider answers"
    );
    let c = chain.apply_completion(&lines, 0, 5, &s.items[0], &s.prefix);
    assert_eq!(c.lines, ["@src/main.rs "]);
    let lines = vec!["yay :tad".to_string()];
    let s = chain.get_suggestions(&lines, 0, 8, false).unwrap();
    assert_eq!(s.items[0].value, "🎉");
    let c = chain.apply_completion(&lines, 0, 8, &s.items[0], &s.prefix);
    assert_eq!(c.lines, ["yay 🎉"]);
}
