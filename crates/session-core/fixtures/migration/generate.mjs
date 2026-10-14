// Generates crates/session-core/fixtures/migration/migration.json from packages/coding-agent/src/ultron/migration.ts.
// Each case writes a Pi session (the fixtures of packages/coding-agent/test/ultron-migration.test.ts, with the cwd
// "/project" so no machine path is recorded), imports it with `importPiSession` into a fresh sessions root, and
// exports the native session back with `exportNativeSessionToPi`. It records the Pi input lines, the import result
// (paths relative to the sessions root), the native session file's exact bytes, and the exported Pi file's exact
// bytes. Everything is deterministic: entry ids and timestamps come from the Pi file, and the native session id is a
// digest of the Pi session id.
// Usage: node crates/session-core/fixtures/migration/generate.mjs
import { mkdtempSync, readFileSync, rmSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const { importPiSession, exportNativeSessionToPi } = await import(
	join(here, "../../../../packages/coding-agent/src/ultron/migration.ts")
);

const PI_SESSION_ID = "0192a0b0-0000-7000-8000-000000000001";
const CWD = "/project";
const usage = {
	input: 10,
	output: 5,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 15,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};
const assistant = (content, stopReason = "stop", at = 0) => ({
	role: "assistant",
	content,
	api: "anthropic-messages",
	provider: "anthropic",
	model: "claude-test",
	usage,
	stopReason,
	timestamp: 1_760_000_000_000 + at,
});
const user = (text, at = 0) => ({ role: "user", content: [{ type: "text", text }], timestamp: 1_760_000_000_000 + at });
const ts = (seconds) => new Date(1_760_000_000_000 + seconds * 1000).toISOString();
const header = { type: "session", version: 3, id: PI_SESSION_ID, timestamp: ts(0), cwd: CWD };

const linear = [
	header,
	{ type: "model_change", id: "e1", parentId: null, timestamp: ts(1), provider: "anthropic", modelId: "claude-test" },
	{ type: "thinking_level_change", id: "e2", parentId: "e1", timestamp: ts(2), thinkingLevel: "high" },
	{ type: "message", id: "m1", parentId: "e2", timestamp: ts(3), message: user("list the files", 3) },
	{
		type: "message",
		id: "m2",
		parentId: "m1",
		timestamp: ts(4),
		message: assistant(
			[
				{ type: "text", text: "Listing." },
				{ type: "toolCall", id: "call_1", name: "bash", arguments: { command: "ls" } },
			],
			"toolUse",
			4,
		),
	},
	{
		type: "message",
		id: "m3",
		parentId: "m2",
		timestamp: ts(5),
		message: {
			role: "toolResult",
			toolCallId: "call_1",
			toolName: "bash",
			content: [{ type: "text", text: "a.txt\nb.txt" }],
			isError: false,
			timestamp: 1_760_000_005_000,
		},
	},
	{ type: "message", id: "m4", parentId: "m3", timestamp: ts(6), message: assistant([{ type: "text", text: "Two files." }], "stop", 6) },
	{ type: "message", id: "m5", parentId: "m4", timestamp: ts(7), message: user("try approach X", 7) },
	{ type: "message", id: "x1", parentId: "m5", timestamp: ts(8), message: assistant([{ type: "text", text: "abandoned answer" }], "stop", 8) },
	{ type: "message", id: "x2", parentId: "x1", timestamp: ts(9), message: user("abandoned follow-up", 9) },
	{ type: "branch_summary", id: "b1", parentId: "m5", timestamp: ts(10), fromId: "x2", summary: "Approach X failed.", fromHook: false },
	{ type: "message", id: "m6", parentId: "b1", timestamp: ts(11), message: assistant([{ type: "text", text: "Using approach Y." }], "stop", 11) },
	{
		type: "compaction",
		id: "c1",
		parentId: "m6",
		timestamp: ts(12),
		summary: "Listed files, X failed, Y chosen.",
		firstKeptEntryId: "m5",
		tokensBefore: 1234,
		details: { readFiles: [], modifiedFiles: [] },
	},
	{ type: "label", id: "l1", parentId: "c1", timestamp: ts(13), targetId: "m4", label: "checkpoint" },
	{ type: "message", id: "m7", parentId: "l1", timestamp: ts(14), message: user("continue", 14) },
	{ type: "session_info", id: "s1", parentId: "m7", timestamp: ts(15), name: "Imported work" },
	{ type: "message", id: "m8", parentId: "s1", timestamp: ts(16), message: assistant([{ type: "text", text: "Continuing with Y." }], "stop", 16) },
];

const warmUsage = { ...usage, input: 3, output: 0, totalTokens: 3 };
const tree = [
	...linear.slice(0, 8),
	{ type: "message", id: "x1", parentId: "m5", timestamp: ts(8), message: assistant([{ type: "text", text: "Trying X." }], "stop", 8) },
	{ type: "label", id: "xl", parentId: "x1", timestamp: ts(9), targetId: "x1", label: "abandoned-x" },
	{ type: "model_change", id: "xm", parentId: "xl", timestamp: ts(10), provider: "openai", modelId: "gpt-test" },
	{ type: "message", id: "x2", parentId: "xm", timestamp: ts(11), message: user("X step two draft", 11) },
	{
		type: "message",
		id: "x3",
		parentId: "x2",
		timestamp: ts(12),
		message: { ...assistant([{ type: "text", text: "X half done." }], "stop", 12), provider: "openai", model: "gpt-test" },
	},
	{ type: "compaction", id: "xc", parentId: "x3", timestamp: ts(13), summary: "X so far.", firstKeptEntryId: "x2", tokensBefore: 500 },
	{ type: "usage", id: "xu", parentId: "xc", timestamp: ts(14), kind: "cache_warm", provider: "openai", model: "gpt-test", usage: warmUsage },
	{
		type: "context_edit",
		id: "xe",
		parentId: "xu",
		timestamp: ts(15),
		targetId: "x2",
		replacement: { content: [{ type: "text", text: "X step two (redacted)" }] },
	},
	{ type: "message", id: "x4", parentId: "xe", timestamp: ts(16), message: user("X failed", 16) },
	{ type: "branch_summary", id: "b1", parentId: "m5", timestamp: ts(17), fromId: "x4", summary: "Approach X failed.", fromHook: false },
	{ type: "custom_message", id: "cm", parentId: "b1", timestamp: ts(18), customType: "note", content: "Remember Y", display: true },
	{ type: "message", id: "m6", parentId: "cm", timestamp: ts(19), message: assistant([{ type: "text", text: "Using approach Y." }], "stop", 19) },
	{
		type: "compaction",
		id: "c1",
		parentId: "m6",
		timestamp: ts(20),
		summary: "Listed files, X failed, Y chosen.",
		firstKeptEntryId: "m5",
		tokensBefore: 1234,
		details: { readFiles: [], modifiedFiles: [] },
	},
	{ type: "label", id: "la", parentId: "c1", timestamp: ts(21), targetId: "m4", label: "checkpoint" },
	{ type: "label", id: "lt", parentId: "la", timestamp: ts(22), targetId: "m2", label: "temp" },
	{ type: "label", id: "lc", parentId: "lt", timestamp: ts(23), targetId: "m2" },
	{ type: "thinking_level_change", id: "t1", parentId: "lc", timestamp: ts(24), thinkingLevel: "low" },
	{
		type: "context_edit",
		id: "ce1",
		parentId: "t1",
		timestamp: ts(25),
		targetId: "m6",
		replacement: { content: [{ type: "text", text: "Using approach Y (edited)." }] },
	},
	{ type: "message", id: "m7", parentId: "ce1", timestamp: ts(26), message: user("noise", 26) },
	{ type: "context_edit", id: "ce2", parentId: "m7", timestamp: ts(27), targetId: "m7", replacement: null },
	{ type: "session_info", id: "s1", parentId: "ce2", timestamp: ts(28), name: "Imported work" },
	{ type: "future_thing", id: "f1", parentId: "s1", timestamp: ts(29), payload: { answer: 42 } },
	{
		type: "usage",
		id: "u1",
		parentId: "f1",
		timestamp: ts(30),
		kind: "cache_warm",
		provider: "anthropic",
		model: "claude-test",
		usage: warmUsage,
		note: "warm",
	},
	{ type: "custom", id: "cx", parentId: "u1", timestamp: ts(31), customType: "ext-state", data: { n: 1 } },
	{ type: "message", id: "m8", parentId: "cx", timestamp: ts(32), message: assistant([{ type: "text", text: "Continuing with Y." }], "stop", 32) },
	{ type: "model_change", id: "mc", parentId: "m8", timestamp: ts(33), provider: "anthropic", modelId: "claude-next" },
];

const edit = [
	header,
	{ type: "message", id: "a", parentId: null, timestamp: ts(1), message: user("shared", 1) },
	{ type: "message", id: "b", parentId: "a", timestamp: ts(2), message: assistant([{ type: "text", text: "one" }], "stop", 2) },
	{ type: "message", id: "c", parentId: "a", timestamp: ts(3), message: assistant([{ type: "text", text: "two" }], "stop", 3) },
	{ type: "context_edit", id: "e", parentId: "c", timestamp: ts(4), targetId: "a", replacement: { content: "edited" } },
];

const text = (id, at, role = "user") => ({
	type: "message",
	id,
	timestamp: ts(at),
	message: role === "user" ? user(`${id} text`, at) : assistant([{ type: "text", text: `${id} text` }], "stop", at),
});
const editOf = (id, parentId, at, targetId, content) => ({
	type: "context_edit",
	id,
	parentId,
	timestamp: ts(at),
	targetId,
	replacement: content === null ? null : { content },
});
const edits = [
	header,
	{ ...text("r", 1), parentId: null },
	{ ...text("t", 2, "assistant"), parentId: "r" },
	{ type: "custom_message", id: "n", parentId: "t", timestamp: ts(3), customType: "note", content: "n text", display: true },
	{ ...text("a1", 4), parentId: "n" },
	editOf("ea", "a1", 5, "t", "t edited on A"),
	{ ...text("a2", 6, "assistant"), parentId: "ea" },
	editOf("en", "a2", 7, "n", null),
	{ ...text("a3", 8), parentId: "en" },
	editOf("ea1", "a3", 9, "t", "t edited again on A1"),
	{ ...text("a4", 10, "assistant"), parentId: "ea1" },
	{ type: "compaction", id: "ca", parentId: "en", timestamp: ts(11), summary: "A so far", firstKeptEntryId: "t", tokensBefore: 50 },
	editOf("ea2", "ca", 12, "a1", "a1 edited after compaction"),
	{ ...text("a5", 13), parentId: "ea2" },
	{ ...text("a6", 14), parentId: "ca" },
	{ ...text("b1", 15), parentId: "n" },
	editOf("eb", "b1", 16, "t", "t edited on B"),
	{ ...text("b2", 17, "assistant"), parentId: "eb" },
	{ ...text("c1", 18), parentId: "n" },
	editOf("ex", "c1", 19, "b1", "never visible"),
];

const priced = (input, output, cost) => ({
	input,
	output,
	cacheRead: 1,
	cacheWrite: 2,
	totalTokens: input + output + 3,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: cost },
});
const usageCase = [
	header,
	{ type: "message", id: "u", parentId: null, timestamp: ts(1), message: user("hi", 1) },
	{
		type: "message",
		id: "a",
		parentId: "u",
		timestamp: ts(2),
		message: { ...assistant([{ type: "text", text: "hello" }], "stop", 2), usage: priced(100, 20, 0.25) },
	},
	{ type: "usage", id: "w", parentId: "a", timestamp: ts(3), kind: "cache_warm", provider: "anthropic", model: "claude-test", usage: priced(7, 0, 0.125) },
	{ type: "compaction", id: "c", parentId: "w", timestamp: ts(4), summary: "s", firstKeptEntryId: "a", tokensBefore: 10, usage: priced(50, 5, 0.5) },
];

// Children written before their parent (Pi tolerates it): the import places parents first, siblings in file order.
const unordered = [
	header,
	{ type: "message", id: "c1", parentId: "p", timestamp: ts(2), message: assistant([{ type: "text", text: "first" }], "stop", 2) },
	{ type: "message", id: "c2", parentId: "p", timestamp: ts(3), message: assistant([{ type: "text", text: "second" }], "stop", 3) },
	{ type: "message", id: "p", parentId: null, timestamp: ts(1), message: user("question", 1) },
];

const cases = [];
for (const [name, lines] of [
	["linear", linear],
	["tree", tree],
	["edit", edit],
	["edits", edits],
	["usage", usageCase],
	["unordered", unordered],
]) {
	const root = mkdtempSync(join(tmpdir(), "ultron-migration-fixture-"));
	try {
		const piPath = join(root, "pi.jsonl");
		const pi = `${lines.map((line) => JSON.stringify(line)).join("\n")}\n`;
		writeFileSync(piPath, pi);
		const sessionsRoot = join(root, "native");
		mkdirSync(sessionsRoot);
		const result = await importPiSession({ piSessionPath: piPath, sessionsRoot });
		const native = readFileSync(result.path, "utf8");
		const outputPath = join(root, "out.jsonl");
		const exported = await exportNativeSessionToPi({ sessionPath: result.path, outputPath });
		cases.push({
			name,
			pi,
			result: { ...result, path: relative(sessionsRoot, result.path) },
			native,
			exported: { sessionId: exported.sessionId, entries: exported.entries, content: readFileSync(outputPath, "utf8") },
		});
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
}
writeFileSync(join(here, "migration.json"), `${JSON.stringify({ cases }, null, "\t")}\n`);
