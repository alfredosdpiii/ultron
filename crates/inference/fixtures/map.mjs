// Generates crates/inference/fixtures/map.json: Python cells calling rlm.map, rlm.infer (frame-model settings) and
// rlm._models through the real runtime.py and the TypeScript kernel supervisor, served by the TS host
// (NativeRlmHost + InferenceRuntime of ultron/rlm/inference.ts) with the scripted frame lanes of generate.mjs (one fake
// openai-completions model). Each cell lists its scripted replies (`{fail: true}` makes the lane's run fail),
// env and model settings; every provider request is recorded. Frame ids are normalized. Also records INFERENCE_PROMPT.
// Usage: node crates/inference/fixtures/map.mjs
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const pkg = join(root, "packages/coding-agent");
const load = (path) => import(pathToFileURL(join(pkg, path)).href);
const { createInferenceRuntime, createMemoryFrameStore, INFERENCE_PROMPT } = await load("src/ultron/rlm/inference.ts");
const { RlmKernel } = await load("src/ultron/rlm/kernel.ts");
const { NativeRlmHost } = await load("src/ultron/rlm/native-host.ts");
const { NativeUsageLedger } = await load("src/ultron/usage.ts");
const { memoryStore, memoryDefinitionStore } = await load("test/ultron-host-fixtures.ts");

const MODEL = { provider: "fake", id: "frame-model", api: "openai-completions", contextWindow: 32_000, maxTokens: 8_192 };
const u = (input, output, cacheRead = 0) => ({ input, output, cacheRead });
// Timeouts: a hung request ends only at its frame's timeout, so the deadlines that must not pass first (sending the
// first request, a re-ask after an instant reply, the next frame's request) get seconds of margin for a loaded machine.
const show = "print(r)\nprint(r.spent, r.budget, r.remaining)\nfor x in r: print(type(x).__name__, repr(x))";
const cells = [
	{
		code: `r = await rlm.map("Classify", ["a", "b", "c"], concurrency=1)\n${show}`,
		replies: [{ text: "A", usage: u(100, 5) }, { text: "B", usage: u(110, 6) }, { text: "C", usage: u(120, 7, 50) }],
	},
	// Default concurrency (8): all three frames claim before any answers, so each tranche shrinks the next (the
	// frames are identical, so the reply order does not matter). `gather` holds each reply until 3 requests are in flight.
	{
		code: `r = await rlm.map("Same", ["a", "a", "a"], budget=Budget(tokens=3000))\n${show}\nprint(r.usage)`,
		replies: [{ text: "S", usage: u(100, 5), gather: 3 }, { text: "S", usage: u(100, 5), gather: 3 }, { text: "S", usage: u(100, 5), gather: 3 }],
	},
	{
		code: `r = await rlm.map("Count", ["x", "yy"], contract=int, concurrency=1)\n${show}`,
		replies: [{ text: "1", usage: u(50, 1) }, { text: "two", usage: u(50, 1) }, { text: "2", usage: u(50, 1) }],
	},
	{
		code: `r = await rlm.map("Pick", ["a", "b", "c"], budget=Budget(calls=2), concurrency=1)\n${show}\nprint([x.detail for x in r if isinstance(x, Incomplete)])`,
		replies: [{ text: "one", usage: u(10, 1) }, { text: "two", usage: u(10, 1) }],
	},
	{
		code: `r = await rlm.map("Pick", ["a", "b", "c"], budget=Budget(tokens=1500), concurrency=1)\n${show}\nprint([(x.detail, x.last_outputs) for x in r if isinstance(x, Incomplete)])`,
		replies: [{ text: "one", usage: u(600, 20) }, { text: "two", usage: u(500, 20) }, { text: "three", usage: u(30, 5) }],
	},
	{
		code: `r = await rlm.map(["Task one", "Task two"], context=["shared"], concurrency=1)\n${show}`,
		replies: [{ text: "first", usage: u(10, 1) }, { text: "second", usage: u(10, 1) }],
	},
	{
		code: `r = await rlm.map(["T1", "T2"], [["i1", "j1"], "i2"], contract={"ok": bool}, max_repairs=0, concurrency=1)\n${show}`,
		replies: [{ text: '{"ok": true}', usage: u(10, 1) }, { text: "nope", usage: u(10, 1) }],
	},
	{
		code: `r = await rlm.map("Fail", ["a", "b"], budget=Budget(calls=2, tokens=1000), concurrency=1)\n${show}\nprint([x.error for x in r if isinstance(x, FrameError)])`,
		replies: [{ fail: true }, { fail: true }],
		noKey: true,
	},
	{ code: `r = await rlm.map("None", [])\n${show}`, replies: [] },
	{
		code: [
			"for args, kwargs in [(('t', ['a']), dict(concurrency=0)), (('t', ['a']), dict(concurrency=17)), (('', ['a']), {}),",
			"                     (('t', None), {}), ((['a', 'b'], ['x']), {}), (('t', ['a']), dict(contract={'type': 'wat'})),",
			"                     (('t', ['a']), dict(model='bad')), (('t', ['a']), dict(thinking='lots')), (('t', ['a']), dict(timeout_ms=0)),",
			"                     (('t', ['a']), dict(budget={'calls': -1})), (('t', ['a']), dict(max_repairs=9)), (('x' * 70000, ['a']), {}),",
			"                     (('t', ['a']), dict(budget={'tokens': 2**60}))]:",
			"    try:",
			"        await rlm.map(*args, **kwargs)",
			"    except Exception as error:",
			"        print(type(error).__name__, str(error)[:200])",
		].join("\n"),
		replies: [],
	},
	{
		code: 'r = await rlm.map("Pick", ["a", "b"], budget=Budget(calls=1, tokens=5000), model="fake/frame-model", timeout_ms=600000, thinking="low", max_repairs=1, concurrency=2)\nprint(r)\nprint(r.remaining)',
		replies: [{ text: "only", usage: u(10, 1) }],
	},
	{
		code: 'print(await rlm._models())\nprint(await rlm.infer("Hi"))\nr = await rlm.map("Each", ["a"])\nprint(r)',
		replies: [{ text: "hello", usage: u(10, 1) }, { text: "each", usage: u(10, 1) }],
		env: { ULTRON_RLM_FRAME_MODEL: " fake/frame-model " },
		settings: { rlm: { frameModel: "other/model", frameThinking: "high", childModel: "kid/model" }, reviewModel: "rev/model" },
	},
	{
		code: "print(await rlm._models())",
		replies: [],
		env: { ULTRON_RLM_FRAME_MODEL: "not-a-ref", ULTRON_REVIEW_MODEL: "env/review" },
		settings: { rlm: { frameModel: "set/frame" } },
	},
	{ code: "print(await rlm._models())", replies: [], env: {}, settings: { rlm: { frameModel: "set/frame" } } },
	{ code: "print(await rlm._models())", replies: [], env: {}, settings: { rlm: {} } },
	{
		code: 'r = await rlm.map("Use", ["a", "b"], concurrency=1)\nprint(r.usage)\nr = await rlm.map("None", [])\nprint(r.usage)',
		replies: [{ text: "x", usage: u(100, 5) }, { text: "y", usage: u(30, 2, 10) }],
	},
	{
		code: 'try:\n    await rlm.infer("Slow", timeout_ms=5000)\nexcept InferenceError as e:\n    print(repr(e), e.spent, e.last_outputs)',
		replies: [{ hang: true }],
	},
	{
		code: `r = await rlm.map("Slow", ["a", "b"], budget=Budget(calls=2, tokens=2000), timeout_ms=5000, concurrency=1)\n${show}\nprint([x.error for x in r if isinstance(x, FrameError)])`,
		replies: [{ hang: true }, { text: "fast", usage: u(10, 1) }],
	},
	{
		code: 'try:\n    await rlm.infer("Count", contract=int, timeout_ms=5000)\nexcept InferenceError as e:\n    print(repr(e), e.spent, e.last_outputs)',
		replies: [{ text: "many", usage: u(40, 2) }, { hang: true }],
	},
	{
		code: [
			'a = await rlm.infer("First", context=["ctx"])',
			'r = await rlm.map("Pick", ["a", "b", "c"], contract=int, max_repairs=0, budget=Budget(calls=2), concurrency=1)',
			"skip = ('taskId', 'lane', 'startedAt', 'endedAt')",
			"listed = await rlm.frames()",
			"for f in listed['frames']: print({k: v for k, v in f.items() if k not in skip}, type(f['startedAt']).__name__, type(f['endedAt']).__name__)",
			"print([f['task'] for f in (await rlm.frames(limit=2))['frames']])",
			"for f in listed['frames']:",
			"    t = await rlm.frames(f['id'])",
			"    t['attempts'] = [{k: v for k, v in a.items() if k != 'at'} for a in t['attempts']]",
			"    t['requests'] = [{k: v for k, v in q.items() if k != 'at'} for q in t['requests']]",
			"    print({k: v for k, v in t.items() if k not in skip})",
			"for bad in [dict(trace_id='nope'), dict(trace_id='frame-0'), dict(limit=0), dict(limit=201)]:",
			"    try:",
			"        await rlm.frames(**bad)",
			"    except Exception as error:",
			"        print(type(error).__name__, error)",
		].join("\n"),
		replies: [{ text: "a", usage: u(10, 1) }, { text: "7", usage: u(20, 1) }, { text: "x", usage: u(20, 1) }],
	},
	{
		code: 'r = await rlm.map("Pick", ["a", "b"], concurrency=1)\n' + show + "\nprint([x.detail for x in r if isinstance(x, Incomplete)])",
		replies: [{ text: "one", usage: u(19900, 10) }],
		env: { ULTRON_RLM_MAP_TOKENS: " 20000 " },
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
			const hung = [];
			let counter = 0;
			const lane = {
				getActiveTools: async () => [],
				setActiveTools: async () => {},
				setModel: async () => {},
				setThinkingLevel: async () => {},
				getModel: async () => MODEL,
				steer: async () => ({ ok: true, value: {} }),
				abort: async () => {
					for (const wake of hung.splice(0)) wake();
					return { ok: true };
				},
				prompt: async (message, _options, context) => {
					// A frame whose provider request never produced a response (Rust: the missing API key): refunded.
					if (queue[0]?.fail) {
						queue.shift();
						return { ok: false, error: "No API key for provider: fake" };
					}
					const fromTipId = entries.at(-1)?.id ?? null;
					conversation.push({ role: "user", content: message });
					const transformed = await run("transform_context", { lane: name, messages: [], systemPrompt: "ROOT" });
					const system = transformed?.systemPrompt ?? "ROOT";
					let payload = { model: MODEL.id, max_completion_tokens: MODEL.maxTokens, messages: [{ role: "system", content: system }, ...conversation] };
					payload = (await run("before_payload", { lane: name, model: MODEL, payload }))?.payload ?? payload;
					calls.push({ system, messages: structuredClone(conversation), max_tokens: payload.max_completion_tokens });
					const reply = queue.shift();
					if (!reply) throw new Error("unexpected request");
					if (reply.gather) while (calls.length < reply.gather) await new Promise((wake) => setTimeout(wake, 1));
					// A request the provider never answers: it ends when the task's timeout cancels the lane.
					if (reply.hang) {
						await new Promise((wake) => {
							hung.push(wake);
							context?.abortSignal?.addEventListener("abort", wake, { once: true });
						});
						conversation.pop();
						return { ok: true, value: { status: "aborted", tipId: fromTipId, fromTipId } };
					}
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

const normalize = (text) => text.replace(/frame-[0-9a-f-]{36}/g, "frame-ID").replace(/budget-[0-9a-f-]{36}/g, "budget-ID");

const cwd = mkdtempSync(join(tmpdir(), "ultron-map-fixture-"));
const results = [];
try {
	for (const cell of cells) {
		const queue = cell.replies.map((reply) => ({ ...reply }));
		const calls = [];
		const harness = lanes(queue, calls);
		const usage = new NativeUsageLedger();
		const settings = cell.settings ?? { rlm: {} };
		const inference = createInferenceRuntime({ contextDir: join(cwd, "context"), traces: createMemoryFrameStore(), usage, env: cell.env ?? {}, modelSettings: () => settings });
		inference.install(harness);
		const host = new NativeRlmHost(harness, {}, { store: memoryStore(), definitionStore: memoryDefinitionStore(), usage, frames: inference.executor, modules: [inference.module] });
		const kernel = new RlmKernel(
			{ cwd, runtimePath: join(pkg, "src/ultron/rlm/runtime.py"), treeMemoryBackend: "off", env: { ULTRON_RLM_SPILL_DIR: "", TMPDIR: cwd } },
			(type, payload, signal) => host.handle(type, payload, signal ? { abortSignal: signal } : {}, undefined),
		);
		// defaultMapTokens() reads process.env, not the runtime's env option.
		if (cell.env?.ULTRON_RLM_MAP_TOKENS) process.env.ULTRON_RLM_MAP_TOKENS = cell.env.ULTRON_RLM_MAP_TOKENS;
		else delete process.env.ULTRON_RLM_MAP_TOKENS;
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
writeFileSync(join(here, "map.json"), `${JSON.stringify({ model: MODEL, inferencePrompt: INFERENCE_PROMPT, cells: results }, null, "\t")}\n`);
