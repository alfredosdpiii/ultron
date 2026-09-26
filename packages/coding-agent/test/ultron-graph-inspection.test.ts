import { afterEach, describe, expect, test } from "vitest";
import { definition, hostFixture } from "./ultron-host-fixtures.ts";

/**
 * Read-only graph fields for the TUI's RLM graph: `agents.status {graph: true}` lists the newest tasks with their
 * lane, admission and end times, spend, a bounded input preview, a result preview instead of the whole value,
 * workflow membership (run, node, dependencies, join), and the Python call that fetches the full result. The plain
 * `agents.status` stays as it was, so the model's own status reads do not grow.
 */

const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
});

type GraphTask = {
	id: string;
	definition: string;
	state: string;
	lane?: string;
	startedAt?: number;
	endedAt?: number;
	input?: string;
	fetch?: string;
	workflow?: { run: string; node: string; dependsOn: string[]; join: string };
	result?: { status: string; preview?: string; error?: string; value?: unknown };
};

describe("agents.status graph view", () => {
	test("adds lane, timing, previews, workflow membership and fetch hints", async () => {
		let clock = 1000;
		const fixture = hostFixture({
			now: () => {
				clock += 10;
				return clock;
			},
			script: () => '{"doubled": 4}',
			deterministic: async ({ input }) => input as never,
		});
		fixtures.push(fixture);
		await fixture.call("agents.register", { definition: definition("double", "rlm") });
		await fixture.call("agents.register", {
			definition: definition("step", "deterministic", { inputSchema: {}, outputSchema: {} }),
		});
		await fixture.call("agents.invoke", { definition: "double@1", input: { n: 2 } });
		await fixture.call("workflows.run", {
			nodes: [
				{ id: "a", definition: "step@1", input: { v: 1 } },
				{ id: "b", definition: "step@1", input: { v: 2 } },
				{ id: "join", definition: "step@1", dependsOn: ["a", "b"], inputFrom: ["a", "b"], join: "any" },
			],
		});

		const plain = await fixture.call<{ tasks: GraphTask[] }>("agents.status");
		expect(Object.keys(plain.tasks[0]!).sort()).toEqual(["definition", "id", "result", "state"]);

		const graph = await fixture.call<{ tasks: GraphTask[] }>("agents.status", { graph: true });
		const invoked = graph.tasks.find((task) => task.definition === "double@1")!;
		expect(invoked.lane).toBe(`ultron.double.${invoked.id}`);
		expect(invoked.startedAt).toBeGreaterThan(1000);
		expect(invoked.endedAt).toBeGreaterThan(invoked.startedAt!);
		expect(invoked.input).toBe('{"n":2}');
		expect(invoked.result).toEqual({ status: "succeeded", preview: '{"doubled":4}' });
		expect(invoked.fetch).toBe(`await agents.result("${invoked.id}")`);

		const members = graph.tasks.filter((task) => task.workflow !== undefined);
		expect(members.map((task) => task.workflow!.node)).toEqual(["a", "b", "join"]);
		const run = members[0]!.workflow!.run;
		expect(run).toMatch(/^wf-[0-9a-f]{8}$/);
		expect(members.every((task) => task.workflow!.run === run)).toBe(true);
		expect(members[2]!.workflow).toEqual({ run, node: "join", dependsOn: ["a", "b"], join: "any" });
		expect(members[0]!.workflow).toEqual({ run, node: "a", dependsOn: [], join: "all" });
	});

	test("bounds previews and rejects unknown fields", async () => {
		const fixture = hostFixture({ deterministic: async ({ input }) => input as never });
		fixtures.push(fixture);
		await fixture.call("agents.register", {
			definition: definition("echo", "deterministic", { inputSchema: {}, outputSchema: {} }),
		});
		await fixture.call("agents.invoke", { definition: "echo@1", input: { text: "x".repeat(5000) } });
		const graph = await fixture.call<{ tasks: GraphTask[] }>("agents.status", { graph: true });
		const task = graph.tasks.at(-1)!;
		expect(task.input!.length).toBeLessThanOrEqual(240);
		expect(task.result!.preview!.length).toBeLessThanOrEqual(240);
		expect(task.result!.value).toBeUndefined();
		await expect(fixture.call("agents.status", { verbose: true })).rejects.toThrow("Unknown payload field: verbose");
		await expect(fixture.call("agents.tasks", { graph: true })).rejects.toThrow("Unknown payload field: graph");
	});
});
