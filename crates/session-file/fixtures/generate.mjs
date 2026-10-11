// Generates crates/session-file/fixtures/session-manager.json from the TypeScript SessionManager
// (packages/coding-agent/src/core/session-manager.ts). Each scenario is a list of steps that the Rust test
// interprets the same way; the generator records every step's result or error, a snapshot of the touched
// SessionManager (identity, entries, tree, projection, file bytes), and every file left in the scenario dir.
// Randomness and the clock are replaced by deterministic sources the Rust test reproduces:
//   - crypto.randomUUID(): a queue set by "ids" steps, then `${hex8(n)}-0000-4000-8000-${hex12(n)}` for n = 1, 2, ...
//   - crypto.getRandomValues (uuidv7): call k fills byte i with (k * 31 + i * 7) & 0xff
//   - Date: a fixed clock set by each step's `now` (ms since epoch)
// Paths are recorded relative to the scenario temp dir as "$TMP" ("$ETMP" where encoded into a session dir name),
// and the process cwd as "$CWD".
// Recorded strings longer than 4096 UTF-16 units become "$LONG:<length>:<first 32>:<last 32>"; a "write" content
// may be a list of strings and [string, count] repeats.
// Usage: node crates/session-file/fixtures/generate.mjs
import { execFileSync } from "node:child_process";
import crypto from "node:crypto";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
process.env.TZ = "UTC";

let idQueue = [];
let idCounter = 0;
const hex = (n, w) => n.toString(16).padStart(w, "0");
crypto.randomUUID = () => {
	if (idQueue.length) return idQueue.shift();
	idCounter++;
	return `${hex(idCounter, 8)}-0000-4000-8000-${hex(idCounter, 12)}`;
};
syncBuiltinESMExports();
let randomCalls = 0;
globalThis.crypto.getRandomValues = (bytes) => {
	randomCalls++;
	for (let i = 0; i < bytes.length; i++) bytes[i] = (randomCalls * 31 + i * 7) & 0xff;
	return bytes;
};
let clock = Date.UTC(2025, 0, 2, 3, 4, 5, 678);
const RealDate = Date;
globalThis.Date = class extends RealDate {
	constructor(...args) {
		if (args.length === 0) super(clock);
		else super(...args);
	}
	static now() {
		return clock;
	}
};

const smModule = await import(pathToFileURL(join(root, "packages/coding-agent/src/core/session-manager.ts")).href);
const {
	SessionManager,
	buildContextEntries,
	buildSessionContext,
	findMostRecentSession,
	loadEntriesFromFile,
	migrateSessionEntries,
	parseSessionEntries,
	getDefaultSessionDir,
} = smModule;

const expand = (c) => (typeof c === "string" ? c : c.map((p) => (typeof p === "string" ? p : p[0].repeat(p[1]))).join(""));
const shorten = (v) =>
	JSON.parse(JSON.stringify(v), (_k, x) =>
		typeof x === "string" && x.length > 4096 ? `$LONG:${x.length}:${x.slice(0, 32)}:${x.slice(-32)}` : x,
	);
const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));
const dateMs = (d) => (Number.isNaN(d.getTime()) ? null : d.getTime());

function treeJson(nodes) {
	return nodes.map((n) => ({
		entry: n.entry,
		children: treeJson(n.children),
		...(n.label !== undefined ? { label: n.label } : {}),
		...(n.labelTimestamp !== undefined ? { labelTimestamp: n.labelTimestamp } : {}),
	}));
}

function fileOrNull(path) {
	try {
		return readFileSync(path, "utf8");
	} catch {
		return null;
	}
}

function snapshot(sm) {
	const file = sm.getSessionFile();
	return clone({
		sessionId: sm.getSessionId(),
		sessionFile: file ?? null,
		cwd: sm.getCwd(),
		sessionDir: sm.getSessionDir(),
		persisted: sm.isPersisted(),
		leafId: sm.getLeafId(),
		header: sm.getHeader(),
		entries: sm.getEntries(),
		tree: treeJson(sm.getTree()),
		contextEntries: sm.buildContextEntries().map((e) => e.id),
		projection: sm.buildSessionProjection(),
		sessionName: sm.getSessionName() ?? null,
		labels: Object.fromEntries(sm.getEntries().flatMap((e) => (sm.getLabel(e.id) ? [[e.id, sm.getLabel(e.id)]] : []))),
		fileContent: file ? fileOrNull(file) : null,
	});
}

function sessionInfoJson(info) {
	return {
		path: info.path,
		id: info.id,
		cwd: info.cwd,
		name: info.name ?? null,
		parentSessionPath: info.parentSessionPath ?? null,
		created: dateMs(info.created),
		modified: dateMs(info.modified),
		messageCount: info.messageCount,
		firstMessage: info.firstMessage,
		allMessagesText: info.allMessagesText,
	};
}

function listFiles(dir) {
	const out = {};
	const walk = (d) => {
		for (const name of readdirSync(d).sort()) {
			const p = join(d, name);
			if (statSync(p).isDirectory()) walk(p);
			else out[relative(dir, p)] = readFileSync(p, "utf8");
		}
	};
	walk(dir);
	return out;
}

const { migrateSessionsFromAgentRoot } = await import(pathToFileURL(join(root, "packages/coding-agent/src/migrations.ts")).href);

// ---- messages used by scenarios ----
const usage = (n) => ({
	input: n,
	output: 1,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: n + 1,
	cost: { input: 0.1, output: 0.2, cacheRead: 0, cacheWrite: 0, total: 0.30000000000000004 },
});
const user = (text, timestamp = 1700000000000) => ({ role: "user", content: text, timestamp });
const assistant = (text, timestamp = 1700000000500, model = "claude-test") => ({
	role: "assistant",
	content: [{ type: "text", text }],
	api: "anthropic-messages",
	provider: "anthropic",
	model,
	usage: usage(10),
	stopReason: "stop",
	timestamp,
});
const toolResult = (text) => ({
	role: "toolResult",
	toolCallId: "call-1",
	toolName: "read",
	content: [{ type: "text", text }],
	details: { path: "a.txt" },
	isError: false,
	timestamp: 1700000000600,
});
const system = (content, extra = {}) => ({ role: "system", content, ...extra, timestamp: 1700000000001 });
const tool = (name) => ({ name, description: `${name} tool`, parameters: { type: "object", properties: {} } });

const header = (extra = {}) => JSON.stringify({ type: "session", version: 3, id: "sess-x", timestamp: "2025-01-01T00:00:00.000Z", cwd: "$TMP/proj", ...extra });
const line = (o) => JSON.stringify(o);

// ---- scenarios ----
// Step ops (sm = handle name):
//   create {sm, cwd, dir, options}            open {sm, path, dir?, cwdOverride?}
//   continueRecent {sm, cwd, dir?}            inMemory {sm, cwd, options?, entries?}
//   forkFrom {sm, source, targetCwd, dir?, options?}
//   call {sm, method, args, as?}  (args "=name" resolve to the result of the step with `as: name`)
//   write {path, content}                     utimes {path, mtime}       ids {queue}
//   loadEntries {path}   findMostRecent {dir, cwd?}   findById {cwd, id, dir?}
//   list {cwd, dir?}     listAll {dir?}       defaultSessionDir {cwd}
//   buildSessionContext {entries, leafId?}    buildContextEntries {entries, leafId?}
//   migrate {entries}    parse {content}       migrateAgentRoot {}  (src/migrations.ts migrateSessionsFromAgentRoot)
// The env var ULTRON_CODING_AGENT_DIR is "$TMP/agent" in every scenario.
const scenarios = [];
const S = (name, steps) => scenarios.push({ name, steps });

S("persisted_lifecycle", [
	{ op: "create", sm: "a", cwd: "$TMP/proj", dir: "$TMP/sessions", now: 1735787045678 },
	{ op: "call", sm: "a", method: "appendMessage", args: [user("hello")], now: 1735787046000 },
	{ op: "call", sm: "a", method: "appendThinkingLevelChange", args: ["high"], now: 1735787046001 },
	{ op: "call", sm: "a", method: "appendModelChange", args: ["openai", "gpt-5"], now: 1735787046002 },
	{ op: "call", sm: "a", method: "appendCustomEntry", args: ["ext-state", { n: 1 }], now: 1735787046003 },
	{ op: "call", sm: "a", method: "appendCustomEntry", args: ["no-data"], now: 1735787046004 },
	{ op: "call", sm: "a", method: "appendMessage", args: [assistant("hi there")], now: 1735787046005 },
	{ op: "call", sm: "a", method: "appendUsage", args: ["cache_warm", "anthropic", "claude-test", usage(3), "warm"], now: 1735787046006 },
	{ op: "call", sm: "a", method: "appendUsage", args: ["cache_warm", "anthropic", "claude-test", usage(4), ""], now: 1735787046007 },
	{ op: "call", sm: "a", method: "appendSessionInfo", args: ["  my\r\n\nsession  "], now: 1735787046008 },
	{ op: "call", sm: "a", method: "appendCustomMessageEntry", args: ["note", "injected", false, { k: "v" }], now: 1735787046009 },
	{ op: "call", sm: "a", method: "appendCustomMessageEntry", args: ["note", [{ type: "text", text: "shown" }], true], now: 1735787046010 },
	{ op: "call", sm: "a", method: "appendMessage", args: [toolResult("raw")], now: 1735787046011 },
	{ op: "call", sm: "a", method: "appendLabelChange", args: ["00000001", "start"], now: 1735787046012 },
	{ op: "call", sm: "a", method: "appendLabelChange", args: ["00000006", "reply"], now: 1735787046013 },
	{ op: "call", sm: "a", method: "appendLabelChange", args: ["00000001", undefined], now: 1735787046014 },
	{ op: "call", sm: "a", method: "appendLabelChange", args: ["00000001", "again"], now: 1735787046015 },
	{ op: "call", sm: "a", method: "appendLabelChange", args: ["missing", "x"], now: 1735787046016 },
	{ op: "call", sm: "a", method: "appendContextEdit", args: ["00000006", { content: "replaced reply" }], now: 1735787046017 },
	{ op: "call", sm: "a", method: "appendContextEdit", args: ["0000000b", null], now: 1735787046018 },
	{ op: "call", sm: "a", method: "appendContextEdit", args: ["00000001", { content: [{ type: "text", text: "edited hello" }] }], now: 1735787046019 },
	{ op: "call", sm: "a", method: "appendContextEdit", args: ["0000000a", { content: "custom edit" }], now: 1735787046020 },
	{ op: "call", sm: "a", method: "appendContextEdit", args: ["00000003", null], now: 1735787046021 },
	{ op: "call", sm: "a", method: "appendContextEdit", args: ["nope", null], now: 1735787046022 },
	{ op: "call", sm: "a", method: "appendContextEdit", args: ["00000001", { content: 5 }], now: 1735787046023 },
	{ op: "call", sm: "a", method: "appendContextEdit", args: ["00000001", {}], now: 1735787046024 },
	{ op: "call", sm: "a", method: "appendSessionInfo", args: [""], now: 1735787046025 },
	{ op: "call", sm: "a", method: "getChildren", args: ["00000001"] },
	{ op: "call", sm: "a", method: "branch", args: ["00000001"], now: 1735787046026 },
	{ op: "call", sm: "a", method: "appendContextEdit", args: ["0000000b", null], now: 1735787046027 },
	{ op: "call", sm: "a", method: "appendMessage", args: [user("second branch", 1700000001000)], now: 1735787046028 },
	{ op: "call", sm: "a", method: "branch", args: ["missing"] },
	{ op: "call", sm: "a", method: "branchWithSummary", args: ["00000006", "Summary of abandoned work", { files: [] }, true, usage(7)], now: 1735787046029 },
	{ op: "call", sm: "a", method: "branchWithSummary", args: ["missing", "x"] },
	{ op: "call", sm: "a", method: "appendMessage", args: [system("You are helpful.", { sections: { b: "B1", a: "A1", 10: "ten", 2: "two" }, toolsAdded: [tool("read"), tool("bash")] })], now: 1735787046030 },
	{ op: "call", sm: "a", method: "appendMessage", args: [system("", { sections: { a: null, b: "B2", c: "C1" }, toolsRemoved: [{ name: "read" }], toolsAdded: [tool("edit"), tool("bash")] })], now: 1735787046031 },
	{ op: "call", sm: "a", method: "appendMessage", args: [user("after system")], now: 1735787046032 },
	{ op: "call", sm: "a", method: "appendCompaction", args: ["Compacted", "00000006", 1234, { v: 1 }, false, usage(5)], now: 1735787046033 },
	{ op: "call", sm: "a", method: "appendMessage", args: [user("after compaction")], now: 1735787046034 },
	{ op: "call", sm: "a", method: "appendCompaction", args: ["Retain none", null, 99], now: 1735787046035 },
	{ op: "call", sm: "a", method: "appendMessage", args: [assistant("tail", 1700000002000, "other-model")], now: 1735787046036 },
	{ op: "call", sm: "a", method: "resetLeaf", args: [], now: 1735787046037 },
	{ op: "call", sm: "a", method: "appendMessage", args: [user("new root")], now: 1735787046038 },
	{ op: "call", sm: "a", method: "branchWithSummary", args: [null, "from root"], now: 1735787046039 },
	{ op: "open", sm: "b", path: "@a", now: 1735787046040 },
	{ op: "call", sm: "b", method: "appendMessage", args: [user("appended after reopen")], now: 1735787046041 },
	{ op: "open", sm: "c", path: "@a", dir: "$TMP/other", cwdOverride: "$TMP/override", now: 1735787046042 },
	{ op: "call", sm: "a", method: "newSession", args: [{ parentSession: "parent.jsonl" }], now: 1735787046043 },
	{ op: "call", sm: "a", method: "newSession", args: [{ id: "-bad" }], now: 1735787046044 },
	{ op: "call", sm: "a", method: "newSession", args: [{ id: "good.id_1-x" }], now: 1735787046045 },
	{ op: "call", sm: "a", method: "setSessionFile", args: ["@b"], now: 1735787046046 },
]);

S("deferred_flush_and_branched_sessions", [
	{ op: "create", sm: "a", cwd: "$TMP/proj", dir: "$TMP/s", options: { id: "created-id" }, now: 1735787045000 },
	{ op: "call", sm: "a", method: "appendMessage", args: [user("first question")], now: 1735787045001 },
	{ op: "call", sm: "a", method: "appendLabelChange", args: ["00000001", "q1"], now: 1735787045002 },
	{ op: "call", sm: "a", method: "appendModelChange", args: ["anthropic", "claude-test"], now: 1735787045003 },
	{ op: "call", sm: "a", method: "appendMessage", args: [assistant("first answer")], now: 1735787045004 },
	{ op: "call", sm: "a", method: "appendLabelChange", args: ["00000004", "a1"], now: 1735787045005 },
	{ op: "call", sm: "a", method: "appendMessage", args: [user("second question")], now: 1735787045006 },
	{ op: "call", sm: "a", method: "appendLabelChange", args: ["00000006", "q2"], now: 1735787045007 },
	{ op: "call", sm: "a", method: "appendCompaction", args: ["sum", "00000002", 10], now: 1735787045008 },
	{ op: "call", sm: "a", method: "appendMessage", args: [assistant("second answer")], now: 1735787045009 },
	{ op: "call", sm: "a", method: "createBranchedSession", args: ["missing"] },
	{ op: "call", sm: "a", method: "createBranchedSession", args: ["00000009"], now: 1735787045010 },
	{ op: "call", sm: "a", method: "createBranchedSession", args: ["00000001"], now: 1735787045011 },
	{ op: "call", sm: "a", method: "appendCustomEntry", args: ["preset-state", { name: "plan" }], now: 1735787045012 },
	{ op: "call", sm: "a", method: "appendMessage", args: [assistant("new answer")], now: 1735787045013 },
	{ op: "call", sm: "a", method: "appendMessage", args: [user("more")], now: 1735787045014 },
	{ op: "ids", queue: ["00000001-dup", "0000000e-dup", "abcdef01-0000-4000-8000-000000000000"] },
	{ op: "call", sm: "a", method: "appendMessage", args: [user("collision retry")], now: 1735787045015 },
	// Same id and clock as "a": the first flush must refuse to overwrite the existing file.
	{ op: "create", sm: "dup", cwd: "$TMP/proj", dir: "$TMP/s", options: { id: "created-id" }, now: 1735787045000 },
	{ op: "call", sm: "dup", method: "appendMessage", args: [user("dup question")], now: 1735787045001 },
	{ op: "call", sm: "dup", method: "appendMessage", args: [assistant("dup answer")], now: 1735787045002 },
	{ op: "inMemory", sm: "m", cwd: "$TMP/proj", options: { id: "mem" }, now: 1735787045016 },
	{ op: "call", sm: "m", method: "appendMessage", args: [user("hello")], as: "hello", now: 1735787045017 },
	{ op: "call", sm: "m", method: "appendLabelChange", args: ["=hello", "checkpoint"], now: 1735787045018 },
	{ op: "call", sm: "m", method: "appendModelChange", args: ["anthropic", "claude-test"], as: "model", now: 1735787045019 },
	{ op: "call", sm: "m", method: "appendMessage", args: [user("followup", 2)], as: "followup", now: 1735787045020 },
	{ op: "call", sm: "m", method: "appendLabelChange", args: ["=followup", "f"], now: 1735787045021 },
	{ op: "call", sm: "m", method: "appendLabelChange", args: ["=model", "m"], now: 1735787045021 },
	{ op: "call", sm: "m", method: "appendLabelChange", args: ["=model", ""], now: 1735787045021 },
	{ op: "call", sm: "m", method: "createBranchedSession", args: ["=followup"], now: 1735787045022 },
	{ op: "call", sm: "m", method: "appendMessage", args: [assistant("in memory")], now: 1735787045023 },
]);

S("open_edge_cases", [
	{ op: "write", path: "$TMP/s/empty.jsonl", content: "" },
	{ op: "open", sm: "e", path: "$TMP/s/empty.jsonl", dir: "$TMP/s", now: 1735787040000 },
	{ op: "open", sm: "e2", path: "$TMP/s/empty.jsonl", dir: "$TMP/s", now: 1735787040001 },
	{ op: "write", path: "$TMP/s/no-header.jsonl", content: '{"type":"message","id":"abc","parentId":"orphaned","timestamp":"2025-01-01T00:00:00Z","message":{"role":"assistant","content":"test"}}\n' },
	{ op: "open", sm: "x", path: "$TMP/s/no-header.jsonl", dir: "$TMP/s" },
	{ op: "write", path: "$TMP/s/not-a-session.log", content: '{"type":"event","data":"not a session"}\n' },
	{ op: "open", sm: "x", path: "$TMP/s/not-a-session.log", dir: "$TMP/s" },
	{ op: "write", path: "$TMP/s/garbage.jsonl", content: "not json at all" },
	{ op: "open", sm: "x", path: "$TMP/s/garbage.jsonl" },
	{ op: "open", sm: "n", path: "$TMP/s/sub/new.jsonl", now: 1735787040002 },
	{ op: "call", sm: "n", method: "appendMessage", args: [user("u")], now: 1735787040003 },
	{ op: "call", sm: "n", method: "appendMessage", args: [assistant("a")], now: 1735787040004 },
	{ op: "write", path: "$TMP/s/prefixed.jsonl", content: `\n  \nnot json\n{broken json\n${header({ id: "prefixed" })}\n` },
	{ op: "open", sm: "p", path: "$TMP/s/prefixed.jsonl", dir: "$TMP/s", now: 1735787040005 },
	// An opened file is already flushed: entries append before any assistant message.
	{ op: "call", sm: "p", method: "appendMessage", args: [user("appended before any assistant")], now: 1735787040010 },
	{ op: "call", sm: "e", method: "appendMessage", args: [user("into the initialized empty file")], now: 1735787040011 },
	{ op: "write", path: "$TMP/s/unterminated.jsonl", content: `${header({ id: "u" })}\n${line({ type: "message", id: "1", parentId: null, timestamp: "2025-01-01T00:00:01Z", message: { role: "user", content: "hi", timestamp: 1 } })}` },
	{ op: "loadEntries", path: "$TMP/s/unterminated.jsonl" },
	{ op: "write", path: "$TMP/s/malformed-tail.jsonl", content: `${header({ id: "mt" })}\n{"type":"message"` },
	{ op: "loadEntries", path: "$TMP/s/malformed-tail.jsonl" },
	{ op: "write", path: "$TMP/s/blank-tail.jsonl", content: `${header({ id: "bt" })}\n   ` },
	{ op: "loadEntries", path: "$TMP/s/blank-tail.jsonl" },
	{ op: "write", path: "$TMP/s/invalid-unterminated.jsonl", content: '{"type":"message","id":"1"}' },
	{ op: "loadEntries", path: "$TMP/s/invalid-unterminated.jsonl" },
	{ op: "loadEntries", path: "$TMP/s/does-not-exist.jsonl" },
	{ op: "write", path: "$TMP/s/odd-values.jsonl", content: `${header({ id: "odd" })}\nnull\n0\nfalse\n""\r\n${line({ type: "custom", id: "c1", parentId: null, timestamp: "x", customType: "t" })}\r\n[]\n` },
	{ op: "loadEntries", path: "$TMP/s/odd-values.jsonl" },
	{ op: "write", path: "$TMP/s/large-header.jsonl", content: [header({ id: "$A" }).split("$A")[0], ["a", 1024 * 1024 + 1], `${header({ id: "$A" }).split("$A")[1]}\n`] },
	{ op: "open", sm: "l", path: "$TMP/s/large-header.jsonl", dir: "$TMP/s", now: 1735787040006 },
	{ op: "write", path: "$TMP/s/large-prefix.jsonl", content: [["x", 1024 * 1024 + 1], `\n${header({ id: "large-prefix" })}\n`] },
	{ op: "open", sm: "l2", path: "$TMP/s/large-prefix.jsonl", dir: "$TMP/s", now: 1735787040007 },
	{ op: "open", sm: "l3", path: "$TMP/s/large-prefix.jsonl", dir: "$TMP/s", cwdOverride: "$TMP/ov", now: 1735787040008 },
	{ op: "write", path: "$TMP/s/no-cwd.jsonl", content: `${line({ type: "session", version: 3, id: "nocwd", timestamp: "2025-01-01T00:00:00Z", cwd: "$TMP/proj/../proj2/./x/" })}\n` },
	{ op: "open", sm: "nc", path: "$TMP/s/no-cwd.jsonl", now: 1735787040009 },
]);

const v1 = [
	line({ type: "session", id: "v1-session", timestamp: "2025-01-01T00:00:00Z", cwd: "$TMP/proj" }),
	line({ type: "message", timestamp: "2025-01-01T00:00:01Z", message: { role: "user", content: "hi", timestamp: 1 } }),
	line({ type: "message", timestamp: "2025-01-01T00:00:02Z", message: { role: "hookMessage", customType: "h", content: "from hook", display: true, timestamp: 2 } }),
	line({ type: "message", timestamp: "2025-01-01T00:00:03Z", message: assistant("hello") }),
	line({ type: "compaction", timestamp: "2025-01-01T00:00:04Z", summary: "s", firstKeptEntryIndex: 2, tokensBefore: 50 }),
	line({ type: "compaction", timestamp: "2025-01-01T00:00:05Z", summary: "s2", firstKeptEntryIndex: 0, tokensBefore: 60 }),
	line({ type: "compaction", timestamp: "2025-01-01T00:00:06Z", summary: "s3", firstKeptEntryId: "keep", firstKeptEntryIndex: 9, tokensBefore: 70 }),
	line({ type: "message", timestamp: "2025-01-01T00:00:07Z", message: user("after") }),
].join("\n");
const v2 = [
	line({ type: "session", version: 2, id: "v2-session", timestamp: "2025-01-01T00:00:00Z", cwd: "$TMP/proj", extra: { "2": "x", a: 1, "1": "y" } }),
	line({ type: "message", id: "abc12345", parentId: null, timestamp: "2025-01-01T00:00:01Z", message: { role: "hookMessage", content: "from a hook", timestamp: 1 } }),
	'{"type":"message","id":"n1","parentId":"abc12345","timestamp":"2025-01-01T00:00:02Z","message":{"role":"user","content":"caf\\u00e9 \\u2028 \\u0001 \\/ \\ud83d\\ude00","timestamp":1.0,"n":[1e21,1.5e-7,0.000001,123456789012345678901,-0,1E2,2.50,9007199254740993,1e-7,100e-2]}}',
].join("\n") + "\n";
S("migrations", [
	{ op: "write", path: "$TMP/s/v1.jsonl", content: v1 + "\n" },
	{ op: "open", sm: "v1", path: "$TMP/s/v1.jsonl", now: 1735787030000 },
	{ op: "write", path: "$TMP/s/v2.jsonl", content: v2 },
	{ op: "open", sm: "v2", path: "$TMP/s/v2.jsonl", now: 1735787030001 },
	{ op: "write", path: "$TMP/s/v3.jsonl", content: v2.replace('"version":2', '"version":3') },
	{ op: "open", sm: "v3", path: "$TMP/s/v3.jsonl", now: 1735787030002 },
	{ op: "write", path: "$TMP/s/vstr.jsonl", content: v2.replace('"version":2', '"version":"2"') },
	{ op: "open", sm: "vs", path: "$TMP/s/vstr.jsonl", now: 1735787030003 },
	{ op: "migrate", entries: [{ type: "session", id: "s", timestamp: "t", cwd: "/tmp" }, { type: "message", timestamp: "t", message: { role: "user", content: "hi", timestamp: 1 } }, { type: "message", timestamp: "t2", message: { role: "hookMessage", content: "h" } }] },
	{ op: "migrate", entries: [{ type: "session", id: "s", version: 2, timestamp: "t", cwd: "/tmp" }, { type: "message", id: "abc12345", parentId: null, timestamp: "t", message: { role: "user", content: "hi", timestamp: 1 } }] },
	{ op: "migrate", entries: [{ type: "message", timestamp: "t", message: { role: "hookMessage" } }] },
	{ op: "parse", content: '\n  {"type":"session","id":"x"}\nnot json\n\n5\nnull\n{"a":1}  \n\n' },
	{ op: "inMemory", sm: "h", cwd: "$TMP/proj", options: { id: "ignored" }, entries: [{ type: "session", version: 2, id: "v2-mem", timestamp: "2026-01-01T00:00:00Z", cwd: "/stored" }, { type: "message", id: "abc12345", parentId: null, timestamp: "2026-01-01T00:00:01Z", message: { role: "hookMessage", content: "from a hook", timestamp: 1 } }], now: 1735787030004 },
	{ op: "inMemory", sm: "hl", cwd: "$TMP/proj", entries: [{ type: "message", id: "abc12345", parentId: null, timestamp: "2026-01-01T00:00:01Z", message: { role: "hookMessage", content: "from a hook", timestamp: 1 } }, { type: "label", id: "l1", parentId: "abc12345", timestamp: "2026-01-01T00:00:02Z", targetId: "abc12345", label: "kept" }], now: 1735787030005 },
	{ op: "call", sm: "hl", method: "appendMessage", args: [user("continued")], now: 1735787030006 },
	{ op: "inMemory", sm: "em", cwd: "$TMP/proj", options: { id: "empty-session" }, entries: [], now: 1735787030007 },
]);

S("fork_and_discovery", [
	{ op: "create", sm: "a", cwd: "$TMP/proj", dir: "$TMP/flat", now: 1735787020000 },
	{ op: "call", sm: "a", method: "appendMessage", args: [user("from A", 1700000010000)], now: 1735787020001 },
	{ op: "call", sm: "a", method: "appendSessionInfo", args: ["Named A"], now: 1735787020002 },
	{ op: "call", sm: "a", method: "appendMessage", args: [assistant("reply to A", 1700000011000)], now: 1735787020003 },
	{ op: "create", sm: "b", cwd: "$TMP/projb", dir: "$TMP/flat", now: 1735787020004 },
	{ op: "call", sm: "b", method: "appendMessage", args: [{ role: "user", content: [{ type: "image", data: "x", mimeType: "image/png" }, { type: "text", text: "from" }, { type: "text", text: "B" }] }], now: 1735787020005 },
	{ op: "call", sm: "b", method: "appendMessage", args: [{ ...assistant("reply to B"), timestamp: undefined }], now: 1735787020006 },
	{ op: "call", sm: "b", method: "appendSessionInfo", args: ["temp"], now: 1735787020007 },
	{ op: "call", sm: "b", method: "appendSessionInfo", args: ["  "], now: 1735787020008 },
	{ op: "write", path: "$TMP/flat/header-only.jsonl", content: `${header({ id: "header-only", cwd: "$TMP/proj", timestamp: "2024-06-01T00:00:00.000Z", parentSession: "$TMP/parent.jsonl" })}\n` },
	{ op: "write", path: "$TMP/flat/bad-ts.jsonl", content: `${header({ id: "bad-ts", cwd: "$TMP/proj", timestamp: "not a date" })}\n${line({ type: "message", id: "m", parentId: null, timestamp: "also bad", message: { role: "user", content: "no time" } })}\n` },
	{ op: "write", path: "$TMP/flat/invalid.jsonl", content: '{"type":"message"}\n' },
	{ op: "write", path: "$TMP/flat/crlf.jsonl", content: `${header({ id: "crlf", cwd: "$TMP/projb" })}\r${line({ type: "message", id: "m", parentId: null, timestamp: "2025-01-01T00:00:00Z", message: user("cr line", 1700000020000) })}\r\n` },
	{ op: "write", path: "$TMP/flat/oversized.jsonl", content: [["x", 1024 * 1024 + 1]] },
	{ op: "write", path: "$TMP/flat/notes.txt", content: "hello" },
	{ op: "utimes", path: "@a", mtime: 1735787100000 },
	{ op: "utimes", path: "@b", mtime: 1735787200000 },
	{ op: "utimes", path: "$TMP/flat/header-only.jsonl", mtime: 1735787300000 },
	{ op: "utimes", path: "$TMP/flat/bad-ts.jsonl", mtime: 1735787250000 },
	{ op: "utimes", path: "$TMP/flat/invalid.jsonl", mtime: 1735787400000 },
	{ op: "utimes", path: "$TMP/flat/crlf.jsonl", mtime: 1735787150000 },
	{ op: "utimes", path: "$TMP/flat/oversized.jsonl", mtime: 1735787500000 },
	{ op: "findMostRecent", dir: "$TMP/flat" },
	{ op: "findMostRecent", dir: "$TMP/flat", cwd: "$TMP/projb" },
	{ op: "findMostRecent", dir: "$TMP/flat", cwd: "$TMP/nothing" },
	{ op: "findMostRecent", dir: "$TMP/missing" },
	{ op: "findById", cwd: "$TMP/proj", id: "header-only", dir: "$TMP/flat" },
	{ op: "findById", cwd: "$TMP/projb", id: "header-only", dir: "$TMP/flat" },
	{ op: "findById", cwd: "$TMP/proj", id: "nope", dir: "$TMP/flat" },
	{ op: "list", cwd: "$TMP/proj", dir: "$TMP/flat" },
	{ op: "list", cwd: "$TMP/projb", dir: "$TMP/flat" },
	{ op: "listAll", dir: "$TMP/flat" },
	{ op: "continueRecent", sm: "r", cwd: "$TMP/proj", dir: "$TMP/flat", now: 1735787020009 },
	{ op: "continueRecent", sm: "r2", cwd: "$TMP/fresh", dir: "$TMP/flat", now: 1735787020010 },
	{ op: "forkFrom", sm: "f", source: "@a", targetCwd: "$TMP/target", dir: "$TMP/forks", now: 1735787020011 },
	{ op: "forkFrom", sm: "f2", source: "@a", targetCwd: "$TMP/target", dir: "$TMP/forks", options: { id: "forked-id" }, now: 1735787020012 },
	{ op: "forkFrom", sm: "f3", source: "@a", targetCwd: "$TMP/target", dir: "$TMP/forks", options: { id: "bad id" }, now: 1735787020013 },
	{ op: "forkFrom", sm: "f4", source: "$TMP/flat/invalid.jsonl", targetCwd: "$TMP/target", dir: "$TMP/forks", now: 1735787020014 },
	{ op: "forkFrom", sm: "f5", source: "@a", targetCwd: "$TMP/target", dir: "$TMP/forks", options: { id: "forked-id" }, now: 1735787020012 },
	{ op: "forkFrom", sm: "f6", source: "@a", targetCwd: "$TMP/target", now: 1735787020015 },
	{ op: "defaultSessionDir", cwd: "$TMP/some/proj" },
	{ op: "create", sm: "d", cwd: "$TMP/proj", now: 1735787020016 },
	{ op: "call", sm: "d", method: "appendMessage", args: [user("default dir", 1700000030000)], now: 1735787020017 },
	{ op: "call", sm: "d", method: "appendMessage", args: [assistant("ok", 1700000031000)], now: 1735787020018 },
	{ op: "continueRecent", sm: "d2", cwd: "$TMP/proj", now: 1735787020019 },
	{ op: "list", cwd: "$TMP/proj" },
	{ op: "write", path: "$TMP/agent/sessions/--x--/old.jsonl", content: `${header({ id: "x-old", cwd: "/x" })}\n` },
	{ op: "utimes", path: "$TMP/agent/sessions/--x--/old.jsonl", mtime: 1600000000000 },
	{ op: "listAll" },
]);

S("agent_root_migration", [
	{ op: "migrateAgentRoot" },
	{ op: "write", path: "$TMP/agent/a.jsonl", content: `${header({ id: "a", cwd: "/proj/x" })}\n${line({ type: "message" })}\n` },
	{ op: "write", path: "$TMP/agent/b.jsonl", content: `${header({ id: "b", cwd: "" })}\n` },
	{ op: "write", path: "$TMP/agent/c.jsonl", content: `${line({ type: "event", cwd: "/proj/x" })}\n` },
	{ op: "write", path: "$TMP/agent/d.jsonl", content: `\n${header({ id: "d", cwd: "/proj/x" })}\n` },
	{ op: "write", path: "$TMP/agent/e.jsonl", content: `${header({ id: "e", cwd: "C:\\work\\repo" })}` },
	{ op: "write", path: "$TMP/agent/f.jsonl", content: `${header({ id: "f", cwd: "/proj/x" })}\n` },
	{ op: "write", path: "$TMP/agent/sessions/--proj-x--/f.jsonl", content: "already here\n" },
	{ op: "write", path: "$TMP/agent/g.jsonl", content: "null\n" },
	{ op: "write", path: "$TMP/agent/h.jsonl", content: `${header({ id: "h", cwd: 5 })}\n` },
	{ op: "write", path: "$TMP/agent/i.jsonl", content: `${header({ id: "i", cwd: "relative/dir/" })}\n` },
	{ op: "write", path: "$TMP/agent/notes.txt", content: "not a session" },
	{ op: "migrateAgentRoot" },
]);

const e = (o) => ({ timestamp: "2025-01-01T00:00:00Z", ...o });
const msg = (id, parentId, role, text) => e({ type: "message", id, parentId, message: role === "user" ? user(text, 1) : assistant(text, 1) });
const tree = [
	msg("1", null, "user", "start"),
	msg("2", "1", "assistant", "r1"),
	msg("3", "2", "user", "q2"),
	msg("4", "3", "assistant", "r2"),
	e({ type: "compaction", id: "5", parentId: "4", summary: "Compacted history", firstKeptEntryId: "3", tokensBefore: 1000 }),
	msg("6", "5", "user", "q3"),
	e({ type: "thinking_level_change", id: "6b", parentId: "6", thinkingLevel: "low" }),
	msg("7", "6b", "assistant", "r3"),
	msg("8", "3", "user", "wrong path"),
	msg("9", "8", "assistant", "wrong response"),
	e({ type: "branch_summary", id: "10", parentId: "3", summary: "Tried wrong approach", fromId: "9" }),
	e({ type: "branch_summary", id: "10b", parentId: "10", summary: "", fromId: "9" }),
	msg("11", "10b", "user", "better approach"),
	e({ type: "custom_message", id: "12", parentId: "11", customType: "c", content: null, display: true, timestamp: "bad" }),
	e({ type: "message", id: "13", parentId: "12", message: { role: "system", content: null, timestamp: 5 } }),
	e({ type: "message", id: "14", parentId: "13", message: { role: "toolResult", toolCallId: "t", toolName: "x", isError: false, timestamp: 6 } }),
	e({ type: "context_edit", id: "15", parentId: "14", targetId: "14", replacement: { content: "string for tool result" } }),
	e({ type: "context_edit", id: "16", parentId: "15", targetId: "12", replacement: { content: "custom edited" } }),
	e({ type: "context_edit", id: "17", parentId: "16", targetId: "13", replacement: { content: "system not edited" } }),
	e({ type: "model_change", id: "18", parentId: "17", provider: "openai", modelId: "gpt" }),
	e({ type: "compaction", id: "19", parentId: "4", summary: "c2", firstKeptEntryId: "2", tokensBefore: 5, systemMessage: system("sys") }),
	e({ type: "compaction", id: "20", parentId: "19", summary: "c3", firstKeptEntryId: "19", tokensBefore: 6 }),
	e({ type: "compaction", id: "21", parentId: "19", summary: "c4", firstKeptEntryId: "missing", tokensBefore: 6 }),
	e({ type: "message", id: "22", parentId: "21", message: { role: "system", content: "dropped before compaction?", timestamp: 7 } }),
	e({ type: "compaction", id: "23", parentId: "22", summary: "c5", firstKeptEntryId: "1", tokensBefore: 6 }),
];
const contextSteps = [];
for (const leafId of [undefined, null, "", "nonexistent", "1", "3", "7", "9", "11", "18", "19", "20", "21", "23"]) {
	contextSteps.push({ op: "buildSessionContext", entries: tree, leafId });
	contextSteps.push({ op: "buildContextEntries", entries: tree, leafId });
}
contextSteps.push({ op: "buildSessionContext", entries: [] });
contextSteps.push({ op: "buildSessionContext", entries: [msg("1", null, "user", "hello"), msg("2", "missing", "assistant", "orphan")], leafId: "2" });
S("pure_context", contextSteps);

S("tree_order", [
	{ op: "inMemory", sm: "t", cwd: "$TMP/proj", options: { id: "t" }, entries: [
		e({ type: "message", id: "r", parentId: null, message: user("root", 1) }),
		{ type: "message", id: "c3", parentId: "r", timestamp: "2025-01-01T00:00:03Z", message: user("third", 3) },
		{ type: "message", id: "c1", parentId: "r", timestamp: "2025-01-01T00:00:01Z", message: user("first", 1) },
		{ type: "message", id: "cx", parentId: "r", timestamp: "garbage", message: user("nan", 1) },
		{ type: "message", id: "c2", parentId: "r", timestamp: "2025-01-01T00:00:02+01:00", message: user("second", 2) },
		{ type: "message", id: "self", parentId: "self", timestamp: "2025-01-01", message: user("self", 2) },
		{ type: "message", id: "orphan", parentId: "gone", timestamp: "2025-01-01T00:00:00.5Z", message: user("orphan", 2) },
		{ type: "label", id: "lab", parentId: "orphan", timestamp: "2025-01-01T00:00:09Z", targetId: "c2", label: "two" },
	], now: 1735787010000 },
	{ op: "call", sm: "t", method: "getChildren", args: ["r"] },
	{ op: "call", sm: "t", method: "getBranch", args: ["c2"] },
	{ op: "call", sm: "t", method: "getBranch", args: ["nope"] },
	{ op: "call", sm: "t", method: "branch", args: ["c1"], now: 1735787010001 },
	{ op: "call", sm: "t", method: "appendContextEdit", args: ["c2", null], now: 1735787010002 },
	{ op: "call", sm: "t", method: "appendContextEdit", args: ["c1", { content: "edited" }], now: 1735787010003 },
	{ op: "call", sm: "t", method: "createBranchedSession", args: ["c1"], now: 1735787010004 },
]);

// Each scenario runs in a fresh node process so module state (uuidv7 sequence, id counter) starts clean.
const only = process.argv[2];
if (only === undefined) {
	const out = { generator: "crates/session-file/fixtures/generate.mjs", source: "packages/coding-agent/src/core/session-manager.ts", scenarios: [] };
	for (const scenario of scenarios) {
		const json = execFileSync(process.execPath, [fileURLToPath(import.meta.url), scenario.name], { encoding: "utf8", maxBuffer: 1 << 30 });
		out.scenarios.push(JSON.parse(json));
	}
	writeFileSync(join(here, "session-manager.json"), `${JSON.stringify(out, null, "\t")}\n`);
	process.exit(0);
}
for (const scenario of scenarios.filter((s) => s.name === only)) {
	const tmp = mkdtempSync(join(tmpdir(), "session-file-fixture-"));
	process.env.ULTRON_CODING_AGENT_DIR = join(tmp, "agent");
	const sub = (v) => JSON.parse(JSON.stringify(v ?? null).replaceAll("$TMP", tmp));
	// Default session dirs embed the temp dir encoded as a file name.
	const encodedTmp = tmp.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-");
	const unsub = (v) =>
		v === undefined
			? null
			: shorten(JSON.parse(JSON.stringify(v).replaceAll(tmp, "$TMP").replaceAll(encodedTmp, "$ETMP").replaceAll(process.cwd(), "$CWD")));
	const managers = {};
	const named = {};
	const path = (p) => (p.startsWith("@") ? managers[p.slice(1)].getSessionFile() : sub(p));
	const steps = [];
	for (const step of scenario.steps) {
		if (step.now !== undefined) clock = step.now;
		const recorded = { ...step };
		try {
			let result;
			switch (step.op) {
				case "create":
					managers[step.sm] = SessionManager.create(sub(step.cwd), step.dir === undefined ? undefined : sub(step.dir), step.options);
					break;
				case "open":
					managers[step.sm] = SessionManager.open(path(step.path), step.dir === undefined ? undefined : sub(step.dir), step.cwdOverride === undefined ? undefined : sub(step.cwdOverride));
					break;
				case "continueRecent":
					managers[step.sm] = SessionManager.continueRecent(sub(step.cwd), step.dir === undefined ? undefined : sub(step.dir));
					break;
				case "inMemory":
					managers[step.sm] = SessionManager.inMemory(sub(step.cwd), step.options, step.entries === undefined ? undefined : sub(step.entries));
					break;
				case "forkFrom":
					managers[step.sm] = SessionManager.forkFrom(path(step.source), sub(step.targetCwd), step.dir === undefined ? undefined : sub(step.dir), step.options);
					break;
				case "call": {
					const args = step.args.map((a) =>
						a === undefined ? undefined : typeof a === "string" && a.startsWith("@") ? path(a) : typeof a === "string" && a.startsWith("=") ? named[a.slice(1)] : sub(a),
					);
					result = managers[step.sm][step.method](...args);
					break;
				}
				case "write":
					mkdirSync(dirname(sub(step.path)), { recursive: true });
					writeFileSync(sub(step.path), expand(sub(step.content)));
					break;
				case "utimes":
					utimesSync(path(step.path), step.mtime / 1000, step.mtime / 1000);
					break;
				case "ids":
					idQueue = [...step.queue];
					break;
				case "loadEntries":
					result = loadEntriesFromFile(sub(step.path));
					recorded.fileAfter = fileOrNull(sub(step.path));
					break;
				case "findMostRecent":
					result = findMostRecentSession(sub(step.dir), step.cwd === undefined ? undefined : sub(step.cwd));
					break;
				case "findById":
					result = SessionManager.findById(sub(step.cwd), step.id, step.dir === undefined ? undefined : sub(step.dir));
					break;
				case "list":
					result = (await SessionManager.list(sub(step.cwd), step.dir === undefined ? undefined : sub(step.dir))).map(sessionInfoJson);
					break;
				case "listAll":
					result = (step.dir === undefined ? await SessionManager.listAll() : await SessionManager.listAll(sub(step.dir))).map(sessionInfoJson);
					break;
				case "defaultSessionDir":
					result = getDefaultSessionDir(sub(step.cwd));
					break;
				case "buildSessionContext":
					result = buildSessionContext(step.entries, step.leafId);
					break;
				case "buildContextEntries":
					result = buildContextEntries(step.entries, step.leafId).map((x) => x.id);
					break;
				case "migrate": {
					const entries = structuredClone(step.entries);
					migrateSessionEntries(entries);
					result = entries;
					break;
				}
				case "migrateAgentRoot":
					result = migrateSessionsFromAgentRoot();
					break;
				case "parse":
					result = parseSessionEntries(step.content);
					break;
				default:
					throw new Error(`unknown op ${step.op}`);
			}
			if (step.as) named[step.as] = result;
			recorded.result = unsub(result);
		} catch (error) {
			recorded.error = error.code ? { code: error.code } : { message: unsub(error.message) };
		}
		if (step.sm && managers[step.sm]) recorded.snapshot = unsub(snapshot(managers[step.sm]));
		if (step.op === "loadEntries") recorded.fileAfter = unsub(recorded.fileAfter);
		if ("leafId" in step && step.leafId === undefined) recorded.leafIdUndefined = true;
		steps.push(recorded);
	}
	const files = unsub(listFiles(tmp));
	rmSync(tmp, { recursive: true, force: true });
	process.stdout.write(JSON.stringify({ name: scenario.name, steps, files }));
}
