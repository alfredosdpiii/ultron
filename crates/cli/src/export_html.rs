//! `ultron --export <session> [out.html]`: a session as one self-contained HTML page (`core/export-html/index.ts`,
//! `ultron/native-export.ts`). A native session's `main` branch is read from its format-4 JSONL log; a Pi session
//! file through `session-file`. The page is the package's template with the theme's colors and the session data
//! (base64 JSON) filled in; the templates are compiled into the binary.

use crate::js::{base64, replace_first};
use serde_json::{Map, Value, json};
use session_file::js::{iso_string, stringify};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const TEMPLATE_HTML: &str = include_str!("../../../packages/coding-agent/src/core/export-html/template.html");
const TEMPLATE_CSS: &str = include_str!("../../../packages/coding-agent/src/core/export-html/template.css");
const TEMPLATE_JS: &str = include_str!("../../../packages/coding-agent/src/core/export-html/template.js");
const MARKED_JS: &str = include_str!("../../../packages/coding-agent/src/core/export-html/vendor/marked.min.js");
const HIGHLIGHT_JS: &str = include_str!("../../../packages/coding-agent/src/core/export-html/vendor/highlight.min.js");

fn parse_color(color: &str) -> Option<(u8, u8, u8)> {
    if let Some(hex) = color.strip_prefix('#')
        && hex.len() == 6
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        let part = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        return Some((part(0)?, part(2)?, part(4)?));
    }
    // `^rgb\s*\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*\)$`
    let inner = color
        .strip_prefix("rgb")?
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')?;
    let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    // `Number.parseInt` of a large value does not fit a byte; such colors are not produced by themes.
    let n = |p: &str| p.parse::<u8>().ok();
    Some((n(parts[0])?, n(parts[1])?, n(parts[2])?))
}

fn luminance(r: u8, g: u8, b: u8) -> f64 {
    let linear = |c: u8| {
        let s = f64::from(c) / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

fn adjust_brightness(color: &str, factor: f64) -> String {
    let Some((r, g, b)) = parse_color(color) else {
        return color.to_string();
    };
    let adjust = |c: u8| crate::js::round(f64::from(c) * factor).clamp(0.0, 255.0);
    format!("rgb({}, {}, {})", adjust(r), adjust(g), adjust(b))
}

/// `deriveExportColors`: page, card and info backgrounds from a base color.
pub fn derive_export_colors(base: &str) -> [String; 3] {
    let Some((r, g, b)) = parse_color(base) else {
        return [
            "rgb(24, 24, 30)".into(),
            "rgb(30, 30, 36)".into(),
            "rgb(60, 55, 40)".into(),
        ];
    };
    let (r, g, b) = (u32::from(r), u32::from(g), u32::from(b));
    if luminance(r as u8, g as u8, b as u8) > 0.5 {
        [
            adjust_brightness(base, 0.96),
            base.to_string(),
            format!(
                "rgb({}, {}, {})",
                (r + 10).min(255),
                (g + 5).min(255),
                b.saturating_sub(20)
            ),
        ]
    } else {
        [
            adjust_brightness(base, 0.7),
            adjust_brightness(base, 0.85),
            format!("rgb({}, {}, {})", (r + 20).min(255), (g + 15).min(255), b),
        ]
    }
}

/// `generateHtml(sessionData)` with the default theme (`getDefaultTheme()`: `COLORFGBG`, else dark).
pub fn generate_html(session_data: &Value, colorfgbg: Option<&str>) -> String {
    use tui::theme::{TerminalTheme, builtin_theme_json, detect_terminal_background_from_env};
    let name = match detect_terminal_background_from_env(colorfgbg).theme {
        TerminalTheme::Light => "light",
        TerminalTheme::Dark => "dark",
    };
    let json = builtin_theme_json(name).expect("built-in theme parses");
    page(session_data, &json, name).expect("built-in theme resolves")
}

/// Where `loadThemeJson` finds a theme by name besides the built-ins: the registered themes (name, source file) and
/// `<agent dir>/themes` (`getCustomThemesDir()`).
pub struct ThemeSources<'a> {
    pub registered: &'a [(String, Option<String>)],
    pub custom_dir: &'a Path,
}

/// `loadThemeJson(name)`: a built-in, else a registered theme's file, else `<custom dir>/<name>.json`.
pub fn load_export_theme_json(name: &str, sources: &ThemeSources<'_>) -> Result<tui::theme::ThemeJson, String> {
    if let Some(json) = tui::theme::builtin_theme_json(name) {
        return Ok(json);
    }
    if let Some((_, source)) = sources.registered.iter().find(|(n, _)| n == name) {
        let Some(path) = source else {
            return Err(format!("Theme \"{name}\" does not have a source path for export"));
        };
        let content = std::fs::read_to_string(path).map_err(|e| format!("Failed to read theme {path}: {e}"))?;
        return tui::theme::parse_theme_json_content(path, &content);
    }
    tui::theme::load_theme_json(name, sources.custom_dir)
}

/// `generateHtml(sessionData, themeName)`: the page in the named theme (`theme.name` of the TUI's active theme);
/// without a name, the default theme. A theme that cannot be loaded fails the export with its error.
pub fn generate_html_themed(
    session_data: &Value,
    theme_name: Option<&str>,
    colorfgbg: Option<&str>,
    sources: &ThemeSources<'_>,
) -> Result<String, String> {
    let Some(name) = theme_name else {
        return Ok(generate_html(session_data, colorfgbg));
    };
    let json = load_export_theme_json(name, sources)?;
    page(session_data, &json, name)
}

fn page(session_data: &Value, json: &tui::theme::ThemeJson, name: &str) -> Result<String, String> {
    let colors = tui::theme::resolved_css_colors(json, name)?;
    let export = tui::theme::theme_export_colors(json);
    let user_bg = colors
        .iter()
        .find(|(k, _)| k == "userMessageBg")
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
        .unwrap_or("#343541");
    let [page, card, info] = derive_export_colors(user_bg);
    let body_bg = export.page_bg.unwrap_or(page);
    let container_bg = export.card_bg.unwrap_or(card);
    let info_bg = export.info_bg.unwrap_or(info);
    let mut vars: Vec<String> = colors.iter().map(|(k, v)| format!("--{k}: {v};")).collect();
    vars.push(format!("--exportPageBg: {body_bg};"));
    vars.push(format!("--exportCardBg: {container_bg};"));
    vars.push(format!("--exportInfoBg: {info_bg};"));
    let data = base64(stringify(session_data).as_bytes());
    let css = replace_first(TEMPLATE_CSS, "{{THEME_VARS}}", &vars.join("\n      "));
    let css = replace_first(&css, "{{BODY_BG}}", &body_bg);
    let css = replace_first(&css, "{{CONTAINER_BG}}", &container_bg);
    let css = replace_first(&css, "{{INFO_BG}}", &info_bg);
    let html = replace_first(TEMPLATE_HTML, "{{CSS}}", &css);
    let html = replace_first(&html, "{{JS}}", TEMPLATE_JS);
    let html = replace_first(&html, "{{SESSION_DATA}}", &data);
    let html = replace_first(&html, "{{MARKED_JS}}", MARKED_JS);
    Ok(replace_first(&html, "{{HIGHLIGHT_JS}}", HIGHLIGHT_JS))
}

/// The `/export` page in the TUI's theme: the built-ins, the themes packages and settings register (by their files),
/// and `<agent dir>/themes`, as `loadThemeJson` finds them.
pub fn export_page(data: &Value, theme: Option<&str>, env: &crate::Env) -> Result<String, String> {
    let registered: Vec<(String, Option<String>)> = match theme {
        Some(name) if tui::theme::builtin_theme_json(name).is_none() => {
            crate::themes::load(env, tui::theme::ColorMode::Truecolor)
                .themes
                .into_iter()
                .filter_map(|t| Some((t.name?, t.source_path)))
                .collect()
        }
        _ => Vec::new(),
    };
    let custom_dir = env.agent_dir().join("themes");
    generate_html_themed(
        data,
        theme,
        (env.var)("COLORFGBG").as_deref(),
        &ThemeSources {
            registered: &registered,
            custom_dir: &custom_dir,
        },
    )
}

const DEFAULT_SHARE_VIEWER_URL: &str = "https://pi.dev/session/";

/// `getShareViewerUrl(gistId)`: `PI_SHARE_VIEWER_URL` (unset or empty: the default viewer) then `#<gist id>`.
pub fn share_viewer_url(var: &dyn Fn(&str) -> Option<String>, gist_id: &str) -> String {
    let base = var("PI_SHARE_VIEWER_URL")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_SHARE_VIEWER_URL.into());
    format!("{base}#{gist_id}")
}

/// `isNativeSessionFile`: the first line parses as an object with `kind: "header"`.
pub fn is_native_session_file(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else { return false };
    let text = String::from_utf8_lossy(&bytes);
    let first = text.split('\n').next().unwrap_or("");
    serde_json::from_str::<Value>(first).is_ok_and(|v| v.get("kind").and_then(Value::as_str) == Some("header"))
}

fn safe_integer_at_least(value: Option<&Value>, minimum: f64) -> bool {
    value
        .and_then(Value::as_f64)
        .is_some_and(|n| n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0 && n >= minimum)
}

/// `isJsonlStorageHeader`.
fn is_v4_header(value: &Value) -> bool {
    let Some(h) = value.as_object() else { return false };
    let optional_string = |k: &str| h.get(k).is_none_or(Value::is_string);
    h.get("kind").and_then(Value::as_str) == Some("header")
        && h.get("v").and_then(Value::as_f64) == Some(4.0)
        && h.get("id").is_some_and(Value::is_string)
        && h.get("cwd").is_some_and(Value::is_string)
        && safe_integer_at_least(h.get("storageVersion"), 1.0)
        && safe_integer_at_least(h.get("createdAt"), 0.0)
        && (h.get("nextSeq").is_none() || safe_integer_at_least(h.get("nextSeq"), 1.0))
        && optional_string("parentSessionId")
        && optional_string("legacyParentSessionPath")
}

/// A transaction's writes, validated as `parseJsonlTransaction` and `validateCommittedWrites` do.
fn parse_transaction(line: &str, state: &mut ReplayState) -> Result<Vec<Map<String, Value>>, ()> {
    let value: Value = serde_json::from_str(line).map_err(|_| ())?;
    let items = match value {
        Value::Array(items) => items,
        other => vec![other],
    };
    let mut writes = Vec::new();
    for item in items {
        let Value::Object(write) = item else { return Err(()) };
        if !safe_integer_at_least(write.get("seq"), 1.0) {
            return Err(());
        }
        let op = write.get("op").and_then(Value::as_str);
        match write.get("kind").and_then(Value::as_str) {
            Some("entry") if safe_integer_at_least(write.get("timestamp"), 0.0) => {}
            Some("usage") => {}
            Some("value") if matches!(op, Some("set" | "delete")) => {}
            Some("list") if matches!(op, Some("append" | "delete")) => {}
            _ => return Err(()),
        }
        writes.push(write);
    }
    let mut previous = state.next_seq - 1.0;
    let mut ids: HashSet<String> = HashSet::new();
    let mut entry_ids: HashSet<String> = HashSet::new();
    for write in &writes {
        let seq = write["seq"].as_f64().unwrap_or(0.0);
        if seq <= previous {
            return Err(());
        }
        previous = seq;
        let kind = write["kind"].as_str();
        if kind != Some("entry") && kind != Some("usage") {
            continue;
        }
        let id = write
            .get("id")
            .map(crate::js::string)
            .unwrap_or_else(|| "undefined".into());
        if state.ids.contains(&id) || ids.contains(&id) {
            return Err(());
        }
        if kind == Some("entry") {
            match write.get("parentId") {
                Some(Value::Null) => {}
                parent => {
                    let parent = parent.map(crate::js::string).unwrap_or_else(|| "undefined".into());
                    if !state.entries.contains_key(&parent) && !entry_ids.contains(&parent) {
                        return Err(());
                    }
                }
            }
        }
        ids.insert(id.clone());
        if kind == Some("entry") {
            entry_ids.insert(id);
        }
    }
    Ok(writes)
}

#[derive(Default)]
struct ReplayState {
    next_seq: f64,
    ids: HashSet<String>,
    entries: HashMap<String, Map<String, Value>>,
    values: HashMap<(String, String), Value>,
}

impl ReplayState {
    fn apply(&mut self, writes: Vec<Map<String, Value>>) {
        for mut write in writes {
            let seq = write["seq"].as_f64().unwrap_or(0.0);
            match write["kind"].as_str() {
                Some("entry") => {
                    write.shift_remove("kind");
                    let id = crate::js::string(&write["id"]);
                    self.ids.insert(id.clone());
                    self.entries.insert(id, write);
                }
                Some("usage") => {
                    self.ids.insert(crate::js::string(&write["id"]));
                }
                Some("value") => {
                    let key = (crate::js::string(&write["namespace"]), crate::js::string(&write["key"]));
                    if write["op"].as_str() == Some("delete") {
                        self.values.remove(&key);
                    } else {
                        self.values
                            .insert(key, write.get("value").cloned().unwrap_or(Value::Null));
                    }
                }
                _ => {}
            }
            self.next_seq = seq + 1.0;
        }
    }
}

fn iso(value: Option<&Value>) -> Value {
    Value::String(iso_string(value.and_then(Value::as_f64).unwrap_or(0.0) as i64))
}

/// `toSessionEntry`: a native entry as the Pi exporter's entry shape; an unknown type is `null`.
fn to_session_entry(entry: &Map<String, Value>) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), entry.get("id").cloned().unwrap_or(Value::Null));
    out.insert("parentId".into(), entry.get("parentId").cloned().unwrap_or(Value::Null));
    out.insert("timestamp".into(), iso(entry.get("timestamp")));
    let copy = |out: &mut Map<String, Value>, key: &str| {
        if let Some(v) = entry.get(key) {
            out.insert(key.into(), v.clone());
        }
    };
    match entry.get("type").and_then(Value::as_str) {
        Some("message") => {
            out.insert("type".into(), "message".into());
            copy(&mut out, "message");
        }
        Some("compaction") => {
            out.insert("type".into(), "compaction".into());
            copy(&mut out, "summary");
            if let Some(id) = entry.get("id") {
                out.insert("firstKeptEntryId".into(), id.clone());
            }
            copy(&mut out, "tokensBefore");
            copy(&mut out, "details");
            copy(&mut out, "usage");
            copy(&mut out, "fromHook");
        }
        Some("branch_summary") => {
            out.insert("type".into(), "branch_summary".into());
            let from = [entry.get("fromId"), entry.get("parentId"), entry.get("id")]
                .into_iter()
                .flatten()
                .find(|v| !v.is_null())
                .cloned()
                .unwrap_or(Value::Null);
            out.insert("fromId".into(), from);
            copy(&mut out, "summary");
            copy(&mut out, "details");
            copy(&mut out, "usage");
            copy(&mut out, "fromHook");
        }
        Some("custom") => {
            out.insert("type".into(), "custom".into());
            copy(&mut out, "customType");
            copy(&mut out, "data");
        }
        _ => return Value::Null,
    }
    Value::Object(out)
}

/// The session data of a native session file's `main` branch (`exportNativeSessionFile` up to `generateHtml`).
/// A torn last line is dropped from the file, as opening the session does.
fn native_session_data(path: &Path) -> Result<Value, String> {
    let shown = path.display();
    // `repo.list()` finds the file only when its header is a format-4 header.
    let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let header: Option<Value> = content
        .find('\n')
        .and_then(|end| serde_json::from_str(&content[..end]).ok())
        .filter(is_v4_header);
    let listed = path.extension().is_some_and(|e| e == "jsonl") && path.is_file();
    let Some(header) = header.filter(|_| listed) else {
        return Err(format!("Session is not in its session directory: {shown}"));
    };
    let (lines, torn): (Vec<&str>, bool) = match content.strip_suffix('\n') {
        Some(body) => (body.split('\n').collect(), false),
        None => {
            let last = content.rfind('\n').unwrap_or(0);
            (content[..last].split('\n').collect(), true)
        }
    };
    let id = header["id"].as_str().unwrap_or_default();
    if header["storageVersion"].as_f64() != Some(1.0) {
        return Err(format!(
            "Session {id} uses unsupported storage version {}",
            crate::js::string(&header["storageVersion"])
        ));
    }
    let mut state = ReplayState {
        next_seq: 1.0,
        ..ReplayState::default()
    };
    for (index, line) in lines.iter().enumerate().skip(1) {
        let writes = parse_transaction(line, &mut state)
            .map_err(|()| format!("Invalid JSONL storage {shown}: line {}", index + 1))?;
        state.apply(writes);
    }
    if torn {
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));
        std::fs::write(&tmp, format!("{}\n", lines.join("\n"))).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    }
    let mut entries: Vec<Value> = Vec::new();
    if let Some(tip) = state.values.get(&("pi.branch.tip".to_string(), "main".to_string()))
        && let Some(start) = tip.as_str()
    {
        let mut path_entries = Vec::new();
        let mut current = state
            .entries
            .get(start)
            .ok_or_else(|| format!("Unknown branch start: {start}"))?;
        loop {
            path_entries.push(current);
            match current.get("parentId") {
                Some(Value::Null) | None => break,
                Some(parent) => {
                    current = state
                        .entries
                        .get(&crate::js::string(parent))
                        .ok_or_else(|| "Corrupt branch: missing parent".to_string())?;
                }
            }
        }
        path_entries.reverse();
        entries = path_entries.into_iter().map(to_session_entry).collect();
    }
    let tip_id = path_tip(&entries);
    Ok(json!({
        "header": {
            "type": "session",
            "version": 3,
            "id": header["id"],
            "timestamp": iso(header.get("createdAt")),
            "cwd": header["cwd"],
        },
        "entries": entries,
        "leafId": tip_id,
    }))
}

/// `toSessionData` of native-export.ts: the session data of native entries (active branch, oldest first), as the
/// RPC `export_html` command renders a running session's transcript.
pub fn native_entries_data(id: &str, cwd: &str, created_at: f64, entries: &[Value]) -> Value {
    let entries: Vec<Value> = entries
        .iter()
        .filter_map(Value::as_object)
        .map(to_session_entry)
        .filter(|entry| !entry.is_null())
        .collect();
    let tip_id = path_tip(&entries);
    json!({
        "header": {"type": "session", "version": 3, "id": id, "timestamp": iso(Some(&json!(created_at))), "cwd": cwd},
        "entries": entries,
        "leafId": tip_id,
    })
}

/// `writeNativeSessionHtml`: render `data` to `output` (normalized; none: `ultron-session-<id>.html`), relative to
/// the process's working directory; returns the path as written.
pub fn write_native_session_html(
    data: &Value,
    id: &str,
    output: Option<&str>,
    env: &crate::Env,
) -> Result<String, String> {
    let target = match output {
        Some(out) if !out.is_empty() => config::paths::expand_tilde(out, &env.home).display().to_string(),
        _ => format!("ultron-session-{id}.html"),
    };
    let html = generate_html(data, (env.var)("COLORFGBG").as_deref());
    std::fs::write(env.cwd.join(&target), html)
        .map_err(|e| crate::usage::log::io_message(&e, "open", Path::new(&target)))?;
    Ok(target)
}

fn path_tip(entries: &[Value]) -> Value {
    entries.last().and_then(|e| e.get("id")).cloned().unwrap_or(Value::Null)
}

/// The session data of a Pi session file (`exportFromFile`).
fn pi_session_data(path: &Path) -> Result<Value, String> {
    let text = path.display().to_string();
    let manager = session_file::SessionManager::open(Box::new(session_file::SystemEnv), &text, None, None)
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "header": manager.header().cloned().unwrap_or(Value::Null),
        "entries": manager.entries().into_iter().cloned().collect::<Vec<_>>(),
        "leafId": manager.leaf_id().cloned().unwrap_or(Value::Null),
    }))
}

/// `ultron --export <input> [output]`: writes the page and returns the path it printed as written.
pub fn export(input: &str, output: Option<&str>, env: &crate::Env) -> Result<String, String> {
    let resolved = config::paths::resolve(input, &env.cwd, &env.home);
    let colorfgbg = (env.var)("COLORFGBG");
    let (data, default_name) = if is_native_session_file(&resolved) {
        let path = config::paths::normalize(&env.cwd.join(input));
        let stem = file_stem(&path);
        (native_session_data(&path)?, format!("ultron-session-{stem}.html"))
    } else {
        if !resolved.exists() {
            return Err(format!("File not found: {}", resolved.display()));
        }
        (
            pi_session_data(&resolved)?,
            format!("ultron-session-{}.html", file_stem(&resolved)),
        )
    };
    let target = match output {
        Some(out) if !out.is_empty() => config::paths::expand_tilde(out, &env.home).display().to_string(),
        _ => default_name,
    };
    let html = generate_html(&data, colorfgbg.as_deref());
    std::fs::write(env.cwd.join(&target), html)
        .map_err(|e| crate::usage::log::io_message(&e, "open", Path::new(&target)))?;
    Ok(target)
}

/// `basename(path, ".jsonl")`.
fn file_stem(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name.strip_suffix(".jsonl") {
        Some(stem) if !stem.is_empty() => stem.to_string(),
        _ => name,
    }
}
