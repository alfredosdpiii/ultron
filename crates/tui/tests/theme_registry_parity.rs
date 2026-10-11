//! Differential parity of the theme registry (theme list, initTheme / setTheme with dark fallback, getThemeByName,
//! change callback, setRegisteredThemes) against packages/coding-agent theme.ts.
//! Fixture: fixtures/theme-registry-parity.json (`node crates/tui/fixtures/generate-theme-registry.mjs`).

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use serde_json::{Value, json};
use tui::theme::registry::ThemeRegistry;
use tui::theme::{ColorMode, load_theme_from_path};

fn accent(theme: Option<&tui::theme::Theme>) -> Value {
    theme.map_or(Value::Null, |t| json!(t.fg("accent", "x").expect("accent")))
}

#[test]
fn tui_theme_registry_matches_ts() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/theme-registry-parity.json");
    let f: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json");
    let root = std::env::temp_dir().join(format!("tui-theme-registry-{}", std::process::id()));
    let custom = root.join("themes");
    std::fs::create_dir_all(&custom).expect("mkdir");
    for (file, content) in f["files"].as_object().expect("files") {
        std::fs::write(custom.join(file), content.as_str().expect("content")).expect("write");
    }
    let builtin = PathBuf::from("/builtin-themes");
    let norm = |s: &str| {
        s.replace(&custom.display().to_string(), "<custom>")
            .replace(&builtin.display().to_string(), "<builtin>")
    };
    let mut reg = ThemeRegistry::new(builtin.clone(), custom.clone(), ColorMode::Truecolor);
    let changes = Rc::new(Cell::new(0));
    let counter = Rc::clone(&changes);
    reg.on_theme_change(Box::new(move || counter.set(counter.get() + 1)));
    for step in f["steps"].as_array().expect("steps") {
        let arg = step["arg"].as_str().unwrap_or("");
        let got = match step["op"].as_str().expect("op") {
            "list" => {
                let list: Vec<Value> = reg
                    .available_themes_with_paths()
                    .into_iter()
                    .map(|t| json!({"name": t.name, "path": t.path.as_deref().map(norm)}))
                    .collect();
                json!({"op": "list", "result": list})
            }
            "init" | "set" | "instance" => {
                let result = match step["op"].as_str().unwrap() {
                    "init" => {
                        reg.init_theme(Some(arg), "dark");
                        Value::Null
                    }
                    "set" => match reg.set_theme(arg) {
                        Ok(()) => json!({"success": true, "error": null}),
                        Err(e) => json!({"success": false, "error": norm(&e)}),
                    },
                    _ => {
                        let theme = reg.theme_by_name(arg).expect("theme");
                        reg.set_theme_instance(theme);
                        Value::Null
                    }
                };
                let current = reg.current();
                json!({"op": step["op"], "arg": arg, "result": result, "changes": changes.get(),
                    "current": {"name": current.and_then(|t| t.name.clone()), "accent": accent(current)}})
            }
            "byName" => json!({"op": "byName", "arg": arg, "result": accent(reg.theme_by_name(arg).as_ref())}),
            "registerInvalid" => {
                let mut theme = load_theme_from_path(&custom.join("Zed.json"), ColorMode::Truecolor).expect("Zed");
                theme.name = Some("a/b".into());
                json!({"op": "registerInvalid", "result": reg.set_registered_themes(vec![theme]).err()})
            }
            other => panic!("unknown op {other}"),
        };
        let mut want = step.clone();
        let mut got = got;
        // serde_json and V8 word JSON syntax errors differently; compare only the prefix of those messages.
        for v in [&mut want, &mut got] {
            if let Some(e) = v.pointer_mut("/result/error")
                && let Some(s) = e.as_str()
                && let Some(i) = s.find("SyntaxError:")
            {
                *e = json!(s[..i].to_string());
            }
        }
        assert_eq!(got, want, "step {step}");
        // The generator registers `reg` right after `setTheme("reg")` first fails.
        if step["op"] == "set" && arg == "reg" && step["result"]["success"] == false {
            let theme = load_theme_from_path(&custom.join("reg-source.json"), ColorMode::Truecolor).expect("reg");
            reg.set_registered_themes(vec![theme]).expect("register");
        }
    }
    std::fs::remove_dir_all(&root).ok();
}
