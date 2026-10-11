//! Replays mutation scripts recorded through the pinned upstream Chord tracker (see fixtures/tracker.mjs): the Rust
//! [`Draft`] must emit the same ops and reach the same value for every batch.

use chord_delta::{Draft, Op, Seg, Tracker, overlap, path};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/tracker.json")).expect("fixture parses")
}

fn segments(value: &Value) -> Vec<Seg> {
    value
        .as_array()
        .expect("path array")
        .iter()
        .map(|segment| match segment {
            Value::String(key) => Seg::Key(key.clone()),
            Value::Number(index) => Seg::Index(index.as_u64().expect("index") as usize),
            other => panic!("bad segment {other}"),
        })
        .collect()
}

fn key(step: &Value) -> Seg {
    match &step["key"] {
        Value::String(key) => Seg::Key(key.clone()),
        Value::Number(index) => Seg::Index(index.as_u64().expect("index") as usize),
        other => panic!("bad key {other}"),
    }
}

fn items(step: &Value) -> Vec<Value> {
    step["items"].as_array().expect("items").clone()
}

fn run(draft: &Draft, step: &Value) {
    let parent = segments(&step["path"]);
    let child = || {
        let mut path = parent.clone();
        path.push(key(step));
        path
    };
    let result = match step["m"].as_str().expect("step name") {
        "set" => draft.set(&child(), step["value"].clone()),
        "delete" => draft.delete(&child()),
        "push" => draft.push(&parent, items(step)).map(drop),
        "pop" => draft.pop(&parent).map(drop),
        "shift" => draft.shift(&parent).map(drop),
        "unshift" => draft.unshift(&parent, items(step)).map(drop),
        "splice" => {
            let at = |name: &str| step[name].as_u64().expect("count") as usize;
            draft.splice(&parent, at("start"), at("remove"), items(step)).map(drop)
        }
        "length" => draft.set_len(&parent, step["len"].as_u64().expect("len") as usize),
        other => panic!("unknown step {other}"),
    };
    result.unwrap_or_else(|error| panic!("{step}: {error}"));
}

#[test]
fn chord_delta_tracker_emits_upstream_ops_for_recorded_mutations() {
    let fixture = fixture();
    let cases = fixture["cases"].as_array().expect("cases");
    assert_eq!(cases.len(), 76);
    for case in cases {
        let name = case["name"].as_str().expect("name");
        let mut tracker = Tracker::new(case["base"].clone());
        for (index, batch) in case["batches"].as_array().expect("batches").iter().enumerate() {
            let draft = tracker.begin_change().expect("container root");
            for step in batch["steps"].as_array().expect("steps") {
                run(&draft, step);
            }
            let prepared = draft.prepare().expect("prepare");
            let ops: Vec<Value> = prepared.ops.iter().map(Op::to_json).collect();
            assert_eq!(Value::Array(ops), batch["ops"], "{name} batch {index}: ops");
            assert_eq!(*prepared.value, batch["value"], "{name} batch {index}: value");
            tracker.adopt(&prepared).expect("adopt");
        }
    }
}

#[test]
fn chord_delta_tracker_revokes_drafts_and_rejects_stale_adoption() {
    let mut tracker = Tracker::new(json!({ "count": 0, "items": [] }));
    let draft = tracker.begin_change().unwrap();
    let alias = draft.clone();
    assert!(alias.ptr_eq(&draft));
    draft.set(&path!["count"], json!(1)).unwrap();
    assert_eq!(alias.get(&path!["count"]).unwrap(), Some(json!(1)));
    let prepared = draft.prepare().unwrap();
    assert_eq!(
        alias.get(&path!["count"]).unwrap_err().to_string(),
        "Cannot use a settled overlay"
    );
    assert_eq!(
        alias.set(&path!["count"], json!(2)).unwrap_err().to_string(),
        "Cannot use a settled overlay"
    );
    assert_eq!(
        draft.prepare().unwrap_err().to_string(),
        "Change has already been settled"
    );

    let competing = tracker.begin_change().unwrap();
    competing.push(&path!["items"], vec![json!("x")]).unwrap();
    let competing = competing.prepare().unwrap();
    tracker.adopt(&prepared).unwrap();
    assert_eq!(**tracker.value(), json!({ "count": 1, "items": [] }));
    assert_eq!(
        tracker.adopt(&competing).unwrap_err().to_string(),
        "Prepared change is stale"
    );
    assert_eq!(tracker.revision(), 1);

    // An empty batch keeps the base revision itself.
    let empty = tracker.begin_change().unwrap().prepare().unwrap();
    assert!(empty.ops.is_empty());
    assert!(std::sync::Arc::ptr_eq(&empty.value, tracker.value()));
}

#[test]
fn chord_delta_tracker_rejects_holes_and_unresolved_paths() {
    let tracker = Tracker::new(json!({ "l": [1], "n": 1 }));
    let draft = tracker.begin_change().unwrap();
    assert_eq!(
        draft.set(&path!["l", 2], json!(0)).unwrap_err().to_string(),
        "Overlay arrays cannot contain holes"
    );
    assert_eq!(
        draft.delete(&path!["l", 0]).unwrap_err().to_string(),
        "Overlay arrays cannot contain holes"
    );
    assert_eq!(
        draft.set(&path!["n", "x"], json!(0)).unwrap_err().to_string(),
        "Draft path [\"n\"] does not resolve"
    );
    assert_eq!(draft.get(&path!["missing", "x"]).unwrap(), None);
}

#[test]
fn chord_delta_overlap_matches_upstream_examples() {
    // Literal expectations from upstream `overlap()` semantics: longest suffix of `a` that prefixes `b`, in UTF-16.
    assert_eq!(overlap("abcdef", "defxyz", 65_536), 3);
    assert_eq!(overlap("a😀b", "😀bc", 65_536), 3);
    assert_eq!(overlap("same", "other", 65_536), 0);
    assert_eq!(overlap("", "x", 65_536), 0);
    // More than eight one-unit candidates: upstream gives up and a set is emitted instead.
    assert_eq!(overlap("aaaaaaaaaaaab", "bz", 65_536), 1);
    assert_eq!(overlap("xaxaxaxaxaxaxaxaxaxa", "ay", 65_536), 0);
}
