import { describe, expect, test } from "vitest";
import { createPredictAdapter } from "../src/ultron/predict-adapter.ts";
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

const classify = {
	id: "classify-severity",
	version: "1",
	strategy: "predict",
	instructions: "Classify the severity of the reported defect.",
	inputSchema: { type: "object", required: ["report"], properties: { report: { type: "string" } } },
	outputSchema: {
		type: "object",
		required: ["severity"],
		additionalProperties: false,
		properties: { severity: { enum: ["low", "high"] } },
	},
	maxRepairs: 1,
	inputDescription: "A defect report",
	outputDescription: "{severity: low|high}",
};

function scriptedModels(replies: string[]) {
	const requests: string[] = [];
	return {
		requests,
		models: {
			completeSimple: async (_model: unknown, request: { messages: Array<{ content: string }> }) => {
				requests.push(request.messages[0]!.content);
				return {
					role: "assistant",
					stopReason: "stop",
					content: [{ type: "text", text: replies.shift() ?? "{}" }],
				};
			},
		},
	};
}

function host(models: ReturnType<typeof scriptedModels>["models"]) {
	const lanes: string[] = [];
	return {
		lanes,
		host: new NativeRlmHost(
			{
				lane: async (name: string) => {
					lanes.push(name);
					throw new Error("predict must not open a lane");
				},
			} as never,
			{} as never,
			{
				store: store(),
				definitionStore: store(),
				predict: createPredictAdapter({ models: models as never, model: () => ({ id: "m" }) as never }),
			},
		),
	};
}

describe("predict adapter", () => {
	test("one call per attempt, repair with the validation error, no lane or kernel", async () => {
		const scripted = scriptedModels(['{"severity": "urgent"}', '```json\n{"severity": "high"}\n```']);
		const { host: instance, lanes } = host(scripted.models);
		await instance.handle("agents.register", { definition: classify }, context);
		const result = await instance.handle(
			"agents.invoke",
			{ definition: "classify-severity@1", input: { report: "data loss on save" } },
			context,
		);
		expect(result).toEqual({ status: "succeeded", value: { severity: "high" }, verification: "unverified" });
		expect(scripted.requests).toHaveLength(2);
		expect(scripted.requests[0]).toContain("data loss on save");
		expect(scripted.requests[1]).toContain("rejected (attempt 1)");
		expect(lanes).toEqual([]);
		await instance.close();
	});

	test("exhausted repairs fail typed instead of claiming success", async () => {
		const scripted = scriptedModels(["not json", '{"severity": "medium"}', '{"severity": "high"}']);
		const { host: instance } = host(scripted.models);
		await instance.handle("agents.register", { definition: classify }, context);
		const result = await instance.handle(
			"agents.invoke",
			{ definition: "classify-severity@1", input: { report: "typo" } },
			context,
		);
		expect(result).toMatchObject({ status: "failed", verification: "unverified" });
		// maxRepairs 1 bounds the attempts to two.
		expect(scripted.requests).toHaveLength(2);
		await instance.close();
	});
});
