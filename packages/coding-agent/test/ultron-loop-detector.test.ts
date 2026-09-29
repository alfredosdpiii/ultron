/**
 * Tool-loop detection (loop-detector.ts) and its `stuck-loop` hint:
 * - trips on consecutive failed cells, on an A, B, A, B alternation of near-identical cells with failures, and on the
 *   run's total failures; a success that is not a repeat resets the phase, and repeated successful cells never trip;
 * - one hint per phase and one escalation, never more; `hints.mute("stuck-loop")` silences it;
 * - a failed `bash` counts as a failed cell (exit 1 of a query command such as grep does not);
 * - through the rlm tool, a lane that keeps failing gets the hint once, then the escalation once.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { describe, expect, test } from "vitest";
import { createLocalBashOperations } from "../src/core/tools/bash.ts";
import { createUltronRlmTool } from "../src/experimental/session-worker.ts";
import { bashFailed, CellHints } from "../src/ultron/rlm/hints.ts";
import { runHostBash } from "../src/ultron/rlm/host-bash.ts";
import { createMemoryModuleStore, type NativeHostApi } from "../src/ultron/rlm/host-module.ts";
import { alternationLength, loopHintText, normalizeCell, ToolLoopDetector } from "../src/ultron/rlm/loop-detector.ts";

describe("loop detector", () => {
	test("three failed cells in a row trip once; three more escalate once; then it stays quiet", () => {
		const loops = new ToolLoopDetector();
		const fail = (code: string, ename = "KeyError") => loops.cellEnded("main", code, true, ename);
		expect(fail("a")).toBeUndefined();
		expect(fail("b")).toBeUndefined();
		expect(fail("c")).toEqual({ level: 1, reason: "consecutive", count: 3, ename: "KeyError" });
		expect(fail("d", "ValueError")).toBeUndefined();
		expect(fail("e", "ValueError")).toBeUndefined();
		// Mixed exception types: the escalation names no single one.
		expect(fail("f")).toEqual({ level: 2, reason: "consecutive", count: 6 });
		for (const code of ["g", "h", "i", "j", "k", "l"]) expect(fail(code)).toBeUndefined();
		expect(loops.level("main")).toBe(2);
	});

	test("a success resets the streak and the phase; a new streak trips again", () => {
		const loops = new ToolLoopDetector();
		loops.cellEnded("main", "a", true);
		loops.cellEnded("main", "b", true);
		expect(loops.cellEnded("main", "c", false)).toBeUndefined();
		expect(loops.cellEnded("main", "d", true)).toBeUndefined();
		expect(loops.cellEnded("main", "e", true)).toBeUndefined();
		expect(loops.cellEnded("main", "f", true)?.level).toBe(1);
		expect(loops.cellEnded("main", "g", false)).toBeUndefined();
		expect(loops.level("main")).toBe(0);
		loops.cellEnded("main", "h", true);
		loops.cellEnded("main", "i", true);
		expect(loops.cellEnded("main", "j", true)).toMatchObject({ level: 1, reason: "consecutive" });
	});

	test("A, B, A, B between near-identical cells trips when they fail, and escalates at twice the length", () => {
		const loops = new ToolLoopDetector();
		// B succeeds (it only "checks"), A fails: the consecutive count never reaches 3.
		const a = (n: number) => loops.cellEnded("main", `x = parse(data, retries=${n})\nprint(x)`, true, "ValueError");
		const b = () => loops.cellEnded("main", "print(  data[:100] )", false);
		expect(a(1)).toBeUndefined();
		expect(b()).toBeUndefined();
		expect(a(2)).toBeUndefined();
		expect(b()).toEqual({ level: 1, reason: "alternation", count: 4 });
		for (let i = 3; i < 5; i++) {
			expect(a(i)).toBeUndefined();
			if (i < 4) expect(b()).toBeUndefined();
		}
		expect(b()).toEqual({ level: 2, reason: "alternation", count: 8 });
		expect(a(9)).toBeUndefined();
		// A different successful cell is progress: the phase ends.
		expect(loops.cellEnded("main", "something_else()", false)).toBeUndefined();
		expect(loops.level("main")).toBe(0);
	});

	test("repeated successful cells never trip (identical or alternating)", () => {
		const loops = new ToolLoopDetector();
		for (let i = 0; i < 20; i++) expect(loops.cellEnded("main", `print(step(${i}))`, false)).toBeUndefined();
		for (let i = 0; i < 20; i++)
			expect(loops.cellEnded("main", i % 2 ? "run_tests()" : "apply_patch()", false)).toBeUndefined();
	});

	test("total failures in a run trip once and escalate once, even with successes between", () => {
		const loops = new ToolLoopDetector({ total: 4 });
		const trips = [];
		for (let i = 0; i < 12; i++) {
			const trip = loops.cellEnded("main", `attempt_${"x".repeat(i)}()`, true);
			if (trip) trips.push(trip);
			loops.cellEnded("main", `ok_${"y".repeat(i)}()`, false);
		}
		expect(trips).toEqual([
			{ level: 1, reason: "total", count: 4 },
			{ level: 2, reason: "total", count: 8 },
		]);
	});

	test("lanes are separate, and a finished run starts clean", () => {
		const loops = new ToolLoopDetector();
		loops.cellEnded("main", "a", true);
		loops.cellEnded("main", "b", true);
		expect(loops.cellEnded("child-1", "a", true)).toBeUndefined();
		loops.reset("main");
		expect(loops.cellEnded("main", "c", true)).toBeUndefined();
	});

	test("normalization ignores whitespace and numbers; the alternation length", () => {
		expect(normalizeCell("x = f( 1 )\n")).toBe(normalizeCell("x=f(22)"));
		expect(normalizeCell("x = f(a)")).not.toBe(normalizeCell("x = f(b)"));
		expect(alternationLength(["a", "b", "a", "b"])).toBe(4);
		expect(alternationLength(["c", "a", "b", "a", "b", "a"])).toBe(5);
		expect(alternationLength(["a", "a"])).toBe(0);
		expect(alternationLength(["a", "b", "c"])).toBe(2);
	});

	test("the hint asks for three hypotheses; the escalation is firmer", () => {
		const first = loopHintText({ level: 1, reason: "consecutive", count: 3, ename: "KeyError" });
		expect(first).toContain("The last 3 cells failed (KeyError each time)");
		expect(first).toContain("three different hypotheses");
		const second = loopHintText({ level: 2, reason: "alternation", count: 8 });
		expect(second).toContain("alternate between two near-identical versions");
		expect(second).toContain("Stop repeating this approach");
	});

	test("bash failures: non-zero exits and timeouts, but not a query command's exit 1", () => {
		expect(bashFailed("make", 2)).toBe(true);
		expect(bashFailed("pytest -x", 1)).toBe(true);
		expect(bashFailed("grep -n foo src", 1)).toBe(false);
		expect(bashFailed("rg foo", 2)).toBe(true);
		expect(bashFailed("ls", 0)).toBe(false);
		expect(bashFailed("sleep 99", null, true)).toBe(true);
	});
});

describe("stuck-loop hint", () => {
	const make = () => new CellHints({ store: createMemoryModuleStore() });
	const cell = (
		hints: CellHints,
		code: string,
		outcome: { ename?: string } = {},
		bash?: { command: string; exit_code: number },
	) => {
		hints.beginCell("main", code);
		if (bash) hints.observe("main", "bash", { command: bash.command }, { ...bash, running: false }, Date.now());
		return hints.endCell("main", { text: "", ...outcome });
	};

	test("fires once per phase, escalates once, and replaces repeated-failure meanwhile", async () => {
		const hints = make();
		const results = [];
		for (let i = 0; i < 9; i++) results.push(await cell(hints, `step_${"i".repeat(i)}()`, { ename: "KeyError" }));
		const fired = results.filter((hint) => hint !== undefined);
		expect(fired).toHaveLength(2);
		expect(results[2]).toMatch(
			/^\[hint:stuck-loop\] The last 3 cells failed \(KeyError each time\); you look stuck\./,
		);
		expect(results[2]).toContain('await hints.mute("stuck-loop")');
		expect(results[5]).toMatch(/^\[hint:stuck-loop\] The last 6 cells failed/);
		expect(results.join("\n")).not.toContain("repeated-failure");
	});

	test("a failed bash makes a failed cell; a muted tag stays silent", async () => {
		const hints = make();
		const failing = { command: "npm test", exit_code: 1 };
		expect(await cell(hints, "await bash('npm test')", {}, failing)).toBeUndefined();
		expect(await cell(hints, "await bash('npm test -- --verbose')", {}, failing)).toBeUndefined();
		expect(await cell(hints, "await bash('npm test -- -x')", {}, failing)).toMatch(/^\[hint:stuck-loop\]/);

		const muted = make();
		await muted.module.handle(
			{
				type: "hints.mute",
				payload: { tags: ["stuck-loop"] },
				caller: { lane: "main" },
				context: BACKGROUND_CONTEXT,
			},
			{} as NativeHostApi,
		);
		const results = [];
		for (let i = 0; i < 6; i++) results.push(await cell(muted, `f${"x".repeat(i)}()`, { ename: "OSError" }));
		// Muted, the narrower repeated-failure hint is what remains.
		expect(results.filter(Boolean).every((hint) => hint!.startsWith("[hint:repeated-failure]"))).toBe(true);
	});

	test("grep finding nothing is not a failure", async () => {
		const hints = make();
		for (let i = 0; i < 5; i++)
			expect(
				await cell(hints, `await bash('grep -rn x${i} .')`, {}, { command: `grep -rn x${i} .`, exit_code: 1 }),
			).toBe(undefined);
	});

	test("runEnded starts the lane over", async () => {
		const hints = make();
		await cell(hints, "a()", { ename: "E" });
		await cell(hints, "b()", { ename: "E" });
		hints.runEnded("main");
		expect(await cell(hints, "c()", { ename: "E" })).toBeUndefined();
	});
});

describe("stuck-loop through the rlm tool", () => {
	test("a lane that keeps failing gets the hint on the third failure and the escalation on the sixth, once each", async () => {
		const dir = mkdtempSync(join(tmpdir(), "ultron-loop-"));
		const hints = new CellHints({ store: createMemoryModuleStore() });
		const tool = createUltronRlmTool(
			dir,
			async (type, payload, signal) => {
				if (type === "bash") return runHostBash(payload, dir, createLocalBashOperations({}), signal);
				throw new Error(`unexpected host request ${type}`);
			},
			async () => "main",
			{ hints },
		);
		const env = new NodeExecutionEnv({ cwd: dir });
		let call = 0;
		const run = async (code: string): Promise<string> => {
			call += 1;
			const invocation = {
				invocationId: `loop-${call}`,
				operationId: `loop-op-${call}`,
				turnId: "loop-turn",
				getMemo: async () => undefined,
				setMemo: async () => undefined,
			};
			try {
				const result = (await tool.execute(
					`loop-${call}`,
					{ code },
					() => {},
					{ env },
					invocation,
					BACKGROUND_CONTEXT,
				)) as {
					content: Array<{ text: string }>;
				};
				return result.content.map((part) => part.text).join("");
			} catch (error) {
				return error instanceof Error ? error.message : String(error);
			}
		};
		try {
			const outputs: string[] = [];
			// Alternate a raising cell and a failing command, each slightly different.
			for (let i = 0; i < 8; i++)
				outputs.push(
					await run(i % 2 ? `await bash("exit ${i + 1}")` : `import json\njson.loads("{bad${"x".repeat(i)}")`),
				);
			const hinted = outputs.map((text) => (text.match(/\[hint:stuck-loop\]/g) ?? []).length);
			expect(hinted).toEqual([0, 0, 1, 0, 0, 1, 0, 0]);
			expect(outputs[2]).toContain("three different hypotheses");
			expect(outputs[5]).toContain("even after the last hint");
		} finally {
			await tool.close();
			rmSync(dir, { recursive: true, force: true });
		}
	});
});
