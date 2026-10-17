//! The full-screen RLM graph (experimental/rlm-focus.ts): the live graph with a selection, keyboard navigation,
//! per-node details and collapse. Navigation is a pure reducer over the laid-out rows ([`apply_focus_action`]);
//! [`RlmGraphFocus`] holds the state, maps keys through the keybindings and renders.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use tui::keybindings::KeybindingsManager;
use tui::utils::{truncate_to_width, visible_width};

use super::graph::{
    CollapseState, GraphRow, RowRenderOptions, budget_gauges, build_rlm_graph, frame_trace_details, graph_active,
    graph_totals, layout_graph, render_details, render_gauges, render_graph_row, render_kernel_strip, spinner_glyph,
};
use super::visualizer::{Color, RlmSnapshot, RlmStyle};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusAction {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Details,
    Collapse,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FocusState {
    /// Selected node key; kept across refreshes while the node exists.
    pub selected_key: Option<String>,
    /// Last selected row index, used when the selected node disappears.
    pub selected_index: usize,
    /// Nodes whose details are open.
    pub details: HashSet<String>,
    /// Collapse overrides (true collapsed, false expanded).
    pub collapse: CollapseState,
}

/// The selected row index for `rows`, following the node key when it still exists (`None`: no rows).
pub fn selected_row(state: &FocusState, rows: &[GraphRow]) -> Option<usize> {
    if rows.is_empty() {
        return None;
    }
    let by_key = state
        .selected_key
        .as_deref()
        .and_then(|k| rows.iter().position(|r| r.node.key == k));
    Some(by_key.unwrap_or(state.selected_index.min(rows.len() - 1)))
}

fn select(state: &FocusState, rows: &[GraphRow], index: isize) -> FocusState {
    let clamped = index.clamp(0, rows.len() as isize - 1) as usize;
    let mut next = state.clone();
    next.selected_index = clamped;
    if let Some(row) = rows.get(clamped) {
        next.selected_key = Some(row.node.key.clone());
    }
    next
}

/// Apply one navigation action; `page` is how many rows a page moves. `Collapse` toggles the selected subtree; on a
/// leaf it folds the parent and selects it.
pub fn apply_focus_action(state: &FocusState, rows: &[GraphRow], action: FocusAction, page: usize) -> FocusState {
    let Some(index) = selected_row(state, rows) else {
        return state.clone();
    };
    let i = index as isize;
    let page = page.max(1) as isize;
    let row = &rows[index];
    match action {
        FocusAction::Up => select(state, rows, i - 1),
        FocusAction::Down => select(state, rows, i + 1),
        FocusAction::PageUp => select(state, rows, i - page),
        FocusAction::PageDown => select(state, rows, i + page),
        FocusAction::Home => select(state, rows, 0),
        FocusAction::End => select(state, rows, rows.len() as isize - 1),
        FocusAction::Details => {
            let mut next = select(state, rows, i);
            if !next.details.remove(&row.node.key) {
                next.details.insert(row.node.key.clone());
            }
            next
        }
        FocusAction::Collapse => {
            if !row.node.children.is_empty() {
                let mut next = select(state, rows, i);
                next.collapse.insert(row.node.key.clone(), !row.collapsed);
                return next;
            }
            let Some(parent) = row.parent_key.as_deref().filter(|p| *p != "turn") else {
                return state.clone();
            };
            let mut next = state.clone();
            next.collapse.insert(parent.to_string(), true);
            next.selected_key = Some(parent.to_string());
            next.selected_index = rows.iter().position(|r| r.node.key == parent).unwrap_or(0);
            next
        }
    }
}

/// How the focus view draws.
#[derive(Clone)]
pub struct FocusRenderOptions<'a> {
    pub style: RlmStyle,
    pub spinner_frame: i64,
    /// Hint line text (keys), shown under the header.
    pub hints: Option<String>,
    /// Loaded detail lines by node key (frame traces).
    pub extra_details: Option<&'a HashMap<String, Vec<(String, String)>>>,
    /// Previous first body line, so the view scrolls only when the selection leaves it.
    pub scroll: usize,
}

impl Default for FocusRenderOptions<'_> {
    fn default() -> Self {
        FocusRenderOptions {
            style: RlmStyle::plain(),
            spinner_frame: 0,
            hints: None,
            extra_details: None,
            scroll: 0,
        }
    }
}

/// What [`render_focus_view`] drew: exactly `height` lines, the scroll it used, and the row keys.
#[derive(Clone, Debug, PartialEq)]
pub struct FocusView {
    pub lines: Vec<String>,
    pub scroll: usize,
    pub row_keys: Vec<String>,
}

/// Render the focus view into exactly `height` lines; the body scrolls to keep the selection and its details visible.
pub fn render_focus_view(
    snapshot: &RlmSnapshot,
    state: &FocusState,
    width: usize,
    height: usize,
    o: &FocusRenderOptions,
) -> FocusView {
    let style = &o.style;
    let bound = width.max(1);
    let spinner = spinner_glyph(o.spinner_frame);
    let graph = build_rlm_graph(snapshot);
    let rows = layout_graph(&graph, &state.collapse);
    let selected = selected_row(state, &rows);
    let selected_key = selected.map(|i| rows[i].node.key.clone());
    let totals = graph_totals(&graph);
    let status = if graph_active(&graph) {
        style.fg(Color::Accent, &format!("{spinner} running"))
    } else if graph.status == super::graph::GraphStatus::Failed {
        style.fg(Color::Error, "✗ failed")
    } else if graph.children.is_empty() {
        style.fg(Color::Dim, "idle")
    } else {
        style.fg(Color::Success, "✓ settled")
    };
    let mut counts = format!("{} nodes", totals.nodes - 1);
    if totals.running > 0 {
        counts += &format!(" · {} active", totals.running);
    }
    if totals.failed > 0 {
        counts += &format!(" · {} failed", totals.failed);
    }
    let mut head = vec![format!(
        "{} {status} {}",
        style.bold(&style.fg(Color::Accent, "RLM graph")),
        style.fg(Color::Muted, &counts)
    )];
    if let Some(e) = &snapshot.error {
        head.push(style.fg(Color::Error, &format!("inspection failed: {e}")));
    }
    if let Some(h) = o.hints.as_deref().filter(|h| !h.is_empty()) {
        head.push(style.fg(Color::Dim, h));
    }
    head.extend(render_gauges(&budget_gauges(snapshot, &graph), bound, style, 2));
    if let Some(strip) = render_kernel_strip(snapshot, bound, style) {
        head.push(strip);
    }
    head.push(style.fg(Color::Dim, &"─".repeat(bound)));

    let row_options = RowRenderOptions {
        style: style.clone(),
        now: snapshot.now,
        spinner: spinner.to_string(),
        selected_key: selected_key.clone(),
        gutter: true,
    };
    let mut body: Vec<String> = Vec::new();
    let mut selected_line = 0;
    let mut selected_span = 1;
    for (index, row) in rows.iter().enumerate() {
        if Some(index) == selected {
            selected_line = body.len();
        }
        body.push(render_graph_row(row, bound, &row_options));
        if state.details.contains(&row.node.key) {
            let extra = o.extra_details.and_then(|m| m.get(&row.node.key)).map(Vec::as_slice);
            let details = render_details(row, extra, bound, &row_options, 10);
            if Some(index) == selected {
                selected_span = 1 + details.len();
            }
            body.extend(details);
        }
    }
    let room = height.saturating_sub(head.len()).max(1);
    let mut scroll = o.scroll.min(selected_line);
    if selected_line + selected_span.min(room) > scroll + room {
        scroll = selected_line + selected_span.min(room) - room;
    }
    scroll = scroll.min(body.len().saturating_sub(room));
    let mut visible: Vec<String> = body.iter().skip(scroll).take(room).cloned().collect();
    if scroll > 0 && !visible.is_empty() {
        visible[0] = style.fg(
            Color::Dim,
            &truncate_to_width(&format!("  ↑ {scroll} more above"), bound, "…", false),
        );
    }
    let hidden_below = body.len() as isize - (scroll + room) as isize;
    if hidden_below > 0 && visible.len() > 1 {
        let last = visible.len() - 1;
        visible[last] = style.fg(
            Color::Dim,
            &truncate_to_width(&format!("  ↓ {hidden_below} more below"), bound, "…", false),
        );
    }
    let mut lines: Vec<String> = head
        .into_iter()
        .chain(visible)
        .take(height.max(1))
        .map(|l| truncate_to_width(&l, bound, "…", false))
        .collect();
    while lines.len() < height {
        lines.push(String::new());
    }
    FocusView {
        lines,
        scroll,
        row_keys: rows.iter().map(|r| r.node.key.clone()).collect(),
    }
}

const ACTIONS: [(FocusAction, &str); 6] = [
    (FocusAction::Up, "app.rlm.graph.up"),
    (FocusAction::Down, "app.rlm.graph.down"),
    (FocusAction::PageUp, "app.rlm.graph.pageUp"),
    (FocusAction::PageDown, "app.rlm.graph.pageDown"),
    (FocusAction::Details, "app.rlm.graph.details"),
    (FocusAction::Collapse, "app.rlm.graph.collapse"),
];

fn key_label(keys: &KeybindingsManager, action: &str) -> String {
    let k = keys.get_keys(action);
    if k.is_empty() { "unbound".into() } else { k.join("/") }
}

/// Key hints from the live bindings, e.g. "up/k down/j move · enter details · c collapse · escape back".
pub fn focus_hints(keys: &KeybindingsManager) -> String {
    [
        format!(
            "{} {} move",
            key_label(keys, "app.rlm.graph.up"),
            key_label(keys, "app.rlm.graph.down")
        ),
        format!("{} details", key_label(keys, "app.rlm.graph.details")),
        format!("{} collapse", key_label(keys, "app.rlm.graph.collapse")),
        format!("{} back", key_label(keys, "app.rlm.graph.exit")),
    ]
    .join(" · ")
}

/// What a key did in the focus view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FocusEvent {
    /// Not the view's key.
    Ignored,
    Render,
    Exit,
    /// Open details of a frame whose trace is not loaded: fetch `rlm.frames {id}` and pass it to
    /// [`RlmGraphFocus::set_trace`] under the node key.
    LoadTrace {
        key: String,
        trace_id: String,
    },
}

/// The focus view's state across frames: selection, details, collapse, scroll and loaded frame traces.
#[derive(Clone, Debug, Default)]
pub struct RlmGraphFocus {
    pub state: FocusState,
    scroll: usize,
    extra: HashMap<String, Vec<(String, String)>>,
    loading: HashSet<String>,
}

impl RlmGraphFocus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle a key against the current snapshot; `height` is the view's height (a page is `height - 8` rows).
    pub fn handle_input(
        &mut self,
        keys: &KeybindingsManager,
        data: &str,
        snapshot: &RlmSnapshot,
        height: usize,
    ) -> FocusEvent {
        if keys.matches(data, "app.rlm.graph.exit") {
            return FocusEvent::Exit;
        }
        let Some(action) = ACTIONS.iter().find(|(_, id)| keys.matches(data, id)).map(|(a, _)| *a) else {
            return FocusEvent::Ignored;
        };
        let graph = build_rlm_graph(snapshot);
        let rows = layout_graph(&graph, &self.state.collapse);
        self.state = apply_focus_action(&self.state, &rows, action, height.saturating_sub(8).max(1));
        if action == FocusAction::Details
            && let Some(key) = self.state.selected_key.clone()
            && self.state.details.contains(&key)
            && !self.extra.contains_key(&key)
            && !self.loading.contains(&key)
            && let Some(trace_id) = rows
                .iter()
                .find(|r| r.node.key == key)
                .and_then(|r| r.node.trace_id.clone())
        {
            self.loading.insert(key.clone());
            return FocusEvent::LoadTrace { key, trace_id };
        }
        FocusEvent::Render
    }

    /// A loaded frame trace (`rlm.frames {id}`), or why it is unavailable.
    pub fn set_trace(&mut self, key: &str, trace: Result<&Value, &str>) {
        self.loading.remove(key);
        let lines = match trace {
            Ok(t) => frame_trace_details(t),
            Err(e) => vec![("trace".to_string(), format!("unavailable: {e}"))],
        };
        self.extra.insert(key.to_string(), lines);
    }

    /// Render `height` (at least 6) lines.
    pub fn render(
        &mut self,
        keys: &KeybindingsManager,
        snapshot: &RlmSnapshot,
        width: usize,
        height: usize,
        style: &RlmStyle,
        spinner_frame: i64,
    ) -> Vec<String> {
        let view = render_focus_view(
            snapshot,
            &self.state,
            width,
            height.max(6),
            &FocusRenderOptions {
                style: style.clone(),
                spinner_frame,
                hints: Some(focus_hints(keys)),
                extra_details: Some(&self.extra),
                scroll: self.scroll,
            },
        );
        self.scroll = view.scroll;
        // Keep the selection on a real row after the graph changed shape.
        let index = if view.row_keys.is_empty() {
            None
        } else {
            Some(
                self.state
                    .selected_key
                    .as_deref()
                    .and_then(|k| view.row_keys.iter().position(|r| r == k))
                    .unwrap_or(self.state.selected_index.min(view.row_keys.len() - 1)),
            )
        };
        if let Some(i) = index
            && self.state.selected_key.as_deref() != Some(view.row_keys[i].as_str())
        {
            self.state.selected_key = Some(view.row_keys[i].clone());
            self.state.selected_index = i;
        }
        view.lines
            .into_iter()
            .map(|l| {
                if visible_width(&l) > width {
                    truncate_to_width(&l, width, "…", false)
                } else {
                    l
                }
            })
            .collect()
    }
}
