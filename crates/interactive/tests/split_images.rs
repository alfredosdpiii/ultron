//! Tool result images in the split viewport (the default tuiMode: finished transcript in the terminal's scrollback):
//! on Kitty the image is drawn into scrollback as TypeScript's main screen draws it (its rows made first, then the
//! image placed on its first row), and a display reset (a replay of scrollback) deletes it before clearing. The
//! renderer's image rules are ported in crates/tui/tests/split_kitty_images.rs.

use std::rc::Rc;

use interactive::app::{App, AppOptions};
use interactive::history::PromptHistoryStore;
use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
use interactive::qol::QolSettings;
use interactive::run::{UiEvent, paint_loop};
use serde_json::{Value, json};
use tui::image::ImageProtocol;
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};
use tui::viewport::ViewportRenderer;

struct Quiet;

impl LaneClient for Quiet {
    fn prompt(&self, _: &str) {}
    fn steer(&self, _: &str) -> String {
        String::new()
    }
    fn follow_up(&self, _: &str) -> String {
        String::new()
    }
    fn cancel_queued(&self, _: &str) {}
    fn abort(&self) {}
    fn new_session(&self) {}
    fn resume(&self, _: &str) {}
    fn list_sessions(&self) {}
    fn set_name(&self, _: &str) {}
    fn select_model(&self, _: &str, _: &str) {}
}

fn png() -> Value {
    let fixture: Value = serde_json::from_str(include_str!("../fixtures/tool-images.json")).unwrap();
    fixture["cases"][0]["content"][1]["data"].clone()
}

fn app() -> App {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let keys = || {
        KeybindingsManager::new(
            native_app_keybindings(Platform::new("linux", false)),
            Vec::new(),
            Default::default(),
        )
    };
    let model = ModelInfo {
        provider: "anthropic".into(),
        id: "m".into(),
        name: "M".into(),
        reasoning: false,
        context_window: 1000,
        thinking_levels: Vec::new(),
    };
    let qol = QolSettings::default();
    assert!(qol.viewport_split);
    let mut app = App::new(
        AppOptions {
            theme,
            keys: keys(),
            editor_keys: keys(),
            version: "1.2.3".into(),
            cwd: "/p".into(),
            home: None,
            server_id: "server".into(),
            models: vec![model.clone()],
            model,
            thinking_level: "medium".into(),
            splash: false,
            history: PromptHistoryStore::new(None, "/p"),
            fd_path: None,
            highlight: Rc::new(|code: &str| code.split('\n').map(str::to_string).collect()),
            qol,
        },
        Box::new(Quiet),
    );
    app.apply(LaneEvent::Opened(SessionOpened {
        session_id: "sess".into(),
        session_file: None,
        name: None,
        messages: vec![
            json!({"role": "assistant", "content": [{"type": "toolCall", "id": "c1", "name": "view", "arguments": {}}],
                "stopReason": "toolUse", "usage": {}}),
            json!({"role": "toolResult", "toolCallId": "c1", "toolName": "view", "isError": false,
                "content": [{"type": "image", "data": png(), "mimeType": "image/png"}]}),
        ],
    }));
    app
}

/// The Kitty id of the first image transmission in `bytes`.
fn image_id(bytes: &str) -> Option<String> {
    let at = bytes.find("\x1b_Ga=T")?;
    let controls = &bytes[at..at + bytes[at..].find(';')?];
    controls
        .split(',')
        .find_map(|c| c.strip_prefix("i="))
        .map(str::to_string)
}

#[test]
fn interactive_split_viewport_draws_tool_images_into_scrollback_on_kitty() {
    let mut app = app();
    app.set_image_protocol(Some(ImageProtocol::Kitty), false);
    let mut renderer = ViewportRenderer::new();
    let plan = app.frame_plan(80, 24);
    let rows = plan
        .history
        .as_ref()
        .expect("the finished transcript goes to scrollback")
        .rows
        .clone();
    let at = rows
        .iter()
        .position(|l| l.contains("\x1b_Ga=T"))
        .expect("the image is in scrollback");
    let frame = renderer.render(&plan, 80, 24);
    assert!(image_id(&frame.bytes).is_some());
    // Its rows are made before it is placed on the first of them.
    let image = &rows[at];
    let reserved = 1 + rows[at + 1..].iter().take_while(|l| l.is_empty()).count();
    assert!(reserved > 1, "{rows:#?}");
    let up = reserved - 1;
    assert!(
        frame
            .bytes
            .contains(&format!("{}\x1b[{up}A{image}\x1b[{up}B", "\r\n".repeat(up))),
        "{:?}",
        frame.bytes
    );
    // Pi's Alt+L in the paint loop: the screen and scrollback are cleared and painted again; the image written to
    // scrollback is deleted before the clear.
    let mut app = self::app();
    app.set_image_protocol(Some(ImageProtocol::Kitty), false);
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(UiEvent::Input("\x1bl".into())).unwrap();
    tx.send(UiEvent::InputClosed).unwrap();
    let mut writes = Vec::new();
    paint_loop(
        &mut app,
        &rx,
        &mut |d| {
            writes.push(d.to_string());
            Ok(())
        },
        &|| (80, 24),
        None,
    )
    .unwrap();
    let painted = writes
        .iter()
        .position(|w| w.contains("\x1b_Ga=T"))
        .expect("the image is drawn");
    let id = image_id(&writes[painted]).unwrap();
    assert!(
        writes.contains(&format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\\x1b[2J\x1b[3J\x1b[H")),
        "{writes:?}"
    );
}

/// Without an image protocol the split viewport names the image, as before.
#[test]
fn interactive_split_viewport_without_images_names_the_image() {
    let mut app = app();
    let plan = app.frame_plan(80, 24);
    let rows = plan.history.unwrap().rows.join("\n");
    assert!(!rows.contains("\x1b_G"), "{rows}");
    assert!(rows.contains("[Image: [image/png]"), "{rows}");
}
