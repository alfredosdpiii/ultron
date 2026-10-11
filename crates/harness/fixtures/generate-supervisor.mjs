// Generates crates/harness/fixtures/task-supervisor.json from the TypeScript task host
// (packages/coding-agent/src/ultron/rlm/native-host.ts, NativeRlmHost) in this checkout: task admission, the
// running transition, first-terminal-wins finishing, subtree cancellation with abort propagation to each task's
// lane, host close, child-lane visibility, and restart recovery over a reopened store.
//
// Lanes are controlled fakes (as in test/ultron-a42-intervention-subtree-stop.test.ts): prompt() blocks until the
// scenario finishes or fails it, or the host aborts the lane. Each step records its outcome (value or error
// message), the stored journal document, and every lane's abort count. Task IDs and generated idempotency keys
// are random in TypeScript, so they are renamed to fixed values; the fixture lists them in mint order.
//
// Usage: ULTRON_CODING_AGENT_DIR=$(mktemp -d) ULTRON_SERVER_DIR=$(mktemp -d) node crates/harness/fixtures/generate-supervisor.mjs
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const { NativeRlmHost } = await import(pathToFileURL(join(root, "packages/coding-agent/src/ultron/rlm/native-host.ts")).href);

const context = {};
const tick = () => new Promise((done) => setTimeout(done, 0));

function controlledHarness() {
	const probes = new Map();
	const probe = (name) => {
		let existing = probes.get(name);
		if (existing) return existing;
		let markStarted;
		const started = new Promise((done) => {
			markStarted = done;
		});
		let release;
		let text = "";
		const created = {
			started,
			aborts: 0,
			finish(value) {
				text = value;
				release?.({ ok: true, value: { status: "completed", tipId: "tip", fromTipId: null } });
			},
			fail(error) {
				release?.({ ok: false, error });
			},
			lane: {
				getActiveTools: async () => [],
				setModel: async () => {},
				prompt: () =>
					new Promise((done) => {
						release = done;
						markStarted();
					}),
				steer: async () => ({ ok: true, value: {} }),
				abort: async () => {
					created.aborts += 1;
					release?.({ ok: false, error: { kind: "aborted" } });
					return { ok: true };
				},
				findEntries: async () => [
					{ id: "tip", type: "message", message: { role: "assistant", content: [{ type: "text", text }] } },
				],
			},
		};
		probes.set(name, created);
		return created;
	};
	return { harness: { lane: async (name) => probe(name).lane }, probe, probes };
}

const laneOf = (id) => `ultron.rlm-child.${id}`;

// Scenario steps name tasks by label ("a", "b", ...) assigned at spawn; "from" is the label whose lane calls.
const scenarios = {
	subtree_cancel_keeps_sibling: [
		{ op: "spawn", as: "a", prompt: "child A" },
		{ op: "spawn", as: "b", prompt: "child B" },
		{ op: "spawn", as: "g", prompt: "grandchild of A", from: "a" },
		{ op: "spawn", as: "gg", prompt: "great-grandchild of A", from: "g" },
		{ op: "status" },
		{ op: "status", from: "a" },
		{ op: "inspect", task: "b", from: "a" },
		{ op: "inspect", task: "gg", from: "a" },
		{ op: "cancel", task: "a" },
		{ op: "result", task: "g" },
		{ op: "inspect", task: "b" },
		{ op: "finish", task: "b", text: "B summary" },
		{ op: "cancel", task: "b" },
		{ op: "cancel", task: "a" },
		{ op: "status" },
	],
	finished_parent_cancel_reaches_live_descendants: [
		{ op: "spawn", as: "p", prompt: "parent" },
		{ op: "spawn", as: "s", prompt: "sibling" },
		{ op: "spawn", as: "c", prompt: "child", from: "p" },
		{ op: "spawn", as: "d", prompt: "grandchild", from: "c" },
		{ op: "finish", task: "p", text: "parent done" },
		{ op: "inspect", task: "c" },
		{ op: "cancel", task: "p" },
		{ op: "inspect", task: "c" },
		{ op: "inspect", task: "d" },
		{ op: "inspect", task: "s" },
		{ op: "close" },
		{ op: "status" },
	],
	failure_and_cancel_from_child_lane: [
		{ op: "spawn", as: "a", prompt: "A" },
		{ op: "spawn", as: "b", prompt: "B", from: "a" },
		{ op: "spawn", as: "c", prompt: "C", from: "a" },
		{ op: "fail", task: "b", error: { kind: "provider", message: "overloaded" } },
		{ op: "cancel", task: "c", from: "b" },
		{ op: "cancel", task: "c", from: "a" },
		{ op: "result", task: "c" },
		{ op: "result", task: "b", from: "a" },
		{ op: "cancel", task: "a", from: "c" },
		{ op: "status" },
	],
	idempotent_keys: [
		{ op: "spawn", as: "a", prompt: "same", key: "k1" },
		{ op: "spawn", as: "a2", prompt: "same", key: "k1" },
		{ op: "spawn", as: "x", prompt: "different", key: "k1" },
		{ op: "spawn", as: "y", prompt: "same", key: "k1", timeout_ms: 1000 },
		{ op: "spawn", as: "z", prompt: "same", key: "k1", model: "p/m" },
		{ op: "finish", task: "a", text: "done" },
		{ op: "spawn", as: "a3", prompt: "same", key: "k1" },
		{ op: "spawn", as: "m", prompt: "with model", key: "k2", model: "prov/model/x", timeout_ms: 3600000 },
		{ op: "spawn", as: "bad", prompt: "x", model: "nomodel" },
		{ op: "spawn", as: "bad", prompt: "x", key: "" },
		{ op: "spawn", as: "bad", prompt: "x", timeout_ms: 0 },
		{ op: "spawn", as: "bad", prompt: "x", timeout_ms: 3600001 },
		{ op: "spawn", as: "bad", prompt: "x", timeout_ms: 1.5 },
		{ op: "spawn", as: "bad", prompt: "x", set: { timeout_ms: null } },
		{ op: "spawn", as: "bad", prompt: "x", set: { timeout_ms: "5" } },
		{ op: "spawn", as: "bad", prompt: "x", key: " \t\n" },
		{ op: "spawn", as: "bad", prompt: "x", key: 7 },
		{ op: "spawn", as: "bad", prompt: "x", model: "a/b c" },
		{ op: "spawn", as: "bad", prompt: "x", model: "/b" },
		{ op: "spawn", as: "bad", prompt: "x", model: "a/" },
		{ op: "spawn", as: "bad", prompt: "x", model: "a//b" },
		{ op: "spawn", as: "bad", prompt: "x", model: 3 },
		{ op: "spawn", as: "bad", prompt: "x", set: { extra: 1 } },
		{ op: "spawn", as: "bad", prompt: "x", set: { definition: "Rlm-child@1" } },
		{ op: "spawn", as: "bad", prompt: "x", set: { definition: "rlm-child@v1" } },
		{ op: "spawn", as: "n", prompt: "null options", set: { key: null, model: null } },
		{ op: "spawn", as: "t", prompt: "timed", key: "kt", timeout_ms: 5000 },
		{ op: "spawn", as: "t2", prompt: "timed", key: "kt", timeout_ms: 5000, model: null },
		{ op: "spawn", as: "u", prompt: "unicode \u00e9\ud83d\ude00", key: "ku", model: "p/m\u00e9x" },
		{ op: "spawn", as: "bad", prompt: "x", model: "p/m\u00a0x" },
		{ op: "spawn", as: "bad", prompt: "x", model: "p/m\u2028x" },
		{ op: "spawn", as: "w", prompt: "nel", key: "kw", model: "p/m\u0085x" },
		{ op: "spawn", as: "v", prompt: "unicode", key: "kv", model: "p/m\ufeffx" },
		{ op: "status" },
	],
	close_cancels_everything_live: [
		{ op: "spawn", as: "a", prompt: "A" },
		{ op: "spawn", as: "b", prompt: "B", from: "a" },
		{ op: "spawn", as: "c", prompt: "C" },
		{ op: "finish", task: "c", text: "C done" },
		{ op: "close" },
		{ op: "spawn", as: "late", prompt: "late" },
		{ op: "status" },
	],
	restart_interrupts_unfinished: [
		{ op: "spawn", as: "a", prompt: "A" },
		{ op: "spawn", as: "b", prompt: "B", from: "a" },
		{ op: "spawn", as: "c", prompt: "C" },
		{ op: "finish", task: "c", text: "C done" },
		{ op: "spawn", as: "d", prompt: "D", key: "kd" },
		{ op: "crash" },
		{ op: "status" },
		{ op: "result", task: "a" },
		{ op: "cancel", task: "a" },
		{ op: "spawn", as: "d2", prompt: "D", key: "kd" },
		{ op: "spawn", as: "e", prompt: "E" },
		{ op: "cancel", task: "e" },
		{ op: "status" },
		{ op: "crash" },
		{ op: "status" },
	],
	close_then_reopen: [
		{ op: "spawn", as: "a", prompt: "A" },
		{ op: "spawn", as: "b", prompt: "B", from: "a" },
		{ op: "reopen" },
		{ op: "status" },
		{ op: "spawn", as: "c", prompt: "C", from: "a" },
		{ op: "status" },
	],
};

const json = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

function memoryStore() {
	return {
		value: undefined,
		async read() {
			return structuredClone(this.value);
		},
		async write(next) {
			this.value = structuredClone(next);
		},
	};
}

async function run(ops) {
	const store = memoryStore();
	let controlled = controlledHarness();
	let host = new NativeRlmHost(controlled.harness, {}, { store });
	const labels = new Map();
	const ids = [];
	const keys = [];
	const call = (type, payload, from) =>
		host.handle(type, payload, context, from === undefined ? undefined : { lane: laneOf(labels.get(from)) });
	const steps = [];
	for (const op of ops) {
		let outcome;
		try {
			let result;
			switch (op.op) {
				case "spawn": {
					const payload = { definition: "rlm-child@1", input: { prompt: op.prompt } };
					for (const field of ["key", "model", "timeout_ms"]) if (op[field] !== undefined) payload[field] = op[field];
					Object.assign(payload, op.set ?? {});
					result = await call("agents.spawn", payload, op.from);
					labels.set(op.as, result.id);
					if (!ids.includes(result.id)) ids.push(result.id);
					if (result.state === "admitted" || result.state === "running") await controlled.probe(laneOf(result.id)).started;
					break;
				}
				case "finish":
					controlled.probe(laneOf(labels.get(op.task))).finish(op.text);
					result = await call("agents.result", { id: labels.get(op.task) });
					break;
				case "fail":
					controlled.probe(laneOf(labels.get(op.task))).fail(op.error);
					result = await call("agents.result", { id: labels.get(op.task) });
					break;
				case "cancel":
					result = await call("agents.cancel", { id: labels.get(op.task) }, op.from);
					break;
				case "inspect":
					result = await call("agents.inspect", { id: labels.get(op.task) }, op.from);
					break;
				case "result":
					result = await call("agents.result", { id: labels.get(op.task) }, op.from);
					break;
				case "status":
					result = (await call("agents.status", {}, op.from)).tasks;
					break;
				case "close":
					await host.close();
					break;
				case "reopen":
					await host.close();
					controlled = controlledHarness();
					host = new NativeRlmHost(controlled.harness, {}, { store });
					break;
				case "crash":
					// The old owner is abandoned mid-run (its lanes never return), as when the process dies.
					controlled = controlledHarness();
					host = new NativeRlmHost(controlled.harness, {}, { store });
					break;
				default:
					throw new Error(`unknown op ${op.op}`);
			}
			outcome = { ok: json(result) };
		} catch (error) {
			outcome = { error: error.message };
		}
		await tick();
		await tick();
		for (const task of store.value?.tasks ?? []) if (!keys.includes(task.key)) keys.push(task.key);
		const aborts = {};
		for (const [label, id] of labels) aborts[label] = controlled.probes.get(laneOf(id))?.aborts ?? 0;
		steps.push({ ...json(op), outcome, stored: json(store.value), aborts });
	}
	return { ids, keys, steps };
}

const out = { source: "packages/coding-agent/src/ultron/rlm/native-host.ts", scenarios: {} };
for (const [name, ops] of Object.entries(scenarios)) {
	const recorded = await run(ops);
	let text = JSON.stringify(recorded);
	recorded.ids.forEach((id, n) => {
		text = text.replaceAll(id, `ultron-task-00000000-0000-4000-8000-${String(n).padStart(12, "0")}`);
	});
	// Generated idempotency keys are random UUIDs; scenario-chosen keys are short and kept.
	recorded.keys
		.filter((key) => key.length === 36)
		.forEach((key, n) => {
			text = text.replaceAll(key, `00000000-0000-4000-9000-${String(n).padStart(12, "0")}`);
		});
	out.scenarios[name] = JSON.parse(text);
}
writeFileSync(join(here, "task-supervisor.json"), `${JSON.stringify(out, null, "\t")}\n`);
