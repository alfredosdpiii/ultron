//! Keep-going mode and the session's persona toggle through the session actor (resources::keep_going): a prompt
//! with a keep-going phrase is confirmed in one line and every working run after it is continued (no announce
//! phrase needed, past the persona's 5-nudge cap) until a run makes no rlm call; a question is answered with the
//! "user is away" nudge; user input, an abort and `/keepgoing off` end it; the ceiling (injected through
//! ULTRON_KEEPGOING_MAX_CONTINUES, and the 8 hours through the kept start time) sends a last nudge for the report;
//! the session's toggle beats the settings in the system prompt and the nudge; the toggle, the mode and its counters
//! survive a hand-over to the next worker, whose resumed run is continued. Faux Anthropic server on loopback,
//! private temp homes; nothing is killed and no signal is sent.

mod p_session_support;
mod reopen_support;

use p_session_support::{HANG, Script, Setup, actor, in_order, text, tool, user_texts};
use reopen_support::Home;
use resources::keep_going::{FINAL_TEXT, KEEP_GOING_ASKED_TEXT, KEEP_GOING_TEXT, STATE_KEY, STATE_NAMESPACE};
use resources::persona::NUDGE_CUSTOM_TYPE;
use serde_json::{Value, json};
use session_core::core::{Op, QueueKind};
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

const PERSONALITY: &str = "# Ultron\n\nYou were built to finish things.";

fn wait_runs(events: &dyn Fn() -> Vec<Value>, n: usize) {
    let end = Instant::now() + HANG;
    while events().len() < n {
        assert!(Instant::now() < end, "timed out waiting for {n} runs: {:?}", events());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn nudges(messages: Vec<Value>) -> Vec<Value> {
    messages
        .into_iter()
        .map(|e| e["message"].clone())
        .filter(|m| m["customType"] == NUDGE_CUSTOM_TYPE)
        .collect()
}

fn start(a: &p_session_support::Actor, message: &str) -> Option<Value> {
    a.call(Op::Prompt {
        message: message.into(),
        images: Vec::new(),
        queue: None,
    })
    .unwrap()
}

fn status(call: impl Fn(Op) -> Result<Option<Value>, String>) -> Value {
    call(Op::KeepGoing { args: String::new() }).unwrap().unwrap()
}

#[test]
fn session_core_keep_going_phrase_continues_every_working_run_until_a_report() {
    // Seven working runs (an rlm call, then text that announces nothing), the second ending with a question; the
    // eighth only reports.
    let script: Script = Box::new(|_, index| match index {
        14 => text("Final report: the parser is ported, 31 tests pass."),
        3 => text("Parser half done. Should I also port the lexer?"),
        i if i % 2 == 0 => tool(&format!("t{i}"), "print(1)"),
        _ => text("Done with that part."),
    });
    let a = actor(script, Setup::default());
    let reply = start(&a, "Port the parser. I'm going to sleep, keep going until it's done");
    assert_eq!(
        reply,
        Some(json!({"keepGoing": "keep-going on: \"i'm going to sleep\" (/keepgoing off ends it)"}))
    );
    wait_runs(&|| a.events("run_end"), 8);
    a.call(Op::Describe).unwrap();
    assert_eq!(a.events("run_start").len(), 8, "the report ends it: no ninth run");
    let sent = nudges(a.events("message_end"));
    let numbers: Vec<Value> = sent.iter().map(|m| m["details"]["nudge"].clone()).collect();
    assert_eq!(
        numbers,
        (1..=7).map(Value::from).collect::<Vec<_>>(),
        "past the persona's cap of 5"
    );
    assert!(
        sent.iter()
            .all(|m| m["details"]["keepGoing"] == true && m["details"]["max"] == 100)
    );
    assert_eq!(sent[0]["content"], KEEP_GOING_TEXT);
    assert_eq!(sent[1]["content"], KEEP_GOING_ASKED_TEXT, "the run that asked");
    assert_eq!(sent[2]["content"], KEEP_GOING_TEXT);
    // The model reads the nudge as the next user message.
    let requests = a.requests();
    assert_eq!(requests.len(), 15);
    assert_eq!(user_texts(&requests[2]).last().unwrap(), &json!(KEEP_GOING_TEXT));
    assert_eq!(user_texts(&requests[4]).last().unwrap(), &json!(KEEP_GOING_ASKED_TEXT));
    // The phrase turned the persona on for the session; the mode is over and the session keeps that.
    let state = status(|op| a.call(op));
    assert_eq!(state["persona"], "on");
    assert_eq!(state["keepGoing"], Value::Null);
    assert_eq!(state["text"], "Persona is on for this session; keep-going off");
    let kept: Vec<Value> = a
        .writes()
        .into_iter()
        .filter(|w| w["namespace"] == STATE_NAMESPACE && w["key"] == STATE_KEY && w["op"] == "set")
        .map(|w| w["value"].clone())
        .collect();
    assert_eq!(kept.first().unwrap()["keepGoing"]["reason"], "i'm going to sleep");
    assert_eq!(kept.last().unwrap()["keepGoing"], Value::Null);
    assert!(kept.iter().any(|v| v["keepGoing"]["continues"] == 7), "{kept:?}");
}

#[test]
fn session_core_keep_going_ceiling_sends_a_final_nudge_and_stops() {
    // Every run works and announces its next step: only the ceiling stops it.
    let script: Script = Box::new(|_, index| {
        if index % 2 == 0 {
            tool(&format!("t{index}"), "print(1)")
        } else {
            text("I'll now run the next check.")
        }
    });
    let a = actor(
        script,
        Setup {
            vars: vec![("ULTRON_KEEPGOING_MAX_CONTINUES", "3".into())],
            ..Setup::default()
        },
    );
    start(&a, "keep going");
    wait_runs(&|| a.events("run_end"), 5);
    a.call(Op::Describe).unwrap();
    assert_eq!(
        a.events("run_start").len(),
        5,
        "three continues, the final nudge's run, nothing after"
    );
    let sent = nudges(a.events("message_end"));
    let details: Vec<Value> = sent.iter().map(|m| m["details"].clone()).collect();
    assert_eq!(
        details,
        [
            json!({"nudge": 1, "max": 3, "keepGoing": true, "final": false}),
            json!({"nudge": 2, "max": 3, "keepGoing": true, "final": false}),
            json!({"nudge": 3, "max": 3, "keepGoing": true, "final": false}),
            json!({"nudge": 4, "max": 3, "keepGoing": true, "final": true}),
        ]
    );
    assert_eq!(sent[3]["content"], FINAL_TEXT);
    assert_eq!(status(|op| a.call(op))["keepGoing"], Value::Null);
}

/// A script whose request `blocked` waits for the test (`started` says it is waiting, `go` releases it).
fn blocking(blocked: usize, replies: Vec<String>) -> (Script, Receiver<()>, Sender<()>) {
    let (started, started_rx) = channel::<()>();
    let (go, go_rx) = channel::<()>();
    let script: Script = Box::new(move |_, index| {
        if index == blocked {
            let _ = started.send(());
            let _ = go_rx.recv_timeout(HANG);
        }
        replies.get(index).cloned().unwrap_or_else(|| text("(no more replies)"))
    });
    (script, started_rx, go)
}

#[test]
fn session_core_keep_going_ends_on_keepgoing_off_abort_and_user_input() {
    let working = || vec![tool("t1", "print(1)"), text("Step done."), text("More.")];
    // `/keepgoing off` while the run works: the run ends and nothing continues.
    let (script, started, go) = blocking(1, working());
    let off = actor(script, Setup::default());
    start(&off, "don't stop until you finish");
    started.recv_timeout(HANG).unwrap();
    let reply = off.call(Op::KeepGoing { args: "off".into() }).unwrap().unwrap();
    assert_eq!(reply["keepGoing"], Value::Null);
    assert_eq!(reply["text"], "Persona is on for this session; keep-going off");
    go.send(()).unwrap();
    wait_runs(&|| off.events("run_end"), 1);
    off.call(Op::Describe).unwrap();
    assert_eq!(off.events("run_start").len(), 1);
    assert!(nudges(off.events("message_end")).is_empty());

    // An abort ends it.
    let (script, started, go) = blocking(1, working());
    let aborted = actor(script, Setup::default());
    start(&aborted, "keep going");
    started.recv_timeout(HANG).unwrap();
    aborted.call(Op::Abort).unwrap();
    go.send(()).unwrap();
    wait_runs(&|| aborted.events("run_end"), 1);
    aborted.call(Op::Describe).unwrap();
    assert_eq!(aborted.events("run_start").len(), 1);
    assert!(nudges(aborted.events("message_end")).is_empty());
    assert_eq!(status(|op| aborted.call(op))["keepGoing"], Value::Null);

    // A message the user queues meanwhile (for the next run) ends it: the run ends and nothing continues.
    let (script, started, go) = blocking(1, working());
    let input = actor(script, Setup::default());
    start(&input, "work through the night on this");
    started.recv_timeout(HANG).unwrap();
    input
        .call(Op::Enqueue {
            kind: QueueKind::NextRun,
            message: "and update the docs".into(),
            images: Vec::new(),
            entry_id: None,
        })
        .unwrap();
    assert_eq!(status(|op| input.call(op))["keepGoing"], Value::Null);
    go.send(()).unwrap();
    wait_runs(&|| input.events("run_end"), 1);
    input.call(Op::Describe).unwrap();
    assert_eq!(
        input.events("run_start").len(),
        1,
        "the queued message waits for the user's next run"
    );
    assert!(nudges(input.events("message_end")).is_empty());
}

fn system_text(request: &Value) -> String {
    request["system"].to_string()
}

#[test]
fn session_core_keep_going_session_toggle_beats_the_setting_and_survives_a_reopen() {
    let home = Home::new(Some(json!({})));
    let agent = home.root.join("agent");
    std::fs::write(agent.join("personality.md"), PERSONALITY).unwrap();
    // The setting is off; the session turns the persona on.
    let first = home.open(in_order(vec![text("hello")]), &[], None);
    let reply = first.call(Op::Persona { args: "on".into() }).unwrap().unwrap();
    assert_eq!(
        reply,
        json!({"text": "Persona is on for this session; keep-going off", "persona": "on", "effective": true, "keepGoing": null})
    );
    first
        .call(Op::PromptAndWait {
            message: "hi".into(),
            images: Vec::new(),
        })
        .unwrap();
    assert!(system_text(&first.requests()[0]).contains("You were built to finish things."));
    assert!(system_text(&first.requests()[0]).contains(resources::persona::GUARDRAILS));
    first.close();

    // The next worker keeps the session's toggle: the prompt has the persona and its nudge runs.
    let next = home.open(
        in_order(vec![
            tool("t1", "print(1)"),
            text("Patched.\n\nNext I'll run the tests."),
            text("All tests pass. Anything else?"),
        ]),
        &[],
        None,
    );
    assert_eq!(status(|op| next.call(op))["persona"], "on");
    next.call(Op::PromptAndWait {
        message: "fix it".into(),
        images: Vec::new(),
    })
    .unwrap();
    wait_runs(&|| next.events("run_end"), 2);
    next.call(Op::Describe).unwrap();
    assert!(system_text(&next.requests()[0]).contains("You were built to finish things."));
    let sent = nudges(next.events("message_end"));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        sent[0]["details"],
        json!({"nudge": 1, "max": 5}),
        "the persona's own nudge"
    );

    // Off beats the setting turned on; auto follows it again.
    std::fs::write(agent.join("settings.json"), json!({"persona": "ultron"}).to_string()).unwrap();
    next.call(Op::Persona { args: "off".into() }).unwrap();
    let before = next.requests().len();
    next.call(Op::PromptAndWait {
        message: "again".into(),
        images: Vec::new(),
    })
    .unwrap();
    let requests = next.requests();
    assert!(!system_text(&requests[before]).contains("You were built to finish things."));
    let auto = next.call(Op::Persona { args: "auto".into() }).unwrap().unwrap();
    assert_eq!(auto["effective"], true);
    assert_eq!(
        auto["text"],
        "Persona for this session follows the settings (on); keep-going off"
    );
    let bad = next.call(Op::Persona { args: "jarvis".into() });
    assert_eq!(bad, Err("Usage: /persona on|off|auto (this session)".into()));
}

const STARTED: &str = "started";
const RELEASE: &str = "release";

fn wait_file(path: &Path) {
    let end = Instant::now() + HANG;
    while !path.exists() {
        assert!(Instant::now() < end, "timed out waiting for {}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The first worker starts keep-going and hands the session over while its run's cell still works.
fn interrupted_stretch(home: &Home) {
    let cell = format!(
        "import asyncio, os\nopen({}, 'w').write('1')\nfor _ in range(12000):\n    if os.path.exists({}):\n        break\n    await asyncio.sleep(0.01)\nprint('step 1 done')",
        json!(STARTED),
        json!(RELEASE)
    );
    let first = home.open(in_order(vec![tool("t1", &cell)]), &[], None);
    let reply = first
        .call(Op::Prompt {
            message: "Port the parser and keep going".into(),
            images: Vec::new(),
            queue: None,
        })
        .unwrap();
    assert_eq!(
        reply.unwrap()["keepGoing"],
        "keep-going on: \"keep going\" (/keepgoing off ends it)"
    );
    wait_file(&home.cwd.join(STARTED));
    first.hand_over();
    std::fs::write(home.cwd.join(RELEASE), "1").unwrap();
}

/// The persona state the session keeps (its last write).
fn kept_state(home: &Home) -> Value {
    home.writes()
        .into_iter()
        .filter(|w| w["namespace"] == STATE_NAMESPACE && w["key"] == STATE_KEY && w["op"] == "set")
        .map(|w| w["value"].clone())
        .next_back()
        .unwrap()
}

#[test]
fn session_core_keep_going_resumes_after_a_restart_with_its_counters() {
    let home = Home::new(None);
    interrupted_stretch(&home);
    let kept = kept_state(&home);
    assert_eq!(kept["keepGoing"]["worked"], true, "the run's rlm call is kept: {kept}");
    assert_eq!(kept["keepGoing"]["continues"], 0);
    // The next worker resumes the run; its end is continued as keep-going, then the report ends it.
    let next = home.open(
        in_order(vec![text("Step 1 is done."), text("Final report: parser ported.")]),
        &[],
        None,
    );
    next.call(Op::Recover).unwrap();
    wait_runs(&|| next.events("run_end"), 2);
    next.call(Op::Describe).unwrap();
    let sent = nudges(next.events("message_end"));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["content"], KEEP_GOING_TEXT);
    assert_eq!(sent[0]["details"]["nudge"], 1);
    assert_eq!(
        next.events("run_start").len(),
        2,
        "the resumed run and the nudge's, nothing after the report"
    );
    assert_eq!(kept_state(&home)["keepGoing"], Value::Null);
}

#[test]
fn session_core_keep_going_eight_hours_from_the_kept_start_ends_with_the_final_nudge() {
    let home = Home::new(None);
    interrupted_stretch(&home);
    // The stretch started more than 8 hours ago (the clock, injected through the kept start time).
    let mut state = kept_state(&home);
    let eight_hours = 8 * 60 * 60 * 1000;
    state["keepGoing"]["startedAt"] = json!(session_core::lane::now_ms() - eight_hours - 1_000);
    state["keepGoing"]["continues"] = json!(2);
    let repo = session_journal::SessionRepo::new(&home.sessions(), session_journal::SystemClock::shared());
    let metadata = repo.list(None).into_iter().next().unwrap();
    let mut session = repo.open(&metadata).unwrap();
    session
        .commit(&[session_journal::Write::set(STATE_NAMESPACE, STATE_KEY, state)])
        .unwrap();
    drop(session);
    let next = home.open(
        in_order(vec![text("Step 1 is done."), text("Final report: half ported.")]),
        &[],
        None,
    );
    next.call(Op::Recover).unwrap();
    wait_runs(&|| next.events("run_end"), 2);
    next.call(Op::Describe).unwrap();
    let sent = nudges(next.events("message_end"));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["content"], FINAL_TEXT);
    assert_eq!(
        sent[0]["details"],
        json!({"nudge": 3, "max": 100, "keepGoing": true, "final": true})
    );
    assert_eq!(kept_state(&home)["keepGoing"], Value::Null);
}
