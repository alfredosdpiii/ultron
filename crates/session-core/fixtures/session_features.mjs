// Records the TypeScript `ultron --mode rpc` (the session worker's harness) for the session features the Rust
// session-core ports: the run's automatic retry of a transient provider error (lane events as RPC maps them, the
// format-4 operation states), `rlm.find_models` from a cell, `view_image` attaching an image to the cell's tool
// result, the image budget (ULTRON_CONTEXT_IMAGES) and the tool-round nudge (ULTRON_TOOL_ROUNDS_NUDGE). A scripted
// Anthropic SSE server on loopback answers; offline-guard.mjs is loaded into the TypeScript process.
// Usage: node crates/session-core/fixtures/session_features.mjs (writes session_features.json beside it)
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { deflateSync, crc32 } from "node:zlib";
import { normalizeJournal } from "../../cli/fixtures/journal.mjs";
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
export function textReply(text) {
	return sse([
		start("msg_1"),
		{ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
		{ type: "content_block_delta", index: 0, delta: { type: "text_delta", text } },
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 5 } },
		{ type: "message_stop" },
	]);
}
export function toolReply(id, code) {
	return sse([
		start("msg_2"),
		{ type: "content_block_start", index: 0, content_block: { type: "tool_use", id, name: "rlm", input: {} } },
		{ type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: JSON.stringify({ code }) } },
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "tool_use" }, usage: { output_tokens: 7 } },
		{ type: "message_stop" },
	]);
}
/** A transient provider error (retryable). */
const overloaded = `event: error\ndata: ${JSON.stringify({ type: "error", error: { type: "overloaded_error", message: "Overloaded" } })}\n\n`;
/** A provider error that is not retryable. */
const invalid = `event: error\ndata: ${JSON.stringify({ type: "error", error: { type: "invalid_request_error", message: "Bad request" } })}\n\n`;

/** A solid-color RGB PNG (as the TypeScript view_image test builds it). */
export function png(width, height, rgb = [220, 30, 30]) {
	const chunk = (type, data) => {
		const length = Buffer.alloc(4);
		length.writeUInt32BE(data.length);
		const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
		const crc = Buffer.alloc(4);
		crc.writeUInt32BE(crc32(body));
		return Buffer.concat([length, body, crc]);
	};
	const header = Buffer.alloc(13);
	header.writeUInt32BE(width, 0);
	header.writeUInt32BE(height, 4);
	header[8] = 8;
	header[9] = 2;
	const row = Buffer.alloc(1 + width * 3);
	for (let x = 0; x < width; x++) row.set(rgb, 1 + x * 3);
	const raw = Buffer.concat(Array.from({ length: height }, () => row));
	return Buffer.concat([
		Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
		chunk("IHDR", header),
		chunk("IDAT", deflateSync(raw)),
		chunk("IEND", Buffer.alloc(0)),
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
	const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-session-features-")));
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
	const sessions = files.map((f) => normalizeJournal(readFileSync(join(sessionsDir, f), "utf8")));
	try {
		assertOffline(join(tmp, "guard.log"));
	} finally {
		rmSync(tmp, { recursive: true, force: true });
	}
	return { lines, requests, sessions };
}

/** The `at` of every operation state of the run, in order, and the retry-wait states as written. */
function operationStates(session) {
	const records = session.transactions.flat().filter((r) => r.namespace === "pi.op.state" && r.op === "set");
	return { states: records.map((r) => r.value.at), retryWaits: records.filter((r) => r.value.at === "assistant.retry_wait").map((r) => ({ ...r.value, notBefore: 0 })) };
}

const prompt = (message) => ({ command: { id: "p", type: "prompt", message }, wait: ["response", "agent_settled"] });
const retryEvents = (lines) => lines.filter((l) => l.type === "auto_retry_start" || l.type === "auto_retry_end");
/** A tool result's content blocks as the next request carries them (the last user message's tool_result). */
const toolResults = (request) =>
	request.messages.flatMap((m) => (Array.isArray(m.content) ? m.content : [])).filter((b) => b.type === "tool_result").map((b) => b.content);
const userTexts = (request) =>
	request.messages.filter((m) => m.role === "user").flatMap((m) => (Array.isArray(m.content) ? m.content : [{ type: "text", text: m.content }])).filter((b) => b.type === "text").map((b) => b.text.replace(/^\[clock\] .*$/, "[clock] <CLOCK>"));
const lastAssistantText = (lines) => {
	const end = lines.filter((l) => l.type === "message_end" && l.message.role === "assistant").at(-1);
	return end?.message.content.filter((b) => b.type === "text").map((b) => b.text).join("") ?? null;
};

const cases = [
	{
		name: "retry-then-answer",
		settings: { retry: { baseDelayMs: 1 } },
		replies: [overloaded, textReply("Recovered.")],
		steps: [prompt("hello")],
		record: (r) => ({ events: retryEvents(r.lines), ...operationStates(r.sessions[0]), answer: lastAssistantText(r.lines), requests: r.requests.length }),
	},
	{
		name: "retry-exhausted",
		settings: { retry: { maxRetries: 1, baseDelayMs: 1 } },
		replies: [overloaded, overloaded],
		steps: [prompt("hello")],
		record: (r) => ({ events: retryEvents(r.lines), ...operationStates(r.sessions[0]), requests: r.requests.length }),
	},
	{
		name: "auto-retry-off",
		settings: { retry: { baseDelayMs: 1 } },
		replies: [overloaded],
		steps: [{ command: { id: "s", type: "set_auto_retry", enabled: false } }, prompt("hello")],
		record: (r) => ({ events: retryEvents(r.lines), ...operationStates(r.sessions[0]), requests: r.requests.length }),
	},
	{
		name: "not-retryable",
		settings: { retry: { baseDelayMs: 1 } },
		replies: [invalid],
		steps: [prompt("hello")],
		record: (r) => ({ events: retryEvents(r.lines), ...operationStates(r.sessions[0]), requests: r.requests.length }),
	},
	{
		name: "find-models",
		replies: [
			toolReply("toolu_1", "import json\nprint(json.dumps(await rlm.host_request('rlm.find_models', {})))\nprint(await rlm.find_models())"),
			textReply("ok"),
		],
		steps: [prompt("models")],
		record: (r) => ({ toolResults: toolResults(r.requests[1]) }),
	},
	{
		name: "view-image",
		files: { "chart.png": png(6, 4) },
		replies: [toolReply("toolu_1", "print(await view_image('chart.png'))"), textReply("seen")],
		steps: [prompt("look at chart.png")],
		record: (r) => ({ toolResults: toolResults(r.requests[1]), png: png(6, 4).toString("base64") }),
	},
	{
		name: "image-budget",
		env: { ULTRON_CONTEXT_IMAGES: "2" },
		files: { "a.png": png(2, 2), "b.png": png(3, 3), "c.png": png(4, 4) },
		replies: [
			toolReply("toolu_1", "print(await view_image('a.png'))"),
			toolReply("toolu_2", "print(await view_image('b.png'))"),
			toolReply("toolu_3", "print(await view_image('c.png'))"),
			textReply("seen all"),
		],
		steps: [prompt("look at the images")],
		record: (r) => ({ toolResults: toolResults(r.requests[3]) }),
	},
	{
		name: "tool-round-nudge",
		env: { ULTRON_TOOL_ROUNDS_NUDGE: "3" },
		replies: [toolReply("toolu_1", "print('round 1')"), toolReply("toolu_2", "print('round 2')"), toolReply("toolu_3", "print('round 3')"), textReply("ANSWERED AFTER NUDGE")],
		steps: [prompt("research something open-ended")],
		record: (r) => ({ userTexts: r.requests.map(userTexts), answer: lastAssistantText(r.lines), requests: r.requests.length }),
	},
];

const out = { generator: "crates/session-core/fixtures/session_features.mjs", cases: {} };
for (const c of cases) {
	const result = await run(c);
	out.cases[c.name] = {
		...c.record(result),
		...(c.files ? { files: Object.fromEntries(Object.entries(c.files).map(([name, bytes]) => [name, bytes.toString("base64")])) } : {}),
	};
}
writeFileSync(new URL("./session_features.json", import.meta.url), `${JSON.stringify(out, null, "\t")}\n`);
