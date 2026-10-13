// Generates crates/interactive/fixtures/rlm_views.json from packages/coding-agent's live RLM views
// (experimental/rlm-visualizer.ts, rlm-graph.ts, rlm-focus.ts, rlm-pane.ts; TypeScript, this checkout). Each case is a
// snapshot in its wire form (the inspection payloads `agents.status`, `instances.list`, `rlm.pool`, `rlm.frames`,
// `ctx.state` plus the transcript-derived turn and cells), built into an RlmSnapshot through the TypeScript parsers,
// then drawn with PLAIN_STYLE: the graph's nodes, the footer summary, the docked panel, the focus view (default state
// and with a node's details open), the RLM pane for every run, and the wave summary, at several widths. The default
// keybindings supply the focus and pane hints. crates/interactive/tests/rlm_views.rs replays every case.
// Usage: node crates/interactive/fixtures/generate-rlm-views.mjs   (deterministic; output is written next to this file)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const ca = (p) => import(join(here, "../../../packages/coding-agent/src", p));
const V = await ca("experimental/rlm-visualizer.ts");
const G = await ca("experimental/rlm-graph.ts");
const F = await ca("experimental/rlm-focus.ts");
const P = await ca("experimental/rlm-pane.ts");
const { KeybindingsManager } = await ca("core/keybindings.ts");
const keys = new KeybindingsManager({});

const NOW = 1_000_000;

/** The wire form into an RlmSnapshot, through the TypeScript parsers. */
function snapshot(w) {
	const status = V.parseAgentsStatus(w.agentsStatus);
	return {
		now: w.now,
		tasks: status.tasks,
		usage: status.usage,
		limits: status.limits,
		jobs: status.jobs,
		toolCalls: status.toolCalls,
		workflows: status.workflows,
		...(status.truncatedTasks === undefined ? {} : { truncatedTasks: status.truncatedTasks }),
		pool: V.parsePool(w.pool),
		retained: V.parseRetained(w.instances),
		frames: V.parseFrames(w.frames),
		context: V.parseContextState(w.ctx),
		cells: w.cells ?? [],
		turn: w.turn ?? null,
		rootCell: w.rootCell ?? null,
		timing: new Map(Object.entries(w.timing ?? {})),
		progress: new Map(Object.entries(w.progress ?? {}).map(([id, value]) => [id, V.parseProgress(value)])),
		...(w.error === undefined ? {} : { error: w.error }),
	};
}

function graphJson(node) {
	return {
		key: node.key,
		kind: node.kind,
		label: node.label,
		status: node.status,
		startedAt: node.startedAt ?? null,
		endedAt: node.endedAt ?? null,
		tokens: node.tokens ?? null,
		cost: node.cost ?? null,
		progress: node.progress ?? null,
		join: node.join ?? null,
		note: node.note ?? null,
		traceId: node.traceId ?? null,
		details: node.details,
		children: node.children.map(graphJson),
	};
}

function mapFrames() {
	return Array.from({ length: 99 }, (_, index) => ({
		id: `frame-${String(index).padStart(8, "0")}`,
		status: index < 77 ? "complete" : index < 80 ? "incomplete" : "running",
		...(index >= 77 && index < 80 ? { reason: "contract_unmet" } : {}),
		task: "Classify the sentiment of each review",
		spent: { calls: index < 88 ? 1 : 0, tokens: index < 88 ? 400 : 0 },
		startedAt: NOW - 12_000,
		...(index < 80 ? { endedAt: NOW - 1000 } : {}),
		taskId: index < 88 ? `ultron-task-f${index}` : null,
		kind: "map",
		batch: 99,
		callerTaskId: null,
		lane: null,
		budget: { id: "budget-map00001", calls: 120, tokens: null, depth: 1 },
	}));
}

const task = (id, state, extra = {}) => ({ id: `ultron-task-${id}`, definition: "reviewer@1", state, ...extra });

const rich = {
	now: NOW,
	turn: { startedAt: NOW - 72_000, prompt: "Summarize the reviews and benchmark in the background" },
	cells: [
		{
			toolCallId: "call-1",
			code: "h = await rlm.load('reviews.csv')\nprint(h)",
			status: "ok",
			startedAt: NOW - 60_000,
			endedAt: NOW - 58_800,
		},
		{
			toolCallId: "call-2",
			code: "# fan out\nvs = await rlm.map('Classify', h.chunks(4000), contract=str)",
			status: "running",
			startedAt: NOW - 14_000,
		},
	],
	agentsStatus: {
		tasks: [
			task("aaaa1111", "completed", {
				startedAt: NOW - 59_500,
				endedAt: NOW - 59_000,
				cost: 0.012,
				tokens: 3400,
				result: { status: "succeeded", preview: "LGTM with 2 nits" },
			}),
			task("bbbb2222", "running", {
				definition: "rlm-child@1",
				startedAt: NOW - 13_000,
				lane: "ultron.rlm-child.bbbb2222",
			}),
			task("cccc3333", "completed", {
				definition: "planner@1",
				parentId: "ultron-task-bbbb2222",
				startedAt: NOW - 12_000,
				endedAt: NOW - 9000,
			}),
			task("w1", "completed", {
				definition: "coder@1",
				startedAt: NOW - 11_000,
				endedAt: NOW - 8000,
				workflow: { run: "wf-12345678", node: "plan", dependsOn: [], join: "all" },
			}),
			task("w2", "completed", {
				definition: "coder@1",
				startedAt: NOW - 10_000,
				endedAt: NOW - 7000,
				workflow: { run: "wf-12345678", node: "code", dependsOn: [], join: "all" },
			}),
			task("w3", "running", {
				startedAt: NOW - 6000,
				input: '{"diff":"..."}',
				workflow: { run: "wf-12345678", node: "review", dependsOn: ["plan", "code"], join: "all" },
			}),
			task("f0", "completed", { definition: "rlm-frame@1" }),
			task("old1", "completed", { startedAt: NOW - 600_000, endedAt: NOW - 590_000 }),
		],
		jobs: [
			{ id: "job-1", status: "running", command: "sleep 30 && echo done", exitCode: null, startedAt: NOW - 9400, lane: "main" },
			{
				id: "job-2",
				status: "running",
				command: "pytest -q",
				exitCode: null,
				startedAt: NOW - 5000,
				lane: "ultron.rlm-child.bbbb2222",
				tail: "....F..",
			},
		],
		usage: {
			admittedTasks: 5,
			remainingWallMs: 26 * 60_000,
			usage: { totalTokens: 38_200, cost: 0.042 },
			cost: { spentUsd: 0.042, maxCostUsd: null },
		},
		limits: { maxAdmittedTasks: 24, maxWallMs: 30 * 60_000 },
	},
	frames: { frames: mapFrames() },
	pool: {
		live: 3,
		maxLive: 16,
		lanes: [
			{ lane: "main", running: 1, pinnedBy: [] },
			{ lane: "ultron.rlm-child.bbbb2222", running: 1, pinnedBy: [] },
			{ lane: "ultron.planner.x", running: 0, pinnedBy: ["instance-1"] },
		],
		evictions: 2,
		memoryBytes: 212 * 1024 * 1024,
		guards: [{ name: "Loki", checks: 4, blocked: 1, afterChecks: 2, turnMs: 85, totalMs: 1430 }],
	},
};

const reference = {
	now: NOW,
	turn: { startedAt: NOW - 30_000, prompt: "Linear gates integration evidence" },
	agentsStatus: {
		tasks: [
			["navigation", "aaaa1111", {
				input: '{"prompt":"Map shell navigation integration against HEAD, then list every route"}',
				model: "cliproxyapi/gpt-6-astra",
				turns: 1,
				toolCallCount: 1,
				lastText: "I read this as fact collection - I'll map the navigation contract first",
			}],
			["gate-wiring", "bbbb2222", {
				input: '{"prompt":"Collect the changed component props, imports, and gates"}',
				model: "cliproxyapi/gpt-6-astra",
				turns: 1,
				toolCallCount: 1,
				lastText: "I read this as evidence collection - I'll compare the scoped wiring",
			}],
			["test-coverage", "cccc3333", { input: '{"prompt":"Check test coverage"}' }],
		].map(([node, id, fields]) => ({
			id: `ultron-task-${id}`,
			definition: "rlm-child@1",
			state: "running",
			startedAt: NOW - 21_000,
			lane: `ultron.rlm-child.${id}`,
			workflow: { run: "wf-01a07520", node, dependsOn: [], join: "all" },
			...fields,
		})),
		workflows: [
			{
				run: "wf-01a07520",
				startedAt: NOW - 21_000,
				nodes: [
					{ id: "navigation", definition: "rlm-child@1", dependsOn: [], join: "all" },
					{ id: "gate-wiring", definition: "rlm-child@1", dependsOn: [], join: "all" },
					{ id: "test-coverage", definition: "rlm-child@1", dependsOn: [], join: "all" },
					{
						id: "verify-evidence",
						definition: "rlm-child@1",
						dependsOn: ["navigation", "gate-wiring", "test-coverage"],
						join: "all",
					},
					{ id: "skipped-one", definition: "x@1", dependsOn: ["verify-evidence"], join: "any", status: "skipped", reason: "gate closed" },
				],
			},
		],
	},
};

const turnRuns = {
	now: NOW,
	turn: { startedAt: NOW - 10_000, prompt: "compare the drafts" },
	cells: [
		{ toolCallId: "call-1", code: "x = 1", status: "ok", startedAt: NOW - 9500, endedAt: NOW - 8000 },
		{ toolCallId: "call-2", code: "x = 2\ny = 3", status: "running", startedAt: NOW - 3000 },
	],
	agentsStatus: {
		tasks: [
			{ id: "ultron-task-c1", definition: "rlm-child@1", state: "running", startedAt: NOW - 9000, input: '{"prompt":"one"}' },
			{
				id: "ultron-task-c2",
				definition: "rlm-child@1",
				state: "completed",
				startedAt: NOW - 9000,
				endedAt: NOW - 4000,
				result: { status: "succeeded", preview: "two done" },
			},
			{ id: "ultron-task-t3", definition: "reviewer@1", state: "admitted", startedAt: NOW - 8900 },
			{ id: "ultron-task-n1", definition: "rlm-child@1", state: "running", parentId: "ultron-task-c1", startedAt: NOW - 5000 },
		],
		toolCalls: [
			{ id: "tc1", name: "web_search", label: "web_search foo", status: "completed", source: "repl", startedAt: NOW - 8800, endedAt: NOW - 8700, preview: "3 hits" },
			{ id: "tc2", name: "web_fetch", label: "web_fetch bar", status: "running", source: "repl", startedAt: NOW - 8600 },
			{ id: "tc3", name: "read", label: "read notes.md", status: "failed", source: "native", startedAt: NOW - 2000, endedAt: NOW - 1900, error: "ENOENT" },
		],
		usage: { admittedTasks: 4, turns: { turns: 3, tokens: 150_000, maxTotalTokens: 200_000, maxTotalTurns: 10 }, cost: { spentUsd: 0.9, maxCostUsd: 1 } },
	},
};

const failed = {
	now: NOW,
	turn: { startedAt: NOW - 5000, prompt: "a\nmultiline   prompt that is quite long and keeps going past sixty characters for sure" },
	rootCell: { toolCallId: "call-9", code: "raise ValueError('x')", status: "error", startedAt: NOW - 4000, endedAt: NOW - 3500, output: "Traceback\nValueError: x" },
	agentsStatus: {
		tasks: [
			{ id: "ultron-task-bad1", definition: "coder@1", state: "failed", startedAt: NOW - 3900, endedAt: NOW - 3600, result: { status: "failed", error: "budget exhausted\nafter 3 turns" } },
			{ id: "ultron-task-ok2", definition: "coder@1", state: "completed", result: { status: "succeeded", value: { n: 2 } } },
			{ id: "ultron-task-cut3", definition: "coder@1", state: "interrupted" },
			{ id: "ultron-instance-ret4", definition: "keeper@2", state: "running", worktree: "ultron/ab12/fix · merged", fetch: "await x()" },
			{ id: "ultron-task-bg5", definition: "background-job@1", state: "cancelled" },
		],
		jobs: [
			{ id: "job-x", status: "completed", command: "make", exitCode: 2, startedAt: NOW - 3800, endedAt: NOW - 3700, lane: "main" },
			{ id: "job-y", status: "timed_out", command: "sleep 99", exitCode: null, startedAt: NOW - 3800 },
		],
		usage: { admittedTasks: 22, remainingWallMs: 60_000, rootId: "root-0199abcdef12", startedAt: NOW - 6000 },
		limits: { maxAdmittedTasks: 24, maxWallMs: 600_000, maxCostUsd: 2 },
		truncatedTasks: 7,
	},
	instances: [{ task_id: "ultron-instance-ret4", state: "open", invocations: [{ task_id: "ultron-task-ok2" }] }],
	progress: { "ultron-instance-ret4": { classification: "advancing", receipts: [{}, {}] } },
	timing: { "ultron-task-cut3": { startedAt: NOW - 3000, endedAt: NOW - 1000 } },
	ctx: {
		forgotten: [{ id: "e1", kind: "toolResult", preview: "big output", reason: "done" }, { id: "e2", kind: "cell", preview: "x", source: "model" }],
		forgottenCount: 2,
		collapsedCount: 1,
		pinned: [{ id: "p1", kind: "note", preview: "remember the schema" }],
		pinnedCount: 1,
		notes: 3,
	},
	pool: { live: 30, maxLive: 32, lanes: [], memoryBytes: 3 * 1024 * 1024 * 1024, memoryCapBytes: 512 * 1024 * 1024 },
	error: "worker restarted",
};

const inference = {
	now: NOW,
	agentsStatus: {
		tasks: [
			task("frame1", "completed", { definition: "rlm-frame@1" }),
			task("loose1", "completed", { definition: "rlm-frame@1", parentId: "ultron-task-parent" }),
			task("loose2", "failed", { definition: "rlm-frame@1", parentId: "ultron-task-parent" }),
			task("parent", "running", { startedAt: NOW - 100 }),
			task("a", "running", { parentId: "ultron-task-b" }),
			task("b", "running", { parentId: "ultron-task-a" }),
		],
	},
	frames: {
		frames: [
			{
				id: "frame-aaaaaaaa1",
				status: "complete",
				task: "outer",
				spent: { calls: 1, tokens: 10 },
				startedAt: NOW - 50,
				endedAt: NOW - 10,
				taskId: "ultron-task-frame1",
				kind: "infer",
				batch: 1,
				callerTaskId: null,
				budget: { id: "budget-outer", calls: 3, tokens: 5000, depth: 2 },
			},
			{
				id: "frame-bbbbbbbb2",
				status: "incomplete",
				reason: "budget_exhausted",
				task: "inner",
				spent: { calls: 1, tokens: 5 },
				startedAt: NOW - 40,
				endedAt: NOW - 20,
				taskId: "ultron-task-frame2",
				kind: "infer",
				batch: 1,
				callerTaskId: "ultron-task-frame1",
				budget: { id: "budget-inner", calls: 1, tokens: null, depth: 1 },
			},
			{ id: "frame-cccccccc3", status: "running", task: "queued", spent: { calls: 0, tokens: 0 }, taskId: null },
		],
	},
};

const idle = {
	now: NOW,
	agentsStatus: { tasks: [], usage: { admittedTasks: 0, turns: { turns: 0, tokens: 0 } }, limits: { maxAdmittedTasks: 24, maxTotalTokens: 10_000_000 } },
	pool: { live: 0, maxLive: 16, lanes: [], evictions: 0 },
};

const wires = { idle, rich, reference, turnRuns, failed, inference };

const plain = (lines) => lines;
const cases = [];
for (const [name, wire] of Object.entries(wires)) {
	const snap = snapshot(wire);
	const graph = G.buildRlmGraph(snap);
	const rows = G.layoutGraph(graph);
	const firstLeaf = rows.find((row) => row.node.children.length === 0 && row.depth > 0) ?? rows.at(-1);
	const detailed = {
		...F.initialFocusState(),
		selectedKey: firstLeaf.node.key,
		details: new Set([firstLeaf.node.key, ...(rows[1] ? [rows[1].node.key] : [])]),
	};
	const runs = P.buildDagRuns(snap);
	const out = {
		name,
		wire,
		graph: graphJson(graph),
		rows: rows.map((row) => ({
			key: row.node.key,
			depth: row.depth,
			rails: row.rails,
			last: row.last,
			collapsed: row.collapsed,
			hidden: row.hidden,
			parentKey: row.parentKey ?? null,
		})),
		footer: Object.fromEntries([200, 120, 40, 20].map((w) => [w, G.renderRlmFooter(snap, w, { focusKey: "alt+g", spinnerFrame: 3 }) ?? null])),
		dock: Object.fromEntries([100, 80, 40, 20].map((w) => [w, plain(G.renderRlmDock(snap, w, { focusKey: "alt+g", spinnerFrame: 3, maxRows: 8 }))])),
		dockDefault: G.renderRlmDock(snap, 80, {}),
		focus: Object.fromEntries(
			[
				[80, 20],
				[40, 12],
				[120, 30],
			].map(([w, h]) => [`${w}x${h}`, F.renderFocusView(snap, F.initialFocusState(), w, h, { hints: F.focusHints(keys), spinnerFrame: 1 }).lines]),
		),
		focusDetails: F.renderFocusView(snap, detailed, 80, 16, { hints: F.focusHints(keys), spinnerFrame: 1 }).lines,
		focusDetailsKey: firstLeaf.node.key,
		focusDetailsOpen: [...detailed.details],
		runs: runs.map((run) => ({ key: run.key, kind: run.kind, id: run.id, title: run.title, waves: run.waves, edges: run.edges, status: run.status })),
		pane: Object.fromEntries(
			[
				[80, 70],
				[60, 40],
				[44, 24],
				[12, 6],
			].map(([w, h]) => [`${w}x${h}`, P.renderRlmPane(runs, P.initialDagState(), w, h, { hints: P.rlmPaneHints(keys), now: snap.now, taskCount: snap.tasks.length, focused: false, focusHint: "alt+w focus" }).lines]),
		),
		paneRuns: runs.map((run) => P.renderRlmPane(runs, { ...P.initialDagState(), runKey: run.key }, 60, 50, { now: snap.now }).lines),
		waveSummary: Object.fromEntries([100, 60].map((w) => [w, P.renderWaveSummary(runs, w, { now: snap.now })])),
		waveSummary3: P.renderWaveSummary(runs, 80, { now: snap.now, maxNodes: 3 }),
		paneWork: P.paneWorkRuns(runs).map((run) => run.key),
		paneWorkSince: P.paneWorkRuns(runs, NOW - 30_000).map((run) => run.key),
	};
	cases.push(out);
}

// The turn and cells from a transcript (extractTurn / extractRootCell), as the TUI reads them.
const call = (id, code, timestamp) => ({
	message: { role: "assistant", timestamp, content: [{ type: "toolCall", id, name: "rlm", arguments: { code } }] },
});
const transcript = [
	{ message: { role: "user", content: "old question", timestamp: 1 } },
	call("c0", "old", 2),
	{ message: { role: "user", content: [{ type: "text", text: "count  the\nrows" }, { type: "text", text: "now" }], timestamp: 100 } },
	call("c1", "print(1)", 105),
	{ message: { role: "toolResult", toolName: "rlm", toolCallId: "c1", isError: true, timestamp: 109, content: [{ type: "text", text: "NameError" }] } },
	call("c2", "print(2)", 110),
	{ message: { role: "toolResult", toolName: "rlm", toolCallId: "c2", isError: false, timestamp: 115, content: [{ type: "text", text: "2" }] } },
	call("c3", "print(3)", 116),
];
const running = [{ status: "running", toolCallId: "c3", toolName: "rlm", args: { code: "print(3)" } }];
const extract = [
	{ operation: running, now: 120 },
	{ operation: null, now: 130 },
].map(({ operation, now }) => {
	const view = { transcript, operation: operation === null ? null : { runningTools: operation } };
	const turn = V.extractTurn(view, new V.RlmClock(), now);
	const root = V.extractRootCell(view, new V.RlmClock(), now);
	return { running: operation, now, turn: turn.turn, cells: turn.cells, rootCell: root };
});

const formats = {
	duration: [0, 250, 999.6, 4200, 9999, 10_000, 59_999, 60_000, 75_000, 600_000, 18 * 60_000, 3_600_000, 3 * 3_600_000 + 5 * 60_000].map((ms) => [ms, V.formatDuration(ms)]),
	tokens: [0, 999, 1000, 9999, 10_000, 999_499, 1_000_000, 10_000_000].map((n) => [n, G.formatTokens(n)]),
	cost: [0, 0.001, 0.0099, 0.01, 0.042, 1, 12.345].map((n) => [n, G.formatCost(n)]),
	bar: [[0, 5], [0.01, 5], [0.5, 6], [0.99, 5], [1, 5], [1.5, 4], [-1, 4]].map(([f, c]) => [f, c, G.bar(f, c)]),
	windows: [[5, 10, 4], [40, 10, 3], [40, 10, 30], [40, 10, 39], [12, 1, 5]].map(([t, m, f]) => [t, m, f, G.windowRows(t, m, f)]),
	waves: [[3, 43, 0], [3, 60, 0], [3, 80, 0], [1, 80, 0], [1, 80, 45], [1, 43, 60], [5, 200, 0], [2, 10, 0]].map(([c, w, n]) => [c, w, n, P.waveGeometry(c, w, n)]),
	prompts: ['{"prompt":"Summarize the file"}', '{"prompt":"Summarize the \\"big\\" fi', '{"task":"cut\\nline', '{"n":2}', '"just text"', "plain words", "  "].map((input) => [input, P.promptOf(input) ?? null]),
	shortIds: ["ultron-task-abcdef123456", "ultron-instance-xyz", "plain-id-0123456789"].map((id) => [id, V.shortId(id)]),
	focusHints: F.focusHints(keys),
	paneHints: P.rlmPaneHints(keys),
};

writeFileSync(join(here, "rlm_views.json"), `${JSON.stringify({ now: NOW, cases, extract, formats }, null, 1)}\n`);
