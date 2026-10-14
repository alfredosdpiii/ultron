//! Automatic compaction's arithmetic and classification (session_core::auto_compaction) against the TypeScript
//! recorded in fixtures/auto_compaction.json (fixtures/generate-auto-compaction.mjs): pi-ai `isContextOverflow` over
//! each provider's overflow text and the errors that only look like one, silent and length-stop overflows,
//! `isRecoverableLength`, `shouldCompact` around the threshold, and `getCompactionSettings(model)`. Plus the
//! checkpoint rule of `prepareCompactionThreshold` (no second compaction at or after the checkpoint's trigger),
//! ported with literal expectations.

use ai::AssistantMessage;
use compaction::CompactionSettings;
use serde_json::{Value, json};
use session_core::auto_compaction::{self, Reason};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/auto_compaction.json")).unwrap()
}

fn message(value: &Value) -> AssistantMessage {
    serde_json::from_value(value.clone()).unwrap()
}

#[test]
fn session_core_auto_compaction_overflow_matches_typescript_per_provider() {
    let cases = fixture()["overflow"].as_array().unwrap().clone();
    assert!(cases.len() > 40);
    for case in cases {
        let m = message(&case["message"]);
        let window = case["contextWindow"].as_u64().unwrap();
        assert_eq!(
            auto_compaction::is_context_overflow(&m, window),
            case["overflow"].as_bool().unwrap(),
            "{} {:?} {:?} {:?}",
            m.provider,
            m.stop_reason,
            m.error_message,
            m.usage
        );
    }
}

#[test]
fn session_core_auto_compaction_recoverable_length_matches_typescript() {
    for case in fixture()["recoverable"].as_array().unwrap() {
        let m = message(&case["message"]);
        let desired = case["desiredMaxOutput"].as_u64().unwrap();
        assert_eq!(
            auto_compaction::is_recoverable_length(&m, desired),
            case["recoverable"].as_bool().unwrap(),
            "{case}"
        );
        // `publishResponse`: a recoverable length stop is an overflow too.
        if case["recoverable"] == true {
            assert!(auto_compaction::overflowed(&m, 0, desired), "{case}");
        }
    }
}

#[test]
fn session_core_auto_compaction_threshold_matches_typescript() {
    for case in fixture()["threshold"].as_array().unwrap() {
        let s = &case["settings"];
        let settings = CompactionSettings {
            enabled: s["enabled"].as_bool().unwrap(),
            reserve_tokens: s["reserveTokens"].as_u64().unwrap(),
            keep_recent_tokens: s["keepRecentTokens"].as_u64().unwrap(),
        };
        let (tokens, window) = (
            case["tokens"].as_u64().unwrap(),
            case["contextWindow"].as_u64().unwrap(),
        );
        assert_eq!(
            compaction::should_compact(tokens, window, &settings),
            case["compact"].as_bool().unwrap(),
            "{case}"
        );
        // At a checkpoint with no compaction on the branch, the threshold alone decides (a window of 0 is unknown).
        let path = [json!({"id": "a", "type": "message"})];
        assert_eq!(
            auto_compaction::threshold_due(&settings, &path, Some("a"), tokens, window),
            case["compact"] == true && window > 0,
            "{case}"
        );
    }
}

#[test]
fn session_core_auto_compaction_settings_match_typescript() {
    for case in fixture()["settings"].as_array().unwrap() {
        let model = &case["model"];
        let got = auto_compaction::settings(
            &case["configured"],
            model["provider"].as_str().unwrap(),
            model["id"].as_str().unwrap(),
            None,
        );
        match (&got, case.get("error")) {
            (Err(error), Some(want)) => assert_eq!(error, want.as_str().unwrap(), "{case}"),
            (Ok(settings), None) => {
                let want = &case["settings"];
                assert_eq!(
                    json!({"enabled": settings.enabled, "reserveTokens": settings.reserve_tokens,
                        "keepRecentTokens": settings.keep_recent_tokens}),
                    *want,
                    "{case}"
                );
            }
            _ => panic!("{case}: {got:?}"),
        }
    }
}

/// `set_auto_compaction` decides whether automatic compaction is on, whatever settings.json says.
#[test]
fn session_core_auto_compaction_live_toggle_overrides_the_setting() {
    let configured = json!({"compaction": {"enabled": false, "reserveTokens": 100}});
    let on = auto_compaction::settings(&configured, "p", "m", Some(true)).unwrap();
    assert!(on.enabled);
    assert_eq!(on.reserve_tokens, 100);
    let off = auto_compaction::settings(&json!({}), "p", "m", Some(false)).unwrap();
    assert!(!off.enabled);
    assert!(
        !auto_compaction::threshold_due(&off, &[json!({"id": "a"})], Some("a"), 1_000_000, 10),
        "off never compacts"
    );
}

/// `prepareCompactionThreshold`: a compaction at or after the checkpoint's trigger entry means the checkpoint
/// already compacted (no second one from the same answer's usage); one before it does not count.
#[test]
fn session_core_auto_compaction_checkpoint_compacts_once_per_trigger() {
    let settings = CompactionSettings::default();
    let entry = |id: &str, kind: &str| json!({"id": id, "type": kind});
    let before = [entry("c", "compaction"), entry("u", "message"), entry("a", "message")];
    assert!(auto_compaction::threshold_due(
        &settings,
        &before,
        Some("a"),
        990_005,
        1_000_000
    ));
    let after = [entry("u", "message"), entry("a", "message"), entry("c", "compaction")];
    assert!(!auto_compaction::threshold_due(
        &settings,
        &after,
        Some("a"),
        990_005,
        1_000_000
    ));
    // The trigger is the compaction itself: compacted.
    assert!(!auto_compaction::threshold_due(
        &settings,
        &after,
        Some("c"),
        990_005,
        1_000_000
    ));
    // A branch with a compaction and no trigger on it: nothing is due.
    assert!(!auto_compaction::threshold_due(
        &settings,
        &after,
        Some("gone"),
        990_005,
        1_000_000
    ));
    // Below the threshold: nothing is due.
    assert!(!auto_compaction::threshold_due(
        &settings,
        &before,
        Some("a"),
        983_616,
        1_000_000
    ));
}

/// `normalizeError`: an overflowed response commits as an error, keeping a provider's error text.
#[test]
fn session_core_auto_compaction_overflow_commits_as_an_error() {
    let silent: AssistantMessage = serde_json::from_value(json!({
        "role": "assistant", "content": [{"type": "text", "text": "partial"}], "api": "a", "provider": "p",
        "model": "m", "stopReason": "stop", "timestamp": 1,
        "usage": {"input": 300, "output": 1, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 301,
            "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "total": 0}},
    }))
    .unwrap();
    assert!(auto_compaction::overflowed(&silent, 200, 64));
    let error = auto_compaction::as_error(&silent);
    assert_eq!(error.stop_reason, ai::StopReason::Error);
    assert_eq!(
        error.error_message.as_deref(),
        Some("Assistant request exceeded the context window")
    );
    let mut provider = silent.clone();
    provider.stop_reason = ai::StopReason::Error;
    provider.error_message = Some("prompt is too long".into());
    assert_eq!(
        auto_compaction::as_error(&provider).error_message.as_deref(),
        Some("prompt is too long")
    );
    assert_eq!(Reason::parse(Reason::Overflow.name()), Some(Reason::Overflow));
    assert_eq!(Reason::parse(Reason::Threshold.name()), Some(Reason::Threshold));
    assert_eq!(Reason::parse("manual"), None);
}
