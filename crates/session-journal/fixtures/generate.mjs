// Generates crates/session-journal/fixtures/storage.json from the TypeScript format-4 session journal
// (packages/agent/src/harness/session/{session,context}.ts and jsonl/{repo,storage,fork,legacy-v3}.ts).
// Each scenario is a list of operations run against a JsonlSessionRepo in a private temp sessions root; the fixture
// records every operation's result and the session files (relative path -> exact bytes) whenever they change.
// Date.now is a counter (+1 per reading) and crypto.getRandomValues yields zeros, so ids, timestamps and files are
// deterministic; the Rust test replays the operations with TestClock and compares results and bytes exactly.
// Usage: node crates/session-journal/fixtures/generate.mjs
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { registerHooks } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");

const packages = new Map();
for (const dir of readdirSync(join(root, "packages"))) {
	try {
		const manifest = JSON.parse(readFileSync(join(root, "packages", dir, "package.json"), "utf8"));
		packages.set(manifest.name, { dir, exports: manifest.exports ?? {} });
	} catch {}
}
registerHooks({
	resolve(specifier, context, next) {
		const match = /^(@ultron\/[^/]+)(\/.*)?$/.exec(specifier);
		const pkg = match && packages.get(match[1]);
		if (!pkg) return next(specifier, context);
		const sub = `.${match[2] ?? ""}`;
		let target;
		for (const [key, value] of Object.entries(pkg.exports)) {
			const built = typeof value === "string" ? value : value.import;
			if (key === sub) target = built;
			else if (key.endsWith("*") && sub.startsWith(key.slice(0, -1)))
				target = built.replace("*", sub.slice(key.length - 1));
		}
		if (!target) return next(specifier, context);
		const source = target.replace(/^\.\/dist\//, "./src/").replace(/\.js$/, ".ts");
		return { url: pathToFileURL(join(root, "packages", pkg.dir, source)).href, shortCircuit: true };
	},
});

let clock = 1_790_000_000_000;
Date.now = () => clock++;
globalThis.crypto.getRandomValues = (bytes) => {
	bytes.fill(0);
	return bytes;
};

const harness = (path) => import(pathToFileURL(join(root, "packages/agent/src/harness", path)).href);
const { JsonlSessionRepo } = await harness("session/jsonl/repo.ts");
const { buildSessionContext } = await harness("session/context.ts");
const { BACKGROUND_CONTEXT: ctx } = await harness("context.ts");
const { NodeExecutionEnv } = await harness("env/nodejs.ts");

const user = (text, timestamp) => ({ role: "user", content: [{ type: "text", text }], timestamp });
const assistant = (text, timestamp, extra = {}) => ({
	role: "assistant",
	content: [{ type: "text", text }],
	api: "anthropic-messages",
	provider: "anthropic",
	model: "claude-sonnet-4-5",
	usage: { input: 3, output: 4, cacheRead: 0, cacheWrite: 0, totalTokens: 7, cost: { input: 0.1, output: 0.2, cacheRead: 0, cacheWrite: 0, total: 0.30000000000000004 } },
	stopReason: "stop",
	timestamp,
	...extra,
});
const lane = (provider = "anthropic", modelId = "claude-sonnet-4-5") => [
	{ kind: "value", op: "set", namespace: "pi.lane.config", key: "main", value: { model: { provider, modelId }, thinkingLevel: "medium", activeToolNames: ["rlm"] } },
	{ kind: "value", op: "set", namespace: "pi.lane.state", key: "main", value: { currentOperationId: null, lastOperationId: null, inbox: [] } },
];
const v3Header = (id, cwd, extra = {}) => JSON.stringify({ type: "session", version: 3, id, timestamp: "2026-01-01T00:00:00.000Z", cwd, ...extra });
const v3 = (record) => JSON.stringify(record);
const v3Usage = { input: 1, output: 2, cacheRead: 0, cacheWrite: 0, totalTokens: 3, cost: { input: 0.001, output: 0.002, cacheRead: 0, cacheWrite: 0, total: 0.003 } };
const v3File = (id, cwd) =>
	[
		v3Header(id, cwd),
		v3({ type: "model_change", id: "m1", parentId: null, timestamp: "2026-01-01T00:00:01.000Z", provider: "anthropic", modelId: "claude-sonnet-4-5" }),
		v3({ type: "thinking_level_change", id: "t1", parentId: "m1", timestamp: "2026-01-01T00:00:01.000Z", thinkingLevel: "high" }),
		v3({ type: "message", id: "u1", parentId: "t1", timestamp: "2026-01-01T00:00:02.000Z", message: user("first", 1767225602000) }),
		v3({ type: "message", id: "a1", parentId: "u1", timestamp: "2026-01-01T00:00:03.000Z", message: { ...assistant("one", 1767225603000), usage: v3Usage } }),
		v3({ type: "message", id: "u2", parentId: "a1", timestamp: "2026-01-01T00:00:04.000Z", message: user("second", 1767225604000) }),
		v3({ type: "message", id: "a2", parentId: "u2", timestamp: "2026-01-01T00:00:05.000Z", message: { ...assistant("two", 1767225605000), usage: v3Usage } }),
		v3({ type: "label", id: "l1", parentId: "a2", timestamp: "2026-01-01T00:00:06.000Z", targetId: "u1", label: "start" }),
		v3({ type: "label", id: "l2", parentId: "l1", timestamp: "2026-01-01T00:00:06.500Z", targetId: "a1", label: "gone" }),
		v3({ type: "label", id: "l3", parentId: "l2", timestamp: "2026-01-01T00:00:06.600Z", targetId: "a1" }),
		v3({ type: "compaction", id: "c1", parentId: "l3", timestamp: "2026-01-01T00:00:07.000Z", summary: "summary text", firstKeptEntryId: "u2", tokensBefore: 1234, usage: v3Usage }),
		v3({ type: "custom_message", id: "cm1", parentId: "c1", timestamp: "2026-01-01T00:00:08.000Z", customType: "note", content: "a note", display: true }),
		v3({ type: "custom", id: "cu1", parentId: "cm1", timestamp: "2026-01-01T00:00:08.500Z", customType: "ext", data: { k: 1 } }),
		v3({ type: "message", id: "u3", parentId: "cu1", timestamp: "2026-01-01T00:00:09.000Z", message: user("third", 1767225609000) }),
		v3({ type: "branch_summary", id: "b1", parentId: "u1", timestamp: "2026-01-01T00:00:10.000Z", fromId: "root", summary: "branch summary" }),
		v3({ type: "message", id: "u4", parentId: "b1", timestamp: "2026-01-01T00:00:11.000Z", message: user("side", 1767225611000) }),
		v3({ type: "active_tools_change", id: "at1", parentId: "u4", timestamp: "2026-01-01T00:00:12.000Z", activeToolNames: ["read"] }),
		v3({ type: "session_info", id: "si1", parentId: "at1", timestamp: "2026-01-01T00:00:13.000Z", name: "legacy session" }),
	].join("\n") + "\n";

const scenarios = [
	{
		name: "create-append-reopen",
		ops: [
			{ op: "create", s: "a", cwd: "/work/project", id: "11111111-aaaa" },
			{ op: "createBranch", s: "a", branch: "main", at: null },
			{ op: "commit", s: "a", writes: lane() },
			{ op: "append", s: "a", message: user("hello", 5) },
			{ op: "append", s: "a", message: assistant("hi", 6) },
			{ op: "setName", s: "a", name: "  my session " },
			{ op: "dump", s: "a" },
			{ op: "commit", s: "a", writes: [{ kind: "usage", row: { id: "usage-1", usage: assistant("x", 0).usage, adjustment: false } }] },
			{ op: "commit", s: "a", writes: [{ kind: "list", op: "append", namespace: "x.list", key: "k", value: 1 }, { kind: "list", op: "append", namespace: "x.list", key: "k", value: { two: 2 } }, { kind: "value", op: "set", namespace: "x.v", key: "b", value: "B" }, { kind: "value", op: "set", namespace: "x.v", key: "a", value: [1, 2.5] }] },
			{ op: "commit", s: "a", writes: [{ kind: "value", op: "delete", namespace: "x.v", key: "b" }, { kind: "list", op: "delete", namespace: "x.list", key: "k" }] },
			{ op: "commit", s: "a", writes: [] },
			{ op: "setName", s: "a", name: null },
			{ op: "close", s: "a" },
			{ op: "list", cwd: "/work/project" },
			{ op: "open", s: "a" },
			{ op: "dump", s: "a" },
			{ op: "context", s: "a" },
			{ op: "append", s: "a", message: user("again", 7) },
			{ op: "dump", s: "a" },
		],
	},
	{
		name: "torn-tail",
		ops: [
			{ op: "create", s: "a", cwd: "/work/torn", id: "22222222-bbbb" },
			{ op: "createBranch", s: "a", branch: "main", at: null },
			{ op: "append", s: "a", message: user("kept", 1) },
			{ op: "close", s: "a" },
			{ op: "appendRaw", s: "a", text: '{"kind":"value","op":"set","seq":99,"namesp' },
			{ op: "open", s: "a" },
			{ op: "append", s: "a", message: user("after", 2) },
			{ op: "dump", s: "a" },
		],
	},
	{
		name: "invalid-lines",
		ops: [
			{ op: "create", s: "a", cwd: "/work/bad", id: "33333333-cccc" },
			{ op: "close", s: "a" },
			{ op: "appendRaw", s: "a", text: '{"kind":"value","op":"set","seq":1,"namespace":"x","key":"","value":1}\n' },
			{ op: "open", s: "a" },
			{ op: "close", s: "a" },
			{ op: "appendRaw", s: "a", text: '{"kind":"value","op":"set","seq":1,"namespace":"x","key":"","value":2}\n' },
			{ op: "open", s: "a" },
			{ op: "create", s: "b", cwd: "/work/bad", id: "33333333-dddd" },
			{ op: "close", s: "b" },
			{ op: "appendRaw", s: "b", text: '{"kind":"entry","id":"e","parentId":"missing","type":"message","message":{},"seq":1,"timestamp":1}\n' },
			{ op: "open", s: "b" },
			{ op: "create", s: "c", cwd: "/work/bad", id: "33333333-eeee" },
			{ op: "close", s: "c" },
			{ op: "appendRaw", s: "c", text: '{"kind":"wat","seq":1}\n' },
			{ op: "open", s: "c" },
			{ op: "create", s: "d", cwd: "/work/bad", id: "33333333-ffff" },
			{ op: "close", s: "d" },
			{ op: "appendRaw", s: "d", text: "not json\n" },
			{ op: "open", s: "d" },
			{ op: "create", s: "e", cwd: "/work/bad", id: "33333333-cccc" },
			{ op: "create", s: "f", cwd: "/work/bad", id: "a b/c?" },
			{ op: "commitOn", s: "f", writes: [{ kind: "entry", entry: { id: "x", parentId: "nope", type: "custom", customType: "t" } }] },
			{ op: "commitOn", s: "f", writes: [{ kind: "entry", entry: { id: "x", parentId: null, type: "custom", customType: "t" } }, { kind: "usage", row: { id: "x", usage: assistant("", 0).usage, adjustment: false } }] },
			{ op: "list", cwd: "/work/bad" },
		],
	},
	{
		name: "legacy-v3",
		ops: [
			{ op: "writeFile", path: "--work-legacy--/2026-01-01T00-00-00-000Z_v3.jsonl", text: v3File("v3-session", "/work/legacy") },
			{ op: "writeFile", path: "--work-legacy--/2026-01-01T00-00-00-000Z_child.jsonl", text: v3Header("v3-child", "/work/legacy", { parentSession: "<ROOT>/--work-legacy--/2026-01-01T00-00-00-000Z_v3.jsonl" }) + "\n" },
			{ op: "writeFile", path: "--work-legacy--/2026-01-01T00-00-00-000Z_orphan.jsonl", text: v3Header("v3-orphan", "/work/legacy", { parentSession: "/nowhere.jsonl" }) + "\n" },
			{ op: "list", cwd: "/work/legacy" },
			{ op: "openListed", s: "a", cwd: "/work/legacy", id: "v3-session" },
			{ op: "dump", s: "a" },
			{ op: "context", s: "a" },
			{ op: "contextBranch", s: "a", tipOf: "u4" },
			{ op: "close", s: "a" },
			{ op: "forkListed", s: "t", cwd: "/work/legacy", source: "v3-session", options: { scope: "tree", id: "fork-v3-tree" } },
			{ op: "dump", s: "t" },
			{ op: "close", s: "t" },
			{ op: "forkListed", s: "x", cwd: "/work/legacy", source: "v3-session", options: { scope: "branch", branch: "main", entryId: "u2", position: "at", id: "fork-v3-x" } },
			{ op: "forkListed", s: "x", cwd: "/work/legacy", source: "v3-session", options: { scope: "branch", branch: "main", entryId: "l1", position: "at", id: "fork-v3-y" } },
			{ op: "forkListed", s: "b", cwd: "/work/legacy", source: "v3-session", options: { scope: "branch", branch: "main", entryId: "u4", position: "before", id: "fork-v3-branch" } },
			{ op: "dump", s: "b" },
			{ op: "close", s: "b" },
			{ op: "openListed", s: "a", cwd: "/work/legacy", id: "v3-session" },
			{ op: "append", s: "a", message: user("upgraded", 9) },
			{ op: "dump", s: "a" },
			{ op: "close", s: "a" },
			{ op: "openListed", s: "a", cwd: "/work/legacy", id: "v3-session" },
			{ op: "dump", s: "a" },
		],
	},
	{
		name: "fork",
		ops: [
			{ op: "create", s: "a", cwd: "/work/fork", id: "44444444-aaaa" },
			{ op: "commit", s: "a", writes: [{ kind: "value", op: "set", namespace: "pi.branch.tip", key: "main", value: null }, ...lane()] },
			{ op: "append", s: "a", message: user("q1", 1) },
			{ op: "append", s: "a", message: assistant("a1", 2) },
			{ op: "append", s: "a", message: user("q2", 3) },
			{ op: "append", s: "a", message: assistant("a2", 4) },
			{ op: "createBranchAtNth", s: "a", branch: "side", nth: 1 },
			{ op: "appendBranch", s: "a", branch: "side", message: user("side q", 5) },
			{ op: "setLabelNth", s: "a", nth: 0, label: "first" },
			{ op: "setLabelNth", s: "a", nth: 3, label: "late" },
			{ op: "setName", s: "a", name: "forked" },
			{ op: "commit", s: "a", writes: [{ kind: "value", op: "set", namespace: "pi.op.state", key: "op1", value: { at: "starting" } }, { kind: "value", op: "set", namespace: "pi.result", key: "op0", value: { status: "completed" } }, { kind: "value", op: "set", namespace: "ultron.usage", key: "root", value: { version: 1, roots: {} } }, { kind: "list", op: "append", namespace: "pi.pending.assistant_frame", key: "op1:r", value: { type: "start" } }, { kind: "list", op: "append", namespace: "ultron.log", key: "k", value: 1 }, { kind: "usage", row: { id: "u-row", usage: assistant("", 0).usage, adjustment: false } }] },
			{ op: "commit", s: "a", writes: [{ kind: "list", op: "delete", namespace: "ultron.log", key: "k" }, { kind: "list", op: "append", namespace: "ultron.log", key: "k", value: 2 }] },
			{ op: "commit", s: "a", writes: [{ kind: "value", op: "set", namespace: "pi.lane.state", key: "main", value: { currentOperationId: "op1", lastOperationId: "op0", inbox: [{ entryId: "x", kind: "steer" }] } }] },
			{ op: "forkOpen", s: "o", source: "a", options: { scope: "tree", id: "fork-open" } },
			{ op: "append", s: "a", message: user("after fork", 6) },
			{ op: "dump", s: "o" },
			{ op: "close", s: "o" },
			{ op: "close", s: "a" },
			{ op: "fork", s: "t", source: "a", options: { scope: "tree", id: "fork-tree" } },
			{ op: "dump", s: "t" },
			{ op: "close", s: "t" },
			{ op: "fork", s: "b", source: "a", options: { scope: "branch", branch: "main", entryNth: 2, position: "at", id: "fork-at" } },
			{ op: "dump", s: "b" },
			{ op: "close", s: "b" },
			{ op: "fork", s: "c", source: "a", options: { scope: "branch", branch: "main", entryNth: 2, position: "before", id: "fork-before" } },
			{ op: "dump", s: "c" },
			{ op: "close", s: "c" },
			{ op: "fork", s: "d", source: "a", options: { scope: "branch", branch: "main", entryNth: 5, anyTreeEntry: true, id: "fork-any" } },
			{ op: "dump", s: "d" },
			{ op: "close", s: "d" },
			{ op: "fork", s: "e", source: "a", options: { scope: "branch", branch: "main", id: "fork-tip" } },
			{ op: "fork", s: "x", source: "a", options: { scope: "branch", branch: "nope" } },
			{ op: "fork", s: "x", source: "a", options: { scope: "branch", branch: "side" } },
			{ op: "fork", s: "x", source: "a", options: { scope: "branch", branch: "main", entryNth: 5 } },
			{ op: "fork", s: "x", source: "a", options: { scope: "tree", id: "fork-tree" } },
			{ op: "list", cwd: "/work/fork" },
		],
	},
	{
		name: "list-and-context",
		ops: [
			{ op: "create", s: "a", cwd: "/a/b", id: "55555555-0001" },
			{ op: "create", s: "b", cwd: "/a-b", id: "55555555-0002" },
			{ op: "create", s: "c", cwd: "/a/b", id: "55555555-0000" },
			{ op: "writeFile", path: "--a-b--/notes.txt", text: "ignored" },
			{ op: "writeFile", path: "--a-b--/garbage.jsonl", text: "{}\n" },
			{ op: "writeFile", path: "--a-b--/dir.jsonl/x", text: "" },
			{ op: "list", cwd: "/a/b" },
			{ op: "list", cwd: "/a-b" },
			{ op: "list", cwd: "/a/b/../b/" },
			{ op: "listAll" },
			{ op: "commit", s: "a", writes: [{ kind: "value", op: "set", namespace: "pi.branch.tip", key: "main", value: null }] },
			{ op: "append", s: "a", message: user("u1", 1) },
			{ op: "append", s: "a", message: { ...assistant("bad", 2), stopReason: "error", errorMessage: "boom" } },
			{ op: "append", s: "a", message: { ...assistant("calls", 3), content: [{ type: "toolCall", id: "c1", name: "rlm", arguments: { code: "1" } }, { type: "toolCall", id: "c2", name: "rlm", arguments: { code: "2" } }], stopReason: "toolUse" } },
			{ op: "append", s: "a", message: { role: "toolResult", toolCallId: "c1", toolName: "rlm", content: [{ type: "text", text: "r1" }], isError: false, timestamp: 4 } },
			{ op: "append", s: "a", message: { role: "toolResult", toolCallId: "c2", toolName: "rlm", content: [{ type: "text", text: "r2" }], isError: false, timestamp: 5 } },
			{ op: "appendCustom", s: "a", customType: "context_edit", data: { edits: [{ targetId: "nth:0", replacement: { content: "u1 edited" } }, { targetId: "nth:4", replacement: null }] } },
			{ op: "context", s: "a" },
			{ op: "appendCustom", s: "a", customType: "context_edit", data: { targetId: "nth:2", replacement: { content: [{ type: "toolCall", id: "c2", name: "rlm", arguments: { code: "2" } }] } } },
			{ op: "context", s: "a" },
			{ op: "appendCustom", s: "a", customType: "context_edit", data: { targetId: "nth:2", replacement: null } },
			{ op: "context", s: "a" },
			{ op: "commitEntry", s: "a", entry: { type: "compaction", summary: "compacted", retainedTail: [user("kept tail", 9), { ...assistant("err tail", 9), stopReason: "aborted" }], tokensBefore: 99, fromHook: false } },
			{ op: "append", s: "a", message: user("post", 10) },
			{ op: "commitEntry", s: "a", entry: { type: "branch_summary", fromId: null, summary: "bs", fromHook: true } },
			{ op: "commitEntry", s: "a", entry: { type: "branch_summary", fromId: null, summary: "", fromHook: true } },
			{ op: "context", s: "a" },
			{ op: "dump", s: "a" },
		],
	},
];

function snapshot(dir) {
	const files = {};
	const walk = (d) => {
		for (const name of readdirSync(d).sort()) {
			const p = join(d, name);
			if (statSync(p).isDirectory()) walk(p);
			else files[relative(dir, p)] = readFileSync(p, "utf8");
		}
	};
	walk(dir);
	return files;
}

async function runScenario(scenario) {
	clock = 1_790_000_000_000;
	const dir = mkdtempSync(join(tmpdir(), "ultron-journal-fixture-"));
	const env = new NodeExecutionEnv({ cwd: dir });
	const repo = new JsonlSessionRepo({ fileSystem: env, sessionsRoot: dir, now: () => Date.now() });
	const sessions = new Map();
	const metas = new Map();
	const nth = new Map();
	const rel = (p) => relative(dir, p);
	const metaJson = (m) => ({ id: m.id, createdAt: m.createdAt, storageVersion: m.storageVersion, cwd: m.cwd, path: rel(m.path), ...(m.parentSessionId === undefined ? {} : { parentSessionId: m.parentSessionId }), ...(m.legacyParentSessionPath === undefined ? {} : { legacyParentSessionPath: m.legacyParentSessionPath }) });
	const track = (key, session) => {
		sessions.set(key, session);
		metas.set(key, session.metadata);
		return metaJson(session.metadata);
	};
	const fixIds = (value, key) => JSON.parse(JSON.stringify(value).replace(/"nth:(\d+)"/g, (_, n) => JSON.stringify(nth.get(key)[Number(n)])));
	const pushNth = (key, id) => {
		if (!nth.has(key)) nth.set(key, []);
		nth.get(key).push(id);
	};
	const forkOptions = (key, options) => {
		const { entryNth, ...rest } = options;
		return entryNth === undefined ? rest : { ...rest, entryId: nth.get(key)[entryNth] };
	};
	const steps = [];
	let previous = {};
	for (const op of scenario.ops) {
		let result;
		try {
			const s = sessions.get(op.s);
			switch (op.op) {
				case "create":
					result = track(op.s, await repo.create({ cwd: op.cwd, id: op.id }, ctx));
					break;
				case "createBranch":
					await s.createBranch(op.branch, op.at, ctx);
					result = null;
					break;
				case "createBranchAtNth":
					await s.createBranch(op.branch, nth.get(op.s)[op.nth], ctx);
					result = null;
					break;
				case "commit":
				case "commitOn":
					result = await s.mutate((m) => m.commit(op.writes, ctx), ctx);
					break;
				case "commitEntry": {
					const tip = (await s.getValue({ namespace: "pi.branch.tip", key: "main", kind: "value" }, ctx)).value;
					const id = `entry-${steps.length}`;
					result = await s.mutate((m) => m.commit([{ kind: "entry", entry: { id, parentId: tip, ...op.entry } }, { kind: "value", op: "set", namespace: "pi.branch.tip", key: "main", value: id }], ctx), ctx);
					pushNth(op.s, id);
					break;
				}
				case "append":
				case "appendBranch":
					result = await (await s.branch(op.branch ?? "main", ctx)).appendMessage(op.message, ctx);
					pushNth(op.s, result);
					break;
				case "appendCustom":
					result = await (await s.branch("main", ctx)).appendCustomEntry(op.customType, fixIds(op.data, op.s), ctx);
					pushNth(op.s, result);
					break;
				case "setName":
					await s.setName(op.name ?? undefined, ctx);
					result = null;
					break;
				case "setLabelNth":
					await s.setLabel(nth.get(op.s)[op.nth], op.label, ctx);
					result = null;
					break;
				case "close":
					await s.close(ctx);
					result = null;
					break;
				case "open":
					result = track(op.s, await repo.open(metas.get(op.s), ctx));
					break;
				case "openListed": {
					const m = (await repo.list({ cwd: op.cwd }, ctx)).find((x) => x.id === op.id);
					result = track(op.s, await repo.open(m, ctx));
					break;
				}
				case "appendRaw":
					writeFileSync(metas.get(op.s).path, op.text, { flag: "a" });
					result = null;
					break;
				case "writeFile": {
					const p = join(dir, op.path);
					mkdirSync(dirname(p), { recursive: true });
					writeFileSync(p, op.text.replaceAll("<ROOT>", dir));
					result = null;
					break;
				}
				case "list":
					result = (await repo.list({ cwd: op.cwd }, ctx)).map(metaJson);
					break;
				case "listAll":
					result = (await repo.list(undefined, ctx)).map(metaJson);
					break;
				case "fork":
					result = track(op.s, await repo.fork(metas.get(op.source), forkOptions(op.source, op.options), ctx));
					break;
				case "forkOpen":
					result = track(op.s, await repo.fork(metas.get(op.source), forkOptions(op.source, op.options), ctx));
					break;
				case "forkListed": {
					const m = (await repo.list({ cwd: op.cwd }, ctx)).find((x) => x.id === op.source);
					const options = { ...op.options };
					result = track(op.s, await repo.fork(m, options, ctx));
					break;
				}
				case "dump": {
					const state = s.storage.storageState;
					const key = (v) => `${v.address.namespace}\u0000${v.address.key}`;
					result = {
						stats: await s.getStats(ctx),
						nextSeq: state.getNextSeq(),
						entries: state.scanEntries({}),
						values: [...state.scalarValues.values()].sort((l, r) => (key(l) < key(r) ? -1 : key(l) > key(r) ? 1 : 0)).map((v) => ({ namespace: v.address.namespace, key: v.address.key, value: v.value, seq: v.seq })),
						lists: [...state.listValues.values()].sort((l, r) => (`${l.address.namespace}\u0000${l.address.key}` < `${r.address.namespace}\u0000${r.address.key}` ? -1 : 1)).map((v) => ({ namespace: v.address.namespace, key: v.address.key, elements: v.elements })),
						usage: state.scanUsage({}),
						legacyV3: s.storage.isLegacyV3(),
					};
					break;
				}
				case "context":
				case "contextBranch": {
					let tip = (await s.getValue({ namespace: "pi.branch.tip", key: "main", kind: "value" }, ctx)).value;
					if (op.tipOf !== undefined) tip = s.storage.storageState.scanEntries({}).find((e) => e.message?.content?.[0]?.text === "side")?.id;
					const entries = tip === null ? [] : await s.scanBranch({ start: tip, order: "oldestFirst" }, ctx);
					result = await buildSessionContext(entries, undefined, ctx);
					break;
				}
				default:
					throw new Error(`unknown op ${op.op}`);
			}
		} catch (error) {
			result = { error: error.message };
		}
		const files = snapshot(dir);
		const changed = JSON.stringify(files) !== JSON.stringify(previous);
		previous = files;
		steps.push({ result: JSON.parse(JSON.stringify(result ?? null).split(dir).join("<ROOT>")), ...(changed ? { files: JSON.parse(JSON.stringify(files).split(dir).join("<ROOT>")) } : {}) });
	}
	rmSync(dir, { recursive: true, force: true });
	return { name: scenario.name, ops: scenario.ops, steps };
}

const out = [];
for (const scenario of scenarios) out.push(await runScenario(scenario));
writeFileSync(join(here, "storage.json"), `${JSON.stringify(out, null, 1)}\n`);
