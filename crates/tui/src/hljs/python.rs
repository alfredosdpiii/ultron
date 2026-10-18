//! highlight.js 10.7.3 `lib/languages/python.js` with the core modes it uses (`BACKSLASH_ESCAPE`,
//! `APOS_STRING_MODE`, `QUOTE_STRING_MODE`, `HASH_COMMENT_MODE`, `UNDERSCORE_TITLE_MODE`), regex sources verbatim.

use super::{Contain, Def, Grammar, KeywordTable};

const RESERVED_WORDS: &[&str] = &[
    "and",
    "as",
    "assert",
    "async",
    "await",
    "break",
    "class",
    "continue",
    "def",
    "del",
    "elif",
    "else",
    "except",
    "finally",
    "for",
    "from",
    "global",
    "if",
    "import",
    "in",
    "is",
    "lambda",
    "nonlocal|10",
    "not",
    "or",
    "pass",
    "raise",
    "return",
    "try",
    "while",
    "with",
    "yield",
];

const BUILT_INS: &[&str] = &[
    "__import__",
    "abs",
    "all",
    "any",
    "ascii",
    "bin",
    "bool",
    "breakpoint",
    "bytearray",
    "bytes",
    "callable",
    "chr",
    "classmethod",
    "compile",
    "complex",
    "delattr",
    "dict",
    "dir",
    "divmod",
    "enumerate",
    "eval",
    "exec",
    "filter",
    "float",
    "format",
    "frozenset",
    "getattr",
    "globals",
    "hasattr",
    "hash",
    "help",
    "hex",
    "id",
    "input",
    "int",
    "isinstance",
    "issubclass",
    "iter",
    "len",
    "list",
    "locals",
    "map",
    "max",
    "memoryview",
    "min",
    "next",
    "object",
    "oct",
    "open",
    "ord",
    "pow",
    "print",
    "property",
    "range",
    "repr",
    "reversed",
    "round",
    "set",
    "setattr",
    "slice",
    "sorted",
    "staticmethod",
    "str",
    "sum",
    "super",
    "tuple",
    "type",
    "vars",
    "zip",
];

const LITERALS: &[&str] = &["__debug__", "Ellipsis", "False", "None", "NotImplemented", "True"];

const TYPES: &[&str] = &[
    "Any",
    "Callable",
    "Coroutine",
    "Dict",
    "List",
    "Literal",
    "Generic",
    "Optional",
    "Sequence",
    "Set",
    "Tuple",
    "Type",
    "Union",
];

const DIGITPART: &str = "[0-9](_?[0-9])*";

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

struct Defs(Vec<Def>);

impl Defs {
    fn add(&mut self, d: Def) -> usize {
        self.0.push(d);
        self.0.len() - 1
    }
}

fn m(i: usize) -> Contain {
    Contain::Mode(i)
}

/// The Python grammar (built once; its composed regex sources live for the process).
pub fn grammar() -> &'static Grammar {
    static GRAMMAR: std::sync::LazyLock<Grammar> = std::sync::LazyLock::new(build);
    &GRAMMAR
}

fn build() -> Grammar {
    let kw = 0;
    let keywords = vec![KeywordTable {
        pattern: Some(r"[A-Za-z]\w+|__\w+__"),
        classes: vec![
            ("keyword", RESERVED_WORDS.to_vec()),
            ("built_in", BUILT_INS.to_vec()),
            ("literal", LITERALS.to_vec()),
            ("type", TYPES.to_vec()),
        ],
    }];
    let mut d = Defs(Vec::new());
    let t = Some(true);

    // Core modes (frozen in highlight.js).
    let backslash = d.add(Def {
        begin: Some(r"\\[\s\S]"),
        frozen: true,
        ..Def::default()
    });
    let apos = d.add(Def {
        class_name: Some("string"),
        begin: Some("'"),
        end: Some("'"),
        illegal: Some(r"\n"),
        contains: Some(vec![m(backslash)]),
        frozen: true,
        ..Def::default()
    });
    let quote = d.add(Def {
        class_name: Some("string"),
        begin: Some("\""),
        end: Some("\""),
        illegal: Some(r"\n"),
        contains: Some(vec![m(backslash)]),
        frozen: true,
        ..Def::default()
    });
    let phrasal = d.add(Def {
        begin: Some(
            r"\b(a|an|the|are|I'm|isn't|don't|doesn't|won't|but|just|should|pretty|simply|enough|gonna|going|wtf|so|such|will|you|your|they|like|more)\b",
        ),
        frozen: true,
        ..Def::default()
    });
    let doctag = d.add(Def {
        class_name: Some("doctag"),
        begin: Some("(?:TODO|FIXME|NOTE|BUG|OPTIMIZE|HACK|XXX):"),
        frozen: true,
        ..Def::default()
    });
    let hash_comment = d.add(Def {
        class_name: Some("comment"),
        begin: Some("#"),
        end: Some("$"),
        contains: Some(vec![m(phrasal), m(doctag)]),
        frozen: true,
        ..Def::default()
    });
    let underscore_title = d.add(Def {
        class_name: Some("title"),
        begin: Some(r"[a-zA-Z_]\w*"),
        frozen: true,
        ..Def::default()
    });

    let prompt = d.add(Def {
        class_name: Some("meta"),
        begin: Some(r"^(>>>|\.\.\.) "),
        ..Def::default()
    });
    let subst = d.add(Def {
        class_name: Some("subst"),
        begin: Some(r"\{"),
        end: Some(r"\}"),
        keywords: Some(kw),
        illegal: Some("#"),
        ..Def::default()
    });
    let literal_bracket = d.add(Def {
        begin: Some(r"\{\{"),
        ..Def::default()
    });

    let prefix = "([uU]|[bB]|[rR]|[bB][rR]|[rR][bB])?";
    let fprefix = "([fF][rR]|[rR][fF]|[fF])";
    let variant = |d: &mut Defs, begin: String, end: &'static str, contains: Option<Vec<Contain>>| {
        d.add(Def {
            begin: Some(leak(begin)),
            end: Some(end),
            contains,
            ..Def::default()
        })
    };
    let string_variants = vec![
        variant(
            &mut d,
            format!("{prefix}'''"),
            "'''",
            Some(vec![m(backslash), m(prompt)]),
        ),
        variant(
            &mut d,
            format!("{prefix}\"\"\""),
            "\"\"\"",
            Some(vec![m(backslash), m(prompt)]),
        ),
        variant(
            &mut d,
            format!("{fprefix}'''"),
            "'''",
            Some(vec![m(backslash), m(prompt), m(literal_bracket), m(subst)]),
        ),
        variant(
            &mut d,
            format!("{fprefix}\"\"\""),
            "\"\"\"",
            Some(vec![m(backslash), m(prompt), m(literal_bracket), m(subst)]),
        ),
        variant(&mut d, "([uU]|[rR])'".into(), "'", None),
        variant(&mut d, "([uU]|[rR])\"".into(), "\"", None),
        variant(&mut d, "([bB]|[bB][rR]|[rR][bB])'".into(), "'", None),
        variant(&mut d, "([bB]|[bB][rR]|[rR][bB])\"".into(), "\"", None),
        variant(
            &mut d,
            format!("{fprefix}'"),
            "'",
            Some(vec![m(backslash), m(literal_bracket), m(subst)]),
        ),
        variant(
            &mut d,
            format!("{fprefix}\""),
            "\"",
            Some(vec![m(backslash), m(literal_bracket), m(subst)]),
        ),
        apos,
        quote,
    ];
    let string = d.add(Def {
        class_name: Some("string"),
        contains: Some(vec![m(backslash)]),
        variants: Some(string_variants),
        ..Def::default()
    });

    let pointfloat = format!(r"(\b({DIGITPART}))?\.({DIGITPART})|\b({DIGITPART})\.");
    let number_variants = [
        format!(r"(\b({DIGITPART})|({pointfloat}))[eE][+-]?({DIGITPART})[jJ]?\b"),
        format!("({pointfloat})[jJ]?"),
        r"\b([1-9](_?[0-9])*|0+(_?0)*)[lLjJ]?\b".to_string(),
        r"\b0[bB](_?[01])+[lL]?\b".to_string(),
        r"\b0[oO](_?[0-7])+[lL]?\b".to_string(),
        r"\b0[xX](_?[0-9a-fA-F])+[lL]?\b".to_string(),
        format!(r"\b({DIGITPART})[jJ]\b"),
    ]
    .into_iter()
    .map(|b| {
        d.add(Def {
            begin: Some(leak(b)),
            ..Def::default()
        })
    })
    .collect();
    let number = d.add(Def {
        class_name: Some("number"),
        variants: Some(number_variants),
        ..Def::default()
    });

    let type_marker = d.add(Def {
        begin: Some("# type:"),
        ..Def::default()
    });
    let type_inner = d.add(Def {
        begin: Some("#"),
        end: Some(r"\b\B"),
        ends_with_parent: t,
        ..Def::default()
    });
    let comment_type = d.add(Def {
        class_name: Some("comment"),
        begin: Some("(?=# type:)"),
        end: Some("$"),
        keywords: Some(kw),
        contains: Some(vec![m(type_marker), m(type_inner)]),
        ..Def::default()
    });

    let params_empty = d.add(Def {
        class_name: Some(""),
        begin: Some(r"\(\s*\)"),
        skip: t,
        ..Def::default()
    });
    let params_list = d.add(Def {
        begin: Some(r"\("),
        end: Some(r"\)"),
        exclude_begin: t,
        exclude_end: t,
        keywords: Some(kw),
        contains: Some(vec![Contain::SelfRef, m(prompt), m(number), m(string), m(hash_comment)]),
        ..Def::default()
    });
    let params = d.add(Def {
        class_name: Some("params"),
        variants: Some(vec![params_empty, params_list]),
        ..Def::default()
    });
    d.0[subst].contains = Some(vec![m(string), m(number), m(prompt)]);

    let self_mode = d.add(Def {
        begin: Some(r"\bself\b"),
        ..Def::default()
    });
    let if_mode = d.add(Def {
        begin_keywords: Some("if"),
        ..Def::default()
    });
    let def_variant = d.add(Def {
        class_name: Some("function"),
        begin_keywords: Some("def"),
        ..Def::default()
    });
    let class_variant = d.add(Def {
        class_name: Some("class"),
        begin_keywords: Some("class"),
        ..Def::default()
    });
    let arrow = d.add(Def {
        begin: Some("->"),
        ends_with_parent: t,
        keywords: Some(kw),
        ..Def::default()
    });
    let def_class = d.add(Def {
        variants: Some(vec![def_variant, class_variant]),
        end: Some(":"),
        illegal: Some(r"[${=;\n,]"),
        contains: Some(vec![m(underscore_title), m(params), m(arrow)]),
        ..Def::default()
    });
    let meta = d.add(Def {
        class_name: Some("meta"),
        begin: Some(r"^[\t ]*@"),
        end: Some(r"(?=#)|$"),
        contains: Some(vec![m(number), m(params), m(string)]),
        ..Def::default()
    });
    let root = d.add(Def {
        keywords: Some(kw),
        illegal: Some(r"(<\/|->|\?)|=>"),
        contains: Some(vec![
            m(prompt),
            m(number),
            m(self_mode),
            m(if_mode),
            m(string),
            m(comment_type),
            m(hash_comment),
            m(def_class),
            m(meta),
        ]),
        ..Def::default()
    });
    Grammar {
        defs: d.0,
        keywords,
        root,
        case_insensitive: false,
    }
}
