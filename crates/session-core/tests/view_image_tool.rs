//! Ported from packages/coding-agent/test/ultron-view-image.test.ts ("view_image in the rlm tool"): an image the
//! kernel names reaches the model as an image block on the `rlm` tool result, never as printed base64; bytes and
//! figure-like objects attach too; the type comes from magic bytes; at most 8 per cell; oversized images are
//! downscaled and `detail='low'` caps the long side at 512; a model without image input gets the note. The stack is
//! the session's composition (`Runtime::stack`), the tool the lane's `rlm`.

mod p_session_support;

use agent::AgentTool;
use ai::{AbortSignal, UserContent};
use base64::Engine;
use serde_json::json;
use session_core::RunEnv;
use session_core::compose::{Runtime, Services, Stack};
use std::path::Path;
use std::sync::Arc;

struct Fixture {
    stack: Arc<Stack>,
    cwd: std::path::PathBuf,
    _tmp: tempfile::TempDir,
}

fn fixture(vision: bool) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let home = root.join("home");
    let agent_dir = home.join(".ultron/agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("settings.json"),
        json!({"sandbox": {"mode": "off"}}).to_string(),
    )
    .unwrap();
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(cwd.join("dot.png"), p_session_support::png(4, 3)).unwrap();
    let vars: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        ("ULTRON_CODING_AGENT_DIR".into(), agent_dir.display().to_string()),
        ("ULTRON_SERVER_DIR".into(), root.join("server").display().to_string()),
        ("ULTRON_HINDSIGHT_URL".into(), "off".into()),
    ];
    let env = RunEnv {
        cwd: cwd.clone(),
        home,
        var: Arc::new(move |key| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())),
        docs: config::paths::normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/coding-agent/docs")),
    };
    let runtime = Runtime::new(&env, "print", root.join("sessions"), Services::default()).unwrap();
    let mut model = ai::models::builtin_model("anthropic", "claude-sonnet-4-5")
        .unwrap()
        .clone();
    model.provider = "test".into();
    model.id = if vision { "vision" } else { "text-only" }.into();
    model.input = if vision {
        vec!["text".into(), "image".into()]
    } else {
        vec!["text".into()]
    };
    let stack = runtime.stack(&model, None, "view-image").unwrap();
    Fixture { stack, cwd, _tmp: tmp }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stack.close();
    }
}

fn run(f: &Fixture, code: &str) -> Vec<UserContent> {
    let tool = f.stack.rlm("main").unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let update = |_| {};
    runtime
        .block_on(tool.execute("call", json!({"code": code}), &AbortSignal::new(), &update))
        .unwrap()
        .content
}

fn text(content: &[UserContent]) -> String {
    match &content[0] {
        UserContent::Text(text) => text.text.clone(),
        other => panic!("not text: {other:?}"),
    }
}

/// `(mime type, width, height)` of each image block.
fn images(content: &[UserContent]) -> Vec<(String, u32, u32)> {
    content
        .iter()
        .filter_map(|c| match c {
            UserContent::Image(image) => {
                let bytes = base64::engine::general_purpose::STANDARD.decode(&image.data).unwrap();
                let size = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap());
                let (w, h) = if image.mime_type == "image/png" {
                    (size(16), size(20))
                } else {
                    (0, 0)
                };
                Some((image.mime_type.clone(), w, h))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn session_core_view_image_a_path_attaches_an_image_block_after_the_text() {
    let f = fixture(true);
    let content = run(&f, "print(await view_image('dot.png'))");
    assert_eq!(content.len(), 2);
    let shown = text(&content);
    assert!(
        shown.starts_with("image 1: 4x3 PNG, ") && shown.ends_with(" B"),
        "{shown}"
    );
    assert_eq!(images(&content), [("image/png".to_string(), 4, 3)]);
    let UserContent::Image(image) = &content[1] else {
        panic!()
    };
    assert!(!shown.contains(&image.data[..16]));
    // A cell without view_image keeps the plain text result.
    let plain = run(&f, "1 + 1");
    assert_eq!(plain.len(), 1);
    assert_eq!(text(&plain), "2");
}

#[test]
fn session_core_view_image_raw_bytes_attach_and_the_type_comes_from_magic_bytes() {
    let f = fixture(true);
    std::fs::write(f.cwd.join("photo.jpg"), p_session_support::png(5, 5)).unwrap();
    std::fs::write(f.cwd.join("fake.png"), "not an image at all").unwrap();
    let content = run(
        &f,
        &[
            "print(await view_image(open('dot.png', 'rb').read()))",
            "from pathlib import Path",
            "print(await view_image(Path('photo.jpg')))",
            "try:\n    await view_image('fake.png')\nexcept ValueError as error:\n    print('refused:', error)",
            "try:\n    await view_image(b'GIF8 but not really')\nexcept ValueError as error:\n    print('refused:', error)",
            "try:\n    await view_image(42)\nexcept TypeError as error:\n    print('refused:', error)",
        ]
        .join("\n"),
    );
    let shown = text(&content);
    assert!(shown.contains("image 1: 4x3 PNG"), "{shown}");
    assert!(shown.contains("image 2: 5x5 PNG"), "{shown}");
    assert!(shown.contains("refused: fake.png is not a png, jpeg, gif or webp image"));
    assert!(shown.contains("refused: view_image bytes are not a png, jpeg, gif or webp image"));
    assert!(shown.contains("refused: view_image takes a path, image bytes"));
    assert_eq!(
        images(&content).iter().map(|i| i.0.as_str()).collect::<Vec<_>>(),
        ["image/png", "image/png"]
    );
}

#[test]
fn session_core_view_image_an_object_with_savefig_is_rendered_to_png() {
    let f = fixture(true);
    let content = run(
        &f,
        &[
            "class Figure:",
            "    def savefig(self, buffer, format=None, bbox_inches=None):",
            "        assert format == 'png'",
            "        buffer.write(open('dot.png', 'rb').read())",
            "await view_image(Figure())",
        ]
        .join("\n"),
    );
    assert!(text(&content).contains("image 1: 4x3 PNG"));
    assert_eq!(images(&content).len(), 1);
}

#[test]
fn session_core_view_image_a_cell_attaches_at_most_8_images() {
    let f = fixture(true);
    let content = run(
        &f,
        "for i in range(9):\n    try:\n        await view_image('dot.png')\n    except RuntimeError as error:\n        print(i, error)",
    );
    assert_eq!(images(&content).len(), 8);
    assert!(
        text(&content).contains("8 view_image: at most 8 images per cell"),
        "{}",
        text(&content)
    );
    // The cap is per cell.
    assert_eq!(images(&run(&f, "await view_image('dot.png')")).len(), 1);
}

#[test]
fn session_core_view_image_an_oversized_image_is_downscaled_and_detail_low_is_a_small_preview() {
    let f = fixture(true);
    std::fs::write(f.cwd.join("big.png"), p_session_support::png(3000, 2400)).unwrap();
    let content = run(
        &f,
        "print(await view_image('big.png'))\nprint(await view_image('big.png', detail='low'))",
    );
    assert!(
        text(&content).contains("(downscaled from 3000x2400)"),
        "{}",
        text(&content)
    );
    assert_eq!(images(&content).len(), 2);
    // The shown sizes, as each description reports them (`image N: WxH ...`).
    let sizes: Vec<u32> = text(&content)
        .lines()
        .filter_map(|line| {
            line.strip_prefix("image ")?
                .split_once(": ")?
                .1
                .split_once(' ')?
                .0
                .split_once('x')
                .map(|(w, h)| w.parse::<u32>().unwrap().max(h.parse().unwrap()))
        })
        .collect();
    assert_eq!(sizes.len(), 2, "{}", text(&content));
    assert!(sizes[0] <= 2000, "{sizes:?}");
    assert!(sizes[1] <= 512, "{sizes:?}");
}

#[test]
fn session_core_view_image_a_model_without_image_input_gets_a_plain_note() {
    let f = fixture(false);
    let content = run(&f, "print(await view_image('dot.png'))");
    let shown = text(&content);
    assert!(shown.contains("image 1: 4x3 PNG"));
    assert!(
        shown.contains("the current model (test/text-only) does not accept image input"),
        "{shown}"
    );
    // The block is still kept so the transcript shows it and a vision model can use it.
    assert_eq!(images(&content).len(), 1);
}

#[test]
fn session_core_view_image_read_on_an_image_file_points_at_view_image() {
    let f = fixture(true);
    let content = run(&f, "await read('dot.png')");
    let shown = text(&content);
    assert!(shown.contains("dot.png is a PNG image"), "{shown}");
    assert!(shown.contains("Use `await view_image('dot.png')`"));
    assert!(images(&content).is_empty());
}
