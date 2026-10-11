// Generates crates/harness/fixtures/task-store.json from the TypeScript task journal
// (packages/coding-agent/src/ultron/rlm/task-store.ts in this checkout). Each scenario is a list of data-only
// operations run against NativeTaskJournal over an in-memory store with injectable read/write failures; the
// fixture records every outcome (value or error message) and the stored document after it. Task IDs are random
// in TypeScript, so the fixture records the minted IDs (renamed to fixed UUIDs) and operations name them as "$n"; Rust replays them.
// Usage: node crates/harness/fixtures/generate.mjs
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const { NativeTaskJournal, taskFingerprint } = await import(
	pathToFileURL(join(root, "packages/coding-agent/src/ultron/rlm/task-store.ts")).href
);

const fp = "a".repeat(64);
const fp2 = "b".repeat(64);
const ok = (status, extra = {}) => ({ status, verification: "unverified", ...extra });
const task = (overrides = {}) => ({
	id: "task-1",
	key: "request-1",
	fingerprint: fp,
	definition: "identity@1",
	state: "completed",
	result: ok("succeeded", { value: 42 }),
	...overrides,
});
const without = (object, key) => {
	const copy = { ...object };
	delete copy[key];
	return copy;
};

const fingerprintInputs = [
	null, true, 0, -1, 1.5, 1e20, 1.2345e20, 1e21, 1e-7, 5e-324, 2 ** 53, "", "a\u0000\u001f\b\n\"\\/\u007f é😀",
	[], {}, [1, [2, [3]]], { b: 2, a: [true, null] }, { a: [true, null], b: 2 },
	{ "｡": 1, "😀": 2, z: 3, A: 4, "": 5 }, { definition: "identity@1", input: { answer: 7 } },
];

const invalidDocuments = [
	{ version: 1, tasks: [task(), task({ key: "request-2" })] },
	{ version: 1, tasks: [task(), task({ id: "task-2" })] },
	{ version: 1, tasks: [without(task({ state: "admitted" }), "result")] },
	{ version: 1, tasks: [task({ state: "running", result: ok("succeeded") })] },
	{ version: 1, tasks: [without(task(), "result")] },
	{ version: 1, tasks: [task({ result: ok("failed") })] },
	{ version: 1, tasks: [task()], extra: true },
	{ version: 1, tasks: [task({ definition: "Identity@1" })] },
	{ version: 1, tasks: [task({ definition: "identity@" })] },
	{ version: 1, tasks: [task({ fingerprint: "A".repeat(64) })] },
	{ version: 1, tasks: [task({ id: "" })] },
	{ version: 1, tasks: [task({ parentId: "" })] },
	{ version: 1, tasks: [task({ extra: 1 })] },
	{ version: 1, tasks: [task({ state: "done" })] },
	{ version: 1, tasks: [task({ result: ok("succeeded", { unverified: false }) })] },
	{ version: 1, tasks: [task({ result: ok("succeeded", { error: 3 }) })] },
	{ version: 1, tasks: [task({ result: { status: "succeeded", verification: "verified" } })] },
	{ version: 1, tasks: [task({ result: ok("succeeded", { other: 1 }) })] },
	{ version: 1 },
	{ version: 1, tasks: {} },
	{ version: 2, tasks: [] },
	{ version: "1", tasks: [] },
	{ version: null, tasks: [] },
	[],
	"tasks",
	7,
];

const scenarios = {
	lifecycle: [
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "request-1" },
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "request-1" },
		{ op: "admit", definition: "other@2", fingerprint: fp2, key: "request-1" },
		{ op: "admit", definition: "child-task@12", fingerprint: fp2, key: "request-2", parentId: "$0" },
		{ op: "transition", id: "$0", state: "running" },
		{ op: "transition", id: "$0", state: "completed", result: ok("succeeded", { value: { n: [1, 2] }, verdict: null, check: { ok: true }, unverified: true, worktree: "w" }) },
		{ op: "transition", id: "$0", state: "failed", result: ok("failed", { error: "late" }) },
		{ op: "transition", id: "$1", state: "running" },
		{ op: "transition", id: "$1", state: "failed", result: ok("failed", { error: "boom" }) },
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "request-1" },
		{ op: "list" },
		{ op: "ready" },
	],
	invalid_requests: [
		{ op: "admit", definition: "bad definition", fingerprint: fp, key: "request-1" },
		{ op: "admit", definition: "identity", fingerprint: fp, key: "request-1" },
		{ op: "admit", definition: "1dentity@1", fingerprint: fp, key: "request-1" },
		{ op: "admit", definition: "identity@1", fingerprint: "not-a-fingerprint", key: "request-1" },
		{ op: "admit", definition: "identity@1", fingerprint: "A".repeat(64), key: "request-1" },
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "" },
		{ op: "list" },
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "request-1" },
		{ op: "transition", id: "", state: "running" },
		{ op: "transition", id: "missing", state: "running" },
		{ op: "transition", id: "$0", state: "completed", result: ok("succeeded") },
		{ op: "transition", id: "$0", state: "running", result: ok("succeeded") },
		{ op: "transition", id: "$0", state: "interrupted", result: ok("interrupted") },
		{ op: "transition", id: "$0", state: "running" },
		{ op: "transition", id: "$0", state: "running" },
		{ op: "transition", id: "$0", state: "admitted" },
		{ op: "transition", id: "$0", state: "completed" },
		{ op: "transition", id: "$0", state: "completed", result: ok("failed") },
		{ op: "transition", id: "$0", state: "completed", result: ok("succeeded", { extra: 1 }) },
		{ op: "transition", id: "$0", state: "completed", result: { status: "succeeded", verification: "verified" } },
		{ op: "transition", id: "$0", state: "completed", result: ok("succeeded", { unverified: false }) },
		{ op: "transition", id: "$0", state: "failed", result: ok("failed", { error: 1 }) },
		{ op: "transition", id: "$0", state: "completed", result: "succeeded" },
		{ op: "list" },
	],
	cancellation: [
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "request-1" },
		{ op: "transition", id: "$0", state: "cancelled", result: ok("cancelled") },
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "request-2" },
		{ op: "transition", id: "$1", state: "running" },
		{ op: "transition", id: "$1", state: "cancelled", result: ok("cancelled") },
		{ op: "transition", id: "$1", state: "completed", result: ok("succeeded", { value: 1 }) },
		{ op: "list" },
	],
	restart_recovery: [
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "request-1" },
		{ op: "transition", id: "$0", state: "running" },
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "request-2", parentId: "$0" },
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "request-3" },
		{ op: "transition", id: "$2", state: "running" },
		{ op: "transition", id: "$2", state: "completed", result: ok("succeeded", { value: "done" }) },
		{ op: "open" },
		{ op: "list" },
		{ op: "transition", id: "$0", state: "completed", result: ok("succeeded") },
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "request-2" },
		{ op: "open" },
		{ op: "ready" },
	],
	seeded_without_version: [
		{ op: "seed", value: { tasks: [without(task({ state: "running" }), "result")] } },
		{ op: "open" },
		{ op: "list" },
	],
	seeded_finished_is_not_rewritten: [
		{ op: "seed", value: { version: 1, tasks: [task({ parentId: "p" })] } },
		{ op: "open" },
		{ op: "list" },
	],
	write_failure_poisons: [
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "request-1" },
		{ op: "failWrites", on: true },
		{ op: "transition", id: "$0", state: "running" },
		{ op: "failWrites", on: false },
		{ op: "list" },
		{ op: "ready" },
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "request-2" },
		{ op: "open" },
		{ op: "list" },
	],
	recovery_write_failure: [
		{ op: "seed", value: { version: 1, tasks: [without(task({ state: "admitted" }), "result")] } },
		{ op: "failWrites", on: true },
		{ op: "open" },
		{ op: "ready" },
		{ op: "failWrites", on: false },
		{ op: "list" },
		{ op: "open" },
		{ op: "list" },
	],
	read_failure_is_sticky: [
		{ op: "failRead", on: true },
		{ op: "open" },
		{ op: "ready" },
		{ op: "failRead", on: false },
		{ op: "list" },
		{ op: "admit", definition: "identity@1", fingerprint: fp, key: "request-1" },
		{ op: "open" },
		{ op: "list" },
	],
	bounded_persistence: [
		{ op: "cycle", count: 130 },
		{ op: "count" },
		{ op: "open" },
		{ op: "count" },
	],
};
for (const [n, document] of invalidDocuments.entries())
	scenarios[`invalid_document_${n}`] = [{ op: "seed", value: document }, { op: "open" }, { op: "ready" }, { op: "list" }];

const json = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

async function run(ops) {
	let value;
	let failWrites = false;
	let failRead = false;
	const store = {
		read: async () => {
			if (failRead) throw new Error("read failed");
			return structuredClone(value);
		},
		write: async (document) => {
			if (failWrites) throw new Error("write failed");
			value = structuredClone(document);
		},
	};
	let journal = new NativeTaskJournal(store);
	const ids = [];
	const ref = (id) => (typeof id === "string" && id.startsWith("$") ? ids[Number(id.slice(1))] : id);
	const steps = [];
	for (const op of ops) {
		let outcome;
		try {
			let result;
			switch (op.op) {
				case "seed": value = structuredClone(op.value); break;
				case "open": journal = new NativeTaskJournal(store); break;
				case "failWrites": failWrites = op.on; break;
				case "failRead": failRead = op.on; break;
				case "ready": result = await journal.ready(); break;
				case "list": result = await journal.list(); break;
				case "count": result = (await journal.list()).length; break;
				case "admit":
					result = await journal.admit(op.definition, op.fingerprint, op.key, undefined, ref(op.parentId));
					if (result.created) ids.push(result.task.id);
					break;
				case "transition": result = await journal.transition(ref(op.id), op.state, op.result); break;
				case "cycle":
					for (let n = 0; n < op.count; n += 1) {
						const { task: t } = await journal.admit("identity@1", fp, `request-${n}`);
						ids.push(t.id);
						await journal.transition(t.id, "running");
						if (n < op.count - 1) await journal.transition(t.id, "completed", ok("succeeded"));
					}
					break;
				default: throw new Error(`unknown op ${op.op}`);
			}
			outcome = { ok: json(result) };
		} catch (error) {
			outcome = { error: error.message };
		}
		steps.push({ ...json(op), outcome, stored: json(value) });
	}
	return { ids, steps };
}

const out = { source: "packages/coding-agent/src/ultron/rlm/task-store.ts", fingerprints: [], scenarios: {} };
for (const input of fingerprintInputs) out.fingerprints.push({ input, hash: taskFingerprint(input) });
// Random IDs become fixed UUIDs so the fixture is deterministic.
for (const [name, ops] of Object.entries(scenarios)) {
	const recorded = await run(ops);
	let text = JSON.stringify(recorded);
	recorded.ids.forEach((id, n) => {
		text = text.replaceAll(id, `ultron-task-00000000-0000-4000-8000-${String(n).padStart(12, "0")}`);
	});
	out.scenarios[name] = JSON.parse(text);
}
writeFileSync(join(here, "task-store.json"), `${JSON.stringify(out, null, "\t")}\n`);
