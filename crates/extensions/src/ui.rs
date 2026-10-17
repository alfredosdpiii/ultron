//! Declarative UI that extensions contribute and the Rust TUI renders (`ExtensionUIContext` without render code).
//!
//! Fire-and-forget `ui/*` notifications become [`UiUpdate`]s; [`UiModel`] folds them into the state a frame paints
//! (status lines, panels, the working line, title, notices). Blocking prompts (`ui/select`, `ui/confirm`, `ui/input`,
//! `ui/editor`) are requests answered by the host's UI; [`no_ui_answer`] is the answer when there is none
//! (`noOpUIContext` in runner.ts).

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    AboveEditor,
    BelowEditor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiUpdate {
    /// `setStatus(key, text)`: one footer status segment; `None` removes it.
    Status {
        key: String,
        text: Option<String>,
    },
    /// `setWidget(key, lines, { placement })`: a panel of plain lines; `None` removes it.
    Panel {
        key: String,
        lines: Option<Vec<String>>,
        placement: Placement,
    },
    Notify {
        message: String,
        level: NoticeLevel,
    },
    WorkingMessage(Option<String>),
    WorkingVisible(bool),
    HiddenThinkingLabel(Option<String>),
    Title(String),
    EditorText(String),
    PasteToEditor(String),
    ToolsExpanded(bool),
    Theme(String),
}

fn string(params: &Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("\"{key}\" must be a string"))
}

fn optional_string(params: &Value, key: &str) -> Result<Option<String>, String> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("\"{key}\" must be a string or null")),
    }
}

/// Parses a `ui/*` notification; `Ok(None)` for methods that are not UI updates.
pub fn parse_update(method: &str, params: &Value) -> Result<Option<UiUpdate>, String> {
    let update = match method {
        "ui/setStatus" => UiUpdate::Status {
            key: string(params, "key")?,
            text: optional_string(params, "text")?,
        },
        "ui/setPanel" => {
            let lines = match params.get("lines") {
                None | Some(Value::Null) => None,
                Some(Value::Array(items)) => Some(
                    items
                        .iter()
                        .map(|l| l.as_str().map(str::to_string).ok_or("\"lines\" must be strings"))
                        .collect::<Result<_, _>>()?,
                ),
                Some(_) => return Err("\"lines\" must be an array of strings or null".into()),
            };
            let placement = match params.get("placement").and_then(Value::as_str) {
                None | Some("aboveEditor") => Placement::AboveEditor,
                Some("belowEditor") => Placement::BelowEditor,
                Some(other) => return Err(format!("unknown placement \"{other}\"")),
            };
            UiUpdate::Panel {
                key: string(params, "key")?,
                lines,
                placement,
            }
        }
        "ui/notify" => UiUpdate::Notify {
            message: string(params, "message")?,
            level: match params.get("type").and_then(Value::as_str) {
                None | Some("info") => NoticeLevel::Info,
                Some("warning") => NoticeLevel::Warning,
                Some("error") => NoticeLevel::Error,
                Some(other) => return Err(format!("unknown notice type \"{other}\"")),
            },
        },
        "ui/setWorkingMessage" => UiUpdate::WorkingMessage(optional_string(params, "message")?),
        "ui/setWorkingVisible" => UiUpdate::WorkingVisible(
            params
                .get("visible")
                .and_then(Value::as_bool)
                .ok_or("\"visible\" must be a boolean")?,
        ),
        "ui/setHiddenThinkingLabel" => UiUpdate::HiddenThinkingLabel(optional_string(params, "label")?),
        "ui/setTitle" => UiUpdate::Title(string(params, "title")?),
        "ui/setEditorText" => UiUpdate::EditorText(string(params, "text")?),
        "ui/pasteToEditor" => UiUpdate::PasteToEditor(string(params, "text")?),
        "ui/setToolsExpanded" => UiUpdate::ToolsExpanded(
            params
                .get("expanded")
                .and_then(Value::as_bool)
                .ok_or("\"expanded\" must be a boolean")?,
        ),
        "ui/setTheme" => UiUpdate::Theme(string(params, "name")?),
        _ => return Ok(None),
    };
    Ok(Some(update))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Panel {
    pub extension: String,
    pub key: String,
    pub lines: Vec<String>,
    pub placement: Placement,
}

/// What extensions currently contribute to the screen. Status lines and panels are keyed by (extension, key), so
/// two extensions never overwrite each other; they keep the order they first appeared in.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct UiModel {
    pub statuses: Vec<(String, String, String)>,
    pub panels: Vec<Panel>,
    pub notices: Vec<(String, NoticeLevel, String)>,
    pub working_message: Option<String>,
    pub working_visible: Option<bool>,
    pub hidden_thinking_label: Option<String>,
    pub title: Option<String>,
    /// Editor edits and theme or expansion changes for the UI to apply once.
    pub pending: Vec<UiUpdate>,
}

impl UiModel {
    pub fn apply(&mut self, extension: &str, update: UiUpdate) {
        match update {
            UiUpdate::Status { key, text } => {
                let slot = self.statuses.iter().position(|(e, k, _)| e == extension && *k == key);
                match (slot, text) {
                    (Some(i), Some(text)) => self.statuses[i].2 = text,
                    (Some(i), None) => {
                        self.statuses.remove(i);
                    }
                    (None, Some(text)) => self.statuses.push((extension.to_string(), key, text)),
                    (None, None) => {}
                }
            }
            UiUpdate::Panel { key, lines, placement } => {
                let slot = self
                    .panels
                    .iter()
                    .position(|p| p.extension == extension && p.key == key);
                match (slot, lines) {
                    (Some(i), Some(lines)) => {
                        self.panels[i].lines = lines;
                        self.panels[i].placement = placement;
                    }
                    (Some(i), None) => {
                        self.panels.remove(i);
                    }
                    (None, Some(lines)) => self.panels.push(Panel {
                        extension: extension.to_string(),
                        key,
                        lines,
                        placement,
                    }),
                    (None, None) => {}
                }
            }
            UiUpdate::Notify { message, level } => self.notices.push((extension.to_string(), level, message)),
            UiUpdate::WorkingMessage(message) => self.working_message = message,
            UiUpdate::WorkingVisible(visible) => self.working_visible = Some(visible),
            UiUpdate::HiddenThinkingLabel(label) => self.hidden_thinking_label = label,
            UiUpdate::Title(title) => self.title = Some(title),
            other => self.pending.push(other),
        }
    }

    /// Status texts in footer order.
    pub fn status_line(&self) -> Vec<&str> {
        self.statuses.iter().map(|(_, _, text)| text.as_str()).collect()
    }
}

/// The answer to a blocking UI request when the session has no UI (print and JSON modes).
pub fn no_ui_answer(method: &str) -> Option<Value> {
    match method {
        "ui/select" | "ui/input" | "ui/editor" => Some(Value::Null),
        "ui/confirm" => Some(json!(false)),
        "ui/getEditorText" => Some(json!("")),
        "ui/getToolsExpanded" => Some(json!(false)),
        "ui/getAllThemes" => Some(json!([])),
        // `setTheme(name)` as a request answers its result (`noOpUIContext`).
        "ui/setTheme" => Some(json!({"success": false, "error": "UI not available"})),
        _ => None,
    }
}

/// The `ui_prompt_start`/`ui_prompt_end` kind of a blocking UI request.
pub fn prompt_kind(method: &str) -> Option<&'static str> {
    match method {
        "ui/select" => Some("select"),
        "ui/confirm" => Some("confirm"),
        "ui/input" => Some("input"),
        "ui/editor" => Some("editor"),
        _ => None,
    }
}
