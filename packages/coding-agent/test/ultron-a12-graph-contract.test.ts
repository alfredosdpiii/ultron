import type { JsonValue } from "@earendil-works/chord";
import { afterEach, describe, expect, test } from "vitest";
import { type NativeDefinitionDescriptor, nativeDefinitionHash } from "../src/ultron/rlm/definition-registry.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";
import { deferred, definition, hostFixture, journal, memoryDefinitionStore, waitFor } from "./ultron-host-fixtures.ts";

/**
 * A12: the workflow graph validates its complete topology before any effect; routes, skips,
 * fan-in, and cycles are explicit.
 *
 * Contract under test (`workflows.run`):
 * - Validation (cycles, unknown/duplicate IDs, bad inputs, unknown or unexecutable definitions,
 *   bad bindings, bad routes) throws before any task is admitted.
 * - `inputFrom: "a"` binds one dependency's value; `inputFrom: ["a", "b"]` fans in as `{a, b}`.
 * - `when: {node, field?, equals}` is an explicit conditional route on a dependency's result;
 *   an unmet route yields `{status: "skipped", reason}` and admits no task.
 * - A node whose dependency did not succeed is `skipped` with a reason naming it, transitively.
 * - A bound input that fails the node's schema, or refused admission, is an explicit `failed`
 *   node record; completed sibling results are preserved.
 * - The default join is all-of: a join after exclusive routes skips when either branch skipped.
 *   Any-of joins and bounded revision loops are covered in ultron-workflow-joins-loops.test.ts.
 * Not provided: per-workflow concurrency limits.
 */

const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
});

const anySchema = { inputSchema: {}, outputSchema: {} };

async function graphFixture(options: { usage?: NativeUsageLedger; hold?: Promise<void> } = {}) {
	const adapterCalls: Array<{ definition: string; input: unknown }> = [];
	const fixture = hostFixture({
		usage: options.usage,
		deterministic: async ({ definition: item, input }) => {
			adapterCalls.push({ definition: item.id, input });
			const value = input as Record<string, JsonValue>;
			if (item.id === "doubler") return { doubled: (value.n as number) * 2 };
			if (value.hold && options.hold) await options.hold;
			if (value.fail) throw new Error("step crashed");
			return input;
		},
	});
	fixtures.push(fixture);
	await fixture.call("agents.register", { definition: definition("step", "deterministic", anySchema) });
	await fixture.call("agents.register", { definition: definition("doubler", "deterministic") });
	const run = (nodes: unknown[]) => fixture.call<Record<string, Record<string, unknown>>>("workflows.run", { nodes });
	return { fixture, adapterCalls, run };
}

describe("A12 workflow graph contract", () => {
	test("every validation error is raised before any task is admitted or any node runs", async () => {
		const { fixture, adapterCalls, run } = await graphFixture();
		const valid = { id: "first", definition: "step@1", input: { v: 1 } };
		const cases: Array<[string, unknown[]]> = [
			[
				"cycle",
				[
					valid,
					{ id: "a", definition: "step@1", input: 1, dependsOn: ["b"] },
					{ id: "b", definition: "step@1", input: 2, dependsOn: ["a"] },
				],
			],
			["cycle", [valid, { id: "self", definition: "step@1", input: 1, dependsOn: ["self"] }]],
			["Unknown workflow dependency", [valid, { id: "a", definition: "step@1", input: 1, dependsOn: ["ghost"] }]],
			["Duplicate workflow node ID", [valid, valid]],
			[
				"doubler@1 input does not match its schema",
				[valid, { id: "a", definition: "doubler@1", input: { n: "x" } }],
			],
			["Unknown Ultron agent definition", [valid, { id: "a", definition: "missing@1", input: 1 }]],
			["inputFrom must name a dependency", [valid, { id: "a", definition: "step@1", inputFrom: "first" }]],
			[
				"Specify input or inputFrom, not both",
				[valid, { id: "a", definition: "step@1", input: 1, inputFrom: "first", dependsOn: ["first"] }],
			],
			[
				"when.node must name a dependency",
				[valid, { id: "a", definition: "step@1", input: 1, when: { node: "first", equals: 1 } }],
			],
			[
				"when.equals must be a JSON value",
				[valid, { id: "a", definition: "step@1", input: 1, dependsOn: ["first"], when: { node: "first" } }],
			],
			["Unknown payload field: retries", [valid, { id: "a", definition: "step@1", input: 1, retries: 3 }]],
			["dependsOn must be an array", [valid, { id: "a", definition: "step@1", input: 1, dependsOn: "first" }]],
		];
		for (const [message, nodes] of cases) await expect(run(nodes), message).rejects.toThrow(message);
		await expect(fixture.call("workflows.run", { nodes: "not a list" })).rejects.toThrow("nodes must be an array");
		expect(await journal(fixture)).toEqual([]);
		expect(adapterCalls).toEqual([]);
		expect(fixture.laneCalls).toEqual([]);
	});

	test("a node whose definition cannot execute is rejected before effects", async () => {
		// A stored predict definition is loadable, but this owner has no predict adapter.
		const descriptor = definition("classifier", "predict") as unknown as NativeDefinitionDescriptor;
		const definitionStore = memoryDefinitionStore();
		await definitionStore.write({
			version: 1,
			definitions: [{ ...descriptor, hash: nativeDefinitionHash(descriptor) }],
		} as unknown as JsonValue);
		const fixture = hostFixture({ definitionStore });
		fixtures.push(fixture);
		await expect(
			fixture.call("workflows.run", {
				nodes: [
					{ id: "first", definition: "identity@1", input: 1 },
					{ id: "classify", definition: "classifier@1", input: { n: 1 } },
				],
			}),
		).rejects.toThrow("requires an injected predict adapter");
		expect(await journal(fixture)).toEqual([]);
	});

	test("fan-in receives every dependency's result, keyed by dependency ID", async () => {
		const { adapterCalls, run } = await graphFixture();
		const output = await run([
			{ id: "left", definition: "step@1", input: { side: "left" } },
			{ id: "right", definition: "doubler@1", input: { n: 4 } },
			{ id: "join", definition: "step@1", dependsOn: ["left", "right"], inputFrom: ["left", "right"] },
			{ id: "single", definition: "step@1", dependsOn: ["right"], inputFrom: "right" },
		]);
		expect(output.join).toEqual({
			status: "succeeded",
			value: { left: { side: "left" }, right: { doubled: 8 } },
			verification: "unverified",
		});
		expect(output.single).toMatchObject({ status: "succeeded", value: { doubled: 8 } });
		expect(adapterCalls.find((call) => call.input && (call.input as { left?: unknown }).left)).toBeDefined();
	});

	test("a failed dependency yields explicit, transitive skip records while independent siblings complete", async () => {
		const { fixture, run } = await graphFixture();
		const output = await run([
			{ id: "broken", definition: "step@1", input: { fail: true } },
			{ id: "after", definition: "step@1", input: 1, dependsOn: ["broken"] },
			{ id: "later", definition: "step@1", input: 2, dependsOn: ["after"] },
			{ id: "independent", definition: "step@1", input: { ok: true } },
		]);
		expect(output).toEqual({
			broken: { status: "failed", error: "step crashed", verification: "unverified" },
			after: { status: "skipped", reason: "Dependency broken did not succeed (failed)" },
			later: { status: "skipped", reason: "Dependency after did not succeed (skipped)" },
			independent: { status: "succeeded", value: { ok: true }, verification: "unverified" },
		});
		// Skipped nodes admit no task.
		expect((await journal(fixture)).map((task) => task.state).sort()).toEqual(["completed", "failed"]);
	});

	test("conditional routes are explicit: the matching branch runs, the other is skipped with a reason", async () => {
		const { fixture, run } = await graphFixture();
		const output = await run([
			{ id: "classify", definition: "step@1", input: { route: "fix", detail: 7 } },
			{
				id: "fix",
				definition: "step@1",
				dependsOn: ["classify"],
				inputFrom: "classify",
				when: { node: "classify", field: "route", equals: "fix" },
			},
			{
				id: "ship",
				definition: "step@1",
				input: { shipping: true },
				dependsOn: ["classify"],
				when: { node: "classify", field: "route", equals: "ship" },
			},
			{ id: "announce", definition: "step@1", input: 1, dependsOn: ["ship"] },
			{
				id: "whole",
				definition: "step@1",
				input: 2,
				dependsOn: ["classify"],
				when: { node: "classify", equals: { detail: 7, route: "fix" } },
			},
		]);
		expect(output.fix).toMatchObject({ status: "succeeded", value: { route: "fix", detail: 7 } });
		expect(output.ship).toEqual({ status: "skipped", reason: "Route condition on classify.route not met" });
		expect(output.announce).toEqual({ status: "skipped", reason: "Dependency ship did not succeed (skipped)" });
		// Whole-value routes compare canonical JSON, independent of key order.
		expect(output.whole).toMatchObject({ status: "succeeded" });
		expect(await journal(fixture)).toHaveLength(3);
	});

	test("a bound input that fails the node's schema is an explicit failed node, with nothing spawned for it", async () => {
		const { fixture, run } = await graphFixture();
		const output = await run([
			{ id: "source", definition: "step@1", input: { x: 1 } },
			{ id: "consumer", definition: "doubler@1", dependsOn: ["source"], inputFrom: "source" },
			{ id: "downstream", definition: "step@1", input: 1, dependsOn: ["consumer"] },
			{ id: "sibling", definition: "doubler@1", input: { n: 2 } },
		]);
		expect(output.consumer).toEqual({
			status: "failed",
			error: "Bound input rejected: doubler@1 input does not match its schema",
			verification: "unverified",
		});
		expect(output.downstream).toEqual({ status: "skipped", reason: "Dependency consumer did not succeed (failed)" });
		expect(output.sibling).toMatchObject({ status: "succeeded", value: { doubled: 4 } });
		expect(output.source).toMatchObject({ status: "succeeded" });
		expect((await journal(fixture)).map((task) => task.definition).sort()).toEqual(["doubler@1", "step@1"]);
	});

	test("refused admission is an explicit failed node and does not abort running siblings", async () => {
		const hold = deferred();
		const usage = new NativeUsageLedger(undefined, { limits: { maxAdmittedTasks: 1 } });
		const { fixture, run } = await graphFixture({ usage, hold: hold.promise });
		const pending = run([
			{ id: "slow", definition: "step@1", input: { hold: true } },
			{ id: "crowded", definition: "step@1", input: { crowded: true } },
		]);
		await waitFor(async () => (await journal(fixture)).length === 1);
		hold.resolve();
		const output = await pending;
		expect(output.slow).toMatchObject({ status: "succeeded", value: { hold: true } });
		expect(output.crowded).toEqual({
			status: "failed",
			error: "Admission refused: Usage admitted-task limit exceeded for root ultron-root",
			verification: "unverified",
		});
	});
});
