//! The persona (resources::persona): the auto-nudge rule replayed against fixtures/persona-nudge.json (recorded from
//! the user's Claude Code hook's rule by generate-persona-nudge.py), the decision's conditions one by one, the
//! chain's count, the prompt's persona section, and engineering mode's proactive skills note.

use resources::persona::{
    Chain, MAX_NUDGES, NUDGE_CUSTOM_TYPE, NUDGE_TEXT, TurnEnd, decide, last_turn, nudge_message, read_personality,
    should_nudge,
};
use resources::skills::{Skill, bundled_skills_note};
use resources::system_prompt::{BuildOptions, build_system_prompt};
use serde_json::{Value, json};

fn fixture() -> Value {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/persona-nudge.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[test]
fn resources_persona_nudge_rule_matches_the_recorded_hook() {
    let fixture = fixture();
    assert_eq!(fixture["nudge"], NUDGE_TEXT);
    assert_eq!(fixture["maxNudges"], MAX_NUDGES);
    let cases = fixture["cases"].as_array().unwrap();
    assert!(cases.len() > 100);
    for case in cases {
        let (text, tools) = (case["text"].as_str().unwrap(), case["tools"].as_u64().unwrap() as usize);
        assert_eq!(should_nudge(text, tools), case["nudge"] == true, "{case}");
    }
    for transcript in fixture["transcripts"].as_array().unwrap() {
        let (text, tools) = last_turn(transcript["jsonl"].as_str().unwrap());
        assert_eq!(
            (text.as_str(), tools as u64),
            (
                transcript["text"].as_str().unwrap(),
                transcript["tools"].as_u64().unwrap()
            ),
            "{}",
            transcript["name"]
        );
        assert_eq!(
            should_nudge(&text, tools),
            transcript["nudge"] == true,
            "{}",
            transcript["name"]
        );
    }
}

fn due() -> TurnEnd<'static> {
    TurnEnd {
        persona: true,
        aborted: false,
        failed: false,
        pending_input: false,
        tool_calls: 1,
        text: "Patched it.\n\nNext I'll run the tests.",
        nudges: 0,
    }
}

#[test]
fn resources_persona_decide_requires_every_condition() {
    assert!(decide(&due()));
    let off = [
        TurnEnd {
            persona: false,
            ..due()
        },
        TurnEnd { aborted: true, ..due() },
        TurnEnd { failed: true, ..due() },
        TurnEnd {
            pending_input: true,
            ..due()
        },
        TurnEnd { tool_calls: 0, ..due() },
        TurnEnd {
            text: "Next I'll run the tests. Shall I?",
            ..due()
        },
        TurnEnd {
            text: "Next I'll run the tests.\n\nAll 14 pass.",
            ..due()
        },
        TurnEnd {
            nudges: MAX_NUDGES,
            ..due()
        },
    ];
    for (index, end) in off.iter().enumerate() {
        assert!(!decide(end), "case {index}: {end:?}");
    }
    // The cap: the fifth nudge is still sent, a sixth is not.
    assert!(decide(&TurnEnd { nudges: 4, ..due() }));
    assert!(decide(&TurnEnd {
        text: "LET ME check that.",
        ..due()
    }));
}

fn assistant(text: &str, calls: usize) -> Value {
    let mut content = vec![json!({"type": "text", "text": text})];
    content.extend((0..calls).map(|i| json!({"type": "toolCall", "id": format!("t{i}"), "name": "rlm"})));
    json!({"role": "assistant", "content": content})
}

#[test]
fn resources_persona_chain_counts_to_the_cap_resets_on_input_and_an_abort_ends_it() {
    let mut chain = Chain::default();
    let turn = |chain: &mut Chain| {
        chain.run_started();
        chain.assistant_message(&assistant("Looking.", 1));
        chain.assistant_message(&assistant("Next I'll run the tests.", 0));
    };
    let mut sent = Vec::new();
    for _ in 0..7 {
        turn(&mut chain);
        sent.extend(chain.run_ended(true, false, false, false));
    }
    assert_eq!(sent, [1, 2, 3, 4, 5]);
    chain.user_input();
    turn(&mut chain);
    assert_eq!(chain.run_ended(true, false, false, false), Some(1));
    // An abort ends the chain until the user's next input.
    turn(&mut chain);
    assert_eq!(chain.run_ended(true, true, false, false), None);
    turn(&mut chain);
    assert_eq!(chain.run_ended(true, false, false, false), None);
    chain.user_input();
    turn(&mut chain);
    assert_eq!(chain.run_ended(true, false, false, false), Some(1));
    // A queued message wins; the count is unchanged.
    turn(&mut chain);
    assert_eq!(chain.run_ended(true, false, false, true), None);
    assert_eq!(chain.nudges, 1);
    // The run's own calls and last text count: a later message without text keeps the text, a new run starts over.
    chain.run_started();
    chain.assistant_message(&assistant("I'll fix it.", 2));
    chain.assistant_message(&json!({"role": "assistant", "content": [{"type": "toolCall", "id": "x"}]}));
    assert_eq!((chain.tool_calls, chain.text.as_str()), (3, "I'll fix it."));
    chain.run_started();
    chain.assistant_message(&assistant("Let me look.", 0));
    assert_eq!(
        chain.run_ended(true, false, false, false),
        None,
        "no rlm call in this run"
    );
}

#[test]
fn resources_persona_nudge_message_is_a_displayed_custom_message() {
    assert_eq!(
        nudge_message(2, 7),
        json!({"role": "custom", "customType": NUDGE_CUSTOM_TYPE, "content": NUDGE_TEXT, "display": true,
               "details": {"nudge": 2, "max": 5}, "timestamp": 7})
    );
    assert_eq!(NUDGE_CUSTOM_TYPE, "ultron.nudge");
}

#[test]
fn resources_persona_personality_is_read_trimmed_and_a_missing_file_is_a_warning() {
    let dir = tempfile::tempdir().unwrap();
    let warning = read_personality(dir.path()).unwrap_err();
    assert!(
        warning.starts_with(&format!(
            "persona: cannot read {}",
            dir.path().join("personality.md").display()
        )),
        "{warning}"
    );
    std::fs::write(dir.path().join("personality.md"), "\n# Ultron\n\nCold.\n\n").unwrap();
    assert_eq!(
        read_personality(dir.path()).unwrap().as_deref(),
        Some("# Ultron\n\nCold.")
    );
    std::fs::write(dir.path().join("personality.md"), " \n").unwrap();
    assert_eq!(read_personality(dir.path()).unwrap(), None);
}

#[test]
fn resources_system_prompt_persona_section_sits_after_the_context_files_and_before_the_skills() {
    let options = BuildOptions {
        selected_tools: vec!["rlm".into()],
        context_files: vec![resources::context_files::ContextFile {
            path: "/p/AGENTS.md".into(),
            content: "Project rules.".into(),
        }],
        skills: vec![skill("/s/tidy/SKILL.md", false)],
        persona: Some("You are Ultron.".into()),
        ..BuildOptions::new("/p")
    };
    let prompt = build_system_prompt(&options, None).unwrap();
    let (context, persona, skills) = (
        prompt.find("<project_context>").unwrap(),
        prompt.find("<persona>\nYou are Ultron.\n</persona>").unwrap(),
        prompt.find("<skills>").unwrap(),
    );
    assert!(context < persona && persona < skills, "{prompt}");
    let without = build_system_prompt(
        &BuildOptions {
            persona: None,
            ..options
        },
        None,
    )
    .unwrap();
    assert!(!without.contains("persona"), "{without}");
}

fn skill(path: &str, hidden: bool) -> Skill {
    Skill {
        name: path.split('/').nth_back(1).unwrap().into(),
        description: "Plan the work. Then do it.".into(),
        file_path: path.into(),
        base_dir: path.trim_end_matches("/SKILL.md").into(),
        source: "path".into(),
        disable_model_invocation: hidden,
        always_on_content: None,
    }
}

#[test]
fn resources_bundled_skills_note_is_proactive_only_when_asked() {
    let skills = [skill("/b/pstack/plan/SKILL.md", true)];
    let reactive = bundled_skills_note(&skills, Some("/b"), false);
    assert_eq!(
        reactive,
        "Bundled skills in /b (<source>/<name>/SKILL.md). Read one only when the user asks for it or the task plainly calls for that workflow; ordinary coding needs none. The principles pstack's skills cite are pstack/principle-<name>/SKILL.md.\n- pstack/plan: Plan the work."
    );
    let proactive = bundled_skills_note(&skills, Some("/b"), true);
    assert_eq!(
        proactive,
        "Bundled skills in /b (<source>/<name>/SKILL.md). Use them on your own: when a task fits one, read it and follow it without being asked, and say which in one line. The principles pstack's skills cite are pstack/principle-<name>/SKILL.md.\n- pstack/plan: Plan the work."
    );
    // Through the prompt builder.
    let options = |proactive_skills| BuildOptions {
        selected_tools: vec!["rlm".into()],
        skills: skills.to_vec(),
        bundled_skills_dir: Some("/b".into()),
        proactive_skills,
        ..BuildOptions::new("/p")
    };
    assert!(
        build_system_prompt(&options(true), None)
            .unwrap()
            .contains("Use them on your own")
    );
    assert!(
        build_system_prompt(&options(false), None)
            .unwrap()
            .contains("Read one only when")
    );
}
