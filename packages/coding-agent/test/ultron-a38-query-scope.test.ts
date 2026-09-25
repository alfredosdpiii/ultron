import { describe, expect, test } from "vitest";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

const context = {} as never;

function store(): NativeHostStore {
	let value: unknown;
	return {
		read: async () => structuredClone(value) as never,
		write: async (next) => {
			value = structuredClone(next);
		},
	};
}

function waitingLane() {
	return {
		findEntries: async () => [],
		getActiveTools: async () => [],
		setModel: async () => {},
		prompt: () => new Promise<never>(() => {}),
		abort: async () => ({ ok: true }),
	};
}

describe("A38 nested query scope", () => {
	test("a child lane sees its own subtree only; siblings are indistinguishable from missing", async () => {
		const lane = waitingLane();
		const host = new NativeRlmHost({ lane: async () => lane } as never, lane as never, { store: store() });
		const a = (await host.handle("background.start", { prompt: "implementer" }, context)) as { id: string };
		const b = (await host.handle("background.start", { prompt: "reviewer" }, context)) as { id: string };
		await new Promise((resolve) => setTimeout(resolve, 10));
		const asA = { lane: `ultron.background-job.${a.id}` };
		const grandchild = (await host.handle(
			"agents.spawn",
			{ definition: "identity@1", input: { from: "a" } },
			context,
			asA,
		)) as { id: string };

		const seenByA = (await host.handle("agents.status", {}, context, asA)) as { tasks: Array<{ id: string }> };
		expect(seenByA.tasks.map((task) => task.id).sort()).toEqual([a.id, grandchild.id].sort());
		for (const request of ["agents.inspect", "agents.result", "agents.cancel"]) {
			await expect(host.handle(request, { id: b.id }, context, asA)).rejects.toThrow("Unknown Ultron task");
		}
		for (const request of ["background.inspect", "background.result", "background.stop"]) {
			await expect(host.handle(request, { id: b.id }, context, asA)).rejects.toThrow("Unknown background job");
		}
		expect(await host.handle("background.list", {}, context, asA)).toEqual([expect.objectContaining({ id: a.id })]);
		// The sibling keeps running: A could not cancel it.
		expect(await host.handle("agents.inspect", { id: b.id }, context)).toMatchObject({ state: "running" });
		// Its own descendant stays fully usable.
		expect(await host.handle("agents.result", { id: grandchild.id }, context, asA)).toMatchObject({
			status: "succeeded",
		});
		// The root sees everything.
		const seenByRoot = (await host.handle("agents.status", {}, context)) as { tasks: unknown[] };
		expect(seenByRoot.tasks).toHaveLength(3);
		await host.close();
	});
});
