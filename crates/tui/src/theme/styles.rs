//! Theme text styles: `Theme.bold` / `italic` / `underline` / `inverse` / `strikethrough` from
//! packages/coding-agent theme.ts, which call chalk 6 at the process's color level.

/// One of chalk's text modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextStyle {
    Bold,
    Italic,
    Underline,
    Inverse,
    Strikethrough,
}

impl TextStyle {
    fn codes(self) -> (&'static str, &'static str) {
        match self {
            TextStyle::Bold => ("\x1b[1m", "\x1b[22m"),
            TextStyle::Italic => ("\x1b[3m", "\x1b[23m"),
            TextStyle::Underline => ("\x1b[4m", "\x1b[24m"),
            TextStyle::Inverse => ("\x1b[7m", "\x1b[27m"),
            TextStyle::Strikethrough => ("\x1b[9m", "\x1b[29m"),
        }
    }

    /// chalk's `applyStyle` at color `level` (0 = no color): re-open the style after every nested close code, close
    /// and re-open it around each line break (`\r\n` kept together), then wrap the whole string.
    pub fn apply(self, text: &str, level: u8) -> String {
        if level == 0 || text.is_empty() {
            return text.to_string();
        }
        let (open, close) = self.codes();
        let reopened;
        let mut text = text;
        if text.contains('\x1b') {
            reopened = text.replace(close, &format!("{close}{open}"));
            text = &reopened;
        }
        let mut out = String::from(open);
        let mut rest = text;
        while let Some(i) = rest.find('\n') {
            let line = &rest[..i];
            let (line, newline) = match line.strip_suffix('\r') {
                Some(l) => (l, "\r\n"),
                None => (line, "\n"),
            };
            out += line;
            out += close;
            out += newline;
            out += open;
            rest = &rest[i + 1..];
        }
        out += rest;
        out += close;
        out
    }
}
