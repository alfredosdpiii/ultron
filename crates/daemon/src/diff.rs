//! `diffRevisions` (packages/chord/src/state/diff.ts): the compact op batch a replicated state publishes between two
//! revisions.
//!
//! Upstream compares array items by reference for its permutation and shift detection, which only finds moved
//! containers when a draft kept their identity. Rust values have no identity: here only primitives are matched by
//! value in those two searches, and containers always fall back to the per-index diff, as a freshly built revision
//! does upstream. Counting mismatches uses deep equality, which emits the same ops as upstream's identity count.

use chord_delta::{Op, Path, RESERVED_SEGMENTS, Seg, overlap};
use serde_json::{Map, Value};

const DEFAULT_OVERLAP_SCAN: usize = 65_536;
const MAX_DELTA_OPERATIONS: usize = 4_096;

fn emit_set(path: &[Seg], value: &Value, ops: &mut Vec<Op>) {
    if path.is_empty() {
        ops.push(Op::Replace(value.clone()));
    } else {
        ops.push(Op::Set(path.to_vec(), value.clone()));
    }
}

fn is_primitive(value: &Value) -> bool {
    !matches!(value, Value::Array(_) | Value::Object(_))
}

/// Upstream `===` on array items: primitives by value, containers never (see the module docs).
fn same(left: &Value, right: &Value) -> bool {
    is_primitive(left) && left == right
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

fn utf16_tail(text: &str, skip: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().skip(skip).collect();
    String::from_utf16_lossy(&units)
}

fn emit_string(before: &str, after: &str, path: &[Seg], ops: &mut Vec<Op>) {
    if before == after {
        return;
    }
    if after.len() > before.len() && after.starts_with(before) {
        ops.push(Op::Append(path.to_vec(), after[before.len()..].to_string()));
        return;
    }
    let shared = overlap(before, after, DEFAULT_OVERLAP_SCAN);
    if shared == 0 {
        ops.push(Op::Set(path.to_vec(), Value::String(after.to_string())));
        return;
    }
    ops.push(Op::Trim(path.to_vec(), utf16_len(before) - shared));
    if utf16_len(after) > shared {
        ops.push(Op::Append(path.to_vec(), utf16_tail(after, shared)));
    }
}

fn permutation(before: &[Value], after: &[Value]) -> Option<Vec<usize>> {
    if before.len() != after.len() || !before.iter().chain(after).all(is_primitive) {
        return None;
    }
    let mut used = vec![false; before.len()];
    let mut order = Vec::with_capacity(after.len());
    for item in after {
        let index = (0..before.len()).find(|&i| !used[i] && before[i] == *item)?;
        used[index] = true;
        order.push(index);
    }
    Some(order)
}

fn emit_same_length_shift(before: &[Value], after: &[Value], path: &[Seg], ops: &mut Vec<Op>) -> bool {
    let length = before.len();
    if length < 2 {
        return false;
    }
    let mut candidates = 0;
    let mut removed = 1;
    while removed < length && candidates < 16 {
        if same(&before[removed], &after[0]) {
            candidates += 1;
            if (removed..length).all(|i| same(&before[i], &after[i - removed])) {
                ops.push(Op::Splice(path.to_vec(), 0, removed, Vec::new()));
                ops.push(Op::Splice(
                    path.to_vec(),
                    length - removed,
                    0,
                    after[length - removed..].to_vec(),
                ));
                return true;
            }
        }
        removed += 1;
    }
    candidates = 0;
    let mut inserted = 1;
    while inserted < length && candidates < 16 {
        if same(&after[inserted], &before[0]) {
            candidates += 1;
            if (inserted..length).all(|i| same(&after[i], &before[i - inserted])) {
                ops.push(Op::Splice(path.to_vec(), 0, 0, after[..inserted].to_vec()));
                ops.push(Op::Splice(path.to_vec(), length, inserted, Vec::new()));
                return true;
            }
        }
        inserted += 1;
    }
    false
}

fn with(path: &[Seg], segment: Seg) -> Path {
    let mut next = path.to_vec();
    next.push(segment);
    next
}

fn diff_array(before: &[Value], after: &[Value], path: &[Seg], ops: &mut Vec<Op>) {
    if before.len() == after.len() {
        let mismatches = before.iter().zip(after).filter(|(b, a)| b != a).count();
        if mismatches == 0 {
            return;
        }
        if mismatches > 1 {
            if let Some(order) = permutation(before, after) {
                ops.push(Op::Move(path.to_vec(), order));
                return;
            }
            if emit_same_length_shift(before, after, path, ops) {
                return;
            }
        }
        for (index, (b, a)) in before.iter().zip(after).enumerate() {
            if ops.len() > MAX_DELTA_OPERATIONS {
                return;
            }
            diff_value(b, a, &with(path, Seg::Index(index)), ops);
        }
        return;
    }
    let shortest = before.len().min(after.len());
    let mut prefix = 0;
    while prefix < shortest && before[prefix] == after[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < shortest - prefix && before[before.len() - 1 - suffix] == after[after.len() - 1 - suffix] {
        suffix += 1;
    }
    ops.push(Op::Splice(
        path.to_vec(),
        prefix,
        before.len() - prefix - suffix,
        after[prefix..after.len() - suffix].to_vec(),
    ));
}

fn diff_object(before: &Map<String, Value>, after: &Map<String, Value>, path: &[Seg], ops: &mut Vec<Op>) {
    if before
        .keys()
        .chain(after.keys())
        .any(|k| RESERVED_SEGMENTS.contains(&k.as_str()))
    {
        if before != after {
            emit_set(path, &Value::Object(after.clone()), ops);
        }
        return;
    }
    for (key, value) in after {
        if ops.len() > MAX_DELTA_OPERATIONS {
            return;
        }
        let child = with(path, Seg::Key(key.clone()));
        match before.get(key) {
            Some(previous) => diff_value(previous, value, &child, ops),
            None => emit_set(&child, value, ops),
        }
    }
    for key in before.keys() {
        if ops.len() > MAX_DELTA_OPERATIONS {
            return;
        }
        if !after.contains_key(key) {
            ops.push(Op::Delete(with(path, Seg::Key(key.clone()))));
        }
    }
}

fn diff_value(before: &Value, after: &Value, path: &[Seg], ops: &mut Vec<Op>) {
    if ops.len() > MAX_DELTA_OPERATIONS || (is_primitive(before) && before == after) {
        return;
    }
    match (before, after) {
        (Value::String(b), Value::String(a)) if !path.is_empty() => emit_string(b, a, path, ops),
        (Value::Array(b), Value::Array(a)) => diff_array(b, a, path, ops),
        (Value::Object(b), Value::Object(a)) => diff_object(b, a, path, ops),
        _ => emit_set(path, after, ops),
    }
}

fn number_text(value: &serde_json::Number) -> usize {
    value.to_string().len()
}

fn json_cost(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::String(s) => utf16_len(s) + 2,
        Value::Number(n) => number_text(n),
        Value::Bool(b) => {
            if *b {
                4
            } else {
                5
            }
        }
        Value::Array(items) => 2 + items.iter().map(json_cost).sum::<usize>() + items.len().saturating_sub(1),
        Value::Object(map) => {
            2 + map.iter().map(|(k, v)| utf16_len(k) + 3 + json_cost(v)).sum::<usize>() + map.len().saturating_sub(1)
        }
    }
}

fn path_cost(path: &[Seg]) -> usize {
    2 + path
        .iter()
        .map(|s| match s {
            Seg::Key(k) => utf16_len(k) + 2,
            Seg::Index(i) => i.to_string().len(),
        })
        .sum::<usize>()
        + path.len().saturating_sub(1)
}

fn op_cost(op: &Op) -> usize {
    match op {
        Op::Replace(v) => 6 + json_cost(v),
        Op::Set(p, v) => 7 + path_cost(p) + json_cost(v),
        Op::Delete(p) => 6 + path_cost(p),
        Op::Append(p, s) => 7 + path_cost(p) + utf16_len(s) + 2,
        Op::Trim(p, n) => 7 + path_cost(p) + n.to_string().len(),
        Op::Splice(p, i, r, items) => {
            10 + path_cost(p) + i.to_string().len() + r.to_string().len() + json_cost(&Value::Array(items.clone()))
        }
        Op::Move(p, order) => {
            7 + path_cost(p) + json_cost(&Value::Array(order.iter().map(|&i| Value::from(i)).collect()))
        }
    }
}

/// Compute a compact operation batch from two JSON revisions.
pub fn diff_revisions(before: &Value, after: &Value) -> Vec<Op> {
    let mut ops = Vec::new();
    diff_value(before, after, &[], &mut ops);
    if ops.len() > MAX_DELTA_OPERATIONS {
        return vec![Op::Replace(after.clone())];
    }
    if ops.is_empty() || matches!(ops[0], Op::Replace(_)) {
        return ops;
    }
    let delta_cost = 2 + ops.iter().map(|op| op_cost(op) + 1).sum::<usize>();
    if delta_cost < 65_536 {
        return ops;
    }
    let snapshot_cost = json_cost(after) + 6;
    if delta_cost >= snapshot_cost {
        vec![Op::Replace(after.clone())]
    } else {
        ops
    }
}
