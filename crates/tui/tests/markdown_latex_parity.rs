//! Differential parity of LaTeX rendering inside the Markdown component against packages/tui markdown.ts
//! (renderLatex on and off). Fixture: fixtures/markdown-latex-parity.json
//! (`node crates/tui/fixtures/generate-markdown-latex.mjs`).

use serde_json::Value;
use tui::components::Component;
use tui::markdown::{DefaultTextStyle, Markdown, MarkdownOptions, MarkdownTheme};
use tui::marked::markdown_tokens;
use tui::select::StyleFn;

fn sty(open: u32, close: u32) -> StyleFn {
    Box::new(move |s| format!("\x1b[{open}m{s}\x1b[{close}m"))
}

fn theme() -> MarkdownTheme {
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
        highlight_code: None,
        code_block_indent: None,
    }
}

#[test]
fn tui_markdown_renders_latex_like_ts() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/markdown-latex-parity.json");
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
        let doc = case["doc"].as_str().unwrap();
        let mut md = Markdown {
            tokens: markdown_tokens(doc),
            padding_x: c["px"].as_u64().unwrap() as usize,
            padding_y: c["py"].as_u64().unwrap() as usize,
            theme: theme(),
            style: c["style"].as_bool().unwrap().then(|| DefaultTextStyle {
                color: Some(sty(31, 39)),
                bg_color: None,
                bold: true,
                italic: false,
                strikethrough: false,
                underline: false,
            }),
            options: MarkdownOptions {
                raw_latex: !c["renderLatex"].as_bool().unwrap(),
                ..Default::default()
            },
        };
        for (w, want) in widths.iter().zip(case["outputs"].as_array().unwrap()) {
            total += 1;
            let got = md.render(*w);
            let want: Vec<String> = want
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            if got != want {
                failures.push(format!(
                    "{doc:?} config {} width {w}\n  want {want:?}\n  got  {got:?}",
                    case["config"]
                ));
            }
        }
    }
    assert!(total > 0);
    assert!(
        failures.is_empty(),
        "{} of {total} differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
