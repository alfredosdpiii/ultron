//! Differential parity of the Markdown lexer (tui::marked, a port of marked's GFM lexer with markdown.ts's
//! extensions) against packages/tui: each document is lexed from source in Rust, rendered, and compared to the TS
//! component's lines per width and configuration. Fixtures: fixtures/markdown-parity.json
//! (`node crates/tui/fixtures/generate-markdown.mjs`) and fixtures/marked-parity.json
//! (`node crates/tui/fixtures/generate-marked.mjs`).

use serde_json::Value;
use tui::components::Component;
use tui::markdown::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};
use tui::marked::markdown_tokens;
use tui::select::StyleFn;

fn st(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn sty(open: u32, close: u32) -> StyleFn {
    Box::new(move |s| format!("\x1b[{open}m{s}\x1b[{close}m"))
}

fn theme(c: &Value) -> MarkdownTheme {
    MarkdownTheme {
        heading: sty(36, 39),
        link: sty(34, 39),
        link_url: sty(90, 39),
        code: sty(33, 39),
        code_block: sty(32, 39),
        code_block_border: sty(90, 39),
        quote: sty(37, 39),
        quote_border: sty(90, 39),
        hr: sty(90, 39),
        list_bullet: sty(35, 39),
        bold: sty(1, 22),
        italic: sty(3, 23),
        strikethrough: sty(9, 29),
        underline: sty(4, 24),
        highlight_code: c["highlight"].as_bool().unwrap().then(|| {
            Box::new(|code: &str, lang: Option<&str>| {
                code.split('\n')
                    .enumerate()
                    .map(|(i, l)| format!("<{}:{i}>{l}", lang.unwrap_or("-")))
                    .collect()
            }) as tui::markdown::HighlightFn
        }),
        code_block_indent: c["indent"].as_str().map(str::to_string),
    }
}

fn style(c: &Value) -> Option<DefaultTextStyle> {
    let s = &c["style"];
    (!s.is_null()).then(|| DefaultTextStyle {
        color: s["color"].as_bool().unwrap_or(false).then(|| sty(31, 39)),
        bg_color: s["bg"].as_bool().unwrap_or(false).then(|| sty(44, 49)),
        bold: s["bold"].as_bool().unwrap_or(false),
        italic: s["italic"].as_bool().unwrap_or(false),
        strikethrough: s["strikethrough"].as_bool().unwrap_or(false),
        underline: s["underline"].as_bool().unwrap_or(false),
    })
}

/// Lexes every fixture document from source and compares the rendered lines; returns (failures, total).
fn replay(name: &str) -> (Vec<String>, usize) {
    let path = format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let f: Value = serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json");
    let widths: Vec<usize> = f["widths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_u64().unwrap() as usize)
        .collect();
    let mut failures = Vec::new();
    let mut total = 0;
    for case in f["cases"].as_array().unwrap() {
        let c = &f["configs"][case["config"].as_u64().unwrap() as usize];
        let mut md = Markdown {
            tokens: markdown_tokens(case["doc"].as_str().unwrap()),
            padding_x: c["px"].as_u64().unwrap() as usize,
            padding_y: c["py"].as_u64().unwrap() as usize,
            theme: theme(c),
            style: style(c),
            options: MarkdownOptions {
                preserve_ordered_list_markers: c["options"]["preserveOrderedListMarkers"].as_bool().unwrap_or(false),
                preserve_backslash_escapes: c["options"]["preserveBackslashEscapes"].as_bool().unwrap_or(false),
                hyperlinks: c["hyperlinks"].as_bool().unwrap(),
                raw_latex: true,
            },
        };
        for (w, want) in widths.iter().zip(case["outputs"].as_array().unwrap()) {
            total += 1;
            let got = md.render(*w);
            let want: Vec<String> = want.as_array().unwrap().iter().map(st).collect();
            if got != want {
                failures.push(format!(
                    "doc {:?} config {} width {w}:\n got {got:?}\nwant {want:?}",
                    case["doc"], case["config"]
                ));
            }
        }
    }
    (failures, total)
}

fn check(name: &str) {
    let (failures, total) = replay(name);
    assert!(
        failures.is_empty(),
        "{} of {total} differ; first: {}",
        failures.len(),
        failures[..failures.len().min(3)].join("\n")
    );
}

#[test]
fn tui_marked_lexes_renderer_corpus_like_ts() {
    check("markdown-parity.json");
}

#[test]
fn tui_marked_lexes_gfm_and_latex_corpus_like_ts() {
    check("marked-parity.json");
}

/// A multi-byte character right before a LaTeX opener (`“$PATH”`, `€$5`, `é` then a `$$` block): marked cuts the
/// text token one character after its start, which the byte-offset port took as one byte and panicked mid-character.
/// Expected lines recorded from packages/tui's Markdown (renderLatex: false) at width 40.
#[test]
fn tui_marked_multibyte_char_before_latex_opener_like_ts() {
    let config = serde_json::json!({ "highlight": false, "style": null });
    let cases: [(&str, &[&str]); 4] = [
        (
            "Set “$PATH” and €$5 then $x$.",
            &["Set “$PATH” and €$5 then $x$.           "],
        ),
        (
            "é\n$$\na+b\n$$",
            &[
                "é                                       ",
                "                                        ",
                "$$                                      ",
                "a+b                                     ",
                "$$                                      ",
            ],
        ),
        ("€$x$ ok", &["€$x$ ok                                 "]),
        ("“\\(a\\)” ü$$y$$", &["“\\(a\\)” ü$$y$$                          "]),
    ];
    for (doc, want) in cases {
        let mut md = Markdown {
            tokens: markdown_tokens(doc),
            padding_x: 0,
            padding_y: 0,
            theme: theme(&config),
            style: style(&config),
            options: MarkdownOptions {
                raw_latex: true,
                ..MarkdownOptions::default()
            },
        };
        assert_eq!(md.render(40), want, "{doc:?}");
    }
}

/// marked's right-delimiter rule opens with a branch, anchored where the scan starts, that skips one delimiter between
/// `__` (or `**`) pairs: `*x__y*z__` stays literal although `y*z` alone would close the emphasis. Expected lines recorded
/// from packages/tui's Markdown at width 40.
#[test]
fn tui_marked_em_strong_skip_branch_like_ts() {
    let config = serde_json::json!({ "highlight": false, "style": null });
    let cases = [
        ("*x__y*z__ end", "*x__y*z__ end                           "),
        (
            "_x**y_z** end",
            "_x\x1b[1my_z\x1b[22m end                               ",
        ),
        (
            "a *x__y*z__ *b*",
            "a *x__y*z__ \x1b[3mb\x1b[23m                           ",
        ),
    ];
    for (doc, want) in cases {
        let mut md = Markdown {
            tokens: markdown_tokens(doc),
            padding_x: 0,
            padding_y: 0,
            theme: theme(&config),
            style: style(&config),
            options: MarkdownOptions::default(),
        };
        assert_eq!(md.render(40), [want], "{doc:?}");
    }
}
