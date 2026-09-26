import { visibleWidth } from "@ultron/tui";
import { describe, expect, test } from "vitest";
import {
	applyFocusAction,
	type FocusState,
	focusHints,
	initialFocusState,
	renderFocusView,
	selectedRow,
} from "../src/experimental/rlm-focus.ts";
import {
	bar,
	budgetGauges,
	buildRlmGraph,
	frameTraceDetails,
	type GraphNode,
	layoutGraph,
	renderGauges,
	renderGraphRow,
	renderKernelStrip,
	renderRlmDock,
	renderRlmFooter,
	windowRows,
} from "../src/experimental/rlm-graph.ts";
import {
	extractRootCell,
	extractTurn,
	formatDuration,
	PLAIN_STYLE,
	parseAgentsStatus,
	parseFrames,
	parsePool,
	parseRetained,
	RlmClock,
	type RlmFrame,
	type RlmSnapshot,
	type RlmTask,
} from "../src/experimental/rlm-visualizer.ts";

const NOW = 1_000_000;

/** Truncation wraps the ellipsis in a reset; compare visible text. */
function plain(line: string): string {
	return line.replace(/\u001b\[[0-9;]*m/g, "");
}

function task(id: string, state: string, extra: Partial<RlmTask> = {}): RlmTask {
	return { id: `ultron-task-${id}`, definition: "reviewer@1", state, ...extra };
}

/** 99 frames of one `rlm.map`: 77 complete, 3 incomplete, 8 running, 11 queued. */
function mapFrames(): RlmFrame[] {
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

/** A turn with two cells: a finished load, then a running map with a child, a workflow and two jobs. */
function fixture(overrides: Partial<RlmSnapshot> = {}): RlmSnapshot {
	return {
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
			// A frame task is shown through its frame, never as a task.
			task("f0", "completed", { definition: "rlm-frame@1" }),
			// Finished before this turn: goes under "earlier".
			task("old1", "completed", { startedAt: NOW - 600_000, endedAt: NOW - 590_000 }),
		],
		jobs: [
			{
				id: "job-1",
				status: "running",
				command: "sleep 30 && echo done",
				exitCode: null,
				startedAt: NOW - 9400,
				lane: "main",
			},
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
		frames: mapFrames(),
		usage: {
			admittedTasks: 5,
			remainingWallMs: 26 * 60_000,
			usage: { totalTokens: 38_200, cost: 0.042 },
			cost: { spentUsd: 0.042, maxCostUsd: null },
		},
		limits: { maxAdmittedTasks: 24, maxWallMs: 30 * 60_000 },
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
		},
		...overrides,
	};
}

function keys(node: GraphNode): string[] {
	return [node.key, ...node.children.flatMap(keys)];
}

describe("RLM graph model", () => {
	test("attaches work to the cell that started it, children to parents, jobs to their lane's task", () => {
		const graph = buildRlmGraph(fixture());
		expect(graph.kind).toBe("turn");
		expect(graph.status).toBe("running");
		expect(graph.children.map((node) => node.key)).toEqual(["earlier", "cell:call-1", "cell:call-2"]);
		const [earlier, first, second] = graph.children;
		expect(earlier!.children.map((node) => node.key)).toEqual(["task:ultron-task-old1"]);
		expect(first!.children.map((node) => node.key)).toEqual(["task:ultron-task-aaaa1111"]);
		expect(second!.children.map((node) => node.key)).toEqual([
			"task:ultron-task-bbbb2222",
			"frames:budget-map00001",
			"workflow:wf-12345678",
			"job:job-1",
		]);
		const child = second!.children[0]!;
		expect(child.children.map((node) => node.key)).toEqual(["task:ultron-task-cccc3333", "job:job-2"]);
		expect(keys(graph)).not.toContain("task:ultron-task-f0");
	});

	test("fans one rlm.map out as a single node with progress and budget details", () => {
		const fanout = buildRlmGraph(fixture()).children[2]!.children[1]!;
		expect(fanout).toMatchObject({
			kind: "fanout",
			label: "rlm.map 99 frames",
			status: "running",
			progress: { total: 99, done: 77, incomplete: 3, running: 8, pending: 11, failed: 0 },
			tokens: 88 * 400,
		});
		expect(fanout.children).toEqual([]);
		expect(fanout.details).toContainEqual(["budget", "map00001 · calls 88/120 · tokens 35200/∞ · depth 1"]);
		expect(fanout.details).toContainEqual(["fetch", 'await rlm.frames("frame-00000077")']);
	});

	test("groups workflow members under one node and marks joins", () => {
		const workflow = buildRlmGraph(fixture()).children[2]!.children[2]!;
		expect(workflow.label).toBe("workflow 12345678 3 nodes");
		expect(workflow.progress).toMatchObject({ total: 3, done: 2, running: 1 });
		expect(workflow.note).toBe("1 join");
		expect(workflow.children.map((node) => node.label)).toEqual([
			"plan coder@1 w1",
			"code coder@1 w2",
			"review reviewer@1 w3",
		]);
		expect(workflow.children[2]!.join).toBe("⇐ plan+code (all)");
	});

	test("single inference frames, nested frames, and frame tasks without a frame listing", () => {
		const graph = buildRlmGraph({
			now: NOW,
			tasks: [
				task("frame1", "completed", { definition: "rlm-frame@1" }),
				task("loose1", "completed", { definition: "rlm-frame@1", parentId: "ultron-task-parent" }),
				task("loose2", "failed", { definition: "rlm-frame@1", parentId: "ultron-task-parent" }),
				task("parent", "running", { startedAt: NOW - 100 }),
			],
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
					budget: { id: "budget-outer", calls: 3, tokens: null, depth: 2 },
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
			],
		});
		const outer = graph.children.find((node) => node.key === "frames:budget-outer")!;
		expect(outer).toMatchObject({
			kind: "infer",
			label: "rlm.infer aaaaaaaa",
			status: "done",
			traceId: "frame-aaaaaaaa1",
		});
		expect(outer.children.map((node) => [node.label, node.status, node.note])).toEqual([
			["rlm.infer bbbbbbbb", "incomplete", "budget_exhausted"],
		]);
		const parent = graph.children.find((node) => node.key === "task:ultron-task-parent")!;
		expect(parent.children.map((node) => [node.label, node.progress?.done, node.progress?.failed])).toEqual([
			["frames 2", 1, 1],
		]);
	});

	test("survives parent cycles and missing parents", () => {
		const graph = buildRlmGraph({
			now: NOW,
			tasks: [
				task("a", "running", { parentId: "ultron-task-b" }),
				task("b", "running", { parentId: "ultron-task-a" }),
				task("lost", "running", { parentId: "ultron-task-missing" }),
			],
		});
		expect(new Set(keys(graph))).toEqual(
			new Set(["turn", "task:ultron-task-a", "task:ultron-task-b", "task:ultron-task-lost"]),
		);
	});
});

describe("RLM graph layout", () => {
	test("draws box connectors, fan-out bars, joins and right-aligned time and spend at 80 columns", () => {
		const graph = buildRlmGraph(fixture());
		const lines = layoutGraph(graph).map((row) =>
			plain(renderGraphRow(row, 80, { style: PLAIN_STYLE, now: NOW, spinner: "⠋" })),
		);
		expect(lines).toEqual([
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
		]);
		// Rows with a time column are right-aligned to the full width.
		for (const line of lines) expect(visibleWidth(line)).toBeLessThanOrEqual(80);
		expect(lines.filter((line) => visibleWidth(line) === 80)).toHaveLength(12);
	});

	test("collapses finished subtrees automatically; an override opens them", () => {
		const graph = buildRlmGraph(fixture());
		const auto = layoutGraph(graph);
		const cell = auto.find((row) => row.node.key === "cell:call-1")!;
		expect(cell.collapsed).toBe(true);
		expect(cell.hidden).toBe(1);
		expect(auto.some((row) => row.node.key === "task:ultron-task-aaaa1111")).toBe(false);
		const opened = layoutGraph(graph, new Map([["cell:call-1", false]]));
		const child = opened.find((row) => row.node.key === "task:ultron-task-aaaa1111")!;
		expect(renderGraphRow(child, 80, { style: PLAIN_STYLE, now: NOW, spinner: "⠋" })).toBe(
			"│  └─ ✓ agent reviewer@1 aaaa1111 → LGTM with 2 nits        500ms 3.4k tok $0.01",
		);
		// A finished cell stays open while a job it started still runs.
		const live = fixture({
			cells: [fixture().cells![0]!],
			tasks: [],
			frames: [],
			jobs: [
				{
					id: "job-9",
					status: "running",
					command: "sleep 90",
					exitCode: null,
					startedAt: NOW - 59_000,
					lane: "main",
				},
			],
		});
		expect(layoutGraph(buildRlmGraph(live)).map((row) => [row.node.key, row.collapsed])).toEqual([
			["turn", false],
			["cell:call-1", false],
			["job:job-9", false],
		]);
		// A running node collapses only when asked.
		const folded = layoutGraph(graph, new Map([["cell:call-2", true]]));
		expect(folded.at(-1)).toMatchObject({ collapsed: true, hidden: 9 });
	});

	test("stays readable at 40 columns and degrades at 20", () => {
		const graph = buildRlmGraph(fixture());
		const rows = layoutGraph(graph);
		const at40 = rows.map((row) => plain(renderGraphRow(row, 40, { style: PLAIN_STYLE, now: NOW, spinner: "⠋" })));
		expect(at40[7]).toBe("   ├─ ⠋ rlm.map 99 frames ▰▰▰▰▱ 80/… 12s");
		expect(at40[11]).toBe("   │  └─ ⠋ review reviewer@1 w3 ⇐ … 6.0s");
		for (const width of [40, 20, 8]) {
			for (const row of rows)
				expect(
					visibleWidth(renderGraphRow(row, width, { style: PLAIN_STYLE, now: NOW, spinner: "⠋" })),
				).toBeLessThanOrEqual(width);
		}
	});

	test("bars and windows are bounded", () => {
		expect(bar(0, 5)).toBe("▱▱▱▱▱");
		expect(bar(0.01, 5)).toBe("▰▱▱▱▱");
		expect(bar(0.99, 5)).toBe("▰▰▰▰▱");
		expect(bar(1, 5)).toBe("▰▰▰▰▰");
		expect(windowRows(5, 10, 4)).toEqual({ start: 0, end: 5 });
		expect(windowRows(40, 10, 3)).toEqual({ start: 0, end: 10 });
		expect(windowRows(40, 10, 30)).toEqual({ start: 22, end: 32 });
		expect(windowRows(40, 10, 39)).toEqual({ start: 30, end: 40 });
	});
});

describe("RLM gauges and kernel strip", () => {
	test("budget gauges for tasks, wall, tokens, cost and the active frame budget", () => {
		const snapshot = fixture({
			usage: {
				admittedTasks: 20,
				remainingWallMs: 26 * 60_000,
				usage: { totalTokens: 38_200 },
				cost: { spentUsd: 0.9, maxCostUsd: 1 },
				turns: { turns: 3, tokens: 150_000, maxTotalTokens: 200_000, maxTotalTurns: 10 },
			},
		});
		const gauges = budgetGauges(snapshot, buildRlmGraph(snapshot));
		expect(gauges.map((gauge) => [gauge.label, gauge.text])).toEqual([
			["tasks", "20/24"],
			["wall", "4m/30m"],
			["tokens", "150k/200k"],
			["turns", "3/10"],
			["cost", "$0.90/$1.00"],
			["calls", "88/120"],
			["depth", "1"],
		]);
		const marked = renderGauges(
			gauges,
			80,
			{ fg: (color, text) => `<${color}>${text}</>`, bold: (text) => text },
			10,
		);
		expect(marked[0]).toContain("<dim>tasks</> <warning>▰▰▰▰▰▱</>");
		expect(marked.join(" ")).toContain("<dim>cost</> <error>▰▰▰▰▰▱</>");
		const unstyled = renderGauges(gauges, 80, PLAIN_STYLE, 2);
		expect(unstyled).toEqual([
			"tasks ▰▰▰▰▰▱ 20/24  wall ▰▱▱▱▱▱ 4m/30m  tokens ▰▰▰▰▰▱ 150k/200k",
			"turns ▰▰▱▱▱▱ 3/10  cost ▰▰▰▰▰▱ $0.90/$1.00  calls ▰▰▰▰▱▱ 88/120  depth 1",
		]);
		for (const line of renderGauges(gauges, 40, PLAIN_STYLE, 2)) expect(visibleWidth(line)).toBeLessThanOrEqual(40);
	});

	test("kernel strip shows live, busy, pinned, evicted and tree memory", () => {
		expect(renderKernelStrip(fixture(), 80, PLAIN_STYLE)).toBe(
			"kernels ■■◆············· 3/16 · 2 busy · 1 pinned · 2 evicted · mem 212 MiB",
		);
		expect(plain(renderKernelStrip(fixture(), 40, PLAIN_STYLE)!)).toBe("kernels 3/16 · 2 busy · 1 pinned · 2 ev…");
		expect(renderKernelStrip(fixture({ pool: null }), 80, PLAIN_STYLE)).toBeUndefined();
	});
});

describe("RLM views", () => {
	test("footer summary: one line while something runs, nothing when idle", () => {
		expect(renderRlmFooter(fixture(), 120, { focusKey: "alt+r" })).toBe(
			"◆ rlm ⠋ turn 1m12s · cell 2 14s · map ▰▰▰▰▱ 80/99 · 6 tasks (2 active) · 2 jobs · $0.04 · alt+r graph",
		);
		expect(visibleWidth(renderRlmFooter(fixture(), 40)!)).toBeLessThanOrEqual(40);
		expect(renderRlmFooter({ now: NOW, tasks: [task("x", "completed")] }, 80)).toBeUndefined();
	});

	test("docked panel: header, gauges, pinned turn row, windowed graph, kernel strip", () => {
		const lines = renderRlmDock(fixture(), 80, { focusKey: "alt+r", maxRows: 8 }).map(plain);
		expect(lines).toEqual([
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
		]);
		for (const width of [80, 40, 20]) {
			for (const line of renderRlmDock(fixture(), width)) expect(visibleWidth(line)).toBeLessThanOrEqual(width);
		}
		const failed = renderRlmDock(
			fixture({ error: "Ultron RLM host is not initialized", cells: [], tasks: [], frames: [], jobs: [] }),
			80,
		);
		expect(failed[0]).toBe("RLM idle 0 nodes · inspection failed: Ultron RLM host is not initialized");
	});

	test("docked panel keeps ctx.state lines", () => {
		const lines = renderRlmDock(
			fixture({
				context: {
					forgotten: [{ id: "e1", kind: "toolResult", preview: "big output", reason: "done" }],
					forgottenCount: 1,
					collapsedCount: 0,
					pinned: [],
					pinnedCount: 0,
					notes: 1,
				},
			}),
			100,
		);
		expect(lines).toContain("context: 1 forgotten · 0 collapsed · 0 pinned · 1 notes");
	});

	test("frame traces become detail lines", () => {
		expect(
			frameTraceDetails({
				model: "p/m",
				value: '"positive"',
				views: [{}, {}],
				attempts: [{}],
				remaining: { calls: 2, tokens: null, depth: 0 },
			}),
		).toEqual([
			["model", "p/m"],
			["value", '"positive"'],
			["views", "2 context views"],
			["attempts", "1"],
			["remaining", "calls 2 · tokens ∞ · depth 0"],
		]);
		expect(frameTraceDetails("nope")).toEqual([]);
	});
});

describe("RLM focus navigation", () => {
	const rows = () => layoutGraph(buildRlmGraph(fixture()));

	test("moves with clamping and keeps the selection by key across refreshes", () => {
		let state = initialFocusState();
		const layout = rows();
		state = applyFocusAction(state, layout, "up");
		expect(selectedRow(state, layout)).toBe(0);
		state = applyFocusAction(state, layout, "down");
		state = applyFocusAction(state, layout, "down");
		expect(state.selectedKey).toBe("cell:call-1");
		state = applyFocusAction(state, layout, "pageDown", 100);
		expect(state.selectedKey).toBe("job:job-1");
		// The graph grew above the selection: the key still wins over the index.
		const extra = {
			id: "job-0",
			status: "completed",
			command: "ls",
			exitCode: 0,
			startedAt: NOW - 13_500,
			endedAt: NOW - 13_400,
			lane: "main",
		};
		const grown = layoutGraph(buildRlmGraph(fixture({ jobs: [...fixture().jobs!, extra] })));
		expect(grown[selectedRow(state, grown)]!.node.key).toBe("job:job-1");
		state = applyFocusAction(state, layout, "home");
		expect(state.selectedKey).toBe("turn");
		// A vanished node falls back to the nearest index.
		const gone: FocusState = { ...state, selectedKey: "task:nope", selectedIndex: 999 };
		expect(selectedRow(gone, layout)).toBe(layout.length - 1);
	});

	test("enter toggles details; c toggles a subtree, and on a leaf folds the parent", () => {
		let state = initialFocusState();
		let layout = rows();
		state = { ...state, selectedKey: "task:ultron-task-bbbb2222" };
		state = applyFocusAction(state, layout, "details");
		expect(state.details.has("task:ultron-task-bbbb2222")).toBe(true);
		state = applyFocusAction(state, layout, "collapse");
		layout = layoutGraph(buildRlmGraph(fixture()), state.collapse);
		expect(layout.find((row) => row.node.key === "task:ultron-task-bbbb2222")).toMatchObject({
			collapsed: true,
			hidden: 2,
		});
		state = applyFocusAction(state, layout, "collapse");
		layout = layoutGraph(buildRlmGraph(fixture()), state.collapse);
		const leaf = layout.findIndex((row) => row.node.key === "job:job-2");
		state = applyFocusAction({ ...state, selectedKey: "job:job-2", selectedIndex: leaf }, layout, "collapse");
		expect(state.selectedKey).toBe("task:ultron-task-bbbb2222");
		expect(state.collapse.get("task:ultron-task-bbbb2222")).toBe(true);
		state = applyFocusAction(state, layout, "details");
		expect(state.details.has("task:ultron-task-bbbb2222")).toBe(false);
	});

	test("focus view fills its height, shows details inline and scrolls to the selection", () => {
		const state: FocusState = {
			...initialFocusState(),
			selectedKey: "task:ultron-task-w3",
			details: new Set(["task:ultron-task-w3"]),
		};
		const { lines } = renderFocusView(fixture(), state, 80, 20, { hints: "up/k down/j move" });
		expect(lines).toHaveLength(20);
		expect(lines[0]).toBe("RLM graph ⠋ running 13 nodes · 7 active");
		expect(lines[1]).toBe("up/k down/j move");
		const selected = lines.find((line) => line.startsWith("▶"))!;
		expect(selected).toContain("review reviewer@1 w3 ⇐ plan+code (all)");
		const text = lines.join("\n");
		expect(text).toContain("┆ id        ultron-task-w3");
		expect(text).toContain('┆ input     {"diff":"..."}');
		expect(text).toContain("┆ workflow  wf-12345678 node review after plan, code (all)");
		expect(text).toContain("↑");
		for (const width of [80, 40]) {
			const view = renderFocusView(fixture(), state, width, 12);
			expect(view.lines).toHaveLength(12);
			for (const line of view.lines) expect(visibleWidth(line)).toBeLessThanOrEqual(width);
		}
	});

	test("hints come from the live keybindings", () => {
		const bindings: Record<string, string[]> = {
			"app.rlm.graph.up": ["up", "k"],
			"app.rlm.graph.down": ["down", "j"],
			"app.rlm.graph.details": ["enter"],
			"app.rlm.graph.collapse": ["c"],
			"app.rlm.graph.exit": ["escape"],
		};
		expect(
			focusHints({ getKeys: ((id: string) => bindings[id] ?? []) as never, matches: (() => false) as never }),
		).toBe("up/k down/j move · enter details · c collapse · escape back");
	});
});

describe("RLM inspection parsing", () => {
	test("parses payloads defensively, including graph fields", () => {
		const parsed = parseAgentsStatus({
			definitions: [],
			tasks: [
				{ id: "t1", definition: "a@1", state: "running", parentId: "p" },
				{
					id: "t2",
					definition: "b@1",
					state: "completed",
					lane: "ultron.b.t2",
					startedAt: 5,
					endedAt: 9,
					cost: 0.01,
					tokens: 120,
					input: "{}",
					fetch: 'await agents.result("t2")',
					workflow: { run: "wf-1", node: "n", dependsOn: ["x", 3], join: "any" },
					result: { status: "succeeded", preview: "ok" },
				},
				{ bogus: true },
			],
			usage: { admittedTasks: 1, limits: { maxAdmittedTasks: 4, maxWallMs: null } },
			limits: null,
			jobs: [{ id: "job-1", status: "running", command: "ls", lane: "main", tail: "a" }],
			truncatedTasks: 3,
		});
		expect(parsed.tasks[0]).toEqual({ id: "t1", definition: "a@1", state: "running", parentId: "p" });
		expect(parsed.tasks[1]).toEqual({
			id: "t2",
			definition: "b@1",
			state: "completed",
			lane: "ultron.b.t2",
			startedAt: 5,
			endedAt: 9,
			cost: 0.01,
			tokens: 120,
			input: "{}",
			fetch: 'await agents.result("t2")',
			workflow: { run: "wf-1", node: "n", dependsOn: ["x"], join: "any" },
			result: { status: "succeeded", preview: "ok" },
		});
		expect(parsed.tasks).toHaveLength(2);
		expect(parsed.truncatedTasks).toBe(3);
		expect(parsed.jobs[0]).toMatchObject({ lane: "main", tail: "a" });
		expect(parsed.limits).toEqual({ maxAdmittedTasks: 4, maxWallMs: null });
		expect(parseAgentsStatus(null).tasks).toEqual([]);
		expect(
			parseRetained([
				{ task_id: "a", state: "open", invocations: [{ task_id: "b" }] },
				{ task_id: "c", state: "closed", invocations: [] },
			]),
		).toEqual(new Set(["a", "b"]));
		expect(
			parsePool({
				live: 1,
				maxLive: 2,
				lanes: [{ lane: "main", running: 1, pinnedBy: ["x"], memoryBytes: 5 }],
				memoryBytes: 5,
				memoryCapBytes: 10,
			}),
		).toEqual({
			live: 1,
			maxLive: 2,
			lanes: [{ lane: "main", running: 1, pinnedBy: ["x"], memoryBytes: 5 }],
			memoryBytes: 5,
			memoryCapBytes: 10,
		});
		expect(parsePool("nope")).toBeNull();
		expect(
			parseFrames({
				frames: [
					{
						id: "frame-1",
						status: "running",
						task: "t",
						spent: { calls: 1, tokens: 2 },
						taskId: null,
						kind: "map",
						batch: 3,
						callerTaskId: null,
						lane: null,
						budget: { id: "budget-1", calls: 5, tokens: null, depth: 1 },
					},
					{ id: 3 },
				],
			}),
		).toEqual([
			{
				id: "frame-1",
				status: "running",
				task: "t",
				spent: { calls: 1, tokens: 2 },
				taskId: null,
				callerTaskId: null,
				kind: "map",
				batch: 3,
				budget: { id: "budget-1", calls: 5, tokens: null, depth: 1 },
			},
		]);
	});

	test("clock uses admission time, freezes on completion, and omits time for tasks first seen finished", () => {
		const clock = new RlmClock();
		const running = [task("a", "running"), task("b", "completed")];
		const first = clock.timings(running, { reservations: [{ taskId: "ultron-task-a", admittedAt: 100 }] }, 500);
		expect(first.get("ultron-task-a")).toEqual({ startedAt: 100 });
		expect(first.get("ultron-task-b")).toEqual({});
		const later = clock.timings([task("a", "completed")], null, 900);
		expect(later.get("ultron-task-a")).toEqual({ startedAt: 100, endedAt: 900 });
	});

	test("extracts the root cell from running tools", () => {
		const clock = new RlmClock();
		const running = extractRootCell(
			{
				transcript: [],
				operation: {
					runningTools: [{ status: "running", toolCallId: "c1", toolName: "rlm", args: { code: "x = 1" } }],
				},
			},
			clock,
			10,
		);
		expect(running).toEqual({ toolCallId: "c1", code: "x = 1", status: "running", startedAt: 10 });
		expect(extractRootCell({ transcript: [], operation: null }, clock, 0)).toBeNull();
	});

	test("extracts the current turn's cells in order, with the running one from the operation", () => {
		const clock = new RlmClock();
		const call = (id: string, code: string, timestamp: number) => ({
			message: {
				role: "assistant",
				timestamp,
				content: [{ type: "toolCall", id, name: "rlm", arguments: { code } }],
			},
		});
		const transcript = [
			{ timestamp: 1, message: { role: "user", content: "old question", timestamp: 1 } },
			call("c0", "old", 2),
			{
				timestamp: 100,
				message: { role: "user", content: [{ type: "text", text: "count the rows" }], timestamp: 100 },
			},
			call("c1", "print(1)", 105),
			{
				message: {
					role: "toolResult",
					toolName: "rlm",
					toolCallId: "c1",
					isError: true,
					timestamp: 109,
					content: [{ type: "text", text: "NameError" }],
				},
			},
			call("c2", "print(2)", 110),
		];
		const live = extractTurn(
			{
				transcript,
				operation: {
					runningTools: [{ status: "running", toolCallId: "c2", toolName: "rlm", args: { code: "print(2)" } }],
				},
			},
			clock,
			120,
		);
		expect(live.turn).toEqual({ startedAt: 100, prompt: "count the rows" });
		expect(live.cells).toEqual([
			{ toolCallId: "c1", code: "print(1)", status: "error", startedAt: 105, endedAt: 109, output: "NameError" },
			{ toolCallId: "c2", code: "print(2)", status: "running", startedAt: 120 },
		]);
		// With the operation gone and no result, the cell was cut off.
		const settled = extractTurn({ transcript, operation: null }, new RlmClock(), 130);
		expect(settled.cells[1]).toMatchObject({ toolCallId: "c2", status: "error", output: "interrupted" });
		expect(extractTurn(undefined, clock, 0)).toEqual({ turn: null, cells: [] });
	});

	test("formats durations compactly", () => {
		expect(formatDuration(250)).toBe("250ms");
		expect(formatDuration(4200)).toBe("4.2s");
		expect(formatDuration(75_000)).toBe("1m15s");
		expect(formatDuration(18 * 60_000)).toBe("18m");
		expect(formatDuration(3 * 3_600_000 + 5 * 60_000)).toBe("3h5m");
	});
});
