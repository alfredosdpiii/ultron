import { fileURLToPath } from "node:url";
import { withAbortSignal } from "@earendil-works/chord/context";
import { afterEach, describe, expect, test } from "vitest";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { context, definition, hostFixture, journal } from "./ultron-host-fixtures.ts";

/**
 * A01: one definition, invoked directly (`agents.invoke`), as a graph node (`workflows.run`), and
 * from a real Python RLM kernel (`await agents.invoke(...)`), shares one result contract and one
 * durable task-record contract. Invalid output and invalid input fail identically on every path.
 */

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const cleanups: Array<() => Promise<void>> = [];

afterEach(async () => {
	for (const cleanup of cleanups.splice(0).reverse()) await cleanup();
});

/** RLM lane: doubles `n`; for negative `n` it answers with a schema-invalid result. */
function doublerScript(_lane: string, prompt: string): string {
	const match = /Input data:\n(.*)\n/.exec(prompt);
	const input = JSON.parse(match![1]) as { n: number };
	return input.n < 0 ? JSON.stringify({ doubled: "not a number" }) : JSON.stringify({ doubled: input.n * 2 });
}

async function setup() {
	const fixture = hostFixture({ script: doublerScript });
	await fixture.call("agents.register", { definition: definition("doubler", "rlm") });
	const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, (type, payload, signal) =>
		fixture.host.handle(type, payload, signal ? withAbortSignal(signal, context) : context, { lane: "main" }),
	);
	cleanups.push(async () => {
		await kernel.shutdown();
		await fixture.host.close();
	});
	/** Three invocation paths for the same definition and input. */
	const paths = {
		direct: (input: unknown) => fixture.call("agents.invoke", { definition: "doubler@1", input }),
		graph: async (input: unknown) =>
			(
				(await fixture.call("workflows.run", { nodes: [{ id: "only", definition: "doubler@1", input }] })) as {
					only: unknown;
				}
			).only,
		rlm: async (input: unknown) => {
			const result = await kernel.execute(
				`import json\nresult = await agents.invoke("doubler@1", json.loads(${JSON.stringify(JSON.stringify(input))}))\nprint(json.dumps(result))`,
			);
			if (result.status !== "ok") throw new Error(`${result.error?.ename}: ${result.error?.evalue}`);
			return JSON.parse(result.stdout);
		},
	};
	return { fixture, kernel, paths };
}

describe("A01 direct, graph, and RLM invocation parity", () => {
	test("the same definition yields the same validated result and durable task record on all three paths", async () => {
		const { fixture, paths } = await setup();
		const direct = await paths.direct({ n: 21 });
		const graph = await paths.graph({ n: 21 });
		const rlm = await paths.rlm({ n: 21 });
		const expected = { status: "succeeded", value: { doubled: 42 }, verification: "unverified" };
		expect(direct).toEqual(expected);
		expect(graph).toEqual(expected);
		expect(rlm).toEqual(expected);

		const tasks = await journal(fixture);
		expect(tasks).toHaveLength(3);
		for (const task of tasks) {
			expect(task).toMatchObject({ definition: "doubler@1", state: "completed", result: expected });
			expect(task.parentId).toBeUndefined();
		}
		// Same definition, input, model, and timeout: one fingerprint, three distinct task identities.
		expect(new Set(tasks.map((task) => task.fingerprint)).size).toBe(1);
		expect(new Set(tasks.map((task) => task.id)).size).toBe(3);
		// Each path executed the identical definition prompt on its own lane.
		expect(fixture.prompts).toHaveLength(3);
		expect(new Set(fixture.prompts.map((entry) => entry.prompt)).size).toBe(1);
		expect(new Set(fixture.prompts.map((entry) => entry.lane)).size).toBe(3);
	});

	test("schema-invalid output fails identically on all three paths and is never recorded as success", async () => {
		const { fixture, paths } = await setup();
		const results = [await paths.direct({ n: -1 }), await paths.graph({ n: -1 }), await paths.rlm({ n: -1 })];
		const expected = {
			status: "failed",
			error: "doubler@1 output does not match its schema",
			verification: "unverified",
		};
		for (const result of results) expect(result).toEqual(expected);
		const tasks = await journal(fixture);
		expect(tasks.map((task) => task.state)).toEqual(["failed", "failed", "failed"]);
		for (const task of tasks) expect(task.result).toEqual(expected);
	});

	test("invalid input is rejected before admission on all three paths with the same error", async () => {
		const { fixture, paths } = await setup();
		const message = "doubler@1 input does not match its schema";
		await expect(paths.direct({ n: "x" })).rejects.toThrow(message);
		await expect(paths.graph({ n: "x" })).rejects.toThrow(message);
		await expect(paths.rlm({ n: "x" })).rejects.toThrow(`RuntimeError: ${message}`);
		expect(await journal(fixture)).toEqual([]);
		expect(fixture.laneCalls).toEqual([]);
	});
});
