//! Revision tracking: a [`Tracker`] owns an immutable root, a [`Draft`] records one change against it, and
//! [`Draft::prepare`] turns the change into the Chord ops upstream's tracker emits for the same mutations.
//!
//! Port of `packages/chord/src/delta/tracker.ts` from earendil-works/pi (MIT). Upstream drafts are JavaScript Proxies;
//! here a draft is an explicit handle whose methods mirror the property writes and array mutators upstream observes.
//!
//! Differences from upstream:
//! - Supported mutations: object `set`/`delete`, array index `set`, `push`, `pop`, `shift`, `unshift`, `splice` and
//!   `set_len`. `reverse`, `sort`, `fill` and `copyWithin` are not ported, so no `m` (move) op is ever emitted.
//! - Dense-region splices (upstream folds 256+ overwritten indices of one array into a single `p` op) are not
//!   ported: such a batch emits one `s` per index. Both replay to the same value.
//! - Revisions share no structure: a prepared value is the base with the ops applied to a copy.

use crate::{DeltaError, Op, Path, RESERVED_SEGMENTS, Seg, apply_immutable};
use indexmap::{IndexMap, IndexSet};
use serde_json::{Map, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

type Result<T> = std::result::Result<T, DeltaError>;

/// Upstream emits one root replacement instead of a batch longer than this.
const MAX_DELTA_OPERATIONS: usize = 4_096;
/// Upstream `overlap()` scan bound used when diffing strings.
const OVERLAP_SCAN: usize = 65_536;

const SETTLED: &str = "Cannot use a settled overlay";

impl From<&str> for Seg {
    fn from(key: &str) -> Seg {
        Seg::Key(key.to_owned())
    }
}

impl From<String> for Seg {
    fn from(key: String) -> Seg {
        Seg::Key(key)
    }
}

impl From<usize> for Seg {
    fn from(index: usize) -> Seg {
        Seg::Index(index)
    }
}

/// Build a [`Path`] from keys and indices: `path!["items", 0]`.
#[macro_export]
macro_rules! path {
    ($($segment:expr),* $(,)?) => { vec![$($crate::Seg::from($segment)),*] as $crate::Path };
}

/// Owner of one immutable revision; adoption swaps in a prepared revision.
#[derive(Debug)]
pub struct Tracker {
    value: Arc<Value>,
    revision: u64,
}

/// A prepared change: the base it was made against, the resulting revision, and the exact ops between them.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub base: Arc<Value>,
    pub value: Arc<Value>,
    pub ops: Vec<Op>,
    pub base_revision: u64,
}

impl Tracker {
    /// Take ownership of a root container (upstream `track()`).
    pub fn new(value: Value) -> Tracker {
        Tracker {
            value: Arc::new(value),
            revision: 0,
        }
    }

    pub fn value(&self) -> &Arc<Value> {
        &self.value
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Start a change against the current revision.
    pub fn begin_change(&self) -> Result<Draft> {
        let root =
            Node::new(&self.value).ok_or_else(|| DeltaError::Invalid("Tracked roots must be containers".into()))?;
        Ok(Draft(Rc::new(RefCell::new(DraftState {
            open: true,
            base: self.value.clone(),
            base_revision: self.revision,
            root,
            dirty_counter: 0,
        }))))
    }

    /// Adopt a change prepared against the current revision.
    pub fn adopt(&mut self, prepared: &Prepared) -> Result<()> {
        if prepared.base_revision != self.revision || !Arc::ptr_eq(&prepared.base, &self.value) {
            return Err(DeltaError::Invalid("Prepared change is stale".into()));
        }
        self.value = prepared.value.clone();
        self.revision += 1;
        Ok(())
    }
}

/// A mutable, transaction-scoped view of a tracked revision. Clones share one change; it is revoked by
/// [`Draft::prepare`] or [`Draft::abort`], after which every method fails.
#[derive(Clone, Debug)]
pub struct Draft(Rc<RefCell<DraftState>>);

#[derive(Debug)]
struct DraftState {
    open: bool,
    base: Arc<Value>,
    base_revision: u64,
    root: Node,
    dirty_counter: u64,
}

#[derive(Debug)]
enum Node {
    Object(ObjectNode),
    Array(ArrayNode),
}

/// Overlay of one base object: the writes, deletions and re-additions upstream records, in first-write order.
#[derive(Debug)]
struct ObjectNode {
    base: Map<String, Value>,
    writes: IndexMap<String, Value>,
    deletes: IndexSet<String>,
    /// Base keys deleted and then written again; replay deletes them first so key order matches.
    readded: IndexSet<String>,
    /// Overlays of unwritten base containers reached by a deeper mutation.
    children: HashMap<String, Node>,
    dirty: Option<u64>,
}

/// Overlay of one base array: each element is a retained base index or an inserted value.
#[derive(Debug)]
struct ArrayNode {
    base: Vec<Value>,
    elements: Vec<Element>,
    /// Replacement values of retained base elements, in first-write order.
    overrides: IndexMap<usize, Value>,
    /// Overlays of base container elements reached by a deeper mutation.
    children: HashMap<usize, Node>,
    structural: bool,
    dirty: Option<u64>,
}

#[derive(Debug)]
enum Element {
    Base(usize),
    Inserted(Value),
}

/// Where a path leads: a tracked base container, or a value placed by this change, which needs no tracking.
enum Place<'a> {
    Node(&'a mut Node),
    Placed(&'a mut Value),
}

fn is_container(value: &Value) -> bool {
    value.is_array() || value.is_object()
}

/// JavaScript `===` for primitives; containers are compared by `equal_json`.
fn same_primitive(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(l), Value::Number(r)) => l.as_f64() == r.as_f64(),
        _ => !is_container(left) && left == right,
    }
}

fn equal_json(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Array(l), Value::Array(r)) => l.len() == r.len() && l.iter().zip(r).all(|(l, r)| equal_json(l, r)),
        (Value::Object(l), Value::Object(r)) => {
            l.len() == r.len()
                && l.iter()
                    .all(|(key, value)| r.get(key).is_some_and(|other| equal_json(value, other)))
        }
        _ => same_primitive(left, right),
    }
}

fn unresolved(path: &[Seg]) -> DeltaError {
    DeltaError::Invalid(format!("Draft path {} does not resolve", crate::path_json(path)))
}

impl Node {
    fn new(value: &Value) -> Option<Node> {
        match value {
            Value::Object(base) => Some(Node::Object(ObjectNode {
                base: base.clone(),
                writes: IndexMap::new(),
                deletes: IndexSet::new(),
                readded: IndexSet::new(),
                children: HashMap::new(),
                dirty: None,
            })),
            Value::Array(base) => Some(Node::Array(ArrayNode {
                elements: (0..base.len()).map(Element::Base).collect(),
                base: base.clone(),
                overrides: IndexMap::new(),
                children: HashMap::new(),
                structural: false,
                dirty: None,
            })),
            _ => None,
        }
    }

    fn dirty(&self) -> Option<u64> {
        match self {
            Node::Object(node) => node.dirty,
            Node::Array(node) => node.dirty,
        }
    }

    fn mark_dirty(&mut self, counter: &mut u64) {
        let dirty = match self {
            Node::Object(node) => &mut node.dirty,
            Node::Array(node) => &mut node.dirty,
        };
        if dirty.is_none() {
            *dirty = Some(*counter);
            *counter += 1;
        }
    }

    /// The current value of this container.
    fn materialize(&self) -> Value {
        match self {
            Node::Object(node) => {
                let mut result = Map::new();
                for (key, value) in &node.base {
                    if node.deletes.contains(key) || node.readded.contains(key) {
                        continue;
                    }
                    let current = match (node.writes.get(key), node.children.get(key)) {
                        (Some(written), _) => written.clone(),
                        (None, Some(child)) => child.materialize(),
                        (None, None) => value.clone(),
                    };
                    result.insert(key.clone(), current);
                }
                for (key, value) in &node.writes {
                    if !result.contains_key(key) {
                        result.insert(key.clone(), value.clone());
                    }
                }
                Value::Object(result)
            }
            Node::Array(node) => Value::Array(
                (0..node.elements.len())
                    .map(|position| node.element_value(position))
                    .collect(),
            ),
        }
    }

    /// One step down a path, creating the overlay of a base container on first use.
    fn child(&mut self, segment: &Seg) -> Option<Place<'_>> {
        match self {
            Node::Object(node) => {
                let key = segment_key(segment);
                if node.deletes.contains(key.as_str()) {
                    return None;
                }
                if node.writes.contains_key(key.as_str()) {
                    return node.writes.get_mut(key.as_str()).map(Place::Placed);
                }
                let base = node.base.get(key.as_str()).filter(|value| is_container(value))?;
                let child = node
                    .children
                    .entry(key)
                    .or_insert_with(|| Node::new(base).expect("container"));
                Some(Place::Node(child))
            }
            Node::Array(node) => {
                let Seg::Index(position) = segment else { return None };
                match node.elements.get_mut(*position)? {
                    Element::Inserted(value) => Some(Place::Placed(value)),
                    Element::Base(index) => {
                        let index = *index;
                        if node.overrides.contains_key(&index) {
                            return node.overrides.get_mut(&index).map(Place::Placed);
                        }
                        let base = node.base.get(index).filter(|value| is_container(value))?;
                        let child = node
                            .children
                            .entry(index)
                            .or_insert_with(|| Node::new(base).expect("container"));
                        Some(Place::Node(child))
                    }
                }
            }
        }
    }

    /// The current value one step down, without creating overlays.
    fn read(&self, segment: &Seg, rest: &[Seg]) -> Option<Value> {
        match self {
            Node::Object(node) => {
                let key = segment_key(segment);
                if node.deletes.contains(key.as_str()) {
                    return None;
                }
                if let Some(written) = node.writes.get(key.as_str()) {
                    return read_value(written, rest);
                }
                if let Some(child) = node.children.get(key.as_str()) {
                    return child.read_path(rest);
                }
                read_value(node.base.get(key.as_str())?, rest)
            }
            Node::Array(node) => {
                let Seg::Index(position) = segment else { return None };
                match node.elements.get(*position)? {
                    Element::Inserted(value) => read_value(value, rest),
                    Element::Base(index) => match (node.overrides.get(index), node.children.get(index)) {
                        (Some(value), _) => read_value(value, rest),
                        (None, Some(child)) => child.read_path(rest),
                        (None, None) => read_value(&node.base[*index], rest),
                    },
                }
            }
        }
    }

    fn read_path(&self, path: &[Seg]) -> Option<Value> {
        match path.split_first() {
            None => Some(self.materialize()),
            Some((segment, rest)) => self.read(segment, rest),
        }
    }
}

fn segment_key(segment: &Seg) -> String {
    match segment {
        Seg::Key(key) => key.clone(),
        Seg::Index(index) => index.to_string(),
    }
}

fn read_value(value: &Value, path: &[Seg]) -> Option<Value> {
    let mut current = value;
    for segment in path {
        current = match (current, segment) {
            (Value::Object(map), segment) => map.get(segment_key(segment).as_str())?,
            (Value::Array(items), Seg::Index(index)) => items.get(*index)?,
            _ => return None,
        };
    }
    Some(current.clone())
}

impl ObjectNode {
    fn set(&mut self, key: String, value: Value) -> bool {
        let was_deleted = self.deletes.contains(key.as_str());
        let current = self.writes.get(key.as_str()).or_else(|| self.base.get(key.as_str()));
        if !was_deleted && !is_container(&value) && current.is_some_and(|current| same_primitive(current, &value)) {
            return false;
        }
        self.children.remove(key.as_str());
        if was_deleted && self.base.contains_key(key.as_str()) {
            self.readded.insert(key.clone());
        }
        self.deletes.shift_remove(key.as_str());
        match self.writes.get_mut(key.as_str()) {
            Some(slot) => *slot = value,
            None => {
                self.writes.insert(key, value);
            }
        }
        true
    }

    fn delete(&mut self, key: String) -> bool {
        let present = !self.deletes.contains(key.as_str())
            && (self.writes.contains_key(key.as_str()) || self.base.contains_key(key.as_str()));
        if !present {
            return false;
        }
        self.writes.shift_remove(key.as_str());
        self.readded.shift_remove(key.as_str());
        self.children.remove(key.as_str());
        self.deletes.insert(key);
        true
    }

    fn has_reserved_mutation(&self) -> bool {
        self.writes
            .keys()
            .chain(self.deletes.iter())
            .any(|key| RESERVED_SEGMENTS.contains(&key.as_str()))
    }

    fn emit(&self, path: &[Seg], ops: &mut Vec<Op>) {
        for key in &self.readded {
            if self.base.contains_key(key.as_str()) {
                ops.push(Op::Delete(join(path, Seg::Key(key.clone()))));
            }
        }
        for (key, value) in &self.writes {
            let before = if self.readded.contains(key.as_str()) {
                None
            } else {
                self.base.get(key.as_str())
            };
            emit_changed(ops, join(path, Seg::Key(key.clone())), before, value);
        }
        for key in &self.deletes {
            if self.base.contains_key(key.as_str()) {
                ops.push(Op::Delete(join(path, Seg::Key(key.clone()))));
            }
        }
    }
}

impl ArrayNode {
    fn element_value(&self, position: usize) -> Value {
        match &self.elements[position] {
            Element::Inserted(value) => value.clone(),
            Element::Base(index) => match (self.overrides.get(index), self.children.get(index)) {
                (Some(value), _) => value.clone(),
                (None, Some(child)) => child.materialize(),
                (None, None) => self.base[*index].clone(),
            },
        }
    }

    /// Replace `remove` elements at `position` with inserted values and return the removed values.
    fn splice(&mut self, position: usize, remove: usize, items: Vec<Value>) -> Vec<Value> {
        let removed = (position..position + remove).map(|at| self.element_value(at)).collect();
        if remove == 0 && items.is_empty() {
            return removed;
        }
        self.elements
            .splice(position..position + remove, items.into_iter().map(Element::Inserted));
        self.structural = true;
        removed
    }

    fn set(&mut self, position: usize, value: Value) -> bool {
        match &mut self.elements[position] {
            Element::Inserted(current) => {
                if !is_container(&value) && same_primitive(current, &value) {
                    return false;
                }
                *current = value;
            }
            Element::Base(index) => {
                let index = *index;
                let current = self.overrides.get(&index).unwrap_or(&self.base[index]);
                if !is_container(&value) && same_primitive(current, &value) {
                    return false;
                }
                self.children.remove(&index);
                if !is_container(&value) && same_primitive(&self.base[index], &value) {
                    self.overrides.shift_remove(&index);
                } else if let Some(slot) = self.overrides.get_mut(&index) {
                    *slot = value;
                } else {
                    self.overrides.insert(index, value);
                }
            }
        }
        true
    }

    fn position_of(&self, index: usize) -> Option<usize> {
        if !self.structural {
            return Some(index);
        }
        self.elements
            .iter()
            .position(|element| matches!(element, Element::Base(base) if *base == index))
    }

    fn emit(&self, path: &[Seg], ops: &mut Vec<Op>) {
        if self.structural {
            let mut retained = vec![false; self.base.len()];
            for element in &self.elements {
                if let Element::Base(index) = element {
                    retained[*index] = true;
                }
            }
            let mut end = self.base.len();
            while end > 0 {
                if retained[end - 1] {
                    end -= 1;
                    continue;
                }
                let mut start = end - 1;
                while start > 0 && !retained[start - 1] {
                    start -= 1;
                }
                ops.push(Op::Splice(path.to_vec(), start, end - start, Vec::new()));
                end = start;
            }
            let mut position = 0;
            while position < self.elements.len() {
                if matches!(self.elements[position], Element::Base(_)) {
                    position += 1;
                    continue;
                }
                let start = position;
                let mut items = Vec::new();
                while let Some(Element::Inserted(value)) = self.elements.get(position) {
                    items.push(value.clone());
                    position += 1;
                }
                ops.push(Op::Splice(path.to_vec(), start, 0, items));
            }
        }
        for (index, value) in &self.overrides {
            if let Some(position) = self.position_of(*index) {
                emit_changed(ops, join(path, Seg::Index(position)), Some(&self.base[*index]), value);
            }
        }
    }
}

fn join(path: &[Seg], segment: Seg) -> Path {
    let mut joined = path.to_vec();
    joined.push(segment);
    joined
}

/// Upstream `emitChangedValue`: nothing for an equal value, append/trim ops for a string that shares an edge, else a
/// set.
fn emit_changed(ops: &mut Vec<Op>, path: Path, before: Option<&Value>, after: &Value) {
    if let Some(before) = before
        && equal_json(before, after)
    {
        return;
    }
    if let (Some(Value::String(before)), Value::String(after)) = (before, after) {
        let (before_units, after_units) = (before.encode_utf16().count(), after.encode_utf16().count());
        if after_units > before_units && after.starts_with(before.as_str()) {
            ops.push(Op::Append(path, after[before.len()..].to_owned()));
            return;
        }
        let shared = overlap(before, after, OVERLAP_SCAN);
        if shared > 0
            && let Some(rest) = utf16_suffix(after, shared)
        {
            ops.push(Op::Trim(path.clone(), before_units - shared));
            if after_units > shared {
                ops.push(Op::Append(path, rest.to_owned()));
            }
            return;
        }
    }
    ops.push(Op::Set(path, after.clone()));
}

/// The part of `text` after its first `units` UTF-16 code units; `None` when that splits a character.
fn utf16_suffix(text: &str, units: usize) -> Option<&str> {
    let mut seen = 0;
    for (offset, character) in text.char_indices() {
        if seen == units {
            return Some(&text[offset..]);
        }
        seen += character.len_utf16();
    }
    (seen == units).then_some("")
}

/// Longest suffix of `a` that is a prefix of `b`, in UTF-16 code units, with upstream's bounded probing: a 64-unit
/// head first, then one unit, at most eight candidates each. Giving up returns 0.
pub fn overlap(a: &str, b: &str, scan: usize) -> usize {
    let (a, b): (Vec<u16>, Vec<u16>) = (a.encode_utf16().collect(), b.encode_utf16().collect());
    if a.is_empty() || b.is_empty() || scan == 0 {
        return 0;
    }
    let tail = &a[a.len().saturating_sub(scan)..];
    for head_len in [64.min(b.len()), 1] {
        let head = &b[..head_len];
        let mut tried = 0;
        let mut from = 0;
        while let Some(found) = find(tail, head, from) {
            tried += 1;
            if tried > 8 {
                break;
            }
            let n = tail.len() - found;
            if n <= b.len() && tail[found..] == b[..n] {
                return n;
            }
            from = found + 1;
        }
        if head_len == 1 {
            break;
        }
    }
    0
}

fn find(haystack: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if from > haystack.len() || needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&at| haystack[at..at + needle.len()] == *needle)
}

/// One dirty overlay to emit, with its current path.
struct Emission<'a> {
    node: &'a Node,
    path: Path,
    dirty: u64,
    fold: bool,
}

fn collect<'a>(node: &'a Node, path: Path, out: &mut Vec<Emission<'a>>) {
    if let Some(dirty) = node.dirty() {
        out.push(Emission {
            node,
            path: path.clone(),
            dirty,
            fold: false,
        });
    }
    match node {
        Node::Object(object) => {
            for (key, child) in &object.children {
                collect(child, join(&path, Seg::Key(key.clone())), out);
            }
        }
        Node::Array(array) => {
            for (index, child) in &array.children {
                if array.overrides.contains_key(index) {
                    continue;
                }
                if let Some(position) = array.position_of(*index) {
                    collect(child, join(&path, Seg::Index(position)), out);
                }
            }
        }
    }
}

fn node_at<'a>(root: &'a Node, path: &[Seg]) -> &'a Node {
    let mut node = root;
    for segment in path {
        node = match (node, segment) {
            (Node::Object(object), segment) => &object.children[segment_key(segment).as_str()],
            (Node::Array(array), Seg::Index(position)) => match array.elements[*position] {
                Element::Base(index) => &array.children[&index],
                Element::Inserted(_) => unreachable!("inserted values have no overlay"),
            },
            _ => unreachable!("collected paths follow overlays"),
        };
    }
    node
}

fn is_reserved(segment: &Seg) -> bool {
    matches!(segment, Seg::Key(key) if RESERVED_SEGMENTS.contains(&key.as_str()))
}

/// Upstream `emitOperations`: dirty overlays shallowest first, in first-dirty order; a reserved key folds the
/// containing overlay into one set; an oversized batch becomes one root replacement.
fn emit_operations(root: &Node) -> Vec<Op> {
    let mut emissions = Vec::new();
    collect(root, Vec::new(), &mut emissions);
    emissions.sort_by_key(|emission| emission.dirty);
    let mut folds: Vec<Path> = Vec::new();
    for emission in &mut emissions {
        let reserved_mutation = matches!(emission.node, Node::Object(object) if object.has_reserved_mutation());
        if reserved_mutation {
            emission.fold = true;
            if !folds.contains(&emission.path) {
                folds.push(emission.path.clone());
            }
        }
        if let Some(at) = emission.path.iter().position(is_reserved) {
            let ancestor = emission.path[..at].to_vec();
            if !folds.contains(&ancestor) {
                folds.push(ancestor);
            }
        }
    }
    for fold in &folds {
        match emissions.iter_mut().find(|emission| emission.path == *fold) {
            Some(emission) => emission.fold = true,
            None => emissions.push(Emission {
                node: node_at(root, fold),
                path: fold.clone(),
                dirty: u64::MAX,
                fold: true,
            }),
        }
    }
    emissions.sort_by_key(|emission| emission.path.len());
    let mut ops = Vec::new();
    let mut folded: Vec<&Path> = Vec::new();
    for emission in &emissions {
        let path = &emission.path;
        if folded
            .iter()
            .any(|fold| fold.len() < path.len() && path.starts_with(fold))
        {
            continue;
        }
        if emission.fold {
            let value = emission.node.materialize();
            ops.push(if path.is_empty() {
                Op::Replace(value)
            } else {
                Op::Set(path.clone(), value)
            });
            folded.push(path);
            continue;
        }
        match emission.node {
            Node::Object(object) => object.emit(path, &mut ops),
            Node::Array(array) => array.emit(path, &mut ops),
        }
        if ops.len() > MAX_DELTA_OPERATIONS {
            return vec![Op::Replace(root.materialize())];
        }
    }
    ops
}

impl Draft {
    pub fn ptr_eq(&self, other: &Draft) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    /// The current value at `path`; `None` when it does not resolve.
    pub fn get(&self, path: &[Seg]) -> Result<Option<Value>> {
        let state = self.0.borrow();
        if !state.open {
            return Err(DeltaError::Invalid(SETTLED.into()));
        }
        Ok(state.root.read_path(path))
    }

    /// Run `edit` on the container at `path`: a tracked overlay or a value this change placed.
    fn edit<R>(&self, path: &[Seg], edit: impl FnOnce(Place<'_>, &mut u64) -> Result<R>) -> Result<R> {
        let mut state = self.0.borrow_mut();
        if !state.open {
            return Err(DeltaError::Invalid(SETTLED.into()));
        }
        let DraftState {
            root, dirty_counter, ..
        } = &mut *state;
        let mut place = Place::Node(root);
        for (depth, segment) in path.iter().enumerate() {
            place = match place {
                Place::Node(node) => node.child(segment),
                Place::Placed(value) => placed_child(value, segment),
            }
            .ok_or_else(|| unresolved(&path[..=depth]))?;
        }
        edit(place, dirty_counter)
    }

    /// Assign an object key or an array index (an index equal to the length appends).
    pub fn set(&self, path: &[Seg], value: Value) -> Result<()> {
        let Some((last, parent)) = path.split_last() else {
            return Err(DeltaError::Invalid("A draft root cannot be assigned".into()));
        };
        self.edit(parent, |place, counter| match place {
            Place::Node(node) => {
                let changed = match node {
                    Node::Object(object) => object.set(segment_key(last), value),
                    Node::Array(array) => {
                        let Seg::Index(position) = last else {
                            return Err(DeltaError::Invalid(
                                "Only array indices and length can be written".into(),
                            ));
                        };
                        let length = array.elements.len();
                        if *position > length {
                            return Err(DeltaError::Invalid("Overlay arrays cannot contain holes".into()));
                        }
                        if *position == length {
                            array.splice(length, 0, vec![value]);
                            true
                        } else {
                            array.set(*position, value)
                        }
                    }
                };
                if changed {
                    node.mark_dirty(counter);
                }
                Ok(())
            }
            Place::Placed(target) => match (target, last) {
                (Value::Object(map), segment) => {
                    map.insert(segment_key(segment), value);
                    Ok(())
                }
                (Value::Array(items), Seg::Index(position)) if *position <= items.len() => {
                    if *position == items.len() {
                        items.push(value);
                    } else {
                        items[*position] = value;
                    }
                    Ok(())
                }
                _ => Err(unresolved(path)),
            },
        })
    }

    /// Delete an object key; deleting an absent key does nothing.
    pub fn delete(&self, path: &[Seg]) -> Result<()> {
        let Some((last, parent)) = path.split_last() else {
            return Err(DeltaError::Invalid("A draft root cannot be deleted".into()));
        };
        self.edit(parent, |place, counter| match place {
            Place::Node(Node::Array(_)) | Place::Placed(Value::Array(_)) => {
                Err(DeltaError::Invalid("Overlay arrays cannot contain holes".into()))
            }
            Place::Node(node) => {
                if let Node::Object(object) = node
                    && object.delete(segment_key(last))
                {
                    node.mark_dirty(counter);
                }
                Ok(())
            }
            Place::Placed(Value::Object(map)) => {
                map.shift_remove(segment_key(last).as_str());
                Ok(())
            }
            Place::Placed(_) => Err(unresolved(path)),
        })
    }

    /// Array `splice(start, remove, ...items)` with `start` and `remove` clamped to the array; returns the removed
    /// values.
    pub fn splice(&self, path: &[Seg], start: usize, remove: usize, items: Vec<Value>) -> Result<Vec<Value>> {
        self.edit(path, |place, counter| match place {
            Place::Node(node) => {
                let Node::Array(array) = node else {
                    return Err(DeltaError::Invalid(
                        "Array mutator called on incompatible receiver".into(),
                    ));
                };
                let length = array.elements.len();
                let start = start.min(length);
                let remove = remove.min(length - start);
                let changed = remove > 0 || !items.is_empty();
                let removed = array.splice(start, remove, items);
                if changed {
                    node.mark_dirty(counter);
                }
                Ok(removed)
            }
            Place::Placed(Value::Array(values)) => {
                let start = start.min(values.len());
                let remove = remove.min(values.len() - start);
                Ok(values.splice(start..start + remove, items).collect())
            }
            Place::Placed(_) => Err(DeltaError::Invalid(
                "Array mutator called on incompatible receiver".into(),
            )),
        })
    }

    fn len(&self, path: &[Seg]) -> Result<usize> {
        match self.get(path)? {
            Some(Value::Array(items)) => Ok(items.len()),
            _ => Err(DeltaError::Invalid(
                "Array mutator called on incompatible receiver".into(),
            )),
        }
    }

    pub fn push(&self, path: &[Seg], items: Vec<Value>) -> Result<usize> {
        let length = self.len(path)?;
        let added = items.len();
        self.splice(path, length, 0, items)?;
        Ok(length + added)
    }

    pub fn pop(&self, path: &[Seg]) -> Result<Option<Value>> {
        let length = self.len(path)?;
        if length == 0 {
            return Ok(None);
        }
        Ok(self.splice(path, length - 1, 1, Vec::new())?.pop())
    }

    pub fn shift(&self, path: &[Seg]) -> Result<Option<Value>> {
        Ok(self.splice(path, 0, 1, Vec::new())?.pop())
    }

    pub fn unshift(&self, path: &[Seg], items: Vec<Value>) -> Result<usize> {
        self.splice(path, 0, 0, items)?;
        self.len(path)
    }

    /// Array `length = len`: truncates, or pads with `null`.
    pub fn set_len(&self, path: &[Seg], len: usize) -> Result<()> {
        let length = self.len(path)?;
        if len < length {
            self.splice(path, len, length - len, Vec::new())?;
        } else if len > length {
            self.splice(path, length, 0, vec![Value::Null; len - length])?;
        }
        Ok(())
    }

    /// Revoke the draft and return the prepared change: the exact ops and the resulting revision.
    pub fn prepare(&self) -> Result<Prepared> {
        let mut state = self.0.borrow_mut();
        if !state.open {
            return Err(DeltaError::Invalid("Change has already been settled".into()));
        }
        state.open = false;
        let ops = emit_operations(&state.root);
        let value = if ops.is_empty() {
            state.base.clone()
        } else {
            Arc::new(apply_immutable(&state.base, &ops)?)
        };
        Ok(Prepared {
            base: state.base.clone(),
            value,
            ops,
            base_revision: state.base_revision,
        })
    }

    /// Revoke the draft without preparing it; idempotent.
    pub fn abort(&self) {
        self.0.borrow_mut().open = false;
    }

    pub fn is_open(&self) -> bool {
        self.0.borrow().open
    }
}

fn placed_child<'a>(value: &'a mut Value, segment: &Seg) -> Option<Place<'a>> {
    match (value, segment) {
        (Value::Object(map), segment) => map.get_mut(segment_key(segment).as_str()),
        (Value::Array(items), Seg::Index(index)) => items.get_mut(*index),
        _ => None,
    }
    .filter(|value| is_container(value))
    .map(Place::Placed)
}
