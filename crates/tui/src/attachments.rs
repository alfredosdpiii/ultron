//! Paste and attachments (ports of omp packages/tui/src/bracketed-paste.ts, the pasted-path helpers of
//! prompt/custom-editor.ts, coding-agent/src/utils/enhanced-paste.ts (OSC 5522), prompt/attachment-chips.ts and
//! the cleared-draft recall of prompt/custom-editor.ts).
//!
//! Images enter the draft as `[Image #N, WxH]` placeholders (atomic in the editor and in Vim mode), backed by an
//! [`Attachments`] registry; large text pastes stay the editor's own `[paste #N ...]` markers. [`chip_band`]
//! renders one rounded card per attachment still referenced by the draft.

use std::path::Path;
use std::sync::OnceLock;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use fancy_regex::Regex;

use crate::utils::{truncate_to_width, visible_width};

pub const PASTE_START: &str = "\x1b[200~";
pub const PASTE_END: &str = "\x1b[201~";
/// Paste the clipboard verbatim, without collapsing a large paste into a marker or turning a path into an image.
pub const RAW_PASTE_KEY: &str = "ctrl+shift+v";
const DEFAULT_BYTE_LIMIT: usize = 64 * 1024 * 1024;

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).unwrap_or_else(|_| unreachable!()))
}

/// Byte spans of `[Image #N]`, `[Image #N, WxH]`, `[Video #N]` placeholders in `text`.
pub fn placeholder_spans(text: &str) -> Vec<(usize, usize)> {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"\[(?:Image|Video|File) #[1-9]\d*(?:,[^\]\n]*)?\]")
        .find_iter(text)
        .filter_map(Result::ok)
        .map(|m| (m.start(), m.end()))
        .collect()
}

/// Decode tmux's re-encoded Ctrl+letter bytes inside a paste (`ESC[106;5u` and `ESC[27;5;106~` -> `\n`).
pub fn decode_reencoded_paste_controls(text: &str) -> String {
    static CSI_U: OnceLock<Regex> = OnceLock::new();
    static XTERM: OnceLock<Regex> = OnceLock::new();
    let decode = |text: &str, re: &Regex| -> String {
        let mut out = String::new();
        let mut last = 0;
        for caps in re.captures_iter(text).filter_map(Result::ok) {
            let (Some(m), Some(code)) = (caps.get(0), caps.get(1)) else {
                continue;
            };
            out.push_str(&text[last..m.start()]);
            let cp: u32 = code.as_str().parse().unwrap_or(0);
            match cp {
                97..=122 => out.push(char::from_u32(cp - 96).unwrap_or('?')),
                65..=90 => out.push(char::from_u32(cp - 64).unwrap_or('?')),
                _ => out.push_str(m.as_str()),
            }
            last = m.end();
        }
        out.push_str(&text[last..]);
        out
    };
    let once = decode(text, regex(&CSI_U, r"\x1b\[(\d+);5u"));
    decode(&once, regex(&XTERM, r"\x1b\[27;5;(\d+)~"))
}

/// Result of feeding one chunk to [`BracketedPaste`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasteResult {
    /// No paste involved: process the chunk normally.
    NotHandled,
    /// Consumed by paste buffering; `content` is set once a paste completed (or the byte cap aborted it).
    Handled { content: Option<String>, remaining: String },
}

/// Assembles bracketed pastes that arrive split across chunks.
pub struct BracketedPaste {
    buffer: String,
    active: bool,
    byte_limit: usize,
}

impl Default for BracketedPaste {
    fn default() -> Self {
        Self::with_byte_limit(DEFAULT_BYTE_LIMIT)
    }
}

impl BracketedPaste {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_byte_limit(byte_limit: usize) -> Self {
        Self {
            buffer: String::new(),
            active: false,
            byte_limit,
        }
    }

    pub fn process(&mut self, data: &str) -> PasteResult {
        let mut data = data.to_string();
        let start = data.find(PASTE_START);
        let end = data.find(PASTE_END);
        if let Some(s) = start
            && end.is_none_or(|e| s < e)
        {
            if !self.active {
                self.buffer.clear();
            }
            self.active = true;
            data = match end {
                None => data.replace(PASTE_START, ""),
                Some(e) => format!("{}{}", data[..e].replace(PASTE_START, ""), &data[e..]),
            };
        }
        if !self.active {
            return PasteResult::NotHandled;
        }
        self.buffer.push_str(&data);
        if let Some(e) = self.buffer.find(PASTE_END) {
            let content = self.buffer[..e].to_string();
            let remaining = self.buffer[e + PASTE_END.len()..].to_string();
            self.buffer.clear();
            self.active = false;
            return PasteResult::Handled {
                content: Some(content),
                remaining,
            };
        }
        // JS measures UTF-16 length; ASCII-dominated pastes agree.
        if self.buffer.encode_utf16().count() > self.byte_limit {
            self.active = false;
            return PasteResult::Handled {
                content: Some(std::mem::take(&mut self.buffer)),
                remaining: String::new(),
            };
        }
        PasteResult::Handled {
            content: None,
            remaining: String::new(),
        }
    }
}

fn is_absolute_prefix(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/')
        || p.starts_with("~/")
        || p.to_ascii_lowercase().starts_with("file://")
        || p.starts_with("\\\\")
        || (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
}

fn is_dot_relative(p: &str) -> bool {
    p.starts_with("./") || p.starts_with(".\\") || p.starts_with("../") || p.starts_with("..\\")
}

fn has_uri_scheme(p: &str) -> bool {
    let mut chars = p.char_indices();
    if !chars.next().is_some_and(|(_, c)| c.is_ascii_alphabetic()) {
        return false;
    }
    for (_, c) in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')) {
            return false;
        }
    }
    false
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Unquote, decode `file://` URLs and shell-unescape a pasted path.
pub fn normalize_pasted_path(path: &str) -> String {
    let trimmed = path.trim();
    let first = trimmed.chars().next();
    let unquoted = if trimmed.chars().count() > 1
        && matches!(first, Some('"' | '\''))
        && trimmed.ends_with(first.unwrap_or(' '))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };
    if unquoted.to_ascii_lowercase().starts_with("file://") {
        let rest = &unquoted[7..];
        let host_end = rest.find('/').unwrap_or(rest.len());
        let host = &rest[..host_end];
        if (host.is_empty() || host == "localhost")
            && let Some(decoded) = percent_decode(&rest[host_end..])
        {
            return decoded;
        }
    }
    let unquoted = unquoted
        .strip_prefix("\\~/")
        .map_or(unquoted.to_string(), |r| format!("~/{r}"));
    let mut out = String::new();
    let mut chars = unquoted.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\'
            && let Some(&n) = chars.peek()
            && (n.is_whitespace() || "\\'\"()[]{}&;<>|?*!$`".contains(n))
        {
            out.push(n);
            chars.next();
            continue;
        }
        out.push(c);
    }
    out
}

fn is_explicit_pasted_path(p: &str) -> bool {
    if is_absolute_prefix(p) || is_dot_relative(p) {
        return true;
    }
    if has_uri_scheme(p) {
        return false;
    }
    p.contains('\\')
}

fn extension(p: &str) -> String {
    Path::new(p)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn is_image_path(p: &str) -> bool {
    matches!(extension(p).as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp")
}

pub fn is_video_path(p: &str) -> bool {
    matches!(
        extension(p).as_str(),
        "mp4" | "mov" | "mkv" | "webm" | "m4v" | "avi" | "wmv"
    )
}

/// An image or video that can become a vision attachment.
pub fn is_previewable_path(p: &str) -> bool {
    is_image_path(p) || is_video_path(p)
}

/// MIME type for an image path.
pub fn image_mime_for_path(p: &str) -> Option<&'static str> {
    Some(match extension(p).as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

fn split_path_segments(payload: &str) -> Option<Vec<String>> {
    let mut segments = Vec::new();
    let mut seg = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for c in payload.chars() {
        if escaped {
            seg.push(c);
            escaped = false;
            continue;
        }
        if c == '\\' {
            seg.push(c);
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            seg.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        if c == '"' || c == '\'' {
            seg.push(c);
            quote = Some(c);
            continue;
        }
        if matches!(c, ' ' | '\t' | '\r' | '\n') {
            if !seg.is_empty() {
                segments.push(std::mem::take(&mut seg));
            }
            continue;
        }
        seg.push(c);
    }
    if escaped || quote.is_some() {
        return None;
    }
    if !seg.is_empty() {
        segments.push(seg);
    }
    (!segments.is_empty()).then_some(segments)
}

/// Whitespace/quote-separated paths when every segment is an explicit local path; otherwise `None`.
pub fn extract_paste_paths_from_text(text: &str) -> Option<Vec<String>> {
    let pasted = text.trim();
    if pasted.is_empty() {
        return None;
    }
    split_path_segments(pasted)?
        .iter()
        .map(|s| {
            let p = normalize_pasted_path(s);
            (!p.is_empty() && is_explicit_pasted_path(&p)).then_some(p)
        })
        .collect()
}

/// A second path anchor after unescaped whitespace: the shape of several dropped files.
fn has_interior_anchor(text: &str) -> bool {
    let b = text.as_bytes();
    for (i, c) in text.char_indices() {
        if !c.is_whitespace() || (i > 0 && b[i - 1] == b'\\') {
            continue;
        }
        let rest = &text[i + c.len_utf8()..];
        if is_absolute_prefix(rest) || is_dot_relative(rest) {
            return true;
        }
    }
    false
}

fn whole_text_attachment_path(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() || t.contains(['\r', '\n']) || !is_absolute_prefix(t) || has_interior_anchor(t) {
        return None;
    }
    let p = normalize_pasted_path(t);
    (!p.is_empty() && is_explicit_pasted_path(&p) && is_previewable_path(&p)).then_some(p)
}

/// Previewable attachment paths in pasted text (several dropped files, or one path with unescaped spaces).
pub fn extract_image_paste_paths_from_text(text: &str) -> Option<Vec<String>> {
    if let Some(paths) = extract_paste_paths_from_text(text) {
        return paths.iter().all(|p| is_previewable_path(p)).then_some(paths);
    }
    whole_text_attachment_path(text).map(|p| vec![p])
}

/// The single previewable path `text` names, if that is all it is.
pub fn extract_image_path_from_text(text: &str) -> Option<String> {
    match extract_paste_paths_from_text(text) {
        Some(paths) if paths.len() == 1 && is_previewable_path(&paths[0]) => paths.into_iter().next(),
        Some(_) => None,
        None => whole_text_attachment_path(text),
    }
}

fn bracketed_payload(data: &str) -> Option<&str> {
    let rest = data.strip_prefix(PASTE_START)?;
    let end = rest.find(PASTE_END)?;
    (end + PASTE_END.len() == rest.len()).then(|| &rest[..end])
}

pub fn extract_bracketed_paste_paths(data: &str) -> Option<Vec<String>> {
    extract_paste_paths_from_text(bracketed_payload(data)?)
}

pub fn extract_bracketed_image_paste_paths(data: &str) -> Option<Vec<String>> {
    extract_image_paste_paths_from_text(bracketed_payload(data)?)
}

// ---- OSC 5522 enhanced (typed) paste ----

const OSC5522: &str = "\x1b]5522;";

/// One parsed OSC 5522 packet: `metadata` keys in order (a later duplicate wins on lookup) and the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Osc5522Packet {
    pub metadata: Vec<(String, String)>,
    pub payload: String,
}

impl Osc5522Packet {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.metadata
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

pub fn is_osc5522_packet(data: &str) -> bool {
    data.starts_with(OSC5522) && (data.ends_with("\x1b\\") || data.ends_with('\x07'))
}

pub fn parse_osc5522_packet(data: &str) -> Option<Osc5522Packet> {
    if !is_osc5522_packet(data) {
        return None;
    }
    let body_end = if data.ends_with('\x07') {
        data.len() - 1
    } else {
        data.len() - 2
    };
    let body = &data[OSC5522.len()..body_end.max(OSC5522.len())];
    let (meta, payload) = body.split_once(';').unwrap_or((body, ""));
    let metadata = meta
        .split(':')
        .filter_map(|part| {
            let eq = part.find('=').filter(|e| *e > 0)?;
            Some((part[..eq].to_string(), part[eq + 1..].to_string()))
        })
        .collect();
    Some(Osc5522Packet {
        metadata,
        payload: payload.to_string(),
    })
}

/// What a packet asked the host to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasteEvent {
    /// Bytes to send to the terminal (the read request).
    Write(String),
    Text(String),
    Image {
        data_b64: String,
        mime: String,
    },
    Status(String),
}

enum PasteState {
    Listing {
        mimes: Vec<String>,
        kitty_dot: bool,
        pw: Option<String>,
        loc: Option<String>,
    },
    Reading {
        text: bool,
        mime: String,
        chunks: Vec<String>,
    },
}

fn b64_utf8(s: &str) -> Option<String> {
    B64.decode(s).ok().and_then(|b| String::from_utf8(b).ok())
}

/// OSC 5522 paste controller: lists the offered MIME types, requests an image (else plain text), assembles it.
#[derive(Default)]
pub struct EnhancedPaste {
    state: Option<PasteState>,
}

impl EnhancedPaste {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enable() -> &'static str {
        "\x1b[?5522h"
    }

    pub fn disable(&mut self) -> &'static str {
        self.state = None;
        "\x1b[?5522l"
    }

    /// `None` when `data` is not an OSC 5522 packet (route it as ordinary input).
    pub fn handle_input(&mut self, data: &str) -> Option<Vec<PasteEvent>> {
        let packet = parse_osc5522_packet(data)?;
        let mut events = Vec::new();
        if packet.get("type") != Some("read") {
            return Some(events);
        }
        match packet.get("status") {
            Some("OK") => {
                if !matches!(self.state, Some(PasteState::Reading { .. })) {
                    self.state = Some(PasteState::Listing {
                        mimes: Vec::new(),
                        kitty_dot: false,
                        pw: packet.get("pw").map(str::to_string),
                        loc: packet.get("loc").filter(|l| *l == "primary").map(str::to_string),
                    });
                }
            }
            Some("DATA") => self.data(&packet),
            Some("DONE") => self.done(&mut events),
            Some(status) if !status.is_empty() => {
                self.state = None;
                events.push(PasteEvent::Status(format!("Enhanced paste failed: {status}")));
            }
            _ => {}
        }
        Some(events)
    }

    fn data(&mut self, packet: &Osc5522Packet) {
        let Some(mime) = packet.get("mime").and_then(b64_utf8) else {
            return;
        };
        match &mut self.state {
            Some(PasteState::Listing { mimes, kitty_dot, .. }) => {
                if mime == "." {
                    if packet.payload.is_empty() {
                        return;
                    }
                    let Some(listing) = b64_utf8(&packet.payload) else {
                        return;
                    };
                    *kitty_dot = true;
                    mimes.extend(listing.split_whitespace().filter(|m| *m != ".").map(str::to_string));
                    return;
                }
                mimes.push(mime);
            }
            Some(PasteState::Reading { mime: want, chunks, .. }) if *want == mime && !packet.payload.is_empty() => {
                chunks.push(packet.payload.clone());
            }
            Some(PasteState::Reading { .. }) | None => {}
        }
    }

    fn done(&mut self, events: &mut Vec<PasteEvent>) {
        match self.state.take() {
            None => {}
            Some(PasteState::Listing {
                mimes,
                kitty_dot,
                pw,
                loc,
            }) => {
                let chosen = ["image/png", "image/jpeg", "image/webp", "image/gif"]
                    .into_iter()
                    .find(|m| mimes.iter().any(|x| x == m))
                    .map(|m| (false, m))
                    .or_else(|| mimes.iter().any(|m| m == "text/plain").then_some((true, "text/plain")));
                let Some((text, mime)) = chosen else {
                    events.push(PasteEvent::Status(
                        "Clipboard paste has no supported text or image data".into(),
                    ));
                    return;
                };
                self.state = Some(PasteState::Reading {
                    text,
                    mime: mime.to_string(),
                    chunks: Vec::new(),
                });
                let encoded = B64.encode(mime);
                let mut meta = vec!["type=read".to_string()];
                if let Some(loc) = loc {
                    meta.push(format!("loc={loc}"));
                }
                if let Some(pw) = pw {
                    meta.push(format!("pw={pw}"));
                    meta.push(format!("name={}", B64.encode("Paste event")));
                }
                if kitty_dot {
                    events.push(PasteEvent::Write(format!("{OSC5522}{};{encoded}\x07", meta.join(":"))));
                } else {
                    meta.push(format!("mime={encoded}"));
                    events.push(PasteEvent::Write(format!("{OSC5522}{}\x07", meta.join(":"))));
                }
            }
            Some(PasteState::Reading { text, mime, chunks }) => {
                let bytes: Vec<u8> = chunks.iter().flat_map(|c| B64.decode(c).unwrap_or_default()).collect();
                if bytes.is_empty() {
                    events.push(PasteEvent::Status("Clipboard paste was empty".into()));
                } else if text {
                    events.push(PasteEvent::Text(String::from_utf8_lossy(&bytes).into_owned()));
                } else {
                    events.push(PasteEvent::Image {
                        data_b64: B64.encode(&bytes),
                        mime,
                    });
                }
            }
        }
    }
}

// ---- attachments and chips ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    Image,
    Video,
}

/// An image staged for the next prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageAttachment {
    pub kind: AttachmentKind,
    pub data_b64: String,
    pub mime: String,
    /// Local file it came from (pasted path or dropped file).
    pub path: Option<String>,
    pub dims: Option<(u32, u32)>,
}

/// Images staged for the draft; numbered from 1 in insertion order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attachments {
    pub images: Vec<ImageAttachment>,
}

impl Attachments {
    /// Stage an image; returns its placeholder (`[Image #N, WxH]`) to insert into the editor.
    pub fn add_image(&mut self, data_b64: String, mime: &str, path: Option<String>) -> String {
        let dims = crate::image::image_dimensions(&data_b64, mime).map(|d| (d.width_px, d.height_px));
        let kind = if path.as_deref().is_some_and(is_video_path) {
            AttachmentKind::Video
        } else {
            AttachmentKind::Image
        };
        self.images.push(ImageAttachment {
            kind,
            data_b64,
            mime: mime.to_string(),
            path,
            dims,
        });
        self.placeholder(self.images.len())
    }

    /// Read an image file (runs on the caller's thread: call it off the paint path) and stage it.
    pub fn add_image_file(&mut self, path: &str) -> std::io::Result<String> {
        let mime = image_mime_for_path(path)
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "not an image path"))?;
        let expanded = match path.strip_prefix("~/") {
            Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
            None => path.to_string(),
        };
        let bytes = std::fs::read(expanded)?;
        Ok(self.add_image(B64.encode(bytes), mime, Some(path.to_string())))
    }

    pub fn placeholder(&self, n: usize) -> String {
        let label = match self.images.get(n - 1).map(|i| i.kind) {
            Some(AttachmentKind::Video) => "Video",
            _ => "Image",
        };
        match self.images.get(n - 1).and_then(|i| i.dims) {
            Some((w, h)) => format!("[{label} #{n}, {w}x{h}]"),
            None => format!("[{label} #{n}]"),
        }
    }

    /// Images whose placeholder is still in `text`, with their numbers.
    pub fn referenced<'a>(&'a self, text: &str) -> Vec<(usize, &'a ImageAttachment)> {
        self.images
            .iter()
            .enumerate()
            .map(|(i, img)| (i + 1, img))
            .filter(|(n, _)| text.contains(&self.placeholder(*n)))
            .collect()
    }

    pub fn clear(&mut self) {
        self.images.clear();
    }
}

/// One card in the chips band.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Chip<'a> {
    Image { n: usize, image: &'a ImageAttachment },
    Paste { n: u64, content: &'a str },
}

const INNER_COLS: usize = 12;
const INNER_ROWS: usize = 4;
const CARD_COLS: usize = INNER_COLS + 2;
const CARD_GAP: usize = 2;
const PALETTE: [(u8, u8, u8); 6] = [
    (255, 179, 102),
    (125, 207, 255),
    (189, 147, 249),
    (105, 220, 158),
    (255, 141, 188),
    (240, 223, 120),
];

/// Stable color for attachment `n` (omp `attachmentRgb`).
pub fn attachment_sgr(kind: &str, n: usize) -> String {
    let i = match kind {
        "image" => (n + PALETTE.len() - 1) % PALETTE.len(),
        "video" => (n + 1) % PALETTE.len(),
        _ => (n + 2) % PALETTE.len(),
    };
    let (r, g, b) = PALETTE[i];
    format!("\x1b[38;2;{r};{g};{b}m")
}

fn border_row(sgr: &str, caption: &str, top: bool) -> String {
    let (l, r) = if top { ("╭", "╮") } else { ("╰", "╯") };
    if caption.is_empty() {
        return format!("{sgr}{l}{}{r}\x1b[39m", "─".repeat(INNER_COLS));
    }
    let cut = truncate_to_width(caption, INNER_COLS - 2, "…", false);
    let fill = INNER_COLS - visible_width(&cut) - 2;
    let lf = fill / 2;
    format!(
        "{sgr}{l}{} \x1b[1m{cut}\x1b[22m {}{r}\x1b[39m",
        "─".repeat(lf),
        "─".repeat(fill - lf)
    )
}

fn card(chip: &Chip) -> Vec<String> {
    let (kind, n, icon) = match chip {
        Chip::Image { n, image } if image.kind == AttachmentKind::Video => ("video", *n, "🎞"),
        Chip::Image { n, .. } => ("image", *n, "🖼"),
        Chip::Paste { n, .. } => ("paste", *n as usize, "📄"),
    };
    let sgr = attachment_sgr(kind, n);
    let (caption, interior): (String, Vec<String>) = match chip {
        Chip::Image { image, .. } => {
            let caption = image.dims.map(|(w, h)| format!("{w}x{h}")).unwrap_or_default();
            let pad = INNER_COLS - visible_width(icon);
            let icon_row = format!("{}{icon}{}", " ".repeat(pad / 2), " ".repeat(pad - pad / 2));
            let blank = " ".repeat(INNER_COLS);
            (caption, vec![blank.clone(), icon_row, blank.clone(), blank])
        }
        Chip::Paste { content, .. } => {
            let lines = content.split('\n').count();
            let caption = if lines > 1 {
                format!("+{lines} lines")
            } else {
                format!("{} chars", content.chars().count())
            };
            let rows = (0..INNER_ROWS)
                .map(|r| {
                    let line = content.split('\n').nth(r).unwrap_or("").replace('\t', "   ");
                    let cut = truncate_to_width(&line, INNER_COLS, "…", false);
                    let pad = INNER_COLS.saturating_sub(visible_width(&cut));
                    format!("{cut}{}", " ".repeat(pad))
                })
                .collect();
            (caption, rows)
        }
    };
    let vertical = format!("{sgr}│\x1b[39m");
    let mut out = vec![border_row(&sgr, &format!("{icon} #{n}"), true)];
    out.extend(interior.iter().map(|row| format!("{vertical}{row}{vertical}")));
    out.push(border_row(&sgr, &caption, false));
    out
}

/// Chips for everything the draft still references: large pastes (by marker) and staged images.
pub fn draft_chips<'a>(text: &str, pastes: &'a [(u64, String)], attachments: &'a Attachments) -> Vec<Chip<'a>> {
    let mut chips: Vec<Chip> = attachments
        .referenced(text)
        .into_iter()
        .map(|(n, image)| Chip::Image { n, image })
        .collect();
    chips.extend(
        pastes
            .iter()
            .filter(|(id, _)| text.contains(&format!("[paste #{id} ")) || text.contains(&format!("[paste #{id}]")))
            .map(|(id, content)| Chip::Paste { n: *id, content }),
    );
    chips
}

/// The attachment band: six rows of side-by-side cards; cards that do not fit `width` are left out.
pub fn chip_band(chips: &[Chip], width: usize) -> Vec<String> {
    if chips.is_empty() {
        return Vec::new();
    }
    let mut rows = vec![String::new(); INNER_ROWS + 2];
    let mut x = 0;
    for chip in chips {
        let gap = if x > 0 { CARD_GAP } else { 0 };
        if x + gap + CARD_COLS > width {
            break;
        }
        for (row, part) in rows.iter_mut().zip(card(chip)) {
            row.push_str(&" ".repeat(gap));
            row.push_str(&part);
        }
        x += gap + CARD_COLS;
    }
    rows
}

/// A canceled draft kept for recall: `Up` on an empty editor brings it back before history.
#[derive(Clone, Debug, Default)]
pub struct DraftRecall {
    saved: Option<(String, Attachments)>,
}

impl DraftRecall {
    /// Keep `text` (and its attachments) when the user clears a non-blank draft.
    pub fn remember(&mut self, text: &str, attachments: &Attachments) {
        if !text.trim().is_empty() {
            self.saved = Some((text.to_string(), attachments.clone()));
        }
    }

    pub fn has_draft(&self) -> bool {
        self.saved.is_some()
    }

    /// On `Up` with an empty editor: the saved draft, once. `None` lets `Up` browse history as usual.
    pub fn recall(&mut self, editor_text: &str) -> Option<(String, Attachments)> {
        if !editor_text.is_empty() {
            return None;
        }
        self.saved.take()
    }
}

/// What a completed paste should become.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PasteAction {
    /// Every segment names a local image or video: attach them instead of inserting the text.
    Attach(Vec<String>),
    /// Insert as text (the editor collapses a large paste into a marker).
    Text(String),
}

/// Route a completed bracketed paste (raw pastes skip this and always insert text).
pub fn classify_paste(content: &str) -> PasteAction {
    let decoded = decode_reencoded_paste_controls(content);
    match extract_image_paste_paths_from_text(&decoded) {
        Some(paths) => PasteAction::Attach(paths),
        None => PasteAction::Text(decoded),
    }
}
