import { describe, expect, test } from "vitest";
import type { NativeHostModule } from "../src/ultron/rlm/host-module.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

const context = {} as never;

function memoryStore(): NativeHostStore {
	let value: unknown;
	return {
		read: async () => value as never,
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
		steer: async () => ({ ok: true, value: {} }),
	};
}

describe("native host modules and caller identity", () => {
	test("dispatches by prefix and passes the calling task from the lane, not the payload", async () => {
		const seen: Array<{ type: string; callerTaskId: string | null }> = [];
		const module: NativeHostModule = {
			prefixes: ["probe."],
			async handle(request, host) {
				seen.push({ type: request.type, callerTaskId: host.callerTaskId(request.caller) });
				return { ok: true };
			},
		};
		const lane = waitingLane();
		const instance = new NativeRlmHost({ lane: async () => lane } as never, lane as never, {
			store: memoryStore(),
			modules: [module],
		});
		const started = (await instance.handle("background.start", { prompt: "wait" }, context)) as { id: string };
		// Let the task reach its lane.
		await new Promise((resolve) => setTimeout(resolve, 10));
		const childLane = { lane: `ultron.background-job.${started.id}` };

		await instance.handle("probe.check", { sender: "forged" }, context);
		await instance.handle("probe.check", {}, context, childLane);
		expect(seen).toEqual([
			{ type: "probe.check", callerTaskId: null },
			{ type: "probe.check", callerTaskId: started.id },
		]);

		const child = (await instance.handle(
			"agents.spawn",
			{ definition: "identity@1", input: { answer: 1 } },
			context,
			childLane,
		)) as { id: string };
		const tasks = (await instance.handle("agents.tasks", {}, context)) as {
			tasks: Array<{ id: string; parentId?: string }>;
		};
		expect(tasks.tasks.find((task) => task.id === child.id)?.parentId).toBe(started.id);
		expect(tasks.tasks.find((task) => task.id === started.id)?.parentId).toBeUndefined();
		await instance.close();
	});
});
