//! The render settings the transcript reads: replays fixtures/render-settings.json (AssistantMessageComponent with
//! `getMarkdownThemeWithSettings()`'s `codeBlockIndent`, and UserMessageComponent, under
//! `getCapabilities().hyperlinks`, recorded by fixtures/generate-render-settings.mjs) through
//! `interactive::render_settings` and the Rust components.

use std::rc::Rc;

use interactive::render_settings::{self, RenderSettings};
use interactive::{AssistantBlock, AssistantMessage, AssistantView, UserMessage};
use serde_json::Value;
use tui::components::Component;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

#[test]
fn interactive_render_settings_reach_the_transcript_markdown_like_ts() {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/render-settings.json")).unwrap();
    let text = fixture["text"].as_str().unwrap().to_string();
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    for case in fixture["cases"].as_array().unwrap() {
        render_settings::set(RenderSettings {
            hyperlinks: case["hyperlinks"] == true,
            code_block_indent: case["codeBlockIndent"].as_str().map(str::to_string),
        });
        let width = case["width"].as_u64().unwrap() as usize;
        let lines = match case["kind"].as_str().unwrap() {
            "assistant" => AssistantMessage {
                theme: Rc::clone(&theme),
                message: AssistantView {
                    content: vec![AssistantBlock::Text(text.clone())],
                    stop_reason: "stop".into(),
                    error_message: None,
                },
                hide_thinking: false,
                hidden_thinking_label: "Thinking...".into(),
                output_pad: 1,
                thinking_overrides: Vec::new(),
                code_highlight: None,
            }
            .render(width),
            _ => UserMessage {
                theme: Rc::clone(&theme),
                text: text.clone(),
                output_pad: 1,
            }
            .render(width),
        };
        let want: Vec<String> = case["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            lines, want,
            "{} indent {:?} hyperlinks {} at {width}",
            case["kind"], case["codeBlockIndent"], case["hyperlinks"]
        );
    }
    render_settings::set(RenderSettings::default());
}
