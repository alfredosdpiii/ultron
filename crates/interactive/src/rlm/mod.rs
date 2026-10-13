//! The client TUI's live RLM views (experimental/rlm-visualizer.ts, rlm-graph.ts, rlm-focus.ts, rlm-pane.ts) as
//! pure model and render code: [`RlmSnapshot`] from the worker's inspections and the lane transcript, the run graph
//! with its one-line footer summary, docked panel and full-screen focus view, and the RLM pane of dependency waves
//! with the live wave summary.

pub mod focus;
pub mod graph;
pub mod pane;
pub mod visualizer;

pub use focus::{FocusAction, FocusState, RlmGraphFocus, apply_focus_action, focus_hints, render_focus_view};
pub use graph::{
    GraphKind, GraphNode, GraphRow, GraphStatus, build_rlm_graph, layout_graph, render_rlm_dock, render_rlm_footer,
};
pub use pane::{DagAction, DagRun, PaneEvent, RlmPane, RlmPaneState, build_dag_runs, render_wave_summary};
pub use visualizer::{RlmClock, RlmSnapshot, RlmStyle};

/// JavaScript `String(n)` for the numbers these views print.
pub(crate) fn js_num(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        format!("{n}")
    }
}

/// UTF-16 length (JavaScript `length`).
pub(crate) fn js_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// JavaScript `s.slice(0, n)` on UTF-16 units.
pub(crate) fn js_slice(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
}

/// `text.replace(/\s+/g, " ").trim()`.
pub(crate) fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `flat(text, limit)`: one line, cut to `limit` UTF-16 units with an ellipsis.
pub(crate) fn flat(text: &str, limit: usize) -> String {
    let line = squash(text);
    if js_len(&line) > limit {
        format!("{}…", js_slice(&line, limit - 1))
    } else {
        line
    }
}
