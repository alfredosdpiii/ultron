//! `ultron config`: the resource selector (cli/config-selector.ts, modes/interactive/components/config-selector.ts).
//! Every local resource (session-core `local_resources`) is listed by group and type; Space toggles it in global settings,
//! or cycles a project override (inherit, load, unload) in project mode; Tab switches mode when the project is
//! trusted; typing filters; Esc closes.

use crate::Env;
use config::settings::SettingsManager;
use interactive::{bold, key_hint, key_text};
use resources::paths::{basename, canonicalize, dirname, join, relative};
use serde_json::{Map, Value};
use session_core::local_resources::{LocalResources, PathMetadata, RESOURCE_TYPES, ResolvedPaths, ResourceType, Scope};
use std::collections::HashMap;
use std::io::Write;
use std::rc::Rc;
use tui::components::Component;
use tui::input::Input;
use tui::keybindings::{KeybindingsManager, Platform};
use tui::theme::{ColorMode, Theme, builtin_theme_json, create_theme};
use tui::utils::{truncate_to_width, visible_width};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WriteScope {
    Global,
    Project,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OverrideState {
    Inherit,
    Load,
    Unload,
}

#[derive(Clone, Debug)]
struct Item {
    path: String,
    enabled: bool,
    metadata: PathMetadata,
    kind: ResourceType,
    display_name: String,
}

#[derive(Clone, Debug)]
struct Subgroup {
    kind: ResourceType,
    items: Vec<Item>,
}

#[derive(Clone, Debug)]
struct Group {
    label: String,
    scope: Scope,
    source: String,
    subgroups: Vec<Subgroup>,
}

/// One row of the list: group header, type header, or item (group, subgroup, item indices).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Row {
    Group(usize),
    Subgroup(usize, usize),
    Item(usize, usize, usize),
}

fn type_label(kind: ResourceType) -> &'static str {
    match kind {
        ResourceType::Skills => "Skills",
        ResourceType::Prompts => "Prompts",
        ResourceType::Themes => "Themes",
    }
}

/// `formatBaseDir`.
fn format_base_dir(base: &str, home: &str) -> String {
    let display = if base == home {
        "~".to_string()
    } else if let Some(rest) = base.strip_prefix(home) {
        format!("~{}", rest.replace('\\', "/"))
    } else {
        base.replace('\\', "/")
    };
    if display.ends_with('/') {
        display
    } else {
        format!("{display}/")
    }
}

/// `getGroupLabel`.
fn group_label(m: &PathMetadata, agent_dir: &str, home: &str) -> String {
    if m.source == "auto" {
        return match (&m.base_dir, m.scope) {
            (Some(base), Scope::User) => format!("User ({})", format_base_dir(base, home)),
            (Some(base), Scope::Project) => format!("Project ({})", format_base_dir(base, home)),
            (None, Scope::User) => format!("User ({})", format_base_dir(agent_dir, home)),
            (None, Scope::Project) => format!("Project ({}/)", config::paths::CONFIG_DIR_NAME),
        };
    }
    if m.scope == Scope::User {
        "User settings"
    } else {
        "Project settings"
    }
    .to_string()
}

/// `String.prototype.localeCompare` for the names shown here: case-insensitive first, lower case first on ties.
fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let key = |s: &str| -> Vec<(char, u8)> {
        s.chars()
            .map(|c| (c.to_lowercase().next().unwrap_or(c), u8::from(c.is_uppercase())))
            .collect()
    };
    let (ka, kb) = (key(a), key(b));
    let primary: Vec<char> = ka.iter().map(|k| k.0).collect();
    let other: Vec<char> = kb.iter().map(|k| k.0).collect();
    primary.cmp(&other).then_with(|| ka.cmp(&kb))
}

/// `buildGroups`.
fn build_groups(resolved: &ResolvedPaths, agent_dir: &str, home: &str) -> Vec<Group> {
    let mut groups: Vec<(String, Group)> = Vec::new();
    for kind in RESOURCE_TYPES {
        for res in resolved.of(kind) {
            let m = &res.metadata;
            let key = format!(
                "{}:{}:{}",
                m.scope.name(),
                m.source,
                m.base_dir.as_deref().unwrap_or("")
            );
            let index = match groups.iter().position(|(k, _)| *k == key) {
                Some(i) => i,
                None => {
                    groups.push((
                        key,
                        Group {
                            label: group_label(m, agent_dir, home),
                            scope: m.scope,
                            source: m.source.clone(),
                            subgroups: Vec::new(),
                        },
                    ));
                    groups.len() - 1
                }
            };
            let group = &mut groups[index].1;
            let sub = match group.subgroups.iter().position(|s| s.kind == kind) {
                Some(i) => i,
                None => {
                    group.subgroups.push(Subgroup {
                        kind,
                        items: Vec::new(),
                    });
                    group.subgroups.len() - 1
                }
            };
            let file = basename(&res.path).to_string();
            let parent_dir = dirname(&res.path);
            let parent = basename(&parent_dir).to_string();
            let display_name = if kind == ResourceType::Skills && file == "SKILL.md" {
                parent
            } else {
                file
            };
            group.subgroups[sub].items.push(Item {
                path: res.path.clone(),
                enabled: res.enabled,
                metadata: m.clone(),
                kind,
                display_name,
            });
        }
    }
    let mut groups: Vec<Group> = groups.into_iter().map(|(_, g)| g).collect();
    groups.sort_by(|a, b| {
        if a.scope != b.scope {
            return if a.scope == Scope::User {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }
        locale_compare(&a.source, &b.source)
    });
    for group in &mut groups {
        group.subgroups.sort_by_key(|s| s.kind);
        for sub in &mut group.subgroups {
            sub.items
                .sort_by(|a, b| locale_compare(&a.display_name, &b.display_name));
        }
    }
    groups
}

fn target(entry: &str) -> &str {
    if entry.starts_with('!') || entry.starts_with('+') || entry.starts_with('-') {
        &entry[1..]
    } else {
        entry
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn to_value(list: Vec<String>) -> Value {
    Value::Array(list.into_iter().map(Value::String).collect())
}

/// What a key did to the selector.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Close,
    Exit,
}

/// `ConfigSelectorComponent` with its `ResourceList`.
pub struct ConfigSelector {
    groups: [Vec<Group>; 2],
    rows: Vec<Row>,
    filtered: Vec<Row>,
    selected: usize,
    search: Input,
    max_visible: usize,
    pub settings: SettingsManager,
    cwd: String,
    agent_dir: String,
    write_scope: WriteScope,
    project_mode: bool,
    inherited: HashMap<String, bool>,
    theme: Rc<Theme>,
    keys: KeybindingsManager,
}

pub fn keybindings() -> KeybindingsManager {
    KeybindingsManager::new(
        tui::qol_keys::native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

impl ConfigSelector {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        global: &ResolvedPaths,
        project: &ResolvedPaths,
        settings: SettingsManager,
        cwd: &str,
        agent_dir: &str,
        home: &str,
        terminal_height: usize,
        write_scope: WriteScope,
        project_mode: bool,
        theme: Rc<Theme>,
    ) -> Self {
        let groups = [
            build_groups(global, agent_dir, home),
            build_groups(project, agent_dir, home),
        ];
        let mut inherited = HashMap::new();
        for g in &groups[0] {
            for s in &g.subgroups {
                for i in &s.items {
                    inherited.insert(Self::item_key(i), i.enabled);
                }
            }
        }
        let mut search = Input::new(keybindings());
        search.focused = true;
        let mut selector = Self {
            groups,
            rows: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            search,
            max_visible: terminal_height.saturating_sub(8).max(5),
            settings,
            cwd: cwd.to_string(),
            agent_dir: agent_dir.to_string(),
            write_scope,
            project_mode,
            inherited,
            theme,
            keys: keybindings(),
        };
        selector.build_rows();
        selector.filtered = selector.rows.clone();
        selector
    }

    fn fg(&self, color: &str, text: &str) -> String {
        self.theme.fg(color, text).unwrap_or_else(|_| text.to_string())
    }

    fn index(&self) -> usize {
        usize::from(self.write_scope == WriteScope::Project)
    }

    fn item(&self, row: Row) -> Option<&Item> {
        match row {
            Row::Item(g, s, i) => self.groups[self.index()].get(g)?.subgroups.get(s)?.items.get(i),
            _ => None,
        }
    }

    fn build_rows(&mut self) {
        let mut rows = Vec::new();
        for (g, group) in self.groups[self.index()].iter().enumerate() {
            rows.push(Row::Group(g));
            for (s, sub) in group.subgroups.iter().enumerate() {
                rows.push(Row::Subgroup(g, s));
                for i in 0..sub.items.len() {
                    rows.push(Row::Item(g, s, i));
                }
            }
        }
        self.rows = rows;
        self.selected = self.rows.iter().position(|r| matches!(r, Row::Item(..))).unwrap_or(0);
    }

    fn select_first(&mut self) {
        self.selected = self
            .filtered
            .iter()
            .position(|r| matches!(r, Row::Item(..)))
            .unwrap_or(0);
    }

    /// `filterItems`.
    fn filter(&mut self) {
        let query = self.search.value().to_string();
        if query.trim().is_empty() {
            self.filtered = self.rows.clone();
            self.select_first();
            return;
        }
        let lower = query.to_lowercase();
        let hits: Vec<Row> = self
            .rows
            .iter()
            .copied()
            .filter(|r| {
                self.item(*r).is_some_and(|i| {
                    i.display_name.to_lowercase().contains(&lower)
                        || i.kind.key().contains(&lower)
                        || i.path.to_lowercase().contains(&lower)
                })
            })
            .collect();
        self.filtered = self
            .rows
            .iter()
            .copied()
            .filter(|r| match r {
                Row::Group(g) => hits.iter().any(|h| matches!(h, Row::Item(hg, _, _) if hg == g)),
                Row::Subgroup(g, s) => hits
                    .iter()
                    .any(|h| matches!(h, Row::Item(hg, hs, _) if hg == g && hs == s)),
                Row::Item(..) => hits.contains(r),
            })
            .collect();
        self.select_first();
    }

    fn next_item(&self, from: usize, down: bool) -> usize {
        let mut i = from as i64;
        loop {
            i += if down { 1 } else { -1 };
            if i < 0 || i as usize >= self.filtered.len() {
                return from;
            }
            if matches!(self.filtered[i as usize], Row::Item(..)) {
                return i as usize;
            }
        }
    }

    /// `ResourceList.handleInput`.
    pub fn handle_input(&mut self, data: &str) -> Action {
        let kb = &self.keys;
        if kb.matches(data, "tui.select.up") {
            self.selected = self.next_item(self.selected, false);
            return Action::None;
        }
        if kb.matches(data, "tui.select.down") {
            self.selected = self.next_item(self.selected, true);
            return Action::None;
        }
        if kb.matches(data, "tui.select.pageUp") {
            let mut t = self.selected.saturating_sub(self.max_visible);
            while t < self.filtered.len() && !matches!(self.filtered[t], Row::Item(..)) {
                t += 1;
            }
            if t < self.filtered.len() {
                self.selected = t;
            }
            return Action::None;
        }
        if kb.matches(data, "tui.select.pageDown") {
            let mut t = (self.selected + self.max_visible).min(self.filtered.len().saturating_sub(1)) as i64;
            while t >= 0 && !matches!(self.filtered[t as usize], Row::Item(..)) {
                t -= 1;
            }
            if t >= 0 {
                self.selected = t as usize;
            }
            return Action::None;
        }
        if kb.matches(data, "tui.select.cancel") {
            return Action::Close;
        }
        if data == "\x03" {
            return Action::Exit;
        }
        if kb.matches(data, "tui.input.tab") {
            if self.project_mode {
                self.write_scope = match self.write_scope {
                    WriteScope::Global => WriteScope::Project,
                    WriteScope::Project => WriteScope::Global,
                };
                self.build_rows();
                self.filter();
            }
            return Action::None;
        }
        if data == " " || kb.matches(data, "tui.select.confirm") {
            if let Some(row) = self.filtered.get(self.selected).copied()
                && let Some(item) = self.item(row).cloned()
                && (self.write_scope == WriteScope::Project || item_scope(&item) == Scope::User)
                && let Some(enabled) = self.toggle(&item)
                && let Row::Item(g, s, i) = row
            {
                let index = self.index();
                self.groups[index][g].subgroups[s].items[i].enabled = enabled;
            }
            return Action::None;
        }
        self.search.handle_input(data);
        self.filter();
        Action::None
    }

    fn item_key(item: &Item) -> String {
        format!("{}:{}", item.kind.key(), canonicalize(&item.path))
    }

    fn base_dir(&self, scope: Scope) -> String {
        match scope {
            Scope::Project => join(&self.cwd, config::paths::CONFIG_DIR_NAME),
            Scope::User => self.agent_dir.clone(),
        }
    }

    fn scope_settings(&self, scope: Scope) -> &Map<String, Value> {
        match scope {
            Scope::Project => self.settings.project_settings(),
            Scope::User => self.settings.global_settings(),
        }
    }

    fn write(&mut self, scope: Scope, key: &str, value: Value) {
        match scope {
            Scope::Project => {
                let _ = self.settings.set_project(key, Some(value));
            }
            Scope::User => self.settings.set_global(key, Some(value)),
        }
    }

    fn toggle(&mut self, item: &Item) -> Option<bool> {
        if self.write_scope == WriteScope::Project {
            let state = self.next_state(item);
            if !self.set_project_override(item, state) {
                return None;
            }
            return Some(match state {
                OverrideState::Inherit => self.inherited_enabled(item),
                OverrideState::Load => true,
                OverrideState::Unload => false,
            });
        }
        let enabled = !item.enabled;
        let scope = item.metadata.scope;
        let pattern = relative(
            item.metadata.base_dir.as_deref().unwrap_or(&self.base_dir(scope)),
            &item.path,
        );
        let mut updated: Vec<String> = strings(self.scope_settings(scope).get(item.kind.key()))
            .into_iter()
            .filter(|p| target(p) != pattern)
            .collect();
        updated.push(format!("{}{pattern}", if enabled { "+" } else { "-" }));
        self.write(scope, item.kind.key(), to_value(updated));
        Some(enabled)
    }

    fn pattern_for_scope(&self, item: &Item, scope: Scope) -> String {
        let source_scope = item_scope(item);
        if scope != source_scope {
            return item.path.clone();
        }
        let base = item
            .metadata
            .base_dir
            .clone()
            .unwrap_or_else(|| self.base_dir(source_scope));
        relative(&base, &item.path)
    }

    fn top_level_patterns(&self, item: &Item) -> Vec<String> {
        let mut set = vec![
            self.pattern_for_scope(item, Scope::Project),
            item.path.clone(),
            relative(&self.base_dir(Scope::Project), &item.path),
        ];
        if let Some(base) = &item.metadata.base_dir {
            set.push(relative(base, &item.path));
        }
        set
    }

    fn is_inherited_global(&self, item: &Item) -> bool {
        item_scope(item) == Scope::User || self.inherited.contains_key(&Self::item_key(item))
    }

    fn inherited_enabled(&self, item: &Item) -> bool {
        self.inherited
            .get(&Self::item_key(item))
            .copied()
            .unwrap_or(if item_scope(item) == Scope::User {
                item.enabled
            } else {
                true
            })
    }

    fn state_from(entries: &[String], patterns: &[String], empty_is_unload: bool) -> OverrideState {
        if entries.is_empty() && empty_is_unload {
            return OverrideState::Unload;
        }
        let mut state = OverrideState::Inherit;
        for e in entries {
            if !patterns.iter().any(|p| p == target(e)) {
                continue;
            }
            state = if e.starts_with('!') || e.starts_with('-') {
                OverrideState::Unload
            } else {
                OverrideState::Load
            };
        }
        state
    }

    fn override_state(&self, item: &Item) -> OverrideState {
        if self.write_scope != WriteScope::Project {
            return OverrideState::Inherit;
        }
        let entries = strings(self.settings.project_settings().get(item.kind.key()));
        Self::state_from(&entries, &self.top_level_patterns(item), false)
    }

    fn next_state(&self, item: &Item) -> OverrideState {
        let state = self.override_state(item);
        let inherited = self.inherited_enabled(item);
        match (state, inherited) {
            (OverrideState::Inherit, true) => OverrideState::Unload,
            (OverrideState::Inherit, false) => OverrideState::Load,
            (OverrideState::Unload, true) => OverrideState::Load,
            (OverrideState::Unload, false) => OverrideState::Inherit,
            (OverrideState::Load, true) => OverrideState::Inherit,
            (OverrideState::Load, false) => OverrideState::Unload,
        }
    }

    fn set_project_override(&mut self, item: &Item, state: OverrideState) -> bool {
        let sign = |s: OverrideState| if s == OverrideState::Load { "+" } else { "-" };
        let current = strings(self.settings.project_settings().get(item.kind.key()));
        let inherited = self.is_inherited_global(item);
        let pattern = if inherited {
            item.path.clone()
        } else {
            self.pattern_for_scope(item, Scope::Project)
        };
        let patterns = self.top_level_patterns(item);
        let mut updated: Vec<String> = current
            .into_iter()
            .filter(|e| {
                let t = target(e);
                if (e.starts_with('!') || e.starts_with('+') || e.starts_with('-')) && patterns.iter().any(|p| p == t) {
                    return false;
                }
                !(state == OverrideState::Inherit && inherited && t == pattern)
            })
            .collect();
        if state != OverrideState::Inherit {
            if inherited && !updated.contains(&pattern) {
                updated.push(pattern.clone());
            }
            updated.push(format!("{}{pattern}", sign(state)));
        }
        let _ = self.settings.set_project(item.kind.key(), Some(to_value(updated)));
        true
    }

    fn checkbox(&self, item: &Item) -> String {
        if self.write_scope == WriteScope::Project {
            return match self.override_state(item) {
                OverrideState::Load => self.fg("success", "[+]"),
                OverrideState::Unload => self.fg("warning", "[-]"),
                OverrideState::Inherit => self.fg("dim", if item.enabled { "[x]" } else { "[ ]" }),
            };
        }
        if item.enabled {
            self.fg("success", "[x]")
        } else {
            self.fg("dim", "[ ]")
        }
    }

    fn suffix(&self, item: &Item) -> String {
        if self.write_scope != WriteScope::Project {
            return String::new();
        }
        match self.override_state(item) {
            OverrideState::Load => self.fg("muted", "  project load"),
            OverrideState::Unload => self.fg("muted", "  project unload"),
            OverrideState::Inherit if self.is_inherited_global(item) => self.fg("dim", "  inherited global"),
            OverrideState::Inherit => String::new(),
        }
    }

    fn header(&self, width: usize) -> Vec<String> {
        let title = bold(if self.write_scope == WriteScope::Project {
            "Project Local Resources"
        } else {
            "Global Resources"
        });
        let sep = self.fg("muted", " · ");
        let switch = if self.project_mode {
            format!(
                "{}{sep}",
                key_hint(
                    &self.theme,
                    &key_text(&self.keys, "tui.input.tab", false),
                    "switch mode"
                )
            )
        } else {
            String::new()
        };
        let action = if self.write_scope == WriteScope::Project {
            key_hint(&self.theme, "space", "cycle inherit/+/-")
        } else {
            key_hint(&self.theme, "space", "toggle")
        };
        let hint = format!("{switch}{action}{sep}{}", key_hint(&self.theme, "esc", "close"));
        let spacing = width
            .saturating_sub(visible_width(&title) + visible_width(&hint))
            .max(1);
        let scope_hint = if self.write_scope == WriteScope::Project {
            self.fg(
                "muted",
                &format!(
                    "{}/settings.json · inherited global resources are dimmed",
                    config::paths::CONFIG_DIR_NAME
                ),
            )
        } else {
            self.fg(
                "muted",
                &format!("~/{}/agent/settings.json", config::paths::CONFIG_DIR_NAME),
            )
        };
        vec![
            truncate_to_width(&format!("{title}{}{hint}", " ".repeat(spacing)), width, "", false),
            truncate_to_width(&scope_hint, width, "", false),
        ]
    }

    fn list(&mut self, width: usize) -> Vec<String> {
        let mut lines = self.search.render(width);
        lines.push(String::new());
        if self.filtered.is_empty() {
            lines.push(self.fg("muted", "  No resources found"));
            return lines;
        }
        let len = self.filtered.len() as i64;
        let max = self.max_visible as i64;
        let start = (self.selected as i64 - max / 2).min(len - max).max(0) as usize;
        let end = (start + self.max_visible).min(self.filtered.len());
        let groups = &self.groups[self.index()];
        for i in start..end {
            match self.filtered[i] {
                Row::Group(g) => {
                    let group = &groups[g];
                    let inherited = self.write_scope == WriteScope::Project && group.scope == Scope::User;
                    let label = bold(&format!(
                        "{}{}",
                        group.label,
                        if inherited { " · inherited global" } else { "" }
                    ));
                    let line = self.fg(if inherited { "dim" } else { "accent" }, &label);
                    lines.push(truncate_to_width(&format!("  {line}"), width, "", false));
                }
                Row::Subgroup(g, s) => {
                    let color = if self.write_scope == WriteScope::Project && groups[g].scope == Scope::User {
                        "dim"
                    } else {
                        "muted"
                    };
                    let label = self.fg(color, type_label(groups[g].subgroups[s].kind));
                    lines.push(truncate_to_width(&format!("    {label}"), width, "", false));
                }
                row @ Row::Item(..) => {
                    let Some(item) = self.item(row) else { continue };
                    let selected = i == self.selected;
                    let dimmed = self.write_scope == WriteScope::Project
                        && self.is_inherited_global(item)
                        && self.override_state(item) == OverrideState::Inherit;
                    let text = if selected && !dimmed {
                        bold(&item.display_name)
                    } else {
                        item.display_name.clone()
                    };
                    let name = if dimmed { self.fg("dim", &text) } else { text };
                    let cursor = if selected { "> " } else { "  " };
                    lines.push(truncate_to_width(
                        &format!("{cursor}    {} {name}{}", self.checkbox(item), self.suffix(item)),
                        width,
                        "...",
                        false,
                    ));
                }
            }
        }
        if start > 0 || end < self.filtered.len() {
            let count = self.filtered.iter().filter(|r| matches!(r, Row::Item(..))).count();
            let current = self.filtered[..self.selected]
                .iter()
                .filter(|r| matches!(r, Row::Item(..)))
                .count()
                + 1;
            lines.push(self.fg("dim", &format!("  ({current}/{count})")));
        }
        lines
    }

    /// The whole component (`ConfigSelectorComponent.render`).
    pub fn render(&mut self, width: usize) -> Vec<String> {
        let border = self.fg("border", &"─".repeat(width.max(1)));
        let mut lines = vec![String::new(), border.clone(), String::new()];
        lines.extend(self.header(width));
        lines.push(String::new());
        lines.extend(self.list(width));
        lines.push(String::new());
        lines.push(border);
        lines
    }
}

fn item_scope(item: &Item) -> Scope {
    item.metadata.scope
}

fn theme_for(env: &Env, settings: &SettingsManager) -> Result<Rc<Theme>, String> {
    let caps = crate::terminal_settings::capabilities(env, settings);
    let name = settings.theme().unwrap_or_else(|| "dark".into());
    let json = builtin_theme_json(&name)
        .or_else(|| builtin_theme_json("dark"))
        .ok_or("the dark theme is missing")?;
    Ok(Rc::new(create_theme(&json, ColorMode::from_capabilities(&caps), None)?))
}

/// Run the selector on the process terminal until it closes (`selectConfig`).
pub fn select_config(
    env: &Env,
    global: &ResolvedPaths,
    project: &ResolvedPaths,
    settings: SettingsManager,
    write_scope: WriteScope,
    project_mode: bool,
) -> Result<(), String> {
    let theme = theme_for(env, &settings)?;
    let (_, rows) = tui::terminal::size();
    let mut selector = ConfigSelector::new(
        global,
        project,
        settings,
        &resolved(&env.cwd.display().to_string(), env),
        &resolved(&env.agent_dir().display().to_string(), env),
        &env.home.display().to_string(),
        rows.max(1) as usize,
        write_scope,
        project_mode,
        theme,
    );
    let input = tui::terminal::spawn_stdin_pump();
    let mut terminal = tui::terminal::Terminal::new(std::io::stdout());
    terminal.start(true).map_err(|e| e.to_string())?;
    let _ = terminal.hide_cursor();
    let mut renderer = tui::renderer::Renderer::new();
    let mut cursor_row: i64 = 0;
    let mut paint = |selector: &mut ConfigSelector,
                     renderer: &mut tui::renderer::Renderer,
                     terminal: &mut tui::terminal::Terminal<std::io::Stdout>| {
        let (w, h) = tui::terminal::size();
        let (w, h) = (w.max(1) as usize, h.max(1) as usize);
        if let Ok(out) = renderer.render(selector.render(w), w, h) {
            track(&mut cursor_row, &out);
            let _ = terminal.write(&out);
        }
    };
    paint(&mut selector, &mut renderer, &mut terminal);
    loop {
        let data = match input.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(data) => data,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                paint(&mut selector, &mut renderer, &mut terminal);
                continue;
            }
            Err(_) => break,
        };
        if selector.handle_input(&data) != Action::None {
            break;
        }
        paint(&mut selector, &mut renderer, &mut terminal);
    }
    // `TuiMainScreen.stop`: below the last line, cursor shown, raw mode off.
    let len = renderer.previous_lines().len() as i64;
    if len > 0 {
        let mut out = String::from(" ");
        let diff = len - cursor_row;
        if diff > 0 {
            out.push_str(&format!("\x1b[{diff}B"));
        } else if diff < 0 {
            out.push_str(&format!("\x1b[{}A", -diff));
        }
        out.push_str("\r\n");
        let _ = terminal.write(&out);
    }
    let _ = terminal.show_cursor();
    let _ = terminal.stop();
    let _ = std::io::stdout().flush();
    Ok(())
}

fn resolved(path: &str, env: &Env) -> String {
    config::paths::resolve(path, &env.cwd, &env.home).display().to_string()
}

/// Advance `row` over the cursor moves of renderer output.
fn track(row: &mut i64, out: &str) {
    let bytes = out.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            *row += 1;
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"\x1b[2J\x1b[H") {
            *row = 0;
            i += 7;
            continue;
        }
        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'[') {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 2 && j < bytes.len() {
                let n: i64 = out[i + 2..j].parse().unwrap_or(0);
                match bytes[j] {
                    b'A' => *row -= n,
                    b'B' => *row += n,
                    _ => {}
                }
            }
            i = j;
            continue;
        }
        i += 1;
    }
}

/// `handleConfigCommand` after its arguments: resolve global and project resources, then run the selector.
pub fn run(env: &Env, settings: SettingsManager, local: bool) -> Result<(), String> {
    let global_settings = SettingsManager::create(&env.cwd, &env.agent_dir(), false);
    let global = LocalResources::new(env, global_settings).resolve();
    let trusted = settings.is_project_trusted();
    let project = if trusted {
        let mut project_settings = SettingsManager::create(&env.cwd, &env.agent_dir(), false);
        project_settings.set_project_trusted(true);
        LocalResources::new(env, project_settings).resolve()
    } else {
        global.clone()
    };
    select_config(
        env,
        &global,
        &project,
        settings,
        if local { WriteScope::Project } else { WriteScope::Global },
        trusted,
    )
}
