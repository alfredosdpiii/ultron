//! Editor autocomplete parity: replays crates/tui/fixtures/editor-autocomplete-parity.json, recorded from
//! packages/tui (autocomplete.ts, components/editor.ts, components/select-list.ts) by
//! fixtures/generate-editor-autocomplete.mjs over a temp file tree that the tests rebuild.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Value, json};
use tui::autocomplete::{
    AutocompleteItem, AutocompleteProvider, AutocompleteSuggestions, CombinedAutocompleteProvider, Completion,
    SlashCommand,
};
use tui::collate::locale_compare;
use tui::components::Component;
use tui::editor::Editor;
use tui::keybindings::{KeybindingsManager, tui_keybindings};
use tui::keys::KeyContext;
use tui::mouse::{MouseButton, MouseEvent, MouseEventType, MouseResult};
use tui::select::{SelectItem, SelectList, SelectListTheme};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/editor-autocomplete-parity.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .expect("array")
        .iter()
        .map(|s| s.as_str().expect("string").to_string())
        .collect()
}

/// The recorded tree under a fresh temp dir; removed on drop.
struct Tree(PathBuf);

impl Tree {
    fn new(fx: &Value, tag: &str) -> Self {
        let base = std::env::temp_dir().join(format!("tui-ac-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(&base).expect("mkdir");
        for entry in strs(&fx["tree"]) {
            if let Some((p, target)) = entry.split_once(" -> ") {
                std::os::unix::fs::symlink(target, base.join(p)).expect("symlink");
            } else if let Some(dir) = entry.strip_suffix('/') {
                std::fs::create_dir_all(base.join(dir)).expect("mkdir");
            } else {
                std::fs::write(base.join(&entry), "").expect("write");
            }
        }
        Tree(base)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).ok();
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

fn fd_path(fx: &Value) -> Option<PathBuf> {
    let p = PathBuf::from(fx["fdPath"].as_str().expect("fdPath"));
    p.exists().then_some(p)
}

/// The command list of the generator.
fn combined(base: &Path, fd: Option<PathBuf>) -> CombinedAutocompleteProvider {
    let mut model = command(
        "model",
        Some("Switch the active model for this session"),
        Some("<name>"),
    );
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
            command("session", Some("Show session info and stats"), None),
            command("skill:review", Some("Review code"), None),
            command("a-very-long-command-name-that-overflows", Some("Long one"), None),
            command("quit", Some("Exit"), None),
            command("export", None, None),
        ],
        base,
        fd,
    )
}

/// The generator's wrapper: `#tag` and `$VAR` items, everything else from the combined provider.
struct Tagged(CombinedAutocompleteProvider);

impl AutocompleteProvider for Tagged {
    fn trigger_characters(&self) -> Vec<String> {
        ["$", "ab", "/", " ", "#"].map(String::from).to_vec()
    }
    fn get_suggestions(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        force: bool,
    ) -> Option<AutocompleteSuggestions> {
        let before = &lines[line][..col];
        let word_start = before
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|&i| matches!(before.as_bytes()[i], b'#' | b'$'));
        if let Some(i) = word_start {
            let (sigil, word) = (&before[i..i + 1], &before[i + 1..]);
            let list: &[&str] = if sigil == "#" {
                &["bug", "build", "feature", "docs"]
            } else {
                &["HOME", "HOST", "PATH"]
            };
            let items: Vec<AutocompleteItem> = list
                .iter()
                .filter(|v| v.to_lowercase().starts_with(&word.to_lowercase()))
                .map(|v| AutocompleteItem {
                    value: format!("{sigil}{v}"),
                    label: v.to_string(),
                    description: None,
                })
                .collect();
            return (!items.is_empty()).then(|| AutocompleteSuggestions {
                items,
                prefix: before[i..].to_string(),
            });
        }
        self.0.get_suggestions(lines, line, col, force)
    }
    fn apply_completion(
        &self,
        lines: &[String],
        line: usize,
        col: usize,
        item: &AutocompleteItem,
        prefix: &str,
    ) -> Completion {
        self.0.apply_completion(lines, line, col, item, prefix)
    }
    fn should_trigger_file_completion(&self, lines: &[String], line: usize, col: usize) -> bool {
        self.0.should_trigger_file_completion(lines, line, col)
    }
}

fn select_theme() -> SelectListTheme {
    SelectListTheme {
        selected_prefix: Box::new(str::to_string),
        selected_text: Box::new(|s| format!("[{s}]")),
        description: Box::new(|s| format!("{{{s}}}")),
        scroll_info: Box::new(|s| format!("({s})")),
        no_match: Box::new(|s| format!("!{s}")),
    }
}

fn item_json(i: &AutocompleteItem) -> Value {
    let mut v = json!({"value": i.value, "label": i.label});
    if let Some(d) = &i.description {
        v["description"] = json!(d);
    }
    v
}

#[test]
fn tui_autocomplete_combined_provider_suggestions_match_ts() {
    let fx = fixture();
    let Some(fd) = fd_path(&fx) else {
        eprintln!("skipped: fd not installed");
        return;
    };
    let tree = Tree::new(&fx, "suggest");
    let base = tree.0.display().to_string();
    let p = combined(&tree.0, Some(fd));
    let mut failures = Vec::new();
    for c in fx["suggestions"].as_array().expect("suggestions") {
        let text = c["text"].as_str().expect("text").replace("<base>", &base);
        let force = c["force"].as_bool().expect("force");
        let got = p
            .get_suggestions(std::slice::from_ref(&text), 0, text.len(), force)
            .map(|s| json!({"items": s.items.iter().map(item_json).collect::<Vec<_>>(), "prefix": s.prefix}))
            .unwrap_or(Value::Null);
        let got: Value = serde_json::from_str(&got.to_string().replace(&base, "<base>")).expect("json");
        if got != c["result"] {
            failures.push(format!("{text:?} force={force}\n got {got}\nwant {}", c["result"]));
        }
    }
    for c in fx["shouldTrigger"].as_array().expect("shouldTrigger") {
        let text = c["text"].as_str().expect("text").to_string();
        let got = p.should_trigger_file_completion(std::slice::from_ref(&text), 0, text.len());
        if json!(got) != c["result"] {
            failures.push(format!("shouldTriggerFileCompletion({text:?}) = {got}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn mouse_event(m: &Value, width: usize) -> MouseEvent {
    let kind = match m["type"].as_str().expect("type") {
        "press" => MouseEventType::Press,
        "release" => MouseEventType::Release,
        "move" => MouseEventType::Move,
        "drag" => MouseEventType::Drag,
        "click" => MouseEventType::Click,
        _ => MouseEventType::Wheel,
    };
    let button = match m["button"].as_str().expect("button") {
        "left" => MouseButton::Left,
        "middle" => MouseButton::Middle,
        "right" => MouseButton::Right,
        _ => MouseButton::None,
    };
    let (x, y) = (m["x"].as_i64().expect("x"), m["y"].as_i64().expect("y"));
    MouseEvent {
        kind,
        button,
        x,
        y,
        screen_x: x,
        screen_y: y + 10,
        width,
        height: 12,
        shift: false,
        alt: false,
        ctrl: false,
        wheel_delta: m["wheelDelta"].as_i64(),
        click_count: None,
    }
}

fn mouse_json(r: Option<MouseResult>) -> Value {
    let Some(r) = r else { return Value::Null };
    let mut v = json!({});
    for (k, f) in [
        ("handled", r.handled),
        ("capture", r.capture),
        ("focus", r.focus),
        ("render", r.render),
    ] {
        if let Some(b) = f {
            v[k] = json!(b);
        }
    }
    v
}

#[test]
fn tui_editor_autocomplete_scripted_sequences_match_ts() {
    let fx = fixture();
    let fd = fd_path(&fx);
    let tree = Tree::new(&fx, "editor");
    let mut failures = Vec::new();
    let mut checked = 0;
    for r in fx["records"].as_array().expect("records") {
        let name = r["name"].as_str().expect("name");
        if fd.is_none() && name.starts_with("at-") {
            continue;
        }
        let width = r["width"].as_u64().expect("width") as usize;
        let keys = KeybindingsManager::new(tui_keybindings(), Vec::new(), KeyContext::default());
        let mut e = Editor::new(
            keys,
            KeyContext::default(),
            Box::new(|s| format!("<{s}>")),
            r["paddingX"].as_u64().expect("padding") as usize,
        );
        e.terminal_rows = 20;
        e.set_autocomplete_max_visible(r["maxVisible"].as_u64().expect("maxVisible") as usize);
        e.set_select_list_theme(select_theme());
        e.set_autocomplete_provider(Box::new(Tagged(combined(&tree.0, fd.clone()))));
        let changes = Rc::new(RefCell::new(Vec::<String>::new()));
        let sink = Rc::clone(&changes);
        e.on_change = Some(Box::new(move |t: &str| sink.borrow_mut().push(t.to_string())));
        for h in strs(&r["history"]).iter().rev() {
            e.add_to_history(h);
        }
        e.render(width);
        changes.borrow_mut().clear();
        let mut submits = Vec::new();
        for (i, step) in r["steps"].as_array().expect("steps").iter().enumerate() {
            let key = &step["key"];
            let mut mouse_result = Value::Null;
            if let Some(k) = key.as_str() {
                submits.extend(e.handle_input(k));
            } else {
                mouse_result = mouse_json(e.handle_mouse(&mouse_event(&key["mouse"], width)));
            }
            let pending = e.has_pending_autocomplete();
            e.fire_autocomplete_debounce();
            let (line, col) = e.cursor();
            let got = json!({
                "pending": pending,
                "mouseResult": mouse_result,
                "lines": e.lines(),
                "line": line,
                "col": e.lines()[line][..col].encode_utf16().count(),
                "showing": e.is_showing_autocomplete(),
                "render": e.render(width),
                "changes": changes.borrow_mut().drain(..).collect::<Vec<_>>(),
            });
            let mut want = step.clone();
            want.as_object_mut().expect("step").remove("key");
            // Object equality ignores key order (TS spreads `focus` after the list's fields).
            if got != want {
                failures.push(format!("{name} w={width} step {i} key {key}:\n got {got}\nwant {want}"));
                break;
            }
        }
        if submits != strs(&r["submits"]) {
            failures.push(format!("{name} w={width}: submits {submits:?} want {}", r["submits"]));
        }
        checked += 1;
    }
    assert!(checked >= 90, "only {checked} records checked");
    assert!(
        failures.is_empty(),
        "{} differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn tui_select_list_truncate_primary_and_columns_match_ts() {
    let fx = fixture();
    let items = vec![
        SelectItem {
            value: "short".into(),
            label: "short".into(),
            description: Some("first".into()),
        },
        SelectItem {
            value: "medium-length-value".into(),
            label: "medium-length-value".into(),
            description: Some("second\nline".into()),
        },
        SelectItem {
            value: "x".repeat(40),
            label: String::new(),
            description: Some("unlabeled long value".into()),
        },
        SelectItem {
            value: "plain".into(),
            label: "plain".into(),
            description: None,
        },
    ];
    for c in fx["select"].as_array().expect("select") {
        let mut list = SelectList::new(items.clone(), 3, select_theme());
        list.min_primary_column_width = c["layout"]["minPrimaryColumnWidth"].as_u64().map(|v| v as usize);
        list.max_primary_column_width = c["layout"]["maxPrimaryColumnWidth"].as_u64().map(|v| v as usize);
        let contexts = Rc::new(RefCell::new(Vec::<Value>::new()));
        if c["custom"].as_bool().expect("custom") {
            let sink = Rc::clone(&contexts);
            list.truncate_primary = Some(Box::new(move |ctx| {
                sink.borrow_mut()
                    .push(json!({"text": ctx.text, "maxWidth": ctx.max_width,
                    "columnWidth": ctx.column_width, "value": ctx.item.value, "isSelected": ctx.is_selected}));
                if ctx.is_selected {
                    format!("*{}*", ctx.text)
                } else {
                    format!("{}~", ctx.text.chars().take(3).collect::<String>())
                }
            }));
        }
        list.set_selected_index(1);
        let width = c["width"].as_u64().expect("width") as usize;
        assert_eq!(list.render(width), strs(&c["render"]), "{c}");
        assert_eq!(Value::Array(contexts.borrow().clone()), c["contexts"], "{c}");
    }
}

#[test]
fn tui_collate_locale_compare_matches_ts_sort() {
    let fx = fixture();
    let mut corpus = strs(&fx["corpus"]);
    corpus.sort_by(|a, b| locale_compare(a, b));
    assert_eq!(corpus, strs(&fx["collate"]));
}
