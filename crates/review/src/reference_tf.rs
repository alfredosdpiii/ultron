//! The Terraform block scanner of the review reference (`autoreview_reference.py` `TfBlock`, `parse_terraform`):
//! top-level blocks with their line range, attributes, one level of nested blocks' attributes, and the references
//! they make (`var.x`, `local.x`, `module.x`, `data.x`, `type_name.x`). Heredoc bodies are skipped.
//!
//! Known differences from the source: none.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;

use crate::py;
use crate::reference::clip;

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid regex")
}

pub static BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r#"^\s*(resource|data|module|variable|output|locals|provider|terraform)(?:\s+"([^"]+)")?(?:\s+"([^"]+)")?\s*\{"#,
    )
});
static NESTED: LazyLock<Regex> = LazyLock::new(|| regex(r"^\s*([a-z_]+)\s*\{"));
static ATTR: LazyLock<Regex> = LazyLock::new(|| regex(r"^\s*([a-z_][a-z0-9_]*)\s*=\s*(.*)$"));
static REF: LazyLock<Regex> = LazyLock::new(|| {
    regex(r"\b(var|local|module|data)\.([A-Za-z_][\w-]*)|\b([a-z][a-z0-9_]*_[a-z0-9_]+)\.([A-Za-z_][\w-]*)\b")
});
static COMMENT: LazyLock<Regex> = LazyLock::new(|| regex(r"(#|//).*$"));
static HEREDOC: LazyLock<Regex> = LazyLock::new(|| regex(r"<<-?\s*([A-Z_]+)\s*$"));

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TfBlock {
    /// resource | data | module | variable | output | locals | provider | terraform
    pub kind: String,
    pub r#type: String,
    pub name: String,
    pub path: String,
    pub line: u64,
    pub end: u64,
    pub attrs: IndexMap<String, String>,
    pub nested: IndexMap<String, IndexMap<String, String>>,
    pub refs: BTreeSet<String>,
}

impl TfBlock {
    fn new(kind: &str, r#type: &str, name: &str, path: &str, line: u64) -> Self {
        Self {
            kind: kind.to_string(),
            r#type: r#type.to_string(),
            name: name.to_string(),
            path: path.to_string(),
            line,
            end: line,
            attrs: IndexMap::new(),
            nested: IndexMap::new(),
            refs: BTreeSet::new(),
        }
    }

    /// How other blocks name it: `type.name`, `data.type.name`, `module.m`, `output.o`, `locals`.
    pub fn address(&self) -> String {
        match self.kind.as_str() {
            "resource" => format!("{}.{}", self.r#type, self.name),
            "data" => format!("data.{}.{}", self.r#type, self.name),
            kind if self.r#type.is_empty() => kind.to_string(),
            kind => format!("{kind}.{}", self.r#type),
        }
    }
}

/// Every top-level block of a Terraform file.
pub fn parse_terraform(path: &str, text: &str) -> Vec<TfBlock> {
    let mut blocks = Vec::new();
    let lines = py::splitlines(text);
    let mut current: Option<TfBlock> = None;
    let mut depth: i64 = 0;
    let mut nested_name: Option<String> = None;
    let mut heredoc: Option<String> = None;
    for (number, line) in lines.iter().enumerate().map(|(i, line)| (i as u64 + 1, *line)) {
        if let Some(marker) = &heredoc {
            if py::strip(line) == marker {
                heredoc = None;
            }
            continue;
        }
        let stripped = COMMENT.replace(line, "");
        let Some(block) = current.as_mut() else {
            if let Some(found) = BLOCK.captures(&stripped) {
                let kind = &found[1];
                let first = found.get(2).map_or("", |m| m.as_str());
                let second = found.get(3).map_or("", |m| m.as_str());
                let name = if matches!(kind, "resource" | "data") {
                    second
                } else {
                    ""
                };
                current = Some(TfBlock::new(kind, first, name, path, number));
                depth = 1;
                nested_name = None;
            }
            continue;
        };
        let opened = stripped.matches('{').count() as i64;
        let closed = stripped.matches('}').count() as i64;
        let doc = HEREDOC.captures(&stripped);
        if let Some(doc) = &doc {
            heredoc = Some(doc[1].to_string());
        }
        // A heredoc attribute (a policy document) is skipped with its body: its lines are not the block's wiring.
        let attr = if doc.is_some() { None } else { ATTR.captures(&stripped) };
        let opens = py::rstrip(&stripped).ends_with('{');
        if let Some(attr) = &attr {
            if depth == 1 && !opens {
                block.attrs.insert(attr[1].to_string(), clip(&attr[2], 200));
            } else if depth == 2
                && !opens
                && let Some(nested) = &nested_name
            {
                block
                    .nested
                    .entry(nested.clone())
                    .or_default()
                    .insert(attr[1].to_string(), clip(&attr[2], 200));
            }
        }
        if let Some(nested) = NESTED.captures(&stripped)
            && depth == 1
            && opens
        {
            nested_name = Some(nested[1].to_string());
        }
        for found in REF.captures_iter(&stripped) {
            if let (Some(head), Some(name)) = (found.get(1), found.get(2)) {
                block.refs.insert(format!("{}.{}", head.as_str(), name.as_str()));
            } else if let (Some(head), Some(name)) = (found.get(3), found.get(4)) {
                block.refs.insert(format!("{}.{}", head.as_str(), name.as_str()));
            }
        }
        depth += opened - closed;
        if depth <= 0 {
            block.end = number;
            blocks.extend(current.take());
        }
    }
    if let Some(mut block) = current {
        block.end = lines.len() as u64;
        blocks.push(block);
    }
    blocks
}
