//! A shown `diagram.mermaid` (kernel_apis::diagram in the engine): the PNG reaches the TUI in the `rlm` result's
//! `details.displayImages`, never in the content the model saw, and the chat draws it inline after the result's own
//! content like any tool image. New in Rust: literal expectations.

use std::rc::Rc;

use interactive::chat::{ChatStyle, ChatView, ImageSettings};
use serde_json::{Value, json};
use tui::image::{DEFAULT_CELL, ImageProtocol};
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

fn png() -> Value {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/tool-images.json")).unwrap();
    fixture["cases"][0]["content"][1]["data"].clone()
}

/// The rows of an `rlm` call whose result is `content` with `details`.
fn rows(content: Value, details: Value) -> Vec<String> {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let mut chat = ChatView::new(ChatStyle {
        theme,
        highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
        code_highlight: None,
        expand_key: String::new(),
        dequeue_key: String::new(),
        interrupt_key: String::new(),
    });
    chat.set_image_settings(ImageSettings {
        protocol: Some(ImageProtocol::Kitty),
        show: true,
        width_cells: 60,
        cell: DEFAULT_CELL,
    });
    chat.add_message(
        &json!({"role": "assistant", "content": [{"type": "toolCall", "id": "c1", "name": "rlm",
        "arguments": {"code": "await diagram.mermaid(src)"}}], "stopReason": "toolUse", "usage": {}}),
    );
    chat.add_message(
        &json!({"role": "toolResult", "toolCallId": "c1", "toolName": "rlm", "content": content,
        "details": details, "isError": false}),
    );
    chat.render_item_rows(80)
        .into_iter()
        .rfind(|rows| !rows.is_empty())
        .unwrap()
}

#[test]
fn interactive_diagram_display_images_are_drawn_inline_from_details() {
    let line = json!([{"type": "text", "text": "diagram: /tmp/d/flowchart-1.png (240x120)"}]);
    let shown = rows(
        line.clone(),
        json!({"displayImages": [{"type": "image", "data": png(), "mimeType": "image/png"}]}),
    );
    let images = shown.iter().filter(|l| l.contains("\x1b_Ga=T")).count();
    assert_eq!(images, 1, "{shown:#?}");
    // It comes after the result's text.
    let text = shown
        .iter()
        .position(|l| l.contains("diagram: /tmp/d/flowchart-1.png"))
        .unwrap();
    let image = shown.iter().position(|l| l.contains("\x1b_Ga=T")).unwrap();
    assert!(text < image, "{shown:#?}");
    // Without displayImages (or with other details) nothing is drawn.
    for details in [json!({}), json!({"fileChanges": []}), Value::Null] {
        let plain = rows(line.clone(), details);
        assert!(plain.iter().all(|l| !l.contains("\x1b_G")), "{plain:#?}");
    }
}
