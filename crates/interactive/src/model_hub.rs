//! The model hub overlay (oh-my-pi packages/tui/src/overlays/model-hub.ts, model-browser.ts and model-picker.ts):
//! a scopes sidebar (Recent, All models, one entry per provider), a searchable browser pane, and an inline
//! thinking-level strip. Picking a model is a session-only switch, as in omp's model picker: saved defaults stay
//! unchanged. The hub returns [`ModelHubEvent::Use`]; the client applies it through the session lane.
//!
//! Ordering is omp's `sortModelItems` without role ranks (Ultron has no model roles): most recently used first,
//! then provider, version (descending) and recency. Searching keeps omp's relevance tiers (exact id or selector,
//! then contiguous, then fuzzy-only) over Ultron's fuzzy matcher, floats providers with matches to the top of the
//! sidebar and makes the scope hop skip scopes without matches.

use std::cmp::Ordering;
use std::rc::Rc;

use tui::collate::locale_compare;
use tui::components::Component;
use tui::fuzzy::fuzzy_match;
use tui::input::Input;
use tui::keybindings::KeybindingsManager;
use tui::outline::key_glyph;
use tui::sgr_mouse::parse_sgr_mouse;
use tui::theme::Theme;
use tui::utils::{truncate_to_width, visible_width};

use crate::components::{bold, fg};
use crate::lane::ModelInfo;

/// Ultron's thinking levels, in strip order.
pub const THINKING_LEVELS: [&str; 6] = ["off", "minimal", "low", "medium", "high", "xhigh"];
/// Recent scope size (`RECENT_LIMIT`).
pub const RECENT_LIMIT: usize = 15;

/// One model row.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelItem {
    pub model: ModelInfo,
    /// `provider/id`.
    pub selector: String,
}

impl ModelItem {
    pub fn new(model: ModelInfo) -> Self {
        let selector = format!("{}/{}", model.provider, model.id);
        ModelItem { model, selector }
    }
}

/// `extractVersionNumber`: `gemini-2.5-pro` -> 2.5, `claude-sonnet-4-6` -> 4.6, `gpt-4o` -> 4.
pub fn extract_version_number(id: &str) -> f64 {
    let b = id.as_bytes();
    let at_sep = |i: usize| i == 0 || b[i - 1] == b'-' || b[i - 1] == b'_';
    let digits = |from: usize| (from..b.len()).take_while(|&j| b[j].is_ascii_digit()).count();
    // Dot form: (?:^|[-_])(\d+\.\d+)
    for i in 0..b.len() {
        if at_sep(i) {
            let int = digits(i);
            if int > 0 && b.get(i + int) == Some(&b'.') {
                let frac = digits(i + int + 1);
                if frac > 0 {
                    return id[i..i + int + 1 + frac].parse().unwrap_or(0.0);
                }
            }
        }
    }
    // Dash form: (?:^|[-_])(\d{1,2})-(\d{1,2})(?=-|$)
    for i in 0..b.len() {
        if !at_sep(i) {
            continue;
        }
        // The regex backtracks over \d{1,2}: try the longest prefix first.
        for major in (1..=digits(i).min(2)).rev() {
            if b.get(i + major) != Some(&b'-') {
                continue;
            }
            let start = i + major + 1;
            for minor in (1..=digits(start).min(2)).rev() {
                let end = start + minor;
                if end == b.len() || b[end] == b'-' {
                    return format!("{}.{}", &id[i..i + major], &id[start..end])
                        .parse()
                        .unwrap_or(0.0);
                }
            }
        }
    }
    // Single number: (?:^|[-_])(\d+)
    for i in 0..b.len() {
        if at_sep(i) {
            let n = digits(i);
            if n > 0 {
                return id[i..i + n].parse().unwrap_or(0.0);
            }
        }
    }
    0.0
}

fn date_suffix(id: &str) -> &str {
    // `-(\d{8})$`
    let b = id.as_bytes();
    if b.len() >= 9 && b[b.len() - 9] == b'-' && b[b.len() - 8..].iter().all(u8::is_ascii_digit) {
        &id[id.len() - 8..]
    } else {
        ""
    }
}

/// `modelItemComparator` with no role assignments: MRU, provider, version (descending), recency, id.
pub fn compare_model_items(a: &ModelItem, b: &ModelItem, mru: &[String]) -> Ordering {
    let rank = |s: &str| mru.iter().position(|m| m == s).unwrap_or(usize::MAX);
    rank(&a.selector)
        .cmp(&rank(&b.selector))
        .then_with(|| locale_compare(&a.model.provider, &b.model.provider))
        .then_with(|| {
            let (va, vb) = (extract_version_number(&a.model.id), extract_version_number(&b.model.id));
            vb.partial_cmp(&va).unwrap_or(Ordering::Equal)
        })
        .then_with(|| {
            let (al, bl) = (a.model.id.ends_with("-latest"), b.model.id.ends_with("-latest"));
            let (ad, bd) = (date_suffix(&a.model.id), date_suffix(&b.model.id));
            let (ar, br) = (al || !ad.is_empty(), bl || !bd.is_empty());
            if ar != br {
                return if ar { Ordering::Less } else { Ordering::Greater };
            }
            if !ar {
                return locale_compare(&a.model.id, &b.model.id);
            }
            if al != bl {
                return if al { Ordering::Less } else { Ordering::Greater };
            }
            if !ad.is_empty() && !bd.is_empty() {
                return bd.cmp(ad);
            }
            locale_compare(&a.model.id, &b.model.id)
        })
}

/// `compactModelSearchText`: lowercase letters, marks and numbers only.
pub fn compact_search_text(value: &str) -> String {
    value.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// `modelSearchTier`: 0 exact id/selector, 1 contiguous, 2 fuzzy-only.
pub fn model_search_tier(query: &str, id: &str, selector: &str) -> u8 {
    if query.is_empty() {
        2
    } else if query == id || query == selector {
        0
    } else if id.contains(query) || selector.contains(query) {
        1
    } else {
        2
    }
}

/// Rank `items` (already in display order) for `query`: matching every token, then by tier, MRU affinity, fuzzy
/// score and display order.
pub fn rank_models(items: &[ModelItem], query: &str, mru: &[String]) -> Vec<ModelItem> {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() {
        return items.to_vec();
    }
    let key = compact_search_text(query);
    let mut rows: Vec<(u8, usize, i64, usize, &ModelItem)> = items
        .iter()
        .enumerate()
        .filter_map(|(order, item)| {
            let mut score = 0.0;
            for t in &tokens {
                let m = fuzzy_match(t, &item.selector);
                if !m.matches {
                    return None;
                }
                score += m.score;
            }
            let tier = model_search_tier(
                &key,
                &compact_search_text(&item.model.id),
                &compact_search_text(&item.selector),
            );
            let affinity = mru
                .iter()
                .position(|m| m.eq_ignore_ascii_case(&item.selector))
                .unwrap_or(usize::MAX);
            Some((tier, affinity, (score / 10.0).round() as i64, order, item))
        })
        .collect();
    rows.sort_by_key(|r| (r.0, r.1, r.2, r.3));
    rows.into_iter().map(|r| r.4.clone()).collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScopeKind {
    Recent,
    All,
    Separator,
    Provider(String),
}

/// One sidebar entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub kind: ScopeKind,
    pub label: String,
    /// Model count (or match count while searching).
    pub count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Scopes,
    Models,
}

/// What the client should do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelHubEvent {
    Ignored,
    Render,
    Cancel,
    /// Switch this session to `selector` (and `thinking`, when chosen). Saved defaults stay unchanged.
    Use {
        selector: String,
        thinking: Option<String>,
    },
}

/// Match counts while searching: Recent hits, hits per provider, total hits.
type SearchCounts = (usize, Vec<(String, usize)>, usize);

/// The inline thinking-level strip of a reasoning model.
struct Strip {
    item: ModelItem,
    index: usize,
}

pub struct ModelHub {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    items: Vec<ModelItem>,
    recent: Vec<ModelItem>,
    mru: Vec<String>,
    current: Option<String>,
    current_thinking: String,
    providers: Vec<String>,
    scopes: Vec<Scope>,
    active: ScopeKind,
    focus: Focus,
    query: Input,
    /// Items of the active scope for the query.
    shown: Vec<ModelItem>,
    cursor: usize,
    list_top: usize,
    search_counts: Option<SearchCounts>,
    strip: Option<Strip>,
    /// Layout of the last frame for mouse routing: sidebar width, first list row, list rows, first sidebar row.
    layout: (usize, usize, usize, usize),
}

impl ModelHub {
    /// `models`: what the session can use; `mru`: recently used selectors, newest first; `current`: the session's
    /// model and thinking level (preselected).
    pub fn new(
        theme: Rc<Theme>,
        keys: KeybindingsManager,
        models: Vec<ModelInfo>,
        mru: Vec<String>,
        current: Option<(String, String)>,
    ) -> Self {
        let mut items: Vec<ModelItem> = models.into_iter().map(ModelItem::new).collect();
        items.sort_by(|a, b| compare_model_items(a, b, &mru));
        let recent: Vec<ModelItem> = mru
            .iter()
            .filter_map(|s| items.iter().find(|i| &i.selector == s).cloned())
            .take(RECENT_LIMIT)
            .collect();
        let mut providers: Vec<String> = Vec::new();
        for i in &items {
            if !providers.contains(&i.model.provider) {
                providers.push(i.model.provider.clone());
            }
        }
        providers.sort_by(|a, b| locale_compare(a, b));
        let mut query = Input::new(keys.clone()).with_options(Some(""), Some("search models…"), None, None);
        query.focused = true;
        let (current, current_thinking) = match current {
            Some((s, t)) => (Some(s), t),
            None => (None, "off".into()),
        };
        let mut hub = ModelHub {
            theme,
            keys,
            items,
            recent,
            mru,
            current,
            current_thinking,
            providers,
            scopes: Vec::new(),
            active: ScopeKind::All,
            focus: Focus::Scopes,
            query,
            shown: Vec::new(),
            cursor: 0,
            list_top: 0,
            search_counts: None,
            strip: None,
            layout: (0, 0, 0, 0),
        };
        hub.compose_scopes();
        hub.apply_scope();
        if let Some(c) = hub.current.clone() {
            hub.cursor = hub.shown.iter().position(|i| i.selector == c).unwrap_or(0);
        }
        hub
    }

    pub fn scopes(&self) -> &[Scope] {
        &self.scopes
    }

    pub fn active_scope(&self) -> &ScopeKind {
        &self.active
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn shown(&self) -> &[ModelItem] {
        &self.shown
    }

    pub fn selected(&self) -> Option<&ModelItem> {
        self.shown.get(self.cursor)
    }

    /// The open thinking strip: model selector and highlighted level.
    pub fn strip(&self) -> Option<(&str, &str)> {
        self.strip
            .as_ref()
            .map(|s| (s.item.selector.as_str(), THINKING_LEVELS[s.index]))
    }

    /// `#composeEntries`: Recent and All, then providers (those with matches first while searching).
    fn compose_scopes(&mut self) {
        let mut scopes = Vec::new();
        let searching = self.search_counts.as_ref();
        if !self.recent.is_empty() {
            scopes.push(Scope {
                kind: ScopeKind::Recent,
                label: "Recent".into(),
                count: searching.map_or(self.recent.len(), |s| s.0),
            });
        }
        scopes.push(Scope {
            kind: ScopeKind::All,
            label: "All models".into(),
            count: searching.map_or(self.items.len(), |s| s.2),
        });
        let count_of = |p: &str| match searching {
            Some((_, counts, _)) => counts.iter().find(|(q, _)| q == p).map_or(0, |c| c.1),
            None => self.items.iter().filter(|i| i.model.provider == p).count(),
        };
        let mut providers = self.providers.clone();
        if searching.is_some() {
            providers.sort_by(|a, b| {
                let (am, bm) = (count_of(a) > 0, count_of(b) > 0);
                bm.cmp(&am).then_with(|| locale_compare(a, b))
            });
        }
        if !providers.is_empty() {
            scopes.push(Scope {
                kind: ScopeKind::Separator,
                label: String::new(),
                count: 0,
            });
        }
        for p in providers {
            let count = count_of(&p);
            scopes.push(Scope {
                kind: ScopeKind::Provider(p.clone()),
                label: p,
                count,
            });
        }
        self.scopes = scopes;
        if !self.scopes.iter().any(|s| s.kind == self.active) {
            self.active = ScopeKind::All;
        }
    }

    /// `#isHopSkipped`: separators always; while searching, scopes without matches.
    fn hop_skipped(&self, scope: &Scope) -> bool {
        match scope.kind {
            ScopeKind::Separator => true,
            _ if self.search_counts.is_none() => false,
            ScopeKind::Recent | ScopeKind::Provider(_) => scope.count == 0,
            ScopeKind::All => false,
        }
    }

    fn scope_items(&self) -> Vec<ModelItem> {
        match &self.active {
            ScopeKind::Recent => self.recent.clone(),
            ScopeKind::Provider(p) => self.items.iter().filter(|i| &i.model.provider == p).cloned().collect(),
            _ => self.items.clone(),
        }
    }

    fn apply_scope(&mut self) {
        let keep = self.selected().map(|i| i.selector.clone());
        self.shown = rank_models(&self.scope_items(), self.query.value(), &self.mru);
        self.cursor = keep
            .and_then(|s| self.shown.iter().position(|i| i.selector == s))
            .unwrap_or(0);
    }

    fn set_active(&mut self, kind: ScopeKind) {
        self.active = kind;
        self.list_top = 0;
        self.apply_scope();
        self.cursor = 0;
    }

    /// `#onQueryChanged`.
    fn query_changed(&mut self) {
        let query = self.query.value().to_string();
        if query.trim().is_empty() {
            self.search_counts = None;
        } else {
            let matches = rank_models(&self.items, &query, &self.mru);
            let mut counts: Vec<(String, usize)> = Vec::new();
            let mut recent = 0;
            for m in &matches {
                match counts.iter_mut().find(|(p, _)| *p == m.model.provider) {
                    Some(c) => c.1 += 1,
                    None => counts.push((m.model.provider.clone(), 1)),
                }
                if self.recent.iter().any(|r| r.selector == m.selector) {
                    recent += 1;
                }
            }
            self.search_counts = Some((recent, counts, matches.len()));
        }
        self.compose_scopes();
        let lost = match &self.active {
            ScopeKind::Provider(p) => self.search_counts.is_some() && self.scope_count(p) == 0,
            ScopeKind::Recent => self.search_counts.as_ref().is_some_and(|s| s.0 == 0),
            _ => false,
        };
        if lost {
            self.active = ScopeKind::All;
        }
        self.list_top = 0;
        self.apply_scope();
        self.cursor = 0;
    }

    fn scope_count(&self, provider: &str) -> usize {
        self.scopes
            .iter()
            .find(|s| s.kind == ScopeKind::Provider(provider.to_string()))
            .map_or(0, |s| s.count)
    }

    fn hop(&mut self, delta: i64) -> ModelHubEvent {
        let Some(at) = self.scopes.iter().position(|s| s.kind == self.active) else {
            return ModelHubEvent::Ignored;
        };
        let mut i = at as i64 + delta;
        while i >= 0 && (i as usize) < self.scopes.len() {
            let scope = &self.scopes[i as usize];
            if !self.hop_skipped(scope) {
                let kind = scope.kind.clone();
                self.set_active(kind);
                return ModelHubEvent::Render;
            }
            i += delta;
        }
        ModelHubEvent::Ignored
    }

    fn move_cursor(&mut self, delta: i64) -> ModelHubEvent {
        if self.shown.is_empty() {
            return ModelHubEvent::Ignored;
        }
        let next = (self.cursor as i64 + delta).clamp(0, self.shown.len() as i64 - 1) as usize;
        if next == self.cursor {
            return ModelHubEvent::Ignored;
        }
        self.cursor = next;
        ModelHubEvent::Render
    }

    /// Enter on a model: a reasoning model opens the thinking strip at the current level, any other switches now.
    fn activate(&mut self) -> ModelHubEvent {
        let Some(item) = self.selected().cloned() else {
            return ModelHubEvent::Ignored;
        };
        if !item.model.reasoning {
            return ModelHubEvent::Use {
                selector: item.selector,
                thinking: None,
            };
        }
        let index = THINKING_LEVELS
            .iter()
            .position(|l| *l == self.current_thinking)
            .unwrap_or(0);
        self.strip = Some(Strip { item, index });
        ModelHubEvent::Render
    }

    fn k(&self, data: &str, id: &str) -> bool {
        self.keys.matches(data, id)
    }

    pub fn handle_input(&mut self, data: &str) -> ModelHubEvent {
        if let Some(mouse) = parse_sgr_mouse(data) {
            return self.mouse(mouse);
        }
        if let Some(strip) = &mut self.strip {
            if self.keys.matches(data, "tui.select.cancel") {
                self.strip = None;
                return ModelHubEvent::Render;
            }
            if self.keys.matches(data, "app.modelHub.scopes") {
                strip.index = strip.index.saturating_sub(1);
                return ModelHubEvent::Render;
            }
            if self.keys.matches(data, "app.modelHub.models") {
                strip.index = (strip.index + 1).min(THINKING_LEVELS.len() - 1);
                return ModelHubEvent::Render;
            }
            if self.keys.matches(data, "tui.select.confirm") {
                let strip = self.strip.take().expect("strip");
                return ModelHubEvent::Use {
                    selector: strip.item.selector,
                    thinking: Some(THINKING_LEVELS[strip.index].to_string()),
                };
            }
            return ModelHubEvent::Ignored;
        }
        if self.k(data, "tui.select.cancel") {
            if !self.query.value().is_empty() {
                self.query.set_value("");
                self.query_changed();
                return ModelHubEvent::Render;
            }
            return ModelHubEvent::Cancel;
        }
        if self.k(data, "app.modelHub.focus") {
            self.focus = match self.focus {
                Focus::Scopes => Focus::Models,
                Focus::Models => Focus::Scopes,
            };
            return ModelHubEvent::Render;
        }
        if self.k(data, "app.modelHub.scopes") && self.query.value().is_empty() {
            self.focus = Focus::Scopes;
            return ModelHubEvent::Render;
        }
        if self.k(data, "app.modelHub.models") && self.query.value().is_empty() {
            self.focus = Focus::Models;
            return ModelHubEvent::Render;
        }
        if self.k(data, "tui.select.up") {
            return match self.focus {
                Focus::Scopes => self.hop(-1),
                Focus::Models => self.move_cursor(-1),
            };
        }
        if self.k(data, "tui.select.down") {
            return match self.focus {
                Focus::Scopes => self.hop(1),
                Focus::Models => self.move_cursor(1),
            };
        }
        let page = self.layout.2.max(1) as i64;
        if self.k(data, "tui.select.pageUp") {
            self.focus = Focus::Models;
            return self.move_cursor(-page);
        }
        if self.k(data, "tui.select.pageDown") {
            self.focus = Focus::Models;
            return self.move_cursor(page);
        }
        if self.k(data, "tui.select.confirm") {
            if self.focus == Focus::Scopes {
                self.focus = Focus::Models;
                return ModelHubEvent::Render;
            }
            return self.activate();
        }
        // Typing anywhere searches and focuses the model list.
        let before = self.query.value().to_string();
        self.query.handle_input(data);
        if self.query.value() != before {
            self.focus = Focus::Models;
            self.query_changed();
            return ModelHubEvent::Render;
        }
        ModelHubEvent::Ignored
    }

    fn mouse(&mut self, mouse: tui::sgr_mouse::SgrMouse) -> ModelHubEvent {
        let (sidebar, list_row, list_rows, side_row) = self.layout;
        let in_sidebar = mouse.col >= 0 && (mouse.col as usize) < sidebar + 2;
        if let Some(wheel) = mouse.wheel {
            return if in_sidebar {
                self.hop(wheel)
            } else {
                self.move_cursor(wheel)
            };
        }
        if !mouse.left_click || mouse.row < 0 {
            return ModelHubEvent::Ignored;
        }
        let row = mouse.row as usize;
        if in_sidebar {
            let Some(scope) = row.checked_sub(side_row).and_then(|i| self.scopes.get(i)).cloned() else {
                return ModelHubEvent::Ignored;
            };
            if self.hop_skipped(&scope) {
                return ModelHubEvent::Ignored;
            }
            self.focus = Focus::Scopes;
            self.set_active(scope.kind);
            return ModelHubEvent::Render;
        }
        let Some(index) = row
            .checked_sub(list_row)
            .filter(|i| *i < list_rows)
            .map(|i| self.list_top + i)
            .filter(|i| *i < self.shown.len())
        else {
            return ModelHubEvent::Ignored;
        };
        self.focus = Focus::Models;
        if index == self.cursor {
            return self.activate();
        }
        self.cursor = index;
        ModelHubEvent::Render
    }

    fn first_key(&self, id: &str) -> String {
        self.keys.get_keys(id).first().map(|k| key_glyph(k)).unwrap_or_default()
    }

    /// The hub at `width` x `height`.
    pub fn render(&mut self, width: usize, height: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let width = width.max(20);
        let height = height.max(8);
        let inner = width - 2;
        let sidebar = (inner / 4).clamp(14, 26);
        let pane = inner.saturating_sub(sidebar + 3).max(1);
        let body_rows = height - 4;
        // Sidebar.
        let mut side: Vec<String> = Vec::new();
        for scope in &self.scopes {
            if scope.kind == ScopeKind::Separator {
                side.push(fg(&t, "dim", &"─".repeat(sidebar)));
                continue;
            }
            let active = scope.kind == self.active;
            let skipped = self.hop_skipped(scope);
            let count = scope.count.to_string();
            let marker = if active { "❯ " } else { "  " };
            let label = truncate_to_width(
                &scope.label,
                sidebar.saturating_sub(visible_width(marker) + count.len() + 1),
                "…",
                false,
            );
            let gap = sidebar.saturating_sub(visible_width(marker) + visible_width(&label) + count.len());
            let text = format!("{marker}{label}{}{count}", " ".repeat(gap));
            side.push(if active && self.focus == Focus::Scopes {
                bold(&fg(&t, "accent", &text))
            } else if active {
                fg(&t, "accent", &text)
            } else if skipped {
                fg(&t, "dim", &text)
            } else {
                text
            });
        }
        // Browser pane: search row, blank, list, blank, detail.
        let mut right: Vec<String> = Vec::new();
        let search = self
            .query
            .render(pane.saturating_sub(2).max(1))
            .into_iter()
            .next()
            .unwrap_or_default();
        right.push(format!("{} {search}", fg(&t, "accent", "⌕")));
        right.push(String::new());
        let list_rows = body_rows.saturating_sub(4).max(1);
        if self.cursor < self.list_top {
            self.list_top = self.cursor;
        } else if self.cursor >= self.list_top + list_rows {
            self.list_top = self.cursor + 1 - list_rows;
        }
        self.list_top = self.list_top.min(self.shown.len().saturating_sub(list_rows));
        let show_provider = !matches!(self.active, ScopeKind::Provider(_));
        if self.shown.is_empty() {
            right.push(fg(&t, "muted", "  No models match"));
        }
        for (i, item) in self.shown.iter().enumerate().skip(self.list_top).take(list_rows) {
            let selected = i == self.cursor;
            let current = self.current.as_deref() == Some(item.selector.as_str());
            let name = if show_provider {
                item.selector.clone()
            } else {
                item.model.id.clone()
            };
            let ctx = format_context(item.model.context_window);
            let glyphs = format!(
                "{}{}",
                if item.model.reasoning { "◒" } else { " " },
                if current { "●" } else { " " }
            );
            let room = pane.saturating_sub(4 + ctx.len() + 3);
            let name = truncate_to_width(&name, room, "…", false);
            let gap = room.saturating_sub(visible_width(&name));
            let row = format!("{name}{} {ctx} {glyphs}", " ".repeat(gap));
            right.push(if selected {
                let style = if self.focus == Focus::Models { "accent" } else { "muted" };
                format!("{} {}", fg(&t, "accent", "❯"), bold(&fg(&t, style, &row)))
            } else if current {
                format!("  {}", fg(&t, "success", &row))
            } else {
                format!("  {row}")
            });
        }
        while right.len() < 2 + list_rows {
            right.push(String::new());
        }
        right.push(String::new());
        let detail = self
            .selected()
            .map(|i| {
                let reasoning = if i.model.reasoning { " · reasoning" } else { "" };
                format!("{}{reasoning}", i.model.name)
            })
            .unwrap_or_default();
        right.push(fg(&t, "dim", &detail));
        self.layout = (sidebar, 3, list_rows, 1);
        // Compose.
        let mut out = vec![top_border(&t, width, "Models")];
        for r in 0..body_rows {
            let l = side.get(r).cloned().unwrap_or_default();
            let lw = visible_width(&l);
            let rr = right.get(r).cloned().unwrap_or_default();
            let rr = truncate_to_width(&rr, pane, "…", false);
            let rw = visible_width(&rr);
            out.push(format!(
                "{} {l}{} {} {rr}{}{}",
                fg(&t, "border", "│"),
                " ".repeat(sidebar.saturating_sub(lw)),
                fg(&t, "border", "│"),
                " ".repeat(pane.saturating_sub(rw)),
                fg(&t, "border", "│")
            ));
        }
        out.push(fg(&t, "border", &format!("├{}┤", "─".repeat(width - 2))));
        let footer = match &self.strip {
            Some(strip) => {
                let chips: Vec<String> = THINKING_LEVELS
                    .iter()
                    .enumerate()
                    .map(|(i, l)| {
                        if i == strip.index {
                            bold(&fg(&t, "accent", &format!("[{l}]")))
                        } else {
                            fg(&t, "muted", &format!(" {l} "))
                        }
                    })
                    .collect();
                format!(
                    "{} {}",
                    fg(&t, "dim", &format!("thinking for {}:", strip.item.model.id)),
                    chips.join("")
                )
            }
            None => fg(
                &t,
                "dim",
                &format!(
                    "Session-only switch · {} {} use · {} focus · type to search · {} close",
                    self.first_key("tui.select.up"),
                    self.first_key("tui.select.confirm"),
                    self.first_key("app.modelHub.focus"),
                    self.first_key("tui.select.cancel"),
                ),
            ),
        };
        let footer = truncate_to_width(&footer, inner.saturating_sub(2), "…", false);
        let pad = inner.saturating_sub(2 + visible_width(&footer));
        out.push(format!(
            "{} {footer}{} {}",
            fg(&t, "border", "│"),
            " ".repeat(pad),
            fg(&t, "border", "│")
        ));
        out.push(fg(&t, "border", &format!("╰{}╯", "─".repeat(width - 2))));
        out
    }
}

/// `400k` context column; empty without a window.
pub fn format_context(window: u64) -> String {
    match window {
        0 => String::new(),
        w if w >= 1_000_000 && w % 1_000_000 == 0 => format!("{}M", w / 1_000_000),
        w if w >= 1_000 => format!("{}k", w / 1_000),
        w => w.to_string(),
    }
}

fn top_border(t: &Theme, width: usize, title: &str) -> String {
    let label = format!(" {title} ");
    let fill = width.saturating_sub(2 + visible_width(&label));
    format!(
        "{}{}{}",
        fg(t, "border", "╭"),
        bold(&label),
        fg(t, "border", &format!("{}╮", "─".repeat(fill)))
    )
}
