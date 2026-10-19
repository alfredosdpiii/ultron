//! Pi's session tree selector (`TreeSelectorComponent`, modes/interactive/components/tree-selector.ts): every branch
//! of the session as an indented tree with connectors and gutters, the active path marked, folding, filters, search
//! and labels. It reads Pi's `getTree()` nodes (`{entry, children, label?, labelTimestamp?}`) and returns
//! [`TreeAction`]s; the client navigates, copies or saves labels.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use serde_json::Value;
use tui::components::{Component, Text};
use tui::input::Input;
use tui::keybindings::KeybindingsManager;
use tui::theme::Theme;
use tui::utils::{slice_with_width, truncate_to_width, visible_width, wrap_text_with_ansi};

use crate::components::{bold, fg, format_key_text, italic};

const TREE_GUTTER_WIDTH: usize = 2;
const MIN_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 4;
const MAX_VISIBLE_ANCHOR_CONTENT_WIDTH: usize = 20;
const MIN_ANCHOR_CONTEXT_WIDTH: usize = 2;
const MAX_ANCHOR_CONTEXT_WIDTH: usize = 12;

/// `FilterMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterMode {
    Default,
    NoTools,
    UserOnly,
    LabeledOnly,
    All,
}

impl FilterMode {
    /// The `treeFilterMode` setting value.
    pub fn parse(value: &str) -> FilterMode {
        match value {
            "no-tools" => FilterMode::NoTools,
            "user-only" => FilterMode::UserOnly,
            "labeled-only" => FilterMode::LabeledOnly,
            "all" => FilterMode::All,
            _ => FilterMode::Default,
        }
    }
}

const MODES: [FilterMode; 5] = [
    FilterMode::Default,
    FilterMode::NoTools,
    FilterMode::UserOnly,
    FilterMode::LabeledOnly,
    FilterMode::All,
];

/// What a key did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeAction {
    Selected(String),
    Cancelled,
    /// Copy the selected entry's text (`None`: it has none).
    Copy(Option<String>),
    /// A label was saved (`None` removes it).
    Label(String, Option<String>),
    /// Restore the working tree's files to the selected entry's checkpoint (after a confirmation).
    RestoreFiles(String),
}

#[derive(Clone, Debug)]
struct Gutter {
    position: usize,
    show: bool,
}

/// One session tree node.
struct Node {
    entry: Value,
    /// The client marked the entry as one with a working-tree checkpoint (Rust-only `checkpoint: true`).
    checkpoint: bool,
    children: Vec<usize>,
    label: Option<String>,
    label_timestamp: Option<String>,
}

#[derive(Clone)]
struct Flat {
    node: usize,
    indent: usize,
    show_connector: bool,
    is_last: bool,
    gutters: Vec<Gutter>,
    virtual_root_child: bool,
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}

fn id_of(entry: &Value) -> &str {
    s(entry, "id").unwrap_or("")
}

fn parent_of(entry: &Value) -> Option<&str> {
    s(entry, "parentId")
}

fn full_content(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}

/// `extractContent`: the first 200 UTF-16 units.
fn content(content: &Value) -> String {
    let full = full_content(content);
    let units: Vec<u16> = full.encode_utf16().take(200).collect();
    String::from_utf16_lossy(&units)
}

fn has_text(content: &Value) -> bool {
    match content {
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(blocks) => blocks
            .iter()
            .any(|b| b["type"] == "text" && b["text"].as_str().is_some_and(|t| !t.trim().is_empty())),
        _ => false,
    }
}

fn normalize(text: &str) -> String {
    text.replace(['\n', '\t'], " ").trim().to_string()
}

/// `JSON.stringify(value).slice(0, n)` on UTF-16 units.
fn js_slice(text: &str, n: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
}

/// `formatToolCall` (paths under `home` shortened to `~`).
fn format_tool_call(name: &str, args: &Value, home: &str) -> String {
    let shorten = |p: String| match p.strip_prefix(home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => p,
    };
    let text = |k: &str| match &args[k] {
        Value::String(v) if !v.is_empty() => Some(v.clone()),
        Value::Null | Value::Bool(false) => None,
        Value::String(_) => None,
        other => Some(other.to_string()),
    };
    let path = || shorten(text("path").or_else(|| text("file_path")).unwrap_or_default());
    match name {
        "read" => {
            let mut display = path();
            let offset = args["offset"].as_i64();
            let limit = args["limit"].as_i64();
            if offset.is_some() || limit.is_some() {
                let start = offset.unwrap_or(1);
                let end = limit.map(|l| (start + l - 1).to_string()).unwrap_or_default();
                display += &format!(
                    ":{start}{}",
                    if end.is_empty() || end == "0" {
                        String::new()
                    } else {
                        format!("-{end}")
                    }
                );
            }
            format!("[read: {display}]")
        }
        "write" => format!("[write: {}]", path()),
        "edit" => format!("[edit: {}]", path()),
        "bash" => {
            let raw = text("command").unwrap_or_default();
            let cmd = js_slice(normalize(&raw).as_str(), 50);
            let more = if raw.encode_utf16().count() > 50 { "..." } else { "" };
            format!("[bash: {cmd}{more}]")
        }
        "grep" => format!(
            "[grep: /{}/ in {}]",
            text("pattern").unwrap_or_default(),
            shorten(text("path").unwrap_or_else(|| ".".into()))
        ),
        "find" => format!(
            "[find: {} in {}]",
            text("pattern").unwrap_or_default(),
            shorten(text("path").unwrap_or_else(|| ".".into()))
        ),
        "ls" => format!("[ls: {}]", shorten(text("path").unwrap_or_else(|| ".".into()))),
        _ => {
            let json = args.to_string();
            let more = if json.encode_utf16().count() > 40 { "..." } else { "" };
            format!("[{name}: {}{more}]", js_slice(&json, 40))
        }
    }
}

/// `TreeList`.
struct TreeList {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    home: String,
    nodes: Vec<Node>,
    flat: Vec<Flat>,
    filtered: Vec<Flat>,
    selected: usize,
    leaf: Option<String>,
    max_visible: usize,
    filter: FilterMode,
    query: String,
    tool_calls: HashMap<String, (String, Value)>,
    multiple_roots: bool,
    show_label_timestamps: bool,
    active_path: HashSet<String>,
    visible_parent: HashMap<String, Option<String>>,
    visible_children: HashMap<Option<String>, Vec<String>>,
    last_selected: Option<String>,
    folded: HashSet<String>,
}

impl TreeList {
    fn flat_by_id(&self) -> HashMap<String, usize> {
        self.flat
            .iter()
            .enumerate()
            .map(|(i, f)| (id_of(&self.nodes[f.node].entry).to_string(), i))
            .collect()
    }

    fn entry(&self, f: &Flat) -> &Value {
        &self.nodes[f.node].entry
    }

    fn find_nearest_visible(&self, entry_id: Option<&str>) -> usize {
        if self.filtered.is_empty() {
            return 0;
        }
        let by_id = self.flat_by_id();
        let visible: HashMap<&str, usize> = self
            .filtered
            .iter()
            .enumerate()
            .map(|(i, f)| (id_of(self.entry(f)), i))
            .collect();
        let mut current = entry_id.map(str::to_string);
        while let Some(id) = current {
            if let Some(i) = visible.get(id.as_str()) {
                return *i;
            }
            let Some(&at) = by_id.get(&id) else { break };
            current = parent_of(self.entry(&self.flat[at])).map(str::to_string);
        }
        self.filtered.len() - 1
    }

    fn build_active_path(&mut self) {
        self.active_path.clear();
        let by_id = self.flat_by_id();
        let mut current = self.leaf.clone();
        while let Some(id) = current {
            self.active_path.insert(id.clone());
            let Some(&at) = by_id.get(&id) else { break };
            current = parent_of(self.entry(&self.flat[at])).map(str::to_string);
        }
    }

    fn flatten(&mut self, roots: &[usize]) {
        let mut result = Vec::new();
        self.tool_calls.clear();
        // Which subtrees hold the active leaf (post-order).
        let mut contains: HashMap<usize, bool> = HashMap::new();
        let mut order = Vec::new();
        let mut stack: Vec<usize> = roots.to_vec();
        while let Some(n) = stack.pop() {
            order.push(n);
            for &c in self.nodes[n].children.iter().rev() {
                stack.push(c);
            }
        }
        for &n in order.iter().rev() {
            let mut has = self.leaf.as_deref() == Some(id_of(&self.nodes[n].entry));
            for c in &self.nodes[n].children {
                if contains.get(c) == Some(&true) {
                    has = true;
                }
            }
            contains.insert(n, has);
        }
        let multiple = roots.len() > 1;
        let mut ordered: Vec<usize> = roots.to_vec();
        // Stable: the root holding the active leaf first.
        ordered.sort_by_key(|r| !contains.get(r).copied().unwrap_or(false));
        type Item = (usize, usize, bool, bool, bool, Vec<Gutter>, bool);
        let mut stack: Vec<Item> = Vec::new();
        for (i, &r) in ordered.iter().enumerate().rev() {
            let last = i == ordered.len() - 1;
            stack.push((r, usize::from(multiple), multiple, multiple, last, Vec::new(), multiple));
        }
        while let Some((n, indent, just_branched, show_connector, is_last, gutters, virtual_root)) = stack.pop() {
            let entry = &self.nodes[n].entry;
            if entry["type"] == "message" && entry["message"]["role"] == "assistant" {
                for block in entry["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] == "toolCall" {
                        self.tool_calls.insert(
                            s(block, "id").unwrap_or("").to_string(),
                            (s(block, "name").unwrap_or("").to_string(), block["arguments"].clone()),
                        );
                    }
                }
            }
            result.push(Flat {
                node: n,
                indent,
                show_connector,
                is_last,
                gutters: gutters.clone(),
                virtual_root_child: virtual_root,
            });
            let children = &self.nodes[n].children;
            let multiple_children = children.len() > 1;
            let ordered: Vec<usize> = children
                .iter()
                .filter(|c| contains.get(c) == Some(&true))
                .chain(children.iter().filter(|c| contains.get(c) != Some(&true)))
                .copied()
                .collect();
            let child_indent = if multiple_children || (just_branched && indent > 0) {
                indent + 1
            } else {
                indent
            };
            let child_gutters = self.child_gutters(indent, show_connector, virtual_root, is_last, &gutters);
            for (i, &c) in ordered.iter().enumerate().rev() {
                let last = i == ordered.len() - 1;
                stack.push((
                    c,
                    child_indent,
                    multiple_children,
                    multiple_children,
                    last,
                    child_gutters.clone(),
                    false,
                ));
            }
        }
        self.flat = result;
    }

    fn child_gutters(
        &self,
        indent: usize,
        show_connector: bool,
        virtual_root: bool,
        is_last: bool,
        gutters: &[Gutter],
    ) -> Vec<Gutter> {
        let displayed = show_connector && !virtual_root;
        let display_indent = if self.multiple_roots {
            indent.saturating_sub(1)
        } else {
            indent
        };
        let mut out = gutters.to_vec();
        if displayed {
            out.push(Gutter {
                position: display_indent.saturating_sub(1),
                show: !is_last,
            });
        }
        out
    }

    fn searchable_text(&self, n: usize) -> String {
        let node = &self.nodes[n];
        let entry = &node.entry;
        let mut parts: Vec<String> = Vec::new();
        if let Some(label) = &node.label {
            parts.push(label.clone());
        }
        match s(entry, "type").unwrap_or("") {
            "message" => {
                let m = &entry["message"];
                parts.push(s(m, "role").unwrap_or("").to_string());
                if !m["content"].is_null() {
                    parts.push(content(&m["content"]));
                }
                if m["role"] == "bashExecution"
                    && let Some(c) = s(m, "command").filter(|c| !c.is_empty())
                {
                    parts.push(c.to_string());
                }
            }
            "custom_message" => {
                parts.push(s(entry, "customType").unwrap_or("").to_string());
                parts.push(match &entry["content"] {
                    Value::String(t) => t.clone(),
                    other => content(other),
                });
            }
            "compaction" => parts.push("compaction".into()),
            "branch_summary" => {
                parts.push("branch summary".into());
                parts.push(s(entry, "summary").unwrap_or("").to_string());
            }
            "session_info" => {
                parts.push("title".into());
                if let Some(name) = s(entry, "name").filter(|n| !n.is_empty()) {
                    parts.push(name.to_string());
                }
            }
            "model_change" => {
                parts.push("model".into());
                parts.push(s(entry, "modelId").unwrap_or("").to_string());
            }
            "thinking_level_change" => {
                parts.push("thinking".into());
                parts.push(s(entry, "thinkingLevel").unwrap_or("").to_string());
            }
            "custom" => {
                parts.push("custom".into());
                parts.push(s(entry, "customType").unwrap_or("").to_string());
            }
            "context_edit" => {
                parts.push("context edit".into());
                parts.push(
                    if entry["replacement"].is_null() {
                        "omit"
                    } else {
                        "replace"
                    }
                    .into(),
                );
                parts.push(s(entry, "targetId").unwrap_or("").to_string());
            }
            "label" => {
                parts.push("label".into());
                parts.push(s(entry, "label").unwrap_or("").to_string());
            }
            _ => {}
        }
        parts.join(" ")
    }

    fn apply_filter(&mut self) {
        if !self.filtered.is_empty() {
            self.last_selected = self
                .filtered
                .get(self.selected)
                .map(|f| id_of(&self.nodes[f.node].entry).to_string())
                .or(self.last_selected.take());
        }
        let lower = self.query.to_lowercase();
        let tokens: Vec<&str> = lower.split_whitespace().collect();
        let mut filtered = Vec::new();
        for f in &self.flat {
            let node = &self.nodes[f.node];
            let entry = &node.entry;
            let kind = s(entry, "type").unwrap_or("");
            if kind == "usage" {
                continue;
            }
            let is_leaf = self.leaf.as_deref() == Some(id_of(entry));
            let role = entry["message"]["role"].as_str().unwrap_or("");
            if kind == "message" && role == "assistant" && !is_leaf {
                let stop = s(&entry["message"], "stopReason");
                let error_or_aborted = stop.is_some_and(|r| !r.is_empty() && r != "stop" && r != "toolUse");
                if !has_text(&entry["message"]["content"]) && !error_or_aborted {
                    continue;
                }
            }
            let settings = matches!(
                kind,
                "label" | "context_edit" | "custom" | "model_change" | "thinking_level_change" | "session_info"
            );
            let passes = match self.filter {
                FilterMode::UserOnly => kind == "message" && role == "user",
                FilterMode::NoTools => !settings && !(kind == "message" && role == "toolResult"),
                FilterMode::LabeledOnly => node.label.is_some(),
                FilterMode::All => true,
                FilterMode::Default => !settings,
            };
            if !passes {
                continue;
            }
            if !tokens.is_empty() {
                let text = self.searchable_text(f.node).to_lowercase();
                if !tokens.iter().all(|t| text.contains(t)) {
                    continue;
                }
            }
            filtered.push(f.clone());
        }
        if !self.folded.is_empty() {
            let mut skip: HashSet<String> = HashSet::new();
            for f in &self.flat {
                let entry = &self.nodes[f.node].entry;
                if let Some(parent) = parent_of(entry)
                    && (self.folded.contains(parent) || skip.contains(parent))
                {
                    skip.insert(id_of(entry).to_string());
                }
            }
            filtered.retain(|f| !skip.contains(id_of(&self.nodes[f.node].entry)));
        }
        self.filtered = filtered;
        self.recalculate();
        if let Some(last) = self.last_selected.clone() {
            self.selected = self.find_nearest_visible(Some(&last));
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
        if !self.filtered.is_empty() {
            self.last_selected = self
                .filtered
                .get(self.selected)
                .map(|f| id_of(&self.nodes[f.node].entry).to_string());
        }
    }

    /// `recalculateVisualStructure`.
    fn recalculate(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        let visible: HashSet<String> = self
            .filtered
            .iter()
            .map(|f| id_of(&self.nodes[f.node].entry).to_string())
            .collect();
        let by_id = self.flat_by_id();
        let parent_id = |id: &str| -> Option<String> {
            by_id
                .get(id)
                .and_then(|&i| parent_of(&self.nodes[self.flat[i].node].entry))
                .map(str::to_string)
        };
        let mut visible_parent: HashMap<String, Option<String>> = HashMap::new();
        let mut visible_children: HashMap<Option<String>, Vec<String>> = HashMap::new();
        visible_children.insert(None, Vec::new());
        for f in &self.filtered {
            let id = id_of(&self.nodes[f.node].entry).to_string();
            let mut ancestor = parent_id(&id);
            while let Some(a) = &ancestor {
                if visible.contains(a) {
                    break;
                }
                ancestor = parent_id(a);
            }
            visible_parent.insert(id.clone(), ancestor.clone());
            visible_children.entry(ancestor).or_default().push(id);
        }
        let roots = visible_children[&None].clone();
        self.multiple_roots = roots.len() > 1;
        let index: HashMap<String, usize> = self
            .filtered
            .iter()
            .enumerate()
            .map(|(i, f)| (id_of(&self.nodes[f.node].entry).to_string(), i))
            .collect();
        type Item = (String, usize, bool, bool, bool, Vec<Gutter>, bool);
        let multiple = self.multiple_roots;
        let mut stack: Vec<Item> = Vec::new();
        for (i, r) in roots.iter().enumerate().rev() {
            let last = i == roots.len() - 1;
            stack.push((
                r.clone(),
                usize::from(multiple),
                multiple,
                multiple,
                last,
                Vec::new(),
                multiple,
            ));
        }
        while let Some((id, indent, just_branched, show_connector, is_last, gutters, virtual_root)) = stack.pop() {
            let Some(&at) = index.get(&id) else { continue };
            let child_gutters = self.child_gutters(indent, show_connector, virtual_root, is_last, &gutters);
            let f = &mut self.filtered[at];
            f.indent = indent;
            f.show_connector = show_connector;
            f.is_last = is_last;
            f.gutters = gutters;
            f.virtual_root_child = virtual_root;
            let children = visible_children.get(&Some(id)).cloned().unwrap_or_default();
            let multiple_children = children.len() > 1;
            let child_indent = if multiple_children || (just_branched && indent > 0) {
                indent + 1
            } else {
                indent
            };
            for (i, c) in children.iter().enumerate().rev() {
                let last = i == children.len() - 1;
                stack.push((
                    c.clone(),
                    child_indent,
                    multiple_children,
                    multiple_children,
                    last,
                    child_gutters.clone(),
                    false,
                ));
            }
        }
        self.visible_parent = visible_parent;
        self.visible_children = visible_children;
    }

    fn status_labels(&self) -> String {
        let mut labels = match self.filter {
            FilterMode::NoTools => " [no-tools]",
            FilterMode::UserOnly => " [user]",
            FilterMode::LabeledOnly => " [labeled]",
            FilterMode::All => " [all]",
            FilterMode::Default => "",
        }
        .to_string();
        if self.show_label_timestamps {
            labels += " [+label time]";
        }
        labels
    }

    fn is_foldable(&self, id: &str) -> bool {
        let Some(children) = self.visible_children.get(&Some(id.to_string())) else {
            return false;
        };
        if children.is_empty() {
            return false;
        }
        match self.visible_parent.get(id).cloned().flatten() {
            None => true,
            Some(parent) => self.visible_children.get(&Some(parent)).is_some_and(|s| s.len() > 1),
        }
    }

    fn branch_segment_start(&self, up: bool) -> usize {
        let Some(selected) = self.filtered.get(self.selected) else {
            return self.selected;
        };
        let index: HashMap<String, usize> = self
            .filtered
            .iter()
            .enumerate()
            .map(|(i, f)| (id_of(&self.nodes[f.node].entry).to_string(), i))
            .collect();
        let mut current = id_of(&self.nodes[selected.node].entry).to_string();
        if !up {
            loop {
                let children = self
                    .visible_children
                    .get(&Some(current.clone()))
                    .cloned()
                    .unwrap_or_default();
                match children.len() {
                    0 => return index[&current],
                    1 => current = children[0].clone(),
                    _ => return index[&children[0]],
                }
            }
        }
        loop {
            let Some(parent) = self.visible_parent.get(&current).cloned().flatten() else {
                return index[&current];
            };
            let siblings = self
                .visible_children
                .get(&Some(parent.clone()))
                .cloned()
                .unwrap_or_default();
            if siblings.len() > 1 {
                let start = index[&current];
                if start < self.selected {
                    return start;
                }
            }
            current = parent;
        }
    }

    fn display_text(&self, n: usize, selected: bool) -> String {
        let t = &self.theme;
        let entry = &self.nodes[n].entry;
        let result = match s(entry, "type").unwrap_or("") {
            "message" => {
                let m = &entry["message"];
                match s(m, "role").unwrap_or("") {
                    "user" => fg(t, "accent", "user: ") + &normalize(&content(&m["content"])),
                    "assistant" => {
                        let text = normalize(&content(&m["content"]));
                        let head = fg(t, "success", "assistant: ");
                        if !text.is_empty() {
                            head + &text
                        } else if m["stopReason"] == "aborted" {
                            head + &fg(t, "muted", "(aborted)")
                        } else if let Some(error) = s(m, "errorMessage").filter(|e| !e.is_empty()) {
                            head + &fg(t, "error", &js_slice(&normalize(error), 80))
                        } else {
                            head + &fg(t, "muted", "(no content)")
                        }
                    }
                    "toolResult" => {
                        let call = s(m, "toolCallId").and_then(|id| self.tool_calls.get(id));
                        match call {
                            Some((name, args)) => fg(t, "muted", &format_tool_call(name, args, &self.home)),
                            None => fg(t, "muted", &format!("[{}]", s(m, "toolName").unwrap_or("tool"))),
                        }
                    }
                    "bashExecution" => fg(
                        t,
                        "dim",
                        &format!("[bash]: {}", normalize(s(m, "command").unwrap_or(""))),
                    ),
                    role => fg(t, "dim", &format!("[{role}]")),
                }
            }
            "custom_message" => {
                let text = match &entry["content"] {
                    Value::String(text) => text.clone(),
                    other => full_content(other),
                };
                fg(
                    t,
                    "customMessageLabel",
                    &format!("[{}]: ", s(entry, "customType").unwrap_or("")),
                ) + &normalize(&text)
            }
            "compaction" => {
                let tokens = (entry["tokensBefore"].as_f64().unwrap_or(0.0) / 1000.0).round();
                fg(t, "borderAccent", &format!("[compaction: {tokens}k tokens]"))
            }
            "branch_summary" => fg(t, "warning", "[branch summary]: ") + &normalize(s(entry, "summary").unwrap_or("")),
            "model_change" => fg(t, "dim", &format!("[model: {}]", s(entry, "modelId").unwrap_or(""))),
            "thinking_level_change" => fg(
                t,
                "dim",
                &format!("[thinking: {}]", s(entry, "thinkingLevel").unwrap_or("")),
            ),
            "custom" => fg(t, "dim", &format!("[custom: {}]", s(entry, "customType").unwrap_or(""))),
            "context_edit" => fg(
                t,
                "dim",
                &format!(
                    "[context {}: {}]",
                    if entry["replacement"].is_null() {
                        "omit"
                    } else {
                        "replace"
                    },
                    s(entry, "targetId").unwrap_or("")
                ),
            ),
            "label" => fg(
                t,
                "dim",
                &format!("[label: {}]", s(entry, "label").unwrap_or("(cleared)")),
            ),
            "session_info" => match s(entry, "name").filter(|n| !n.is_empty()) {
                Some(name) => fg(t, "dim", "[title: ") + &fg(t, "dim", name) + &fg(t, "dim", "]"),
                None => fg(t, "dim", "[title: ") + &italic(&fg(t, "dim", "empty")) + &fg(t, "dim", "]"),
            },
            _ => String::new(),
        };
        if selected { bold(&result) } else { result }
    }

    fn copy_text(&self, n: usize) -> Option<String> {
        let entry = &self.nodes[n].entry;
        let text = match s(entry, "type").unwrap_or("") {
            "message" => {
                let m = &entry["message"];
                if m["role"] == "bashExecution" {
                    s(m, "command").map(str::to_string)
                } else if m.get("content").is_some() {
                    let text = full_content(&m["content"]);
                    if text.is_empty() && m["role"] == "assistant" {
                        s(m, "errorMessage").map(str::to_string)
                    } else {
                        Some(text)
                    }
                } else {
                    None
                }
            }
            "custom_message" => Some(full_content(&entry["content"])),
            "compaction" | "branch_summary" => s(entry, "summary").map(str::to_string),
            _ => None,
        };
        text.filter(|t| !t.trim().is_empty())
    }

    fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        if self.filtered.is_empty() {
            return vec![
                truncate_to_width(&fg(&t, "muted", "  No entries found"), width, "...", false),
                truncate_to_width(
                    &fg(&t, "muted", &format!("  (0/0){}", self.status_labels())),
                    width,
                    "...",
                    false,
                ),
            ];
        }
        let n = self.filtered.len() as isize;
        let max = self.max_visible as isize;
        let start = (self.selected as isize - max / 2).min(n - max).max(0) as usize;
        let end = (start + self.max_visible).min(self.filtered.len());
        let mut rows = Vec::new();
        for i in start..end {
            let f = &self.filtered[i];
            let entry = &self.nodes[f.node].entry;
            let id = id_of(entry).to_string();
            let selected = i == self.selected;
            let cursor = if selected {
                fg(&t, "accent", "› ")
            } else {
                "  ".to_string()
            };
            let display_indent = if self.multiple_roots {
                f.indent.saturating_sub(1)
            } else {
                f.indent
            };
            let connector = f.show_connector && !f.virtual_root_child;
            let connector_position = if connector { display_indent as isize - 1 } else { -1 };
            let folded = self.folded.contains(&id);
            let mut prefix = String::new();
            for c in 0..display_indent * 3 {
                let level = c / 3;
                let pos = c % 3;
                if let Some(g) = f.gutters.iter().find(|g| g.position == level) {
                    prefix.push(if pos == 0 && g.show { '│' } else { ' ' });
                } else if connector && level as isize == connector_position {
                    prefix.push(match pos {
                        0 if f.is_last => '└',
                        0 => '├',
                        1 if folded => '⊞',
                        1 if self.is_foldable(&id) => '⊟',
                        1 => '─',
                        _ => ' ',
                    });
                } else {
                    prefix.push(' ');
                }
            }
            let fold_marker = if folded && !connector {
                fg(&t, "accent", "⊞ ")
            } else {
                String::new()
            };
            let path_marker = if self.active_path.contains(&id) {
                fg(&t, "accent", "• ")
            } else {
                String::new()
            };
            let node = &self.nodes[f.node];
            let label = node
                .label
                .as_ref()
                .map(|l| fg(&t, "warning", &format!("[{l}] ")))
                .unwrap_or_default();
            let label_time = match (&node.label, &node.label_timestamp) {
                (Some(_), Some(ts)) if self.show_label_timestamps => {
                    fg(&t, "muted", &format!("{} ", format_label_timestamp(ts)))
                }
                _ => String::new(),
            };
            let text = self.display_text(f.node, selected);
            let prefix_part = fg(&t, "dim", &prefix) + &fold_marker + &path_marker;
            let anchor = visible_width(&prefix_part);
            let mut gutter = cursor;
            let mut body = prefix_part + &label + &label_time + &text;
            if selected {
                gutter = t.bg("selectedBg", &gutter).unwrap_or(gutter);
                body = t.bg("selectedBg", &body).unwrap_or(body);
            }
            let body_width = visible_width(&body);
            rows.push((gutter, body, anchor, body_width, selected));
        }
        let mut lines = horizontal_viewport(&rows, width);
        lines.push(truncate_to_width(
            &fg(
                &t,
                "muted",
                &format!(
                    "  ({}/{}){}",
                    self.selected + 1,
                    self.filtered.len(),
                    self.status_labels()
                ),
            ),
            width,
            "...",
            false,
        ));
        lines
    }

    fn set_filter(&mut self, mode: FilterMode) {
        self.filter = mode;
        self.folded.clear();
        self.apply_filter();
    }

    fn handle_input(&mut self, data: &str) -> Option<TreeAction> {
        let k = |id: &str| self.keys.matches(data, id);
        let n = self.filtered.len();
        if k("tui.select.up") {
            self.selected = if self.selected == 0 {
                n.saturating_sub(1)
            } else {
                self.selected - 1
            };
        } else if k("tui.select.down") {
            self.selected = if self.selected + 1 >= n { 0 } else { self.selected + 1 };
        } else if k("app.tree.foldOrUp") {
            let current = self
                .filtered
                .get(self.selected)
                .map(|f| id_of(&self.nodes[f.node].entry).to_string());
            match current {
                Some(id) if self.is_foldable(&id) && !self.folded.contains(&id) => {
                    self.folded.insert(id);
                    self.apply_filter();
                }
                _ => self.selected = self.branch_segment_start(true),
            }
        } else if k("app.tree.unfoldOrDown") {
            let current = self
                .filtered
                .get(self.selected)
                .map(|f| id_of(&self.nodes[f.node].entry).to_string());
            match current {
                Some(id) if self.folded.contains(&id) => {
                    self.folded.remove(&id);
                    self.apply_filter();
                }
                _ => self.selected = self.branch_segment_start(false),
            }
        } else if k("tui.editor.cursorLeft") || k("tui.select.pageUp") {
            self.selected = self.selected.saturating_sub(self.max_visible);
        } else if k("tui.editor.cursorRight") || k("tui.select.pageDown") {
            self.selected = (self.selected + self.max_visible).min(n.saturating_sub(1));
        } else if k("tui.select.confirm") {
            return self
                .filtered
                .get(self.selected)
                .map(|f| TreeAction::Selected(id_of(&self.nodes[f.node].entry).to_string()));
        } else if k("app.message.copy") {
            return Some(TreeAction::Copy(
                self.filtered.get(self.selected).and_then(|f| self.copy_text(f.node)),
            ));
        } else if k("tui.select.cancel") {
            if self.query.is_empty() {
                return Some(TreeAction::Cancelled);
            }
            self.query.clear();
            self.folded.clear();
            self.apply_filter();
        } else if k("app.tree.filter.default") {
            self.set_filter(FilterMode::Default);
        } else if k("app.tree.filter.noTools") {
            self.toggle(FilterMode::NoTools);
        } else if k("app.tree.filter.userOnly") {
            self.toggle(FilterMode::UserOnly);
        } else if k("app.tree.filter.labeledOnly") {
            self.toggle(FilterMode::LabeledOnly);
        } else if k("app.tree.filter.all") {
            self.toggle(FilterMode::All);
        } else if k("app.tree.filter.cycleBackward") {
            let i = MODES.iter().position(|m| *m == self.filter).unwrap_or(0);
            self.set_filter(MODES[(i + MODES.len() - 1) % MODES.len()]);
        } else if k("app.tree.filter.cycleForward") {
            let i = MODES.iter().position(|m| *m == self.filter).unwrap_or(0);
            self.set_filter(MODES[(i + 1) % MODES.len()]);
        } else if k("tui.editor.deleteCharBackward") {
            if self.query.pop().is_some() {
                self.folded.clear();
                self.apply_filter();
            }
        } else if k("app.tree.toggleLabelTimestamp") {
            self.show_label_timestamps = !self.show_label_timestamps;
        } else if !data.is_empty()
            && !data
                .chars()
                .any(|c| (c as u32) < 32 || c as u32 == 0x7f || (0x80..=0x9f).contains(&(c as u32)))
        {
            self.query.push_str(data);
            self.folded.clear();
            self.apply_filter();
        }
        None
    }

    fn toggle(&mut self, mode: FilterMode) {
        self.set_filter(if self.filter == mode { FilterMode::Default } else { mode });
    }
}

/// `formatLabelTimestamp`: the label's local time (UTC here), with the date when not today.
fn format_label_timestamp(ts: &str) -> String {
    // ISO `YYYY-MM-DDTHH:MM...`; the TUI process runs with the user's zone, shown as stored.
    let date = ts.get(..10).unwrap_or("");
    let time = ts.get(11..16).unwrap_or("");
    let today = {
        let days = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() / 86_400) as i64;
        civil_date(days)
    };
    if date == today {
        return time.to_string();
    }
    let year = date.get(..4).unwrap_or("");
    let month: u32 = date.get(5..7).and_then(|m| m.parse().ok()).unwrap_or(0);
    let day: u32 = date.get(8..10).and_then(|d| d.parse().ok()).unwrap_or(0);
    if Some(year) == today.get(..4) {
        format!("{month}/{day} {time}")
    } else {
        format!("{}/{month}/{day} {time}", year.get(2..).unwrap_or(""))
    }
}

/// `YYYY-MM-DD` of a day count since the epoch.
fn civil_date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `renderHorizontalViewport`: the gutter stays, the bodies pan when the selected row's anchor is too far right.
fn horizontal_viewport(rows: &[(String, String, usize, usize, bool)], width: usize) -> Vec<String> {
    let viewport = width.saturating_sub(TREE_GUTTER_WIDTH);
    let max_body = rows.iter().map(|r| r.3).max().unwrap_or(0);
    let max_scroll = max_body.saturating_sub(viewport);
    let mut scroll = 0;
    if let Some(selected) = rows.iter().find(|r| r.4)
        && max_scroll > 0
    {
        let min_visible = (viewport / 3).clamp(MIN_VISIBLE_ANCHOR_CONTENT_WIDTH, MAX_VISIBLE_ANCHOR_CONTENT_WIDTH);
        if selected.2 as isize > viewport as isize - min_visible as isize {
            let context = (viewport / 4).clamp(MIN_ANCHOR_CONTEXT_WIDTH, MAX_ANCHOR_CONTEXT_WIDTH);
            scroll = max_scroll.min(selected.2.saturating_sub(context));
        }
    }
    rows.iter()
        .map(|(gutter, body, ..)| {
            let line = if scroll > 0 {
                format!("{gutter}{}\x1b[0m", slice_with_width(body, scroll, viewport, true).0)
            } else {
                format!("{gutter}{body}")
            };
            truncate_to_width(&line, width, "", false)
        })
        .collect()
}

/// `TREE_HELP_ITEMS`.
const HELP: [(&[&str], &str, bool); 8] = [
    (&["tui.select.up", "tui.select.down"], "move", false),
    (&["tui.editor.cursorLeft", "tui.editor.cursorRight"], "page", false),
    (&["app.tree.foldOrUp", "app.tree.unfoldOrDown"], "branch", false),
    (&["app.message.copy"], "copy", false),
    (&["app.tree.editLabel"], "label", false),
    (&["app.tree.toggleLabelTimestamp"], "label time", false),
    (
        &[
            "app.tree.filter.default",
            "app.tree.filter.noTools",
            "app.tree.filter.userOnly",
            "app.tree.filter.labeledOnly",
            "app.tree.filter.all",
        ],
        "filters",
        true,
    ),
    (
        &["app.tree.filter.cycleForward", "app.tree.filter.cycleBackward"],
        "cycle",
        true,
    ),
];

/// `formatHelpKeys`.
fn help_keys(keys: &KeybindingsManager, ids: &[&str]) -> String {
    let first: Vec<String> = ids.iter().filter_map(|id| keys.get_keys(id).first().cloned()).collect();
    if first.is_empty() {
        return String::new();
    }
    let compact = if first.len() == 1 {
        first[0].clone()
    } else {
        let parts: Vec<(String, String)> = first
            .iter()
            .map(|k| match k.rfind('+') {
                Some(i) => (k[..=i].to_string(), k[i + 1..].to_string()),
                None => (String::new(), k.clone()),
            })
            .collect();
        let prefix = &parts[0].0;
        if !prefix.is_empty() && parts.iter().all(|p| &p.0 == prefix) {
            format!(
                "{prefix}{}",
                parts.iter().map(|p| p.1.as_str()).collect::<Vec<_>>().join("/")
            )
        } else {
            first.join("/")
        }
    };
    let text = format_key_text(&compact, false, false);
    word_replace(&text)
}

/// The `\bpageUp\b`, `\bup\b`, ... replacements of `formatHelpKeys`.
fn word_replace(text: &str) -> String {
    let mut out = String::new();
    let mut word = String::new();
    let flush = |word: &mut String, out: &mut String| {
        out.push_str(match word.as_str() {
            "pageUp" => "pgup",
            "pageDown" => "pgdn",
            "up" => "↑",
            "down" => "↓",
            "left" => "←",
            "right" => "→",
            w => w,
        });
        word.clear();
    };
    for c in text.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            word.push(c);
        } else {
            flush(&mut word, &mut out);
            out.push(c);
        }
    }
    flush(&mut word, &mut out);
    out
}

/// `TreeHelp`.
/// The help item of the file-restoring rewind, shown only when some entry has a checkpoint (so a session without
/// checkpoints shows TypeScript's help).
const RESTORE_HELP: (&[&str], &str, bool) = (&["app.tree.restoreFiles"], "restore files", false);

fn help_lines(theme: &Theme, keys: &KeybindingsManager, width: usize, checkpoints: bool) -> Vec<String> {
    let mut help = HELP.to_vec();
    if checkpoints {
        help.insert(6, RESTORE_HELP);
    }
    let items: Vec<String> = help
        .iter()
        .map(|(ids, label, label_first)| {
            let text = help_keys(keys, ids);
            if text.is_empty() {
                (*label).to_string()
            } else if *label_first {
                format!("{label} {text}")
            } else {
                format!("{text} {label}")
            }
        })
        .collect();
    let available = width.max(1);
    let (indent, separator) = ("  ", " · ");
    let mut lines = Vec::new();
    let mut current = String::new();
    for item in items {
        let indented = format!("{indent}{item}");
        let candidate = if !current.is_empty() {
            format!("{current}{separator}{item}")
        } else if visible_width(&indented) <= available {
            indented.clone()
        } else {
            item.clone()
        };
        if current.is_empty() || visible_width(&candidate) <= available {
            current = candidate;
            continue;
        }
        lines.extend(wrap_text_with_ansi(current.trim_end(), available));
        current = if visible_width(&indented) <= available {
            indented
        } else {
            item
        };
    }
    if !current.is_empty() {
        lines.extend(wrap_text_with_ansi(current.trim_end(), available));
    }
    lines.into_iter().map(|l| fg(theme, "muted", &l)).collect()
}

/// `TreeSelectorComponent`.
pub struct TreeSelector {
    theme: Rc<Theme>,
    list: TreeList,
    /// `(entry id, input)` while a label is edited.
    label_input: Option<(String, Input)>,
}

impl TreeSelector {
    /// `tree`: Pi's `getTree()` nodes; the current leaf and the entry to select first; `rows`: the terminal height.
    pub fn new(
        theme: Rc<Theme>,
        keys: KeybindingsManager,
        tree: &Value,
        (leaf, initial): (Option<&str>, Option<&str>),
        rows: usize,
        filter: FilterMode,
        home: &str,
    ) -> Self {
        let mut nodes = Vec::new();
        fn add(nodes: &mut Vec<Node>, value: &Value) -> usize {
            let index = nodes.len();
            nodes.push(Node {
                entry: value["entry"].clone(),
                checkpoint: value["checkpoint"] == true,
                children: Vec::new(),
                label: value["label"].as_str().map(str::to_string),
                label_timestamp: value["labelTimestamp"].as_str().map(str::to_string),
            });
            let children: Vec<usize> = value["children"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|c| add(nodes, c))
                .collect();
            nodes[index].children = children;
            index
        }
        let roots: Vec<usize> = tree
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| add(&mut nodes, r))
            .collect();
        let mut list = TreeList {
            theme: Rc::clone(&theme),
            keys,
            home: home.to_string(),
            nodes,
            flat: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            leaf: leaf.map(str::to_string),
            max_visible: (rows / 2).max(5),
            filter,
            query: String::new(),
            tool_calls: HashMap::new(),
            multiple_roots: roots.len() > 1,
            show_label_timestamps: false,
            active_path: HashSet::new(),
            visible_parent: HashMap::new(),
            visible_children: HashMap::new(),
            last_selected: None,
            folded: HashSet::new(),
        };
        list.flatten(&roots);
        list.build_active_path();
        list.apply_filter();
        let target = initial.map(str::to_string).or(list.leaf.clone());
        list.selected = list.find_nearest_visible(target.as_deref());
        list.last_selected = list
            .filtered
            .get(list.selected)
            .map(|f| id_of(&list.nodes[f.node].entry).to_string());
        TreeSelector {
            theme,
            list,
            label_input: None,
        }
    }

    pub fn handle_input(&mut self, data: &str) -> Option<TreeAction> {
        let keys = &self.list.keys;
        if let Some((id, input)) = self.label_input.as_mut() {
            if keys.matches(data, "tui.select.confirm") {
                let value = input.value().trim().to_string();
                let label = (!value.is_empty()).then_some(value);
                let id = id.clone();
                for node in &mut self.list.nodes {
                    if id_of(&node.entry) == id {
                        node.label_timestamp = label.as_ref().map(|_| tui_now_iso());
                        node.label = label.clone();
                    }
                }
                self.label_input = None;
                return Some(TreeAction::Label(id, label));
            }
            if keys.matches(data, "tui.select.cancel") {
                self.label_input = None;
                return None;
            }
            input.handle_input(data);
            return None;
        }
        if keys.matches(data, "app.tree.restoreFiles") {
            return self
                .list
                .filtered
                .get(self.list.selected)
                .map(|f| TreeAction::RestoreFiles(id_of(&self.list.nodes[f.node].entry).to_string()));
        }
        if keys.matches(data, "app.tree.editLabel") {
            if let Some(f) = self.list.filtered.get(self.list.selected) {
                let node = &self.list.nodes[f.node];
                let mut input = Input::new(keys.clone());
                if let Some(label) = &node.label {
                    input.set_value(label);
                }
                self.label_input = Some((id_of(&node.entry).to_string(), input));
            }
            return None;
        }
        self.list.handle_input(data)
    }
}

fn tui_now_iso() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    format!(
        "{}T{:02}:{:02}:{:02}.{:03}Z",
        civil_date(days),
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

impl Component for TreeSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let border = fg(&t, "border", &"─".repeat(width.max(1)));
        let mut lines = vec![String::new(), border.clone()];
        lines.extend(Text::new(&bold("  Session Tree"), 1, 0, None).render(width));
        let checkpoints = self.list.nodes.iter().any(|n| n.checkpoint);
        lines.extend(help_lines(&t, &self.list.keys, width, checkpoints));
        let query = &self.list.query;
        let search = if query.is_empty() {
            format!("  {}", fg(&t, "muted", "Type to search:"))
        } else {
            format!("  {} {}", fg(&t, "muted", "Type to search:"), fg(&t, "accent", query))
        };
        lines.push(truncate_to_width(&search, width, "...", false));
        lines.push(border.clone());
        lines.push(String::new());
        match self.label_input.as_mut() {
            None => lines.extend(self.list.render(width)),
            Some((_, input)) => {
                let indent = "  ";
                lines.push(truncate_to_width(
                    &format!("{indent}{}", fg(&t, "muted", "Label (empty to remove):")),
                    width,
                    "...",
                    false,
                ));
                for l in input.render(width.saturating_sub(indent.len())) {
                    lines.push(truncate_to_width(&format!("{indent}{l}"), width, "...", false));
                }
                let hint = |id: &str, d: &str| {
                    crate::components::key_hint(&t, &crate::components::key_text(&self.list.keys, id, false), d)
                };
                lines.push(truncate_to_width(
                    &format!(
                        "{indent}{}  {}",
                        hint("tui.select.confirm", "save"),
                        hint("tui.select.cancel", "cancel")
                    ),
                    width,
                    "...",
                    false,
                ));
            }
        }
        lines.push(String::new());
        lines.push(border);
        lines
    }
}
