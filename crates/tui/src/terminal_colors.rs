//! The terminal's background color (packages/tui/src/terminal-colors.ts and `TUI.queryTerminalBackgroundColor` /
//! `consumeOsc11BackgroundResponse` of tui.ts): the OSC 11 query, its reply's parsing, and a sans-IO reader that takes
//! the reply out of the input stream wherever it arrives, also when the reply is split across reads.
//!
//! The reply is `ESC ] 11 ; <color> BEL` or `... ESC \` (ST). The color is `rgb:R/G/B` (`rgba:` too, a fourth part
//! ignored) with any number of hex digits per channel scaled to 0..=255, or `#RRGGBB` / `#RRRRGGGGBBBB`.

/// `ESC ] 11 ; ? BEL`: asks for the default background color.
pub const OSC11_BACKGROUND_QUERY: &str = "\x1b]11;?\x07";

const PREFIX: &str = "\x1b]11;";
/// Longest color body a split reply may grow to while it is held (real replies are under 30 bytes).
const MAX_HELD_BODY: usize = 64;

/// The body of `^\x1b\]11;([^\x07\x1b]*)(?:\x07|\x1b\\)$`.
fn response_body(data: &str) -> Option<&str> {
    let rest = data.strip_prefix(PREFIX)?;
    let end = rest.find(['\x07', '\x1b'])?;
    matches!(&rest[end..], "\x07" | "\x1b\\").then_some(&rest[..end])
}

/// `isOsc11BackgroundColorResponse`.
pub fn is_osc11_background_color_response(data: &str) -> bool {
    response_body(data).is_some()
}

/// `parseOscHexChannel`: hex digits scaled from their own range (`16^n - 1`) to 0..=255, rounded.
fn hex_channel(channel: &str) -> Option<u8> {
    if channel.is_empty() || !channel.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let value = channel
        .chars()
        .fold(0.0_f64, |acc, c| acc * 16.0 + f64::from(c.to_digit(16).unwrap_or(0)));
    let max = 16_f64.powi(channel.len() as i32) - 1.0;
    Some((value / max * 255.0).round() as u8)
}

/// `parseOsc11BackgroundColor`: the reported background as `(r, g, b)`.
pub fn parse_osc11_background_color(data: &str) -> Option<(u8, u8, u8)> {
    let value = response_body(data)?.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let width = match hex.len() {
            6 => 2,
            12 => 4,
            _ => return None,
        };
        let channel = |i: usize| hex_channel(&hex[i * width..(i + 1) * width]);
        return Some((channel(0)?, channel(1)?, channel(2)?));
    }
    // `replace(/^rgba?:/i, "")`.
    let strip = |prefix: &str| {
        value
            .get(..prefix.len())
            .filter(|head| head.eq_ignore_ascii_case(prefix))
            .map(|_| &value[prefix.len()..])
    };
    let rgb = strip("rgba:").or_else(|| strip("rgb:")).unwrap_or(value);
    let mut parts = rgb.split('/');
    let (red, green, blue) = (parts.next()?, parts.next()?, parts.next()?);
    Some((hex_channel(red)?, hex_channel(green)?, hex_channel(blue)?))
}

/// Whether `data` can still grow into a reply: a prefix of `ESC ] 11 ;` from `ESC ]` on, or that prefix and a color
/// body of printable ASCII so far (possibly ending in the ESC of an ST).
fn is_response_prefix(data: &str) -> bool {
    if data.len() <= PREFIX.len() {
        return data.len() >= 2 && PREFIX.starts_with(data);
    }
    let Some(rest) = data.strip_prefix(PREFIX) else {
        return false;
    };
    let body = rest.strip_suffix('\x1b').unwrap_or(rest);
    body.len() <= MAX_HELD_BODY && body.bytes().all(|b| (b' '..=b'~').contains(&b))
}

/// What one input sequence was.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Osc11Read {
    /// A complete reply arrived: its color, `None` when it does not parse.
    pub reply: Option<Option<(u8, u8, u8)>>,
    /// Input for the components, in order.
    pub forward: Vec<String>,
}

/// Takes OSC 11 replies out of the input (the stdin buffer's sequences, one at a time). A complete reply is always
/// taken, whether or not a query is outstanding, so it never reaches the editor as typed text. While a query is
/// outstanding, a reply cut short by the stdin buffer's timeout is held and joined with the sequences that follow
/// until it completes; a held fragment that stops looking like a reply is dropped when it already began
/// `ESC ] 11 ;`, and handed on as input when it was shorter (it may be keys).
#[derive(Debug, Default)]
pub struct Osc11Reader {
    held: String,
}

impl Osc11Reader {
    pub fn new() -> Self {
        Self::default()
    }

    /// The held fragment of a reply, if any.
    pub fn held(&self) -> &str {
        &self.held
    }

    /// One input sequence; `expecting`: an OSC 11 query is waiting for its reply.
    pub fn input(&mut self, data: &str, expecting: bool) -> Osc11Read {
        let mut read = Osc11Read::default();
        if !self.held.is_empty() {
            let joined = format!("{}{data}", self.held);
            if is_osc11_background_color_response(&joined) {
                self.held.clear();
                read.reply = Some(parse_osc11_background_color(&joined));
                return read;
            }
            if expecting && is_response_prefix(&joined) {
                self.held = joined;
                return read;
            }
            let released = std::mem::take(&mut self.held);
            if released.len() < PREFIX.len() {
                read.forward.push(released);
            }
        }
        if is_osc11_background_color_response(data) {
            read.reply = Some(parse_osc11_background_color(data));
        } else if expecting && is_response_prefix(data) {
            self.held = data.to_string();
        } else {
            read.forward.push(data.to_string());
        }
        read
    }
}
