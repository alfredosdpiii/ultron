// Generates crates/harness/fixtures/reducer.json from the TypeScript lane snapshot reducer
// (packages/agent/src/harness/runtime/reducer.ts). Two kinds of cases:
// - "lane": a real AgentHarness lane over StorageBackedSession + MemoryStorage with the faux provider (the
//   scenarios of packages/agent/test/harness/runtime/reducer.test.ts). Records the watch snapshot, every event
//   delivered to the watcher, and the authoritative resnapshot after the operation; Rust folds the events.
// - "synthetic": hand-written event sequences covering every reducer branch. Records the reduction and the
//   snapshot after each event.
// Date.now, Math.random and randomUUID are pinned and random UUIDv7 IDs renamed, so the output is deterministic.
// Usage: node crates/harness/fixtures/generate-reducer.mjs
import crypto from "node:crypto";
import { readdirSync, readFileSync, writeFileSync } from "node:fs";
import { registerHooks } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");

// Workspace packages have no dist/ here: resolve each @ultron/* export to the TypeScript source it is built from.
const packages = new Map();
for (const dir of readdirSync(join(root, "packages"))) {
	try {
		const manifest = JSON.parse(readFileSync(join(root, "packages", dir, "package.json"), "utf8"));
		packages.set(manifest.name, { dir, exports: manifest.exports ?? {} });
	} catch {}
}
registerHooks({
	resolve(specifier, context, next) {
		const match = /^(@ultron\/[^/]+)(\/.*)?$/.exec(specifier);
		const pkg = match && packages.get(match[1]);
		if (!pkg) return next(specifier, context);
		const sub = `.${match[2] ?? ""}`;
		let target;
		for (const [key, value] of Object.entries(pkg.exports)) {
			const built = typeof value === "string" ? value : value.import;
			if (key === sub) target = built;
			else if (key.endsWith("*") && sub.startsWith(key.slice(0, -1)))
				target = built.replace("*", sub.slice(key.length - 1));
		}
		if (!target) return next(specifier, context);
		const source = target.replace(/^\.\/dist\//, "./src/").replace(/\.js$/, ".ts");
		return { url: pathToFileURL(join(root, "packages", pkg.dir, source)).href, shortCircuit: true };
	},
});

let clock = 1_700_000_000_000;
Date.now = () => clock++;
let seed = 1;
Math.random = () => {
	seed = (seed * 48271) % 2147483647;
	return seed / 2147483647;
};
let uuid = 0;
crypto.randomUUID = () => `00000000-0000-4000-8000-${String(uuid++).padStart(12, "0")}`;

const { createModels, fauxAssistantMessage, fauxProvider } = await import("@ultron/ai");
const agent = (path) => import(pathToFileURL(join(root, "packages/agent/src/harness", path)).href);
const { AgentHarness } = await agent("agent-harness.ts");
const { BACKGROUND_CONTEXT } = await agent("context.ts");
const { reduceLaneSnapshot } = await agent("runtime/reducer.ts");
const { MemoryStorage } = await agent("session/memory.ts");
const { StorageBackedSession } = await agent("session/session.ts");

const json = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));
const settle = () => new Promise((done) => setTimeout(done, 0));
let sessions = 0;

async function fixture(options = {}) {
	const session = new StorageBackedSession(
		{ id: `reducer-${sessions++}`, createdAt: 1, storageVersion: 1 },
		new MemoryStorage(),
	);
	const faux = fauxProvider();
	const models = createModels();
	models.setProvider(faux.provider);
	const { harness } = await AgentHarness.create(
		{
			session,
			models,
			model: faux.getModel(),
			...(options.deferred === true ? { streamOptions: { deferred: true } } : {}),
		},
		BACKGROUND_CONTEXT,
	);
	const lane = await harness.lane("main", BACKGROUND_CONTEXT);
	return { session, lane, faux };
}

/** Watch `lane` across `act`, recording the snapshot, the events and the authoritative resnapshot. */
async function watched(lane, act) {
	const watch = await lane.watch(BACKGROUND_CONTEXT);
	const events = [];
	watch.start((event) => events.push(json(event)));
	const phases = [];
	let start = json(watch.snapshot);
	for (const step of act) {
		events.length = 0;
		const outcome = await step();
		await settle();
		const snapshot = json(await watch.resnapshot(BACKGROUND_CONTEXT));
		// TypeScript's own fold, as reducer.test.ts checks it against the resnapshot.
		const folded = structuredClone(start);
		const reductions = events.map((event) => reduceLaneSnapshot(folded, event) ?? null);
		phases.push({ start, events: [...events], reductions, folded: json(folded), resnapshot: snapshot, outcome: json(outcome) });
		start = json(folded);
	}
	watch.unsubscribe();
	return phases;
}

const lane = {};
{
	const { session, lane: main, faux } = await fixture();
	faux.setResponses([fauxAssistantMessage("answer")]);
	lane.ordinary_run = await watched(main, [() => main.prompt("question", undefined, BACKGROUND_CONTEXT)]);
	await session.close(BACKGROUND_CONTEXT);
}
{
	const { session, lane: main, faux } = await fixture({ deferred: true });
	faux.setResponses([fauxAssistantMessage("answer")]);
	lane.suspend_resume = await watched(main, [
		() => main.prompt("question", undefined, BACKGROUND_CONTEXT),
		() => main.resume(BACKGROUND_CONTEXT),
	]);
	await session.close(BACKGROUND_CONTEXT);
}
{
	const { session, lane: main, faux } = await fixture();
	await main.appendMessage({ role: "user", content: "history", timestamp: 1 }, BACKGROUND_CONTEXT);
	faux.setResponses([fauxAssistantMessage("summary")]);
	lane.standalone_compaction = await watched(main, [() => main.compact(undefined, BACKGROUND_CONTEXT)]);
	await session.close(BACKGROUND_CONTEXT);
}
{
	const { session, lane: main } = await fixture();
	lane.queue_changes = await watched(main, [
		async () => {
			await main.nextRun("next", undefined, BACKGROUND_CONTEXT);
			const steer = await main.steer("steer", undefined, BACKGROUND_CONTEXT);
			await main.followUp("follow", undefined, BACKGROUND_CONTEXT);
			await main.cancelQueued(steer.value.entryId, BACKGROUND_CONTEXT);
		},
	]);
	await session.close(BACKGROUND_CONTEXT);
}
{
	const { session, lane: main, faux } = await fixture();
	faux.setResponses([fauxAssistantMessage("first"), fauxAssistantMessage("second")]);
	lane.two_runs_and_config = await watched(main, [
		() => main.prompt("one", undefined, BACKGROUND_CONTEXT),
		async () => {
			await main.setThinkingLevel("high", BACKGROUND_CONTEXT);
			await main.setActiveTools([], BACKGROUND_CONTEXT);
			return main.prompt("two", undefined, BACKGROUND_CONTEXT);
		},
	]);
	await session.close(BACKGROUND_CONTEXT);
}

// --- synthetic sequences --------------------------------------------------------------------------------------
const base = (overrides = {}) => ({
	lane: "main",
	tipId: null,
	transcript: [],
	configuration: { model: { provider: "p", modelId: "m" }, thinkingLevel: "off", activeToolNames: ["a"] },
	queues: [],
	operation: null,
	stats: { messageCount: 0, usage: { input: 0 } },
	faulted: false,
	...overrides,
});
const open = (kind, id = "run") => ({ id, kind, startedAt: 1, fromTipId: null, status: "open", runningTools: [] });
const result = (text) => ({ content: [{ type: "text", text }], details: { text } });
const toolStart = (n, runId = "run", lane = "main") => ({
	type: "tool_start", lane, runId, turnId: "turn", toolCallId: `call-${n}`, toolName: `tool-${n}`, args: { index: n },
});
const toolEnd = (n, runId = "run", isError = false) => ({
	type: "tool_end", lane: "main", runId, turnId: "turn", toolCallId: `call-${n}`, toolName: `tool-${n}`,
	result: result(`done-${n}`), isError, terminate: false,
});
const toolUpdate = (n, runId, text) => ({
	type: "tool_update", lane: "main", runId, turnId: "turn", toolCallId: `call-${n}`, toolName: `tool-${n}`,
	partialResult: result(text),
});
const toolEntry = (n) => ({
	type: "entry_added", lane: "main",
	entry: {
		id: `result-${n}`, parentId: n === 0 ? null : `result-${n - 1}`, seq: n + 1, timestamp: n + 1, type: "message",
		message: { role: "toolResult", toolCallId: `call-${n}`, toolName: `tool-${n}`, content: [{ type: "text", text: `done-${n}` }], isError: false, timestamp: n + 1 },
	},
});
const assistant = (stopReason, text = "hi") => ({ role: "assistant", content: [{ type: "text", text }], stopReason, timestamp: 5 });

const synthetic = {
	in_run_compaction: {
		snapshot: base({ operation: open("run") }),
		events: [
			{ type: "compaction_start", lane: "main", runId: "run", reason: "threshold", startedAt: 2 },
			{ type: "compaction_end", lane: "main", runId: "run", reason: "threshold", status: "declined", endedAt: 3 },
		],
	},
	parallel_tools: {
		snapshot: base({ operation: open("run") }),
		events: [
			toolStart(0), toolStart(1), toolStart(2), toolStart(0), toolUpdate(1, "stale", "stale"), toolUpdate(1, "run", "partial"),
			toolEnd(2), toolUpdate(2, "run", "after-end"), toolEnd(0), toolEntry(0), toolEnd(1, "run", true), toolEntry(1), toolEntry(2),
			toolEnd(3), toolStart(4, "other"), toolStart(5, "run", "side"),
		],
	},
	navigation: {
		snapshot: base(),
		events: [
			{ type: "navigation_start", lane: "main", runId: "nav", startedAt: 4 },
			{ type: "operation_abort", lane: "main", operationId: "other" },
			{ type: "operation_abort", lane: "main", operationId: "nav" },
			{ type: "navigation_end", lane: "main", runId: "nav", status: "completed", fromTipId: null, tipId: "target", endedAt: 5 },
		],
	},
	run_lifecycle: {
		snapshot: base({ tipId: "t0" }),
		events: [
			{ type: "run_start", lane: "main", runId: "r1", startedAt: 10 },
			{ type: "retry_scheduled", lane: "main", runId: "r1", attempt: 1, maxAttempts: 3, notBefore: 99 },
			{ type: "retry_scheduled", lane: "main", runId: "zz", attempt: 2, maxAttempts: 3, notBefore: 100 },
			{ type: "retry_start", lane: "main", runId: "r1" },
			{ type: "retry_scheduled", lane: "main", runId: "r1", attempt: 2, maxAttempts: 3, notBefore: 101 },
			{ type: "retry_end", lane: "main", runId: "r1" },
			{ type: "message_start", lane: "main", runId: "r1", message: assistant("pending") },
			{ type: "message_update", lane: "main", runId: "r1", message: assistant("pending", "hi there") },
			{ type: "message_update", lane: "main", runId: "r1", message: { role: "user", content: "x", timestamp: 1 } },
			{ type: "run_suspend", lane: "main", runId: "r1", deferred: { id: "d" }, poll: 2 },
			{ type: "run_resume", lane: "main", runId: "r1" },
			{ type: "message_start", lane: "main", runId: "r1", message: assistant("stop") },
			{ type: "message_start", lane: "main", message: assistant("pending") },
			{ type: "message_start", lane: "main", runId: "r1", message: { role: "user", content: "u", stopReason: "pending", timestamp: 1 } },
			{ type: "message_end", lane: "main", message: assistant("stop") },
			{ type: "message_end", lane: "main", runId: "r1", message: assistant("stop") },
			{ type: "entry_added", lane: "main", entry: { id: "e1", parentId: "t0", seq: 1, timestamp: 1, type: "message", message: assistant("stop") } },
			{ type: "entry_added", lane: "main", entry: { id: "c1", parentId: "e1", seq: 2, timestamp: 2, type: "custom", customType: "x" } },
			{ type: "compaction_start", lane: "main", runId: "r1", reason: "threshold", startedAt: 11 },
			{ type: "run_end", lane: "main", runId: "other", status: "completed", fromTipId: "t0", tipId: "zz", endedAt: 12 },
			{ type: "run_end", lane: "main", runId: "r1", status: "failed", error: "boom", fromTipId: "t0", tipId: "c1", endedAt: 13 },
			{ type: "run_start", lane: "main", runId: "r2", startedAt: 14 },
			{ type: "run_end", lane: "main", runId: "r2", status: "completed", error: "ignored", fromTipId: "c1", tipId: "c9", endedAt: 15 },
		],
	},
	compaction_lifecycle: {
		snapshot: base({ tipId: "t0", transcript: [{ id: "t0", parentId: null, seq: 1, timestamp: 1, type: "message", message: { role: "user", content: "x", timestamp: 1 } }] }),
		events: [
			{ type: "compaction_start", lane: "main", runId: "k1", reason: "manual", startedAt: 20 },
			{ type: "compaction_start", lane: "main", runId: "k2", reason: "manual", startedAt: 21 },
			{ type: "entry_added", lane: "main", entry: { id: "s1", parentId: "t0", seq: 2, timestamp: 2, type: "compaction", summary: "sum" } },
			{ type: "run_end", lane: "main", runId: "k1", status: "completed", fromTipId: "t0", tipId: "zz", endedAt: 22 },
			{ type: "compaction_end", lane: "main", runId: "k1", reason: "manual", status: "failed", error: "nope", endedAt: 23 },
			{ type: "compaction_start", lane: "main", runId: "k3", reason: "manual", startedAt: 24 },
			{ type: "compaction_end", lane: "main", runId: "k3", reason: "manual", status: "completed", error: "dropped", endedAt: 25 },
		],
	},
	lanes_config_and_misc: {
		snapshot: base({ operation: open("run") }),
		events: [
			{ type: "run_start", lane: "side", runId: "x", startedAt: 1 },
			{ type: "usage", lane: "side", totals: { input: 7 } },
			{ type: "usage", totals: { input: 9 } },
			{ type: "queue_update", lane: "main", queues: [{ kind: "steer", entryId: "q1" }] },
			{ type: "queue_update", lane: "side", queues: [] },
			{ type: "config_update", lane: "main", property: "model", value: { provider: "q", modelId: "n" } },
			{ type: "config_update", lane: "main", property: "thinkingLevel", value: "high" },
			{ type: "config_update", lane: "main", property: "activeTools", value: ["b", "c"] },
			{ type: "config_update", lane: "main", property: "unknown", value: 1 },
			{ type: "config_update", lane: "side", property: "thinkingLevel", value: "low" },
			{ type: "config_update", property: "thinkingLevel", value: "minimal" },
			{ type: "fault", lane: "main", error: "x" },
			{ type: "handler_error", lane: "main" },
			{ type: "turn_start", lane: "main", runId: "run" },
			{ type: "turn_end", lane: "main", runId: "run" },
			{ type: "value_update", lane: "main" },
			{ type: "lane_created", lane: "main" },
			{ type: "entry_added", entry: { id: "free", parentId: null, seq: 1, timestamp: 1, type: "message", message: { role: "toolResult", toolCallId: "none", toolName: "t", content: [], isError: false, timestamp: 1 } } },
			{ type: "run_start", runId: "lane-less", startedAt: 30 },
		],
	},
};

const out = { source: "packages/agent/src/harness/runtime/reducer.ts", lane, synthetic: {} };
for (const [name, { snapshot, events }] of Object.entries(synthetic)) {
	const state = structuredClone(snapshot);
	const steps = events.map((event) => {
		const reduction = reduceLaneSnapshot(state, event) ?? null;
		return { event, reduction, snapshot: json(state) };
	});
	out.synthetic[name] = { snapshot, steps };
}
// Run, turn and entry IDs are random UUIDv7s: rename each to a fixed ID in order of first appearance.
const renamed = new Map();
const text = JSON.stringify(out, null, "\t").replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g, (id) => {
	if (!renamed.has(id)) renamed.set(id, `00000000-0000-7000-8000-${String(renamed.size).padStart(12, "0")}`);
	return renamed.get(id);
});
writeFileSync(join(here, "reducer.json"), `${text}\n`);
