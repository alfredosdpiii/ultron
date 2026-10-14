// Generates crates/hindsight/fixtures/exchanges.json: the TypeScript Hindsight client driven against the fake
// Hindsight server (fake_hindsight.py, the same server the Rust tests start), recording what each call returned and
// every HTTP request the server received.
//
// - backend: createHindsightBackend (ultron/memory.ts) built from each scenario's options, then its calls;
// - worker: createWorkerServices (ultron/worker-services.ts, what the session worker runs) for two sessions in two
//   project directories under one home, sharing one Hindsight bank, plus a worker with memory off;
// - lessons: HindsightLessons (ultron/autoreview/lessons.ts) retaining fates and rules;
// - urls: the Hindsight URL resolution of the session worker, autoreview and setup.
//
// Requests are recorded raw (request line, headers in order, body) and normalized: the server port becomes <port>,
// uuids become <uuid-N> in order of first appearance per scenario, ISO times become <time>. Transport headers the
// HTTP client adds on its own (not the Hindsight client) are dropped; see TRANSPORT_HEADERS.
// Usage: node crates/hindsight/fixtures/generate.mjs   (ULTRON_PYTHON, else /usr/bin/python3, runs the server)
import { spawn } from "node:child_process";
import { createServer } from "node:net";
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = join(resolve(here, "../../.."), "packages/coding-agent");
const load = (path) => import(pathToFileURL(join(pkg, path)).href);
// The global scope tag hashes the home directory; a fixed one keeps the fixture independent of the machine.
process.env.HOME = "/home/fixture";
const { createHindsightBackend } = await load("src/ultron/memory.ts");
const { createWorkerServices } = await load("src/ultron/worker-services.ts");
const { HindsightLessons, hindsightUrlFrom } = await load("src/ultron/autoreview/lessons.ts");
const { hindsightUrl } = await load("src/experimental/session-worker.ts");
const { effectiveHindsightUrl } = await load("src/cli/setup/core.ts");
const { DEFAULT_HINDSIGHT_URL } = await load("src/core/defaults.ts");

/**
 * Headers the HTTP stack adds by itself; the client under test sets none of them. undici adds all of these (with
 * `accept: *\/*`, compared as both stacks send it); reqwest adds only `host`.
 */
export const TRANSPORT_HEADERS = ["host", "connection", "user-agent", "accept-language", "accept-encoding", "sec-fetch-mode"];

const python = process.env.ULTRON_PYTHON || "/usr/bin/python3";
const server = spawn(python, ["-I", join(here, "fake_hindsight.py")], { stdio: ["pipe", "pipe", "inherit"] });
const port = await new Promise((done, fail) => {
	let text = "";
	server.stdout.on("data", (chunk) => {
		text += chunk;
		if (text.includes("\n")) done(Number(text.trim()));
	});
	server.on("exit", () => fail(new Error("fake Hindsight exited")));
});
const base = `http://127.0.0.1:${port}`;
const closed = await new Promise((done) => {
	const probe = createServer().listen(0, "127.0.0.1", () => {
		const { port: free } = probe.address();
		probe.close(() => done(free));
	});
});
const control = async (path, method = "GET") => (await fetch(`${base}${path}`, { method })).json();

/** `{base}` / `{closed}` in strings → this run's addresses. */
const fill = (value) => JSON.parse(JSON.stringify(value).replaceAll("{base}", base).replaceAll("{closed}", `http://127.0.0.1:${closed}`));

function normalizer() {
	const ids = new Map();
	return (value) => {
		let text = JSON.stringify(value).split(`127.0.0.1:${port}`).join("127.0.0.1:<port>").split(`127.0.0.1:${closed}`).join("127.0.0.1:<closed>");
		text = text.replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g, (id) => {
			if (!ids.has(id)) ids.set(id, `<uuid-${ids.size + 1}>`);
			return ids.get(id);
		});
		text = text.replace(/\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z/g, "<time>");
		return JSON.parse(text);
	};
}

/** The server's log since the last reset, without transport headers. */
async function requests() {
	return (await control("/__log")).map((entry) => ({
		line: entry.line,
		headers: entry.headers
			.map(([name, value]) => [name.toLowerCase(), value])
			.filter(([name]) => !TRANSPORT_HEADERS.includes(name))
			.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0)),
		body: entry.body,
	}));
}

const outcome = async (run) => {
	try {
		return { result: await run() };
	} catch (error) {
		return { error: error?.code ?? error?.message ?? String(error) };
	}
};

const tags = { session: ["ultron:session:s1"], project: ["ultron:project:p1"], global: ["ultron:global:g1"] };
const retain = (doc, text, operation = "op-1", tag = "ultron:session:s1") => ({
	async: true,
	operation_id: operation,
	items: [{ content: text, document_id: doc, tags: [tag], observation_scopes: [[tag]], update_mode: "replace", metadata: { ultron_operation: operation, ultron_evidence_class: "hypothesis" } }],
});
const recall = (query, tag = "ultron:session:s1") => ({ query, tags: [tag], tags_match: "exact", types: ["world", "experience", "observation"], budget: "mid", max_tokens: 4096, trace: false });
const opts = (extra) => ({ baseUrl: "{base}", bankId: "ultron", scopeTags: tags, ...extra });

const backendScenarios = [
	{
		name: "every endpoint",
		options: opts({}),
		calls: [
			{ op: "retain", arg: retain("doc-1", "The build uses cargo workspaces") },
			{ op: "operation", arg: "op-1" },
			{ op: "operation", arg: "op-unknown" },
			{ op: "recall", arg: recall("cargo build") },
			{ op: "recall", arg: recall("cargo", "ultron:project:p1") },
			{ op: "get", arg: "doc-1" },
			{ op: "get", arg: "doc-missing" },
			{ op: "delete", arg: "doc-1" },
			{ op: "delete", arg: "doc-1" },
			{ op: "retain", arg: { ...retain("doc-2", "sync text"), async: false } },
		],
	},
	{
		name: "bank created once before the first call",
		options: opts({ ensureBank: true }),
		calls: [
			{ op: "recall", arg: recall("anything") },
			{ op: "retain", arg: retain("doc-1", "unicode é ✓   \"quoted\" \\ \t tab") },
			{ op: "get", arg: "doc-1" },
		],
	},
	{
		name: "a failed bank creation is retried on the next call",
		options: opts({ ensureBank: true, bankId: "flaky-bank" }),
		calls: [
			{ op: "recall", arg: recall("x") },
			{ op: "recall", arg: recall("x") },
			{ op: "recall", arg: recall("x") },
		],
	},
	{
		name: "custom headers, content-type forced",
		options: opts({ headers: { Authorization: "Bearer token-1", "X-Extra": "v", "Content-Type": "text/plain" } }),
		calls: [{ op: "get", arg: "doc-1" }, { op: "recall", arg: recall("x") }],
	},
	{
		name: "repeated header names joined, Latin-1 value",
		options: opts({ headers: { "X-Extra": " v ", "x-extra": "w", "X-Latin": "café" } }),
		calls: [{ op: "get", arg: "doc-1" }],
	},
	{ name: "header value outside Latin-1", options: opts({ headers: { "x-a": "✓" } }), calls: [] },
	{ name: "empty header name", options: opts({ headers: { "": "x" } }), calls: [] },
	{
		name: "base path, encoded bank and ids",
		options: opts({ baseUrl: "{base}/hindsight/api/", bankId: "my bank/ü?#%" }),
		calls: [
			{ op: "get", arg: "doc/with space?#%&=+é" },
			{ op: "delete", arg: "a b" },
			{ op: "operation", arg: "op/1" },
			{ op: "get", arg: "!'()*-._~" },
			{ op: "get", arg: " " },
			{ op: "get", arg: "." },
			{ op: "get", arg: ".." },
			{ op: "get", arg: "" },
			{ op: "get", arg: "x".repeat(2049) },
		],
	},
	{ name: "HTTP 500", options: opts({ bankId: "fail-500" }), calls: [{ op: "recall", arg: recall("x") }] },
	{ name: "HTTP 404", options: opts({ baseUrl: "{base}/elsewhere/v2" }), calls: [{ op: "get", arg: "d" }] },
	{ name: "redirect refused", options: opts({ bankId: "redirect" }), calls: [{ op: "get", arg: "d" }] },
	{ name: "declared length over the limit", options: opts({ bankId: "big", maxResponseBytes: 1000 }), calls: [{ op: "recall", arg: recall("x") }] },
	{ name: "length within a raised limit", options: opts({ bankId: "big", maxResponseBytes: 4000 }), calls: [{ op: "recall", arg: recall("x") }] },
	{ name: "declared length over the limit refused before the body", options: opts({ bankId: "big-slow", maxResponseBytes: 1000, timeoutMs: 10000 }), calls: [{ op: "recall", arg: recall("x") }] },
	{ name: "streamed body over the limit", options: opts({ bankId: "big-chunked", maxResponseBytes: 1000 }), calls: [{ op: "recall", arg: recall("x") }] },
	{ name: "streamed body at the limit is read", options: opts({ bankId: "big-chunked", maxResponseBytes: 2000 }), calls: [{ op: "recall", arg: recall("x") }] },
	{ name: "invalid JSON", options: opts({ bankId: "bad-json" }), calls: [{ op: "recall", arg: recall("x") }] },
	{ name: "empty body", options: opts({ bankId: "empty" }), calls: [{ op: "get", arg: "d" }] },
	{ name: "no content", options: opts({ bankId: "no-content" }), calls: [{ op: "delete", arg: "d" }] },
	{ name: "slow answer times out", options: opts({ bankId: "slow", timeoutMs: 300 }), calls: [{ op: "recall", arg: recall("x") }] },
	{ name: "slow body times out", options: opts({ bankId: "slow-body", timeoutMs: 300 }), calls: [{ op: "recall", arg: recall("x") }] },
	{ name: "aborted while waiting", options: opts({ bankId: "slow" }), calls: [{ op: "recall", arg: recall("x"), abortAfterMs: 200 }] },
	{ name: "aborted before the call", options: opts({ ensureBank: true }), calls: [{ op: "recall", arg: recall("x"), abortBefore: true }] },
	{ name: "server unreachable", options: opts({ baseUrl: "{closed}", ensureBank: true }), calls: [{ op: "recall", arg: recall("x") }, { op: "get", arg: "d" }] },
	{ name: "scope tags kept", options: opts({ scopeTags: { project: ["ultron:project:p1", "extra"] } }), calls: [] },
	{ name: "normalized base URL", options: opts({ baseUrl: "HTTP://LocalHost:80/a/./b/../c" }), calls: [] },
	{ name: "https default port", options: opts({ baseUrl: "https://example.test:443" }), calls: [] },
	{ name: "query mark alone", options: opts({ baseUrl: "http://h.test/?" }), calls: [] },
	{ name: "IPv6 host", options: opts({ baseUrl: "http://[::1]:8888" }), calls: [] },
	{ name: "bank with spaces only", options: opts({ bankId: "  " }), calls: [] },
	{ name: "bank dot", options: opts({ bankId: "." }), calls: [] },
	{ name: "bank too long", options: opts({ bankId: "b".repeat(2049) }), calls: [] },
	{ name: "bank at length limit", options: opts({ bankId: "b".repeat(2048) }), calls: [] },
	{ name: "not a URL", options: opts({ baseUrl: "localhost:8888" }), calls: [] },
	{ name: "relative URL", options: opts({ baseUrl: "/v1" }), calls: [] },
	{ name: "ftp", options: opts({ baseUrl: "ftp://h.test" }), calls: [] },
	{ name: "credentials", options: opts({ baseUrl: "http://user:pw@h.test" }), calls: [] },
	{ name: "query", options: opts({ baseUrl: "http://h.test/?a=1" }), calls: [] },
	{ name: "fragment", options: opts({ baseUrl: "http://h.test/#x" }), calls: [] },
	{ name: "timeout zero", options: opts({ timeoutMs: 0 }), calls: [] },
	{ name: "timeout too long", options: opts({ timeoutMs: 120_001 }), calls: [] },
	{ name: "timeout at limit", options: opts({ timeoutMs: 120_000 }), calls: [] },
	{ name: "timeout fractional", options: opts({ timeoutMs: 1.5 }), calls: [] },
	{ name: "limit zero", options: opts({ maxResponseBytes: 0 }), calls: [] },
	{ name: "limit too large", options: opts({ maxResponseBytes: 16_777_217 }), calls: [] },
	{ name: "bad header name", options: opts({ headers: { "bad header": "x" } }), calls: [] },
	{ name: "bad header value", options: opts({ headers: { "x-a": "line\nbreak" } }), calls: [] },
	{ name: "unknown scope", options: opts({ scopeTags: { team: ["ultron:team:t"] } }), calls: [] },
	{ name: "scope tag without its scope", options: opts({ scopeTags: { session: ["ultron:project:p1"] } }), calls: [] },
	{ name: "empty scope tags", options: opts({ scopeTags: { session: [] } }), calls: [] },
	{ name: "duplicate scope tags", options: opts({ scopeTags: { session: ["ultron:session:a", "ultron:session:a"] } }), calls: [] },
	{ name: "bare scope tag", options: opts({ scopeTags: { session: ["session:x"] } }), calls: [] },
	{ name: "scope name inside a word", options: opts({ scopeTags: { session: ["ultron:xsession:abc"] } }), calls: [] },
	{ name: "scope tag with space", options: opts({ scopeTags: { session: ["ultron:session:a b"] } }), calls: [] },
];

async function runBackend(scenario) {
	await control("/__reset", "POST");
	const norm = normalizer();
	const record = { name: scenario.name, options: scenario.options };
	let backend;
	try {
		backend = createHindsightBackend(fill(scenario.options));
		record.created = { namespace: backend.namespace, scopeTags: backend.scopeTags };
	} catch (error) {
		record.created = { error: error.code ?? error.message };
	}
	record.calls = [];
	for (const call of scenario.calls) {
		const controller = new AbortController();
		if (call.abortBefore) controller.abort();
		const timer = call.abortAfterMs ? setTimeout(() => controller.abort(), call.abortAfterMs) : undefined;
		const result = await outcome(() => backend[call.op](call.arg, controller.signal));
		clearTimeout(timer);
		record.calls.push({ ...call, ...result });
	}
	// A slow handler logs its request on arrival and then stalls: wait until the log has it (each slow scenario
	// sends one request) instead of sleeping.
	if (scenario.calls.some((c) => c.abortAfterMs || /slow/.test(scenario.options.bankId ?? ""))) {
		while ((await control("/__log")).length === 0) await new Promise((done) => setTimeout(done, 10));
	}
	record.requests = await requests();
	return norm(record);
}

// Two session workers in two project directories under one home share one bank: a global memory proposed in alpha
// is recalled in beta; a project memory is not. Then a worker with memory off.
const workerSteps = [
	["alpha", "memory.propose", { text: "The build uses cargo workspaces", evidence: [{ ref: "Cargo.toml" }], scope: "global" }],
	["alpha", "memory.propose", { text: "Project alpha deploys with make", evidence: [{ ref: "Makefile" }], scope: "project" }],
	["alpha", "memory.propose", { text: "Session note about lunch plans", evidence: [{ ref: "chat" }] }],
	["alpha", "memory.get", { id: "$0" }],
	["alpha", "memory.prepare", { query: "cargo build", taskId: "t1", scope: "global" }],
	["alpha", "memory.correct", { id: "$0", text: "The build uses cargo workspaces and nextest", evidence: [{ ref: "Cargo.toml", sha256: "a".repeat(64) }], evidenceClass: "verified" }],
	["alpha", "memory.prepare", { query: "cargo build", taskId: "t2", scope: "global" }],
	["alpha", "memory.forget", { id: "$2" }],
	["alpha", "memory.get", { id: "$2" }],
	["alpha", "memory.forget", { id: "$2" }],
	["alpha", "memory.prepare", { query: "lunch", taskId: "t3" }],
	["alpha", "memory.propose", { text: "The deploy key is sk-ant-" + "api03-abcdefghijklmnopqrstuvwxyz0123456789", evidence: [{ ref: "env" }] }],
	["alpha", "memory.list", {}],
	["alpha", "memory.why", { taskId: "t2" }],
	["beta", "memory.prepare", { query: "cargo", taskId: "b1", scope: "global" }],
	["beta", "memory.prepare", { query: "deploys make", taskId: "b2", scope: "project" }],
	["beta", "memory.get", { id: "$0" }],
	["beta", "memory.list", {}],
	["alpha", "memory.prepare", { query: "deploys make", taskId: "t4", scope: "project" }],
	["down", "memory.propose", { text: "Unreachable server note", evidence: [{ ref: "x" }] }],
	["down", "memory.prepare", { query: "note", taskId: "d1" }],
	["down", "memory.list", {}],
	["off", "memory.list", {}],
	["off", "memory.prepare", { query: "x", taskId: "o1" }],
	["alpha", "memory.bogus", {}],
	["alpha", "memory.list", { extra: 1 }],
];
const workers = [
	{ name: "alpha", sessionId: "s-alpha", cwd: "/work/alpha", hindsightUrl: "{base}", bankId: "ultron" },
	{ name: "beta", sessionId: "s-beta", cwd: "/work/beta", hindsightUrl: "{base}", bankId: "ultron" },
	{ name: "down", sessionId: "s-down", cwd: "/work/down", hindsightUrl: "{closed}", bankId: "ultron" },
	{ name: "off", sessionId: "s-off", cwd: "/work/off" },
];

function sessionValues() {
	const values = new Map();
	return {
		getValue: async (address) => (values.has(JSON.stringify(address)) ? { value: structuredClone(values.get(JSON.stringify(address))) } : undefined),
		setValue: async (address, next) => void values.set(JSON.stringify(address), structuredClone(next)),
		scanValues: async () => [],
	};
}

async function runWorkers() {
	await control("/__reset", "POST");
	const norm = normalizer();
	const services = Object.fromEntries(
		workers.map((w) => [w.name, createWorkerServices({ ...fill(w), session: sessionValues() })]),
	);
	const ids = [];
	const steps = [];
	for (const [worker, type, payload] of workerSteps) {
		const resolved = JSON.parse(JSON.stringify(payload).replace(/"\$(\d+)"/g, (_, i) => JSON.stringify(ids[Number(i)])));
		const result = await outcome(() => services[worker].handle(type, resolved, {}));
		if (type === "memory.propose" && result.result?.memoryId) ids.push(result.result.memoryId);
		steps.push({ worker, type, payload, ...result });
	}
	return norm({ workers, steps, requests: await requests() });
}

const fates = [
	{ id: "f1", pull: "github.com/o/r#7", file: "src/a.rs", line: 12, category: "bug", kind: "fast", level: "high", claim: "Off by one in the loop bound", fate: "accepted", how: "resolved", at: "2026-01-02T03:04:05.006Z" },
	{ id: "f2", pull: "github.com/o/r#7", file: "src/b.rs", line: 3, category: "style", kind: "deep:naming", level: "low", claim: "Name is unclear", fate: "rejected", how: "thumbs down", at: "2026-01-02T03:04:05.006Z" },
];
const lessons = [
	{ category: "style", kind: "deep:naming", accepted: 1, rejected: 5, lowered: true },
	{ category: "bug", kind: "fast", accepted: 6, rejected: 0, lowered: false },
];

async function runLessons() {
	await control("/__reset", "POST");
	const norm = normalizer();
	const cases = [];
	for (const [name, baseUrl, repo] of [
		["retained", "{base}", "github.com/O/R"],
		["unreachable", "{closed}", "github.com/o/r"],
	]) {
		const memory = new HindsightLessons({ baseUrl: fill(baseUrl), now: () => 1_760_000_000_000 });
		cases.push({ name, baseUrl, repo, ...(await outcome(() => memory.retain(repo, fates, lessons))) });
	}
	return norm({ fates, lessons, cases, requests: await requests() });
}

function runUrls() {
	const inputs = [
		[undefined, undefined],
		["", undefined],
		["  ", "  "],
		[" http://h:1 ", undefined],
		[undefined, " http://s:2 "],
		["http://e:3", "http://s:4"],
		["OFF", "http://s:5"],
		["none", undefined],
		["0", undefined],
		["False", undefined],
		[undefined, "off"],
		["no", undefined],
		["  ", "off"],
	];
	return inputs.map(([env, setting]) => ({
		env: env ?? null,
		setting: setting ?? null,
		worker: hindsightUrl(env, setting) ?? null,
		autoreview: hindsightUrlFrom(env, setting, DEFAULT_HINDSIGHT_URL) ?? null,
		setup: effectiveHindsightUrl({ ULTRON_HINDSIGHT_URL: env }, setting),
	}));
}

const out = { transportHeaders: TRANSPORT_HEADERS, backend: [], worker: undefined, lessons: undefined, urls: runUrls() };
try {
	for (const scenario of backendScenarios) out.backend.push(await runBackend(scenario));
	out.worker = await runWorkers();
	out.lessons = await runLessons();
} finally {
	server.stdin.end();
}
writeFileSync(join(here, "exchanges.json"), `${JSON.stringify(out, null, 1)}\n`);
process.exit(0);
