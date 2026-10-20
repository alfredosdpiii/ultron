//! What a file is made of: its constructs and the symbols it defines.
//!
//! A construct is a kind of thing a file does, named the same way in every repository so knowledge transfers:
//! `sql:trigger:before-update`, `sql:rls-enable`, `ts:decorator:Controller`, `ts:call:runWithBypass`,
//! `py:import:sqlalchemy.orm`. Similar files are files that share rare constructs; the reader weighs them.
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
pub const EXTRACTOR_VERSION: i64 = 1;

/// Files larger than this are not read: generated bundles and dumps, not code anyone reviews.
pub const MAX_BYTES: u64 = 512 * 1024;

/// Construct kinds kept per file, most frequent first: a bound on generated or vendored files.
const MAX_KINDS: usize = 400;

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
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extracted {
    pub constructs: Vec<Construct>,
    pub symbols: Vec<Symbol>,
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
            Lang::TypeScript => tree(&mut self.typescript, text, typescript_node),
            Lang::Tsx => tree(&mut self.tsx, text, typescript_node),
            Lang::Python => tree(&mut self.python, text, python_node),
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
        ("sql:create-table", r"\bCREATE\s+(?:UNLOGGED\s+|TEMP(?:ORARY)?\s+)?TABLE\b"),
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
        ("sql:catalog-guard", r"\bpg_(?:constraint|trigger|policies|indexes|class)\b"),
        ("sql:if-not-exists", r"\bIF\s+NOT\s+EXISTS\b"),
        ("sql:create-type", r"\bCREATE\s+TYPE\b"),
        ("sql:create-extension", r"\bCREATE\s+EXTENSION\b"),
        ("sql:create-view", r"\bCREATE\s+(?:OR\s+REPLACE\s+)?(?:MATERIALIZED\s+)?VIEW\b"),
        ("sql:jsonb", r"\bJSONB\b"),
        ("sql:delete-rows", r"\bDELETE\s+FROM\b"),
        ("sql:update-rows", r"\bUPDATE\s+\S+\s+SET\b"),
        ("sql:insert-rows", r"\bINSERT\s+INTO\b"),
        ("sql:truncate", r"\bTRUNCATE\b"),
    ]
    .into_iter()
    .map(|(kind, pattern)| (kind, Regex::new(&format!("(?i){pattern}")).expect("a valid SQL pattern")))
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
            let when = timing[1].split_whitespace().collect::<Vec<_>>().join("-").to_lowercase();
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
        symbols: Vec::new(),
    }
}

// --- Tree-sitter languages ---------------------------------------------------------------------------------------

type Visit = fn(&Node, &[u8], &mut Kinds, &mut Vec<Symbol>);

fn tree(parser: &mut Parser, text: &str, visit: Visit) -> Extracted {
    let Some(tree) = parser.parse(text, None) else {
        return Extracted::default();
    };
    let source = text.as_bytes();
    let mut kinds = Kinds::default();
    let mut symbols = Vec::new();
    let mut cursor = tree.walk();
    'walk: loop {
        visit(&cursor.node(), source, &mut kinds, &mut symbols);
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
    Extracted {
        constructs: kinds.into_constructs(),
        symbols,
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
    let ok = !name.is_empty()
        && name.len() <= 80
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    ok.then_some(name)
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

fn typescript_node(node: &Node, source: &[u8], kinds: &mut Kinds, symbols: &mut Vec<Symbol>) {
    let line = line_of(node);
    match node.kind() {
        "decorator" => {
            let Some(expression) = node.named_child(0) else { return };
            let callee = if expression.kind() == "call_expression" {
                expression.child_by_field_name("function")
            } else {
                Some(expression)
            };
            if let Some(name) = callee.and_then(|callee| callee_name(&callee, source, "member_expression", "property")) {
                kinds.add(format!("ts:decorator:{name}"), line);
            }
        }
        "call_expression" => {
            if let Some(name) = node
                .child_by_field_name("function")
                .and_then(|callee| callee_name(&callee, source, "member_expression", "property"))
            {
                kinds.add(format!("ts:call:{name}"), line);
            }
        }
        "new_expression" => {
            if let Some(name) = node
                .child_by_field_name("constructor")
                .and_then(|callee| callee_name(&callee, source, "member_expression", "property"))
            {
                kinds.add(format!("ts:new:{name}"), line);
            }
        }
        "import_statement" => {
            if let Some(source_node) = node.child_by_field_name("source") {
                let module = text_of(&source_node, source).trim_matches(|c| c == '"' || c == '\'' || c == '`');
                // Relative imports name this repository's own files; the symbols carry those.
                if !module.is_empty() && !module.starts_with('.') && !module.starts_with('/') {
                    kinds.add(format!("ts:import:{module}"), line);
                }
            }
        }
        "function_declaration" | "generator_function_declaration" => symbol(node, source, "function", symbols),
        "class_declaration" | "abstract_class_declaration" => symbol(node, source, "class", symbols),
        "method_definition" => symbol(node, source, "method", symbols),
        "variable_declarator" => {
            let is_function = node
                .child_by_field_name("value")
                .is_some_and(|value| matches!(value.kind(), "arrow_function" | "function_expression" | "function"));
            if is_function {
                symbol(node, source, "function", symbols);
            }
        }
        _ => {}
    }
}

fn python_node(node: &Node, source: &[u8], kinds: &mut Kinds, symbols: &mut Vec<Symbol>) {
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
            if let Some(name) = node
                .child_by_field_name("function")
                .and_then(|callee| callee_name(&callee, source, "attribute", "attribute"))
            {
                kinds.add(format!("py:call:{name}"), line);
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
                }
            }
        }
        "import_from_statement" => {
            if let Some(module) = node.child_by_field_name("module_name") {
                let module = text_of(&module, source);
                if !module.is_empty() && !module.starts_with('.') {
                    kinds.add(format!("py:import:{module}"), line);
                }
            }
        }
        "function_definition" => symbol(node, source, "function", symbols),
        "class_definition" => symbol(node, source, "class", symbols),
        _ => {}
    }
}

fn symbol(node: &Node, source: &[u8], kind: &str, symbols: &mut Vec<Symbol>) {
    if let Some(name) = node
        .child_by_field_name("name")
        .and_then(|name| plain_name(text_of(&name, source)))
    {
        symbols.push(Symbol {
            name: name.to_string(),
            kind: kind.to_string(),
            line: line_of(node),
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
            vec![("TopicsController", "class"), ("find", "method"), ("helper", "function")]
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
