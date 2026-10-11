// Generates crates/delegation/fixtures/spawn.json from NativeRlmHost (packages/coding-agent/src/ultron/rlm/native-host.ts)
// with its `externalChild` seam: each spawned child is a scripted run that may call the host from its own lane
// (rlm.finish, nested rlm.spawn), wait on a gate the scenario opens (before or after those calls), then reply with
// text or fail. Records every host
// call's result or error, every child run's inputs, every detached-end (completion) event, and the final journal.
// No workspace, no usage ledger, no worktrees; a fixed clock. Task ids are numbered by first appearance.
// Usage: node crates/delegation/fixtures/generate-spawn.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const { NativeRlmHost } = await import(join(here, "../../../packages/coding-agent/src/ultron/rlm/native-host.ts"));

const passed = { status: "passed", summary: "did it", evidence: ["cargo test: exit 0, 4 passed"], changed_files: [] };
// Child scripts, by spawn order: calls made from the child's lane, optional gate, then reply text or a thrown error.
const scenarios = [
	{ name: "spawn, finish, collect", env: {}, children: [
		{ calls: [["rlm.finish", passed]], text: "done" },
		{ calls: [], text: "plain reply" },
		{ calls: [["rlm.finish", { status: "failed", summary: "  could not  " }]], text: "" },
	], ops: [
		["call", "rlm.spawn", { prompt: "brief one", kwargs: { name: "one" } }], ["settle"],
		["call", "rlm.spawn", { prompt: "brief two", kwargs: { name: "two", model: "faux/m", timeout_ms: 5000, thinking: "low" } }], ["settle"],
		["call", "rlm.spawn", { prompt: "brief three", kwargs: { name: "three" } }],
		["settle"],
		["call", "rlm.collect", {}],
		["call", "rlm.list_subagents", {}],
	]},
	{ name: "spawn validation", env: {}, children: [], ops: [
		["call", "rlm.spawn", { prompt: "", kwargs: { name: "x" } }],
		["call", "rlm.spawn", { prompt: "p", kwargs: {} }],
		["call", "rlm.spawn", { prompt: "p", kwargs: { name: "  " } }],
		["call", "rlm.spawn", { prompt: "p", kwargs: { name: "x", bogus: 1 } }],
		["call", "rlm.spawn", { prompt: "p", extra: 1 }],
		["call", "rlm.spawn", { prompt: "p", kwargs: { name: "x", fork: "yes" } }],
		["call", "rlm.spawn", { prompt: "p", kwargs: { name: "x", fork: true } }],
		["call", "rlm.spawn", { prompt: "p", kwargs: { name: "x", worktree: "maybe" } }],
		["call", "rlm.spawn", { prompt: "p", kwargs: { name: "x", worktree_setup: { link: ["a"] } } }],
		["call", "rlm.spawn", { prompt: "p", kwargs: { name: "x", depth: -1 } }],
		["call", "rlm.spawn", { prompt: "p", kwargs: { name: "x", depth: 1.5 } }],
		["call", "rlm.spawn", { prompt: "p", kwargs: { name: "x", depth: 3 } }],
		["call", "rlm.spawn", { prompt: "p", kwargs: { name: "x", timeout_ms: 0 } }],
		["call", "rlm.spawn", { prompt: "p", kwargs: { name: "x", timeout_ms: 3600001 } }],
		["call", "rlm.spawn", { prompt: "p", kwargs: { name: "x", timeout_ms: 1.5 } }],
		["call", "rlm.spawn", { prompt: "p", kwargs: { name: "x", thinking: "huge" } }],
		["call", "rlm.spawn", { prompt: "p", kwargs: "nope" }],
		["call", "rlm.finish", passed],
		["call", "rlm.collect", { selectors: [""] }],
		["call", "rlm.collect", { other: 1 }],
		["call", "rlm.delete_subagent", { selector: "missing" }],
		["call", "rlm.collect", {}],
	]},
	{ name: "verdict rejections", env: {}, children: [
		{ calls: [["rlm.finish", { status: "passed", summary: "x" }], ["rlm.finish", { status: "maybe", summary: "", extra: 1 }],
			["rlm.finish", { status: "passed", summary: "ok", evidence: ["looks good"] }], ["rlm.finish", passed]], text: "reply" },
		{ calls: [["rlm.finish", { status: "passed", summary: "x" }], ["rlm.finish", passed], ["rlm.finish", { status: "bad" }],
			["rlm.finish", { status: "bad" }], ["rlm.finish", passed]], text: "second" },
		{ calls: [["rlm.finish", passed], ["rlm.finish", { status: "failed", summary: "changed my mind" }]], text: "" },
	], ops: [
		["call", "rlm.spawn", { prompt: "a", kwargs: { name: "a" } }], ["settle"],
		["call", "rlm.spawn", { prompt: "b", kwargs: { name: "b" } }], ["settle"],
		["call", "rlm.spawn", { prompt: "c", kwargs: { name: "c" } }], ["settle"],
		["call", "rlm.collect", {}],
	]},
	{ name: "nesting depth", env: { ULTRON_SPAWN_DEPTH: "3" }, children: [
		{ calls: [["rlm.spawn", { prompt: "grandchild", kwargs: { name: "g" } }]], text: "child0" },
		{ calls: [["rlm.spawn", { prompt: "grandchild", kwargs: { name: "g", depth: 1 } }],
			["rlm.spawn", { prompt: "grandchild", kwargs: { name: "g2" } }]], gate: true, text: "child1" },
		{ calls: [["rlm.spawn", { prompt: "great", kwargs: { name: "gg" } }], ["rlm.list_subagents", {}]], gate: "before", text: "grandchild1" },
		{ calls: [["rlm.collect", {}]], text: "grandchild2" },
		{ calls: [["rlm.spawn", { prompt: "too deep", kwargs: { name: "z" } }]], gate: "before", text: "great" },
	], ops: [
		["call", "rlm.spawn", { prompt: "c0", kwargs: { name: "c0" } }], ["settle"],
		["call", "rlm.spawn", { prompt: "c1", kwargs: { name: "c1", depth: 2 } }],
		["call", "rlm.spawn", { prompt: "deep", kwargs: { name: "d", depth: 3 } }],
		["settle"], ["release", 2], ["settle"], ["release", 4], ["settle"], ["release", 1], ["settle"],
		["call", "rlm.collect", {}],
	]},
	{ name: "depth ceiling zero means none", env: { ULTRON_SPAWN_DEPTH: "0" }, children: [
		{ calls: [], text: "deep ok" },
	], ops: [["call", "rlm.spawn", { prompt: "c", kwargs: { name: "c", depth: 7 } }], ["settle"], ["call", "rlm.collect", {}]] },
	{ name: "awaited vs detached, failure, delete", env: {}, children: [
		{ calls: [], gate: true, text: "waited for" },
		{ calls: [], gate: true, throws: "model exploded" },
		{ calls: [], gate: true, text: "never" },
		{ calls: [], text: "" },
	], ops: [
		["call", "rlm.spawn", { prompt: "w", kwargs: { name: "w" } }],
		["call", "rlm.spawn", { prompt: "f", kwargs: { name: "f" } }],
		["call", "rlm.spawn", { prompt: "d", kwargs: { name: "d" } }],
		["settle"],
		["async", "rlm.collect", { selectors: ["$0"] }], ["settle"],
		["release", 0], ["join", 0],
		["release", 1], ["settle"],
		["call", "rlm.delete_subagent", { selector: "$2" }],
		["call", "rlm.delete_subagent", { selector: "$2" }],
		["release", 2], ["settle"],
		["call", "rlm.spawn", { prompt: "empty", kwargs: { name: "e" } }], ["settle"],
		["call", "rlm.collect", {}],
	]},
	{ name: "timeout", env: {}, children: [{ calls: [], gate: true, text: "late" }], ops: [
		["call", "rlm.spawn", { prompt: "slow", kwargs: { name: "s", timeout_ms: 30 } }], ["sleep", 120], ["call", "rlm.collect", {}],
	]},
];

const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));
const settle = async () => { for (let n = 0; n < 20; n++) await new Promise((r) => setImmediate(r)); };
const results = [];
for (const s of scenarios) {
	for (const key of ["ULTRON_SPAWN_DEPTH"]) delete process.env[key];
	Object.assign(process.env, s.env);
	let value;
	const store = { read: async () => clone(value) ?? undefined, write: async (d) => void (value = clone(d)) };
	const events = [];
	const runs = [];
	const gates = s.children.map(() => { let open; const p = new Promise((r) => (open = r)); return { p, open }; });
	let host;
	let spawned = 0;
	host = new NativeRlmHost({ lane: async () => { throw new Error("no harness lanes in this fixture"); } }, {}, {
		store,
		// A fixed clock: every run overlaps every other in `check.concurrent`, independent of timing.
		now: () => 1000,
		externalChild: async (run) => {
			const index = spawned++;
			const script = s.children[index];
			const record = { index, taskId: run.taskId, laneName: run.laneName, prompt: run.prompt, level: run.level,
				allowance: run.allowance, model: run.model ?? null, timeoutMs: run.timeoutMs, calls: [] };
			runs.push(record);
			// gate "before": wait for the scenario to open the gate before making any call; true: after the calls.
			if (script.gate === "before") await gates[index].p;
			for (const [type, payload] of script.calls) {
				try { record.calls.push({ type, result: clone(await host.handle(type, clone(payload), {}, { lane: run.laneName })) }); }
				catch (e) { record.calls.push({ type, error: e.message }); }
			}
			if (script.gate === true) await gates[index].p;
			if (script.throws) throw new Error(script.throws);
			return { text: script.text, turns: 1, toolCalls: 0 };
		},
		onDetachedEnd: (end) => events.push({ task: clone(end.task), kind: end.kind, ownerLane: end.ownerLane, awaited: end.awaited, fetch: end.fetch }),
	});
	const steps = [];
	const pending = [];
	const ids = [];
	const resolveArgs = (payload) => JSON.parse(JSON.stringify(payload).replace(/"\$(\d+)"/g, (_, n) => JSON.stringify(ids[Number(n)])));
	const record = async (op, promise) => {
		try { const r = clone(await promise); if (op[1] === "rlm.spawn") ids.push(r.rlm_child_id); steps.push({ op, result: r }); }
		catch (e) { steps.push({ op, error: e.message }); }
	};
	for (const op of s.ops) {
		if (op[0] === "call") await record(op, host.handle(op[1], resolveArgs(op[2]), {}));
		else if (op[0] === "async") pending.push(host.handle(op[1], resolveArgs(op[2]), {}));
		else if (op[0] === "join") await record(op, pending[op[1]]);
		else if (op[0] === "release") gates[op[1]].open();
		else if (op[0] === "sleep") await new Promise((r) => setTimeout(r, op[1]));
		else await settle();
		if (op[0] !== "call" && op[0] !== "join") steps.push({ op });
	}
	gates.forEach((g) => g.open());
	await settle();
	await host.close();
	results.push({ name: s.name, env: s.env, children: s.children, steps, runs, events, journal: clone(value) });
}
const seen = new Map();
const text = JSON.stringify(results, null, "\t").replace(
	/ultron-task-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g,
	(id) => { if (!seen.has(id)) seen.set(id, `ultron-task-#${seen.size}`); return seen.get(id); },
);
// Idempotency keys are random UUIDs too: numbered the same way.
const keys = new Map();
const keyed = text.replace(/"key": "([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})"/g, (_, key) => {
	if (!keys.has(key)) keys.set(key, `key-#${keys.size}`);
	return `"key": "${keys.get(key)}"`;
});
writeFileSync(join(here, "spawn.json"), `${keyed}\n`);
