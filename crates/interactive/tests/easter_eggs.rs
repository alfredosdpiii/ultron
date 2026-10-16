//! Replays fixtures/easter_eggs.json (recorded from Pi's ArminComponent and EarendilAnnouncementComponent by
//! fixtures/easter_eggs.mjs): with the same `Math.random` sequence the portrait picks the same effect and paints the
//! same frames at the recorded frame numbers, and the announcement renders the same lines at each width. The
//! recording's color codes stand for the theme's colors, so each recorded line is translated to this theme's codes
//! and compared whole.

use interactive::easter_eggs::{Armin, EarendilAnnouncement};
use serde_json::Value;
use std::rc::Rc;
use tui::components::Component;
use tui::theme::{ColorMode, Theme, builtin_theme_json, create_theme};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/easter_eggs.json")).unwrap()
}

fn theme() -> Rc<Theme> {
    Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap())
}

/// The fixture's `mulberry32`.
fn mulberry32(seed: u32) -> impl FnMut() -> f64 {
    let mut a = seed;
    move || {
        a = a.wrapping_add(0x6d2b_79f5);
        let mut t = a;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

/// The recorded line with the fixture's color codes replaced by `theme`'s.
fn translate(line: &str, colors: &[Value], theme: &Theme) -> String {
    let mut out = line.to_string();
    for (i, color) in colors.iter().enumerate() {
        let code = format!("\x1b[38;5;{}m", i + 1);
        let styled = theme.fg(color.as_str().unwrap(), "\u{0}").unwrap();
        let open = styled.split('\u{0}').next().unwrap().to_string();
        // Wrapped lines carry the active styles combined into one SGR sequence (`\x1b[1;38;5;Nm`).
        let combined = format!("\x1b[1;38;5;{}m", i + 1);
        out = out.replace(&combined, &format!("\x1b[1;{}", &open[2..]));
        out = out.replace(&code, &open);
    }
    let close = theme.fg("accent", "\u{0}").unwrap();
    let close = close.split('\u{0}').nth(1).unwrap();
    out.replace("\x1b[39m", close)
}

#[test]
fn interactive_easter_eggs_armin_effects_paint_the_typescript_frames() {
    let fixture = fixture();
    let (theme, colors) = (theme(), fixture["colors"].as_array().unwrap().clone());
    let widths: Vec<usize> = fixture["widths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_u64().unwrap() as usize)
        .collect();
    let cases = fixture["armin"].as_array().unwrap();
    assert_eq!(cases.len(), 7);
    for case in cases {
        let effect = case["effect"].as_u64().unwrap();
        let mut seeded = mulberry32(case["seed"].as_u64().unwrap() as u32);
        let mut first = true;
        let random = Box::new(move || {
            if std::mem::take(&mut first) {
                (effect as f64 + 0.5) / 7.0
            } else {
                seeded()
            }
        });
        let mut armin = Armin::new(Rc::clone(&theme), random);
        assert_eq!(armin.fps(), case["fps"].as_u64().unwrap(), "effect {effect} fps");
        let mut played = 0u64;
        for frame in case["frames"].as_array().unwrap() {
            let tick = frame["tick"].as_u64().unwrap();
            while played < tick {
                armin.tick();
                played += 1;
            }
            for (w, lines) in widths.iter().zip(frame["lines"].as_array().unwrap()) {
                let want: Vec<String> = lines
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|l| translate(l.as_str().unwrap(), &colors, &theme))
                    .collect();
                assert_eq!(armin.render(*w), want, "effect {effect} tick {tick} width {w}");
            }
        }
        // TypeScript stops after `ticks` frames; the rain never stops there (a column without pixels keeps its
        // timer running), but its frames stop changing: Rust ends it then.
        if case["ended"] == true {
            let ticks = case["ticks"].as_u64().unwrap();
            let mut rust = Armin::new(Rc::clone(&theme), {
                let mut seeded = mulberry32(case["seed"].as_u64().unwrap() as u32);
                let mut first = true;
                Box::new(move || {
                    if std::mem::take(&mut first) {
                        (effect as f64 + 0.5) / 7.0
                    } else {
                        seeded()
                    }
                })
            });
            for _ in 0..ticks - 1 {
                rust.tick();
                assert!(!rust.done(), "effect {effect} ended early");
            }
            rust.tick();
            assert!(rust.done(), "effect {effect} ends after {ticks} frames");
        } else {
            assert!(armin.done(), "effect {effect}: the static rain has ended");
        }
    }
}

#[test]
fn interactive_easter_eggs_earendil_announcement_renders_like_typescript() {
    let fixture = fixture();
    let (theme, colors) = (theme(), fixture["colors"].as_array().unwrap().clone());
    for case in fixture["earendil"].as_array().unwrap() {
        let width = case["width"].as_u64().unwrap() as usize;
        let want: Vec<String> = case["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| translate(l.as_str().unwrap(), &colors, &theme))
            .collect();
        assert_eq!(
            EarendilAnnouncement::new(Rc::clone(&theme)).render(width),
            want,
            "width {width}"
        );
    }
}
