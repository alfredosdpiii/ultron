//! The agent hub overlay (oh-my-pi packages/tui/src/overlays/agent-hub.ts, agent-hub-projection.ts,
//! agent-hub-renderer.ts, agent-transcript-viewer.ts and running-subagent-badge.ts).
//!
//! Ultron's agents are RLM children (`rlm.spawn`). The hub renders a plain view model ([`AgentView`]) so the client
//! can feed it from the daemon protocol: a roster of every agent except the main session (status, unread messages,
//! task, last activity, usage) ordered by status then recency, flat or grouped by parent; keyboard, wheel, hover
//! and click navigation; `r` revives a parked agent and `x` aborts and releases one. Enter opens the agent's
//! transcript with its own input line; submitting steers the agent. The hub never talks to the session: it returns
//! [`HubEvent`]s and the client feeds transcripts back with [`AgentHub::set_transcript`].

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use tui::components::Component;
use tui::input::Input;
use tui::keybindings::KeybindingsManager;
use tui::outline::{Anchor, BrowserFrame, FrameBrowser, key_glyph};
use tui::sgr_mouse::parse_sgr_mouse;
use tui::theme::Theme;
use tui::utils::{strip_terminal_sequences, truncate_to_width, visible_width};

use crate::chat::{ChatStyle, ChatView};
use crate::components::{bold, fg};
use crate::copy_targets::TranscriptEntry;

/// Identity of the ambient main session in the roster.
pub const MAIN_AGENT_ID: &str = "Main";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentStatus {
    Running,
    Idle,
    Parked,
    Aborted,
}

impl AgentStatus {
    /// `STATUS_ORDER`.
    pub fn order(self) -> u8 {
        match self {
            AgentStatus::Running => 0,
            AgentStatus::Idle => 1,
            AgentStatus::Parked => 2,
            AgentStatus::Aborted => 3,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            AgentStatus::Running => "running",
            AgentStatus::Idle => "idle",
            AgentStatus::Parked => "parked",
            AgentStatus::Aborted => "aborted",
        }
    }

    /// Theme color of the status (`STATUS_TONE`).
    pub fn color(self) -> &'static str {
        match self {
            AgentStatus::Running => "accent",
            AgentStatus::Idle => "success",
            AgentStatus::Parked => "muted",
            AgentStatus::Aborted => "error",
        }
    }

    /// omp's unicode status glyph.
    pub fn glyph(self) -> &'static str {
        match self {
            AgentStatus::Running => "⟳",
            AgentStatus::Idle => "●",
            AgentStatus::Parked => "○",
            AgentStatus::Aborted => "⏹",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentKind {
    Main,
    /// An RLM child.
    Sub,
    /// A read-only transcript.
    Advisor,
}

/// Usage of one agent.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AgentMetrics {
    pub tokens: u64,
    pub requests: u64,
    pub tools: u64,
    pub cost: f64,
}

/// One roster row as the daemon reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentView {
    pub id: String,
    pub kind: AgentKind,
    pub parent_id: Option<String>,
    pub status: AgentStatus,
    /// Unread messages from the agent.
    pub unread: usize,
    /// Current or last task.
    pub task: Option<String>,
    /// Last activity time, epoch milliseconds.
    pub last_activity: i64,
    /// Model id and thinking level that produced the work.
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub metrics: Option<AgentMetrics>,
}

/// `compareRosterAgents`: status, then most recent activity, then id.
pub fn compare_roster_agents(a: &AgentView, b: &AgentView) -> std::cmp::Ordering {
    a.status
        .order()
        .cmp(&b.status.order())
        .then(b.last_activity.cmp(&a.last_activity))
        .then_with(|| a.id.cmp(&b.id))
}

/// The roster ranking captured once (`#rowOrder`), so heartbeats never reorder rows under the cursor; each batch
/// of newcomers is sorted and ranked above every row already on screen.
#[derive(Clone, Debug, Default)]
pub struct RosterOrder {
    rank: Option<HashMap<String, i64>>,
    next_new: i64,
}

impl RosterOrder {
    /// Order `refs` (the main session excluded by the caller). `settled`: persisted agents finished loading.
    pub fn order(&mut self, mut refs: Vec<AgentView>, settled: bool) -> Vec<AgentView> {
        match &mut self.rank {
            None => {
                refs.sort_by(compare_roster_agents);
                if settled && !refs.is_empty() {
                    self.rank = Some(refs.iter().enumerate().map(|(i, r)| (r.id.clone(), i as i64)).collect());
                }
                refs
            }
            Some(rank) => {
                rank.retain(|id, _| refs.iter().any(|r| &r.id == id));
                let mut newcomers: Vec<&AgentView> = refs.iter().filter(|r| !rank.contains_key(&r.id)).collect();
                newcomers.sort_by(|a, b| compare_roster_agents(a, b));
                for r in newcomers.iter().rev() {
                    self.next_new -= 1;
                    rank.insert(r.id.clone(), self.next_new);
                }
                refs.sort_by_key(|r| rank[&r.id]);
                refs
            }
        }
    }

    /// Rank of `id` in the captured order.
    pub fn rank(&self, id: &str) -> Option<i64> {
        self.rank.as_ref().and_then(|r| r.get(id).copied())
    }
}

/// `projectAgentTree`: parent-before-child rows with depth, parent and last-sibling facts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TreeProjection {
    /// Indices into the input rows.
    pub rows: Vec<usize>,
    pub depth: HashMap<String, usize>,
    pub parent: HashMap<String, String>,
    pub last_sibling: HashMap<String, bool>,
}

/// A roster rank lookup by agent id.
pub type RankFn<'a> = dyn Fn(&str) -> Option<i64> + 'a;

/// `projectAgentTree(refs, rosterRank)`: groups keep the position of their earliest row; a child spawned after its
/// parent was ranked (negative rank below the parent's) cannot move the parent's group; parent cycles stay visible.
pub fn project_agent_tree(refs: &[AgentView], rank: Option<&RankFn<'_>>) -> TreeProjection {
    let index: HashMap<&str, usize> = refs.iter().enumerate().map(|(i, r)| (r.id.as_str(), i)).collect();
    let mut parent: HashMap<String, String> = HashMap::new();
    let mut children: HashMap<String, Vec<usize>> = HashMap::new();
    let mut child_order: Vec<String> = Vec::new();
    for (i, r) in refs.iter().enumerate() {
        let p = match &r.parent_id {
            Some(p) if p != MAIN_AGENT_ID && index.contains_key(p.as_str()) => p.clone(),
            _ => MAIN_AGENT_ID.to_string(),
        };
        parent.insert(r.id.clone(), p.clone());
        if !children.contains_key(&p) {
            child_order.push(p.clone());
        }
        children.entry(p).or_default().push(i);
    }
    let rank_of = |i: usize| rank.and_then(|f| f(&refs[i].id));
    let mut subtree: HashMap<usize, usize> = HashMap::new();
    let mut visiting: HashSet<usize> = HashSet::new();
    let mut ranked: HashSet<usize> = HashSet::new();
    for start in 0..refs.len() {
        if ranked.contains(&start) {
            continue;
        }
        let mut stack = vec![(start, false)];
        while let Some((cur, expanded)) = stack.pop() {
            if expanded {
                let mut order = cur;
                let parent_rank = rank_of(cur);
                for &child in children.get(&refs[cur].id).map(Vec::as_slice).unwrap_or(&[]) {
                    let child_rank = rank_of(child);
                    if let (Some(pr), Some(cr)) = (parent_rank, child_rank)
                        && cr < 0
                        && cr < pr
                    {
                        continue;
                    }
                    order = order.min(subtree.get(&child).copied().unwrap_or(usize::MAX));
                }
                subtree.insert(cur, order);
                visiting.remove(&cur);
                ranked.insert(cur);
                continue;
            }
            if ranked.contains(&cur) || visiting.contains(&cur) {
                continue;
            }
            visiting.insert(cur);
            stack.push((cur, true));
            if let Some(desc) = children.get(&refs[cur].id) {
                for &child in desc.iter().rev() {
                    if !ranked.contains(&child) && !visiting.contains(&child) {
                        stack.push((child, false));
                    }
                }
            }
        }
    }
    for p in &child_order {
        if let Some(siblings) = children.get_mut(p) {
            siblings.sort_by_key(|&i| (subtree.get(&i).copied().unwrap_or(usize::MAX), i));
        }
    }
    let mut last_sibling = HashMap::new();
    for siblings in children.values() {
        for (k, &i) in siblings.iter().enumerate() {
            last_sibling.insert(refs[i].id.clone(), k == siblings.len() - 1);
        }
    }
    let mut rows = Vec::new();
    let mut visited: HashSet<usize> = HashSet::new();
    let mut depth = HashMap::new();
    let mut visit = |root: usize, root_depth: usize, rows: &mut Vec<usize>| {
        let mut stack = vec![(root, root_depth)];
        while let Some((cur, d)) = stack.pop() {
            if !visited.insert(cur) {
                continue;
            }
            depth.insert(refs[cur].id.clone(), d);
            rows.push(cur);
            if let Some(desc) = children.get(&refs[cur].id) {
                for &child in desc.iter().rev() {
                    stack.push((child, d + 1));
                }
            }
        }
    };
    for &root in children.get(MAIN_AGENT_ID).map(Vec::as_slice).unwrap_or(&[]) {
        visit(root, 0, &mut rows);
    }
    for i in 0..refs.len() {
        visit(i, 0, &mut rows);
    }
    TreeProjection {
        rows,
        depth,
        parent,
        last_sibling,
    }
}

const TREE_SEGMENT_WIDTH: usize = 4;

/// `treePrefix`: the bash `tree`-style ancestry of a row (`continuation` rows carry rails only), unstyled.
pub fn tree_prefix(id: &str, max_width: usize, tree: &TreeProjection, continuation: bool) -> String {
    if tree.depth.get(id).copied().unwrap_or(0) == 0 {
        return String::new();
    }
    let last = tree.last_sibling.get(id).copied().unwrap_or(false);
    let mut segments: Vec<&str> = vec![match (continuation, last) {
        (true, true) => "    ",
        (true, false) => "│   ",
        (false, true) => "└── ",
        (false, false) => "├── ",
    }];
    let mut ancestry: HashSet<&str> = HashSet::new();
    let mut p = tree.parent.get(id).map(String::as_str);
    while let Some(par) = p {
        if par == MAIN_AGENT_ID || ancestry.contains(par) {
            break;
        }
        let grand = tree.parent.get(par).map(String::as_str);
        match grand {
            None | Some(MAIN_AGENT_ID) => break,
            Some(g) => {
                ancestry.insert(par);
                segments.push(if tree.last_sibling.get(par).copied().unwrap_or(false) {
                    "    "
                } else {
                    "│   "
                });
                p = Some(g);
            }
        }
    }
    let max_segments = (max_width.saturating_sub(2).max(TREE_SEGMENT_WIDTH) / TREE_SEGMENT_WIDTH).max(1);
    let omitted = segments.len().saturating_sub(max_segments);
    let shown: Vec<&str> = segments.iter().take(max_segments).rev().copied().collect();
    let omitted_prefix = if omitted > 0 {
        if continuation { "  " } else { "… " }
    } else {
        ""
    };
    format!("{omitted_prefix}{}", shown.concat())
}

/// `sanitizeDisplaySingleLine`: escape sequences and control characters removed, tabs expanded, line breaks as
/// spaces.
pub fn sanitize_single_line(text: &str) -> String {
    let stripped = strip_terminal_sequences(text);
    let mut out = String::with_capacity(stripped.len());
    let mut in_break = false;
    for c in stripped.chars() {
        if c == '\r' || c == '\n' {
            if !in_break {
                out.push(' ');
            }
            in_break = true;
            continue;
        }
        in_break = false;
        if c == '\t' {
            out.push_str("   ");
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// `taskSummary`: the spawn prompt's fixed preamble is not part of the task.
pub fn task_summary(task: &str) -> String {
    let trimmed = task.trim_start();
    let prefix = "complete assignment thoroughly:";
    let rest = if trimmed.len() >= prefix.len()
        && trimmed.is_char_boundary(prefix.len())
        && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix)
    {
        trimmed[prefix.len()..].trim_start()
    } else {
        task
    };
    sanitize_single_line(rest)
}

/// omp `formatAge(seconds)`.
pub fn format_age(seconds: i64) -> String {
    if seconds == 0 {
        return String::new();
    }
    let mins = seconds.div_euclid(60);
    let hours = mins.div_euclid(60);
    let days = hours.div_euclid(24);
    let (weeks, months) = (days.div_euclid(7), days.div_euclid(30));
    if months > 0 {
        format!("{months}mo ago")
    } else if weeks > 0 {
        format!("{weeks}w ago")
    } else if days > 0 {
        format!("{days}d ago")
    } else if hours > 0 {
        format!("{hours}h ago")
    } else if mins > 0 {
        format!("{mins}m ago")
    } else {
        "just now".into()
    }
}

/// omp `formatCost`.
pub fn format_cost(cost: f64) -> String {
    let amount = if cost.is_finite() { cost } else { 0.0 };
    if amount < 0.01 {
        format!("${amount:.4}")
    } else if amount < 1.0 {
        format!("${amount:.3}")
    } else {
        format!("${amount:.2}")
    }
}

/// The rows of a roster window: entries `start..end` around the selection, plus overflow markers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RosterWindow {
    pub start: usize,
    pub end: usize,
    /// `… N more` above / below.
    pub top: bool,
    pub bottom: bool,
    /// The selected entry alone overflows the budget and is cut to it.
    pub clipped: bool,
}

/// `#renderRosterWindow`: grow a window of whole entries around `selected` within `budget` rows (only visible
/// entries render, so a 5,000-agent roster keeps bounded paint cost), then trim the farthest neighbors so both
/// overflow labels fit.
pub fn roster_window(heights: &[usize], selected: usize, budget: usize) -> RosterWindow {
    let n = heights.len();
    let (mut start, mut end) = (selected, selected + 1);
    let mut used = heights[selected];
    if used > budget {
        return RosterWindow {
            start,
            end,
            top: false,
            bottom: false,
            clipped: true,
        };
    }
    let mut grew = true;
    while grew {
        grew = false;
        if end < n && used + heights[end] <= budget {
            used += heights[end];
            end += 1;
            grew = true;
        }
        if start > 0 && used + heights[start - 1] <= budget {
            start -= 1;
            used += heights[start];
            grew = true;
        }
    }
    loop {
        let markers = usize::from(start > 0) + usize::from(end < n);
        if used + markers <= budget || start >= end {
            break;
        }
        if end - 1 > selected {
            end -= 1;
            used -= heights[end];
        } else if start < selected {
            used -= heights[start];
            start += 1;
        } else {
            break;
        }
    }
    let top = start > 0 && used < budget;
    let bottom = end < n && used + usize::from(top) < budget;
    RosterWindow {
        start,
        end,
        top,
        bottom,
        clipped: false,
    }
}

/// `getRunningSubagentBadgeAgentIds`: running RLM children, for the status-line badge.
pub fn running_subagent_ids(agents: &[AgentView]) -> Vec<String> {
    agents
        .iter()
        .filter(|a| a.kind == AgentKind::Sub && a.status == AgentStatus::Running)
        .map(|a| a.id.clone())
        .collect()
}

/// The status-line badge: `⟳ 2 agents` while children run, nothing otherwise.
pub fn running_subagent_badge(theme: &Theme, agents: &[AgentView]) -> Option<String> {
    let n = running_subagent_ids(agents).len();
    (n > 0).then(|| {
        fg(
            theme,
            "accent",
            &format!("⟳ {n} agent{}", if n == 1 { "" } else { "s" }),
        )
    })
}

/// What the client should do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HubEvent {
    /// Not the hub's key.
    Ignored,
    Render,
    Close,
    /// The transcript view of `id` opened: feed it with [`AgentHub::set_transcript`].
    OpenTranscript(String),
    /// Revive a parked agent.
    Revive(String),
    /// Abort (when running) and release an agent.
    Kill(String),
    /// Send `text` to the agent: steers a running one, prompts an idle one, revives a parked one first.
    Steer {
        id: String,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode {
    Roster,
    Tree,
}

/// The per-agent transcript with its input line.
struct Viewer {
    id: String,
    entries: Vec<TranscriptEntry>,
    chat: ChatView,
    browser: FrameBrowser,
    input: Input,
    expanded: bool,
    /// Follow appended entries while at the bottom.
    follow: bool,
}

pub struct AgentHub {
    theme: Rc<Theme>,
    keys: KeybindingsManager,
    agents: Vec<AgentView>,
    order: RosterOrder,
    rows: Vec<AgentView>,
    tree: TreeProjection,
    children: HashSet<String>,
    view: ViewMode,
    selected: usize,
    hovered: Option<usize>,
    filter: Input,
    filter_editing: bool,
    notice: Option<String>,
    settled: bool,
    /// Screen row -> roster row of the last frame.
    hit_rows: Vec<Option<usize>>,
    viewer: Option<Viewer>,
    chat_style: Rc<dyn Fn() -> ChatStyle>,
    now: i64,
}

impl AgentHub {
    /// `chat_style` builds the transcript viewer's chat styling.
    pub fn new(theme: Rc<Theme>, keys: KeybindingsManager, chat_style: Rc<dyn Fn() -> ChatStyle>) -> Self {
        let mut filter = Input::new(keys.clone()).with_options(Some(""), None, None, None);
        filter.focused = true;
        AgentHub {
            theme,
            keys,
            agents: Vec::new(),
            order: RosterOrder::default(),
            rows: Vec::new(),
            tree: TreeProjection::default(),
            children: HashSet::new(),
            view: ViewMode::Roster,
            selected: 0,
            hovered: None,
            filter,
            filter_editing: false,
            notice: None,
            settled: true,
            hit_rows: Vec::new(),
            viewer: None,
            chat_style,
            now: 0,
        }
    }

    /// Replace the roster (every registry change). `settled`: persisted agents have loaded.
    pub fn set_agents(&mut self, agents: Vec<AgentView>, settled: bool) {
        self.agents = agents;
        self.settled = settled;
        self.refresh_rows();
    }

    pub fn rows(&self) -> &[AgentView] {
        &self.rows
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.rows.get(self.selected).map(|r| r.id.as_str())
    }

    pub fn view_mode(&self) -> ViewMode {
        self.view
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// The agent whose transcript is open.
    pub fn viewing(&self) -> Option<&str> {
        self.viewer.as_ref().map(|v| v.id.as_str())
    }

    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }

    fn refresh_rows(&mut self) {
        let selected_id = self.rows.get(self.selected).map(|r| r.id.clone());
        let refs: Vec<AgentView> = self.agents.iter().filter(|a| a.id != MAIN_AGENT_ID).cloned().collect();
        let ordered = self.order.order(refs, self.settled);
        let query = self.filter.value().trim().to_lowercase();
        let roster: Vec<AgentView> = if query.is_empty() {
            ordered
        } else {
            ordered
                .into_iter()
                .filter(|r| tui::fuzzy::fuzzy_match(&query, &r.id.to_lowercase()).matches)
                .collect()
        };
        self.children = roster.iter().filter_map(|r| r.parent_id.clone()).collect();
        match self.view {
            ViewMode::Tree => {
                let order = &self.order;
                let rank = |id: &str| order.rank(id);
                self.tree = project_agent_tree(&roster, Some(&rank));
                self.rows = self.tree.rows.iter().map(|&i| roster[i].clone()).collect();
            }
            ViewMode::Roster => {
                self.tree = TreeProjection::default();
                self.rows = roster;
            }
        }
        let kept = selected_id.and_then(|id| self.rows.iter().position(|r| r.id == id));
        self.selected = kept.unwrap_or_else(|| self.selected.min(self.rows.len().saturating_sub(1)));
    }

    /// Set the agent's transcript (opening or a full reload).
    pub fn set_transcript(&mut self, id: &str, entries: Vec<TranscriptEntry>) {
        let style = (self.chat_style)();
        if let Some(v) = self.viewer.as_mut().filter(|v| v.id == id) {
            let mut chat = ChatView::new(style);
            for e in &entries {
                replay(&mut chat, e);
            }
            chat.set_tools_expanded(v.expanded);
            v.chat = chat;
            v.entries = entries;
        }
    }

    /// Append one live entry to an open transcript.
    pub fn push_transcript_entry(&mut self, id: &str, entry: TranscriptEntry) {
        if let Some(v) = self.viewer.as_mut().filter(|v| v.id == id) {
            replay(&mut v.chat, &entry);
            v.entries.push(entry);
        }
    }

    fn k(&self, data: &str, id: &str) -> bool {
        self.keys.matches(data, id)
    }

    pub fn handle_input(&mut self, data: &str) -> HubEvent {
        if self.viewer.is_some() {
            return self.handle_viewer_input(data);
        }
        if let Some(mouse) = parse_sgr_mouse(data) {
            let row = usize::try_from(mouse.row)
                .ok()
                .and_then(|r| self.hit_rows.get(r).copied().flatten());
            if let Some(wheel) = mouse.wheel {
                self.hovered = None;
                if !self.rows.is_empty() {
                    self.selected = (self.selected as i64 + wheel).clamp(0, self.rows.len() as i64 - 1) as usize;
                }
                return HubEvent::Render;
            }
            if mouse.motion {
                let changed = self.hovered != row;
                self.hovered = row;
                return if changed { HubEvent::Render } else { HubEvent::Ignored };
            }
            if mouse.left_click
                && let Some(index) = row
            {
                self.hovered = Some(index);
                self.selected = index;
                return self.activate();
            }
            return HubEvent::Ignored;
        }
        if self.k(data, "app.agents.hub") {
            return HubEvent::Close;
        }
        if self.filter_editing {
            if self.k(data, "app.agentHub.close") || self.k(data, "tui.select.confirm") {
                self.filter_editing = false;
                if self.k(data, "app.agentHub.close") {
                    self.filter.set_value("");
                }
            } else {
                let before = self.filter.value().to_string();
                self.filter.handle_input(data);
                if self.filter.value() == before {
                    return HubEvent::Render;
                }
            }
            self.refresh_rows();
            return HubEvent::Render;
        }
        if self.k(data, "app.agentHub.close") {
            if !self.filter.value().is_empty() {
                self.filter.set_value("");
                self.refresh_rows();
                return HubEvent::Render;
            }
            return HubEvent::Close;
        }
        if self.k(data, "app.agentHub.filter") {
            self.filter_editing = true;
            return HubEvent::Render;
        }
        if self.k(data, "app.agentHub.tree") {
            self.hovered = None;
            self.view = match self.view {
                ViewMode::Roster => ViewMode::Tree,
                ViewMode::Tree => ViewMode::Roster,
            };
            self.refresh_rows();
            return HubEvent::Render;
        }
        if self.k(data, "app.agentHub.down") || self.k(data, "tui.select.down") {
            self.hovered = None;
            if !self.rows.is_empty() {
                self.selected = (self.selected + 1).min(self.rows.len() - 1);
            }
            return HubEvent::Render;
        }
        if self.k(data, "app.agentHub.up") || self.k(data, "tui.select.up") {
            self.hovered = None;
            self.selected = self.selected.saturating_sub(1);
            return HubEvent::Render;
        }
        if self.k(data, "tui.select.confirm") {
            return self.activate();
        }
        if self.k(data, "app.agentHub.revive") {
            return self.revive();
        }
        if self.k(data, "app.agentHub.kill") {
            return self.kill();
        }
        HubEvent::Ignored
    }

    fn activate(&mut self) -> HubEvent {
        let Some(row) = self.rows.get(self.selected) else {
            return HubEvent::Ignored;
        };
        self.notice = None;
        let id = row.id.clone();
        let mut input = Input::new(self.keys.clone()).with_options(Some("› "), Some("message this agent…"), None, None);
        input.focused = true;
        self.viewer = Some(Viewer {
            id: id.clone(),
            entries: Vec::new(),
            chat: ChatView::new((self.chat_style)()),
            browser: FrameBrowser::default(),
            input,
            expanded: false,
            follow: true,
        });
        HubEvent::OpenTranscript(id)
    }

    fn revive(&mut self) -> HubEvent {
        let Some(row) = self.rows.get(self.selected) else {
            return HubEvent::Ignored;
        };
        if row.kind == AgentKind::Advisor {
            self.notice = Some(format!(
                "\"{}\" is a read-only advisor transcript — nothing to revive.",
                row.id
            ));
            return HubEvent::Render;
        }
        if row.status != AgentStatus::Parked {
            self.notice = Some(format!(
                "Agent \"{}\" is {} — only parked agents can be revived.",
                row.id,
                row.status.name()
            ));
            return HubEvent::Render;
        }
        self.notice = None;
        HubEvent::Revive(row.id.clone())
    }

    fn kill(&mut self) -> HubEvent {
        let Some(row) = self.rows.get(self.selected) else {
            return HubEvent::Ignored;
        };
        if row.kind == AgentKind::Advisor {
            self.notice = Some(format!(
                "\"{}\" is a read-only advisor transcript — cannot be killed.",
                row.id
            ));
            return HubEvent::Render;
        }
        self.notice = None;
        HubEvent::Kill(row.id.clone())
    }

    fn handle_viewer_input(&mut self, data: &str) -> HubEvent {
        if self.k(data, "app.agents.hub") {
            self.viewer = None;
            return HubEvent::Close;
        }
        let keys = self.keys.clone();
        let Some(v) = self.viewer.as_mut() else {
            return HubEvent::Ignored;
        };
        if let Some(mouse) = parse_sgr_mouse(data) {
            if let Some(wheel) = mouse.wheel {
                v.browser.body.scroll(wheel * 3);
                v.follow = v.browser.body.offset >= v.browser.body.max_offset();
                return HubEvent::Render;
            }
            return HubEvent::Ignored;
        }
        if keys.matches(data, "app.agentHub.close") {
            if !v.input.value().is_empty() {
                v.input.set_value("");
                return HubEvent::Render;
            }
            self.viewer = None;
            return HubEvent::Render;
        }
        if keys.matches(data, "app.tools.expand") {
            v.expanded = !v.expanded;
            v.chat.set_tools_expanded(v.expanded);
            return HubEvent::Render;
        }
        // Once the reader starts typing, the input owns every key.
        if v.input.value().trim().is_empty() {
            let body = &mut v.browser.body;
            let scrolled = if keys.matches(data, "tui.altScreen.pageUp") {
                Some(body.page(-1))
            } else if keys.matches(data, "tui.altScreen.pageDown") {
                Some(body.page(1))
            } else if keys.matches(data, "app.agentHub.down") || keys.matches(data, "tui.select.down") {
                Some(body.scroll(1))
            } else if keys.matches(data, "app.agentHub.up") || keys.matches(data, "tui.select.up") {
                Some(body.scroll(-1))
            } else if keys.matches(data, "app.agentHub.top") {
                Some(body.to_top())
            } else if keys.matches(data, "app.agentHub.bottom") {
                Some(body.to_bottom())
            } else if keys.matches(data, "app.transcript.scrollUp") {
                Some(body.scroll(-5))
            } else if keys.matches(data, "app.transcript.scrollDown") {
                Some(body.scroll(5))
            } else {
                None
            };
            if let Some(_moved) = scrolled {
                v.follow = v.browser.body.offset >= v.browser.body.max_offset();
                return HubEvent::Render;
            }
        }
        if keys.matches(data, "tui.input.submit") {
            let text = v.input.value().trim().to_string();
            v.input.set_value("");
            if text.is_empty() {
                return HubEvent::Render;
            }
            v.follow = true;
            return HubEvent::Steer { id: v.id.clone(), text };
        }
        v.input.handle_input(data);
        HubEvent::Render
    }

    /// The hub (or the open transcript) at `width` x `height`; `now` is epoch milliseconds for activity ages.
    pub fn render(&mut self, width: usize, height: usize, now: i64) -> Vec<String> {
        self.now = now;
        let width = width.max(1);
        let height = height.max(1);
        if self.viewer.is_some() {
            return self.render_viewer(width, height);
        }
        let t = Rc::clone(&self.theme);
        let inner = width.saturating_sub(4).max(1);
        let content_rows = height.saturating_sub(4).max(1);
        let mut body: Vec<String> = Vec::new();
        let mut hits: Vec<Option<usize>> = Vec::new();
        // Summary: projection toggle and status counts.
        let active = |s: &str| bold(&fg(&t, "accent", &format!(" {s} ")));
        let inactive = |s: &str| fg(&t, "muted", &format!(" {s} "));
        let projection = match self.view {
            ViewMode::Roster => format!("{}{}{}", active("Flat"), fg(&t, "dim", "/"), inactive("By parent")),
            ViewMode::Tree => format!("{}{}{}", inactive("Flat"), fg(&t, "dim", "/"), active("By parent")),
        };
        let mut counts = Vec::new();
        for status in [
            AgentStatus::Running,
            AgentStatus::Idle,
            AgentStatus::Parked,
            AgentStatus::Aborted,
        ] {
            let n = self.rows.iter().filter(|r| r.status == status).count();
            if n > 0 {
                counts.push(fg(&t, status.color(), &format!("{n} {}", status.name())));
            }
        }
        let dot = fg(&t, "dim", " · ");
        let mut summary = format!("{}{dot}{projection}", bold("Roster"));
        if !counts.is_empty() {
            summary.push_str(&format!("{dot}{}", counts.join(&dot)));
        }
        body.push(summary);
        hits.push(None);
        if content_rows >= 8 {
            body.push(String::new());
            hits.push(None);
        }
        let notice: Vec<String> = self
            .notice
            .iter()
            .map(|n| {
                fg(
                    &t,
                    "error",
                    &truncate_to_width(&sanitize_single_line(n), inner.max(10), "…", false),
                )
            })
            .collect();
        let budget = content_rows.saturating_sub(body.len() + notice.len());
        if self.rows.is_empty() {
            let empty = [
                format!("{} {}", fg(&t, "muted", "○"), bold("No agents in this session")),
                fg(
                    &t,
                    "dim",
                    "Spawned RLM children (rlm.spawn) appear here while the session runs.",
                ),
            ];
            for line in empty.into_iter().take(budget) {
                body.push(line);
                hits.push(None);
            }
        } else if budget > 0 {
            let entries: Vec<Vec<String>> = (0..self.rows.len()).map(|i| self.entry_lines(i, inner)).collect();
            let heights: Vec<usize> = entries.iter().map(Vec::len).collect();
            let w = roster_window(&heights, self.selected, budget);
            if w.clipped {
                for line in entries[self.selected].iter().take(budget) {
                    body.push(line.clone());
                    hits.push(Some(self.selected));
                }
            } else {
                if w.top {
                    body.push(fg(&t, "dim", &format!("… {} more", w.start)));
                    hits.push(None);
                }
                for (i, entry) in entries.iter().enumerate().take(w.end).skip(w.start) {
                    for line in entry {
                        body.push(line.clone());
                        hits.push(Some(i));
                    }
                }
                if w.bottom {
                    body.push(fg(&t, "dim", &format!("… {} more", self.rows.len() - w.end)));
                    hits.push(None);
                }
            }
        }
        for n in notice {
            body.push(n);
            hits.push(None);
        }
        body.truncate(content_rows);
        hits.truncate(content_rows);
        while body.len() < content_rows {
            body.push(String::new());
            hits.push(None);
        }
        let mut out = vec![top_border(&t, width, "Agent Hub")];
        self.hit_rows = vec![None];
        for (line, hit) in body.iter().zip(hits) {
            out.push(boxed_row(&t, line, width));
            self.hit_rows.push(hit);
        }
        out.push(divider(&t, width));
        out.push(boxed_row(&t, &fg(&t, "dim", &self.footer()), width));
        out.push(bottom_border(&t, width));
        self.hit_rows.extend([None, None, None]);
        out.truncate(height);
        out
    }

    fn first_key(&self, id: &str) -> String {
        self.keys.get_keys(id).first().map(|k| key_glyph(k)).unwrap_or_default()
    }

    fn footer(&self) -> String {
        let filter = if self.filter_editing || !self.filter.value().is_empty() {
            format!("/{}  ·  ", self.filter.value())
        } else {
            String::new()
        };
        let next = match self.view {
            ViewMode::Roster => "by parent",
            ViewMode::Tree => "flat",
        };
        format!(
            "{filter}{}/{}:select  {}:open  {}:{next}  {}/{}:manage  {}:filter  {}:close",
            self.first_key("app.agentHub.down"),
            self.first_key("app.agentHub.up"),
            self.first_key("tui.select.confirm"),
            self.first_key("app.agentHub.tree"),
            self.first_key("app.agentHub.revive"),
            self.first_key("app.agentHub.kill"),
            self.first_key("app.agentHub.filter"),
            self.first_key("app.agentHub.close"),
        )
    }

    /// One roster entry: identity (cursor, tree branch, status, id, parent, unread, model), task, activity age and
    /// usage (`#renderEntry`).
    fn entry_lines(&self, index: usize, width: usize) -> Vec<String> {
        let t = &self.theme;
        let r = &self.rows[index];
        let selected = index == self.selected;
        let cursor = if selected { fg(t, "accent", "❯") } else { " ".into() };
        let tree = self.view == ViewMode::Tree;
        let branch = if tree {
            fg(t, "dim", &tree_prefix(&r.id, width, &self.tree, false))
        } else {
            String::new()
        };
        let id = sanitize_single_line(&r.id);
        let styled = if selected {
            bold(&fg(t, "accent", &id))
        } else {
            bold(&id)
        };
        let mut fields = vec![format!(
            "{cursor} {branch}{} {styled}",
            fg(t, r.status.color(), r.status.glyph())
        )];
        if !tree && let Some(p) = r.parent_id.as_deref().filter(|p| *p != MAIN_AGENT_ID) {
            fields.push(fg(t, "dim", &format!("↳ {}", sanitize_single_line(p))));
        }
        if r.kind == AgentKind::Advisor {
            fields.push(fg(t, "warning", "read-only"));
        }
        if r.unread > 0 {
            fields.push(fg(t, "warning", &format!("✉ {}", r.unread)));
        }
        let left = fields.join("  ");
        let right = r
            .model
            .as_deref()
            .map(|m| {
                let model = fg(t, "muted", &sanitize_single_line(m));
                match r.thinking.as_deref().filter(|l| *l != "off" && *l != "inherit") {
                    Some(level) => format!("{model} {}", t.thinking_border(level, level)),
                    None => model,
                }
            })
            .unwrap_or_default();
        let (lw, rw) = (visible_width(&left), visible_width(&right));
        let mut lines = vec![if rw > 0 && lw + 2 + rw <= width {
            format!("{left}{}{right}", " ".repeat(width - lw - rw))
        } else {
            truncate_to_width(&left, width, "…", false)
        }];
        let detail_indent = (4 + visible_width(&branch)).min(width.saturating_sub(1));
        let rail = if self.children.contains(&r.id) {
            fg(t, "dim", "│ ")
        } else {
            "  ".into()
        };
        let continuation = if tree {
            format!("  {}{rail}", fg(t, "dim", &tree_prefix(&r.id, width, &self.tree, true)))
        } else {
            String::new()
        };
        let indent = if tree && visible_width(&continuation) == detail_indent {
            continuation
        } else {
            " ".repeat(detail_indent)
        };
        let detail_width = width.saturating_sub(detail_indent).max(1);
        if let Some(task) = r.task.as_deref().map(task_summary).filter(|s| !s.trim().is_empty()) {
            lines.push(format!(
                "{indent}{}",
                fg(t, "muted", &truncate_to_width(&task, detail_width, "…", false))
            ));
        }
        let age = format_age(((self.now - r.last_activity) as f64 / 1000.0).round().max(1.0) as i64);
        let meta = match r.metrics {
            Some(m) => format!(
                "{} · {} req · {} tools · {} tok · {age}",
                format_cost(m.cost),
                m.requests,
                m.tools,
                m.tokens
            ),
            None => format!("usage · {age}"),
        };
        lines.push(format!(
            "{indent}{}",
            fg(t, "dim", &truncate_to_width(&meta, detail_width, "…", false))
        ));
        if self.hovered == Some(index) {
            return lines
                .into_iter()
                .map(|l| {
                    let w = visible_width(&l);
                    let padded = if w < width {
                        format!("{l}{}", " ".repeat(width - w))
                    } else {
                        l
                    };
                    t.bg("selectedBg", &padded).unwrap_or(padded)
                })
                .collect();
        }
        lines
    }

    fn render_viewer(&mut self, width: usize, height: usize) -> Vec<String> {
        let t = Rc::clone(&self.theme);
        let row = self
            .viewer
            .as_ref()
            .and_then(|v| self.agents.iter().find(|a| a.id == v.id))
            .cloned();
        let footer_hint = format!(
            "{}/{} scroll  {} send  {} expand  {} back  {} close hub",
            self.first_key("app.agentHub.down"),
            self.first_key("app.agentHub.up"),
            self.first_key("tui.input.submit"),
            self.first_key("app.tools.expand"),
            self.first_key("app.agentHub.close"),
            self.first_key("app.agents.hub"),
        );
        let Some(v) = self.viewer.as_mut() else {
            return Vec::new();
        };
        let status = row
            .as_ref()
            .map(|r| {
                format!(
                    " {}",
                    fg(
                        &t,
                        r.status.color(),
                        &format!("{} {}", r.status.glyph(), r.status.name())
                    )
                )
            })
            .unwrap_or_default();
        let header = vec![format!(
            "{}{status}",
            bold(&format!("Agent Hub · {}", sanitize_single_line(&v.id)))
        )];
        let body: Vec<String> = v.chat.render_item_rows(width).into_iter().flatten().collect();
        let body = if body.is_empty() {
            vec![fg(&t, "muted", "  (no messages yet)")]
        } else {
            body
        };
        let input = v.input.render(width).into_iter().next().unwrap_or_default();
        let total = body.len();
        let anchor = v.follow.then(|| Anchor {
            id: format!("tail:{total}"),
            start: total.saturating_sub(1),
            end: total,
        });
        let frame = BrowserFrame {
            header,
            body,
            anchor,
            footer: vec![input, fg(&t, "dim", &footer_hint)],
        };
        v.browser.render(&frame, width, height)
    }
}

fn replay(chat: &mut ChatView, entry: &TranscriptEntry) {
    match entry.role() {
        "user" | "assistant" | "toolResult" => chat.add_message(&entry.message),
        _ => {
            let text = crate::chat::user_message_text(&entry.message);
            if !text.trim().is_empty() {
                chat.append_notice_text(&text);
            }
        }
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

fn divider(t: &Theme, width: usize) -> String {
    fg(t, "border", &format!("├{}┤", "─".repeat(width.saturating_sub(2))))
}

fn bottom_border(t: &Theme, width: usize) -> String {
    fg(t, "border", &format!("╰{}╯", "─".repeat(width.saturating_sub(2))))
}

fn boxed_row(t: &Theme, content: &str, width: usize) -> String {
    let inner = width.saturating_sub(4);
    let cut = truncate_to_width(content, inner, "…", false);
    let pad = inner.saturating_sub(visible_width(&cut));
    format!(
        "{} {cut}{} {}",
        fg(t, "border", "│"),
        " ".repeat(pad),
        fg(t, "border", "│")
    )
}
