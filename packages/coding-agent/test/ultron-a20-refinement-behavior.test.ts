import type { Context, JsonValue } from "@earendil-works/chord";
import { describe, expect, test } from "vitest";
import { type DurableDocumentStorage, NativeLocalServices } from "../src/ultron/local-services.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

const context = {} as Context;

function documents(): DurableDocumentStorage {
	const values = new Map<string, JsonValue>();
	return {
		get: async (key) => structuredClone(values.get(key)),
		set: async (key, value) => {
			values.set(key, structuredClone(value));
		},
		list: async (prefix) =>
			[...values].filter(([key]) => key.startsWith(prefix)).map(([key, value]) => ({ key, value })),
	};
}

function taskStore(): NativeHostStore {
	let value: unknown;
	return {
		read: async () => structuredClone(value) as never,
		write: async (next) => {
			value = structuredClone(next);
		},
	};
}

describe("A20 versioned refinement changes later behavior and rolls back", () => {
	test("activation reaches later model runs, a new version supersedes it, and rollback removes it", async () => {
		const prompts: string[] = [];
		const lane = {
			getActiveTools: async () => [],
			setModel: async () => {},
			abort: async () => ({ ok: true }),
			prompt: async (text: string) => {
				prompts.push(text);
				return { ok: true, value: { status: "completed", tipId: "tip" } };
			},
			findEntries: async () => [
				{ id: "tip", type: "message", message: { role: "assistant", content: [{ type: "text", text: "done" }] } },
			],
		};
		const services = new NativeLocalServices(documents());
		const host = new NativeRlmHost({ lane: async () => lane } as never, lane as never, {
			store: taskStore(),
			services,
			refinements: async (definitionId, ctx) => {
				const current = (await services.handle(
					"refinements.current",
					{ kind: "instruction", target: `instruction:${definitionId}` },
					ctx,
				)) as { id: string; version: number | null; content: JsonValue } | null;
				return current ? [{ id: current.id, version: current.version, text: String(current.content) }] : [];
			},
		});
		const run = async () => {
			const task = (await host.handle(
				"agents.spawn",
				{ definition: "rlm-child@1", input: { prompt: "task" } },
				context,
			)) as {
				id: string;
			};
			const result = await host.handle("agents.result", { id: task.id }, context);
			expect(result).toMatchObject({ status: "succeeded" });
			return prompts.at(-1)!;
		};
		const propose = async (content: string, baseVersion: number) =>
			(
				(await host.handle(
					"refinements.propose",
					{
						kind: "instruction",
						target: "instruction:rlm-child",
						baseVersion,
						content,
						evidence: [{ failure: "missed edge case" }],
					},
					context,
				)) as { id: string }
			).id;

		expect(await run()).toBe("task");

		const first = await propose("Always check empty inputs.", 0);
		expect(await run()).not.toContain("empty inputs"); // proposed only, not active
		await host.handle("refinements.activate", { id: first }, context);
		expect(await run()).toContain("Always check empty inputs.");

		const second = await propose("Always check empty and huge inputs.", 1);
		await host.handle("refinements.activate", { id: second }, context);
		const afterSecond = await run();
		expect(afterSecond).toContain("empty and huge inputs");
		expect(afterSecond).not.toContain("Always check empty inputs.");

		await host.handle("refinements.rollback", { id: second }, context);
		const afterRollback = await run();
		expect(afterRollback).not.toContain("huge inputs");
		await host.close();
	});
});
