//! The small YAML reader of the review reference (`autoreview_reference.py` `YamlNode`, `_yaml_scalar`,
//! `_split_flow`, `_strip_comment`, `parse_yaml`, `_yaml_get`): block mappings and sequences, scalars, one-line flow
//! lists and maps, block scalars and comments, with the line of each top-level entry. Every scalar is a string or
//! null, as in the source (no numbers, booleans, anchors or tags).
//!
//! Known differences from the source: Python gives up on a document nested past its recursion limit (about a
//! thousand levels, less the caller's frames) and returns None; here the limit is [`MAX_DEPTH`] levels. A key that is
//! a flow list or map (`[a]: b`) is unhashable in Python and raises `TypeError` out of `parse_yaml`; here it is
//! [`Unhashable`], which the reference treats as the source's `_read` treats the exception (the file is skipped).

use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use serde_json::{Value, json};

use crate::py;

/// Nesting (blocks and flow collections) past which a document is not read.
pub const MAX_DEPTH: usize = 400;

/// A mapping key: a string, or null (`~:` or `null:`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum YamlKey {
    Null,
    Str(String),
}

/// A value: null, a string, a list or a mapping (keys in first-insertion order; a repeated key keeps its first
/// place and takes the last value, as a Python dict does).
#[derive(Debug, Clone, PartialEq)]
pub enum Yaml {
    Null,
    Str(String),
    List(Vec<Yaml>),
    Map(IndexMap<YamlKey, Yaml>),
}

/// Where an entry of a node is: a mapping key or a sequence index.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Entry {
    Key(YamlKey),
    Index(usize),
}

/// A mapping or sequence read from YAML, with the line of each entry.
#[derive(Debug, Clone, PartialEq)]
pub struct YamlNode {
    pub value: Yaml,
    pub line: u64,
    pub lines: IndexMap<Entry, u64>,
}

/// Python raised: a flow list or map used as a mapping key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unhashable;

/// Read failures: the source's `None` (index or recursion errors) or a raised `TypeError`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Failure {
    Gone,
    Unhashable,
}

impl Yaml {
    /// `dict.get(key)` on a mapping; `None` for anything else.
    pub fn get(&self, key: &str) -> Option<&Yaml> {
        match self {
            Yaml::Map(map) => map.get(&YamlKey::Str(key.to_string())),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Yaml::Str(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&IndexMap<YamlKey, Yaml>> {
        match self {
            Yaml::Map(map) => Some(map),
            _ => None,
        }
    }

    /// Python truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            Yaml::Null => false,
            Yaml::Str(text) => !text.is_empty(),
            Yaml::List(items) => !items.is_empty(),
            Yaml::Map(map) => !map.is_empty(),
        }
    }

    /// The value as JSON, mappings as `{"$map": [[key, value], ...]}` so null keys and order survive.
    pub fn to_json(&self) -> Value {
        match self {
            Yaml::Null => Value::Null,
            Yaml::Str(text) => Value::String(text.clone()),
            Yaml::List(items) => Value::Array(items.iter().map(Yaml::to_json).collect()),
            Yaml::Map(map) => {
                json!({"$map": map.iter().map(|(key, value)| json!([key.to_json(), value.to_json()])).collect::<Vec<_>>()})
            }
        }
    }
}

impl YamlKey {
    pub fn to_json(&self) -> Value {
        match self {
            YamlKey::Null => Value::Null,
            YamlKey::Str(text) => Value::String(text.clone()),
        }
    }

    /// `str(key)`.
    pub fn py_str(&self) -> String {
        match self {
            YamlKey::Null => "None".to_string(),
            YamlKey::Str(text) => text.clone(),
        }
    }
}

/// `str(value)` as Python prints the reader's values.
pub fn py_str(value: &Yaml) -> String {
    match value {
        Yaml::Str(text) => text.clone(),
        other => py_repr(other),
    }
}

fn py_repr(value: &Yaml) -> String {
    match value {
        Yaml::Null => "None".to_string(),
        Yaml::Str(text) => py::repr(text),
        Yaml::List(items) => format!("[{}]", items.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Yaml::Map(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, item)| {
                    let key = match key {
                        YamlKey::Null => "None".to_string(),
                        YamlKey::Str(text) => py::repr(text),
                    };
                    format!("{key}: {}", py_repr(item))
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn key_of(value: Yaml) -> Result<YamlKey, Failure> {
    match value {
        Yaml::Null => Ok(YamlKey::Null),
        Yaml::Str(text) => Ok(YamlKey::Str(text)),
        _ => Err(Failure::Unhashable),
    }
}

/// `_yaml_scalar`: a scalar, quoted string, or one-line flow list or map.
pub fn yaml_scalar(text: &str) -> Result<Yaml, Unhashable> {
    scalar(text, 0).map_err(|_| Unhashable)
}

fn scalar(text: &str, depth: usize) -> Result<Yaml, Failure> {
    if depth > MAX_DEPTH {
        return Err(Failure::Gone);
    }
    let text = py::strip(text);
    if text.is_empty() || matches!(text, "~" | "null" | "Null" | "NULL") {
        return Ok(Yaml::Null);
    }
    let chars: Vec<char> = text.chars().collect();
    let (first, last) = (chars[0], chars[chars.len() - 1]);
    if chars.len() >= 2 && first == last && (first == '\'' || first == '"') {
        let inner: String = chars[1..chars.len() - 1].iter().collect();
        if first == '\'' {
            return Ok(Yaml::Str(inner.replace("''", "'")));
        }
        let mut out = String::new();
        let mut rest = inner.chars().peekable();
        while let Some(c) = rest.next() {
            match (c, rest.peek()) {
                ('\\', Some(&next)) if next != '\n' => {
                    out.push(next);
                    rest.next();
                }
                _ => out.push(c),
            }
        }
        return Ok(Yaml::Str(out));
    }
    if text.starts_with('[') && text.ends_with(']') {
        let inner = py::strip(&text[1..text.len() - 1]);
        if inner.is_empty() {
            return Ok(Yaml::List(Vec::new()));
        }
        return split_flow(inner)
            .iter()
            .map(|part| scalar(part, depth + 1))
            .collect::<Result<_, _>>()
            .map(Yaml::List);
    }
    if text.starts_with('{') && text.ends_with('}') {
        let mut out = IndexMap::new();
        for part in split_flow(&text[1..text.len() - 1]) {
            let (key, value) = part.split_once(':').unwrap_or((&part, ""));
            if !py::strip(key).is_empty() {
                let key = key_of(scalar(key, depth + 1)?)?;
                let value = scalar(value, depth + 1)?;
                out.insert(key, value);
            }
        }
        return Ok(Yaml::Map(out));
    }
    Ok(Yaml::Str(text.to_string()))
}

/// `_split_flow`: the top-level comma-separated parts of a flow collection's inside (nested brackets and quotes
/// kept).
pub fn split_flow(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth: i64 = 0;
    let mut quote: Option<char> = None;
    let mut current = String::new();
    for c in text.chars() {
        if let Some(open) = quote {
            if c == open {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c == '[' || c == '{' {
            depth += 1;
        } else if c == ']' || c == '}' {
            depth -= 1;
        } else if c == ',' && depth == 0 {
            parts.push(std::mem::take(&mut current));
            continue;
        }
        current.push(c);
    }
    if !py::strip(&current).is_empty() {
        parts.push(current);
    }
    parts
}

/// `_strip_comment`: the line without a `#` comment (one at the start or after a blank, outside quotes), trailing
/// blanks dropped.
pub fn strip_comment(line: &str) -> String {
    let mut out = String::new();
    let mut quote: Option<char> = None;
    let mut previous: Option<char> = None;
    for c in line.chars() {
        if let Some(open) = quote {
            if c == open {
                quote = None;
            }
        } else if c == '\'' || c == '"' {
            quote = Some(c);
        } else if c == '#' && previous.is_none_or(|p| p == ' ' || p == '\t') {
            break;
        }
        out.push(c);
        previous = Some(c);
    }
    py::rstrip(&out).to_string()
}

/// `str.expandtabs(size)`.
fn expand_tabs(line: &str, size: usize) -> String {
    let mut out = String::new();
    let mut column = 0;
    for c in line.chars() {
        match c {
            '\t' => {
                let pad = size - column % size;
                out.extend(std::iter::repeat_n(' ', pad));
                column += pad;
            }
            '\n' | '\r' => {
                out.push(c);
                column = 0;
            }
            _ => {
                out.push(c);
                column += 1;
            }
        }
    }
    out
}

static KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^([^\s:'"]+|'[^']*'|"[^"]*")\s*:(?:\s+|$)(.*)$"#).expect("valid regex"));
static ITEM_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^([^\s:'"]+|'[^']*'|"[^"]*")\s*:(?:\s|$)(.*)$"#).expect("valid regex"));

struct Parser<'t> {
    raw: Vec<&'t str>,
    /// (number, indent, content) of every line with content.
    lines: Vec<(u64, usize, String)>,
    position: usize,
}

fn dash(content: &str) -> bool {
    content.starts_with("- ") || content == "-"
}

impl Parser<'_> {
    fn block_scalar(&mut self, indent: usize, marker: &str) -> String {
        let mut parts = Vec::new();
        while self.position < self.lines.len() && self.lines[self.position].1 > indent {
            let number = self.lines[self.position].0 as usize;
            parts.push(py::strip(self.raw[number - 1]).to_string());
            self.position += 1;
        }
        parts.join(if marker.starts_with('|') { "\n" } else { " " })
    }

    fn parse_block(&mut self, indent: usize, depth: usize) -> Result<YamlNode, Failure> {
        if depth > MAX_DEPTH {
            return Err(Failure::Gone);
        }
        let (number, _, content) = self.lines.get(self.position).cloned().ok_or(Failure::Gone)?;
        if dash(&content) {
            let mut items = Vec::new();
            let mut lines = IndexMap::new();
            while self.position < self.lines.len()
                && self.lines[self.position].1 == indent
                && dash(&self.lines[self.position].2)
            {
                let (item_number, _, item) = self.lines[self.position].clone();
                let rest = py::strip(&item[1..]).to_string();
                lines.insert(Entry::Index(items.len()), item_number);
                if rest.is_empty() {
                    self.position += 1;
                    if self.position < self.lines.len() && self.lines[self.position].1 > indent {
                        let inner = self.lines[self.position].1;
                        items.push(self.parse_block(inner, depth + 1)?.value);
                    } else {
                        items.push(Yaml::Null);
                    }
                    continue;
                }
                if ITEM_KEY.is_match(&rest) && !rest.starts_with('[') && !rest.starts_with('{') {
                    // A mapping that starts on the dash line: its keys are indented past the dash.
                    self.lines[self.position] = (item_number, indent + 2, rest);
                    items.push(self.parse_block(indent + 2, depth + 1)?.value);
                    continue;
                }
                items.push(scalar(&rest, depth + 1)?);
                self.position += 1;
            }
            return Ok(YamlNode {
                value: Yaml::List(items),
                line: number,
                lines,
            });
        }
        let mut map = IndexMap::new();
        let mut lines = IndexMap::new();
        while self.position < self.lines.len() && self.lines[self.position].1 == indent {
            let (key_number, _, content) = self.lines[self.position].clone();
            let Some(found) = KEY.captures(&content) else {
                self.position += 1;
                continue;
            };
            let key = key_of(scalar(&found[1], depth + 1)?)?;
            let rest = py::strip(&found[2]).to_string();
            lines.insert(Entry::Key(key.clone()), key_number);
            self.position += 1;
            let value = if matches!(rest.as_str(), "|" | ">" | "|-" | ">-" | "|+" | ">+") {
                Yaml::Str(self.block_scalar(indent, &rest))
            } else if rest.is_empty() {
                if self.position < self.lines.len() && self.lines[self.position].1 > indent {
                    let inner = self.lines[self.position].1;
                    self.parse_block(inner, depth + 1)?.value
                } else if self.position < self.lines.len()
                    && self.lines[self.position].1 == indent
                    && self.lines[self.position].2.starts_with("- ")
                {
                    self.parse_block(indent, depth + 1)?.value // a sequence at the key's own indent
                } else {
                    Yaml::Null
                }
            } else {
                scalar(&rest, depth + 1)?
            };
            map.insert(key, value);
        }
        Ok(YamlNode {
            value: Yaml::Map(map),
            line: number,
            lines,
        })
    }
}

/// `parse_yaml`: the first document's root node; `Ok(None)` for an empty file or one past the nesting limit;
/// `Err` where the source raised.
pub fn parse_yaml(text: &str) -> Result<Option<YamlNode>, Unhashable> {
    let raw = py::splitlines(text);
    let mut lines = Vec::new();
    for (number, line) in raw.iter().enumerate().map(|(i, line)| (i as u64 + 1, *line)) {
        let bare = py::strip(line);
        if (bare == "---" || bare == "...") && !lines.is_empty() {
            break;
        }
        if bare == "---" {
            continue;
        }
        let content = strip_comment(&expand_tabs(line, 2));
        if py::strip(&content).is_empty() {
            continue;
        }
        let indent = content.len() - content.trim_start_matches(' ').len();
        lines.push((number, indent, py::strip(&content).to_string()));
    }
    if lines.is_empty() {
        return Ok(None);
    }
    let first = lines[0].1;
    let mut parser = Parser {
        raw,
        lines,
        position: 0,
    };
    match parser.parse_block(first, 0) {
        Ok(node) => Ok(Some(node)),
        Err(Failure::Gone) => Ok(None),
        Err(Failure::Unhashable) => Err(Unhashable),
    }
}

/// A key of [`yaml_get`]: a mapping key or a list index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step<'k> {
    Key(&'k str),
    Index(usize),
}

/// `_yaml_get`: follow keys and indexes; `None` (Python's None) when a step does not apply.
pub fn yaml_get<'v>(value: &'v Yaml, keys: &[Step<'_>]) -> Option<&'v Yaml> {
    let mut value = value;
    for key in keys {
        value = match (value, key) {
            (Yaml::Map(map), Step::Key(key)) => map.get(&YamlKey::Str(key.to_string()))?,
            (Yaml::Map(_), Step::Index(_)) => return None,
            (Yaml::List(items), Step::Index(index)) => items.get(*index)?,
            _ => return None,
        };
    }
    Some(value)
}
