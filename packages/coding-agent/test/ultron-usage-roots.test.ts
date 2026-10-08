import type { Context } from "@ultron/agent-core";
import type { JsonValue } from "@ultron/chord";
import { afterEach, describe, expect, test } from "vitest";
import { DEFAULT_MAX_TOTAL_TOKENS, NativeUsageLedger, nativeUsageLimitsFromEnv } from "../src/ultron/usage.ts";
import { aborted, deferred, hostFixture, journal, waitFor } from "./ultron-host-fixtures.ts";

/**
 * The wall budget, admission cap and optional cost cap apply per root turn: each main-lane run opens a
 * fresh window, while work admitted earlier keeps the root and deadline it was admitted under.
 */

const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
});

function clock(start = 1_000_000) {
	let now = start;
	return {
		now: () => now,
		advance: (ms: number) => {
			now += ms;
		},
	};
}

function turnFixture(options: { maxWallMs?: number; maxAdmittedTasks?: number; now?: () => number }) {
	const usage = new NativeUsageLedger(undefined, {
		limits: {
			...(options.maxWallMs === undefined ? {} : { maxWallMs: options.maxWallMs }),
			...(options.maxAdmittedTasks === undefined ? {} : { maxAdmittedTasks: options.maxAdmittedTasks }),
		},
		...(options.now === undefined ? {} : { now: options.now }),
	});
	const gate = deferred();
	const children: Array<{ id: string }> = [];
	const fixture = hostFixture({
		usage,
		rootTurns: true,
		...(options.now === undefined ? {} : { now: options.now }),
		script: async (lane: string, prompt: string, laneContext: Context) => {
			if (prompt === "spawn-child")
				children.push(
					(await fixture.host.handle(
						"agents.spawn",
						{ definition: "rlm-child@1", input: { prompt: "hold" } },
						laneContext,
						{ lane },
					)) as { id: string },
				);
			await Promise.race([gate.promise, aborted(laneContext)]);
			return "done";
		},
	});
	fixtures.push(fixture);
	const spawn = (prompt = "hold") =>
		fixture.call<{ id: string; state: string }>("agents.spawn", { definition: "rlm-child@1", input: { prompt } });
	return { fixture, usage, gate, spawn, children };
}

describe("per-turn wall budget", () => {
	test("ledger: an expired root refuses while a new root admits with a fresh window", async () => {
		const time = clock();
		const ledger = new NativeUsageLedger(undefined, { limits: { maxWallMs: 1_000 }, now: time.now });
		const first = await ledger.reserve({ kind: "task", rootId: "turn:a", requestKey: "a1" });
		expect(first.deadlineAt).toBe(time.now() + 1_000);
		time.advance(1_001);
		await expect(ledger.reserve({ kind: "task", rootId: "turn:a", requestKey: "a2" })).rejects.toThrow(
			"Usage wall deadline exceeded for root turn:a",
		);
		const next = await ledger.reserve({ kind: "task", rootId: "turn:b", requestKey: "b1" });
		expect(next.deadlineAt).toBe(time.now() + 1_000);
		// The old reservation keeps the deadline it was admitted under.
		expect((await ledger.status("turn:a")).reservations[0]?.deadlineAt).toBe(first.deadlineAt);
	});

	test("a session whose first turn started beyond maxWallMs ago can still spawn in a new turn", async () => {
		const time = clock();
		const { fixture, usage, gate, spawn } = turnFixture({ maxWallMs: 60_000, now: time.now });
		fixture.host.beginRootTurn("run-1");
		const background = await fixture.call<{ id: string }>("background.start", { prompt: "hold" });
		await spawn();
		// Both are running (their model calls admitted) before the turn's budget runs out.
		await waitFor(() => fixture.prompts.length === 2);
		time.advance(2 * 60 * 60 * 1000);
		// The same turn is out of budget, as before (A06/A09).
		await expect(spawn()).rejects.toThrow("Usage wall deadline exceeded for root turn:run-1");
		fixture.host.endRootTurn("run-1");
		fixture.host.beginRootTurn("run-2");
		await expect(spawn()).resolves.toMatchObject({ id: expect.any(String) });
		const status = await fixture.call<{ usage: { rootId: string; admittedTasks: number; deadlineAt: number } }>(
			"agents.status",
		);
		expect(status.usage).toMatchObject({ rootId: "turn:run-2", admittedTasks: 1, deadlineAt: time.now() + 60_000 });
		expect((await usage.status("turn:run-1")).admittedTasks).toBe(2);
		// Starting a new turn does not stop work started in an earlier one.
		expect((await journal(fixture)).find((task) => task.id === background.id)?.state).not.toMatch(/cancelled|failed/);
		gate.resolve();
		await waitFor(async () => (await journal(fixture)).every((task) => task.state === "completed"));
	});

	test("a task admitted in an old turn still hits its own deadline; the new turn's task does not", async () => {
		const { fixture, spawn } = turnFixture({ maxWallMs: 400 });
		fixture.host.beginRootTurn("old");
		const old = await spawn();
		await new Promise((resolve) => setTimeout(resolve, 150));
		fixture.host.beginRootTurn("new");
		const fresh = await spawn();
		await waitFor(async () => (await journal(fixture)).find((task) => task.id === old.id)?.state === "cancelled");
		const tasks = await journal(fixture);
		expect(tasks.find((task) => task.id === old.id)?.result?.error).toBe("Ultron root wall deadline exceeded");
		expect(tasks.find((task) => task.id === fresh.id)?.state).toBe("running");
	});

	test("a child spawned by an old turn's task is charged to that task's root, not the current turn", async () => {
		const { fixture, usage, gate, spawn, children } = turnFixture({ maxAdmittedTasks: 2 });
		fixture.host.beginRootTurn("one");
		const parent = await spawn("spawn-child");
		fixture.host.beginRootTurn("two");
		await waitFor(() => children.length === 1);
		expect((await usage.status("turn:one")).admittedTasks).toBe(2);
		expect((await usage.status("turn:two")).admittedTasks).toBe(0);
		// The old turn's admission cap still binds its subtree.
		await expect(
			fixture.host.handle("agents.spawn", { definition: "rlm-child@1", input: { prompt: "hold" } }, {} as never, {
				lane: `ultron.rlm-child.${parent.id}`,
			}),
		).rejects.toThrow("Usage admitted-task limit exceeded for root turn:one");
		// The new turn has its own admission window.
		await expect(spawn()).resolves.toMatchObject({ id: expect.any(String) });
		gate.resolve();
	});

	test("top-level work admitted between turns gets a root of its own", async () => {
		const time = clock();
		const { fixture, usage, gate, spawn } = turnFixture({ maxWallMs: 1_000, now: time.now });
		fixture.host.beginRootTurn("done");
		await spawn();
		fixture.host.endRootTurn("done");
		time.advance(5_000);
		const task = await fixture.host.api.spawn(
			{ definition: "rlm-child@1", input: { prompt: "hold" } },
			null,
			{} as never,
		);
		expect(task.state).not.toBe("failed");
		// Charged to a fresh job root, not to the ended turn whose budget is spent.
		expect((await usage.status("turn:done")).admittedTasks).toBe(1);
		gate.resolve();
	});

	test("without root turns the host keeps the single default root", async () => {
		const usage = new NativeUsageLedger();
		const fixture = hostFixture({ usage, script: () => "done" });
		fixtures.push(fixture);
		const task = await fixture.call<{ id: string }>("agents.spawn", {
			definition: "rlm-child@1",
			input: { prompt: "x" },
		});
		await fixture.call("agents.result", { id: task.id });
		expect((await usage.status()).usage.taskCalls).toBe(1);
	});
});

describe("ledgers saved before Jev was removed", () => {
	test("a stored Jev call and the old jevCalls totals still load, and the call still counts", async () => {
		let saved: JsonValue | undefined;
		const store = {
			read: async () => saved,
			write: async (document: JsonValue) => {
				saved = structuredClone(document);
			},
		};
		const ledger = new NativeUsageLedger(store);
		const reservation = await ledger.reserve({ kind: "model", rootId: "turn:x", requestKey: "m1" });
		await ledger.settle(reservation, {
			status: "succeeded",
			usage: { inputTokens: 1, outputTokens: 1, totalTokens: 2, cost: 0 },
		});
		// Rewrite the document as an older Ultron stored it: the call is a Jev call, and totals carry jevCalls.
		const legacy = (value: unknown): void => {
			if (Array.isArray(value)) {
				for (const item of value) legacy(item);
				return;
			}
			if (value === null || typeof value !== "object") return;
			const record = value as Record<string, unknown>;
			if (record.kind === "model") record.kind = "jev";
			if ("modelCalls" in record) record.jevCalls = 0;
			if (record.byKind !== undefined && typeof record.byKind === "object")
				(record.byKind as Record<string, unknown>).jev = structuredClone(
					(record.byKind as Record<string, unknown>).model,
				);
			for (const child of Object.values(record)) legacy(child);
		};
		legacy(saved);
		expect(JSON.stringify(saved)).toContain('"kind":"jev"');
		const reopened = new NativeUsageLedger(store);
		const status = await reopened.status("turn:x");
		expect(status.usage).toMatchObject({ calls: 1, modelCalls: 0, taskCalls: 0 });
		expect(status.usage).not.toHaveProperty("jevCalls");
		// The ledger keeps working: new work is admitted and counted alongside the old call.
		await reopened.settle(await reopened.reserve({ kind: "task", rootId: "turn:x", requestKey: "t1" }), {
			status: "succeeded",
		});
		expect((await reopened.status("turn:x")).usage).toMatchObject({ calls: 2, taskCalls: 1 });
	});
});

describe("optional cost cap", () => {
	async function settleModel(ledger: NativeUsageLedger, key: string, cost: number | null) {
		const reservation = await ledger.reserve({ kind: "model", rootId: "turn:x", requestKey: key });
		await ledger.settle(reservation, {
			status: "succeeded",
			usage: { inputTokens: 1, outputTokens: 1, totalTokens: 2, cost },
		});
	}

	test("under the cap admits; at or over the cap refuses new model-backed work", async () => {
		const ledger = new NativeUsageLedger(undefined, { limits: { maxCostUsd: 1 } });
		await settleModel(ledger, "m1", 0.4);
		await expect(ledger.reserve({ kind: "task", rootId: "turn:x", requestKey: "t1" })).resolves.toBeDefined();
		await settleModel(ledger, "m2", 0.6);
		await expect(ledger.reserve({ kind: "task", rootId: "turn:x", requestKey: "t2" })).rejects.toThrow(
			"Usage cost cap reached for root turn:x",
		);
		await expect(ledger.reserve({ kind: "model", rootId: "turn:x", requestKey: "m3" })).rejects.toThrow(
			"Usage cost cap reached",
		);
		// Work that calls no model is still admitted.
		await expect(
			ledger.reserve({ kind: "task", rootId: "turn:x", requestKey: "d1", modelBacked: false }),
		).resolves.toBeDefined();
		// The cap is per root: a new turn starts with nothing spent.
		await expect(ledger.reserve({ kind: "task", rootId: "turn:y", requestKey: "t3" })).resolves.toBeDefined();
		const status = await ledger.status("turn:x");
		expect(status.limits.maxCostUsd).toBe(1);
		expect(status.cost).toEqual({ maxCostUsd: 1, spentUsd: 1, unknownPricedCalls: 0, remainingUsd: 0 });
	});

	test("unknown pricing refuses model-backed admission when a cap is set", async () => {
		const ledger = new NativeUsageLedger(undefined, { limits: { maxCostUsd: 100 } });
		await settleModel(ledger, "m1", null);
		await expect(ledger.reserve({ kind: "task", rootId: "turn:x", requestKey: "t1" })).rejects.toThrow(
			"pricing unknown; cannot enforce cost cap",
		);
		expect((await ledger.status("turn:x")).cost).toMatchObject({ spentUsd: 0, unknownPricedCalls: 1 });
	});

	test("no cap never refuses, whatever was spent or unknown", async () => {
		const ledger = new NativeUsageLedger();
		await settleModel(ledger, "m1", null);
		await settleModel(ledger, "m2", 1_000_000);
		await expect(ledger.reserve({ kind: "task", rootId: "turn:x", requestKey: "t1" })).resolves.toBeDefined();
		await expect(ledger.reserve({ kind: "model", rootId: "turn:x", requestKey: "m3" })).resolves.toBeDefined();
		expect((await ledger.status("turn:x")).cost).toEqual({
			maxCostUsd: null,
			spentUsd: 1_000_000,
			unknownPricedCalls: 1,
			remainingUsd: null,
		});
	});

	test("agents.status reports the cap and spend of the current turn", async () => {
		const usage = new NativeUsageLedger(undefined, { limits: { maxCostUsd: 2.5 } });
		const fixture = hostFixture({ usage, rootTurns: true, script: () => "done" });
		fixtures.push(fixture);
		fixture.host.beginRootTurn("r");
		const status = await fixture.call<{ limits: { maxCostUsd: number }; usage: { cost: unknown } }>("agents.status");
		expect(status.limits.maxCostUsd).toBe(2.5);
		expect(status.usage.cost).toEqual({ maxCostUsd: 2.5, spentUsd: 0, unknownPricedCalls: 0, remainingUsd: 2.5 });
	});
});

describe("usage limits from the environment", () => {
	test("defaults, overrides, unlimited and invalid values", () => {
		const tokens = { defaultMaxTotalTokens: DEFAULT_MAX_TOTAL_TOKENS };
		expect(nativeUsageLimitsFromEnv({})).toEqual({ maxAdmittedTasks: 24, maxWallMs: 30 * 60 * 1000, ...tokens });
		expect(
			nativeUsageLimitsFromEnv({
				ULTRON_MAX_WALL_MS: "90000",
				ULTRON_MAX_ADMITTED_TASKS: "4",
				ULTRON_MAX_COST_USD: "$1.50",
			}),
		).toEqual({ maxAdmittedTasks: 4, maxWallMs: 90_000, maxCostUsd: 1.5, ...tokens });
		expect(nativeUsageLimitsFromEnv({ ULTRON_MAX_WALL_MS: "none", ULTRON_MAX_ADMITTED_TASKS: "off" })).toEqual(
			tokens,
		);
		expect(
			nativeUsageLimitsFromEnv({
				ULTRON_MAX_WALL_MS: "none",
				ULTRON_MAX_ADMITTED_TASKS: "off",
				ULTRON_MAX_TOTAL_TOKENS: "off",
			}),
		).toEqual({});
		expect(
			nativeUsageLimitsFromEnv({
				ULTRON_MAX_WALL_MS: "soon",
				ULTRON_MAX_ADMITTED_TASKS: "-1",
				ULTRON_MAX_COST_USD: "x",
			}),
		).toEqual({ maxAdmittedTasks: 24, maxWallMs: 30 * 60 * 1000, ...tokens });
	});
});
