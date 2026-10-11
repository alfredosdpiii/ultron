//! `matchesKey` from packages/tui/src/keys.ts: does an input sequence match a key identifier such as `ctrl+shift+p`.

use super::{
    ALT, CTRL, DELETE, DOWN, END, HOME, INSERT, KeyContext, LEFT, LOCK_MASK, PAGE_DOWN, PAGE_UP, RIGHT, SHIFT, SUPER,
    UP, is_symbol, normalize_kitty_functional, parse_kitty, parse_modify_other_keys,
};

const ESCAPE: i64 = 27;
const TAB: i64 = 9;
const ENTER: i64 = 13;
const SPACE: i64 = 32;
const BACKSPACE: i64 = 127;
const KP_ENTER: i64 = 57414;

fn legacy(key: &str) -> &'static [&'static str] {
    match key {
        "up" => &["\x1b[A", "\x1bOA"],
        "down" => &["\x1b[B", "\x1bOB"],
        "right" => &["\x1b[C", "\x1bOC"],
        "left" => &["\x1b[D", "\x1bOD"],
        "home" => &["\x1b[H", "\x1bOH", "\x1b[1~", "\x1b[7~"],
        "end" => &["\x1b[F", "\x1bOF", "\x1b[4~", "\x1b[8~"],
        "insert" => &["\x1b[2~"],
        "delete" => &["\x1b[3~"],
        "pageup" => &["\x1b[5~", "\x1b[[5~"],
        "pagedown" => &["\x1b[6~", "\x1b[[6~"],
        "clear" => &["\x1b[E", "\x1bOE"],
        "f1" => &["\x1bOP", "\x1b[11~", "\x1b[[A"],
        "f2" => &["\x1bOQ", "\x1b[12~", "\x1b[[B"],
        "f3" => &["\x1bOR", "\x1b[13~", "\x1b[[C"],
        "f4" => &["\x1bOS", "\x1b[14~", "\x1b[[D"],
        "f5" => &["\x1b[15~", "\x1b[[E"],
        "f6" => &["\x1b[17~"],
        "f7" => &["\x1b[18~"],
        "f8" => &["\x1b[19~"],
        "f9" => &["\x1b[20~"],
        "f10" => &["\x1b[21~"],
        "f11" => &["\x1b[23~"],
        "f12" => &["\x1b[24~"],
        _ => &[],
    }
}

fn legacy_modified(data: &str, key: &str, modifier: u32) -> bool {
    let (shift, ctrl) = match key {
        "up" => ("\x1b[a", "\x1bOa"),
        "down" => ("\x1b[b", "\x1bOb"),
        "right" => ("\x1b[c", "\x1bOc"),
        "left" => ("\x1b[d", "\x1bOd"),
        "clear" => ("\x1b[e", "\x1bOe"),
        "insert" => ("\x1b[2$", "\x1b[2^"),
        "delete" => ("\x1b[3$", "\x1b[3^"),
        "pageup" => ("\x1b[5$", "\x1b[5^"),
        "pagedown" => ("\x1b[6$", "\x1b[6^"),
        "home" => ("\x1b[7$", "\x1b[7^"),
        "end" => ("\x1b[8$", "\x1b[8^"),
        _ => return false,
    };
    (modifier == SHIFT && data == shift) || (modifier == CTRL && data == ctrl)
}

fn shifted_identity(cp: i64, modifier: u32) -> i64 {
    if (modifier & !LOCK_MASK) & SHIFT != 0 && (65..=90).contains(&cp) {
        cp + 32
    } else {
        cp
    }
}

fn kitty(data: &str, expected_cp: i64, expected_mod: u32) -> bool {
    let Some(p) = parse_kitty(data) else { return false };
    if p.modifier & !LOCK_MASK != expected_mod & !LOCK_MASK {
        return false;
    }
    let cp = shifted_identity(normalize_kitty_functional(p.codepoint), p.modifier);
    if cp == shifted_identity(normalize_kitty_functional(expected_cp), expected_mod) {
        return true;
    }
    p.base_layout == Some(expected_cp) && !(97..=122).contains(&cp) && !is_symbol(cp)
}

fn modify_other_keys(data: &str, expected_cp: i64, expected_mod: u32) -> bool {
    parse_modify_other_keys(data) == Some((expected_cp, expected_mod))
}

fn printable_modify_other_keys(data: &str, expected_cp: i64, expected_mod: u32) -> bool {
    if expected_mod == 0 {
        return false;
    }
    match parse_modify_other_keys(data) {
        Some((cp, m)) if m == expected_mod => shifted_identity(cp, m) == shifted_identity(expected_cp, expected_mod),
        _ => false,
    }
}

fn raw_backspace(data: &str, expected_mod: u32, ctx: KeyContext) -> bool {
    match data {
        "\x7f" => expected_mod == 0,
        "\x08" => expected_mod == if ctx.windows_terminal { CTRL } else { 0 },
        _ => false,
    }
}

fn raw_ctrl_char(key: char) -> Option<char> {
    match key {
        'a'..='z' | '[' | '\\' | ']' | '_' => Some(char::from(key as u8 & 0x1f)),
        '-' => Some('\x1f'),
        _ => None,
    }
}

/// True when `data` is the key `key_id` (`escape`, `ctrl+c`, `shift+enter`, `alt+left`, `super+k`, ...).
/// `ctx` stands in for the TS globals: Kitty protocol state and the Windows Terminal session check.
pub fn matches_key(data: &str, key_id: &str, ctx: KeyContext) -> bool {
    let lower = key_id.to_lowercase();
    let parts: Vec<&str> = lower.split('+').collect();
    let key = parts[parts.len() - 1];
    if key.is_empty() {
        return false;
    }
    let mut m = 0;
    for (name, bit) in [("shift", SHIFT), ("alt", ALT), ("ctrl", CTRL), ("super", SUPER)] {
        if parts.contains(&name) {
            m |= bit;
        }
    }
    let kitty_active = ctx.kitty_active;
    let functional = |cp: i64| {
        if m == 0 {
            legacy(key).contains(&data) || kitty(data, cp, 0)
        } else {
            legacy_modified(data, key, m) || kitty(data, cp, m)
        }
    };
    match key {
        "escape" | "esc" => m == 0 && (data == "\x1b" || kitty(data, ESCAPE, 0) || modify_other_keys(data, ESCAPE, 0)),
        "space" => {
            if !kitty_active && ((m == CTRL && data == "\0") || (m == ALT && data == "\x1b ")) {
                return true;
            }
            (m == 0 && data == " ") || kitty(data, SPACE, m) || modify_other_keys(data, SPACE, m)
        }
        "tab" => match m {
            SHIFT => data == "\x1b[Z" || kitty(data, TAB, SHIFT) || modify_other_keys(data, TAB, SHIFT),
            0 => data == "\t" || kitty(data, TAB, 0),
            _ => kitty(data, TAB, m) || modify_other_keys(data, TAB, m),
        },
        "enter" | "return" => {
            let common = kitty(data, ENTER, m) || kitty(data, KP_ENTER, m);
            match m {
                SHIFT => {
                    common
                        || modify_other_keys(data, ENTER, SHIFT)
                        || (kitty_active && (data == "\x1b\r" || data == "\n"))
                }
                ALT => common || modify_other_keys(data, ENTER, ALT) || (!kitty_active && data == "\x1b\r"),
                0 => data == "\r" || (!kitty_active && data == "\n") || data == "\x1bOM" || common,
                _ => common || modify_other_keys(data, ENTER, m),
            }
        }
        "backspace" => match m {
            ALT => {
                data == "\x1b\x7f"
                    || data == "\x1b\x08"
                    || kitty(data, BACKSPACE, ALT)
                    || modify_other_keys(data, BACKSPACE, ALT)
            }
            CTRL | 0 => {
                raw_backspace(data, m, ctx) || kitty(data, BACKSPACE, m) || modify_other_keys(data, BACKSPACE, m)
            }
            _ => kitty(data, BACKSPACE, m) || modify_other_keys(data, BACKSPACE, m),
        },
        "insert" => functional(INSERT),
        "delete" => functional(DELETE),
        "clear" => {
            if m == 0 {
                legacy(key).contains(&data)
            } else {
                legacy_modified(data, key, m)
            }
        }
        "home" => functional(HOME),
        "end" => functional(END),
        "pageup" => functional(PAGE_UP),
        "pagedown" => functional(PAGE_DOWN),
        "up" | "down" if m == ALT => {
            data == if key == "up" { "\x1bp" } else { "\x1bn" } || kitty(data, if key == "up" { UP } else { DOWN }, ALT)
        }
        "up" => functional(UP),
        "down" => functional(DOWN),
        "left" | "right" => {
            let (cp, alt_csi, alt_upper, alt_lower, ctrl_csi) = if key == "left" {
                (LEFT, "\x1b[1;3D", "\x1bB", "\x1bb", "\x1b[1;5D")
            } else {
                (RIGHT, "\x1b[1;3C", "\x1bF", "\x1bf", "\x1b[1;5C")
            };
            match m {
                ALT => {
                    data == alt_csi || (!kitty_active && data == alt_upper) || data == alt_lower || kitty(data, cp, ALT)
                }
                CTRL => data == ctrl_csi || functional(cp),
                _ => functional(cp),
            }
        }
        "f1" | "f2" | "f3" | "f4" | "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            m == 0 && legacy(key).contains(&data)
        }
        _ => {
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return false;
            };
            let is_letter = c.is_ascii_lowercase();
            let is_digit = c.is_ascii_digit();
            if !(is_letter || is_digit || is_symbol(c as i64)) {
                return false;
            }
            let cp = c as i64;
            let raw_ctrl = raw_ctrl_char(c);
            let esc_ctrl = raw_ctrl.is_some_and(|r| {
                data.strip_prefix('\x1b')
                    .is_some_and(|rest| rest.len() == 1 && rest.starts_with(r))
            });
            if m == CTRL + ALT && !kitty_active && esc_ctrl {
                return true;
            }
            if m == ALT && !kitty_active && data.strip_prefix('\x1b') == Some(key) {
                return true;
            }
            if m == CTRL && raw_ctrl.is_some_and(|r| data.len() == 1 && data.starts_with(r)) {
                return true;
            }
            if m == SHIFT && is_letter && data == key.to_uppercase() {
                return true;
            }
            if m == 0 {
                return data == key || kitty(data, cp, 0);
            }
            kitty(data, cp, m) || printable_modify_other_keys(data, cp, m)
        }
    }
}
