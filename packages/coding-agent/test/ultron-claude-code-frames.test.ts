/**
 * Inference frames on the claude-code provider (the Claude Code CLI): the frame's contract reaches the CLI as its
 * payload's `json_schema`, ULTRON_RLM_FRAME_MODEL picks the model of code-free frames, and an exhausted
 * subscription window makes a frame Incomplete (`usage_limit`) instead of a failure.
 */
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Context } from "@ultron/agent-core";
import { afterEach, describe, expect, test } from "vitest";
import {
	createInferenceRuntime,
	createMemoryFrameStore,
	defaultFrameModel,
	FRAME_MODEL_ENV,
	withContractSchema,
} from "../src/ultron/rlm/inference.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";
import { memoryDefinitionStore, memoryStore } from "./ultron-host-fixtures.ts";

const MODEL = {
	provider: "claude-code",
	id: "haiku",
	api: "claude-code-cli",
	contextWindow: 200_000,
	maxTokens: 32_000,
};

type Reply = { text: string } | { usageLimit: true };
type Request = { lane: string; model: string | undefined; payload: Record<string, unknown> };

/** Lanes that run the harness hooks for one request per prompt, like the agent harness does. */
function lanes(reply: (message: string) => Reply) {
	const handlers = new Map<string, Array<(event: unknown, context: Context) => unknown>>();
	const requests: Request[] = [];
	const run = async (name: string, event: Record<string, unknown>) => {
		let result: unknown;
		for (const handler of handlers.get(name) ?? []) result = (await handler(event, {} as Context)) ?? result;
		return result as Record<string, unknown> | undefined;
	};
	const harness = {
		hooks: {
			on(name: string, handler: (event: unknown, context: Context) => unknown) {
				const list = handlers.get(name) ?? [];
				list.push(handler);
				handlers.set(name, list);
				return () => list.splice(list.indexOf(handler), 1);
			},
		},
		lane: async (name: string) => {
			const entries: Array<{ id: string; type: "message"; message: Record<string, unknown> }> = [];
			let model: string | undefined;
			return {
				getActiveTools: async () => [],
				setActiveTools: async () => {},
				setModel: async (selected: { provider: string; modelId: string }) => {
					model = `${selected.provider}/${selected.modelId}`;
				},
				getModel: async () => MODEL,
				steer: async () => ({ ok: true, value: {} }),
				abort: async () => ({ ok: true }),
				prompt: async (message: string) => {
					const fromTipId = entries.at(-1)?.id ?? null;
					await run("transform_context", { lane: name, messages: [], systemPrompt: "ROOT" });
					let payload: Record<string, unknown> = {
						model: MODEL.id,
						max_tokens: MODEL.maxTokens,
						content: message,
					};
					payload =
						((await run("before_payload", { lane: name, model: MODEL, payload }))?.payload as Record<
							string,
							unknown
						>) ?? payload;
					requests.push({ lane: name, model, payload });
					const answer = reply(message);
					const id = `${name}#${entries.length + 1}`;
					if ("usageLimit" in answer) {
						const failed = {
							role: "assistant",
							content: [],
							stopReason: "error",
							errorMessage:
								"Claude Code usage limit reached (five_hour window; resets at 2026-10-01T00:00:00.000Z)",
							diagnostics: [{ type: "provider_usage_limit", timestamp: 0 }],
						};
						await run("after_response", { lane: name, message: failed });
						entries.push({ id, type: "message", message: failed });
						return { ok: true, value: { status: "failed", tipId: id, fromTipId, error: { message: "limit" } } };
					}
					const usage = {
						input: 400,
						output: 5,
						cacheRead: 0,
						cacheWrite: 0,
						totalTokens: 405,
						cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0.0004 },
					};
					const assistant = {
						role: "assistant",
						content: [{ type: "text", text: answer.text }],
						usage,
						stopReason: "stop",
					};
					await run("after_response", { lane: name, message: assistant });
					entries.push({ id, type: "message", message: assistant });
					return { ok: true, value: { status: "completed", tipId: id, fromTipId } };
				},
				findEntries: async (query?: { stopAtId?: string }) => {
					const start =
						query?.stopAtId === undefined ? 0 : entries.findIndex((entry) => entry.id === query.stopAtId) + 1;
					return entries.slice(start).reverse();
				},
			};
		},
	};
	return { harness, requests };
}

const dirs: string[] = [];
afterEach(() => {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
	delete process.env[FRAME_MODEL_ENV];
});

function setup(reply: (message: string) => Reply) {
	const fake = lanes(reply);
	const contextDir = mkdtempSync(join(tmpdir(), "ultron-cc-frames-"));
	dirs.push(contextDir);
	const inference = createInferenceRuntime({
		contextDir,
		traces: createMemoryFrameStore(),
		usage: new NativeUsageLedger(),
	});
	inference.install(fake.harness as never);
	const host = new NativeRlmHost(fake.harness as never, {} as never, {
		store: memoryStore(),
		definitionStore: memoryDefinitionStore(),
		usage: new NativeUsageLedger(),
		frames: inference.executor,
		modules: [inference.module],
	});
	const call = <T>(type: string, payload: Record<string, unknown>) =>
		host.handle(type, payload, {} as Context, undefined) as Promise<T>;
	return { ...fake, call };
}

type Observation = { status: string; value?: unknown; reason?: string; detail?: string };

describe("inference frames on the Claude Code CLI provider", () => {
	test("the frame contract reaches the CLI payload as json_schema; other providers' payloads are untouched", async () => {
		const contract = { type: "object", properties: { n: { type: "integer" } }, required: ["n"] };
		const { call, requests } = setup(() => ({ text: '{"n":8}' }));
		const reply = await call<Observation>("rlm.infer", {
			task: "Count.",
			context: [],
			contract,
			model: "claude-code/haiku",
		});
		expect(reply).toMatchObject({ status: "complete", value: { n: 8 } });
		expect(requests[0].payload.json_schema).toEqual(contract);
		expect(requests[0].model).toBe("claude-code/haiku");
		expect(withContractSchema({ max_tokens: 10 }, contract, "openai-completions")).toEqual({ max_tokens: 10 });
		expect(withContractSchema({ max_tokens: 10 }, undefined, "claude-code-cli")).toEqual({ max_tokens: 10 });
	});

	test("ULTRON_RLM_FRAME_MODEL is the model of code-free frames that name none", async () => {
		expect(defaultFrameModel({ [FRAME_MODEL_ENV]: " claude-code/haiku " })).toBe("claude-code/haiku");
		expect(defaultFrameModel({ [FRAME_MODEL_ENV]: "haiku" })).toBeUndefined();
		process.env[FRAME_MODEL_ENV] = "claude-code/haiku";
		const { call, requests } = setup(() => ({ text: "chat" }));
		await call("rlm.map", { frames: [{ task: "cat in French", context: [] }] });
		expect(requests[0].model).toBe("claude-code/haiku");
	});

	test("an exhausted usage window makes the frame Incomplete (usage_limit), not a silent failure", async () => {
		const { call } = setup((message) => (message.includes("limited") ? { usageLimit: true } : { text: "3" }));
		const reply = await call<{ results: Observation[] }>("rlm.map", {
			frames: [
				{ task: "This one is limited.", context: [] },
				{ task: "This one works.", context: [] },
			],
			contract: { type: "integer" },
		});
		expect(reply.results[0]).toMatchObject({
			status: "incomplete",
			reason: "usage_limit",
			detail: expect.stringMatching(/^Claude Code usage limit reached/),
		});
		expect(reply.results[1]).toMatchObject({ status: "complete", value: 3 });
	});
});
