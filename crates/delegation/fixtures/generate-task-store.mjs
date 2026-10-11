// Generates crates/delegation/fixtures/task-store.json from packages/coding-agent/src/ultron/rlm/task-store.ts.
// Each scenario runs a list of journal operations on an in-memory store and records every result or error and the
// persisted document after each operation. Task ids are random in both implementations; they are recorded as is and
// the Rust replay maps them by order of appearance.
// Usage: node crates/delegation/fixtures/generate-task-store.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
const here = dirname(fileURLToPath(import.meta.url));
const { NativeTaskJournal, taskFingerprint, persistedTasks } = await import(
	join(here, "../../../packages/coding-agent/src/ultron/rlm/task-store.ts")
);

const F = "a".repeat(64);
const ok = (value = 42) => ({ status: "succeeded", value, verification: "unverified" });
const task = (o = {}) => ({ id: "task-1", key: "request-1", fingerprint: F, definition: "identity@1", state: "completed", result: ok(), ...o });

const scenarios = [
	{ name: "lifecycle", initial: null, ops: [
		["admit", "identity@1", F, "k1"], ["admit", "identity@1", F, "k1"], ["admit", "identity@1", "b".repeat(64), "k1"],
		["transition", "$0", "running"], ["transition", "$0", "running"],
		["transition", "$0", "completed", { status: "failed", error: "x", verification: "unverified" }],
		["transition", "$0", "completed", ok(7)], ["transition", "$0", "failed", { status: "failed", verification: "unverified" }],
		["admit", "child@2", F, "k2", "$0"], ["transition", "$1", "cancelled", { status: "cancelled", error: "stop", verification: "unverified" }],
		["admit", "x@1", F, "k3"], ["transition", "$2", "completed", ok()], ["transition", "$2", "bogus"], ["transition", "", "running"],
		["transition", "missing", "running"], ["transition", "$2", "running", ok()], ["transition", "$2", "running"],
		["transition", "$2", "failed"], ["transition", "$2", "failed", { status: "failed", verification: "verified" }],
		["transition", "$2", "failed", { status: "failed", verification: "unverified", extra: 1 }],
		["transition", "$2", "failed", { status: "failed", verification: "unverified", verdict: { status: "passed" }, check: { outcome: "verified" }, unverified: true, worktree: null }],
		["list"],
	]},
	{ name: "admission validation", initial: null, ops: [
		["admit", "Identity@1", F, "k"], ["admit", "identity", F, "k"], ["admit", "identity@1", "A".repeat(64), "k"],
		["admit", "identity@1", "a".repeat(63), "k"], ["admit", "identity@1", F, ""], ["admit", "a-b2@10", F, "k"], ["list"],
	]},
	{ name: "recovery interrupts unfinished", initial: { version: 1, tasks: [
		task(), task({ id: "t2", key: "r2", state: "running", result: undefined }), task({ id: "t3", key: "r3", state: "admitted", result: undefined, parentId: "t2" })] },
		ops: [["list"], ["transition", "t2", "running"]] },
	{ name: "no version reads as v1", initial: { tasks: [task()] }, ops: [["list"]] },
	{ name: "unsupported version", initial: { version: 2, tasks: [] }, ops: [["list"], ["admit", "identity@1", F, "k"]] },
	{ name: "duplicate id", initial: { version: 1, tasks: [task(), task({ key: "other" })] }, ops: [["list"]] },
	{ name: "duplicate key", initial: { version: 1, tasks: [task(), task({ id: "other" })] }, ops: [["list"]] },
	{ name: "running with result", initial: { version: 1, tasks: [task({ state: "running" })] }, ops: [["list"]] },
	{ name: "completed without result", initial: { version: 1, tasks: [task({ result: undefined })] }, ops: [["list"]] },
	{ name: "state result mismatch", initial: { version: 1, tasks: [task({ state: "failed" })] }, ops: [["list"]] },
	{ name: "bad definition", initial: { version: 1, tasks: [task({ definition: "nope" })] }, ops: [["list"]] },
	{ name: "extra field", initial: { version: 1, tasks: [task({ extra: true })] }, ops: [["list"]] },
	{ name: "extra document field", initial: { version: 1, tasks: [], more: 1 }, ops: [["list"]] },
	{ name: "not an object", initial: [1, 2], ops: [["list"]] },
	{ name: "bad result shape", initial: { version: 1, tasks: [task({ result: { status: "succeeded", verification: "verified" } })] }, ops: [["list"]] },
	{ name: "write failure poisons", initial: null, failWriteAt: 2, ops: [
		["admit", "identity@1", F, "k1"], ["transition", "$0", "running"], ["list"], ["admit", "identity@1", F, "k2"]] },
	{ name: "read failure", initial: null, failRead: true, ops: [["list"], ["list"]] },
	{ name: "persist bound", initial: null, bulk: 130, ops: [["list"]] },
];

function clone(v) { return v === undefined ? null : JSON.parse(JSON.stringify(v)); }
const out = [];
for (const s of scenarios) {
	let value = s.initial === null ? undefined : clone(s.initial);
	let writes = 0;
	const store = {
		read: async () => { if (s.failRead) throw new Error("disk"); return clone(value) ?? undefined; },
		write: async (doc) => { writes += 1; if (s.failWriteAt === writes) throw new Error("disk"); value = clone(doc); },
	};
	const journal = new NativeTaskJournal(store);
	const ids = [];
	const steps = [];
	if (s.bulk) {
		for (let n = 0; n < s.bulk; n++) {
			const { task: t } = await journal.admit("identity@1", F, `request-${n}`);
			await journal.transition(t.id, "running");
			if (n < s.bulk - 1) await journal.transition(t.id, "completed", ok(n));
		}
	}
	for (const op of s.ops) {
		const args = op.slice(1).map((a) => (typeof a === "string" && /^\$\d+$/.test(a) ? ids[Number(a.slice(1))] : a));
		let result, error;
		try {
			if (op[0] === "admit") { result = await journal.admit(args[0], args[1], args[2], undefined, args[3]); if (result.created) ids.push(result.task.id); }
			else if (op[0] === "transition") result = await journal.transition(...args);
			else result = await journal.list();
		} catch (e) { error = e.message; }
		steps.push({ op, ...(error === undefined ? { result: clone(result) } : { error }), stored: clone(value) });
	}
	out.push({ name: s.name, initial: clone(s.initial), failWriteAt: s.failWriteAt ?? null, failRead: s.failRead ?? false, bulk: s.bulk ?? 0, steps });
}
const fingerprints = [
	{ b: 2, a: [true, null] }, { a: [true, null], b: 2 }, "text", 1, 1.5, -0, 1e21, null, [], {}, { "é": " \n\"" },
	{ nested: { z: 1, y: [{ b: 1, a: 2 }] } },
].map((input) => ({ input, hash: taskFingerprint(input) }));
// Random ids become ultron-task-#N by order of first appearance (the Rust replay normalizes the same way).
const seen = new Map();
const text = JSON.stringify({ scenarios: out, fingerprints }, null, "\t").replace(
	/ultron-task-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g,
	(id) => { if (!seen.has(id)) seen.set(id, `ultron-task-#${seen.size}`); return seen.get(id); },
);
writeFileSync(join(here, "task-store.json"), `${text}\n`);
