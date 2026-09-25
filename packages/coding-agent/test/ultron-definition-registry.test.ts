import { describe, expect, test } from "vitest";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";

const context = {} as never;

type Store = NativeHostStore & { value: () => unknown };

function store(initial?: unknown): Store {
	let saved = initial;
	return {
		read: async () => saved as never,
		write: async (next) => {
			saved = structuredClone(next);
		},
		value: () => saved,
	};
}

function host(taskStore: NativeHostStore, definitionStore?: NativeHostStore, options: Record<string, unknown> = {}) {
	return new NativeRlmHost(
		{ lane: async () => ({}) } as never,
		{} as never,
		{ store: taskStore, definitionStore, ...options } as never,
	);
}

function definition(overrides: Record<string, unknown> = {}) {
	return {
		id: "echo",
		version: "1",
		strategy: "rlm",
		instructions: "Return the input as JSON.",
		inputSchema: { type: "object", properties: { value: { type: "string" } }, required: ["value"] },
		outputSchema: { type: "object", properties: { value: { type: "string" } }, required: ["value"] },
		maxRepairs: 0,
		inputDescription: "{value:string}",
		outputDescription: "{value:string}",
		...overrides,
	};
}

describe("NativeRlmHost runtime definition registration", () => {
	test("registers a strict public definition while retaining built-ins", async () => {
		const taskStore = store();
		const definitionStore = store();
		const instance = host(taskStore, definitionStore);
		const registered = await instance.handle("agents.register", { definition: definition() }, context);
		const listed = (await instance.handle("agents.list", {}, context)) as Array<Record<string, unknown>>;

		expect(registered).toEqual(definition());
		expect(listed).toContainEqual(definition());
		expect(listed).toContainEqual(
			expect.objectContaining({ id: "identity", version: "1", strategy: "deterministic" }),
		);
		expect((definitionStore.value() as { definitions: unknown[] }).definitions).toContainEqual(
			expect.objectContaining({ id: "echo", version: "1", hash: expect.any(String) }),
		);
		await instance.close();
	});

	test("rejects malformed schemas and executable payload values", async () => {
		const instance = host(store(), store());
		await expect(
			instance.handle(
				"agents.register",
				{ definition: definition({ inputSchema: { type: "not-a-json-type" } }) },
				context,
			),
		).rejects.toThrow("Invalid JSON schema");
		await expect(
			instance.handle(
				"agents.register",
				{ definition: definition({ outputSchema: { type: "object", properties: { value: () => "no" } } }) },
				context,
			),
		).rejects.toThrow("Host payload must be a JSON object");
		await instance.close();
	});

	test("rejects hash conflicts for immutable id@version entries", async () => {
		const instance = host(store(), store());
		await instance.handle("agents.register", { definition: definition() }, context);
		await expect(
			instance.handle("agents.register", { definition: definition({ instructions: "changed" }) }, context),
		).rejects.toThrow("Definition hash conflict for echo@1");
		await instance.close();
	});

	test("persists registrations and restores them in a new host", async () => {
		const taskStore = store();
		const definitionStore = store();
		const first = host(taskStore, definitionStore);
		await first.handle("agents.register", { definition: definition() }, context);
		await first.close();

		const reopened = host(store(), definitionStore);
		const listed = (await reopened.handle("agents.list", {}, context)) as Array<Record<string, unknown>>;
		expect(listed).toContainEqual(definition());
		await reopened.close();
	});

	test("requires adapters for predict and non-identity deterministic definitions", async () => {
		const predict = definition({ id: "predicted", strategy: "predict" });
		const deterministic = definition({ id: "calculated", strategy: "deterministic" });
		const instance = host(store(), store());
		await expect(instance.handle("agents.register", { definition: predict }, context)).rejects.toThrow(
			"requires an injected predict adapter",
		);
		await expect(instance.handle("agents.register", { definition: deterministic }, context)).rejects.toThrow(
			"requires an injected deterministic adapter",
		);
		await instance.close();
	});
});
