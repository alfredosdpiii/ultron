// Generates crates/session-journal/fixtures/all-messages.json: the text the session selector searches
// (`SessionInfo.allMessagesText`, packages/coding-agent/src/core/session-manager.ts `buildSessionInfo`) for each
// case's messages, plus `nativeSessionDetails` (packages/coding-agent/src/experimental/session-listing.ts).
// Each case's messages are written twice: as a format-4 journal through JsonlSessionRepo (packages/agent harness)
// and as a Pi v3 file. SessionManager.list reads the v3 file and records `allMessagesText`; the Rust test feeds both
// texts to `names::session_details` and compares. A message marked `throws` makes upstream drop the whole file
// (`extractTextContent` throws on it); the generator records that, and the text of the file without those messages,
// which is what Rust lists (it keeps the session and skips the message).
// Date.now is a counter and crypto.getRandomValues yields zeros, so ids and files are deterministic.
// Usage: node crates/session-journal/fixtures/all-messages.mjs
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { registerHooks } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
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
const { BACKGROUND_CONTEXT: ctx } = await harness("context.ts");
const { NodeExecutionEnv } = await harness("env/nodejs.ts");
const { SessionManager } = await import(pathToFileURL(join(root, "packages/coding-agent/src/core/session-manager.ts")).href);
const { nativeSessionDetails } = await import(
	pathToFileURL(join(root, "packages/coding-agent/src/experimental/session-listing.ts")).href
);

const text = (t) => ({ type: "text", text: t });
const user = (content, timestamp = 1) => ({ role: "user", content, timestamp });
const assistant = (content, timestamp = 2) => ({
	role: "assistant",
	content,
	api: "anthropic-messages",
	provider: "anthropic",
	model: "claude-sonnet-4-5",
	usage: { input: 1, output: 1, cacheRead: 0, cacheWrite: 0, totalTokens: 2, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } },
	stopReason: "stop",
	timestamp,
});

const cases = [
	{
		name: "later-messages-are-searched",
		messages: [user([text("fix the parser")]), assistant([text("Done.")]), user("now the kestrel lexer"), assistant([text("Kestrel lexer fixed.")])],
	},
	{
		name: "only-text-blocks-joined-by-spaces",
		messages: [
			user([text("look at"), { type: "image", data: "aGk=", mimeType: "image/png" }, text("this image")]),
			assistant([
				{ type: "thinking", thinking: "secret plan" },
				text("I see"),
				{ type: "toolCall", id: "c1", name: "rlm", arguments: { code: "print(1)" } },
				text("a cat"),
			]),
		],
	},
	{
		name: "tool-results-and-custom-roles-are-not-searched",
		messages: [
			user([text("run it")]),
			{ role: "toolResult", toolCallId: "c1", toolName: "rlm", content: [text("tool output zebra")], isError: false, timestamp: 3 },
			{ role: "custom", customType: "note", content: "custom yak", display: true, timestamp: 4 },
			assistant([text("ran")]),
		],
	},
	{
		name: "empty-text-is-skipped",
		messages: [
			user([{ type: "image", data: "aGk=", mimeType: "image/png" }]),
			assistant([]),
			user(""),
			assistant([text("")]),
			user([text("real ask")]),
			assistant([text(""), text("")]),
		],
	},
	{
		name: "a-message-without-content-is-skipped",
		messages: [user([text("ask")]), { role: "assistant", timestamp: 5 }, assistant([text("answer")])],
	},
	{
		name: "text-values-render-like-array-join",
		messages: [
			user([{ type: "text" }, text("missing text first")]),
			assistant([{ type: "text", text: 5 }, { type: "text", text: null }, { type: "text", text: true }]),
			assistant([{ type: "text", text: { a: 1 } }, { type: "text", text: [1, [2, 3]] }]),
			user([text("multi\nline"), "stray", 7, { type: "text", text: "  spaced  " }]),
		],
	},
	{
		name: "unicode-text",
		messages: [user([text("héllo wörld 🦀")]), assistant("plain string reply ✓")],
	},
	{
		name: "content-upstream-throws-on",
		messages: [
			user([text("before")]),
			{ ...assistant([text("x"), null]), throws: true },
			{ ...user({ type: "text", text: "object content" }), throws: true },
			{ ...user(42), throws: true },
			{ ...assistant(null), throws: true },
			user([text("after")]),
		],
	},
];

const strip = ({ throws: _t, ...message }) => message;

async function nativeText(messages) {
	clock = 1_790_000_000_000;
	const dir = mkdtempSync(join(tmpdir(), "ultron-all-messages-native-"));
	try {
		const env = new NodeExecutionEnv({ cwd: dir });
		const repo = new JsonlSessionRepo({ fileSystem: env, sessionsRoot: dir, now: () => Date.now() });
		const session = await repo.create({ cwd: "/work/project", id: "11111111-aaaa" }, ctx);
		await session.createBranch("main", null, ctx);
		const branch = await session.branch("main", ctx);
		for (const message of messages) await branch.appendMessage(message, ctx);
		return readFileSync(session.metadata.path, "utf8");
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
}

function piText(messages) {
	const lines = [JSON.stringify({ type: "session", version: 3, id: "22222222-bbbb", timestamp: "2026-01-01T00:00:00.000Z", cwd: "/work/project" })];
	let parent = null;
	messages.forEach((message, i) => {
		const id = `m${i}`;
		lines.push(JSON.stringify({ type: "message", id, parentId: parent, timestamp: "2026-01-01T00:00:01.000Z", message }));
		parent = id;
	});
	return `${lines.join("\n")}\n`;
}

async function sessionInfo(text) {
	const dir = mkdtempSync(join(tmpdir(), "ultron-all-messages-pi-"));
	try {
		mkdirSync(join(dir, "s"));
		writeFileSync(join(dir, "s", "2026-01-01T00-00-00-000Z_22222222-bbbb.jsonl"), text);
		const infos = await SessionManager.list("/work/project", join(dir, "s"));
		return infos.length === 1 ? { allMessagesText: infos[0].allMessagesText, firstMessage: infos[0].firstMessage } : null;
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
}

const out = [];
for (const c of cases) {
	const messages = c.messages.map(strip);
	const kept = c.messages.filter((m) => !m.throws).map(strip);
	const pi = piText(messages);
	const native = await nativeText(messages);
	const info = await sessionInfo(pi);
	const keptInfo = await sessionInfo(piText(kept));
	out.push({
		name: c.name,
		piText: pi,
		nativeText: native,
		// What SessionManager.list says of the whole file: null when it drops the file.
		sessionInfo: info,
		// SessionManager.list's allMessagesText of the file without the messages it throws on.
		allMessagesText: keptInfo.allMessagesText,
		nativeDetails: nativeSessionDetails(native),
		piDetails: nativeSessionDetails(pi),
	});
}
writeFileSync(join(here, "all-messages.json"), `${JSON.stringify(out, null, 1)}\n`);
