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
        prompt.contains(&format!("<persona>\n{PERSONALITY}\n\n{pstack}\n</persona>")),
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
