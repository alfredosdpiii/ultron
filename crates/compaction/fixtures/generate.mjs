// Generates crates/compaction/fixtures/compaction.json from the TypeScript compaction module in this checkout
// (packages/coding-agent/src/core/compaction). Scripted session transcripts go through token estimation, cut-point
// search, compaction preparation, summary request construction (captured through `streamFn` with scripted
// responses) and branch summarization; the Rust tests replay every case and compare exactly.
// Usage: node crates/compaction/fixtures/generate.mjs [out.json]
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const mod = (p) => import(pathToFileURL(join(repo, "packages/coding-agent/src/core", p)).href);
const c = await mod("compaction/index.ts");
const { convertToLlm } = await mod("messages.ts");
const { buildSessionProjection } = await mod("session-manager.ts");

const NOW = 1_760_000_000_000;
Date.now = () => NOW;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

// ---------------------------------------------------------------------------
// Scripted transcripts
// ---------------------------------------------------------------------------
function usage(input, output, extra = {}) {
	return {
		input, output, cacheRead: 0, cacheWrite: 0, totalTokens: input + output,
		cost: { input: input / 1e6, output: output / 1e5, cacheRead: 0, cacheWrite: 0, total: input / 1e6 + output / 1e5 },
		...extra,
	};
}
const user = (text) => ({ role: "user", content: text, timestamp: 1 });
const userBlocks = (blocks) => ({ role: "user", content: blocks, timestamp: 1 });
const assistant = (content, extra = {}) => ({
	role: "assistant", content, api: "anthropic-messages", provider: "anthropic", model: "m1",
	usage: usage(0, 0), stopReason: "stop", timestamp: 2, ...extra,
});
const text = (t) => ({ type: "text", text: t });
const call = (id, name, args) => ({ type: "toolCall", id, name, arguments: args });
const result = (id, name, t, isError = false) => ({
	role: "toolResult", toolCallId: id, toolName: name, content: [text(t)], isError, timestamp: 3,
});
const long = (word, n) => Array.from({ length: n }, (_, i) => `${word}${i}`).join(" ");

class Tree {
	constructor() { this.entries = []; this.n = 0; this.leaf = null; }
	add(entry) {
		const id = `e${++this.n}`;
		this.entries.push({ ...entry, id, parentId: this.leaf, timestamp: `2025-01-01T00:00:${String(this.n).padStart(2, "0")}.000Z` });
		this.leaf = id;
		return id;
	}
	msg(message) { return this.add({ type: "message", message }); }
	at(id) { this.leaf = id; return this; }
	path(leaf = this.leaf) {
		const byId = new Map(this.entries.map((e) => [e.id, e]));
		const out = [];
		for (let e = byId.get(leaf); e; e = e.parentId ? byId.get(e.parentId) : undefined) out.push(e);
		return out.reverse();
	}
}

function basic() {
	const t = new Tree();
	t.add({ type: "thinking_level_change", thinkingLevel: "high" });
	t.msg({ role: "system", content: "You are Ultron.", sections: { tools: "rlm only", empty: null }, toolsAdded: [{ name: "rlm", description: "repl", parameters: { type: "object" } }], timestamp: 0 });
	t.msg(user("Refactor the parser in src/parse.ts please " + long("ctx", 30)));
	t.msg(assistant([{ type: "thinking", thinking: "plan: read first" }, text("Reading."), call("c1", "read", { path: "src/parse.ts" })], { usage: usage(1200, 40) }));
	t.msg(result("c1", "read", long("line", 700)));
	t.msg(assistant([call("c2", "edit", { path: "src/parse.ts", oldText: "a", newText: "b" }), call("c3", "write", { path: "src/new.ts", content: "x" })], { usage: usage(2400, 60) }));
	t.msg(result("c2", "edit", "ok"));
	t.msg(result("c3", "write", "ok"));
	t.add({ type: "label", targetId: "e3", label: "start" });
	t.msg({ role: "bashExecution", command: "npm test", output: long("out", 40), exitCode: 1, cancelled: false, truncated: true, fullOutputPath: "/tmp/full.log", timestamp: 4 });
	t.msg({ role: "bashExecution", command: "ls", output: "", exitCode: 0, cancelled: false, truncated: false, excludeFromContext: true, timestamp: 4 });
	t.msg({ role: "bashExecution", command: "pwd", output: "/w", exitCode: 0, cancelled: false, truncated: true, timestamp: 4 });
	t.msg({ role: "bashExecution", command: "sleep 9", output: "", exitCode: null, cancelled: true, truncated: false, timestamp: 4 });
	t.add({ type: "custom_message", customType: "note", content: "Remember the 80 column limit 🙂", display: true, details: { k: 1 } });
	t.msg(user("Now also read docs/README.md " + long("more", 20)));
	t.msg(assistant([call("c4", "read", { path: "docs/README.md" }), call("c5", "grep", { pattern: "x", path: "src" })], { usage: usage(3000, 30) }));
	t.msg(result("c4", "read", "readme body"));
	t.msg(result("c5", "grep", "match"));
	t.msg(assistant([text("Done with step one."), text("Second block.")], { usage: usage(0, 0), stopReason: "aborted" }));
	t.msg(userBlocks([text("Look at this screenshot"), { type: "image", data: "AAAA", mimeType: "image/png" }]));
	t.msg(assistant([text("I see a chart. " + long("desc", 50))], { usage: usage(5200, 90) }));
	t.msg(user("thanks"));
	return t;
}

function withCompaction() {
	const t = basic();
	const kept = t.entries.find((e) => e.type === "message" && e.message.role === "user" && String(e.message.content).startsWith("Now also")).id;
	t.add({ type: "compaction", summary: "## Goal\nRefactor parser", firstKeptEntryId: kept, tokensBefore: 9000, details: { readFiles: ["old/read.ts", "src/parse.ts"], modifiedFiles: ["old/mod.ts"] } });
	t.msg(user("continue " + long("after", 60)));
	t.msg(assistant([text("Continuing. " + long("work", 80)), call("c6", "edit", { path: "src/after.ts" })], { usage: usage(7000, 120) }));
	t.msg(result("c6", "edit", "ok"));
	t.add({ type: "model_change", provider: "openai", modelId: "gpt" });
	t.msg(assistant([text("Wrapping up " + long("wrap", 40))], { usage: usage(7600, 50) }));
	return t;
}

function hookedCompaction() {
	const t = withCompaction();
	const c0 = t.entries.find((e) => e.type === "compaction");
	c0.fromHook = true;
	return t;
}

function splitTurn() {
	const t = new Tree();
	t.msg(user("Big task " + long("req", 10)));
	t.msg(assistant([text("Step 1 " + long("s", 200)), call("t1", "read", { path: "a.ts" })], { usage: usage(800, 100) }));
	t.msg(result("t1", "read", long("aaa", 300)));
	t.msg(assistant([text("Step 2 " + long("s", 200)), call("t2", "write", { path: "b.ts", content: "q" })], { usage: usage(1600, 100) }));
	t.msg(result("t2", "write", "ok"));
	t.msg(assistant([text("Step 3 " + long("s", 200))], { usage: usage(2400, 100) }));
	return t;
}

function contextEdits() {
	const t = new Tree();
	t.msg(user("First " + long("u", 40)));
	t.msg(assistant([text("Answer " + long("a", 40))], { usage: usage(500, 50) }));
	t.msg(user("Second " + long("v", 40)));
	const bad = t.msg(assistant([text("Broken attempt " + long("b", 80))], { usage: usage(900, 70), stopReason: "error", errorMessage: "boom" }));
	t.add({ type: "context_edit", targetId: bad, replacement: null });
	const edited = t.msg(assistant([text("Retry " + long("r", 60))], { usage: usage(1300, 70) }));
	t.add({ type: "context_edit", targetId: edited, replacement: { content: "short replacement" } });
	t.add({ type: "custom", customType: "state", data: { x: 1 } });
	return t;
}

function recoveryOmission() {
	const t = new Tree();
	t.msg(user("Hello " + long("h", 60)));
	t.msg(assistant([text("Hi " + long("i", 60))], { usage: usage(400, 40) }));
	t.msg(user("Do it " + long("d", 60)));
	const a1 = t.msg(assistant([text("Attempt " + long("x", 60))], { stopReason: "error", errorMessage: "overloaded" }));
	t.add({ type: "context_edit", targetId: a1, replacement: null });
	return t;
}

function branchTree() {
	const t = new Tree();
	const root = t.msg(user("root question"));
	const a = t.msg(assistant([text("root answer")], { usage: usage(100, 10) }));
	t.msg(user("Explore option A " + long("optA", 30)));
	t.msg(assistant([text("Option A analysis " + long("anal", 60)), call("b1", "read", { path: "lib/a.ts" })], { usage: usage(400, 50) }));
	t.msg(result("b1", "read", "contents of a"));
	t.add({ type: "branch_summary", fromId: "zzz", summary: "Earlier exploration of option Z", details: { readFiles: ["lib/z.ts"], modifiedFiles: ["lib/zz.ts"] } });
	t.add({ type: "branch_summary", fromId: "yyy", summary: "Hooked summary", details: { readFiles: ["hook/r.ts"] }, fromHook: true });
	t.add({ type: "compaction", summary: "Compacted part of A", firstKeptEntryId: a, tokensBefore: 500 });
	t.msg(assistant([call("b2", "edit", { path: "lib/a.ts" }), text("Edited A " + long("ed", 30))], { usage: usage(700, 60) }));
	t.add({ type: "custom_message", customType: "hint", content: [text("custom block"), { type: "image", data: "BB", mimeType: "image/png" }], display: false });
	t.add({ type: "session_info", name: "named" });
	const oldLeaf = t.leaf;
	t.at(a);
	t.msg(user("Explore option B instead"));
	const target = t.msg(assistant([text("B")], { usage: usage(150, 5) }));
	return { t, oldLeaf, target, root, a };
}

const transcripts = {
	basic: basic().entries,
	withCompaction: withCompaction().entries,
	hookedCompaction: hookedCompaction().entries,
	splitTurn: splitTurn().entries,
	contextEdits: contextEdits().entries,
	recoveryOmission: recoveryOmission().entries,
	branch: branchTree().t.entries,
};

// ---------------------------------------------------------------------------
// Pure decisions
// ---------------------------------------------------------------------------
const BUDGETS = [0, 1, 20, 50, 150, 400, 900, 1500, 3000, 100000];
const prepJson = (p) =>
	p === undefined ? null : {
		firstKeptEntryId: p.firstKeptEntryId,
		messagesToSummarize: clone(p.messagesToSummarize),
		turnPrefixMessages: clone(p.turnPrefixMessages),
		isSplitTurn: p.isSplitTurn,
		tokensBefore: p.tokensBefore,
		previousSummary: p.previousSummary ?? null,
		fileOps: { read: [...p.fileOps.read], written: [...p.fileOps.written], edited: [...p.fileOps.edited] },
	};

const decisions = {};
for (const [name, entries] of Object.entries(transcripts)) {
	const projection = buildSessionProjection(entries);
	const allMessages = entries.flatMap((e) => (e.type === "message" ? [e.message] : []));
	decisions[name] = {
		entries,
		messageTokens: projection.messages.map((m) => c.estimateTokens(m)),
		rawMessageTokens: allMessages.map((m) => c.estimateTokens(m)),
		contextEstimate: c.estimateContextTokens(projection.messages),
		projectedEstimate: c.estimateProjectedContextTokens(projection, entries),
		lastAssistantUsage: clone(c.getLastAssistantUsage(entries)),
		serialized: c.serializeConversation(convertToLlm(allMessages)),
		cutPoints: BUDGETS.map((k) => ({ keep: k, result: c.findCutPoint(entries, 0, entries.length, k) })),
		turnStarts: entries.map((_, i) => c.findTurnStartIndex(entries, i, 0)),
		preparations: BUDGETS.map((k) => ({ keep: k, prep: prepJson(c.prepareCompaction(entries, { enabled: true, reserveTokens: 16384, keepRecentTokens: k })) })),
		branchPrep: [0, 30, 120, 400, 2000].map((budget) => {
			const p = c.prepareBranchEntries(entries, budget);
			return { budget, messages: clone(p.messages), totalTokens: p.totalTokens, fileOps: { read: [...p.fileOps.read], written: [...p.fileOps.written], edited: [...p.fileOps.edited] } };
		}),
	};
}

// A summary entry over the budget is still taken while the total stays under 90% of it.
const summaryBudget = (() => {
	const t = new Tree();
	t.add({ type: "branch_summary", fromId: "x", summary: "s".repeat(12) });
	t.add({ type: "compaction", summary: "c".repeat(12), firstKeptEntryId: "e1", tokensBefore: 1 });
	t.msg(user("u".repeat(36)));
	return [10, 11, 12, 13].map((budget) => ({ budget, entries: t.entries, messages: clone(c.prepareBranchEntries(t.entries, budget).messages) }));
})();

const shouldCompact = [];
for (const [tokens, window, reserve, enabled] of [[100, 1000, 100, true], [900, 1000, 100, true], [901, 1000, 100, true], [901, 1000, 100, false], [0, 0, 0, true], [5, 0, 10, true]]) {
	shouldCompact.push({ tokens, window, reserve, enabled, result: c.shouldCompact(tokens, window, { enabled, reserveTokens: reserve, keepRecentTokens: 1 }) });
}

const fileLists = [];
for (const ops of [
	{ read: ["b", "a", "z/ü", "z/😀", "z/￿"], written: [], edited: [] },
	{ read: ["a", "b", "c"], written: ["b"], edited: ["c", "d"] },
	{ read: [], written: [], edited: [] },
	{ read: [], written: ["w"], edited: [] },
]) {
	const set = { read: new Set(ops.read), written: new Set(ops.written), edited: new Set(ops.edited) };
	const lists = c.computeFileLists(set);
	fileLists.push({ ops, lists, formatted: c.formatFileOperations(lists.readFiles, lists.modifiedFiles) });
}

// ---------------------------------------------------------------------------
// Summarization requests through a scripted model
// ---------------------------------------------------------------------------
const model = (over = {}) => ({
	id: "sum-1", name: "Summarizer", api: "anthropic-messages", provider: "anthropic", baseUrl: "https://example.invalid",
	reasoning: true, input: ["text"], cost: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 }, contextWindow: 3000, maxTokens: 6000, ...over,
});
function scripted(responses) {
	const requests = [];
	const streamFn = async (m, context, options) => {
		const { signal, ...rest } = options;
		const sessionId = UUID.test(rest.sessionId ?? "") ? "<fresh>" : rest.sessionId;
		requests.push({ model: m.id, context: clone(context), options: clone({ ...rest, sessionId }) });
		const r = responses[Math.min(requests.length - 1, responses.length - 1)];
		return { result: async () => r };
	};
	return { requests, streamFn };
}
const reply = (content, extra = {}) => assistant(content, { usage: usage(300, 20, extra.usageExtra ?? {}), ...extra });

const compactCases = [];
const compactInputs = [
	{ name: "plain", transcript: "basic", keep: 400, responses: [reply([text("SUMMARY A")])] },
	{ name: "custom+thinking", transcript: "basic", keep: 400, customInstructions: "focus on tests", thinkingLevel: "high", sessionId: "sess-1", responses: [reply([{ type: "thinking", thinking: "hm" }, text("S1"), text("S2")])] },
	{ name: "thinking-off", transcript: "basic", keep: 400, thinkingLevel: "off", responses: [reply([text("X")])] },
	{ name: "no-reasoning-model", transcript: "basic", keep: 400, thinkingLevel: "low", model: { reasoning: false, maxTokens: 0 }, responses: [reply([text("X")])] },
	{ name: "update", transcript: "withCompaction", keep: 150, responses: [reply([text("UPDATED")])] },
	{ name: "hooked", transcript: "hookedCompaction", keep: 150, responses: [reply([text("H")])] },
	{ name: "split", transcript: "withCompaction", keep: 150, model: { maxTokens: 100 }, responses: [reply([text("HISTORY")], { usageExtra: { reasoning: 3 } }), reply([text("PREFIX")], { usageExtra: { cacheWrite1h: 2, reasoning: 4 } })] },
	{ name: "split-prefix-only", transcript: "splitTurn", keep: 900, responses: [reply([text("PREFIX ONLY")])] },
	{ name: "error", transcript: "basic", keep: 400, responses: [reply([], { stopReason: "error", errorMessage: "rate limited" })] },
	{ name: "error-empty", transcript: "basic", keep: 400, responses: [reply([], { stopReason: "error" })] },
	{ name: "length", transcript: "basic", keep: 400, responses: [reply([text("partial")], { stopReason: "length" })] },
	{ name: "tool-call", transcript: "basic", keep: 400, responses: [reply([call("x", "read", {})])] },
	{ name: "split-prefix-error", transcript: "withCompaction", keep: 150, responses: [reply([text("ok")]), reply([], { stopReason: "length" })] },
	{ name: "split-prefix-tool", transcript: "splitTurn", keep: 900, responses: [reply([call("x", "read", {})])] },
	{ name: "edits", transcript: "contextEdits", keep: 50, responses: [reply([text("E")])] },
];
for (const input of compactInputs) {
	const entries = transcripts[input.transcript];
	const prep = c.prepareCompaction(entries, { enabled: true, reserveTokens: 1000, keepRecentTokens: input.keep });
	if (!prep) throw new Error(`no preparation for ${input.name}`);
	const { requests, streamFn } = scripted(input.responses);
	let outcome;
	try {
		outcome = { ok: clone(await c.compact(prep, model(input.model), "key", undefined, input.customInstructions, undefined, input.thinkingLevel, streamFn, undefined, undefined, undefined, input.sessionId)) };
	} catch (e) {
		outcome = { error: e.message };
	}
	compactCases.push({ ...input, model: model(input.model), reserveTokens: 1000, requests, outcome });
}

const { t: bt, oldLeaf, target, root, a } = branchTree();
const tree = bt.entries;
const session = {
	getBranch: (id) => bt.path(id),
	getEntry: (id) => tree.find((e) => e.id === id),
};
const collect = [
	[oldLeaf, target], [target, oldLeaf], [null, target], [oldLeaf, oldLeaf], [oldLeaf, root], [oldLeaf, a], [target, "missing"],
].map(([from, to]) => ({ from, to, result: c.collectEntriesForBranchSummary(session, from, to) }));
const collected = c.collectEntriesForBranchSummary(session, oldLeaf, target).entries;

const branchCases = [];
const branchInputs = [
	{ name: "plain", entries: collected, responses: [reply([text("BRANCH")])] },
	{ name: "custom", entries: collected, customInstructions: "only files", responses: [reply([text("C")])] },
	{ name: "replace", entries: collected, customInstructions: "Just list files.", replaceInstructions: true, responses: [reply([text("R")])] },
	{ name: "replace-without-custom", entries: collected, replaceInstructions: true, responses: [reply([text("R")])] },
	{ name: "budget", entries: collected, model: { contextWindow: 400, maxTokens: 1000 }, reserveTokens: 300, responses: [reply([text("small")])] },
	{ name: "default-window", entries: collected, model: { contextWindow: 0, maxTokens: 0 }, responses: [reply([text("w")])] },
	{ name: "empty", entries: [], responses: [reply([text("never")])] },
	{ name: "only-metadata", entries: tree.filter((e) => e.type === "session_info"), responses: [reply([text("never")])] },
	{ name: "aborted", entries: collected, responses: [reply([], { stopReason: "aborted" })] },
	{ name: "error", entries: collected, responses: [reply([], { stopReason: "error", errorMessage: "nope" })] },
	{ name: "length", entries: collected, responses: [reply([text("p")], { stopReason: "length" })] },
	{ name: "tool", entries: collected, responses: [reply([call("q", "edit", {})])] },
	{ name: "basic-transcript", entries: transcripts.basic, responses: [reply([text("b")])] },
];
for (const input of branchInputs) {
	const { requests, streamFn } = scripted(input.responses);
	const out = await c.generateBranchSummary(input.entries, {
		model: model(input.model), apiKey: "key", signal: new AbortController().signal,
		customInstructions: input.customInstructions, replaceInstructions: input.replaceInstructions, reserveTokens: input.reserveTokens, streamFn,
	});
	branchCases.push({ ...input, model: model(input.model), requests, outcome: clone(out) });
}

const fixture = { generator: "crates/compaction/fixtures/generate.mjs", now: NOW, decisions, summaryBudget, shouldCompact, fileLists, compactCases, collect, tree, branchCases };
const out = process.argv[2] ?? join(here, "compaction.json");
writeFileSync(out, `${JSON.stringify(fixture, null, 1)}\n`);
