//! Keep-going mode and the session's persona toggle (resources::keep_going), Rust-only: the phrase matcher against
//! its fixture of positives and negatives, the session value's round trip, the toggles, and the run-end decision
//! with its ceiling (literal expectations; the clock is the `now` argument).

use resources::keep_going::{
    FINAL_TEXT, KEEP_GOING_ASKED_TEXT, KEEP_GOING_TEXT, KeepGoing, Limits, Next, RunEnd, SessionPersona, Toggle,
    after_run, nudge_message, toggle_command, trigger,
};
use serde_json::{Value, json};

fn fixture() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/keep-going-phrases.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn resources_keep_going_trigger_matches_the_fixture_positives_and_no_negative() {
    let fixture = fixture();
    let positives = fixture["positives"].as_array().unwrap();
    assert!(positives.len() >= 20);
    for case in positives {
        let prompt = case["prompt"].as_str().unwrap();
        assert_eq!(
            trigger(prompt).as_deref(),
            case["phrase"].as_str(),
            "positive {prompt:?}"
        );
    }
    let negatives = fixture["negatives"].as_array().unwrap();
    assert!(negatives.len() >= 15);
    for case in negatives {
        let prompt = case.as_str().unwrap();
        assert_eq!(trigger(prompt), None, "negative {prompt:?}");
    }
}

fn stretch(started_at: i64, continues: u32) -> SessionPersona {
    SessionPersona {
        toggle: Toggle::On,
        keep_going: Some(KeepGoing {
            started_at,
            continues,
            reason: "keep going".into(),
            worked: false,
            tool_calls_seen: None,
        }),
    }
}

#[test]
fn resources_keep_going_state_round_trips_and_unreadable_values_are_the_default() {
    let mut state = stretch(1_000, 7);
    state.keep_going.as_mut().unwrap().worked = true;
    state.keep_going.as_mut().unwrap().tool_calls_seen = Some(4);
    let value = state.to_json();
    assert_eq!(
        value,
        json!({"version": 1, "persona": "on", "keepGoing": {"startedAt": 1000, "continues": 7, "reason": "keep going", "worked": true, "toolCallsSeen": 4}})
    );
    assert_eq!(SessionPersona::from_json(Some(&value)), state);
    let off = SessionPersona {
        toggle: Toggle::Off,
        keep_going: None,
    };
    assert_eq!(
        off.to_json(),
        json!({"version": 1, "persona": "off", "keepGoing": null})
    );
    assert_eq!(SessionPersona::from_json(Some(&off.to_json())), off);
    for unreadable in [json!("on"), json!(null), json!({"persona": 3})] {
        assert_eq!(SessionPersona::from_json(Some(&unreadable)), SessionPersona::default());
    }
    assert_eq!(SessionPersona::from_json(None), SessionPersona::default());
}

#[test]
fn resources_keep_going_session_toggle_overrides_the_setting() {
    let mut state = SessionPersona::default();
    assert!(state.effective(true) && !state.effective(false));
    assert_eq!(state.prompt_override(), None);
    let limits = Limits::default();
    assert_eq!(
        state.command("persona", "on", 0, false, limits).unwrap(),
        "Persona is on for this session; keep-going off"
    );
    assert!(state.effective(false));
    assert_eq!(state.prompt_override(), Some(true));
    state.command("persona", "OFF", 0, true, limits).unwrap();
    assert!(!state.effective(true));
    assert_eq!(state.prompt_override(), Some(false));
    assert_eq!(
        state.command("persona", "auto", 0, true, limits).unwrap(),
        "Persona for this session follows the settings (on); keep-going off"
    );
    assert_eq!(state.toggle, Toggle::Auto);
    assert_eq!(
        state.command("persona", "jarvis", 0, true, limits),
        Err("Usage: /persona on|off|auto (this session)".into())
    );
    // `/keepgoing on` turns the persona on; `/persona off` ends keep-going as well.
    let text = state.command("keepgoing", "on", 5, false, limits).unwrap();
    assert_eq!(
        text,
        "Persona is on for this session; keep-going on (0/100 continues, \"/keepgoing on\")"
    );
    assert_eq!(state.keep_going.as_ref().unwrap().started_at, 5);
    state.command("persona", "off", 6, false, limits).unwrap();
    assert_eq!(state.keep_going, None);
    state.command("keepgoing", "on", 7, false, limits).unwrap();
    assert_eq!(
        state.command("keepgoing", "off", 8, false, limits).unwrap(),
        "Persona is on for this session; keep-going off"
    );
    assert_eq!(
        state.command("keepgoing", "auto", 8, false, limits),
        Err("Usage: /keepgoing on|off".into())
    );
}

#[test]
fn resources_keep_going_phrase_starts_the_mode_and_other_input_ends_it() {
    let mut state = SessionPersona {
        toggle: Toggle::Off,
        keep_going: None,
    };
    assert_eq!(state.user_input("fix the bug", 1), None);
    assert_eq!(
        state,
        SessionPersona {
            toggle: Toggle::Off,
            keep_going: None
        }
    );
    let notice = state.user_input("Fix the bug. I'm going to sleep, don't stop until it's done", 42);
    assert_eq!(
        notice.as_deref(),
        Some("keep-going on: \"i'm going to sleep\" (/keepgoing off ends it)")
    );
    assert_eq!(state.toggle, Toggle::On, "the phrase turns the persona on");
    assert_eq!(state.keep_going.as_ref().unwrap().started_at, 42);
    assert_eq!(state.keep_going.as_ref().unwrap().continues, 0);
    assert_eq!(
        state.user_input("also check the docs", 50).as_deref(),
        Some("keep-going off: you sent input")
    );
    assert_eq!(state.keep_going, None);
    assert_eq!(state.toggle, Toggle::On, "the persona stays on");
}

#[test]
fn resources_keep_going_toggle_commands_are_whole_prompts() {
    assert_eq!(toggle_command("/persona on"), Some(("persona", "on".into())));
    assert_eq!(toggle_command("  /Persona AUTO "), Some(("persona", "auto".into())));
    assert_eq!(toggle_command("/persona"), Some(("persona", String::new())));
    assert_eq!(toggle_command("/keepgoing off"), Some(("keepgoing", "off".into())));
    assert_eq!(toggle_command("keepgoing off"), Some(("keepgoing", "off".into())));
    assert_eq!(toggle_command("/keep-going on"), Some(("keepgoing", "on".into())));
    for not in [
        "persona",
        "/persona ultron",
        "/keepgoing auto",
        "persona on the prompt",
        "the persona off",
        "/goal x",
    ] {
        assert_eq!(toggle_command(not), None, "{not:?}");
    }
}

fn end(tool_calls: usize, text: &str) -> RunEnd<'_> {
    RunEnd {
        tool_calls,
        text,
        ..RunEnd::default()
    }
}

#[test]
fn resources_keep_going_after_run_continues_work_whatever_it_says_and_ends_on_a_report() {
    let limits = Limits::default();
    let mut state = stretch(0, 0);
    // No announce phrase needed, and no 5-nudge cap.
    for n in 1..=7 {
        assert_eq!(
            after_run(&mut state, &end(1, "Done with that part."), 10, limits),
            Next::Continue { n, asked: false }
        );
    }
    assert_eq!(
        after_run(&mut state, &end(2, "Which option do you prefer?"), 10, limits),
        Next::Continue { n: 8, asked: true }
    );
    // A run that made no rlm call is the final report: the mode ends.
    assert_eq!(
        after_run(&mut state, &end(0, "Final report: all done."), 10, limits),
        Next::Stop
    );
    assert_eq!(state.keep_going, None);
    assert_eq!(
        after_run(&mut state, &end(1, "x"), 10, limits),
        Next::Persona,
        "the persona's rule again"
    );
    // Work recorded before a restart counts for the resumed run.
    let mut resumed = stretch(0, 3);
    assert!(resumed.worked());
    assert!(!resumed.worked(), "already recorded");
    assert_eq!(
        after_run(&mut resumed, &end(0, "Tests pass."), 10, limits),
        Next::Continue { n: 4, asked: false }
    );
    assert!(
        !resumed.keep_going.as_ref().unwrap().worked,
        "a continue starts the next run's count"
    );
}

#[test]
fn resources_keep_going_after_run_ends_on_abort_failure_and_waits_for_input() {
    let limits = Limits::default();
    let mut aborted = stretch(0, 0);
    let abort = RunEnd {
        aborted: true,
        tool_calls: 1,
        ..RunEnd::default()
    };
    assert_eq!(after_run(&mut aborted, &abort, 1, limits), Next::Stop);
    assert_eq!(aborted.keep_going, None);
    let mut failed = stretch(0, 0);
    let failure = RunEnd {
        failed: true,
        tool_calls: 1,
        ..RunEnd::default()
    };
    assert_eq!(after_run(&mut failed, &failure, 1, limits), Next::Stop);
    assert_eq!(failed.keep_going, None);
    let mut waiting = stretch(0, 0);
    let pending = RunEnd {
        pending_input: true,
        tool_calls: 1,
        ..RunEnd::default()
    };
    assert_eq!(after_run(&mut waiting, &pending, 1, limits), Next::Stop);
    assert!(waiting.keep_going.is_some(), "the waiting input decides");
}

#[test]
fn resources_keep_going_ceiling_is_100_continues_or_8_hours_then_a_final_nudge() {
    let limits = Limits::default();
    assert_eq!((limits.max_continues, limits.max_ms), (100, 8 * 60 * 60 * 1000));
    let mut counted = stretch(0, 99);
    assert_eq!(
        after_run(&mut counted, &end(1, "ok"), 1, limits),
        Next::Continue { n: 100, asked: false }
    );
    assert_eq!(
        after_run(&mut counted, &end(1, "ok"), 2, limits),
        Next::Final { n: 101 }
    );
    assert_eq!(counted.keep_going, None);
    let eight_hours = 8 * 60 * 60 * 1000;
    let mut timed = stretch(1_000, 3);
    assert_eq!(
        after_run(&mut timed, &end(1, "ok"), 1_000 + eight_hours - 1, limits),
        Next::Continue { n: 4, asked: false }
    );
    assert_eq!(
        after_run(&mut timed, &end(1, "ok"), 1_000 + eight_hours, limits),
        Next::Final { n: 5 }
    );
    assert_eq!(timed.keep_going, None);
    let env = |key: &str| match key {
        "ULTRON_KEEPGOING_MAX_CONTINUES" => Some("3".to_string()),
        "ULTRON_KEEPGOING_MAX_MS" => Some("bad".to_string()),
        _ => None,
    };
    assert_eq!(
        Limits::from_env(&env),
        Limits {
            max_continues: 3,
            max_ms: eight_hours
        }
    );
}

#[test]
fn resources_keep_going_nudge_messages_carry_their_text_and_count() {
    let limits = Limits::default();
    let message = nudge_message(&Next::Continue { n: 2, asked: false }, limits, 9).unwrap();
    assert_eq!(
        message,
        json!({"role": "custom", "customType": "ultron.nudge", "content": KEEP_GOING_TEXT, "display": true,
               "details": {"nudge": 2, "max": 100, "keepGoing": true, "final": false}, "timestamp": 9})
    );
    let asked = nudge_message(&Next::Continue { n: 3, asked: true }, limits, 9).unwrap();
    assert_eq!(asked["content"], KEEP_GOING_ASKED_TEXT);
    assert!(KEEP_GOING_ASKED_TEXT.contains("record the decision and the alternative"));
    let last = nudge_message(&Next::Final { n: 101 }, limits, 9).unwrap();
    assert_eq!(last["content"], FINAL_TEXT);
    assert_eq!(last["details"]["final"], true);
    assert_eq!(nudge_message(&Next::Stop, limits, 9), None);
    // The guardrails: outward or irreversible actions are skipped and listed, schedules never started.
    for text in [KEEP_GOING_TEXT, KEEP_GOING_ASKED_TEXT] {
        assert!(text.contains("Skip irreversible or outward actions"), "{text}");
        assert!(text.contains("never start schedules"), "{text}");
        assert!(text.contains("final report instead of asking"), "{text}");
    }
}
