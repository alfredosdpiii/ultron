// Generates crates/durable/fixtures/document-state.json from the pinned upstream Pi Durable source: `Session.documentState`
// (Chord `replicatedState` over the Session's `CommittedStateSource`) driven by step scripts over a MemoryStorage
// Session. Each scenario records what the steps observed (state values, `undefined` for an absent document, errors)
// and every subscriber's deliveries (`kind`, `sequence`, value). crates/durable/tests/document_state.rs replays the
// same steps against the Rust Session on every backend and compares both.
// Usage: node crates/durable/fixtures/generate-document-state.mjs <pi-reference-checkout>
// Pi Durable is MIT licensed, (c) the upstream authors; see the reference checkout's LICENSE.
import "../../cli/fixtures/offline-guard.mjs";
import { execFileSync } from "node:child_process";
import { existsSync, writeFileSync } from "node:fs";
import { registerHooks } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const reference = resolve(process.argv[2] ?? "../ultron-rust-reference");
const here = dirname(fileURLToPath(import.meta.url));
const PACKAGES = { chord: "chord", "pi-ai": "ai", "pi-durable": "durable" };
registerHooks({
	resolve(specifier, context, next) {
		const match = /^@earendil-works\/(chord|pi-ai|pi-durable)(\/.*)?$/.exec(specifier);
		if (!match) return next(specifier, context);
		// A subpath is a directory with an index.ts or a file of that name.
		const root = join(reference, "packages", PACKAGES[match[1]], `src${match[2] ?? ""}`);
		const path = existsSync(join(root, "index.ts")) ? join(root, "index.ts") : `${root}.ts`;
		return { url: pathToFileURL(path).href, shortCircuit: true };
	},
});
const src = (path) => import(pathToFileURL(join(reference, "packages/durable/src", path)).href);
const { MemoryStorage } = await src("storage/memory.ts");
const { defineDoc } = await src("documents.ts");
const { SessionImpl } = await src("session/session.ts");
const { BACKGROUND_CONTEXT: context } = await import("@earendil-works/chord/context");
const revision = execFileSync("git", ["-C", reference, "rev-parse", "HEAD"], { encoding: "utf8" }).trim();

const DOCS = {
	notes: defineDoc({ kind: "state.notes", version: 1, scope: "session", initial: () => ({ text: "", count: 0 }) }),
	v1: defineDoc({ kind: "cache.versioned", version: 1, scope: "session", initial: () => ({ name: "first" }) }),
	v2: defineDoc({
		kind: "cache.versioned",
		version: 2,
		scope: "session",
		initial: () => ({ names: [] }),
		migrate: (value) => ({ names: [value.name] }),
	}),
};
const clone = (v) => (v === undefined ? "undefined" : JSON.parse(JSON.stringify(v)));
const flush = () => new Promise((done) => setTimeout(done, 0));

// Steps: set/push/touch/retire commit to a document; state attaches (or records "undefined"); subscribe and
// subscribeBlocked (the listener's first delivery waits for `release`) add a subscriber; value records a state's value;
// close closes the Session; stateError records the rejection of a documentState call; flush lets deliveries run.
const SCENARIOS = {
	"hydrate-then-updates": [
		{ op: "touch", doc: "notes" },
		{ op: "state", doc: "notes", name: "s" },
		{ op: "value", state: "s" },
		{ op: "subscribe", state: "s", name: "a" },
		{ op: "set", doc: "notes", field: "text", value: "one" },
		{ op: "value", state: "s" },
		{ op: "set", doc: "notes", field: "count", value: 2 },
		{ op: "flush" },
		{ op: "subscribe", state: "s", name: "b" },
		{ op: "set", doc: "notes", field: "text", value: "three" },
		{ op: "flush" },
		{ op: "unsubscribe", name: "a" },
		{ op: "set", doc: "notes", field: "count", value: 4 },
		{ op: "flush" },
		{ op: "value", state: "s" },
		{ op: "touch", doc: "notes" },
		{ op: "flush" },
	],
	"never-creates": [
		{ op: "state", doc: "notes", name: "s" },
		{ op: "touch", doc: "notes" },
		{ op: "state", doc: "notes", name: "t" },
		{ op: "value", state: "t" },
	],
	"older-shape": [
		{ op: "touch", doc: "v1" },
		{ op: "state", doc: "v1", name: "old" },
		{ op: "subscribe", state: "old", name: "o" },
		{ op: "push", doc: "v2", field: "names", value: "second" },
		{ op: "flush" },
		{ op: "value", state: "old" },
		{ op: "state", doc: "v2", name: "new" },
		{ op: "subscribe", state: "new", name: "n" },
		{ op: "push", doc: "v2", field: "names", value: "third" },
		{ op: "flush" },
		{ op: "value", state: "old" },
		{ op: "value", state: "new" },
	],
	"migration-only-base": [
		{ op: "touch", doc: "v1" },
		{ op: "state", doc: "v1", name: "old" },
		{ op: "subscribe", state: "old", name: "o" },
		{ op: "touch", doc: "v2" },
		{ op: "flush" },
		{ op: "state", doc: "v2", name: "new" },
		{ op: "subscribe", state: "new", name: "n" },
		{ op: "touch", doc: "v2" },
		{ op: "flush" },
		{ op: "value", state: "old" },
		{ op: "value", state: "new" },
	],
	"retire-then-recreate": [
		{ op: "set", doc: "notes", field: "text", value: "kept" },
		{ op: "state", doc: "notes", name: "s" },
		{ op: "subscribe", state: "s", name: "a" },
		{ op: "retire", doc: "notes" },
		{ op: "flush" },
		{ op: "value", state: "s" },
		{ op: "set", doc: "notes", field: "text", value: "again" },
		{ op: "flush" },
		{ op: "value", state: "s" },
		{ op: "subscribe", state: "s", name: "late" },
		{ op: "flush" },
	],
	"dispose-keeps-value": [
		{ op: "set", doc: "notes", field: "text", value: "before" },
		{ op: "state", doc: "notes", name: "s" },
		{ op: "subscribe", state: "s", name: "a" },
		{ op: "dispose", state: "s" },
		{ op: "dispose", state: "s" },
		{ op: "set", doc: "notes", field: "text", value: "after" },
		{ op: "flush" },
		{ op: "value", state: "s" },
		{ op: "state", doc: "notes", name: "t" },
		{ op: "value", state: "t" },
	],
	"close-keeps-value": [
		{ op: "set", doc: "notes", field: "text", value: "open" },
		{ op: "state", doc: "notes", name: "s" },
		{ op: "subscribe", state: "s", name: "a" },
		{ op: "set", doc: "notes", field: "count", value: 1 },
		{ op: "flush" },
		{ op: "close" },
		{ op: "flush" },
		{ op: "value", state: "s" },
		{ op: "stateError", doc: "notes" },
	],
	"unsubscribe-discards-pending": [
		{ op: "touch", doc: "notes" },
		{ op: "state", doc: "notes", name: "s" },
		{ op: "subscribeBlocked", state: "s", name: "slow" },
		{ op: "set", doc: "notes", field: "count", value: 1 },
		{ op: "set", doc: "notes", field: "count", value: 2 },
		{ op: "unsubscribe", name: "slow" },
		{ op: "set", doc: "notes", field: "count", value: 3 },
		{ op: "release", name: "slow" },
		{ op: "flush" },
		{ op: "value", state: "s" },
	],
	"overflow-keeps-newest": [
		{ op: "touch", doc: "notes" },
		{ op: "state", doc: "notes", name: "s" },
		{ op: "subscribeBlocked", state: "s", name: "slow" },
		{ op: "subscribe", state: "s", name: "fast" },
		...Array.from({ length: 101 }, (_, i) => ({ op: "set", doc: "notes", field: "count", value: i + 1 })),
		{ op: "release", name: "slow" },
		{ op: "flush" },
		{ op: "subscribeBlocked", state: "s", name: "exact" },
		...Array.from({ length: 100 }, (_, i) => ({ op: "set", doc: "notes", field: "count", value: i + 200 })),
		{ op: "release", name: "exact" },
		{ op: "flush" },
	],
};

async function run(steps) {
	const session = new SessionImpl(new MemoryStorage());
	const states = {};
	const subscriptions = {};
	const gates = {};
	const deliveries = {};
	const trace = [];
	const commit = (doc, mutate) =>
		session.commit(async (tx) => {
			const draft = await tx.doc(DOCS[doc]);
			mutate?.(draft);
		}, context);
	for (const step of steps) {
		switch (step.op) {
			case "set":
				await commit(step.doc, (draft) => {
					draft[step.field] = step.value;
				});
				break;
			case "push":
				await commit(step.doc, (draft) => {
					draft[step.field].push(step.value);
				});
				break;
			case "touch":
				await commit(step.doc);
				break;
			case "retire":
				await session.commit((tx) => tx.retireDoc(DOCS[step.doc]), context);
				break;
			case "state": {
				const state = await session.documentState(DOCS[step.doc], context);
				if (state === undefined) trace.push({ state: step.name, value: "undefined" });
				else states[step.name] = state;
				break;
			}
			case "value":
				trace.push({ state: step.state, value: clone(states[step.state].value) });
				break;
			case "subscribe":
			case "subscribeBlocked": {
				const received = (deliveries[step.name] = []);
				let gate;
				if (step.op === "subscribeBlocked") gate = Promise.withResolvers();
				gates[step.name] = gate;
				subscriptions[step.name] = states[step.state].subscribe((value, _context, delivery) => {
					received.push({ kind: delivery.kind, sequence: delivery.sequence, value: clone(value) });
					if (gate !== undefined && received.length === 1) return gate.promise;
				});
				break;
			}
			case "release":
				gates[step.name].resolve();
				break;
			case "unsubscribe":
				subscriptions[step.name]();
				break;
			case "dispose":
				states[step.state].dispose();
				break;
			case "close":
				await session.close(context);
				break;
			case "stateError":
				try {
					await session.documentState(DOCS[step.doc], context);
					trace.push({ error: null });
				} catch (error) {
					trace.push({ error: error.message });
				}
				break;
			case "flush":
				await flush();
				await flush();
				break;
			default:
				throw new Error(`unknown step ${step.op}`);
		}
	}
	return { trace, deliveries };
}

const scenarios = [];
for (const [name, steps] of Object.entries(SCENARIOS)) scenarios.push({ name, steps, ...(await run(steps)) });
writeFileSync(join(here, "document-state.json"), `${JSON.stringify({ revision, scenarios }, null, 1)}\n`);
console.log(`wrote ${scenarios.length} scenarios`);
