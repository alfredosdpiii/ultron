//! Syntax highlighting with syntect mapped to the theme's syntax categories (port of omp
//! crates/pi-natives/src/highlight.rs).
//!
//! Scopes map to 11 categories (comment, keyword, function, variable, string, number, type, operator, punctuation,
//! inserted, deleted); each colored run ends with `ESC[39m`, so every output line is self-contained.
//!
//! syntect's bundled grammars load at first use; the vendored TypeScript/TSX/Nix grammars (from omp) compile in
//! [`warm`], meant for a background thread. Until that finishes, `ts`/`tsx` highlight with the JavaScript grammar,
//! so a paint never waits on grammar compilation.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::OnceLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, ScopeStackOp, SyntaxDefinition, SyntaxReference, SyntaxSet};

use crate::theme::Theme;

static DEFAULTS: OnceLock<SyntaxSet> = OnceLock::new();
static EXTENDED: OnceLock<SyntaxSet> = OnceLock::new();

const EXTRA_SYNTAXES: &[(&str, &str)] = &[
    ("TypeScript", include_str!("syntaxes/TypeScript.sublime-syntax")),
    (
        "TypeScriptReact",
        include_str!("syntaxes/TypeScriptReact.sublime-syntax"),
    ),
    ("Nix", include_str!("syntaxes/Nix.sublime-syntax")),
];

fn defaults() -> &'static SyntaxSet {
    DEFAULTS.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// The richest syntax set available right now (never blocks on the vendored grammars).
fn syntax_set() -> &'static SyntaxSet {
    EXTENDED.get().unwrap_or_else(defaults)
}

/// Compile the vendored grammars and the regexes of common languages. Run it once on a background thread.
pub fn warm() {
    let ss = EXTENDED.get_or_init(|| {
        let mut builder = defaults().clone().into_builder();
        for (_, source) in EXTRA_SYNTAXES {
            if let Ok(def) = SyntaxDefinition::load_from_str(source, true, None) {
                builder.add(def);
            }
        }
        builder.build()
    });
    for (lang, code) in [
        (
            "ts",
            "import { a } from \"b\";\nexport const f = async <T,>(x: T): Promise<T> => `${x}`;\n",
        ),
        ("python", "def f(x: int = 1) -> str:\n    return f\"{x}\"  # c\n"),
        ("rust", "fn main() { let s = \"x\"; println!(\"{s}\"); }\n"),
        ("bash", "for f in *.ts; do echo \"$f\"; done\n"),
    ] {
        if let Some(syntax) = find_syntax(ss, lang) {
            let mut state = ParseState::new(syntax);
            for line in syntect::util::LinesWithEndings::from(code) {
                if state.parse_line(line, ss).is_err() {
                    break;
                }
            }
        }
    }
}

/// True once [`warm`] finished (the vendored grammars are in use).
pub fn warmed() -> bool {
    EXTENDED.get().is_some()
}

/// ANSI sequences per category.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HighlightColors {
    pub comment: String,
    pub keyword: String,
    pub function: String,
    pub variable: String,
    pub string: String,
    pub number: String,
    pub r#type: String,
    pub operator: String,
    pub punctuation: String,
    pub inserted: Option<String>,
    pub deleted: Option<String>,
}

impl HighlightColors {
    /// The theme's `syntax*` colors, with diff additions/removals from `toolDiffAdded`/`toolDiffRemoved`.
    pub fn from_theme(theme: &Theme) -> Self {
        let fg = |name: &str| theme.fg_ansi(name).unwrap_or_default();
        Self {
            comment: fg("syntaxComment"),
            keyword: fg("syntaxKeyword"),
            function: fg("syntaxFunction"),
            variable: fg("syntaxVariable"),
            string: fg("syntaxString"),
            number: fg("syntaxNumber"),
            r#type: fg("syntaxType"),
            operator: fg("syntaxOperator"),
            punctuation: fg("syntaxPunctuation"),
            inserted: theme.fg_ansi("toolDiffAdded").ok(),
            deleted: theme.fg_ansi("toolDiffRemoved").ok(),
        }
    }

    fn palette(&self) -> [&str; 11] {
        [
            &self.comment,
            &self.keyword,
            &self.function,
            &self.variable,
            &self.string,
            &self.number,
            &self.r#type,
            &self.operator,
            &self.punctuation,
            self.inserted.as_deref().unwrap_or(""),
            self.deleted.as_deref().unwrap_or(""),
        ]
    }
}

const LANG_ALIASES: &[(&[&str], &str)] = &[
    (&["ts", "mts", "cts", "typescript"], "TypeScript"),
    (&["tsx"], "TypeScriptReact"),
    (&["js", "jsx", "javascript", "mjs", "cjs"], "JavaScript"),
    (&["py", "python"], "Python"),
    (&["rb", "ruby"], "Ruby"),
    (&["nix"], "Nix"),
    (&["rs", "rust"], "Rust"),
    (&["go", "golang"], "Go"),
    (&["java"], "Java"),
    (&["kt", "kotlin"], "Java"),
    (&["swift"], "Objective-C"),
    (&["c", "h"], "C"),
    (&["cpp", "cc", "cxx", "c++", "hpp", "hxx", "hh"], "C++"),
    (&["cs", "csharp"], "C#"),
    (&["php"], "PHP"),
    (&["sh", "bash", "zsh", "shell"], "Bash"),
    (&["ps1", "powershell"], "PowerShell"),
    (&["html", "htm", "vue", "svelte"], "HTML"),
    (&["css"], "CSS"),
    (&["scss"], "SCSS"),
    (&["sass"], "Sass"),
    (&["less"], "LESS"),
    (&["json"], "JSON"),
    (&["yaml", "yml"], "YAML"),
    (&["toml"], "TOML"),
    (&["xml"], "XML"),
    (&["md", "markdown"], "Markdown"),
    (&["sql"], "SQL"),
    (&["lua"], "Lua"),
    (&["r"], "R"),
    (&["scala"], "Scala"),
    (&["clj", "clojure"], "Clojure"),
    (&["el", "elisp", "emacs-lisp", "emacslisp"], "Lisp"),
    (&["ex", "exs", "elixir"], "Ruby"),
    (&["erl", "erlang"], "Erlang"),
    (&["hs", "haskell"], "Haskell"),
    (&["ml", "ocaml"], "OCaml"),
    (&["vim"], "VimL"),
    (&["graphql", "gql"], "GraphQL"),
    (&["proto", "protobuf"], "Protocol Buffers"),
    (&["tf", "hcl", "terraform"], "Terraform"),
    (&["dockerfile", "docker", "containerfile"], "Dockerfile"),
    (&["makefile", "make", "just", "justfile"], "Makefile"),
    (&["cmake", "cmakelists"], "CMake"),
    (&["ini", "cfg", "conf", "config", "properties"], "INI"),
    (&["diff", "patch"], "Diff"),
    (&["gitignore", "gitattributes", "gitmodules"], "Git Ignore"),
];

fn find_alias(lang: &str) -> Option<&'static str> {
    LANG_ALIASES
        .iter()
        .find(|(aliases, _)| aliases.iter().any(|a| lang.eq_ignore_ascii_case(a)))
        .map(|(_, target)| *target)
}

/// omp `find_syntax`: token, then extension, then alias; TypeScript falls back to JavaScript before [`warm`].
fn find_syntax<'a>(ss: &'a SyntaxSet, lang: &str) -> Option<&'a SyntaxReference> {
    if let Some(s) = ss
        .find_syntax_by_token(lang)
        .or_else(|| ss.find_syntax_by_extension(lang))
    {
        return Some(s);
    }
    let alias = find_alias(lang)?;
    ss.find_syntax_by_name(alias)
        .or_else(|| ss.find_syntax_by_token(alias))
        .or_else(|| {
            alias
                .starts_with("TypeScript")
                .then(|| ss.find_syntax_by_name("JavaScript"))
                .flatten()
        })
}

/// Whether `lang` resolves to a grammar.
pub fn supports_language(lang: &str) -> bool {
    find_alias(lang).is_some() || find_syntax(syntax_set(), lang).is_some()
}

struct Matchers {
    by_index: Vec<(usize, Vec<Scope>)>,
}

fn matchers() -> &'static Matchers {
    static M: OnceLock<Matchers> = OnceLock::new();
    M.get_or_init(|| {
        let s = |names: &[&str]| names.iter().filter_map(|n| Scope::new(n).ok()).collect::<Vec<_>>();
        Matchers {
            by_index: vec![
                (0, s(&["comment"])),
                (9, s(&["markup.inserted"])),
                (10, s(&["markup.deleted"])),
                (1, s(&["meta.diff.header", "meta.diff.range"])),
                (4, s(&["string", "constant.character", "meta.string"])),
                (5, s(&["constant.numeric", "constant.integer"])),
                (1, s(&["keyword", "storage.type", "storage.modifier"])),
                (
                    2,
                    s(&[
                        "entity.name.function",
                        "support.function",
                        "meta.function-call",
                        "variable.function",
                    ]),
                ),
                (
                    6,
                    s(&[
                        "entity.name.type",
                        "support.type",
                        "support.class",
                        "entity.name.class",
                        "entity.name.struct",
                        "entity.name.enum",
                        "entity.name.interface",
                        "entity.name.trait",
                    ]),
                ),
                (7, s(&["keyword.operator", "punctuation.accessor"])),
                (8, s(&["punctuation"])),
                (3, s(&["variable", "entity.name", "meta.path"])),
                (5, s(&["constant"])),
            ],
        }
    })
}

fn scope_color(s: Scope) -> usize {
    matchers()
        .by_index
        .iter()
        .find(|(_, scopes)| scopes.iter().any(|m| m.is_prefix_of(s)))
        .map_or(usize::MAX, |(i, _)| *i)
}

thread_local! {
    static CACHE: RefCell<HashMap<Scope, usize>> = RefCell::new(HashMap::new());
}

fn stack_color(stack: &ScopeStack) -> usize {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        stack
            .as_slice()
            .iter()
            .rev()
            .map(|s| *cache.entry(*s).or_insert_with(|| scope_color(*s)))
            .find(|i| *i != usize::MAX)
            .unwrap_or(usize::MAX)
    })
}

fn highlight_into(
    code: &str,
    ss: &SyntaxSet,
    state: &mut ParseState,
    stack: &mut ScopeStack,
    palette: &[&str; 11],
    out: &mut String,
) {
    let emit = |text: &str, stack: &ScopeStack, out: &mut String| {
        let i = stack_color(stack);
        match palette.get(i).filter(|c| !c.is_empty()) {
            Some(color) => {
                out.push_str(color);
                out.push_str(text);
                out.push_str("\x1b[39m");
            }
            None => out.push_str(text),
        }
    };
    for line in syntect::util::LinesWithEndings::from(code) {
        let Ok(ops) = state.parse_line(line, ss) else {
            out.push_str(line);
            continue;
        };
        let mut prev = 0;
        for (offset, op) in ops {
            let offset = offset.min(line.len());
            if offset > prev {
                emit(&line[prev..offset], stack, out);
            }
            prev = offset;
            match op {
                ScopeStackOp::Push(s) => stack.push(s),
                ScopeStackOp::Pop(n) => {
                    for _ in 0..n {
                        stack.pop();
                    }
                }
                ScopeStackOp::Restore | ScopeStackOp::Clear(_) | ScopeStackOp::Noop => {}
            }
        }
        if prev < line.len() {
            emit(&line[prev..], stack, out);
        }
    }
}

/// Highlight `code`; unknown or missing languages return it unchanged.
pub fn highlight_code(code: &str, lang: Option<&str>, colors: &HighlightColors) -> String {
    let ss = syntax_set();
    let Some(syntax) = lang.and_then(|l| find_syntax(ss, l)) else {
        return code.to_string();
    };
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = String::with_capacity(code.len() * 2);
    highlight_into(code, ss, &mut state, &mut stack, &colors.palette(), &mut out);
    out
}

/// One highlighted string per line of `code`.
pub fn highlight_lines(code: &str, lang: Option<&str>, colors: &HighlightColors) -> Vec<String> {
    highlight_code(code, lang, colors)
        .split('\n')
        .map(str::to_string)
        .collect()
}

/// Incremental highlighter for streamed code: chunks of whole lines give the same output as one call.
pub struct HighlightStream {
    ss: &'static SyntaxSet,
    state: Option<(ParseState, ScopeStack)>,
    colors: HighlightColors,
}

impl HighlightStream {
    pub fn new(lang: Option<&str>, colors: HighlightColors) -> Self {
        // Pin the set: a parse state only works with the syntax set it came from.
        let ss = syntax_set();
        let state = lang
            .and_then(|l| find_syntax(ss, l))
            .map(|s| (ParseState::new(s), ScopeStack::new()));
        Self { ss, state, colors }
    }

    pub fn supported(&self) -> bool {
        self.state.is_some()
    }

    /// Highlight the next newline-terminated chunk (only the last may lack the newline).
    pub fn push(&mut self, chunk: &str) -> String {
        let Some((state, stack)) = self.state.as_mut() else {
            return chunk.to_string();
        };
        let mut out = String::with_capacity(chunk.len() * 2);
        highlight_into(chunk, self.ss, state, stack, &self.colors.palette(), &mut out);
        out
    }
}
