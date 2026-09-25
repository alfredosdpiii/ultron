import { visibleWidth } from "@ultron/tui";
import { describe, expect, test } from "vitest";
import {
	buildTaskTree,
	extractRootCell,
	formatDuration,
	parseAgentsStatus,
	parsePool,
	parseRetained,
	RlmClock,
	type RlmSnapshot,
	type RlmTask,
	renderRlmPanel,
	renderRlmStatusLine,
	summarizeRlm,
} from "../src/experimental/rlm-visualizer.ts";

const NOW = 1_000_000;

function task(id: string, state: string, extra: Partial<RlmTask> = {}): RlmTask {
	return { id: `ultron-task-${id}`, definition: "reviewer@1", state, ...extra };
}

function fixture(overrides: Partial<RlmSnapshot> = {}): RlmSnapshot {
	const tasks: RlmTask[] = [
		task("aaaa1111", "running", { definition: "planner@1" }),
		task("bbbb2222", "completed", {
			definition: "rlm-child@1",
			parentId: "ultron-task-aaaa1111",
			result: { status: "succeeded", value: "found 3 issues\nin two files" },
		}),
		task("cccc3333", "failed", {
			parentId: "ultron-task-aaaa1111",
			result: { status: "failed", error: "TimeoutError: deadline exceeded" },
		}),
		task("dddd4444", "cancelled", { definition: "background-job@1" }),
		task("eeee5555", "running", { definition: "coder@1", parentId: "ultron-task-dddd4444" }),
	];
	return {
		now: NOW,
		tasks,
		usage: { admittedTasks: 5, remainingWallMs: 18 * 60_000, usage: { cost: 0.42 } },
		limits: { maxAdmittedTasks: 24, maxWallMs: 30 * 60_000 },
		pool: {
			live: 3,
			maxLive: 16,
			lanes: [
				{ lane: "main", running: 1, pinnedBy: [] },
				{ lane: "ultron.planner.x", running: 0, pinnedBy: ["instance-1"] },
			],
			evictions: 2,
		},
		rootCell: {
			toolCallId: "call-1",
			code: 'plan = agents.spawn("planner", {"goal": g})\nchild = rlm.spawn("look deeper", name="deep")\nwait([plan, child])\nprint(plan.result())',
			status: "running",
			startedAt: NOW - 4200,
		},
		retained: new Set(["ultron-task-aaaa1111"]),
		progress: new Map([["ultron-task-aaaa1111", { classification: "progressing", receipts: 3 }]]),
		timing: new Map([
			["ultron-task-aaaa1111", { startedAt: NOW - 12_000 }],
			["ultron-task-bbbb2222", { startedAt: NOW - 8000, endedAt: NOW - 5000 }],
		]),
		...overrides,
	};
}

describe("RLM visualizer", () => {
	test("builds the tree by parentId, active first, orphans at the top level", () => {
		const tree = buildTaskTree([
			task("child", "completed", { parentId: "ultron-task-root" }),
			task("root", "running"),
			task("lost", "running", { parentId: "ultron-task-missing" }),
			task("done", "completed"),
		]);
		expect(tree.map((node) => node.task.id)).toEqual(["ultron-task-root", "ultron-task-lost", "ultron-task-done"]);
		expect(tree[0]!.children.map((node) => node.task.id)).toEqual(["ultron-task-child"]);
		expect(tree[1]!.orphanOf).toBe("ultron-task-missing");
	});

	test("survives parent cycles by surfacing them as orphans", () => {
		const tree = buildTaskTree([
			task("a", "running", { parentId: "ultron-task-b" }),
			task("b", "running", { parentId: "ultron-task-a" }),
		]);
		const ids = new Set<string>();
		const walk = (nodes: typeof tree): void => {
			for (const node of nodes) {
				ids.add(node.task.id);
				walk(node.children);
			}
		};
		walk(tree);
		expect(ids).toEqual(new Set(["ultron-task-a", "ultron-task-b"]));
	});

	test("renders glyphs, states, elapsed time, retained marker, progress and result summaries", () => {
		const lines = renderRlmPanel(fixture(), 120);
		const text = lines.join("\n");
		expect(lines[0]).toBe("RLM 2 running · 1 done · 1 failed · 1 cancelled · 5/24 tasks · 18m left · $0.42");
		expect(text).toContain("⠋ root kernel running 4.2s");
		expect(text).toContain('│ plan = agents.spawn("planner", {"goal": g})');
		expect(text).toContain("│ … 1 more lines");
		expect(text).toContain("├─ ⠋ planner@1 aaaa1111 running 12s ◆ retained progressing·3r");
		expect(text).toContain("│  ├─ ✗ reviewer@1 cccc3333 failed TimeoutError: deadline exceeded");
		expect(text).toContain("│  └─ ✓ rlm-child@1 bbbb2222 completed 3.0s → found 3 issues in two files");
		expect(text).toContain("└─ ⊘ background-job@1 dddd4444 cancelled");
		expect(text).toContain("   └─ ⠋ coder@1 eeee5555 running");
		expect(text).toContain("kernels 3/16 live · 1 busy · 1 pinned · 2 evicted");
	});

	test("greys a cancelled subtree, including running descendants", () => {
		const marked = renderRlmPanel(fixture(), 120, {
			style: { fg: (color, text) => `<${color}>${text}</>`, bold: (text) => text },
		});
		const cancelled = marked.find((line) => line.includes("dddd4444"))!;
		const descendant = marked.find((line) => line.includes("eeee5555"))!;
		for (const line of [cancelled, descendant]) {
			expect(line.startsWith("<dim>")).toBe(true);
			expect(line).not.toContain("<accent>");
			expect(line).not.toContain("<error>");
		}
		const live = marked.find((line) => line.includes("aaaa1111"))!;
		expect(live).toContain("<accent>");
	});

	test("every line fits narrow terminals", () => {
		for (const width of [8, 20, 40]) {
			for (const line of renderRlmPanel(fixture(), width)) expect(visibleWidth(line)).toBeLessThanOrEqual(width);
			expect(visibleWidth(renderRlmStatusLine(fixture(), width)!)).toBeLessThanOrEqual(width);
		}
	});

	test("bounds the number of task rows", () => {
		const tasks = Array.from({ length: 40 }, (_, index) =>
			task(`t${index}`, index % 2 === 0 ? "running" : "completed"),
		);
		const lines = renderRlmPanel(fixture({ tasks, rootCell: null, pool: null }), 100, { maxNodes: 15 });
		expect(lines.filter((line) => /^[├└]─ /.test(line))).toHaveLength(15);
		expect(lines.at(-1)).toBe("+25 more (5 active)");
	});

	test("summary line only appears with activity", () => {
		expect(renderRlmStatusLine(fixture(), 200)).toBe(
			"RLM ▸ 2 running · 1 done · 1 failed · 1 cancelled · 5/24 tasks · 18m left · $0.42",
		);
		const idle = fixture({ tasks: [task("x", "completed")], rootCell: null });
		expect(renderRlmStatusLine(idle, 200)).toBeUndefined();
		expect(summarizeRlm(fixture({ tasks: [], usage: null, rootCell: null }))).toBe("idle");
	});

	test("shows an error cell and an inspection error", () => {
		const lines = renderRlmPanel(
			fixture({
				error: "Ultron RLM host is not initialized",
				rootCell: {
					toolCallId: "c",
					code: "1/0",
					status: "error",
					startedAt: NOW - 1500,
					endedAt: NOW - 1000,
					output: "ZeroDivisionError: division by zero",
				},
			}),
			100,
		);
		expect(lines[1]).toBe("inspection failed: Ultron RLM host is not initialized");
		expect(lines[2]).toBe("✗ root kernel error 500ms  ZeroDivisionError: division by zero");
	});

	test("parses inspection payloads defensively", () => {
		const parsed = parseAgentsStatus({
			definitions: [],
			tasks: [{ id: "t1", definition: "a@1", state: "running", parentId: "p" }, { bogus: true }],
			usage: { admittedTasks: 1, limits: { maxAdmittedTasks: 4, maxWallMs: null } },
			limits: null,
		});
		expect(parsed.tasks).toEqual([{ id: "t1", definition: "a@1", state: "running", parentId: "p" }]);
		expect(parsed.limits).toEqual({ maxAdmittedTasks: 4, maxWallMs: null });
		expect(parseAgentsStatus(null).tasks).toEqual([]);
		expect(
			parseRetained([
				{ task_id: "a", state: "open", invocations: [{ task_id: "b" }] },
				{ task_id: "c", state: "closed", invocations: [] },
			]),
		).toEqual(new Set(["a", "b"]));
		expect(parsePool({ live: 1, maxLive: 2, lanes: [{ lane: "main", running: 1, pinnedBy: ["x"] }] })).toEqual({
			live: 1,
			maxLive: 2,
			lanes: [{ lane: "main", running: 1, pinnedBy: ["x"] }],
		});
		expect(parsePool("nope")).toBeNull();
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

	test("extracts the root cell from running tools or the transcript", () => {
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
		const settled = extractRootCell(
			{
				transcript: [
					{
						message: {
							role: "assistant",
							timestamp: 5,
							content: [{ type: "toolCall", id: "c2", name: "rlm", arguments: { code: "print(2)" } }],
						},
					},
					{
						message: {
							role: "toolResult",
							toolName: "rlm",
							toolCallId: "c2",
							isError: false,
							timestamp: 9,
							content: [{ type: "text", text: "2" }],
						},
					},
				],
				operation: null,
			},
			clock,
			20,
		);
		expect(settled).toEqual({
			toolCallId: "c2",
			code: "print(2)",
			status: "ok",
			startedAt: 5,
			endedAt: 9,
			output: "2",
		});
		expect(extractRootCell({ transcript: [], operation: null }, clock, 0)).toBeNull();
	});

	test("formats durations compactly", () => {
		expect(formatDuration(250)).toBe("250ms");
		expect(formatDuration(4200)).toBe("4.2s");
		expect(formatDuration(75_000)).toBe("1m15s");
		expect(formatDuration(18 * 60_000)).toBe("18m");
		expect(formatDuration(3 * 3_600_000 + 5 * 60_000)).toBe("3h5m");
	});
});
