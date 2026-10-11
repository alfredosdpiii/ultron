//! Lexer for the Markdown component: a port of the marked v18.0.11 GFM lexer (block and inline rules, MIT,
//! Copyright (c) 2018-2026 MarkedJS, (c) 2011-2018 Christopher Jeffrey) configured the way
//! packages/tui/src/components/markdown.ts configures it: strict `~~` strikethrough and the `latexBlock` / `latex`
//! extensions. It produces the token tree `crate::markdown` renders. Offsets are UTF-8 bytes where marked uses
//! UTF-16 units; masks keep byte lengths so the two agree on every slice.

use std::collections::HashMap;

use fancy_regex::{Captures, Regex};

use crate::markdown::{ListItem, Token, trim_partial_closing_fences};

type Caps<'t> = Captures<'t, str>;

fn re(s: &str) -> Regex {
    Regex::new(s).expect("marked rule")
}

/// marked's GFM rules (`Lexer.rules.block.gfm` / `inline.gfm`), with JS-only syntax rewritten: `\w` is ASCII and
/// `\b_` is a lookbehind, as in JavaScript.
mod rules {
    use super::re;
    use fancy_regex::Regex;
    use std::sync::LazyLock;
    pub static NEWLINE: LazyLock<Regex> = LazyLock::new(|| re(r##"^(?:[ \t]*(?:\n|$))+"##));
    pub static CODE: LazyLock<Regex> =
        LazyLock::new(|| re(r##"^((?: {4}| {0,3}\t)[^\n]+(?:\n(?:[ \t]*(?:\n|$))*)?)+"##));
    pub static FENCES: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^ {0,3}(`{3,}(?=[^`\n]*(?:\n|$))|~{3,})([^\n]*)(?:\n|$)(?:|([\s\S]*?)(?:\n|$))(?: {0,3}\1[~`]* *(?=\n|$)|$)"##,
        )
    });
    pub static HR: LazyLock<Regex> =
        LazyLock::new(|| re(r##"^ {0,3}((?:-[\t ]*){3,}|(?:_[ \t]*){3,}|(?:\*[ \t]*){3,})(?:\n+|$)"##));
    pub static HEADING: LazyLock<Regex> = LazyLock::new(|| re(r##"^ {0,3}(#{1,6})(?=\s|$)(.*)(?:\n+|$)"##));
    pub static BLOCKQUOTE: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^( {0,3}> ?(([^\n]+(?:\n(?! {0,3}((?:-[\t ]*){3,}|(?:_[ \t]*){3,}|(?:\*[ \t]*){3,})(?:\n+|$)| {0,3}#{1,6}(?:\s|$)| {0,3}>| {0,3}(?:`{3,}(?=[^`\n]*(?:\n|$))|~~~)[^\n]*(?:\n|$)| {0,3}(?:[*+-]|\d{1,9}[.)])(?:[ \t]|\n|$)|</?(?:address|article|aside|base|basefont|blockquote|body|caption|center|col|colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|footer|form|frame|frameset|h[1-6]|head|header|hr|html|iframe|legend|li|link|main|menu|menuitem|meta|nav|noframes|ol|optgroup|option|p|param|search|section|summary|table|tbody|td|tfoot|th|thead|title|tr|track|ul)(?: +|\n|/?>)|<(?:script|pre|style|textarea|!--)|[ \t]+\n)[^\n]+)*)|[^\n]*)(?:\n|$))+"##,
        )
    });
    pub static LIST: LazyLock<Regex> =
        LazyLock::new(|| re(r##"^( {0,3}(?:[*+-]|\d{1,9}[.)]))([ \t][^\n]*?)?(?:\n|$)"##));
    pub static HTML: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"(?i)^ {0,3}(?:<(script|pre|style|textarea)[\s>][\s\S]*?(?:</\1>[^\n]*\n*|$)|<!--(?:-?>|[\s\S]*?(?:-->|$))[^\n]*(\n+|$)|<\?[\s\S]*?(?:\?>[^\n]*\n*|$)|<![A-Z][\s\S]*?(?:>[^\n]*\n*|$)|<!\[CDATA\[[\s\S]*?(?:\]\]>[^\n]*\n*|$)|</?(address|article|aside|base|basefont|blockquote|body|caption|center|col|colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|footer|form|frame|frameset|h[1-6]|head|header|hr|html|iframe|legend|li|link|main|menu|menuitem|meta|nav|noframes|ol|optgroup|option|p|param|search|section|summary|table|tbody|td|tfoot|th|thead|title|tr|track|ul)(?: +|\n|/?>)[\s\S]*?(?:(?:\n[ 	]*)+\n|$)|<(?!script|pre|style|textarea)([a-z][A-Za-z0-9_-]*)(?: +[a-zA-Z:_][A-Za-z0-9_.:-]*(?: *= *"[^"\n]*"| *= *'[^'\n]*'| *= *[^\s"'=<>`]+)?)*? */?>(?=[ \t]*(?:\n|$))[\s\S]*?(?:(?:\n[ 	]*)+\n|$)|</(?!script|pre|style|textarea)[a-z][A-Za-z0-9_-]*\s*>(?=[ \t]*(?:\n|$))[\s\S]*?(?:(?:\n[ 	]*)+\n|$))"##,
        )
    });
    pub static DEF: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^ {0,3}\[((?!\s*\])(?:\\[\s\S]|[^\[\]\\])+)\]: *(?:\n[ \t]*)?([^<\s][^\s]*|<.*?>)(?:(?: +(?:\n[ \t]*)?| *\n[ \t]*)((?:"(?:\\"?|[^"\\])*"|'[^'\n]*(?:\n[^'\n]+)*\n?'|\([^()]*\))))? *(?:\n+|$)"##,
        )
    });
    pub static TABLE: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^ *([^\n ].*)\n {0,3}((?:\| *)?:?-+:? *(?:\| *:?-+:? *)*(?:\| *)?)(?:\n((?:(?! *\n| {0,3}((?:-[\t ]*){3,}|(?:_[ \t]*){3,}|(?:\*[ \t]*){3,})(?:\n+|$)| {0,3}#{1,6}(?:\s|$)| {0,3}>|(?: {4}| {0,3}	)[^\n]| {0,3}(?:`{3,}(?=[^`\n]*(?:\n|$))|~~~)[^\n]*(?:\n|$)| {0,3}(?:[*+-]|1[.)])[ \t]|</?(?:address|article|aside|base|basefont|blockquote|body|caption|center|col|colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|footer|form|frame|frameset|h[1-6]|head|header|hr|html|iframe|legend|li|link|main|menu|menuitem|meta|nav|noframes|ol|optgroup|option|p|param|search|section|summary|table|tbody|td|tfoot|th|thead|title|tr|track|ul)(?: +|\n|/?>)|<(?:script|pre|style|textarea|!--)).*(?:\n|$))*)\n*|$)"##,
        )
    });
    pub static LHEADING: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^(?! {0,3}(?:[*+-]|\d{1,9}[.)]) |(?: {4}| {0,3}\t)| {0,3}(?:`{3,}|~{3,})| {0,3}>| {0,3}#{1,6}(?:\s|$)| {0,3}<[^\n>]+>\n| {0,3}\|?(?:[:\- ]*\|)+[\:\- ]*\n)((?:.|\n(?!\s*?\n| {0,3}(?:[*+-]|\d{1,9}[.)]) |(?: {4}| {0,3}\t)| {0,3}(?:`{3,}|~{3,})| {0,3}>| {0,3}#{1,6}(?:\s|$)| {0,3}<[^\n>]+>\n| {0,3}\|?(?:[:\- ]*\|)+[\:\- ]*\n))+?)\n {0,3}(=+|-+) *(?:\n+|$)"##,
        )
    });
    pub static PARAGRAPH: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^([^\n]+(?:\n(?! {0,3}((?:-[\t ]*){3,}|(?:_[ \t]*){3,}|(?:\*[ \t]*){3,})(?:\n+|$)| {0,3}#{1,6}(?:\s|$)| {0,3}>| {0,3}(?:`{3,}(?=[^`\n]*(?:\n|$))|~~~)[^\n]*(?:\n|$)| {0,3}(?:[*+-]|1[.)])[ \t]+[^ \t\n]|</?(?:address|article|aside|base|basefont|blockquote|body|caption|center|col|colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|footer|form|frame|frameset|h[1-6]|head|header|hr|html|iframe|legend|li|link|main|menu|menuitem|meta|nav|noframes|ol|optgroup|option|p|param|search|section|summary|table|tbody|td|tfoot|th|thead|title|tr|track|ul)(?: +|\n|/?>)|<(?:script|pre|style|textarea|!--)| *([^\n ].*)\n {0,3}((?:\| *)?:?-+:? *(?:\| *:?-+:? *)*(?:\| *)?)(?:\n((?:(?! *\n| {0,3}((?:-[\t ]*){3,}|(?:_[ \t]*){3,}|(?:\*[ \t]*){3,})(?:\n+|$)| {0,3}#{1,6}(?:\s|$)| {0,3}>|(?: {4}| {0,3}	)[^\n]| {0,3}(?:`{3,}(?=[^`\n]*(?:\n|$))|~~~)[^\n]*(?:\n|$)| {0,3}(?:[*+-]|1[.)])[ \t]|</?(?:address|article|aside|base|basefont|blockquote|body|caption|center|col|colgroup|dd|details|dialog|dir|div|dl|dt|fieldset|figcaption|figure|footer|form|frame|frameset|h[1-6]|head|header|hr|html|iframe|legend|li|link|main|menu|menuitem|meta|nav|noframes|ol|optgroup|option|p|param|search|section|summary|table|tbody|td|tfoot|th|thead|title|tr|track|ul)(?: +|\n|/?>)|<(?:script|pre|style|textarea|!--)).*(?:\n|$))*)\n*|$)|[ \t]+\n)[^\n]+)*)"##,
        )
    });
    pub static TEXT: LazyLock<Regex> = LazyLock::new(|| re(r##"^[^\n]+"##));
    pub static ESCAPE: LazyLock<Regex> = LazyLock::new(|| re(r##"^\\([!"#$%&'()*+,\-./:;<=>?@\[\]\\^_`{|}~])"##));
    pub static TAG: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^<!--(?:-?>|[\s\S]*?-->)|^</[a-zA-Z][A-Za-z0-9_:-]*\s*>|^<[a-zA-Z][A-Za-z0-9_-]*(?:\s+[a-zA-Z:_][A-Za-z0-9_.:-]*(?:\s*=\s*"[^"]*"|\s*=\s*'[^']*'|\s*=\s*[^\s"'=<>`]+)?)*?\s*/?>|^<\?[\s\S]*?\?>|^<![a-zA-Z]+\s[\s\S]*?>|^<!\[CDATA\[[\s\S]*?\]\]>"##,
        )
    });
    pub static LINK: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^!?\[((?:\[(?:\\[\s\S]|[^\[\]\\])*\]|\\[\s\S]|`+(?!`)[^`]*?`+(?!`)|``+(?=\])|[^\[\]\\`])*?)\]\(\s*(<(?:\\.|[^\n<>\\])+>|[^ \t\n\x00-\x1f]+|(?=\)))(?:(?:[ \t]+(?:\n[ \t]*)?|\n[ \t]*)("(?:\\"?|[^"\\])*"|'(?:\\'?|[^'\\])*'|\((?:\\\)?|[^)\\])*\)))?\s*\)"##,
        )
    });
    pub static REFLINK: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^!?\[((?:\[(?:\\[\s\S]|[^\[\]\\])*\]|\\[\s\S]|`+(?!`)[^`]*?`+(?!`)|``+(?=\])|[^\[\]\\`])*?)\]\[((?!\s*\])(?:\\[\s\S]|[^\[\]\\])+)\]"##,
        )
    });
    pub static NOLINK: LazyLock<Regex> =
        LazyLock::new(|| re(r##"^!?\[((?!\s*\])(?:\\[\s\S]|[^\[\]\\])+)\](?:\[\])?"##));
    pub static REFLINK_SEARCH: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"!?\[((?:\[(?:\\[\s\S]|[^\[\]\\])*\]|\\[\s\S]|`+(?!`)[^`]*?`+(?!`)|``+(?=\])|[^\[\]\\`])*?)\]\[((?!\s*\])(?:\\[\s\S]|[^\[\]\\])+)\]|!?\[((?!\s*\])(?:\\[\s\S]|[^\[\]\\])+)\](?:\[\])?(?!\()"##,
        )
    });
    pub static BLOCK_SKIP: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"\[(?:[^\[\]`]|(?<a>`+)[^`]+\k<a>(?!`))*?\]\((?:\\[\s\S]|[^\\\(\)]|\((?:\\[\s\S]|[^\\\(\)])*\))*\)|(?<!`)()(?<b>`+)[^`]+\k<b>(?!`)|<(?! )[^<>]*?>"##,
        )
    });
    pub static ANY_PUNCT: LazyLock<Regex> = LazyLock::new(|| re(r##"\\([\p{P}\p{S}])"##));
    pub static EM_L: LazyLock<Regex> = LazyLock::new(|| {
        re(r##"^(?:\*+(?:((?!\*)(?!~)[\p{P}\p{S}])|([^\s*]))?)|^_+(?:((?!_)(?!~)[\p{P}\p{S}])|([^\s_]))?"##)
    });
    pub static EM_R_AST: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^[^_*]*?__[^_*]*?\*[^_*]*?(?=__)|[^*]+(?=[^*])|(?!\*)(?!~)[\p{P}\p{S}](\*+)(?=[\s]|$)|(?:[^\s\p{P}\p{S}]|~)(\*+)(?!\*)(?=(?!~)[\s\p{P}\p{S}]|$)|(?!\*)(?!~)[\s\p{P}\p{S}](\*+)(?=(?:[^\s\p{P}\p{S}]|~))|[\s](\*+)(?!\*)(?=(?!~)[\p{P}\p{S}])|(?!\*)(?!~)[\p{P}\p{S}](\*+)(?!\*)(?=(?!~)[\p{P}\p{S}])|(?:[^\s\p{P}\p{S}]|~)(\*+)(?=(?:[^\s\p{P}\p{S}]|~))"##,
        )
    });
    pub static EM_R_UND: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^[^_*]*?\*\*[^_*]*?_[^_*]*?(?=\*\*)|[^_]+(?=[^_])|(?!_)[\p{P}\p{S}](_+)(?=[\s]|$)|[^\s\p{P}\p{S}](_+)(?!_)(?=[\s\p{P}\p{S}]|$)|(?!_)[\s\p{P}\p{S}](_+)(?=[^\s\p{P}\p{S}])|[\s](_+)(?!_)(?=[\p{P}\p{S}])|(?!_)[\p{P}\p{S}](_+)(?!_)(?=[\p{P}\p{S}])"##,
        )
    });
    pub static CODESPAN: LazyLock<Regex> = LazyLock::new(|| re(r##"^(`+)([^`]|[^`][\s\S]*?[^`])\1(?!`)"##));
    pub static BR: LazyLock<Regex> = LazyLock::new(|| re(r##"^( {2,}|\\)\n(?!\s*$)"##));
    pub static AUTOLINK: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^<([a-zA-Z][a-zA-Z0-9+.-]{1,31}:[^\s\x00-\x1f<>]*|[a-zA-Z0-9.!#$%&'*+/=?_`{|}~-]+(@)[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?(?:\.[a-zA-Z0-9](?:[a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?)+(?![-_]))>"##,
        )
    });
    pub static URL: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^((?:[hH][tT][tT][pP][sS]?|[fF][tT][pP])://|www\.)(?:[a-zA-Z0-9\-]+\.?)+[^\s<]*|^[A-Za-z0-9._+-]+(@)[a-zA-Z0-9-_]+(?:\.[a-zA-Z0-9-_]*[a-zA-Z0-9])+(?![-_])"##,
        )
    });
    pub static BACKPEDAL: LazyLock<Regex> =
        LazyLock::new(|| re(r##"(?:[^?!.,:;*_'"~()&]+|\([^)]*\)|&(?![a-zA-Z0-9]+;$)|[?!.,:;*_'"~)]+(?!$))+"##));
    pub static INLINE_TEXT: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^(`+|~+|[^`~])(?:(?=[`~])|(?= {2,}\n)|(?=[a-zA-Z0-9.!#$%&'*+/=?_`{\|}~-]+@)|[\s\S]*?(?:(?=[\\<!\[`*~_]|(?<![A-Za-z0-9_])_|[hH][tT][tT][pP][sS]?|[fF][tT][pP]://|www\.|$)|[^ ](?= {2,}\n)|[^a-zA-Z0-9.!#$%&'*+/=?_`{\|}~-](?=[a-zA-Z0-9.!#$%&'*+/=?_`{\|}~-]+@)))"##,
        )
    });
    pub static PUNCT: LazyLock<Regex> = LazyLock::new(|| re(r##"^((?![*_])[\s\p{P}\p{S}])"##));
    pub static BULLET_DEPTH: LazyLock<Regex> = LazyLock::new(|| re(r"^\[[ xX]\] +\S"));
    pub static TASK: LazyLock<Regex> = LazyLock::new(|| re(r"^\[[ xX]\] +"));
    pub static CHECKBOX: LazyLock<Regex> = LazyLock::new(|| re(r"\[[ xX]\]"));
    pub static BQ_START: LazyLock<Regex> = LazyLock::new(|| re(r"^ {0,3}>"));
    pub static BQ_SETEXT: LazyLock<Regex> = LazyLock::new(|| re(r"\n {0,3}((?:=+|-+) *)(?=\n|$)"));
    pub static BQ_PREFIX: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^ {0,3}>[ \t]?"));
    pub static DOUBLE_BLANK: LazyLock<Regex> = LazyLock::new(|| re(r"\n[ \t]*\n[ \t]*$"));
    pub static ANY_LINE: LazyLock<Regex> = LazyLock::new(|| re(r"\n.*\n"));
    pub static INDENT_COMP: LazyLock<Regex> = LazyLock::new(|| re(r"^(\s+)(?:```)"));
    pub static CODE_INDENT: LazyLock<Regex> = LazyLock::new(|| re(r"(?m)^(?: {1,4}| {0,3}\t)"));
    pub static STRICT_DEL: LazyLock<Regex> =
        LazyLock::new(|| re(r"^(~~)(?=[^\s~])((?:\\.|[^\\])*?(?:\\.|[^\s~\\]))\1(?=[^~]|$)"));
    pub static LATEX_BLOCK_START: LazyLock<Regex> = LazyLock::new(|| re(r"(?:^|\n) {0,3}(?:\$\$|\\\[)"));
    pub static LATEX_DOLLAR: LazyLock<Regex> =
        LazyLock::new(|| re(r"^ {0,3}\$\$[ \t]*(?:\n)?([\s\S]*?)\$\$[ \t]*(?:\n|$)"));
    pub static LATEX_BRACKET: LazyLock<Regex> =
        LazyLock::new(|| re(r"^ {0,3}\\\[[ \t]*(?:\n)?([\s\S]*?)\\\][ \t]*(?:\n|$)"));
    pub static LATEX_PENDING_BRACKET: LazyLock<Regex> = LazyLock::new(|| re(r"^ {0,3}\\\[[ \t]*(?:\n)?([\s\S]*)$"));
    pub static LATEX_PENDING_DOLLAR: LazyLock<Regex> = LazyLock::new(|| re(r"^ {0,3}\$\$[ \t]*(?:\n)?([\s\S]*)$"));
    pub static PENDING_MATH: LazyLock<Regex> = LazyLock::new(|| re(r"\\[A-Za-z]+|[_^=+*/<>()\[\]|±≤≥≠≈∈→⇒∞∫∑√-]"));
    pub static ENV_LIKE: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Z_][A-Z0-9_]*(?:[^A-Za-z0-9_\s])?$"));
    pub static IDENT_START: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z_][A-Za-z0-9_]*"));
    pub static ALNUM: LazyLock<Regex> = LazyLock::new(|| re(r"[\p{L}\p{N}]"));
}

fn exec<'t>(r: &Regex, s: &'t str) -> Option<Caps<'t>> {
    r.captures(s).ok().flatten()
}

fn test(r: &Regex, s: &str) -> bool {
    r.is_match(s).unwrap_or(false)
}

fn group<'t>(c: &Caps<'t>, i: usize) -> Option<&'t str> {
    c.get(i).map(|m| m.as_str())
}

fn g(c: &Caps, i: usize) -> String {
    group(c, i).unwrap_or("").to_string()
}

fn replace(r: &Regex, s: &str, with: &str) -> String {
    r.replace_all(s, with).into_owned()
}

/// `s.slice(i)` that tolerates out-of-range and mid-character indices.
fn from(s: &str, mut i: usize) -> &str {
    if i >= s.len() {
        return "";
    }
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

/// marked `rtrim(str, c)`.
fn rtrim(s: &str, c: char) -> &str {
    s.trim_end_matches(c)
}

/// marked `trimTrailingBlankLines`.
fn trim_trailing_blank_lines(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let mut t = lines.len() as isize - 1;
    while t >= 0 && lines[t as usize].chars().all(|c| c == ' ' || c == '\t') {
        t -= 1;
    }
    if lines.len() as isize - t <= 2 {
        s.to_string()
    } else {
        lines[..(t + 1) as usize].join("\n")
    }
}

/// marked `expandTabs` (tab stops of 4 from column `start`).
fn expand_tabs(s: &str, start: usize) -> String {
    let mut col = start;
    let mut out = String::new();
    for c in s.chars() {
        if c == '\t' {
            let n = 4 - col % 4;
            out.push_str(&" ".repeat(n));
            col += n;
        } else {
            out.push(c);
            col += 1;
        }
    }
    out
}

fn first_non_space(s: &str) -> Option<usize> {
    s.find(|c| c != ' ')
}

fn split_cells(row: &str, count: Option<usize>) -> Vec<String> {
    let mut t = String::new();
    let b = row.as_bytes();
    for (i, c) in row.char_indices() {
        if c == '|' {
            let escaped = b[..i].iter().rev().take_while(|&&x| x == b'\\').count() % 2 == 1;
            t.push_str(if escaped { "|" } else { " |" });
        } else {
            t.push(c);
        }
    }
    let mut cells: Vec<String> = t.split(" |").map(str::to_string).collect();
    if cells[0].trim().is_empty() {
        cells.remove(0);
    }
    if cells.last().is_some_and(|c| c.trim().is_empty()) {
        cells.pop();
    }
    if let Some(n) = count {
        cells.resize(n, String::new());
    }
    cells.iter().map(|c| c.trim().replace("\\|", "|")).collect()
}

/// marked `findClosingBracket` for `()`: index of the unmatched `)`, -1 when absent, -2 when unbalanced.
fn find_closing_paren(s: &str) -> isize {
    if !s.contains(')') {
        return -1;
    }
    let b = s.as_bytes();
    let mut level = 0;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'(' => level += 1,
            b')' => {
                level -= 1;
                if level < 0 {
                    return i as isize;
                }
            }
            _ => {}
        }
        i += 1;
    }
    if level > 0 { -2 } else { -1 }
}

#[derive(Clone, Debug, Default)]
struct Block {
    kind: &'static str,
    raw: String,
    text: String,
    depth: usize,
    lang: Option<String>,
    tokens: Vec<Block>,
    ordered: bool,
    start: Option<i64>,
    loose: bool,
    items: Vec<Item>,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    /// Loose task item: a checkbox token precedes the paragraph's inline tokens.
    checkbox: Option<bool>,
}

#[derive(Clone, Debug, Default)]
struct Item {
    raw: String,
    task: bool,
    checked: bool,
    text: String,
    tokens: Vec<Block>,
}

fn block(kind: &'static str, raw: &str, text: &str) -> Block {
    Block {
        kind,
        raw: raw.to_string(),
        text: text.to_string(),
        ..Block::default()
    }
}

struct Lexer {
    links: HashMap<String, (String, String)>,
    in_link: bool,
    in_raw_block: bool,
    link_emitted: bool,
    top: bool,
}

fn tokenize_block_latex(src: &str) -> Option<Block> {
    for r in [&*rules::LATEX_DOLLAR, &*rules::LATEX_BRACKET] {
        if let Some(c) = exec(r, src)
            && !g(&c, 1).is_empty()
        {
            return Some(block("latexBlock", &g(&c, 0), g(&c, 1).trim()));
        }
    }
    if let Some(c) = exec(&rules::LATEX_PENDING_BRACKET, src) {
        return Some(block("latexBlock", &g(&c, 0), &g(&c, 1)));
    }
    if let Some(c) = exec(&rules::LATEX_PENDING_DOLLAR, src)
        && !g(&c, 1).is_empty()
        && test(&rules::PENDING_MATH, &g(&c, 1))
    {
        return Some(block("latexBlock", &g(&c, 0), &g(&c, 1)));
    }
    None
}

fn is_escaped(s: &str, index: usize) -> bool {
    s.as_bytes()[..index].iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 1
}

fn find_closing_delimiter(s: &str, closing: &str, start: usize) -> Option<usize> {
    let mut index = s.get(start..)?.find(closing).map(|i| i + start);
    while let Some(i) = index
        && is_escaped(s, i)
    {
        index = s
            .get(i + closing.len()..)
            .and_then(|r| r.find(closing))
            .map(|j| j + i + closing.len());
    }
    index
}

/// `tokenizeInlineLatex` from markdown.ts; returns the token's raw source.
fn tokenize_inline_latex(src: &str) -> Option<String> {
    let (opening, closing) = if src.starts_with("$$") {
        ("$$", "$$")
    } else if src.starts_with("\\(") {
        ("\\(", "\\)")
    } else if src.starts_with("\\[") {
        ("\\[", "\\]")
    } else if src.starts_with('$') && !src[1..].starts_with(char::is_whitespace) {
        ("$", "$")
    } else {
        return None;
    };
    let close = find_closing_delimiter(src, closing, opening.len());
    if let Some(ci) = close
        && opening == "$"
    {
        let inner = &src[1..ci];
        let after = &src[ci + 1..];
        if inner.ends_with(char::is_whitespace)
            || after.starts_with(|c: char| c.is_ascii_digit())
            || (test(&rules::ENV_LIKE, inner) && test(&rules::IDENT_START, after))
            || inner.contains('`')
        {
            return None;
        }
    }
    let Some(ci) = close else {
        let pending = &src[opening.len()..];
        return (opening.starts_with('\\') || test(&rules::PENDING_MATH, pending)).then(|| src.to_string());
    };
    let text = &src[opening.len()..ci];
    if text.is_empty() || text.contains('\n') {
        return None;
    }
    Some(src[..ci + closing.len()].to_string())
}

fn inline_latex_start(s: &str) -> Option<usize> {
    [s.find('$'), s.find("\\("), s.find("\\[")].into_iter().flatten().min()
}

impl Lexer {
    fn block_tokens(&mut self, src: &str, tokens: &mut Vec<Block>, mut last_paragraph_clipped: bool) {
        let mut src = src.to_string();
        let mut shortest = usize::MAX;
        while !src.is_empty() {
            if src.len() < shortest {
                shortest = src.len();
            } else {
                break;
            }
            if let Some(t) = tokenize_block_latex(&src) {
                src = src[t.raw.len()..].to_string();
                tokens.push(t);
                continue;
            }
            if let Some(c) = exec(&rules::NEWLINE, &src)
                && !c[0].is_empty()
            {
                let raw = g(&c, 0);
                src = src[raw.len()..].to_string();
                match tokens.last_mut() {
                    Some(last) if raw.len() == 1 => last.raw.push('\n'),
                    _ => tokens.push(block("space", &raw, "")),
                }
                continue;
            }
            if let Some(c) = exec(&rules::CODE, &src) {
                let raw = trim_trailing_blank_lines(&c[0]);
                let text = replace(&rules::CODE_INDENT, &raw, "");
                src = src[raw.len()..].to_string();
                match tokens.last_mut() {
                    Some(last) if last.kind == "paragraph" || last.kind == "text" => {
                        if !last.raw.ends_with('\n') {
                            last.raw.push('\n');
                        }
                        last.raw += &raw;
                        last.text = format!("{}\n{text}", last.text);
                    }
                    _ => tokens.push(Block {
                        kind: "code",
                        raw,
                        text,
                        ..Block::default()
                    }),
                }
                continue;
            }
            if let Some(c) = exec(&rules::FENCES, &src) {
                let raw = g(&c, 0);
                let text = indent_code_compensation(&raw, group(&c, 3).unwrap_or(""));
                let lang = group(&c, 2).map(|l| replace(&rules::ANY_PUNCT, l.trim(), "$1"));
                src = src[raw.len()..].to_string();
                tokens.push(Block {
                    kind: "code",
                    raw,
                    text,
                    lang,
                    ..Block::default()
                });
                continue;
            }
            if let Some(c) = exec(&rules::HEADING, &src) {
                let mut text = g(&c, 2).trim().to_string();
                if text.ends_with('#') {
                    let trimmed = rtrim(&text, '#');
                    if trimmed.is_empty() || trimmed.ends_with(' ') {
                        text = trimmed.trim().to_string();
                    }
                }
                tokens.push(Block {
                    depth: c[1].len(),
                    ..block("heading", rtrim(&c[0], '\n'), &text)
                });
                let n = c[0].len();
                drop(c);
                src = src[n..].to_string();
                continue;
            }
            if let Some(c) = exec(&rules::HR, &src) {
                tokens.push(block("hr", rtrim(&c[0], '\n'), ""));
                let n = c[0].len();
                drop(c);
                src = src[n..].to_string();
                continue;
            }
            if let Some(t) = self.blockquote(&src) {
                src = src[t.raw.len()..].to_string();
                tokens.push(t);
                continue;
            }
            if let Some(t) = self.list(&src) {
                src = src[t.raw.len()..].to_string();
                tokens.push(t);
                continue;
            }
            if let Some(c) = exec(&rules::HTML, &src) {
                let raw = trim_trailing_blank_lines(&c[0]);
                src = src[raw.len()..].to_string();
                tokens.push(block("html", &raw, &raw));
                continue;
            }
            if let Some(c) = exec(&rules::DEF, &src) {
                let raw = rtrim(&c[0], '\n').to_string();
                let consumed = c[0].len();
                match tokens.last_mut() {
                    Some(last) if last.kind == "paragraph" || last.kind == "text" => {
                        if !last.raw.ends_with('\n') {
                            last.raw.push('\n');
                        }
                        last.raw += &raw;
                        last.text = format!("{}\n{raw}", last.text);
                    }
                    _ => {
                        let tag = c[1].to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
                        let href = group(&c, 2).map_or(String::new(), |h| {
                            let h = h.strip_prefix('<').and_then(|h| h.strip_suffix('>')).unwrap_or(h);
                            replace(&rules::ANY_PUNCT, h, "$1")
                        });
                        let title = group(&c, 3)
                            .map_or(String::new(), |t| replace(&rules::ANY_PUNCT, &t[1..t.len() - 1], "$1"));
                        if let std::collections::hash_map::Entry::Vacant(e) = self.links.entry(tag) {
                            e.insert((href, title));
                            tokens.push(block("def", &raw, ""));
                        }
                    }
                }
                drop(c);
                src = src[consumed..].to_string();
                continue;
            }
            if let Some(t) = table(&src) {
                src = src[t.raw.len()..].to_string();
                tokens.push(t);
                continue;
            }
            if let Some(c) = exec(&rules::LHEADING, &src) {
                tokens.push(Block {
                    depth: if c[2].starts_with('=') { 1 } else { 2 },
                    ..block("heading", rtrim(&c[0], '\n'), c[1].trim())
                });
                let n = c[0].len();
                drop(c);
                src = src[n..].to_string();
                continue;
            }
            let mut cut = src.as_str();
            if let Some(c) = exec(&rules::LATEX_BLOCK_START, from(&src, 1)) {
                let m = c.get(0).unwrap();
                let at = m.start() + usize::from(m.as_str().starts_with('\n'));
                cut = &src[..at + 1];
            }
            if self.top
                && let Some(c) = exec(&rules::PARAGRAPH, cut)
            {
                let raw = g(&c, 0);
                let text = c[1].strip_suffix('\n').unwrap_or(&c[1]).to_string();
                let clipped = cut.len() != src.len();
                match tokens.last_mut() {
                    Some(last) if last_paragraph_clipped && last.kind == "paragraph" => {
                        if !last.raw.ends_with('\n') {
                            last.raw.push('\n');
                        }
                        last.raw += &raw;
                        last.text = format!("{}\n{text}", last.text);
                    }
                    _ => tokens.push(block("paragraph", &raw, &text)),
                }
                last_paragraph_clipped = clipped;
                src = src[raw.len()..].to_string();
                continue;
            }
            if let Some(c) = exec(&rules::TEXT, &src) {
                let raw = g(&c, 0);
                src = src[raw.len()..].to_string();
                match tokens.last_mut() {
                    Some(last) if last.kind == "text" => {
                        if !last.raw.ends_with('\n') {
                            last.raw.push('\n');
                        }
                        last.raw += &raw;
                        last.text = format!("{}\n{raw}", last.text);
                    }
                    _ => tokens.push(block("text", &raw, &raw)),
                }
                continue;
            }
            break;
        }
        self.top = true;
    }

    fn blockquote(&mut self, src: &str) -> Option<Block> {
        let c = exec(&rules::BLOCKQUOTE, src)?;
        let mut lines: Vec<String> = rtrim(&c[0], '\n').split('\n').map(str::to_string).collect();
        let (mut raw, mut text) = (String::new(), String::new());
        let mut tokens: Vec<Block> = Vec::new();
        while !lines.is_empty() {
            let mut in_quote = false;
            let mut current = Vec::new();
            let mut a = 0;
            while a < lines.len() {
                if test(&rules::BQ_START, &lines[a]) {
                    current.push(lines[a].clone());
                    in_quote = true;
                } else if !in_quote {
                    current.push(lines[a].clone());
                } else {
                    break;
                }
                a += 1;
            }
            lines.drain(..a);
            let p = current.join("\n");
            let inner = replace(&rules::BQ_PREFIX, &replace(&rules::BQ_SETEXT, &p, "\n    $1"), "");
            raw = if raw.is_empty() { p } else { format!("{raw}\n{p}") };
            text = if text.is_empty() {
                inner.clone()
            } else {
                format!("{text}\n{inner}")
            };
            let top = self.top;
            self.top = true;
            self.block_tokens(&inner, &mut tokens, true);
            self.top = top;
            if lines.is_empty() {
                break;
            }
            let last = tokens.last().cloned();
            match last.as_ref().map(|t| t.kind) {
                Some("code") => break,
                Some("blockquote") => {
                    let last = last.unwrap();
                    let rest = lines.join("\n");
                    let w = format!("{}\n{}", last.raw, replace(&rules::BQ_PREFIX, &rest, ""));
                    let quote = self.blockquote(&w)?;
                    text = format!("{}{}", &text[..text.len() - last.text.len()], quote.text);
                    *tokens.last_mut().unwrap() = quote;
                    raw = format!("{raw}\n{rest}");
                    break;
                }
                Some("list") => {
                    let last = last.unwrap();
                    let joined = format!("{}\n{}", last.raw, lines.join("\n"));
                    let list = self.list(&joined)?;
                    raw = format!("{}{}", &raw[..raw.len() - last.raw.len()], list.raw);
                    text = format!("{}{}", &text[..text.len() - last.raw.len()], list.raw);
                    lines = joined[list.raw.len()..].split('\n').map(str::to_string).collect();
                    *tokens.last_mut().unwrap() = list;
                    continue;
                }
                _ => {}
            }
        }
        Some(Block {
            tokens,
            ..block("blockquote", &raw, &text)
        })
    }

    fn list(&mut self, src: &str) -> Option<Block> {
        let c = exec(&rules::LIST, src)?;
        let bull = c[1].trim().to_string();
        let ordered = bull.len() > 1;
        let mut list = Block {
            kind: "list",
            ordered,
            start: if ordered {
                bull[..bull.len() - 1].parse().ok()
            } else {
                None
            },
            ..Block::default()
        };
        let bull_re = if ordered {
            format!(r"\d{{1,9}}\{}", &bull[bull.len() - 1..])
        } else {
            format!(r"\{bull}")
        };
        let item_re = re(&format!(r"^( {{0,3}}{bull_re})((?:[\t ][^\n]*)?(?:\n|$))"));
        let mut src = src.to_string();
        let mut ends_with_blank = false;
        while !src.is_empty() {
            let Some(c) = exec(&item_re, &src) else { break };
            if test(&rules::HR, &src) {
                break;
            }
            let mut end_early = false;
            let mut raw = g(&c, 0);
            let marker_len = c[1].len();
            let mut line = expand_tabs(c[2].split('\n').next().unwrap_or(""), marker_len);
            src = src[raw.len()..].to_string();
            let mut next_line = src.split('\n').next().unwrap_or("").to_string();
            let mut blank = line.trim().is_empty();
            let mut item_contents = String::new();
            let indent;
            if blank {
                indent = marker_len + 1;
            } else {
                let i = first_non_space(&line).map_or(-1, |i| i as isize);
                let i = if i > 4 { 1 } else { i };
                item_contents = from(&line, i.max(0) as usize).to_string();
                indent = (i + marker_len as isize).max(0) as usize;
                if i < 0 {
                    item_contents = line.clone();
                }
            }
            if blank && next_line.chars().all(|c| c == ' ' || c == '\t') {
                raw += &next_line;
                raw.push('\n');
                src = from(&src, next_line.len() + 1).to_string();
                end_early = true;
            }
            if !end_early {
                let lim = indent.saturating_sub(1).min(3);
                let next_bullet = re(&format!(
                    r"^ {{0,{lim}}}(?:[*+-]|\d{{1,9}}[.)])((?:[ \t][^\n]*)?(?:\n|$))"
                ));
                let hr = re(&format!(
                    r"^ {{0,{lim}}}((?:- *){{3,}}|(?:_ *){{3,}}|(?:\* *){{3,}})(?:\n+|$)"
                ));
                let fences = re(&format!(r"^ {{0,{lim}}}(?:```|~~~)"));
                let heading = re(&format!(r"^ {{0,{lim}}}#"));
                let html = re(&format!(r"(?i)^ {{0,{lim}}}<(?:[a-z].*>|!--)"));
                let quote = re(&format!(r"^ {{0,{lim}}}>"));
                while !src.is_empty() {
                    let raw_line = src.split('\n').next().unwrap_or("").to_string();
                    next_line = raw_line.clone();
                    let expanded = next_line.replace('\t', "    ");
                    if [&fences, &heading, &html, &quote, &next_bullet, &hr]
                        .iter()
                        .any(|r| test(r, &next_line))
                    {
                        break;
                    }
                    if first_non_space(&expanded).is_some_and(|i| i >= indent) || next_line.trim().is_empty() {
                        item_contents = format!("{item_contents}\n{}", from(&expanded, indent));
                    } else {
                        let line_x = line.replace('\t', "    ");
                        if blank
                            || first_non_space(&line_x).is_some_and(|i| i >= 4)
                            || test(&fences, &line)
                            || test(&heading, &line)
                            || test(&hr, &line)
                        {
                            break;
                        }
                        item_contents = format!("{item_contents}\n{next_line}");
                    }
                    blank = next_line.trim().is_empty();
                    raw += &raw_line;
                    raw.push('\n');
                    src = from(&src, raw_line.len() + 1).to_string();
                    line = from(&expanded, indent).to_string();
                }
            }
            if !list.loose {
                if ends_with_blank {
                    list.loose = true;
                } else if test(&rules::DOUBLE_BLANK, &raw) {
                    ends_with_blank = true;
                }
            }
            list.raw += &raw;
            list.items.push(Item {
                task: test(&rules::BULLET_DEPTH, &item_contents),
                raw,
                text: item_contents,
                ..Item::default()
            });
        }
        let last = list.items.last_mut()?;
        last.raw = last.raw.trim_end().to_string();
        last.text = last.text.trim_end().to_string();
        list.raw = list.raw.trim_end().to_string();
        for i in 0..list.items.len() {
            self.top = false;
            let mut tokens = Vec::new();
            let text = list.items[i].text.clone();
            self.block_tokens(&text, &mut tokens, false);
            if !list.loose {
                list.loose = tokens
                    .iter()
                    .any(|t| t.kind == "space" && test(&rules::ANY_LINE, &t.raw));
            }
            list.items[i].tokens = tokens;
        }
        let loose = list.loose;
        for item in &mut list.items {
            let first_kind = item.tokens.first().map(|t| t.kind);
            if item.task && matches!(first_kind, Some("text" | "paragraph")) {
                item.text = replace(&rules::TASK, &item.text, "");
                let first = &mut item.tokens[0];
                first.raw = replace(&rules::TASK, &first.raw, "");
                first.text = replace(&rules::TASK, &first.text, "");
                if let Some(c) = exec(&rules::CHECKBOX, &item.raw) {
                    let checked = &c[0] != "[ ]";
                    item.checked = checked;
                    if loose {
                        item.tokens[0].checkbox = Some(checked);
                    } else {
                        item.tokens.insert(0, block("checkbox", "", ""));
                    }
                }
            } else {
                item.task = false;
            }
        }
        if loose {
            for item in &mut list.items {
                for t in &mut item.tokens {
                    if t.kind == "text" {
                        t.kind = "paragraph";
                    }
                }
            }
        }
        Some(list)
    }

    fn convert_all(&mut self, blocks: Vec<Block>) -> Vec<Token> {
        blocks.into_iter().map(|b| self.convert(b)).collect()
    }

    fn convert(&mut self, b: Block) -> Token {
        match b.kind {
            "space" => Token::Space,
            "code" => Token::Code {
                raw: b.raw,
                lang: b.lang,
                text: b.text,
            },
            "heading" => Token::Heading {
                depth: b.depth,
                tokens: self.inline_tokens(&b.text),
            },
            "hr" => Token::Hr,
            "blockquote" => Token::Blockquote {
                tokens: self.convert_all(b.tokens),
            },
            "list" => Token::List {
                ordered: b.ordered,
                start: b.start,
                loose: b.loose,
                items: b
                    .items
                    .into_iter()
                    .map(|i| ListItem {
                        raw: i.raw,
                        task: i.task,
                        checked: i.checked,
                        tokens: self.convert_all(i.tokens),
                    })
                    .collect(),
            },
            "html" => Token::Html { raw: b.raw },
            "table" => Token::Table {
                raw: b.raw,
                header: b.header.iter().map(|c| self.inline_tokens(c)).collect(),
                rows: b
                    .rows
                    .iter()
                    .map(|r| r.iter().map(|c| self.inline_tokens(c)).collect())
                    .collect(),
            },
            "paragraph" => {
                let mut tokens = Vec::new();
                if b.checkbox.is_some() {
                    tokens.push(checkbox());
                }
                tokens.extend(self.inline_tokens(&b.text));
                Token::Paragraph { tokens }
            }
            "text" => Token::Text {
                tokens: Some(self.inline_tokens(&b.text)),
                text: b.text,
            },
            "latexBlock" => Token::LatexBlock { raw: b.raw },
            "checkbox" => checkbox(),
            kind => Token::Other {
                kind: kind.to_string(),
                text: None,
            },
        }
    }

    fn mask(&self, src: &str) -> String {
        let mut masked = src.to_string();
        if !self.links.is_empty() && masked.contains('[') {
            masked = self.mask_reflinks(&masked);
        }
        masked = rules::ANY_PUNCT
            .replace_all(&masked, |c: &Caps| "+".repeat(c[0].len()))
            .into_owned();
        rules::BLOCK_SKIP
            .replace_all(&masked, |c: &Caps| format!("[{}]", "a".repeat(c[0].len() - 2)))
            .into_owned()
    }

    fn mask_reflinks(&self, s: &str) -> String {
        rules::REFLINK_SEARCH
            .replace_all(s, |c: &Caps| {
                let a = &c[0];
                let p = a.rfind('[').unwrap_or(0);
                if !self.links.contains_key(&a[p + 1..a.len() - 1]) {
                    return a.to_string();
                }
                if p > 1 && !a.starts_with('!') {
                    let inner = &a[1..p - 1];
                    if self.link_in_text(inner) {
                        return format!("[{}][{}]", self.mask_reflinks(inner), "a".repeat(a.len() - p - 2));
                    }
                }
                format!("[{}]", "a".repeat(a.len() - 2))
            })
            .into_owned()
    }

    fn link_in_text(&self, s: &str) -> bool {
        if !s.contains('[') {
            return false;
        }
        for m in rules::BLOCK_SKIP.find_iter(s).flatten() {
            if test(&rules::LINK, m.as_str()) && (m.start() == 0 || s.as_bytes()[m.start() - 1] != b'!') {
                return true;
            }
        }
        for m in rules::REFLINK_SEARCH.find_iter(s).flatten() {
            let t = m.as_str();
            let r = t.rfind('[').unwrap_or(0);
            if !(t.starts_with('!') || !self.links.contains_key(&t[r + 1..t.len() - 1]))
                && !(r > 1 && self.link_in_text(&t[1..r - 1]))
            {
                return true;
            }
        }
        false
    }

    fn inline_tokens(&mut self, src: &str) -> Vec<Token> {
        let masked = self.mask(src);
        let mut tokens: Vec<Token> = Vec::new();
        let mut src = src.to_string();
        let mut keep_prev = false;
        let mut prev_char = String::new();
        let mut shortest = usize::MAX;
        while !src.is_empty() {
            if src.len() < shortest {
                shortest = src.len();
            } else {
                break;
            }
            if !keep_prev {
                prev_char.clear();
            }
            keep_prev = false;
            if let Some(raw) = tokenize_inline_latex(&src) {
                src = src[raw.len()..].to_string();
                tokens.push(Token::Latex { raw });
                continue;
            }
            if let Some(c) = exec(&rules::ESCAPE, &src) {
                tokens.push(Token::Escape {
                    raw: g(&c, 0),
                    text: g(&c, 1),
                });
                let n = c[0].len();
                drop(c);
                src = src[n..].to_string();
                continue;
            }
            if let Some(c) = exec(&rules::TAG, &src) {
                let raw = g(&c, 0);
                let lower = raw.to_ascii_lowercase();
                if !self.in_link && lower.starts_with("<a ") {
                    self.in_link = true;
                } else if self.in_link && lower.starts_with("</a>") {
                    self.in_link = false;
                }
                let pre = |s: &str| {
                    ["pre", "code", "kbd", "script"].iter().any(|t| {
                        s.strip_prefix(t)
                            .is_some_and(|r| r.starts_with(|c: char| c == '>' || c.is_whitespace()))
                    })
                };
                if !self.in_raw_block && lower.strip_prefix('<').is_some_and(pre) {
                    self.in_raw_block = true;
                } else if self.in_raw_block && lower.strip_prefix("</").is_some_and(pre) {
                    self.in_raw_block = false;
                }
                src = src[raw.len()..].to_string();
                tokens.push(Token::Html { raw });
                continue;
            }
            if let Some((len, t)) = self.link(&src) {
                src = src[len..].to_string();
                tokens.push(t);
                continue;
            }
            if let Some((len, t)) = self.reflink(&src) {
                src = src[len..].to_string();
                push_text(&mut tokens, t);
                continue;
            }
            if let Some((len, t)) = self.em_strong(&src, &masked, &prev_char) {
                src = src[len..].to_string();
                tokens.push(t);
                continue;
            }
            if let Some(c) = exec(&rules::CODESPAN, &src) {
                let mut text = c[2].replace('\n', " ");
                if text.contains(|c| c != ' ') && text.starts_with(' ') && text.ends_with(' ') {
                    text = text[1..text.len() - 1].to_string();
                }
                src = src[c[0].len()..].to_string();
                tokens.push(Token::Codespan { text });
                continue;
            }
            if let Some(c) = exec(&rules::BR, &src) {
                src = src[c[0].len()..].to_string();
                tokens.push(Token::Br);
                continue;
            }
            if let Some(c) = exec(&rules::STRICT_DEL, &src) {
                let inner = self.inline_tokens(&c[2]);
                src = src[c[0].len()..].to_string();
                tokens.push(Token::Del { tokens: inner });
                continue;
            }
            if let Some(c) = exec(&rules::AUTOLINK, &src) {
                let text = g(&c, 1);
                let href = if group(&c, 2) == Some("@") {
                    format!("mailto:{text}")
                } else {
                    text.clone()
                };
                src = src[c[0].len()..].to_string();
                tokens.push(link_token(href, text));
                continue;
            }
            if !self.in_link
                && let Some(c) = exec(&rules::URL, &src)
            {
                let (text, href);
                if group(&c, 2) == Some("@") {
                    text = g(&c, 0);
                    href = format!("mailto:{text}");
                } else {
                    let mut cur = g(&c, 0);
                    loop {
                        let prev = cur.clone();
                        cur = exec(&rules::BACKPEDAL, &cur).map_or(String::new(), |m| g(&m, 0));
                        if prev == cur {
                            break;
                        }
                    }
                    text = cur.clone();
                    href = if group(&c, 1) == Some("www.") {
                        format!("http://{cur}")
                    } else {
                        cur
                    };
                }
                src = src[text.len()..].to_string();
                tokens.push(link_token(href, text));
                continue;
            }
            let mut cut = src.as_str();
            if let Some(at) = inline_latex_start(from(&src, 1)) {
                cut = &src[..at + 1];
            }
            if let Some(c) = exec(&rules::INLINE_TEXT, cut) {
                let raw = g(&c, 0);
                src = src[raw.len()..].to_string();
                if !raw.ends_with('_') {
                    prev_char = raw.chars().last().map(String::from).unwrap_or_default();
                }
                keep_prev = true;
                push_text(
                    &mut tokens,
                    Token::Text {
                        text: raw,
                        tokens: None,
                    },
                );
                continue;
            }
            break;
        }
        tokens
    }

    fn link(&mut self, src: &str) -> Option<(usize, Token)> {
        let c = exec(&rules::LINK, src)?;
        let mut raw = g(&c, 0);
        let mut href = g(&c, 2);
        let trimmed = href.trim().to_string();
        if trimmed.starts_with('<') {
            if !trimmed.ends_with('>') {
                return None;
            }
            let body = &trimmed[..trimmed.len() - 1];
            if (trimmed.len() - rtrim(body, '\\').len()).is_multiple_of(2) {
                return None;
            }
        } else {
            let i = find_closing_paren(&href);
            if i == -2 {
                return None;
            }
            if i > -1 {
                let end = if raw.starts_with('!') { 5 } else { 4 } + c[1].len() + i as usize;
                href.truncate(i as usize);
                raw = raw[..end].trim().to_string();
            }
        }
        let mut href = href.trim().to_string();
        if href.starts_with('<') {
            href = href[1..href.len() - 1].to_string();
        }
        let href = replace(&rules::ANY_PUNCT, &href, "$1");
        let label = c[1].to_string();
        self.output_link(raw, label, href)
    }

    fn reflink(&mut self, src: &str) -> Option<(usize, Token)> {
        let c = exec(&rules::REFLINK, src).or_else(|| exec(&rules::NOLINK, src))?;
        let key = group(&c, 2).filter(|s| !s.is_empty()).unwrap_or(&c[1]);
        let key = key.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        let Some((href, _)) = self.links.get(&key).cloned() else {
            let first = c[0][..1].to_string();
            return Some((
                1,
                Token::Text {
                    text: first,
                    tokens: None,
                },
            ));
        };
        self.output_link(g(&c, 0), c[1].to_string(), href)
    }

    /// marked `outputLink`.
    fn output_link(&mut self, raw: String, label: String, href: String) -> Option<(usize, Token)> {
        let text = label.replace("\\[", "[").replace("\\]", "]");
        let image = raw.starts_with('!');
        self.in_link = true;
        let emitted = self.link_emitted;
        let raw_block = self.in_raw_block;
        self.link_emitted = false;
        let tokens = self.inline_tokens(&text);
        let inner_emitted = self.link_emitted;
        self.link_emitted = emitted;
        self.in_link = false;
        if image {
            return Some((
                raw.len(),
                Token::Other {
                    kind: "image".into(),
                    text: Some(text),
                },
            ));
        }
        if inner_emitted {
            self.in_raw_block = raw_block;
            return None;
        }
        self.link_emitted = true;
        Some((raw.len(), Token::Link { href, text, tokens }))
    }

    fn em_strong(&mut self, src: &str, masked: &str, prev_char: &str) -> Option<(usize, Token)> {
        let m = exec(&rules::EM_L, src)?;
        let has = |i| group(&m, i).is_some_and(|s| !s.is_empty());
        if !(has(1) || has(2) || has(3) || has(4)) || (has(4) && test(&rules::ALNUM, prev_char)) {
            return None;
        }
        if !(!(has(1) || has(3)) || prev_char.is_empty() || test(&rules::PUNCT, prev_char)) {
            return None;
        }
        let l_len = m[0].chars().count() - 1;
        let delim = m[0].as_bytes()[0];
        let same_prev = prev_char.as_bytes() == [delim];
        let r_delim = if delim == b'*' {
            &*rules::EM_R_AST
        } else {
            &*rules::EM_R_UND
        };
        let mut left = l_len as isize;
        let mut mid = 0isize;
        let hay = from(masked, masked.len() - src.len() + l_len);
        for c in r_delim.captures_iter(hay).flatten() {
            let Some(d) = (1..=6).find_map(|i| group(&c, i).filter(|s| !s.is_empty())) else {
                continue;
            };
            let mut r_len = d.chars().count() as isize;
            let gi = |i| group(&c, i).is_some_and(|s| !s.is_empty());
            if gi(3) || gi(4) {
                left += r_len;
                continue;
            } else if gi(5) || gi(6) {
                if l_len % 3 != 0 && (l_len as isize + r_len) % 3 == 0 {
                    mid += r_len;
                    continue;
                }
                if same_prev {
                    break;
                }
            }
            left -= r_len;
            if left > 0 {
                continue;
            }
            r_len = r_len.min(r_len + left + mid);
            let first_len = c[0].chars().next().map_or(0, char::len_utf8);
            let m0 = c.get(0).unwrap().start();
            let raw = &src[..l_len + m0 + first_len + r_len as usize];
            if l_len.min(r_len as usize) % 2 == 1 {
                let text = &raw[1..raw.len() - 1];
                return Some((
                    raw.len(),
                    Token::Em {
                        tokens: self.inline_tokens(text),
                    },
                ));
            }
            let text = &raw[2..raw.len() - 2];
            return Some((
                raw.len(),
                Token::Strong {
                    tokens: self.inline_tokens(text),
                },
            ));
        }
        None
    }
}

fn checkbox() -> Token {
    Token::Other {
        kind: "checkbox".into(),
        text: None,
    }
}

fn link_token(href: String, text: String) -> Token {
    Token::Link {
        href,
        tokens: vec![Token::Text {
            text: text.clone(),
            tokens: None,
        }],
        text,
    }
}

fn push_text(tokens: &mut Vec<Token>, t: Token) {
    if let (
        Some(Token::Text {
            text: last,
            tokens: None,
        }),
        Token::Text { text, tokens: None },
    ) = (tokens.last_mut(), &t)
    {
        last.push_str(text);
        return;
    }
    tokens.push(t);
}

/// marked `indentCodeCompensation`.
fn indent_code_compensation(raw: &str, text: &str) -> String {
    let Some(c) = exec(&rules::INDENT_COMP, raw) else {
        return text.to_string();
    };
    let indent = c[1].len();
    text.split('\n')
        .map(|line| {
            let lead = line.len() - line.trim_start().len();
            if lead == 0 {
                line
            } else if lead >= indent {
                &line[indent..]
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn table(src: &str) -> Option<Block> {
    let c = exec(&rules::TABLE, src)?;
    if !c[2].contains([':', '|']) {
        return None;
    }
    let header = split_cells(&c[1], None);
    let align_row = c[2].strip_prefix('|').unwrap_or(&c[2]);
    let align_row = align_row.trim_end_matches(' ');
    let align_row = align_row.strip_suffix('|').map_or(align_row, |r| r);
    let aligns = align_row.split('|').count();
    if header.len() != aligns {
        return None;
    }
    let body = group(&c, 3).unwrap_or("");
    let rows: Vec<&str> = if body.trim().is_empty() {
        Vec::new()
    } else {
        let t = body.trim_end_matches([' ', '\t']);
        t.strip_suffix('\n').unwrap_or(body).split('\n').collect()
    };
    Some(Block {
        kind: "table",
        raw: rtrim(&c[0], '\n').to_string(),
        rows: rows.iter().map(|r| split_cells(r, Some(header.len()))).collect(),
        header,
        ..Block::default()
    })
}

/// Tokens the Markdown component renders for `text`: marked's GFM lexer with markdown.ts's strict strikethrough and
/// LaTeX extensions, after tabs become three spaces and streamed partial closing fences are trimmed. Blank input
/// renders nothing.
pub fn markdown_tokens(text: &str) -> Vec<Token> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut tokens = lex(&text.replace('\t', "   "));
    trim_partial_closing_fences(&mut tokens);
    tokens
}

/// `markdownParser.lexer(src)`.
pub fn lex(src: &str) -> Vec<Token> {
    let src = src.replace("\r\n", "\n").replace('\r', "\n");
    let mut lexer = Lexer {
        links: HashMap::new(),
        in_link: false,
        in_raw_block: false,
        link_emitted: false,
        top: true,
    };
    let mut blocks = Vec::new();
    lexer.block_tokens(&src, &mut blocks, false);
    lexer.convert_all(blocks)
}
