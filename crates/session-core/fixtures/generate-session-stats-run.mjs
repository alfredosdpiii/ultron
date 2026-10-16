// Records the runtime counters the TypeScript session worker (`ultron --mode rpc`) writes to its session file
// (session-stats.ts, the session value `ultron.module/stats` behind `/usage`): one root turn whose cells print, run a
// shell command that writes a file (Loki checks it before it runs), raise, mask a secret and write a file through
// `write()` (Loki checks it before writing). A scripted Anthropic SSE server on loopback answers; offline-guard.mjs
// is loaded into the TypeScript process. The last stored document is recorded with its times and guard
// milliseconds zeroed. The Rust test (crates/session-core/tests/session_stats_run.rs) runs the same replies through
// the session actor.
// Usage: node crates/session-core/fixtures/generate-session-stats-run.mjs (writes session_stats_run.json beside it;
// deterministic)
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { assertOffline, guardNodeOptions } from "../../cli/fixtures/offline-guard.mjs";

const root = new URL("../../../", import.meta.url).pathname;
const SYSTEM_PATH = "/usr/local/bin:/usr/bin:/bin";

function sse(events) {
	return events.map((e) => `event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`).join("");
}
const start = (id) => ({
	type: "message_start",
	message: { id, type: "message", role: "assistant", model: "claude-sonnet-4-5", content: [], stop_reason: null, usage: { input_tokens: 10, output_tokens: 1 } },
});
function textReply(text) {
	return sse([
		start("msg_1"),
		{ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
		{ type: "content_block_delta", index: 0, delta: { type: "text_delta", text } },
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 5 } },
		{ type: "message_stop" },
	]);
}
function toolReply(id, code) {
	return sse([
		start("msg_2"),
		{ type: "content_block_start", index: 0, content_block: { type: "tool_use", id, name: "rlm", input: {} } },
		{ type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: JSON.stringify({ code }) } },
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "tool_use" }, usage: { output_tokens: 7 } },
		{ type: "message_stop" },
	]);
}
async function run(c) {
	const requests = [];
	const server = createServer((req, res) => {
		let body = "";
		req.on("data", (chunk) => (body += chunk));
		req.on("end", () => {
			const parsed = JSON.parse(body || "null");
			// Session naming is off; anything else is a model request.
			requests.push(parsed);
			const reply = c.replies[requests.length - 1] ?? textReply("(no more replies)");
			res.writeHead(200, { "content-type": "text/event-stream" });
			res.end(reply);
		});
	});
	await new Promise((r) => server.listen(0, "127.0.0.1", r));
	const port = server.address().port;
	const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-session-stats-")));
	const agentDir = join(tmp, "agent");
	const cwd = join(tmp, "cwd");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(cwd);
	writeFileSync(join(agentDir, "models.json"), JSON.stringify({ providers: { anthropic: { baseUrl: `http://127.0.0.1:${port}` } } }));
	if (c.settings) writeFileSync(join(agentDir, "settings.json"), JSON.stringify(c.settings));
	for (const [name, bytes] of Object.entries(c.files ?? {})) writeFileSync(join(cwd, name), bytes);
	const child = spawn(process.execPath, [join(root, "packages/coding-agent/src/cli.ts"), "--mode", "rpc", "--model", "anthropic/claude-sonnet-4-5"], {
		cwd,
		env: {
			PATH: SYSTEM_PATH,
			HOME: tmp,
			ULTRON_CODING_AGENT_DIR: agentDir,
			ULTRON_SERVER_DIR: join(tmp, "server"),
			ANTHROPIC_API_KEY: "test-key",
			ULTRON_HINDSIGHT_URL: "off",
			ULTRON_AUTO_NAME: "off",
			PI_OFFLINE: "1",
			NO_COLOR: "1",
			TZ: "UTC",
			NODE_OPTIONS: guardNodeOptions,
			ULTRON_GUARD_LOG: join(tmp, "guard.log"),
			...c.env,
		},
		stdio: ["pipe", "pipe", "pipe"],
	});
	const lines = [];
	let buffer = "";
	let stderr = "";
	child.stdout.on("data", (chunk) => {
		buffer += chunk;
		let i;
		while ((i = buffer.indexOf("\n")) !== -1) {
			lines.push(JSON.parse(buffer.slice(0, i)));
			buffer = buffer.slice(i + 1);
		}
	});
	child.stderr.on("data", (chunk) => (stderr += chunk));
	const closed = new Promise((r) => child.on("close", r));
	for (const step of c.steps) {
		const from = lines.length;
		child.stdin.write(`${JSON.stringify(step.command)}\n`);
		const deadline = Date.now() + 120_000;
		const want = step.wait ?? ["response"];
		while (!want.every((w) => lines.slice(from).some((l) => (w === "response" ? l.type === "response" && l.id === step.command.id : l.type === w)))) {
			if (Date.now() > deadline) throw new Error(`${c.name}: timed out on ${JSON.stringify(step)}\n${stderr}`);
			await new Promise((r) => setTimeout(r, 20));
		}
	}
	child.stdin.end();
	await closed;
	server.close();
	const sessionsDir = join(agentDir, "experimental", "sessions");
	let files = [];
	try {
		files = readdirSync(sessionsDir, { recursive: true }).filter((f) => f.endsWith(".jsonl")).sort();
	} catch {}
	const sessions = files.map((f) => readFileSync(join(sessionsDir, f), "utf8"));
	try {
		assertOffline(join(tmp, "guard.log"));
	} finally {
		rmSync(tmp, { recursive: true, force: true });
	}
	return { lines, requests, sessions };
}

const prompt = (message) => ({ command: { id: "p", type: "prompt", message }, wait: ["response", "agent_settled"] });
/** A tool result's content blocks as the last request carries them. */
const toolResults = (request) =>
	request.messages.flatMap((m) => (Array.isArray(m.content) ? m.content : [])).filter((b) => b.type === "tool_result").map((b) => b.content);

/** The last `ultron.module/stats` document a session file holds, times and guard milliseconds zeroed. */
function storedStats(text) {
	let stats = null;
	for (const line of text.split("\n").slice(1).filter(Boolean)) {
		for (const write of [JSON.parse(line)].flat()) {
			if (write.kind === "value" && write.namespace === "ultron.module" && write.key === "stats" && write.op === "set") stats = write.value;
		}
	}
	if (stats === null) return null;
	stats.since = 0;
	stats.updatedAt = 0;
	for (const guard of Object.values(stats.guards)) guard.ms = 0;
	return stats;
}

const CELLS = [
	"print('one')",
	"print(await bash('echo hi > out.txt'))",
	"1/0",
	"print('ghp_' + 'a' * 36)",
	"print(await write('notes.md', 'hello'))",
];
const replies = [...CELLS.map((code, i) => toolReply(`toolu_${i + 1}`, code)), textReply("done")];
const c = { name: "cells", replies, steps: [prompt("count")] };
const result = await run(c);
if (result.sessions.length !== 1) throw new Error(`expected one session file, got ${result.sessions.length}`);
const out = {
	generator: "crates/session-core/fixtures/generate-session-stats-run.mjs",
	cells: CELLS,
	stats: storedStats(result.sessions[0]),
	toolResults: toolResults(result.requests.at(-1)),
};
writeFileSync(new URL("./session_stats_run.json", import.meta.url), `${JSON.stringify(out, null, "\t")}\n`);
