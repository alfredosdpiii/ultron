import { afterEach, describe, expect, test } from "vitest";
import { aborted, deferred, hostFixture, journal, memoryStore, waitFor } from "./ultron-host-fixtures.ts";

/**
 * A13: admitting (or starting) a task does not satisfy a graph dependency. A dependent is
 * neither admitted nor started until its dependency's terminal result is durably committed, and a
 * dependency that ends without success skips its dependent even if a valid result arrives late.
 */

const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
});

type StoredDocument = { tasks: Array<{ id: string; definition: string; state: string; result?: unknown }> };

async function slowFixture() {
	const release = deferred<string>();
	const events: Array<{ event: string; at: number; slowStored?: string }> = [];
	const store = memoryStore();
	const slowState = () =>
		(store.document() as StoredDocument | undefined)?.tasks.find((task) => task.definition === "rlm-child@1")?.state;
	const fixture = hostFixture({
		store,
		script: async (_lane, prompt, laneContext) => {
			events.push({ event: `${prompt}:start`, at: performance.now() });
			const reply = await Promise.race([release.promise, aborted(laneContext)]);
			events.push({ event: `${prompt}:reply`, at: performance.now() });
			return reply;
		},
		deterministic: async ({ input }) => {
			// Snapshot what the durable journal says about the dependency at the moment we start.
			events.push({ event: "dependent:start", at: performance.now(), slowStored: slowState() });
			return input;
		},
	});
	fixtures.push(fixture);
	return { fixture, release, events };
}

async function settle(): Promise<void> {
	for (let turn = 0; turn < 20; turn += 1) await new Promise((resolve) => setTimeout(resolve, 1));
}

describe("A13 admission does not satisfy a graph dependency", () => {
	test("a dependent is not admitted or started until the slow dependency's result is durable", async () => {
		const { fixture, release, events } = await slowFixture();
		const workflow = fixture.call<Record<string, { status: string; value?: unknown }>>("workflows.run", {
			nodes: [
				{ id: "slow", definition: "rlm-child@1", input: { prompt: "slow" } },
				{ id: "dependent", definition: "identity@1", dependsOn: ["slow"], inputFrom: "slow" },
			],
		});
		await waitFor(() => events.some((entry) => entry.event === "slow:start"));
		await settle();
		// The dependency is admitted and running, which is not enough to start the dependent.
		const during = await journal(fixture);
		expect(during).toHaveLength(1);
		expect(during[0]).toMatchObject({ definition: "rlm-child@1", state: "running" });
		expect(during[0]!.result).toBeUndefined();
		expect(events.map((entry) => entry.event)).toEqual(["slow:start"]);

		release.resolve("delayed valid result");
		const output = await workflow;
		expect(output.slow).toEqual({ status: "succeeded", value: "delayed valid result", verification: "unverified" });
		expect(output.dependent).toEqual({
			status: "succeeded",
			value: "delayed valid result",
			verification: "unverified",
		});
		expect(events.map((entry) => entry.event)).toEqual(["slow:start", "slow:reply", "dependent:start"]);
		const [, reply, dependent] = events;
		expect(dependent!.at).toBeGreaterThanOrEqual(reply!.at);
		// When the dependent started, the dependency's terminal result was already committed.
		expect(dependent!.slowStored).toBe("completed");
	});

	test("agents.spawn confirms admission only; the result is not available until the task ends", async () => {
		const { fixture, release } = await slowFixture();
		const handle = await fixture.call<{ id: string; state: string }>("agents.spawn", {
			definition: "rlm-child@1",
			input: { prompt: "slow" },
		});
		expect(handle.state).toBe("admitted");
		let resolved = false;
		const result = fixture.call("agents.result", { id: handle.id }).then((value) => {
			resolved = true;
			return value;
		});
		await settle();
		expect(resolved).toBe(false);
		expect(await fixture.call("agents.inspect", { id: handle.id })).not.toHaveProperty("result");
		release.resolve("late but valid");
		expect(await result).toEqual({ status: "succeeded", value: "late but valid", verification: "unverified" });
	});

	test("a dependency that ends unsuccessfully skips its dependent, and a late valid result cannot revive it", async () => {
		const { fixture, release, events } = await slowFixture();
		const output = await fixture.call<Record<string, { status: string; reason?: string }>>("workflows.run", {
			nodes: [
				{ id: "slow", definition: "rlm-child@1", input: { prompt: "slow" }, timeout_ms: 30 },
				{ id: "dependent", definition: "identity@1", dependsOn: ["slow"], inputFrom: "slow" },
			],
		});
		expect(output.slow).toMatchObject({ status: "cancelled", verification: "unverified" });
		expect(output.dependent).toEqual({ status: "skipped", reason: "Dependency slow did not succeed (cancelled)" });
		release.resolve("too late");
		await settle();
		expect(events.some((entry) => entry.event === "dependent:start")).toBe(false);
		const tasks = await journal(fixture);
		expect(tasks).toHaveLength(1);
		expect(tasks[0]).toMatchObject({ state: "cancelled", result: { status: "cancelled" } });
	});
});
