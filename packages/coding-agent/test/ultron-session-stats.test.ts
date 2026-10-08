/**
 * The runtime counters behind the session report (`ultron.module/stats`): what a cell's code names, which steer a
 * message is, and the recorder's durability: it never throws, saves shortly after a change, and carries on from the
 * stored document when a session resumes.
 */
import type { JsonValue } from "@ultron/chord";
import { describe, expect, test } from "vitest";
import { ExternalRootController } from "../src/ultron/claude/external-root.ts";
import { createMemoryModuleStore, type HostModuleStore } from "../src/ultron/rlm/host-module.ts";
import { defaultSecretDetector, maskCellOutputCounted } from "../src/ultron/rlm/output-secrets.ts";
import {
	cellApis,
	classifyNudge,
	parseSessionStats,
	SessionStatsRecorder,
	statsLane,
} from "../src/ultron/session-stats.ts";
import { nudgeMessage, skillNudgeMessage, waitNudgeMessage } from "../src/ultron/tool-round-nudge.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";

describe("what a cell's code names", () => {
	test("the REPL's own functions and namespaces, each once", () => {
		expect(
			cellApis(
				[
					'out = await bash("ls")',
					'out2 = await bash("pwd")',
					'text = await read("a.py")',
					'await edit("a.py", "x", "y")',
					'await write("b.py", text)',
					'h = await rlm.load("big.log")',
					'v = await rlm.infer("judge", context=[h])',
					'rs = await rlm.map("label", items)',
					'c = await rlm.spawn("fix it", name="fix")',
					"await rlm.collect([c])",
					"await rlm.merge([c])",
					'await mcp.call("search", {"q": "x"})',
					'await tools.call("fetch", {})',
					"await workflows.run(nodes)",
					'await agents.invoke("identity@1", {})',
					'await background.start("slow")',
					'await view_image("plot.png")',
				].join("\n"),
			),
		).toEqual([
			"bash",
			"read",
			"edit",
			"write",
			"rlm.load",
			"rlm.infer",
			"rlm.map",
			"rlm.spawn",
			"rlm.collect",
			"rlm.merge",
			"mcp.call",
			"tools.call",
			"workflows.run",
			"agents.*",
			"background.start",
			"view_image",
		]);
	});

	test("methods and look-alikes are not the REPL's functions", () => {
		expect(cellApis("f = open('a')\ndata = f.read()\nf.write(data)\npath.read_text()\nproc.stdout.read()")).toEqual(
			[],
		);
		expect(cellApis("def rebash(x): return x\nrebash(1)\nmy_edit(2)\nsys.stdout.write('x')")).toEqual([]);
		expect(cellApis("s = await agents.status()\nt = await agents.tasks()")).toEqual([]);
		expect(cellApis("await agents.spawn('reviewer@1', {})")).toEqual(["agents.*"]);
		expect(cellApis("x = 1 + 1")).toEqual([]);
	});

	test("lanes: the root, rlm.spawn subagents, everything else", () => {
		expect(statsLane("main")).toBe("root");
		expect(statsLane("ultron.rlm-child.ultron-task-1")).toBe("subagents");
		expect(statsLane("ultron.correctness-reviewer.ultron-task-2")).toBe("other");
		expect(statsLane("ultron.rlm-frame.ultron-task-3")).toBe("other");
	});
});

describe("which steer a message is", () => {
	test("the messages the nudgers send, in every wording", () => {
		expect(classifyNudge(nudgeMessage(10, false))).toBe("toolRounds");
		expect(classifyNudge(nudgeMessage(20, true))).toBe("toolRounds");
		for (const asyncEvents of [true, false])
			for (const nextCall of [true, false])
				for (const running of [1, 3])
					expect(classifyNudge(waitNudgeMessage(running, asyncEvents, nextCall))).toBe("wait");
		expect(classifyNudge(skillNudgeMessage(8))).toBe("skill");
	});

	test("other text is no steer", () => {
		expect(
			classifyNudge("[Ultron] Usage turn limit reached for root turn:1; answer with what you have."),
		).toBeUndefined();
		expect(classifyNudge("You have used 10 rounds of tool calls")).toBeUndefined();
		expect(classifyNudge("please fix the bug")).toBeUndefined();
	});
});

describe("masked secrets are counted where they are masked", () => {
	test("the count is the number of findings replaced", () => {
		const token = `ghp_${"a".repeat(36)}`;
		expect(maskCellOutputCounted(`one ${token}\ntwo ${token}`, {})).toEqual({
			text: "one [REDACTED:github_token]\ntwo [REDACTED:github_token]",
			masked: 2,
		});
		expect(maskCellOutputCounted("nothing secret", {})).toEqual({ text: "nothing secret", masked: 0 });
		expect(maskCellOutputCounted(token, { ULTRON_MASK_SECRETS: "off" })).toEqual({ text: token, masked: 0 });
		expect(defaultSecretDetector().redact(`x ${token}`)).toBe("x [REDACTED:github_token]");
	});
});

function recorder(store: HostModuleStore, options: { root?: "lane" | "external"; now?: () => number } = {}) {
	return new SessionStatsRecorder({ store, flushMs: 5, ...options });
}

describe("the recorder", () => {
	test("counts cells per lane kind, host requests, guards, steers, refusals, merges and child models", async () => {
		const store = createMemoryModuleStore();
		const stats = recorder(store, { now: () => 1000 });
		stats.cell("main", 'await bash("ls")\nawait rlm.spawn("x", name="x")', { failed: false, masked: 2 });
		stats.cell("main", "1/0", { failed: true });
		stats.cell("ultron.rlm-child.ultron-task-1", 'await write("a", "b")', { failed: false, masked: 1 });
		stats.cell("ultron.reviewer.ultron-task-2", "x = 1", { failed: false });
		for (const type of ["shell.run", "rlm.spawn", "rlm.map", "rlm.map", "workflows.run"]) stats.hostCall(type);
		stats.guard({ guard: "Loki", phase: "before_write", outcome: "allowed", ms: 10 });
		stats.guard({ guard: "Loki", phase: "before_write", outcome: "blocked", ms: 20 });
		stats.guard({ guard: "Loki", phase: "before_write", outcome: "unchecked", ms: 5000 });
		stats.guard({ guard: "Loki", phase: "after_cell", outcome: "clean", ms: 30 });
		stats.guard({ guard: "Loki", phase: "after_cell", outcome: "findings", ms: 40 });
		stats.guard({ guard: "my-extension", phase: "before_write", outcome: "allowed", ms: 1 });
		stats.nudge(nudgeMessage(10, false));
		stats.nudge(waitNudgeMessage(2));
		stats.nudge("not a steer");
		stats.usageLimitBlock();
		stats.merge("ultron-task-1", "conflict");
		stats.merge("ultron-task-1", "merged");
		stats.childModel("ultron-task-1", "claude-code/sonnet");
		const snapshot = await stats.snapshot();
		expect(snapshot).toEqual({
			version: 1,
			root: "lane",
			since: 1000,
			updatedAt: 1000,
			cells: {
				root: { count: 2, failed: 1, apis: { bash: 1, "rlm.spawn": 1 } },
				subagents: { count: 1, failed: 0, apis: { write: 1 } },
				other: { count: 1, failed: 0, apis: {} },
			},
			hostCalls: { "shell.run": 1, "rlm.spawn": 1, "rlm.map": 2, "workflows.run": 1 },
			secretsMasked: 3,
			guards: {
				Loki: { checks: 3, blocked: 1, unchecked: 1, afterChecks: 2, afterFindings: 1, ms: 5100 },
				"my-extension": { checks: 1, blocked: 0, unchecked: 0, afterChecks: 0, afterFindings: 0, ms: 1 },
			},
			nudges: { toolRounds: 1, wait: 1, skill: 0 },
			usageLimitBlocks: 1,
			merges: { "ultron-task-1": "merged" },
			childModels: { "ultron-task-1": "claude-code/sonnet" },
			externalTurns: { count: 0, wallMs: 0 },
		});
		await stats.close();
		expect(parseSessionStats(await store.read())).toEqual(snapshot);
	});

	test("nothing is written until something ran; a change is saved shortly after, without a flush call", async () => {
		const store = createMemoryModuleStore();
		const stats = recorder(store);
		expect(await store.read()).toBeUndefined();
		await stats.flush();
		expect(await store.read()).toBeUndefined();
		// A root turn with no cell still leaves the (all zero) counters behind.
		stats.touch();
		await expect.poll(() => store.read(), { timeout: 2000 }).toBeDefined();
		expect(parseSessionStats(await store.read())).toMatchObject({
			cells: { root: { count: 0 } },
			usageLimitBlocks: 0,
		});
		await stats.close();
	});

	test("a resumed session carries on from the stored counters and keeps when counting began", async () => {
		const store = createMemoryModuleStore();
		const first = recorder(store, { now: () => 1000 });
		first.cell("main", "x = 1", { failed: false });
		first.usageLimitBlock();
		await first.close();
		const second = recorder(store, { now: () => 9000, root: "external" });
		second.cell("main", "y = 2", { failed: true });
		const snapshot = await second.snapshot();
		expect(snapshot).toMatchObject({
			since: 1000,
			updatedAt: 9000,
			// The session is now driven by Claude Code.
			root: "external",
			cells: { root: { count: 2, failed: 1 } },
			usageLimitBlocks: 1,
		});
		await second.close();
		expect(parseSessionStats(await store.read())).toEqual(snapshot);
	});

	test("an external root's turns: how many, and how long they were open", async () => {
		let now = 0;
		const stats = recorder(createMemoryModuleStore(), { root: "external", now: () => now });
		stats.externalTurnStarted("t1");
		stats.externalTurnStarted("t1");
		now = 4000;
		stats.externalTurnEnded("t1");
		stats.externalTurnEnded("t1");
		stats.externalTurnStarted("t2");
		now = 5000;
		// A turn still open when the session closes is closed with it.
		await stats.close();
		expect((await stats.snapshot()).externalTurns).toEqual({ count: 2, wallMs: 5000 });
	});

	test("a store that cannot be read or written never breaks the caller; the counters stay in memory", async () => {
		let writes = 0;
		let failing = true;
		let saved: JsonValue | undefined;
		const store: HostModuleStore = {
			read: async () => {
				throw new Error("read failed");
			},
			write: async (document) => {
				writes += 1;
				if (failing) throw new Error("disk full");
				saved = document;
			},
		};
		const stats = recorder(store);
		stats.cell("main", "x = 1", { failed: false });
		await stats.flush();
		expect(writes).toBe(1);
		expect(saved).toBeUndefined();
		expect((await stats.snapshot()).cells.root.count).toBe(1);
		// The next flush tries again.
		failing = false;
		await stats.flush();
		expect(parseSessionStats(saved)).toMatchObject({ cells: { root: { count: 1 } } });
		await stats.close();
		// Closed: later changes are dropped rather than written to a session that is going away.
		stats.cell("main", "y = 2", { failed: false });
		expect((await stats.snapshot()).cells.root.count).toBe(1);
	});

	test("a document written by a newer Ultron is left alone", async () => {
		const store = createMemoryModuleStore({ version: 7, cells: {} });
		const stats = recorder(store);
		stats.cell("main", "x = 1", { failed: false });
		await stats.close();
		// This worker counts in memory for its own reports; the newer document is never overwritten.
		expect((await stats.snapshot()).cells.root.count).toBe(1);
		expect(await store.read()).toEqual({ version: 7, cells: {} });
	});

	test("the per-task maps and request types are bounded", async () => {
		const stats = recorder(createMemoryModuleStore());
		for (let index = 0; index < 250; index += 1) {
			stats.merge(`task-${index}`, "merged");
			stats.childModel(`task-${index}`, "p/m");
			stats.hostCall(`type.${index}`);
		}
		stats.hostCall("type.0");
		const snapshot = await stats.snapshot();
		expect(Object.keys(snapshot.merges)).toHaveLength(200);
		expect(snapshot.merges["task-249"]).toBe("merged");
		expect(snapshot.merges["task-0"]).toBeUndefined();
		expect(Object.keys(snapshot.childModels)).toHaveLength(200);
		expect(Object.keys(snapshot.hostCalls)).toHaveLength(200);
		expect(snapshot.hostCalls["type.0"]).toBe(2);
		expect(snapshot.hostCalls["type.249"]).toBeUndefined();
		await stats.close();
	});
});

describe("an external root (Claude Code) reports its steers and refused cells", () => {
	function externalRoot(stats: SessionStatsRecorder, options: { exhausted?: string; running?: number }) {
		return new ExternalRootController({
			execute: async (code) => ({ content: [{ type: "text", text: `ran ${code}` }] }),
			host: {
				beginRootTurn: (turn) => stats.externalTurnStarted(turn),
				endRootTurn: (turn) => stats.externalTurnEnded(turn),
				rootIdOfRun: (turn) => `turn:${turn}`,
				pendingRootNotifications: () => options.running ?? 0,
			},
			usage: { turnBudgetExhausted: async () => options.exhausted },
			hints: { runEnded: () => {} },
			fileHooks: { beginTurn: () => {} },
			lokiNotice: Promise.resolve(undefined),
			toolRoundsNudge: 2,
			skillNudge: 0,
			asyncEvents: true,
			onNudge: (message) => stats.nudge(message),
			onUsageLimit: () => stats.usageLimitBlock(),
		});
	}

	test("the research brake and the wait steer are counted as they are appended to a cell result", async () => {
		const stats = recorder(createMemoryModuleStore(), { root: "external" });
		const root = externalRoot(stats, {});
		await root.runCell("a");
		const second = await root.runCell("b");
		expect(second.content[0]).toMatchObject({
			text: expect.stringContaining("You have used 2 rounds of tool calls"),
		});
		await root.endTurn();
		const waiting = externalRoot(stats, { running: 2 });
		for (const code of ["c", "d", "e"]) await waiting.runCell(code);
		await waiting.endTurn();
		expect(await stats.snapshot()).toMatchObject({
			nudges: { toolRounds: 1, wait: 1, skill: 0 },
			usageLimitBlocks: 0,
			externalTurns: { count: 2 },
		});
		await stats.close();
	});

	test("a cell refused because the turn's budget is spent is a usage-limit block", async () => {
		const stats = recorder(createMemoryModuleStore(), { root: "external" });
		const root = externalRoot(stats, { exhausted: "Usage token limit reached for root turn:x" });
		expect((await root.runCell("x")).isError).toBe(true);
		expect((await root.runCell("y")).isError).toBe(true);
		expect((await stats.snapshot()).usageLimitBlocks).toBe(2);
		await stats.close();
	});
});

describe("refused reservations are reported", () => {
	test("a limit refusal calls onRefused once and still throws the same error", async () => {
		const refused: string[] = [];
		let now = 1000;
		const ledger = new NativeUsageLedger(undefined, {
			limits: { maxAdmittedTasks: 1, maxWallMs: 10_000 },
			now: () => now,
			onRefused: (reason) => {
				refused.push(reason);
				throw new Error("an observer's error is not the caller's");
			},
		});
		await ledger.reserve({ kind: "task", rootId: "r", taskId: "t1" });
		await expect(ledger.reserve({ kind: "task", rootId: "r", taskId: "t2" })).rejects.toThrow(
			"Usage admitted-task limit exceeded for root r",
		);
		now += 20_000;
		await expect(ledger.reserve({ kind: "model", rootId: "r" })).rejects.toThrow(
			"Usage wall deadline exceeded for root r",
		);
		expect(refused).toEqual([
			"Usage admitted-task limit exceeded for root r",
			"Usage wall deadline exceeded for root r",
		]);
		// An invalid request is not a limit.
		await expect(ledger.reserve({ kind: "task", rootId: "r2", requestKey: " " })).rejects.toThrow(
			"requestKey is empty",
		);
		expect(refused).toHaveLength(2);
	});
});
