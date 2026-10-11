//! Replays crates/review/fixtures/ledger.json, recorded from autoreview/lessons.ts (fixtures/generate.mjs).

use review::ledger::{Fate, FateInput, PostedFinding, ThreadState, fates_of, lessons_for, merge_fates, parse_iso_ms};
use serde_json::Value;

fn fixture() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/ledger.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("fixture json")
}

fn from<T: serde::de::DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).expect("fixture shape")
}

#[test]
fn review_ledger_fates_of_matches_typescript() {
    for case in fixture()["fates"].as_array().expect("fates") {
        let input = &case["input"];
        let pairs = |key: &str| input[key].as_array().cloned().unwrap_or_default();
        let fate_input = FateInput {
            pull: from(&input["pull"]),
            now: from(&input["now"]),
            findings: from::<Vec<PostedFinding>>(&input["findings"]),
            statuses: pairs("statuses")
                .iter()
                .map(|pair| (from(&pair[0]), from(&pair[1])))
                .collect(),
            threads: pairs("threads")
                .iter()
                .map(|pair| (from(&pair[0]), from::<ThreadState>(&pair[1])))
                .collect(),
            resolved_by_us: pairs("resolvedByUs").iter().map(from).collect(),
            merged: input["merged"].as_bool().unwrap_or(false),
        };
        assert_eq!(
            serde_json::to_value(fates_of(&fate_input)).expect("json"),
            case["fates"],
            "{}",
            input["pull"]
        );
    }
}

#[test]
fn review_ledger_lessons_and_merge_match_typescript() {
    let fx = fixture();
    for case in fx["lessons"].as_array().expect("lessons") {
        let ledger: Vec<Fate> = from(&case["ledger"]);
        let lessons = lessons_for(&ledger, case["now"].as_str().expect("now"));
        assert_eq!(
            serde_json::to_value(lessons).expect("json"),
            case["lessons"],
            "{}",
            case["now"]
        );
    }
    for case in fx["merges"].as_array().expect("merges") {
        let merged = merge_fates(&from::<Vec<Fate>>(&case["ledger"]), &from::<Vec<Fate>>(&case["fates"]));
        assert_eq!(serde_json::to_value(merged).expect("json"), case["merged"]);
    }
}

#[test]
fn review_ledger_parses_iso_times_like_date_parse() {
    // Expected values are JS `Date.parse` results (node 26, TZ=UTC).
    assert_eq!(parse_iso_ms("2026-10-10T00:00:00.000Z"), Some(1_791_590_400_000));
    assert_eq!(parse_iso_ms("2026-09-20"), Some(1_789_862_400_000));
    assert_eq!(parse_iso_ms("2026-09-10T00:00:00+02:00"), Some(1_788_991_200_000));
    assert_eq!(parse_iso_ms("2026-08-01T12:30:45.5Z"), Some(1_785_587_445_500));
    assert_eq!(parse_iso_ms("1969-12-31T23:59:59Z"), Some(-1000));
    assert_eq!(parse_iso_ms("not a date"), None);
    assert_eq!(parse_iso_ms("2026-13-01"), None);
}
