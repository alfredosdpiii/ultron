// Generates crates/services/fixtures/gates.json from createReleaseGateModule (packages/coding-agent/src/ultron/
// release-gate.ts) over an in-memory module store: scripted sessions of gates.* requests from a caller (the root,
// or a task), recording each result or error and the stored document after every step; steps may reopen the
// module over the same store, seed or tamper with the document, or make writes fail. The clock is the request's
// `at`; randomUUID is replaced by a counter. Usage: node crates/services/fixtures/generate-gates.mjs
import crypto from "node:crypto";
import { writeFileSync } from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

let counter = 0;
crypto.randomUUID = () => `00000000-0000-4000-8000-${String(++counter).padStart(12, "0")}`;
syncBuiltinESMExports();

const here = dirname(fileURLToPath(import.meta.url));
const pkg = join(here, "../../../packages/coding-agent/src/ultron");
const { createReleaseGateModule } = await import(join(pkg, "release-gate.ts"));

const F = "f".repeat(64);
const E = "e".repeat(64);
const gate = {
	id: "release-core",
	checks: [
		{ name: "unit", required: true },
		{ name: "typecheck", required: true },
		{ name: "perf", required: false },
	],
	fixture_hash: F,
};
const run = (variant, results, fixture_hash = F) => ({ variant, fixture_hash, results });
const allPass = { unit: "passed", typecheck: "passed", perf: "passed" };
const compare = (baseline, candidate, gate_id = "release-core") => ["gates.compare", { gate_id, baseline, candidate }];

const sessions = [
	{
		name: "decisions",
		steps: [
			["gates.list", {}],
			["gates.define", gate],
			["gates.define", { ...gate, checks: [...gate.checks].reverse() }],
			["gates.define", { ...gate, checks: gate.checks.map((c) => (c.name === "unit" ? { ...c, required: false } : c)) }],
			["gates.define", { ...gate, fixture_hash: E }],
			compare(run("main@abc", { unit: "passed", typecheck: "failed", perf: "passed" }), run("branch@def", allPass)),
			compare(run("main", allPass), run("branch", { ...allPass, unit: "failed" })),
			compare(run("main", allPass), run("branch", { ...allPass, perf: "failed" })),
			compare(run("main", { unit: "failed", typecheck: "missing" }), run("branch", { unit: "passed" })),
			compare(run("main", allPass), run("branch", { ...allPass, typecheck: "missing" })),
			compare(run("main", allPass), run("branch", allPass, E)),
			compare(run("main", allPass, E), run("branch", { unit: "failed" }, E)),
			compare(run("main", {}), run("branch", { perf: "passed" })),
			compare(run("main", allPass), run("branch", { ...allPass, bonus: "passed" })),
			["gates.define", { ...gate, id: "optional-only", checks: [{ name: "a", required: false }] }],
			["gates.define", { id: "second", checks: [{ name: "lint", required: true }, { name: "A b/c:d.e-f_g", required: false }], fixture_hash: E }],
			compare(run("x", { lint: "passed" }, E), run("y", { "A b/c:d.e-f_g": "passed" }, E), "second"),
			["gates.history", { gate_id: "release-core" }],
			["gates.history", { gate_id: "second" }],
			["gates.list", {}],
			["reopen"],
			["gates.history", { gate_id: "second" }],
			["gates.define", { ...gate, fixture_hash: E }],
			["gates.define", gate],
		],
	},
	{
		name: "validation",
		steps: [
			["gates.define", gate],
			["gates.define", { ...gate, id: "g2", strict: true }],
			["gates.define", { ...gate, id: "g3", checks: [{ name: "unit", required: true, weight: 2 }] }],
			["gates.define", { ...gate, id: "Upper" }],
			["gates.define", { ...gate, id: "9lives" }],
			["gates.define", { ...gate, id: "a".repeat(129) }],
			["gates.define", { ...gate, id: "a".repeat(128) }],
			["gates.define", { ...gate, id: 5 }],
			["gates.define", { ...gate, id: "g4", checks: [] }],
			["gates.define", { ...gate, id: "g4", checks: "unit" }],
			["gates.define", { ...gate, id: "g4", checks: Array.from({ length: 257 }, (_, i) => ({ name: `c${i}`, required: true })) }],
			["gates.define", { ...gate, id: "g4", checks: [null] }],
			["gates.define", { ...gate, id: "g4", checks: [[]] }],
			["gates.define", { ...gate, id: "g4", checks: [{ name: "-bad", required: true }] }],
			["gates.define", { ...gate, id: "g4", checks: [{ name: "x".repeat(129), required: true }] }],
			["gates.define", { ...gate, id: "g4", checks: [{ name: "ok", required: "yes" }] }],
			["gates.define", { ...gate, id: "g4", checks: [{ name: "ok", required: true }, { name: "ok", required: false }] }],
			["gates.define", { ...gate, id: "g4", checks: [{ name: "ok" }] }],
			["gates.define", { ...gate, id: "g4", fixture_hash: "F".repeat(64) }],
			["gates.define", { ...gate, id: "g4", fixture_hash: "f".repeat(63) }],
			["gates.define", { id: "g4", checks: gate.checks }],
			compare(run("main", allPass), run("branch", allPass), "nope"),
			["gates.compare", { gate_id: "release-core", baseline: run("main", allPass), candidate: run("branch", allPass), override: "passed" }],
			compare({ ...run("main", allPass), notes: "x" }, run("branch", allPass)),
			compare(run("main", allPass), run("branch", { ...allPass, unit: "skipped" })),
			compare(run("main", allPass), run("branch", { ...allPass, unit: null })),
			compare(run("", allPass), run("branch", allPass)),
			compare(run(" lead", allPass), run("branch", allPass)),
			compare(run("two\nlines", allPass), run("branch", allPass)),
			compare(run("v".repeat(257), allPass), run("branch", allPass)),
			compare(run("v".repeat(256), allPass), run("𝄞".repeat(128), allPass)),
			compare(run("main", allPass, "nothex"), run("branch", allPass)),
			compare(run("main", []), run("branch", allPass)),
			compare("main", run("branch", allPass)),
			compare(run("main", allPass), null),
			["gates.compare", { gate_id: "release-core", baseline: run("main", allPass) }],
			["gates.history", { gate_id: "nope" }],
			["gates.history", { gate_id: "release-core", all: true }],
			["gates.history", {}],
			["gates.list", { all: true }],
			["gates.remove", { gate_id: "release-core" }],
		],
	},
	{
		name: "callers, documents and durability",
		steps: [
			["caller", "ultron-task-1"],
			["at", 7000],
			["gates.define", gate],
			compare(run("main", allPass), run("branch", allPass)),
			["caller", null],
			compare(run("main", allPass), run("branch", { unit: "failed" })),
			["tamper"],
			["reopen"],
			["gates.list", {}],
			["gates.define", gate],
			["seed", { gates: [], attempts: [] }],
			["reopen"],
			["gates.list", {}],
			["seed", { version: 2, gates: [], attempts: [] }],
			["reopen"],
			["gates.list", {}],
			["seed", { version: 1, gates: [], attempts: [], extra: 1 }],
			["reopen"],
			["gates.list", {}],
			["seed", { version: 1, gates: [{ id: "g", checks: [], fixture_hash: "x", digest: "d", defined_at: 1, defined_by: "" }], attempts: [] }],
			["reopen"],
			["gates.list", {}],
			["seed", "nope"],
			["reopen"],
			["gates.list", {}],
			["seed", { version: 1, gates: [], attempts: [] }],
			["reopen"],
			["gates.define", gate],
			["failWrites", true],
			compare(run("main", allPass), run("branch", allPass)),
			["failWrites", false],
			["gates.list", {}],
			["reopen"],
			["gates.history", { gate_id: "release-core" }],
		],
	},
];

function store() {
	let document;
	const s = {
		failWrites: false,
		get document() {
			return document;
		},
		set document(next) {
			document = structuredClone(next);
		},
		read: async () => (document === undefined ? undefined : structuredClone(document)),
		write: async (next) => {
			if (s.failWrites) throw new Error("disk full");
			document = structuredClone(next);
		},
	};
	return s;
}

const recorded = [];
for (const session of sessions) {
	counter = 0;
	const documents = store();
	let caller = null;
	let at = 5000;
	const host = { now: () => at, callerTaskId: () => caller };
	const open = () => createReleaseGateModule({ store: documents });
	let module = open();
	let started = false;
	const steps = [];
	for (const step of session.steps) {
		const [type, ...args] = step;
		if (type === "reopen") {
			module = open();
			started = false;
		} else if (type === "caller") caller = args[0];
		else if (type === "at") at = args[0];
		else if (type === "seed") documents.document = args[0];
		else if (type === "tamper") {
			const saved = structuredClone(documents.document);
			for (const check of saved.gates[0].checks) check.required = false;
			documents.document = saved;
		} else if (type === "failWrites") documents.failWrites = args[0];
		else {
			let outcome;
			try {
				if (!started) {
					started = true;
					await module.start(host);
				}
				outcome = { ok: await module.handle({ type, payload: args[0], caller: "lane" }, host) };
			} catch (error) {
				outcome = { error: error.message };
			}
			steps.push({ step, ...outcome, document: JSON.stringify(documents.document ?? null) });
			continue;
		}
		steps.push({ step });
	}
	recorded.push({ name: session.name, steps });
}
writeFileSync(join(here, "gates.json"), `${JSON.stringify(recorded, null, "\t")}\n`);
console.log(`gates: ${recorded.length} sessions`);
