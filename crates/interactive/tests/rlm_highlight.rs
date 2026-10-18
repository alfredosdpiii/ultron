//! The `rlm` cell highlighter against highlight.js 10.7.3 and the TS `highlightCode(code, "python")`.
//! Fixture: fixtures/rlm-highlight.json, produced by `node crates/interactive/fixtures/generate-rlm-highlight.mjs`.

use std::rc::Rc;

use interactive::rlm::highlight::{highlight_code, rlm_highlighter};
use serde_json::Value;
use tui::hljs::Token;
use tui::theme::{ColorMode, Theme, builtin_theme_json, create_theme};

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/rlm-highlight.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn themes() -> Vec<Rc<Theme>> {
    [("dark", ColorMode::Truecolor), ("light", ColorMode::Ansi256)]
        .into_iter()
        .map(|(n, m)| Rc::new(create_theme(&builtin_theme_json(n).expect("theme"), m, None).expect("create")))
        .collect()
}

/// highlight.js's `escapeHTML`.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

/// The `HTMLRenderer` output for a token list (classPrefix `hljs-`).
fn html(tokens: &[Token]) -> String {
    tokens
        .iter()
        .map(|t| match t {
            Token::Text(s) => escape_html(s),
            Token::Open(k) => format!("<span class=\"hljs-{k}\">"),
            Token::Close => "</span>".into(),
        })
        .collect()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

/// Every cell in fixture order (highlight.js keeps matcher state across calls, so the order is part of the
/// replay): the token tree matches highlight.js's HTML, and the themed lines match `highlightCode` per theme.
#[test]
fn interactive_rlm_highlight_matches_highlightjs_and_ts_highlight_code() {
    let f = fixture();
    assert_eq!(f["hljs"], "10.7.3");
    let themes = themes();
    let highlighters: Vec<_> = themes.iter().map(rlm_highlighter).collect();
    let cells = f["cells"].as_array().unwrap();
    assert!(cells.len() > 400);
    let mut colored = 0;
    for (i, cell) in cells.iter().enumerate() {
        let code = cell["code"].as_str().unwrap();
        let tokens = tui::hljs::highlight(code, "python").expect("highlighted");
        assert_eq!(html(&tokens), cell["html"].as_str().unwrap(), "cell {i} html: {code:?}");
        for (ti, hl) in highlighters.iter().enumerate() {
            let want = strings(&cell["lines"][ti]);
            assert_eq!(hl(code), want, "cell {i} theme {ti}: {code:?}");
            colored += usize::from(want.iter().any(|l| l.contains('\x1b')));
        }
    }
    assert!(colored > 600, "most cells carry colors ({colored})");
}

/// `highlightCode` for a language highlight.js does not know: every line in `mdCodeBlock` (literal TS behaviour:
/// `code.split("\n").map((line) => theme.fg("mdCodeBlock", line))`).
#[test]
fn interactive_rlm_highlight_unknown_language_uses_md_code_block() {
    let theme = &themes()[0];
    let ansi = theme.fg_ansi("mdCodeBlock").unwrap();
    assert_eq!(
        highlight_code(theme, "a\nb", Some("klingon")),
        [format!("{ansi}a\x1b[39m"), format!("{ansi}b\x1b[39m")]
    );
    assert_eq!(highlight_code(theme, "a", None), [format!("{ansi}a\x1b[39m")]);
    // Aliases resolve like `hljs.getLanguage`.
    assert_eq!(
        highlight_code(theme, "None", Some("py")),
        highlight_code(theme, "None", Some("python"))
    );
}
