import type { Context } from "@earendil-works/pi-agent-core";
import { afterEach, describe, expect, test } from "vitest";
import { NativeUsageLedger } from "../src/ultron/usage.ts";
import { aborted, deferred, hostFixture, journal, waitFor } from "./ultron-host-fixtures.ts";

/**
 * A09: concurrent calls, nested children, and idempotent retries cannot bypass tree budget
 * admission. The admitted-task limit caps unfinished tasks across the whole root tree; the wall
 * deadline caps every child deadline and refuses admission once spent; missing usage and unknown
 * pricing stay unknown (null) rather than becoming zero.
 */

const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
});

const LIMIT_ERROR = "Usage admitted-task limit exceeded";

function budgetFixture(limits: { maxAdmittedTasks?: number; maxWallMs?: number }) {
	const usage = new NativeUsageLedger(undefined, { limits });
	const gate = deferred();
	let running = 0;
	let peak = 0;
	const nested: Array<PromiseSettledResult<unknown>> = [];
	const fixture = hostFixture({
		usage,
		script: async (lane: string, prompt: string, laneContext: Context) => {
			running += 1;
			peak = Math.max(peak, running);
			try {
				if (prompt.startsWith("parent:")) {
					// A child lane spawns its own children through the same host, as its kernel would.
					const count = Number(prompt.slice("parent:".length));
					const spawns = Array.from({ length: count }, () =>
						fixture.host.handle(
							"agents.spawn",
							{ definition: "rlm-child@1", input: { prompt: "hold" } },
							laneContext,
							{ lane },
						),
					);
					nested.push(...(await Promise.allSettled(spawns)));
				}
				await Promise.race([gate.promise, aborted(laneContext)]);
				return "done";
			} finally {
				running -= 1;
			}
		},
	});
	fixtures.push(fixture);
	const spawn = (prompt = "hold", key?: string) =>
		fixture.call<{ id: string; state: string }>("agents.spawn", {
			definition: "rlm-child@1",
			input: { prompt },
			...(key === undefined ? {} : { key }),
		});
	return { fixture, usage, gate, spawn, nested, peak: () => peak };
}

function rejectedWith(results: PromiseSettledResult<unknown>[], message: string): number {
	return results.filter((result) => result.status === "rejected" && String(result.reason).includes(message)).length;
}

describe("A09 tree budget admission under concurrency", () => {
	test("a burst of concurrent spawns admits exactly the limit and rejects the rest", async () => {
		const { fixture, usage, gate, spawn, peak } = budgetFixture({ maxAdmittedTasks: 3 });
		const results = await Promise.allSettled(Array.from({ length: 20 }, () => spawn()));
		expect(results.filter((result) => result.status === "fulfilled")).toHaveLength(3);
		expect(rejectedWith(results, LIMIT_ERROR)).toBe(17);
		expect(await journal(fixture)).toHaveLength(3);
		expect((await usage.status()).admittedTasks).toBe(3);
		await waitFor(() => fixture.prompts.length === 3);
		gate.resolve();
		await waitFor(async () => (await journal(fixture)).every((task) => task.state === "completed"));
		expect(peak()).toBeLessThanOrEqual(3);
		// Finished tasks release their slots; the limit is on unfinished work in the tree.
		expect((await usage.status()).admittedTasks).toBe(0);
		await expect(spawn("fresh")).resolves.toMatchObject({ id: expect.any(String) });
	});

	test("nested children spawned from child lanes share the root tree budget", async () => {
		const { fixture, usage, gate, spawn, nested } = budgetFixture({ maxAdmittedTasks: 4 });
		const parents = await Promise.all([spawn("parent:3"), spawn("parent:3")]);
		await waitFor(() => nested.length === 6);
		expect(nested.filter((result) => result.status === "fulfilled")).toHaveLength(2);
		expect(rejectedWith(nested, LIMIT_ERROR)).toBe(4);
		const tasks = await journal(fixture);
		expect(tasks).toHaveLength(4);
		const children = tasks.filter((task) => task.parentId !== undefined);
		expect(children).toHaveLength(2);
		for (const child of children) expect(parents.map((parent) => parent.id)).toContain(child.parentId);
		expect((await usage.status()).admittedTasks).toBe(4);
		// The root is refused too while its descendants hold the tree's slots.
		await expect(spawn()).rejects.toThrow(LIMIT_ERROR);
		gate.resolve();
		await waitFor(async () => (await journal(fixture)).every((task) => task.state === "completed"));
		expect((await usage.status()).admittedTasks).toBe(0);
	});

	test("idempotent retries neither consume a slot nor release the live original's slot", async () => {
		const { fixture, usage, gate, spawn } = budgetFixture({ maxAdmittedTasks: 2 });
		const original = await spawn("hold", "retry-key");
		await spawn();
		const retries = await Promise.all(Array.from({ length: 10 }, () => spawn("hold", "retry-key")));
		for (const retry of retries) expect(retry.id).toBe(original.id);
		// Retrying must not have settled the original's reservation: the tree is still full.
		expect((await usage.status()).admittedTasks).toBe(2);
		await expect(spawn()).rejects.toThrow(LIMIT_ERROR);
		await expect(spawn("different input", "retry-key")).rejects.toThrow("Idempotency key reused");
		expect(await journal(fixture)).toHaveLength(2);

		gate.resolve();
		await waitFor(async () => (await journal(fixture)).every((task) => task.state === "completed"));
		const before = (await usage.status()).usage;
		// A retry after completion returns the durable result and records no new task call.
		expect(
			await fixture.call("agents.invoke", {
				definition: "rlm-child@1",
				input: { prompt: "hold" },
				key: "retry-key",
			}),
		).toEqual({ status: "succeeded", value: "done", verification: "unverified" });
		expect((await usage.status()).usage).toEqual(before);
		expect(fixture.prompts.filter((entry) => entry.prompt === "hold")).toHaveLength(2);
	});

	test("the root wall deadline caps child deadlines and refuses admission once spent", async () => {
		const { fixture, usage, spawn } = budgetFixture({ maxWallMs: 150 });
		// The default 30-minute task timeout is capped by the 150ms root deadline, not refused.
		const first = await spawn();
		const second = await spawn();
		const results = await Promise.all([first, second].map((task) => fixture.call("agents.result", { id: task.id })));
		for (const result of results)
			expect(result).toEqual({
				status: "cancelled",
				error: "Ultron root wall deadline exceeded",
				verification: "unverified",
			});
		await expect(spawn()).rejects.toThrow("Usage wall deadline exceeded");
		const status = await usage.status();
		expect(status.remainingWallMs).toBe(0);
		expect(status.activeReservations).toBe(0);
		expect(status.usage.taskCalls).toBe(2);
	});

	test("missing usage and unknown pricing stay unknown, never zero", async () => {
		const { fixture, usage, gate, spawn } = budgetFixture({});
		gate.resolve();
		const task = await spawn();
		await fixture.call("agents.result", { id: task.id });
		// The fake provider reports no usage: task and model calls are recorded as unknown.
		const recorded = (await usage.status()).usage;
		expect(recorded).toMatchObject({ calls: 2, taskCalls: 1, modelCalls: 1, unknownCalls: 2 });
		expect(recorded).toMatchObject({ inputTokens: null, outputTokens: null, totalTokens: null, cost: null });

		const ledger = new NativeUsageLedger();
		const priced = await ledger.reserve({ kind: "model", requestKey: "priced" });
		const unpriced = await ledger.reserve({ kind: "model", requestKey: "unpriced" });
		await ledger.settle(priced, {
			status: "succeeded",
			usage: { inputTokens: 10, outputTokens: 5, totalTokens: 15, cost: 0.01 },
		});
		// Known tokens, unknown price: tokens total, cost stays unresolved for the whole root.
		await ledger.settle(unpriced, {
			status: "succeeded",
			usage: { inputTokens: 20, outputTokens: 5, totalTokens: 25, cost: null },
		});
		expect((await ledger.status()).usage).toMatchObject({
			inputTokens: 30,
			outputTokens: 10,
			totalTokens: 40,
			cost: null,
			unknownCalls: 1,
		});
		const invalid = await ledger.reserve({ kind: "model", requestKey: "invalid" });
		await expect(ledger.settle(invalid, { usage: { cost: Number.NaN } })).rejects.toThrow(
			"Invalid usage ledger cost",
		);
	});

	test("direct ledger reservations race to exactly the limit", async () => {
		const ledger = new NativeUsageLedger(undefined, { limits: { maxAdmittedTasks: 5 } });
		const results = await Promise.allSettled(
			Array.from({ length: 50 }, (_, index) => ledger.reserve({ kind: "task", requestKey: `task-${index}` })),
		);
		expect(results.filter((result) => result.status === "fulfilled")).toHaveLength(5);
		expect(rejectedWith(results, LIMIT_ERROR)).toBe(45);
		// A retried request key reuses its reservation instead of taking another slot.
		await expect(ledger.reserve({ kind: "task", requestKey: "task-0" })).resolves.toMatchObject({
			requestKey: "task-0",
		});
		expect((await ledger.status()).admittedTasks).toBe(5);
	});
});
