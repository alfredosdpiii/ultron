//! Attachment chips (ported from omp packages/tui/test/attachment-chips.test.ts), image placeholders, draft recall,
//! paste routing and the off-thread clipboard service.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tui::attachments::*;
use tui::clipboard::*;
use tui::utils::{strip_terminal_sequences, visible_width};

/// 2x2 red PNG with a real header.
const TINY_PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAYAAABytg0kAAAAEklEQVR4nGP8z8DwnwEKmBhQAAA9+AQBHYLp7wAAAABJRU5ErkJggg==";

fn plain(rows: &[String]) -> Vec<String> {
    rows.iter().map(|r| strip_terminal_sequences(r)).collect()
}

#[test]
fn tui_attachments_chip_band_renders_paste_and_image_cards() {
    let none = Attachments::default();
    assert_eq!(chip_band(&draft_chips("", &[], &none), 80), Vec::<String>::new());

    let pastes = vec![(1u64, "line1\nline2\nline3".to_string())];
    let rows = chip_band(&draft_chips("[paste #1 +3 lines]", &pastes, &none), 80);
    let p = plain(&rows);
    assert_eq!(p.len(), 6);
    assert_eq!(p[0], "╭── 📄 #1 ───╮");
    assert_eq!(p[1], "│line1       │");
    assert_eq!(p[2], "│line2       │");
    assert_eq!(p[3], "│line3       │");
    assert_eq!(p[4], "│            │");
    assert_eq!(p[5], "╰─ +3 lines ─╯");
    assert!(rows.iter().all(|r| visible_width(r) == 14));
    assert!(
        rows[0].starts_with("\x1b[38;2;105;220;158m"),
        "paste #1 takes palette slot 3: {:?}",
        rows[0]
    );

    let pastes = vec![(1u64, "x".repeat(42))];
    let p = plain(&chip_band(&draft_chips("[paste #1 42 chars]", &pastes, &none), 80));
    assert_eq!(p[5], "╰─ 42 chars ─╯");
    assert_eq!(p[1], "│xxxxxxxxxxx…│");

    // the card disappears once its token leaves the buffer
    assert!(draft_chips("no token here", &pastes, &none).is_empty());

    let mut att = Attachments::default();
    let token = att.add_image(TINY_PNG.to_string(), "image/png", None);
    assert_eq!(token, "[Image #1, 2x2]");
    let pastes = vec![(1u64, "hello".to_string())];
    let text = format!("{token} and [paste #1 5 chars]");
    let rows = chip_band(&draft_chips(&text, &pastes, &att), 80);
    let p = plain(&rows);
    assert_eq!(p[0], "╭─── 🖼 #1 ───╮  ╭── 📄 #1 ───╮");
    assert_eq!(p[2], "│     🖼      │  │            │");
    assert!(rows.iter().all(|r| visible_width(r) == 30));
    assert_eq!(p[5], "╰─── 2x2 ────╯  ╰─ 5 chars ──╯");
    assert!(
        rows[0].starts_with("\x1b[38;2;255;179;102m"),
        "image #1 takes palette slot 0"
    );

    // cards that do not fit are omitted, not wrapped: 14 cols + 2 gap
    let pastes = vec![(1u64, "first".to_string()), (2, "second".to_string())];
    let both = "[paste #1 5 chars][paste #2 6 chars]";
    let p = plain(&chip_band(&draft_chips(both, &pastes, &none), 20));
    assert!(p[0].contains("📄 #1") && !p[0].contains("📄 #2"));
    let p = plain(&chip_band(&draft_chips(both, &pastes, &none), 30));
    assert!(p[0].contains("📄 #2"));
}

#[test]
fn tui_attachments_video_and_files() {
    let dir = std::env::temp_dir().join(format!("tui-attach-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("shot.png");
    use base64::Engine;
    std::fs::write(
        &png,
        base64::engine::general_purpose::STANDARD.decode(TINY_PNG).unwrap(),
    )
    .unwrap();
    let mut att = Attachments::default();
    let token = att.add_image_file(png.to_str().unwrap()).unwrap();
    assert_eq!(token, "[Image #1, 2x2]");
    assert_eq!(att.images[0].path.as_deref(), png.to_str());
    assert_eq!(att.images[0].mime, "image/png");
    assert!(att.add_image_file("/nonexistent/x.png").is_err());
    assert!(att.add_image_file(dir.join("notes.txt").to_str().unwrap()).is_err());
    let token = att.add_image("AAAA".into(), "image/jpeg", Some("/tmp/launch.mp4".into()));
    assert_eq!(token, "[Video #2]", "undecodable header: no size");
    let p = plain(&chip_band(&draft_chips(&token, &[], &att), 80));
    assert_eq!(p[0], "╭─── 🎞 #2 ───╮");
    assert_eq!(p[5], "╰────────────╯");
    assert_eq!(att.referenced("[Image #1, 2x2] [Video #2]").len(), 2);
    att.clear();
    assert!(att.images.is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(
        placeholder_spans("a [Image #1, 2x2] b [Video #2] [Image #0]"),
        vec![(2, 17), (20, 30)]
    );
}

#[test]
fn tui_attachments_paste_routing() {
    assert_eq!(
        classify_paste("/tmp/a.png ./b.jpg"),
        PasteAction::Attach(vec!["/tmp/a.png".into(), "./b.jpg".into()])
    );
    assert_eq!(
        classify_paste("/tmp/report.csv"),
        PasteAction::Text("/tmp/report.csv".into())
    );
    assert_eq!(
        classify_paste("line1\x1b[106;5uline2"),
        PasteAction::Text("line1\nline2".into())
    );
    assert_eq!(RAW_PASTE_KEY, "ctrl+shift+v");
    assert!(tui::keys::matches_key(
        "\x1b[118;6u",
        RAW_PASTE_KEY,
        tui::keys::KeyContext::default()
    ));
}

#[test]
fn tui_attachments_draft_recall() {
    let mut r = DraftRecall::default();
    let mut att = Attachments::default();
    att.add_image(TINY_PNG.into(), "image/png", None);
    r.remember("   ", &att);
    assert!(!r.has_draft(), "a blank draft is not kept");
    r.remember("fix the [Image #1, 2x2] bug", &att);
    assert!(r.has_draft());
    assert_eq!(r.recall("typed"), None, "Up in a non-empty editor is cursor movement");
    let (text, restored) = r.recall("").unwrap();
    assert_eq!(text, "fix the [Image #1, 2x2] bug");
    assert_eq!(restored, att);
    assert_eq!(r.recall(""), None, "recalled once; the next Up browses history");
}

#[derive(Clone, Default)]
struct FakeClipboard {
    text: Arc<Mutex<String>>,
    image: Option<Vec<u8>>,
    delay: Duration,
}

impl Clipboard for FakeClipboard {
    fn get_text(&mut self) -> Result<String, String> {
        std::thread::sleep(self.delay);
        Ok(self.text.lock().unwrap().clone())
    }
    fn set_text(&mut self, text: &str) -> Result<(), String> {
        std::thread::sleep(self.delay);
        *self.text.lock().unwrap() = text.to_string();
        Ok(())
    }
    fn get_image_png(&mut self) -> Result<Vec<u8>, String> {
        self.image.clone().ok_or_else(|| "no image".into())
    }
}

#[test]
fn tui_clipboard_service_is_off_thread_and_ordered() {
    let fake = FakeClipboard {
        delay: Duration::from_millis(100),
        ..FakeClipboard::default()
    };
    let store = fake.text.clone();
    let svc = ClipboardService::spawn(fake);
    let start = Instant::now();
    svc.copy("yanked");
    let mut sink = svc.yank_sink();
    sink("from vim");
    svc.request(ClipboardRead::Text);
    svc.request(ClipboardRead::Image);
    svc.request(ClipboardRead::ImageOrText);
    assert!(start.elapsed() < Duration::from_millis(50), "{:?}", start.elapsed());
    assert_eq!(svc.try_reply(), None);
    let replies: Vec<_> = (0..3)
        .map(|_| svc.replies().recv_timeout(Duration::from_secs(5)).unwrap())
        .collect();
    assert_eq!(
        replies,
        [
            ClipboardReply::Text("from vim".into()),
            ClipboardReply::Error("no image".into()),
            ClipboardReply::Text("from vim".into()),
        ]
    );
    assert_eq!(*store.lock().unwrap(), "from vim");
    svc.shutdown();
    sink("after shutdown is a no-op");

    let img = FakeClipboard {
        image: Some(b"\x89PNGdata".to_vec()),
        ..FakeClipboard::default()
    };
    let svc = ClipboardService::spawn(img);
    svc.request(ClipboardRead::ImageOrText);
    assert_eq!(
        svc.replies().recv_timeout(Duration::from_secs(5)).unwrap(),
        ClipboardReply::Image("iVBOR2RhdGE=".into())
    );
}
