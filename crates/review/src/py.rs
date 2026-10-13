//! Python string semantics the ported code depends on: `str.splitlines`, `str.split()`, `repr`, `str.format` with
//! named fields, and `shlex.split` (POSIX mode, no comments).

/// Python's `str.isspace` for the characters `str.split()` and `str.strip()` treat as whitespace.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// `str.strip()`.
pub fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// `str.split()` with no separator.
pub fn split_ws(text: &str) -> impl Iterator<Item = &str> {
    text.split(is_space).filter(|part| !part.is_empty())
}

/// `" ".join(text.split())`.
pub fn squash(text: &str) -> String {
    split_ws(text).collect::<Vec<_>>().join(" ")
}

/// `str.splitlines()`: splits on every Python line boundary (`\r\n` counts once) and drops no final empty line.
pub fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, c)) = chars.next() {
        if !matches!(
            c,
            '\n' | '\r' | '\u{0b}' | '\u{0c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}'
        ) {
            continue;
        }
        lines.push(&text[start..index]);
        let mut end = index + c.len_utf8();
        if c == '\r' && chars.peek().is_some_and(|&(_, next)| next == '\n') {
            chars.next();
            end += 1;
        }
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// Cut `text` to `limit` characters, ending in an ellipsis when cut.
pub fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit - 1).collect();
    out.push('…');
    out
}

/// `repr(str)`.
pub fn repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            // ponytail: control characters only; Python also escapes other unprintable classes (Zl, Zp, Cf, unassigned).
            c if c.is_control() => {
                let code = c as u32;
                if code < 0x100 {
                    out.push_str(&format!("\\x{code:02x}"));
                } else {
                    out.push_str(&format!("\\u{code:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `template.format(**fields)` for named fields; `{{` and `}}` are literal braces. Fails on an unknown field.
pub fn format(template: &str, fields: &[(&str, &str)]) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(index) = rest.find(['{', '}']) {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
        } else if let Some(stripped) = tail.strip_prefix('{') {
            let end = stripped.find('}').ok_or("single '{' encountered in format string")?;
            let name = &stripped[..end];
            let value = fields
                .iter()
                .find(|(key, _)| *key == name)
                .ok_or_else(|| format!("KeyError: {name:?}"))?;
            out.push_str(value.1);
            rest = &stripped[end + 1..];
        } else {
            return Err("single '}' encountered in format string".to_string());
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// `shlex.split(text)`: POSIX quoting, `\` escapes outside quotes and before `"` or `\` inside double quotes, no
/// comments. Errors carry Python's messages.
pub fn shlex_split(text: &str) -> Result<Vec<String>, &'static str> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Space,
        Word,
        Quote(char),
        Escape(Option<char>),
    }
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut state = State::Space;
    for c in text.chars() {
        match state {
            State::Space | State::Word => {
                if matches!(c, ' ' | '\t' | '\r' | '\n') {
                    if !token.is_empty() || quoted {
                        tokens.push(std::mem::take(&mut token));
                        quoted = false;
                    }
                    state = State::Space;
                } else if c == '\'' || c == '"' {
                    quoted = true;
                    state = State::Quote(c);
                } else if c == '\\' {
                    state = State::Escape(None);
                } else {
                    token.push(c);
                    state = State::Word;
                }
            }
            State::Quote(q) => {
                if c == q {
                    state = State::Word;
                } else if q == '"' && c == '\\' {
                    state = State::Escape(Some(q));
                } else {
                    token.push(c);
                }
            }
            State::Escape(inside) => {
                if let Some(q) = inside
                    && c != q
                    && c != '\\'
                {
                    token.push('\\');
                }
                token.push(c);
                state = inside.map_or(State::Word, State::Quote);
            }
        }
    }
    match state {
        State::Quote(_) => return Err("No closing quotation"),
        State::Escape(_) => return Err("No escaped character"),
        State::Space | State::Word => {}
    }
    if !token.is_empty() || quoted {
        tokens.push(token);
    }
    Ok(tokens)
}

/// `str(value)` for a JSON value as Python would hold it (`None` is the empty string here, as `_text` treats it).
pub fn str_of(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(text) => text.clone(),
        other => repr_value(other),
    }
}

pub fn repr_value(value: &serde_json::Value) -> String {
    use serde_json::Value;
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => match number.as_f64() {
            Some(float) if number.is_f64() => float_repr(float),
            _ => number.to_string(),
        },
        Value::String(text) => repr(text),
        Value::Array(items) => format!("[{}]", items.iter().map(repr_value).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(key, item)| format!("{}: {}", repr(key), repr_value(item)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Python's float repr: shortest round-trip digits, `.0` on integral values, exponent outside [1e-4, 1e16).
pub fn float_repr(value: f64) -> String {
    let magnitude = value.abs();
    if magnitude != 0.0 && !(1e-4..1e16).contains(&magnitude) {
        let text = format!("{value:e}");
        let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
        let (sign, digits) = exponent
            .strip_prefix('-')
            .map_or(("+", exponent), |digits| ("-", digits));
        return format!("{mantissa}e{sign}{digits:0>2}");
    }
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        format!("{value}")
    }
}

/// `f"{n:,}"`: an integer with comma thousands separators.
pub fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// `str.rstrip()`.
pub fn rstrip(text: &str) -> &str {
    text.trim_end_matches(is_space)
}

/// `f"{value}"` for a JSON value as Python holds it (`None` for null or a missing key).
pub fn fstr(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => "None".to_string(),
        Some(value) => str_of(value),
    }
}
