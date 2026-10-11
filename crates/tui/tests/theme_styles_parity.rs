//! Differential parity of theme text styles against packages/coding-agent theme.ts (chalk 6) at levels 0-3.
//! Fixture: fixtures/theme-styles-parity.json (`node crates/tui/fixtures/generate-theme-styles.mjs`).

use serde_json::Value;
use tui::theme::styles::TextStyle;

#[test]
fn tui_theme_text_styles_match_ts() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/theme-styles-parity.json");
    let f: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json");
    let cases = f["cases"].as_array().expect("cases");
    assert_eq!(cases.len(), 240);
    for c in cases {
        let style = match c["style"].as_str().unwrap() {
            "bold" => TextStyle::Bold,
            "italic" => TextStyle::Italic,
            "underline" => TextStyle::Underline,
            "inverse" => TextStyle::Inverse,
            _ => TextStyle::Strikethrough,
        };
        let level = c["level"].as_u64().unwrap() as u8;
        let input = c["input"].as_str().unwrap();
        assert_eq!(style.apply(input, level), c["output"].as_str().unwrap(), "{c}");
    }
}

#[test]
fn tui_theme_text_styles_literal_examples() {
    assert_eq!(TextStyle::Bold.apply("x", 3), "\x1b[1mx\x1b[22m");
    assert_eq!(
        TextStyle::Inverse.apply("a\r\nb", 1),
        "\x1b[7ma\x1b[27m\r\n\x1b[7mb\x1b[27m"
    );
    assert_eq!(
        TextStyle::Italic.apply("a\x1b[23mb", 2),
        "\x1b[3ma\x1b[23m\x1b[3mb\x1b[23m"
    );
    assert_eq!(TextStyle::Underline.apply("plain", 0), "plain");
}
