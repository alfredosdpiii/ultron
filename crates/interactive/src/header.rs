//! The startup header at the top of the transcript (`StartupHeader` in experimental/client-tui-commands.ts) and the
//! splash logo above it when the terminal is tall enough (experimental/ultron-logo.ts).

use std::rc::Rc;

use tui::components::{Component, Text};
use tui::keybindings::KeybindingsManager;
use tui::theme::Theme;

use crate::components::{bold, fg, key_hint, key_text};

/// `ULTRON_LOGO`: `$` is background, every other character is logo; 100 columns by 55 rows.
pub const ULTRON_LOGO: &str = include_str!("ultron-logo.txt");
const LOGO_BACKGROUND: char = '$';
const INK: &str = "$ .',:;Il!i>";
/// Terminal rows the rest of the UI needs below the splash.
const RESERVED_ROWS: usize = 14;

fn ink(c: char) -> usize {
    if c == LOGO_BACKGROUND || c == ' ' {
        return 0;
    }
    INK.find(c).unwrap_or(INK.len())
}

/// `halveLogo`: half size in both directions, keeping the most inked character of each 2x2 block.
pub fn halve_logo(art: &str) -> Vec<String> {
    let rows: Vec<Vec<char>> = art.split('\n').map(|r| r.chars().collect()).collect();
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let grid: Vec<Vec<char>> = rows
        .into_iter()
        .map(|mut r| {
            r.resize(width, LOGO_BACKGROUND);
            r
        })
        .collect();
    let mut out = Vec::new();
    for y in (0..grid.len()).step_by(2) {
        let mut line = String::new();
        for x in (0..width).step_by(2) {
            let mut best = LOGO_BACKGROUND;
            for row in [grid.get(y), grid.get(y + 1)].into_iter().flatten() {
                for c in [row.get(x), row.get(x + 1)].into_iter().flatten() {
                    if ink(*c) > ink(best) {
                        best = *c;
                    }
                }
            }
            line.push(best);
        }
        out.push(line);
    }
    out
}

/// `splashLines`: the full or half logo centered at `width`, or none when even the half logo does not fit.
pub fn splash_lines(width: usize, rows: Option<usize>) -> Vec<String> {
    let full: Vec<String> = ULTRON_LOGO.split('\n').map(str::to_string).collect();
    let half = halve_logo(ULTRON_LOGO);
    let fits = |logo: &[String]| {
        let w = logo.iter().map(|l| l.chars().count()).max().unwrap_or(0);
        w <= width && rows.is_none_or(|r| logo.len() + RESERVED_ROWS <= r)
    };
    let logo = if fits(&full) {
        full
    } else if fits(&half) {
        half
    } else {
        return Vec::new();
    };
    let logo_width = logo.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let pad = " ".repeat((width - logo_width) / 2);
    logo.into_iter()
        .map(|l| if l.is_empty() { l } else { format!("{pad}{l}") })
        .collect()
}

/// `colorLogoLine`: runs of background in one style, logo characters in another, padding plain.
pub fn color_logo_line(line: &str, logo: &dyn Fn(&str) -> String, background: &dyn Fn(&str) -> String) -> String {
    let class = |c: char| match c {
        ' ' => 0,
        LOGO_BACKGROUND => 1,
        _ => 2,
    };
    let mut out = String::new();
    let mut run = String::new();
    let mut current = None;
    let flush = |run: &mut String, kind: Option<u8>, out: &mut String| {
        match kind {
            Some(0) => out.push_str(run),
            Some(1) => out.push_str(&background(run)),
            Some(_) => out.push_str(&logo(run)),
            None => {}
        }
        run.clear();
    };
    for c in line.chars() {
        let k = class(c);
        if current != Some(k) {
            flush(&mut run, current, &mut out);
            current = Some(k);
        }
        run.push(c);
    }
    flush(&mut run, current, &mut out);
    out
}

/// `StartupHeader`: app name and version, the compact key hints (the full list when expanded), and details (the
/// server and session ids) under them; the splash above when enabled and it fits.
pub struct StartupHeader {
    pub theme: Rc<Theme>,
    pub version: String,
    pub expanded: bool,
    /// Already styled.
    pub details: String,
    pub splash: bool,
    pub rows: Option<usize>,
}

impl StartupHeader {
    fn text(&self, keys: &KeybindingsManager) -> String {
        let t = &self.theme;
        let k = |id: &str| key_text(keys, id, false);
        let hint = |id: &str, desc: &str| key_hint(t, &k(id), desc);
        let raw = |key: &str, desc: &str| key_hint(t, key, desc);
        let logo = format!(
            "{}{}",
            bold(&fg(t, "accent", "ultron")),
            fg(t, "dim", &format!(" v{}", self.version))
        );
        let hints = if self.expanded {
            [
                hint("app.interrupt", "to interrupt"),
                hint("app.clear", "to clear"),
                raw(&format!("{} twice", k("app.clear")), "to exit"),
                hint("app.exit", "to exit (empty)"),
                hint("app.suspend", "to suspend"),
                raw(
                    &format!("{}/{}", k("tui.editor.cursorUp"), k("tui.editor.cursorDown")),
                    "for prompt history",
                ),
                hint("app.history.search", "to search prompt history"),
                hint("app.thinking.cycle", "to cycle thinking level"),
                raw(
                    &format!("{}/{}", k("app.model.cycleForward"), k("app.model.cycleBackward")),
                    "to cycle models",
                ),
                hint("app.model.select", "to select model"),
                hint("app.tools.expand", "to expand tools"),
                hint("app.thinking.toggle", "to expand thinking"),
                hint("app.editor.external", "for external editor"),
                raw("/", "for commands"),
                raw("!", "to run bash"),
                raw("!!", "to run bash (no context)"),
                hint("app.message.followUp", "to queue follow-up"),
                hint("app.message.dequeue", "to edit all queued messages"),
                hint("app.clipboard.pasteImage", "to paste image (with text fallback)"),
                hint("app.rlm.toggle", "for the RLM panel"),
                hint("app.rlm.pane", "for the RLM pane"),
            ]
            .join("\n")
        } else {
            [
                hint("app.interrupt", "interrupt"),
                raw(&format!("{}/{}", k("app.clear"), k("app.exit")), "clear/exit"),
                raw(
                    &format!("{}/{}", k("tui.editor.cursorUp"), k("app.history.search")),
                    "history",
                ),
                raw("/", "commands"),
                raw("!", "bash"),
                hint("app.tools.expand", "more"),
            ]
            .join(&fg(t, "muted", " · "))
        };
        let details = if self.details.is_empty() {
            String::new()
        } else {
            format!("\n{}", self.details)
        };
        format!("{logo}\n{hints}{details}")
    }

    pub fn render(&self, keys: &KeybindingsManager, width: usize) -> Vec<String> {
        let text = Text::new(&self.text(keys), 1, 0, None).render(width);
        if !self.splash {
            return text;
        }
        let t = &self.theme;
        let logo = |s: &str| bold(&fg(t, "accent", s));
        let background = |s: &str| fg(t, "dim", s);
        let splash: Vec<String> = splash_lines(width, self.rows)
            .iter()
            .map(|l| color_logo_line(l, &logo, &background))
            .collect();
        if splash.is_empty() {
            return text;
        }
        let mut lines = splash;
        lines.push(String::new());
        lines.extend(text);
        lines
    }
}
