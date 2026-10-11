//! Model hub: replays fixtures/omp-model-hub-parity.json (omp model-browser.ts ordering and search tiers, recorded
//! by fixtures/generate-omp-model-hub.mjs), then drives the hub: scopes sidebar, search, inline thinking strip and
//! the session-only pick, with keys and mouse.

use std::rc::Rc;

use interactive::lane::ModelInfo;
use interactive::model_hub::{
    Focus, ModelHub, ModelHubEvent, ModelItem, ScopeKind, compact_search_text, compare_model_items,
    extract_version_number, model_search_tier,
};
use serde_json::Value;
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};
use tui::utils::strip_terminal_sequences;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/omp-model-hub-parity.json")).unwrap()
}

fn info(provider: &str, id: &str, reasoning: bool) -> ModelInfo {
    ModelInfo {
        provider: provider.into(),
        id: id.into(),
        name: id.into(),
        reasoning,
        context_window: 200_000,
    }
}

#[test]
fn interactive_model_hub_ordering_matches_omp() {
    let f = fixture();
    for c in f["versions"].as_array().unwrap() {
        assert_eq!(
            extract_version_number(c[0].as_str().unwrap()),
            c[1].as_f64().unwrap(),
            "{c}"
        );
    }
    for case in f["sorts"].as_array().unwrap() {
        let mru: Vec<String> = case["mru"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().into())
            .collect();
        let mut items: Vec<ModelItem> = case["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                let (p, id) = s.as_str().unwrap().split_once('/').unwrap();
                ModelItem::new(info(p, id, false))
            })
            .collect();
        items.sort_by(|a, b| compare_model_items(a, b, &mru));
        let got: Vec<&str> = items.iter().map(|i| i.selector.as_str()).collect();
        let want: Vec<&str> = case["sorted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(got, want, "{case}");
    }
    for c in f["compact"].as_array().unwrap() {
        assert_eq!(compact_search_text(c[0].as_str().unwrap()), c[1].as_str().unwrap());
    }
    for c in f["tiers"].as_array().unwrap() {
        let got = model_search_tier(c[0].as_str().unwrap(), c[1].as_str().unwrap(), c[2].as_str().unwrap());
        assert_eq!(got as u64, c[3].as_u64().unwrap(), "{c}");
    }
}

fn hub() -> ModelHub {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let keys = KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    );
    ModelHub::new(
        theme,
        keys,
        vec![
            info("openai", "gpt-4o", false),
            info("anthropic", "claude-sonnet-4-6", true),
            info("openai", "o3", true),
            info("google", "gemini-2.5-pro", true),
            info("anthropic", "claude-opus-4-1", true),
        ],
        vec!["openai/o3".into(), "missing/x".into()],
        Some(("anthropic/claude-sonnet-4-6".into(), "medium".into())),
    )
}

fn labels(h: &ModelHub) -> Vec<String> {
    h.scopes().iter().map(|s| format!("{}:{}", s.label, s.count)).collect()
}

fn shown(h: &ModelHub) -> Vec<&str> {
    h.shown().iter().map(|i| i.selector.as_str()).collect()
}

const UP: &str = "\x1b[A";
const DOWN: &str = "\x1b[B";
const LEFT: &str = "\x1b[D";
const RIGHT: &str = "\x1b[C";

#[test]
fn interactive_model_hub_scopes_recent_all_and_providers() {
    let mut h = hub();
    assert_eq!(
        labels(&h),
        ["Recent:1", "All models:5", ":0", "anthropic:2", "google:1", "openai:2"]
    );
    assert_eq!(h.active_scope(), &ScopeKind::All);
    assert_eq!(
        h.selected().unwrap().selector,
        "anthropic/claude-sonnet-4-6",
        "the session model is preselected"
    );
    assert_eq!(shown(&h)[0], "openai/o3", "recently used first");
    assert_eq!(h.handle_input(UP), ModelHubEvent::Render);
    assert_eq!(h.active_scope(), &ScopeKind::Recent);
    assert_eq!(shown(&h), ["openai/o3"]);
    h.handle_input(DOWN);
    h.handle_input(DOWN);
    assert_eq!(
        h.active_scope(),
        &ScopeKind::Provider("anthropic".into()),
        "the hop skips the separator"
    );
    assert_eq!(shown(&h), ["anthropic/claude-sonnet-4-6", "anthropic/claude-opus-4-1"]);
    let screen: Vec<String> = h.render(90, 16).iter().map(|l| strip_terminal_sequences(l)).collect();
    assert_eq!(screen.len(), 16);
    assert!(screen[0].starts_with("╭ Models ─"));
    assert!(screen[1].contains("⌕"), "search row: {:?}", screen[1]);
    assert!(screen[4].contains("❯ anthropic"), "{screen:#?}");
    assert!(
        screen[3].contains("❯ claude-sonnet-4-6"),
        "provider scope drops the provider prefix: {:?}",
        screen[3]
    );
    assert!(screen[3].contains("200k ◒●"), "context, reasoning and current markers");
    assert!(screen[14].contains("Session-only switch"));
}

#[test]
fn interactive_model_hub_search_floats_matching_providers_and_skips_empty_scopes() {
    let mut h = hub();
    h.handle_input(DOWN);
    h.handle_input(DOWN);
    assert_eq!(h.active_scope(), &ScopeKind::Provider("google".into()));
    for c in "gpt".chars() {
        h.handle_input(&c.to_string());
    }
    assert_eq!(h.focus(), Focus::Models, "typing focuses the model list");
    assert_eq!(
        h.active_scope(),
        &ScopeKind::All,
        "a scope that lost its matches falls back to All"
    );
    assert_eq!(
        labels(&h),
        ["Recent:0", "All models:1", ":0", "openai:1", "anthropic:0", "google:0"]
    );
    assert_eq!(shown(&h), ["openai/gpt-4o"]);
    h.handle_input("\t");
    assert_eq!(h.focus(), Focus::Scopes);
    assert_eq!(h.handle_input(UP), ModelHubEvent::Ignored, "Recent has no matches");
    h.handle_input(DOWN);
    assert_eq!(h.active_scope(), &ScopeKind::Provider("openai".into()));
    assert_eq!(
        h.handle_input(DOWN),
        ModelHubEvent::Ignored,
        "providers without matches are skipped"
    );
    // Exact and contiguous matches rank above fuzzy-only ones.
    let mut h = hub();
    for c in "o3".chars() {
        h.handle_input(&c.to_string());
    }
    assert_eq!(shown(&h), ["openai/o3"]);
    assert_eq!(
        h.handle_input("\x1b"),
        ModelHubEvent::Render,
        "Esc clears the search first"
    );
    assert_eq!(shown(&h).len(), 5);
    assert_eq!(h.handle_input("\x1b"), ModelHubEvent::Cancel);
}

#[test]
fn interactive_model_hub_thinking_strip_picks_for_this_session() {
    let mut h = hub();
    assert_eq!(
        h.handle_input("\r"),
        ModelHubEvent::Render,
        "Enter on the scopes focuses the list"
    );
    assert_eq!(h.focus(), Focus::Models);
    assert_eq!(h.handle_input("\r"), ModelHubEvent::Render);
    assert_eq!(
        h.strip(),
        Some(("anthropic/claude-sonnet-4-6", "medium")),
        "preselects the current level"
    );
    let screen: Vec<String> = h.render(100, 16).iter().map(|l| strip_terminal_sequences(l)).collect();
    assert!(
        screen[14].contains("thinking for claude-sonnet-4-6:  off  minimal  low [medium] high  xhigh"),
        "{:?}",
        screen[14]
    );
    h.handle_input(RIGHT);
    h.handle_input(RIGHT);
    assert_eq!(h.strip().unwrap().1, "xhigh");
    assert_eq!(h.handle_input(RIGHT), ModelHubEvent::Render);
    assert_eq!(h.strip().unwrap().1, "xhigh", "stops at the last level");
    h.handle_input(LEFT);
    assert_eq!(
        h.handle_input("\r"),
        ModelHubEvent::Use {
            selector: "anthropic/claude-sonnet-4-6".into(),
            thinking: Some("high".into())
        }
    );
    // Esc closes the strip without picking; a model without reasoning switches at once.
    h.handle_input("\r");
    assert_eq!(h.handle_input("\x1b"), ModelHubEvent::Render);
    assert_eq!(h.strip(), None);
    while h.selected().unwrap().selector != "openai/gpt-4o" {
        assert_eq!(h.handle_input(DOWN), ModelHubEvent::Render);
    }
    assert_eq!(
        h.handle_input("\r"),
        ModelHubEvent::Use {
            selector: "openai/gpt-4o".into(),
            thinking: None
        }
    );
}

#[test]
fn interactive_model_hub_mouse_selects_scopes_and_models() {
    let mut h = hub();
    let screen: Vec<String> = h.render(90, 16).iter().map(|l| strip_terminal_sequences(l)).collect();
    let google = screen.iter().position(|l| l.contains("  google")).unwrap();
    assert_eq!(
        h.handle_input(&format!("\x1b[<0;4;{}M", google + 1)),
        ModelHubEvent::Render
    );
    assert_eq!(h.active_scope(), &ScopeKind::Provider("google".into()));
    let screen: Vec<String> = h.render(90, 16).iter().map(|l| strip_terminal_sequences(l)).collect();
    let row = screen.iter().position(|l| l.contains("gemini-2.5-pro")).unwrap();
    let col = 40;
    assert_eq!(
        h.handle_input(&format!("\x1b[<0;{col};{}M", row + 1)),
        ModelHubEvent::Render,
        "the first click on the selected row's scope focuses the list and opens the strip"
    );
    assert_eq!(h.strip().map(|s| s.0), Some("google/gemini-2.5-pro"));
    h.handle_input("\x1b");
    assert_eq!(
        h.handle_input("\x1b[<65;40;5M"),
        ModelHubEvent::Ignored,
        "wheel past the only model"
    );
}
