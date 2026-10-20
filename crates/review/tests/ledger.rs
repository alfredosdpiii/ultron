//! Replays crates/review/fixtures/ledger.json, recorded from autoreview/lessons.ts (fixtures/generate.mjs).

use review::ledger::{
    Fate, FateInput, FateKind, MERGED_OPEN, PostedFinding, ThreadState, fates_of, lessons_for, merge_fates, parse_iso_ms,
    without_unposted_merges,
};
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

/// fisher-ki-saas#2945: 199 findings listed only in the review body were recorded "merged with the finding open"
/// and lowered whole categories. Those fates leave the ledger; a fix of an unposted finding and every fate of a
/// posted one stay.
#[test]
fn review_ledger_forgets_merge_rejections_of_findings_nobody_saw() {
    let fate = |id: &str, kind: FateKind, how: &str| Fate {
        id: id.into(),
        pull: "github.com/o/r#1".into(),
        file: "f.ts".into(),
        line: 1,
        category: "correctness".into(),
        kind: "fast".into(),
        level: "low".into(),
        claim: "c".into(),
        fate: kind,
        how: how.into(),
        at: "2026-10-01T00:00:00.000Z".into(),
    };
    let ledger = vec![
        fate("a-1", FateKind::Rejected, MERGED_OPEN),
        fate("a-2", FateKind::Rejected, MERGED_OPEN),
        fate("a-3", FateKind::Accepted, "fixed in a later commit"),
    ];
    let unposted = [("github.com/o/r#1".to_string(), "a-1".to_string()), ("github.com/o/r#1".into(), "a-3".into())]
        .into_iter()
        .collect();
    let kept: Vec<String> = without_unposted_merges(&ledger, &unposted).into_iter().map(|f| f.id).collect();
    assert_eq!(kept, ["a-2", "a-3"]);
}
