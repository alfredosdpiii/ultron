// Generates crates/kernel-apis/fixtures/memory-journal.json: NativeMemoryService (ultron/memory.ts) reopened over
// stored journals: the journal one run of propose/get/correct/prepare/forget wrote, then variants of it (missing or
// unsupported version, other namespace, unknown fields, duplicate ids, bad supersedes, interrupted operations).
// Each case records what list() returns (or its error) and what the store holds afterwards. Normalized like generate.mjs.
// Usage: node crates/kernel-apis/fixtures/generate-memory-journal.mjs
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { fakeHindsight, normalize } from "./generate.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = join(resolve(here, "../../.."), "packages/coding-agent");
const { NativeMemoryService } = await import(pathToFileURL(join(pkg, "src/ultron/memory.ts")).href);
const gate = async (request) => (request.action === "recall" ? { retrieve: true, probability: 1 } : { action: "keep", confidence: 1 });
const store = (value) => ({ value, async read() { return structuredClone(this.value); }, async write(next) { this.value = structuredClone(next); } });

const base = store(undefined);
const service = new NativeMemoryService({ store: base, backend: fakeHindsight([]), gate });
const first = await service.propose({ text: "fact one", evidence: [{ ref: "a" }] });
await service.get(first.memoryId);
await service.correct(first.memoryId, { text: "fact one, corrected", evidence: [{ ref: "b" }] });
await service.prepare({ query: "fact", taskId: "t" });
const second = await service.propose({ text: "fact two sync", evidence: [{ ref: "c" }], scope: "project" });
await service.forget(second.memoryId);
// Deterministic ids and times, so the recorded inputs are valid journals and the fixture is stable.
const ids = new Map();
const journal = JSON.parse(
	JSON.stringify(base.value)
		.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g, (id) => {
			if (!ids.has(id)) ids.set(id, `00000000-0000-4000-8000-${String(ids.size + 1).padStart(12, "0")}`);
			return ids.get(id);
		})
		.replace(/\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z/g, "2026-01-02T03:04:05.006Z"),
);
const ops = journal.operations;
const edit = (change) => {
	const copy = structuredClone(journal);
	change(copy, copy.operations);
	return copy;
};
const inputs = {
	written: journal,
	"no version": edit((doc) => delete doc.version),
	"version 2": edit((doc) => (doc.version = 2)),
	"other namespace": edit((doc) => (doc.namespace = "fake://other")),
	"extra document field": edit((doc) => (doc.extra = 1)),
	"extra operation field": edit((_, o) => (o[0].text = "secret")),
	"duplicate id": edit((_, o) => o.push(structuredClone(o[0]))),
	"unknown kind": edit((_, o) => (o[0].kind = "bogus")),
	"bad created": edit((_, o) => (o[0].createdAt = "yesterday")),
	"bad text hash": edit((_, o) => (o[0].textHash = "xyz")),
	"supersedes unknown": edit((_, o) => (o.find((op) => op.supersedes).supersedes = "nope")),
	"get before owner": edit((_, o) => o.unshift(structuredClone(o.find((op) => op.kind === "get")))),
	"document proposed twice": edit((_, o) => o.push({ ...structuredClone(o[0]), id: "00000000-0000-4000-8000-0000000000ff" })),
	"bad gate": edit((_, o) => (o[0].gate = { action: "keep", extra: 1 })),
	"bad receipt": edit((_, o) => (o.find((op) => op.receipts).receipts[0].status = "lost")),
	"bad scope tag": edit((_, o) => (o[0].tags = ["ultron:project:p1"])),
	"interrupted": edit((_, o) => {
		o.push({ ...structuredClone(ops[0]), id: "started-gate", state: "started", phase: "gate", memoryId: undefined, tags: undefined, operationIds: undefined, gate: undefined });
		o.push({ ...structuredClone(ops[0]), id: "started-backend", memoryId: "m-new", state: "started", operationIds: ["started-backend"] });
		o.push({ id: "started-prepare", kind: "prepare", state: "started", phase: "gate", createdAt: ops[0].createdAt, updatedAt: ops[0].updatedAt, scope: "session", taskId: "t", queryHash: ops[0].textHash });
		for (const op of o) for (const key of Object.keys(op)) if (op[key] === undefined) delete op[key];
	}),
	"not an object": [1, 2],
	"operations not array": edit((doc) => (doc.operations = {})),
};
const cases = [];
for (const [name, input] of Object.entries(inputs)) {
	const reopened = store(structuredClone(input));
	const fresh = new NativeMemoryService({ store: reopened, backend: fakeHindsight([]), gate });
	let outcome;
	try {
		outcome = { ok: await fresh.list() };
	} catch (error) {
		outcome = { error: error.message };
	}
	// Inputs are deterministic and kept verbatim; what the service produced is normalized.
	const produced = JSON.parse(normalize(JSON.stringify({ ...outcome, stored: reopened.value ?? null })));
	cases.push({ name, input, ...produced });
}
writeFileSync(join(here, "memory-journal.json"), `${JSON.stringify({ cases }, null, "\t")}\n`);
console.log(`memory-journal: ${cases.length} cases`);
