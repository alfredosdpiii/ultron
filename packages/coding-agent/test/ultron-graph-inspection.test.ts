import { afterEach, describe, expect, test } from "vitest";
import { deferred, definition, hostFixture, waitFor } from "./ultron-host-fixtures.ts";

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
	model?: string;
	turns?: number;
	toolCallCount?: number;
	lastText?: string;
};

type GraphWorkflow = {
	run: string;
	parentId?: string;
	startedAt: number;
	endedAt?: number;
	nodes: { id: string; definition: string; dependsOn: string[]; join: string; status?: string; reason?: string }[];
	truncatedNodes?: number;
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

	test("a task lane's model, turns, tool calls and latest assistant text, live and bounded", async () => {
		const gate = deferred();
		const fixture = hostFixture({
			script: async () => {
				await gate.promise;
				return '{"doubled": 4}';
			},
		});
		fixtures.push(fixture);
		await fixture.call("agents.register", { definition: definition("double", "rlm") });
		const { id } = await fixture.call<{ id: string }>("agents.spawn", { definition: "double@1", input: { n: 2 } });
		const lane = `ultron.double.${id}`;
		await waitFor(() => fixture.prompts.some((prompt) => prompt.lane === lane));
		// Mid-run: one assistant message with a long text and two tool calls, from the lane's harness events.
		fixture.emit("message_end", {
			lane,
			message: {
				role: "assistant",
				provider: "cliproxyapi",
				model: "glm-5.3-flash",
				content: [
					{ type: "thinking", thinking: "hidden" },
					{ type: "text", text: `I read this as fact collection. ${"x".repeat(400)}` },
					{ type: "toolCall", id: "c1", name: "rlm", arguments: {} },
				],
			},
		});
		fixture.emit("tool_start", { lane, toolCallId: "c1", toolName: "rlm" });
		fixture.emit("tool_start", { lane, toolCallId: "c2", toolName: "rlm" });
		// Another lane's events and user messages do not count.
		fixture.emit("tool_start", { lane: "main", toolCallId: "c3", toolName: "rlm" });
		fixture.emit("message_end", { lane, message: { role: "user", content: "next" } });

		const running = (await fixture.call<{ tasks: GraphTask[] }>("agents.status", { graph: true })).tasks.find(
			(task) => task.id === id,
		)!;
		expect(running.state).toBe("running");
		expect(running.model).toBe("cliproxyapi/glm-5.3-flash");
		expect(running.turns).toBe(1);
		expect(running.toolCallCount).toBe(2);
		expect(running.lastText!.startsWith("I read this as fact collection. xxx")).toBe(true);
		expect(running.lastText!.length).toBe(160);
		expect(running.lastText!.endsWith("…")).toBe(true);

		gate.resolve();
		await waitFor(async () => {
			const status = await fixture.call<{ tasks: GraphTask[] }>("agents.status", { graph: true });
			return status.tasks.find((task) => task.id === id)?.state === "completed";
		});
		const done = (await fixture.call<{ tasks: GraphTask[] }>("agents.status", { graph: true })).tasks.find(
			(task) => task.id === id,
		)!;
		// After the run, its own entries are authoritative: the final text, and counts never go down.
		expect(done.lastText).toBe('{"doubled": 4}');
		expect(done.turns).toBe(1);
		expect(done.toolCallCount).toBe(2);
		// Events after the task ended do not change it.
		fixture.emit("tool_start", { lane, toolCallId: "c9", toolName: "rlm" });
		const after = (await fixture.call<{ tasks: GraphTask[] }>("agents.status", { graph: true })).tasks.find(
			(task) => task.id === id,
		)!;
		expect(after.toolCallCount).toBe(2);
		// The plain listing stays as it was.
		const plain = await fixture.call<{ tasks: GraphTask[] }>("agents.status");
		expect(Object.keys(plain.tasks.find((task) => task.id === id)!).sort()).toEqual([
			"definition",
			"id",
			"result",
			"state",
		]);
	});

	test("workflow plans list every node, including skipped ones and those not admitted yet, bounded", async () => {
		const fixture = hostFixture({ deterministic: async ({ input }) => input as never });
		fixtures.push(fixture);
		await fixture.call("agents.register", {
			definition: definition("step", "deterministic", { inputSchema: {}, outputSchema: {} }),
		});
		await fixture.call("workflows.run", {
			nodes: [
				{ id: "a", definition: "step@1", input: { go: false } },
				{ id: "b", definition: "step@1", input: { v: 2 } },
				{
					id: "gated",
					definition: "step@1",
					dependsOn: ["a"],
					inputFrom: "a",
					when: { node: "a", field: "go", equals: true },
				},
				{ id: "join", definition: "step@1", dependsOn: ["a", "b"], inputFrom: ["a", "b"] },
			],
		});
		const status = await fixture.call<{ workflows: GraphWorkflow[] }>("agents.status", { graph: true });
		expect(status.workflows).toHaveLength(1);
		const [run] = status.workflows;
		expect(run!.run).toMatch(/^wf-[0-9a-f]{8}$/);
		expect(run!.endedAt).toBeGreaterThanOrEqual(run!.startedAt);
		expect(run!.nodes.map((node) => [node.id, node.dependsOn, node.status])).toEqual([
			["a", [], "succeeded"],
			["b", [], "succeeded"],
			["gated", ["a"], "skipped"],
			["join", ["a", "b"], "succeeded"],
		]);
		expect(run!.nodes[2]!.reason).toMatch(/Route condition on a/);
		expect(run!.nodes[0]!.definition).toBe("step@1");
		expect("workflows" in (await fixture.call<Record<string, unknown>>("agents.status"))).toBe(false);

		// Bounds: the newest 20 runs, 64 nodes per run.
		const wide = Array.from({ length: 70 }, (_, index) => ({ id: `n${index}`, definition: "step@1", input: {} }));
		await fixture.call("workflows.run", { nodes: wide });
		for (let index = 0; index < 20; index++)
			await fixture.call("workflows.run", { nodes: [{ id: "x", definition: "step@1", input: {} }] });
		const bounded = await fixture.call<{ workflows: GraphWorkflow[] }>("agents.status", { graph: true });
		expect(bounded.workflows).toHaveLength(20);
		expect(bounded.workflows.every((workflow) => workflow.nodes.length === 1)).toBe(true);
	});

	test("the node limit keeps a wide workflow's plan bounded", async () => {
		const fixture = hostFixture({ deterministic: async ({ input }) => input as never });
		fixtures.push(fixture);
		await fixture.call("agents.register", {
			definition: definition("step", "deterministic", { inputSchema: {}, outputSchema: {} }),
		});
		const wide = Array.from({ length: 70 }, (_, index) => ({ id: `n${index}`, definition: "step@1", input: {} }));
		await fixture.call("workflows.run", { nodes: wide });
		const status = await fixture.call<{ workflows: GraphWorkflow[] }>("agents.status", { graph: true });
		expect(status.workflows[0]!.nodes).toHaveLength(64);
		expect(status.workflows[0]!.truncatedNodes).toBe(6);
	});
});
