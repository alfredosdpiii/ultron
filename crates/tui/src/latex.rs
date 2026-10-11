//! LaTeX math to terminal Unicode: port of packages/tui/src/latex.ts (`renderLatex`). Inline math renders on one
//! line; display math stacks fractions, operator limits, scripts, cases and matrices over several lines.
//! Positions index Unicode scalar values where TypeScript indexes UTF-16 units; both see the same characters.

use std::sync::LazyLock;

use fancy_regex::Regex;

use crate::utils::{is_whitespace_char as is_ws, js_trim_end, visible_width};

const NEGATIVE_SPACE: &str = "\0";
const NAMED_OPERATOR_START: char = '\u{f0004}';
const NAMED_OPERATOR_END: char = '\u{f0005}';
const LAYOUT_MARKER_START: char = '\u{f0000}';
const LAYOUT_MARKER_END: char = '\u{f0001}';
const PROTECTED_SPACE: char = '\u{f0002}';

static SIMPLE_VALUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\p{L}\p{N}.]+$").expect("regex"));
static NUMERIC_VALUE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\p{N}.]+$").expect("regex"));
static LEADING_SPEC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[\t\n\x0B\f\r \u{a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]*\{[^}]*\}",
    )
    .expect("regex")
});
static ROW_SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\\\(?:\[[^\]\n]*\])?").expect("regex"));
static OPERATOR_MODIFIER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\\(limits|nolimits)(?![A-Za-z])").expect("regex"));

fn js_trim(s: &str) -> &str {
    s.trim_matches(is_ws)
}

fn js_trim_start(s: &str) -> &str {
    s.trim_start_matches(is_ws)
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

fn replace_characters(value: &str, map: fn(char) -> Option<&'static str>) -> Option<String> {
    value.chars().map(map).collect()
}

/// `value.trim().replace(/\s*([=+-])\s*/g, "$1")`.
fn normalize_script_value(value: &str) -> String {
    let chars: Vec<char> = js_trim(value).chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let mut j = i;
        while j < chars.len() && is_ws(chars[j]) {
            j += 1;
        }
        if j < chars.len() && matches!(chars[j], '=' | '+' | '-') {
            out.push(chars[j]);
            i = j + 1;
            while i < chars.len() && is_ws(chars[i]) {
                i += 1;
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Sub,
    Sup,
}

fn format_unicode_script(value: &str, kind: Kind) -> Option<String> {
    let map = if kind == Kind::Sub { subscript } else { superscript };
    replace_characters(&normalize_script_value(value), map)
}

fn format_script(value: &str, kind: Kind) -> String {
    let value = normalize_script_value(value);
    if let Some(unicode) = format_unicode_script(&value, kind) {
        return unicode;
    }
    let prefix = if kind == Kind::Sub { '_' } else { '^' };
    if char_count(&value) == 1
        || (kind == Kind::Sub && !value.is_empty() && value.chars().all(|c| c.is_ascii_alphabetic()))
    {
        return format!("{prefix}{value}");
    }
    format!("{prefix}({value})")
}

fn is_simple(value: &str) -> bool {
    SIMPLE_VALUE.is_match(value).unwrap_or(false)
}

fn format_fraction(numerator: &str, denominator: &str) -> String {
    let (n, d) = (js_trim(numerator), js_trim(denominator));
    let simple_d = NUMERIC_VALUE.is_match(d).unwrap_or(false) || char_count(d) == 1;
    let n = if is_simple(n) { n.to_string() } else { format!("({n})") };
    let d = if simple_d { d.to_string() } else { format!("({d})") };
    format!("{n}/{d}")
}

fn format_root(value: &str, symbol: &str) -> String {
    let value = js_trim(value);
    if is_simple(value) {
        format!("{symbol}{value}")
    } else {
        format!("{symbol}({value})")
    }
}

fn is_letter_or_number(c: char) -> bool {
    let mut b = [0u8; 4];
    is_simple(c.encode_utf8(&mut b)) && c != '.'
}

fn normalize_output(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut s = Vec::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == NAMED_OPERATOR_START {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            if prev.is_some_and(|p| is_letter_or_number(p) || matches!(p, ')' | ']' | '}' | LAYOUT_MARKER_END)) {
                s.push(' ');
            }
        } else {
            s.push(c);
        }
    }
    let mut t = String::new();
    for (i, &c) in s.iter().enumerate() {
        if c == NAMED_OPERATOR_END {
            let next = s.get(i + 1).copied();
            if next.is_some_and(|n| is_letter_or_number(n) || n == '√' || n == LAYOUT_MARKER_START) {
                t.push(' ');
            }
        } else {
            t.push(c);
        }
    }
    let lines: Vec<String> = t
        .split('\n')
        .map(|line| {
            let mut out = String::new();
            let mut in_space = false;
            for c in line.chars() {
                if c == ' ' || c == '\t' {
                    if !in_space {
                        out.push(' ');
                    }
                    in_space = true;
                } else {
                    out.push(c);
                    in_space = false;
                }
            }
            js_trim(&out).to_string()
        })
        .collect();
    let n = lines.len();
    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| !l.is_empty() || (*i > 0 && *i + 1 < n))
        .map(|(_, l)| l.as_str())
        .collect();
    js_trim(&kept.join("\n")).to_string()
}

enum Node {
    Fraction {
        numerator: String,
        denominator: String,
    },
    Operator {
        operator: String,
        lower: Option<String>,
        upper: Option<String>,
    },
    Script {
        lower: Option<String>,
        upper: Option<String>,
    },
    Matrix {
        lines: Vec<String>,
        baseline: usize,
    },
}

struct Layout {
    lines: Vec<String>,
    width: usize,
    baseline: usize,
}

fn text_layout(text: String) -> Layout {
    Layout {
        width: visible_width(&text),
        lines: vec![text],
        baseline: 0,
    }
}

fn pad_layout_line(line: &str, width: usize, centered: bool) -> String {
    let padding = width.saturating_sub(visible_width(line));
    let left = if centered { padding / 2 } else { 0 };
    format!("{}{line}{}", " ".repeat(left), " ".repeat(padding - left))
}

fn join_layouts(layouts: &[Layout]) -> Layout {
    if layouts.is_empty() {
        return Layout {
            lines: vec![String::new()],
            width: 0,
            baseline: 0,
        };
    }
    let baseline = layouts.iter().map(|l| l.baseline).max().unwrap_or(0);
    let below = layouts
        .iter()
        .map(|l| l.lines.len() as isize - l.baseline as isize - 1)
        .max()
        .unwrap_or(0);
    let mut lines = Vec::new();
    let mut row: isize = 0;
    while row <= baseline as isize + below {
        let mut line = String::new();
        for layout in layouts {
            let source_row = row - baseline as isize + layout.baseline as isize;
            if source_row >= 0 && (source_row as usize) < layout.lines.len() {
                line += &pad_layout_line(&layout.lines[source_row as usize], layout.width, false);
            } else {
                line += &" ".repeat(layout.width);
            }
        }
        lines.push(js_trim_end(&line).to_string());
        row += 1;
    }
    Layout {
        lines,
        width: layouts.iter().map(|l| l.width).sum(),
        baseline,
    }
}

/// Finds `\u{f0000}(\d+)\u{f0001}` markers: (start byte, end byte, index).
fn layout_markers(line: &str) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(off) = line[from..].find(LAYOUT_MARKER_START) {
        let start = from + off;
        let digits_start = start + LAYOUT_MARKER_START.len_utf8();
        let digits_len = line[digits_start..].bytes().take_while(u8::is_ascii_digit).count();
        let digits_end = digits_start + digits_len;
        if digits_len > 0 && line[digits_end..].starts_with(LAYOUT_MARKER_END) {
            let end = digits_end + LAYOUT_MARKER_END.len_utf8();
            out.push((start, end, line[digits_start..digits_end].parse().unwrap_or(usize::MAX)));
            from = end;
        } else {
            from = digits_start;
        }
    }
    out
}

fn render_layout(source: &str, nodes: &[Node]) -> Layout {
    let mut rendered_lines: Vec<String> = Vec::new();
    let mut first_baseline = 0;
    for source_line in source.split('\n') {
        let mut layouts = Vec::new();
        let mut position = 0;
        let mut previous: Option<&Node> = None;
        for (index, end, node_index) in layout_markers(source_line) {
            let Some(node) = nodes.get(node_index) else { continue };
            let prev_matrix = matches!(previous, Some(Node::Matrix { .. }));
            if index > position {
                let sliced = &source_line[position..index];
                let trimmed = js_trim_end(if previous.is_some() {
                    js_trim_start(sliced)
                } else {
                    sliced
                });
                let lead = prev_matrix && sliced.starts_with(is_ws);
                let trail = matches!(node, Node::Matrix { .. }) && sliced.ends_with(is_ws);
                let text = if !trimmed.is_empty() {
                    format!(
                        "{}{trimmed}{}",
                        if lead { " " } else { "" },
                        if trail { " " } else { "" }
                    )
                } else if lead || trail {
                    " ".to_string()
                } else {
                    String::new()
                };
                layouts.push(text_layout(text));
            }
            match node {
                Node::Fraction { numerator, denominator } => {
                    let numerator = render_layout(numerator, nodes);
                    let denominator = render_layout(denominator, nodes);
                    let content = numerator.width.max(denominator.width).max(1);
                    let width = content + 2;
                    let mut lines: Vec<String> = numerator
                        .lines
                        .iter()
                        .map(|l| pad_layout_line(l, width, true))
                        .collect();
                    lines.push(format!(" {} ", "─".repeat(content)));
                    lines.extend(denominator.lines.iter().map(|l| pad_layout_line(l, width, true)));
                    layouts.push(Layout {
                        lines,
                        width,
                        baseline: numerator.lines.len(),
                    });
                }
                Node::Operator { operator, lower, upper } => {
                    let content = visible_width(operator)
                        .max(lower.as_deref().map_or(0, visible_width))
                        .max(upper.as_deref().map_or(0, visible_width));
                    let mut lines = Vec::new();
                    if let Some(u) = upper {
                        lines.push(format!("{} ", pad_layout_line(u, content, true)));
                    }
                    lines.push(format!("{} ", pad_layout_line(operator, content, true)));
                    if let Some(l) = lower {
                        lines.push(format!("{} ", pad_layout_line(l, content, true)));
                    }
                    layouts.push(Layout {
                        lines,
                        width: content + 1,
                        baseline: usize::from(upper.is_some()),
                    });
                }
                Node::Script { lower, upper } => {
                    let upper = upper.as_deref().map(|u| render_layout(u, nodes));
                    let lower = lower.as_deref().map(|l| render_layout(l, nodes));
                    let width = upper
                        .as_ref()
                        .map_or(0, |u| u.width)
                        .max(lower.as_ref().map_or(0, |l| l.width));
                    let mut lines = Vec::new();
                    if let Some(u) = &upper {
                        lines.extend(u.lines.iter().map(|l| pad_layout_line(l, width, false)));
                    }
                    lines.push(" ".repeat(width));
                    if let Some(l) = &lower {
                        lines.extend(l.lines.iter().map(|x| pad_layout_line(x, width, false)));
                    }
                    let baseline = upper.as_ref().map_or(0, |u| u.lines.len());
                    layouts.push(Layout { lines, width, baseline });
                }
                Node::Matrix { lines, baseline } => {
                    let width = lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);
                    let lines = lines.iter().map(|l| pad_layout_line(l, width, false)).collect();
                    layouts.push(Layout {
                        lines,
                        width,
                        baseline: *baseline,
                    });
                }
            }
            position = end;
            previous = Some(node);
        }
        if position < source_line.len() {
            let sliced = &source_line[position..];
            let trimmed = if previous.is_some() {
                js_trim_start(sliced)
            } else {
                sliced
            };
            let text = if matches!(previous, Some(Node::Matrix { .. })) && sliced.starts_with(is_ws) {
                format!(" {trimmed}")
            } else {
                trimmed.to_string()
            };
            layouts.push(text_layout(text));
        }
        let line_layout = join_layouts(&layouts);
        if rendered_lines.is_empty() {
            first_baseline = line_layout.baseline;
        }
        rendered_lines.extend(line_layout.lines);
    }
    let width = rendered_lines.iter().map(|l| visible_width(l)).max().unwrap_or(0);
    Layout {
        lines: rendered_lines,
        width,
        baseline: first_baseline,
    }
}

fn marker(index: usize) -> String {
    format!("{LAYOUT_MARKER_START}{index}{LAYOUT_MARKER_END}")
}

/// `/\u{f0000}(\d+)\u{f0001}$/` on the result so far.
fn trailing_marker(result: &str) -> Option<usize> {
    let rest = result.strip_suffix(LAYOUT_MARKER_END)?;
    let digits = rest.len() - rest.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return None;
    }
    let (head, num) = rest.split_at(rest.len() - digits);
    if !head.ends_with(LAYOUT_MARKER_START) {
        return None;
    }
    num.parse().ok()
}

fn trim_end_in_place(s: &mut String) {
    let n = js_trim_end(s).len();
    s.truncate(n);
}

fn split_rows(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut last = 0;
    for m in ROW_SPLIT.find_iter(body).flatten() {
        out.push(body[last..m.start()].to_string());
        last = m.end();
    }
    out.push(body[last..].to_string());
    out
}

fn strip_leading_spec(body: &str) -> String {
    match LEADING_SPEC.find(body).ok().flatten() {
        Some(m) => body[m.end()..].to_string(),
        None => body.to_string(),
    }
}

/// `value.replace(/,\s*$/, "")`.
fn strip_trailing_comma(value: &str) -> &str {
    js_trim_end(value).strip_suffix(',').unwrap_or(value)
}

/// `/^(?:if|when|for|otherwise)\b/i`.
fn starts_with_condition_word(s: &str) -> bool {
    ["if", "when", "for", "otherwise"].iter().any(|w| {
        s.get(..w.len()).is_some_and(|p| p.eq_ignore_ascii_case(w))
            && !s[w.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

struct Parser<'a> {
    source: Vec<char>,
    nodes: &'a mut Vec<Node>,
    display: bool,
    position: usize,
    supported: bool,
    stack_fractions: bool,
    script_depth: usize,
}

impl<'a> Parser<'a> {
    fn new(source: &str, nodes: &'a mut Vec<Node>, display: bool) -> Self {
        Parser {
            source: source.chars().collect(),
            nodes,
            display,
            position: 0,
            supported: true,
            stack_fractions: true,
            script_depth: 0,
        }
    }

    fn at(&self, i: usize) -> Option<char> {
        self.source.get(i).copied()
    }

    fn slice(&self, a: usize, b: usize) -> String {
        self.source[a..b].iter().collect()
    }

    fn push_node(&mut self, node: Node) -> String {
        self.nodes.push(node);
        marker(self.nodes.len() - 1)
    }

    fn skip(&mut self, pred: fn(char) -> bool) {
        while self.at(self.position).is_some_and(pred) {
            self.position += 1;
        }
    }

    fn render(mut self) -> Option<String> {
        let rendered = self.parse_sequence(None);
        if !self.supported || self.position != self.source.len() {
            return None;
        }
        Some(normalize_output(&rendered))
    }

    fn parse_sequence(&mut self, end: Option<char>) -> String {
        let mut result = String::new();
        while let Some(c) = self.at(self.position) {
            if end == Some(c) {
                self.position += 1;
                return result;
            }
            match c {
                '}' => {
                    self.supported = false;
                    return result;
                }
                '{' => {
                    self.position += 1;
                    result += &self.parse_sequence(Some('}'));
                }
                '\\' => {
                    let command = self.parse_command();
                    if command == NEGATIVE_SPACE {
                        trim_end_in_place(&mut result);
                        if result.ends_with(NAMED_OPERATOR_END) {
                            result.pop();
                        }
                    } else {
                        result += &command;
                    }
                }
                '^' | '_' => {
                    self.position += 1;
                    trim_end_in_place(&mut result);
                    let script = self.parse_scripts(c);
                    if result.ends_with(NAMED_OPERATOR_END) {
                        result.pop();
                        result += &script;
                        result.push(NAMED_OPERATOR_END);
                    } else {
                        result += &script;
                    }
                }
                c if is_ws(c) => {
                    self.skip(is_ws);
                    result.push(' ');
                }
                '=' | '<' | '>' => {
                    trim_end_in_place(&mut result);
                    result.push(' ');
                    result.push(c);
                    result.push(' ');
                    self.position += 1;
                }
                '&' => self.position += 1,
                '~' => {
                    self.position += 1;
                    result.push(' ');
                }
                _ => {
                    if c == '.'
                        && let Some(Node::Matrix { lines, .. }) =
                            trailing_marker(&result).and_then(|i| self.nodes.get_mut(i))
                    {
                        if let Some(last) = lines.last_mut() {
                            last.push('.');
                        }
                        self.position += 1;
                        continue;
                    }
                    result.push(c);
                    self.position += 1;
                }
            }
        }
        if end.is_some() {
            self.supported = false;
        }
        result
    }

    fn parse_scripts(&mut self, initial: char) -> String {
        let mut sub: Option<String> = None;
        let mut sup: Option<String> = None;
        let mut order = Vec::new();
        let mut parse = |p: &mut Self, m: char| {
            p.script_depth += 1;
            let value = p.parse_required_argument(false);
            p.script_depth -= 1;
            if m == '_' {
                sub = Some(value);
                order.push(Kind::Sub);
            } else {
                sup = Some(value);
                order.push(Kind::Sup);
            }
        };
        parse(self, initial);
        let mut next = self.position;
        while self.at(next).is_some_and(is_ws) {
            next += 1;
        }
        if let Some(m) = self.at(next)
            && (m == '^' || m == '_')
            && m != initial
        {
            self.position = next + 1;
            parse(self, m);
        }
        let sub_unicode = sub.as_deref().and_then(|v| format_unicode_script(v, Kind::Sub));
        let sup_unicode = sup.as_deref().and_then(|v| format_unicode_script(v, Kind::Sup));
        let can_use_layout = ![&sub, &sup].iter().any(|v| {
            v.as_deref().is_some_and(|v| {
                v.contains('/')
                    || (!v.contains(LAYOUT_MARKER_START)
                        && char_count(v) > 1
                        && !v.chars().any(|c| c.is_ascii_uppercase() || c == '*' || c == '∗'))
            })
        });
        let needs_layout = self.display
            && can_use_layout
            && (self.script_depth > 0
                || (sub.is_some() && sub_unicode.is_none())
                || (sup.is_some() && sup_unicode.is_none()));
        if !needs_layout {
            return order
                .iter()
                .map(|&k| match k {
                    Kind::Sub => sub_unicode
                        .clone()
                        .unwrap_or_else(|| format_script(sub.as_deref().unwrap_or(""), k)),
                    Kind::Sup => sup_unicode
                        .clone()
                        .unwrap_or_else(|| format_script(sup.as_deref().unwrap_or(""), k)),
                })
                .collect();
        }
        self.push_node(Node::Script {
            lower: sub.as_deref().map(normalize_output),
            upper: sup.as_deref().map(normalize_output),
        })
    }

    fn parse_command(&mut self) -> String {
        self.position += 1;
        let Some(first) = self.at(self.position) else {
            self.supported = false;
            return String::new();
        };
        if first == '\n' || first == '\r' {
            self.position += 1;
            if first == '\r' && self.at(self.position) == Some('\n') {
                self.position += 1;
            }
            return " ".into();
        }
        let command = if first.is_ascii_alphabetic() {
            let start = self.position;
            self.skip(|c| c.is_ascii_alphabetic());
            self.slice(start, self.position)
        } else {
            self.position += 1;
            first.to_string()
        };
        let command = command.as_str();
        if command == "\\" {
            return "\n".into();
        }
        if is_spacing(command) {
            return " ".into();
        }
        if is_negative_spacing(command) {
            return NEGATIVE_SPACE.into();
        }
        if is_font_switch(command) {
            self.skip(is_ws);
            return String::new();
        }
        if is_ignored(command) {
            return String::new();
        }
        if matches!(command, "{" | "}" | "$" | "%" | "#" | "_" | "&") {
            return command.into();
        }
        if command == "|" {
            return "‖".into();
        }
        if command == "not" {
            let value = self.parse_required_argument(false);
            let value = js_trim(&value);
            if let Some(n) = negated(value) {
                return format!(" {n} ");
            }
            let mut chars = value.chars();
            let Some(head) = chars.next() else {
                self.supported = false;
                return String::new();
            };
            return format!(" {head}\u{338}{} ", chars.as_str());
        }
        if is_limit_operator(command) {
            return self.parse_operator(command, true, true, true);
        }
        if let Some(sym) = symbol(command) {
            if is_display_limit_symbol(command) {
                return self.parse_operator(sym, false, true, false);
            }
            return if command == "cdot" || command == "times" || is_relation(command) {
                format!(" {sym} ")
            } else {
                sym.into()
            };
        }
        if is_named_operator(command) {
            return format!("{NAMED_OPERATOR_START}{command}{NAMED_OPERATOR_END}");
        }
        if is_size(command) {
            return String::new();
        }
        if matches!(command, "left" | "middle" | "right") {
            if self.at(self.position) == Some('.') {
                self.position += 1;
            }
            return String::new();
        }
        if matches!(command, "frac" | "dfrac" | "tfrac") {
            let stack = self.display && self.stack_fractions && command != "tfrac";
            let numerator = self.parse_required_argument(!stack);
            let denominator = self.parse_required_argument(!stack);
            if stack {
                return self.push_node(Node::Fraction {
                    numerator: normalize_output(&numerator),
                    denominator: normalize_output(&denominator),
                });
            }
            return format_fraction(&numerator, &denominator);
        }
        if command == "sqrt" {
            let degree = self.parse_optional_argument();
            let value = self.parse_required_argument(true);
            return match degree.as_deref().map(js_trim) {
                None | Some("2") => format_root(&value, "√"),
                Some("3") => format_root(&value, "∛"),
                Some("4") => format_root(&value, "∜"),
                Some(d) => format!("{}{}", format_script(d, Kind::Sup), format_root(&value, "√")),
            };
        }
        if command == "boxed" || command == "fbox" {
            return format!("[{}]", js_trim(&self.parse_required_argument(true)));
        }
        if matches!(command, "binom" | "dbinom" | "tbinom") {
            let a = self.parse_required_argument(true);
            let b = self.parse_required_argument(true);
            return format!("({a} choose {b})");
        }
        if let Some(acc) = accent(command) {
            let value = self.parse_required_argument(true);
            return if char_count(&value) == 1 {
                format!("{value}{acc}")
            } else {
                format!("{command}({value})")
            };
        }
        if command == "mathbb" {
            let value = self.parse_required_argument(true);
            return value
                .chars()
                .map(|c| blackboard(c).map_or_else(|| c.to_string(), str::to_string))
                .collect();
        }
        if command == "operatorname" {
            let starred = self.at(self.position) == Some('*');
            if starred {
                self.position += 1;
            }
            let arg = self.parse_required_argument(true);
            let operator = js_trim(&normalize_output(&arg)).to_string();
            return self.parse_operator(&operator, true, starred, true);
        }
        if command == "mod" || command == "bmod" {
            return " mod ".into();
        }
        if command == "pmod" || command == "pod" {
            let value = self.parse_required_argument(true);
            let value = js_trim(&value);
            return if command == "pmod" {
                format!(" (mod {value})")
            } else {
                format!(" ({value})")
            };
        }
        if command == "overset" || command == "stackrel" || command == "underset" {
            let script = self.parse_required_argument(true);
            let value = self.parse_required_argument(true);
            let kind = if command == "underset" { Kind::Sub } else { Kind::Sup };
            return format!("{}{}", js_trim(&value), format_script(&script, kind));
        }
        if is_plain_wrapper(command) {
            let value = self.parse_required_argument(true);
            return if command.starts_with("text") || command == "mbox" {
                value
            } else {
                js_trim(&value).into()
            };
        }
        if command == "begin" {
            return self.parse_environment();
        }
        self.supported = false;
        if command == "end" {
            String::new()
        } else {
            format!("\\{command}")
        }
    }

    fn parse_operator(&mut self, operator: &str, bracket: bool, display_limits: bool, spaced: bool) -> String {
        let mut use_display_limits = display_limits;
        let mut modifier_position = self.position;
        while self.at(modifier_position).is_some_and(|c| c == ' ' || c == '\t') {
            modifier_position += 1;
        }
        let rest = self.slice(modifier_position, self.source.len());
        if let Ok(Some(m)) = OPERATOR_MODIFIER.captures(&rest) {
            let all = m.get(0).map_or("", |x| x.as_str());
            use_display_limits = m.get(1).is_some_and(|x| x.as_str() == "limits");
            self.position = modifier_position + char_count(all);
        }
        let mut lower: Option<String> = None;
        let mut upper: Option<String> = None;
        loop {
            let mut script_position = self.position;
            while self.at(script_position).is_some_and(|c| c == ' ' || c == '\t') {
                script_position += 1;
            }
            let kind = self.at(script_position);
            if kind != Some('_') && kind != Some('^') {
                break;
            }
            self.position = script_position + 1;
            let arg = self.parse_required_argument(false);
            let value = normalize_output(&arg).replace(' ', "");
            let slot = if kind == Some('_') { &mut lower } else { &mut upper };
            if slot.is_some() {
                self.supported = false;
            }
            *slot = Some(value);
        }
        if self.display && use_display_limits && (lower.is_some() || upper.is_some()) {
            return self.push_node(Node::Operator {
                operator: operator.to_string(),
                lower,
                upper,
            });
        }
        let mut rendered = operator.to_string();
        if let Some(l) = &lower {
            rendered += &if bracket {
                format!("[{l}]")
            } else {
                format_script(l, Kind::Sub)
            };
        }
        if let Some(u) = &upper {
            rendered += &format_script(u, Kind::Sup);
        }
        if spaced { format!(" {rendered} ") } else { rendered }
    }

    fn parse_required_argument(&mut self, stack_fractions: bool) -> String {
        let previous = self.stack_fractions;
        self.stack_fractions = previous && stack_fractions;
        let value = self.parse_required_argument_value();
        self.stack_fractions = previous;
        value
    }

    fn parse_required_argument_value(&mut self) -> String {
        self.skip(is_ws);
        match self.at(self.position) {
            None => {
                self.supported = false;
                String::new()
            }
            Some('{') => {
                self.position += 1;
                self.parse_sequence(Some('}'))
            }
            Some('\\') => self.parse_command(),
            Some(c) => {
                self.position += 1;
                c.to_string()
            }
        }
    }

    fn parse_optional_argument(&mut self) -> Option<String> {
        self.skip(|c| c == ' ' || c == '\t');
        if self.at(self.position) != Some('[') {
            return None;
        }
        let Some(end) = (self.position + 1..self.source.len()).find(|&i| self.source[i] == ']') else {
            self.supported = false;
            return None;
        };
        let value = self.slice(self.position + 1, end);
        self.position = end + 1;
        Some(self.render_nested(&value, true))
    }

    fn read_raw_group(&mut self) -> Option<String> {
        self.skip(|c| c == ' ' || c == '\t');
        if self.at(self.position) != Some('{') {
            self.supported = false;
            return None;
        }
        self.position += 1;
        let start = self.position;
        let mut depth = 1;
        while let Some(c) = self.at(self.position) {
            if c == '\\' {
                self.position += 2;
                continue;
            }
            if c == '{' {
                depth += 1;
            }
            if c == '}' {
                depth -= 1;
            }
            if depth == 0 {
                let value = self.slice(start, self.position);
                self.position += 1;
                return Some(value);
            }
            self.position += 1;
        }
        self.supported = false;
        None
    }

    fn parse_environment(&mut self) -> String {
        let environment = match self.read_raw_group() {
            Some(e) if !e.is_empty() => e,
            _ => return String::new(),
        };
        let end_marker: Vec<char> = format!("\\end{{{environment}}}").chars().collect();
        let from = self.position.min(self.source.len());
        let Some(end) = (from..self.source.len()).find(|&i| self.source[i..].starts_with(&end_marker)) else {
            self.supported = false;
            return String::new();
        };
        let body = self.slice(from, end);
        self.position = end + end_marker.len();
        let env = environment.as_str();
        if matches!(env, "equation" | "equation*" | "displaymath") {
            return js_trim(&self.render_nested(&body, true)).to_string();
        }
        if matches!(
            env,
            "aligned"
                | "align"
                | "align*"
                | "alignedat"
                | "alignat"
                | "alignat*"
                | "gather"
                | "gathered"
                | "multline"
                | "multline*"
                | "split"
        ) {
            let aligned_at = matches!(env, "alignedat" | "alignat" | "alignat*");
            let body = if aligned_at { strip_leading_spec(&body) } else { body };
            let mut rows = Vec::new();
            for row in split_rows(&body) {
                let cells: Vec<&str> = row.split('&').collect();
                let source = if aligned_at {
                    cells.chunks(2).map(|c| c.concat()).collect::<Vec<_>>().join(" ")
                } else {
                    cells.concat()
                };
                let rendered = js_trim(&self.render_nested(&source, true)).to_string();
                if !rendered.is_empty() {
                    rows.push(rendered);
                }
            }
            return rows.join("\n");
        }
        if env == "cases" || env == "cases*" {
            return self.render_cases(&body);
        }
        if matches!(
            env,
            "array" | "matrix" | "smallmatrix" | "pmatrix" | "bmatrix" | "Bmatrix" | "vmatrix" | "Vmatrix"
        ) {
            let body = if env == "array" {
                strip_leading_spec(&body)
            } else {
                body
            };
            return self.render_matrix(env, &body);
        }
        self.supported = false;
        body
    }

    fn table(&mut self, body: &str) -> Vec<Vec<String>> {
        let mut rows = Vec::new();
        for row in split_rows(body) {
            let cells: Vec<String> = row
                .split('&')
                .map(|cell| js_trim(&self.render_nested(cell, false)).to_string())
                .collect();
            if cells.iter().any(|c| !c.is_empty()) {
                rows.push(cells);
            }
        }
        rows
    }

    fn render_cases(&mut self, body: &str) -> String {
        let rows = self.table(body);
        let value_width = rows
            .iter()
            .map(|r| visible_width(strip_trailing_comma(&r[0])))
            .max()
            .unwrap_or(0);
        let contents: Vec<String> = rows
            .iter()
            .map(|row| {
                let value = strip_trailing_comma(&row[0]);
                let condition = row.get(1).map_or("", String::as_str);
                if condition.is_empty() {
                    return value.to_string();
                }
                let prefix = if starts_with_condition_word(condition) {
                    " "
                } else {
                    " if "
                };
                let pad: String = std::iter::repeat_n(PROTECTED_SPACE, value_width - visible_width(value)).collect();
                format!("{value}{pad}{prefix}{condition}")
            })
            .collect();
        if contents.len() <= 1 {
            return contents.first().map_or(String::new(), |c| format!("⎧ {c}"));
        }
        let middle = contents.len() / 2;
        let mut visual: Vec<Option<String>> = contents.into_iter().map(Some).collect();
        if visual.len().is_multiple_of(2) {
            visual.insert(middle, None);
        }
        let n = visual.len();
        let lines = visual
            .into_iter()
            .enumerate()
            .map(|(i, content)| {
                let delimiter = if i == 0 {
                    "⎧"
                } else if i == n - 1 {
                    "⎩"
                } else {
                    "⎨"
                };
                content.map_or_else(|| delimiter.to_string(), |c| format!("{delimiter} {c}"))
            })
            .collect();
        self.push_node(Node::Matrix {
            lines,
            baseline: middle,
        })
    }

    fn render_matrix(&mut self, environment: &str, body: &str) -> String {
        let matrix = self.table(body);
        let columns = matrix.iter().map(Vec::len).max().unwrap_or(0);
        let widths: Vec<usize> = (0..columns)
            .map(|c| {
                matrix
                    .iter()
                    .map(|r| r.get(c).map_or(0, |x| visible_width(x)))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let rows: Vec<String> = matrix
            .iter()
            .map(|row| {
                (0..columns)
                    .map(|c| {
                        let cell = row.get(c).map_or("", String::as_str);
                        let pad: String =
                            std::iter::repeat_n(PROTECTED_SPACE, widths[c].saturating_sub(visible_width(cell)))
                                .collect();
                        format!("{cell}{pad}")
                    })
                    .collect::<Vec<_>>()
                    .join(" │ ")
            })
            .collect();
        let lines: Vec<String> = if matches!(environment, "array" | "matrix" | "smallmatrix") {
            rows
        } else {
            let d: [&str; 6] = match environment {
                "pmatrix" => ["⎛", "⎞", "⎜", "⎟", "⎝", "⎠"],
                "bmatrix" => ["⎡", "⎤", "⎢", "⎥", "⎣", "⎦"],
                "Bmatrix" => ["⎧", "⎫", "⎨", "⎬", "⎩", "⎭"],
                "vmatrix" => ["│"; 6],
                _ => ["║"; 6],
            };
            let n = rows.len();
            rows.iter()
                .enumerate()
                .map(|(i, row)| {
                    let (l, r) = if i == 0 {
                        (d[0], d[1])
                    } else if i == n - 1 {
                        (d[4], d[5])
                    } else {
                        (d[2], d[3])
                    };
                    format!("{l} {row} {r}")
                })
                .collect()
        };
        if lines.len() <= 1 {
            return lines.into_iter().next().unwrap_or_default();
        }
        self.push_node(Node::Matrix { lines, baseline: 0 })
    }

    fn render_nested(&mut self, source: &str, stack_fractions: bool) -> String {
        let display = self.display && stack_fractions;
        match Parser::new(source, self.nodes, display).render() {
            Some(r) => r,
            None => {
                self.supported = false;
                source.to_string()
            }
        }
    }
}

/// `renderLatex(source, { display })`: Unicode text for a LaTeX math expression, or `None` when the expression
/// uses unsupported or malformed syntax.
pub fn render_latex(source: &str, display: bool) -> Option<String> {
    let mut nodes = Vec::new();
    let rendered = Parser::new(source, &mut nodes, display).render()?;
    let protect = |s: &str| s.replace(PROTECTED_SPACE, " ");
    if nodes.is_empty() {
        return Some(protect(&rendered));
    }
    let lines = render_layout(&rendered, &nodes).lines;
    let indentation = lines
        .iter()
        .filter(|l| !js_trim(l).is_empty())
        .map(|l| l.len() - js_trim_start(l).len())
        .min()
        .unwrap_or(0);
    let out: Vec<&str> = lines
        .iter()
        .map(|l| js_trim_end(l.get(indentation..).unwrap_or("")))
        .collect();
    Some(protect(js_trim_end(&out.join("\n"))))
}

static BLOCK_DOLLAR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}\$\$[ \t]*(?:\n)?([\s\S]*?)\$\$[ \t]*(?:\n|$)").expect("regex"));
static BLOCK_BRACKET: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}\\\[[ \t]*(?:\n)?([\s\S]*?)\\\][ \t]*(?:\n|$)").expect("regex"));
static BLOCK_PENDING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^ {0,3}(?:\\\[|\$\$)[ \t]*(?:\n)?([\s\S]*)$").expect("regex"));

fn is_escaped(s: &str, index: usize) -> bool {
    s.as_bytes()[..index].iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 1
}

/// The math source of a `latexBlock` token (markdown.ts `tokenizeBlockLatex`), recovered from its raw text, and
/// whether the block is still unclosed (`pending`).
fn block_text(raw: &str) -> (String, bool) {
    for r in [&*BLOCK_DOLLAR, &*BLOCK_BRACKET] {
        if let Ok(Some(c)) = r.captures(raw)
            && let Some(text) = c.get(1).filter(|m| !m.as_str().is_empty())
        {
            return (js_trim(text.as_str()).to_string(), false);
        }
    }
    let text = BLOCK_PENDING
        .captures(raw)
        .ok()
        .flatten()
        .and_then(|c| c.get(1))
        .map_or("", |m| m.as_str());
    (text.to_string(), true)
}

/// The math source of an inline `latex` token (markdown.ts `tokenizeInlineLatex`) and whether it is unclosed.
fn inline_text(raw: &str) -> (String, bool) {
    let (open, close) = [("$$", "$$"), ("\\(", "\\)"), ("\\[", "\\]"), ("$", "$")]
        .into_iter()
        .find(|(o, _)| raw.starts_with(o))
        .unwrap_or(("", ""));
    let closed = raw.len() >= open.len() + close.len() && raw.ends_with(close) && {
        let end = raw.len() - close.len();
        let mut i = raw[open.len()..].find(close).map(|i| i + open.len());
        while let Some(at) = i
            && is_escaped(raw, at)
        {
            i = raw[at + close.len()..].find(close).map(|j| j + at + close.len());
        }
        i == Some(end)
    };
    if closed {
        (raw[open.len()..raw.len() - close.len()].to_string(), false)
    } else {
        (raw[open.len()..].to_string(), true)
    }
}

/// What the Markdown component shows for a `latexBlock` (`block`) or inline `latex` token: rendered Unicode math,
/// or the token's source when it is unclosed, unsupported, or `render` is off (markdown.ts `renderLatex: false`).
pub fn render_latex_token(raw: &str, block: bool, render: bool) -> String {
    let (text, pending) = if block { block_text(raw) } else { inline_text(raw) };
    let fallback = if block { js_trim(raw) } else { raw };
    if pending || !render {
        return fallback.to_string();
    }
    render_latex(&text, block).unwrap_or_else(|| fallback.to_string())
}

fn symbol(k: &str) -> Option<&'static str> {
    Some(match k {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ϵ",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "varkappa" => "ϰ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "div" => "÷",
        "cdot" => "·",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "•",
        "oplus" => "⊕",
        "ominus" => "⊖",
        "otimes" => "⊗",
        "oslash" => "⊘",
        "odot" => "⊙",
        "bigcirc" => "○",
        "dagger" => "†",
        "ddagger" => "‡",
        "amalg" => "⨿",
        "uplus" => "⊎",
        "sqcap" => "⊓",
        "sqcup" => "⊔",
        "bowtie" => "⋈",
        "Join" => "⋈",
        "ltimes" => "⋉",
        "rtimes" => "⋊",
        "leftouterjoin" => "⟕",
        "rightouterjoin" => "⟖",
        "fullouterjoin" => "⟗",
        "triangleleft" => "◁",
        "triangleright" => "▷",
        "wr" => "≀",
        "cap" => "∩",
        "cup" => "∪",
        "bigcap" => "⋂",
        "bigcup" => "⋃",
        "bigwedge" => "⋀",
        "bigvee" => "⋁",
        "bigsqcup" => "⨆",
        "biguplus" => "⨄",
        "bigoplus" => "⨁",
        "bigotimes" => "⨂",
        "bigodot" => "⨀",
        "setminus" => "∖",
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "subset" => "⊂",
        "supset" => "⊃",
        "subseteq" => "⊆",
        "supseteq" => "⊇",
        "sqsubset" => "⊏",
        "sqsupset" => "⊐",
        "sqsubseteq" => "⊑",
        "sqsupseteq" => "⊒",
        "prec" => "≺",
        "preceq" => "≼",
        "succ" => "≻",
        "succeq" => "≽",
        "ll" => "≪",
        "gg" => "≫",
        "le" => "≤",
        "leq" => "≤",
        "leqslant" => "≤",
        "ge" => "≥",
        "geq" => "≥",
        "geqslant" => "≥",
        "ne" => "≠",
        "neq" => "≠",
        "equiv" => "≡",
        "approx" => "≈",
        "sim" => "∼",
        "simeq" => "≃",
        "cong" => "≅",
        "asymp" => "≍",
        "doteq" => "≐",
        "propto" => "∝",
        "parallel" => "∥",
        "perp" => "⊥",
        "mid" => "∣",
        "vdash" => "⊢",
        "dashv" => "⊣",
        "models" => "⊨",
        "Vdash" => "⊩",
        "Vvdash" => "⊪",
        "nvdash" => "⊬",
        "nvDash" => "⊭",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "neg" => "¬",
        "land" => "∧",
        "wedge" => "∧",
        "lor" => "∨",
        "vee" => "∨",
        "to" => "→",
        "rightarrow" => "→",
        "longrightarrow" => "→",
        "leftarrow" => "←",
        "longleftarrow" => "←",
        "gets" => "←",
        "leftrightarrow" => "↔",
        "longleftrightarrow" => "↔",
        "hookleftarrow" => "↩",
        "hookrightarrow" => "↪",
        "twoheadleftarrow" => "↞",
        "twoheadrightarrow" => "↠",
        "leftharpoonup" => "↼",
        "leftharpoondown" => "↽",
        "rightharpoonup" => "⇀",
        "rightharpoondown" => "⇁",
        "rightleftharpoons" => "⇌",
        "leftrightharpoons" => "⇋",
        "nearrow" => "↗",
        "searrow" => "↘",
        "swarrow" => "↙",
        "nwarrow" => "↖",
        "rightsquigarrow" => "⇝",
        "leadsto" => "⇝",
        "Rightarrow" => "⇒",
        "Longrightarrow" => "⇒",
        "Leftarrow" => "⇐",
        "Longleftarrow" => "⇐",
        "Leftrightarrow" => "⇔",
        "Longleftrightarrow" => "⇔",
        "implies" => "⇒",
        "iff" => "⇔",
        "mapsto" => "↦",
        "longmapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "partial" => "∂",
        "nabla" => "∇",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "infty" => "∞",
        "emptyset" => "∅",
        "varnothing" => "∅",
        "angle" => "∠",
        "therefore" => "∴",
        "because" => "∵",
        "aleph" => "ℵ",
        "beth" => "ℶ",
        "gimel" => "ℷ",
        "daleth" => "ℸ",
        "top" => "⊤",
        "bot" => "⊥",
        "triangle" => "△",
        "square" => "□",
        "lozenge" => "◊",
        "checkmark" => "✓",
        "complement" => "∁",
        "wp" => "℘",
        "prime" => "′",
        "ldots" => "…",
        "dots" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "ell" => "ℓ",
        "hbar" => "ℏ",
        "Im" => "ℑ",
        "Re" => "ℜ",
        "langle" => "⟨",
        "rangle" => "⟩",
        "vert" => "|",
        "lvert" => "|",
        "rvert" => "|",
        "Vert" => "‖",
        "lVert" => "‖",
        "rVert" => "‖",
        "lbrace" => "{",
        "rbrace" => "}",
        "backslash" => "\\",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "colon" => ":",
        _ => return None,
    })
}

fn is_named_operator(k: &str) -> bool {
    matches!(
        k,
        "arccos"
            | "arcsin"
            | "arctan"
            | "arg"
            | "cos"
            | "cosh"
            | "cot"
            | "coth"
            | "csc"
            | "deg"
            | "det"
            | "dim"
            | "exp"
            | "gcd"
            | "hom"
            | "inf"
            | "ker"
            | "lg"
            | "lim"
            | "liminf"
            | "limsup"
            | "ln"
            | "log"
            | "max"
            | "min"
            | "Pr"
            | "sec"
            | "sin"
            | "sinh"
            | "sup"
            | "tan"
            | "tanh"
    )
}

fn is_limit_operator(k: &str) -> bool {
    matches!(
        k,
        "argmax" | "argmin" | "inf" | "injlim" | "lim" | "liminf" | "limsup" | "max" | "min" | "projlim" | "sup"
    )
}

fn is_display_limit_symbol(k: &str) -> bool {
    matches!(
        k,
        "bigcap"
            | "bigcup"
            | "bigodot"
            | "bigoplus"
            | "bigotimes"
            | "bigsqcup"
            | "biguplus"
            | "bigvee"
            | "bigwedge"
            | "coprod"
            | "int"
            | "iint"
            | "iiint"
            | "oint"
            | "prod"
            | "sum"
    )
}

fn is_relation(k: &str) -> bool {
    matches!(
        k,
        "Leftarrow"
            | "Leftrightarrow"
            | "Longleftarrow"
            | "Longleftrightarrow"
            | "Longrightarrow"
            | "Rightarrow"
            | "Join"
            | "Vdash"
            | "Vvdash"
            | "approx"
            | "asymp"
            | "bowtie"
            | "cong"
            | "dashv"
            | "fullouterjoin"
            | "doteq"
            | "downarrow"
            | "equiv"
            | "ge"
            | "geq"
            | "geqslant"
            | "gets"
            | "gg"
            | "hookleftarrow"
            | "hookrightarrow"
            | "iff"
            | "implies"
            | "in"
            | "leadsto"
            | "le"
            | "leftarrow"
            | "leftharpoondown"
            | "leftharpoonup"
            | "leftrightarrow"
            | "leftrightharpoons"
            | "leftouterjoin"
            | "leq"
            | "leqslant"
            | "ll"
            | "longleftarrow"
            | "longleftrightarrow"
            | "longmapsto"
            | "longrightarrow"
            | "ltimes"
            | "mapsto"
            | "mid"
            | "models"
            | "ne"
            | "nearrow"
            | "neq"
            | "ni"
            | "notin"
            | "nvdash"
            | "nvDash"
            | "nwarrow"
            | "parallel"
            | "perp"
            | "prec"
            | "preceq"
            | "propto"
            | "rightharpoondown"
            | "rightharpoonup"
            | "rightleftharpoons"
            | "rightouterjoin"
            | "rightarrow"
            | "rightsquigarrow"
            | "rtimes"
            | "searrow"
            | "sim"
            | "simeq"
            | "sqsubset"
            | "sqsubseteq"
            | "sqsupset"
            | "sqsupseteq"
            | "subset"
            | "subseteq"
            | "succ"
            | "succeq"
            | "supset"
            | "supseteq"
            | "swarrow"
            | "to"
            | "triangleleft"
            | "triangleright"
            | "twoheadleftarrow"
            | "twoheadrightarrow"
            | "uparrow"
            | "vdash"
    )
}

fn negated(k: &str) -> Option<&'static str> {
    Some(match k {
        "<" => "≮",
        ">" => "≯",
        "=" => "≠",
        "∈" => "∉",
        "∋" => "∌",
        "∣" => "∤",
        "∥" => "∦",
        "∼" => "≁",
        "≃" => "≄",
        "≅" => "≇",
        "≈" => "≉",
        "≡" => "≢",
        "≤" => "≰",
        "≥" => "≱",
        "≺" => "⊀",
        "≻" => "⊁",
        "⊂" => "⊄",
        "⊃" => "⊅",
        "⊆" => "⊈",
        "⊇" => "⊉",
        "⊢" => "⊬",
        "⊨" => "⊭",
        "↔" => "↮",
        "←" => "↚",
        "→" => "↛",
        "⇒" => "⇏",
        "⇐" => "⇍",
        "⇔" => "⇎",
        "≼" => "⋠",
        "≽" => "⋡",
        _ => return None,
    })
}

fn blackboard(c: char) -> Option<&'static str> {
    Some(match c {
        'C' => "ℂ",
        'H' => "ℍ",
        'N' => "ℕ",
        'P' => "ℙ",
        'Q' => "ℚ",
        'R' => "ℝ",
        'Z' => "ℤ",
        _ => return None,
    })
}

fn superscript(c: char) -> Option<&'static str> {
    Some(match c {
        '0' => "⁰",
        '1' => "¹",
        '2' => "²",
        '3' => "³",
        '4' => "⁴",
        '5' => "⁵",
        '6' => "⁶",
        '7' => "⁷",
        '8' => "⁸",
        '9' => "⁹",
        '+' => "⁺",
        '-' => "⁻",
        '=' => "⁼",
        '(' => "⁽",
        ')' => "⁾",
        'a' => "ᵃ",
        'b' => "ᵇ",
        'c' => "ᶜ",
        'd' => "ᵈ",
        'e' => "ᵉ",
        'f' => "ᶠ",
        'g' => "ᵍ",
        'h' => "ʰ",
        'i' => "ⁱ",
        'j' => "ʲ",
        'k' => "ᵏ",
        'l' => "ˡ",
        'm' => "ᵐ",
        'n' => "ⁿ",
        'o' => "ᵒ",
        'p' => "ᵖ",
        'r' => "ʳ",
        's' => "ˢ",
        't' => "ᵗ",
        'u' => "ᵘ",
        'v' => "ᵛ",
        'w' => "ʷ",
        'x' => "ˣ",
        'y' => "ʸ",
        'z' => "ᶻ",
        _ => return None,
    })
}

fn subscript(c: char) -> Option<&'static str> {
    Some(match c {
        '0' => "₀",
        '1' => "₁",
        '2' => "₂",
        '3' => "₃",
        '4' => "₄",
        '5' => "₅",
        '6' => "₆",
        '7' => "₇",
        '8' => "₈",
        '9' => "₉",
        '+' => "₊",
        '-' => "₋",
        '=' => "₌",
        '(' => "₍",
        ')' => "₎",
        'a' => "ₐ",
        'e' => "ₑ",
        'h' => "ₕ",
        'i' => "ᵢ",
        'j' => "ⱼ",
        'k' => "ₖ",
        'l' => "ₗ",
        'm' => "ₘ",
        'n' => "ₙ",
        'o' => "ₒ",
        'p' => "ₚ",
        'r' => "ᵣ",
        's' => "ₛ",
        't' => "ₜ",
        'u' => "ᵤ",
        'v' => "ᵥ",
        'x' => "ₓ",
        _ => return None,
    })
}

fn is_spacing(k: &str) -> bool {
    matches!(
        k,
        "," | ":" | ";" | " " | ">" | "enspace" | "enskip" | "medspace" | "quad" | "qquad" | "thickspace" | "thinspace"
    )
}

fn is_negative_spacing(k: &str) -> bool {
    matches!(k, "!" | "negmedspace" | "negthickspace" | "negthinspace")
}

fn is_font_switch(k: &str) -> bool {
    matches!(k, "bf" | "cal" | "it" | "rm" | "sf" | "sl" | "tt")
}

fn is_ignored(k: &str) -> bool {
    matches!(
        k,
        "displaystyle" | "limits" | "nolimits" | "scriptstyle" | "scriptscriptstyle" | "textstyle"
    )
}

fn is_size(k: &str) -> bool {
    matches!(
        k,
        "big" | "Big" | "bigg" | "Bigg" | "bigl" | "Bigl" | "biggl" | "Biggl" | "bigr" | "Bigr" | "biggr" | "Biggr"
    )
}

fn is_plain_wrapper(k: &str) -> bool {
    matches!(
        k,
        "emph"
            | "mathcal"
            | "mathbf"
            | "mathfrak"
            | "mathit"
            | "mathrm"
            | "mathnormal"
            | "mathscr"
            | "mathsf"
            | "mathtt"
            | "mathup"
            | "mbox"
            | "overbrace"
            | "pmb"
            | "smash"
            | "substack"
            | "text"
            | "textbf"
            | "textit"
            | "textmd"
            | "textnormal"
            | "textrm"
            | "textsc"
            | "textsf"
            | "textsl"
            | "texttt"
            | "textup"
            | "underbrace"
            | "bm"
            | "boldsymbol"
    )
}

fn accent(k: &str) -> Option<&'static str> {
    Some(match k {
        "acute" => "\u{301}",
        "bar" => "\u{305}",
        "breve" => "\u{306}",
        "check" => "\u{30c}",
        "ddot" => "\u{308}",
        "dot" => "\u{307}",
        "grave" => "\u{300}",
        "hat" => "\u{302}",
        "mathring" => "\u{30a}",
        "overleftarrow" => "\u{20d6}",
        "overleftrightarrow" => "\u{20e1}",
        "overline" => "\u{305}",
        "overrightarrow" => "\u{20d7}",
        "tilde" => "\u{303}",
        "underline" => "\u{332}",
        "vec" => "\u{20d7}",
        "widehat" => "\u{302}",
        "widetilde" => "\u{303}",
        _ => return None,
    })
}
