// Generates crates/kernel-apis/fixtures/context.json: `ctx.*` cells (rlm/context_api.py) through the real runtime.py
// and kernel.ts, served by NativeRlmHost with the module of ultron/context-control.ts (ContextControl). The module
// reads and appends to scripted lane branches (findEntries / appendCustomEntry / getTipId over fixed entries); a
// `test.*` module switches the calling lane, appends transcript entries, and holds appended entries back as a running
// turn does (so the module's pending entries show). tests/context.rs implements the same fakes.
// Also records the pure helpers (taskLine, collapsedCellText, trimmedResultText, applyContextEdits) over a table.
// Usage: node crates/kernel-apis/fixtures/generate-context.mjs
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { attempt, record, show } from "./generate.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const pkg = join(root, "packages/coding-agent");
const load = (path) => import(pathToFileURL(join(pkg, path)).href);
const { ContextControl, taskLine, collapsedCellText, trimmedResultText } = await load("src/ultron/context-control.ts");
const { applyContextEdits } = await import(pathToFileURL(join(root, "packages/agent/src/harness/session/context.ts")).href);

const user = (text) => ({ role: "user", content: text, timestamp: 0 });
const assistant = (text, calls = []) => ({
	role: "assistant",
	content: [{ type: "text", text }, ...calls.map((id) => ({ type: "toolCall", id, name: "rlm", arguments: { code: `run("${id}")` } }))],
	api: "openai-completions",
	provider: "faux",
	model: "faux",
	stopReason: calls.length ? "toolUse" : "stop",
	timestamp: 0,
});
const result = (callId, text) => ({ role: "toolResult", toolCallId: callId, toolName: "rlm", content: [{ type: "text", text }], isError: false, timestamp: 0 });

/** The seeded branches: entry specs [id, type, fields]. */
export function seed() {
	const branches = {
		main: [
			["m0", { type: "model_change", provider: "faux", modelId: "faux" }],
			["u1", { type: "message", message: user("first question") }],
			["a1", { type: "message", message: assistant("calling", ["c1", "c2"]) }],
			["r1", { type: "message", message: result("c1", "BIG ONE ".repeat(40)) }],
			["r2", { type: "message", message: result("c2", "small two\n\twith   spaces") }],
			["a2", { type: "message", message: assistant("answer one — ünïcödé 😀") }],
			["bs1", { type: "branch_summary", summary: "earlier branch work", fromId: "u1" }],
			["cm1", { type: "message", message: { role: "custom", customType: "x", content: "custom text", display: true, timestamp: 0 } }],
			["u2", { type: "message", message: user("second question") }],
			["a3", { type: "message", message: assistant("working", ["c3"]) }],
		],
		"lane-x": [
			["ux", { type: "message", message: user("child task") }],
			["ax", { type: "message", message: assistant("child answer") }],
		],
	};
	let seq = 0;
	const out = {};
	for (const [lane, specs] of Object.entries(branches))
		out[lane] = specs.map(([id, fields]) => {
			seq += 1;
			return { id, parentId: null, seq, timestamp: seq, ...fields };
		});
	return { branches: out, seq };
}

function contextHost() {
	const state = seed();
	const current = { lane: "main", seq: state.seq, appended: 0, deferred: false, held: [] };
	const lanes = new Map();
	const lane = (name) => {
		if (!lanes.has(name)) {
			state.branches[name] ??= [];
			lanes.set(name, {
				name,
				findEntries: async () => structuredClone(state.branches[name]),
				getTipId: async () => state.branches[name].at(-1)?.id ?? null,
				appendCustomEntry: async (customType, data) => {
					current.seq += 1;
					current.appended += 1;
					const entry = { id: `${name}-x${current.appended}`, parentId: null, seq: current.seq, timestamp: current.seq, type: "custom", customType, ...(data === undefined ? {} : { data: structuredClone(data) }) };
					if (current.deferred) current.held.push([name, entry]);
					else state.branches[name].push(entry);
					return entry.id;
				},
			});
		}
		return lanes.get(name);
	};
	const harness = { lane: async (name) => lane(name), events: { on: () => () => {} }, hooks: { on: () => () => {} } };
	const control = new ContextControl({ harness, rootLane: lane("main"), trimBeforeCompaction: false });
	return {
		modules: [
			{ prefixes: ["ctx."], handle: (request) => control.module.handle({ ...request, caller: { lane: current.lane } }) },
			{
				prefixes: ["test."],
				handle: async (request) => {
					const { payload } = request;
					if (request.type === "test.lane") current.lane = payload.lane;
					if (request.type === "test.defer") current.deferred = payload.on;
					if (request.type === "test.flush") {
						for (const [name, entry] of current.held.splice(0)) state.branches[name].push(entry);
					}
					if (request.type === "test.append") {
						const payload = JSON.parse(request.payload.json);
						current.seq += 1;
						state.branches[payload.lane].push({ id: payload.id, parentId: null, seq: current.seq, timestamp: current.seq, ...payload.entry });
					}
					return { lane: current.lane };
				},
			},
		],
		state: async () => state.branches,
	};
}

const tries = (calls) => `import json as _json\n${calls.map(attempt).join("\n")}`;
const lane = (name) => `await rlm.host_request("test.lane", {"lane": "${name}"})`;
const append = (laneName, id, entry) => `await rlm.host_request("test.append", {"json": ${JSON.stringify(JSON.stringify({ lane: laneName, id, entry }))}})`;
const noAt = (expr) => `import json as _json\n_h = ${expr}\nfor _i in _h["items"]:\n    _i.pop("at", None)\nprint(_json.dumps(_h, sort_keys=True))`;

const scenarios = [
	[
		"history, get, forget, pin, summarize, note, state",
		[
			show("await ctx.history()"),
			tries([
				'ctx.history(limit=3, kinds=["tool", "user"])',
				'ctx.history(kinds="summary")',
				"ctx.history(kinds=[])",
				"ctx.history(limit=0)",
				"ctx.history(limit=201)",
				"ctx.history(limit=2.5)",
				'ctx.history(kinds=["bogus"])',
				'rlm.host_request("ctx.history", {"limit": 5, "kinds": None})',
				'rlm.host_request("ctx.history", {"kinds": "user"})',
				'rlm.host_request("ctx.history", {"bogus": 1})',
			]),
			tries(['ctx.get("r1")', 'ctx.get("a1")', 'ctx.get("cm1")', 'ctx.get("bs1")', 'ctx.get("m0")', 'ctx.get("nope")', 'ctx.get("")']),
			tries([
				'ctx.forget("r1", "too big")',
				'ctx.forget(["r1"], "again")',
				'ctx.forget(["u2"], "x")',
				'ctx.forget(["a3"], "x")',
				'ctx.forget(["bs1", "nope", "u2"], "x")',
				'ctx.forget(["m0"], "x")',
				'ctx.forget([], "x")',
				'ctx.forget(["r2"], "  ")',
				'ctx.forget(["r2", 3], "x")',
				'ctx.forget(["r2"] * 201, "x")',
				'ctx.get("r1")',
			]),
			noAt("await ctx.history()"),
			tries([
				'ctx.pin("a2")',
				'ctx.pin("a2")',
				'ctx.forget(["a2"], "x")',
				'ctx.summarize(["a2"], "x")',
				'ctx.pin("m0")',
				'ctx.pin("nope")',
				'ctx.unpin("cm1")',
				'rlm.host_request("ctx.state", {})',
			]),
			tries([
				'ctx.summarize(["r2"], "the two calls printed numbers")',
				'ctx.summarize(["cm1"], "x" * 8001)',
				'ctx.summarize("cm1", "custom said hello")',
				'ctx.get("a1")',
				'ctx.get("cm1")',
			]),
			noAt("await ctx.history(limit=50)"),
			tries([
				'ctx.note("remember: tests live in test/")',
				'ctx.note("y" * 4001)',
				'ctx.note("")',
				'ctx.history(kinds=["note"])',
				'ctx.get("main-x5")',
				'ctx.forget(["main-x5"], "noise")',
				'ctx.unpin("a2")',
				'ctx.forget(["a2"], "now allowed")',
				'rlm.host_request("ctx.state", {})',
				'rlm.host_request("ctx.state", {"x": 1})',
				'rlm.host_request("ctx.nope", {})',
			]),
		],
	],
	[
		"compaction window, pending entries, lanes",
		[
			tries(['ctx.pin("u1")', 'ctx.note("before compaction")', 'ctx.forget(["r2"], "x")']),
			`${append("main", "cp1", { type: "compaction", summary: "compacted: earlier work", firstKeptEntryId: "u2", tokensBefore: 1000 })}\n${append("main", "u3", { type: "message", message: user("third question") })}\n${append("main", "a4", { type: "message", message: assistant("third answer", ["c4"]) })}\n${append("main", "r4", { type: "message", message: result("c4", "out") })}`,
			tries([
				"ctx.history()",
				'ctx.forget(["u1"], "x")',
				'ctx.forget(["cp1"], "x")',
				'ctx.get("cp1")',
				'ctx.forget(["r4"], "x")',
				'ctx.forget(["u3"], "x")',
				'rlm.host_request("ctx.state", {})',
			]),
			'await rlm.host_request("test.defer", {"on": True})\n' + tries(['ctx.note("while the turn runs")', 'ctx.pin("a4")']),
			noAt("await ctx.history()"),
			tries(['ctx.forget(["a4"], "x")', 'rlm.host_request("ctx.state", {})']),
			'await rlm.host_request("test.flush", {})\nawait rlm.host_request("test.defer", {"on": False})\n' + noAt("await ctx.history()"),
			`${lane("lane-x")}\n${tries(["ctx.history()", 'ctx.note("child note")', 'ctx.forget(["ax"], "x")', 'ctx.get("u1")', 'rlm.host_request("ctx.state", {})'])}`,
			`${lane("main")}\n${tries(['ctx.forget(["a4"], "drop the call")', "ctx.history()"])}`,
		],
	],
];

// Pure helpers over a table.
const entry = (id, fields, seq) => ({ id, parentId: null, seq, timestamp: seq, ...fields });
const base = () => [
	entry("u1", { type: "message", message: user("first") }, 1),
	entry("a1", { type: "message", message: assistant("calling", ["c1", "c2"]) }, 2),
	entry("r1", { type: "message", message: result("c1", "BIG ONE") }, 3),
	entry("r2", { type: "message", message: result("c2", "BIG TWO") }, 4),
	entry("a2", { type: "message", message: assistant("answer") }, 5),
	entry("n1", { type: "custom", customType: "ultron:context-note", data: { text: "a note" } }, 6),
];
const edit = (id, data) => entry(id, { type: "custom", customType: "context_edit", data }, 9);
const applyCases = [
	[],
	[edit("e1", { edits: [{ targetId: "a1", replacement: null }] })],
	[edit("e1", { targetId: "r1", replacement: null })],
	[edit("e1", { edits: [{ targetId: "u1", replacement: null }] }), edit("e2", { edits: [{ targetId: "a1", replacement: { content: "SUMMARY" } }, { targetId: "u1", replacement: { content: "kept after all" } }] })],
	[edit("e1", { edits: [{ targetId: "a1", replacement: { content: [{ type: "text", text: "x" }, { type: "toolCall", id: "c2", name: "rlm", arguments: {} }] } }] })],
	[edit("e1", { edits: [{ targetId: "n1", replacement: null }, { targetId: "r2", replacement: { content: "short" } }] })],
	[edit("e1", { edits: [{ targetId: "", replacement: null }, { targetId: "u1", replacement: 3 }, { targetId: "a2", replacement: { content: 3 } }, "junk"] })],
	[edit("e1", { edits: [{ targetId: "r2", replacement: { content: [{ type: "text", text: "array" }] } }] })],
];

// Collapse on return, trim before compaction, notes and pins kept across compaction, and edit observation: steps
// (data, replayed by tests/context.rs) driving ContextControl.install() over a scripted harness that emits
// `entry_added` for every entry it appends. Each step's output is recorded.
const long = (tag, n) => `${tag} `.repeat(n);
const steps = [
	{ op: "append", lane: "main", entry: { id: "u1", type: "message", message: user("q1") } },
	{ op: "append", lane: "main", entry: { id: "a1", type: "message", message: assistant("run", ["c1"]) } },
	{ op: "event", name: "tool_start", event: { lane: "main", toolName: "rlm", toolCallId: "c1" } },
	{ op: "taskEnded", task: { id: "t1", key: "batch-1", definition: "rlm-child@1", state: "running", result: { status: "completed" } }, cost: 0.0123 },
	{ op: "taskEnded", task: { id: "t2", parentId: "t1", key: "k", definition: "rlm-child@1", state: "completed" }, cost: 1 },
	{ op: "taskEnded", task: { id: "t3", key: "0b4a3c3e-1d2f-4a5b-8c9d-0e1f2a3b4c5d", definition: "reviewer@1", state: "failed" }, cost: null },
	{ op: "event", name: "tool_end", event: { lane: "main", toolCallId: "c1" } },
	{ op: "append", lane: "main", entry: { id: "r1", type: "message", message: result("c1", long("R1", 300)) } },
	{ op: "event", name: "message_end", event: { lane: "main", message: { role: "toolResult" } } },
	{ op: "settle" },
	{ op: "snapshot", lane: "main" },
	{ op: "append", lane: "main", entry: { id: "a2", type: "message", message: assistant("seen it") } },
	{ op: "event", name: "message_end", event: { lane: "main", message: { role: "assistant" } } },
	{ op: "settle" },
	{ op: "snapshot", lane: "main" },
	{ op: "request", lane: "main", type: "ctx.history", payload: { limit: 50 } },
	{ op: "taskEnded", task: { id: "t4", key: "k4", definition: "worker@1", state: "completed" }, cost: 0.5 },
	{ op: "append", lane: "main", entry: { id: "a3", type: "message", message: assistant("again", ["c2"]) } },
	{ op: "event", name: "tool_start", event: { lane: "main", toolName: "rlm", toolCallId: "c2" } },
	{ op: "event", name: "tool_end", event: { lane: "main", toolCallId: "c2" } },
	{ op: "append", lane: "main", entry: { id: "r2", type: "message", message: result("c2", "short") } },
	{ op: "append", lane: "main", entry: { id: "a4", type: "message", message: assistant("ok") } },
	{ op: "event", name: "run_end", event: { lane: "main" } },
	{ op: "settle" },
	{ op: "append", lane: "main", entry: { id: "a5", type: "message", message: assistant("third", ["c5", "c6"]) } },
	{ op: "event", name: "tool_start", event: { lane: "main", toolName: "rlm", toolCallId: "c5" } },
	{ op: "taskEnded", task: { id: "t5", key: "k5", definition: "worker@1", state: "completed" }, cost: 0.25 },
	{ op: "event", name: "tool_end", event: { lane: "main", toolCallId: "c5" } },
	{ op: "event", name: "tool_start", event: { lane: "other", toolName: "rlm", toolCallId: "c6" } },
	{ op: "event", name: "tool_start", event: { lane: "main", toolName: "bash", toolCallId: "c6" } },
	{ op: "taskEnded", task: { id: "t6", key: "k6", definition: "worker@1", state: "completed" }, cost: 0.5 },
	{ op: "append", lane: "main", entry: { id: "r5", type: "message", message: result("c5", long("R5", 400)) } },
	{ op: "append", lane: "main", entry: { id: "r6", type: "message", message: result("c6", long("R6", 400)) } },
	{ op: "request", lane: "main", type: "ctx.pin", payload: { id: "r5" } },
	{ op: "append", lane: "main", entry: { id: "a6", type: "message", message: assistant("done") } },
	{ op: "event", name: "message_end", event: { lane: "main", message: { role: "assistant" } } },
	{ op: "settle" },
	{ op: "snapshot", lane: "main" },
	{ op: "append", lane: "main", entry: { id: "a7", type: "message", message: assistant("read", ["c7", "c8"]) } },
	{ op: "append", lane: "main", entry: { id: "r7", type: "message", message: result("c7", "x".repeat(10000)) } },
	{ op: "append", lane: "main", entry: { id: "r8", type: "message", message: result("c8", "y".repeat(9000)) } },
	{ op: "hook", name: "before_compaction", event: { lane: "main", reason: "threshold", preparation: { tokensBefore: 28000, settings: { reserveTokens: 10000 } } } },
	{ op: "append", lane: "main", entry: { id: "a8", type: "message", message: assistant("noted") } },
	{ op: "hook", name: "before_compaction", event: { lane: "main", reason: "threshold", preparation: { tokensBefore: 20000, settings: { reserveTokens: 10000 } } } },
	{ op: "hook", name: "before_compaction", event: { lane: "main", reason: "manual", preparation: { tokensBefore: 99000, settings: { reserveTokens: 10000 } } } },
	{ op: "hook", name: "before_compaction", event: { lane: "main", reason: "threshold", preparation: { tokensBefore: 29900, settings: { reserveTokens: 10000 } } } },
	{ op: "hook", name: "before_compaction", event: { lane: "main", reason: "threshold", preparation: { tokensBefore: 28000, settings: { reserveTokens: 10000 } } } },
	{ op: "snapshot", lane: "main" },
	{ op: "request", lane: "main", type: "ctx.note", payload: { text: "keep: the port is 8080" } },
	{ op: "request", lane: "main", type: "ctx.note", payload: { text: "dropped note" } },
	{ op: "request", lane: "main", type: "ctx.pin", payload: { id: "u1" } },
	{ op: "request", lane: "main", type: "ctx.pin", payload: { id: "a8" } },
	{ op: "request", lane: "main", type: "ctx.forget", payload: { ids: ["main-y5"], reason: "x" } },
	{ op: "hook", name: "transform_context", event: { lane: "main", messages: [{ role: "compactionSummary", summary: "S" }] } },
	{ op: "append", lane: "main", entry: { id: "cp1", type: "compaction", summary: "S", firstKeptEntryId: "a8", tokensBefore: 1, retainedTail: [assistant("noted")] } },
	{ op: "hook", name: "transform_context", event: { lane: "main", messages: [{ role: "user", content: "x" }] } },
	{ op: "hook", name: "transform_context", event: { lane: "main", messages: [{ role: "user", content: "x" }, { role: "compactionSummary", summary: "S" }, { role: "user", content: "y" }] } },
	{ op: "hook", name: "transform_context", event: { lane: "lane-z", messages: [{ role: "compactionSummary", summary: "S" }] } },
	{ op: "request", lane: "main", type: "ctx.state", payload: {} },
];

async function recordHooks() {
	const branches = { main: [] };
	let seq = 0;
	let appended = 0;
	const listeners = new Map();
	const on = (name, fn) => {
		listeners.set(name, [...(listeners.get(name) ?? []), fn]);
		return () => {};
	};
	const emit = async (name, event) => {
		let result;
		for (const fn of listeners.get(name) ?? []) result = (await fn(event, {})) ?? result;
		return result;
	};
	const push = (name, entry) => {
		branches[name] ??= [];
		branches[name].push(entry);
		void emit("event:entry_added", { lane: name, entry });
	};
	const lanes = new Map();
	const lane = (name) => {
		if (!lanes.has(name))
			lanes.set(name, {
				name,
				findEntries: async () => structuredClone(branches[name] ?? []),
				getTipId: async () => branches[name]?.at(-1)?.id ?? null,
				getModel: async () => ({ contextWindow: 40000 }),
				appendCustomEntry: async (customType, data) => {
					seq += 1;
					appended += 1;
					const id = `${name}-y${appended}`;
					push(name, { id, parentId: null, seq, timestamp: seq, type: "custom", customType, data: structuredClone(data) });
					return id;
				},
			});
		return lanes.get(name);
	};
	const observed = [];
	const harness = {
		lane: async (name) => lane(name),
		events: { on: (name, fn) => on(`event:${name}`, fn) },
		hooks: { on: (name, fn) => on(`hook:${name}`, fn) },
	};
	const control = new ContextControl({ harness, rootLane: lane("main"), observe: (event) => observed.push(event), trimBeforeCompaction: true });
	control.install();
	const outputs = [];
	for (const step of steps) {
		let output = null;
		if (step.op === "append") {
			seq += 1;
			push(step.lane, { id: step.entry.id, parentId: null, seq, timestamp: seq, ...structuredClone(step.entry) });
		} else if (step.op === "event") await emit(`event:${step.name}`, structuredClone(step.event));
		else if (step.op === "taskEnded") control.taskEnded(structuredClone(step.task), { cost: step.cost });
		else if (step.op === "settle") await new Promise((done) => setTimeout(done, 30));
		else if (step.op === "snapshot") output = structuredClone(branches[step.lane]);
		else if (step.op === "request") {
			try {
				output = { ok: await control.module.handle({ type: step.type, payload: step.payload, caller: { lane: step.lane }, context: {} }) };
			} catch (error) {
				output = { error: error.message };
			}
		} else if (step.op === "hook") {
			const value = await emit(`hook:${step.name}`, structuredClone(step.event));
			output = value === undefined ? null : JSON.parse(JSON.stringify(value));
			for (const message of output?.messages ?? []) if (message.customType === "ultron-context-kept") message.timestamp = 0;
		}
		outputs.push(output);
	}
	return { steps, outputs, observed };
}

const helpers = {
	hooks: await recordHooks(),
	seed: seed(),
	taskLines: [
		{ id: "t1", definition: "rlm-child@1", key: "0b4a3c3e-1d2f-4a5b-8c9d-0e1f2a3b4c5d", status: "completed", cost: 0.012345 },
		{ id: "t2", definition: "reviewer@2", key: "pr-12", status: "failed", cost: null },
		{ id: "t3", definition: "x@1", key: "k", status: "cancelled", cost: 0 },
	].map((task) => [task, taskLine(task)]),
	collapsed: [
		["r9", "out ".repeat(300), [{ id: "t1", definition: "d@1", key: "k", status: "completed", cost: 1.5 }]],
		["r8", "é\n\tlong ".repeat(100), Array.from({ length: 10 }, (_, i) => ({ id: `t${i}`, definition: "d@1", key: `k${i}`, status: "completed", cost: null }))],
	].map(([id, text, tasks]) => [id, text, tasks, collapsedCellText(id, text, tasks)]),
	trimmed: [["r7", "abcdefghij".repeat(1000)], ["r6", "é".repeat(5000)]].map(([id, text]) => [id, text, trimmedResultText(id, text)]),
	apply: applyCases.map((edits) => {
		const entries = [...base(), ...edits];
		return [entries, applyContextEdits(entries)];
	}),
};

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
	await record({ context: [contextHost, scenarios] });
	writeFileSync(join(here, "context-helpers.json"), `${JSON.stringify(helpers, null, "\t")}\n`);
}
