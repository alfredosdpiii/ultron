//! Ported from omp crates/pi-natives/src/highlight.rs tests (literal category markers), plus the theme mapping
//! and the never-blocking grammar warm-up.

use std::time::Instant;

use tui::highlight::*;

fn colors() -> HighlightColors {
    let c = |s: &str| s.to_string();
    HighlightColors {
        comment: c("<c>"),
        keyword: c("<k>"),
        function: c("<f>"),
        variable: c("<v>"),
        string: c("<s>"),
        number: c("<n>"),
        r#type: c("<t>"),
        operator: c("<o>"),
        punctuation: c("<p>"),
        inserted: Some(c("<+>")),
        deleted: Some(c("<->")),
    }
}

#[test]
fn tui_highlight_stream_chunks_match_whole_text() {
    let code = "def f():\n\t\"\"\"doc\n\tstring\"\"\"\n\treturn 1\n";
    let whole = highlight_code(code, Some("python"), &colors());
    let mut s = HighlightStream::new(Some("python"), colors());
    assert!(s.supported());
    let chunked: String = ["def f():\n", "\t\"\"\"doc\n\tstring\"\"\"\n", "\treturn 1\n"]
        .iter()
        .map(|c| s.push(c))
        .collect();
    assert_eq!(chunked, whole);
    let mut none = HighlightStream::new(Some("no-such-lang"), colors());
    assert!(!none.supported());
    assert_eq!(none.push("x = 1\n"), "x = 1\n");
}

#[test]
fn tui_highlight_maps_scopes_to_categories() {
    let out = highlight_code(
        "def greet(name):\n    return \"hi\" + 42  # done\n",
        Some("py"),
        &colors(),
    );
    for marker in [
        "<k>def\x1b[39m",
        "<f>greet\x1b[39m",
        "<s>hi\x1b[39m",
        "<n>42\x1b[39m",
        "<c> done",
        "<k>return",
    ] {
        assert!(out.contains(marker), "{marker:?} missing in {out:?}");
    }
    let rust = highlight_code("struct Foo;\nfn main() { let x: u32 = 1; }\n", Some("rs"), &colors());
    for marker in [
        "<t>Foo\x1b[39m",
        "<k>fn\x1b[39m",
        "<f>main\x1b[39m",
        "<k>u32\x1b[39m",
        "<n>1\x1b[39m",
        "<k>let",
    ] {
        assert!(rust.contains(marker), "{marker:?} missing in {rust:?}");
    }
    let diff = highlight_code("--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n", Some("diff"), &colors());
    assert!(diff.contains("<+>new"), "{diff:?}");
    assert!(diff.contains("<->old"), "{diff:?}");
    // every colored run closes, so lines are self-contained
    for line in highlight_lines("a = 'x'\nb = 2\n", Some("python"), &colors()) {
        assert_eq!(line.matches('<').count(), line.matches("\x1b[39m").count(), "{line:?}");
    }
    assert_eq!(highlight_code("plain", None, &colors()), "plain");
    assert_eq!(highlight_code("plain", Some("klingon"), &colors()), "plain");
    assert!(supports_language("rust") && supports_language("RS") && supports_language("ts"));
    assert!(!supports_language("klingon"));
}

#[test]
fn tui_highlight_typescript_and_nix_after_warm() {
    // Before warm-up, TypeScript borrows the JavaScript grammar instead of waiting.
    let early = highlight_code("const x = 1;\n", Some("ts"), &colors());
    assert!(early.contains("<k>const") || warmed(), "{early:?}");
    let start = Instant::now();
    warm();
    assert!(warmed());
    eprintln!("warm took {:?}", start.elapsed());
    assert!(supported_languages().contains(&"TypeScript".to_string()));
    assert!(supported_languages().contains(&"Nix".to_string()));

    let code = "function icons(): Record<\"close\" | \"hunk\", string> {\n\treturn {};\n}\nfunction chip(label: string): string {\n\tconst text = ` ${label} `;\n\treturn text;\n}\nconst zzz = 1;\n";
    let out = highlight_code(code, Some("ts"), &colors());
    let last = out.lines().last().unwrap();
    assert!(
        last.contains("<k>const"),
        "trailing code lost keyword highlighting: {last}"
    );
    assert!(last.contains("<n>1"), "trailing code lost number highlighting: {last}");
    assert!(
        !last.contains("<s>const"),
        "string scope leaked past template literal: {last}"
    );

    let nix = highlight_code(
        "{ pkgs ? import <nixpkgs> {} }:\nlet message = \"hello\"; in pkgs.writeText \"msg\" message # greeting\n",
        Some("nix"),
        &colors(),
    );
    assert!(nix.contains("<k>let"), "{nix}");
    assert!(nix.contains("<s>hello"), "{nix}");
    assert!(nix.contains("<c># greeting"), "{nix}");
    let tsx = highlight_code(
        "const C = () => <div className=\"x\">{y}</div>;\n",
        Some("tsx"),
        &colors(),
    );
    assert!(tsx.contains("<k>const"), "{tsx}");
}

#[test]
fn tui_highlight_theme_colors_and_markdown_hook() {
    let json = tui::theme::builtin_theme_json("dark").unwrap();
    let theme = tui::theme::create_theme(&json, tui::theme::ColorMode::Truecolor, None).unwrap();
    let c = HighlightColors::from_theme(&theme);
    assert_eq!(c.keyword, theme.fg_ansi("syntaxKeyword").unwrap());
    assert_eq!(c.inserted, theme.fg_ansi("toolDiffAdded").ok());
    let hook = markdown_highlighter(colors());
    assert_eq!(hook("x = 1\ny", Some("python")).len(), 2);
    assert!(hook("x = 1\ny", Some("python"))[0].contains("<n>1\x1b[39m"));
}
