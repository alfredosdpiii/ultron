//! Differential parity of the Markdown renderer against packages/tui (TypeScript): the fixture holds marked's tokens
//! for each document and the TS component's lines per width and configuration. Fixture:
//! fixtures/markdown-parity.json, produced by `node crates/tui/fixtures/generate-markdown.mjs`.

use serde_json::Value;
use tui::components::Component;
use tui::markdown::{
    DefaultTextStyle, ListItem, Markdown, MarkdownOptions, MarkdownTheme, Token, trim_partial_closing_fences,
};
use tui::select::StyleFn;

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/markdown-parity.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
}

fn st(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn list(v: &Value) -> Vec<Token> {
    v.as_array().map(|a| a.iter().map(token).collect()).unwrap_or_default()
}

fn token(t: &Value) -> Token {
    let raw = st(&t["raw"]);
    match t["type"].as_str().unwrap() {
        "space" => Token::Space,
        "heading" => Token::Heading {
            depth: t["depth"].as_u64().unwrap() as usize,
            tokens: list(&t["tokens"]),
        },
        "paragraph" => Token::Paragraph {
            tokens: list(&t["tokens"]),
        },
        "text" => Token::Text {
            text: st(&t["text"]),
            tokens: t["tokens"].as_array().map(|_| list(&t["tokens"])),
        },
        "code" => Token::Code {
            raw,
            lang: t["lang"].as_str().map(str::to_string),
            text: st(&t["text"]),
        },
        "list" => Token::List {
            ordered: t["ordered"].as_bool().unwrap(),
            start: t["start"].as_i64(),
            loose: t["loose"].as_bool().unwrap(),
            items: t["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| ListItem {
                    raw: st(&i["raw"]),
                    task: i["task"].as_bool().unwrap_or(false),
                    checked: i["checked"].as_bool().unwrap_or(false),
                    tokens: list(&i["tokens"]),
                })
                .collect(),
        },
        "table" => Token::Table {
            raw,
            header: t["header"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| list(&c["tokens"]))
                .collect(),
            rows: t["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_array().unwrap().iter().map(|c| list(&c["tokens"])).collect())
                .collect(),
        },
        "blockquote" => Token::Blockquote {
            tokens: list(&t["tokens"]),
        },
        "hr" => Token::Hr,
        "html" => Token::Html { raw },
        "latexBlock" => Token::LatexBlock { raw },
        "latex" => Token::Latex { raw },
        "escape" => Token::Escape {
            raw,
            text: st(&t["text"]),
        },
        "strong" => Token::Strong {
            tokens: list(&t["tokens"]),
        },
        "em" => Token::Em {
            tokens: list(&t["tokens"]),
        },
        "codespan" => Token::Codespan { text: st(&t["text"]) },
        "link" => Token::Link {
            href: st(&t["href"]),
            text: st(&t["text"]),
            tokens: list(&t["tokens"]),
        },
        "br" => Token::Br,
        "del" => Token::Del {
            tokens: list(&t["tokens"]),
        },
        other => Token::Other {
            kind: other.to_string(),
            text: t["text"].as_str().map(str::to_string),
        },
    }
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

#[test]
fn tui_markdown_renders_marked_tokens_like_ts() {
    let f = fixture();
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
            tokens: list(&case["tokens"]),
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
    assert!(
        failures.is_empty(),
        "{} of {total} differ; first: {}",
        failures.len(),
        failures[..failures.len().min(2)].join("\n")
    );
}

/// Ported from markdown.ts trimPartialClosingFences (pi issue 5825): a streamed partial closing fence is not code.
#[test]
fn tui_markdown_trims_partial_closing_fences() {
    let code = |raw: &str, text: &str| Token::Code {
        raw: raw.into(),
        lang: None,
        text: text.into(),
    };
    let mut tokens = vec![code("```\nlet a\n``", "let a\n``")];
    trim_partial_closing_fences(&mut tokens);
    assert_eq!(tokens, [code("```\nlet a\n``", "let a")]);
    let mut tokens = vec![code("~~~~\nx\n~~~", "x\n~~~")];
    trim_partial_closing_fences(&mut tokens);
    assert_eq!(tokens, [code("~~~~\nx\n~~~", "x")]);
    // A full-length last line, a mixed last line, or an indented-code token stays.
    for raw in ["```\nx\n```", "```\nx\n`~", "    x\n``"] {
        let mut tokens = vec![code(raw, "x\n``")];
        trim_partial_closing_fences(&mut tokens);
        assert_eq!(tokens, [code(raw, "x\n``")]);
    }
    let mut tokens = vec![Token::Blockquote {
        tokens: vec![code("```\ny\n`", "y\n`")],
    }];
    trim_partial_closing_fences(&mut tokens);
    assert_eq!(
        tokens,
        [Token::Blockquote {
            tokens: vec![code("```\ny\n`", "y")]
        }]
    );
}
