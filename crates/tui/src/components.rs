//! Basic components: Text, TruncatedText, Spacer and Container (ports of packages/tui/src/components/*.ts).

use crate::utils::{apply_background_to_line, truncate_to_width, visible_width, wrap_text_with_ansi};

/// A renderable: lines for a width, each at most `width` columns.
pub trait Component {
    fn render(&mut self, width: usize) -> Vec<String>;

    /// Drop render caches (a block retired to scrollback renders again only on replay).
    fn release_cache(&mut self) {}
}

pub type BgFn = Box<dyn Fn(&str) -> String>;

/// Word-wrapped text with padding and an optional background.
pub struct Text {
    text: String,
    padding_x: usize,
    padding_y: usize,
    bg: Option<BgFn>,
    cache: Option<(usize, Vec<String>)>,
}

impl Text {
    pub fn new(text: &str, padding_x: usize, padding_y: usize, bg: Option<BgFn>) -> Self {
        Self {
            text: text.to_string(),
            padding_x,
            padding_y,
            bg,
            cache: None,
        }
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cache = None;
    }
}

impl Component for Text {
    fn render(&mut self, width: usize) -> Vec<String> {
        if let Some((w, lines)) = &self.cache
            && *w == width
        {
            return lines.clone();
        }
        let lines = self.render_uncached(width);
        self.cache = Some((width, lines.clone()));
        lines
    }

    fn release_cache(&mut self) {
        self.cache = None;
    }
}

impl Text {
    fn render_uncached(&self, width: usize) -> Vec<String> {
        if self.text.chars().all(crate::utils::is_whitespace_char) {
            return Vec::new();
        }
        let normalized = self.text.replace('\t', "   ");
        let padding_x = self.padding_x.min(width.saturating_sub(1) / 2);
        let content_width = width.saturating_sub(padding_x * 2).max(1);
        let margin = " ".repeat(padding_x);
        let pad_line = |line: &str| match &self.bg {
            Some(bg) => apply_background_to_line(line, width, bg.as_ref()),
            None => format!("{line}{}", " ".repeat(width.saturating_sub(visible_width(line)))),
        };
        let empty = pad_line(&" ".repeat(width));
        let mut out = vec![empty.clone(); self.padding_y];
        for line in wrap_text_with_ansi(&normalized, content_width) {
            out.push(pad_line(&format!("{margin}{line}{margin}")));
        }
        out.extend(std::iter::repeat_n(empty, self.padding_y));
        out
    }
}

/// One line of text, cut at the first newline and truncated with an ellipsis.
pub struct TruncatedText {
    pub text: String,
    pub padding_x: usize,
    pub padding_y: usize,
}

impl Component for TruncatedText {
    fn render(&mut self, width: usize) -> Vec<String> {
        let empty = " ".repeat(width);
        let mut out = vec![empty.clone(); self.padding_y];
        let available = width.saturating_sub(self.padding_x * 2).max(1);
        let single = self.text.split('\n').next().unwrap_or("");
        let pad = " ".repeat(self.padding_x);
        let line = format!("{pad}{}{pad}", truncate_to_width(single, available, "...", false));
        let fill = width.saturating_sub(visible_width(&line));
        out.push(format!("{line}{}", " ".repeat(fill)));
        out.extend(std::iter::repeat_n(empty, self.padding_y));
        out
    }
}

/// Padded group of children with an optional background (`Box` in TS).
pub struct Panel {
    pub children: Vec<Box<dyn Component>>,
    pub padding_x: usize,
    pub padding_y: usize,
    pub bg: Option<BgFn>,
}

impl Component for Panel {
    fn render(&mut self, width: usize) -> Vec<String> {
        let content_width = width.saturating_sub(self.padding_x * 2).max(1);
        let child_lines: Vec<String> = self.children.iter_mut().flat_map(|c| c.render(content_width)).collect();
        if child_lines.is_empty() {
            return Vec::new();
        }
        let left = " ".repeat(self.padding_x);
        let apply = |line: &str| {
            let padded = format!("{line}{}", " ".repeat(width.saturating_sub(visible_width(line))));
            match &self.bg {
                Some(bg) => apply_background_to_line(&padded, width, bg.as_ref()),
                None => padded,
            }
        };
        let empty = apply("");
        let mut out = vec![empty.clone(); self.padding_y];
        out.extend(child_lines.iter().map(|l| apply(&format!("{left}{l}"))));
        out.extend(std::iter::repeat_n(empty, self.padding_y));
        out
    }
}

/// `lines` empty lines.
pub struct Spacer(pub usize);

impl Component for Spacer {
    fn render(&mut self, _width: usize) -> Vec<String> {
        vec![String::new(); self.0]
    }
}

/// Children rendered top to bottom.
#[derive(Default)]
pub struct Container {
    pub children: Vec<Box<dyn Component>>,
}

impl Component for Container {
    fn render(&mut self, width: usize) -> Vec<String> {
        self.children.iter_mut().flat_map(|c| c.render(width)).collect()
    }

    fn release_cache(&mut self) {
        self.children.iter_mut().for_each(|c| c.release_cache());
    }
}
