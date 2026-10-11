//! Key parsing for legacy, xterm modifyOtherKeys and Kitty keyboard sequences (port of packages/tui/src/keys.ts).

pub mod matching;
pub use matching::matches_key;

const SHIFT: u32 = 1;
const ALT: u32 = 2;
const CTRL: u32 = 4;
const SUPER: u32 = 8;
const LOCK_MASK: u32 = 64 + 128;

const UP: i64 = -1;
const DOWN: i64 = -2;
const RIGHT: i64 = -3;
const LEFT: i64 = -4;
const DELETE: i64 = -10;
const INSERT: i64 = -11;
const PAGE_UP: i64 = -12;
const PAGE_DOWN: i64 = -13;
const HOME: i64 = -14;
const END: i64 = -15;

const SYMBOL_KEYS: &str = "`-=[]\\;',./!@#$%^&*()_+|~{}:<>?";

fn is_symbol(cp: i64) -> bool {
    u8::try_from(cp).is_ok_and(|b| SYMBOL_KEYS.as_bytes().contains(&b))
}

fn normalize_kitty_functional(cp: i64) -> i64 {
    match cp {
        57399..=57408 => cp - 57399 + 48,
        57409 => 46,
        57410 => 47,
        57411 => 42,
        57412 => 45,
        57413 => 43,
        57415 => 61,
        57416 => 44,
        57417 => LEFT,
        57418 => RIGHT,
        57419 => UP,
        57420 => DOWN,
        57421 => PAGE_UP,
        57422 => PAGE_DOWN,
        57423 => HOME,
        57424 => END,
        57425 => INSERT,
        57426 => DELETE,
        _ => cp,
    }
}

/// Kitty event type (flag 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEventType {
    Press,
    Repeat,
    Release,
}

fn event_type(s: Option<&str>) -> KeyEventType {
    match s.and_then(|s| s.parse::<u32>().ok()) {
        Some(2) => KeyEventType::Repeat,
        Some(3) => KeyEventType::Release,
        _ => KeyEventType::Press,
    }
}

struct Kitty {
    codepoint: i64,
    base_layout: Option<i64>,
    modifier: u32,
    event: KeyEventType,
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn num(s: &str) -> i64 {
    s.parse().unwrap_or(i64::MAX)
}

/// `^\x1b\[(\d+)(?::(\d*))?(?::(\d+))?(?:;(\d+))?(?::(\d+))?u$` -> (cp, shifted, base, mod, event)
#[allow(clippy::type_complexity)]
fn match_csi_u(data: &str) -> Option<(&str, Option<&str>, Option<&str>, Option<&str>, Option<&str>)> {
    let body = data.strip_prefix("\x1b[")?.strip_suffix('u')?;
    let (keys, mods) = match body.split_once(';') {
        Some((k, m)) => (k, Some(m)),
        None => (body, None),
    };
    let mut kp = keys.split(':');
    let cp = kp.next()?;
    let shifted = kp.next();
    let base = kp.next();
    // Without `;mod`, a fourth `:n` group is the regex's trailing event-type group.
    let trailing_event = kp.next();
    if kp.next().is_some()
        || (trailing_event.is_some() && mods.is_some())
        || !digits(cp)
        || base.is_some_and(|b| !digits(b))
        || trailing_event.is_some_and(|e| !digits(e))
    {
        return None;
    }
    if shifted.is_some_and(|s| !s.is_empty() && !digits(s)) {
        return None;
    }
    let (modv, ev) = match mods {
        None => (None, trailing_event),
        Some(m) => {
            let mut mp = m.split(':');
            let mv = mp.next()?;
            let ev = mp.next();
            if mp.next().is_some() || !digits(mv) || ev.is_some_and(|e| !digits(e)) {
                return None;
            }
            (Some(mv), ev)
        }
    };
    Some((cp, shifted, base, modv, ev))
}

/// `^\x1b\[1;(\d+)(?::(\d+))?([<finals>])$`
fn match_csi1(data: &str, finals: &str) -> Option<(i64, KeyEventType, char)> {
    let body = data.strip_prefix("\x1b[1;")?;
    let last = body.chars().last()?;
    if !finals.contains(last) {
        return None;
    }
    let body = &body[..body.len() - 1];
    let (m, ev) = match body.split_once(':') {
        Some((m, e)) => (m, Some(e)),
        None => (body, None),
    };
    if !digits(m) || ev.is_some_and(|e| !digits(e)) {
        return None;
    }
    Some((num(m), event_type(ev), last))
}

fn parse_kitty(data: &str) -> Option<Kitty> {
    if let Some((cp, _, base, modv, ev)) = match_csi_u(data) {
        return Some(Kitty {
            codepoint: num(cp),
            base_layout: base.map(num),
            modifier: modv.map_or(1, num).wrapping_sub(1) as u32,
            event: event_type(ev),
        });
    }
    if let Some((m, event, c)) = match_csi1(data, "ABCD") {
        let codepoint = match c {
            'A' => UP,
            'B' => DOWN,
            'C' => RIGHT,
            _ => LEFT,
        };
        return Some(Kitty {
            codepoint,
            base_layout: None,
            modifier: (m - 1) as u32,
            event,
        });
    }
    // `^\x1b\[(\d+)(?:;(\d+))?(?::(\d+))?~$`
    if let Some(body) = data.strip_prefix("\x1b[").and_then(|b| b.strip_suffix('~')) {
        let (head, ev) = match body.split_once(':') {
            Some((h, e)) => (h, Some(e)),
            None => (body, None),
        };
        let (k, m) = match head.split_once(';') {
            Some((k, m)) => (k, Some(m)),
            None => (head, None),
        };
        if digits(k) && m.is_none_or(digits) && ev.is_none_or(digits) {
            let codepoint = match num(k) {
                2 => Some(INSERT),
                3 => Some(DELETE),
                5 => Some(PAGE_UP),
                6 => Some(PAGE_DOWN),
                7 => Some(HOME),
                8 => Some(END),
                _ => None,
            };
            if let Some(codepoint) = codepoint {
                return Some(Kitty {
                    codepoint,
                    base_layout: None,
                    modifier: (m.map_or(1, num) - 1) as u32,
                    event: event_type(ev),
                });
            }
        }
    }
    if let Some((m, event, c)) = match_csi1(data, "HF") {
        let codepoint = if c == 'H' { HOME } else { END };
        return Some(Kitty {
            codepoint,
            base_layout: None,
            modifier: (m - 1) as u32,
            event,
        });
    }
    None
}

/// `^\x1b\[27;(\d+);(\d+)~$` -> (codepoint, modifier)
fn parse_modify_other_keys(data: &str) -> Option<(i64, u32)> {
    let body = data.strip_prefix("\x1b[27;")?.strip_suffix('~')?;
    let (m, cp) = body.split_once(';')?;
    if !digits(m) || !digits(cp) {
        return None;
    }
    Some((num(cp), (num(m) - 1) as u32))
}

fn format_with_modifiers(name: &str, modifier: u32) -> Option<String> {
    let m = modifier & !LOCK_MASK;
    if m & !(SHIFT | CTRL | ALT | SUPER) != 0 {
        return None;
    }
    let mut out = String::new();
    for (bit, label) in [(SHIFT, "shift+"), (CTRL, "ctrl+"), (ALT, "alt+"), (SUPER, "super+")] {
        if m & bit != 0 {
            out.push_str(label);
        }
    }
    out.push_str(name);
    Some(out)
}

fn format_parsed(codepoint: i64, modifier: u32, base_layout: Option<i64>) -> Option<String> {
    let mut identity = normalize_kitty_functional(codepoint);
    if (modifier & !LOCK_MASK) & SHIFT != 0 && (65..=90).contains(&identity) {
        identity += 32;
    }
    let authoritative = (97..=122).contains(&identity) || (48..=57).contains(&identity) || is_symbol(identity);
    let cp = if authoritative {
        identity
    } else {
        base_layout.unwrap_or(identity)
    };
    let name = match cp {
        27 => "escape".to_string(),
        9 => "tab".to_string(),
        13 | 57414 => "enter".to_string(),
        32 => "space".to_string(),
        127 => "backspace".to_string(),
        DELETE => "delete".to_string(),
        INSERT => "insert".to_string(),
        HOME => "home".to_string(),
        END => "end".to_string(),
        PAGE_UP => "pageUp".to_string(),
        PAGE_DOWN => "pageDown".to_string(),
        UP => "up".to_string(),
        DOWN => "down".to_string(),
        LEFT => "left".to_string(),
        RIGHT => "right".to_string(),
        48..=57 | 97..=122 => char::from(cp as u8).to_string(),
        _ if is_symbol(cp) => char::from(cp as u8).to_string(),
        _ => return None,
    };
    format_with_modifiers(&name, modifier)
}

fn legacy_key_id(data: &str) -> Option<&'static str> {
    Some(match data {
        "\x1bOA" => "up",
        "\x1bOB" => "down",
        "\x1bOC" => "right",
        "\x1bOD" => "left",
        "\x1bOH" => "home",
        "\x1bOF" => "end",
        "\x1b[E" | "\x1bOE" => "clear",
        "\x1bOe" => "ctrl+clear",
        "\x1b[e" => "shift+clear",
        "\x1b[2~" => "insert",
        "\x1b[2$" => "shift+insert",
        "\x1b[2^" => "ctrl+insert",
        "\x1b[3$" => "shift+delete",
        "\x1b[3^" => "ctrl+delete",
        "\x1b[[5~" => "pageUp",
        "\x1b[[6~" => "pageDown",
        "\x1b[a" => "shift+up",
        "\x1b[b" => "shift+down",
        "\x1b[c" => "shift+right",
        "\x1b[d" => "shift+left",
        "\x1bOa" => "ctrl+up",
        "\x1bOb" => "ctrl+down",
        "\x1bOc" => "ctrl+right",
        "\x1bOd" => "ctrl+left",
        "\x1b[5$" => "shift+pageUp",
        "\x1b[6$" => "shift+pageDown",
        "\x1b[7$" => "shift+home",
        "\x1b[8$" => "shift+end",
        "\x1b[5^" => "ctrl+pageUp",
        "\x1b[6^" => "ctrl+pageDown",
        "\x1b[7^" => "ctrl+home",
        "\x1b[8^" => "ctrl+end",
        "\x1bOP" | "\x1b[11~" | "\x1b[[A" => "f1",
        "\x1bOQ" | "\x1b[12~" | "\x1b[[B" => "f2",
        "\x1bOR" | "\x1b[13~" | "\x1b[[C" => "f3",
        "\x1bOS" | "\x1b[14~" | "\x1b[[D" => "f4",
        "\x1b[[E" | "\x1b[15~" => "f5",
        "\x1b[17~" => "f6",
        "\x1b[18~" => "f7",
        "\x1b[19~" => "f8",
        "\x1b[20~" => "f9",
        "\x1b[21~" => "f10",
        "\x1b[23~" => "f11",
        "\x1b[24~" => "f12",
        "\x1bb" => "alt+left",
        "\x1bf" => "alt+right",
        "\x1bp" => "alt+up",
        "\x1bn" => "alt+down",
        _ => return None,
    })
}

/// Parsing context the TS module reads from globals: Kitty protocol state and the Windows Terminal session check.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyContext {
    pub kitty_active: bool,
    pub windows_terminal: bool,
}

/// Key identifier (`ctrl+shift+a`, `pageUp`, ...) for one input sequence.
pub fn parse_key(data: &str, ctx: KeyContext) -> Option<String> {
    if let Some(k) = parse_kitty(data) {
        return format_parsed(k.codepoint, k.modifier, k.base_layout);
    }
    if let Some((cp, m)) = parse_modify_other_keys(data) {
        return format_parsed(cp, m, None);
    }
    let kitty = ctx.kitty_active;
    if kitty && (data == "\x1b\r" || data == "\n") {
        return Some("shift+enter".into());
    }
    if let Some(id) = legacy_key_id(data) {
        return Some(id.into());
    }
    let fixed = match data {
        "\x1b" => Some("escape"),
        "\x1c" => Some("ctrl+\\"),
        "\x1d" => Some("ctrl+]"),
        "\x1f" => Some("ctrl+-"),
        "\x1b\x1b" => Some("ctrl+alt+["),
        "\x1b\x1c" => Some("ctrl+alt+\\"),
        "\x1b\x1d" => Some("ctrl+alt+]"),
        "\x1b\x1f" => Some("ctrl+alt+-"),
        "\t" => Some("tab"),
        "\r" | "\x1bOM" => Some("enter"),
        "\n" if !kitty => Some("enter"),
        "\x00" => Some("ctrl+space"),
        " " => Some("space"),
        "\x7f" => Some("backspace"),
        "\x08" => Some(if ctx.windows_terminal {
            "ctrl+backspace"
        } else {
            "backspace"
        }),
        "\x1b[Z" => Some("shift+tab"),
        "\x1b\r" if !kitty => Some("alt+enter"),
        "\x1b " if !kitty => Some("alt+space"),
        "\x1b\x7f" | "\x1b\x08" => Some("alt+backspace"),
        "\x1bB" if !kitty => Some("alt+left"),
        "\x1bF" if !kitty => Some("alt+right"),
        _ => None,
    };
    if let Some(f) = fixed {
        return Some(f.into());
    }
    let units: Vec<u16> = data.encode_utf16().collect();
    if !kitty && units.len() == 2 && units[0] == 0x1b {
        let code = i64::from(units[1]);
        if (1..=26).contains(&code) {
            return Some(format!("ctrl+alt+{}", char::from((code + 96) as u8)));
        }
        if (97..=122).contains(&code) || (48..=57).contains(&code) || is_symbol(code) {
            return Some(format!("alt+{}", char::from(code as u8)));
        }
    }
    let arrows = match data {
        "\x1b[A" => Some("up"),
        "\x1b[B" => Some("down"),
        "\x1b[C" => Some("right"),
        "\x1b[D" => Some("left"),
        "\x1b[H" | "\x1bOH" => Some("home"),
        "\x1b[F" | "\x1bOF" => Some("end"),
        "\x1b[3~" => Some("delete"),
        "\x1b[5~" => Some("pageUp"),
        "\x1b[6~" => Some("pageDown"),
        _ => None,
    };
    if let Some(a) = arrows {
        return Some(a.into());
    }
    if units.len() == 1 {
        let code = units[0];
        if (1..=26).contains(&code) {
            return Some(format!("ctrl+{}", char::from((code + 96) as u8)));
        }
        if (32..=126).contains(&code) {
            return Some(data.to_string());
        }
    }
    None
}

/// Kitty event type of a sequence, when it is a Kitty sequence.
pub fn key_event_type(data: &str) -> Option<KeyEventType> {
    parse_kitty(data).map(|k| k.event)
}

/// Printable character carried by a Kitty CSI-u or modifyOtherKeys sequence (plain or shifted keys only).
pub fn decode_printable_key(data: &str) -> Option<String> {
    if let Some((cp, shifted, _, modv, _)) = match_csi_u(data) {
        let codepoint = num(cp);
        let modifier = (modv.map_or(1, num) - 1) as u32;
        if modifier & !(SHIFT | LOCK_MASK) != 0 || modifier & (ALT | CTRL) != 0 {
            return None;
        }
        let mut eff = codepoint;
        if modifier & SHIFT != 0
            && let Some(s) = shifted.filter(|s| !s.is_empty())
        {
            eff = num(s);
        }
        let eff = normalize_kitty_functional(eff);
        if eff < 32 {
            return None;
        }
        return u32::try_from(eff).ok().and_then(char::from_u32).map(String::from);
    }
    let (cp, m) = parse_modify_other_keys(data)?;
    if (m & !LOCK_MASK) & !SHIFT != 0 || cp < 32 {
        return None;
    }
    u32::try_from(cp).ok().and_then(char::from_u32).map(String::from)
}
