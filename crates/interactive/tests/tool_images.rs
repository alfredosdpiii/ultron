//! Tool result images drawn inline on Kitty: replays fixtures/tool-images.json, recorded from the TypeScript
//! `ToolExecutionComponent` (rlm renderers for `rlm`, none otherwise) by fixtures/generate-tool-images.mjs, through the
//! Rust chat (`ChatView` with Kitty image settings). Kitty image ids are random on both sides and compared as `i=ID`.
//! Images TypeScript converts to PNG (JPEG, WebP) are compared before the conversion exactly and after it by the
//! image controls and the decoded pixels (the two PNG encoders write different bytes for the same pixels). Plus the
//! App: the settings toggle offered only when images are drawn, and the terminal's cell size answer.

use std::rc::Rc;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use interactive::chat::{ChatStyle, ChatView, ImageSettings};
use interactive::key_text;
use serde_json::{Value, json};
use tui::image::{DEFAULT_CELL, ImageProtocol};
use tui::keybindings::{KeybindingsManager, Platform, app_keybindings};
use tui::keys::KeyContext;
use tui::theme::{ColorMode, builtin_theme_json, create_theme};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/tool-images.json")).unwrap()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|l| l.as_str().unwrap().to_string())
        .collect()
}

fn chat(case: &Value) -> ChatView {
    let theme = Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap());
    let keys = KeybindingsManager::new(
        app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        KeyContext::default(),
    );
    let hl = case["hl"].clone();
    let mut chat = ChatView::new(ChatStyle {
        theme,
        highlight: Rc::new(move |code: &str| {
            assert_eq!(code, hl["input"].as_str().unwrap(), "highlighter input");
            strings(&hl["lines"])
        }),
        code_highlight: None,
        expand_key: key_text(&keys, "app.tools.expand", false),
        dequeue_key: String::new(),
        interrupt_key: String::new(),
    });
    chat.set_image_settings(ImageSettings {
        protocol: Some(ImageProtocol::Kitty),
        show: true,
        width_cells: 60,
        cell: DEFAULT_CELL,
    });
    let name = case["tool"].as_str().unwrap();
    chat.add_message(
        &json!({"role": "assistant", "content": [{"type": "toolCall", "id": "c1", "name": name,
        "arguments": case["args"]}], "stopReason": "toolUse", "usage": {}}),
    );
    chat.add_message(&json!({"role": "toolResult", "toolCallId": "c1", "toolName": name,
        "content": case["content"], "isError": false}));
    chat
}

/// `i=<digits>` as `i=ID`.
fn ids(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find("i=") {
        out.push_str(&rest[..at + 2]);
        let after = &rest[at + 2..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        out.push_str(if digits > 0 { "ID" } else { "" });
        rest = &after[digits..];
    }
    out.push_str(rest);
    out
}

/// A Kitty line's first controls (ids normalized) and its whole payload; other lines unchanged.
fn kitty_parts(line: &str) -> (String, Option<String>) {
    let Some(start) = line.find("\x1b_G") else {
        return (line.to_string(), None);
    };
    let mut payload = String::new();
    let mut first = None;
    let mut rest = &line[start..];
    while let Some(body) = rest.strip_prefix("\x1b_G") {
        let semi = body.find(';').unwrap();
        let end = body.find("\x1b\\").unwrap();
        first.get_or_insert_with(|| body[..semi].to_string());
        payload.push_str(&body[semi + 1..end]);
        rest = &body[end + 2..];
    }
    (
        format!("{}KITTY[{}]{rest}", &line[..start], ids(&first.unwrap())),
        Some(payload),
    )
}

fn pixels(b64: &str) -> (u32, u32, Vec<u8>) {
    let image = image::load_from_memory(&B64.decode(b64).unwrap()).unwrap().to_rgba8();
    (image.width(), image.height(), image.into_raw())
}

/// The tool item's rows (the assistant message of a lone tool call renders no rows).
fn tool_rows(chat: &mut ChatView, width: usize) -> Vec<String> {
    chat.render_item_rows(width)
        .into_iter()
        .rfind(|rows| !rows.is_empty())
        .unwrap()
}

fn wait_conversions(chat: &mut ChatView) {
    let deadline = Instant::now() + Duration::from_secs(120);
    while !chat.poll_images() {
        assert!(Instant::now() < deadline, "the PNG conversion never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn interactive_tool_images_draw_inline_on_kitty_like_ts() {
    let fixture = fixture();
    for case in fixture["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let converted = case["converted"].as_array().unwrap();
        for (i, width) in case["widths"].as_array().unwrap().iter().enumerate() {
            let width = width.as_u64().unwrap() as usize;
            let mut chat = chat(case);
            let norm = |rows: &[String]| rows.iter().map(|l| ids(l)).collect::<Vec<_>>();
            let before = tool_rows(&mut chat, width);
            assert_eq!(
                norm(&before),
                norm(&strings(&case["before"][i])),
                "{name} at {width}: before conversion"
            );
            if converted.is_empty() {
                assert_eq!(before, tool_rows(&mut chat, width));
                continue;
            }
            wait_conversions(&mut chat);
            let after = tool_rows(&mut chat, width);
            let want = strings(&case["after"][i]);
            let (got_rows, got_payloads): (Vec<_>, Vec<_>) = after.iter().map(|l| kitty_parts(l)).unzip();
            let (want_rows, _): (Vec<_>, Vec<_>) = want.iter().map(|l| kitty_parts(l)).unzip();
            assert_eq!(got_rows, want_rows, "{name} at {width}: after conversion");
            let got: Vec<String> = got_payloads.into_iter().flatten().collect();
            let want: Vec<&str> = converted.iter().filter_map(Value::as_str).collect();
            assert_eq!(got.len(), want.len(), "{name}: converted images drawn");
            // JPEG decoders round their inverse DCT differently (TypeScript's photon uses jpeg-decoder, this build
            // zune-jpeg): a channel may differ by one or two; lossless sources decode exactly.
            let tolerance = if name.contains("jpeg") { 2 } else { 0 };
            for (got, want) in got.iter().zip(want) {
                let (got, want) = (pixels(got), pixels(want));
                assert_eq!((got.0, got.1), (want.0, want.1), "{name}: converted size");
                let worst = got.2.iter().zip(&want.2).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
                assert!(worst <= tolerance, "{name}: converted pixels differ by {worst}");
            }
        }
    }
}

/// Without Kitty images (none, or images turned off) the result names its images in the text, as before.
#[test]
fn interactive_tool_images_without_protocol_name_the_image() {
    let fixture = fixture();
    let case = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "rlm-png")
        .unwrap();
    let mut chat = chat(case);
    chat.set_image_settings(ImageSettings::default());
    let rows = tool_rows(&mut chat, 80).join("\n");
    assert!(
        rows.contains("[Image: [image/png] 6x4]") && !rows.contains("\x1b_G"),
        "{rows}"
    );
    chat.set_image_settings(ImageSettings {
        protocol: Some(ImageProtocol::Kitty),
        show: false,
        ..ImageSettings::default()
    });
    let rows = tool_rows(&mut chat, 80).join("\n");
    assert!(
        rows.contains("[Image: [image/png] 6x4]") && !rows.contains("\x1b_G"),
        "{rows}"
    );
    // The image keeps its id across renders and setting changes.
    chat.set_image_settings(ImageSettings {
        protocol: Some(ImageProtocol::Kitty),
        ..ImageSettings::default()
    });
    let first = tool_rows(&mut chat, 80);
    chat.set_image_settings(ImageSettings {
        protocol: Some(ImageProtocol::Kitty),
        width_cells: 30,
        ..ImageSettings::default()
    });
    let second = tool_rows(&mut chat, 80);
    let id = |rows: &[String]| {
        let line = rows.iter().find(|l| l.contains("\x1b_G")).unwrap();
        line.split(",i=").nth(1).unwrap().split(';').next().unwrap().to_string()
    };
    assert_eq!(id(&first), id(&second));
    assert!(second.iter().any(|l| l.contains(",c=30,")));
}

#[test]
fn interactive_tool_images_cell_size_answer_like_consume_cell_size_response() {
    use interactive::app::parse_cell_size_response;
    assert_eq!(parse_cell_size_response("\x1b[6;20;10t"), Some((20, 10)));
    assert_eq!(parse_cell_size_response("\x1b[6;0;10t"), Some((0, 10)));
    assert_eq!(parse_cell_size_response("\x1b[6;20t"), None);
    assert_eq!(parse_cell_size_response("\x1b[6;a;10t"), None);
    assert_eq!(parse_cell_size_response("x\x1b[6;20;10t"), None);
}

mod app {
    use interactive::app::{App, AppOptions};
    use interactive::history::PromptHistoryStore;
    use interactive::lane::{LaneClient, LaneEvent, ModelInfo, SessionOpened};
    use serde_json::json;
    use std::rc::Rc;
    use tui::image::ImageProtocol;
    use tui::keybindings::{KeybindingsManager, Platform};
    use tui::qol_keys::native_app_keybindings;
    use tui::theme::{ColorMode, builtin_theme_json, create_theme};

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
                qol: interactive::qol::QolSettings::parity(),
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
                    "content": [{"type": "image", "data": super::fixture()["cases"][0]["content"][1]["data"], "mimeType": "image/png"}]}),
            ],
        }));
        app
    }

    fn settings_screen(app: &mut App) -> String {
        app.apply(LaneEvent::Settings {
            settings: json!({}),
            env_overrides: Default::default(),
            themes: vec!["dark".into()],
            terminal_theme: "dark".into(),
        });
        let screen = app
            .render(100, 40)
            .iter()
            .map(|l| tui::utils::strip_terminal_sequences(l))
            .collect::<Vec<_>>()
            .join("\n");
        app.handle_input("\x1b");
        screen
    }

    /// `supportsImages = getCapabilities().images`: the image toggles are offered only where images are drawn; the
    /// TUI asks for the cell size then (`queryCellSize`) and its answer re-renders the images at that size.
    #[test]
    fn interactive_tool_images_app_offers_the_toggle_and_reads_the_cell_size() {
        let mut plain = app();
        let screen = settings_screen(&mut plain);
        assert!(
            !screen.contains("Show images") && !screen.contains("Image width"),
            "{screen}"
        );
        assert!(plain.terminal_writes.iter().all(|w| !w.contains("\x1b[16t")));
        assert!(plain.render(100, 40).iter().all(|l| !l.contains("\x1b_G")));

        let mut kitty = app();
        kitty.set_image_protocol(Some(ImageProtocol::Kitty), false);
        assert!(kitty.terminal_writes.iter().any(|w| w == "\x1b[16t"));
        let screen = settings_screen(&mut kitty);
        assert!(
            screen.contains("Show images") && screen.contains("Image width"),
            "{screen}"
        );
        let image = |app: &mut App| {
            app.render(100, 60)
                .into_iter()
                .find(|l| l.contains("\x1b_G"))
                .expect("the image is drawn")
        };
        // 6x4 px at 60 columns of 9x18 px cells: 20 rows; at 10x10 px cells: 40 rows.
        assert!(image(&mut kitty).contains(",c=60,r=20,"));
        kitty.handle_input("\x1b[6;10;10t");
        assert!(image(&mut kitty).contains(",c=60,r=40,"));
        // A zero size is consumed and changes nothing.
        kitty.handle_input("\x1b[6;0;10t");
        assert!(image(&mut kitty).contains(",c=60,r=40,"));
        assert_eq!(kitty.editor_text(), "");
    }
}
