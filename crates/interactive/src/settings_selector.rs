//! Pi's `/settings` (`SettingsSelectorComponent`, modes/interactive/components/settings-selector.ts, on the TUI's
//! `SettingsList` with search and submenus, settings-submenu.ts, models-settings.ts and the native TUI's Models
//! section, experimental/client-tui-models-settings.ts). It reads a snapshot of the settings ([`SettingsInput`])
//! and returns [`SettingsEvent`]s; the client saves each change and applies the live ones.

use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::{Value, json};
use tui::components::Component;
use tui::fuzzy::fuzzy_filter;
use tui::input::Input;
use tui::keybindings::KeybindingsManager;
use tui::mouse::{MouseButton, MouseEvent, MouseEventType, MouseResult};
use tui::select::{SelectEvent, SelectItem, SelectList, SelectListTheme};
use tui::theme::Theme;
use tui::utils::{truncate_to_width, visible_width, wrap_text_with_ansi};

use crate::components::{bold, fg};
use crate::lane::ModelInfo;

const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5-5";
const CLAUDE_CODE: &str = "claude-code";
const CLEAR_OVERRIDE: &str = "__clear__";
const GROUP_PREFIX: &str = "\u{0}group:";
const AUTOMATIC_THEME: &str = "/";
const HTTP_IDLE: [(&str, u64); 5] = [
    ("30 sec", 30_000),
    ("1 min", 60_000),
    ("2 min", 120_000),
    ("5 min", 300_000),
    ("disabled", 0),
];

fn thinking_description(level: &str) -> &'static str {
    match level {
        "off" => "No reasoning",
        "minimal" => "Very brief reasoning (~1k tokens)",
        "low" => "Light reasoning (~2k tokens)",
        "medium" => "Moderate reasoning (~8k tokens)",
        "high" => "Deep reasoning (~16k tokens)",
        "xhigh" => "Extra-high reasoning (~32k tokens)",
        _ => "Maximum reasoning",
    }
}

/// `formatHttpIdleTimeoutMs`.
pub fn format_http_idle(ms: u64) -> String {
    HTTP_IDLE
        .iter()
        .find(|(_, v)| *v == ms)
        .map_or_else(|| format!("{} sec", ms as f64 / 1000.0), |(l, _)| (*l).to_string())
}

/// What `/settings` reads.
pub struct SettingsInput {
    /// The merged settings document.
    pub settings: Value,
    /// Model settings an environment variable overrides: key -> (variable, value).
    pub env_overrides: BTreeMap<String, (String, String)>,
    pub models: Vec<ModelInfo>,
    pub session: Option<ModelInfo>,
    pub themes: Vec<String>,
    /// `light` or `dark`.
    pub terminal_theme: String,
    pub supports_images: bool,
    /// Formatted `app.message.followUp` and `app.thinking.cycle` keys (`keyDisplayText`).
    pub follow_up_key: String,
    pub cycle_thinking_key: String,
}

impl SettingsInput {
    fn get(&self, key: &str) -> &Value {
        let mut v = &self.settings;
        for part in key.split('.') {
            v = &v[part];
        }
        v
    }
    fn bool(&self, key: &str, fallback: bool) -> bool {
        self.get(key).as_bool().unwrap_or(fallback)
    }
    fn num(&self, key: &str, fallback: f64) -> f64 {
        self.get(key).as_f64().unwrap_or(fallback)
    }
    fn str(&self, key: &str, fallback: &str) -> String {
        self.get(key).as_str().unwrap_or(fallback).to_string()
    }
    fn text(&self, key: &str) -> Option<String> {
        self.get(key).as_str().filter(|s| !s.is_empty()).map(str::to_string)
    }
}

/// What the client does.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingsEvent {
    /// Save `key` (a settings path, or `modelThinkingLevel` with `{provider, modelId, level}`).
    Change(String, Value),
    /// The Models section's session row: switch the session's model and save it as the default.
    SessionModel(String),
    /// A Models row changed (`label`, value, whether it applies to `ultron claude`).
    ModelSetting {
        key: String,
        label: String,
        value: String,
    },
    Cancelled,
    /// `onThemePreview`: show this theme setting (a name or `light/dark`) while the user browses themes.
    Preview(String),
}

fn str_bool(b: bool) -> String {
    if b { "true" } else { "false" }.to_string()
}

#[derive(Clone)]
enum SubKind {
    Warnings,
    Theme,
    ModelThinking,
    Models,
    ModelPick(String),
    LightTheme,
    DarkTheme,
}

#[derive(Clone)]
struct Item {
    id: String,
    label: String,
    description: Option<String>,
    value: String,
    values: Vec<String>,
    sub: Option<SubKind>,
}

fn item(id: &str, label: &str, description: &str, value: String, values: &[&str]) -> Item {
    Item {
        id: id.into(),
        label: label.into(),
        description: Some(description.into()),
        value,
        values: values.iter().map(|v| (*v).to_string()).collect(),
        sub: None,
    }
}

/// `SettingsList`: aligned label/value rows, optional search, the selected item's description and a hint.
struct List {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    items: Vec<Item>,
    filtered: Vec<usize>,
    selected: usize,
    max_visible: usize,
    search: Option<Input>,
    /// The row a left press selected (`mousePressedIndex`).
    pressed: Option<usize>,
}

enum ListAction {
    Changed(String, String),
    Open(usize),
    Cancel,
}

impl List {
    fn new(theme: Rc<Theme>, keys: KeybindingsManager, items: Vec<Item>, max_visible: usize, search: bool) -> Self {
        let filtered = (0..items.len()).collect();
        let search = search.then(|| {
            let mut input = Input::new(keys.clone());
            input.focused = true;
            input
        });
        List {
            theme,
            keys,
            items,
            filtered,
            selected: 0,
            max_visible,
            search,
            pressed: None,
        }
    }

    /// `SettingsList.handleMouse` (no submenu open): the search row goes to the search input, the wheel moves the
    /// selection, a left press selects the row and the click that follows activates the pressed row.
    fn handle_mouse(&mut self, event: &MouseEvent) -> Option<(MouseResult, Option<ListAction>)> {
        if let Some(search) = self.search.as_mut() {
            if event.y == 0 {
                let result = search.handle_mouse(event)?;
                return Some((
                    MouseResult {
                        focus: Some(true),
                        ..result
                    },
                    None,
                ));
            }
            if event.y == 1 {
                return None;
            }
        }
        let n = self.filtered.len();
        if n == 0 {
            return None;
        }
        let handled = |render: Option<bool>, focus: Option<bool>| MouseResult {
            handled: Some(true),
            render,
            focus,
            ..MouseResult::default()
        };
        if event.kind == MouseEventType::Wheel
            && let Some(delta) = event.wheel_delta.filter(|d| *d != 0)
        {
            let previous = self.selected;
            self.selected = if delta < 0 {
                self.selected.saturating_sub(1)
            } else {
                (self.selected + 1).min(n - 1)
            };
            return Some((handled(Some(self.selected != previous), None), None));
        }
        if event.button != MouseButton::Left || !matches!(event.kind, MouseEventType::Press | MouseEventType::Click) {
            return None;
        }
        let offset = if self.search.is_some() { 2 } else { 0 };
        let (start, end) = self.range();
        let index = start as i64 + event.y - offset;
        if index < start as i64 || index >= end as i64 {
            return None;
        }
        let index = index as usize;
        if event.kind == MouseEventType::Press {
            self.pressed = Some(index);
            self.selected = index;
            return Some((handled(None, Some(true)), None));
        }
        self.selected = self.pressed.take().unwrap_or(index);
        let action = self.activate();
        Some((handled(None, None), action))
    }

    fn update(&mut self, id: &str, value: &str) {
        if let Some(item) = self.items.iter_mut().find(|i| i.id == id) {
            item.value = value.to_string();
        }
    }

    fn range(&self) -> (usize, usize) {
        let n = self.filtered.len() as isize;
        let max = self.max_visible as isize;
        let start = (self.selected as isize - max / 2).min(n - max).max(0) as usize;
        (start, (start + self.max_visible).min(self.filtered.len()))
    }

    fn hint(&self, lines: &mut Vec<String>, width: usize) {
        lines.push(String::new());
        let text = if self.search.is_some() {
            "  Type to search · Enter/Space to change · Esc to cancel"
        } else {
            "  Enter/Space to change · Esc to cancel"
        };
        lines.push(truncate_to_width(&fg(&self.theme, "dim", text), width, "...", false));
    }

    fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let mut lines = Vec::new();
        if let Some(search) = self.search.as_mut() {
            lines.extend(search.render(width));
            lines.push(String::new());
        }
        if self.items.is_empty() {
            lines.push(fg(&t, "dim", "  No settings available"));
            if self.search.is_some() {
                self.hint(&mut lines, width);
            }
            return lines;
        }
        if self.filtered.is_empty() {
            lines.push(truncate_to_width(
                &fg(&t, "dim", "  No matching settings"),
                width,
                "...",
                false,
            ));
            self.hint(&mut lines, width);
            return lines;
        }
        let (start, end) = self.range();
        let label_width = self
            .items
            .iter()
            .map(|i| visible_width(&i.label))
            .max()
            .unwrap_or(0)
            .min(36);
        for row in start..end {
            let it = &self.items[self.filtered[row]];
            let selected = row == self.selected;
            let prefix = if selected {
                fg(&t, "accent", "→ ")
            } else {
                "  ".to_string()
            };
            let padded = format!(
                "{}{}",
                it.label,
                " ".repeat(label_width.saturating_sub(visible_width(&it.label)))
            );
            let label = if selected { fg(&t, "accent", &padded) } else { padded };
            let used = visible_width(&prefix) + label_width + 2;
            let value = truncate_to_width(&it.value, width.saturating_sub(used + 2), "", false);
            let value = if selected {
                fg(&t, "accent", &value)
            } else {
                fg(&t, "muted", &value)
            };
            lines.push(truncate_to_width(
                &format!("{prefix}{label}  {value}"),
                width,
                "...",
                false,
            ));
        }
        if start > 0 || end < self.filtered.len() {
            let text = format!("  ({}/{})", self.selected + 1, self.filtered.len());
            lines.push(fg(
                &t,
                "dim",
                &truncate_to_width(&text, width.saturating_sub(2), "", false),
            ));
        }
        if let Some(description) = self
            .filtered
            .get(self.selected)
            .and_then(|&i| self.items[i].description.clone())
        {
            lines.push(String::new());
            for line in wrap_text_with_ansi(&description, width.saturating_sub(4)) {
                lines.push(fg(&t, "dim", &format!("  {line}")));
            }
        }
        self.hint(&mut lines, width);
        lines
    }

    fn handle_input(&mut self, data: &str) -> Option<ListAction> {
        let k = |id: &str| self.keys.matches(data, id);
        let n = self.filtered.len();
        let search_empty = self.search.as_ref().is_none_or(|s| s.value().is_empty());
        if k("tui.select.up") {
            if n > 0 {
                self.selected = if self.selected == 0 { n - 1 } else { self.selected - 1 };
            }
        } else if k("tui.select.down") {
            if n > 0 {
                self.selected = if self.selected + 1 >= n { 0 } else { self.selected + 1 };
            }
        } else if k("tui.select.confirm") || (data == " " && search_empty) {
            return self.activate();
        } else if k("tui.select.cancel") {
            return Some(ListAction::Cancel);
        } else if let Some(search) = self.search.as_mut() {
            search.handle_input(data);
            let query = search.value().to_string();
            let indexed: Vec<usize> = (0..self.items.len()).collect();
            self.filtered = fuzzy_filter(&indexed, &query, |&i| self.items[i].label.clone());
            self.selected = 0;
        }
        None
    }

    fn activate(&mut self) -> Option<ListAction> {
        let &index = self.filtered.get(self.selected)?;
        let it = &mut self.items[index];
        if it.sub.is_some() {
            return Some(ListAction::Open(index));
        }
        if it.values.is_empty() {
            return None;
        }
        let at = it.values.iter().position(|v| *v == it.value);
        let next = it.values[at.map_or(0, |i| (i + 1) % it.values.len())].clone();
        it.value = next.clone();
        Some(ListAction::Changed(it.id.clone(), next))
    }
}

/// `SelectSubmenu`: a titled select list, optionally searchable.
struct SelectSubmenu {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    title: String,
    description: String,
    all: Vec<SelectItem>,
    list: SelectList,
    search: Option<Input>,
    layout: (usize, usize),
}

enum SelectOutcome {
    Picked(String),
    Back,
    /// The highlighted item moved to this value.
    Highlight(String),
}

fn select_theme(t: &Rc<Theme>) -> SelectListTheme {
    let style = |color: &'static str| {
        let t = Rc::clone(t);
        Box::new(move |s: &str| fg(&t, color, s)) as Box<dyn Fn(&str) -> String>
    };
    SelectListTheme {
        selected_prefix: style("accent"),
        selected_text: style("accent"),
        description: style("muted"),
        scroll_info: style("muted"),
        no_match: style("muted"),
    }
}

impl SelectSubmenu {
    fn new(
        theme: Rc<Theme>,
        keys: KeybindingsManager,
        (title, description): (&str, &str),
        items: Vec<SelectItem>,
        current: &str,
        searchable: bool,
        layout: (usize, usize),
    ) -> Self {
        let list = Self::build(&theme, &items, current, layout);
        let search = searchable.then(|| {
            let mut input = Input::new(keys.clone());
            input.focused = true;
            input
        });
        SelectSubmenu {
            theme,
            keys,
            title: title.into(),
            description: description.into(),
            all: items,
            list,
            search,
            layout,
        }
    }

    fn build(theme: &Rc<Theme>, items: &[SelectItem], current: &str, layout: (usize, usize)) -> SelectList {
        let mut list = SelectList::new(items.to_vec(), items.len().min(10), select_theme(theme));
        list.min_primary_column_width = Some(layout.0);
        list.max_primary_column_width = Some(layout.1);
        if let Some(i) = items.iter().position(|o| o.value == current) {
            list.set_selected_index(i);
        }
        list
    }

    fn handle_input(&mut self, data: &str) -> Option<SelectOutcome> {
        let nav = [
            "tui.select.up",
            "tui.select.down",
            "tui.select.confirm",
            "tui.select.cancel",
        ]
        .iter()
        .any(|id| self.keys.matches(data, id));
        if let Some(search) = self.search.as_mut()
            && !nav
        {
            search.handle_input(data);
            let query = search.value().to_string();
            let filtered = if query.is_empty() {
                self.all.clone()
            } else {
                fuzzy_filter(&self.all, &query, |i| {
                    format!("{} {}", i.label, i.description.clone().unwrap_or_default())
                })
            };
            self.list = Self::build(&self.theme, &filtered, "", self.layout);
            return None;
        }
        match self.list.handle_input(data) {
            Some(SelectEvent::Selected(item)) => Some(SelectOutcome::Picked(item.value)),
            Some(SelectEvent::Cancelled) => Some(SelectOutcome::Back),
            Some(SelectEvent::SelectionChanged(item)) => Some(SelectOutcome::Highlight(item.value)),
            None => None,
        }
    }

    fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let mut lines = wrap_text_with_ansi(&bold(&fg(&t, "accent", &self.title)), width);
        if !self.description.is_empty() {
            lines.push(String::new());
            lines.extend(wrap_text_with_ansi(&fg(&t, "muted", &self.description), width));
        }
        if let Some(search) = self.search.as_mut() {
            lines.push(String::new());
            lines.extend(search.render(width));
        }
        lines.push(String::new());
        lines.extend(self.list.render(width));
        lines.push(String::new());
        let hint = if self.search.is_some() {
            "  Type to filter · Enter to select · Esc to go back"
        } else {
            "  Enter to select · Esc to go back"
        };
        lines.extend(wrap_text_with_ansi(&fg(&t, "dim", hint), width));
        lines
    }
}

/// A model picker entry: `(value, label, description, group)`.
type ModelChoice = (String, String, Option<String>, Option<String>);

/// One row of the Models section.
#[derive(Clone)]
struct ModelRow {
    id: String,
    label: String,
    description: String,
    display: String,
    locked: Option<String>,
    heading: bool,
    /// `(value, label, description, group)`.
    choices: Vec<ModelChoice>,
    current: String,
}

/// The open submenu.
enum Sub {
    Select {
        kind: SubKind,
        menu: SelectSubmenu,
    },
    List {
        kind: SubKind,
        list: List,
        header: Vec<String>,
    },
}

pub struct SettingsSelector {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    input: SettingsInput,
    list: List,
    /// Submenus, innermost last.
    stack: Vec<Sub>,
    warnings_extra: bool,
    model_thinking: BTreeMap<String, String>,
    /// The per-model thinking step: the chosen model.
    thinking_model: Option<String>,
    /// The theme submenu's automatic pair.
    light: String,
    dark: String,
}

fn model_ref(m: &ModelInfo) -> String {
    format!("{}/{}", m.provider, m.id)
}

impl SettingsSelector {
    pub fn new(theme: Rc<Theme>, keys: KeybindingsManager, input: SettingsInput) -> Self {
        let model_thinking: BTreeMap<String, String> = input
            .get("modelThinkingLevels")
            .as_object()
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        let theme_setting = input.str("theme", "dark");
        let (light, dark) = automatic_themes(&theme_setting, &input.themes);
        let mut selector = SettingsSelector {
            list: List::new(Rc::clone(&theme), keys.clone(), Vec::new(), 10, true),
            theme,
            keys,
            warnings_extra: input.get("warnings")["anthropicExtraUsage"].as_bool().unwrap_or(true),
            input,
            stack: Vec::new(),
            model_thinking,
            thinking_model: None,
            light,
            dark,
        };
        let items = selector.items();
        selector.list.items = items;
        selector.list.filtered = (0..selector.list.items.len()).collect();
        selector
    }

    fn items(&self) -> Vec<Item> {
        let c = &self.input;
        let b = |key: &str, fallback: bool| str_bool(c.bool(key, fallback));
        let mut items = vec![
            item(
                "autocompact",
                "Auto-compact",
                "Automatically compact context when it gets too large",
                b("compaction.enabled", true),
                &["true", "false"],
            ),
            item(
                "steering-mode",
                "Steering mode",
                "Enter while streaming queues steering messages. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once.",
                c.str("steeringMode", "all"),
                &["one-at-a-time", "all"],
            ),
            item(
                "follow-up-mode",
                "Follow-up mode",
                &format!(
                    "{} queues follow-up messages until agent stops. 'one-at-a-time': deliver one, wait for response. 'all': deliver all at once.",
                    c.follow_up_key
                ),
                c.str("followUpMode", "all"),
                &["one-at-a-time", "all"],
            ),
            item(
                "transport",
                "Transport",
                "Preferred transport for providers that support multiple transports",
                c.str("transport", "auto"),
                &["sse", "websocket", "websocket-cached", "auto"],
            ),
            item(
                "http-idle-timeout",
                "HTTP idle timeout",
                "Maximum idle gap while waiting for HTTP headers or body chunks. Disable for local models that pause longer than five minutes.",
                format_http_idle(c.num("httpIdleTimeoutMs", 300_000.0) as u64),
                &HTTP_IDLE.map(|(l, _)| l),
            ),
            item(
                "cache-warming-mode",
                "Cache warming",
                "off; streaming while the agent runs; idle also between runs while continuation stays profitable",
                c.str("cacheWarming", "streaming"),
                &["off", "streaming", "idle"],
            ),
            item(
                "hide-thinking",
                "Hide thinking",
                "Hide thinking blocks in assistant responses",
                b("hideThinkingBlock", false),
                &["true", "false"],
            ),
            item(
                "mermaid-rendering",
                "Mermaid diagrams",
                "Render Mermaid code blocks as Unicode diagrams",
                c.str("markdown.mermaid", "streaming"),
                &["off", "final", "streaming"],
            ),
            item(
                "cache-miss-notices",
                "Cache miss notices",
                "Show transcript notices for cache costs and provider recovery diagnostics",
                b("showCacheMissNotices", false),
                &["true", "false"],
            ),
            item(
                "collapse-changelog",
                "Collapse changelog",
                "Show condensed changelog after updates",
                b("collapseChangelog", false),
                &["true", "false"],
            ),
            item(
                "quiet-startup",
                "Quiet startup",
                "Disable verbose printing at startup",
                b("quietStartup", false),
                &["true", "false"],
            ),
            item(
                "rlm-pane-auto-open",
                "RLM pane auto-open",
                "Open the RLM pane beside the chat when RLM work starts (wide terminals; the editor keeps the focus)",
                b("rlmPaneAutoOpen", true),
                &["true", "false"],
            ),
            item(
                "autoreview-enabled",
                "Autoreview",
                "Automatic pull-request reviews as your gh account (ultron autoreview run and its service; /autoreview on starts the service)",
                b("autoreview.enabled", true),
                &["true", "false"],
            ),
            item(
                "autoreview-ack",
                "Autoreview acknowledgement",
                "Post a short comment (an Ultron line) when a review starts",
                b("autoreview.ack", true),
                &["true", "false"],
            ),
            {
                let art = c.str("autoreview.ackArt", "logo");
                let values: &[&str] = if art == "custom" {
                    &["custom", "logo", "none"]
                } else {
                    &["logo", "none"]
                };
                item(
                    "autoreview-ack-art",
                    "Autoreview acknowledgement art",
                    "Ultron's logo under the acknowledgement line, or none (custom text is set in settings.json)",
                    art,
                    values,
                )
            },
            item(
                "install-telemetry",
                "Install telemetry",
                "Send provider attribution headers (OpenRouter, NVIDIA NIM, Cloudflare); Ultron sends no install pings",
                b("enableInstallTelemetry", true),
                &["true", "false"],
            ),
            item(
                "default-project-trust",
                "Default project trust",
                "Fallback behavior when no extension or saved trust decision decides project trust",
                match c.str("defaultProjectTrust", "ask").as_str() {
                    "always" => "Always trust",
                    "never" => "Never trust",
                    _ => "Ask",
                }
                .to_string(),
                &["Ask", "Always trust", "Never trust"],
            ),
            item(
                "double-escape-action",
                "Double-escape action",
                "Action when pressing Escape twice with empty editor",
                c.str("doubleEscapeAction", "tree"),
                &["tree", "fork", "none"],
            ),
            item(
                "tree-filter-mode",
                "Tree filter mode",
                "Default filter when opening /tree",
                c.str("treeFilterMode", "default"),
                &["default", "no-tools", "user-only", "labeled-only", "all"],
            ),
            Item {
                sub: Some(SubKind::Warnings),
                ..item(
                    "warnings",
                    "Warnings",
                    "Enable or disable individual warnings",
                    "configure".into(),
                    &[],
                )
            },
            Item {
                sub: Some(SubKind::ModelThinking),
                ..item(
                    "model-thinking",
                    "Default thinking level per model",
                    &format!(
                        "Override the default thinking level for specific models. {} cycles in-session.",
                        c.cycle_thinking_key
                    ),
                    self.thinking_summary(),
                    &[],
                )
            },
            item(
                "tui-mode",
                "TUI mode",
                "Interface layout; regular keeps the finished transcript in the terminal's scrollback",
                c.str("tuiMode", "fullscreen"),
                &["regular", "fullscreen"],
            ),
            item(
                "fullscreen-exit-output",
                "Fullscreen exit output",
                "Print the transcript or only a session resume hint when exiting fullscreen mode",
                c.str("fullscreenExitOutput", "transcript"),
                &["transcript", "resume-hint"],
            ),
            item(
                "fullscreen-scrollbar",
                "Fullscreen scrollbar",
                "Scrollbar behavior in fullscreen mode; has no effect in regular mode",
                c.str("fullscreenScrollbar", "auto"),
                &["auto", "always", "hidden"],
            ),
            item(
                "fullscreen-copy-on-select",
                "Fullscreen copy on select",
                "Automatically copy selected text in fullscreen mode; disable to copy selections with Ctrl+X",
                b("fullscreenCopyOnSelect", true),
                &["true", "false"],
            ),
            Item {
                sub: Some(SubKind::Theme),
                ..item(
                    "theme",
                    "Theme",
                    "Color theme for the interface",
                    c.str("theme", "dark"),
                    &[],
                )
            },
        ];
        let mut at = 1;
        if c.supports_images {
            items.insert(
                1,
                item(
                    "show-images",
                    "Show images",
                    "Render images inline in terminal",
                    b("terminal.showImages", true),
                    &["true", "false"],
                ),
            );
            items.insert(
                2,
                item(
                    "image-width-cells",
                    "Image width",
                    "Preferred inline image width in terminal cells",
                    (c.num("terminal.imageWidthCells", 60.0) as i64).to_string(),
                    &["60", "80", "120"],
                ),
            );
            at = 3;
        }
        let tail = [
            item(
                "auto-resize-images",
                "Auto-resize images",
                "Resize large images to 2000x2000 max for better model compatibility",
                b("images.autoResize", true),
                &["true", "false"],
            ),
            item(
                "block-images",
                "Block images",
                "Prevent images from being sent to LLM providers",
                b("images.blockImages", false),
                &["true", "false"],
            ),
            item(
                "skill-commands",
                "Skill commands",
                "Register skills as /skill:name commands",
                b("enableSkillCommands", true),
                &["true", "false"],
            ),
            item(
                "show-hardware-cursor",
                "Show hardware cursor",
                "Show the terminal cursor while still positioning it for IME support",
                b("showHardwareCursor", false),
                &["true", "false"],
            ),
            item(
                "editor-padding",
                "Editor padding",
                "Horizontal padding for input editor (0-3)",
                (c.num("editorPaddingX", 0.0) as i64).to_string(),
                &["0", "1", "2", "3"],
            ),
            item(
                "output-padding",
                "Output padding",
                "Horizontal padding for user messages, assistant messages, and thinking",
                if c.num("outputPad", 1.0) == 0.0 { "0" } else { "1" }.into(),
                &["0", "1"],
            ),
            item(
                "autocomplete-max-visible",
                "Autocomplete max items",
                "Max visible items in autocomplete dropdown (3-20)",
                (c.num("autocompleteMaxVisible", 5.0) as i64).to_string(),
                &["3", "5", "7", "10", "15", "20"],
            ),
            item(
                "clear-on-shrink",
                "Clear on shrink",
                "Clear empty rows when content shrinks (may cause flicker)",
                b("terminal.clearOnShrink", false),
                &["true", "false"],
            ),
            item(
                "terminal-progress",
                "Terminal progress",
                "Show OSC 9;4 progress indicators in the terminal tab bar",
                b("terminal.showTerminalProgress", false),
                &["true", "false"],
            ),
        ];
        for (i, it) in tail.into_iter().enumerate() {
            items.insert(at + i, it);
        }
        items.insert(
            0,
            Item {
                sub: Some(SubKind::Models),
                ..item(
                    "models",
                    "Models",
                    "Models of the session, inference frames, /review and sub-agents, and of Claude Code mode. Changes apply to the running session.",
                    self.models_summary(),
                    &[],
                )
            },
        );
        items
    }

    fn thinking_summary(&self) -> String {
        match self.model_thinking.len() {
            0 => "none".into(),
            n => format!("{n} configured"),
        }
    }

    fn model_rows(&self) -> Vec<ModelRow> {
        let c = &self.input;
        let choice = |m: &ModelInfo| {
            (
                model_ref(m),
                m.id.clone(),
                (!m.name.is_empty() && m.name != m.id).then(|| m.name.clone()),
                Some(m.provider.clone()),
            )
        };
        let all: Vec<_> = c.models.iter().map(choice).collect();
        let tools: Vec<_> = c
            .models
            .iter()
            .filter(|m| m.provider != CLAUDE_CODE)
            .map(choice)
            .collect();
        let claude: Vec<_> = c
            .models
            .iter()
            .filter(|m| m.provider == CLAUDE_CODE)
            .map(choice)
            .collect();
        let session = c.session.as_ref().map_or("none".to_string(), model_ref);
        let plain = |v: &str, l: &str| (v.to_string(), l.to_string(), None, None);
        let with = |head: ModelChoice, rest: &[ModelChoice]| {
            std::iter::once(head).chain(rest.iter().cloned()).collect::<Vec<_>>()
        };
        let row =
            |id: &str, label: &str, description: &str, display: String, choices: Vec<_>, current: Option<String>| {
                let env = c.env_overrides.get(id);
                ModelRow {
                    id: id.into(),
                    label: label.into(),
                    description: description.into(),
                    display: env.map_or(display, |(name, value)| format!("{value} (set by {name})")),
                    locked: env.map(|(name, _)| {
                        format!("Set by {name}; unset it to choose here (the variable wins over the setting).")
                    }),
                    heading: false,
                    choices,
                    current: current.unwrap_or_default(),
                }
            };
        let frame = c.text("rlm.frameModel");
        let effective_frame = c
            .env_overrides
            .get("rlm.frameModel")
            .map(|e| e.1.clone())
            .or(frame.clone());
        let review = c.text("review.model");
        let child = c.text("rlm.childModel");
        let frame_thinking = c.text("rlm.frameThinking");
        let cc_root = c.text("claudeCode.model");
        let cc_frame = c.text("claudeCode.frameModel");
        let cc_child = c.text("claudeCode.childModel");
        let cc_ultron = c.text("claudeCode.ultronChildModel");
        let saved_default = match (c.text("defaultProvider"), c.text("defaultModel")) {
            (Some(p), Some(m)) => Some(format!("{p}/{m}")),
            _ => None,
        };
        let alias = |v: &Option<String>| v.as_ref().map(|v| format!("{CLAUDE_CODE}/{v}"));
        let setting = |v: &Option<String>, fallback: String| v.as_ref().map_or(fallback, |v| format!("{v} (setting)"));
        vec![
            row(
                "session",
                "Session model",
                &format!(
                    "The Session's own model (tool-capable models only). Choosing one switches the running Session now and saves it as the default, as /model does. Saved default: {}.",
                    saved_default.clone().unwrap_or_else(|| "none".into())
                ),
                format!("{session} (session)"),
                tools.clone(),
                c.session.as_ref().map(model_ref),
            ),
            row(
                "rlm.frameModel",
                "Frame model",
                "Code-free rlm.infer / rlm.map frames that name no model= (and /review frames without a review model). Order: model= > ULTRON_RLM_FRAME_MODEL > this setting > the session model.",
                setting(&frame, format!("same as session ({session})")),
                with(plain("", "Same as session model"), &all),
                frame.clone(),
            ),
            row(
                "review.model",
                "Review model",
                "/review frames. Order: --model > ULTRON_REVIEW_MODEL > this setting > the frame model.",
                match (&review, &effective_frame) {
                    (Some(r), _) => format!("{r} (setting)"),
                    (None, Some(f)) => format!("{f} (frame model)"),
                    _ => format!("same as session ({session})"),
                },
                with(plain("", "Same as frame model"), &all),
                review.clone(),
            ),
            row(
                "rlm.childModel",
                "Sub-agent model",
                "rlm.spawn sub-agents that name no model=. Sub-agents call tools, so claude-code models are not offered.",
                setting(&child, format!("same as session ({session})")),
                with(plain("", "Same as session model"), &tools),
                child.clone(),
            ),
            row(
                "rlm.frameThinking",
                "Frame thinking",
                "Thinking level of inference frames (claude-code frames pass it as --effort).",
                setting(&frame_thinking, "same as session".into()),
                with(
                    plain("", "Same as session"),
                    &["off", "low", "medium", "high"].map(|l| plain(l, l)),
                ),
                frame_thinking.clone(),
            ),
            ModelRow {
                id: "claude-code-heading".into(),
                label: "Claude Code mode".into(),
                description: "`ultron claude`: Claude Code as the root agent. These apply when it next starts.".into(),
                display: String::new(),
                locked: None,
                heading: true,
                choices: Vec::new(),
                current: String::new(),
            },
            row(
                "claudeCode.model",
                "Claude Code root model",
                "The model Claude Code itself runs (claude --model); ULTRON_CLAUDE_MODEL overrides it.",
                setting(&cc_root, format!("{DEFAULT_CLAUDE_MODEL} (default)")),
                with(plain("", &format!("Default ({DEFAULT_CLAUDE_MODEL})")), &claude),
                alias(&cc_root),
            ),
            row(
                "claudeCode.frameModel",
                "Claude Code frame model",
                "rlm.map / rlm.infer frames under `ultron claude`; ULTRON_CLAUDE_FRAME_MODEL overrides it.",
                setting(&cc_frame, format!("{CLAUDE_CODE}/{DEFAULT_CLAUDE_MODEL} (default)")),
                with(
                    plain("", &format!("Default ({CLAUDE_CODE}/{DEFAULT_CLAUDE_MODEL})")),
                    &all,
                ),
                cc_frame.clone(),
            ),
            row(
                "claudeCode.childModel",
                "Claude Code sub-agent model",
                "Model of Claude Code sub-agents (children=claude); ULTRON_CLAUDE_CHILD_MODEL overrides it.",
                setting(&cc_child, format!("{DEFAULT_CLAUDE_MODEL} (default)")),
                with(plain("", &format!("Default ({DEFAULT_CLAUDE_MODEL})")), &claude),
                alias(&cc_child),
            ),
            row(
                "claudeCode.ultronChildModel",
                "Ultron sub-agent model",
                "Model of Ultron sub-agents under `ultron claude` (children=ultron); tool-capable models only.",
                setting(
                    &cc_ultron,
                    format!("default model ({})", saved_default.unwrap_or_else(|| "none".into())),
                ),
                with(plain("", "Default model"), &tools),
                cc_ultron.clone(),
            ),
        ]
    }

    fn models_summary(&self) -> String {
        let rows = self.model_rows();
        let frame = rows
            .iter()
            .find(|r| r.id == "rlm.frameModel")
            .map(|r| r.display.clone());
        format!("frames: {}", frame.unwrap_or_default())
    }

    /// The models submenu's list.
    fn models_list(&self) -> (List, Vec<String>) {
        let t = &self.theme;
        let items = self
            .model_rows()
            .into_iter()
            .map(|row| Item {
                id: row.id.clone(),
                label: if row.heading {
                    bold(&row.label)
                } else {
                    row.label.clone()
                },
                description: Some(match &row.locked {
                    None => row.description.clone(),
                    Some(l) => format!("{} {l}", row.description),
                }),
                value: match &row.locked {
                    None => row.display.clone(),
                    Some(_) => format!("{} · locked", row.display),
                },
                values: Vec::new(),
                sub: (!row.heading && row.locked.is_none() && !row.choices.is_empty())
                    .then(|| SubKind::ModelPick(row.id.clone())),
            })
            .collect::<Vec<_>>();
        let n = items.len().min(14);
        let header = vec![
            bold(&fg(t, "accent", "Models")),
            fg(
                t,
                "muted",
                "Changes apply to the running session: the next frame, review or sub-agent uses them. Claude Code mode rows apply when `ultron claude` next starts.",
            ),
            String::new(),
        ];
        (List::new(Rc::clone(t), self.keys.clone(), items, n, false), header)
    }

    fn picker_items(&self, row: &ModelRow) -> Vec<SelectItem> {
        let mark = |(v, l, d, _): &ModelChoice| SelectItem {
            value: v.clone(),
            label: format!("{}{l}", if *v == row.current { "✓ " } else { "  " }),
            description: d.clone(),
        };
        let mut items: Vec<SelectItem> = row.choices.iter().filter(|c| c.3.is_none()).map(mark).collect();
        let mut groups: BTreeMap<String, Vec<&ModelChoice>> = BTreeMap::new();
        for c in row.choices.iter().filter(|c| c.3.is_some()) {
            groups.entry(c.3.clone().unwrap_or_default()).or_default().push(c);
        }
        for (group, mut list) in groups {
            items.push(SelectItem {
                value: format!("{GROUP_PREFIX}{group}"),
                label: fg(&self.theme, "muted", &format!("── {group} ──")),
                description: None,
            });
            list.sort_by(|a, b| a.1.cmp(&b.1));
            items.extend(list.into_iter().map(mark));
        }
        items
    }

    fn open(&mut self, kind: SubKind) {
        let t = Rc::clone(&self.theme);
        let keys = self.keys.clone();
        let sub = match &kind {
            SubKind::Warnings => {
                let items = vec![item(
                    "anthropic-extra-usage",
                    "Anthropic extra usage",
                    "Warn when Anthropic subscription auth may use paid extra usage",
                    str_bool(self.warnings_extra),
                    &["true", "false"],
                )];
                Sub::List {
                    kind,
                    list: List::new(t, keys, items, 1, false),
                    header: Vec::new(),
                }
            }
            SubKind::Theme => self.theme_single(),
            SubKind::LightTheme | SubKind::DarkTheme => {
                let (title, description, current) = if matches!(kind, SubKind::LightTheme) {
                    (
                        "Light Theme",
                        "Select the theme to use for light terminal appearance",
                        self.light.clone(),
                    )
                } else {
                    (
                        "Dark Theme",
                        "Select the theme to use for dark terminal appearance",
                        self.dark.clone(),
                    )
                };
                Sub::Select {
                    menu: SelectSubmenu::new(
                        t,
                        keys,
                        (title, description),
                        theme_items(&self.input.themes, &current),
                        &current,
                        false,
                        (12, 32),
                    ),
                    kind,
                }
            }
            SubKind::ModelThinking => self.thinking_step(),
            SubKind::Models => {
                let (list, header) = self.models_list();
                Sub::List { kind, list, header }
            }
            SubKind::ModelPick(id) => {
                let Some(row) = self.model_rows().into_iter().find(|r| r.id == *id) else {
                    return;
                };
                Sub::Select {
                    menu: SelectSubmenu::new(
                        t,
                        keys,
                        (&row.label, &row.description),
                        self.picker_items(&row),
                        &row.current,
                        true,
                        (12, 46),
                    ),
                    kind,
                }
            }
        };
        self.stack.push(sub);
    }

    fn theme_single(&self) -> Sub {
        let setting = self.input.str("theme", "dark");
        let fixed = (!setting.contains('/')).then_some(setting.as_str());
        let current = preferred(&self.input.themes, fixed, "dark");
        let mut items = vec![SelectItem {
            value: AUTOMATIC_THEME.into(),
            label: "  Automatic".into(),
            description: Some("Use separate themes for light and dark terminal appearance".into()),
        }];
        items.extend(theme_items(&self.input.themes, &current));
        Sub::Select {
            kind: SubKind::Theme,
            menu: SelectSubmenu::new(
                Rc::clone(&self.theme),
                self.keys.clone(),
                (
                    "Theme",
                    "Select a theme, or choose Automatic to follow terminal appearance.",
                ),
                items,
                &current,
                false,
                (12, 32),
            ),
        }
    }

    fn theme_automatic(&self) -> Sub {
        let t = &self.theme;
        let items = vec![
            Item {
                sub: Some(SubKind::LightTheme),
                ..item(
                    "light-theme",
                    "Light theme",
                    "Theme to use in automatic mode when the terminal is light",
                    self.light.clone(),
                    &[],
                )
            },
            Item {
                sub: Some(SubKind::DarkTheme),
                ..item(
                    "dark-theme",
                    "Dark theme",
                    "Theme to use in automatic mode when the terminal is dark",
                    self.dark.clone(),
                    &[],
                )
            },
            item(
                "apply",
                "Apply",
                "Save and go back",
                "save and go back".into(),
                &["save and go back"],
            ),
            item(
                "single-mode",
                "Change mode",
                "Switch to one theme for light and dark",
                "switch to single theme".into(),
                &["switch to single theme"],
            ),
        ];
        Sub::List {
            kind: SubKind::Theme,
            list: List::new(Rc::clone(t), self.keys.clone(), items, 4, false),
            header: vec![
                bold(&fg(t, "accent", "Automatic Theme")),
                String::new(),
                fg(t, "muted", "Choose themes for terminal light and dark appearance."),
                fg(t, "muted", "Light/dark detection requires terminal support."),
                String::new(),
            ],
        }
    }

    fn thinking_step(&self) -> Sub {
        let t = Rc::clone(&self.theme);
        let keys = self.keys.clone();
        let current_key = self.input.session.as_ref().map(model_ref);
        let default_key = match (self.input.text("defaultProvider"), self.input.text("defaultModel")) {
            (Some(p), Some(m)) => Some(format!("{p}/{m}")),
            _ => None,
        };
        match &self.thinking_model {
            None => {
                let mut models: Vec<&ModelInfo> = self.input.models.iter().collect();
                let rank = |m: &ModelInfo| {
                    let key = model_ref(m);
                    if Some(&key) == current_key.as_ref() {
                        0
                    } else if Some(&key) == default_key.as_ref() {
                        1
                    } else {
                        2
                    }
                };
                models.sort_by(|a, b| rank(a).cmp(&rank(b)).then(a.provider.cmp(&b.provider)));
                let mut items: Vec<SelectItem> = models
                    .iter()
                    .map(|m| SelectItem {
                        value: model_ref(m),
                        label: format!("{} {}", m.id, fg(&t, "muted", &format!("[{}]", m.provider))),
                        description: self.model_thinking.get(&model_ref(m)).cloned(),
                    })
                    .collect();
                if items.is_empty() {
                    items.push(SelectItem {
                        value: "__none__".into(),
                        label: "No models available".into(),
                        description: Some("Log in to a provider or configure an API key first".into()),
                    });
                }
                let preselect = current_key.or(default_key).unwrap_or_default();
                Sub::Select {
                    kind: SubKind::ModelThinking,
                    menu: SelectSubmenu::new(
                        t,
                        keys,
                        ("Per-Model Thinking Level", "Step 1/2 · Select a model to configure"),
                        items,
                        &preselect,
                        true,
                        (12, 46),
                    ),
                }
            }
            Some(key) => {
                let model = self.input.models.iter().find(|m| model_ref(m) == *key);
                let levels: Vec<String> = match model {
                    Some(m) if m.reasoning => m.levels(),
                    _ => vec!["off".into()],
                };
                let active = self.model_thinking.get(key).cloned();
                let mut items: Vec<SelectItem> = levels
                    .iter()
                    .map(|l| SelectItem {
                        value: l.clone(),
                        label: format!("{}{l}", if Some(l) == active.as_ref() { "✓ " } else { "  " }),
                        description: Some(thinking_description(l).into()),
                    })
                    .collect();
                if active.is_some() {
                    items.push(SelectItem {
                        value: CLEAR_OVERRIDE.into(),
                        label: "  (clear override)".into(),
                        description: Some(format!(
                            "Revert to global default ({})",
                            self.input.str("defaultThinkingLevel", "medium")
                        )),
                    });
                }
                let title = match model {
                    Some(m) => format!("Thinking Level for {} [{}]", m.id, m.provider),
                    None => format!("Thinking Level for {key}"),
                };
                Sub::Select {
                    kind: SubKind::ModelThinking,
                    menu: SelectSubmenu::new(
                        t,
                        keys,
                        (&title, "Step 2/2 · Select default thinking level for this model"),
                        items,
                        active.as_deref().unwrap_or(""),
                        false,
                        (12, 32),
                    ),
                }
            }
        }
    }

    /// `handleMouse` on the selector (`event` in its rows: the border, then the list) while no submenu is open:
    /// what the click answered, as [`Self::handle_input`] would.
    pub fn handle_mouse(&mut self, event: &MouseEvent) -> Option<(MouseResult, Option<SettingsEvent>)> {
        if !self.stack.is_empty() || event.y < 1 {
            return None;
        }
        let local = MouseEvent {
            y: event.y - 1,
            ..*event
        };
        let (result, action) = self.list.handle_mouse(&local)?;
        let event = match action {
            None => None,
            Some(ListAction::Cancel) => Some(SettingsEvent::Cancelled),
            Some(ListAction::Open(index)) => {
                if let Some(kind) = self.list.items[index].sub.clone() {
                    self.open(kind);
                }
                None
            }
            Some(ListAction::Changed(id, value)) => Some(self.main_change(&id, &value)),
        };
        Some((result, event))
    }

    pub fn handle_input(&mut self, data: &str) -> Option<SettingsEvent> {
        let Some(sub) = self.stack.pop() else {
            return match self.list.handle_input(data)? {
                ListAction::Cancel => Some(SettingsEvent::Cancelled),
                ListAction::Open(index) => {
                    if let Some(kind) = self.list.items[index].sub.clone() {
                        self.open(kind);
                    }
                    None
                }
                ListAction::Changed(id, value) => Some(self.main_change(&id, &value)),
            };
        };
        match sub {
            Sub::List { kind, mut list, header } => match (list.handle_input(data), &kind) {
                (Some(ListAction::Cancel), SubKind::Models) => {
                    let summary = self.models_summary();
                    self.list.update("models", &summary);
                    None
                }
                // Leaving the theme submenu restores the theme the setting names (`cancel`).
                (Some(ListAction::Cancel), SubKind::Theme) => {
                    Some(SettingsEvent::Preview(self.input.str("theme", "dark")))
                }
                (Some(ListAction::Cancel), _) => None,
                (Some(ListAction::Open(index)), _) => {
                    let next = list.items[index].sub.clone();
                    self.stack.push(Sub::List { kind, list, header });
                    if let Some(next) = next {
                        self.open(next);
                    }
                    None
                }
                (Some(ListAction::Changed(id, value)), SubKind::Warnings) => {
                    self.warnings_extra = value == "true";
                    let _ = id;
                    self.stack.push(Sub::List { kind, list, header });
                    Some(SettingsEvent::Change(
                        "warnings".into(),
                        json!({"anthropicExtraUsage": self.warnings_extra}),
                    ))
                }
                (Some(ListAction::Changed(id, _)), SubKind::Theme) => {
                    if id == "apply" {
                        return Some(self.set_theme(format!("{}/{}", self.light, self.dark)));
                    }
                    self.stack.push(self.theme_single());
                    Some(SettingsEvent::Preview(self.active_automatic_theme()))
                }
                (_, _) => {
                    self.stack.push(Sub::List { kind, list, header });
                    None
                }
            },
            Sub::Select { kind, mut menu } => match menu.handle_input(data) {
                None => {
                    self.stack.push(Sub::Select { kind, menu });
                    None
                }
                Some(SelectOutcome::Highlight(value)) => {
                    let preview = match kind {
                        SubKind::Theme if value == AUTOMATIC_THEME => Some(self.automatic_setting()),
                        SubKind::Theme | SubKind::LightTheme | SubKind::DarkTheme => Some(value),
                        _ => None,
                    };
                    self.stack.push(Sub::Select { kind, menu });
                    preview.map(SettingsEvent::Preview)
                }
                Some(SelectOutcome::Back) => {
                    match kind {
                        SubKind::Theme => return Some(SettingsEvent::Preview(self.input.str("theme", "dark"))),
                        SubKind::LightTheme | SubKind::DarkTheme => {
                            return Some(SettingsEvent::Preview(self.automatic_setting()));
                        }
                        _ => {}
                    }
                    if matches!(kind, SubKind::ModelThinking) && self.thinking_model.take().is_some() {
                        self.stack.push(self.thinking_step());
                    }
                    if matches!(kind, SubKind::ModelThinking) {
                        self.list.update("model-thinking", &self.thinking_summary());
                    }
                    None
                }
                Some(SelectOutcome::Picked(value)) => self.picked(kind, value),
            },
        }
    }

    fn picked(&mut self, kind: SubKind, value: String) -> Option<SettingsEvent> {
        match kind {
            SubKind::Theme => {
                if value == AUTOMATIC_THEME {
                    self.stack.push(self.theme_automatic());
                    return Some(SettingsEvent::Preview(self.automatic_setting()));
                }
                Some(self.set_theme(value))
            }
            SubKind::LightTheme | SubKind::DarkTheme => {
                if matches!(kind, SubKind::LightTheme) {
                    self.light = value.clone();
                } else {
                    self.dark = value.clone();
                }
                if let Some(Sub::List { list, .. }) = self.stack.last_mut() {
                    list.update(
                        if matches!(kind, SubKind::LightTheme) {
                            "light-theme"
                        } else {
                            "dark-theme"
                        },
                        &value,
                    );
                }
                Some(SettingsEvent::Preview(self.automatic_setting()))
            }
            SubKind::ModelThinking => match self.thinking_model.take() {
                None => {
                    if value != "__none__" {
                        self.thinking_model = Some(value);
                    }
                    self.stack.push(self.thinking_step());
                    None
                }
                Some(key) => {
                    let (provider, id) = key.split_once('/').unwrap_or(("", key.as_str()));
                    let level = if value == CLEAR_OVERRIDE {
                        self.model_thinking.remove(&key);
                        Value::Null
                    } else {
                        self.model_thinking.insert(key.clone(), value.clone());
                        Value::String(value)
                    };
                    let event = SettingsEvent::Change(
                        "modelThinkingLevel".into(),
                        json!({"provider": provider, "modelId": id, "level": level}),
                    );
                    // Loops back to the model step.
                    self.stack.push(self.thinking_step());
                    Some(event)
                }
            },
            SubKind::ModelPick(id) => {
                if value.starts_with(GROUP_PREFIX) {
                    if let Some(row) = self.model_rows().into_iter().find(|r| r.id == id) {
                        self.stack.push(Sub::Select {
                            menu: SelectSubmenu::new(
                                Rc::clone(&self.theme),
                                self.keys.clone(),
                                (&row.label, &row.description),
                                self.picker_items(&row),
                                &row.current,
                                true,
                                (12, 46),
                            ),
                            kind: SubKind::ModelPick(id),
                        });
                    }
                    return None;
                }
                let row = self.model_rows().into_iter().find(|r| r.id == id)?;
                let event = if id == "session" {
                    if let Some(m) = self.input.models.iter().find(|m| model_ref(m) == value) {
                        self.input.session = Some(m.clone());
                    }
                    SettingsEvent::SessionModel(value.clone())
                } else {
                    let stored = if value.is_empty() {
                        Value::Null
                    } else if id == "claudeCode.model" || id == "claudeCode.childModel" {
                        Value::String(value.trim_start_matches("claude-code/").to_string())
                    } else {
                        Value::String(value.clone())
                    };
                    set_path(&mut self.input.settings, &id, stored);
                    SettingsEvent::ModelSetting {
                        key: id.clone(),
                        label: row.label.clone(),
                        value: value.clone(),
                    }
                };
                // Back on the Models list, every row re-derived.
                let rows = self.model_rows();
                if let Some(Sub::List { list, .. }) = self.stack.last_mut() {
                    for row in rows {
                        if !row.heading {
                            let shown = match &row.locked {
                                None => row.display.clone(),
                                Some(_) => format!("{} · locked", row.display),
                            };
                            list.update(&row.id, &shown);
                        }
                    }
                }
                Some(event)
            }
            SubKind::Warnings | SubKind::Models => None,
        }
    }

    /// `getAutomaticThemeSetting`.
    fn automatic_setting(&self) -> String {
        format!("{}/{}", self.light, self.dark)
    }

    /// `getActiveAutomaticTheme`: the side of the pair the terminal's appearance selects.
    fn active_automatic_theme(&self) -> String {
        if self.input.terminal_theme == "light" {
            self.light.clone()
        } else {
            self.dark.clone()
        }
    }

    fn set_theme(&mut self, theme: String) -> SettingsEvent {
        self.stack.clear();
        self.list.update("theme", &theme);
        set_path(&mut self.input.settings, "theme", Value::String(theme.clone()));
        SettingsEvent::Change("theme".into(), Value::String(theme))
    }

    /// The setting a main-list change writes.
    fn main_change(&mut self, id: &str, value: &str) -> SettingsEvent {
        let b = Value::Bool(value == "true");
        let n = |v: &str| Value::from(v.parse::<i64>().unwrap_or(0));
        let (key, v) = match id {
            "autocompact" => ("compaction.enabled", b),
            "show-images" => ("terminal.showImages", b),
            "image-width-cells" => ("terminal.imageWidthCells", n(value)),
            "auto-resize-images" => ("images.autoResize", b),
            "block-images" => ("images.blockImages", b),
            "skill-commands" => ("enableSkillCommands", b),
            "steering-mode" => ("steeringMode", value.into()),
            "follow-up-mode" => ("followUpMode", value.into()),
            "transport" => ("transport", value.into()),
            "http-idle-timeout" => (
                "httpIdleTimeoutMs",
                Value::from(HTTP_IDLE.iter().find(|(l, _)| *l == value).map_or(300_000, |(_, v)| *v)),
            ),
            "cache-warming-mode" => ("cacheWarming", value.into()),
            "hide-thinking" => ("hideThinkingBlock", b),
            "mermaid-rendering" => ("markdown.mermaid", value.into()),
            "cache-miss-notices" => ("showCacheMissNotices", b),
            "collapse-changelog" => ("collapseChangelog", b),
            "quiet-startup" => ("quietStartup", b),
            "rlm-pane-auto-open" => ("rlmPaneAutoOpen", b),
            "autoreview-enabled" => ("autoreview.enabled", b),
            "autoreview-ack" => ("autoreview.ack", b),
            "autoreview-ack-art" => ("autoreview.ackArt", value.into()),
            "install-telemetry" => ("enableInstallTelemetry", b),
            "default-project-trust" => (
                "defaultProjectTrust",
                match value {
                    "Always trust" => "always",
                    "Never trust" => "never",
                    _ => "ask",
                }
                .into(),
            ),
            "double-escape-action" => ("doubleEscapeAction", value.into()),
            "tree-filter-mode" => ("treeFilterMode", value.into()),
            "show-hardware-cursor" => ("showHardwareCursor", b),
            "editor-padding" => ("editorPaddingX", n(value)),
            "output-padding" => ("outputPad", n(value)),
            "autocomplete-max-visible" => ("autocompleteMaxVisible", n(value)),
            "clear-on-shrink" => ("terminal.clearOnShrink", b),
            "terminal-progress" => ("terminal.showTerminalProgress", b),
            "tui-mode" => ("tuiMode", value.into()),
            "fullscreen-exit-output" => ("fullscreenExitOutput", value.into()),
            "fullscreen-scrollbar" => ("fullscreenScrollbar", value.into()),
            "fullscreen-copy-on-select" => ("fullscreenCopyOnSelect", b),
            other => (other, value.into()),
        };
        set_path(&mut self.input.settings, key, v.clone());
        SettingsEvent::Change(key.to_string(), v)
    }
}

fn set_path(settings: &mut Value, key: &str, value: Value) {
    let mut v = settings;
    let parts: Vec<&str> = key.split('.').collect();
    for part in &parts[..parts.len() - 1] {
        if !v[*part].is_object() {
            v[*part] = json!({});
        }
        v = &mut v[*part];
    }
    v[parts[parts.len() - 1]] = value;
}

fn theme_items(themes: &[String], current: &str) -> Vec<SelectItem> {
    themes
        .iter()
        .map(|name| SelectItem {
            value: name.clone(),
            label: format!("{}{name}", if name == current { "✓ " } else { "  " }),
            description: None,
        })
        .collect()
}

fn preferred(themes: &[String], preferred: Option<&str>, fallback: &str) -> String {
    match preferred {
        Some(p) if themes.iter().any(|t| t == p) => p.to_string(),
        _ if themes.iter().any(|t| t == fallback) => fallback.to_string(),
        _ => themes.first().cloned().unwrap_or_else(|| fallback.to_string()),
    }
}

/// `defaultAutomaticThemes`: `light/dark` from an automatic setting, else the fixed theme for both.
fn automatic_themes(setting: &str, themes: &[String]) -> (String, String) {
    if let Some((light, dark)) = setting.split_once('/')
        && !light.is_empty()
        && !dark.is_empty()
    {
        return (light.to_string(), dark.to_string());
    }
    let fixed = (!setting.contains('/')).then_some(setting);
    let name = preferred(themes, fixed, "dark");
    (name.clone(), name)
}

impl Component for SettingsSelector {
    fn render(&mut self, width: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let border = fg(&t, "border", &"─".repeat(width.max(1)));
        let mut lines = vec![border.clone()];
        // A submenu list keeps its header above what it opened (its `Container` holds both).
        let depth = self.stack.len();
        for sub in self.stack.iter().take(depth.saturating_sub(1)) {
            if let Sub::List { header, .. } = sub {
                for h in header {
                    if h.is_empty() {
                        lines.push(String::new());
                    } else {
                        lines.extend(wrap_text_with_ansi(h, width));
                    }
                }
            }
        }
        match self.stack.last_mut() {
            None => lines.extend(self.list.render(width)),
            Some(Sub::Select { menu, .. }) => lines.extend(menu.render(width)),
            Some(Sub::List { list, header, .. }) => {
                for h in header.iter() {
                    if h.is_empty() {
                        lines.push(String::new());
                    } else {
                        lines.extend(wrap_text_with_ansi(h, width));
                    }
                }
                lines.extend(list.render(width));
            }
        }
        lines.push(border);
        lines
    }
}

impl SettingsSelector {
    /// The input value of the search line (tests).
    pub fn query(&self) -> String {
        self.list
            .search
            .as_ref()
            .map(|s| s.value().to_string())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_http_idle_labels_like_ts() {
        assert_eq!(format_http_idle(300_000), "5 min");
        assert_eq!(format_http_idle(0), "disabled");
        assert_eq!(format_http_idle(45_000), "45 sec");
        assert_eq!(format_http_idle(1_500), "1.5 sec");
    }

    #[test]
    fn settings_automatic_themes_split_or_repeat() {
        let themes = vec!["dark".to_string(), "light".to_string()];
        assert_eq!(automatic_themes("light/dark", &themes), ("light".into(), "dark".into()));
        assert_eq!(automatic_themes("light", &themes), ("light".into(), "light".into()));
        assert_eq!(automatic_themes("nope", &themes), ("dark".into(), "dark".into()));
    }

    #[test]
    fn settings_set_path_creates_objects() {
        let mut v = json!({"a": 1});
        set_path(&mut v, "terminal.showImages", json!(false));
        set_path(&mut v, "theme", json!("light"));
        assert_eq!(v, json!({"a": 1, "terminal": {"showImages": false}, "theme": "light"}));
    }
}
