/**
 * Bounded inference (Phase 2): `rlm.load`, `rlm.infer`, `rlm.map` on the real host, with lanes that run the
 * harness's request hooks the way the agent harness does (transform_context, before_payload, after_response).
 * Frames are `rlm-frame@1` tasks: journal, admission, cancellation cascade and the usage ledger all apply.
 */
import { createHash } from "node:crypto";
import { mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import type { Context } from "@ultron/agent-core";
import { visibleWidth } from "@ultron/tui";
import { afterEach, describe, expect, test } from "vitest";
import { renderRlmDock } from "../src/experimental/rlm-graph.ts";
import { parseFrames } from "../src/experimental/rlm-visualizer.ts";
import {
	createInferenceRuntime,
	createMemoryFrameStore,
	DEFAULT_MAP_TOKENS,
	defaultMapTokens,
	INFERENCE_PROMPT,
} from "../src/ultron/rlm/inference.ts";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { DEFAULT_SPAWN_DEPTH, NativeRlmHost, spawnDepthLimit } from "../src/ultron/rlm/native-host.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";
import { deferred, memoryDefinitionStore, memoryStore, waitFor } from "./ultron-host-fixtures.ts";

type Reply = {
	text?: string;
	/** Provider-reported usage; defaults to input = payload chars / 4, output = 8. */
	usage?: { input?: number; output?: number; cacheRead?: number };
	/** The provider fails this request (an error response with no usage). */
	fail?: boolean;
	/** Resolves the reply later (the request stays in flight until then). */
	wait?: Promise<void>;
	/** After this response the harness makes a follow-up request in the same prompt (a tool round). */
	followUp?: { text: string };
};
type Call = { lane: string; message: string; system: string; payload: Record<string, unknown>; attempt: number };
type Script = (call: Call) => Reply;

const MODEL = {
	provider: "fake",
	id: "frame-model",
	api: "openai-completions",
	contextWindow: 32_000,
	maxTokens: 8_192,
};

/** Lanes that behave like harness lanes for one request per prompt, running the registered hooks. */
function hookedHarness(script: Script) {
	const handlers = new Map<string, Array<(event: unknown, context: Context) => unknown>>();
	const calls: Call[] = [];
	const aborts: string[] = [];
	const activeTools = new Map<string, string[]>();
	const lanes = new Map<string, object>();
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
			let lane = lanes.get(name);
			if (lane) return lane;
			const entries: Array<{ id: string; type: "message"; message: Record<string, unknown> }> = [];
			const conversation: Array<{ role: string; content: string }> = [];
			let abortLane = () => {};
			let counter = 0;
			lane = {
				getActiveTools: async () => activeTools.get(name) ?? ["read", "bash", "rlm"],
				setActiveTools: async (names: string[]) => {
					activeTools.set(name, names);
				},
				setModel: async () => {},
				getModel: async () => MODEL,
				steer: async () => ({ ok: true, value: {} }),
				abort: async () => {
					aborts.push(name);
					abortLane();
					return { ok: true };
				},
				prompt: async (message: string, _images: unknown, laneContext: Context) => {
					const fromTipId = entries.at(-1)?.id ?? null;
					conversation.push({ role: "user", content: message });
					const transformed = await run("transform_context", {
						lane: name,
						messages: [],
						systemPrompt: "ROOT SYSTEM PROMPT",
					});
					const system = (transformed?.systemPrompt as string | undefined) ?? "ROOT SYSTEM PROMPT";
					let payload: Record<string, unknown> = {
						model: MODEL.id,
						max_completion_tokens: MODEL.maxTokens,
						messages: [{ role: "system", content: system }, ...conversation],
					};
					payload =
						((await run("before_payload", { lane: name, model: MODEL, payload }))?.payload as Record<
							string,
							unknown
						>) ?? payload;
					const call: Call = { lane: name, message, system, payload, attempt: counter };
					calls.push(call);
					const reply = script(call);
					const stopped = new Promise<"aborted">((resolve) => {
						abortLane = () => resolve("aborted");
						laneContext?.abortSignal?.addEventListener("abort", () => resolve("aborted"), { once: true });
					});
					if (
						reply.wait &&
						(await Promise.race([reply.wait.then(() => "ready" as const), stopped])) === "aborted"
					) {
						await run("after_response", { lane: name, message: { stopReason: "aborted", usage: undefined } });
						return { ok: true, value: { status: "aborted", tipId: fromTipId, fromTipId } };
					}
					counter += 1;
					const id = `${name}#${counter}`;
					if (reply.fail) {
						const failed = { role: "assistant", content: [], stopReason: "error", errorMessage: "HTTP 500" };
						await run("after_response", { lane: name, message: failed });
						entries.push({ id, type: "message", message: failed });
						return {
							ok: true,
							value: { status: "failed", tipId: id, fromTipId, error: { message: "HTTP 500" } },
						};
					}
					const text = reply.text ?? "";
					const usage = {
						input: reply.usage?.input ?? Math.ceil(JSON.stringify(payload).length / 4),
						output: reply.usage?.output ?? 8,
						cacheRead: reply.usage?.cacheRead ?? 0,
						cacheWrite: 0,
						totalTokens: 0,
						cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
					};
					const assistant = { role: "assistant", content: [{ type: "text", text }], usage, stopReason: "stop" };
					await run("after_response", { lane: name, message: assistant });
					conversation.push({ role: "assistant", content: text });
					entries.push({ id, type: "message", message: assistant });
					if (reply.followUp) {
						// A tool round: the harness asks before_request first, and a block fails the run unsent.
						const blocked = (await run("before_request", { lane: name, model: MODEL, step: "assistant" }))
							?.block as { reason: string } | undefined;
						if (blocked)
							return {
								ok: true,
								value: {
									status: "failed",
									tipId: id,
									fromTipId,
									error: { code: "request_blocked", message: blocked.reason },
								},
							};
						conversation.push({ role: "user", content: "tool result" });
						let next: Record<string, unknown> = {
							...payload,
							messages: [...(payload.messages as unknown[]), ...conversation.slice(-2)],
						};
						next =
							((await run("before_payload", { lane: name, model: MODEL, payload: next }))?.payload as Record<
								string,
								unknown
							>) ?? next;
						calls.push({ lane: name, message: "tool round", system, payload: next, attempt: counter });
						counter += 1;
						const followId = `${name}#${counter}`;
						const second = {
							role: "assistant",
							content: [{ type: "text", text: reply.followUp.text }],
							usage: { ...usage, input: Math.ceil(JSON.stringify(next).length / 4) },
							stopReason: "stop",
						};
						await run("after_response", { lane: name, message: second });
						conversation.push({ role: "assistant", content: reply.followUp.text });
						entries.push({ id: followId, type: "message", message: second });
						return { ok: true, value: { status: "completed", tipId: followId, fromTipId } };
					}
					return { ok: true, value: { status: "completed", tipId: id, fromTipId } };
				},
				findEntries: async (query?: { stopAtId?: string }) => {
					const start =
						query?.stopAtId === undefined ? 0 : entries.findIndex((entry) => entry.id === query.stopAtId) + 1;
					return entries.slice(start).reverse();
				},
			};
			lanes.set(name, lane);
			return lane;
		},
	};
	return { harness, calls, aborts, activeTools };
}

const dirs: string[] = [];
afterEach(() => {
	for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function setup(script: Script, options: { usage?: NativeUsageLedger } = {}) {
	const fake = hookedHarness(script);
	const contextDir = mkdtempSync(join(tmpdir(), "ultron-infer-"));
	dirs.push(contextDir);
	const traces = createMemoryFrameStore();
	const usage = options.usage ?? new NativeUsageLedger();
	const inference = createInferenceRuntime({ contextDir, traces, usage });
	inference.install(fake.harness as never);
	const store = memoryStore();
	const host = new NativeRlmHost(fake.harness as never, {} as never, {
		store,
		definitionStore: memoryDefinitionStore(),
		usage,
		frames: inference.executor,
		modules: [inference.module],
	});
	const call = <T = Record<string, unknown>>(
		type: string,
		payload: Record<string, unknown>,
		options: { lane?: string; context?: Context } = {},
	) =>
		host.handle(
			type,
			payload,
			options.context ?? ({} as Context),
			options.lane === undefined ? undefined : { lane: options.lane },
		) as Promise<T>;
	return { ...fake, host, call, contextDir, traces, usage, store };
}

type Observation = {
	status: "complete" | "incomplete" | "error";
	value?: unknown;
	reason?: string;
	detail?: string;
	error?: string;
	trace_id: string;
	task_id?: string;
	spent: { calls: number; tokens: number };
	remaining: { calls: number | null; tokens: number | null; depth: number };
	last_outputs?: string[];
};
type MapReply = {
	results: Observation[];
	budget: { spent: { calls: number; tokens: number }; limits: { calls: number | null; tokens: number | null } };
};

const text = (value: string) => ({ kind: "text", label: "literal", text: value });
const viewOf = (message: string): string =>
	/--- view 1: [^\n]*---\n([\s\S]*?)\n--- end of view 1 ---/.exec(message)?.[1] ?? "";
const count = (body: string) => body.split("\n").filter((line) => line.includes("ERROR")).length;

describe("rlm.load: content-addressed handles under the session", () => {
	test("registers content by digest, reports whether it is stored, and reopens it", async () => {
		const { call, contextDir } = setup(() => ({ text: "unused" }));
		const content = "alpha\nbeta\n";
		const hex = createHash("sha256").update(content).digest("hex");
		const first = await call<{ path: string; stored: boolean }>("rlm.load", {
			digest: `sha256:${hex}`,
			label: "notes.txt",
			size: content.length,
			chars: content.length,
		});
		expect(first).toEqual({ path: join(contextDir, hex), stored: false });
		await expect(call("rlm.load", { digest: `sha256:${hex}` })).rejects.toThrow(/Unknown context handle/);
		writeFileSync(first.path, content);
		expect(
			await call("rlm.load", { digest: `sha256:${hex}`, label: "notes.txt", size: content.length }),
		).toMatchObject({ stored: true });
		expect(await call("rlm.load", { digest: `sha256:${hex}` })).toMatchObject({ label: "notes.txt", stored: true });
		await expect(call("rlm.load", { digest: "sha256:../../etc/passwd", size: 1 })).rejects.toThrow(/digest/);
	});
});

describe("rlm.infer: a private frame over explicit views", () => {
	test("the frame sees only its task and views under its own system prompt, with no tools", async () => {
		const { call, calls, activeTools, store } = setup(({ message }) => ({ text: String(count(viewOf(message))) }));
		const body = "ok\nERROR one\nok\nERROR two\n";
		const reply = await call<Observation>("rlm.infer", {
			task: "Count the ERROR lines.",
			context: [{ kind: "text", label: "app.log[0:26]", text: body }],
			contract: { type: "integer" },
		});
		expect(reply.status).toBe("complete");
		expect(reply.value).toBe(2);
		expect(calls).toHaveLength(1);
		expect(calls[0]!.system).toContain("inference frame");
		expect(calls[0]!.system).not.toContain("ROOT SYSTEM PROMPT");
		expect(calls[0]!.message).toContain("Count the ERROR lines.");
		expect(calls[0]!.message).toContain(body);
		expect(activeTools.get(calls[0]!.lane)).toEqual([]);
		// The frame is an ordinary journaled task.
		const tasks = (
			store as unknown as { document(): { tasks: Array<{ definition: string; state: string }> } }
		).document();
		expect(JSON.stringify(tasks)).toContain("rlm-frame@1");
		expect(reply.spent.calls).toBe(1);
		expect(reply.spent.tokens).toBeGreaterThan(0);
	});

	test("without a contract the reply text is returned", async () => {
		const { call } = setup(() => ({ text: "a short answer" }));
		const reply = await call<Observation>("rlm.infer", { task: "Say something.", context: [text("x")] });
		expect(reply).toMatchObject({ status: "complete", value: "a short answer" });
	});

	test("a malformed contract is refused before any frame runs", async () => {
		const { call, calls } = setup(() => ({ text: "1" }));
		await expect(call("rlm.infer", { task: "t", context: [], contract: { type: "banana" } })).rejects.toThrow(
			/Invalid JSON schema/,
		);
		expect(calls).toHaveLength(0);
	});
});

describe("A48 contract repair: re-ask within budget, Incomplete on exhaustion", () => {
	test("a frame that first answers malformed JSON is re-asked and returns a valid value", async () => {
		const { call, calls } = setup(({ attempt }) => ({
			text: attempt === 0 ? "I think the answer is {count: 2" : '{"count": 2}',
		}));
		const reply = await call<Observation>("rlm.infer", {
			task: "Count.",
			context: [text("ERROR\nERROR\n")],
			contract: {
				type: "object",
				properties: { count: { type: "integer" } },
				required: ["count"],
				additionalProperties: false,
			},
		});
		expect(reply).toMatchObject({ status: "complete", value: { count: 2 } });
		expect(calls).toHaveLength(2);
		expect(calls[1]!.message).toMatch(/does not satisfy the contract/);
		// The re-ask continues the same private conversation.
		expect(calls[1]!.lane).toBe(calls[0]!.lane);
		expect(reply.spent.calls).toBe(2);
	});

	test("exhausted repairs yield an Incomplete with status, spent and remaining budget, trace id and outputs", async () => {
		const { call, calls, traces } = setup(() => ({ text: "not json at all" }));
		const reply = await call<Observation>("rlm.infer", {
			task: "Count.",
			context: [text("x")],
			contract: { type: "integer" },
			max_repairs: 1,
			budget: { calls: 5, tokens: 100_000 },
		});
		expect(reply.status).toBe("incomplete");
		expect(reply.reason).toBe("contract_unmet");
		expect(calls).toHaveLength(2);
		expect(reply.spent.calls).toBe(2);
		expect(reply.remaining.calls).toBe(3);
		expect(reply.remaining.tokens).toBe(100_000 - reply.spent.tokens);
		expect(reply.last_outputs).toEqual(["not json at all", "not json at all"]);
		expect(reply.trace_id).toMatch(/^frame-/);
		const trace = traces.documents.get(reply.trace_id) as Record<string, unknown>;
		expect(trace).toMatchObject({ id: reply.trace_id, status: "incomplete", reason: "contract_unmet" });
		expect((trace.attempts as unknown[]).length).toBe(2);
	});

	test("running out of calls mid-repair is an Incomplete, not an exception", async () => {
		const { call, calls } = setup(() => ({ text: "nope" }));
		const reply = await call<Observation>("rlm.infer", {
			task: "Count.",
			context: [text("x")],
			contract: { type: "integer" },
			budget: { calls: 1 },
		});
		expect(reply).toMatchObject({ status: "incomplete", reason: "budget_exhausted" });
		expect(calls).toHaveLength(1);
		expect(reply.remaining.calls).toBe(0);
	});
});

describe("A49 budget subtree: shared pool, tranches, refunds", () => {
	test("a map of 8 frames under calls=6 runs 6 and marks 2 incomplete, in order", async () => {
		const { call, calls } = setup(({ message }) => ({ text: /item (\d+)/.exec(message)![1]! }));
		const reply = await call<MapReply>("rlm.map", {
			frames: Array.from({ length: 8 }, (_, index) => ({ task: `Echo item ${index}.`, context: [] })),
			contract: { type: "string" },
			budget: { calls: 6 },
		});
		expect(calls).toHaveLength(6);
		expect(reply.results.map((result) => result.status)).toEqual([
			...Array(6).fill("complete"),
			"incomplete",
			"incomplete",
		]);
		expect(reply.results.slice(0, 6).map((result) => result.value)).toEqual(["0", "1", "2", "3", "4", "5"]);
		expect(reply.results[6]).toMatchObject({ reason: "budget_exhausted", remaining: { calls: 0 } });
		expect(reply.budget.spent.calls).toBe(6);
	});

	test("concurrent siblings hold tranches that never oversubscribe the token pool", async () => {
		const release = deferred();
		const pool = 40_000;
		const { call, calls } = setup(() => ({ text: "1", usage: { input: 100, output: 8 }, wait: release.promise }));
		const pending = call<MapReply>("rlm.map", {
			frames: Array.from({ length: 8 }, (_, index) => ({
				task: `Frame ${index}.`,
				context: [text("x".repeat(200))],
			})),
			contract: { type: "integer" },
			budget: { tokens: pool },
			concurrency: 8,
		});
		await waitFor(() => calls.length === 8);
		// Every in-flight request is capped at its tranche: at most a quarter of what remained, at most 16k.
		const caps = calls.map((request) => request.payload.max_completion_tokens as number);
		for (const cap of caps) expect(cap).toBeLessThanOrEqual(Math.min(16_384, pool / 4));
		expect(caps.reduce((sum, cap) => sum + cap, 0)).toBeLessThan(pool);
		release.resolve();
		const reply = await pending;
		expect(reply.results.every((result) => result.status === "complete")).toBe(true);
		expect(reply.budget.spent.tokens).toBe(8 * 108);
		expect(reply.budget.spent.tokens).toBeLessThanOrEqual(pool);
	});

	test("when the pool cannot cover a request the frame is incomplete and nothing is sent", async () => {
		const { call, calls } = setup(() => ({ text: "1", usage: { input: 50, output: 8 } }));
		const reply = await call<MapReply>("rlm.map", {
			frames: [
				{ task: "Small.", context: [] },
				{ task: "Huge.", context: [text("y".repeat(4_000))] },
			],
			contract: { type: "integer" },
			budget: { tokens: 700 },
			concurrency: 1,
		});
		expect(reply.results[0]!.status).toBe("complete");
		expect(reply.results[1]).toMatchObject({ status: "incomplete", reason: "budget_exhausted" });
		expect(calls).toHaveLength(1);
		expect(reply.budget.spent.tokens).toBeLessThanOrEqual(700);
	});

	test("a follow-up request the budget cannot cover is refused before it is sent, and the frame is incomplete", async () => {
		const { call, calls, aborts } = setup(() => ({ text: "1", followUp: { text: "2" } }));
		const reply = await call<MapReply>("rlm.map", {
			frames: [{ task: "Count.", context: [text("z".repeat(600))] }],
			contract: { type: "integer" },
			budget: { calls: 1 },
			concurrency: 1,
		});
		expect(reply.results[0]).toMatchObject({ status: "incomplete", reason: "budget_exhausted" });
		// Only the first request went out; the tool round was refused, not aborted mid-flight.
		expect(calls).toHaveLength(1);
		expect(aborts).toEqual([]);
		expect(reply.budget.spent.calls).toBe(1);
	});

	test("a provider failure refunds its tranche; cache reads are discounted", async () => {
		const usage = new NativeUsageLedger();
		const { call } = setup(
			({ message }) =>
				message.includes("fails")
					? { fail: true }
					: { text: "7", usage: { input: 100, output: 10, cacheRead: 1_000 } },
			{ usage },
		);
		const reply = await call<MapReply>("rlm.map", {
			frames: [
				{ task: "This one fails.", context: [] },
				{ task: "This one works.", context: [] },
			],
			contract: { type: "integer" },
			budget: { tokens: 50_000 },
		});
		expect(reply.results[0]).toMatchObject({ status: "error", spent: { calls: 1, tokens: 0 } });
		expect(reply.results[1]).toMatchObject({ status: "complete", value: 7, spent: { calls: 1, tokens: 210 } });
		// Nothing is left held: the whole pool minus what was charged remains.
		expect(reply.budget.spent).toEqual({ calls: 2, tokens: 210 });
		const status = await usage.status();
		expect(status.activeReservations).toBe(0);
		const frameCalls = (status as unknown as { usage: { modelCalls: number } }).usage.modelCalls;
		expect(frameCalls).toBe(2);
	});

	test("nested frames draw on their parent's pool and one level less depth", async () => {
		const release = deferred();
		const harness = setup(({ message }) =>
			message.includes("inner") ? { text: "5" } : { text: "outer done", wait: release.promise },
		);
		const outer = harness.call<Observation>("rlm.infer", {
			task: "outer task",
			context: [],
			budget: { calls: 3, depth: 2 },
		});
		await waitFor(() => harness.calls.length === 1);
		const outerLane = harness.calls[0]!.lane;
		// A frame with depth gets the rlm cell (and only it).
		expect(harness.activeTools.get(outerLane)).toEqual(["rlm"]);
		expect(harness.calls[0]!.system).toContain("`rlm`");
		// A request from the outer frame's kernel is a nested frame under the outer frame's budget.
		const inner = await harness.call<Observation>(
			"rlm.infer",
			{ task: "inner task", context: [], budget: { depth: 4 } },
			{ lane: outerLane },
		);
		expect(inner).toMatchObject({ status: "complete", value: "5", remaining: { calls: 1, depth: 1 } });
		expect(harness.activeTools.get(harness.calls[1]!.lane)).toEqual([]);
		release.resolve();
		expect(await outer).toMatchObject({ status: "complete", value: "outer done" });
	});

	test("a frame without depth cannot start nested frames", async () => {
		const release = deferred();
		const harness = setup(() => ({ text: "done", wait: release.promise }));
		const outer = harness.call<Observation>("rlm.infer", { task: "outer", context: [] });
		await waitFor(() => harness.calls.length === 1);
		const nested = await harness.call<Observation>(
			"rlm.infer",
			{ task: "nested", context: [] },
			{ lane: harness.calls[0]!.lane },
		);
		expect(nested).toMatchObject({ status: "incomplete", reason: "depth_exhausted" });
		expect(harness.calls).toHaveLength(1);
		release.resolve();
		await outer;
	});
});

describe("A50 cancellation: aborting the root cancels every running frame", () => {
	test("all in-flight frames are cancelled within 2 s and the map returns per-item errors", async () => {
		const never = new Promise<void>(() => {});
		const { call, calls, aborts, store } = setup(() => ({ text: "1", wait: never }));
		const controller = new AbortController();
		const pending = call<MapReply>(
			"rlm.map",
			{
				frames: Array.from({ length: 4 }, (_, index) => ({ task: `Wait ${index}.`, context: [] })),
				contract: { type: "integer" },
			},
			{ context: { abortSignal: controller.signal } as Context },
		);
		await waitFor(() => calls.length === 4);
		const started = Date.now();
		controller.abort(new Error("user pressed Esc"));
		const reply = await pending;
		expect(Date.now() - started).toBeLessThan(2_000);
		expect(reply.results.map((result) => result.status)).toEqual(["error", "error", "error", "error"]);
		expect(new Set(aborts)).toEqual(new Set(calls.map((request) => request.lane)));
		const journal = (store as unknown as { document(): { tasks: Array<{ definition: string; state: string }> } })
			.document()
			.tasks.filter((task) => task.definition === "rlm-frame@1");
		expect(journal.map((task) => task.state)).toEqual(["cancelled", "cancelled", "cancelled", "cancelled"]);
	});
});

describe("frame traces", () => {
	test("traces are stored as session values and listed newest first", async () => {
		const { call, traces } = setup(() => ({ text: "42" }));
		const first = await call<Observation>("rlm.infer", {
			task: "first",
			context: [text("a")],
			contract: { type: "integer" },
		});
		const second = await call<Observation>("rlm.infer", { task: "second", context: [text("b")] });
		const listed = await call<{ frames: Array<{ id: string; status: string }> }>("rlm.frames", {});
		expect(listed.frames.map((frame) => frame.id)).toEqual([second.trace_id, first.trace_id]);
		const trace = await call<Record<string, unknown>>("rlm.frames", { id: first.trace_id });
		expect(trace).toMatchObject({ id: first.trace_id, status: "complete", task: "first", value: "42" });
		expect(traces.documents.has("index")).toBe(true);
		// The trace records view metadata, never the materialized text.
		expect(JSON.stringify(traces.documents.get(first.trace_id))).not.toContain('"text"');
	});
});

describe("the /rlm graph shows frames", () => {
	test("frame summaries carry their call, batch and budget, and render as graph nodes", async () => {
		const { call } = setup(({ message }) => ({ text: message.includes("bad") ? "nope" : "3" }));
		await call("rlm.infer", { task: "good frame", context: [text("a")], contract: { type: "integer" } });
		await call("rlm.infer", {
			task: "bad frame",
			context: [text("b")],
			contract: { type: "integer" },
			max_repairs: 0,
		});
		await call("rlm.map", {
			frames: [1, 2, 3].map((n) => ({ task: `item ${n}`, context: [text(String(n))] })),
			contract: { type: "integer" },
			budget: { calls: 9 },
		});
		const raw = (await call("rlm.frames", { limit: 20 })) as { frames: Record<string, unknown>[] };
		const mapped = raw.frames.filter((frame) => frame.kind === "map");
		expect(mapped).toHaveLength(3);
		expect(new Set(mapped.map((frame) => (frame.budget as { id: string }).id)).size).toBe(1);
		expect(mapped[0]).toMatchObject({
			batch: 3,
			callerTaskId: null,
			budget: { calls: 9, tokens: DEFAULT_MAP_TOKENS, depth: 1 },
		});
		expect(raw.frames.filter((frame) => frame.kind === "infer").map((frame) => frame.batch)).toEqual([1, 1]);

		const frames = parseFrames(raw);
		const lines = renderRlmDock({ now: Date.now(), tasks: [], frames }, 80);
		const rendered = lines.join("\n");
		expect(rendered).toMatch(/✓ rlm\.map 3 frames ▰▰▰▰▰▰▰▰▰▰ 3\/3 +item/);
		expect(rendered).toMatch(/◐ rlm\.infer \w{8} contract_unmet/);
		expect(rendered).toMatch(/✓ rlm\.infer \w{8} good frame/);
		expect(lines.every((line) => visibleWidth(line) <= 80)).toBe(true);
	});
});

describe("cost defaults: map budget, repairs, cacheable frames, spawn depth", () => {
	afterEach(() => {
		delete process.env.ULTRON_RLM_MAP_TOKENS;
		delete process.env.ULTRON_SPAWN_DEPTH;
	});

	test("a top-level map without a token limit gets the default budget and says how to raise it", async () => {
		expect(defaultMapTokens({})).toBe(DEFAULT_MAP_TOKENS);
		expect(defaultMapTokens({ ULTRON_RLM_MAP_TOKENS: "1200" })).toBe(1200);
		expect(defaultMapTokens({ ULTRON_RLM_MAP_TOKENS: "lots" })).toBe(DEFAULT_MAP_TOKENS);
		process.env.ULTRON_RLM_MAP_TOKENS = "1200";
		const { call, calls } = setup(() => ({ text: "1", usage: { input: 500, output: 8 } }));
		const reply = await call<MapReply>("rlm.map", {
			frames: Array.from({ length: 6 }, (_, index) => ({ task: `item ${index}`, context: [text("x")] })),
			contract: { type: "integer" },
			concurrency: 1,
		});
		expect(reply.budget.limits.tokens).toBe(1200);
		const complete = reply.results.filter((result) => result.status === "complete");
		const incomplete = reply.results.filter((result) => result.status === "incomplete");
		expect(complete.length).toBeGreaterThan(0);
		expect(incomplete.length).toBeGreaterThan(0);
		expect(calls.length).toBe(complete.length);
		expect(reply.budget.spent.tokens).toBeLessThanOrEqual(1200);
		expect(incomplete[0]).toMatchObject({ reason: "budget_exhausted" });
		expect(incomplete[0]!.detail).toContain("default budget is 1200 tokens; pass budget=Budget(tokens=...)");
	});

	test("an explicit token limit is kept, and rlm.infer gets no default", async () => {
		const { call } = setup(() => ({ text: "1" }));
		const map = await call<MapReply>("rlm.map", {
			frames: [{ task: "a", context: [] }],
			contract: { type: "integer" },
			budget: { tokens: 9_999_999 },
		});
		expect(map.budget.limits.tokens).toBe(9_999_999);
		const infer = await call<Observation>("rlm.infer", { task: "a", context: [], contract: { type: "integer" } });
		expect(infer.remaining.tokens).toBeNull();
	});

	test("a scalar contract is re-asked once by default, a structured one twice, an explicit count wins", async () => {
		const scalar = setup(() => ({ text: "not a number" }));
		const one = await scalar.call<Observation>("rlm.infer", {
			task: "n",
			context: [],
			contract: { type: "integer" },
		});
		expect(one).toMatchObject({ status: "incomplete", reason: "contract_unmet" });
		expect(scalar.calls).toHaveLength(2);
		const structured = setup(() => ({ text: "not json" }));
		await structured.call("rlm.infer", {
			task: "o",
			context: [],
			contract: { type: "object", properties: { n: { type: "integer" } }, required: ["n"] },
		});
		expect(structured.calls).toHaveLength(3);
		const explicit = setup(() => ({ text: "not a number" }));
		await explicit.call("rlm.infer", { task: "n", context: [], contract: { type: "integer" }, max_repairs: 3 });
		expect(explicit.calls).toHaveLength(4);
	});

	test("a map's frames share the task and shared views as a byte-identical prefix", async () => {
		const { call, calls } = setup(() => ({ text: "ok" }));
		const shared = { kind: "text", label: "guide", text: "Shared rubric. ".repeat(40) };
		await call("rlm.map", {
			frames: ["short", "a much longer item ".repeat(10)].map((item) => ({
				task: "Judge the item by the rubric.",
				context: [shared, text(item)],
			})),
			contract: { type: "string" },
		});
		expect(calls).toHaveLength(2);
		const [first, second] = calls.map((entry) => entry.message);
		const prefix = first!.slice(0, first!.indexOf("--- view 2"));
		expect(prefix).toContain("Shared rubric.");
		expect(second!.startsWith(prefix)).toBe(true);
		expect(first).not.toMatch(/view\(s\)/);
	});

	test("rlm.spawn refuses subagents nested deeper than ULTRON_SPAWN_DEPTH", async () => {
		expect(spawnDepthLimit({})).toBe(DEFAULT_SPAWN_DEPTH);
		expect(spawnDepthLimit({ ULTRON_SPAWN_DEPTH: "0" })).toBe(0);
		process.env.ULTRON_SPAWN_DEPTH = "1";
		const release = deferred();
		const { call, calls } = setup(() => ({ text: "child done", wait: release.promise }));
		const child = await call<{ rlm_child_id: string }>("rlm.spawn", { prompt: "child brief", kwargs: { name: "c" } });
		await waitFor(() => calls.length === 1);
		const lane = calls[0]!.lane;
		await expect(call("rlm.spawn", { prompt: "grandchild", kwargs: { name: "g" } }, { lane })).rejects.toThrow(
			/nest at most 1 level/,
		);
		process.env.ULTRON_SPAWN_DEPTH = "2";
		const grandchild = await call<{ rlm_child_id: string }>(
			"rlm.spawn",
			{ prompt: "grandchild", kwargs: { name: "g" } },
			{ lane },
		);
		release.resolve();
		const collected = await call<{ results: Array<{ result: { status: string } }> }>("rlm.collect", {
			selectors: [child.rlm_child_id, grandchild.rlm_child_id],
		});
		expect(collected.results.map((entry) => entry.result.status)).toEqual(["succeeded", "succeeded"]);
	});

	test("the guide's inference section stays short and keeps the rules that matter", () => {
		expect(INFERENCE_PROMPT.length).toBeLessThan(2_000);
		for (const rule of ["rlm.load", "rlm.infer", "rlm.map", "Incomplete", "Budget(tokens=", "filter with code first"])
			expect(INFERENCE_PROMPT).toContain(rule);
	});
});

describe("ContextHandle in the kernel", () => {
	test("handles never render content; views, search, lines and chunks slice it; reopen by digest", async () => {
		const dir = mkdtempSync(join(tmpdir(), "ultron-handle-"));
		dirs.push(dir);
		const secret = Array.from(
			{ length: 400 },
			(_, index) => `row ${index} ${index % 9 === 0 ? "ERROR disk" : "ok"}`,
		).join("\n");
		writeFileSync(join(dir, "data.log"), `${secret}\n`);
		const { call } = setup(() => ({ text: "unused" }));
		const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
		const kernel = new RlmKernel({ cwd: dir, runtimePath }, (type, payload) =>
			call(type, payload as Record<string, unknown>),
		);
		try {
			const loaded = await kernel.execute("h = await rlm.load('data.log')\nh");
			expect(loaded.status).toBe("ok");
			expect(loaded.result).toMatch(
				/^ContextHandle\(label='data.log', chars=\d+, size=\d+, digest='sha256:[0-9a-f]{12}'…\)$/,
			);
			expect(JSON.stringify(loaded)).not.toContain("row 1 ok");
			const probe = await kernel.execute(
				[
					"hits = h.search(r'ERROR', limit=3)",
					"view = h.lines(hits[1]['line'], hits[1]['line'] + 1)",
					"chunks = h.chunks(500)",
					"again = await rlm.open(h.digest)",
					"print([m['line'] for m in hits], view.text.strip(), ''.join(c.text for c in chunks) == h.slice().text, all(len(c) <= 500 for c in chunks), again is h, h.count('ERROR'), h.line_count())",
				].join("\n"),
			);
			expect(probe).toMatchObject({ status: "ok", stdout: "[0, 9, 18] row 9 ERROR disk True True True 45 400\n" });
		} finally {
			await kernel.shutdown();
		}
	});
});

describe("large maps and contracts in the kernel", () => {
	const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
	function kernelWith(script: Script) {
		const dir = mkdtempSync(join(tmpdir(), "ultron-bigmap-"));
		dirs.push(dir);
		const harness = setup(script);
		const kernel = new RlmKernel({ cwd: dir, runtimePath }, (type, payload) =>
			harness.call(type, payload as Record<string, unknown>),
		);
		return { ...harness, kernel };
	}

	test("a map whose inline text passes the 1 MiB protocol frame goes by handle, in one call, keeping state", async () => {
		const { kernel, calls, contextDir } = kernelWith(({ message }) => ({ text: String(viewOf(message).length) }));
		try {
			const cell = [
				"kept = 'still here'",
				"docs = [f'report {i} ' + 'x' * 8000 for i in range(157)]",
				"rubric = 'Shared rubric. ' * 1400",
				"res = await rlm.map('Length of the item.', docs, contract=int)",
				"shared = await rlm.map('Length.', [d[:100] for d in docs[:40]], context=rubric, contract=int)",
				"print(len(res), res[0] == len(docs[0]), res[-1] == len(docs[-1]), shared[0] == len(rubric))",
			].join("\n");
			const result = await kernel.execute(cell);
			expect(result).toMatchObject({ status: "ok" });
			expect(result.stdout).toContain("157 True True True\n");
			expect(calls).toHaveLength(197);
			// Each item went to the host as a stored handle; frames still see the same labelled view.
			expect(calls[0]!.message).toContain("--- view 1: literal (8009 chars) ---\nreport 0 xxx");
			expect(readdirSync(contextDir).length).toBe(158);
			expect(await kernel.execute("kept")).toMatchObject({ status: "ok", result: "'still here'" });
		} finally {
			await kernel.shutdown();
		}
	}, 60_000);

	test("contracts may be dataclasses, TypedDicts, Literal and {field: type} shorthands", async () => {
		const { kernel, calls } = kernelWith(() => ({ text: '{"ok": true, "role": "leaf", "note": null}' }));
		try {
			const result = await kernel.execute(
				[
					"from dataclasses import dataclass",
					"from typing import Literal",
					"@dataclass",
					"class Verdict:",
					"    ok: bool",
					"    role: Literal['leaf', 'client']",
					"    note: str | None = None",
					"a = await rlm.infer('Judge.', 'text', contract=Verdict)",
					"b = await rlm.infer('Judge.', 'text', contract={'ok': bool, 'role': Literal['leaf', 'client'], 'note': str | None})",
					"print(a == b == {'ok': True, 'role': 'leaf', 'note': None})",
				].join("\n"),
			);
			expect(result).toMatchObject({ status: "ok", stdout: "True\n" });
			expect(calls).toHaveLength(2);
		} finally {
			await kernel.shutdown();
		}
	}, 30_000);
});
