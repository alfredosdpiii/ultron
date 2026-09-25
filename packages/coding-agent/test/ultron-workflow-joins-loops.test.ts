import type { JsonValue } from "@earendil-works/chord";
import { afterEach, describe, expect, test } from "vitest";
import { NativeUsageLedger } from "../src/ultron/usage.ts";
import { definition, hostFixture, journal, memoryDefinitionStore, memoryStore } from "./ultron-host-fixtures.ts";

/**
 * Workflow any-of joins and bounded revision loops (`workflows.run`).
 *
 * Joins: `join: "all"` (default) runs a node only when every dependency succeeded. `join: "any"` waits until every
 * dependency is terminal, runs when at least one succeeded, and is skipped only when none did. Its fan-in input
 * (`inputFrom: [...]`) holds only the dependencies that succeeded; a single-string `inputFrom` or fewer than two
 * dependencies is rejected before effects.
 *
 * Loops: `revise: {from: reviewer, until: {field?, equals}, max_rounds}` on a node re-runs it after each review until
 * the reviewer's result meets `until` (outcome `converged`) or `max_rounds` (1..10) is reached (status and outcome
 * `exhausted`). Round n > 1 receives `{input, previous, review, round}`. Every round is its own task and admission,
 * keyed `<key>:round-<n>`. The revised node's record carries `revision` and every round; the loop is not an edge,
 * so arbitrary cycles stay rejected.
 */

const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
});

const anySchema = { inputSchema: {}, outputSchema: {} };

type Options = {
	usage?: NativeUsageLedger;
	approveAt?: number;
	store?: ReturnType<typeof memoryStore>;
	definitionStore?: ReturnType<typeof memoryDefinitionStore>;
};

async function loopFixture(options: Options = {}) {
	const calls: Array<{ definition: string; input: unknown }> = [];
	const fixture = hostFixture({
		usage: options.usage,
		store: options.store,
		definitionStore: options.definitionStore,
		deterministic: async ({ definition: item, input }) => {
			calls.push({ definition: item.id, input });
			const value = input as Record<string, JsonValue>;
			if (item.id === "implement") {
				// Round 1 drafts version 1; each revision bumps the previous version and cites the review.
				if (value.round === undefined) return { version: 1, task: value.task as string };
				const previous = value.previous as { version: number };
				return { version: previous.version + 1, addressed: value.review };
			}
			if (item.id === "review") {
				const version = (value as { version: number }).version;
				return { verdict: version >= (options.approveAt ?? 2) ? "approved" : "changes", version };
			}
			if (value.fail) throw new Error("step crashed");
			return input;
		},
	});
	fixtures.push(fixture);
	for (const id of ["step", "implement", "review"])
		await fixture.call("agents.register", { definition: definition(id, "deterministic", anySchema) });
	const run = (nodes: unknown[], key?: string) =>
		fixture.call<Record<string, Record<string, unknown>>>("workflows.run", {
			nodes,
			...(key === undefined ? {} : { key }),
		});
	return { fixture, calls, run };
}

const loop = (maxRounds: number, extra: Record<string, unknown> = {}) => [
	{
		id: "implement",
		definition: "implement@1",
		input: { task: "fix bug" },
		revise: { from: "review", until: { field: "verdict", equals: "approved" }, max_rounds: maxRounds },
		...extra,
	},
	{ id: "review", definition: "review@1", dependsOn: ["implement"], inputFrom: "implement" },
	{ id: "ship", definition: "step@1", dependsOn: ["implement"], inputFrom: "implement" },
];

const routes = [
	{ id: "classify", definition: "step@1", input: { route: "fix" } },
	{
		id: "fix",
		definition: "step@1",
		input: { fixed: true },
		dependsOn: ["classify"],
		when: { node: "classify", field: "route", equals: "fix" },
	},
	{
		id: "ship",
		definition: "step@1",
		input: { shipped: true },
		dependsOn: ["classify"],
		when: { node: "classify", field: "route", equals: "ship" },
	},
];

describe("workflow any-of joins", () => {
	test("an any-of join after exclusive routes runs with only the succeeded branch in its fan-in", async () => {
		const { run } = await loopFixture();
		const output = await run([
			...routes,
			{ id: "report", definition: "step@1", dependsOn: ["fix", "ship"], inputFrom: ["fix", "ship"], join: "any" },
		]);
		expect(output.ship).toEqual({ status: "skipped", reason: "Route condition on classify.route not met" });
		expect(output.report).toEqual({
			status: "succeeded",
			value: { fix: { fixed: true } },
			verification: "unverified",
		});
	});

	test("an any-of join is skipped only when no dependency succeeded", async () => {
		const { fixture, run } = await loopFixture();
		const output = await run([
			{ id: "classify", definition: "step@1", input: { route: "neither" } },
			...routes.slice(1),
			{ id: "broken", definition: "step@1", input: { fail: true } },
			{ id: "report", definition: "step@1", input: 1, dependsOn: ["fix", "ship", "broken"], join: "any" },
		]);
		expect(output.report).toEqual({
			status: "skipped",
			reason: "No dependency succeeded (fix: skipped, ship: skipped, broken: failed)",
		});
		expect((await journal(fixture)).map((task) => task.definition).sort()).toEqual(["step@1", "step@1"]);
	});

	test("an all-join after exclusive routes is unchanged: skipped when either branch was skipped", async () => {
		const { run } = await loopFixture();
		const output = await run([
			...routes,
			{ id: "report", definition: "step@1", dependsOn: ["fix", "ship"], inputFrom: ["fix", "ship"] },
			{ id: "explicit", definition: "step@1", input: 1, dependsOn: ["fix", "ship"], join: "all" },
		]);
		expect(output.report).toEqual({ status: "skipped", reason: "Dependency ship did not succeed (skipped)" });
		expect(output.explicit).toEqual(output.report);
	});

	test("nonsensical join combinations are rejected before effects", async () => {
		const { fixture, calls, run } = await loopFixture();
		const first = { id: "a", definition: "step@1", input: 1 };
		const second = { id: "b", definition: "step@1", input: 2 };
		const cases: Array<[string, unknown[]]> = [
			['join must be "all" or "any"', [first, { id: "j", definition: "step@1", input: 1, join: "some" }]],
			[
				'join "any" requires at least two dependencies',
				[first, { id: "j", definition: "step@1", input: 1, dependsOn: ["a"], join: "any" }],
			],
			[
				'join "any" requires inputFrom to be an array',
				[first, second, { id: "j", definition: "step@1", dependsOn: ["a", "b"], inputFrom: "a", join: "any" }],
			],
		];
		for (const [message, nodes] of cases) await expect(run(nodes), message).rejects.toThrow(message);
		expect(await journal(fixture)).toEqual([]);
		expect(calls).toEqual([]);
	});
});

describe("workflow bounded revision loops", () => {
	test("a loop converges in round 2 with the review as the revision input, and downstream sees the final work", async () => {
		const { fixture, calls, run } = await loopFixture({ approveAt: 2 });
		const output = await run(loop(3));
		expect(output.implement).toMatchObject({
			status: "succeeded",
			value: { version: 2, addressed: { verdict: "changes", version: 1 } },
			revision: { outcome: "converged", rounds: 2, max_rounds: 3 },
		});
		const rounds = output.implement.rounds as Array<Record<string, Record<string, unknown>>>;
		expect(rounds.map((round) => [round.work.value, round.review.value])).toEqual([
			[
				{ version: 1, task: "fix bug" },
				{ verdict: "changes", version: 1 },
			],
			[
				{ version: 2, addressed: { verdict: "changes", version: 1 } },
				{ verdict: "approved", version: 2 },
			],
		]);
		expect(output.review).toEqual({
			status: "succeeded",
			value: { verdict: "approved", version: 2 },
			verification: "unverified",
		});
		expect(output.ship).toMatchObject({ status: "succeeded", value: { version: 2 } });
		expect(calls[2]).toEqual({
			definition: "implement",
			input: {
				input: { task: "fix bug" },
				previous: { version: 1, task: "fix bug" },
				review: { verdict: "changes", version: 1 },
				round: 2,
			},
		});
		// Every round is its own task: two implement rounds, two reviews, one ship.
		expect((await journal(fixture)).map((task) => task.definition).sort()).toEqual([
			"implement@1",
			"implement@1",
			"review@1",
			"review@1",
			"step@1",
		]);
	});

	test("a loop that never converges stops at max_rounds with an explicit exhausted outcome", async () => {
		const { fixture, run } = await loopFixture({ approveAt: 99 });
		const output = await run(loop(3));
		expect(output.implement).toMatchObject({
			status: "exhausted",
			value: { version: 3 },
			revision: { outcome: "exhausted", rounds: 3, max_rounds: 3 },
		});
		expect(output.implement.rounds).toHaveLength(3);
		expect(output.review).toMatchObject({ status: "succeeded", value: { verdict: "changes", version: 3 } });
		// Unapproved work does not flow on under an all-join.
		expect(output.ship).toEqual({ status: "skipped", reason: "Dependency implement did not succeed (exhausted)" });
		expect(await journal(fixture)).toHaveLength(6);
	});

	test("each round is admitted against the budget; a refused round fails the loop explicitly", async () => {
		const usage = new NativeUsageLedger();
		const admissions: string[] = [];
		const reserve = usage.reserve.bind(usage);
		// The ledger admits every round separately; this one refuses the second revision.
		usage.reserve = async (request) => {
			admissions.push(request.requestKey ?? "");
			if (request.requestKey === "wf:implement:round-2") throw new Error("Usage budget exhausted");
			return reserve(request);
		};
		const { fixture, run } = await loopFixture({ usage, approveAt: 3 });
		const output = await run(loop(3), "wf");
		expect(admissions).toEqual(["wf:implement:round-1", "wf:review:round-1", "wf:implement:round-2"]);
		expect(output.implement).toMatchObject({
			status: "failed",
			error: "Admission refused: Usage budget exhausted",
			revision: { outcome: "failed", rounds: 2, max_rounds: 3 },
		});
		expect((output.implement.rounds as unknown[])[0]).toMatchObject({
			round: 1,
			work: { status: "succeeded", value: { version: 1 } },
			review: { status: "succeeded", value: { verdict: "changes" } },
		});
		expect(output.review).toEqual({ status: "skipped", reason: "Dependency implement did not succeed (failed)" });
		expect(output.ship).toEqual({ status: "skipped", reason: "Dependency implement did not succeed (failed)" });
		expect(await journal(fixture)).toHaveLength(2);
	});

	test("max_rounds and loop shape are validated before effects; arbitrary cycles stay rejected", async () => {
		const { fixture, calls, run } = await loopFixture();
		const [work, review] = loop(2);
		const revised = (revise: unknown) => [{ ...work, revise }, review];
		const until = { field: "verdict", equals: "approved" };
		const cases: Array<[string, unknown[]]> = [
			["revise.max_rounds must be an integer between 1 and 10", revised({ from: "review", until, max_rounds: 11 })],
			["revise.max_rounds must be an integer between 1 and 10", revised({ from: "review", until, max_rounds: 0 })],
			["revise.max_rounds must be an integer between 1 and 10", revised({ from: "review", until, max_rounds: 1.5 })],
			["revise.max_rounds must be an integer between 1 and 10", revised({ from: "review", until })],
			["revise.until is required", revised({ from: "review", max_rounds: 2 })],
			["revise.until.equals must be a JSON value", revised({ from: "review", until: {}, max_rounds: 2 })],
			["revise.from must name a workflow node", revised({ from: "ghost", until, max_rounds: 2 })],
			[
				"revise.from must depend on the revised node",
				[
					{ ...work, revise: { from: "other", until, max_rounds: 2 } },
					{ id: "other", definition: "step@1", input: 1 },
				],
			],
			["Unknown payload field: extra", revised({ from: "review", until, max_rounds: 2, extra: 1 })],
			[
				"A revision reviewer cannot have a when route",
				[work, { ...review, when: { node: "implement", field: "version", equals: 1 } }],
			],
			[
				"A revision reviewer may depend only on the revised node and its ancestors",
				[
					work,
					{ ...review, dependsOn: ["implement", "later"] },
					{ id: "later", definition: "step@1", dependsOn: ["implement"], inputFrom: "implement" },
				],
			],
			[
				"A revision reviewer cannot itself be revised",
				[
					work,
					{ ...review, revise: { from: "check", until, max_rounds: 2 } },
					{ ...review, id: "check", dependsOn: ["review"], inputFrom: "review" },
				],
			],
			// A back edge is still a cycle, even alongside a declared loop.
			["Workflow contains a cycle", [{ ...work, dependsOn: ["review"] }, review]],
			[
				"Workflow contains a cycle",
				[
					{ id: "a", definition: "step@1", input: 1, dependsOn: ["b"] },
					{ id: "b", definition: "step@1", input: 2, dependsOn: ["a"] },
				],
			],
		];
		for (const [message, nodes] of cases) await expect(run(nodes), message).rejects.toThrow(message);
		expect(await journal(fixture)).toEqual([]);
		expect(calls).toEqual([]);
	});

	test("a keyed loop rerun reuses completed rounds and runs only the new ones", async () => {
		const store = memoryStore();
		const definitionStore = memoryDefinitionStore();
		const first = await loopFixture({ store, definitionStore, approveAt: 3 });
		const exhausted = await first.run(loop(2), "change-1");
		expect(exhausted.implement).toMatchObject({ status: "exhausted", revision: { rounds: 2 } });
		expect((await journal(first.fixture)).map((task) => task.key).sort()).toEqual([
			"change-1:implement:round-1",
			"change-1:implement:round-2",
			"change-1:review:round-1",
			"change-1:review:round-2",
		]);
		await first.fixture.host.close();

		// A reopened owner reruns the same keyed workflow with a larger bound.
		const second = await loopFixture({ store, definitionStore, approveAt: 3 });
		const output = await second.run(loop(3), "change-1");
		expect(output.implement).toMatchObject({
			status: "succeeded",
			value: { version: 3 },
			revision: { outcome: "converged", rounds: 3, max_rounds: 3 },
		});
		expect(second.calls.map((call) => call.definition)).toEqual(["implement", "review", "step"]);
		expect((await journal(second.fixture)).map((task) => task.key).sort()).toEqual([
			"change-1:implement:round-1",
			"change-1:implement:round-2",
			"change-1:implement:round-3",
			"change-1:review:round-1",
			"change-1:review:round-2",
			"change-1:review:round-3",
			"change-1:ship",
		]);
		// Rerunning the converged workflow admits nothing new.
		await second.run(loop(3), "change-1");
		expect(second.calls).toHaveLength(3);
	});
});
