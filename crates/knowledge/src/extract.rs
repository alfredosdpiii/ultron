//! What a file is made of: its constructs and the symbols it defines.
//!
//! A construct is a kind of thing a file does, named the same way in every repository so knowledge transfers:
//! `sql:trigger:before-update`, `sql:rls-enable`, `ts:decorator:Controller`, `ts:call:runWithBypass`,
//! `py:import:sqlalchemy.orm`. Similar files are files that share rare constructs; the reader weighs them.
//!
//! The graph layer is what a review asks of the whole repository (CodexGraph, arXiv 2408.03910): the symbols with
//! their line ranges and the symbol that contains each, every call with the symbol it is made from, and every
//! import with the names it brings in. Calls and imports are kept by name, unresolved: a reader joins them to the
//! symbols of the same revision.
//!
//! SQL is scanned, not parsed: PL/pgSQL bodies (`$$ ... $$`) defeat SQL grammars, and the statements that matter
//! (triggers, row-level security, grants, constraints) are recognisable from their keywords. TypeScript, JavaScript
//! and Python are parsed with tree-sitter.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;
use tree_sitter::{Language, Node, Parser};

/// The version of the extraction rules. Bump it whenever what is extracted changes: the indexer then reads every
/// blob again instead of keeping constructs the new rules would not produce.
pub const EXTRACTOR_VERSION: i64 = 2;

/// Files larger than this are not read: generated bundles and dumps, not code anyone reviews.
pub const MAX_BYTES: u64 = 512 * 1024;

/// Construct kinds kept per file, most frequent first: a bound on generated or vendored files.
const MAX_KINDS: usize = 400;

/// Calls and imports kept per file, in order: the same bound.
pub const MAX_CALLS: usize = 4000;
pub const MAX_IMPORTS: usize = 400;

/// Characters of a call's receiver kept (`this.db` in `this.db.runWithBypass(...)`).
const RECEIVER_CHARS: usize = 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Sql,
    TypeScript,
    Tsx,
    Python,
}

impl Lang {
    /// The language as stored: TSX and JavaScript share TypeScript's constructs.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Sql => "sql",
            Lang::TypeScript | Lang::Tsx => "ts",
            Lang::Python => "py",
        }
    }
}

/// The language of `path`, when it is one the indexer reads; vendored and built trees are left out.
pub fn language(path: &str) -> Option<Lang> {
    const SKIPPED: [&str; 7] = [
        "node_modules/",
        "dist/",
        "build/",
        "vendor/",
        ".venv/",
        "__pycache__/",
        "coverage/",
    ];
    if SKIPPED
        .iter()
        .any(|dir| path.starts_with(dir) || path.contains(&format!("/{dir}")))
    {
        return None;
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.ends_with(".min.js") || name.ends_with(".d.ts") {
        return None;
    }
    let extension = name.rsplit_once('.').map(|(_, extension)| extension)?;
    match extension {
        "sql" => Some(Lang::Sql),
        "ts" | "mts" | "cts" | "js" | "mjs" | "cjs" => Some(Lang::TypeScript),
        "tsx" | "jsx" => Some(Lang::Tsx),
        "py" => Some(Lang::Python),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Construct {
    pub kind: String,
    /// The first line (1-based) where it occurs.
    pub line: i64,
    pub count: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// `function`, `class` or `method`.
    pub kind: String,
    pub line: i64,
    /// The last line of its definition.
    pub end_line: i64,
    /// The symbol whose definition contains this one (a method's class, a nested function's function).
    pub parent: Option<String>,
}

/// A call site: what is called, on what, and from which symbol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// The innermost symbol containing the call; none at the top level of the file.
    pub caller: Option<String>,
    /// The called name: `m` in `a.b.m(...)`, the class in `new C(...)`.
    pub callee: String,
    /// The expression it is called on (`a.b`), when it is a member call.
    pub receiver: Option<String>,
    pub line: i64,
}

/// One name an import brings in: `from m import a` is (`m`, `a`); `import m` is (`m`, none). TypeScript's default
/// import is `default`, a namespace import `*`. Relative modules stay as written (`./local`, `.models`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    pub module: String,
    pub name: Option<String>,
    pub line: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extracted {
    pub constructs: Vec<Construct>,
    pub symbols: Vec<Symbol>,
    pub calls: Vec<Call>,
    pub imports: Vec<Import>,
}

/// What a tree-sitter walk collects besides constructs.
#[derive(Default)]
struct Graph {
    symbols: Vec<Symbol>,
    calls: Vec<Call>,
    imports: Vec<Import>,
}

impl Graph {
    fn call(&mut self, callee: &str, receiver: Option<String>, line: i64) {
        if self.calls.len() < MAX_CALLS {
            self.calls.push(Call {
                caller: None,
                callee: callee.to_string(),
                receiver,
                line,
            });
        }
    }

    fn import(&mut self, module: &str, name: Option<&str>, line: i64) {
        if self.imports.len() < MAX_IMPORTS && !module.is_empty() {
            self.imports.push(Import {
                module: module.to_string(),
                name: name.map(str::to_string),
                line,
            });
        }
    }

    /// Sets each symbol's parent and each call's caller: the innermost symbol whose lines contain it. A Python
    /// function directly in a class is a method.
    fn nest(mut self, python: bool) -> (Vec<Symbol>, Vec<Call>, Vec<Import>) {
        let spans: Vec<(i64, i64, String, String)> = self
            .symbols
            .iter()
            .map(|s| (s.line, s.end_line, s.name.clone(), s.kind.clone()))
            .collect();
        let innermost = |line: i64, end: i64, skip: Option<usize>| {
            spans
                .iter()
                .enumerate()
                .filter(|(index, (from, to, _, _))| Some(*index) != skip && *from <= line && end <= *to)
                .max_by(|(_, a), (_, b)| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
                .map(|(_, (_, _, name, kind))| (name.clone(), kind.clone()))
        };
        for index in 0..self.symbols.len() {
            let (line, end) = (self.symbols[index].line, self.symbols[index].end_line);
            if let Some((name, kind)) = innermost(line, end, Some(index)) {
                if python && kind == "class" && self.symbols[index].kind == "function" {
                    self.symbols[index].kind = "method".into();
                }
                self.symbols[index].parent = Some(name);
            }
        }
        for call in &mut self.calls {
            call.caller = innermost(call.line, call.line, None).map(|(name, _)| name);
        }
        (self.symbols, self.calls, self.imports)
    }
}

/// Collects constructs: each kind once, at its first line, counted.
#[derive(Default)]
struct Kinds(BTreeMap<String, (i64, i64)>);

impl Kinds {
    fn add(&mut self, kind: String, line: i64) {
        let entry = self.0.entry(kind).or_insert((line, 0));
        entry.0 = entry.0.min(line);
        entry.1 += 1;
    }

    fn into_constructs(self) -> Vec<Construct> {
        let mut constructs: Vec<Construct> = self
            .0
            .into_iter()
            .map(|(kind, (line, count))| Construct { kind, line, count })
            .collect();
        if constructs.len() > MAX_KINDS {
            constructs.sort_by(|a, b| b.count.cmp(&a.count).then(a.kind.cmp(&b.kind)));
            constructs.truncate(MAX_KINDS);
        }
        constructs.sort_by(|a, b| a.line.cmp(&b.line).then(a.kind.cmp(&b.kind)));
        constructs
    }
}

/// Reads files; keeps one parser per grammar.
pub struct Extractor {
    typescript: Parser,
    tsx: Parser,
    python: Parser,
}

impl Extractor {
    pub fn new() -> Result<Self, String> {
        let parser = |language: Language| -> Result<Parser, String> {
            let mut parser = Parser::new();
            parser
                .set_language(&language)
                .map_err(|error| format!("tree-sitter: {error}"))?;
            Ok(parser)
        };
        Ok(Self {
            typescript: parser(Language::new(tree_sitter_typescript::LANGUAGE_TYPESCRIPT))?,
            tsx: parser(Language::new(tree_sitter_typescript::LANGUAGE_TSX))?,
            python: parser(Language::new(tree_sitter_python::LANGUAGE))?,
        })
    }

    pub fn extract(&mut self, lang: Lang, text: &str) -> Extracted {
        match lang {
            Lang::Sql => sql(text),
            Lang::TypeScript => tree(&mut self.typescript, text, typescript_node, false),
            Lang::Tsx => tree(&mut self.tsx, text, typescript_node, false),
            Lang::Python => tree(&mut self.python, text, python_node, true),
        }
    }
}

// --- SQL ---------------------------------------------------------------------------------------------------------

/// The text with comments and single-quoted literals blanked (newlines kept, so offsets keep their lines).
/// Dollar-quoted bodies stay: they are PL/pgSQL, where `RAISE EXCEPTION` and the guarded statements live.
fn sql_code(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        if rest.starts_with(b"--") {
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(b' ');
                i += 1;
            }
        } else if rest.starts_with(b"/*") {
            out.extend_from_slice(b"  ");
            i += 2;
            while i < bytes.len() && !bytes[i..].starts_with(b"*/") {
                out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                i += 1;
            }
            if i < bytes.len() {
                out.extend_from_slice(b"  ");
                i += 2;
            }
        } else if bytes[i] == b'\'' {
            out.push(b' ');
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\'' {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        out.extend_from_slice(b"  ");
                        i += 2;
                        continue;
                    }
                    out.push(b' ');
                    i += 1;
                    break;
                }
                out.push(if bytes[i] == b'\n' { b'\n' } else { b' ' });
                i += 1;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // Only ASCII bytes were replaced, by ASCII, so the result is still UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

fn line_at(text: &str, offset: usize) -> i64 {
    text.as_bytes()[..offset.min(text.len())]
        .iter()
        .filter(|&&byte| byte == b'\n')
        .count() as i64
        + 1
}

static SQL_PATTERNS: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
    [
        (
            "sql:create-table",
            r"\bCREATE\s+(?:UNLOGGED\s+|TEMP(?:ORARY)?\s+)?TABLE\b",
        ),
        ("sql:alter-table", r"\bALTER\s+TABLE\b"),
        ("sql:add-column", r"\bADD\s+COLUMN\b"),
        ("sql:drop-column", r"\bDROP\s+COLUMN\b"),
        ("sql:drop-table", r"\bDROP\s+TABLE\b"),
        ("sql:create-function", r"\bCREATE\s+(?:OR\s+REPLACE\s+)?FUNCTION\b"),
        ("sql:trigger-function", r"\bRETURNS\s+TRIGGER\b"),
        ("sql:raise-exception", r"\bRAISE\s+EXCEPTION\b"),
        ("sql:rls-enable", r"\bENABLE\s+ROW\s+LEVEL\s+SECURITY\b"),
        ("sql:rls-force", r"\bFORCE\s+ROW\s+LEVEL\s+SECURITY\b"),
        ("sql:policy", r"\bCREATE\s+POLICY\b"),
        ("sql:grant", r"\bGRANT\b"),
        ("sql:revoke", r"\bREVOKE\b"),
        ("sql:check", r"\bCHECK\s*\("),
        ("sql:foreign-key", r"\bREFERENCES\b"),
        ("sql:on-delete-cascade", r"\bON\s+DELETE\s+CASCADE\b"),
        ("sql:on-delete-set-null", r"\bON\s+DELETE\s+SET\s+NULL\b"),
        ("sql:on-delete-restrict", r"\bON\s+DELETE\s+RESTRICT\b"),
        ("sql:on-update-cascade", r"\bON\s+UPDATE\s+CASCADE\b"),
        ("sql:unique-index", r"\bCREATE\s+UNIQUE\s+INDEX\b"),
        ("sql:index", r"\bCREATE\s+INDEX\b"),
        ("sql:concurrently", r"\bCONCURRENTLY\b"),
        ("sql:do-block", r"\bDO\s+\$"),
        (
            "sql:catalog-guard",
            r"\bpg_(?:constraint|trigger|policies|indexes|class)\b",
        ),
        ("sql:if-not-exists", r"\bIF\s+NOT\s+EXISTS\b"),
        ("sql:create-type", r"\bCREATE\s+TYPE\b"),
        ("sql:create-extension", r"\bCREATE\s+EXTENSION\b"),
        (
            "sql:create-view",
            r"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:MATERIALIZED\s+)?VIEW\b",
        ),
        ("sql:jsonb", r"\bJSONB\b"),
        ("sql:delete-rows", r"\bDELETE\s+FROM\b"),
        ("sql:update-rows", r"\bUPDATE\s+\S+\s+SET\b"),
        ("sql:insert-rows", r"\bINSERT\s+INTO\b"),
        ("sql:truncate", r"\bTRUNCATE\b"),
    ]
    .into_iter()
    .map(|(kind, pattern)| {
        (
            kind,
            Regex::new(&format!("(?i){pattern}")).expect("a valid SQL pattern"),
        )
    })
    .collect()
});

static TRIGGER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:CONSTRAINT\s+)?TRIGGER\b").expect("a valid trigger pattern")
});
static TIMING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(BEFORE|AFTER|INSTEAD\s+OF)\b").expect("a valid timing pattern"));
static EVENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(INSERT|UPDATE|DELETE|TRUNCATE)\b").expect("a valid event pattern"));
static ON: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bON\b").expect("a valid ON pattern"));
static FOR_EACH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bFOR\s+EACH\s+(ROW|STATEMENT)\b").expect("a valid FOR EACH pattern"));

fn sql(text: &str) -> Extracted {
    let code = sql_code(text);
    let mut kinds = Kinds::default();
    for (kind, pattern) in SQL_PATTERNS.iter() {
        for found in pattern.find_iter(&code) {
            kinds.add((*kind).to_string(), line_at(&code, found.start()));
        }
    }
    // A trigger: when it fires, on which events, per row or per statement. A trigger statement holds no `;` of its
    // own (its function is a separate statement), so it ends at the next one.
    for found in TRIGGER.find_iter(&code) {
        let line = line_at(&code, found.start());
        let statement = &code[found.start()..];
        let statement = &statement[..statement.find(';').unwrap_or(statement.len())];
        kinds.add("sql:trigger".into(), line);
        if let Some(timing) = TIMING.captures(statement) {
            let when = timing[1]
                .split_whitespace()
                .collect::<Vec<_>>()
                .join("-")
                .to_lowercase();
            let after_timing = &statement[timing.get(0).map_or(0, |m| m.end())..];
            let events = &after_timing[..ON.find(after_timing).map_or(after_timing.len(), |m| m.start())];
            for event in EVENT.captures_iter(events) {
                kinds.add(format!("sql:trigger:{when}-{}", event[1].to_lowercase()), line);
            }
        }
        if let Some(each) = FOR_EACH.captures(statement) {
            kinds.add(format!("sql:trigger:for-each-{}", each[1].to_lowercase()), line);
        }
    }
    Extracted {
        constructs: kinds.into_constructs(),
        ..Extracted::default()
    }
}

// --- Tree-sitter languages ---------------------------------------------------------------------------------------

type Visit = fn(&Node, &[u8], &mut Kinds, &mut Graph);

fn tree(parser: &mut Parser, text: &str, visit: Visit, python: bool) -> Extracted {
    let Some(tree) = parser.parse(text, None) else {
        return Extracted::default();
    };
    let source = text.as_bytes();
    let mut kinds = Kinds::default();
    let mut graph = Graph::default();
    let mut cursor = tree.walk();
    'walk: loop {
        visit(&cursor.node(), source, &mut kinds, &mut graph);
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
    let (symbols, calls, imports) = graph.nest(python);
    Extracted {
        constructs: kinds.into_constructs(),
        symbols,
        calls,
        imports,
    }
}

fn text_of<'a>(node: &Node, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

fn line_of(node: &Node) -> i64 {
    node.start_position().row as i64 + 1
}

/// A name worth keying a construct on: an identifier, not an expression.
fn plain_name(name: &str) -> Option<&str> {
    let ok =
        !name.is_empty() && name.len() <= 80 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    ok.then_some(name)
}

/// The receiver of a member callee (`a.b` of `a.b.m`), on one line and bounded.
fn receiver_of(callee: &Node, source: &[u8], member: &str, object: &str) -> Option<String> {
    if callee.kind() != member {
        return None;
    }
    let text = text_of(&callee.child_by_field_name(object)?, source);
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!flat.is_empty()).then(|| flat.chars().take(RECEIVER_CHARS).collect())
}

/// The called name of a callee: `f` for `f(...)`, `m` for `a.b.m(...)`.
fn callee_name<'a>(callee: &Node, source: &'a [u8], member: &str, property: &str) -> Option<&'a str> {
    if callee.kind() == member {
        return callee
            .child_by_field_name(property)
            .and_then(|name| plain_name(text_of(&name, source)));
    }
    plain_name(text_of(callee, source))
}

fn typescript_node(node: &Node, source: &[u8], kinds: &mut Kinds, graph: &mut Graph) {
    let line = line_of(node);
    match node.kind() {
        "decorator" => {
            let Some(expression) = node.named_child(0) else { return };
            let callee = if expression.kind() == "call_expression" {
                expression.child_by_field_name("function")
            } else {
                Some(expression)
            };
            if let Some(name) = callee.and_then(|callee| callee_name(&callee, source, "member_expression", "property"))
            {
                kinds.add(format!("ts:decorator:{name}"), line);
            }
        }
        "call_expression" => {
            if let Some(callee) = node.child_by_field_name("function")
                && let Some(name) = callee_name(&callee, source, "member_expression", "property")
            {
                kinds.add(format!("ts:call:{name}"), line);
                graph.call(name, receiver_of(&callee, source, "member_expression", "object"), line);
            }
        }
        "new_expression" => {
            if let Some(callee) = node.child_by_field_name("constructor")
                && let Some(name) = callee_name(&callee, source, "member_expression", "property")
            {
                kinds.add(format!("ts:new:{name}"), line);
                graph.call(name, receiver_of(&callee, source, "member_expression", "object"), line);
            }
        }
        "import_statement" | "export_statement" => {
            if let Some(source_node) = node.child_by_field_name("source") {
                let module = text_of(&source_node, source).trim_matches(|c| c == '"' || c == '\'' || c == '`');
                // Relative imports name this repository's own files; the symbols carry those.
                if node.kind() == "import_statement"
                    && !module.is_empty()
                    && !module.starts_with('.')
                    && !module.starts_with('/')
                {
                    kinds.add(format!("ts:import:{module}"), line);
                }
                let names = typescript_imported(node, source);
                if names.is_empty() {
                    graph.import(module, None, line);
                }
                for name in names {
                    graph.import(module, Some(&name), line);
                }
            }
        }
        "function_declaration" | "generator_function_declaration" => symbol(node, source, "function", graph),
        "class_declaration" | "abstract_class_declaration" => symbol(node, source, "class", graph),
        "method_definition" => symbol(node, source, "method", graph),
        "variable_declarator" => {
            let is_function = node
                .child_by_field_name("value")
                .is_some_and(|value| matches!(value.kind(), "arrow_function" | "function_expression" | "function"));
            if is_function {
                symbol(node, source, "function", graph);
            }
        }
        _ => {}
    }
}

/// The names an import or re-export brings in: `{ A, B as C }` gives A and B, a default import `default`, a
/// namespace import `*`; a side-effect import none.
fn typescript_imported(node: &Node, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut stack = vec![*node];
    while let Some(current) = stack.pop() {
        match current.kind() {
            "import_specifier" | "export_specifier" => {
                if let Some(name) = current
                    .child_by_field_name("name")
                    .and_then(|name| plain_name(text_of(&name, source)))
                {
                    names.push(name.to_string());
                }
                continue;
            }
            "namespace_import" | "namespace_export" => {
                names.push("*".into());
                continue;
            }
            "import_clause" => {
                let mut cursor = current.walk();
                if current
                    .named_children(&mut cursor)
                    .any(|child| child.kind() == "identifier")
                {
                    names.push("default".into());
                }
            }
            "string" => continue,
            _ => {}
        }
        let mut cursor = current.walk();
        let children: Vec<Node> = current.named_children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    if node.kind() == "export_statement" && names.is_empty() {
        // `export * from 'm'`
        let mut cursor = node.walk();
        if node.children(&mut cursor).any(|child| child.kind() == "*") {
            names.push("*".into());
        }
    }
    names
}

fn python_node(node: &Node, source: &[u8], kinds: &mut Kinds, graph: &mut Graph) {
    let line = line_of(node);
    match node.kind() {
        "decorator" => {
            let Some(expression) = node.named_child(0) else { return };
            let callee = if expression.kind() == "call" {
                expression.child_by_field_name("function")
            } else {
                Some(expression)
            };
            if let Some(name) = callee.and_then(|callee| callee_name(&callee, source, "attribute", "attribute")) {
                kinds.add(format!("py:decorator:{name}"), line);
            }
        }
        "call" => {
            if let Some(callee) = node.child_by_field_name("function")
                && let Some(name) = callee_name(&callee, source, "attribute", "attribute")
            {
                kinds.add(format!("py:call:{name}"), line);
                graph.call(name, receiver_of(&callee, source, "attribute", "object"), line);
            }
        }
        "import_statement" => {
            let mut cursor = node.walk();
            for name in node.children_by_field_name("name", &mut cursor) {
                let module = if name.kind() == "aliased_import" {
                    name.child_by_field_name("name").map(|inner| text_of(&inner, source))
                } else {
                    Some(text_of(&name, source))
                };
                if let Some(module) = module.filter(|module| !module.is_empty()) {
                    kinds.add(format!("py:import:{module}"), line);
                    graph.import(module, None, line);
                }
            }
        }
        "import_from_statement" => {
            if let Some(module) = node.child_by_field_name("module_name") {
                let module = text_of(&module, source);
                if !module.is_empty() && !module.starts_with('.') {
                    kinds.add(format!("py:import:{module}"), line);
                }
                let mut cursor = node.walk();
                let mut names = Vec::new();
                for name in node.children_by_field_name("name", &mut cursor) {
                    let name = if name.kind() == "aliased_import" {
                        name.child_by_field_name("name").map(|inner| text_of(&inner, source))
                    } else {
                        Some(text_of(&name, source))
                    };
                    names.extend(name.filter(|name| !name.is_empty()));
                }
                let mut cursor = node.walk();
                if node
                    .children(&mut cursor)
                    .any(|child| child.kind() == "wildcard_import")
                {
                    names.push("*");
                }
                if names.is_empty() {
                    graph.import(module, None, line);
                }
                for name in names {
                    graph.import(module, Some(name), line);
                }
            }
        }
        "function_definition" => symbol(node, source, "function", graph),
        "class_definition" => symbol(node, source, "class", graph),
        _ => {}
    }
}

fn symbol(node: &Node, source: &[u8], kind: &str, graph: &mut Graph) {
    if let Some(name) = node
        .child_by_field_name("name")
        .and_then(|name| plain_name(text_of(&name, source)))
    {
        graph.symbols.push(Symbol {
            name: name.to_string(),
            kind: kind.to_string(),
            line: line_of(node),
            end_line: node.end_position().row as i64 + 1,
            parent: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(extracted: &Extracted) -> Vec<&str> {
        extracted.constructs.iter().map(|c| c.kind.as_str()).collect()
    }

    #[test]
    fn sql_triggers_name_their_timing_events_and_granularity() {
        let text = "\
-- BEFORE DELETE in a comment is not a trigger
CREATE TABLE \"t\" (id int);
CREATE OR REPLACE FUNCTION refuse() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
  RAISE EXCEPTION 'append-only: UPDATE is not permitted';
END $$;
CREATE TRIGGER \"t_no_update\"
  BEFORE UPDATE OR DELETE ON \"t\"
  FOR EACH STATEMENT EXECUTE FUNCTION refuse();
";
        let extracted = Extractor::new().unwrap().extract(Lang::Sql, text);
        let found = kinds(&extracted);
        for kind in [
            "sql:create-table",
            "sql:create-function",
            "sql:trigger-function",
            "sql:raise-exception",
            "sql:trigger",
            "sql:trigger:before-update",
            "sql:trigger:before-delete",
            "sql:trigger:for-each-statement",
        ] {
            assert!(found.contains(&kind), "{kind} missing from {found:?}");
        }
        assert!(!found.contains(&"sql:trigger:before-truncate"), "{found:?}");
        let trigger = extracted.constructs.iter().find(|c| c.kind == "sql:trigger").unwrap();
        assert_eq!(trigger.line, 7);
    }

    #[test]
    fn sql_string_literals_and_comments_are_not_code() {
        let text = "SELECT 'CREATE TRIGGER x BEFORE TRUNCATE ON t'; /* REVOKE ALL */ -- GRANT\n";
        let extracted = Extractor::new().unwrap().extract(Lang::Sql, text);
        assert!(extracted.constructs.is_empty(), "{:?}", extracted.constructs);
    }

    #[test]
    fn typescript_decorators_calls_imports_and_symbols() {
        let text = "\
import { Controller, Get } from '@nestjs/common';
import { local } from './local';
@Controller('topics')
export class TopicsController {
  @Get(':id')
  async find() {
    return this.db.runWithBypass(() => prisma.topic.findMany());
  }
}
export const helper = () => new Error('x');
";
        let extracted = Extractor::new().unwrap().extract(Lang::TypeScript, text);
        let found = kinds(&extracted);
        for kind in [
            "ts:import:@nestjs/common",
            "ts:decorator:Controller",
            "ts:decorator:Get",
            "ts:call:runWithBypass",
            "ts:call:findMany",
            "ts:new:Error",
        ] {
            assert!(found.contains(&kind), "{kind} missing from {found:?}");
        }
        assert!(!found.iter().any(|kind| kind.contains("./local")), "{found:?}");
        let names: Vec<(&str, &str)> = extracted
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("TopicsController", "class"),
                ("find", "method"),
                ("helper", "function")
            ]
        );
    }

    #[test]
    fn python_decorators_calls_imports_and_symbols() {
        let text = "\
import os, sqlalchemy.orm as orm
from fastapi import APIRouter
from .local import thing

router = APIRouter()

@router.post('/modify')
def modify(request):
    return orm.Session().execute(request)

class Store:
    pass
";
        let extracted = Extractor::new().unwrap().extract(Lang::Python, text);
        let found = kinds(&extracted);
        for kind in [
            "py:import:os",
            "py:import:sqlalchemy.orm",
            "py:import:fastapi",
            "py:decorator:post",
            "py:call:APIRouter",
            "py:call:execute",
            "py:call:Session",
        ] {
            assert!(found.contains(&kind), "{kind} missing from {found:?}");
        }
        assert!(!found.iter().any(|kind| kind.contains("local")), "{found:?}");
        let names: Vec<&str> = extracted.symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["modify", "Store"]);
    }

    fn calls(extracted: &Extracted) -> Vec<(Option<&str>, &str, Option<&str>, i64)> {
        extracted
            .calls
            .iter()
            .map(|c| (c.caller.as_deref(), c.callee.as_str(), c.receiver.as_deref(), c.line))
            .collect()
    }

    fn imports(extracted: &Extracted) -> Vec<(&str, Option<&str>)> {
        extracted
            .imports
            .iter()
            .map(|i| (i.module.as_str(), i.name.as_deref()))
            .collect()
    }

    #[test]
    fn typescript_graph_has_ranges_callers_and_imported_names() {
        let text = "\
import { Controller, Get as G } from '@nestjs/common';
import prisma from '../prisma';
import * as audit from './audit';
import './side-effect';
export { helper } from './helpers';
export * from './all';
@Controller('topics')
export class TopicsController {
  find() {
    return this.db.runWithBypass(() => prisma.topic.findMany());
  }
}
export const helper = () => new Error('x');
audit.log();
";
        let extracted = Extractor::new().unwrap().extract(Lang::TypeScript, text);
        let symbols: Vec<(&str, &str, i64, i64, Option<&str>)> = extracted
            .symbols
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    s.kind.as_str(),
                    s.line,
                    s.end_line,
                    s.parent.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            symbols,
            vec![
                ("TopicsController", "class", 8, 12, None),
                ("find", "method", 9, 11, Some("TopicsController")),
                ("helper", "function", 13, 13, None),
            ]
        );
        assert_eq!(
            calls(&extracted),
            vec![
                (None, "Controller", None, 7),
                (Some("find"), "runWithBypass", Some("this.db"), 10),
                (Some("find"), "findMany", Some("prisma.topic"), 10),
                (Some("helper"), "Error", None, 13),
                (None, "log", Some("audit"), 14),
            ]
        );
        assert_eq!(
            imports(&extracted),
            vec![
                ("@nestjs/common", Some("Controller")),
                ("@nestjs/common", Some("Get")),
                ("../prisma", Some("default")),
                ("./audit", Some("*")),
                ("./side-effect", None),
                ("./helpers", Some("helper")),
                ("./all", Some("*")),
            ]
        );
    }

    #[test]
    fn python_graph_has_methods_callers_and_imported_names() {
        let text = "\
import os.path
from .models import Topic, Version as V
from x import *

class Store:
    def save(self, topic):
        def inner():
            return os.path.join('a')
        return self.session.add(Topic(topic))

def purge():
    Store().save(None)
";
        let extracted = Extractor::new().unwrap().extract(Lang::Python, text);
        let symbols: Vec<(&str, &str, Option<&str>)> = extracted
            .symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind.as_str(), s.parent.as_deref()))
            .collect();
        assert_eq!(
            symbols,
            vec![
                ("Store", "class", None),
                ("save", "method", Some("Store")),
                ("inner", "function", Some("save")),
                ("purge", "function", None),
            ]
        );
        assert_eq!(
            calls(&extracted),
            vec![
                (Some("inner"), "join", Some("os.path"), 8),
                (Some("save"), "add", Some("self.session"), 9),
                (Some("save"), "Topic", None, 9),
                (Some("purge"), "save", Some("Store()"), 12),
                (Some("purge"), "Store", None, 12),
            ]
        );
        assert_eq!(
            imports(&extracted),
            vec![
                ("os.path", None),
                (".models", Some("Topic")),
                (".models", Some("Version")),
                ("x", Some("*")),
            ]
        );
    }

    #[test]
    fn languages_by_path() {
        assert_eq!(language("backend/prisma/migrations/1_x/migration.sql"), Some(Lang::Sql));
        assert_eq!(language("frontend/src/App.tsx"), Some(Lang::Tsx));
        assert_eq!(language("backend/src/a.service.ts"), Some(Lang::TypeScript));
        assert_eq!(language("ml/app/main.py"), Some(Lang::Python));
        assert_eq!(language("frontend/node_modules/x/index.js"), None);
        assert_eq!(language("types/global.d.ts"), None);
        assert_eq!(language("README.md"), None);
    }
}
