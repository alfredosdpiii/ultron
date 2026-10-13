// Generates crates/delegation/fixtures/reconcile.json from reconcile.ts (packages/coding-agent/src/ultron/
// reconcile.ts): the records of the A24 runs (ultron-a24-accounting-reconciliation.test.ts: a mixed run through
// NativeRlmHost with the usage ledger, progress and schedule modules; a crashed owner reopened), each with
// reconcileRecords(...) and inspectionDiscrepancies(...) of its agents.status, scoped and unscoped, plus tampered
// and synthetic documents (folded history, imported roots, unattributed and unknown calls, malformed input).
// Clock and ids are fixed. Usage: node crates/delegation/fixtures/generate-reconcile.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = join(here, "../../../packages/coding-agent/src/ultron");
const { inspectionDiscrepancies, reconcileRecords } = await import(join(src, "reconcile.ts"));
const { NativeRlmHost } = await import(join(src, "rlm/native-host.ts"));
const { createProgressModule } = await import(join(src, "progress.ts"));
const { createScheduleModule } = await import(join(src, "schedules.ts"));
const { NativeUsageLedger } = await import(join(src, "usage.ts"));

let ids = 0;
Object.defineProperty(globalThis.crypto, "randomUUID", { value: () => `id-${++ids}`, configurable: true });
const RealDate = Date;
let clock = 1_700_000_000_000;
globalThis.Date = class extends RealDate {
	constructor(...args) {
		super(...(args.length === 0 ? [clock] : args));
	}
	static now() {
		return clock;
	}
};

const context = {};
function durable() {
	let document;
	let generation = 0;
	return {
		get document() {
			return structuredClone(document);
		},
		open() {
			const mine = ++generation;
			return {
				read: async () => structuredClone(document),
				write: async (next) => {
					if (mine !== generation) throw new Error("owner fenced");
					document = structuredClone(next);
				},
			};
		},
	};
}
function assistant(id, text, usage) {
	return {
		id,
		type: "message",
		message: {
			role: "assistant",
			content: [{ type: "text", text }],
			...(usage === undefined
				? {}
				: {
						usage: {
							input: usage.input,
							output: usage.output,
							cacheRead: 0,
							cacheWrite: 0,
							totalTokens: usage.input + usage.output,
							cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: usage.cost },
						},
					}),
		},
	};
}
function fakeHarness() {
	const lanes = new Map();
	return {
		lane: async (name) => {
			let lane = lanes.get(name);
			if (!lane) {
				let entries = [];
				let release;
				lane = {
					getActiveTools: async () => [],
					setModel: async () => {},
					steer: async () => ({ ok: true, value: {} }),
					abort: async () => {
						release?.({ ok: false, error: { kind: "aborted" } });
						return { ok: true };
					},
					findEntries: async () => entries,
					prompt: async (text) => {
						clock += 3;
						if (text === "fail") return { ok: false, error: { kind: "provider", message: "503" } };
						if (text === "hang") return new Promise((resolve) => (release = resolve));
						entries =
							text === "unmetered"
								? [assistant("tip", "done"), { id: "from", type: "message", message: { role: "user" } }]
								: [
										assistant("tip", "done", { input: 100, output: 20, cost: 0.01 }),
										assistant("mid", "tool call", { input: 50, output: 10, cost: 0.005 }),
										{ id: "from", type: "message", message: { role: "user", content: text } },
									];
						return { ok: true, value: { status: "completed", tipId: "tip", fromTipId: "from" } };
					},
				};
				lanes.set(name, lane);
			}
			return lane;
		},
	};
}
const stores = () => ({ tasks: durable(), usage: durable(), progress: durable(), schedules: durable(), definitions: durable() });
const hosts = [];
function build(state) {
	const host = new NativeRlmHost(fakeHarness(), {}, {
		store: state.tasks.open(),
		definitionStore: state.definitions.open(),
		usage: new NativeUsageLedger(state.usage.open(), { limits: { maxAdmittedTasks: 24 } }),
		deterministic: async ({ definition, input }) => {
			clock += 1;
			if (definition.id === "check-pass") return { passed: true };
			if (definition.id === "check-fail") return { passed: false };
			return input;
		},
		modules: [
			createProgressModule({ store: state.progress.open() }),
			createScheduleModule({ store: state.schedules.open(), tickIntervalMs: 0 }),
		],
	});
	hosts.push(host);
	return (type, payload = {}) => host.handle(type, payload, context);
}
const check = (id) => ({
	id,
	version: "1",
	strategy: "deterministic",
	instructions: "Report whether the check passed.",
	inputSchema: {},
	outputSchema: { type: "object", properties: { passed: { type: "boolean" } }, required: ["passed"] },
	maxRepairs: 0,
	inputDescription: "Any JSON value",
	outputDescription: "{passed:boolean}",
});
const records = (state) => ({
	tasks: state.tasks.document,
	usage: state.usage.document,
	progress: state.progress.document,
	schedules: state.schedules.document,
});
const settle = async () => {
	for (let n = 0; n < 20; n++) await new Promise((r) => setImmediate(r));
};

const cases = [];
const add = (name, input, status) => {
	const entry = { name, input: structuredClone(input) };
	try {
		entry.reconciliation = reconcileRecords(input);
		if (status !== undefined) {
			entry.status = structuredClone(status);
			entry.inspection = inspectionDiscrepancies(entry.reconciliation, status);
		}
	} catch (error) {
		entry.error = error.message;
	}
	cases.push(entry);
};

// The mixed run.
{
	const state = stores();
	const call = build(state);
	await call("agents.register", { definition: check("check-pass") });
	await call("agents.register", { definition: check("check-fail") });
	await call("agents.invoke", { definition: "identity@1", input: { n: 1 }, key: "direct" });
	await call("agents.invoke", { definition: "identity@1", input: { n: 1 }, key: "direct" });
	await call("agents.invoke", { definition: "identity@1", input: { n: 2 }, key: "direct" }).catch(() => {});
	const metered = await call("agents.spawn", { definition: "rlm-child@1", input: { prompt: "work" } });
	await call("agents.result", { id: metered.id });
	const unmetered = await call("agents.spawn", { definition: "rlm-child@1", input: { prompt: "unmetered" } });
	await call("agents.result", { id: unmetered.id });
	const failed = await call("agents.spawn", { definition: "rlm-child@1", input: { prompt: "fail" } });
	await call("agents.result", { id: failed.id });
	const hanging = await call("agents.spawn", { definition: "rlm-child@1", input: { prompt: "hang" } });
	await settle();
	await call("agents.cancel", { id: hanging.id });
	clock += 10;
	await call("progress.report", { task_id: metered.id, summary: "computed answer", evidence: [{ kind: "file", ref: "out.txt" }] });
	clock += 10;
	await call("progress.reassess", { task_id: metered.id, claim: "complete", verifier: "check-pass@1" });
	clock += 10;
	const goal = await call("goals.create", { title: "release", required_checks: ["check-pass@1", "check-fail@1"] });
	await call("goals.verify", { id: goal.id });
	clock += 10;
	await call("schedules.create", { definition: "identity@1", input: { tick: true }, every_ms: 60_000 });
	clock += 60_000;
	await call("schedules.tick");
	await settle();
	const status = await call("agents.status");
	const all = records(state);
	add("mixed run", all, status);
	add("mixed run scoped", { ...all, rootId: "ultron-root" }, status);
	add("mixed run scoped to a missing root", { ...all, rootId: "elsewhere" }, status);
	add("journal and ledger only", { tasks: all.tasks, usage: all.usage }, status);
	const forged = structuredClone(status);
	forged.tasks[0].state = "failed";
	forged.tasks[1].parentId = "someone";
	delete forged.tasks[2].result;
	forged.tasks.push({ id: "ghost", definition: "x@1", state: "completed" });
	forged.usage.usage.calls += 1;
	forged.usage.activeReservations = 3;
	forged.usage.session.usage.cost = 1;
	add("forged inspection", all, forged);
	const dropped = structuredClone(all);
	const root = Object.values(dropped.usage.roots)[0];
	root.calls.splice(root.calls.findIndex((item) => item.kind === "task"), 1);
	root.calls.push({ ...root.calls.find((item) => item.kind === "model"), id: "stray", taskId: "nobody" });
	dropped.progress.receipts[0].task_id = "nobody";
	dropped.progress.decisions[0].claim.verifier_status = "failed";
	dropped.schedules.goals[0].verification.checks[0].status = "failed";
	dropped.schedules.goals[0].verification.status = "achieved";
	dropped.schedules.schedules[0].last_task_id = "nobody";
	add("tampered records", dropped);
}

// The crashed owner.
{
	const state = stores();
	const first = build(state);
	const done = await first("agents.spawn", { definition: "rlm-child@1", input: { prompt: "work" } });
	await first("agents.result", { id: done.id });
	await first("agents.spawn", { definition: "rlm-child@1", input: { prompt: "hang" } });
	await settle();
	add("before the crash", records(state));
	const restarted = build(state);
	const status = await restarted("agents.status");
	add("after the restart", records(state), status);
}

// Synthetic documents.
const call = (id, kind, extra = {}, usage = {}) => ({
	id,
	reservationId: `r-${id}`,
	rootId: "a",
	kind,
	admittedAt: 5,
	settledAt: 9,
	status: "succeeded",
	usage: { inputTokens: 1, outputTokens: 1, totalTokens: 2, cost: 0.5, wallMs: 4, ...usage },
	...extra,
});
const task = (id, key, state, extra = {}) => ({ id, key, fingerprint: "f".repeat(64), definition: "x@1", state, ...extra });
add("folded history and imports", {
	tasks: {
		version: 1,
		tasks: [
			task("t1", "k1", "completed", { result: { status: "succeeded", verification: "unverified" } }),
			task("t2", "k2", "failed", { result: { status: "failed", verification: "unverified" } }),
			task("t3", "k3", "running", { parentId: "t9" }),
			task("t4", "k4", "cancelled", { result: { status: "cancelled", verification: "unverified" } }),
		],
	},
	usage: {
		version: 1,
		roots: {
			a: {
				rootId: "a",
				startedAt: 1,
				deadlineAt: null,
				reservations: [
					{ id: "r3", rootId: "a", kind: "task", requestKey: "k3", admittedAt: 3, deadlineAt: null },
					{ id: "r9", rootId: "a", kind: "task", requestKey: "k9", admittedAt: 3, deadlineAt: null },
					{ id: "r10", rootId: "a", kind: "model", requestKey: "t3:model", admittedAt: 3, deadlineAt: null },
				],
				calls: [
					call("c1", "task", { requestKey: "k1" }),
					call("c1b", "task", { requestKey: "k1", admittedAt: 2, settledAt: 12 }),
					call("c2", "model", { taskId: "t1", settledAt: 7 }, { cost: null }),
					call("c3", "task", { requestKey: "nope", status: "failed" }),
					call("c4", "jev", {}, { wallMs: 1.5 }),
				],
			},
			pi: {
				rootId: "pi",
				startedAt: 1,
				deadlineAt: 1,
				reservations: [],
				calls: [call("p1", "model")],
				imported: { source: "pi", entries: 1 },
			},
		},
		history: {
			roots: 2,
			firstStartedAt: 0,
			lastSettledAt: 1,
			usage: { calls: 3, taskCalls: 2, modelCalls: 1, wallMs: 7, inputTokens: 3, outputTokens: 4, totalTokens: 7, cost: null, unknownCalls: 1 },
			byKind: {},
			spentUsd: 0,
			unknownPricedCalls: 0,
			taskAdmissions: { succeeded: 0, failed: 1, cancelled: 0, unknown: 0 },
			importedRoots: 0,
		},
	},
	progress: {
		version: 1,
		receipts: [{ id: "p1", task_id: "t1", at: 20, summary: "ok" }],
		decisions: [
			{ id: "d1", task_id: "t1", at: 21, assessment: { classification: "finished" }, claim: { decision: "verified", verifier_task_id: "t2", verifier_status: "failed" } },
			{ id: "d2", task_id: "t1", at: 2, assessment: { classification: "stalled" } },
			{ id: "d3", task_id: "t9", at: 22, assessment: { classification: "finished" }, claim: { decision: "verified", verifier_task_id: "t9" } },
		],
	},
	schedules: {
		goals: [
			{ id: "g1", state: "open", task_ids: ["t1", "zz"], verification: { status: "achieved", at: 30, checks: [{ definition: "c@1", task_id: "t2", status: "succeeded" }, { definition: "c@1", task_id: null, status: "skipped" }, { definition: "c@1", task_id: "t5" }] } },
			{ id: "g2", state: "open", verification: null },
		],
		schedules: [{ id: "s1", last_task_id: "t4", last_fired_at: 25 }, { id: "s2", last_task_id: null }],
	},
});
add("empty", { tasks: undefined, usage: undefined });
add("malformed journal", { tasks: { tasks: "x" }, usage: undefined });
add("malformed roots", { tasks: undefined, usage: { roots: [] } });
add("malformed call", { tasks: undefined, usage: { roots: { a: { calls: [{ kind: "task" }], reservations: [] } } } });
add("malformed progress", { tasks: undefined, usage: undefined, progress: { receipts: {} } });

for (const host of hosts) await host.close().catch(() => {});
const numbered = (pattern, label) => {
	const seen = new Map();
	return (text) =>
		text.replace(pattern, (match) => {
			if (!seen.has(match)) seen.set(match, `${label}#${seen.size}`);
			return seen.get(match);
		});
};
let text = JSON.stringify(cases, null, 1);
text = numbered(/ultron-task-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g, "ultron-task-")(text);
text = numbered(/\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/g, "uuid-")(text);
// Module work's inputs carry random ids, so their fingerprints vary from run to run.
text = numbered(/\b[0-9a-f]{64}\b/g, "fingerprint-")(text);
writeFileSync(join(here, "reconcile.json"), `${text}\n`);
