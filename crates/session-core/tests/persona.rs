//! The persona (`"persona": "ultron"`) through the product: the root system prompt carries personality.md as its own
//! section, rendered again only when the settings change; engineering mode's bundled-skills note asks the model to
//! use them on its own with the Rust guide only; and the session actor's auto-nudge continues a turn that announced
//! its next step after an rlm call (faux Anthropic server on loopback, private temp profile).

mod p_session_support;

use p_session_support::{HANG, Setup, actor, in_order, text, tool, user_texts};
use resources::persona::{NUDGE_CUSTOM_TYPE, NUDGE_TEXT};
use serde_json::{Value, json};
use session_core::RunEnv;
use session_core::compose::{Runtime, Services};
use session_core::core::Op;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

const PERSONALITY: &str = "# Ultron\n\nYou were built to finish things.";

fn runtime(
    root: &Path,
    settings: Value,
    personality: Option<&str>,
    vars: &[(&str, &str)],
) -> (Runtime, std::path::PathBuf) {
    let home = root.join("home");
    let agent = home.join(".ultron/agent");
    std::fs::create_dir_all(&agent).unwrap();
    if let Some(text) = personality {
        std::fs::write(agent.join("personality.md"), text).unwrap();
    }
    let settings_path = agent.join("settings.json");
    std::fs::write(&settings_path, settings.to_string()).unwrap();
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("AGENTS.md"), "Project rules.").unwrap();
    let mut all: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
    ];
    all.extend(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    let env = RunEnv {
        cwd,
        home,
        var: Arc::new(move |key| all.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let runtime = Runtime::new(&env, "print", root.join("sessions"), Services::default()).unwrap();
    (runtime, settings_path)
}

fn section(prompt: &str) -> Option<String> {
    let start = prompt.find("<persona>\n")? + "<persona>\n".len();
    let end = prompt[start..].find("\n</persona>")?;
    Some(prompt[start..start + end].to_string())
}

#[test]
fn session_core_persona_puts_personality_md_in_the_prompt_and_follows_the_settings() {
    let dir = tempfile::tempdir().unwrap();
    let (runtime, settings_path) = runtime(dir.path(), json!({"persona": "ultron"}), Some(PERSONALITY), &[]);
    let on = runtime.system.current();
    assert_eq!(section(&on).as_deref(), Some(PERSONALITY), "{on}");
    let (context, persona, skills) = (
        on.find("<project_context>").unwrap(),
        on.find("<persona>").unwrap(),
        on.find("<skills>").unwrap(),
    );
    assert!(context < persona && persona < skills, "{on}");
    assert!(runtime.diagnostics.is_empty(), "{:?}", runtime.diagnostics);
    // The prefix stays stable: an edit of the file while the mode is unchanged is not read per request.
    let personality = runtime.system.current();
    let agent = settings_path.parent().unwrap();
    std::fs::write(agent.join("personality.md"), "Changed.").unwrap();
    assert_eq!(runtime.system.current(), personality);
    // Off (as `/persona off` writes it): no section; on again: the file is read again.
    std::fs::write(&settings_path, json!({}).to_string()).unwrap();
    let off = runtime.system.current();
    assert_eq!(section(&off), None, "{off}");
    std::fs::write(&settings_path, json!({"persona": "ultron"}).to_string()).unwrap();
    assert_eq!(section(&runtime.system.current()).as_deref(), Some("Changed."));
    // Any other value is off: the prompt is the one without a persona.
    std::fs::write(&settings_path, json!({"persona": "jarvis"}).to_string()).unwrap();
    assert_eq!(runtime.system.current(), off);
}

#[test]
fn session_core_persona_without_personality_md_warns_once_and_has_no_section() {
    let dir = tempfile::tempdir().unwrap();
    let (setup, _) = runtime(dir.path(), json!({"persona": "ultron"}), None, &[]);
    let prompt = setup.system.current();
    assert_eq!(section(&prompt), None, "{prompt}");
    let warnings: Vec<&String> = setup.diagnostics.iter().filter(|d| d.starts_with("persona:")).collect();
    assert_eq!(warnings.len(), 1, "{:?}", setup.diagnostics);
    assert!(warnings[0].contains("personality.md"), "{warnings:?}");
    // Unset: the same prompt and no warning.
    let other = tempfile::tempdir().unwrap();
    let (plain, _) = runtime(other.path(), json!({}), None, &[]);
    assert!(plain.diagnostics.iter().all(|d| !d.starts_with("persona:")));
    assert_eq!(section(&plain.system.current()), None);
}

const PROACTIVE: &str =
    "Use them on your own: when a task fits one, read it and follow it without being asked, and say which in one line.";
const REACTIVE: &str =
    "Read one only when the user asks for it or the task plainly calls for that workflow; ordinary coding needs none.";

#[test]
fn session_core_engineering_mode_with_the_rust_guide_uses_bundled_skills_proactively() {
    let prompt = |settings: Value, vars: &[(&str, &str)]| {
        let dir = tempfile::tempdir().unwrap();
        let (runtime, _) = runtime(dir.path(), settings, None, vars);
        runtime.system.current()
    };
    let on = prompt(json!({"engineering": true}), &[]);
    assert!(on.contains(PROACTIVE) && !on.contains(REACTIVE), "{on}");
    let off = prompt(json!({"engineering": false}), &[]);
    assert!(off.contains(REACTIVE) && !off.contains(PROACTIVE), "{off}");
    // The TypeScript guide keeps TypeScript's wording.
    let ts = prompt(json!({"engineering": true}), &[("ULTRON_GUIDE", "ts")]);
    assert!(ts.contains(REACTIVE) && !ts.contains(PROACTIVE), "{ts}");
}

fn wait_runs(a: &p_session_support::Actor, n: usize) {
    let end = Instant::now() + HANG;
    while a.events("run_end").len() < n {
        assert!(
            Instant::now() < end,
            "timed out waiting for {n} runs: {:?}",
            a.events("run_end")
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// The actor has handled everything before this call (its input is in order).
fn settle(a: &p_session_support::Actor) {
    a.call(Op::Describe).unwrap();
}

fn persona_setup() -> Setup {
    Setup {
        settings: Some(json!({"persona": "ultron"})),
        ..Setup::default()
    }
}

fn nudges(a: &p_session_support::Actor) -> Vec<Value> {
    a.events("message_end")
        .into_iter()
        .map(|e| e["message"].clone())
        .filter(|m| m["customType"] == NUDGE_CUSTOM_TYPE)
        .collect()
}

#[test]
fn session_core_persona_nudge_continues_a_turn_that_announced_its_next_step() {
    let script = in_order(vec![
        tool("toolu_1", "print(1)"),
        text("Patched.\n\nNext I'll run the tests."),
        text("All 14 tests pass. Anything else?"),
    ]);
    let a = actor(script, persona_setup());
    let ended = a.prompt("fix the bug");
    assert_eq!(ended["error"], Value::Null, "{ended}");
    wait_runs(&a, 2);
    settle(&a);
    assert_eq!(a.events("run_start").len(), 2, "the answer that asks is not nudged");
    assert!(a.events("run_end").iter().all(|e| e["status"] == "completed"));
    let sent = nudges(&a);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["content"], NUDGE_TEXT);
    assert_eq!(sent[0]["details"], json!({"nudge": 1, "max": 5}));
    // Clients see it once, as the nudge (no user message with its text).
    let echoed: Vec<Value> = a
        .events("message_end")
        .into_iter()
        .filter(|e| e["message"]["role"] != "custom" && e.to_string().contains("take that next step now"))
        .collect();
    assert!(echoed.is_empty(), "{echoed:?}");
    // The model reads the nudge as the next user message.
    let requests = a.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(user_texts(&requests[2]).last().unwrap(), &json!(NUDGE_TEXT));
    // The transcript a resumed session opens with has it as the nudge.
    let messages = a.call(Op::Messages).unwrap().unwrap();
    let kept: Vec<&Value> = messages
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m.to_string().contains("take that next step now"))
        .collect();
    assert_eq!(kept.len(), 1, "{messages}");
    assert_eq!(kept[0]["role"], "custom");
    assert_eq!(kept[0]["customType"], NUDGE_CUSTOM_TYPE);
    assert_eq!(kept[0]["details"]["nudge"], 1);
    // It is journaled: resume shows it.
    let journaled = a
        .writes()
        .iter()
        .any(|w| w.to_string().contains(NUDGE_CUSTOM_TYPE) && w.to_string().contains("take that next step now"));
    assert!(journaled);
}

#[test]
fn session_core_persona_nudge_skips_questions_runs_without_rlm_and_persona_off() {
    // Ends with a question after an rlm call.
    let asks = actor(
        in_order(vec![tool("t1", "print(1)"), text("Next I'll run the tests. Shall I?")]),
        persona_setup(),
    );
    asks.prompt("go");
    settle(&asks);
    assert_eq!(asks.events("run_start").len(), 1, "no nudge run started");
    assert!(nudges(&asks).is_empty());
    // Announces, but made no rlm call.
    let talks = actor(in_order(vec![text("Let me think about it.")]), persona_setup());
    talks.prompt("go");
    settle(&talks);
    assert_eq!(talks.events("run_start").len(), 1, "no nudge run started");
    // Persona off: the same announced step is never nudged.
    let off = actor(
        in_order(vec![tool("t1", "print(1)"), text("Next I'll run the tests.")]),
        Setup::default(),
    );
    off.prompt("go");
    settle(&off);
    assert_eq!(off.events("run_start").len(), 1, "no nudge run started");
    assert!(nudges(&off).is_empty());
    assert_eq!(off.requests().len(), 2);
}

#[test]
fn session_core_persona_nudge_stops_at_five_and_user_input_starts_over() {
    // Every run makes an rlm call and announces its next step.
    let script: p_session_support::Script = Box::new(|_, index| {
        if index % 2 == 0 {
            tool(&format!("t{index}"), "print(1)")
        } else {
            text("I'll now run the next check.")
        }
    });
    let a = actor(script, persona_setup());
    a.prompt("go");
    wait_runs(&a, 6);
    settle(&a);
    assert_eq!(a.events("run_start").len(), 6, "the prompt's run and five nudges");
    let numbers: Vec<Value> = nudges(&a).iter().map(|m| m["details"]["nudge"].clone()).collect();
    assert_eq!(numbers, [1, 2, 3, 4, 5].map(Value::from));
    // The user's next prompt resets the count.
    a.prompt("again");
    wait_runs(&a, 12);
    settle(&a);
    assert_eq!(a.events("run_start").len(), 12);
    let numbers: Vec<Value> = nudges(&a).iter().map(|m| m["details"]["nudge"].clone()).collect();
    assert_eq!(numbers, [1, 2, 3, 4, 5, 1, 2, 3, 4, 5].map(Value::from));
}

#[test]
fn session_core_persona_nudge_yields_to_a_queued_user_message() {
    // The first reply waits until the test queued a message for the next run.
    let (started, started_rx) = std::sync::mpsc::channel::<()>();
    let (go, go_rx) = std::sync::mpsc::channel::<()>();
    let script: p_session_support::Script = Box::new(move |_, index| {
        if index == 0 {
            let _ = started.send(());
            let _ = go_rx.recv_timeout(HANG);
            tool("t1", "print(1)")
        } else {
            text("Next I'll run the tests.")
        }
    });
    let a = actor(script, persona_setup());
    a.call(Op::Prompt {
        message: "go".into(),
        images: Vec::new(),
        queue: None,
    })
    .unwrap();
    started_rx.recv_timeout(HANG).unwrap();
    a.call(Op::Enqueue {
        kind: session_core::core::QueueKind::NextRun,
        message: "then the docs".into(),
        images: Vec::new(),
        entry_id: None,
    })
    .unwrap();
    go.send(()).unwrap();
    wait_runs(&a, 1);
    settle(&a);
    assert_eq!(a.events("run_start").len(), 1, "the queued message wins over the nudge");
    assert!(nudges(&a).is_empty());
}
