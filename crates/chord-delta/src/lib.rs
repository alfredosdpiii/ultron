//! Chord delta operations over JSON: validation, application, and the wire codec.
//!
//! Port of `packages/chord/src/delta/index.ts` from earendil-works/pi (MIT); the pinned revision is recorded in
//! `fixtures/delta.json`. Errors carry the upstream error name and message so fixtures compare them exactly.
//!
//! Differences from upstream, all of which surface as errors rather than divergent values:
//! - Ops are parsed into [`Op`] before application, so a malformed op is reported even when an earlier op in the
//!   same batch would have failed first.
//! - A trim that splits a UTF-16 surrogate pair fails ([`DeltaError::SplitSurrogate`]): upstream leaves a lone
//!   surrogate, which a Rust `String` cannot hold.
//! - An integer path segment above `usize::MAX` is rejected as unsafe; upstream reports it as unresolvable.
//! - Object key order follows insertion; upstream JavaScript lists integer-like keys first. Key order is not part of
//!   the upstream contract.

mod tracker;

pub use tracker::{Draft, Prepared, Tracker, overlap};

use serde_json::Value;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;

/// Path segments that reach the JavaScript prototype chain; rejected everywhere for wire compatibility.
pub const RESERVED_SEGMENTS: [&str; 3] = ["__proto__", "constructor", "prototype"];

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Seg {
    Key(String),
    Index(usize),
}

impl Seg {
    fn key(&self) -> Cow<'_, str> {
        match self {
            Seg::Key(key) => Cow::Borrowed(key),
            Seg::Index(index) => Cow::Owned(index.to_string()),
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Seg::Key(key) => Value::String(key.clone()),
            Seg::Index(index) => Value::from(*index),
        }
    }
}

pub type Path = Vec<Seg>;

/// A decoded operation. `Set`, `Delete`, `Append` and `Trim` never target the root; `Splice` and `Move` may.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Replace(Value),
    Set(Path, Value),
    Delete(Path),
    Append(Path, String),
    /// Remove this many UTF-16 code units from the front of a string.
    Trim(Path, usize),
    /// Splice an array: index, remove count, inserted items.
    Splice(Path, usize, usize, Vec<Value>),
    /// Reorder an array: `new[i] = old[permutation[i]]`.
    Move(Path, Vec<usize>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeltaError {
    /// Malformed op or wire tuple (upstream `TypeError`).
    Invalid(String),
    /// The path or path id does not resolve (upstream `PathError`); holds the JSON rendering.
    Path(String),
    /// A reserved, negative, fractional, or out-of-range segment (upstream `UnsafePathError`).
    UnsafePath(String),
    /// A trim would split a UTF-16 surrogate pair; holds the JSON rendering of the path.
    SplitSurrogate(String),
}

impl DeltaError {
    /// The upstream error class name.
    pub fn name(&self) -> &'static str {
        match self {
            DeltaError::Invalid(_) => "TypeError",
            DeltaError::Path(_) => "PathError",
            DeltaError::UnsafePath(_) => "UnsafePathError",
            DeltaError::SplitSurrogate(_) => "SplitSurrogateError",
        }
    }
}

impl fmt::Display for DeltaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeltaError::Invalid(message) => f.write_str(message),
            DeltaError::Path(path) => write!(f, "unresolvable path: {path}"),
            DeltaError::UnsafePath(segment) => write!(f, "unsafe path segment: {segment}"),
            DeltaError::SplitSurrogate(path) => write!(f, "trim splits a surrogate pair: {path}"),
        }
    }
}

impl std::error::Error for DeltaError {}

type Result<T> = std::result::Result<T, DeltaError>;

fn invalid<T>(message: &str) -> Result<T> {
    Err(DeltaError::Invalid(message.to_owned()))
}

fn path_json(path: &[Seg]) -> Value {
    Value::Array(path.iter().map(Seg::to_json).collect())
}

fn path_error(path: &[Seg]) -> DeltaError {
    DeltaError::Path(path_json(path).to_string())
}

/// JavaScript `String(value)`, for error messages that echo untrusted input.
fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| if item.is_null() { String::new() } else { js_string(item) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
        other => other.to_string(),
    }
}

/// `Number.isInteger(value) && value >= 0`, saturated to `usize`.
fn non_negative_int(value: &Value) -> Option<usize> {
    if let Some(n) = value.as_u64() {
        return Some(usize::try_from(n).unwrap_or(usize::MAX));
    }
    let n = value.as_f64()?;
    // Saturating float-to-int cast: integers past usize::MAX only ever act as "past the end".
    (n.is_finite() && n.fract() == 0.0 && n >= 0.0).then_some(n as usize)
}

fn parse_seg(value: &Value) -> Result<Seg> {
    if let Value::String(key) = value {
        if RESERVED_SEGMENTS.contains(&key.as_str()) {
            return Err(DeltaError::UnsafePath(key.clone()));
        }
        return Ok(Seg::Key(key.clone()));
    }
    match non_negative_int(value) {
        Some(index) if index != usize::MAX => Ok(Seg::Index(index)),
        _ => Err(DeltaError::UnsafePath(js_string(value))),
    }
}

fn parse_path(value: &Value, non_empty: bool) -> Result<Path> {
    let Value::Array(segments) = value else {
        return invalid("path is not an array");
    };
    if non_empty && segments.is_empty() {
        return invalid("path is empty");
    }
    segments.iter().map(parse_seg).collect()
}

fn parse_permutation(value: &Value) -> Result<Vec<usize>> {
    let Value::Array(items) = value else {
        return invalid("m permutation is not an array");
    };
    let mut seen = vec![false; items.len()];
    items
        .iter()
        .map(|item| match non_negative_int(item) {
            Some(index) if index < seen.len() && !seen[index] => {
                seen[index] = true;
                Ok(index)
            }
            _ => invalid("m permutation is not a bijection"),
        })
        .collect()
}

fn is_container(value: &Value) -> bool {
    value.is_array() || value.is_object()
}

impl Op {
    /// Parse and validate a decoded op tuple (upstream `assertValidOp`).
    pub fn from_json(value: &Value) -> Result<Op> {
        let op = match value {
            Value::Array(op) if !op.is_empty() => op,
            _ => return invalid("op is not a tuple"),
        };
        let verb = match &op[0] {
            Value::String(verb) => verb.as_str(),
            other => return Err(DeltaError::Invalid(format!("unknown op verb: {}", js_string(other)))),
        };
        match verb {
            "r" if op.len() == 2 => Ok(Op::Replace(op[1].clone())),
            "r" => invalid("r arity"),
            "s" if op.len() == 3 => Ok(Op::Set(parse_path(&op[1], true)?, op[2].clone())),
            "s" => invalid("s arity"),
            "d" if op.len() == 2 => Ok(Op::Delete(parse_path(&op[1], true)?)),
            "d" => invalid("d arity"),
            "a" => match (op.len(), op.get(2)) {
                (3, Some(Value::String(text))) => Ok(Op::Append(parse_path(&op[1], true)?, text.clone())),
                _ => invalid("a shape"),
            },
            "t" => match (op.len(), op.get(2).and_then(non_negative_int)) {
                (3, Some(count)) => Ok(Op::Trim(parse_path(&op[1], true)?, count)),
                _ => invalid("t shape"),
            },
            "p" => {
                if op.len() != 5 {
                    return invalid("p arity");
                }
                let path = parse_path(&op[1], false)?;
                let Some(index) = non_negative_int(&op[2]) else {
                    return invalid("p index");
                };
                let Some(remove) = non_negative_int(&op[3]) else {
                    return invalid("p remove");
                };
                let Value::Array(items) = &op[4] else {
                    return invalid("p items");
                };
                Ok(Op::Splice(path, index, remove, items.clone()))
            }
            "m" if op.len() == 3 => Ok(Op::Move(parse_path(&op[1], false)?, parse_permutation(&op[2])?)),
            "m" => invalid("m arity"),
            other => Err(DeltaError::Invalid(format!("unknown op verb: {other}"))),
        }
    }

    pub fn to_json(&self) -> Value {
        match self.path() {
            None => Value::Array(vec![Value::from(self.verb()), self.tail().remove(0)]),
            Some(path) => {
                let mut tuple = vec![Value::from(self.verb()), path_json(path)];
                tuple.extend(self.tail());
                Value::Array(tuple)
            }
        }
    }

    fn verb(&self) -> &'static str {
        match self {
            Op::Replace(_) => "r",
            Op::Set(..) => "s",
            Op::Delete(_) => "d",
            Op::Append(..) => "a",
            Op::Trim(..) => "t",
            Op::Splice(..) => "p",
            Op::Move(..) => "m",
        }
    }

    fn path(&self) -> Option<&Path> {
        match self {
            Op::Replace(_) => None,
            Op::Set(path, _)
            | Op::Delete(path)
            | Op::Append(path, _)
            | Op::Trim(path, _)
            | Op::Splice(path, ..)
            | Op::Move(path, _) => Some(path),
        }
    }

    /// The tuple elements after the path (for `Replace`, the value).
    fn tail(&self) -> Vec<Value> {
        match self {
            Op::Replace(value) | Op::Set(_, value) => vec![value.clone()],
            Op::Delete(_) => vec![],
            Op::Append(_, text) => vec![Value::from(text.as_str())],
            Op::Trim(_, count) => vec![Value::from(*count)],
            Op::Splice(_, index, remove, items) => {
                vec![Value::from(*index), Value::from(*remove), Value::Array(items.clone())]
            }
            Op::Move(_, permutation) => vec![Value::from(permutation.clone())],
        }
    }
}

// ─── Applier ─────────────────────────────────────────────────────────────────

/// Apply ops to a value the caller owns exclusively. On error the value is lost: recover from a later `Replace`.
pub fn apply(mut root: Value, ops: Vec<Op>) -> Result<Value> {
    for op in ops {
        apply_op(&mut root, op)?;
    }
    Ok(root)
}

/// Apply one batch without changing `root`.
pub fn apply_immutable(root: &Value, ops: &[Op]) -> Result<Value> {
    apply_immutable_batches(root, [ops])
}

/// Apply ordered batches without changing `root`, returning only the final value.
pub fn apply_immutable_batches<'a>(root: &Value, batches: impl IntoIterator<Item = &'a [Op]>) -> Result<Value> {
    // ponytail: clones the whole root once per call; move to a structurally shared tree if replay cost shows up.
    let mut result = root.clone();
    for ops in batches {
        for op in ops {
            match op {
                Op::Replace(_) => {}
                Op::Splice(path, ..) | Op::Move(path, _) => check_copy_walk(&result, path)?,
                Op::Set(path, _) | Op::Delete(path) | Op::Append(path, _) | Op::Trim(path, _) => {
                    check_copy_walk(&result, &path[..path.len().saturating_sub(1)])?;
                }
            }
            apply_op(&mut result, op.clone())?;
        }
    }
    Ok(result)
}

/// Upstream's copy-on-write walk checks the path in a different order than the applier, so a few malformed paths
/// fail with a different error there. Mirror it to report the same error.
fn check_copy_walk(root: &Value, path: &[Seg]) -> Result<()> {
    if !is_container(root) {
        return Err(path_error(path));
    }
    let mut node = root;
    for seg in path {
        let child = match (node, seg) {
            (Value::Array(items), Seg::Index(index)) => items.get(*index),
            (Value::Array(items), Seg::Key(key)) => {
                // JavaScript `Object.hasOwn(array, key)` is true for "length" and canonical in-range indices.
                let own_index = key
                    .parse::<usize>()
                    .is_ok_and(|i| i < items.len() && i.to_string() == *key);
                if key == "length" || own_index {
                    return Err(DeltaError::UnsafePath(key.clone()));
                }
                None
            }
            (Value::Object(map), seg) => map.get(seg.key().as_ref()),
            _ => None,
        };
        match child {
            Some(child) if is_container(child) => node = child,
            _ => return Err(path_error(path)),
        }
    }
    Ok(())
}

/// Resolve a container at `path` (upstream `resolve`).
fn resolve_mut<'a>(root: &'a mut Value, path: &[Seg]) -> Result<&'a mut Value> {
    let mut node = root;
    for seg in path {
        node = match (node, seg) {
            (Value::Array(items), Seg::Index(index)) => items.get_mut(*index).ok_or_else(|| path_error(path))?,
            (Value::Array(_), Seg::Key(key)) => return Err(DeltaError::UnsafePath(key.clone())),
            (Value::Object(map), seg) => map.get_mut(seg.key().as_ref()).ok_or_else(|| path_error(path))?,
            _ => return Err(path_error(path)),
        };
    }
    if is_container(node) {
        Ok(node)
    } else {
        Err(path_error(path))
    }
}

enum Leaf {
    Set(Value),
    Delete,
    Append(String),
    Trim(usize),
}

fn apply_op(root: &mut Value, op: Op) -> Result<()> {
    let (path, leaf) = match op {
        Op::Replace(value) => {
            *root = value;
            return Ok(());
        }
        Op::Splice(path, index, remove, items) => {
            let Value::Array(target) = resolve_mut(root, &path)? else {
                return Err(path_error(&path));
            };
            let start = index.min(target.len());
            let end = start + remove.min(target.len() - start);
            target.splice(start..end, items);
            return Ok(());
        }
        Op::Move(path, permutation) => {
            let target = match resolve_mut(root, &path)? {
                Value::Array(target) if target.len() == permutation.len() => target,
                _ => return Err(path_error(&path)),
            };
            let mut previous: Vec<Option<Value>> = target.drain(..).map(Some).collect();
            target.extend(permutation.iter().map(|&i| previous[i].take().unwrap_or(Value::Null)));
            return Ok(());
        }
        Op::Set(path, value) => (path, Leaf::Set(value)),
        Op::Delete(path) => (path, Leaf::Delete),
        Op::Append(path, text) => (path, Leaf::Append(text)),
        Op::Trim(path, count) => (path, Leaf::Trim(count)),
    };
    let Some((key, parent_path)) = path.split_last() else {
        return Err(path_error(&path));
    };
    let slot = match resolve_mut(root, parent_path)? {
        Value::Array(items) => {
            let index = match key {
                Seg::Index(index) => *index,
                Seg::Key(key) => return Err(DeltaError::UnsafePath(key.clone())),
            };
            if index > items.len() {
                return Err(DeltaError::UnsafePath(index.to_string()));
            }
            match leaf {
                Leaf::Set(value) if index == items.len() => {
                    items.push(value);
                    return Ok(());
                }
                Leaf::Delete if index < items.len() => {
                    items.remove(index);
                    return Ok(());
                }
                Leaf::Delete => return Err(path_error(&path)),
                leaf => (items.get_mut(index), leaf),
            }
        }
        Value::Object(map) => {
            let key = key.key();
            match leaf {
                Leaf::Set(value) => {
                    map.insert(key.into_owned(), value);
                    return Ok(());
                }
                Leaf::Delete => {
                    map.shift_remove(key.as_ref());
                    return Ok(());
                }
                leaf => (map.get_mut(key.as_ref()), leaf),
            }
        }
        _ => return Err(path_error(parent_path)),
    };
    match slot {
        (Some(slot), Leaf::Set(value)) => *slot = value,
        (Some(Value::String(current)), Leaf::Append(text)) => current.push_str(&text),
        (Some(Value::String(current)), Leaf::Trim(count)) => trim_utf16(current, count, &path)?,
        _ => return Err(path_error(&path)),
    }
    Ok(())
}

/// Remove `count` UTF-16 code units from the front of `text`.
fn trim_utf16(text: &mut String, count: usize, path: &[Seg]) -> Result<()> {
    let mut units = 0;
    for (offset, ch) in text.char_indices() {
        if units >= count {
            if units > count {
                return Err(DeltaError::SplitSurrogate(path_json(path).to_string()));
            }
            text.drain(..offset);
            return Ok(());
        }
        units += ch.len_utf16();
    }
    if units > count {
        return Err(DeltaError::SplitSurrogate(path_json(path).to_string()));
    }
    text.clear();
    Ok(())
}

// ─── Wire codec ──────────────────────────────────────────────────────────────
//
// One encoder/decoder pair per ordered state stream. Path ids span batches; a `Replace` resets them, and the
// omitted-path short form is scoped to one batch.

/// Interns a path on its second use and omits a path equal to the previous op's.
#[derive(Default)]
pub struct Encoder {
    seen: HashSet<Path>,
    ids: HashMap<Path, u64>,
    next_id: u64,
}

impl Encoder {
    pub fn encode(&mut self, ops: &[Op]) -> Vec<Value> {
        let mut previous: Option<&Path> = None;
        let mut out = Vec::new();
        for op in ops {
            let Some(path) = op.path() else {
                out.push(op.to_json());
                self.seen.clear();
                self.ids.clear();
                self.next_id = 0;
                previous = None;
                continue;
            };
            let mut tuple = vec![Value::from(op.verb())];
            if previous != Some(path) {
                if let Some(id) = self.ids.get(path) {
                    tuple.push(Value::from(*id));
                } else if self.seen.contains(path) {
                    let id = self.next_id;
                    self.next_id += 1;
                    self.ids.insert(path.clone(), id);
                    out.push(Value::Array(vec![Value::from("#"), Value::from(id), path_json(path)]));
                    tuple.push(Value::from(id));
                } else {
                    self.seen.insert(path.clone());
                    tuple.push(path_json(path));
                }
                previous = Some(path);
            }
            tuple.extend(op.tail());
            out.push(Value::Array(tuple));
        }
        out
    }
}

/// Validate a wire tuple: ids and short forms are legal here (upstream `assertValidWireOp`).
pub fn validate_wire_op(value: &Value) -> Result<()> {
    let op = match value {
        Value::Array(op) if !op.is_empty() => op,
        _ => return invalid("op is not a tuple"),
    };
    let reference = |r: &Value| -> Result<()> {
        if r.is_number() {
            return non_negative_int(r)
                .map(|_| ())
                .ok_or_else(|| DeltaError::Invalid("bad path id".to_owned()));
        }
        parse_path(r, false).map(|_| ())
    };
    let count = |v: &Value, message: &str| non_negative_int(v).map(|_| ()).map_or_else(|| invalid(message), Ok);
    let verb = match &op[0] {
        Value::String(verb) => verb.as_str(),
        other => return Err(DeltaError::Invalid(format!("unknown op verb: {}", js_string(other)))),
    };
    match (verb, op.len()) {
        ("r", 2) | ("s", 2) | ("d", 1) => Ok(()),
        ("r", _) => invalid("r arity"),
        ("s", 3) | ("d", 2) => reference(&op[1]),
        ("s", _) => invalid("s arity"),
        ("d", _) => invalid("d arity"),
        ("a", 2 | 3) => {
            if op.len() == 3 {
                reference(&op[1])?;
            }
            if op[op.len() - 1].is_string() {
                Ok(())
            } else {
                invalid("a value")
            }
        }
        ("a", _) => invalid("a arity"),
        ("t", 2 | 3) => {
            if op.len() == 3 {
                reference(&op[1])?;
            }
            count(&op[op.len() - 1], "t count")
        }
        ("t", _) => invalid("t arity"),
        ("p", 4 | 5) => {
            let rest = &op[op.len() - 3..];
            if op.len() == 5 {
                reference(&op[1])?;
            }
            count(&rest[0], "p index")?;
            count(&rest[1], "p remove")?;
            if rest[2].is_array() { Ok(()) } else { invalid("p items") }
        }
        ("p", _) => invalid("p arity"),
        ("m", 2 | 3) => {
            if op.len() == 3 {
                reference(&op[1])?;
            }
            parse_permutation(&op[op.len() - 1]).map(|_| ())
        }
        ("m", _) => invalid("m arity"),
        ("#", 3) if non_negative_int(&op[1]).is_some() && op[2].is_array() => parse_path(&op[2], false).map(|_| ()),
        ("#", _) => invalid("# shape"),
        (other, _) => Err(DeltaError::Invalid(format!("unknown op verb: {other}"))),
    }
}

/// Validates wire tuples and restores decoded ops. After an error, discard it and recover from a later `Replace`.
#[derive(Default)]
pub struct Decoder {
    paths: HashMap<usize, Path>,
}

impl Decoder {
    pub fn decode(&mut self, wire: &[Value]) -> Result<Vec<Op>> {
        let mut previous: Option<Path> = None;
        let mut out = Vec::new();
        for value in wire {
            validate_wire_op(value)?;
            let op = value.as_array().map(Vec::as_slice).unwrap_or_default();
            let verb = op[0].as_str().unwrap_or_default();
            if verb == "#" {
                let id = non_negative_int(&op[1]).unwrap_or(usize::MAX);
                self.paths.insert(id, parse_path(&op[2], false)?);
                continue;
            }
            if verb == "r" {
                out.push(Op::Replace(op[1].clone()));
                self.paths.clear();
                previous = None;
                continue;
            }
            let short = match verb {
                "d" => op.len() == 1,
                "p" => op.len() == 4,
                _ => op.len() == 2,
            };
            let (path, tail) = if short {
                (previous.clone().ok_or_else(|| path_error(&[]))?, &op[1..])
            } else {
                let path = match non_negative_int(&op[1]) {
                    Some(id) => self
                        .paths
                        .get(&id)
                        .cloned()
                        .ok_or_else(|| DeltaError::Path(id.to_string()))?,
                    None => parse_path(&op[1], false)?,
                };
                previous = Some(path.clone());
                (path, &op[2..])
            };
            if path.is_empty() && verb != "p" && verb != "m" {
                return Err(path_error(&path));
            }
            let mut tuple = vec![op[0].clone(), path_json(&path)];
            tuple.extend_from_slice(tail);
            out.push(Op::from_json(&Value::Array(tuple))?);
        }
        Ok(out)
    }
}
