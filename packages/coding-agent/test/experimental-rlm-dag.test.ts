import { visibleWidth } from "@ultron/tui";
import { describe, expect, test } from "vitest";
import {
	applyDagAction,
	buildDagRuns,
	computeWaves,
	type DagPaneState,
	dagPaneHints,
	initialDagState,
	promptOf,
	renderDagPane,
	renderWaveSummary,
	selectedNode,
	waveGeometry,
} from "../src/experimental/rlm-dag.ts";
import { parseAgentsStatus, type RlmSnapshot } from "../src/experimental/rlm-visualizer.ts";

const NOW = 1_000_000;

/** The reference's shape: three parallel workflow nodes and one node joining all three. */
function referenceSnapshot(overrides: Partial<RlmSnapshot> = {}): RlmSnapshot {
	const task = (node: string, id: string, fields: Record<string, unknown>) => ({
		id: `ultron-task-${id}`,
		definition: "rlm-child@1",
		state: "running",
		startedAt: NOW - 21_000,
		lane: `ultron.rlm-child.${id}`,
		workflow: { run: "wf-01a07520", node, dependsOn: [], join: "all" },
		...fields,
	});
	const parsed = parseAgentsStatus({
		tasks: [
			task("navigation", "aaaa1111", {
				input: '{"prompt":"Map shell navigation integration against HEAD, then list every route"}',
				model: "cliproxyapi/gpt-6-astra",
				turns: 1,
				toolCallCount: 1,
				lastText: "I read this as fact collection - I'll map the navigation contract first",
			}),
			task("gate-wiring", "bbbb2222", {
				input: '{"prompt":"Collect the changed component props, imports, and gates"}',
				model: "cliproxyapi/gpt-6-astra",
				turns: 1,
				toolCallCount: 1,
				lastText: "I read this as evidence collection - I'll compare the scoped wiring",
			}),
			task("test-coverage", "cccc3333", { input: '{"prompt":"Check test coverage"}' }),
		],
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
				],
			},
		],
	});
	return {
		now: NOW,
		tasks: parsed.tasks,
		workflows: parsed.workflows,
		turn: { startedAt: NOW - 30_000, prompt: "Linear gates integration evidence" },
		...overrides,
	};
}

const ANSI = /\u001b\[[0-9;]*m/g;
const plain = (lines: readonly string[]) => lines.join("\n").replace(ANSI, "");
const HINTS = [
	"Tab/n next",
	"Shift-Tab/p prev",
	"Space/Enter fold",
	"d Details",
	"↑↓ Scroll",
	"←→ Runs",
	"q/Esc Close",
];

/** Marks colors so tests can see what is highlighted. */
const MARKED = {
	fg: (color: string, text: string) => (color === "accent" ? `\u001b[35m${text}\u001b[39m` : text),
	bold: (text: string) => text,
};
/** Accent-colored text as MARKED draws it. */
const accent = (text: string) => `\u001b[35m${text}\u001b[39m`;

function workflowRun(snapshot: RlmSnapshot) {
	return buildDagRuns(snapshot).filter((run) => run.kind === "workflow");
}

function pane(width: number, state: DagPaneState = initialDagState(), height = 70, snapshot = referenceSnapshot()) {
	return renderDagPane(workflowRun(snapshot), state, width, height, { hints: HINTS, now: NOW });
}

describe("DAG layout", () => {
	test("waves are the topological levels of dependsOn, cycles cut", () => {
		expect(
			computeWaves([
				{ key: "a", dependsOn: [] },
				{ key: "b", dependsOn: ["a"] },
				{ key: "c", dependsOn: ["a"] },
				{ key: "d", dependsOn: ["b", "c", "missing"] },
			]),
		).toEqual([["a"], ["b", "c"], ["d"]]);
		const cyclic = computeWaves([
			{ key: "x", dependsOn: ["y"] },
			{ key: "y", dependsOn: ["x"] },
		]);
		expect(cyclic.flat().sort()).toEqual(["x", "y"]);
	});

	test("three parallel nodes and one joining node, as in the reference", () => {
		const [run] = workflowRun(referenceSnapshot());
		expect(run!.waves).toEqual([
			["wf:wf-01a07520:navigation", "wf:wf-01a07520:gate-wiring", "wf:wf-01a07520:test-coverage"],
			["wf:wf-01a07520:verify-evidence"],
		]);
		const text = plain(pane(80).lines);
		expect(text).toContain("DAG · 01a07520");
		expect(text).toContain("RLM / DAG / workflow  Tasks (0)");
		expect(text).toContain("Linear gates integration evidence  1/1");
		expect(text).toContain("Running · Done 0/4 · 3 running · wave 1/2");
		// One row of three boxes, the frontier note, an arrow, then the joining node waiting on all three.
		expect(text).toMatch(/│ > \[-\] navigation +│ {2}│ {3}\[-\] gate-wiring +│ {2}│ {3}\[-\] test-coverage +│/);
		expect(text).toMatch(/│ ● Running {2}21s +│ {2}│ ● Running {2}21s/);
		expect(text).toContain("│ Start node");
		expect(text).toContain("  · Same frontier");
		expect(text).toMatch(/\n +│\n +▼\n/);
		expect(text).toMatch(/│ {3}\[-\] verify-evidence +│/);
		expect(text).toContain("│ ○ Pending");
		expect(text).toContain("│ ← navigation, gate-wiring, test-coverage │");
		// A lone box widens to fit its dependencies, within the pane.
		expect(plain(pane(44).lines)).toContain("│ ← navigation, gate-wiring, test-coverage │");
	});

	test("a wave that does not fit wraps to more rows; every line fits at 44, 60 and 80 columns", () => {
		expect(waveGeometry(3, 43)).toEqual({ perRow: 2, boxWidth: 20 });
		expect(waveGeometry(3, 60)).toEqual({ perRow: 2, boxWidth: 29 });
		expect(waveGeometry(3, 80)).toEqual({ perRow: 3, boxWidth: 25 });
		expect(waveGeometry(1, 80)).toEqual({ perRow: 1, boxWidth: 30 });
		expect(waveGeometry(1, 80, 45)).toEqual({ perRow: 1, boxWidth: 45 });
		expect(waveGeometry(1, 43, 60)).toEqual({ perRow: 1, boxWidth: 43 });
		for (const width of [44, 60, 80]) {
			const { lines } = pane(width, initialDagState(), 40);
			expect(lines).toHaveLength(40);
			for (const line of lines) expect(visibleWidth(line)).toBeLessThanOrEqual(width);
		}
		const narrow = plain(pane(44).lines).split("\n");
		const first = narrow.findIndex((line) => line.includes("navigation") && line.includes("gate-wiring"));
		expect(first).toBeGreaterThan(0);
		expect(narrow[first]).not.toContain("test-cov");
		// The third box starts a second row after a blank line, before the frontier note.
		const second = narrow.findIndex((line) => line.includes("test-cover"));
		expect(second).toBe(first + 6);
		expect(narrow[second + 4]).toContain("· Same frontier");
	});

	test("long names and texts are cut with an ellipsis", () => {
		const snapshot = referenceSnapshot();
		const long = {
			...snapshot,
			workflows: [
				{
					...snapshot.workflows![0]!,
					nodes: [
						{
							id: "a-very-long-node-name-that-cannot-fit-in-any-box",
							definition: "x@1",
							dependsOn: [],
							join: "all",
						},
					],
				},
			],
			tasks: [],
		};
		const { lines } = pane(44, initialDagState(), 40, long);
		expect(plain(lines)).toMatch(/│ > \[-\] a-very-long-node-name-that-cannot… │/);
		for (const line of lines) expect(visibleWidth(line)).toBeLessThanOrEqual(44);
		const cards = plain(pane(44).lines);
		expect(cards).toContain("│ > ● Map shell navigation integration ag… │");
		expect(cards).toContain("│ I read this as fact collection - I'll m… │");
	});

	test("the selected node's box and card are highlighted; the dependencies section lists every edge", () => {
		const rendered = renderDagPane(workflowRun(referenceSnapshot()), initialDagState(), 60, 70, {
			style: MARKED,
			now: NOW,
		});
		const text = rendered.lines.join("\n");
		// The first running node is selected by default: ">" marker and an accent border on its box and card.
		expect(selectedNode(rendered.run!, rendered.state)).toBe("wf:wf-01a07520:navigation");
		expect(text).toContain(`${accent("│")} ${accent(">")} [-] navigation`);
		expect(text).toContain("\u001b[35m┌───");
		expect(text).toContain(`${accent("│")} ${accent(">")} ${accent("●")} \u001b[35mMap shell navigation integration`);
		// Other boxes and cards keep a plain border.
		expect(text).toContain("│   [-] gate-wiring");
		expect(text.replace(ANSI, "")).toContain("│ ● Collect the changed component props, imports, and gat… │");
		expect(text).toMatch(/\n│ \u001b\[35m●\u001b\[39m Collect/);
		const plainText = plain(rendered.lines);
		expect(plainText).toContain(
			"Dependencies\n  navigation → verify-evidence\n  gate-wiring → verify-evidence\n  test-coverage → verify-evidence",
		);
		expect(plainText).toContain("Node details");
		expect(plainText).toContain("│ navigation · rlm-child@1 · cliproxyapi/gpt-6-astra");
		expect(plainText).toContain("│ 21s · 1 turn · 1 tool");
		expect(plainText).toContain("│ waiting 21s");
	});

	test("the footer shows the connection, the scroll position and the key hints", () => {
		const text = plain(pane(44, initialDagState(), 30).lines).split("\n");
		expect(text).toHaveLength(30);
		const status = text.findIndex((line) => line.startsWith("● Connected"));
		expect(text[status]).toMatch(/^● Connected {2}1-\d+\/\d+$/);
		expect(text[status - 1]).toMatch(/^─+$/);
		expect(text.slice(status + 1).join("  ")).toBe(HINTS.join("  ").replace("d Details  ↑↓", "d Details  ↑↓"));
		const failed = renderDagPane(workflowRun(referenceSnapshot()), initialDagState(), 44, 30, {
			now: NOW,
			error: "worker closed",
		});
		expect(plain(failed.lines)).toContain("● Inspection failed: worker");
	});

	test("a pane with no runs says so; a tiny pane still fits", () => {
		const empty = renderDagPane([], initialDagState(), 44, 20, { now: NOW });
		expect(plain(empty.lines)).toContain("No RLM work in this turn yet.");
		for (const [width, height] of [
			[12, 6],
			[44, 8],
		] as const) {
			const { lines } = pane(width, initialDagState(), height);
			expect(lines).toHaveLength(height);
			for (const line of lines) expect(visibleWidth(line)).toBeLessThanOrEqual(width);
		}
	});
});

describe("DAG navigation", () => {
	test("next and previous wrap around the nodes wave by wave; fold and details toggle", () => {
		const runs = workflowRun(referenceSnapshot());
		let state = pane(60).state;
		expect(selectedNode(runs[0]!, state)).toBe("wf:wf-01a07520:navigation");
		state = applyDagAction(state, runs, "next");
		expect(state.selectedKey).toBe("wf:wf-01a07520:gate-wiring");
		state = applyDagAction(applyDagAction(state, runs, "next"), runs, "next");
		expect(state.selectedKey).toBe("wf:wf-01a07520:verify-evidence");
		state = applyDagAction(state, runs, "next");
		expect(state.selectedKey).toBe("wf:wf-01a07520:navigation");
		state = applyDagAction(state, runs, "prev");
		expect(state.selectedKey).toBe("wf:wf-01a07520:verify-evidence");

		state = applyDagAction(state, runs, "fold");
		let text = plain(pane(60, state).lines);
		expect(text).toMatch(/│ > \[\+\] verify-evidence ○ +│/);
		expect(text).not.toContain("← navigation, gate-wiring");
		state = applyDagAction(state, runs, "fold");
		expect(plain(pane(60, state).lines)).toContain("← navigation, gate-wiring");

		state = applyDagAction(state, runs, "details");
		text = plain(pane(60, state).lines);
		expect(text).not.toContain("Node details");
		expect(text).toContain("Dependencies");
	});

	test("the body scrolls, keeps the selected node in view, and runs switch with left and right", () => {
		const snapshot = referenceSnapshot({
			workflows: [
				...referenceSnapshot().workflows!,
				{
					run: "wf-22222222",
					startedAt: NOW - 5000,
					nodes: [{ id: "solo", definition: "x@1", dependsOn: [], join: "all" }],
				},
			],
		});
		const runs = workflowRun(snapshot);
		const render = (state: DagPaneState) => renderDagPane(runs, state, 44, 24, { hints: HINTS, now: NOW });
		let rendered = render(initialDagState());
		// The newest active run shows by default.
		expect(plain(rendered.lines)).toContain("DAG · 22222222");
		expect(plain(rendered.lines)).toContain("2/2");
		let state = applyDagAction(rendered.state, runs, "prevRun");
		rendered = render(state);
		expect(plain(rendered.lines)).toContain("DAG · 01a07520");
		expect(plain(rendered.lines)).toMatch(/● Connected {2}1-\d+\/\d+/);
		state = applyDagAction(rendered.state, runs, "scrollDown");
		state = applyDagAction(state, runs, "scrollDown");
		rendered = render(state);
		expect(plain(rendered.lines)).toMatch(/● Connected {2}3-\d+\/\d+/);
		state = applyDagAction(rendered.state, runs, "scrollUp");
		expect(state.scroll).toBe(1);
		// Selecting a node below the window scrolls it into view.
		state = applyDagAction(applyDagAction(applyDagAction(state, runs, "next"), runs, "next"), runs, "next");
		rendered = render(state);
		expect(plain(rendered.lines)).toContain("> [-] verify-evidence");
		state = applyDagAction(rendered.state, runs, "nextRun");
		expect(plain(render(state).lines)).toContain("DAG · 22222222");
	});

	test("hints come from the live bindings", () => {
		const bindings: Record<string, string[]> = {
			"app.rlm.pane.next": ["tab", "n"],
			"app.rlm.pane.prev": ["shift+tab", "p"],
			"app.rlm.pane.fold": ["space", "enter"],
			"app.rlm.pane.details": ["d"],
			"app.rlm.pane.scrollUp": ["up"],
			"app.rlm.pane.scrollDown": ["down"],
			"app.rlm.pane.prevRun": ["left"],
			"app.rlm.pane.nextRun": ["right"],
			"app.rlm.pane.close": ["q", "escape"],
		};
		expect(
			dagPaneHints({ getKeys: ((id: string) => bindings[id] ?? []) as never, matches: (() => false) as never }),
		).toEqual(HINTS);
	});
});

describe("turn runs", () => {
	test("each cell's children form one frontier; tool calls are one node; nested work follows its parent", () => {
		const cell = (id: string, start: number, end?: number) => ({
			toolCallId: id,
			code: "x = 1",
			status: end === undefined ? ("running" as const) : ("ok" as const),
			startedAt: start,
			...(end === undefined ? {} : { endedAt: end }),
		});
		const parsed = parseAgentsStatus({
			tasks: [
				{
					id: "ultron-task-c1",
					definition: "rlm-child@1",
					state: "running",
					startedAt: NOW - 9000,
					input: '{"prompt":"one"}',
				},
				{
					id: "ultron-task-c2",
					definition: "rlm-child@1",
					state: "completed",
					startedAt: NOW - 9000,
					endedAt: NOW - 4000,
					result: { status: "succeeded", preview: "two done" },
				},
				{ id: "ultron-task-t3", definition: "reviewer@1", state: "admitted", startedAt: NOW - 8900 },
				{
					id: "ultron-task-n1",
					definition: "rlm-child@1",
					state: "running",
					parentId: "ultron-task-c1",
					startedAt: NOW - 5000,
				},
			],
			toolCalls: [
				{
					id: "tc1",
					name: "web_search",
					label: "web_search foo",
					status: "completed",
					source: "repl",
					startedAt: NOW - 8800,
					endedAt: NOW - 8700,
				},
				{
					id: "tc2",
					name: "web_fetch",
					label: "web_fetch bar",
					status: "running",
					source: "repl",
					startedAt: NOW - 8600,
				},
			],
		});
		const snapshot: RlmSnapshot = {
			now: NOW,
			tasks: parsed.tasks,
			toolCalls: parsed.toolCalls,
			cells: [cell("call-1", NOW - 9500, NOW - 8000), cell("call-2", NOW - 3000)],
			turn: { startedAt: NOW - 10_000, prompt: "compare the drafts" },
		};
		const [run] = buildDagRuns(snapshot);
		expect(run!.kind).toBe("turn");
		expect(run!.waves).toEqual([
			["task:ultron-task-c1", "task:ultron-task-c2", "task:ultron-task-t3", "tools:cell:call-1"],
			["task:ultron-task-n1"],
		]);
		expect(run!.edges).toEqual([["task:ultron-task-c1", "task:ultron-task-n1"]]);
		const text = plain(renderDagPane([run!], initialDagState(), 80, 80, { now: NOW }).lines);
		expect(text).toContain("RLM / DAG / turn");
		expect(text).toContain("[-] child c1");
		expect(text).toContain("[-] 2 tool calls");
		expect(text).toContain("│ ▰▰▰▰▰▱▱▱▱▱ 1/2");
		expect(text).toContain("  · Same frontier");
		expect(text).toContain("│ cell 1");
		expect(text).toContain("← child c1");
		expect(text).toContain("  child c1 → child n1");
		expect(text).toContain("│ → two done");
	});

	test("prompts come from bounded input previews, even cut mid-JSON", () => {
		expect(promptOf('{"prompt":"Summarize the file"}')).toBe("Summarize the file");
		expect(promptOf('{"prompt":"Summarize the \\"big\\" fi')).toBe('Summarize the "big" fi');
		expect(promptOf('{"n":2}')).toBe('{"n":2}');
		expect(promptOf(undefined)).toBeUndefined();
	});
});

describe("wave summary", () => {
	test("the active run: title, wave and counts, then one line per node, bounded with +N more", () => {
		const lines = renderWaveSummary(buildDagRuns(referenceSnapshot()), 100, { now: NOW });
		const text = plain(lines).split("\n");
		expect(text[0]).toBe("▶ Linear gates integration evidence running wave 1/2  0/4 done, 3 running");
		expect(text[1]).toMatch(
			/^ {2}▶ navigation · category:rlm-child@1 · I read this as fact collection - I'll map the .*… · 21s$/,
		);
		expect(text[3]).toBe("  ▶ test-coverage · category:rlm-child@1 · 21s");
		expect(text[4]).toBe("  ○ verify-evidence · category:rlm-child@1 · waiting 21s");
		for (const line of lines) expect(visibleWidth(line)).toBeLessThanOrEqual(100);

		const bounded = plain(renderWaveSummary(buildDagRuns(referenceSnapshot()), 60, { now: NOW, maxNodes: 3 })).split(
			"\n",
		);
		expect(bounded).toHaveLength(4);
		expect(bounded[3]).toBe("  +2 more");
		expect(bounded[0]).toBe("▶ Linear gates integr… running wave 1/2  0/4 done, 3 running");
	});

	test("empty when nothing runs or waits", () => {
		const done = referenceSnapshot({
			tasks: referenceSnapshot().tasks.map((task) => ({ ...task, state: "completed", endedAt: NOW })),
			workflows: [],
		});
		expect(renderWaveSummary(buildDagRuns(done), 80, { now: NOW })).toEqual([]);
		expect(renderWaveSummary([], 80)).toEqual([]);
	});
});
