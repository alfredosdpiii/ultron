//! Lexer for the Markdown component: a port of the marked v18.0.11 GFM lexer (block and inline rules, MIT,
//! Copyright (c) 2018-2026 MarkedJS, (c) 2011-2018 Christopher Jeffrey) configured the way
//! packages/tui/src/components/markdown.ts configures it: strict `~~` strikethrough and the `latexBlock` / `latex`
//! extensions. It produces the token tree `crate::markdown` renders. Offsets are UTF-8 bytes where marked uses
//! UTF-16 units; masks keep byte lengths so the two agree on every slice.

use std::collections::HashMap;

use fancy_regex::{Captures, Regex, RegexInput};

use crate::markdown::{ListItem, Token, trim_partial_closing_fences};

type Caps<'t> = Captures<'t, str>;

thread_local! {
    static WORK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Charge `units` of scanning work to this thread's counter (see [`lex_work`]).
fn work(units: usize) {
    WORK.with(|w| w.set(w.get() + units as u64 + 1));
}

/// A deterministic measure of the lexer's work on this thread so far, for complexity tests (no clocks): one unit
/// per rule call plus the bytes it can scan. An anchored rule is charged its match length (the bytes it consumed;
/// a failed one only its call), an unanchored search, replace or iteration the whole haystack it was given (the
/// most it can read), a substring search the bytes up to where it stopped. A lexer that rescans the rest of the
/// document per token shows up here as quadratic growth, whatever the machine load.
pub fn lex_work() -> u64 {
    WORK.with(std::cell::Cell::get)
}

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
    pub static EM_R_AST_SKIP: LazyLock<Regex> = LazyLock::new(|| re(r##"^[^_*]*?__[^_*]*?\*[^_*]*?(?=__)"##));
    pub static EM_R_UND_SKIP: LazyLock<Regex> = LazyLock::new(|| re(r##"^[^_*]*?\*\*[^_*]*?_[^_*]*?(?=\*\*)"##));
    pub static PUNCT_OR_SYMBOL: LazyLock<Regex> = LazyLock::new(|| re(r"[\p{P}\p{S}]"));
    #[cfg(test)]
    pub static EM_R_AST: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r##"^[^_*]*?__[^_*]*?\*[^_*]*?(?=__)|[^*]+(?=[^*])|(?!\*)(?!~)[\p{P}\p{S}](\*+)(?=[\s]|$)|(?:[^\s\p{P}\p{S}]|~)(\*+)(?!\*)(?=(?!~)[\s\p{P}\p{S}]|$)|(?!\*)(?!~)[\s\p{P}\p{S}](\*+)(?=(?:[^\s\p{P}\p{S}]|~))|[\s](\*+)(?!\*)(?=(?!~)[\p{P}\p{S}])|(?!\*)(?!~)[\p{P}\p{S}](\*+)(?!\*)(?=(?!~)[\p{P}\p{S}])|(?:[^\s\p{P}\p{S}]|~)(\*+)(?=(?:[^\s\p{P}\p{S}]|~))"##,
        )
    });
    #[cfg(test)]
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
    #[cfg(test)]
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
    /// `(?:^|\n) {0,3}(?:\$\$|\\\[)` split into its two branches, so the `\n` one can be searched once per document.
    pub static LATEX_BLOCK_AT_START: LazyLock<Regex> = LazyLock::new(|| re(r"^ {0,3}(?:\$\$|\\\[)"));
    pub static LATEX_BLOCK_AFTER_NEWLINE: LazyLock<Regex> = LazyLock::new(|| re(r"\n {0,3}(?:\$\$|\\\[)"));
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

    /// A list's item rule per bullet (`*`, `+`, `-`, `1.`, `1)`), compiled once instead of per list.
    pub static LIST_ITEM: LazyLock<[Regex; 5]> = LazyLock::new(|| {
        [r"\*", r"\+", r"\-", r"\d{1,9}\.", r"\d{1,9}\)"]
            .map(|b| re(&format!(r"^( {{0,3}}{b})((?:[\t ][^\n]*)?(?:\n|$))")))
    });

    /// The lines that end a list item, for an item indent `lim + 1` (`lim` 0..=3).
    pub struct ItemEnds {
        pub next_bullet: Regex,
        pub hr: Regex,
        pub fences: Regex,
        pub heading: Regex,
        pub html: Regex,
        pub quote: Regex,
    }

    /// [`ItemEnds`] per `lim`, compiled once instead of per list item.
    pub static LIST_ITEM_ENDS: LazyLock<[ItemEnds; 4]> = LazyLock::new(|| {
        [0, 1, 2, 3].map(|lim| ItemEnds {
            next_bullet: re(&format!(
                r"^ {{0,{lim}}}(?:[*+-]|\d{{1,9}}[.)])((?:[ \t][^\n]*)?(?:\n|$))"
            )),
            hr: re(&format!(
                r"^ {{0,{lim}}}((?:- *){{3,}}|(?:_ *){{3,}}|(?:\* *){{3,}})(?:\n+|$)"
            )),
            fences: re(&format!(r"^ {{0,{lim}}}(?:```|~~~)")),
            heading: re(&format!(r"^ {{0,{lim}}}#")),
            html: re(&format!(r"(?i)^ {{0,{lim}}}<(?:[a-z].*>|!--)")),
            quote: re(&format!(r"^ {{0,{lim}}}>")),
        })
    });
}

/// The item rule for a list opened by `bull` (`-`, `+`, `*`, or digits and `.`/`)`).
fn item_rule(bull: &str) -> &'static Regex {
    let i = match bull {
        "*" => 0,
        "+" => 1,
        "-" => 2,
        b if b.ends_with('.') => 3,
        _ => 4,
    };
    &rules::LIST_ITEM[i]
}

/// `rule.exec(s)` for a rule whose every branch starts with `^`. The search is anchored at the start: fancy-regex only
/// anchors a pattern on its own when it is a single `^…` sequence without a trailing lookahead, and otherwise retries
/// at every later position of `s`, which made each call cost the length of the unlexed rest of the document.
fn exec<'t>(r: &Regex, s: &'t str) -> Option<Caps<'t>> {
    let caps = r.captures_input(RegexInput::new(s).anchored(true)).ok().flatten();
    work(caps.as_ref().and_then(|c| c.get(0)).map_or(0, |m| m.end()));
    caps
}

/// `rule.test(s)` for a rule whose every branch starts with `^` (see [`exec`]).
fn test(r: &Regex, s: &str) -> bool {
    work(0);
    r.is_match_input(RegexInput::new(s).anchored(true)).unwrap_or(false)
}

/// Unanchored `rule.exec(s)`.
fn search<'t>(r: &Regex, s: &'t str) -> Option<Caps<'t>> {
    work(s.len());
    r.captures(s).ok().flatten()
}

/// Unanchored `rule.test(s)`.
fn contains(r: &Regex, s: &str) -> bool {
    work(s.len());
    r.is_match(s).unwrap_or(false)
}

fn group<'t>(c: &Caps<'t>, i: usize) -> Option<&'t str> {
    c.get(i).map(|m| m.as_str())
}

fn g(c: &Caps, i: usize) -> String {
    group(c, i).unwrap_or("").to_string()
}

fn replace(r: &Regex, s: &str, with: &str) -> String {
    work(s.len());
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
        && contains(&rules::PENDING_MATH, &g(&c, 1))
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
    work(index.unwrap_or(s.len()).saturating_sub(start));
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
        return (opening.starts_with('\\') || contains(&rules::PENDING_MATH, pending)).then(|| src.to_string());
    };
    let text = &src[opening.len()..ci];
    if text.is_empty() || text.contains('\n') {
        return None;
    }
    Some(src[..ci + closing.len()].to_string())
}

/// The next match of a search at or after a moving start offset into `whole`. The lexers only ever drop a prefix of
/// `whole`, so a match found once stays the answer until the start passes it: repeated "find the next X in the rest"
/// scans cost one pass over `whole` instead of one pass per token.
#[derive(Default)]
struct NextAt {
    searched: bool,
    at: Option<usize>,
}

impl NextAt {
    fn get(&mut self, whole: &str, start: usize, find: impl Fn(&str) -> Option<usize>) -> Option<usize> {
        if !self.searched || self.at.is_some_and(|a| a < start) {
            self.searched = true;
            self.at = find(&whole[start..]).map(|i| i + start);
            work(self.at.unwrap_or(whole.len()) - start);
        }
        self.at
    }
}

/// Byte offset of `s.slice(1)` (`from(s, 1)`): the length of the first character.
fn second_char(s: &str) -> usize {
    s.chars().next().map_or(0, char::len_utf8)
}

/// Length of the paragraph cut before a block LaTeX start (`(?:^|\n) {0,3}(?:\$\$|\\\[)` searched in
/// `src.slice(1)`), where `src` is the unlexed suffix of `whole`.
fn latex_block_start(whole: &str, src: &str, next: &mut NextAt) -> Option<usize> {
    let skip = second_char(src);
    let rest = &src[skip..];
    if test(&rules::LATEX_BLOCK_AT_START, rest) {
        return Some(skip);
    }
    let start = whole.len() - rest.len();
    let at = next.get(whole, start, |s| {
        rules::LATEX_BLOCK_AFTER_NEWLINE
            .find(s)
            .ok()
            .flatten()
            .map(|m| m.start())
    })?;
    Some(at - (whole.len() - src.len()) + 1)
}

/// Length of the inline text cut before the next LaTeX opener (`$`, `\(` or `\[`) after the first character of
/// `src`, the unlexed suffix of `whole`.
fn inline_latex_start(whole: &str, src: &str, next: &mut [NextAt; 3]) -> Option<usize> {
    let start = whole.len() - src.len() + second_char(src);
    let [dollar, paren, bracket] = next;
    let found = [
        dollar.get(whole, start, |s| s.find('$')),
        paren.get(whole, start, |s| s.find("\\(")),
        bracket.get(whole, start, |s| s.find("\\[")),
    ];
    found
        .into_iter()
        .flatten()
        .min()
        .map(|at| at - (whole.len() - src.len()))
}

impl Lexer {
    fn block_tokens(&mut self, src: &str, tokens: &mut Vec<Block>, mut last_paragraph_clipped: bool) {
        let whole = src;
        let mut src = src;
        let mut latex_start = NextAt::default();
        let mut shortest = usize::MAX;
        while !src.is_empty() {
            if src.len() < shortest {
                shortest = src.len();
            } else {
                break;
            }
            if let Some(t) = tokenize_block_latex(src) {
                src = &src[t.raw.len()..];
                tokens.push(t);
                continue;
            }
            if let Some(c) = exec(&rules::NEWLINE, src)
                && !c[0].is_empty()
            {
                let raw = g(&c, 0);
                src = &src[raw.len()..];
                match tokens.last_mut() {
                    Some(last) if raw.len() == 1 => last.raw.push('\n'),
                    _ => tokens.push(block("space", &raw, "")),
                }
                continue;
            }
            if let Some(c) = exec(&rules::CODE, src) {
                let raw = trim_trailing_blank_lines(&c[0]);
                let text = replace(&rules::CODE_INDENT, &raw, "");
                src = &src[raw.len()..];
                match tokens.last_mut() {
                    Some(last) if last.kind == "paragraph" || last.kind == "text" => {
                        if !last.raw.ends_with('\n') {
                            last.raw.push('\n');
                        }
                        last.raw += &raw;
                        last.text.push('\n');
                        last.text.push_str(&text);
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
            if let Some(c) = exec(&rules::FENCES, src) {
                let raw = g(&c, 0);
                let text = indent_code_compensation(&raw, group(&c, 3).unwrap_or(""));
                let lang = group(&c, 2).map(|l| replace(&rules::ANY_PUNCT, l.trim(), "$1"));
                src = &src[raw.len()..];
                tokens.push(Block {
                    kind: "code",
                    raw,
                    text,
                    lang,
                    ..Block::default()
                });
                continue;
            }
            if let Some(c) = exec(&rules::HEADING, src) {
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
                src = &src[n..];
                continue;
            }
            if let Some(c) = exec(&rules::HR, src) {
                tokens.push(block("hr", rtrim(&c[0], '\n'), ""));
                let n = c[0].len();
                src = &src[n..];
                continue;
            }
            if let Some(t) = self.blockquote(src) {
                src = &src[t.raw.len()..];
                tokens.push(t);
                continue;
            }
            if let Some(t) = self.list(src) {
                src = &src[t.raw.len()..];
                tokens.push(t);
                continue;
            }
            if let Some(c) = exec(&rules::HTML, src) {
                let raw = trim_trailing_blank_lines(&c[0]);
                src = &src[raw.len()..];
                tokens.push(block("html", &raw, &raw));
                continue;
            }
            if let Some(c) = exec(&rules::DEF, src) {
                let raw = rtrim(&c[0], '\n').to_string();
                let consumed = c[0].len();
                match tokens.last_mut() {
                    Some(last) if last.kind == "paragraph" || last.kind == "text" => {
                        if !last.raw.ends_with('\n') {
                            last.raw.push('\n');
                        }
                        last.raw += &raw;
                        last.text.push('\n');
                        last.text.push_str(&raw);
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
                src = &src[consumed..];
                continue;
            }
            if let Some(t) = table(src) {
                src = &src[t.raw.len()..];
                tokens.push(t);
                continue;
            }
            if let Some(c) = exec(&rules::LHEADING, src) {
                tokens.push(Block {
                    depth: if c[2].starts_with('=') { 1 } else { 2 },
                    ..block("heading", rtrim(&c[0], '\n'), c[1].trim())
                });
                let n = c[0].len();
                src = &src[n..];
                continue;
            }
            let mut cut = src;
            if let Some(at) = latex_block_start(whole, src, &mut latex_start) {
                cut = &src[..at];
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
                        last.text.push('\n');
                        last.text.push_str(&text);
                    }
                    _ => tokens.push(block("paragraph", &raw, &text)),
                }
                last_paragraph_clipped = clipped;
                src = &src[raw.len()..];
                continue;
            }
            if let Some(c) = exec(&rules::TEXT, src) {
                let raw = g(&c, 0);
                src = &src[raw.len()..];
                match tokens.last_mut() {
                    Some(last) if last.kind == "text" => {
                        if !last.raw.ends_with('\n') {
                            last.raw.push('\n');
                        }
                        last.raw += &raw;
                        last.text.push('\n');
                        last.text.push_str(&raw);
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
        let item_re = item_rule(&bull);
        let mut src = src;
        let mut ends_with_blank = false;
        while !src.is_empty() {
            let Some(c) = exec(item_re, src) else { break };
            if test(&rules::HR, src) {
                break;
            }
            let mut end_early = false;
            let mut raw = g(&c, 0);
            let marker_len = c[1].len();
            let mut line = expand_tabs(c[2].split('\n').next().unwrap_or(""), marker_len);
            src = &src[raw.len()..];
            let mut next_line = src.split('\n').next().unwrap_or("");
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
                raw += next_line;
                raw.push('\n');
                src = from(src, next_line.len() + 1);
                end_early = true;
            }
            if !end_early {
                let r = &rules::LIST_ITEM_ENDS[indent.saturating_sub(1).min(3)];
                while !src.is_empty() {
                    let raw_line = src.split('\n').next().unwrap_or("");
                    next_line = raw_line;
                    let expanded = next_line.replace('\t', "    ");
                    if [&r.fences, &r.heading, &r.html, &r.quote, &r.next_bullet, &r.hr]
                        .iter()
                        .any(|r| test(r, next_line))
                    {
                        break;
                    }
                    if first_non_space(&expanded).is_some_and(|i| i >= indent) || next_line.trim().is_empty() {
                        item_contents.push('\n');
                        item_contents.push_str(from(&expanded, indent));
                    } else {
                        let line_x = line.replace('\t', "    ");
                        if blank
                            || first_non_space(&line_x).is_some_and(|i| i >= 4)
                            || test(&r.fences, &line)
                            || test(&r.heading, &line)
                            || test(&r.hr, &line)
                        {
                            break;
                        }
                        item_contents.push('\n');
                        item_contents.push_str(next_line);
                    }
                    blank = next_line.trim().is_empty();
                    raw += raw_line;
                    raw.push('\n');
                    src = from(src, raw_line.len() + 1);
                    line = from(&expanded, indent).to_string();
                }
            }
            if !list.loose {
                if ends_with_blank {
                    list.loose = true;
                } else if contains(&rules::DOUBLE_BLANK, &raw) {
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
                    .any(|t| t.kind == "space" && contains(&rules::ANY_LINE, &t.raw));
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
                if let Some(c) = search(&rules::CHECKBOX, &item.raw) {
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
        work(masked.len());
        masked = rules::ANY_PUNCT
            .replace_all(&masked, |c: &Caps| "+".repeat(c[0].len()))
            .into_owned();
        mask_block_skip(&masked)
    }

    fn mask_reflinks(&self, s: &str) -> String {
        work(s.len());
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
        work(2 * s.len());
        for m in rules::BLOCK_SKIP.find_iter(s).flatten() {
            work(0);
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
        let whole = src;
        let mut src = src;
        let mut latex_start = [NextAt::default(), NextAt::default(), NextAt::default()];
        let mut runs = DelimiterRuns::default();
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
            if let Some(raw) = tokenize_inline_latex(src) {
                src = &src[raw.len()..];
                tokens.push(Token::Latex { raw });
                continue;
            }
            if let Some(c) = exec(&rules::ESCAPE, src) {
                tokens.push(Token::Escape {
                    raw: g(&c, 0),
                    text: g(&c, 1),
                });
                let n = c[0].len();
                src = &src[n..];
                continue;
            }
            if let Some(c) = exec(&rules::TAG, src) {
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
                src = &src[raw.len()..];
                tokens.push(Token::Html { raw });
                continue;
            }
            if let Some((len, t)) = self.link(src) {
                src = &src[len..];
                tokens.push(t);
                continue;
            }
            if let Some((len, t)) = self.reflink(src) {
                src = &src[len..];
                push_text(&mut tokens, t);
                continue;
            }
            if let Some((len, t)) = self.em_strong(src, &masked, &prev_char, &mut runs) {
                src = &src[len..];
                tokens.push(t);
                continue;
            }
            if let Some(c) = exec(&rules::CODESPAN, src) {
                let mut text = c[2].replace('\n', " ");
                if text.contains(|c| c != ' ') && text.starts_with(' ') && text.ends_with(' ') {
                    text = text[1..text.len() - 1].to_string();
                }
                src = &src[c[0].len()..];
                tokens.push(Token::Codespan { text });
                continue;
            }
            if let Some(c) = exec(&rules::BR, src) {
                src = &src[c[0].len()..];
                tokens.push(Token::Br);
                continue;
            }
            if let Some(c) = exec(&rules::STRICT_DEL, src) {
                let inner = self.inline_tokens(&c[2]);
                src = &src[c[0].len()..];
                tokens.push(Token::Del { tokens: inner });
                continue;
            }
            if let Some(c) = exec(&rules::AUTOLINK, src) {
                let text = g(&c, 1);
                let href = if group(&c, 2) == Some("@") {
                    format!("mailto:{text}")
                } else {
                    text.clone()
                };
                src = &src[c[0].len()..];
                tokens.push(link_token(href, text));
                continue;
            }
            if !self.in_link
                && let Some(c) = exec(&rules::URL, src)
            {
                let (text, href);
                if group(&c, 2) == Some("@") {
                    text = g(&c, 0);
                    href = format!("mailto:{text}");
                } else {
                    let mut cur = g(&c, 0);
                    loop {
                        let prev = cur.clone();
                        cur = search(&rules::BACKPEDAL, &cur).map_or(String::new(), |m| g(&m, 0));
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
                src = &src[text.len()..];
                tokens.push(link_token(href, text));
                continue;
            }
            let mut cut = src;
            if let Some(at) = inline_latex_start(whole, src, &mut latex_start) {
                cut = &src[..at];
            }
            if let Some(n) = inline_text_len(cut).inspect(|&n| work(n)) {
                let raw = cut[..n].to_string();
                src = &src[n..];
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

    fn em_strong(
        &mut self,
        src: &str,
        masked: &str,
        prev_char: &str,
        runs: &mut DelimiterRuns,
    ) -> Option<(usize, Token)> {
        let m = exec(&rules::EM_L, src)?;
        let has = |i| group(&m, i).is_some_and(|s| !s.is_empty());
        if !(has(1) || has(2) || has(3) || has(4)) || (has(4) && contains(&rules::ALNUM, prev_char)) {
            return None;
        }
        if !(!(has(1) || has(3)) || prev_char.is_empty() || test(&rules::PUNCT, prev_char)) {
            return None;
        }
        let l_len = m[0].chars().count() - 1;
        let delim = m[0].as_bytes()[0];
        let same_prev = prev_char.as_bytes() == [delim];
        let hay_start = masked.len() - src.len() + l_len;
        let runs = runs.get(masked, delim);
        // marked's emStrongRDelim regex starts with a branch anchored at the search start that skips one delimiter.
        let skip_to = exec(
            if delim == b'*' {
                &rules::EM_R_AST_SKIP
            } else {
                &rules::EM_R_UND_SKIP
            },
            from(masked, hay_start),
        )
        .map_or(0, |c| c[0].len());
        let first = runs.runs.partition_point(|r| r.start < hay_start + skip_to.max(1));
        let (j, left, mid) = runs.close(first, l_len, same_prev)?;
        let run = runs.runs[j];
        let r_len = (run.len as isize).min(run.len as isize + left + mid);
        let raw = &src[..l_len + (run.start - hay_start) + r_len as usize];
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
        Some((
            raw.len(),
            Token::Strong {
                tokens: self.inline_tokens(text),
            },
        ))
    }
}

/// `s.replace(blockSkip, m => `[${"a".repeat(m.length - 2)}]`)`. Every blockSkip match starts at `[`, `` ` `` or `<`,
/// so the rule is tried anchored at those bytes only (with the whole string still visible to its lookbehind) instead
/// of at every position, which the fancy-regex VM does for an unanchored search.
fn mask_block_skip(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let (mut copied, mut i) = (0, 0);
    while let Some(off) = b[i..].iter().position(|c| matches!(c, b'[' | b'`' | b'<')) {
        let at = i + off;
        let m = rules::BLOCK_SKIP
            .find_input(RegexInput::new(s).from_pos(at).anchored(true))
            .ok()
            .flatten();
        work(off + m.as_ref().map_or(0, |m| m.end() - at));
        match m {
            Some(m) => {
                out.push_str(&s[copied..at]);
                out.push('[');
                out.push_str(&"a".repeat(m.end() - at - 2));
                out.push(']');
                copied = m.end();
                i = m.end();
            }
            None => i = at + 1,
        }
    }
    if copied == 0 {
        return s.to_string();
    }
    out.push_str(&s[copied..]);
    out
}

/// `[a-zA-Z0-9.!#$%&'*+/=?_`{|}~-]`: a character of an email local part as marked's inline text rule spells it.
fn is_email_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b".!#$%&'*+/=?_`{|}~-".contains(&c)
}

/// End of the maximal run of bytes satisfying `f` that starts at `i`, remembered per run so repeated lookups inside
/// one run cost one scan of it.
#[derive(Default)]
struct RunEnd {
    start: usize,
    end: usize,
}

impl RunEnd {
    fn get(&mut self, b: &[u8], i: usize, f: impl Fn(u8) -> bool) -> usize {
        if !(self.start..self.end).contains(&i) {
            self.start = i;
            self.end = i + b[i..].iter().take_while(|&&c| f(c)).count();
            work(self.end - i);
        }
        self.end
    }
}

/// Length of marked's inline text token at the start of `s` (the `inlineText` rule, `None` for empty `s`):
///
/// ```text
/// ^(`+|~+|[^`~])(?:(?=[`~])|(?= {2,}\n)|(?=[a-zA-Z0-9.!#$%&'*+/=?_`{\|}~-]+@)|[\s\S]*?(?:(?=[\\<!\[`*~_]
///   |(?<![A-Za-z0-9_])_|[hH][tT][tT][pP][sS]?|[fF][tT][pP]://|www\.|$)|[^ ](?= {2,}\n)
///   |[^a-zA-Z0-9.!#$%&'*+/=?_`{\|}~-](?=[a-zA-Z0-9.!#$%&'*+/=?_`{\|}~-]+@)))
/// ```
///
/// The rule is a lazy scan with lookaheads at every character; run by fancy-regex's backtracking VM it was the
/// largest single cost of lexing. This scanner tries the same alternatives in the same order at each position.
fn inline_text_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let first = *b.first()?;
    let p = if first == b'`' || first == b'~' {
        b.iter().take_while(|&&c| c == first).count()
    } else {
        second_char(s)
    };
    let mut spaces = RunEnd::default();
    let mut email = RunEnd::default();
    // `(?= {2,}\n)` and `(?=[email]+@)` at byte `i`.
    let mut break_ahead = |i: usize| {
        b.get(i) == Some(&b' ') && {
            let e = spaces.get(b, i, |c| c == b' ');
            e - i >= 2 && b.get(e) == Some(&b'\n')
        }
    };
    let mut email_ahead =
        |i: usize| b.get(i).copied().is_some_and(is_email_char) && b.get(email.get(b, i, is_email_char)) == Some(&b'@');
    if b.get(p).is_some_and(|c| *c == b'`' || *c == b'~') || break_ahead(p) || email_ahead(p) {
        return Some(p);
    }
    let mut q = p;
    loop {
        let rest = &b[q..];
        let starts = |pat: &[u8]| rest.len() >= pat.len() && rest[..pat.len()].eq_ignore_ascii_case(pat);
        if rest.is_empty()
            || b"\\<![`*~_".contains(&rest[0])
            || starts(b"http")
            || starts(b"ftp://")
            || rest.starts_with(b"www.")
        {
            return Some(q);
        }
        let next = q + second_char(&s[q..]);
        if (rest[0] != b' ' && break_ahead(next)) || (!is_email_char(rest[0]) && email_ahead(next)) {
            return Some(next);
        }
        q = next;
    }
}

/// One maximal run of `*` or `_` in a masked inline source, with the emStrongRDelim branch (capture group 1-6) that
/// matches it. Each branch consumes the character before the run, which no earlier match can have consumed (a match
/// ends at a run's end or just before a run's preceding character), so a run's branch depends only on its two
/// neighbours and not on where the search started. That lets one pass classify every run of a paragraph instead of
/// re-running the regex over the rest of the paragraph at each delimiter.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Run {
    start: usize,
    len: usize,
    class: Option<u8>,
}

/// The `*` and `_` runs of one inline source, computed on first use.
#[derive(Default)]
struct DelimiterRuns {
    ast: Option<RunTable>,
    und: Option<RunTable>,
}

impl DelimiterRuns {
    fn get(&mut self, masked: &str, delim: u8) -> &mut RunTable {
        let slot = if delim == b'*' { &mut self.ast } else { &mut self.und };
        slot.get_or_insert_with(|| RunTable::new(delimiter_runs(masked, delim)))
    }
}

/// A source's runs with what lets the emStrong scan find an opener's closing run without walking the runs between:
/// `opener_len[i]` sums the lengths of opener-only runs (branches 3 and 4) before run `i`, and `searches` hold the
/// prefix sums and trees of [`CloserSearch`].
struct RunTable {
    runs: Vec<Run>,
    opener_len: Vec<usize>,
    /// The closing searches per (opener length mod 3, previous character is the delimiter), built on first use.
    searches: [Option<CloserSearch>; 6],
}

/// Where a closing search from run `first` ends, for one (opener length mod 3, same previous character) variant,
/// without walking the runs in between.
///
/// The walk ([`RunTable::close_by_walk`], marked's emStrongRDelim loop) keeps `left` (open delimiters): opener-only
/// runs add their length, closing runs subtract theirs, and the first closing run that brings `left` to 0 or below
/// closes. A closing run of branch 5 or 6 instead adds to `mid` when the rule of 3 applies (`skip`), and otherwise
/// ends the search with no match when the previous character is the delimiter (`stop`). Walking from every opener
/// made a paragraph with many unmatched openers quadratic (each walks to its end). With prefix sums `left` after a
/// subtracting run `j` is `l_len + opener_len[j] - opener_len[first] - (sub[j + 1] - sub[first])`, so the closing
/// run is the first subtracting `j` in `[first, next_stop[first])` with `q[j] = opener_len[j] - sub[j + 1]` at most
/// `opener_len[first] - sub[first] - l_len`: a minimum segment tree over `q` finds it in O(log n).
struct CloserSearch {
    /// Sum of the lengths of subtracting runs before run `i`.
    sub: Vec<isize>,
    /// Sum of the lengths of `skip` runs before run `i`.
    skip: Vec<isize>,
    /// The first `stop` run at or after `i` (`runs.len()` when none).
    next_stop: Vec<usize>,
    /// Minimum segment tree over `q` (`isize::MAX` for runs that do not subtract), leaves from `size`.
    tree: Vec<isize>,
    size: usize,
}

impl CloserSearch {
    fn new(table: &RunTable, l_mod: usize, same_prev: bool) -> Self {
        let runs = &table.runs;
        let n = runs.len();
        let (mut sub, mut skip) = (vec![0isize; n + 1], vec![0isize; n + 1]);
        let mut next_stop = vec![n; n + 1];
        let size = n.max(1).next_power_of_two();
        let mut tree = vec![isize::MAX; 2 * size];
        let mut kinds = vec![0u8; n];
        for (i, run) in runs.iter().enumerate() {
            let r_len = run.len as isize;
            // 0: neither, 1: subtracts, 2: skip (adds to `mid`), 3: stop.
            kinds[i] = match run.class {
                Some(5 | 6) if l_mod != 0 && (l_mod as isize + r_len) % 3 == 0 => 2,
                Some(5 | 6) if same_prev => 3,
                Some(1 | 2 | 5 | 6) => 1,
                _ => 0,
            };
            sub[i + 1] = sub[i] + if kinds[i] == 1 { r_len } else { 0 };
            skip[i + 1] = skip[i] + if kinds[i] == 2 { r_len } else { 0 };
            if kinds[i] == 1 {
                tree[size + i] = table.opener_len[i] as isize - sub[i + 1];
            }
        }
        for i in (0..n).rev() {
            next_stop[i] = if kinds[i] == 3 { i } else { next_stop[i + 1] };
        }
        for i in (1..size).rev() {
            tree[i] = tree[2 * i].min(tree[2 * i + 1]);
        }
        work(n);
        CloserSearch {
            sub,
            skip,
            next_stop,
            tree,
            size,
        }
    }

    /// The first leaf in `[lo, hi)` whose value is at most `t`.
    fn first_at_most(&self, lo: usize, hi: usize, t: isize) -> Option<usize> {
        self.descend(1, 0, self.size, lo, hi, t)
    }

    fn descend(&self, node: usize, a: usize, b: usize, lo: usize, hi: usize, t: isize) -> Option<usize> {
        work(0);
        if b <= lo || hi <= a || self.tree[node] > t {
            return None;
        }
        if b - a == 1 {
            return Some(a);
        }
        let m = (a + b) / 2;
        self.descend(2 * node, a, m, lo, hi, t)
            .or_else(|| self.descend(2 * node + 1, m, b, lo, hi, t))
    }
}

impl RunTable {
    fn new(runs: Vec<Run>) -> Self {
        let mut opener_len = vec![0; runs.len() + 1];
        for (i, r) in runs.iter().enumerate() {
            opener_len[i + 1] = opener_len[i] + if matches!(r.class, Some(3 | 4)) { r.len } else { 0 };
        }
        RunTable {
            runs,
            opener_len,
            searches: Default::default(),
        }
    }

    /// The run that closes an opener of length `l_len` whose search starts at run `first`, with `left` (0 or below)
    /// and `mid` at that run; None when no run closes it. Same result as [`RunTable::close_by_walk`], in O(log n).
    fn close(&mut self, first: usize, l_len: usize, same_prev: bool) -> Option<(usize, isize, isize)> {
        let variant = l_len % 3 * 2 + usize::from(same_prev);
        if self.searches[variant].is_none() {
            self.searches[variant] = Some(CloserSearch::new(self, l_len % 3, same_prev));
        }
        let search = self.searches[variant].as_ref()?;
        let first = first.min(self.runs.len());
        let target = self.opener_len[first] as isize - search.sub[first] - l_len as isize;
        let j = search.first_at_most(first, search.next_stop[first], target)?;
        let left = l_len as isize + (self.opener_len[j] - self.opener_len[first]) as isize
            - (search.sub[j + 1] - search.sub[first]);
        Some((j, left, search.skip[j] - search.skip[first]))
    }

    /// marked's emStrongRDelim loop over the runs from `first`, one run at a time: the reference for
    /// [`RunTable::close`].
    #[cfg(test)]
    fn close_by_walk(&self, first: usize, l_len: usize, same_prev: bool) -> Option<(usize, isize, isize)> {
        let (mut left, mut mid) = (l_len as isize, 0isize);
        for (j, run) in self.runs.iter().enumerate().skip(first) {
            let r_len = run.len as isize;
            match run.class {
                Some(3 | 4) => left += r_len,
                Some(5 | 6) if !l_len.is_multiple_of(3) && (l_len as isize + r_len) % 3 == 0 => mid += r_len,
                Some(5 | 6) if same_prev => return None,
                Some(1 | 2 | 5 | 6) => {
                    left -= r_len;
                    if left <= 0 {
                        return Some((j, left, mid));
                    }
                }
                _ => {}
            }
        }
        None
    }
}

/// `[\p{P}\p{S}]`.
fn is_punct(c: char) -> bool {
    if c.is_ascii() {
        c.is_ascii_punctuation()
    } else {
        contains(&rules::PUNCT_OR_SYMBOL, c.encode_utf8(&mut [0; 4]))
    }
}

/// `\s` (Unicode White_Space).
fn is_space(c: char) -> bool {
    c.is_whitespace()
}

/// The emStrongRDelimAst branch (1-6) for a `*` run between `c` and `d` (`None` at the end of the source).
fn classify_ast(c: char, d: Option<char>) -> Option<u8> {
    let word = |x: char| (!is_space(x) && !is_punct(x)) || x == '~';
    let punct = is_punct(c) && c != '~';
    let punct_after = |d: char| d != '~' && is_punct(d);
    if punct && d.is_none_or(is_space) {
        Some(1)
    } else if word(c) && d.is_none_or(|d| is_space(d) || punct_after(d)) {
        Some(2)
    } else if (is_space(c) || punct) && d.is_some_and(word) {
        Some(3)
    } else if is_space(c) && d.is_some_and(punct_after) {
        Some(4)
    } else if punct && d.is_some_and(punct_after) {
        Some(5)
    } else if word(c) && d.is_some_and(word) {
        Some(6)
    } else {
        None
    }
}

/// The emStrongRDelimUnd branch (1-5) for a `_` run between `c` and `d` (`None` at the end of the source).
fn classify_und(c: char, d: Option<char>) -> Option<u8> {
    let word = |x: char| !is_space(x) && !is_punct(x);
    let punct = is_punct(c);
    if punct && d.is_none_or(is_space) {
        Some(1)
    } else if word(c) && d.is_none_or(|d| is_space(d) || is_punct(d)) {
        Some(2)
    } else if (is_space(c) || punct) && d.is_some_and(word) {
        Some(3)
    } else if is_space(c) && d.is_some_and(is_punct) {
        Some(4)
    } else if punct && d.is_some_and(is_punct) {
        Some(5)
    } else {
        None
    }
}

/// Every maximal `delim` run of `masked`, classified as [`Run`] describes.
fn delimiter_runs(masked: &str, delim: u8) -> Vec<Run> {
    work(masked.len());
    let b = masked.as_bytes();
    let mut runs = Vec::new();
    let mut i = 0;
    while let Some(off) = b[i..].iter().position(|&x| x == delim) {
        let start = i + off;
        let end = start + b[start..].iter().take_while(|&&x| x == delim).count();
        let c = masked[..start].chars().next_back();
        let d = masked[end..].chars().next();
        let class = c.and_then(|c| {
            if delim == b'*' {
                classify_ast(c, d)
            } else {
                classify_und(c, d)
            }
        });
        runs.push(Run {
            start,
            len: end - start,
            class,
        });
        i = end;
    }
    runs
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The closing-delimiter matches marked's emStrongRDelim regex finds scanning `masked` from `hay_start`, as
    /// (group, run start, run length): the reference the delimiter-run classifier must reproduce.
    fn regex_closers(masked: &str, hay_start: usize, delim: u8) -> Vec<(u8, usize, usize)> {
        let r = if delim == b'*' {
            &*rules::EM_R_AST
        } else {
            &*rules::EM_R_UND
        };
        r.captures_iter(&masked[hay_start..])
            .flatten()
            .filter_map(|c| {
                (1..=6).find_map(|i| {
                    let m = c.get(i).filter(|m| !m.as_str().is_empty())?;
                    Some((i as u8, hay_start + m.start(), m.as_str().len()))
                })
            })
            .collect()
    }

    fn run_closers(masked: &str, hay_start: usize, delim: u8) -> Vec<(u8, usize, usize)> {
        let skip = if delim == b'*' {
            &*rules::EM_R_AST_SKIP
        } else {
            &*rules::EM_R_UND_SKIP
        };
        let skip_to = exec(skip, &masked[hay_start..]).map_or(0, |c| c[0].len());
        delimiter_runs(masked, delim)
            .into_iter()
            .filter(|r| r.start >= hay_start + skip_to.max(1))
            .filter_map(|r| r.class.map(|c| (c, r.start, r.len)))
            .collect()
    }

    /// The closing search by prefix sums and segment tree ends where marked's one-run-at-a-time walk does, from
    /// every start run, for every opener length and previous character, on generated delimiter-heavy sources.
    #[test]
    fn tui_marked_closing_search_matches_the_walk() {
        let mut next = xorshift(0x5eed_1234_abcd_0001);
        let alphabet = [
            "*", "_", "**", "__", "***", " ", "a", "b", ".", ",", "!", "~", "\n", "é",
        ];
        for _ in 0..3000 {
            let masked = generated(&mut next, &alphabet, 40);
            for delim in *b"*_" {
                let mut table = RunTable::new(delimiter_runs(&masked, delim));
                for first in 0..=table.runs.len() {
                    for l_len in 1..=3 {
                        for same_prev in [false, true] {
                            assert_eq!(
                                table.close(first, l_len, same_prev),
                                table.close_by_walk(first, l_len, same_prev),
                                "{masked:?} delim {} first {first} l_len {l_len} same_prev {same_prev}",
                                delim as char
                            );
                        }
                    }
                }
            }
        }
    }

    fn xorshift(mut seed: u64) -> impl FnMut() -> usize {
        move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as usize
        }
    }

    fn generated(next: &mut impl FnMut() -> usize, alphabet: &[&str], max: usize) -> String {
        let len = 1 + next() % max;
        (0..len).map(|_| alphabet[next() % alphabet.len()]).collect()
    }

    /// The inline text scanner ends every token where marked's `inlineText` regex does, on generated strings over the
    /// characters its alternatives test (stop characters, `http`/`ftp://`/`www.` prefixes, double-space line breaks,
    /// email local parts before `@`, backtick and tilde runs, non-ASCII).
    #[test]
    fn tui_marked_inline_text_scanner_matches_the_regex() {
        let alphabet = [
            "a", "Z", "0", " ", " ", "  \n", "\n", "`", "~", "_", "*", "\\", "<", "!", "[", "@", ".", "-", "é", "€",
            "http", "HTTPs", "ftp://", "FTP:/", "www.", "ww", "x@y", "(",
        ];
        let mut next = xorshift(0x9e37_79b9_7f4a_7c15);
        for _ in 0..50_000 {
            let s = generated(&mut next, &alphabet, 16);
            let want = exec(&rules::INLINE_TEXT, &s).map(|c| c[0].len());
            assert_eq!(inline_text_len(&s), want, "{s:?}");
        }
        assert_eq!(inline_text_len(""), None);
    }

    /// Masking code spans, links and tags by candidate positions equals blockSkip's `replace_all`.
    #[test]
    fn tui_marked_block_skip_mask_matches_the_regex() {
        let alphabet = ["[", "]", "(", ")", "`", "``", "<", ">", " ", "a", "\\", "!", "é", "\n"];
        let mut next = xorshift(0x2545_f491_4f6c_dd1d);
        for _ in 0..50_000 {
            let s = generated(&mut next, &alphabet, 24);
            let want = rules::BLOCK_SKIP
                .replace_all(&s, |c: &Caps| format!("[{}]", "a".repeat(c[0].len() - 2)))
                .into_owned();
            assert_eq!(mask_block_skip(&s), want, "{s:?}");
        }
    }

    /// Generated strings over delimiters, word characters, spaces, ASCII and Unicode punctuation and symbols, `~`
    /// and non-ASCII spaces, scanned from every start a left delimiter can leave (just after a run, before a
    /// non-delimiter character): the per-run classes equal the regex's matches.
    #[test]
    fn tui_marked_delimiter_runs_match_the_em_strong_regex() {
        let alphabet = [
            "*", "*", "_", "_", "a", "é", " ", "\u{a0}", "\n", ".", "~", "\"", "!", "«", "–", "€", "+", "[", "]", "0",
        ];
        let mut next = xorshift(0x2545_f491_4f6c_dd1d);
        let mut checked = 0;
        for _ in 0..20_000 {
            let s = generated(&mut next, &alphabet, 40);
            for delim in *b"*_" {
                for (start, ch) in s.char_indices() {
                    let prev_is_delim = s[..start].ends_with(delim as char);
                    if !prev_is_delim || ch as u32 == u32::from(delim) {
                        continue;
                    }
                    assert_eq!(
                        run_closers(&s, start, delim),
                        regex_closers(&s, start, delim),
                        "{s:?} from {start} ({})",
                        delim as char
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked > 50_000, "only {checked} scans");
    }
}
