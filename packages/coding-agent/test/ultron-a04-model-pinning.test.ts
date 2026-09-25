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

describe("A04 root pin and explicit child model", () => {
	test("an explicit child model reaches only the child lane; the root lane's model is never changed", async () => {
		const modelSets: Array<{ lane: string; model: unknown }> = [];
		const lane = (name: string) => ({
			getActiveTools: async () => [],
			setModel: async (model: unknown) => {
				modelSets.push({ lane: name, model });
			},
			abort: async () => ({ ok: true }),
			prompt: async () => ({ ok: true, value: { status: "completed", tipId: "tip" } }),
			findEntries: async () => [
				{ id: "tip", type: "message", message: { role: "assistant", content: [{ type: "text", text: "done" }] } },
			],
		});
		const root = lane("main");
		const host = new NativeRlmHost({ lane: async (name: string) => lane(name) } as never, root as never, {
			store: store(),
		});
		const pinned = (await host.handle(
			"agents.spawn",
			{ definition: "rlm-child@1", input: { prompt: "explicit" }, model: "cliproxyapi/gpt-5.6-sol" },
			context,
		)) as { id: string };
		await host.handle("agents.result", { id: pinned.id }, context);
		const inherited = (await host.handle(
			"agents.spawn",
			{ definition: "rlm-child@1", input: { prompt: "inherits" } },
			context,
		)) as { id: string };
		await host.handle("agents.result", { id: inherited.id }, context);

		expect(modelSets).toEqual([
			{ lane: `ultron.rlm-child.${pinned.id}`, model: { provider: "cliproxyapi", modelId: "gpt-5.6-sol" } },
		]);
		// A malformed explicit model is refused rather than silently replaced.
		await expect(
			host.handle("agents.spawn", { definition: "rlm-child@1", input: { prompt: "x" }, model: "no-slash" }, context),
		).rejects.toThrow("provider/model");
		await host.close();
	});
});
