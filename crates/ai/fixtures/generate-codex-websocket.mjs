// Generates crates/ai/fixtures/codex-websocket.json: the OpenAI Codex provider's WebSocket transport
// (packages/ai/src/api/openai-codex-responses.ts: `auto`, `websocket`, `websocket-cached`) from this worktree's
// TypeScript, against a scripted fake Codex WebSocket server on 127.0.0.1 (node's own `WebSocket` connects to it).
// Each scenario is a sequence of provider calls; each connection the server accepts follows the next script of the
// scenario (handshake ok, rejected, hanging or dropped; per received `response.create`, the events to send and what
// to do then: nothing, close with a code, or drop the connection). Recorded: for every connection its path, the
// handshake headers, each message received, and each close frame received; for every call the pushed events, the
// final message and the SSE fallback requests (a stubbed `fetch` with canned SSE responses); after each scenario the
// debug stats of its sessions, then the sessions are closed (`closeOpenAICodexWebSocketSessions`). Date.now is pinned
// (and moved forward where a scenario says so); runs behind offline-guard.mjs with provider variables scrubbed.
// Usage: node crates/ai/fixtures/generate-codex-websocket.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { createHash } from "node:crypto";
import { writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const src = (path) => import(pathToFileURL(join(root, "packages/ai/src", path)).href);
for (const name of Object.keys(process.env)) {
	if (/^(PI_|AZURE_|OPENAI_|ULTRON_|AWS_|HTTPS?_PROXY|https?_proxy|NO_PROXY|no_proxy)/.test(name)) delete process.env[name];
}
const NOW = 1767225600000;
let offset = 0;
Date.now = () => NOW + offset;

const { AssistantMessageEventStream } = await src("utils/event-stream.ts");
const { getPiUserAgent } = await src("utils/pi-user-agent.ts");
const api = await src("api/openai-codex-responses.ts");
const UA = getPiUserAgent();
const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

let recording = null;
const originalPush = AssistantMessageEventStream.prototype.push;
AssistantMessageEventStream.prototype.push = function (event) {
	if (recording) {
		recording.events.push(clone(event));
		const trigger = recording.abortOn;
		if (trigger && event.type === trigger.type && ++recording.seen === trigger.nth) recording.controller.abort();
	}
	return originalPush.call(this, event);
};

// ------------------------------------------------------------------------------------------- the fake WS server

const UNRECORDED = new Set(["host", "connection", "upgrade", "sec-websocket-key", "sec-websocket-version", "sec-websocket-extensions", "accept", "accept-language", "accept-encoding", "sec-fetch-mode", "pragma", "cache-control", "content-length"]);
const UUIDV7 = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
function pickHeaders(headers) {
	const out = {};
	for (const [key, value] of Object.entries(headers)) {
		if (UNRECORDED.has(key)) continue;
		out[key] = UUIDV7.test(value) ? "<uuidv7>" : value.replaceAll(UA, "<pi-user-agent>");
	}
	return Object.fromEntries(Object.entries(out).sort(([a], [b]) => (a < b ? -1 : 1)));
}

function frame(opcode, payload) {
	const data = Buffer.from(payload);
	let head;
	if (data.length < 126) head = Buffer.from([0x80 | opcode, data.length]);
	else if (data.length < 65536) {
		head = Buffer.alloc(4);
		head[0] = 0x80 | opcode;
		head[1] = 126;
		head.writeUInt16BE(data.length, 2);
	} else {
		head = Buffer.alloc(10);
		head[0] = 0x80 | opcode;
		head[1] = 127;
		head.writeBigUInt64BE(BigInt(data.length), 2);
	}
	return Buffer.concat([head, data]);
}
const closePayload = (code, reason) => (code === undefined ? Buffer.alloc(0) : Buffer.concat([Buffer.from([code >> 8, code & 255]), Buffer.from(reason ?? "")]));

let current = null;
let lastActivity = 0;
const touch = () => (lastActivity = performance.now());
const server = createServer((_req, res) => {
	res.writeHead(404);
	res.end();
});
server.on("upgrade", (req, sock) => {
	touch();
	const script = current.scripts[current.connections.length] ?? { handshake: "reject" };
	const conn = { path: req.url, headers: pickHeaders(req.headers), received: [], closes: [], pongs: 0 };
	current.connections.push(conn);
	if (script.handshake === "reject") {
		sock.end("HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
		return;
	}
	if (script.handshake === "destroy") {
		sock.destroy();
		return;
	}
	sock.on("error", () => {});
	sock.on("close", touch);
	if (script.handshake === "hang") return;
	const accept = createHash("sha1").update(`${req.headers["sec-websocket-key"]}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest("base64");
	sock.write(`HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`);
	let buffer = Buffer.alloc(0);
	let closed = false;
	const act = (turn) => {
		if (!turn) return;
		if (turn.ping) sock.write(frame(0x9, "hb"));
		for (const event of turn.events ?? []) {
			const text = typeof event === "string" ? event : JSON.stringify(event);
			sock.write(frame(turn.binary ? 0x2 : 0x1, text));
		}
		if (turn.then === "drop") sock.destroy();
		else if (Array.isArray(turn.then)) {
			closed = true;
			sock.write(frame(0x8, closePayload(turn.then[0], turn.then[1])));
		}
	};
	sock.on("data", (chunk) => {
		touch();
		buffer = Buffer.concat([buffer, chunk]);
		while (buffer.length >= 2) {
			const opcode = buffer[0] & 0x0f;
			let len = buffer[1] & 0x7f;
			let at = 2;
			if (len === 126) {
				if (buffer.length < 4) return;
				len = buffer.readUInt16BE(2);
				at = 4;
			} else if (len === 127) {
				if (buffer.length < 10) return;
				len = Number(buffer.readBigUInt64BE(2));
				at = 10;
			}
			if (buffer.length < at + 4 + len) return;
			const mask = buffer.subarray(at, at + 4);
			const payload = Buffer.from(buffer.subarray(at + 4, at + 4 + len)).map((b, i) => b ^ mask[i % 4]);
			buffer = buffer.subarray(at + 4 + len);
			if (opcode === 0x1) {
				conn.received.push(JSON.parse(Buffer.from(payload).toString("utf8")));
				act(script.turns?.[conn.received.length - 1]);
			} else if (opcode === 0x8) {
				conn.closes.push(payload.length >= 2 ? { code: payload.readUInt16BE(0), reason: Buffer.from(payload.subarray(2)).toString("utf8") } : { code: null, reason: "" });
				if (!closed) {
					closed = true;
					sock.write(frame(0x8, payload.length >= 2 ? payload.subarray(0, 2) : Buffer.alloc(0)));
				}
				sock.end();
			} else if (opcode === 0xa) {
				conn.pongs++;
			}
		}
	});
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;
const normalize = (value) => JSON.parse(JSON.stringify(value).replaceAll(String(port), "<port>"));

async function quiet() {
	touch();
	while (performance.now() - lastActivity < 400) await new Promise((resolve) => setTimeout(resolve, 100));
}

// ------------------------------------------------------------------------------------------------ the requests

const b64 = (v) => Buffer.from(JSON.stringify(v)).toString("base64").replace(/=+$/, "");
// Unsigned fakes; the signature says so, which also keeps scripts/secret-scan.mjs from taking them for real tokens.
const jwt = (account) => `${b64({ alg: "none" })}.${b64({ "https://api.openai.com/auth": { chatgpt_account_id: account } })}.example-sig`;
const TOKEN = jwt("acc-123");
const TOKEN_B = jwt("acc-456");
const MODEL = {
	id: "gpt-5.5",
	name: "Test Model",
	api: "openai-codex-responses",
	provider: "openai-codex",
	baseUrl: "http://127.0.0.1:<port>/backend-api",
	reasoning: true,
	input: ["text", "image"],
	cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 },
	contextWindow: 200000,
	maxTokens: 32000,
};
const system = (content) => ({ role: "system", content, timestamp: 1 });
const user = (content, timestamp = 2) => ({ role: "user", content, timestamp });
const ev = (type, fields) => ({ type, ...fields });
const resp = (id, status = "completed", extra = {}) => ({ id, object: "response", status, output: [], usage: { input_tokens: 20, output_tokens: 5, total_tokens: 25, input_tokens_details: { cached_tokens: 8 }, output_tokens_details: { reasoning_tokens: 0 } }, ...extra });
const text = (t, n = 1) => [
	ev("response.output_item.added", { output_index: 0, item: { type: "message", id: `msg_${n}`, role: "assistant", content: [] } }),
	ev("response.output_text.delta", { output_index: 0, delta: t }),
	ev("response.output_item.done", { output_index: 0, item: { type: "message", id: `msg_${n}`, role: "assistant", status: "completed", content: [{ type: "output_text", text: t, annotations: [] }] } }),
];
const done = (id, type = "response.completed", extra = {}) => ev(type, { response: resp(id, "completed", extra) });
const reply = (t, id, n = 1, extra = {}) => ({ events: [...text(t, n), done(id)], ...extra });
const sseOk = (t = "via sse") => ({ chunks: [...text(t, 9), done("resp_sse")].map((d) => `data: ${JSON.stringify(d)}\n\n`) });
const base = [system("You are terse."), user("hi")];

/** A call's context: `base` messages, then the results of earlier calls (by index) and new messages, in order. */
const ctx = (...parts) => ({ parts });

const SCENARIOS = [
	{
		name: "auto-cached-deltas",
		sessions: ["s-delta"],
		scripts: [{ handshake: "ok", turns: [reply("one", "resp_a", 1, { ping: true }), reply("two", "resp_b", 2), reply("three", "resp_c", 3), reply("four", "resp_d", 4)] }],
		calls: [
			{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-delta" } },
			{ context: ctx({ result: 0 }, user("next", 5)), options: { apiKey: TOKEN, sessionId: "s-delta", transport: "auto" } },
			{ context: ctx({ result: 0 }, user("next", 5), { result: 1 }, user("third", 6)), options: { apiKey: TOKEN, sessionId: "s-delta", transport: "websocket" } },
			{ context: ctx({ result: 0 }, user("next", 5), { result: 1 }, user("third", 6), { result: 2 }, user("fourth", 7)), options: { apiKey: TOKEN, sessionId: "s-delta", transport: "websocket-cached" } },
		],
	},
	{
		name: "cached-delta-reset-when-request-changes",
		sessions: ["s-change"],
		scripts: [{ handshake: "ok", turns: [reply("one", "resp_a"), reply("two", "resp_b", 2)] }],
		calls: [
			{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-change", transport: "websocket-cached" } },
			{ context: ctx({ result: 0 }, user("next", 5)), options: { apiKey: TOKEN, sessionId: "s-change", transport: "websocket-cached", textVerbosity: "high" } },
		],
	},
	{
		name: "websocket-uncached-binary-frames",
		sessions: ["s-ws"],
		scripts: [{ handshake: "ok", turns: [{ ...reply("bin", "resp_w"), binary: true }, reply("again", "resp_w2", 2)] }],
		calls: [
			{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-ws", transport: "websocket", headers: { "X-Extra": "1" } } },
			{ context: ctx({ result: 0 }, user("more", 5)), options: { apiKey: TOKEN, sessionId: "s-ws", transport: "websocket" } },
		],
	},
	{
		name: "no-session-one-shot",
		sessions: ["ignored"],
		scripts: [{ handshake: "ok", turns: [reply("solo", "resp_s")] }, { handshake: "ok", turns: [reply("solo2", "resp_s2")] }],
		calls: [
			{ context: ctx(), options: { apiKey: TOKEN, sessionId: "ignored", cacheRetention: "none" } },
			{ context: ctx(), options: { apiKey: TOKEN, transport: "websocket-cached" } },
		],
	},
	{
		name: "accounts-have-their-own-sockets",
		sessions: ["s-acc"],
		scripts: [
			{ handshake: "ok", turns: [reply("a1", "resp_1"), reply("a3", "resp_3", 3)] },
			{ handshake: "ok", turns: [reply("b2", "resp_2", 2)] },
		],
		calls: [
			{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-acc", transport: "websocket-cached" } },
			{ context: ctx(), options: { apiKey: TOKEN_B, sessionId: "s-acc", transport: "websocket-cached" } },
			{ context: ctx({ result: 0 }, user("again", 5)), options: { apiKey: TOKEN, sessionId: "s-acc", transport: "websocket-cached" } },
		],
	},
	{
		name: "connect-timeout-falls-back-and-is-remembered",
		sessions: ["s-timeout"],
		scripts: [{ handshake: "hang" }],
		sse: [sseOk(), sseOk("still sse")],
		calls: [
			{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-timeout", websocketConnectTimeoutMs: 200 } },
			{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-timeout", transport: "websocket" } },
		],
	},
	{
		name: "handshake-rejected-falls-back",
		sessions: ["s-reject"],
		scripts: [{ handshake: "reject" }],
		sse: [sseOk()],
		calls: [{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-reject" } }],
	},
	{
		name: "connection-dropped-before-start-falls-back",
		sessions: ["s-drop"],
		scripts: [{ handshake: "ok", turns: [{ events: [], then: "drop" }] }],
		sse: [sseOk()],
		calls: [{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-drop" } }],
	},
	{
		name: "connection-limit-reconnects-once",
		sessions: ["s-limit"],
		scripts: [
			{ handshake: "ok", turns: [{ events: [ev("error", { error: { code: "websocket_connection_limit_reached", message: "too many" } })] }] },
			{ handshake: "ok", turns: [reply("after limit", "resp_l")] },
		],
		calls: [{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-limit" } }],
	},
	{
		name: "connection-limit-twice-falls-back",
		sessions: ["s-limit2"],
		scripts: [
			{ handshake: "ok", turns: [{ events: [ev("error", { code: "websocket_connection_limit_reached" })] }] },
			{ handshake: "ok", turns: [{ events: [ev("error", { code: "websocket_connection_limit_reached" })] }] },
		],
		sse: [sseOk()],
		calls: [{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-limit2" } }],
	},
	{
		name: "idle-before-start-falls-back",
		sessions: ["s-idle"],
		scripts: [{ handshake: "ok", turns: [{ events: [] }] }],
		sse: [sseOk()],
		calls: [{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-idle", timeoutMs: 300 } }],
	},
	{
		name: "idle-after-start-errors",
		sessions: ["s-idle2"],
		scripts: [{ handshake: "ok", turns: [{ events: text("partial").slice(0, 2) }] }],
		calls: [{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-idle2", timeoutMs: 300 } }],
	},
	{
		name: "closed-too-big-before-start-falls-back",
		sessions: ["s-big"],
		scripts: [{ handshake: "ok", turns: [{ events: [], then: [1009] }] }],
		sse: [sseOk()],
		calls: [{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-big" } }],
	},
	{
		name: "closed-after-start-errors",
		sessions: ["s-closed"],
		scripts: [{ handshake: "ok", turns: [{ events: text("cut").slice(0, 2), then: [1011, "boom"] }] }],
		calls: [{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-closed" } }],
	},
	{
		name: "previous-response-missing-retries-full-context",
		sessions: ["s-prev"],
		scripts: [
			{ handshake: "ok", turns: [reply("first", "resp_p1"), { events: [ev("error", { error: { code: "previous_response_not_found", message: "gone" } })] }] },
			{ handshake: "ok", turns: [reply("second", "resp_p2", 2)] },
		],
		calls: [
			{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-prev", transport: "auto" } },
			{ context: ctx({ result: 0 }, user("next", 5)), options: { apiKey: TOKEN, sessionId: "s-prev", transport: "auto" } },
		],
	},
	{
		name: "api-error-event-is-not-a-transport-failure",
		sessions: ["s-api"],
		scripts: [{ handshake: "ok", turns: [{ events: [...text("x").slice(0, 2), ev("error", { error: { code: "server_error", message: "Model crashed" } })] }] }],
		calls: [{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-api" } }],
	},
	{
		name: "response-failed-before-start",
		sessions: ["s-failed"],
		scripts: [{ handshake: "ok", turns: [{ events: [ev("response.failed", { response: { error: { code: "bad", message: "Model failed" } } })] }] }],
		calls: [{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-failed" } }],
	},
	{
		name: "invalid-json-event",
		sessions: ["s-json"],
		scripts: [{ handshake: "ok", turns: [{ events: ["not json"] }] }],
		calls: [{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-json" } }],
	},
	{
		name: "connection-age-limit-opens-a-fresh-socket",
		sessions: ["s-age"],
		scripts: [{ handshake: "ok", turns: [reply("old", "resp_o")] }, { handshake: "ok", turns: [reply("new", "resp_n", 2)] }],
		calls: [
			{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-age", transport: "websocket-cached" } },
			{ advanceMs: 55 * 60 * 1000, context: ctx({ result: 0 }, user("later", 5)), options: { apiKey: TOKEN, sessionId: "s-age", transport: "websocket-cached" } },
		],
	},
	{
		name: "abort-mid-stream-closes-the-socket",
		sessions: ["s-abort"],
		scripts: [{ handshake: "ok", turns: [{ events: [...text("one").slice(0, 2), ev("response.output_text.delta", { output_index: 0, delta: "two" })] }] }],
		calls: [{ abortOn: { type: "text_delta", nth: 1 }, context: ctx(), options: { apiKey: TOKEN, sessionId: "s-abort" } }],
	},
	{
		name: "incomplete-response-and-tool-call-continuation",
		sessions: ["s-tool"],
		scripts: [
			{
				handshake: "ok",
				turns: [
					{
						events: [
							ev("response.output_item.added", { output_index: 0, item: { type: "function_call", id: "fc_1", call_id: "call_1", name: "read", arguments: "" } }),
							ev("response.function_call_arguments.delta", { output_index: 0, delta: '{"path":"a"}' }),
							ev("response.output_item.done", { output_index: 0, item: { type: "function_call", id: "fc_1", call_id: "call_1", name: "read", arguments: '{"path":"a"}' } }),
							done("resp_t1", "response.done", { end_turn: false }),
						],
					},
					{ events: [...text("read it"), done("resp_t2", "response.incomplete", { status: "incomplete", incomplete_details: { reason: "max_output_tokens" } })] },
				],
			},
		],
		calls: [
			{ context: ctx(), options: { apiKey: TOKEN, sessionId: "s-tool" } },
			{ context: ctx({ result: 0 }, { toolResult: ["call_1|fc_1", "read", "file a"] }), options: { apiKey: TOKEN, sessionId: "s-tool" } },
		],
	},
];

function buildContext(spec, results) {
	const messages = [...base];
	for (const part of spec.parts) {
		if (part.result !== undefined) messages.push(results[part.result]);
		else if (part.toolResult) {
			const [toolCallId, toolName, content] = part.toolResult;
			messages.push({ role: "toolResult", toolCallId, toolName, content: [{ type: "text", text: content }], isError: false, timestamp: 4 });
		} else messages.push(part);
	}
	return { messages };
}

function stubFetch(queue, requests) {
	return async (url, init = {}) => {
		const headers = {};
		new Headers(init.headers).forEach((value, key) => {
			headers[key] = value.replaceAll(UA, "<pi-user-agent>");
		});
		const body = typeof init.body === "string" ? JSON.parse(init.body) : JSON.parse(new TextDecoder().decode(init.body));
		requests.push({ url: String(url), method: init.method ?? "GET", headers: Object.fromEntries(Object.entries(headers).sort(([a], [b]) => (a < b ? -1 : 1))), body });
		const canned = queue.shift();
		if (!canned) throw new Error("unexpected request");
		const chunks = canned.chunks.map((c) => new TextEncoder().encode(c));
		return new Response(new ReadableStream({ pull(controller) { const next = chunks.shift(); if (next === undefined) controller.close(); else controller.enqueue(next); } }), { status: 200, headers: { "content-type": "text/event-stream" } });
	};
}

const savedBuiltin = process.getBuiltinModule;
// No zstd for the SSE body, as the Rust port sends it uncompressed.
process.getBuiltinModule = (id) => (id === "node:zlib" ? undefined : savedBuiltin.call(process, id));
api.resetOpenAICodexWebSocketDebugStats();
const scenarios = [];
for (const scenario of SCENARIOS) {
	current = { scripts: scenario.scripts, connections: [] };
	const sseQueue = [...(scenario.sse ?? [])];
	const model = { ...MODEL, baseUrl: MODEL.baseUrl.replace("<port>", String(port)) };
	const results = [];
	const calls = [];
	for (const call of scenario.calls) {
		offset += call.advanceMs ?? 0;
		const context = buildContext(call.context, results);
		const controller = new AbortController();
		recording = { events: [], abortOn: call.abortOn, seen: 0, controller };
		const requests = [];
		const s = api.stream(model, context, { ...call.options, fetch: stubFetch(sseQueue, requests), signal: controller.signal });
		const result = clone(await s.result());
		results.push(result);
		calls.push({ advanceMs: call.advanceMs ?? 0, abortOn: call.abortOn ?? null, context: normalize(context), options: call.options, events: normalize(recording.events), result: normalize(result), sseRequests: normalize(requests) });
		recording = null;
	}
	await quiet();
	const stats = Object.fromEntries(scenario.sessions.map((id) => [id, clone(api.getOpenAICodexWebSocketDebugStats(id))]));
	api.closeOpenAICodexWebSocketSessions();
	await quiet();
	api.resetOpenAICodexWebSocketDebugStats();
	scenarios.push({ name: scenario.name, scripts: scenario.scripts, sse: scenario.sse ?? [], calls, stats, connections: normalize(current.connections) });
}
process.getBuiltinModule = savedBuiltin;
server.close();
assertOffline();
// Diagnostics carry the JS stack; the Rust port has none.
for (const s of scenarios) for (const c of s.calls) for (const d of c.result?.diagnostics ?? []) delete d.error?.stack;
for (const s of scenarios) for (const c of s.calls) for (const e of c.events) for (const d of (e.message ?? e.error ?? e.partial)?.diagnostics ?? []) delete d.error?.stack;
writeFileSync(join(here, "codex-websocket.json"), `${JSON.stringify({ generator: "crates/ai/fixtures/generate-codex-websocket.mjs", now: NOW, model: MODEL, scenarios }, null, "\t")}\n`);
console.log(`${scenarios.length} scenarios`);
