//! `highlightCode` (packages/coding-agent/src/modes/interactive/theme/theme.ts) for the `rlm` cell renderer: the
//! highlight.js port in [`tui::hljs`], themed with the TS `buildCliHighlightTheme` mapping and split into lines.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use tui::hljs::Token;
use tui::theme::Theme;

use crate::components::{bold, fg, italic, underline};
use crate::rlm_cell::Highlighter;

/// `buildCliHighlightTheme`: the style for each highlight.js scope that has one.
fn scope_style(scope: &str) -> Option<&'static str> {
    Some(match scope {
        "keyword" | "name" => "syntaxKeyword",
        "built_in" | "class" | "type" => "syntaxType",
        "literal" | "number" => "syntaxNumber",
        "regexp" | "string" => "syntaxString",
        "comment" | "doctag" => "syntaxComment",
        "meta" => "muted",
        "function" | "title" => "syntaxFunction",
        "tag" | "punctuation" => "syntaxPunctuation",
        "attr" | "variable" | "params" => "syntaxVariable",
        "operator" => "syntaxOperator",
        "emphasis" => "italic",
        "strong" => "bold",
        "link" => "underline",
        "addition" => "toolDiffAdded",
        "deletion" => "toolDiffRemoved",
        _ => return None,
    })
}

/// `highlightCode(code, lang)`: one styled string per line. A language highlight.js does not know is shown in
/// `mdCodeBlock`; code highlight.js gives up on comes back plain.
pub fn highlight_code(theme: &Theme, code: &str, lang: Option<&str>) -> Vec<String> {
    let Some(lang) = lang.filter(|l| tui::hljs::supports_language(l)) else {
        return code.split('\n').map(|line| fg(theme, "mdCodeBlock", line)).collect();
    };
    themed(theme, code, tui::hljs::highlight(code, lang).as_deref())
}

/// The token list in the theme's colors, split into lines (`None`: the plain code).
fn themed(theme: &Theme, code: &str, tokens: Option<&[Token]>) -> Vec<String> {
    let Some(tokens) = tokens else {
        return code.split('\n').map(str::to_string).collect();
    };
    let apply = |style: &str, text: &str| match style {
        "italic" => italic(text),
        "bold" => bold(text),
        "underline" => underline(text),
        color => fg(theme, color, text),
    };
    let formatter = |scope: &str| scope_style(scope).map(str::to_string);
    tui::hljs::render(tokens, &formatter, &apply)
        .split('\n')
        .map(str::to_string)
        .collect()
}

/// Cells whose tokens are kept: the TS renderer highlights a cell once per change of its code (`state.highlighted`),
/// and a frame repaints the same cells again.
const CACHED_CELLS: usize = 64;

/// The `rlm` cell highlighter: `highlightCode(code, "python")` with the theme's current colors. A cell's tokens are
/// computed once per code and themed on every call, so a theme change shows at once.
pub fn rlm_highlighter(theme: &Rc<Theme>) -> Highlighter {
    let theme = Rc::clone(theme);
    let cache: RefCell<VecDeque<(String, Option<Vec<Token>>)>> = RefCell::default();
    Rc::new(move |code: &str| {
        let mut cache = cache.borrow_mut();
        let at = match cache.iter().position(|(cached, _)| cached == code) {
            Some(at) => at,
            None => {
                if cache.len() == CACHED_CELLS {
                    cache.pop_front();
                }
                cache.push_back((code.to_string(), tui::hljs::highlight(code, "python")));
                cache.len() - 1
            }
        };
        themed(&theme, code, cache[at].1.as_deref())
    })
}
