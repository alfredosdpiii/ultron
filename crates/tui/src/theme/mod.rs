//! Themes (port of packages/coding-agent/src/modes/interactive/theme/theme.ts): built-in dark/light documents, var
//! resolution, truecolor/256-color ANSI palettes, HTML export colors, theme-setting parsing and terminal background
//! detection. Process globals (current theme, registry, watcher) stay with the caller; everything here is a function
//! of its inputs.

pub mod registry;
mod schema;
pub mod styles;

use std::path::Path;

use serde_json::Value;

use crate::image::Capabilities;
pub use schema::{
    ColorValue, REQUIRED_COLORS, ThemeExport, ThemeJson, assert_theme_name_is_valid, validate_theme_json,
};

pub const DARK_JSON: &str = include_str!("dark.json");
pub const LIGHT_JSON: &str = include_str!("light.json");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    Truecolor,
    Ansi256,
}

impl ColorMode {
    /// `createTheme`'s default: truecolor when the terminal supports it, else 256 colors.
    pub fn from_capabilities(caps: &Capabilities) -> Self {
        if caps.true_color {
            Self::Truecolor
        } else {
            Self::Ansi256
        }
    }
}

/// Foreground tokens, in `ThemeColor` order.
pub const THEME_COLORS: [&str; 49] = [
    "accent",
    "border",
    "borderAccent",
    "borderMuted",
    "success",
    "error",
    "warning",
    "muted",
    "dim",
    "text",
    "thinkingText",
    "scrollbarTrack",
    "scrollbarThumb",
    "searchMatchText",
    "userMessageText",
    "customMessageText",
    "customMessageLabel",
    "toolTitle",
    "toolOutput",
    "mdHeading",
    "mdLink",
    "mdLinkUrl",
    "mdCode",
    "mdCodeBlock",
    "mdCodeBlockBorder",
    "mdQuote",
    "mdQuoteBorder",
    "mdHr",
    "mdListBullet",
    "toolDiffAdded",
    "toolDiffRemoved",
    "toolDiffContext",
    "syntaxComment",
    "syntaxKeyword",
    "syntaxFunction",
    "syntaxVariable",
    "syntaxString",
    "syntaxNumber",
    "syntaxType",
    "syntaxOperator",
    "syntaxPunctuation",
    "thinkingOff",
    "thinkingMinimal",
    "thinkingLow",
    "thinkingMedium",
    "thinkingHigh",
    "thinkingXhigh",
    "thinkingMax",
    "bashMode",
];

/// Background tokens, in `ThemeBg` order.
pub const THEME_BGS: [&str; 7] = [
    "selectedBg",
    "searchMatchBg",
    "userMessageBg",
    "customMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
];

/// A color after var resolution: `""`, a hex string, or a 256-color index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    Str(String),
    Index(u8),
}

pub fn hex_to_rgb(hex: &str) -> Result<(u8, u8, u8), String> {
    let cleaned = hex.replacen('#', "", 1);
    let invalid = || format!("Invalid hex color: {hex}");
    if cleaned.chars().count() != 6 {
        return Err(invalid());
    }
    // parseInt accepts a hex prefix ("1z" -> 1) and fails only when the first digit is invalid.
    let channel = |i: usize| -> Result<u8, String> {
        let pair: Vec<char> = cleaned.chars().skip(i).take(2).collect();
        let digits: String = pair.iter().take_while(|c| c.is_ascii_hexdigit()).collect();
        u8::from_str_radix(&digits, 16).map_err(|_| invalid())
    };
    Ok((channel(0)?, channel(2)?, channel(4)?))
}

const CUBE_VALUES: [i32; 6] = [0, 95, 135, 175, 215, 255];

fn closest(values: impl Iterator<Item = i32>, target: i32) -> usize {
    let mut best = (i32::MAX, 0);
    for (i, v) in values.enumerate() {
        let dist = (target - v).abs();
        if dist < best.0 {
            best = (dist, i);
        }
    }
    best.1
}

fn color_distance(a: (i32, i32, i32), b: (i32, i32, i32)) -> f64 {
    let (dr, dg, db) = (f64::from(a.0 - b.0), f64::from(a.1 - b.1), f64::from(a.2 - b.2));
    dr * dr * 0.299 + dg * dg * 0.587 + db * db * 0.114
}

/// `rgbTo256`: nearest 6x6x6 cube entry, or the grayscale ramp for nearly neutral colors when it is closer.
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    let (ri, gi, bi) = (
        closest(CUBE_VALUES.into_iter(), r),
        closest(CUBE_VALUES.into_iter(), g),
        closest(CUBE_VALUES.into_iter(), b),
    );
    let cube_dist = color_distance((r, g, b), (CUBE_VALUES[ri], CUBE_VALUES[gi], CUBE_VALUES[bi]));
    let gray = (0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b) + 0.5).floor() as i32;
    let gray_idx = closest((0..24).map(|i| 8 + i * 10), gray);
    let gray_value = 8 + gray_idx as i32 * 10;
    let gray_dist = color_distance((r, g, b), (gray_value, gray_value, gray_value));
    let spread = r.max(g).max(b) - r.min(g).min(b);
    if spread < 10 && gray_dist < cube_dist {
        return 232 + gray_idx as u8;
    }
    (16 + 36 * ri + 6 * gi + bi) as u8
}

fn ansi(color: &Resolved, mode: ColorMode, layer: u8) -> Result<String, String> {
    match color {
        Resolved::Str(s) if s.is_empty() => Ok(format!("\x1b[{layer}9m")),
        Resolved::Index(i) => Ok(format!("\x1b[{layer}8;5;{i}m")),
        Resolved::Str(s) if s.starts_with('#') => {
            let (r, g, b) = hex_to_rgb(s)?;
            Ok(match mode {
                ColorMode::Truecolor => format!("\x1b[{layer}8;2;{r};{g};{b}m"),
                ColorMode::Ansi256 => format!("\x1b[{layer}8;5;{}m", rgb_to_256(r, g, b)),
            })
        }
        Resolved::Str(s) => Err(format!("Invalid color value: {s}")),
    }
}

/// `fgAnsi`.
pub fn fg_ansi(color: &Resolved, mode: ColorMode) -> Result<String, String> {
    ansi(color, mode, 3)
}

/// `bgAnsi`.
pub fn bg_ansi(color: &Resolved, mode: ColorMode) -> Result<String, String> {
    ansi(color, mode, 4)
}

/// `resolveVarRefs`: follow variable names until a hex, `""` or index, rejecting cycles and unknown names.
pub fn resolve_var_refs(value: &ColorValue, vars: &[(String, ColorValue)]) -> Result<Resolved, String> {
    let mut visited: Vec<&str> = Vec::new();
    let mut value = value;
    loop {
        let name = match value {
            ColorValue::Index(i) => return Ok(Resolved::Index(*i)),
            ColorValue::Str(s) if s.is_empty() || s.starts_with('#') => return Ok(Resolved::Str(s.clone())),
            ColorValue::Str(s) => s.as_str(),
        };
        if visited.contains(&name) {
            return Err(format!("Circular variable reference detected: {name}"));
        }
        let Some((_, next)) = vars.iter().find(|(k, _)| k == name) else {
            return Err(format!("Variable reference not found: {name}"));
        };
        visited.push(name);
        value = next;
    }
}

fn json_color(value: &Value) -> Result<ColorValue, String> {
    match value {
        Value::String(s) => Ok(ColorValue::Str(s.clone())),
        Value::Number(n) if n.as_u64().is_some_and(|n| n <= 255) => {
            Ok(ColorValue::Index(n.as_u64().unwrap_or(0) as u8))
        }
        other => Err(format!("Invalid color value: {other}")),
    }
}

/// `resolveThemeColors(withThemeColorFallbacks(colors), vars)`: every color key in document order, then the optional
/// tokens a document left out, filled from their fallbacks.
pub fn resolve_theme_colors(json: &ThemeJson) -> Result<Vec<(String, Resolved)>, String> {
    let mut entries: Vec<(String, ColorValue)> = Vec::new();
    for (k, v) in &json.colors {
        entries.push((k.clone(), json_color(v)?));
    }
    for (key, fallback) in [
        ("scrollbarTrack", "muted"),
        ("scrollbarThumb", "text"),
        ("thinkingMax", "thinkingXhigh"),
        ("searchMatchBg", "selectedBg"),
        ("searchMatchText", "text"),
    ] {
        if !entries.iter().any(|(k, _)| k == key) {
            let value = entries.iter().find(|(k, _)| k == fallback).map(|(_, v)| v.clone());
            entries.push((
                key.to_string(),
                value.ok_or_else(|| format!("Missing theme color: {fallback}"))?,
            ));
        }
    }
    entries
        .into_iter()
        .map(|(k, v)| Ok((k, resolve_var_refs(&v, &json.vars)?)))
        .collect()
}

/// A resolved palette: one ANSI sequence per foreground and background token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Theme {
    pub name: Option<String>,
    pub source_path: Option<String>,
    mode: ColorMode,
    fg: Vec<String>,
    bg: Vec<String>,
}

impl Theme {
    /// `new Theme(fgColors, bgColors, mode)`: `lookup` returns the resolved color of a token, `None` for the optional
    /// tokens (scrollbarTrack, scrollbarThumb, thinkingMax, searchMatchText, searchMatchBg), which then fall back.
    pub fn new(lookup: &dyn Fn(&str) -> Option<Resolved>, mode: ColorMode) -> Result<Self, String> {
        let get = |key: &str| -> Result<Resolved, String> {
            let fallback = match key {
                "scrollbarTrack" => "muted",
                "scrollbarThumb" | "searchMatchText" => "text",
                "thinkingMax" => "thinkingXhigh",
                "searchMatchBg" => "selectedBg",
                _ => key,
            };
            lookup(key)
                .or_else(|| lookup(fallback))
                .ok_or_else(|| format!("Missing theme color: {key}"))
        };
        let fg = THEME_COLORS
            .iter()
            .map(|k| fg_ansi(&get(k)?, mode))
            .collect::<Result<_, _>>()?;
        let bg = THEME_BGS
            .iter()
            .map(|k| bg_ansi(&get(k)?, mode))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            name: None,
            source_path: None,
            mode,
            fg,
            bg,
        })
    }

    /// `getFgAnsi`.
    pub fn fg_ansi(&self, color: &str) -> Result<&str, String> {
        THEME_COLORS
            .iter()
            .position(|c| *c == color)
            .map(|i| self.fg[i].as_str())
            .ok_or_else(|| format!("Unknown theme color: {color}"))
    }

    /// `getBgAnsi`.
    pub fn bg_ansi(&self, color: &str) -> Result<&str, String> {
        THEME_BGS
            .iter()
            .position(|c| *c == color)
            .map(|i| self.bg[i].as_str())
            .ok_or_else(|| format!("Unknown theme background color: {color}"))
    }

    /// `fg`: color `text`, resetting only the foreground.
    pub fn fg(&self, color: &str, text: &str) -> Result<String, String> {
        Ok(format!("{}{text}\x1b[39m", self.fg_ansi(color)?))
    }

    /// `bg`: color the background of `text`, resetting only the background.
    pub fn bg(&self, color: &str, text: &str) -> Result<String, String> {
        Ok(format!("{}{text}\x1b[49m", self.bg_ansi(color)?))
    }

    pub fn color_mode(&self) -> ColorMode {
        self.mode
    }

    /// `getThinkingBorderColor(level)(text)`; unknown levels use `thinkingOff`.
    pub fn thinking_border(&self, level: &str, text: &str) -> String {
        let token = match level {
            "minimal" => "thinkingMinimal",
            "low" => "thinkingLow",
            "medium" => "thinkingMedium",
            "high" => "thinkingHigh",
            "xhigh" => "thinkingXhigh",
            "max" => "thinkingMax",
            _ => "thinkingOff",
        };
        format!(
            "{}{text}\x1b[39m",
            self.fg[THEME_COLORS.iter().position(|c| *c == token).unwrap_or(0)]
        )
    }

    /// `getBashModeBorderColor()(text)`.
    pub fn bash_mode_border(&self, text: &str) -> String {
        format!("{}{text}\x1b[39m", self.fg[THEME_COLORS.len() - 1])
    }
}

/// `createTheme`: resolve a validated document into a palette. Every color key is converted, extra keys included, so an
/// invalid hex anywhere fails the theme as in TypeScript.
pub fn create_theme(json: &ThemeJson, mode: ColorMode, source_path: Option<&str>) -> Result<Theme, String> {
    let resolved = resolve_theme_colors(json)?;
    let (bgs, fgs): (Vec<_>, Vec<_>) = resolved.iter().partition(|(k, _)| THEME_BGS.contains(&k.as_str()));
    for (_, v) in &fgs {
        fg_ansi(v, mode)?;
    }
    for (_, v) in &bgs {
        bg_ansi(v, mode)?;
    }
    let lookup = |key: &str| resolved.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    let mut theme = Theme::new(&lookup, mode)?;
    theme.name = Some(json.name.clone());
    theme.source_path = source_path.map(str::to_string);
    Ok(theme)
}

/// `parseThemeJsonContent` with the full validator installed: strip a BOM, parse, validate.
pub fn parse_theme_json_content(label: &str, content: &str) -> Result<ThemeJson, String> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let json: Value =
        serde_json::from_str(content).map_err(|e| format!("Failed to parse theme {label}: SyntaxError: {e}"))?;
    validate_theme_json(label, &json)
}

/// The built-in `dark` or `light` document.
pub fn builtin_theme_json(name: &str) -> Option<ThemeJson> {
    let content = match name {
        "dark" => DARK_JSON,
        "light" => LIGHT_JSON,
        _ => return None,
    };
    parse_theme_json_content(name, content).ok()
}

/// `loadThemeFromPath`.
pub fn load_theme_from_path(path: &Path, mode: ColorMode) -> Result<Theme, String> {
    let label = path.display().to_string();
    let content = std::fs::read_to_string(path).map_err(|e| format!("Failed to read theme {label}: {e}"))?;
    create_theme(&parse_theme_json_content(&label, &content)?, mode, Some(&label))
}

/// `loadThemeJson` without the registry: a built-in name, else `<custom_dir>/<name>.json`.
pub fn load_theme_json(name: &str, custom_themes_dir: &Path) -> Result<ThemeJson, String> {
    if let Some(json) = builtin_theme_json(name) {
        return Ok(json);
    }
    let path = custom_themes_dir.join(format!("{name}.json"));
    if !path.exists() {
        return Err(format!("Theme not found: {name}"));
    }
    let content = std::fs::read_to_string(&path).map_err(|e| format!("Failed to read theme {name}: {e}"))?;
    parse_theme_json_content(name, &content)
}

/// `ansi256ToHex`: basic 16 colors (approximate), the 6x6x6 cube, then the grayscale ramp.
pub fn ansi256_to_hex(index: u8) -> String {
    const BASIC: [&str; 16] = [
        "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0", "#808080", "#ff0000",
        "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    let index = usize::from(index);
    if index < 16 {
        return BASIC[index].to_string();
    }
    if index < 232 {
        let cube = index - 16;
        let level = |n: usize| if n == 0 { 0 } else { 55 + n * 40 };
        return format!(
            "#{:02x}{:02x}{:02x}",
            level(cube / 36),
            level((cube % 36) / 6),
            level(cube % 6)
        );
    }
    let gray = 8 + (index - 232) * 10;
    format!("#{gray:02x}{gray:02x}{gray:02x}")
}

/// `getResolvedThemeColors`: every resolved color as CSS hex; `""` becomes the default text color of the theme.
pub fn resolved_css_colors(json: &ThemeJson, theme_name: &str) -> Result<Vec<(String, String)>, String> {
    let default_text = if is_light_theme(theme_name) {
        "#000000"
    } else {
        "#e5e5e7"
    };
    Ok(resolve_theme_colors(json)?
        .into_iter()
        .map(|(k, v)| {
            let css = match v {
                Resolved::Index(i) => ansi256_to_hex(i),
                Resolved::Str(s) if s.is_empty() => default_text.to_string(),
                Resolved::Str(s) => s,
            };
            (k, css)
        })
        .collect())
}

/// `isLightTheme`.
pub fn is_light_theme(theme_name: &str) -> bool {
    theme_name == "light"
}

/// Explicit HTML export colors (`getThemeExportColors`). Unset or `""` entries are `None`; any resolution error
/// yields all `None`, as the TypeScript version swallows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportColors {
    pub page_bg: Option<String>,
    pub card_bg: Option<String>,
    pub info_bg: Option<String>,
}

pub fn theme_export_colors(json: &ThemeJson) -> ExportColors {
    let resolve = |value: &Option<ColorValue>| -> Result<Option<String>, String> {
        let Some(value) = value else { return Ok(None) };
        Ok(match resolve_var_refs(value, &json.vars)? {
            Resolved::Index(i) => Some(ansi256_to_hex(i)),
            Resolved::Str(s) if s.is_empty() => None,
            Resolved::Str(s) => Some(s),
        })
    };
    let e = &json.export;
    match (resolve(&e.page_bg), resolve(&e.card_bg), resolve(&e.info_bg)) {
        (Ok(page_bg), Ok(card_bg), Ok(info_bg)) => ExportColors {
            page_bg,
            card_bg,
            info_bg,
        },
        _ => ExportColors::default(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalTheme {
    Dark,
    Light,
}

/// `parseAutoThemeSetting`: `"light/dark"` with exactly one slash and both sides non-empty after trimming.
pub fn parse_auto_theme_setting(setting: Option<&str>) -> Option<(String, String)> {
    let setting = setting.filter(|s| !s.is_empty())?;
    let (light, dark) = setting.split_once('/')?;
    if dark.contains('/') {
        return None;
    }
    let (light, dark) = (light.trim(), dark.trim());
    if light.is_empty() || dark.is_empty() {
        return None;
    }
    Some((light.to_string(), dark.to_string()))
}

/// `resolveThemeSetting`: pick the side of an automatic setting; a malformed one with a slash is `None`.
pub fn resolve_theme_setting(setting: Option<&str>, terminal: TerminalTheme) -> Option<String> {
    if let Some((light, dark)) = parse_auto_theme_setting(setting) {
        return Some(if terminal == TerminalTheme::Light { light } else { dark });
    }
    setting.filter(|s| !s.contains('/')).map(str::to_string)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalThemeDetection {
    pub theme: TerminalTheme,
    /// "terminal background", "COLORFGBG" or "fallback".
    pub source: &'static str,
    pub detail: String,
    /// "high" or "low".
    pub confidence: &'static str,
}

/// JavaScript `parseInt(s, 10)` for the integer range we care about.
fn parse_int(s: &str) -> Option<i64> {
    let s = s.trim_start();
    let (neg, rest) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let n: i64 = digits.chars().take(18).collect::<String>().parse().ok()?;
    Some(if neg { -n } else { n })
}

fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let linear = |c: u8| {
        let v = f64::from(c) / 255.0;
        if v <= 0.03928 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// `getThemeForRgbColor`.
pub fn theme_for_rgb(r: u8, g: u8, b: u8) -> TerminalTheme {
    if luminance((r, g, b)) >= 0.5 {
        TerminalTheme::Light
    } else {
        TerminalTheme::Dark
    }
}

/// `detectTerminalBackgroundFromEnv`, given the `COLORFGBG` value: the last numeric field in 0..=255 is the
/// background color index.
pub fn detect_terminal_background_from_env(colorfgbg: Option<&str>) -> TerminalThemeDetection {
    let bg = colorfgbg
        .unwrap_or("")
        .split(';')
        .rev()
        .filter_map(parse_int)
        .find(|n| (0..=255).contains(n));
    match bg {
        Some(bg) => {
            let rgb = hex_to_rgb(&ansi256_to_hex(bg as u8)).unwrap_or((0, 0, 0));
            TerminalThemeDetection {
                theme: if luminance(rgb) >= 0.5 {
                    TerminalTheme::Light
                } else {
                    TerminalTheme::Dark
                },
                source: "COLORFGBG",
                detail: format!("background color index {bg}"),
                confidence: "high",
            }
        }
        None => TerminalThemeDetection {
            theme: TerminalTheme::Dark,
            source: "fallback",
            detail: "no terminal background hint found".to_string(),
            confidence: "low",
        },
    }
}

/// `detectTerminalBackgroundTheme` once the OSC 11 query has answered (or not): the reported background wins, else
/// `COLORFGBG`.
pub fn detect_terminal_background_theme(
    osc11: Option<(u8, u8, u8)>,
    colorfgbg: Option<&str>,
) -> TerminalThemeDetection {
    match osc11 {
        Some((r, g, b)) => TerminalThemeDetection {
            theme: theme_for_rgb(r, g, b),
            source: "terminal background",
            detail: format!("OSC 11 background rgb({r}, {g}, {b})"),
            confidence: "high",
        },
        None => detect_terminal_background_from_env(colorfgbg),
    }
}

/// `getLanguageFromPath`: highlight language for a file extension.
pub fn language_from_path(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_lowercase();
    Some(match ext.as_str() {
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "rb" => "ruby",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "kt" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "fish" => "fish",
        "ps1" => "powershell",
        "sql" => "sql",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "sass" => "sass",
        "less" => "less",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        "md" | "markdown" => "markdown",
        "dockerfile" => "dockerfile",
        "makefile" => "makefile",
        "cmake" => "cmake",
        "lua" => "lua",
        "perl" => "perl",
        "r" => "r",
        "scala" => "scala",
        "clj" => "clojure",
        "ex" | "exs" => "elixir",
        "erl" => "erlang",
        "hs" => "haskell",
        "ml" => "ocaml",
        "vim" => "vim",
        "graphql" => "graphql",
        "proto" => "protobuf",
        "tf" | "hcl" => "hcl",
        _ => return None,
    })
}
