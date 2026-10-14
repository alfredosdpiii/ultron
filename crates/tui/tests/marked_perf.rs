//! Lexer and layout cost of the Markdown component (tui::marked + tui::markdown).
//!
//! `tui_marked_lex_work_grows_linearly` guards against quadratic regressions without a clock: it lexes generated
//! documents at 1x and 4x size and compares the lexer's own work counter (`tui::marked::lex_work`: rule calls plus the
//! bytes each can scan), which is the same on every run and every machine. Linear lexing gives a ratio of ~4, a
//! lexer that rescans the rest of the input per token ~16. The wall-clock version of the same check
//! (`tui_marked_lex_time_grows_linearly`) is ignored: a time ratio failed under load.
//!
//! `tui_marked_bench` (ignored) is the timing harness behind the before/after numbers:
//! `cargo test -p tui --release --test marked_perf -- --ignored --nocapture`. `MARKED_BENCH_ONLY=<substring>` and
//! `MARKED_BENCH_SKIP=<a,b>` pick inputs by name; `MARKED_BENCH_DUMP=<dir>` also writes each input's tokens and
//! rendered lines there, to diff two builds' outputs byte for byte.

use std::time::{Duration, Instant};

use tui::components::Component;
use tui::markdown::{Markdown, MarkdownOptions, MarkdownTheme};
use tui::marked::{lex_work, markdown_tokens};

fn sty(open: u32, close: u32) -> tui::select::StyleFn {
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

fn markdown(text: &str) -> Markdown {
    Markdown {
        tokens: markdown_tokens(text),
        padding_x: 1,
        padding_y: 0,
        theme: theme(),
        style: None,
        options: MarkdownOptions {
            hyperlinks: true,
            raw_latex: true,
            ..MarkdownOptions::default()
        },
    }
}

/// One assistant-style section: headings, prose with emphasis, links and code spans, lists, a table, a quote, a code
/// fence, a reference link and a bit of math. `i` varies the text so no two sections are identical.
fn section(i: usize) -> String {
    format!(
        "## Step {i}: update the `parser_{i}` module\n\n\
         The **lexer** reads _each_ line once and the *renderer* wraps it; see [the docs](https://example.com/d/{i}) \
         and <https://example.org/{i}> or www.example.net/p{i}. A ~~stale~~ value of `x_{i}` costs $O(n)$ here, \
         and snake_case_names_{i} must not turn into emphasis.\n\n\
         - first item with **bold {i}** and a [ref link][r{i}]\n\
         - second item\n  continued on a lazy line with `code`\n  - nested item {i}\n    - deeper *item*\n\
         - [x] done task\n\
         - [ ] open task\n\n\
         1. ordered one\n2. ordered two with \\*escaped\\* stars\n3. ordered three\n\n\
         | Name | Value | Note |\n|:-----|------:|:----:|\n| a{i} | {i} | **ok** |\n| b | 2 | `c|d` |\n\n\
         > A quote with *emphasis* and a [link](https://example.com/q{i}).\n> Second quoted line.\n\n\
         ```rust\nfn f_{i}(x: usize) -> usize {{\n    // **not bold** in code\n    x * {i}\n}}\n```\n\n\
         [r{i}]: https://example.com/ref/{i} \"Ref {i}\"\n\n"
    )
}

fn assistant_doc(bytes: usize) -> String {
    let mut s = String::from("# Plan\n\n");
    let mut i = 0;
    while s.len() < bytes {
        s.push_str(&section(i));
        i += 1;
    }
    s
}

/// Inputs that stress the inline scanner and the block rules.
fn pathological() -> Vec<(&'static str, String)> {
    let words = |n: usize| (0..n).map(|i| format!("word{i}")).collect::<Vec<_>>().join(" ");
    vec![
        ("long line 100 KB", words(12_000)),
        ("unclosed * x5000", "*a ".repeat(5_000)),
        ("unclosed _ x5000", "_a ".repeat(5_000)),
        ("** pairs x5000", "**a** ".repeat(5_000)),
        ("mixed *_ x5000", "*_a_* b_*c ".repeat(5_000)),
        ("[ brackets x5000", "[a ".repeat(5_000)),
        ("` backticks x5000", "`a ".repeat(5_000)),
        (
            "nested quotes x40",
            (1..=40).map(|d| format!("{} line {d}\n", "> ".repeat(d))).collect(),
        ),
        (
            "nested lists x40",
            (0..40).map(|d| format!("{}- item {d}\n", "  ".repeat(d))).collect(),
        ),
        ("short lines x20000", "a\n".repeat(20_000)),
    ]
}

fn time<T>(f: impl FnOnce() -> T) -> (Duration, T) {
    let t = Instant::now();
    let v = f();
    (t.elapsed(), v)
}

/// Best of `n` runs of lexing `doc`.
fn best_lex(doc: &str, n: usize) -> Duration {
    (0..n).map(|_| time(|| markdown_tokens(doc)).0).min().unwrap()
}

fn report(name: &str, doc: &str, dump: Option<&str>) {
    let (lex, tokens) = time(|| markdown_tokens(doc));
    let mut md = markdown(doc);
    let (render, lines) = time(|| md.render(80));
    let us_per_byte = lex.as_secs_f64() * 1e6 / doc.len().max(1) as f64;
    println!(
        "{name:<26} {:>9} B  lex {:>10.2?} ({us_per_byte:>7.3} us/B)  render80 {:>10.2?}  {} lines",
        doc.len(),
        lex,
        render,
        lines.len()
    );
    if let Some(dir) = dump {
        let file = name.replace(|c: char| !c.is_ascii_alphanumeric(), "_");
        std::fs::write(format!("{dir}/{file}.tokens"), format!("{tokens:#?}")).unwrap();
        std::fs::write(format!("{dir}/{file}.lines"), lines.join("\n")).unwrap();
    }
}

#[test]
#[ignore = "timing harness; run with --ignored --nocapture"]
fn tui_marked_bench() {
    let dump = std::env::var("MARKED_BENCH_DUMP").ok();
    let changelog = concat!(env!("CARGO_MANIFEST_DIR"), "/../../packages/coding-agent/CHANGELOG.md");
    let changelog = std::fs::read_to_string(changelog).expect("CHANGELOG.md");
    let filter = std::env::var("MARKED_BENCH_ONLY").ok();
    let skip = std::env::var("MARKED_BENCH_SKIP").unwrap_or_default();
    let mut inputs: Vec<(&str, String)> = vec![
        ("changelog", changelog),
        ("assistant 50 KB", assistant_doc(50_000)),
        ("assistant 500 KB", assistant_doc(500_000)),
    ];
    inputs.extend(pathological());
    for (name, doc) in &inputs {
        if filter.as_deref().is_some_and(|f| !name.contains(f))
            || skip.split(',').any(|s| !s.is_empty() && name.contains(s))
        {
            continue;
        }
        report(name, doc, dump.as_deref());
    }
}

/// The lexer's work on `doc` (deterministic: the counter is per thread and the lexer has no randomness).
fn work_of(doc: &str) -> u64 {
    let before = lex_work();
    markdown_tokens(doc);
    lex_work() - before
}

/// Lexing work must grow linearly: 4x the input may cost at most 5x the work (quadratic would be ~16x). The ratio is
/// the same on every run; the bound leaves room only for per-document constants.
fn assert_linear_work(name: &str, make: impl Fn(usize) -> String, unit: usize) {
    let (small, large) = (make(unit), make(unit * 4));
    let (s, l) = (work_of(&small), work_of(&large));
    assert_eq!(work_of(&small), s, "{name}: the work count is not deterministic");
    let ratio = l as f64 / s as f64;
    println!("{name:<34} work {s:>9} -> {l:>9} ({ratio:.2}x for 4x the input)");
    assert!(
        ratio < 5.0,
        "{name}: lexing 4x the input did {ratio:.2}x the work ({s} -> {l})"
    );
}

/// The same inputs as [`assert_linear_work`], timed: 4x the input may take at most ~6x the time. Each side is the
/// best of several runs, but a load spike still decides the ratio, so this only runs on demand.
fn assert_linear(name: &str, make: impl Fn(usize) -> String, unit: usize) {
    let small = make(unit);
    let large = make(unit * 4);
    // Warm up lazily compiled rules before timing.
    markdown_tokens(&small);
    let (s, l) = (best_lex(&small, 5), best_lex(&large, 5));
    let ratio = l.as_secs_f64() / s.as_secs_f64().max(1e-9);
    assert!(
        ratio < 6.0,
        "{name}: lexing 4x the input took {ratio:.1}x the time ({s:?} -> {l:?})"
    );
}

/// Every input of the timing check, plus the pathological ones of the bench, by work count.
#[test]
fn tui_marked_lex_work_grows_linearly() {
    assert_linear_work("assistant document", assistant_doc, 40_000);
    assert_linear_work("one long paragraph", |n| "word *em* _u_ `c` ".repeat(n / 18), 40_000);
    assert_linear_work("many short paragraphs", |n| "para line\n\n".repeat(n / 11), 40_000);
    assert_linear_work("unclosed delimiters", |n| "*a _b **c ".repeat(n / 10), 10_000);
    assert_linear_work("nested delimiters", |n| "*_a_* b_*c ".repeat(n / 11), 10_000);
    assert_linear_work("unclosed brackets and backticks", |n| "[a `b <c ".repeat(n / 9), 10_000);
    assert_linear_work("long line", |n| "word ".repeat(n / 5), 40_000);
    assert_linear_work("** pairs", |n| "**a** ".repeat(n / 6), 10_000);
    assert_linear_work(
        "nested quotes",
        |n| (1..=n / 400).map(|d| format!("> line {d}\n")).collect(),
        4_000,
    );
    assert_linear_work("short lines", |n| "a\n".repeat(n / 2), 20_000);
    assert_linear_work("inline math", |n| "x $a+b$ and \\(c\\) y ".repeat(n / 22), 10_000);
    assert_linear_work("unclosed math", |n| "$a ".repeat(n / 3), 10_000);
}

#[test]
#[ignore = "wall clock: load decides a time ratio; tui_marked_lex_work_grows_linearly is the gate"]
fn tui_marked_lex_time_grows_linearly() {
    assert_linear("assistant document", assistant_doc, 40_000);
    assert_linear("one long paragraph", |n| "word *em* _u_ `c` ".repeat(n / 18), 40_000);
    assert_linear("many short paragraphs", |n| "para line\n\n".repeat(n / 11), 40_000);
    assert_linear("unclosed delimiters", |n| "*a _b **c ".repeat(n / 10), 10_000);
    assert_linear("nested delimiters", |n| "*_a_* b_*c ".repeat(n / 11), 10_000);
    assert_linear("unclosed brackets and backticks", |n| "[a `b <c ".repeat(n / 9), 10_000);
}
