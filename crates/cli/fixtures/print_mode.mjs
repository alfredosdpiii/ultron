// Records the TypeScript `ultron -p` / `--mode json` runs against a scripted Anthropic SSE server.
import { spawnSync, spawn } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, readdirSync, readFileSync, rmSync, statSync, utimesSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { filteredJournal, normalizeJournal, renameIds, zeroTimes } from "./journal.mjs";

const root = new URL("../../../", import.meta.url).pathname;

function sse(events) {
	return events.map((e) => `event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`).join("");
}
export function textReply(text) {
	return sse([
		{ type: "message_start", message: { id: "msg_1", type: "message", role: "assistant", model: "claude-sonnet-4-5", content: [], stop_reason: null, usage: { input_tokens: 10, output_tokens: 1 } } },
		{ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
		{ type: "content_block_delta", index: 0, delta: { type: "text_delta", text } },
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 5 } },
		{ type: "message_stop" },
	]);
}

export function toolReply(id, code) {
	return sse([
		{ type: "message_start", message: { id: "msg_2", type: "message", role: "assistant", model: "claude-sonnet-4-5", content: [], stop_reason: null, usage: { input_tokens: 10, output_tokens: 1 } } },
		{ type: "content_block_start", index: 0, content_block: { type: "tool_use", id, name: "rlm", input: {} } },
		{ type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: JSON.stringify({ code }) } },
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "tool_use" }, usage: { output_tokens: 7 } },
		{ type: "message_stop" },
	]);
}

/** OpenAI chat-completions chunks: text, or one `rlm` call. */
export function completionsReply({ text, code }) {
	const chunk = (delta, finish, usage) => ({ id: "chatcmpl-1", object: "chat.completion.chunk", created: 1, model: "llama-3.3-70b-versatile", choices: [{ index: 0, delta, finish_reason: finish }], ...(usage ? { usage } : {}) });
	const body = code === undefined
		? [chunk({ role: "assistant", content: "" }, null), chunk({ content: text }, null), chunk({}, "stop", { prompt_tokens: 12, completion_tokens: 3, total_tokens: 15 })]
		: [chunk({ role: "assistant", content: null, tool_calls: [{ index: 0, id: "call_1", type: "function", function: { name: "rlm", arguments: "" } }] }, null), chunk({ tool_calls: [{ index: 0, function: { arguments: JSON.stringify({ code }) } }] }, null), chunk({}, "tool_calls", { prompt_tokens: 12, completion_tokens: 9, total_tokens: 21 })];
	return body.map((c) => `data: ${JSON.stringify(c)}\n\n`).join("") + "data: [DONE]\n\n";
}

/**
 * What a session file says about the conversation: the header's cwd and the message entries in order (each the child
 * of the one before), message timestamps zeroed. The format-4 journal's other records (operation state, usage roots,
 * stats) are the harness's own bookkeeping.
 */
function transcript(text) {
	let cwd;
	const messages = [];
	for (const line of text.split("\n").filter(Boolean)) {
		for (const record of [JSON.parse(line)].flat()) {
			if (record.kind === "header") cwd = record.cwd;
			if (record.kind === "entry" && record.type === "message") {
				const parent = messages.length === 0 ? null : messages.at(-1).id;
				if (record.parentId !== parent) throw new Error("message entries are not one chain");
				messages.push({ id: record.id, message: { ...record.message, timestamp: 0 } });
			}
		}
	}
	return { cwd, messages: messages.map((m) => m.message) };
}

/**
 * A TypeScript-written session file made deterministic but still a valid journal: the timing-dependent records
 * (progress frames, Ultron module documents) dropped with sequences renumbered, ids renamed, times zeroed.
 */
function deterministicFile(text, ids) {
	const { header, transactions } = filteredJournal(text);
	return [header, ...transactions.map((records) => (records.length === 1 ? records[0] : records))]
		.map((value) => JSON.stringify(zeroTimes(JSON.parse(renameIds(JSON.stringify(value), ids)))))
		.join("\n") + "\n";
}

function sessionFiles(sessionsDir) {
	try {
		return readdirSync(sessionsDir, { recursive: true }).filter((f) => f.endsWith(".jsonl")).sort();
	} catch {
		return [];
	}
}

/** Run `ultron` (TypeScript) in `tmp` against a scripted server; returns its output and the requests it made. */
async function runCli(tmp, args, replies, extraEnv = {}) {
	const requests = [];
	const server = createServer((req, res) => {
		let body = "";
		req.on("data", (c) => (body += c));
		req.on("end", () => {
			requests.push({ method: req.method, url: req.url, body: JSON.parse(body || "null") });
			const reply = replies[requests.length - 1] ?? textReply("(no more replies)");
			res.writeHead(200, { "content-type": "text/event-stream" });
			res.end(reply);
		});
	});
	await new Promise((r) => server.listen(0, "127.0.0.1", r));
	const port = server.address().port;
	const agentDir = join(tmp, "agent");
	writeFileSync(join(agentDir, "models.json"), JSON.stringify({ providers: { anthropic: { baseUrl: `http://127.0.0.1:${port}` }, groq: { baseUrl: `http://127.0.0.1:${port}/v1` } } }));
	const child = spawn(process.execPath, [join(root, "packages/coding-agent/src/cli.ts"), ...args], {
		cwd: join(tmp, "cwd"),
		env: { PATH: process.env.PATH, HOME: tmp, ULTRON_CODING_AGENT_DIR: agentDir, ULTRON_SERVER_DIR: join(tmp, "server"), ANTHROPIC_API_KEY: "test-key", NO_COLOR: "1", TZ: "UTC", ...extraEnv },
		stdio: ["ignore", "pipe", "pipe"],
	});
	let stdout = "", stderr = "";
	child.stdout.on("data", (c) => (stdout += c));
	child.stderr.on("data", (c) => (stderr += c));
	const code = await new Promise((r) => child.on("close", r));
	server.close();
	return { code, stdout, stderr, requests };
}

/**
 * One case: optional setup (TypeScript runs and written files) whose session files are then made deterministic
 * and recorded as `setupFiles` (the Rust test starts from the same files: TypeScript writes, Rust reads), then the
 * recorded run. Args may name setup sessions as `<SESSION:n>` (id) and `<SESSION_PATH:n>` (file).
 */
async function run(c) {
	const tmp = mkdtempSync(join(tmpdir(), "ultron-cli-fixture-"));
	const agentDir = join(tmp, "agent");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(join(tmp, "cwd"));
	const sessionsDir = join(agentDir, "experimental", "sessions");
	for (const step of c.setup ?? []) {
		if (step.write !== undefined) {
			const path = join(agentDir, step.write);
			mkdirSync(dirname(path), { recursive: true });
			writeFileSync(path, step.content.replaceAll("<TMP>", tmp));
		} else {
			await runCli(tmp, step.args, step.replies, step.env);
		}
	}
	const ids = new Map();
	const byAge = sessionFiles(sessionsDir).sort((l, r) => statSync(join(sessionsDir, l)).mtimeMs - statSync(join(sessionsDir, r)).mtimeMs);
	const setupFiles = [];
	for (const [rank, file] of byAge.entries()) {
		const path = join(sessionsDir, file);
		const original = readFileSync(path, "utf8");
		const written = (c.setup ?? []).some((s) => s.write !== undefined && join(agentDir, s.write) === path);
		const content = written ? original : deterministicFile(original, ids);
		const header = JSON.parse(content.split("\n")[0]);
		const name = written ? file : join(dirname(file), `1970-01-01T00-00-00-000Z_${header.id}.jsonl`);
		rmSync(path);
		writeFileSync(join(sessionsDir, name), content);
		utimesSync(join(sessionsDir, name), 1_700_000_000 + rank, 1_700_000_000 + rank);
		setupFiles.push({ path: name, id: header.id, content });
	}
	const args = c.args.map((a) =>
		a.replace(/<SESSION:(\d+)>/g, (_, n) => setupFiles[n].id).replace(/<SESSION_PATH:(\d+)>/g, (_, n) => join(sessionsDir, setupFiles[n].path)),
	);
	const result = await runCli(tmp, args, c.replies, c.env);
	const sessions = sessionFiles(sessionsDir).map((f) => {
		const text = readFileSync(join(sessionsDir, f), "utf8");
		return { dir: dirname(f), ...transcript(text), journal: normalizeJournal(text) };
	});
	rmSync(tmp, { recursive: true, force: true });
	return { args: c.args, ...result, sessions, setupFiles: setupFiles.map((f) => ({ ...f, content: f.content.split(tmp).join("<TMP>") })), tmp };
}

/** Run-specific text replaced by placeholders: the temp dir (as a path and a session directory name), the clock line, server/session ids. */
function normalize(value, tmp) {
	const text = JSON.stringify(value)
		.split(tmp).join("<TMP>")
		// The checkout path (bundled skills, docs) so the fixture does not depend on where it was recorded.
		.split(root.replace(/\/$/, "")).join("<root>")
		// The temp dir as a session directory name (sessionDirectoryName) names it too.
		.split(tmp.slice(1).replace(/[/\\:]/g, "-")).join("<TMPNAME>")
		.replace(/\[clock\] \d{4}-\d\d-\d\d \d\d:\d\d UTC[+-]\d\d:\d\d(, \d+ (min|h|d) after the previous message)?/g, "[clock] <CLOCK>")
		.replace(/\\"(serverId|sessionId)\\":\\"[^"\\]*\\"/g, '\\"$1\\":\\"<ID>\\"')
;
	return JSON.parse(text);
}

const M = ["--model", "anthropic/claude-sonnet-4-5"];
const first = { args: ["-p", ...M, "hello"], replies: [textReply("one")] };
const v3Session = [
	JSON.stringify({ type: "session", version: 3, id: "0193a000-0000-7000-8000-00000000aaaa", timestamp: "2026-01-01T00:00:00.000Z", cwd: "<TMP>/cwd" }),
	JSON.stringify({ type: "model_change", id: "a1", parentId: null, timestamp: "2026-01-01T00:00:01.000Z", provider: "anthropic", modelId: "claude-sonnet-4-5" }),
	JSON.stringify({ type: "thinking_level_change", id: "a2", parentId: "a1", timestamp: "2026-01-01T00:00:01.000Z", thinkingLevel: "low" }),
	JSON.stringify({ type: "message", id: "a3", parentId: "a2", timestamp: "2026-01-01T00:00:02.000Z", message: { role: "user", content: [{ type: "text", text: "old question" }], timestamp: 1767225602000 } }),
	JSON.stringify({ type: "message", id: "a4", parentId: "a3", timestamp: "2026-01-01T00:00:03.000Z", message: { role: "assistant", content: [{ type: "text", text: "old answer" }], api: "anthropic-messages", provider: "anthropic", model: "claude-sonnet-4-5", usage: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0, totalTokens: 3, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } }, stopReason: "stop", timestamp: 1767225603000 } }),
	JSON.stringify({ type: "session_info", id: "a5", parentId: "a4", timestamp: "2026-01-01T00:00:04.000Z", name: "legacy name" }),
].join("\n") + "\n";

const cases = [
	{ name: "print-text", args: ["-p", "hello", "--model", "anthropic/claude-sonnet-4-5"], replies: [textReply("Hi there.")] },
	{ name: "json-text", args: ["--mode", "json", "--model", "anthropic/claude-sonnet-4-5", "say hi"], replies: [textReply("Hello \"world\"\nline two")] },
	{ name: "print-rlm-cell", args: ["-p", "--model", "anthropic/claude-sonnet-4-5", "compute"], replies: [toolReply("toolu_01", "x = 6 * 7\nprint('x is', x)\nx + 1"), textReply("Done: 43.")] },
	{ name: "json-rlm-error", args: ["--mode", "json", "--model", "anthropic/claude-sonnet-4-5", "fail"], replies: [toolReply("toolu_02", "1/0"), textReply("It failed.")] },
	{ name: "print-rlm-state", args: ["-p", "--model", "anthropic/claude-sonnet-4-5", "keep state"], replies: [toolReply("toolu_03", "y = 2"), toolReply("toolu_04", "print(y * 21)"), textReply("42")] },
	{ name: "print-rlm-bash", args: ["-p", "--model", "anthropic/claude-sonnet-4-5", "shell"], replies: [toolReply("toolu_05", "out = await bash('echo hi; echo err >&2; exit 3')\nprint(out)\nout.exit_code"), textReply("exit 3")] },
	{ name: "print-no-session", args: ["-p", "--no-session", "--model", "anthropic/claude-sonnet-4-5", "forget"], replies: [toolReply("toolu_06", "print('kept nowhere')"), textReply("ok")] },
	{ name: "print-groq-rlm", args: ["-p", "--model", "groq/llama-3.3-70b-versatile", "compute"], env: { GROQ_API_KEY: "test-groq" }, replies: [completionsReply({ code: "sum(range(10))" }), completionsReply({ text: "45" })] },
	{ name: "json-groq-text", args: ["--mode", "json", "--model", "groq/llama-3.3-70b-versatile", "hi"], env: { GROQ_API_KEY: "test-groq" }, replies: [completionsReply({ text: "Hello from groq" })] },
	{ name: "print-thinking-off", args: ["-p", "--thinking", "off", "--model", "anthropic/claude-sonnet-4-5", "two", "parts"], replies: [textReply("ok")] },
	// Loki guards the cells' writes (the bundled loki.py engine; the cwd is not a Git repository, so its root is the cwd).
	{ name: "print-loki-write-blocked", args: ["-p", "--model", "anthropic/claude-sonnet-4-5", "store key"], replies: [toolReply("toolu_07", "key = 'AKIA' + 'Q7XRJ2M4PL3ZWNVB'\nawait write('app.env', f'KEY={key}\\n')"), toolReply("toolu_08", "import os\nprint(os.path.exists('app.env'))\nawait write('.loki/loki.json', '{}')"), textReply("blocked")] },
	{ name: "print-loki-shell-refused", args: ["-p", "--model", "anthropic/claude-sonnet-4-5", "shell write"], replies: [toolReply("toolu_09", "await bash('echo hi > out.py')"), toolReply("toolu_10", "print(await bash('echo hi > notes.md && cat notes.md'))"), textReply("refused")] },
	{ name: "print-loki-advise", args: ["-p", "--model", "anthropic/claude-sonnet-4-5", "advise"], env: { ULTRON_LOKI: "advise" }, replies: [toolReply("toolu_11", "key = 'AKIA' + 'Q7XRJ2M4PL3ZWNVB'\nawait write('app.env', f'KEY={key}\\n')"), textReply("written")] },
	{ name: "print-loki-off", args: ["-p", "--model", "anthropic/claude-sonnet-4-5", "off"], env: { ULTRON_LOKI: "off" }, replies: [toolReply("toolu_12", "key = 'AKIA' + 'Q7XRJ2M4PL3ZWNVB'\nawait write('app.env', f'KEY={key}\\n')"), textReply("written")] },
	// Sessions written by a TypeScript run, then continued.
	{ name: "print-continue", setup: [first], args: ["-p", "--continue", ...M, "again"], replies: [textReply("two")] },
	{ name: "print-continue-keeps-lane-model", setup: [first], args: ["-p", "--continue", "--model", "groq/llama-3.3-70b-versatile", "--thinking", "off", "again"], env: { GROQ_API_KEY: "test-groq" }, replies: [textReply("two")] },
	{ name: "print-continue-rlm", setup: [{ args: ["-p", ...M, "compute"], replies: [toolReply("toolu_07", "z = 5"), textReply("set")] }], args: ["-p", "-c", ...M, "more"], replies: [toolReply("toolu_08", "print(1 + 1)"), textReply("two")] },
	{ name: "print-continue-newest", setup: [first, { args: ["-p", ...M, "second session"], replies: [textReply("b")] }], args: ["-p", "--continue", ...M, "again"], replies: [textReply("c")] },
	{ name: "print-resume-flag", setup: [first], args: ["-p", ...M, "again", "--resume"], replies: [textReply("two")] },
	{ name: "print-session-id", setup: [first, { args: ["-p", ...M, "second session"], replies: [textReply("b")] }], args: ["-p", "--session", "<SESSION:0>", ...M, "again"], replies: [textReply("two")] },
	{ name: "print-session-prefix", setup: [first], args: ["-p", "--session", "00000000-0000-7000-8000-0000000000", ...M, "again"], replies: [textReply("two")] },
	{ name: "print-session-path", setup: [first], args: ["-p", "--session", "<SESSION_PATH:0>", ...M, "again"], replies: [textReply("two")] },
	{ name: "print-session-new-id", args: ["-p", "--session-id", "0193a000-0000-7000-8000-00000000beef", ...M, "fresh"], replies: [textReply("new")] },
	{ name: "print-session-unknown", args: ["-p", "--session", "nope", ...M, "x"], replies: [] },
	{ name: "json-fork", setup: [first], args: ["--mode", "json", "--fork", "<SESSION:0>", ...M, "forked"], replies: [textReply("copy")] },
	{ name: "print-fork-rlm", setup: [{ args: ["-p", ...M, "compute"], replies: [toolReply("toolu_09", "w = 9"), textReply("set")] }], args: ["-p", "--fork", "<SESSION:0>", ...M, "next"], replies: [textReply("forked")] },
	{ name: "print-fork-unknown", args: ["-p", "--fork", "nope", ...M, "x"], replies: [] },
	{ name: "print-name", args: ["-p", "-n", "  my session ", ...M, "hello"], replies: [textReply("named")] },
	{ name: "print-continue-rename", setup: [{ args: ["-p", "-n", "first name", ...M, "hello"], replies: [textReply("one")] }], args: ["-p", "-c", "-n", "second name", ...M, "again"], replies: [textReply("two")] },
	{ name: "print-continue-v3", setup: [{ write: "experimental/sessions/--v3--/2026-01-01T00-00-00-000Z_0193a000-0000-7000-8000-00000000aaaa.jsonl", content: v3Session }], args: ["-p", "--continue", ...M, "again"], replies: [textReply("two")] },
];
const out = [];
for (const c of cases) {
	const r = await run(c);
	const { tmp, ...rest } = r;
	out.push({ name: c.name, env: c.env ?? {}, replies: c.replies, ...normalize(rest, tmp) });
}
writeFileSync(new URL("./print_mode.json", import.meta.url), `${JSON.stringify(out, null, 1)}\n`);
