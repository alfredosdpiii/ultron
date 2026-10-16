//! Differential parity of the theme port against packages/coding-agent theme.ts and theme-json.ts (TypeScript).
//! Fixture: fixtures/theme-parity.json, produced by `node crates/tui/fixtures/generate-theme.mjs`.

use std::path::Path;

use serde_json::{Value, json};
use tui::theme::{
    ColorMode, TerminalTheme, Theme, ansi256_to_hex, assert_theme_name_is_valid, builtin_theme_json, create_theme,
    detect_terminal_background_from_env, detect_terminal_background_theme, is_light_theme, language_from_path,
    load_theme_from_path, load_theme_json, parse_auto_theme_setting, parse_theme_json_content, resolve_theme_setting,
    resolved_css_colors, theme_export_colors, theme_for_rgb, validate_theme_json,
};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/theme-parity.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn s(v: &Value) -> &str {
    v.as_str().expect("string")
}

fn mode(v: &Value) -> ColorMode {
    if s(v) == "truecolor" {
        ColorMode::Truecolor
    } else {
        ColorMode::Ansi256
    }
}

fn theme_name(t: TerminalTheme) -> &'static str {
    match t {
        TerminalTheme::Dark => "dark",
        TerminalTheme::Light => "light",
    }
}

/// The theme files the fixture generator wrote to a temp dir, rebuilt from the recorded palettes' sources.
fn ts_theme_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/src/modes/interactive/theme")
}

#[test]
fn tui_theme_validation_errors_match_typebox_messages() {
    let fx = fixture();
    let cases = fx["validation"].as_array().expect("cases");
    assert!(cases.len() >= 29);
    for case in cases {
        let got = validate_theme_json(s(&case["id"]), &case["doc"]).err();
        assert_eq!(got.as_deref(), case["error"].as_str(), "case {}", case["id"]);
    }
}

#[test]
fn tui_theme_validation_caps_errors_at_eight() {
    let mut doc: Value = serde_json::from_str(tui::theme::DARK_JSON).expect("dark");
    doc["vars"] = json!({ "a": true, "b": true, "c": true });
    let err = validate_theme_json("x", &doc).expect_err("invalid");
    assert_eq!(
        err.split("Other errors:\n").nth(1),
        Some(
            "  - /vars/a: must be string\n  - /vars/a: must be integer\n  - /vars/a: must match a schema in anyOf\n  - /vars/b: must be string\n  - /vars/b: must be integer\n  - /vars/b: must match a schema in anyOf\n  - /vars/c: must be string\n  - /vars/c: must be integer"
        )
    );
    doc["vars"] = json!({});
    doc["colors"]["accent"] = json!(300.5);
    let err = validate_theme_json("x", &doc).expect_err("invalid");
    assert_eq!(
        err.split("Other errors:\n").nth(1),
        Some(
            "  - /colors/accent: must be string\n  - /colors/accent: must be integer\n  - /colors/accent: must be <= 255\n  - /colors/accent: must match a schema in anyOf"
        )
    );
}

fn check_palette(theme: &Theme, p: &Value) {
    let ctx = format!("{} {}", p["name"], p["mode"]);
    assert_eq!(theme.name.as_deref(), p["themeName"].as_str(), "{ctx}");
    for (k, v) in p["fg"].as_object().expect("fg") {
        assert_eq!(theme.fg_ansi(k), Ok(s(v).to_string()), "{ctx} fg {k}");
    }
    for (k, v) in p["bg"].as_object().expect("bg") {
        assert_eq!(theme.bg_ansi(k), Ok(s(v).to_string()), "{ctx} bg {k}");
    }
    assert_eq!(theme.fg("accent", "hi").as_deref(), Ok(s(&p["sampleFg"])), "{ctx}");
    assert_eq!(theme.bg("selectedBg", "hi").as_deref(), Ok(s(&p["sampleBg"])), "{ctx}");
    let levels = ["off", "minimal", "low", "medium", "high", "xhigh", "max", "bogus"];
    for (level, want) in levels.iter().zip(p["thinking"].as_array().expect("thinking")) {
        assert_eq!(theme.thinking_border(level, "x"), s(want), "{ctx} thinking {level}");
    }
    assert_eq!(theme.bash_mode_border("x"), s(&p["bash"]), "{ctx}");
    assert_eq!(theme.color_mode(), mode(&p["mode"]));
}

#[test]
fn tui_theme_builtin_palettes_match_typescript() {
    let fx = fixture();
    let mut seen = 0;
    for p in fx["palettes"].as_array().expect("palettes") {
        let name = s(&p["name"]);
        if name != "dark" && name != "light" {
            continue;
        }
        let path = ts_theme_dir().join(format!("{name}.json"));
        let from_file = load_theme_from_path(&path, mode(&p["mode"])).expect("load");
        check_palette(&from_file, p);
        let embedded =
            create_theme(&builtin_theme_json(name).expect("builtin"), mode(&p["mode"]), None).expect("theme");
        check_palette(&embedded, p);
        seen += 1;
    }
    assert_eq!(seen, 4);
}

/// The generator's custom documents, rebuilt here exactly as generate-theme.mjs builds them.
fn custom_docs() -> Vec<(&'static str, Value)> {
    let dark: Value = serde_json::from_str(tui::theme::DARK_JSON).expect("dark");
    let mut custom = dark.clone();
    let colors = custom["colors"].as_object_mut().expect("colors");
    for k in [
        "thinkingMax",
        "searchMatchBg",
        "searchMatchText",
        "scrollbarTrack",
        "scrollbarThumb",
    ] {
        colors.remove(k);
    }
    for (k, v) in [
        ("accent", json!(39)),
        ("border", json!(0)),
        ("text", json!("")),
        ("muted", json!(244)),
        ("selectedBg", json!(17)),
        ("toolErrorBg", json!("")),
        ("dim", json!("#ABCDEF")),
        ("warning", json!("deep")),
    ] {
        colors.insert(k.into(), v);
    }
    custom["name"] = json!("custom");
    let vars = custom["vars"].as_object_mut().expect("vars");
    vars.insert("deep".into(), json!("deeper"));
    vars.insert("deeper".into(), json!(208));
    vars.insert("page".into(), json!(16));
    custom["export"] = json!({ "pageBg": "page", "cardBg": "", "infoBg": "#123456" });
    let mut plain: Value = serde_json::from_str(tui::theme::LIGHT_JSON).expect("light");
    plain.as_object_mut().expect("obj").remove("export");
    plain["name"] = json!("plain");
    vec![("custom", custom), ("plain", plain)]
}

#[test]
fn tui_theme_custom_palettes_with_indices_defaults_and_fallbacks_match_typescript() {
    let fx = fixture();
    let docs = custom_docs();
    let mut seen = 0;
    for p in fx["palettes"].as_array().expect("palettes") {
        let Some((_, doc)) = docs.iter().find(|(n, _)| *n == s(&p["name"])) else {
            continue;
        };
        let json = validate_theme_json("x", doc).expect("valid");
        check_palette(&create_theme(&json, mode(&p["mode"]), None).expect("theme"), p);
        seen += 1;
    }
    assert_eq!(seen, 4);
}

#[test]
fn tui_theme_export_and_css_colors_match_typescript() {
    let fx = fixture();
    let dir = tempfile_dir();
    for (name, doc) in custom_docs() {
        std::fs::write(dir.join(format!("{name}.json")), doc.to_string()).expect("write");
    }
    for e in fx["exports"].as_array().expect("exports") {
        let name = s(&e["name"]);
        assert_eq!(is_light_theme(name), e["isLight"].as_bool().expect("bool"));
        let json = match load_theme_json(name, &dir) {
            Ok(json) => json,
            Err(err) => {
                assert_eq!(err, s(&e["css"]["error"]), "{name}");
                assert_eq!(e["exportColors"], json!({}));
                continue;
            }
        };
        let css = resolved_css_colors(&json, name).expect("css");
        let want = e["css"].as_object().expect("css");
        assert_eq!(css.len(), want.len(), "{name}");
        for (k, v) in &css {
            assert_eq!(want.get(k).and_then(Value::as_str), Some(v.as_str()), "{name} {k}");
        }
        let got = theme_export_colors(&json);
        let x = &e["exportColors"];
        assert_eq!(got.page_bg.as_deref(), x["pageBg"].as_str(), "{name}");
        assert_eq!(got.card_bg.as_deref(), x["cardBg"].as_str(), "{name}");
        assert_eq!(got.info_bg.as_deref(), x["infoBg"].as_str(), "{name}");
    }
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

fn tempfile_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tui-theme-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

#[test]
fn tui_theme_resolution_errors_match_typescript() {
    let fx = fixture();
    for c in fx["loadErrors"].as_array().expect("cases") {
        let json = validate_theme_json("x", &c["doc"]).expect("valid");
        let got = create_theme(&json, mode(&c["mode"]), None).err();
        assert_eq!(got.as_deref(), c["error"].as_str(), "{} {}", c["id"], c["mode"]);
    }
}

#[test]
fn tui_theme_bom_and_parse_errors() {
    let fx = fixture();
    let bom = format!(
        "\u{feff}{}",
        tui::theme::DARK_JSON.replace("\"name\": \"dark\"", "\"name\": \"bom\"")
    );
    assert_eq!(
        parse_theme_json_content("p", &bom).map(|j| j.name).as_deref(),
        Ok(s(&fx["bomName"]))
    );
    let err = parse_theme_json_content("<path>", "{ nope").expect_err("broken");
    let want = s(&fx["parseError"]);
    // The prefix is shared; the JSON parser's own wording after "SyntaxError: " differs (see decisions).
    let prefix = "Failed to parse theme <path>: SyntaxError: ";
    assert!(want.starts_with(prefix) && err.starts_with(prefix), "{err}");
}

#[test]
fn tui_theme_256_color_conversion_matches_typescript() {
    let fx = fixture();
    let cases = fx["to256"].as_array().expect("cases");
    assert!(cases.len() > 90);
    for c in cases {
        let hex = s(&c["hex"]);
        let lookup = |_: &str| Some(tui::theme::Resolved::Str(hex.to_string()));
        let theme = Theme::new(&lookup, ColorMode::Ansi256).expect("theme");
        assert_eq!(theme.fg_ansi("accent"), Ok(s(&c["fg"]).to_string()), "{hex}");
        assert_eq!(theme.bg_ansi("selectedBg"), Ok(s(&c["bg"]).to_string()), "{hex}");
    }
}

#[test]
fn tui_theme_ansi256_to_hex_ported() {
    assert_eq!(ansi256_to_hex(9), "#ff0000");
    assert_eq!(ansi256_to_hex(16), "#000000");
    assert_eq!(ansi256_to_hex(17), "#00005f");
    assert_eq!(ansi256_to_hex(208), "#ff8700");
    assert_eq!(ansi256_to_hex(231), "#ffffff");
    assert_eq!(ansi256_to_hex(232), "#080808");
    assert_eq!(ansi256_to_hex(255), "#eeeeee");
}

#[test]
fn tui_theme_terminal_detection_matches_typescript() {
    let fx = fixture();
    for c in fx["colorfgbg"].as_array().expect("cases") {
        let v = s(&c["value"]);
        let d = detect_terminal_background_from_env(if v.is_empty() { None } else { Some(v) });
        let want = &c["detection"];
        assert_eq!(theme_name(d.theme), s(&want["theme"]), "{v}");
        assert_eq!(d.source, s(&want["source"]), "{v}");
        assert_eq!(d.detail, s(&want["detail"]), "{v}");
        assert_eq!(d.confidence, s(&want["confidence"]), "{v}");
    }
    for c in fx["rgbThemes"].as_array().expect("cases") {
        let rgb: Vec<u8> = c["rgb"]
            .as_array()
            .expect("rgb")
            .iter()
            .map(|n| n.as_u64().expect("n") as u8)
            .collect();
        assert_eq!(
            theme_name(theme_for_rgb(rgb[0], rgb[1], rgb[2])),
            s(&c["theme"]),
            "{rgb:?}"
        );
    }
    let osc = detect_terminal_background_theme(Some((250, 250, 250)), Some("15;0"));
    assert_eq!(
        (osc.theme, osc.source, osc.detail.as_str()),
        (
            TerminalTheme::Light,
            "terminal background",
            "OSC 11 background rgb(250, 250, 250)"
        )
    );
    assert_eq!(detect_terminal_background_theme(None, Some("15;0")).source, "COLORFGBG");
}

#[test]
fn tui_theme_settings_resolution_matches_typescript() {
    let fx = fixture();
    for c in fx["settings"].as_array().expect("cases") {
        let setting = c["setting"].as_str();
        let auto = parse_auto_theme_setting(setting);
        let want = c["auto"]
            .as_object()
            .map(|o| (s(&o["lightTheme"]).to_string(), s(&o["darkTheme"]).to_string()));
        assert_eq!(auto, want, "{setting:?}");
        assert_eq!(
            resolve_theme_setting(setting, TerminalTheme::Light).as_deref(),
            c["light"].as_str(),
            "{setting:?}"
        );
        assert_eq!(
            resolve_theme_setting(setting, TerminalTheme::Dark).as_deref(),
            c["dark"].as_str(),
            "{setting:?}"
        );
    }
    assert_eq!(
        assert_theme_name_is_valid("a/b").err().as_deref(),
        fx["registerSlash"].as_str()
    );
    assert_eq!(assert_theme_name_is_valid("ab"), Ok(()));
}

#[test]
fn tui_theme_language_from_path_matches_typescript() {
    let fx = fixture();
    for c in fx["languages"].as_array().expect("cases") {
        assert_eq!(language_from_path(s(&c["path"])), c["lang"].as_str(), "{}", c["path"]);
    }
}

#[test]
fn tui_theme_embedded_builtins_equal_typescript_files() {
    for (name, embedded) in [("dark", tui::theme::DARK_JSON), ("light", tui::theme::LIGHT_JSON)] {
        let ts = std::fs::read_to_string(ts_theme_dir().join(format!("{name}.json"))).expect("ts theme");
        assert_eq!(embedded, ts, "{name}.json drifted from the TypeScript copy");
    }
}

#[test]
fn tui_theme_constructor_fills_optional_tokens_like_typescript() {
    let fx = fixture();
    let c = &fx["ctorFallbacks"];
    let lookup = |key: &str| {
        let v = c["inputFg"].get(key).or_else(|| c["inputBg"].get(key))?;
        Some(tui::theme::Resolved::Index(v.as_u64().expect("index") as u8))
    };
    let theme = Theme::new(&lookup, ColorMode::Ansi256).expect("theme");
    for (k, v) in c["fg"].as_object().expect("fg") {
        assert_eq!(theme.fg_ansi(k), Ok(s(v).to_string()), "fg {k}");
    }
    for (k, v) in c["bg"].as_object().expect("bg") {
        assert_eq!(theme.bg_ansi(k), Ok(s(v).to_string()), "bg {k}");
    }
    assert_eq!(
        theme.fg_ansi("nope").err().as_deref(),
        Some("Unknown theme color: nope")
    );
    assert_eq!(
        theme.bg_ansi("nope").err().as_deref(),
        Some("Unknown theme background color: nope")
    );
}
