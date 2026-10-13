//! The live RLM views (crates/interactive/src/rlm): replays `fixtures/rlm_views.json`, recorded from the TypeScript
//! rlm-visualizer.ts / rlm-graph.ts / rlm-focus.ts / rlm-pane.ts by `fixtures/generate-rlm-views.mjs`, and ports
//! experimental-rlm-visualizer.test.ts and experimental-rlm-pane.test.ts with their literal expectations.

use std::collections::{HashMap, HashSet};

use interactive::rlm::RlmGraphFocus;
use interactive::rlm::focus::{
    FocusAction, FocusRenderOptions, FocusState, apply_focus_action, render_focus_view, selected_row,
};
use interactive::rlm::graph::{
    CollapseState, GraphNode, GraphStatus, RlmViewOptions, RowRenderOptions, bar, budget_gauges, build_rlm_graph,
    format_cost, format_tokens, frame_trace_details, layout_graph, render_gauges, render_graph_row,
    render_kernel_strip, render_rlm_dock, render_rlm_footer, window_rows,
};
use interactive::rlm::pane::{
    DagAction, DagRenderOptions, RlmPaneState, apply_dag_action, build_dag_runs, compute_waves, pane_work_runs,
    prompt_of, render_rlm_pane, render_wave_summary, rlm_pane_hints, selected_node, wave_geometry,
};
use interactive::rlm::visualizer::{
    CellStatus, RlmClock, RlmSnapshot, RlmStyle, RunningTool, TranscriptView, extract_root_cell, extract_turn,
    format_duration, parse_agents_status, parse_frames, parse_pool, parse_retained, short_id,
};
use serde_json::{Value, json};
use tui::keybindings::{KeybindingsManager, Platform, app_keybindings};
use tui::utils::visible_width;

const NOW: f64 = 1_000_000.0;

fn fixture() -> &'static Value {
    static F: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    F.get_or_init(|| serde_json::from_str(include_str!("../fixtures/rlm_views.json")).unwrap())
}

fn keys() -> KeybindingsManager {
    KeybindingsManager::new(
        app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    )
}

fn plain() -> RlmStyle {
    RlmStyle::plain()
}

/// A JSON number as JavaScript writes it (integral values without a fraction).
fn num(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 9e15 {
        json!(n as i64)
    } else {
        json!(n)
    }
}

fn opt(n: Option<f64>) -> Value {
    n.map_or(Value::Null, num)
}

fn graph_json(node: &GraphNode) -> Value {
    json!({
        "key": node.key,
        "kind": node.kind.name(),
        "label": node.label,
        "status": node.status.name(),
        "startedAt": opt(node.started_at),
        "endedAt": opt(node.ended_at),
        "tokens": opt(node.tokens),
        "cost": opt(node.cost),
        "progress": node.progress.map_or(Value::Null, |p| json!({"total": p.total, "done": p.done, "running": p.running, "pending": p.pending, "incomplete": p.incomplete, "failed": p.failed})),
        "join": node.join,
        "note": node.note,
        "traceId": node.trace_id,
        "details": node.details.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>(),
        "children": node.children.iter().map(graph_json).collect::<Vec<_>>(),
    })
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn assert_lines(name: &str, what: &str, got: &[String], want: &Value) {
    let want = strings(want);
    if got != want.as_slice() {
        let diff: Vec<String> = got
            .iter()
            .zip(&want)
            .enumerate()
            .filter(|(_, (g, w))| g != w)
            .map(|(i, (g, w))| format!("line {i}:\n  got  {g:?}\n  want {w:?}"))
            .collect();
        panic!(
            "{name} {what} differs ({} vs {} lines):\n{}\n--- got\n{}",
            got.len(),
            want.len(),
            diff.join("\n"),
            got.join("\n")
        );
    }
}

#[test]
fn interactive_rlm_views_replay_typescript_fixture() {
    let keys = keys();
    for case in fixture()["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let snap = RlmSnapshot::from_wire(&case["wire"]);
        let graph = build_rlm_graph(&snap);
        assert_eq!(graph_json(&graph), case["graph"], "{name}: graph");
        let rows = layout_graph(&graph, &CollapseState::new());
        let rows_json: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({"key": r.node.key, "depth": r.depth, "rails": r.rails, "last": r.last, "collapsed": r.collapsed,
                    "hidden": r.hidden, "parentKey": r.parent_key})
            })
            .collect();
        assert_eq!(Value::Array(rows_json), case["rows"], "{name}: rows");
        let view = RlmViewOptions {
            focus_key: Some("alt+g".into()),
            spinner_frame: 3,
            ..RlmViewOptions::default()
        };
        for (width, want) in case["footer"].as_object().unwrap() {
            let w: usize = width.parse().unwrap();
            let got = render_rlm_footer(&snap, w, &view);
            assert_eq!(got.as_deref(), want.as_str(), "{name}: footer at {w}");
        }
        for (width, want) in case["dock"].as_object().unwrap() {
            let w: usize = width.parse().unwrap();
            assert_lines(
                name,
                &format!("dock at {w}"),
                &render_rlm_dock(&snap, w, &view, 8),
                want,
            );
        }
        assert_lines(
            name,
            "default dock",
            &render_rlm_dock(&snap, 80, &RlmViewOptions::default(), 12),
            &case["dockDefault"],
        );
        let hints = interactive::rlm::focus_hints(&keys);
        for (size, want) in case["focus"].as_object().unwrap() {
            let (w, h) = size.split_once('x').unwrap();
            let view = render_focus_view(
                &snap,
                &FocusState::default(),
                w.parse().unwrap(),
                h.parse().unwrap(),
                &FocusRenderOptions {
                    hints: Some(hints.clone()),
                    spinner_frame: 1,
                    ..FocusRenderOptions::default()
                },
            );
            assert_lines(name, &format!("focus {size}"), &view.lines, want);
        }
        let detailed = FocusState {
            selected_key: case["focusDetailsKey"].as_str().map(str::to_string),
            details: strings(&case["focusDetailsOpen"]).into_iter().collect(),
            ..FocusState::default()
        };
        let view = render_focus_view(
            &snap,
            &detailed,
            80,
            16,
            &FocusRenderOptions {
                hints: Some(hints.clone()),
                spinner_frame: 1,
                ..FocusRenderOptions::default()
            },
        );
        assert_lines(name, "focus details", &view.lines, &case["focusDetails"]);

        let runs = build_dag_runs(&snap);
        let runs_json: Vec<Value> = runs
            .iter()
            .map(|r| {
                json!({"key": r.key, "kind": match r.kind { interactive::rlm::pane::RunKind::Turn => "turn", _ => "workflow" },
                    "id": r.id, "title": r.title, "waves": r.waves,
                    "edges": r.edges.iter().map(|(a, b)| json!([a, b])).collect::<Vec<_>>(), "status": r.status.name()})
            })
            .collect();
        assert_eq!(Value::Array(runs_json), case["runs"], "{name}: runs");
        for (size, want) in case["pane"].as_object().unwrap() {
            let (w, h) = size.split_once('x').unwrap();
            let options = DagRenderOptions {
                hints: rlm_pane_hints(&keys),
                task_count: snap.tasks.len(),
                focused: Some(false),
                focus_hint: Some("alt+w focus".into()),
                ..DagRenderOptions::new(snap.now)
            };
            let got = render_rlm_pane(
                &runs,
                &RlmPaneState::default(),
                w.parse().unwrap(),
                h.parse().unwrap(),
                &options,
            );
            assert_lines(name, &format!("pane {size}"), &got.lines, want);
        }
        for (run, want) in runs.iter().zip(case["paneRuns"].as_array().unwrap()) {
            let state = RlmPaneState {
                run_key: Some(run.key.clone()),
                ..RlmPaneState::default()
            };
            let got = render_rlm_pane(&runs, &state, 60, 50, &DagRenderOptions::new(snap.now));
            assert_lines(name, &format!("pane run {}", run.key), &got.lines, want);
        }
        for (width, want) in case["waveSummary"].as_object().unwrap() {
            let w: usize = width.parse().unwrap();
            assert_lines(
                name,
                &format!("wave summary at {w}"),
                &render_wave_summary(&runs, w, &plain(), snap.now, 4),
                want,
            );
        }
        assert_lines(
            name,
            "wave summary of 3",
            &render_wave_summary(&runs, 80, &plain(), snap.now, 3),
            &case["waveSummary3"],
        );
        let keys_of = |list: Vec<&interactive::rlm::DagRun>| list.iter().map(|r| r.key.clone()).collect::<Vec<_>>();
        assert_eq!(
            keys_of(pane_work_runs(&runs, None)),
            strings(&case["paneWork"]),
            "{name}"
        );
        assert_eq!(
            keys_of(pane_work_runs(&runs, Some(NOW - 30_000.0))),
            strings(&case["paneWorkSince"]),
            "{name}"
        );
    }
}

fn cell_json(c: &interactive::rlm::visualizer::RlmRootCell) -> Value {
    let mut v = json!({"toolCallId": c.tool_call_id, "code": c.code, "status": match c.status {
        CellStatus::Running => "running", CellStatus::Ok => "ok", CellStatus::Error => "error" }});
    if let Some(s) = c.started_at {
        v["startedAt"] = num(s);
    }
    if let Some(e) = c.ended_at {
        v["endedAt"] = num(e);
    }
    if let Some(o) = &c.output {
        v["output"] = json!(o);
    }
    v
}

#[test]
fn interactive_rlm_extract_turn_and_root_cell_replay_typescript() {
    for case in fixture()["extract"].as_array().unwrap() {
        let running = case["running"].as_array().map(|tools| {
            tools
                .iter()
                .map(|t| RunningTool {
                    status: t["status"].as_str().unwrap().into(),
                    tool_call_id: t["toolCallId"].as_str().unwrap().into(),
                    tool_name: t["toolName"].as_str().unwrap().into(),
                    args: t["args"].clone(),
                    result: Value::Null,
                    is_error: false,
                })
                .collect()
        });
        let view = TranscriptView {
            transcript: extract_transcript(),
            running_tools: running,
        };
        let now = case["now"].as_f64().unwrap();
        let (turn, cells) = extract_turn(Some(&view), &mut RlmClock::new(), now, 30);
        let turn = turn.unwrap();
        assert_eq!(
            json!({"startedAt": opt(turn.started_at), "prompt": turn.prompt}),
            case["turn"],
            "turn at {now}"
        );
        assert_eq!(
            Value::Array(cells.iter().map(cell_json).collect()),
            case["cells"],
            "cells at {now}"
        );
        let root = extract_root_cell(Some(&view), &mut RlmClock::new(), now).unwrap();
        assert_eq!(cell_json(&root), case["rootCell"], "root cell at {now}");
    }
}

/// The transcript the generator extracts from (its `transcript`).
fn extract_transcript() -> Vec<Value> {
    let call = |id: &str, code: &str, t: i64| json!({"message": {"role": "assistant", "timestamp": t, "content": [{"type": "toolCall", "id": id, "name": "rlm", "arguments": {"code": code}}]}});
    vec![
        json!({"message": {"role": "user", "content": "old question", "timestamp": 1}}),
        call("c0", "old", 2),
        json!({"message": {"role": "user", "content": [{"type": "text", "text": "count  the\nrows"}, {"type": "text", "text": "now"}], "timestamp": 100}}),
        call("c1", "print(1)", 105),
        json!({"message": {"role": "toolResult", "toolName": "rlm", "toolCallId": "c1", "isError": true, "timestamp": 109, "content": [{"type": "text", "text": "NameError"}]}}),
        call("c2", "print(2)", 110),
        json!({"message": {"role": "toolResult", "toolName": "rlm", "toolCallId": "c2", "isError": false, "timestamp": 115, "content": [{"type": "text", "text": "2"}]}}),
        call("c3", "print(3)", 116),
    ]
}

#[test]
fn interactive_rlm_formats_replay_typescript() {
    let f = &fixture()["formats"];
    for row in f["duration"].as_array().unwrap() {
        assert_eq!(format_duration(row[0].as_f64().unwrap()), row[1], "{row}");
    }
    for row in f["tokens"].as_array().unwrap() {
        assert_eq!(format_tokens(row[0].as_f64().unwrap()), row[1], "{row}");
    }
    for row in f["cost"].as_array().unwrap() {
        assert_eq!(format_cost(row[0].as_f64().unwrap()), row[1], "{row}");
    }
    for row in f["bar"].as_array().unwrap() {
        assert_eq!(
            bar(row[0].as_f64().unwrap(), row[1].as_u64().unwrap() as usize),
            row[2],
            "{row}"
        );
    }
    for row in f["windows"].as_array().unwrap() {
        let (s, e) = window_rows(
            row[0].as_u64().unwrap() as usize,
            row[1].as_u64().unwrap() as usize,
            row[2].as_u64().unwrap() as usize,
        );
        assert_eq!(json!({"start": s, "end": e}), row[3], "{row}");
    }
    for row in f["waves"].as_array().unwrap() {
        let (per_row, box_width) = wave_geometry(
            row[0].as_u64().unwrap() as usize,
            row[1].as_u64().unwrap() as usize,
            row[2].as_u64().unwrap() as usize,
        );
        assert_eq!(json!({"perRow": per_row, "boxWidth": box_width}), row[3], "{row}");
    }
    for row in f["prompts"].as_array().unwrap() {
        assert_eq!(json!(prompt_of(row[0].as_str())), row[1], "{row}");
    }
    for row in f["shortIds"].as_array().unwrap() {
        assert_eq!(short_id(row[0].as_str().unwrap()), row[1], "{row}");
    }
    let keys = keys();
    assert_eq!(interactive::rlm::focus_hints(&keys), f["focusHints"]);
    assert_eq!(json!(rlm_pane_hints(&keys)), f["paneHints"]);
}

// ---- ported from experimental-rlm-visualizer.test.ts ----

fn task(id: &str, state: &str, extra: Value) -> Value {
    let mut t = json!({"id": format!("ultron-task-{id}"), "definition": "reviewer@1", "state": state});
    for (k, v) in extra.as_object().unwrap() {
        t[k] = v.clone();
    }
    t
}

fn map_frames() -> Vec<Value> {
    (0..99)
        .map(|i| {
            let mut f = json!({
                "id": format!("frame-{i:08}"),
                "status": if i < 77 { "complete" } else if i < 80 { "incomplete" } else { "running" },
                "task": "Classify the sentiment of each review",
                "spent": {"calls": if i < 88 { 1 } else { 0 }, "tokens": if i < 88 { 400 } else { 0 }},
                "startedAt": NOW - 12_000.0,
                "taskId": if i < 88 { json!(format!("ultron-task-f{i}")) } else { Value::Null },
                "kind": "map", "batch": 99, "callerTaskId": null, "lane": null,
                "budget": {"id": "budget-map00001", "calls": 120, "tokens": null, "depth": 1},
            });
            if (77..80).contains(&i) {
                f["reason"] = json!("contract_unmet");
            }
            if i < 80 {
                f["endedAt"] = json!(NOW - 1000.0);
            }
            f
        })
        .collect()
}

/// The TS test's `fixture()` in wire form; `edit` adjusts it.
fn rich(edit: impl FnOnce(&mut Value)) -> RlmSnapshot {
    let mut w = json!({
        "now": NOW,
        "turn": {"startedAt": NOW - 72_000.0, "prompt": "Summarize the reviews and benchmark in the background"},
        "cells": [
            {"toolCallId": "call-1", "code": "h = await rlm.load('reviews.csv')\nprint(h)", "status": "ok", "startedAt": NOW - 60_000.0, "endedAt": NOW - 58_800.0},
            {"toolCallId": "call-2", "code": "# fan out\nvs = await rlm.map('Classify', h.chunks(4000), contract=str)", "status": "running", "startedAt": NOW - 14_000.0},
        ],
        "agentsStatus": {
            "tasks": [
                task("aaaa1111", "completed", json!({"startedAt": NOW - 59_500.0, "endedAt": NOW - 59_000.0, "cost": 0.012, "tokens": 3400, "result": {"status": "succeeded", "preview": "LGTM with 2 nits"}})),
                task("bbbb2222", "running", json!({"definition": "rlm-child@1", "startedAt": NOW - 13_000.0, "lane": "ultron.rlm-child.bbbb2222"})),
                task("cccc3333", "completed", json!({"definition": "planner@1", "parentId": "ultron-task-bbbb2222", "startedAt": NOW - 12_000.0, "endedAt": NOW - 9000.0})),
                task("w1", "completed", json!({"definition": "coder@1", "startedAt": NOW - 11_000.0, "endedAt": NOW - 8000.0, "workflow": {"run": "wf-12345678", "node": "plan", "dependsOn": [], "join": "all"}})),
                task("w2", "completed", json!({"definition": "coder@1", "startedAt": NOW - 10_000.0, "endedAt": NOW - 7000.0, "workflow": {"run": "wf-12345678", "node": "code", "dependsOn": [], "join": "all"}})),
                task("w3", "running", json!({"startedAt": NOW - 6000.0, "input": "{\"diff\":\"...\"}", "workflow": {"run": "wf-12345678", "node": "review", "dependsOn": ["plan", "code"], "join": "all"}})),
                task("f0", "completed", json!({"definition": "rlm-frame@1"})),
                task("old1", "completed", json!({"startedAt": NOW - 600_000.0, "endedAt": NOW - 590_000.0})),
            ],
            "jobs": [
                {"id": "job-1", "status": "running", "command": "sleep 30 && echo done", "exitCode": null, "startedAt": NOW - 9400.0, "lane": "main"},
                {"id": "job-2", "status": "running", "command": "pytest -q", "exitCode": null, "startedAt": NOW - 5000.0, "lane": "ultron.rlm-child.bbbb2222", "tail": "....F.."},
            ],
            "usage": {"admittedTasks": 5, "remainingWallMs": 26.0 * 60_000.0, "usage": {"totalTokens": 38_200, "cost": 0.042}, "cost": {"spentUsd": 0.042, "maxCostUsd": null}},
            "limits": {"maxAdmittedTasks": 24, "maxWallMs": 30.0 * 60_000.0},
        },
        "frames": {"frames": map_frames()},
        "pool": {"live": 3, "maxLive": 16, "lanes": [
            {"lane": "main", "running": 1, "pinnedBy": []},
            {"lane": "ultron.rlm-child.bbbb2222", "running": 1, "pinnedBy": []},
            {"lane": "ultron.planner.x", "running": 0, "pinnedBy": ["instance-1"]},
        ], "evictions": 2, "memoryBytes": 212 * 1024 * 1024},
    });
    edit(&mut w);
    RlmSnapshot::from_wire(&w)
}

fn all_keys(node: &GraphNode) -> Vec<String> {
    std::iter::once(node.key.clone())
        .chain(node.children.iter().flat_map(all_keys))
        .collect()
}

fn child_keys(node: &GraphNode) -> Vec<&str> {
    node.children.iter().map(|c| c.key.as_str()).collect()
}

fn row_options() -> RowRenderOptions {
    RowRenderOptions::new(plain(), NOW, "⠋")
}

fn strip(line: &str) -> String {
    tui::utils::strip_terminal_sequences(line)
}

#[test]
fn interactive_rlm_graph_attaches_work_to_cells_parents_and_lanes() {
    let graph = build_rlm_graph(&rich(|_| {}));
    assert_eq!(graph.status, GraphStatus::Running);
    assert_eq!(child_keys(&graph), ["earlier", "cell:call-1", "cell:call-2"]);
    assert_eq!(child_keys(&graph.children[0]), ["task:ultron-task-old1"]);
    assert_eq!(child_keys(&graph.children[1]), ["task:ultron-task-aaaa1111"]);
    assert_eq!(
        child_keys(&graph.children[2]),
        [
            "task:ultron-task-bbbb2222",
            "frames:budget-map00001",
            "workflow:wf-12345678",
            "job:job-1"
        ]
    );
    assert_eq!(
        child_keys(&graph.children[2].children[0]),
        ["task:ultron-task-cccc3333", "job:job-2"]
    );
    assert!(!all_keys(&graph).contains(&"task:ultron-task-f0".to_string()));
}

#[test]
fn interactive_rlm_graph_fans_a_map_out_as_one_node() {
    let graph = build_rlm_graph(&rich(|_| {}));
    let fanout = &graph.children[2].children[1];
    assert_eq!(fanout.label, "rlm.map 99 frames");
    assert_eq!(fanout.status, GraphStatus::Running);
    let p = fanout.progress.unwrap();
    assert_eq!(
        (p.total, p.done, p.incomplete, p.running, p.pending, p.failed),
        (99, 77, 3, 8, 11, 0)
    );
    assert_eq!(fanout.tokens, Some(88.0 * 400.0));
    assert!(fanout.children.is_empty());
    assert!(fanout.details.contains(&(
        "budget".into(),
        "map00001 · calls 88/120 · tokens 35200/∞ · depth 1".into()
    )));
    assert!(
        fanout
            .details
            .contains(&("fetch".into(), "await rlm.frames(\"frame-00000077\")".into()))
    );
}

#[test]
fn interactive_rlm_graph_groups_workflows_and_marks_joins() {
    let graph = build_rlm_graph(&rich(|_| {}));
    let workflow = &graph.children[2].children[2];
    assert_eq!(workflow.label, "workflow 12345678 3 nodes");
    let p = workflow.progress.unwrap();
    assert_eq!((p.total, p.done, p.running), (3, 2, 1));
    assert_eq!(workflow.note.as_deref(), Some("1 join"));
    let labels: Vec<&str> = workflow.children.iter().map(|c| c.label.as_str()).collect();
    assert_eq!(labels, ["plan coder@1 w1", "code coder@1 w2", "review reviewer@1 w3"]);
    assert_eq!(workflow.children[2].join.as_deref(), Some("⇐ plan+code (all)"));
}

#[test]
fn interactive_rlm_graph_nested_frames_orphan_frame_tasks_and_cycles() {
    let snap = RlmSnapshot::from_wire(&json!({
        "now": NOW,
        "agentsStatus": {"tasks": [
            task("frame1", "completed", json!({"definition": "rlm-frame@1"})),
            task("loose1", "completed", json!({"definition": "rlm-frame@1", "parentId": "ultron-task-parent"})),
            task("loose2", "failed", json!({"definition": "rlm-frame@1", "parentId": "ultron-task-parent"})),
            task("parent", "running", json!({"startedAt": NOW - 100.0})),
        ]},
        "frames": {"frames": [
            {"id": "frame-aaaaaaaa1", "status": "complete", "task": "outer", "spent": {"calls": 1, "tokens": 10}, "startedAt": NOW - 50.0, "endedAt": NOW - 10.0, "taskId": "ultron-task-frame1", "kind": "infer", "batch": 1, "callerTaskId": null, "budget": {"id": "budget-outer", "calls": 3, "tokens": null, "depth": 2}},
            {"id": "frame-bbbbbbbb2", "status": "incomplete", "reason": "budget_exhausted", "task": "inner", "spent": {"calls": 1, "tokens": 5}, "startedAt": NOW - 40.0, "endedAt": NOW - 20.0, "taskId": "ultron-task-frame2", "kind": "infer", "batch": 1, "callerTaskId": "ultron-task-frame1", "budget": {"id": "budget-inner", "calls": 1, "tokens": null, "depth": 1}},
        ]},
    }));
    let graph = build_rlm_graph(&snap);
    let outer = graph.children.iter().find(|n| n.key == "frames:budget-outer").unwrap();
    assert_eq!(outer.label, "rlm.infer aaaaaaaa");
    assert_eq!(outer.status, GraphStatus::Done);
    assert_eq!(outer.trace_id.as_deref(), Some("frame-aaaaaaaa1"));
    let inner: Vec<(&str, GraphStatus, Option<&str>)> = outer
        .children
        .iter()
        .map(|c| (c.label.as_str(), c.status, c.note.as_deref()))
        .collect();
    assert_eq!(
        inner,
        [("rlm.infer bbbbbbbb", GraphStatus::Incomplete, Some("budget_exhausted"))]
    );
    let parent = graph
        .children
        .iter()
        .find(|n| n.key == "task:ultron-task-parent")
        .unwrap();
    let frames = &parent.children[0];
    assert_eq!(frames.label, "frames 2");
    assert_eq!((frames.progress.unwrap().done, frames.progress.unwrap().failed), (1, 1));

    let cyclic = build_rlm_graph(&RlmSnapshot::from_wire(&json!({"now": NOW, "agentsStatus": {"tasks": [
        task("a", "running", json!({"parentId": "ultron-task-b"})),
        task("b", "running", json!({"parentId": "ultron-task-a"})),
        task("lost", "running", json!({"parentId": "ultron-task-missing"})),
    ]}})));
    let keys: HashSet<String> = all_keys(&cyclic).into_iter().collect();
    let want: HashSet<String> = [
        "turn",
        "task:ultron-task-a",
        "task:ultron-task-b",
        "task:ultron-task-lost",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(keys, want);
}

#[test]
fn interactive_rlm_layout_connectors_bars_joins_and_spend_at_80() {
    let graph = build_rlm_graph(&rich(|_| {}));
    let lines: Vec<String> = layout_graph(&graph, &CollapseState::new())
        .iter()
        .map(|r| strip(&render_graph_row(r, 80, &row_options())))
        .collect();
    assert_eq!(
        lines,
        [
            "⠋ turn  “Summarize the reviews and benchmark in the backgro… 1m12s 38k tok $0.04",
            "├─ ✓ [+1] earlier turns · 1 finished",
            "├─ ✓ [+1] cell 1  h = await rlm.load('reviews.csv')                         1.2s",
            "└─ ⠋ cell 2  vs = await rlm.map('Classify', h.chunks(4000), contract=str)    14s",
            "   ├─ ⠋ rlm.spawn child bbbb2222                                             13s",
            "   │  ├─ ✓ agent planner@1 cccc3333                                         3.0s",
            "   │  └─ ⠋ bash pytest -q                                                   5.0s",
            "   ├─ ⠋ rlm.map 99 frames ▰▰▰▰▰▰▰▰▱▱ 80/99 · 8 running · 3 incomple… 12s 35k tok",
            "   ├─ ⠋ workflow 12345678 3 nodes ▰▰▰▰▰▰▰▱▱▱ 2/3 · 1 running 1 join          11s",
            "   │  ├─ ✓ plan coder@1 w1                                                  3.0s",
            "   │  ├─ ✓ code coder@1 w2                                                  3.0s",
            "   │  └─ ⠋ review reviewer@1 w3 ⇐ plan+code (all)                           6.0s",
            "   └─ ⠋ bash sleep 30 && echo done                                          9.4s",
        ]
    );
    assert_eq!(lines.iter().filter(|l| visible_width(l) == 80).count(), 12);
}

#[test]
fn interactive_rlm_layout_collapses_finished_subtrees_unless_opened() {
    let graph = build_rlm_graph(&rich(|_| {}));
    let auto = layout_graph(&graph, &CollapseState::new());
    let cell = auto.iter().find(|r| r.node.key == "cell:call-1").unwrap();
    assert!(cell.collapsed);
    assert_eq!(cell.hidden, 1);
    assert!(!auto.iter().any(|r| r.node.key == "task:ultron-task-aaaa1111"));
    let open: CollapseState = HashMap::from([("cell:call-1".to_string(), false)]);
    let opened = layout_graph(&graph, &open);
    let child = opened
        .iter()
        .find(|r| r.node.key == "task:ultron-task-aaaa1111")
        .unwrap();
    assert_eq!(
        render_graph_row(child, 80, &row_options()),
        "│  └─ ✓ agent reviewer@1 aaaa1111 → LGTM with 2 nits        500ms 3.4k tok $0.01"
    );
    // A finished cell stays open while a job it started still runs.
    let live = rich(|w| {
        w["cells"] = json!([w["cells"][0].clone()]);
        w["agentsStatus"]["tasks"] = json!([]);
        w["frames"] = json!({"frames": []});
        w["agentsStatus"]["jobs"] = json!([{"id": "job-9", "status": "running", "command": "sleep 90", "exitCode": null, "startedAt": NOW - 59_000.0, "lane": "main"}]);
    });
    let live_graph = build_rlm_graph(&live);
    let rows: Vec<(String, bool)> = layout_graph(&live_graph, &CollapseState::new())
        .iter()
        .map(|r| (r.node.key.clone(), r.collapsed))
        .collect();
    assert_eq!(
        rows,
        [
            ("turn".to_string(), false),
            ("cell:call-1".to_string(), false),
            ("job:job-9".to_string(), false)
        ]
    );
    let folded = layout_graph(&graph, &HashMap::from([("cell:call-2".to_string(), true)]));
    let last = folded.last().unwrap();
    assert!(last.collapsed);
    assert_eq!(last.hidden, 9);
}

#[test]
fn interactive_rlm_layout_stays_readable_at_40_and_degrades() {
    let graph = build_rlm_graph(&rich(|_| {}));
    let rows = layout_graph(&graph, &CollapseState::new());
    let at40: Vec<String> = rows
        .iter()
        .map(|r| strip(&render_graph_row(r, 40, &row_options())))
        .collect();
    assert_eq!(at40[7], "   ├─ ⠋ rlm.map 99 frames ▰▰▰▰▱ 80/… 12s");
    assert_eq!(at40[11], "   │  └─ ⠋ review reviewer@1 w3 ⇐ … 6.0s");
    for width in [40, 20, 8] {
        for row in &rows {
            assert!(visible_width(&render_graph_row(row, width, &row_options())) <= width);
        }
    }
}

#[test]
fn interactive_rlm_bars_and_windows_are_bounded() {
    assert_eq!(bar(0.0, 5), "▱▱▱▱▱");
    assert_eq!(bar(0.01, 5), "▰▱▱▱▱");
    assert_eq!(bar(0.99, 5), "▰▰▰▰▱");
    assert_eq!(bar(1.0, 5), "▰▰▰▰▰");
    assert_eq!(window_rows(5, 10, 4), (0, 5));
    assert_eq!(window_rows(40, 10, 3), (0, 10));
    assert_eq!(window_rows(40, 10, 30), (22, 32));
    assert_eq!(window_rows(40, 10, 39), (30, 40));
}

#[test]
fn interactive_rlm_budget_gauges_and_their_packing() {
    let snap = rich(|w| {
        w["agentsStatus"]["usage"] = json!({"admittedTasks": 20, "remainingWallMs": 26.0 * 60_000.0, "usage": {"totalTokens": 38_200},
            "cost": {"spentUsd": 0.9, "maxCostUsd": 1}, "turns": {"turns": 3, "tokens": 150_000, "maxTotalTokens": 200_000, "maxTotalTurns": 10}});
    });
    let gauges = budget_gauges(&snap, &build_rlm_graph(&snap));
    let pairs: Vec<(&str, &str)> = gauges.iter().map(|g| (g.label.as_str(), g.text.as_str())).collect();
    assert_eq!(
        pairs,
        [
            ("tasks", "20/24"),
            ("wall", "4m/30m"),
            ("tokens", "150k/200k"),
            ("turns", "3/10"),
            ("cost", "$0.90/$1.00"),
            ("calls", "88/120"),
            ("depth", "1")
        ]
    );
    let marked = RlmStyle {
        fg: std::rc::Rc::new(|c, t| format!("<{}>{t}</>", c.token())),
        bold: std::rc::Rc::new(str::to_string),
    };
    let lines = render_gauges(&gauges, 80, &marked, 10);
    assert!(lines[0].contains("<dim>tasks</> <warning>▰▰▰▰▰▱</>"), "{lines:?}");
    assert!(lines.join(" ").contains("<dim>cost</> <error>▰▰▰▰▰▱</>"));
    assert_eq!(
        render_gauges(&gauges, 80, &plain(), 2),
        [
            "tasks ▰▰▰▰▰▱ 20/24  wall ▰▱▱▱▱▱ 4m/30m  tokens ▰▰▰▰▰▱ 150k/200k",
            "turns ▰▰▱▱▱▱ 3/10  cost ▰▰▰▰▰▱ $0.90/$1.00  calls ▰▰▰▰▱▱ 88/120  depth 1"
        ]
    );
    for line in render_gauges(&gauges, 40, &plain(), 2) {
        assert!(visible_width(&line) <= 40);
    }
}

#[test]
fn interactive_rlm_kernel_strip_live_busy_pinned_evicted_memory_and_guards() {
    let snap = rich(|_| {});
    assert_eq!(
        render_kernel_strip(&snap, 80, &plain()).unwrap(),
        "kernels ■■◆············· 3/16 · 2 busy · 1 pinned · 2 evicted · mem 212 MiB"
    );
    assert_eq!(
        strip(&render_kernel_strip(&snap, 40, &plain()).unwrap()),
        "kernels 3/16 · 2 busy · 1 pinned · 2 ev…"
    );
    assert!(render_kernel_strip(&rich(|w| w["pool"] = Value::Null), 80, &plain()).is_none());
    let guarded = rich(|w| {
        w["pool"]["guards"] = json!([
            {"name": "Loki", "checks": 4, "unchecked": 0, "afterChecks": 2, "afterFindings": 1, "blocked": 1, "turnMs": 85, "totalMs": 1430},
            {"name": "extension", "checks": 0, "unchecked": 0, "afterChecks": 0, "afterFindings": 1, "blocked": 0, "turnMs": 0, "totalMs": 0},
        ]);
    });
    assert_eq!(
        render_kernel_strip(&guarded, 120, &plain()).unwrap(),
        "kernels ■■◆············· 3/16 · 2 busy · 1 pinned · 2 evicted · mem 212 MiB · loki 85ms turn/1.4s 1 blocked"
    );
    let pool = parse_pool(&json!({"live": 1, "maxLive": 2, "lanes": [], "guards": [{"name": "Loki", "checks": 3, "turnMs": 12}, {"bogus": 1}]})).unwrap();
    let g = &pool.guards.unwrap()[0];
    assert_eq!(
        (g.name.as_str(), g.checks, g.blocked, g.turn_ms, g.total_ms),
        ("Loki", 3.0, 0.0, 12.0, 0.0)
    );
}

#[test]
fn interactive_rlm_footer_summary_while_running_nothing_when_idle() {
    let o = RlmViewOptions {
        focus_key: Some("alt+r".into()),
        ..RlmViewOptions::default()
    };
    assert_eq!(
        render_rlm_footer(&rich(|_| {}), 120, &o).unwrap(),
        "◆ rlm ⠋ turn 1m12s · cell 2 14s · map ▰▰▰▰▱ 80/99 · 6 tasks (2 active) · 2 jobs · $0.04 · alt+r graph"
    );
    assert!(visible_width(&render_rlm_footer(&rich(|_| {}), 40, &RlmViewOptions::default()).unwrap()) <= 40);
    let idle =
        RlmSnapshot::from_wire(&json!({"now": NOW, "agentsStatus": {"tasks": [task("x", "completed", json!({}))]}}));
    assert!(render_rlm_footer(&idle, 80, &RlmViewOptions::default()).is_none());
    let o = RlmViewOptions {
        focus_key: Some("alt+g".into()),
        ..RlmViewOptions::default()
    };
    assert_eq!(
        render_rlm_footer(&rich(|_| {}), 200, &o).unwrap(),
        "◆ rlm ⠋ turn 1m12s · cell 2 14s · map ▰▰▰▰▱ 80/99 · 6 tasks (2 active) · 2 jobs · $0.04 · alt+g graph"
    );
    for width in [80, 40, 20] {
        assert!(visible_width(&render_rlm_footer(&rich(|_| {}), width, &o).unwrap()) <= width);
    }
}

#[test]
fn interactive_rlm_docked_panel_pins_the_turn_and_windows_live_rows() {
    let o = RlmViewOptions {
        focus_key: Some("alt+r".into()),
        ..RlmViewOptions::default()
    };
    let lines: Vec<String> = render_rlm_dock(&rich(|_| {}), 80, &o, 8)
        .iter()
        .map(|l| strip(l))
        .collect();
    assert_eq!(
        lines,
        [
            "RLM ⠋ running 13 nodes · 7 active  alt+r focus",
            "tasks ▰▱▱▱▱▱ 5/24  wall ▰▱▱▱▱▱ 4m/30m  tokens 38k  cost $0.04",
            "calls ▰▰▰▰▱▱ 88/120  depth 1",
            "⠋ turn  “Summarize the reviews and benchmark in the backgro… 1m12s 38k tok $0.04",
            "   ↑ 5 more",
            "   │  └─ ⠋ bash pytest -q                                                   5.0s",
            "   ├─ ⠋ rlm.map 99 frames ▰▰▰▰▰▰▰▰▱▱ 80/99 · 8 running · 3 incomple… 12s 35k tok",
            "   ├─ ⠋ workflow 12345678 3 nodes ▰▰▰▰▰▰▰▱▱▱ 2/3 · 1 running 1 join          11s",
            "   │  ├─ ✓ plan coder@1 w1                                                  3.0s",
            "   │  ├─ ✓ code coder@1 w2                                                  3.0s",
            "   │  └─ ⠋ review reviewer@1 w3 ⇐ plan+code (all)                           6.0s",
            "   └─ ⠋ bash sleep 30 && echo done                                          9.4s",
            "kernels ■■◆············· 3/16 · 2 busy · 1 pinned · 2 evicted · mem 212 MiB",
        ]
    );
    for width in [80, 40, 20] {
        for line in render_rlm_dock(&rich(|_| {}), width, &RlmViewOptions::default(), 12) {
            assert!(visible_width(&line) <= width);
        }
    }
    let failed = rich(|w| {
        w["error"] = json!("Ultron RLM host is not initialized");
        w["cells"] = json!([]);
        w["agentsStatus"]["tasks"] = json!([]);
        w["frames"] = json!({"frames": []});
        w["agentsStatus"]["jobs"] = json!([]);
    });
    assert_eq!(
        render_rlm_dock(&failed, 80, &RlmViewOptions::default(), 12)[0],
        "RLM idle 0 nodes · inspection failed: Ultron RLM host is not initialized"
    );
    let with_context = rich(|w| {
        w["ctx"] = json!({"forgotten": [{"id": "e1", "kind": "toolResult", "preview": "big output", "reason": "done"}],
            "forgottenCount": 1, "collapsedCount": 0, "pinned": [], "pinnedCount": 0, "notes": 1});
    });
    assert!(
        render_rlm_dock(&with_context, 100, &RlmViewOptions::default(), 12)
            .contains(&"context: 1 forgotten · 0 collapsed · 0 pinned · 1 notes".to_string())
    );
}

#[test]
fn interactive_rlm_frame_traces_become_detail_lines() {
    let lines = frame_trace_details(
        &json!({"model": "p/m", "value": "\"positive\"", "views": [{}, {}], "attempts": [{}],
        "remaining": {"calls": 2, "tokens": null, "depth": 0}}),
    );
    let want: Vec<(String, String)> = [
        ("model", "p/m"),
        ("value", "\"positive\""),
        ("views", "2 context views"),
        ("attempts", "1"),
        ("remaining", "calls 2 · tokens ∞ · depth 0"),
    ]
    .iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    assert_eq!(lines, want);
    assert!(frame_trace_details(&json!("nope")).is_empty());
}

#[test]
fn interactive_rlm_focus_moves_with_clamping_and_keeps_the_selection_by_key() {
    let snap = rich(|_| {});
    let graph = build_rlm_graph(&snap);
    let layout = layout_graph(&graph, &CollapseState::new());
    let mut state = apply_focus_action(&FocusState::default(), &layout, FocusAction::Up, 10);
    assert_eq!(selected_row(&state, &layout), Some(0));
    state = apply_focus_action(&state, &layout, FocusAction::Down, 10);
    state = apply_focus_action(&state, &layout, FocusAction::Down, 10);
    assert_eq!(state.selected_key.as_deref(), Some("cell:call-1"));
    state = apply_focus_action(&state, &layout, FocusAction::PageDown, 100);
    assert_eq!(state.selected_key.as_deref(), Some("job:job-1"));
    let grown = rich(|w| {
        let mut jobs = w["agentsStatus"]["jobs"].as_array().unwrap().clone();
        jobs.push(json!({"id": "job-0", "status": "completed", "command": "ls", "exitCode": 0, "startedAt": NOW - 13_500.0, "endedAt": NOW - 13_400.0, "lane": "main"}));
        w["agentsStatus"]["jobs"] = Value::Array(jobs);
    });
    let grown_graph = build_rlm_graph(&grown);
    let grown_rows = layout_graph(&grown_graph, &CollapseState::new());
    assert_eq!(
        grown_rows[selected_row(&state, &grown_rows).unwrap()].node.key,
        "job:job-1"
    );
    state = apply_focus_action(&state, &layout, FocusAction::Home, 10);
    assert_eq!(state.selected_key.as_deref(), Some("turn"));
    let gone = FocusState {
        selected_key: Some("task:nope".into()),
        selected_index: 999,
        ..state
    };
    assert_eq!(selected_row(&gone, &layout), Some(layout.len() - 1));
}

#[test]
fn interactive_rlm_focus_details_and_collapse_on_subtrees_and_leaves() {
    let snap = rich(|_| {});
    let graph = build_rlm_graph(&snap);
    let mut state = FocusState {
        selected_key: Some("task:ultron-task-bbbb2222".into()),
        ..FocusState::default()
    };
    let layout = layout_graph(&graph, &state.collapse);
    state = apply_focus_action(&state, &layout, FocusAction::Details, 10);
    assert!(state.details.contains("task:ultron-task-bbbb2222"));
    state = apply_focus_action(&state, &layout, FocusAction::Collapse, 10);
    let layout = layout_graph(&graph, &state.collapse);
    let row = layout
        .iter()
        .find(|r| r.node.key == "task:ultron-task-bbbb2222")
        .unwrap();
    assert!(row.collapsed);
    assert_eq!(row.hidden, 2);
    state = apply_focus_action(&state, &layout, FocusAction::Collapse, 10);
    let layout = layout_graph(&graph, &state.collapse);
    let leaf = layout.iter().position(|r| r.node.key == "job:job-2").unwrap();
    state = apply_focus_action(
        &FocusState {
            selected_key: Some("job:job-2".into()),
            selected_index: leaf,
            ..state
        },
        &layout,
        FocusAction::Collapse,
        10,
    );
    assert_eq!(state.selected_key.as_deref(), Some("task:ultron-task-bbbb2222"));
    assert_eq!(state.collapse.get("task:ultron-task-bbbb2222"), Some(&true));
    state = apply_focus_action(&state, &layout, FocusAction::Details, 10);
    assert!(!state.details.contains("task:ultron-task-bbbb2222"));
}

#[test]
fn interactive_rlm_focus_view_fills_its_height_with_inline_details() {
    let snap = rich(|_| {});
    let state = FocusState {
        selected_key: Some("task:ultron-task-w3".into()),
        details: HashSet::from(["task:ultron-task-w3".to_string()]),
        ..FocusState::default()
    };
    let view = render_focus_view(
        &snap,
        &state,
        80,
        20,
        &FocusRenderOptions {
            hints: Some("up/k down/j move".into()),
            ..FocusRenderOptions::default()
        },
    );
    assert_eq!(view.lines.len(), 20);
    assert_eq!(view.lines[0], "RLM graph ⠋ running 13 nodes · 7 active");
    assert_eq!(view.lines[1], "up/k down/j move");
    let selected = view.lines.iter().find(|l| l.starts_with('▶')).unwrap();
    assert!(selected.contains("review reviewer@1 w3 ⇐ plan+code (all)"));
    let text = view.lines.join("\n");
    assert!(text.contains("┆ id        ultron-task-w3"));
    assert!(text.contains("┆ input     {\"diff\":\"...\"}"));
    assert!(text.contains("┆ workflow  wf-12345678 node review after plan, code (all)"));
    assert!(text.contains('↑'));
    for width in [80, 40] {
        let view = render_focus_view(&snap, &state, width, 12, &FocusRenderOptions::default());
        assert_eq!(view.lines.len(), 12);
        assert!(view.lines.iter().all(|l| visible_width(l) <= width));
    }
}

#[test]
fn interactive_rlm_focus_hints_follow_the_bindings() {
    let k = KeybindingsManager::new(
        app_keybindings(Platform::new("linux", false)),
        vec![
            ("app.rlm.graph.up".to_string(), vec!["up".to_string(), "k".to_string()]),
            (
                "app.rlm.graph.down".to_string(),
                vec!["down".to_string(), "j".to_string()],
            ),
            ("app.rlm.graph.details".to_string(), vec!["enter".to_string()]),
            ("app.rlm.graph.collapse".to_string(), vec!["c".to_string()]),
            ("app.rlm.graph.exit".to_string(), vec!["escape".to_string()]),
        ]
        .into_iter()
        .map(|(k, v)| (k, Some(v)))
        .collect(),
        Default::default(),
    );
    assert_eq!(
        interactive::rlm::focus_hints(&k),
        "up/k down/j move · enter details · c collapse · escape back"
    );
}

#[test]
fn interactive_rlm_focus_component_keys_exit_and_trace_loading() {
    let keys = keys();
    let snap = RlmSnapshot::from_wire(&json!({"now": NOW, "frames": {"frames": [
        {"id": "frame-aaaaaaaa1", "status": "complete", "task": "outer", "spent": {"calls": 1, "tokens": 10}, "startedAt": NOW - 50.0, "endedAt": NOW - 10.0, "taskId": "t1", "kind": "infer", "batch": 1}
    ]}}));
    let mut focus = RlmGraphFocus::new();
    assert_eq!(
        focus.handle_input(&keys, "\x1b[B", &snap, 20),
        interactive::rlm::focus::FocusEvent::Render
    );
    assert_eq!(focus.state.selected_key.as_deref(), Some("frames:frame-aaaaaaaa1"));
    assert_eq!(
        focus.handle_input(&keys, "\r", &snap, 20),
        interactive::rlm::focus::FocusEvent::LoadTrace {
            key: "frames:frame-aaaaaaaa1".into(),
            trace_id: "frame-aaaaaaaa1".into()
        }
    );
    focus.set_trace("frames:frame-aaaaaaaa1", Ok(&json!({"model": "p/m"})));
    let lines = focus.render(&keys, &snap, 80, 20, &plain(), 0);
    assert!(lines.iter().any(|l| l.contains("┆ model     p/m")), "{lines:?}");
    assert_eq!(
        focus.handle_input(&keys, "x", &snap, 20),
        interactive::rlm::focus::FocusEvent::Ignored
    );
    assert_eq!(
        focus.handle_input(&keys, "\x1b", &snap, 20),
        interactive::rlm::focus::FocusEvent::Exit
    );
}

#[test]
fn interactive_rlm_parses_payloads_defensively() {
    let parsed = parse_agents_status(&json!({
        "definitions": [],
        "tasks": [
            {"id": "t1", "definition": "a@1", "state": "running", "parentId": "p"},
            {"id": "t2", "definition": "b@1", "state": "completed", "lane": "ultron.b.t2", "startedAt": 5, "endedAt": 9,
                "cost": 0.01, "tokens": 120, "input": "{}", "fetch": "await agents.result(\"t2\")",
                "workflow": {"run": "wf-1", "node": "n", "dependsOn": ["x", 3], "join": "any"},
                "result": {"status": "succeeded", "preview": "ok"}},
            {"bogus": true},
        ],
        "usage": {"admittedTasks": 1, "limits": {"maxAdmittedTasks": 4, "maxWallMs": null}},
        "limits": null,
        "jobs": [{"id": "job-1", "status": "running", "command": "ls", "lane": "main", "tail": "a"}],
        "truncatedTasks": 3,
    }));
    assert_eq!(parsed.tasks.len(), 2);
    assert_eq!(parsed.tasks[0].parent_id.as_deref(), Some("p"));
    let t2 = &parsed.tasks[1];
    assert_eq!(t2.lane.as_deref(), Some("ultron.b.t2"));
    assert_eq!(
        (t2.started_at, t2.ended_at, t2.cost, t2.tokens),
        (Some(5.0), Some(9.0), Some(0.01), Some(120.0))
    );
    let w = t2.workflow.as_ref().unwrap();
    assert_eq!(
        (w.run.as_str(), w.node.as_str(), w.depends_on.clone(), w.join.as_str()),
        ("wf-1", "n", vec!["x".to_string()], "any")
    );
    assert_eq!(t2.result.as_ref().unwrap().preview.as_deref(), Some("ok"));
    assert_eq!(parsed.truncated_tasks, Some(3.0));
    assert_eq!(parsed.jobs[0].lane.as_deref(), Some("main"));
    assert_eq!(parsed.jobs[0].tail.as_deref(), Some("a"));
    let limits = parsed.limits.unwrap();
    assert_eq!((limits.max_admitted_tasks, limits.max_wall_ms), (Some(4.0), None));
    assert!(parse_agents_status(&Value::Null).tasks.is_empty());
    let child = &parse_agents_status(&json!({"tasks": [{"id": "c1", "definition": "rlm-child@1", "state": "completed", "worktree": "ultron/ab12/fix · merged"}]})).tasks[0];
    assert_eq!(child.worktree.as_deref(), Some("ultron/ab12/fix · merged"));
    let node = build_rlm_graph(&RlmSnapshot {
        tasks: vec![child.clone()],
        ..RlmSnapshot::default()
    });
    assert!(
        node.children[0]
            .details
            .contains(&("worktree".into(), "ultron/ab12/fix · merged".into()))
    );
    assert_eq!(
        parse_retained(&json!([
            {"task_id": "a", "state": "open", "invocations": [{"task_id": "b"}]},
            {"task_id": "c", "state": "closed", "invocations": []},
        ])),
        HashSet::from(["a".to_string(), "b".to_string()])
    );
    let pool = parse_pool(&json!({"live": 1, "maxLive": 2, "lanes": [{"lane": "main", "running": 1, "pinnedBy": ["x"], "memoryBytes": 5}], "memoryBytes": 5, "memoryCapBytes": 10})).unwrap();
    assert_eq!(pool.lanes[0].pinned_by, ["x"]);
    assert_eq!(
        (pool.memory_bytes, pool.memory_cap_bytes, pool.evictions),
        (Some(5.0), Some(10.0), None)
    );
    assert!(parse_pool(&json!("nope")).is_none());
    let frames = parse_frames(&json!({"frames": [
        {"id": "frame-1", "status": "running", "task": "t", "spent": {"calls": 1, "tokens": 2}, "taskId": null, "kind": "map", "batch": 3,
            "callerTaskId": null, "lane": null, "budget": {"id": "budget-1", "calls": 5, "tokens": null, "depth": 1}},
        {"id": 3},
    ]}));
    assert_eq!(frames.len(), 1);
    let f = &frames[0];
    assert_eq!(f.task_id, interactive::rlm::visualizer::Tri::Null);
    assert_eq!(
        (f.kind.as_deref(), f.batch, f.lane.as_deref()),
        (Some("map"), Some(3.0), None)
    );
    let b = f.budget.as_ref().unwrap();
    assert_eq!(
        (b.id.as_str(), b.calls, b.tokens, b.depth),
        ("budget-1", Some(5.0), None, 1.0)
    );
}

#[test]
fn interactive_rlm_clock_uses_admission_freezes_and_omits_unknown_starts() {
    let mut clock = RlmClock::new();
    let tasks =
        parse_agents_status(&json!({"tasks": [task("a", "running", json!({})), task("b", "completed", json!({}))]}))
            .tasks;
    let usage = interactive::rlm::visualizer::RlmUsage::from_json(
        &json!({"reservations": [{"taskId": "ultron-task-a", "admittedAt": 100}]}),
    );
    let first = clock.timings(&tasks, usage.as_ref(), 500.0);
    assert_eq!(first["ultron-task-a"].started_at, Some(100.0));
    assert_eq!(first["ultron-task-a"].ended_at, None);
    assert_eq!(first["ultron-task-b"], Default::default());
    let later_tasks = parse_agents_status(&json!({"tasks": [task("a", "completed", json!({}))]})).tasks;
    let later = clock.timings(&later_tasks, None, 900.0);
    assert_eq!(
        (later["ultron-task-a"].started_at, later["ultron-task-a"].ended_at),
        (Some(100.0), Some(900.0))
    );
}

#[test]
fn interactive_rlm_extracts_the_running_root_cell_and_the_turn() {
    let mut clock = RlmClock::new();
    let running = TranscriptView {
        transcript: Vec::new(),
        running_tools: Some(vec![RunningTool {
            status: "running".into(),
            tool_call_id: "c1".into(),
            tool_name: "rlm".into(),
            args: json!({"code": "x = 1"}),
            ..RunningTool::default()
        }]),
    };
    let cell = extract_root_cell(Some(&running), &mut clock, 10.0).unwrap();
    assert_eq!(
        (
            cell.tool_call_id.as_str(),
            cell.code.as_str(),
            cell.status,
            cell.started_at
        ),
        ("c1", "x = 1", CellStatus::Running, Some(10.0))
    );
    assert!(extract_root_cell(Some(&TranscriptView::default()), &mut clock, 0.0).is_none());
    // `extractTurn` from the TUI's messages.
    let messages = vec![
        json!({"role": "user", "content": "old question", "timestamp": 1}),
        json!({"role": "user", "content": [{"type": "text", "text": "count the rows"}], "timestamp": 100}),
        json!({"role": "assistant", "timestamp": 105, "content": [{"type": "toolCall", "id": "c1", "name": "rlm", "arguments": {"code": "print(1)"}}]}),
        json!({"role": "toolResult", "toolName": "rlm", "toolCallId": "c1", "isError": true, "timestamp": 109, "content": [{"type": "text", "text": "NameError"}]}),
        json!({"role": "assistant", "timestamp": 110, "content": [{"type": "toolCall", "id": "c2", "name": "rlm", "arguments": {"code": "print(2)"}}]}),
    ];
    let live = TranscriptView::from_messages(
        &messages,
        Some(vec![RunningTool {
            status: "running".into(),
            tool_call_id: "c2".into(),
            tool_name: "rlm".into(),
            args: json!({"code": "print(2)"}),
            ..RunningTool::default()
        }]),
    );
    let (turn, cells) = extract_turn(Some(&live), &mut RlmClock::new(), 120.0, 30);
    let turn = turn.unwrap();
    assert_eq!(
        (turn.started_at, turn.prompt.as_deref()),
        (Some(100.0), Some("count the rows"))
    );
    assert_eq!(cells.len(), 2);
    assert_eq!(
        (
            cells[0].status,
            cells[0].started_at,
            cells[0].ended_at,
            cells[0].output.as_deref()
        ),
        (CellStatus::Error, Some(105.0), Some(109.0), Some("NameError"))
    );
    assert_eq!(
        (cells[1].status, cells[1].started_at),
        (CellStatus::Running, Some(120.0))
    );
    let settled = TranscriptView::from_messages(&messages, None);
    let (_, cells) = extract_turn(Some(&settled), &mut RlmClock::new(), 130.0, 30);
    assert_eq!(
        (cells[1].status, cells[1].output.as_deref()),
        (CellStatus::Error, Some("interrupted"))
    );
    assert_eq!(extract_turn(None, &mut clock, 0.0, 30), (None, Vec::new()));
}

#[test]
fn interactive_rlm_formats_durations_compactly() {
    assert_eq!(format_duration(250.0), "250ms");
    assert_eq!(format_duration(4200.0), "4.2s");
    assert_eq!(format_duration(75_000.0), "1m15s");
    assert_eq!(format_duration(18.0 * 60_000.0), "18m");
    assert_eq!(format_duration(3.0 * 3_600_000.0 + 5.0 * 60_000.0), "3h5m");
}

// ---- ported from experimental-rlm-pane.test.ts ----

/// Three parallel workflow nodes and one node joining all three.
fn reference() -> RlmSnapshot {
    let t = |node: &str, id: &str, fields: Value| {
        let mut v = json!({"id": format!("ultron-task-{id}"), "definition": "rlm-child@1", "state": "running",
            "startedAt": NOW - 21_000.0, "lane": format!("ultron.rlm-child.{id}"),
            "workflow": {"run": "wf-01a07520", "node": node, "dependsOn": [], "join": "all"}});
        for (k, x) in fields.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    RlmSnapshot::from_wire(&json!({
        "now": NOW,
        "turn": {"startedAt": NOW - 30_000.0, "prompt": "Linear gates integration evidence"},
        "agentsStatus": {
            "tasks": [
                t("navigation", "aaaa1111", json!({"input": "{\"prompt\":\"Map shell navigation integration against HEAD, then list every route\"}",
                    "model": "cliproxyapi/gpt-6-astra", "turns": 1, "toolCallCount": 1,
                    "lastText": "I read this as fact collection - I'll map the navigation contract first"})),
                t("gate-wiring", "bbbb2222", json!({"input": "{\"prompt\":\"Collect the changed component props, imports, and gates\"}",
                    "model": "cliproxyapi/gpt-6-astra", "turns": 1, "toolCallCount": 1,
                    "lastText": "I read this as evidence collection - I'll compare the scoped wiring"})),
                t("test-coverage", "cccc3333", json!({"input": "{\"prompt\":\"Check test coverage\"}"})),
            ],
            "workflows": [{"run": "wf-01a07520", "startedAt": NOW - 21_000.0, "nodes": [
                {"id": "navigation", "definition": "rlm-child@1", "dependsOn": [], "join": "all"},
                {"id": "gate-wiring", "definition": "rlm-child@1", "dependsOn": [], "join": "all"},
                {"id": "test-coverage", "definition": "rlm-child@1", "dependsOn": [], "join": "all"},
                {"id": "verify-evidence", "definition": "rlm-child@1", "dependsOn": ["navigation", "gate-wiring", "test-coverage"], "join": "all"},
            ]}],
        },
    }))
}

const HINTS: [&str; 7] = [
    "Tab/n next",
    "Shift-Tab/p prev",
    "Space/Enter fold",
    "d Details",
    "↑↓ Scroll",
    "←→ Runs",
    "q/Esc Close",
];

fn workflow_runs(snapshot: &RlmSnapshot) -> Vec<interactive::rlm::DagRun> {
    build_dag_runs(snapshot)
        .into_iter()
        .filter(|r| r.kind == interactive::rlm::pane::RunKind::Workflow)
        .collect()
}

fn pane(width: usize, state: &RlmPaneState, height: usize, snapshot: &RlmSnapshot) -> (String, RlmPaneState) {
    let runs = workflow_runs(snapshot);
    let rendered = render_rlm_pane(
        &runs,
        state,
        width,
        height,
        &DagRenderOptions {
            hints: HINTS.iter().map(|s| s.to_string()).collect(),
            ..DagRenderOptions::new(NOW)
        },
    );
    (strip(&rendered.lines.join("\n")), rendered.state)
}

#[test]
fn interactive_rlm_pane_work_runs_live_or_from_the_running_turn() {
    let live = reference();
    let keys_of = |runs: Vec<&interactive::rlm::DagRun>| runs.iter().map(|r| r.key.clone()).collect::<Vec<_>>();
    assert_eq!(
        keys_of(pane_work_runs(&build_dag_runs(&live), None)),
        ["turn", "workflow:wf-01a07520"]
    );
    let mut finished = live.clone();
    for t in &mut finished.tasks {
        t.state = "completed".into();
    }
    for plan in &mut finished.workflows {
        for node in &mut plan.nodes {
            node.status = Some("succeeded".into());
        }
    }
    let runs = build_dag_runs(&finished);
    assert_eq!(
        runs.iter().map(|r| r.status).collect::<Vec<_>>(),
        [GraphStatus::Done, GraphStatus::Done]
    );
    assert!(pane_work_runs(&runs, None).is_empty());
    assert_eq!(
        keys_of(pane_work_runs(&runs, Some(NOW - 30_000.0))),
        ["turn", "workflow:wf-01a07520"]
    );
    assert_eq!(keys_of(pane_work_runs(&runs, Some(NOW))), ["turn"]);
    let mut bare = reference();
    bare.tasks.clear();
    bare.workflows.clear();
    assert!(pane_work_runs(&build_dag_runs(&bare), Some(NOW - 30_000.0)).is_empty());
}

#[test]
fn interactive_rlm_pane_waves_are_topological_levels_cycles_cut() {
    let n = |k: &str, d: &[&str]| (k.to_string(), d.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(
        compute_waves(&[
            n("a", &[]),
            n("b", &["a"]),
            n("c", &["a"]),
            n("d", &["b", "c", "missing"])
        ]),
        [vec!["a"], vec!["b", "c"], vec!["d"]]
    );
    let mut cyclic: Vec<String> = compute_waves(&[n("x", &["y"]), n("y", &["x"])])
        .into_iter()
        .flatten()
        .collect();
    cyclic.sort();
    assert_eq!(cyclic, ["x", "y"]);
}

#[test]
fn interactive_rlm_pane_three_parallel_nodes_and_one_join() {
    let runs = workflow_runs(&reference());
    assert_eq!(
        runs[0].waves,
        [
            vec![
                "wf:wf-01a07520:navigation",
                "wf:wf-01a07520:gate-wiring",
                "wf:wf-01a07520:test-coverage"
            ],
            vec!["wf:wf-01a07520:verify-evidence"]
        ]
    );
    let (text, _) = pane(80, &RlmPaneState::default(), 70, &reference());
    assert!(text.contains("RLM · 01a07520"));
    assert!(text.contains("RLM pane / workflow  Tasks (0)"));
    assert!(text.contains("Linear gates integration evidence  1/1"));
    assert!(text.contains("Running · Done 0/4 · 3 running · wave 1/2"));
    let re = |p: &str| fancy_regex::Regex::new(p).unwrap().is_match(&text).unwrap();
    assert!(re(
        r"│ > \[-\] navigation +│ {2}│ {3}\[-\] gate-wiring +│ {2}│ {3}\[-\] test-coverage +│"
    ));
    assert!(re(r"│ ● Running {2}21s +│ {2}│ ● Running {2}21s"));
    assert!(text.contains("│ Start node"));
    assert!(text.contains("  · Same frontier"));
    assert!(re(r"\n +│\n +▼\n"));
    assert!(re(r"│ {3}\[-\] verify-evidence +│"));
    assert!(text.contains("│ ○ Pending"));
    assert!(text.contains("│ ← navigation, gate-wiring, test-coverage │"));
    let (narrow, _) = pane(44, &RlmPaneState::default(), 70, &reference());
    assert!(narrow.contains("│ ← navigation, gate-wiring, test-coverage │"));
}

#[test]
fn interactive_rlm_pane_waves_wrap_and_fit_every_width() {
    assert_eq!(wave_geometry(3, 43, 0), (2, 20));
    assert_eq!(wave_geometry(3, 60, 0), (2, 29));
    assert_eq!(wave_geometry(3, 80, 0), (3, 25));
    assert_eq!(wave_geometry(1, 80, 0), (1, 30));
    assert_eq!(wave_geometry(1, 80, 45), (1, 45));
    assert_eq!(wave_geometry(1, 43, 60), (1, 43));
    for width in [44, 60, 80] {
        let runs = workflow_runs(&reference());
        let r = render_rlm_pane(&runs, &RlmPaneState::default(), width, 40, &DagRenderOptions::new(NOW));
        assert_eq!(r.lines.len(), 40);
        assert!(r.lines.iter().all(|l| visible_width(l) <= width));
    }
    let (text, _) = pane(44, &RlmPaneState::default(), 70, &reference());
    let lines: Vec<&str> = text.split('\n').collect();
    let first = lines
        .iter()
        .position(|l| l.contains("navigation") && l.contains("gate-wiring"))
        .unwrap();
    assert!(first > 0);
    assert!(!lines[first].contains("test-cov"));
    let second = lines.iter().position(|l| l.contains("test-cover")).unwrap();
    assert_eq!(second, first + 6);
    assert!(lines[second + 4].contains("· Same frontier"));
}

#[test]
fn interactive_rlm_pane_cuts_long_names_and_texts() {
    let mut long = reference();
    long.workflows[0].nodes = vec![interactive::rlm::visualizer::RlmWorkflowNode {
        id: "a-very-long-node-name-that-cannot-fit-in-any-box".into(),
        definition: "x@1".into(),
        join: "all".into(),
        ..Default::default()
    }];
    long.tasks.clear();
    let (text, _) = pane(44, &RlmPaneState::default(), 40, &long);
    assert!(text.contains("│ > [-] a-very-long-node-name-that-cannot… │"), "{text}");
    let (cards, _) = pane(44, &RlmPaneState::default(), 70, &reference());
    assert!(cards.contains("│ > ● Map shell navigation integration ag… │"));
    assert!(cards.contains("│ I read this as fact collection - I'll m… │"));
}

#[test]
fn interactive_rlm_pane_highlights_the_selection_and_lists_dependencies() {
    let runs = workflow_runs(&reference());
    let marked = RlmStyle {
        fg: std::rc::Rc::new(|c, t| {
            if c.token() == "accent" {
                format!("\x1b[35m{t}\x1b[39m")
            } else {
                t.to_string()
            }
        }),
        bold: std::rc::Rc::new(str::to_string),
    };
    let rendered = render_rlm_pane(
        &runs,
        &RlmPaneState::default(),
        60,
        70,
        &DagRenderOptions {
            style: marked,
            ..DagRenderOptions::new(NOW)
        },
    );
    let text = rendered.lines.join("\n");
    let run = runs.iter().find(|r| Some(&r.key) == rendered.run_key.as_ref()).unwrap();
    assert_eq!(
        selected_node(run, &rendered.state).as_deref(),
        Some("wf:wf-01a07520:navigation")
    );
    let accent = |t: &str| format!("\x1b[35m{t}\x1b[39m");
    assert!(text.contains(&format!("{} {} [-] navigation", accent("│"), accent(">"))));
    assert!(text.contains("\x1b[35m┌───"));
    assert!(text.contains(&format!(
        "{} {} {} \x1b[35mMap shell navigation integration",
        accent("│"),
        accent(">"),
        accent("●")
    )));
    assert!(text.contains("│   [-] gate-wiring"));
    let plain_text = strip(&text);
    assert!(plain_text.contains("│ ● Collect the changed component props, imports, and gat… │"));
    assert!(plain_text.contains(
        "Dependencies\n  navigation → verify-evidence\n  gate-wiring → verify-evidence\n  test-coverage → verify-evidence"
    ));
    assert!(plain_text.contains("Node details"));
    assert!(plain_text.contains("│ navigation · rlm-child@1 · cliproxyapi/gpt-6-astra"));
    assert!(plain_text.contains("│ 21s · 1 turn · 1 tool"));
    assert!(plain_text.contains("│ waiting 21s"));
}

#[test]
fn interactive_rlm_pane_footer_connection_position_and_hints() {
    let (text, _) = pane(44, &RlmPaneState::default(), 30, &reference());
    let lines: Vec<&str> = text.split('\n').collect();
    assert_eq!(lines.len(), 30);
    let status = lines.iter().position(|l| l.starts_with("● Connected")).unwrap();
    assert!(
        fancy_regex::Regex::new(r"^● Connected {2}1-\d+/\d+$")
            .unwrap()
            .is_match(lines[status])
            .unwrap()
    );
    assert!(lines[status - 1].chars().all(|c| c == '─'));
    assert_eq!(lines[status + 1..].join("  "), HINTS.join("  "));
    let runs = workflow_runs(&reference());
    let failed = render_rlm_pane(
        &runs,
        &RlmPaneState::default(),
        44,
        30,
        &DagRenderOptions {
            error: Some("worker closed".into()),
            ..DagRenderOptions::new(NOW)
        },
    );
    assert!(strip(&failed.lines.join("\n")).contains("● Inspection failed: worker"));
    let empty = render_rlm_pane(&[], &RlmPaneState::default(), 44, 20, &DagRenderOptions::new(NOW));
    assert!(strip(&empty.lines.join("\n")).contains("No RLM work in this turn yet."));
    for (w, h) in [(12, 6), (44, 8)] {
        let r = render_rlm_pane(&runs, &RlmPaneState::default(), w, h, &DagRenderOptions::new(NOW));
        assert_eq!(r.lines.len(), h);
        assert!(r.lines.iter().all(|l| visible_width(l) <= w));
    }
}

#[test]
fn interactive_rlm_pane_navigation_wraps_folds_and_toggles_details() {
    let snap = reference();
    let runs = workflow_runs(&snap);
    let (_, mut state) = pane(60, &RlmPaneState::default(), 70, &snap);
    assert_eq!(
        selected_node(&runs[0], &state).as_deref(),
        Some("wf:wf-01a07520:navigation")
    );
    state = apply_dag_action(&state, &runs, DagAction::Next, 10);
    assert_eq!(state.selected_key.as_deref(), Some("wf:wf-01a07520:gate-wiring"));
    state = apply_dag_action(
        &apply_dag_action(&state, &runs, DagAction::Next, 10),
        &runs,
        DagAction::Next,
        10,
    );
    assert_eq!(state.selected_key.as_deref(), Some("wf:wf-01a07520:verify-evidence"));
    state = apply_dag_action(&state, &runs, DagAction::Next, 10);
    assert_eq!(state.selected_key.as_deref(), Some("wf:wf-01a07520:navigation"));
    state = apply_dag_action(&state, &runs, DagAction::Prev, 10);
    assert_eq!(state.selected_key.as_deref(), Some("wf:wf-01a07520:verify-evidence"));
    state = apply_dag_action(&state, &runs, DagAction::Fold, 10);
    let (text, _) = pane(60, &state, 70, &snap);
    assert!(
        fancy_regex::Regex::new(r"│ > \[\+\] verify-evidence ○ +│")
            .unwrap()
            .is_match(&text)
            .unwrap()
    );
    assert!(!text.contains("← navigation, gate-wiring"));
    state = apply_dag_action(&state, &runs, DagAction::Fold, 10);
    assert!(pane(60, &state, 70, &snap).0.contains("← navigation, gate-wiring"));
    state = apply_dag_action(&state, &runs, DagAction::Details, 10);
    let (text, _) = pane(60, &state, 70, &snap);
    assert!(!text.contains("Node details"));
    assert!(text.contains("Dependencies"));
}

#[test]
fn interactive_rlm_pane_scrolls_keeps_selection_in_view_and_switches_runs() {
    let mut snap = reference();
    snap.workflows.push(interactive::rlm::visualizer::RlmWorkflowRun {
        run: "wf-22222222".into(),
        started_at: Some(NOW - 5000.0),
        nodes: vec![interactive::rlm::visualizer::RlmWorkflowNode {
            id: "solo".into(),
            definition: "x@1".into(),
            join: "all".into(),
            ..Default::default()
        }],
        ..Default::default()
    });
    let runs = workflow_runs(&snap);
    let render = |state: &RlmPaneState| {
        let r = render_rlm_pane(
            &runs,
            state,
            44,
            24,
            &DagRenderOptions {
                hints: HINTS.iter().map(|s| s.to_string()).collect(),
                ..DagRenderOptions::new(NOW)
            },
        );
        (strip(&r.lines.join("\n")), r.state)
    };
    let (text, state) = render(&RlmPaneState::default());
    assert!(text.contains("RLM · 22222222"));
    assert!(text.contains("2/2"));
    let state = apply_dag_action(&state, &runs, DagAction::PrevRun, 10);
    let (text, state) = render(&state);
    assert!(text.contains("RLM · 01a07520"));
    let state = apply_dag_action(
        &apply_dag_action(&state, &runs, DagAction::ScrollDown, 10),
        &runs,
        DagAction::ScrollDown,
        10,
    );
    let (text, state) = render(&state);
    assert!(
        fancy_regex::Regex::new(r"● Connected {2}3-\d+/\d+")
            .unwrap()
            .is_match(&text)
            .unwrap()
    );
    let mut state = apply_dag_action(&state, &runs, DagAction::ScrollUp, 10);
    assert_eq!(state.scroll, 1);
    for _ in 0..3 {
        state = apply_dag_action(&state, &runs, DagAction::Next, 10);
    }
    let (text, state) = render(&state);
    assert!(text.contains("> [-] verify-evidence"));
    let state = apply_dag_action(&state, &runs, DagAction::NextRun, 10);
    assert!(render(&state).0.contains("RLM · 22222222"));
}

#[test]
fn interactive_rlm_pane_hints_follow_the_bindings() {
    assert_eq!(rlm_pane_hints(&keys()), HINTS);
}

#[test]
fn interactive_rlm_turn_runs_cells_frontiers_tool_nodes_and_nesting() {
    let snap = RlmSnapshot::from_wire(&json!({
        "now": NOW,
        "cells": [
            {"toolCallId": "call-1", "code": "x = 1", "status": "ok", "startedAt": NOW - 9500.0, "endedAt": NOW - 8000.0},
            {"toolCallId": "call-2", "code": "x = 1", "status": "running", "startedAt": NOW - 3000.0},
        ],
        "turn": {"startedAt": NOW - 10_000.0, "prompt": "compare the drafts"},
        "agentsStatus": {
            "tasks": [
                {"id": "ultron-task-c1", "definition": "rlm-child@1", "state": "running", "startedAt": NOW - 9000.0, "input": "{\"prompt\":\"one\"}"},
                {"id": "ultron-task-c2", "definition": "rlm-child@1", "state": "completed", "startedAt": NOW - 9000.0, "endedAt": NOW - 4000.0, "result": {"status": "succeeded", "preview": "two done"}},
                {"id": "ultron-task-t3", "definition": "reviewer@1", "state": "admitted", "startedAt": NOW - 8900.0},
                {"id": "ultron-task-n1", "definition": "rlm-child@1", "state": "running", "parentId": "ultron-task-c1", "startedAt": NOW - 5000.0},
            ],
            "toolCalls": [
                {"id": "tc1", "name": "web_search", "label": "web_search foo", "status": "completed", "source": "repl", "startedAt": NOW - 8800.0, "endedAt": NOW - 8700.0},
                {"id": "tc2", "name": "web_fetch", "label": "web_fetch bar", "status": "running", "source": "repl", "startedAt": NOW - 8600.0},
            ],
        },
    }));
    let runs = build_dag_runs(&snap);
    let run = &runs[0];
    assert_eq!(run.kind, interactive::rlm::pane::RunKind::Turn);
    assert_eq!(
        run.waves,
        [
            vec![
                "task:ultron-task-c1",
                "task:ultron-task-c2",
                "task:ultron-task-t3",
                "tools:cell:call-1"
            ],
            vec!["task:ultron-task-n1"]
        ]
    );
    assert_eq!(
        run.edges,
        [("task:ultron-task-c1".to_string(), "task:ultron-task-n1".to_string())]
    );
    let text = strip(
        &render_rlm_pane(
            &runs[..1],
            &RlmPaneState::default(),
            80,
            80,
            &DagRenderOptions::new(NOW),
        )
        .lines
        .join("\n"),
    );
    for part in [
        "RLM pane / turn",
        "[-] child c1",
        "[-] 2 tool calls",
        "│ ▰▰▰▰▰▱▱▱▱▱ 1/2",
        "  · Same frontier",
        "│ cell 1",
        "← child c1",
        "  child c1 → child n1",
        "│ → two done",
    ] {
        assert!(text.contains(part), "missing {part:?}\n{text}");
    }
}

#[test]
fn interactive_rlm_prompts_from_bounded_input_previews() {
    assert_eq!(
        prompt_of(Some("{\"prompt\":\"Summarize the file\"}")).as_deref(),
        Some("Summarize the file")
    );
    assert_eq!(
        prompt_of(Some("{\"prompt\":\"Summarize the \\\"big\\\" fi")).as_deref(),
        Some("Summarize the \"big\" fi")
    );
    assert_eq!(prompt_of(Some("{\"n\":2}")).as_deref(), Some("{\"n\":2}"));
    assert_eq!(prompt_of(None), None);
}

#[test]
fn interactive_rlm_wave_summary_active_run_bounded() {
    let runs = build_dag_runs(&reference());
    let lines = render_wave_summary(&runs, 100, &plain(), NOW, 4);
    let text: Vec<String> = lines.iter().map(|l| strip(l)).collect();
    assert_eq!(
        text[0],
        "▶ Linear gates integration evidence running wave 1/2  0/4 done, 3 running"
    );
    assert!(
        fancy_regex::Regex::new(
            r"^ {2}▶ navigation · category:rlm-child@1 · I read this as fact collection - I'll map the .*… · 21s$"
        )
        .unwrap()
        .is_match(&text[1])
        .unwrap()
    );
    assert_eq!(text[3], "  ▶ test-coverage · category:rlm-child@1 · 21s");
    assert_eq!(text[4], "  ○ verify-evidence · category:rlm-child@1 · waiting 21s");
    assert!(lines.iter().all(|l| visible_width(l) <= 100));
    let bounded: Vec<String> = render_wave_summary(&runs, 60, &plain(), NOW, 3)
        .iter()
        .map(|l| strip(l))
        .collect();
    assert_eq!(bounded.len(), 4);
    assert_eq!(bounded[3], "  +2 more");
    assert_eq!(
        bounded[0],
        "▶ Linear gates integr… running wave 1/2  0/4 done, 3 running"
    );
    let mut done = reference();
    for t in &mut done.tasks {
        t.state = "completed".into();
        t.ended_at = Some(NOW);
    }
    done.workflows.clear();
    assert!(render_wave_summary(&build_dag_runs(&done), 80, &plain(), NOW, 4).is_empty());
    assert!(render_wave_summary(&[], 80, &plain(), NOW, 4).is_empty());
}

#[test]
fn interactive_rlm_pane_component_keys_and_render() {
    let keys = keys();
    let snap = reference();
    let mut pane = interactive::rlm::RlmPane::new();
    let first = pane.render(&keys, &snap, 50, 30, false, Some("alt+w"), &plain());
    assert_eq!(first.len(), 30);
    assert!(first.iter().all(|l| l.starts_with(' ')));
    assert!(first.iter().any(|l| l.contains("alt+w focus")));
    assert_eq!(
        pane.handle_input(&keys, "n", &snap, 30),
        interactive::rlm::PaneEvent::Render
    );
    let moved = pane.render(&keys, &snap, 50, 30, true, Some("alt+w"), &plain());
    assert_ne!(moved, first);
    assert!(moved.iter().any(|l| l.contains("alt+w chat")));
    assert_eq!(
        pane.handle_input(&keys, "z", &snap, 30),
        interactive::rlm::PaneEvent::Ignored
    );
    assert_eq!(
        pane.handle_input(&keys, "q", &snap, 30),
        interactive::rlm::PaneEvent::Close
    );
}

// ---- added for mutations that survived the ported tests (values recorded from the TypeScript) ----

#[test]
fn interactive_rlm_waves_cut_a_cycle_where_it_is_entered() {
    let n = |k: &str, d: &[&str]| (k.to_string(), d.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(
        compute_waves(&[n("x", &["y"]), n("y", &["x"]), n("z", &["x"])]),
        [vec!["y"], vec!["x"], vec!["z"]]
    );
    // The cut edge counts as level 0, so the cycle shares a wave with acyclic nodes at the same depth.
    assert_eq!(
        compute_waves(&[n("w", &[]), n("v", &["w"]), n("x", &["y"]), n("y", &["x"])]),
        [vec!["w"], vec!["v", "y"], vec!["x"]]
    );
}

#[test]
fn interactive_rlm_clock_freezes_a_cancelled_task() {
    let mut clock = RlmClock::new();
    let running = parse_agents_status(&json!({"tasks": [task("a", "running", json!({}))]})).tasks;
    let usage = interactive::rlm::visualizer::RlmUsage::from_json(
        &json!({"reservations": [{"taskId": "ultron-task-a", "admittedAt": 100}]}),
    );
    clock.timings(&running, usage.as_ref(), 500.0);
    let cancelled = parse_agents_status(&json!({"tasks": [task("a", "cancelled", json!({}))]})).tasks;
    let later = clock.timings(&cancelled, None, 900.0);
    assert_eq!(
        (later["ultron-task-a"].started_at, later["ultron-task-a"].ended_at),
        (Some(100.0), Some(900.0))
    );
}

#[test]
fn interactive_rlm_extract_turn_keeps_the_newest_cells() {
    let mut messages = vec![json!({"role": "user", "content": "q", "timestamp": 1})];
    for i in 0..4 {
        messages.push(json!({"role": "assistant", "timestamp": 10 + i * 2, "content": [{"type": "toolCall", "id": format!("c{i}"), "name": "rlm", "arguments": {"code": format!("x{i}")}}]}));
        messages.push(json!({"role": "toolResult", "toolName": "rlm", "toolCallId": format!("c{i}"), "isError": false, "timestamp": 11 + i * 2, "content": [{"type": "text", "text": format!("o{i}")}]}));
    }
    let view = TranscriptView::from_messages(&messages, None);
    let (turn, cells) = extract_turn(Some(&view), &mut RlmClock::new(), 100.0, 2);
    assert_eq!(turn.unwrap().started_at, Some(1.0));
    assert_eq!(
        Value::Array(cells.iter().map(cell_json).collect()),
        json!([
            {"toolCallId": "c2", "code": "x2", "status": "ok", "startedAt": 14, "endedAt": 15, "output": "o2"},
            {"toolCallId": "c3", "code": "x3", "status": "ok", "startedAt": 16, "endedAt": 17, "output": "o3"},
        ])
    );
}
