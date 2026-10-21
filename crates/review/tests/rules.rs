//! The self-improving loop of the reviewer (`reviewer::classify_notes`, `learn`'s credit): a person's addressed
//! comment where this reviewer raised nothing is a miss; a finding that cites a rule credits it with its fate, once.
//! Against the fake hub and a scripted engine. No network, no model calls.

mod github_common;

use std::path::Path;
use std::sync::Arc;

use github_common::*;
use indexmap::IndexMap;
use review::accounts::Account;
use review::github::{GitHub, PullRef};
use review::reviewer::{MISS_WINDOW, ReviewOptions, classify_notes, finding_rules, review_pull};
use review::state::{AutoreviewState, PullState};
use serde_json::{Value, json};

fn note(path: &str, line: Option<i64>, author: &str, state: &str, human: bool) -> knowledge::Note {
    knowledge::Note {
        pull: 1,
        path: path.into(),
        line,
        author: author.into(),
        body: "b".into(),
        url: format!("u/{path}/{line:?}/{author}"),
        at: "2026-10-01T00:00:00Z".into(),
        state: state.into(),
        human,
        missed: false,
    }
}

fn state_with(pulls: Vec<(&str, Value)>) -> AutoreviewState {
    AutoreviewState {
        version: 1,
        accounts: IndexMap::new(),
        pulls: pulls
            .into_iter()
            .map(|(key, pull)| {
                (
                    key.to_string(),
                    serde_json::from_value::<PullState>(pull).expect("pull"),
                )
            })
            .collect(),
        recent: Vec::new(),
        queue: None,
        lessons: None,
        daemon: None,
    }
}

fn finding(file: &str, line: i64) -> Value {
    json!({ "id": "x", "file": file, "line": line, "severity": "high", "claim": "c", "claimHash": "h", "sha": "s",
        "status": "open" })
}

#[test]
fn rules_a_persons_addressed_comment_where_we_raised_nothing_is_a_miss() {
    let r = PullRef::new("github.com", "o", "r", 1);
    let state = state_with(vec![(
        "ultron-bot@github.com/o/r#1",
        json!({ "lastVerdict": "approve", "findings": [finding("calc.py", 40)] }),
    )]);
    let mut notes = vec![
        // Far from our finding: a miss.
        note("calc.py", Some(4), "bob", "addressed", true),
        // Within the window of our finding: we raised it.
        note("calc.py", Some(40 + MISS_WINDOW), "bob", "addressed", true),
        // On a file we raised nothing on, without a line: a miss.
        note("db.sql", None, "carol", "addressed", true),
        // Open: not a fix anyone made. Outdated: the lines changed, which may have ignored it.
        note("db.sql", Some(1), "carol", "open", true),
        note("db.sql", Some(9), "carol", "outdated", true),
        // A bot, and our own account: not a person's.
        note("db.sql", Some(2), "coderabbitai", "addressed", false),
        note("db.sql", Some(3), "Ultron-Bot", "addressed", true),
    ];
    classify_notes(&mut notes, &state, &r);
    let got: Vec<(bool, bool)> = notes.iter().map(|n| (n.human, n.missed)).collect();
    assert_eq!(
        got,
        [
            (true, true),
            (true, false),
            (true, true),
            (true, false),
            (true, false),
            (false, false),
            (false, false)
        ]
    );
    // A pull request this reviewer never reviewed has rules, but no misses.
    let mut unseen = vec![note("calc.py", Some(4), "bob", "addressed", true)];
    classify_notes(&mut unseen, &state_with(Vec::new()), &r);
    assert_eq!((unseen[0].human, unseen[0].missed), (true, false));
}

#[test]
fn rules_a_rule_a_finding_cited_is_credited_once_with_the_findings_fate() {
    let first = engine_result(json!({ "findings": [
        major_with(json!({ "kind": "deep:claims", "rules": [1] })),
        // Not ids: nothing to credit.
        major_with(json!({ "line": 5, "claim": "price may be missing", "replacement": null, "kind": "fast", "rules": ["R1"] })),
    ] }));
    let second = engine_result(json!({ "earlier": [
        { "id": "ccccccc-1", "file": "calc.py", "line": 4, "claim": "total() skips the last item.", "severity": "major",
          "status": "fixed", "evidence": "" },
        { "id": "ccccccc-2", "file": "calc.py", "line": 5, "claim": "price may be missing", "severity": "major",
          "status": "still_present", "evidence": "" },
    ] }));
    let context = setup(
        json!({}),
        Some(FakeEngine::new(vec![
            Ok(first),
            Ok(second),
            Ok(engine_result(json!({}))),
        ])),
    );
    let mut pull = FakePull::new("o", "r", 1).requested(&[BOT]);
    pull.head_sha = old();
    context.hub.add_pull(pull);
    review_pull(
        &context.deps,
        &context.candidate(&["review_requested"]),
        ReviewOptions::default(),
    )
    .expect("review");
    let saved = context.deps.store.read().pulls["ultron-bot@github.com/o/r#1"].clone();
    let cited: Vec<(String, Vec<i64>)> = saved
        .findings
        .iter()
        .map(|f| (f.id.clone(), finding_rules(f)))
        .collect();
    assert_eq!(
        cited,
        [("ccccccc-1".to_string(), vec![1]), ("ccccccc-2".to_string(), vec![])]
    );

    // The rule: a person's addressed comment, the first note of a fresh store.
    let db = context.deps.paths.knowledge.clone();
    std::fs::create_dir_all(Path::new(&db).parent().expect("dir")).expect("knowledge dir");
    let mut store = knowledge::Store::open(Path::new(&db)).expect("store");
    store
        .record_notes("github.com/o/r", &[note("calc.py", Some(4), "bob", "addressed", true)])
        .expect("note");
    assert_eq!(
        store.note_id("github.com/o/r", "u/calc.py/Some(4)/bob").expect("id"),
        Some(1)
    );

    // New commits fix the first finding: accepted, and its rule with it.
    context.hub.with_pull(|pull| {
        pull.head_sha = new();
        pull.ancestors = vec![old()];
        pull.diffs
            .insert(format!("{}..{}", old(), new()), DIFF.replace("- 1)", "- 2)"));
    });
    context.hub.set_now("2026-10-04T11:00:00Z");
    review_pull(
        &context.deps,
        &context.candidate(&["requested_changes"]),
        ReviewOptions::default(),
    )
    .expect("review");
    assert_eq!(store.rule_credit(1).expect("credit"), Some((1, 0, false, true)));
    assert!(context.logs().contains("credited 1 rules"), "{}", context.logs());

    // It merges with the second finding open: that fate is new, the first one's is not credited again.
    context.hub.with_pull(|pull| {
        pull.state = "closed".into();
        pull.merged = true;
    });
    context.hub.set_now("2026-10-04T12:00:00Z");
    review_pull(
        &context.deps,
        &context.candidate(&["requested_changes"]),
        ReviewOptions::default(),
    )
    .expect("close");
    assert_eq!(store.rule_credit(1).expect("credit"), Some((1, 0, false, true)));
}

/// Only a resolved thread is addressed: one a later commit only made outdated may have been ignored, and a rule
/// learned from it would teach what nobody asked for.
#[test]
fn rules_a_thread_is_addressed_when_resolved_not_when_only_outdated() {
    let context = setup(json!({}), None);
    let mut pull = FakePull::new("o", "r", 1);
    let thread = |resolved: bool, outdated: bool, kind: &str, login: &str| {
        json!({ "isResolved": resolved, "isOutdated": outdated, "path": "calc.py", "line": null, "originalLine": 4,
            "comments": { "nodes": [{ "author": { "__typename": kind, "login": login }, "body": "b",
                "url": format!("u/{resolved}/{outdated}/{login}"), "createdAt": "2026-10-01T00:00:00Z" }] } })
    };
    pull.notes = vec![
        thread(true, true, "User", "bob"),
        thread(false, true, "User", "bob"),
        thread(false, false, "User", "bob"),
        thread(true, false, "Bot", "coderabbitai"),
    ];
    context.hub.add_pull(pull);
    let token = context.token.clone();
    let github = GitHub::new(
        context.hub.clone(),
        Account::new(BOT, "github.com"),
        Arc::new(move || {
            let mut env = review::runner::Env::new();
            env.insert("GH_TOKEN".into(), token.clone());
            env.insert("GH_HOST".into(), "github.com".into());
            Ok(env)
        }),
        None,
    );
    let notes = github
        .review_notes(&PullRef::new("github.com", "o", "r", 1))
        .expect("notes");
    let got: Vec<(&str, bool, Option<i64>)> = notes.iter().map(|n| (n.state.as_str(), n.human, n.line)).collect();
    assert_eq!(
        got,
        [
            ("addressed", true, Some(4)),
            ("outdated", true, Some(4)),
            ("open", true, Some(4)),
            ("addressed", false, Some(4))
        ]
    );
}
