//! Ported packages/tui/test expectations (truncate-to-width, keys, stdin-buffer) with their literal values,
//! plus the 2,000-line transcript frame-diff check.

use tui::components::{Component, Spacer, Text};
use tui::keys::{KeyContext, parse_key};
use tui::renderer::Renderer;
use tui::stdin_buffer::{InputEvent, StdinBuffer};
use tui::utils::{normalize_terminal_output, truncate_to_width, visible_width, wrap_text_with_ansi};

#[test]
fn tui_truncate_closes_bel_osc8_link() {
    let open = "\x1b]8;;https://example.com\x07";
    let close = "\x1b]8;;\x07";
    let text = format!("{open}some-longer-label-here{close}");
    assert_eq!(
        truncate_to_width(&text, 15, "...", false),
        format!("{open}some-longer-{close}\x1b[0m...\x1b[0m")
    );
}

#[test]
fn tui_truncate_wide_ellipsis_and_contiguous_prefix() {
    assert_eq!(truncate_to_width("abcdef", 1, "🙂", false), "");
    assert_eq!(truncate_to_width("abcdef", 2, "🙂", false), "\x1b[0m🙂\x1b[0m");
    assert_eq!(truncate_to_width("a", 2, "🙂", false), "a");
    assert_eq!(truncate_to_width("界", 2, "🙂", false), "界");
    assert_eq!(
        truncate_to_width("🙂\t界 \x1b_abc\x07", 7, "…", true),
        "🙂\t\x1b[0m…\x1b[0m "
    );
    assert_eq!(visible_width(&truncate_to_width("🙂界🙂界🙂界", 8, "…", true)), 8);
}

#[test]
fn tui_truncate_large_unicode_input_stays_in_width() {
    let text = "🙂界".repeat(100_000);
    let out = truncate_to_width(&text, 40, "…", false);
    assert!(visible_width(&out) <= 40);
    assert!(out.ends_with("…\x1b[0m"));
    let styled = format!("\x1b[31m{}\x1b[0m", "hello ".repeat(1000));
    assert!(truncate_to_width(&styled, 20, "…", false).ends_with("\x1b[0m…\x1b[0m"));
}

#[test]
fn tui_visible_width_script_cases() {
    let cases = [
        ("\t\x1b[31m界\x1b[0m", 5),
        ("र्क", 2),
        ("नेटवर्क", 5),
        ("सर्वाधिकार सुरक्षित। ऑर्डर पर क्लिक करें", 33),
        ("ര്‍ക", 2),
        ("e\u{301}", 1),
        ("čřžůú", 5),
        ("ᜠ᜴", 1),
        ("가〮", 2),
        ("ネットワーク", 12),
        ("か\u{3099}", 2),
        ("ကာ", 2),
        ("ကို", 1),
        ("က္", 1),
        ("ำ", 1),
        ("กำ", 2),
        ("ກຳ", 2),
        ("🇺", 2),
    ];
    for (s, w) in cases {
        assert_eq!(visible_width(s), w, "{s:?}");
    }
    assert_eq!(normalize_terminal_output("ำ"), "ํา");
    assert_eq!(normalize_terminal_output("ຳ"), "ໍາ");
}

#[test]
fn tui_parse_key_ported_cases() {
    let ctx = KeyContext::default();
    let cases = [
        ("\x1b[107;9u", "super+k"),
        ("\x1b[13;9u", "super+enter"),
        ("\x1b[107;13u", "ctrl+super+k"),
        ("\x1b[107;14u", "shift+ctrl+super+k"),
        ("\x1b[49;5u", "ctrl+1"),
        ("\x1b[57399u", "0"),
        ("\x1b[57413u", "+"),
        ("\x1b[57417u", "left"),
        ("\x1b[57426u", "delete"),
        ("\x1b[27;5;99~", "ctrl+c"),
        ("\x1b[27;5;13~", "ctrl+enter"),
        ("\x1b[27;3;13~", "alt+enter"),
        ("\x1b[27;2;9~", "shift+tab"),
        ("\x1b[27;1;127~", "backspace"),
    ];
    for (input, want) in cases {
        assert_eq!(parse_key(input, ctx).as_deref(), Some(want), "{input:?}");
    }
    let kitty = KeyContext {
        kitty_active: true,
        windows_terminal: false,
    };
    assert_eq!(parse_key("\x1b\r", kitty).as_deref(), Some("shift+enter"));
    assert_eq!(parse_key("\x1b\r", ctx).as_deref(), Some("alt+enter"));
    let wt = KeyContext {
        kitty_active: false,
        windows_terminal: true,
    };
    assert_eq!(parse_key("\x08", wt).as_deref(), Some("ctrl+backspace"));
}

#[test]
fn tui_stdin_buffer_partial_sequences_and_paste() {
    let mut buf = StdinBuffer::new();
    assert_eq!(buf.process("\x1b["), vec![]);
    assert_eq!(buf.flush_after(), Some(std::time::Duration::from_millis(50)));
    assert_eq!(buf.process("1;5A"), vec![InputEvent::Data("\x1b[1;5A".into())]);
    assert_eq!(buf.process("\x1b"), vec![]);
    assert_eq!(buf.flush_after(), Some(std::time::Duration::from_millis(10)));
    assert_eq!(buf.flush(), vec!["\x1b".to_string()]);
    assert_eq!(
        buf.process("x\x1b[200~a\x1b[201~y"),
        vec![
            InputEvent::Data("x".into()),
            InputEvent::Paste("a".into()),
            InputEvent::Data("y".into())
        ]
    );
    assert_eq!(buf.process_bytes(&[0xe1]), vec![InputEvent::Data("\x1ba".into())]);
}

#[test]
fn tui_wrap_keeps_style_across_breaks() {
    assert_eq!(
        wrap_text_with_ansi("\x1b[4mhello world\x1b[0m", 5),
        vec!["\x1b[4mhello\x1b[24m".to_string(), "\x1b[4mworld\x1b[0m".to_string()]
    );
}

/// Markdown-like transcript of 2,000 rendered lines; editing one line must repaint only that line.
/// Expected bytes recorded from TuiMainScreen with the same 400 Text + Spacer children (TS run, see DECISIONS).
#[test]
fn tui_renderer_one_changed_line_in_2000_line_transcript_writes_only_that_line() {
    let width = 100;
    let body =
        |i: usize, last: &str| format!("## Turn {i}\n\n- item **{i}** with `code` and \x1b[36mcolour\x1b[39m\n{last}");
    let mut texts: Vec<Text> = (0..400)
        .map(|i| Text::new(&body(i, "> quoted text"), 1, 0, None))
        .collect();
    let frame = |texts: &mut Vec<Text>| {
        let mut lines = Vec::new();
        for t in texts.iter_mut() {
            lines.extend(t.render(width));
            lines.extend(Spacer(1).render(width));
        }
        lines
    };
    let lines = frame(&mut texts);
    assert_eq!(lines.len(), 2000);
    let mut r = Renderer::new();
    r.render(lines, width, 40).expect("first frame");

    texts[398].set_text(&body(398, "X"));
    let edited = frame(&mut texts);
    let out = r.render(edited, width, 40).expect("diff frame");
    let want = format!(
        "\x1b[?2026h\x1b[6A\r\x1b[2K X{}\x1b[0m\x1b]8;;\x07\x1b[?2026l\x1b[?25l",
        " ".repeat(98)
    );
    assert_eq!(out, want);
}
