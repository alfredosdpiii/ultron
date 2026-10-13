// Generates crates/delegation/fixtures/usage.json from the TypeScript usage ledger (packages/coding-agent/src/ultron/
// usage.ts): scripted sessions of reserve / settle / status / recordTurn / budgetExhaustion / remaining / reconcile /
// importHistory against NativeUsageLedger with a fixed clock and numbered ids, recording each result or error and
// the stored document after every step; documents that fail validation on load; nativeUsageLimitsFromEnv and
// budgetLine. Usage: node crates/delegation/fixtures/generate-usage.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const usage = await import(join(here, "../../../packages/coding-agent/src/ultron/usage.ts"));
const { NativeUsageLedger, nativeUsageLimitsFromEnv, budgetLine } = usage;

let counter = 0;
Object.defineProperty(globalThis.crypto, "randomUUID", { value: () => `id-${++counter}`, configurable: true });

const encode = (value) => JSON.parse(JSON.stringify(value, (_key, item) => (Number.isNaN(item) ? "NaN" : item)));
const decodeNaN = (value) => JSON.parse(JSON.stringify(value), (_key, item) => (item === "NaN" ? Number.NaN : item));

async function session(name, options, steps, initial) {
	counter = 0;
	let now = 1_000;
	let document = initial;
	const store = {
		read: async () => structuredClone(document),
		write: async (next) => {
			document = structuredClone(next);
		},
	};
	const refused = [];
	const ledger = new NativeUsageLedger(store, { ...options, now: () => now, onRefused: (reason) => refused.push(reason) });
	const reservations = [];
	const ops = [];
	for (const step of steps) {
		const [op, ...args] = step;
		let result;
		try {
			if (op === "advance") {
				now += args[0];
				result = { ok: now };
			} else if (op === "reserve") {
				const reservation = await ledger.reserve(decodeNaN(args[0]));
				reservations.push(reservation);
				result = { ok: reservation };
			} else if (op === "settle") {
				result = { ok: (await ledger.settle(reservations[args[0]], decodeNaN(args[1]))) ?? null };
			} else if (op === "status") {
				result = { ok: await ledger.status(args[0]) };
			} else if (op === "recordTurn") {
				result = { ok: (await ledger.recordTurn(args[0] ?? undefined, decodeNaN(args[1] ?? {}))) ?? null };
			} else if (op === "exhaustion") {
				result = { ok: (await ledger.budgetExhaustion(args[0] ?? undefined)) ?? null };
			} else if (op === "remaining") {
				result = { ok: await ledger.remaining(args[0] ?? undefined) };
			} else if (op === "reconcile") {
				result = { ok: (await ledger.reconcile(args[0])) ?? null };
			} else if (op === "import") {
				result = { ok: (await ledger.importHistory(args[0], args[1], args[2])) ?? null };
			} else if (op === "ready") {
				result = { ok: (await ledger.ready()) ?? null };
			} else throw new Error(`unknown op ${op}`);
		} catch (error) {
			result = { error: error.message };
		}
		ops.push({ step: encode(step), ...encode(result), document: document ?? null });
	}
	return { name, options: encode(options), initial: initial ?? null, ops, refused };
}

const task = (key, extra = {}) => ({ kind: "task", requestKey: key, ...extra });
const model = (key, extra = {}) => ({ kind: "model", requestKey: key, ...extra });

const sessions = [
	await session("admission and wall deadline", { limits: { maxAdmittedTasks: 2, maxWallMs: 1_000 } }, [
		["reserve", task("a", { timeoutMs: 500, taskId: "t1" })],
		["advance", 10],
		["reserve", task("b", { timeoutMs: 5_000, taskId: "t2" })],
		["reserve", task("c")],
		["reserve", task("a")],
		["reserve", model("t1:model", { parentTaskId: "t1", taskId: "t1", deadlineAt: 1_400, budgetId: "frame-1" })],
		["advance", 20],
		["settle", 0, { status: "succeeded", usage: { inputTokens: 10, outputTokens: 5, totalTokens: 15, cost: 0.01 } }],
		["settle", 0, { status: "failed" }],
		["reserve", task("c")],
		["settle", 3, { status: "succeeded", usage: { inputTokens: 1, outputTokens: 2, totalTokens: 3, cost: null } }],
		["status"],
		["advance", 2_000],
		["reserve", task("d")],
		["reserve", model("late")],
		["status"],
		["settle", 1, {}],
		["settle", 4, { status: "maybe" }],
		["settle", 4, { usage: { cost: "NaN" } }],
		["settle", 4, { usage: { wallMs: -1 } }],
		["status", "elsewhere"],
	]),
	await session("validation of requests", {}, [
		["reserve", { kind: "jev" }],
		["reserve", task("  ")],
		["reserve", task("x", { budgetId: " " })],
		["reserve", task("x", { rootId: " " })],
		["reserve", task("x", { rootId: "a\u0000b" })],
		["reserve", task("x", { timeoutMs: 0 })],
		["reserve", task("x", { timeoutMs: 1.5 })],
		["reserve", task("x", { deadlineAt: 999 })],
		["reserve", task("y", { timeoutMs: 100 })],
		["status"],
	]),
	await session("goal roots and untimed work", { limits: { maxAdmittedTasks: 1, maxWallMs: 50 } }, [
		["reserve", task("g1", { rootId: "goal:release" })],
		["reserve", task("g2", { rootId: "goal:release" })],
		["advance", 10_000],
		["reserve", task("g3", { rootId: "goal:release", timeoutMs: 100 })],
		["status", "goal:release"],
		["reserve", task("j1", { rootId: "job:one" })],
		["reserve", task("j2", { rootId: "job:one" })],
	]),
	await session("turn, token and cost limits", { limits: { maxTotalTurns: 3, maxCostUsd: 0.05, defaultMaxTotalTokens: 100 } }, [
		["recordTurn", "turn:a", { totalTokens: 40, cost: 0.01 }],
		["exhaustion", "turn:a"],
		["remaining", "turn:a"],
		["recordTurn", "turn:a", { totalTokens: 70, cost: 0.01 }],
		["exhaustion", "turn:a"],
		["reserve", model("m1", { rootId: "turn:a" })],
		["reserve", task("det", { rootId: "turn:a", modelBacked: false })],
		["recordTurn", "turn:b", { totalTokens: 1 }],
		["exhaustion", "turn:b"],
		["recordTurn", "turn:c", { totalTokens: 1, cost: 0.06 }],
		["exhaustion", "turn:c"],
		["recordTurn", "goal:g", { totalTokens: 500, cost: 0.001 }],
		["recordTurn", "goal:g", { totalTokens: 1, cost: 0.001 }],
		["recordTurn", "goal:g", { totalTokens: 1, cost: 0.001 }],
		["exhaustion", "goal:g"],
		["remaining", "goal:g"],
		["status", "turn:a"],
		["recordTurn", null, { totalTokens: -5, cost: -1 }],
		["status"],
	]),
	await session("history import and reconcile", { limits: { maxAdmittedTasks: 9 } }, [
		["import", "pi:old", "pi", [{ at: 10, usage: { inputTokens: 1, outputTokens: 2, totalTokens: 3, cost: 0.5 } }, { at: 5, usage: {}, status: "failed" }]],
		["import", "pi:old", "pi", []],
		["reserve", task("x", { rootId: "pi:old" })],
		["recordTurn", "pi:old", { totalTokens: 9 }],
		["reserve", task("t1", { taskId: "t1" })],
		["reserve", model("t1:model", { taskId: "t1", parentTaskId: "t1", budgetId: "b" })],
		["reserve", task("t2", { taskId: "t2" })],
		["reserve", model("t2:model", { taskId: "t2", parentTaskId: "t2" })],
		["reserve", task("orphan")],
		["advance", 7],
		["reconcile", ["t1"]],
		["status"],
		["reconcile", []],
		["status"],
	]),
	await session("folding idle roots", { detailedRoots: 2, limits: { maxWallMs: 100 } }, [
		["reserve", task("a", { rootId: "r1" })],
		["settle", 0, { status: "succeeded" }],
		["advance", 1],
		["reserve", task("b", { rootId: "r2" })],
		["settle", 1, { status: "cancelled", usage: { inputTokens: 1, outputTokens: 1, totalTokens: 2, cost: 0.25 } }],
		["advance", 1],
		["reserve", model("c", { rootId: "r3" })],
		["advance", 200],
		["reserve", task("d", { rootId: "r4" })],
		["settle", 3, { status: "failed" }],
		["recordTurn", "r5", { totalTokens: 3, cost: 0.5 }],
		["reserve", task("e", { rootId: "r6" })],
		["settle", 2, {}],
		["reserve", task("f", { rootId: "r7" })],
		["status"],
	]),
	await session("reopen with open reservations", {}, [["ready"], ["status"]], {
		version: 1,
		roots: {
			"ultron-root": {
				rootId: "ultron-root",
				startedAt: 5,
				deadlineAt: null,
				reservations: [
					{ id: "r-1", rootId: "ultron-root", kind: "task", taskId: "t", requestKey: "k", admittedAt: 5, deadlineAt: null },
				],
				calls: [],
			},
		},
	}),
	await session("reconcile on reopen", {}, [["reconcile", []], ["status"]], {
		roots: {
			"ultron-root": {
				rootId: "ultron-root",
				startedAt: 5,
				deadlineAt: null,
				reservations: [
					{ id: "r-1", rootId: "ultron-root", kind: "jev", parentTaskId: "t", admittedAt: 5, deadlineAt: 900 },
				],
				calls: [],
				turns: { count: 2, tokens: 10, lastAt: 6 },
			},
		},
	}),
];

const invalidDocuments = [
	{ version: 2, roots: {} },
	{ version: 1 },
	{ version: 1, roots: { a: { rootId: "b", startedAt: null, deadlineAt: null, reservations: [], calls: [] } } },
	{ version: 1, roots: { a: { rootId: "a", deadlineAt: null, reservations: [], calls: [] } } },
	{ version: 1, roots: { a: { rootId: "a", startedAt: null, deadlineAt: null, reservations: {}, calls: [] } } },
	{ version: 1, roots: { a: { rootId: "a", startedAt: null, deadlineAt: null, reservations: [{ id: "x", rootId: "a", kind: "bogus", admittedAt: 1, deadlineAt: null }], calls: [] } } },
	{ version: 1, roots: { a: { rootId: "a", startedAt: null, deadlineAt: null, reservations: [{ id: "x", rootId: "a", kind: "task", admittedAt: -1, deadlineAt: null }], calls: [] } } },
	{ version: 1, roots: { a: { rootId: "a", startedAt: null, deadlineAt: null, reservations: [{ id: "x", rootId: "a", kind: "task", admittedAt: 1, deadlineAt: null }, { id: "x", rootId: "a", kind: "task", admittedAt: 1, deadlineAt: null }], calls: [] } } },
	{ version: 1, roots: { a: { rootId: "a", startedAt: null, deadlineAt: null, reservations: [], calls: [{ id: "c", reservationId: "r", rootId: "a", kind: "model", admittedAt: 1, settledAt: 2, status: "won", usage: { inputTokens: null, outputTokens: null, totalTokens: null, cost: null, wallMs: 1 } }] } } },
	{ version: 1, roots: { a: { rootId: "a", startedAt: null, deadlineAt: null, reservations: [], calls: [{ id: "c", reservationId: "r", rootId: "a", kind: "model", admittedAt: 1, settledAt: 2, status: "unknown", usage: { inputTokens: null, outputTokens: null, totalTokens: null, wallMs: 1 } }] } } },
	{ version: 1, roots: { a: { rootId: "a", startedAt: null, deadlineAt: null, reservations: [], calls: [], turns: { count: 1.5, tokens: 1, lastAt: 1 } } } },
	{ version: 1, roots: { a: { rootId: "a", startedAt: null, deadlineAt: null, reservations: [], calls: [], imported: { source: "", entries: 1 } } } },
	{ version: 1, roots: {}, history: { roots: 1 } },
	"not an object",
];
const invalid = [];
for (const document of invalidDocuments) {
	const ledger = new NativeUsageLedger({ read: async () => structuredClone(document), write: async () => {} });
	try {
		await ledger.ready();
		invalid.push({ document, ok: null });
	} catch (error) {
		invalid.push({ document, error: error.message });
	}
}

const envCases = [
	[{}, {}],
	[{ ULTRON_MAX_ADMITTED_TASKS: "0", ULTRON_MAX_WALL_MS: "off" }, {}],
	[{ ULTRON_MAX_ADMITTED_TASKS: "unlimited", ULTRON_MAX_WALL_MS: "1.5" }, {}],
	[{ ULTRON_MAX_COST_USD: "$2.50", ULTRON_MAX_TOTAL_TOKENS: "500", ULTRON_MAX_TOTAL_TURNS: "7" }, {}],
	[{ ULTRON_MAX_COST_USD: "-1", ULTRON_MAX_TOTAL_TOKENS: "off" }, { maxTotalTokens: 900 }],
	[{}, { maxTotalTokens: 900, maxTotalTurns: 3 }],
	[{ ULTRON_MAX_TOTAL_TURNS: "none" }, { maxTotalTurns: 3, maxTotalTokens: 0.5 }],
	[{ ULTRON_MAX_TOTAL_TOKENS: " 12 ", ULTRON_MAX_WALL_MS: "  " }, {}],
];
const env = envCases.map(([vars, settings]) => ({ vars, settings, limits: nativeUsageLimitsFromEnv(vars, settings) }));
const lineCases = [
	[{ turnsLeft: null, tokensLeft: 9_000_000, tokenCap: 10_000_000 }, false],
	[{ turnsLeft: null, tokensLeft: 4_000_000, tokenCap: 10_000_000 }, false],
	[{ turnsLeft: null, tokensLeft: 900, tokenCap: 1000 }, true],
	[{ turnsLeft: 3, tokensLeft: null, tokenCap: null }, false],
	[{ turnsLeft: 0, tokensLeft: 5, tokenCap: 10 }, false],
	[{ turnsLeft: null, tokensLeft: null, tokenCap: null }, true],
];
const lines = lineCases.map(([remaining, explicit]) => ({ remaining, explicit, line: budgetLine(remaining, explicit) ?? null }));

writeFileSync(join(here, "usage.json"), `${JSON.stringify({ sessions, invalid, env, lines }, null, 1)}\n`);
