//! Theme document validation (port of packages/coding-agent/src/modes/interactive/theme/theme-json.ts). The TypeScript
//! version validates with a compiled typebox schema; this reproduces that schema's error list (schema property order,
//! `required` before properties, an 8-error cap) and the message `validateThemeJson` builds from it.

use serde_json::{Map, Value};

/// A color in a theme document: a hex string `#rrggbb`, a variable name, `""` (terminal default) or a 256-color index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColorValue {
    Str(String),
    Index(u8),
}

/// A validated theme document. `colors` keeps document order and any extra keys the schema lets through.
#[derive(Clone, Debug, PartialEq)]
pub struct ThemeJson {
    pub name: String,
    pub vars: Vec<(String, ColorValue)>,
    pub colors: Vec<(String, Value)>,
    pub export: ThemeExport,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThemeExport {
    pub page_bg: Option<ColorValue>,
    pub card_bg: Option<ColorValue>,
    pub info_bg: Option<ColorValue>,
}

/// Required color tokens, in schema order.
pub const REQUIRED_COLORS: [&str; 51] = [
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
    "selectedBg",
    "userMessageBg",
    "userMessageText",
    "customMessageBg",
    "customMessageText",
    "customMessageLabel",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
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
    "bashMode",
];

/// All schema color properties in schema order: required ones plus the optional ones at their schema positions.
const SCHEMA_COLORS: [&str; 56] = [
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
    "selectedBg",
    "searchMatchBg",
    "searchMatchText",
    "userMessageBg",
    "userMessageText",
    "customMessageBg",
    "customMessageText",
    "customMessageLabel",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
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

/// typebox stops yielding errors after this many.
const MAX_ERRORS: usize = 8;

struct SchemaError {
    path: String,
    message: String,
    /// Missing properties of a `required` check at `/colors`, reported separately.
    missing_colors: Vec<String>,
}

#[derive(Default)]
struct Errors(Vec<SchemaError>);

impl Errors {
    fn push(&mut self, path: &str, message: String) {
        self.0.push(SchemaError {
            path: path.to_string(),
            message,
            missing_colors: Vec::new(),
        });
    }

    fn required(&mut self, path: &str, obj: &Map<String, Value>, keys: &[&str]) {
        let missing: Vec<String> = keys
            .iter()
            .filter(|k| !obj.contains_key(**k))
            .map(|k| k.to_string())
            .collect();
        if !missing.is_empty() {
            let message = format!("must have required properties {}", missing.join(", "));
            let missing_colors = if path == "/colors" { missing } else { Vec::new() };
            self.0.push(SchemaError {
                path: path.to_string(),
                message,
                missing_colors,
            });
        }
    }

    fn color(&mut self, path: &str, value: &Value) {
        if value.is_string() {
            return;
        }
        let start = self.0.len();
        self.push(path, "must be string".into());
        match value.as_f64() {
            Some(n) => {
                if n.fract() != 0.0 {
                    self.push(path, "must be integer".into());
                }
                if n > 255.0 {
                    self.push(path, "must be <= 255".into());
                }
                if n < 0.0 {
                    self.push(path, "must be >= 0".into());
                }
            }
            None => self.push(path, "must be integer".into()),
        }
        if self.0.len() - start > 1 {
            self.push(path, "must match a schema in anyOf".into());
        } else {
            self.0.truncate(start);
        }
    }

    fn string(&mut self, path: &str, value: &Value) {
        if !value.is_string() {
            self.push(path, "must be string".into());
        }
    }
}

fn object<'a>(errors: &mut Errors, path: &str, value: &'a Value) -> Option<&'a Map<String, Value>> {
    let obj = value.as_object();
    if obj.is_none() {
        errors.push(path, "must be object".into());
    }
    obj
}

fn schema_errors(json: &Value) -> Errors {
    let mut errors = Errors::default();
    let Some(root) = object(&mut errors, "/", json) else {
        return errors;
    };
    errors.required("/", root, &["name", "colors"]);
    if let Some(v) = root.get("$schema") {
        errors.string("/$schema", v);
    }
    if let Some(v) = root.get("name") {
        errors.string("/name", v);
    }
    if let Some(vars) = root.get("vars").and_then(|v| object(&mut errors, "/vars", v)) {
        for (key, value) in vars {
            errors.color(&format!("/vars/{key}"), value);
        }
    }
    if let Some(colors) = root.get("colors").and_then(|v| object(&mut errors, "/colors", v)) {
        errors.required("/colors", colors, &REQUIRED_COLORS);
        for key in SCHEMA_COLORS {
            if let Some(value) = colors.get(key) {
                errors.color(&format!("/colors/{key}"), value);
            }
        }
    }
    if let Some(export) = root.get("export").and_then(|v| object(&mut errors, "/export", v)) {
        for key in ["pageBg", "cardBg", "infoBg"] {
            if let Some(value) = export.get(key) {
                errors.color(&format!("/export/{key}"), value);
            }
        }
    }
    errors.0.truncate(MAX_ERRORS);
    errors
}

/// `assertThemeNameIsValid`: "/" is reserved for `light/dark` automatic theme settings.
pub fn assert_theme_name_is_valid(name: &str) -> Result<(), String> {
    if name.contains('/') {
        return Err(format!(
            "Invalid theme name \"{name}\": theme names cannot contain \"/\" because it is reserved for automatic light/dark theme settings."
        ));
    }
    Ok(())
}

fn color_value(value: &Value) -> ColorValue {
    match value {
        Value::String(s) => ColorValue::Str(s.clone()),
        // Validation guarantees an integer in 0..=255 here.
        other => ColorValue::Index(other.as_f64().map_or(0, |n| n as u8)),
    }
}

/// `validateThemeJson`: validate one theme document, with an error message that names the offending tokens.
pub fn validate_theme_json(label: &str, json: &Value) -> Result<ThemeJson, String> {
    let errors = schema_errors(json);
    if !errors.0.is_empty() {
        let mut missing: Vec<&str> = Vec::new();
        let mut other: Vec<String> = Vec::new();
        for e in &errors.0 {
            if e.path == "/colors" && !e.missing_colors.is_empty() {
                missing.extend(e.missing_colors.iter().map(String::as_str));
                continue;
            }
            other.push(format!("  - {}: {}", e.path, e.message));
        }
        let mut message = format!("Invalid theme \"{label}\":\n");
        if !missing.is_empty() {
            missing.sort_unstable();
            message.push_str("\nMissing required color tokens:\n");
            message.push_str(
                &missing
                    .iter()
                    .map(|c| format!("  - {c}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
            message.push_str("\n\nPlease add these colors to your theme's \"colors\" object.");
            message.push_str("\nSee the built-in themes (dark.json, light.json) for reference values.");
        }
        if !other.is_empty() {
            message.push_str(&format!("\n\nOther errors:\n{}", other.join("\n")));
        }
        return Err(message);
    }
    let empty = Map::new();
    let root = json.as_object().unwrap_or(&empty);
    let name = root.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    assert_theme_name_is_valid(&name)?;
    let pairs = |key: &str| root.get(key).and_then(Value::as_object).unwrap_or(&empty);
    let export = pairs("export");
    let export_color = |key: &str| export.get(key).map(color_value);
    Ok(ThemeJson {
        name,
        vars: pairs("vars").iter().map(|(k, v)| (k.clone(), color_value(v))).collect(),
        colors: pairs("colors").iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        export: ThemeExport {
            page_bg: export_color("pageBg"),
            card_bg: export_color("cardBg"),
            info_bg: export_color("infoBg"),
        },
    })
}
