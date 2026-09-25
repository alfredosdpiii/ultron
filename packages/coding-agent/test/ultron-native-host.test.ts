import { withAbortSignal } from "@earendil-works/chord/context";
import { describe, expect, test } from "vitest";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

const context = {} as never;

function memoryStore(initial?: unknown): NativeHostStore {
	let value = initial;
	return {
		read: async () => value as never,
		write: async (next) => {
			value = structuredClone(next);
		},
	};
}

type LaneOptions = {
	entries?: unknown[];
	promptResult?: unknown;
	waitForPrompt?: boolean;
};

function fakeLane(options: LaneOptions = {}) {
	let promptCalls = 0;
	let abortCalls = 0;
	const promptResult = options.promptResult ?? { ok: true, value: { status: "completed", tipId: "tip" } };
	return {
		get promptCalls() {
			return promptCalls;
		},
		get abortCalls() {
			return abortCalls;
		},
		findEntries: async () => options.entries ?? [],
		getActiveTools: async () => [],
		setActiveTools: async () => {},
		setModel: async () => {},
		prompt: async () => {
			promptCalls += 1;
			if (options.waitForPrompt) await new Promise<never>(() => {});
			return promptResult;
		},
		abort: async () => {
			abortCalls += 1;
			return { ok: true };
		},
	};
}

function host(lane: ReturnType<typeof fakeLane>, store = memoryStore()) {
	return new NativeRlmHost({ lane: async () => lane } as never, lane as never, { store });
}

describe("Ultron native host", () => {
	test("runs an explicit background job through the same durable task host", async () => {
		const lane = fakeLane({
			entries: [
				{
					id: "tip",
					type: "message",
					message: { role: "assistant", content: [{ type: "text", text: "background answer" }] },
				},
			],
		});
		const instance = host(lane);
		const started = (await instance.handle("background.start", { prompt: "do work" }, context)) as { id: string };
		expect(await instance.handle("background.result", { id: started.id }, context)).toMatchObject({
			status: "succeeded",
			value: "background answer",
			verification: "unverified",
		});
		expect(await instance.handle("background.list", {}, context)).toMatchObject([
			{ id: started.id, definition: "background-job@1", state: "completed" },
		]);
		await instance.close();
	});

	test("runs deterministic agents and preserves durable task records", async () => {
		const store = memoryStore();
		const instance = host(fakeLane(), store);
		const result = await instance.handle(
			"agents.invoke",
			{ definition: "identity@1", input: { answer: 42 } },
			context,
		);
		expect(result).toMatchObject({ status: "succeeded", value: { answer: 42 }, verification: "unverified" });
		const status = (await instance.handle("agents.status", {}, context)) as {
			tasks: Array<{ state: string; result?: unknown }>;
		};
		expect(status.tasks.at(-1)).toMatchObject({ state: "completed", result: { status: "succeeded" } });
		await instance.close();
	});

	test("concurrent idempotent invokes share the same live promise", async () => {
		const lane = fakeLane({
			entries: [
				{
					id: "tip",
					type: "message",
					message: {
						role: "assistant",
						content: [{ type: "text", text: '{"outcome":"no_findings","findings":[]}' }],
					},
				},
			],
		});
		const instance = host(lane);
		const request = { definition: "security-reviewer@1", input: { request: "review" }, key: "same" };
		const [first, second] = await Promise.all([
			instance.handle("agents.invoke", request, context),
			instance.handle("agents.invoke", request, context),
		]);
		expect(first).toEqual(second);
		expect(lane.promptCalls).toBe(1);
		await instance.close();
	});

	test("cancellation commits a terminal result without waiting for a stuck lane", async () => {
		const lane = fakeLane({ waitForPrompt: true });
		const instance = host(lane);
		const spawned = (await instance.handle(
			"agents.spawn",
			{ definition: "security-reviewer@1", input: { request: "review" } },
			context,
		)) as { id: string };
		await new Promise((resolve) => setTimeout(resolve, 0));
		await expect(instance.handle("agents.cancel", { id: spawned.id }, context)).resolves.toEqual({ cancelled: true });
		await expect(instance.handle("agents.result", { id: spawned.id }, context)).resolves.toMatchObject({
			status: "cancelled",
			verification: "unverified",
		});
		expect(await instance.handle("agents.inspect", { id: spawned.id }, context)).toMatchObject({
			id: spawned.id,
			state: "cancelled",
			result: { status: "cancelled" },
		});
		await instance.close();
	});

	test("parent cancellation before admission does not start a task", async () => {
		const controller = new AbortController();
		controller.abort(new Error("parent stopped"));
		const instance = host(fakeLane());
		await expect(
			instance.handle(
				"agents.spawn",
				{ definition: "identity@1", input: 1 },
				withAbortSignal(controller.signal, context),
			),
		).rejects.toThrow("parent stopped");
		expect((await instance.handle("agents.tasks", {}, context)) as { tasks: unknown[] }).toMatchObject({ tasks: [] });
		await instance.close();
	});

	test("timeout has a bounded range and commits cancellation", async () => {
		const instance = host(fakeLane({ waitForPrompt: true }));
		const spawned = (await instance.handle(
			"agents.spawn",
			{ definition: "security-reviewer@1", input: { request: "review" }, timeout_ms: 1 },
			context,
		)) as { id: string };
		await expect(instance.handle("agents.result", { id: spawned.id }, context)).resolves.toMatchObject({
			status: "cancelled",
		});
		await expect(
			instance.handle("agents.spawn", { definition: "identity@1", input: 1, timeout_ms: 3_600_001 }, context),
		).rejects.toThrow("timeout_ms");
		await instance.close();
	});

	test("reopened invoke returns the durable result, not undefined", async () => {
		const store = memoryStore();
		const request = { definition: "identity@1", input: { value: 1 }, key: "reopen" };
		const first = host(fakeLane(), store);
		await first.handle("agents.invoke", request, context);
		const reopened = host(fakeLane(), store);
		expect(await reopened.handle("agents.invoke", request, context)).toMatchObject({
			status: "succeeded",
			value: { value: 1 },
		});
		await first.close();
		await reopened.close();
	});

	test("only a completed prompt at its returned tip can succeed", async () => {
		const historical = {
			id: "historical",
			type: "message",
			message: { role: "assistant", content: [{ type: "text", text: '{"outcome":"no_findings","findings":[]}' }] },
		};
		const missingTip = host(
			fakeLane({
				entries: [historical],
				promptResult: { ok: true, value: { status: "completed", tipId: "current" } },
			}),
		);
		expect(
			await missingTip.handle(
				"agents.invoke",
				{ definition: "security-reviewer@1", input: { request: "review" } },
				context,
			),
		).toMatchObject({
			status: "failed",
		});
		const suspended = host(
			fakeLane({ promptResult: { ok: true, value: { status: "suspended", deferred: { id: "x" } } } }),
		);
		expect(
			await suspended.handle(
				"agents.invoke",
				{ definition: "security-reviewer@1", input: { request: "review" } },
				context,
			),
		).toMatchObject({
			status: "failed",
		});
		await missingTip.close();
		await suspended.close();
	});

	test("validates every workflow node, normalizes missing dependencies, and skips failed dependencies", async () => {
		const instance = host(fakeLane());
		await expect(
			instance.handle(
				"workflows.run",
				{
					nodes: [
						{ id: "first", definition: "identity@1", input: { value: 1 } },
						{ id: "second", definition: "security-reviewer@1", input: {} },
					],
				},
				context,
			),
		).rejects.toThrow("reviewer input.request");
		await expect(
			instance.handle(
				"workflows.run",
				{
					nodes: [
						{ id: "a", definition: "identity@1", input: 1, dependsOn: ["b"] },
						{ id: "b", definition: "identity@1", input: 2, dependsOn: ["a"] },
					],
				},
				context,
			),
		).rejects.toThrow("cycle");
		expect((await instance.handle("agents.tasks", {}, context)) as { tasks: unknown[] }).toMatchObject({ tasks: [] });
		await instance.close();
	});

	test("rejects malformed reviewer output instead of claiming success", async () => {
		const assistant = {
			id: "tip",
			type: "message",
			message: {
				role: "assistant",
				content: [
					{
						type: "text",
						text: '{"outcome":"no_findings","findings":[{"file":"x.ts","line":0,"severity":"low","explanation":"bad"}]}',
					},
				],
			},
		};
		const instance = host(fakeLane({ entries: [assistant] }));
		const result = await instance.handle(
			"agents.invoke",
			{ definition: "security-reviewer@1", input: { request: "review" } },
			context,
		);
		expect(result).toMatchObject({ status: "failed", verification: "unverified" });
		await instance.close();
	});
});
