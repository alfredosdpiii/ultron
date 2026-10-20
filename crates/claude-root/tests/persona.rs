//! The persona for `ultron claude` (resources::persona), with nothing in ~/.claude: the launch's system prompt
//! carries personality.md and the pstack directory for the root only, engineering mode's skills note asks Claude to
//! use the bundled skills on its own, and the Stop hook's answer (crate::persona_hook) applies the nudge rule with
//! its cap. Private temp profile; the real claude CLI never runs.

use claude_root::guide::{Audience, GuideOptions, ResourceFlags, claude_system_prompt, load_prompt_resources};
use claude_root::launcher::{LaunchInput, LauncherOptions, build_claude_launch};
use claude_root::persona_hook::stop_answer;
use claude_root::self_cmd::SelfCommand;
use resources::persona::NUDGE_TEXT;
use serde_json::{Value, json};
use session_core::RunEnv;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const PERSONALITY: &str = "# Ultron\n\nYou were built to finish things.";

fn bundled() -> PathBuf {
    std::fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/skills")).unwrap()
}

fn env(root: &Path, settings: Value) -> RunEnv {
    let (home, agent, cwd) = (root.join("h"), root.join("a"), root.join("w"));
    for dir in [&home, &agent, &cwd] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(agent.join("settings.json"), settings.to_string()).unwrap();
    std::fs::write(agent.join("personality.md"), format!("{PERSONALITY}\n")).unwrap();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("s").display().to_string()),
    ];
    RunEnv {
        cwd,
        home,
        var: Arc::new(move |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())),
        docs: bundled().join("../docs"),
    }
}

/// The system prompt file `ultron claude` launches Claude Code with.
fn launch_prompt(env: &RunEnv) -> String {
    let resources = load_prompt_resources(env, Some(&bundled()), ResourceFlags::default());
    let flags: BTreeSet<String> = ["system-prompt-file", "mcp-config", "settings"]
        .map(String::from)
        .into();
    let self_cmd = SelfCommand {
        command: "/opt/ultron/bin/ultron".into(),
        args: Vec::new(),
    };
    let dir = env.home.join("launch");
    let bundled = bundled();
    let launch = build_claude_launch(&LaunchInput {
        claude: "claude",
        flags: &flags,
        self_cmd: &self_cmd,
        options: &LauncherOptions {
            hooks: true,
            ..LauncherOptions::default()
        },
        cwd: &env.cwd.display().to_string(),
        dir: Some(&dir),
        socket: "/run/s.sock",
        var: &|_| None,
        date: "2026-10-19".into(),
        platform: "linux",
        resources: &resources,
        bundled_skills: Some(&bundled),
        hints: false,
        skip_permissions: Vec::new(),
    });
    launch
        .files
        .iter()
        .find(|(path, _)| path.file_name().is_some_and(|n| n.to_string_lossy().contains("prompt")))
        .map(|(_, text)| text.clone())
        .unwrap_or_else(|| panic!("no prompt file in {:?}", launch.files))
}

#[test]
fn claude_root_persona_is_in_the_root_launch_prompt_with_the_pstack_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let env = env(tmp.path(), json!({"persona": "ultron"}));
    let prompt = launch_prompt(&env);
    let pstack = format!(
        "The pstack skills directory is {} (principle-<name>/SKILL.md).",
        bundled().join("pstack").display()
    );
    assert!(
        prompt.contains(&format!(
            "<persona>\n{PERSONALITY}\n\n{pstack}\n\n{}\n</persona>",
            resources::persona::GUARDRAILS
        )),
        "{prompt}"
    );
    assert!(prompt.find("<persona>").unwrap() < prompt.find("<skills>").unwrap());
    // A subagent's prompt has no persona.
    let resources = load_prompt_resources(&env, Some(&bundled()), ResourceFlags::default());
    let child = claude_system_prompt(
        Audience::ClaudeChild,
        &GuideOptions {
            allowance: Some(1),
            resources: Some(&resources),
            bundled_skills: Some(&bundled()),
            ..GuideOptions::default()
        },
        false,
    );
    assert!(
        !child.contains("<persona>") && !child.contains("You were built to finish things"),
        "{child}"
    );
    // `--no-context-files --no-skills` keeps the persona.
    let bare = load_prompt_resources(
        &env,
        Some(&bundled()),
        ResourceFlags {
            no_context_files: true,
            no_skills: true,
        },
    );
    assert_eq!(bare.persona.as_deref(), Some(PERSONALITY));
}

#[test]
fn claude_root_persona_off_leaves_the_launch_prompt_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let off = launch_prompt(&env(tmp.path(), json!({})));
    assert!(
        !off.contains("<persona>") && !off.contains("You were built to finish things"),
        "{off}"
    );
    assert_eq!(launch_prompt(&env(tmp.path(), json!({"persona": "jarvis"}))), off);
}

#[test]
fn claude_root_engineering_mode_asks_claude_to_use_bundled_skills_on_its_own() {
    let proactive = "Use them on your own: when a task fits one, read it and follow it without being asked, and say which in one line.";
    let reactive = "Read one only when the user asks for it or the task plainly calls for that workflow";
    let tmp = tempfile::tempdir().unwrap();
    let on = launch_prompt(&env(tmp.path(), json!({"engineering": true})));
    assert!(on.contains(proactive) && !on.contains(reactive), "{on}");
    let other = tempfile::tempdir().unwrap();
    let off = launch_prompt(&env(other.path(), json!({"engineering": false})));
    assert!(off.contains(reactive) && !off.contains(proactive), "{off}");
    // The TypeScript guide (`ULTRON_GUIDE=ts`) keeps TypeScript's wording.
    let third = tempfile::tempdir().unwrap();
    let base = env(third.path(), json!({"engineering": true}));
    let inner = base.var.clone();
    let ts = RunEnv {
        var: Arc::new(move |k: &str| {
            if k == "ULTRON_GUIDE" {
                Some("ts".into())
            } else {
                inner(k)
            }
        }),
        ..base
    };
    let ts = launch_prompt(&ts);
    assert!(ts.contains(reactive) && !ts.contains(proactive), "{ts}");
}

fn transcript(final_text: &str, tools: usize) -> String {
    let mut lines = vec![json!({"type": "user", "message": {"role": "user", "content": "fix it"}})];
    for i in 0..tools {
        lines.push(json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "id": format!("t{i}"), "name": "mcp__ultron__rlm", "input": {}}]}}));
        lines.push(
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": format!("t{i}")}]}}),
        );
    }
    lines.push(json!({"type": "assistant", "message": {"content": [{"type": "text", "text": final_text}]}}));
    lines.iter().map(|l| format!("{l}\n")).collect()
}

#[test]
fn claude_root_persona_stop_answer_blocks_announced_steps_up_to_the_cap() {
    let block = json!({"decision": "block", "reason": NUDGE_TEXT});
    let announced = transcript("Patched.\n\nNext I'll run the tests.", 1);
    let mut nudges = 0;
    assert_eq!(stop_answer(false, &announced, &mut nudges), json!({}), "persona off");
    assert_eq!(nudges, 0);
    for n in 1..=5 {
        assert_eq!(stop_answer(true, &announced, &mut nudges), block);
        assert_eq!(nudges, n);
    }
    assert_eq!(stop_answer(true, &announced, &mut nudges), json!({}), "the cap");
    let mut fresh = 0;
    assert_eq!(
        stop_answer(true, &transcript("Next I'll run the tests. OK?", 1), &mut fresh),
        json!({})
    );
    assert_eq!(
        stop_answer(true, &transcript("Next I'll run the tests.", 0), &mut fresh),
        json!({})
    );
    assert_eq!(stop_answer(true, &transcript("All 14 pass.", 2), &mut fresh), json!({}));
    assert_eq!(stop_answer(true, "", &mut fresh), json!({}));
    assert_eq!(fresh, 0);
}

fn line(value: Value) -> String {
    format!("{value}\n")
}

fn tool_line(id: &str) -> String {
    line(
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "mcp__ultron__rlm", "input": {}}]}}),
    )
}

fn text_line(text: &str) -> String {
    line(json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}}))
}

/// Keep-going in `ultron claude`'s Stop hook: a run is work when the transcript's tool calls grew since the last
/// decision (however Claude Code records the blocked stop), a question gets the "user is away" nudge, a run without a
/// tool call ends the mode, and the session's toggle beats the setting for the persona's own nudge.
#[test]
fn claude_root_keep_going_stop_answer_follows_the_transcript_and_the_session_toggle() {
    use claude_root::persona_hook::stop_answer_in;
    use resources::keep_going::{KEEP_GOING_ASKED_TEXT, KEEP_GOING_TEXT, Limits, SessionPersona, Toggle};
    let limits = Limits::default();
    let mut state = SessionPersona::default();
    let (notice, context) = claude_root::persona_hook::prompt_input(&mut state, "fix it and keep going", 100);
    assert_eq!(
        notice.as_deref(),
        Some("keep-going on: \"keep going\" (/keepgoing off ends it)")
    );
    assert_eq!(context.as_deref(), Some(resources::keep_going::START_TEXT));
    assert_eq!(state.toggle, Toggle::On);
    let mut nudges = 0;
    let mut log = line(json!({"type": "user", "message": {"role": "user", "content": "fix it and keep going"}}));
    log += &tool_line("t1");
    log += &text_line("Done with part 1.");
    let block = |reason: &str| json!({"decision": "block", "reason": reason});
    assert_eq!(
        stop_answer_in(&mut state, &mut nudges, false, &log, 200, limits),
        block(KEEP_GOING_TEXT)
    );
    assert_eq!(state.keep_going.as_ref().unwrap().tool_calls_seen, Some(1));
    // Claude Code records the stop's reason as a user line; then another tool call and a question.
    log += &line(json!({"type": "user", "message": {"role": "user", "content": KEEP_GOING_TEXT}}));
    log += &tool_line("t2");
    log += &text_line("Part 2 done. Should I also do part 3?");
    assert_eq!(
        stop_answer_in(&mut state, &mut nudges, false, &log, 300, limits),
        block(KEEP_GOING_ASKED_TEXT)
    );
    assert_eq!(state.keep_going.as_ref().unwrap().continues, 2);
    // A run with no tool call is the report: the mode ends and nothing blocks.
    log += &text_line("Final report: all three parts done.");
    assert_eq!(
        stop_answer_in(&mut state, &mut nudges, false, &log, 400, limits),
        json!({})
    );
    assert_eq!(state.keep_going, None);
    // The persona's own rule with the session's toggle over the setting.
    let announced = transcript("Patched.\n\nNext I'll run the tests.", 1);
    let mut off = SessionPersona {
        toggle: Toggle::Off,
        keep_going: None,
    };
    assert_eq!(stop_answer_in(&mut off, &mut 0, true, &announced, 1, limits), json!({}));
    let mut on = SessionPersona {
        toggle: Toggle::On,
        keep_going: None,
    };
    assert_eq!(
        stop_answer_in(&mut on, &mut 0, false, &announced, 1, limits),
        block(NUDGE_TEXT)
    );
    // The ceiling's last nudge.
    let mut capped = SessionPersona::default();
    capped
        .command(
            "keepgoing",
            "on",
            0,
            false,
            Limits {
                max_continues: 0,
                max_ms: 1,
            },
        )
        .unwrap();
    assert_eq!(
        stop_answer_in(
            &mut capped,
            &mut nudges,
            false,
            &announced,
            5,
            Limits {
                max_continues: 0,
                max_ms: 1
            }
        ),
        block(resources::keep_going::FINAL_TEXT)
    );
    assert_eq!(
        nudges,
        resources::persona::MAX_NUDGES,
        "nothing continues after the report"
    );
}

#[test]
fn claude_root_keep_going_prompt_toggles_block_the_prompt_with_the_status() {
    use claude_root::persona_hook::prompt_toggle;
    use resources::keep_going::{Limits, SessionPersona, Toggle};
    let mut state = SessionPersona::default();
    let limits = Limits::default();
    assert_eq!(
        prompt_toggle(&mut state, "/persona on", 1, false, limits),
        Some(json!({"decision": "block", "reason": "Persona is on for this session; keep-going off"}))
    );
    assert_eq!(state.toggle, Toggle::On);
    assert_eq!(
        prompt_toggle(&mut state, "/persona maybe", 1, false, limits),
        None,
        "not a toggle: the prompt goes on"
    );
    assert_eq!(
        prompt_toggle(&mut state, "keepgoing on", 1, false, limits).unwrap()["reason"],
        "Persona is on for this session; keep-going on (0/100 continues, \"/keepgoing on\")"
    );
    prompt_toggle(&mut state, "/keepgoing off", 1, false, limits).unwrap();
    assert_eq!(state.keep_going, None);
    assert_eq!(
        prompt_toggle(&mut state, "fix the persona module", 1, false, limits),
        None
    );
}

#[test]
fn claude_root_engineering_mode_puts_the_always_on_skills_in_the_launch_prompt() {
    const BODY: &str = "ENG-RULE-BODY: prove it works before you say done.";
    let with_skill = |settings: Value| {
        let tmp = tempfile::tempdir().unwrap();
        let env = env(tmp.path(), settings);
        let skill = tmp.path().join("a/skills/eng-rule");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: eng-rule\ndescription: The engineering rule.\n---\n\n{BODY}\n"),
        )
        .unwrap();
        launch_prompt(&env)
    };
    // Engineering mode: the skill's text is in the prompt, as in native Ultron.
    let on = with_skill(json!({"engineering": true, "alwaysSkills": ["eng-rule"]}));
    assert!(on.contains(BODY), "{on}");
    // Outside it the skill is only listed.
    let off = with_skill(json!({"engineering": false, "alwaysSkills": ["eng-rule"]}));
    assert!(!off.contains(BODY) && off.contains("eng-rule"), "{off}");
}
