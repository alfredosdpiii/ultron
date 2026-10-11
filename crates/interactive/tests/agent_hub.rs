//! Agent hub: replays fixtures/omp-agent-hub-parity.json (omp projectAgentTree, compareRosterAgents, treePrefix,
//! taskSummary, formatCost, formatAge and the roster window arithmetic, recorded by
//! fixtures/generate-omp-agent-hub.mjs), then drives the hub component over a plain view model.

use std::collections::HashMap;
use std::rc::Rc;

use interactive::agent_hub::{
    AgentHub, AgentKind, AgentMetrics, AgentStatus, AgentView, HubEvent, RosterOrder, ViewMode, compare_roster_agents,
    format_age, format_cost, project_agent_tree, roster_window, running_subagent_ids, task_summary, tree_prefix,
};
use interactive::chat::ChatStyle;
use interactive::copy_targets::TranscriptEntry;
use serde_json::{Value, json};
use tui::keybindings::{KeybindingsManager, Platform};
use tui::qol_keys::native_app_keybindings;
use tui::theme::{ColorMode, Theme, builtin_theme_json, create_theme};
use tui::utils::strip_terminal_sequences;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/omp-agent-hub-parity.json")).unwrap()
}

fn status(s: &str) -> AgentStatus {
    match s {
        "running" => AgentStatus::Running,
        "idle" => AgentStatus::Idle,
        "parked" => AgentStatus::Parked,
        _ => AgentStatus::Aborted,
    }
}

fn agent(id: &str, st: AgentStatus, last: i64, parent: Option<&str>) -> AgentView {
    AgentView {
        id: id.into(),
        kind: AgentKind::Sub,
        parent_id: parent.map(str::to_string),
        status: st,
        unread: 0,
        task: None,
        last_activity: last,
        model: None,
        thinking: None,
        metrics: None,
    }
}

fn roster(v: &Value) -> Vec<AgentView> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|r| {
            agent(
                r["id"].as_str().unwrap(),
                status(r["status"].as_str().unwrap()),
                r["lastActivity"].as_i64().unwrap(),
                r["parentId"].as_str(),
            )
        })
        .collect()
}

#[test]
fn interactive_agent_hub_roster_order_and_tree_match_omp() {
    let f = fixture();
    let rosters: Vec<Vec<AgentView>> = f["rosters"].as_array().unwrap().iter().map(roster).collect();
    for (refs, want) in rosters.iter().zip(f["sorted"].as_array().unwrap()) {
        let mut sorted = refs.clone();
        sorted.sort_by(compare_roster_agents);
        let ids: Vec<&str> = sorted.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            want.as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>()
        );
    }
    for case in f["trees"].as_array().unwrap() {
        let ids: Vec<&str> = case["ids"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let refs = rosters
            .iter()
            .find(|r| r.iter().map(|a| a.id.as_str()).eq(ids.iter().copied()))
            .unwrap();
        let ranks: Option<HashMap<String, i64>> = case["ranks"].as_array().map(|r| {
            ids.iter()
                .zip(r)
                .map(|(id, v)| (id.to_string(), v.as_i64().unwrap()))
                .collect()
        });
        let rank_fn = |id: &str| ranks.as_ref().and_then(|r| r.get(id).copied());
        let tree = project_agent_tree(refs, ranks.as_ref().map(|_| &rank_fn as &dyn Fn(&str) -> Option<i64>));
        let rows: Vec<&str> = tree.rows.iter().map(|&i| refs[i].id.as_str()).collect();
        assert_eq!(
            rows,
            case["rows"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect::<Vec<_>>(),
            "{case}"
        );
        for (id, d) in case["depth"].as_object().unwrap() {
            assert_eq!(tree.depth[id] as u64, d.as_u64().unwrap(), "{id}");
        }
        for (id, p) in case["parent"].as_object().unwrap() {
            assert_eq!(tree.parent[id], p.as_str().unwrap(), "{id}");
        }
        for (id, l) in case["last"].as_object().unwrap() {
            assert_eq!(tree.last_sibling[id], l.as_bool().unwrap(), "{id}");
        }
        let mut k = 0;
        for width in [3, 6, 9, 40] {
            for continuation in [false, true] {
                let got: Vec<String> = rows
                    .iter()
                    .map(|id| tree_prefix(id, width, &tree, continuation))
                    .collect();
                let want: Vec<&str> = case["prefixes"][k]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect();
                assert_eq!(got, want, "{case} {width} {continuation}");
                k += 1;
            }
        }
    }
}

#[test]
fn interactive_agent_hub_formatting_and_window_match_omp() {
    let f = fixture();
    for c in f["tasks"].as_array().unwrap() {
        assert_eq!(task_summary(c[0].as_str().unwrap()), c[1].as_str().unwrap());
    }
    for c in f["costs"].as_array().unwrap() {
        assert_eq!(format_cost(c[0].as_f64().unwrap()), c[1].as_str().unwrap());
    }
    for c in f["ages"].as_array().unwrap() {
        assert_eq!(format_age(c[0].as_i64().unwrap()), c[1].as_str().unwrap());
    }
    for c in f["windows"].as_array().unwrap() {
        let heights: Vec<usize> = c["heights"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h.as_u64().unwrap() as usize)
            .collect();
        let selected = c["selected"].as_u64().unwrap() as usize;
        let budget = c["budget"].as_u64().unwrap() as usize;
        let w = roster_window(&heights, selected, budget);
        let mut lines = Vec::new();
        let mut hits = Vec::new();
        let entry = |i: usize| (0..heights[i]).map(move |k| format!("{i}.{k}"));
        if w.clipped {
            lines.extend(entry(selected).take(budget));
            hits.extend(std::iter::repeat_n(json!(selected), lines.len()));
        } else {
            if w.top {
                lines.push(format!("… {} more", w.start));
                hits.push(Value::Null);
            }
            for i in w.start..w.end {
                for l in entry(i) {
                    lines.push(l);
                    hits.push(json!(i));
                }
            }
            if w.bottom {
                lines.push(format!("… {} more", heights.len() - w.end));
                hits.push(Value::Null);
            }
        }
        assert_eq!(json!(lines), c["lines"], "{c}");
        assert_eq!(Value::Array(hits), c["hits"], "{c}");
    }
}

#[test]
fn interactive_agent_hub_new_agents_rank_above_the_captured_order() {
    let mut order = RosterOrder::default();
    let first = order.order(
        vec![
            agent("a", AgentStatus::Idle, 1, None),
            agent("b", AgentStatus::Running, 1, None),
        ],
        true,
    );
    assert_eq!(first.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["b", "a"]);
    // A heartbeat that would re-sort (a becomes running and newest) keeps the captured order.
    let again = order.order(
        vec![
            agent("a", AgentStatus::Running, 9, None),
            agent("b", AgentStatus::Idle, 1, None),
            agent("c", AgentStatus::Idle, 5, None),
            agent("d", AgentStatus::Running, 2, None),
        ],
        true,
    );
    assert_eq!(
        again.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        ["d", "c", "b", "a"]
    );
    // Unsettled discovery does not freeze a partial roster.
    let mut loading = RosterOrder::default();
    loading.order(vec![agent("z", AgentStatus::Idle, 1, None)], false);
    assert_eq!(loading.rank("z"), None);
}

fn theme() -> Rc<Theme> {
    Rc::new(create_theme(&builtin_theme_json("dark").unwrap(), ColorMode::Truecolor, None).unwrap())
}

fn hub() -> AgentHub {
    let t = theme();
    let style_theme = Rc::clone(&t);
    let keys = KeybindingsManager::new(
        native_app_keybindings(Platform::new("linux", false)),
        Vec::new(),
        Default::default(),
    );
    let mut hub = AgentHub::new(
        t,
        keys,
        Rc::new(move || ChatStyle {
            theme: Rc::clone(&style_theme),
            highlight: Rc::new(|c: &str| c.lines().map(str::to_string).collect()),
            expand_key: "ctrl+o".into(),
            dequeue_key: "Alt+Up".into(),
            interrupt_key: "escape".into(),
            code_highlight: None,
        }),
    );
    let mut main = agent("Main", AgentStatus::Running, 0, None);
    main.kind = AgentKind::Main;
    let mut scout = agent("scout", AgentStatus::Running, 50_000, Some("Main"));
    scout.task = Some("Complete assignment thoroughly: map the parser".into());
    scout.unread = 2;
    scout.model = Some("claude-sonnet".into());
    scout.thinking = Some("high".into());
    scout.metrics = Some(AgentMetrics {
        tokens: 1200,
        requests: 3,
        tools: 4,
        cost: 0.0123,
    });
    let mut fixer = agent("fixer", AgentStatus::Parked, 10_000, Some("scout"));
    fixer.task = Some("fix the lexer".into());
    let done = agent("done", AgentStatus::Aborted, 90_000, None);
    hub.set_agents(vec![main, scout, fixer, done], true);
    hub
}

fn plain(lines: &[String]) -> Vec<String> {
    lines.iter().map(|l| strip_terminal_sequences(l)).collect()
}

#[test]
fn interactive_agent_hub_table_lists_agents_but_main() {
    let mut h = hub();
    let ids: Vec<&str> = h.rows().iter().map(|r| r.id.as_str()).collect();
    assert_eq!(
        ids,
        ["scout", "fixer", "done"],
        "status then recency; Main is the ambient chat"
    );
    let screen = plain(&h.render(80, 20, 170_000));
    assert_eq!(screen.len(), 20);
    assert!(screen[0].starts_with("╭ Agent Hub ─"));
    assert!(
        screen[1].contains("Roster ·  Flat / By parent  · 1 running · 1 parked · 1 aborted"),
        "{:?}",
        screen[1]
    );
    let scout = screen.iter().position(|l| l.contains("❯ ⟳ scout")).unwrap();
    assert!(screen[scout].contains("✉ 2"), "unread badge: {:?}", screen[scout]);
    assert!(screen[scout].contains("claude-sonnet high"));
    assert!(
        screen[scout + 1].contains("map the parser"),
        "task without the spawn preamble"
    );
    assert!(
        screen[scout + 2].contains("$0.012 · 3 req · 4 tools · 1200 tok · 2m ago"),
        "{:?}",
        screen[scout + 2]
    );
    let fixer = screen.iter().position(|l| l.contains("○ fixer")).unwrap();
    assert!(screen[fixer].contains("↳ scout"), "the flat roster names the parent");
    assert!(screen[fixer + 2].contains("usage · 2m ago"));
    assert!(
        screen[18].contains("j/k:select  ↵:open  t:by parent  r/x:manage  /:filter  esc:close"),
        "{:?}",
        screen[18]
    );
}

#[test]
fn interactive_agent_hub_navigates_with_keys_wheel_hover_and_click() {
    let mut h = hub();
    h.render(80, 20, 170_000);
    assert_eq!(h.handle_input("j"), HubEvent::Render);
    assert_eq!(h.selected_id(), Some("fixer"));
    h.handle_input("\x1b[B");
    assert_eq!(h.selected_id(), Some("done"));
    h.handle_input("\x1b[B");
    assert_eq!(h.selected_id(), Some("done"), "stops at the last agent");
    h.handle_input("k");
    h.handle_input("\x1b[<64;5;5M");
    assert_eq!(h.selected_id(), Some("scout"), "wheel up");
    let screen = plain(&h.render(80, 20, 170_000));
    let done_row = screen.iter().position(|l| l.contains("⏹ done")).unwrap();
    assert_eq!(
        h.handle_input(&format!("\x1b[<35;10;{}M", done_row + 1)),
        HubEvent::Render,
        "hover"
    );
    assert_eq!(
        h.handle_input(&format!("\x1b[<35;10;{}M", done_row + 2)),
        HubEvent::Ignored,
        "same entry"
    );
    assert_eq!(
        h.handle_input(&format!("\x1b[<0;10;{}M", done_row + 1)),
        HubEvent::OpenTranscript("done".into()),
        "click opens"
    );
}

#[test]
fn interactive_agent_hub_manages_agents_with_notices() {
    let mut h = hub();
    assert_eq!(h.handle_input("r"), HubEvent::Render);
    assert_eq!(
        h.notice(),
        Some("Agent \"scout\" is running — only parked agents can be revived.")
    );
    h.handle_input("j");
    assert_eq!(h.handle_input("r"), HubEvent::Revive("fixer".into()));
    assert_eq!(h.notice(), None);
    assert_eq!(h.handle_input("x"), HubEvent::Kill("fixer".into()));
    assert_eq!(
        h.handle_input("\x1ba"),
        HubEvent::Close,
        "the hub key toggles it closed"
    );
}

#[test]
fn interactive_agent_hub_groups_by_parent_and_filters() {
    let mut h = hub();
    h.handle_input("t");
    assert_eq!(h.view_mode(), ViewMode::Tree);
    let ids: Vec<&str> = h.rows().iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["scout", "fixer", "done"]);
    let screen = plain(&h.render(80, 20, 170_000));
    assert!(screen.iter().any(|l| l.contains("└── ○ fixer")), "{screen:#?}");
    assert!(screen[1].contains("Flat / By parent"));
    h.handle_input("/");
    for c in "fix".chars() {
        h.handle_input(&c.to_string());
    }
    let ids: Vec<&str> = h.rows().iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, ["fixer"]);
    let screen = plain(&h.render(80, 20, 170_000));
    assert!(screen[18].starts_with("│ /fix  ·  "), "{:?}", screen[18]);
    h.handle_input("\r");
    assert_eq!(h.handle_input("\x1b"), HubEvent::Render, "Esc clears the filter first");
    assert_eq!(h.rows().len(), 3);
    assert_eq!(h.handle_input("\x1b"), HubEvent::Close);
}

#[test]
fn interactive_agent_hub_transcript_viewer_steers_the_agent() {
    let mut h = hub();
    assert_eq!(h.handle_input("\r"), HubEvent::OpenTranscript("scout".into()));
    assert_eq!(h.viewing(), Some("scout"));
    h.set_transcript(
        "scout",
        vec![
            TranscriptEntry::new("1", "t", json!({"role": "user", "content": "map the parser"})),
            TranscriptEntry::new(
                "2",
                "t",
                json!({"role": "assistant", "content": [{"type": "text", "text": "Reading parser.rs"}]}),
            ),
        ],
    );
    let screen = plain(&h.render(60, 16, 170_000));
    assert!(screen[0].starts_with("Agent Hub · scout ⟳ running"), "{:?}", screen[0]);
    assert!(screen.iter().any(|l| l.contains("Reading parser.rs")));
    h.push_transcript_entry(
        "scout",
        TranscriptEntry::new(
            "3",
            "t",
            json!({"role": "assistant", "content": [{"type": "text", "text": "Found 3 bugs"}]}),
        ),
    );
    let screen = plain(&h.render(60, 16, 170_000));
    assert!(
        screen.iter().any(|l| l.contains("Found 3 bugs")),
        "live entries follow the tail"
    );
    // Letters scroll while the input is empty; typing takes over once it is not.
    assert_eq!(h.handle_input("k"), HubEvent::Render);
    assert_eq!(h.handle_input(" "), HubEvent::Render);
    for c in "focus on errors".chars() {
        h.handle_input(&c.to_string());
    }
    assert_eq!(
        h.handle_input("\r"),
        HubEvent::Steer {
            id: "scout".into(),
            text: "focus on errors".into()
        }
    );
    assert_eq!(h.handle_input("\r"), HubEvent::Render, "an empty line sends nothing");
    assert_eq!(h.handle_input("\x1b"), HubEvent::Render);
    assert_eq!(h.viewing(), None, "Esc returns to the table");
    h.handle_input("\r");
    assert_eq!(
        h.handle_input("\x1ba"),
        HubEvent::Close,
        "the hub key closes everything"
    );
    assert_eq!(h.viewing(), None);
}

#[test]
fn interactive_agent_hub_badge_counts_running_children() {
    let agents = vec![
        agent("a", AgentStatus::Running, 0, None),
        agent("b", AgentStatus::Idle, 0, None),
        {
            let mut m = agent("Main", AgentStatus::Running, 0, None);
            m.kind = AgentKind::Main;
            m
        },
    ];
    assert_eq!(running_subagent_ids(&agents), ["a"]);
    let t = theme();
    let badge = interactive::agent_hub::running_subagent_badge(&t, &agents).unwrap();
    assert_eq!(strip_terminal_sequences(&badge), "⟳ 1 agent");
    assert!(interactive::agent_hub::running_subagent_badge(&t, &agents[1..]).is_none());
}
