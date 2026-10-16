//! Ported from packages/coding-agent/test/ultron-tool-round-nudge.test.ts (the brake, the wait steer, the skill hint)
//! and packages/coding-agent/test/ultron-image-budget.test.ts, with their literal expectations.

use ai::{Message, ToolResultMessage, UserContent};
use serde_json::json;
use session_core::nudge::{
    SkillExtractionNudger, ToolRoundNudger, skill_nudge_from_env, tool_rounds_nudge_from_env, wait_nudge_message,
};
use session_core::view_images::{OFFLOADED_IMAGE_TEXT, context_image_budget, images_to_drop, with_image_budget};

fn steer(nudger: &mut ToolRoundNudger, steered: &mut Vec<String>, calls: usize, running: u64) {
    steered.extend(nudger.turn_ended("run", calls, running));
}

#[test]
fn session_core_nudge_steers_once_at_the_threshold_again_at_twice_it_and_resets_when_the_model_answers() {
    let mut steered = Vec::new();
    let mut nudger = ToolRoundNudger::new(3, true);
    for _ in 0..7 {
        steer(&mut nudger, &mut steered, 2, 0);
    }
    assert_eq!(steered.len(), 2);
    assert!(steered[0].contains("3 rounds"));
    assert!(steered[1].contains("Stop calling tools"));
    assert_eq!(
        steered[0],
        "[Ultron] You have used 3 rounds of tool calls in this turn without answering. Unless one more specific call is essential, stop gathering and answer now with what you have, noting what remains unknown."
    );
    assert_eq!(
        steered[1],
        "[Ultron] You have now used 6 rounds of tool calls in this turn. Stop calling tools and write your answer now from what you already have. List anything still unknown instead of searching further."
    );
    steer(&mut nudger, &mut steered, 0, 0);
    steer(&mut nudger, &mut steered, 1, 0);
    steer(&mut nudger, &mut steered, 1, 0);
    assert_eq!(steered.len(), 2);
    // Off unless asked for; `on` is the default threshold.
    assert_eq!(tool_rounds_nudge_from_env(None), 0);
    assert_eq!(tool_rounds_nudge_from_env(Some("on")), 10);
    assert_eq!(tool_rounds_nudge_from_env(Some("12")), 12);
    assert_eq!(tool_rounds_nudge_from_env(Some("0")), 0);
    assert_eq!(tool_rounds_nudge_from_env(Some("junk")), 0);
    assert_eq!(skill_nudge_from_env(Some("yes")), 8);
    assert_eq!(skill_nudge_from_env(Some(" OFF ")), 0);
}

#[test]
fn session_core_nudge_with_subagents_running_busy_rounds_are_steered_to_wait() {
    let mut steered = Vec::new();
    let mut nudger = ToolRoundNudger::new(10, true);
    // The spawn cell and two cells of checking while six children run.
    for _ in 0..3 {
        steer(&mut nudger, &mut steered, 1, 6);
    }
    assert_eq!(steered, [wait_nudge_message(6, true, false)]);
    assert!(steered[0].contains("6 subagents or tasks you started are still running"));
    assert!(steered[0].contains("`child_done`"));
    assert!(steered[0].contains("do not check on them through their files, logs or progress"));
    assert!(steered[0].contains("`await rlm.collect(handles)`, or end your turn"));
    // Once per running phase; at the brake threshold the wait wording replaces "answer now".
    for _ in 3..10 {
        steer(&mut nudger, &mut steered, 1, 2);
    }
    assert_eq!(steered.len(), 2);
    assert_eq!(steered[1], wait_nudge_message(2, true, false));
    assert!(!steered.join("\n").contains("answer now"));
    // The last child ended: reconciling its results starts a fresh streak, so no brake at round 10 or 20.
    for _ in 0..9 {
        steer(&mut nudger, &mut steered, 1, 0);
    }
    assert_eq!(steered.len(), 2);
    steer(&mut nudger, &mut steered, 1, 0);
    assert!(steered[2].contains("10 rounds of tool calls"));
    // A new spawn phase may be nudged again.
    nudger.run_ended("run");
    for _ in 0..3 {
        steer(&mut nudger, &mut steered, 1, 1);
    }
    assert!(steered[3].contains("1 subagent or task you started is still running"));
    // A turn without tools (waiting ended the turn) breaks the waiting streak.
    let mut quiet = Vec::new();
    let mut patient = ToolRoundNudger::new(10, false);
    for (calls, running) in [(1, 3), (0, 3), (1, 3), (1, 3)] {
        quiet.extend(patient.turn_ended("r", calls, running));
    }
    assert!(quiet.is_empty());
    quiet.extend(patient.turn_ended("r", 1, 3));
    // Without completion events the only way to wait is rlm.collect.
    assert_eq!(quiet, [wait_nudge_message(3, false, false)]);
    assert!(quiet[0].contains("`await rlm.collect(handles)`"));
    assert!(!quiet[0].contains("child_done"));
    assert!(!quiet[0].contains("end your turn"));
}

#[test]
fn session_core_nudge_the_skill_hint_is_suppressed_while_subagents_run() {
    let mut steered = Vec::new();
    let mut nudger = SkillExtractionNudger::new(3);
    for _ in 0..8 {
        steered.extend(nudger.turn_ended("run", 1, 0, 4));
    }
    assert!(steered.is_empty());
    // Rounds after the children ended count from zero.
    steered.extend(nudger.turn_ended("run", 1, 0, 0));
    steered.extend(nudger.turn_ended("run", 1, 0, 0));
    assert!(steered.is_empty());
    steered.extend(nudger.turn_ended("run", 1, 0, 0));
    assert_eq!(steered.len(), 1);
    assert!(steered[0].contains("save it as a tested skill"));
    // Once per run, until the run ends; a failed round resets the streak.
    steered.extend(nudger.turn_ended("run", 1, 0, 0));
    assert_eq!(steered.len(), 1);
    nudger.run_ended("run");
    steered.extend(nudger.turn_ended("run", 1, 0, 0));
    steered.extend(nudger.turn_ended("run", 1, 1, 0));
    steered.extend(nudger.turn_ended("run", 1, 0, 0));
    steered.extend(nudger.turn_ended("run", 1, 0, 0));
    assert_eq!(steered.len(), 1);
}

fn shot(id: &str, images: usize) -> Message {
    let mut content = vec![UserContent::Text(ai::TextContent {
        text: format!("cell {id}"),
        text_signature: None,
    })];
    content.extend((0..images).map(|index| {
        UserContent::Image(ai::ImageContent {
            data: format!("{id}-{index}"),
            mime_type: "image/png".into(),
        })
    }));
    Message::ToolResult(ToolResultMessage {
        tool_call_id: id.into(),
        tool_name: "rlm".into(),
        content,
        details: None,
        usage: None,
        is_error: false,
        timestamp: 1,
    })
}

fn images(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .flat_map(|m| match m {
            Message::ToolResult(result) => result
                .content
                .iter()
                .filter_map(|c| match c {
                    UserContent::Image(image) => Some(image.data.clone()),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

#[test]
fn session_core_image_budget_drops_in_batches_of_half_the_budget() {
    let kept: Vec<usize> = [1, 8, 9, 11, 12, 15, 16, 20]
        .into_iter()
        .map(|total| total - images_to_drop(total, 8))
        .collect();
    assert_eq!(kept, [1, 8, 5, 7, 4, 7, 4, 4]);
    assert_eq!(images_to_drop(100, 0), 0);
}

#[test]
fn session_core_image_budget_the_oldest_images_become_a_note() {
    let messages = [shot("a", 3), shot("b", 3), shot("c", 3)];
    let out = with_image_budget(&messages, 8).unwrap();
    assert_eq!(images(&out), ["b-1", "b-2", "c-0", "c-1", "c-2"]);
    assert_eq!(
        serde_json::to_value(&out[0]).unwrap()["content"],
        json!([
            {"type": "text", "text": "cell a"},
            {"type": "text", "text": OFFLOADED_IMAGE_TEXT},
            {"type": "text", "text": OFFLOADED_IMAGE_TEXT},
            {"type": "text", "text": OFFLOADED_IMAGE_TEXT},
        ])
    );
    assert_eq!(images(&messages).len(), 9);
    assert!(with_image_budget(&[shot("a", 8)], 8).is_none());
}

#[test]
fn session_core_image_budget_reads_ultron_context_images() {
    assert_eq!(context_image_budget(None), 8);
    assert_eq!(context_image_budget(Some("0")), 0);
    assert_eq!(context_image_budget(Some("x")), 8);
    assert_eq!(context_image_budget(Some(" 3 ")), 3);
    assert_eq!(context_image_budget(Some("-1")), 8);
    assert_eq!(context_image_budget(Some("2.5")), 8);
}
