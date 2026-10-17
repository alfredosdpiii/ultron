//! Inline terminal images: Kitty and iTerm2 encoding, image header sizing, cell math, capability detection and the
//! Image component (ports of packages/tui/src/terminal-image.ts and components/image.ts). Capabilities, cell size
//! and the environment are passed in instead of read from process globals.

use crate::components::Component;
use crate::select::StyleFn;
use crate::utils::truncate_to_width;
use std::sync::{LazyLock, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    Iterm2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub images: Option<ImageProtocol>,
    pub true_color: bool,
    pub hyperlinks: bool,
}

/// `detectCapabilities`: environment detection plus the PI_HYPERLINKS, PI_IMAGE_PROTOCOL and PI_TRUE_COLOR overrides.
/// `env` reads a variable, `tmux_forwards_hyperlinks` is the tmux probe result.
pub fn detect_capabilities(
    env: &dyn Fn(&str) -> Option<String>,
    windows: bool,
    tmux_forwards_hyperlinks: bool,
) -> Capabilities {
    let flag = |k: &str| match env(k).as_deref() {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    };
    let hyperlinks = flag("PI_HYPERLINKS");
    let mut caps = detect_from_environment(env, windows, hyperlinks.unwrap_or(tmux_forwards_hyperlinks));
    match env("PI_IMAGE_PROTOCOL").map(|v| v.to_lowercase()).as_deref() {
        Some("kitty") => caps.images = Some(ImageProtocol::Kitty),
        Some("iterm2") => caps.images = Some(ImageProtocol::Iterm2),
        Some("none" | "0") => caps.images = None,
        _ => {}
    }
    if let Some(t) = flag("PI_TRUE_COLOR") {
        caps.true_color = t;
    }
    if let Some(h) = hyperlinks {
        caps.hyperlinks = h;
    }
    caps
}

fn detect_from_environment(
    env: &dyn Fn(&str) -> Option<String>,
    windows: bool,
    tmux_forwards_hyperlinks: bool,
) -> Capabilities {
    let get = |k: &str| env(k).unwrap_or_default().to_lowercase();
    let has = |k: &str| env(k).is_some_and(|v| !v.is_empty());
    let (program, emulator, term, color) = (
        get("TERM_PROGRAM"),
        get("TERMINAL_EMULATOR"),
        get("TERM"),
        get("COLORTERM"),
    );
    let tc_hint = color == "truecolor" || color == "24bit";
    let caps = |images, true_color, hyperlinks| Capabilities {
        images,
        true_color,
        hyperlinks,
    };
    let kitty = caps(Some(ImageProtocol::Kitty), true, true);
    if has("TMUX") || term.starts_with("tmux") {
        return caps(None, tc_hint, tmux_forwards_hyperlinks);
    }
    if term.starts_with("screen") {
        return caps(None, tc_hint, false);
    }
    if has("KITTY_WINDOW_ID") || program == "kitty" {
        return kitty;
    }
    if program == "ghostty" || term.contains("ghostty") || has("GHOSTTY_RESOURCES_DIR") {
        return kitty;
    }
    if has("WEZTERM_PANE") || program == "wezterm" {
        return kitty;
    }
    if program == "warpterminal" || has("WARP_SESSION_ID") || has("WARP_TERMINAL_SESSION_UUID") {
        return kitty;
    }
    if has("ITERM_SESSION_ID") || program == "iterm.app" {
        return caps(Some(ImageProtocol::Iterm2), true, true);
    }
    if has("WT_SESSION") || ["alacritty", "vscode", "zed"].contains(&program.as_str()) {
        return caps(None, true, true);
    }
    if emulator == "jetbrains-jediterm" {
        return caps(None, true, false);
    }
    if windows {
        return caps(None, true, false);
    }
    caps(None, tc_hint, false)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dimensions {
    pub width_px: u32,
    pub height_px: u32,
}

pub const DEFAULT_CELL: Dimensions = Dimensions {
    width_px: 9,
    height_px: 18,
};

const CHUNK_SIZE: usize = 4096;

/// Kitty graphics transmit+display, chunked at 4096 base64 characters.
pub fn encode_kitty(
    b64: &str,
    columns: Option<u32>,
    rows: Option<u32>,
    image_id: Option<u32>,
    move_cursor: bool,
) -> String {
    let mut params = vec!["a=T".to_string(), "f=100".into(), "q=2".into()];
    if !move_cursor {
        params.push("C=1".into());
    }
    for (k, v) in [("c", columns), ("r", rows), ("i", image_id)] {
        if let Some(v) = v.filter(|v| *v != 0) {
            params.push(format!("{k}={v}"));
        }
    }
    let params = params.join(",");
    if b64.len() <= CHUNK_SIZE {
        return format!("\x1b_G{params};{b64}\x1b\\");
    }
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(CHUNK_SIZE)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    let last = chunks.len() - 1;
    chunks
        .iter()
        .enumerate()
        .map(|(i, c)| match i {
            0 => format!("\x1b_G{params},m=1;{c}\x1b\\"),
            i if i == last => format!("\x1b_Gm=0;{c}\x1b\\"),
            _ => format!("\x1b_Gm=1;{c}\x1b\\"),
        })
        .collect()
}

pub fn delete_kitty_image(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image_id},q=2\x1b\\")
}

pub const DELETE_ALL_KITTY_IMAGES: &str = "\x1b_Ga=d,d=A,q=2\x1b\\";
pub const DELETE_ALL_KITTY_PLACEMENTS: &str = "\x1b_Ga=d,d=a,q=2\x1b\\";

/// Node's `Buffer.byteLength(s, "base64")`.
fn base64_byte_len(s: &str) -> usize {
    let b = s.as_bytes();
    let mut len = b.len();
    for _ in 0..2 {
        if len > 0 && b[len - 1] == b'=' {
            len -= 1;
        }
    }
    (len * 3) >> 2
}

/// Lenient base64 decode like Node's `Buffer.from(s, "base64")`: skips unknown characters, accepts url-safe ones,
/// stops at padding.
fn base64_decode(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn base64_encode(data: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n =
            (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= c.len() {
                A[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// iTerm2 inline image (OSC 1337).
pub fn encode_iterm2(
    b64: &str,
    width: Option<&str>,
    height: Option<&str>,
    name: Option<&str>,
    preserve_aspect: bool,
    inline: bool,
) -> String {
    let mut params = vec![
        format!("inline={}", u8::from(inline)),
        format!("size={}", base64_byte_len(b64)),
    ];
    if let Some(w) = width {
        params.push(format!("width={w}"));
    }
    if let Some(h) = height {
        params.push(format!("height={h}"));
    }
    if let Some(n) = name.filter(|n| !n.is_empty()) {
        params.push(format!("name={}", base64_encode(n.as_bytes())));
    }
    if !preserve_aspect {
        params.push("preserveAspectRatio=0".into());
    }
    format!("\x1b]1337;File={}:{b64}\x07", params.join(";"))
}

/// Columns and rows an image occupies when scaled to fit `max_width` (and `max_height`) cells.
pub fn calculate_image_cell_size(
    image: Dimensions,
    max_width: u32,
    max_height: Option<u32>,
    cell: Dimensions,
) -> (u32, u32) {
    let max_w = max_width.max(1);
    let max_h = max_height.map(|h| h.max(1));
    let (iw, ih) = (f64::from(image.width_px.max(1)), f64::from(image.height_px.max(1)));
    let ws = f64::from(max_w * cell.width_px) / iw;
    let hs = max_h.map_or(ws, |h| f64::from(h * cell.height_px) / ih);
    let scale = ws.min(hs);
    let columns = (iw * scale / f64::from(cell.width_px)).ceil() as u32;
    let rows = (ih * scale / f64::from(cell.height_px)).ceil() as u32;
    (columns.min(max_w).max(1), max_h.map_or(rows, |h| h.min(rows)).max(1))
}

fn u16be(b: &[u8], o: usize) -> u32 {
    u32::from(b[o]) << 8 | u32::from(b[o + 1])
}

fn u16le(b: &[u8], o: usize) -> u32 {
    u32::from(b[o + 1]) << 8 | u32::from(b[o])
}

/// Pixel size from a PNG, JPEG, GIF or WebP header.
pub fn image_dimensions(b64: &str, mime: &str) -> Option<Dimensions> {
    let b = base64_decode(b64);
    let d = |w, h| {
        Some(Dimensions {
            width_px: w,
            height_px: h,
        })
    };
    match mime {
        "image/png" => {
            if b.len() < 24 || b[..4] != [0x89, 0x50, 0x4e, 0x47] {
                return None;
            }
            d(
                u32::from_be_bytes([b[16], b[17], b[18], b[19]]),
                u32::from_be_bytes([b[20], b[21], b[22], b[23]]),
            )
        }
        "image/jpeg" => {
            if b.len() < 2 || b[0] != 0xff || b[1] != 0xd8 {
                return None;
            }
            let mut o = 2;
            while o + 9 < b.len() {
                if b[o] != 0xff {
                    o += 1;
                    continue;
                }
                if (0xc0..=0xc2).contains(&b[o + 1]) {
                    return d(u16be(&b, o + 7), u16be(&b, o + 5));
                }
                let len = u16be(&b, o + 2) as usize;
                if len < 2 {
                    return None;
                }
                o += 2 + len;
            }
            None
        }
        "image/gif" => {
            if b.len() < 10 || (&b[..6] != b"GIF87a" && &b[..6] != b"GIF89a") {
                return None;
            }
            d(u16le(&b, 6), u16le(&b, 8))
        }
        "image/webp" => {
            if b.len() < 30 || &b[..4] != b"RIFF" || &b[8..12] != b"WEBP" {
                return None;
            }
            match &b[12..16] {
                b"VP8 " => d(u16le(&b, 26) & 0x3fff, u16le(&b, 28) & 0x3fff),
                b"VP8L" => {
                    let bits = u32::from_le_bytes([b[21], b[22], b[23], b[24]]);
                    d((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1)
                }
                b"VP8X" => {
                    let v = |o: usize| (u32::from(b[o]) | u32::from(b[o + 1]) << 8 | u32::from(b[o + 2]) << 16) + 1;
                    d(v(24), v(27))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

#[derive(Clone, Copy, Debug)]
pub struct RenderOptions {
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub image_id: Option<u32>,
    pub move_cursor: bool,
}

pub struct RenderedImage {
    pub sequence: String,
    pub columns: u32,
    pub rows: u32,
}

/// Escape sequence for an image fitted to `max_width` x `max_height` cells, or None without image support.
pub fn render_image(
    b64: &str,
    image: Dimensions,
    caps: &Capabilities,
    cell: Dimensions,
    opts: RenderOptions,
) -> Option<RenderedImage> {
    let (columns, rows) = calculate_image_cell_size(image, opts.max_width.unwrap_or(80), opts.max_height, cell);
    let sequence = match caps.images? {
        ImageProtocol::Kitty => {
            if let Some(image_id) = opts.image_id {
                register_kitty_image_metadata(KittyImageMetadata {
                    image_id,
                    columns,
                    rows,
                    width_px: image.width_px,
                    height_px: image.height_px,
                });
            }
            encode_kitty(b64, Some(columns), Some(rows), opts.image_id, opts.move_cursor)
        }
        ImageProtocol::Iterm2 => encode_iterm2(b64, Some(&columns.to_string()), Some("auto"), None, true, true),
    };
    Some(RenderedImage {
        sequence,
        columns,
        rows,
    })
}

/// OSC 8 hyperlink.
pub fn hyperlink(text: &str, url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// `pathToFileURL(path).href` for absolute POSIX paths.
fn file_url(path: &str) -> String {
    let mut out = String::from("file://");
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Text shown instead of an image. `home` shortens home-prefixed paths to `~/...`.
pub fn image_fallback(
    mime: &str,
    dims: Option<Dimensions>,
    filename: Option<&str>,
    hyperlinks: bool,
    home: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    if let Some(f) = filename.filter(|f| !f.is_empty()) {
        let display = match home.filter(|h| !h.is_empty()) {
            Some(h) if f == h || f.starts_with(&format!("{h}/")) || f.starts_with(&format!("{h}\\")) => {
                format!("~{}", &f[h.len()..])
            }
            _ => f.to_string(),
        };
        parts.push(if hyperlinks && f.starts_with('/') {
            hyperlink(&display, &file_url(f))
        } else {
            display
        });
    }
    parts.push(format!("[{mime}]"));
    if let Some(d) = dims {
        parts.push(format!("{}x{}", d.width_px, d.height_px));
    }
    format!("[Image: {}]", parts.join(" "))
}

/// Inline image component; falls back to a one-line description without image support.
pub struct Image {
    pub b64: String,
    pub mime: String,
    pub dimensions: Dimensions,
    pub fallback_color: StyleFn,
    pub caps: Capabilities,
    pub cell: Dimensions,
    pub max_width_cells: Option<u32>,
    pub max_height_cells: Option<u32>,
    pub filename: Option<String>,
    pub home: Option<String>,
    /// Kitty image id; the owner allocates one (TS: random in [1, 0xffffffff]) before rendering on Kitty.
    pub image_id: Option<u32>,
}

impl Image {
    pub fn new(b64: &str, mime: &str, fallback_color: StyleFn, caps: Capabilities) -> Self {
        Self {
            b64: b64.to_string(),
            mime: mime.to_string(),
            dimensions: image_dimensions(b64, mime).unwrap_or(Dimensions {
                width_px: 800,
                height_px: 600,
            }),
            fallback_color,
            caps,
            cell: DEFAULT_CELL,
            max_width_cells: None,
            max_height_cells: None,
            filename: None,
            home: None,
            image_id: None,
        }
    }
}

impl Component for Image {
    fn render(&mut self, width: usize) -> Vec<String> {
        let max_w = (width as i64 - 2)
            .min(i64::from(self.max_width_cells.unwrap_or(60)))
            .max(1) as u32;
        let default_h = (f64::from(max_w * self.cell.width_px) / f64::from(self.cell.height_px))
            .ceil()
            .max(1.0) as u32;
        let max_h = self.max_height_cells.unwrap_or(default_h);
        let rendered = render_image(
            &self.b64,
            self.dimensions,
            &self.caps,
            self.cell,
            RenderOptions {
                max_width: Some(max_w),
                max_height: Some(max_h),
                image_id: self.image_id,
                move_cursor: false,
            },
        );
        match (rendered, self.caps.images) {
            (Some(r), Some(ImageProtocol::Kitty)) => {
                let mut lines = vec![r.sequence];
                lines.resize(r.rows as usize, String::new());
                lines
            }
            (Some(r), _) => {
                let offset = r.rows - 1;
                let up = if offset > 0 {
                    format!("\x1b[{offset}A")
                } else {
                    String::new()
                };
                let mut lines = vec![String::new(); offset as usize];
                lines.push(format!("{up}{}", r.sequence));
                lines
            }
            (None, _) => {
                let fb = image_fallback(
                    &self.mime,
                    Some(self.dimensions),
                    self.filename.as_deref(),
                    self.caps.hyperlinks,
                    self.home.as_deref(),
                );
                vec![truncate_to_width(&(self.fallback_color)(&fb), width, "...", false)]
            }
        }
    }
}

/// `allocateImageId`: a random Kitty image id in `[1, 0xfffffffe]` (random so separate renderers never collide).
pub fn allocate_image_id() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    (hasher.finish() % 0xffff_fffe) as u32 + 1
}

/// `KittyImageMetadata`: what a placed Kitty image covers, for cropping and placement-only redraws.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KittyImageMetadata {
    pub image_id: u32,
    pub columns: u32,
    pub rows: u32,
    pub width_px: u32,
    pub height_px: u32,
}

/// `KittyImagePlacement`: a placement-only command for a transmitted image line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KittyImagePlacement {
    pub image_id: u32,
    pub transmission_generation: u64,
    pub transmission_bytes: usize,
    pub estimated_decoded_bytes: u64,
    pub sequence: String,
    pub replacement_line: String,
}

/// The process registry of transmitted images (TypeScript's module map): newest last, at most 1000.
struct Registry {
    images: Vec<(KittyImageMetadata, u64)>,
    generation: u64,
}

static KITTY_IMAGES: LazyLock<Mutex<Registry>> = LazyLock::new(|| {
    Mutex::new(Registry {
        images: Vec::new(),
        generation: 0,
    })
});

fn registry() -> std::sync::MutexGuard<'static, Registry> {
    KITTY_IMAGES.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `registerKittyImageMetadata`: each registration is a new transmission generation.
pub fn register_kitty_image_metadata(metadata: KittyImageMetadata) {
    let mut registry = registry();
    registry.generation += 1;
    let generation = registry.generation;
    registry.images.retain(|(m, _)| m.image_id != metadata.image_id);
    registry.images.push((metadata, generation));
    if registry.images.len() > 1000 {
        registry.images.remove(0);
    }
}

const KITTY_PREFIX: &str = "\x1b_G";

/// `/\x1b_G([^;]*);/`: where the first Kitty command starts, its controls, and where its payload starts.
fn kitty_command(line: &str) -> Option<(usize, &str, usize)> {
    let at = line.find(KITTY_PREFIX)?;
    let controls_start = at + KITTY_PREFIX.len();
    let end = controls_start + line[controls_start..].find(';')?;
    Some((at, &line[controls_start..end], end + 1))
}

fn has_control(controls: &str, key: &str, value: &str) -> bool {
    controls.split(',').any(|c| c.split_once('=') == Some((key, value)))
}

fn registered(line: &str) -> Option<(KittyImageMetadata, u64)> {
    let (_, controls, _) = kitty_command(line)?;
    let id = controls.split(',').find_map(|c| {
        let digits = c.strip_prefix("i=")?;
        (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then(|| digits.parse::<u64>().ok())?
    })?;
    registry()
        .images
        .iter()
        .find(|(m, _)| u64::from(m.image_id) == id)
        .copied()
}

/// `getKittyImageMetadata`: the registered image a line transmits.
pub fn kitty_image_metadata(line: &str) -> Option<KittyImageMetadata> {
    registered(line).map(|(m, _)| m)
}

/// Controls a placement keeps (`KITTY_PLACEMENT_CONTROL_KEYS`).
const PLACEMENT_KEYS: [&str; 17] = [
    "i", "p", "x", "y", "w", "h", "X", "Y", "c", "r", "C", "U", "z", "P", "Q", "H", "V",
];

/// `getKittyImagePlacement`: the line with its transmission (every chunk) replaced by a placement of the image
/// already uploaded.
pub fn kitty_image_placement(line: &str) -> Option<KittyImagePlacement> {
    let (at, first_controls, _) = kitty_command(line)?;
    let (metadata, generation) = registered(line)?;
    let mut command_start = at;
    let mut controls = first_controls.to_string();
    let transmission_end = loop {
        let terminator =
            command_start + KITTY_PREFIX.len() + line[command_start + KITTY_PREFIX.len()..].find("\x1b\\")?;
        let end = terminator + 2;
        if !has_control(&controls, "m", "1") {
            break end;
        }
        command_start = end;
        if !line[command_start..].starts_with(KITTY_PREFIX) {
            return None;
        }
        let from = command_start + KITTY_PREFIX.len();
        let controls_end = from + line[from..].find(';')?;
        controls = line[from..controls_end].to_string();
    };
    let kept: Vec<&str> = first_controls
        .split(',')
        .filter(|c| PLACEMENT_KEYS.contains(&c.split('=').next().unwrap_or("")))
        .collect();
    let sequence = format!("\x1b_Ga=p,q=2,{}\x1b\\", kept.join(","));
    Some(KittyImagePlacement {
        image_id: metadata.image_id,
        transmission_generation: generation,
        transmission_bytes: transmission_end - at,
        estimated_decoded_bytes: u64::from(metadata.width_px) * u64::from(metadata.height_px) * 4,
        replacement_line: format!("{}{sequence}{}", &line[..at], &line[transmission_end..]),
        sequence,
    })
}

/// `cropKittyImageLine`: draw only `visible_rows` of the image, starting `hidden_rows` into it (source rows in
/// pixels, `y`/`h`, and the cell rows `r`).
pub fn crop_kitty_image_line(line: &str, hidden_rows: i64, visible_rows: i64) -> String {
    let (Some(metadata), Some((at, controls, payload))) = (kitty_image_metadata(line), kitty_command(line)) else {
        return line.to_string();
    };
    let rows = i64::from(metadata.rows);
    if hidden_rows < 0 || hidden_rows >= rows || visible_rows <= 0 {
        return line.to_string();
    }
    let cropped = visible_rows.min(rows - hidden_rows);
    if hidden_rows == 0 && cropped == rows {
        return line.to_string();
    }
    let height = i64::from(metadata.height_px);
    let source_y = (height * hidden_rows).div_euclid(rows);
    let source_end = (height * (hidden_rows + cropped) + rows - 1).div_euclid(rows);
    let source_height = (height.min(source_end) - source_y).max(1);
    let mut kept: Vec<String> = controls
        .split(',')
        .filter(|c| !["y=", "h=", "r="].iter().any(|p| c.starts_with(p)))
        .map(str::to_string)
        .collect();
    kept.extend([
        format!("y={source_y}"),
        format!("h={source_height}"),
        format!("r={cropped}"),
    ]);
    format!("{}\x1b_G{};{}", &line[..at], kept.join(","), &line[payload..])
}
