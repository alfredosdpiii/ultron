import { fileURLToPath } from "node:url";
import { describe, expect, test } from "vitest";
import { createInstanceModule } from "../src/ultron/instances.ts";
import { createMemoryModuleStore } from "../src/ultron/rlm/host-module.ts";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

const context = {} as never;

function memoryStore(): NativeHostStore {
	let value: unknown;
	return {
		read: async () => structuredClone(value) as never,
		write: async (next) => {
			value = structuredClone(next);
		},
	};
}

/** Harness whose lanes remember every prompt, so lane reuse and continuation are observable. */
function fakeHarness() {
	const prompts = new Map<string, string[]>();
	let hold: Promise<void> | undefined;
	const lanes = new Map<string, object>();
	const lane = (name: string) => {
		let existing = lanes.get(name);
		if (!existing) {
			prompts.set(name, []);
			existing = {
				getActiveTools: async () => [],
				setModel: async () => {},
				abort: async () => ({ ok: true }),
				prompt: async (text: string) => {
					prompts.get(name)!.push(text);
					if (hold) await hold;
					return { ok: true, value: { status: "completed", tipId: `tip-${prompts.get(name)!.length}` } };
				},
				findEntries: async () => [
					{
						id: `tip-${prompts.get(name)!.length}`,
						type: "message",
						message: {
							role: "assistant",
							content: [{ type: "text", text: `reply ${prompts.get(name)!.length}` }],
						},
					},
				],
			};
			lanes.set(name, existing);
		}
		return existing;
	};
	return {
		harness: { lane: async (name: string) => lane(name) },
		prompts,
		holdPrompts() {
			let release!: () => void;
			hold = new Promise((resolve) => {
				release = resolve;
			});
			return () => {
				hold = undefined;
				release();
			};
		},
	};
}

function build(
	fake: ReturnType<typeof fakeHarness>,
	stores = { tasks: memoryStore(), instances: createMemoryModuleStore() },
) {
	const resets: string[] = [];
	const host = new NativeRlmHost(fake.harness as never, {} as never, {
		store: stores.tasks,
		modules: [createInstanceModule({ store: stores.instances })],
		beforeLaneReuse: async (lane) => {
			resets.push(lane);
		},
	});
	return { host, resets, stores };
}

describe("A36/A40 retained instances", () => {
	test("continue on the same lane with fresh scratch, and never rewrite the earlier result", async () => {
		const fake = fakeHarness();
		const { host, resets } = build(fake);
		const first = (await host.handle(
			"agents.spawn",
			{ definition: "rlm-child@1", input: { prompt: "one" } },
			context,
		)) as {
			id: string;
		};
		const firstResult = await host.handle("agents.result", { id: first.id }, context);
		expect(firstResult).toMatchObject({ status: "succeeded", value: "reply 1" });

		const instance = (await host.handle("instances.retain", { task_id: first.id }, context)) as {
			id: string;
			lane: string;
		};
		expect(instance.lane).toBe(`ultron.rlm-child.${first.id}`);
		expect(await host.handle("instances.retain", { task_id: first.id }, context)).toMatchObject({ id: instance.id });

		const second = (await host.handle(
			"instances.invoke",
			{ id: instance.id, input: { prompt: "two" } },
			context,
		)) as {
			task_id: string;
		};
		expect(await host.handle("agents.result", { id: second.task_id }, context)).toMatchObject({
			status: "succeeded",
			value: "reply 2",
		});
		// Same lane, continued conversation; scratch reset only for the reuse.
		expect(fake.prompts.get(instance.lane)).toEqual(["one", "two"]);
		expect(resets).toEqual([instance.lane]);
		// The first invocation's durable outcome is untouched.
		expect(await host.handle("agents.result", { id: first.id }, context)).toEqual(firstResult);
		await host.close();
	});

	test("reject concurrent use, closed instances, non-parents, and laneless tasks", async () => {
		const fake = fakeHarness();
		const { host } = build(fake);
		const first = (await host.handle(
			"agents.spawn",
			{ definition: "rlm-child@1", input: { prompt: "one" } },
			context,
		)) as {
			id: string;
		};
		await host.handle("agents.result", { id: first.id }, context);
		const instance = (await host.handle("instances.retain", { task_id: first.id }, context)) as { id: string };

		const release = fake.holdPrompts();
		await host.handle("instances.invoke", { id: instance.id, input: { prompt: "slow" } }, context);
		await expect(
			host.handle("instances.invoke", { id: instance.id, input: { prompt: "again" } }, context),
		).rejects.toThrow("busy");
		release();

		const deterministic = (await host.handle(
			"agents.spawn",
			{ definition: "identity@1", input: { a: 1 } },
			context,
		)) as {
			id: string;
		};
		await host.handle("agents.result", { id: deterministic.id }, context);
		await expect(host.handle("instances.retain", { task_id: deterministic.id }, context)).rejects.toThrow(
			"Only RLM tasks",
		);
		// A task that is not the origin's parent cannot retain it.
		await expect(
			host.handle("instances.retain", { task_id: first.id }, context, { lane: `ultron.rlm-child.${first.id}` }),
		).rejects.toThrow();

		await new Promise((resolve) => setTimeout(resolve, 10));
		await host.handle("instances.close", { id: instance.id }, context);
		await expect(
			host.handle("instances.invoke", { id: instance.id, input: { prompt: "x" } }, context),
		).rejects.toThrow("closed");
		await expect(host.handle("instances.retain", { task_id: first.id, owner: "forged" }, context)).rejects.toThrow(
			"Unknown payload field",
		);
		await host.close();
	});

	test("an instance survives an owner restart and keeps its lane", async () => {
		const fake = fakeHarness();
		const first = build(fake);
		const origin = (await first.host.handle(
			"agents.spawn",
			{ definition: "rlm-child@1", input: { prompt: "one" } },
			context,
		)) as { id: string };
		await first.host.handle("agents.result", { id: origin.id }, context);
		const instance = (await first.host.handle("instances.retain", { task_id: origin.id }, context)) as {
			id: string;
			lane: string;
		};
		await first.host.close();

		const restarted = build(fake, first.stores);
		const next = (await restarted.host.handle(
			"instances.invoke",
			{ id: instance.id, input: { prompt: "after restart" } },
			context,
		)) as { task_id: string };
		expect(await restarted.host.handle("agents.result", { id: next.task_id }, context)).toMatchObject({
			status: "succeeded",
		});
		expect(fake.prompts.get(instance.lane)).toEqual(["one", "after restart"]);
		expect(await restarted.host.handle("instances.get", { id: instance.id }, context)).toMatchObject({
			invocations: [{ task_id: next.task_id }],
		});
		await restarted.host.close();
	});
});

describe("A36 invocation scratch", () => {
	test("reset clears scratch and restores bindings while declared state persists", async () => {
		const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
		const kernel = new RlmKernel({ cwd: process.cwd(), runtimePath }, async () => ({}));
		try {
			await kernel.execute("scratch = 'first call'\nstate['calls'] = state.get('calls', 0) + 1\nagents = None");
			expect(await kernel.resetScratch()).toMatchObject({ status: "ok" });
			await kernel.execute("state['calls'] = state.get('calls', 0) + 1");
			expect(await kernel.execute("('scratch' in globals(), state['calls'], agents is not None)")).toMatchObject({
				status: "ok",
				result: "(False, 2, True)",
			});
		} finally {
			await kernel.shutdown();
		}
	});
});
