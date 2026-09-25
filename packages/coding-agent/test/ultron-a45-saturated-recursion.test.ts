import type { Context } from "@earendil-works/pi-agent-core";
import { afterEach, describe, expect, test } from "vitest";
import { NativeUsageLedger } from "../src/ultron/usage.ts";
import { aborted, deferred, hostFixture, journal, waitFor } from "./ultron-host-fixtures.ts";

/**
 * A45: saturated nested recursion makes progress or returns a bounded capacity failure, never a
 * deadlock. Admission beyond the tree limit is refused immediately (no hidden queue), a parent
 * that is awaiting its children does not block them, and cancelling a saturated parent cancels
 * its waiting descendants and releases every slot.
 *
 * Children are spawned with the context the host handed the parent's lane, as the RLM kernel does
 * (its host requests carry the active cell's abort signal).
 */

const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
});

const LIMIT_ERROR = "Usage admitted-task limit exceeded";
const BOUND_MS = 2_000;

function saturatedFixture(maxAdmittedTasks: number) {
	const usage = new NativeUsageLedger(undefined, { limits: { maxAdmittedTasks } });
	const saturated = deferred();
	const grandchildRefusals: string[] = [];
	const fixture = hostFixture({
		usage,
		script: async (lane: string, prompt: string, laneContext: Context) => {
			const call = <T>(type: string, payload: Record<string, unknown>) =>
				fixture.host.handle(type, payload, laneContext, { lane }) as Promise<T>;
			const child = (childPrompt: string) =>
				call<{ id: string }>("agents.spawn", { definition: "rlm-child@1", input: { prompt: childPrompt } });
			if (prompt === "parent" || prompt === "hanging-parent") {
				const children = [];
				for (let index = 0; index < 2; index += 1)
					children.push(await child(prompt === "parent" ? "child" : "hanging-child"));
				let refused = "";
				try {
					await call("agents.invoke", { definition: "rlm-child@1", input: { prompt: "child" } });
				} catch (error) {
					refused = (error as Error).message;
				}
				const graph = await call<Record<string, { status: string; error?: string }>>("workflows.run", {
					nodes: [{ id: "extra", definition: "rlm-child@1", input: { prompt: "child" } }],
				});
				saturated.resolve();
				// The parent now waits on children while the tree is full.
				const results = await Promise.all(
					children.map((handle) => call<{ status: string }>("agents.result", { id: handle.id })),
				);
				return JSON.stringify({ refused, graph: graph.extra, children: results.map((result) => result.status) });
			}
			if (prompt === "hanging-child") return aborted(laneContext);
			await saturated.promise;
			try {
				await child("grandchild");
			} catch (error) {
				grandchildRefusals.push((error as Error).message);
			}
			return "child done";
		},
	});
	fixtures.push(fixture);
	return { fixture, usage, grandchildRefusals };
}

async function within<T>(promise: Promise<T>, ms = BOUND_MS): Promise<T> {
	let timer: ReturnType<typeof setTimeout> | undefined;
	try {
		return await Promise.race([
			promise,
			new Promise<never>((_resolve, reject) => {
				timer = setTimeout(() => reject(new Error(`no progress within ${ms}ms (deadlock)`)), ms);
			}),
		]);
	} finally {
		clearTimeout(timer);
	}
}

describe("A45 saturated nested recursion", () => {
	test("a parent awaiting children at saturation gets bounded capacity failures and still completes", async () => {
		const { fixture, usage, grandchildRefusals } = saturatedFixture(3);
		const started = Date.now();
		const result = await within(
			fixture.call<{ status: string; value: string }>("agents.invoke", {
				definition: "rlm-child@1",
				input: { prompt: "parent" },
			}),
		);
		expect(Date.now() - started).toBeLessThan(BOUND_MS);
		expect(result.status).toBe("succeeded");
		const summary = JSON.parse(result.value) as {
			refused: string;
			graph: { status: string; error: string };
			children: string[];
		};
		// Excess admission is refused immediately with a typed capacity error, not queued.
		expect(summary.refused).toContain(LIMIT_ERROR);
		expect(summary.graph).toMatchObject({ status: "failed", verification: "unverified" });
		expect(summary.graph.error).toContain(LIMIT_ERROR);
		// The children the parent was waiting for made progress and finished.
		expect(summary.children).toEqual(["succeeded", "succeeded"]);
		// Grandchildren were refused while the tree was full; the children finished anyway.
		expect(grandchildRefusals).toHaveLength(2);
		for (const refusal of grandchildRefusals) expect(refusal).toContain(LIMIT_ERROR);
		const tasks = await journal(fixture);
		expect(tasks).toHaveLength(3);
		expect(tasks.every((task) => task.state === "completed")).toBe(true);
		const status = await usage.status();
		expect(status.admittedTasks).toBe(0);
		expect(status.activeReservations).toBe(0);
	});

	test("cancelling a saturated parent cancels its waiting children and releases every slot", async () => {
		const { fixture, usage } = saturatedFixture(3);
		const parent = await fixture.call<{ id: string }>("agents.spawn", {
			definition: "rlm-child@1",
			input: { prompt: "hanging-parent" },
		});
		await within(waitFor(async () => (await journal(fixture)).length === 3));
		// Saturated: parent plus two hanging children hold every slot.
		expect((await usage.status()).admittedTasks).toBe(3);
		await expect(
			fixture.call("agents.spawn", { definition: "rlm-child@1", input: { prompt: "child" } }),
		).rejects.toThrow(LIMIT_ERROR);

		expect(await within(fixture.call("agents.cancel", { id: parent.id }))).toEqual({ cancelled: true });
		await within(waitFor(async () => (await journal(fixture)).every((task) => task.result !== undefined)));
		const tasks = await journal(fixture);
		expect(tasks.map((task) => task.state)).toEqual(["cancelled", "cancelled", "cancelled"]);
		for (const task of tasks.filter((candidate) => candidate.id !== parent.id)) {
			expect(task.parentId).toBe(parent.id);
			expect(task.result).toMatchObject({ status: "cancelled", error: "Parent task aborted" });
		}
		await within(waitFor(() => fixture.aborts.length === 3));
		const status = await usage.status();
		expect(status.admittedTasks).toBe(0);
		expect(status.activeReservations).toBe(0);
		// Capacity is available again.
		const next = await fixture.call<{ id: string }>("agents.spawn", {
			definition: "identity@1",
			input: { after: "cancel" },
		});
		expect(await fixture.call("agents.result", { id: next.id })).toMatchObject({ status: "succeeded" });
	});
});
