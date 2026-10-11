// Generates crates/inference/fixtures/infer.json: Python cells calling rlm.infer through the real runtime.py and the
// TypeScript kernel supervisor (kernel.ts), served by the TS host (NativeRlmHost + InferenceRuntime from
// ultron/rlm/inference.ts). Frame lanes are the scripted lanes of test/ultron-a47-a50-bounded-inference.test.ts
// (they run the runtime's transform_context / before_payload / after_response hooks like the agent harness), with
// one fake openai-completions model. Each cell lists its scripted replies; every provider request is recorded
// (system prompt, conversation, capped max_completion_tokens). Frame ids are normalized.
// Usage: node crates/inference/fixtures/generate.mjs
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const pkg = join(root, "packages/coding-agent");
const load = (path) => import(pathToFileURL(join(pkg, path)).href);
const { createInferenceRuntime, createMemoryFrameStore } = await load("src/ultron/rlm/inference.ts");
const { RlmKernel } = await load("src/ultron/rlm/kernel.ts");
const { NativeRlmHost } = await load("src/ultron/rlm/native-host.ts");
const { NativeUsageLedger } = await load("src/ultron/usage.ts");
const { memoryStore, memoryDefinitionStore } = await load("test/ultron-host-fixtures.ts");

export const MODEL = { provider: "fake", id: "frame-model", api: "openai-completions", contextWindow: 32_000, maxTokens: 8_192 };

const u = (input, output, cacheRead = 0) => ({ input, output, cacheRead });
// Each cell: Python code and the replies its frames get, in request order.
export const cells = [
	{ code: 'await rlm.infer("Say hi")', replies: [{ text: "hello", usage: u(100, 5) }] },
	{
		code: 'r = await rlm.infer("How many lines?", context=["a\\nb", "c"], contract=int)\nprint(r, type(r).__name__)',
		replies: [{ text: "3", usage: u(200, 2) }],
	},
	{
		code: 'r = await rlm.infer("Count", contract={"n": int})\nr',
		replies: [{ text: "nope", usage: u(50, 3) }, { text: '{"n": "x"}', usage: u(60, 4) }, { text: '{"n": 3}', usage: u(70, 4, 40) }],
	},
	{
		code: 'r = await rlm.infer("Pick", contract=int)\nprint(bool(r), r.status, r.detail, r.spent, r.remaining, r.last_outputs)',
		replies: [{ text: "x", usage: u(10, 1) }, { text: "y", usage: u(10, 1) }],
	},
	{
		code: 'r = await rlm.infer("Pick", contract=int, budget=Budget(calls=1))\nprint(r.status, r.detail, r.spent, r.remaining)',
		replies: [{ text: "x", usage: u(10, 1) }],
	},
	{
		code: 'r = await rlm.infer("Pick", contract=int, budget=Budget(tokens=100))\nprint(r.status, r.detail, r.spent, r.remaining, r.last_outputs)',
		replies: [],
	},
	{
		code: 'r = await rlm.infer("Pick", budget=Budget(calls=0))\nprint(r.status, r.detail, r.spent, r.remaining)',
		replies: [],
	},
	{
		code: 'r = await rlm.infer("Summarize", context=["x" * 300], budget=Budget(tokens=2000))\nprint(r, await rlm.infer("Again", budget={"tokens": 500, "calls": 3}))',
		replies: [{ text: "short", usage: u(150, 20) }, { text: "again", usage: u(100, 10) }],
	},
	{
		code: 'r = await rlm.infer("List", contract=list[int])\nr',
		replies: [{ text: "```json\n[1, 2]\n```", usage: u(10, 5) }],
	},
	{
		code: 'r = await rlm.infer("Words", contract=str)\nprint(repr(r))\nprint(repr(await rlm.infer("Quoted", contract=str)))',
		replies: [{ text: "  plain words  ", usage: u(10, 5) }, { text: '"json string"', usage: u(10, 5) }],
	},
	{
		code: 'from typing import Literal\nr = await rlm.infer("Choose", contract=Literal["yes", "no"], max_repairs=3)\nr',
		replies: [{ text: '"maybe"', usage: u(10, 2) }, { text: "", usage: u(10, 0) }, { text: 'Answer: "yes" ok', usage: u(10, 2) }, { text: '"no"', usage: u(10, 2) }],
	},
	{
		code: 'r = await rlm.infer("Obj", contract={"type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "array", "items": {"type": "string"}}}, "required": ["a", "b"], "additionalProperties": False}, max_repairs=0)\nprint(r.status, r.detail)',
		replies: [{ text: 'here: {"b": [1, "x"], "c": true}', usage: u(10, 2) }],
	},
	{
		code: 'r = await rlm.infer("Maybe", contract=int | None, max_repairs=1)\nprint(repr(r))',
		replies: [{ text: '"1"', usage: u(10, 2) }, { text: "null", usage: u(10, 2) }],
	},
	{
		code: 'print(await rlm.infer("Cached", context=["é😀 data"], budget=Budget(calls=2, tokens=5000)))\nprint(repr(await rlm.infer("Empty", max_repairs=0)))',
		replies: [{ text: "ok", usage: u(1000, 10, 1234) }, { text: "  ", usage: u(10, 1) }],
	},
	{
		code: [
			"for kwargs in [dict(task=''), dict(task='t', contract={'type': 'wat'}), dict(task='t', thinking='bogus'),",
			"               dict(task='t', model='nomodel'), dict(task='t', max_repairs=9), dict(task='t', timeout_ms=0),",
			"               dict(task='t', contract={'properties': 3}), dict(task='t', contract={'$ref': '#/x'}),",
			"               dict(task='t', contract={'type': 'string', 'minLength': -1}), dict(task='x' * 70000)]:",
			"    try:",
			"        await rlm.infer(**kwargs)",
			"    except Exception as error:",
			"        print(type(error).__name__, str(error)[:200])",
		].join("\n"),
		replies: [],
	},
	{
		code: 'h = await rlm.load(text="line one\\nline two\\n", label="notes")\nprint(await rlm.infer("Second line?", context=[h.lines(1, 2)], budget=Budget(calls=1)))',
		replies: [{ text: "line two", usage: u(30, 3) }],
	},
	{
		code: 'print(repr(await rlm.infer("Cache", contract=int, max_repairs=0)))\nprint(repr(await rlm.infer("Float", contract=float)), repr(await rlm.infer("Floats", contract=list[float])))',
		replies: [{ text: "x", usage: u(10, 1, 1234) }, { text: "2.0", usage: u(10, 1) }, { text: "[1.0, 2.5, -0.0]", usage: u(10, 1) }],
	},
	{
		code: 'r = await rlm.infer("Tight", budget=Budget(tokens=330))\nprint(r, await rlm.infer("Tight", budget=Budget(tokens=300)))',
		replies: [{ text: "fits", usage: u(10, 1) }, { text: "fits too", usage: u(10, 1) }],
	},
	{
		code: 'h = await rlm.load(text="abc", label="é" * 150)\nk = await rlm.load(text="def", label="ü" * 250)\nprint(await rlm.infer("Labels", context=[h.slice(0, 2), k.slice(1)]))',
		replies: [{ text: "seen", usage: u(10, 1) }],
	},
	{
		code: 'r = await rlm.infer("Bool", contract=bool, budget=Budget(calls=5, tokens=900))\nprint(repr(r))',
		replies: [{ text: "true!", usage: u(200, 30) }, { text: "1", usage: u(200, 30) }],
	},
];

function lanes(queue, calls) {
	const handlers = new Map();
	const run = async (name, event) => {
		let result;
		for (const handler of handlers.get(name) ?? []) result = (await handler(event, {})) ?? result;
		return result;
	};
	const made = new Map();
	return {
		hooks: {
			on(name, handler) {
				const list = handlers.get(name) ?? [];
				list.push(handler);
				handlers.set(name, list);
				return () => list.splice(list.indexOf(handler), 1);
			},
		},
		lane: async (name) => {
			if (made.has(name)) return made.get(name);
			const entries = [];
			const conversation = [];
			let counter = 0;
			const lane = {
				getActiveTools: async () => [],
				setActiveTools: async () => {},
				setModel: async () => {},
				setThinkingLevel: async () => {},
				getModel: async () => MODEL,
				steer: async () => ({ ok: true, value: {} }),
				abort: async () => ({ ok: true }),
				prompt: async (message) => {
					const fromTipId = entries.at(-1)?.id ?? null;
					conversation.push({ role: "user", content: message });
					const transformed = await run("transform_context", { lane: name, messages: [], systemPrompt: "ROOT" });
					const system = transformed?.systemPrompt ?? "ROOT";
					let payload = { model: MODEL.id, max_completion_tokens: MODEL.maxTokens, messages: [{ role: "system", content: system }, ...conversation] };
					payload = (await run("before_payload", { lane: name, model: MODEL, payload }))?.payload ?? payload;
					calls.push({ system, messages: structuredClone(conversation), max_tokens: payload.max_completion_tokens });
					const reply = queue.shift();
					if (!reply) throw new Error("unexpected request");
					counter += 1;
					const id = `${name}#${counter}`;
					const usage = { input: reply.usage.input, output: reply.usage.output, cacheRead: reply.usage.cacheRead, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };
					const assistant = { role: "assistant", content: [{ type: "text", text: reply.text }], usage, stopReason: "stop" };
					await run("after_response", { lane: name, message: assistant });
					conversation.push({ role: "assistant", content: reply.text });
					entries.push({ id, type: "message", message: assistant });
					return { ok: true, value: { status: "completed", tipId: id, fromTipId } };
				},
				findEntries: async (query) => {
					const start = query?.stopAtId === undefined ? 0 : entries.findIndex((entry) => entry.id === query.stopAtId) + 1;
					return entries.slice(start).reverse();
				},
			};
			made.set(name, lane);
			return lane;
		},
	};
}


// Contract checks: typebox Check/Errors (the first three messages, as parseReply joins them) over a table.
const { Check, Errors } = await import(pathToFileURL(join(pkg, "node_modules/typebox/build/value/index.mjs")).href).catch(() => import("typebox/value"));
const S = { type: "object", properties: { a: { type: "integer" }, b: { type: "array", items: { type: "string" } } }, required: ["a", "b"] };
const checkInputs = [
	[{ type: "integer" }, "x"], [{ type: "integer" }, 1.5], [{ type: "integer" }, 2], [{ type: "number" }, "1"], [{ type: "boolean" }, 1],
	[{ type: "string" }, 3], [{ type: "null" }, 0], [{ type: "null" }, null], [{ type: "array" }, {}], [{ type: "object" }, []],
	[{ type: "array", items: { type: "integer" } }, [1, "a", 2.5, "b", "c"]], [S, { a: "x" }], [S, {}], [S, { a: 1, b: ["x", 2] }], [S, { a: 1, b: [] }],
	[{ ...S, additionalProperties: false }, { a: 1, b: [], z: 1, y: 2 }], [{ ...S, additionalProperties: false }, { a: "x", b: [2], z: 1 }],
	[{ type: "object", properties: { a: { type: "integer" } }, additionalProperties: { type: "string" } }, { a: "x", q: 1 }], [{ type: "object", additionalProperties: { type: "integer" } }, { a: 1, b: "x" }],
	[{ enum: ["a", "b"] }, "c"], [{ enum: ["a", { k: [1] }] }, { k: [1] }], [{ enum: [1, null] }, null], [{ const: 3 }, 4], [{ const: { a: [1] } }, { a: [1] }],
	[{ anyOf: [{ type: "integer" }, { type: "null" }] }, "x"], [{ anyOf: [{ type: "integer" }, { type: "null" }] }, null],
	[{ type: ["string", "null"] }, 3], [{ type: ["string", "null"] }, null], [{ type: ["string", "integer", "null"] }, true],
	[{ type: "string", minLength: 3 }, "ab"], [{ type: "string", maxLength: 1 }, "ab"], [{ type: "string", minLength: 2 }, "😀"],
	[{ type: "integer", minimum: 3 }, 1], [{ type: "number", maximum: 2.5 }, 5], [{ type: "number", exclusiveMinimum: 0 }, 0], [{ type: "number", exclusiveMaximum: 1 }, 1],
	[{ type: "array", minItems: 3, items: { type: "string" } }, [1]], [{ type: "array", maxItems: 1 }, [1, 2]], [{ type: "array", uniqueItems: true }, [1, [2], [2]]],
	[{ type: "integer", enum: [1, 2] }, 3], [{ type: "integer", enum: [1, 2] }, "x"], [{ enum: [1, 2], const: 1 }, 3], [{ type: "string", minLength: 5, enum: ["a"] }, "b"],
	[{ anyOf: [{ type: "string" }, { type: "integer" }], minLength: 2 }, "a"], [{}, 3], [{ title: "T", description: "d" }, "x"],
	[{ type: "object", properties: { a: { type: "object", properties: { b: { type: "integer" } }, required: ["b"] } } }, { a: { b: "x" } }],
	[{ type: "object", properties: { "a/b": { type: "integer" }, "c~d": { type: "integer" } } }, { "a/b": "x", "c~d": "y" }],
];
const checks = checkInputs.map(([schema, value]) => ({ schema, value, ok: Check(schema, value), errors: [...Errors(schema, value)].map((e) => `${e.instancePath || "value"} ${e.message}`) }));
const { validateJsonSchema } = await load("src/ultron/rlm/definition-registry.ts");
const schemaInputs = [
	{ type: "integer" }, { type: "wat" }, 5, "x", null, true, { properties: 3 }, { anyOf: [] }, { anyOf: [{ type: "x" }] }, { type: [] }, { type: ["string", "string"] }, { type: 3 },
	{ type: "object", properties: { a: { type: "nope" } } }, { minLength: "x" }, { minLength: -1 }, { maximum: "1" }, { required: ["a", "a"] }, { required: [""] },
	{ enum: [] }, { $ref: "#/x" }, { foo: 1 }, { "x-note": 1 }, { title: 3 }, { items: true }, { items: { type: "q" } }, { additionalProperties: false },
	{ additionalProperties: 3 }, { uniqueItems: "yes" }, { description: "d", title: "t", $comment: "c" }, { pattern: "(" }, { multipleOf: 0 },
	{ type: "object", properties: { a: { type: "integer" } }, required: ["a"], additionalProperties: false },
];
const schemas = schemaInputs.map((schema) => {
	try {
		validateJsonSchema(schema, "$.contract");
		return { schema, ok: true };
	} catch (error) {
		return { schema, error: error.message };
	}
});

const normalize = (text) => text.replace(/frame-[0-9a-f-]{36}/g, "frame-ID");

const cwd = mkdtempSync(join(tmpdir(), "ultron-infer-fixture-"));
const results = [];
try {
	for (const cell of cells) {
		const queue = cell.replies.map((reply) => ({ ...reply }));
		const calls = [];
		const harness = lanes(queue, calls);
		const usage = new NativeUsageLedger();
		const inference = createInferenceRuntime({ contextDir: join(cwd, "context"), traces: createMemoryFrameStore(), usage, env: {} });
		inference.install(harness);
		const host = new NativeRlmHost(harness, {}, { store: memoryStore(), definitionStore: memoryDefinitionStore(), usage, frames: inference.executor, modules: [inference.module] });
		const kernel = new RlmKernel(
			{ cwd, runtimePath: join(pkg, "src/ultron/rlm/runtime.py"), treeMemoryBackend: "off", env: { ULTRON_RLM_SPILL_DIR: "", TMPDIR: cwd } },
			(type, payload, signal) => host.handle(type, payload, signal ? { abortSignal: signal } : {}, undefined),
		);
		try {
			const { status, stdout, result, error } = await kernel.execute(cell.code);
			if (queue.length) throw new Error(`cell left ${queue.length} replies unused: ${cell.code}`);
			const ok = { status, stdout, result: result ?? null, error: error ? `${error.ename}: ${error.evalue}` : null };
			results.push({ ...cell, ok: JSON.parse(normalize(JSON.stringify(ok))), calls });
		} finally {
			await kernel.shutdown();
			await host.close?.();
		}
	}
} finally {
	rmSync(cwd, { recursive: true, force: true });
}
writeFileSync(join(here, "infer.json"), `${JSON.stringify({ model: MODEL, cells: results, checks, schemas }, null, "\t")}\n`);
