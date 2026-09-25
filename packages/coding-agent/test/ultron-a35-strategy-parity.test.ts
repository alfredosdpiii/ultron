import type { Context } from "@ultron/agent-core";
import type { JsonValue } from "@ultron/chord";
import { afterEach, describe, expect, test } from "vitest";
import type { NativeDefinitionAdapterRequest } from "../src/ultron/rlm/definition-registry.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";
import { aborted, definition, hostFixture, journal } from "./ultron-host-fixtures.ts";

/**
 * A35: deterministic, predict, and rlm methods share policy, validation, accounting, and task
 * semantics. The same typed definition implemented by each strategy gets the same idempotency,
 * timeout/cancel, usage reservation, and journal behavior. Only rlm launches an iterative lane.
 */

const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
});

const STRATEGIES = [
	["deterministic", "det@1"],
	["predict", "pred@1"],
	["rlm", "loop@1"],
] as const;

/** n doubles; negative n answers schema-invalid; n = 999 hangs until the task is aborted. */
async function answer(n: number, signal: Promise<never>): Promise<JsonValue> {
	if (n === 999) return signal;
	return n < 0 ? { doubled: "invalid" } : { doubled: n * 2 };
}

type Journaled = NativeHostStore & { states: Map<string, string[]> };

/** Records every durable state each task passes through. */
function journaledStore(): Journaled {
	let value: JsonValue | undefined;
	const states = new Map<string, string[]>();
	return {
		states,
		read: async () => structuredClone(value),
		write: async (next) => {
			value = structuredClone(next);
			for (const task of (next as { tasks: Array<{ id: string; state: string }> }).tasks) {
				const seen = states.get(task.id) ?? [];
				if (seen.at(-1) !== task.state) seen.push(task.state);
				states.set(task.id, seen);
			}
		},
	};
}

async function parityFixture() {
	const adapterCalls: Array<{ strategy: string; aborted: () => boolean }> = [];
	const usageDocument: { value?: JsonValue } = {};
	const usage = new NativeUsageLedger({
		read: async () => usageDocument.value,
		write: async (next) => {
			usageDocument.value = structuredClone(next);
		},
	});
	const adapter =
		(strategy: string) =>
		({ input, signal }: NativeDefinitionAdapterRequest) => {
			adapterCalls.push({ strategy, aborted: () => signal.aborted });
			return answer(
				(input as { n: number }).n,
				new Promise((_resolve, reject) => signal.addEventListener("abort", () => reject(signal.reason))),
			);
		};
	const store = journaledStore();
	const fixture = hostFixture({
		store,
		usage,
		deterministic: adapter("deterministic"),
		predict: adapter("predict"),
		script: async (_lane, prompt, laneContext: Context) =>
			JSON.stringify(
				await answer((JSON.parse(/Input data:\n(.*)\n/.exec(prompt)![1]) as { n: number }).n, aborted(laneContext)),
			),
	});
	fixtures.push(fixture);
	for (const [strategy, key] of STRATEGIES)
		await fixture.call("agents.register", { definition: definition(key.split("@")[0]!, strategy) });
	const calls = () =>
		(
			(usageDocument.value as {
				roots: Record<string, { calls: Array<{ kind: string; status: string; requestKey?: string }> }>;
			}) ?? { roots: {} }
		).roots["ultron-root"]?.calls ?? [];
	return { fixture, adapterCalls, usage, store, calls };
}

describe("A35 deterministic/predict/rlm strategy parity", () => {
	test("only rlm launches a lane; predict and deterministic never start an iterative kernel", async () => {
		const { fixture, adapterCalls } = await parityFixture();
		for (const [, key] of STRATEGIES) {
			for (const n of [2, -1]) await fixture.call("agents.invoke", { definition: key, input: { n } });
			await fixture.call("agents.invoke", { definition: key, input: { n: 999 }, timeout_ms: 10 });
		}
		expect(fixture.laneCalls).toHaveLength(3);
		for (const lane of fixture.laneCalls) expect(lane).toMatch(/^ultron\.loop\.ultron-task-/);
		expect(adapterCalls.map((call) => call.strategy)).toEqual([
			"deterministic",
			"deterministic",
			"deterministic",
			"predict",
			"predict",
			"predict",
		]);
	});

	test("the same validation and result contract applies to every strategy", async () => {
		const { fixture } = await parityFixture();
		for (const [, key] of STRATEGIES) {
			const id = key.split("@")[0];
			expect(await fixture.call("agents.invoke", { definition: key, input: { n: 4 } })).toEqual({
				status: "succeeded",
				value: { doubled: 8 },
				verification: "unverified",
			});
			expect(await fixture.call("agents.invoke", { definition: key, input: { n: -1 } })).toEqual({
				status: "failed",
				error: `${id}@1 output does not match its schema`,
				verification: "unverified",
			});
			await expect(fixture.call("agents.invoke", { definition: key, input: { n: "4" } })).rejects.toThrow(
				`${id}@1 input does not match its schema`,
			);
		}
		expect(await journal(fixture)).toHaveLength(6);
	});

	test("idempotency keys behave identically across strategies", async () => {
		const { fixture, adapterCalls } = await parityFixture();
		for (const [strategy, key] of STRATEGIES) {
			const request = { definition: key, input: { n: 3 }, key: `same-${strategy}` };
			const [first, second] = await Promise.all([
				fixture.call("agents.invoke", request),
				fixture.call("agents.invoke", request),
			]);
			expect(first).toEqual(second);
			expect(await fixture.call("agents.invoke", request)).toEqual(first);
			await expect(fixture.call("agents.invoke", { ...request, input: { n: 4 } })).rejects.toThrow(
				"Idempotency key reused for a different task",
			);
		}
		expect(await journal(fixture)).toHaveLength(3);
		expect(adapterCalls).toHaveLength(2);
		expect(fixture.prompts).toHaveLength(1);
	});

	test("timeout and cancellation commit the same cancelled result and abort the work for every strategy", async () => {
		const { fixture, adapterCalls } = await parityFixture();
		for (const [, key] of STRATEGIES) {
			expect(await fixture.call("agents.invoke", { definition: key, input: { n: 999 }, timeout_ms: 10 })).toEqual({
				status: "cancelled",
				error: "Ultron task exceeded 10ms timeout",
				verification: "unverified",
			});
			const spawned = await fixture.call<{ id: string }>("agents.spawn", { definition: key, input: { n: 999 } });
			await new Promise((resolve) => setTimeout(resolve, 5));
			expect(await fixture.call("agents.cancel", { id: spawned.id })).toEqual({ cancelled: true });
			expect(await fixture.call("agents.result", { id: spawned.id })).toEqual({
				status: "cancelled",
				error: "Ultron task cancelled",
				verification: "unverified",
			});
		}
		for (const call of adapterCalls) expect(call.aborted()).toBe(true);
		expect(fixture.aborts).toHaveLength(2);
	});

	test("usage reservations and durable journal transitions match across strategies", async () => {
		const { fixture, usage, store, calls } = await parityFixture();
		const ids: Record<string, string[]> = {};
		for (const [strategy, key] of STRATEGIES) {
			for (const n of [5, -5]) {
				const spawned = await fixture.call<{ id: string }>("agents.spawn", { definition: key, input: { n } });
				await fixture.call("agents.result", { id: spawned.id });
				ids[strategy] = [...(ids[strategy] ?? []), spawned.id];
			}
		}
		// Every task, whatever its strategy, is admitted, runs, and ends durably.
		for (const [strategy] of STRATEGIES) {
			const [succeeded, failed] = ids[strategy]!;
			expect(store.states.get(succeeded!)).toEqual(["admitted", "running", "completed"]);
			expect(store.states.get(failed!)).toEqual(["admitted", "running", "failed"]);
		}
		// One task reservation per task, settled with the task's outcome; model calls are charged
		// for the strategies that infer (predict, rlm) and never for deterministic code.
		const recorded = calls();
		const taskCalls = recorded.filter((call) => call.kind === "task");
		expect(taskCalls.map((call) => call.status)).toEqual([
			"succeeded",
			"failed",
			"succeeded",
			"failed",
			"succeeded",
			"failed",
		]);
		const modelCallsFor = (taskIds: string[]) =>
			recorded.filter(
				(call) => call.kind === "model" && taskIds.some((id) => call.requestKey?.startsWith(`${id}:model`)),
			).length;
		expect(modelCallsFor(ids.deterministic!)).toBe(0);
		expect(modelCallsFor(ids.predict!)).toBe(2);
		expect(modelCallsFor(ids.rlm!)).toBe(2);
		const status = await usage.status();
		expect(status.activeReservations).toBe(0);
		expect(status.usage).toMatchObject({ taskCalls: 6, modelCalls: 4 });
	});
});
