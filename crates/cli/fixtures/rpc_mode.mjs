// Records the TypeScript `ultron --mode rpc` against a scripted Anthropic SSE server: each case sends JSONL commands
// one step at a time (a step waits for the lines it names before the next is sent, then creates its `touch` file in the
// working directory, or as soon as its `touchAfter` line was seen), then closes stdin and records
// every stdout line, the provider requests, the exit code and the session files left (format-4 journals, normalized
// by journal.mjs). Ids, timestamps and temp paths are normalized.
// Usage: node crates/cli/fixtures/rpc_mode.mjs
import { spawn } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, readdirSync, readFileSync, rmSync, realpathSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { normalizeJournal } from "./journal.mjs";

function sse(events) {
	return events.map((e) => `event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`).join("");
}
const start = (id) => ({ type: "message_start", message: { id, type: "message", role: "assistant", model: "claude-sonnet-4-5", content: [], stop_reason: null, usage: { input_tokens: 10, output_tokens: 1 } } });
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

const root = new URL("../../../", import.meta.url).pathname;
// System directories only: a user-installed `claude` CLI would add the claude-code provider's models.
const SYSTEM_PATH = "/usr/local/bin:/usr/bin:/bin";

/** `"$<id>.<path>"` string values are taken from the earlier response to command <id>. */
export function resolveRefs(command, lines) {
	return Object.fromEntries(
		Object.entries(command).map(([key, value]) => {
			if (typeof value !== "string" || !value.startsWith("$")) return [key, value];
			const [id, ...path] = value.slice(1).split(".");
			let found = lines.find((line) => line.type === "response" && line.id === id);
			for (const part of path) found = found?.[part];
			return [key, found];
		}),
	);
}

/** A step is done once every awaited line was seen: "response" is the response to the step's command id. */
function stepDone(step, lines) {
	return (step.wait ?? ["response"]).every((want) =>
		lines.some((line) => (want === "response" ? line.type === "response" && line.id === step.command.id : line.type === want)),
	);
}

/** Run one case. A reply is SSE text, or `{ status, contentType, body }` for a non-streaming (error) answer. */
export async function run(c) {
	const requests = [];
	const server = createServer((req, res) => {
		let body = "";
		req.on("data", (chunk) => (body += chunk));
		req.on("end", () => {
			requests.push({ method: req.method, url: req.url, body: JSON.parse(body || "null") });
			const reply = c.replies[requests.length - 1] ?? textReply("(no more replies)");
			const { status = 200, contentType = "text/event-stream", body: replyBody } =
				typeof reply === "string" ? { body: reply } : reply;
			res.writeHead(status, { "content-type": contentType });
			res.end(replyBody);
		});
	});
	await new Promise((r) => server.listen(0, "127.0.0.1", r));
	const port = server.address().port;
	const tmp = realpathSync(mkdtempSync(join(tmpdir(), "ultron-rpc-fixture-")));
	const agentDir = join(tmp, "agent");
	const cwd = join(tmp, "cwd");
	mkdirSync(agentDir, { recursive: true });
	mkdirSync(cwd);
	// `c.provider`: more of the provider's models.json configuration (`modelOverrides`).
	writeFileSync(join(agentDir, "models.json"), JSON.stringify({ providers: { anthropic: { baseUrl: `http://127.0.0.1:${port}`, ...c.provider } } }));
	const child = spawn(process.execPath, [join(root, "packages/coding-agent/src/cli.ts"), "--mode", "rpc", ...c.args], {
		cwd,
		env: { PATH: SYSTEM_PATH, HOME: tmp, ULTRON_CODING_AGENT_DIR: agentDir, ULTRON_SERVER_DIR: join(tmp, "server"), ANTHROPIC_API_KEY: "test-key", ULTRON_HINDSIGHT_URL: "off", PI_OFFLINE: "1", NO_COLOR: "1", TZ: "UTC" },
		stdio: ["pipe", "pipe", "pipe"],
	});
	const lines = [];
	let buffer = "";
	let stderr = "";
	let wake = () => {};
	child.stdout.on("data", (chunk) => {
		buffer += chunk;
		let i;
		while ((i = buffer.indexOf("\n")) !== -1) {
			lines.push(JSON.parse(buffer.slice(0, i)));
			buffer = buffer.slice(i + 1);
		}
		wake();
	});
	child.stderr.on("data", (chunk) => (stderr += chunk));
	const closed = new Promise((r) => child.on("close", r));
	for (const step of c.steps) {
		const from = lines.length;
		const command = typeof step.command === "string" ? step.command : JSON.stringify(resolveRefs(step.command, lines));
		child.stdin.write(`${command}\n`);
		const deadline = Date.now() + 60_000;
		// `touch`: create this file in the working directory (a readiness signal for a cell) once the step is done,
		// or once its `touchAfter` line was seen when the step's later lines need the cell to go on.
		let touched = !step.touch;
		const touch = () => {
			if (!touched) writeFileSync(join(cwd, step.touch), "");
			touched = true;
		};
		while (!stepDone(step, lines.slice(from))) {
			if (step.touchAfter && stepDone({ ...step, wait: [step.touchAfter] }, lines.slice(from))) touch();
			if (Date.now() > deadline) throw new Error(`${c.name}: timed out on ${JSON.stringify(step)}\n${stderr}`);
			await new Promise((r) => {
				wake = r;
				setTimeout(r, 50);
			});
		}
		touch();
	}
	child.stdin.end();
	const code = await closed;
	server.close();
	// The session files the run left (format-4 journals), normalized as print_mode.mjs normalizes them.
	const sessionsDir = join(agentDir, "experimental", "sessions");
	let files = [];
	try {
		files = readdirSync(sessionsDir, { recursive: true }).filter((f) => f.endsWith(".jsonl")).sort();
	} catch {}
	const sessions = files.map((f) => normalizeJournal(readFileSync(join(sessionsDir, f), "utf8")));
	rmSync(tmp, { recursive: true, force: true });
	return { code, lines, stderr, requests, tmp, sessions };
}

/** Run-specific text and times become placeholders: temp paths, the checkout, the provider port, file-name times, the clock line. */
export function scrub(value, tmp) {
	const walk = (v, key) => {
		if (Array.isArray(v)) return v.map((x) => walk(x, key));
		if (v !== null && typeof v === "object") return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, walk(x, k)]));
		if (typeof v === "number" && /^(timestamp|startedAt|endedAt|createdAt|modifiedAt)$/.test(key)) return 0;
		if (typeof v !== "string") return v;
		if (/^(timestamp|labelTimestamp)$/.test(key)) return "<time>";
		return v
			.split(tmp).join("<TMP>")
			.split(root.replace(/\/$/, "")).join("<root>")
			.split(tmp.slice(1).replace(/[/\\:]/g, "-")).join("<TMPNAME>")
			.replace(/127\.0\.0\.1:\d+/g, "127.0.0.1:<PORT>")
			.replace(/\d{4}-\d\d-\d\dT\d\d-\d\d-\d\d-\d{3}Z_/g, "<TIME>_")
			.replace(/\[clock\] \d{4}-\d\d-\d\d \d\d:\d\d UTC[+-]\d\d:\d\d/g, "[clock] <CLOCK>");
	};
	return walk(value, "");
}

const UUID = /[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g;
const ID_KEY = /^(id|sessionId|parentId|leafId|entryId|firstKeptEntryId|parentSession|fromId|targetId)$/;

/** Generated ids (UUIDs, 8-hex entry ids under an id key) become `<id:N>` by first appearance; equal ids stay equal. */
export function numberIds(value) {
	const ids = new Map();
	const idFor = (raw) => {
		if (!ids.has(raw)) ids.set(raw, `<id:${ids.size}>`);
		return ids.get(raw);
	};
	const walk = (v, key) => {
		if (Array.isArray(v)) return v.map((x) => walk(x, key));
		if (v !== null && typeof v === "object") return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, walk(x, k)]));
		if (typeof v !== "string") return v;
		if (ID_KEY.test(key) && /^[0-9a-f]{8}$/.test(v)) return idFor(v);
		return v.replace(UUID, (m) => idFor(m));
	};
	return walk(value, "");
}

/**
 * What the comparison covers. Responses and events are separate streams (their interleaving is timing). Streaming
 * updates are a third list: the TypeScript client sees the lane through replicated state, which coalesces updates,
 * so its updates must be a subsequence of the Rust ones, and an assistant message_start's partial content, usage and response id
 * are whatever the coalesced state held. Harness bookkeeping (entry `seq`, a compaction entry's native fields) is
 * dropped.
 */
export function canonical(lines, tmp) {
	const responses = [];
	const events = [];
	const updates = [];
	for (const line of scrub(lines, tmp)) {
		if (line.type === "response") responses.push(line);
		else if (line.type === "message_update") updates.push(line.assistantMessageEvent);
		else if (line.type === "message_start" && line.message.role === "assistant") {
			const { content: _content, usage: _usage, responseId: _responseId, ...message } = line.message;
			events.push({ ...line, message });
		} else if (line.type === "entry_appended") {
			const { seq: _seq, ...entry } = line.entry;
			const kept = entry.type === "compaction"
				? Object.fromEntries(Object.entries(entry).filter(([k]) => ["id", "parentId", "type", "summary", "tokensBefore", "details", "timestamp"].includes(k)))
				: entry;
			events.push({ ...line, entry: kept });
		} else events.push(line);
	}
	return { responses: numberIds(responses), events: numberIds(events), updates };
}

const MODEL = ["--model", "anthropic/claude-sonnet-4-5"];
const settled = ["response", "agent_settled"];
export const cases = [
	{
		name: "prompt-text",
		args: MODEL,
		replies: [textReply("Hi there.")],
		steps: [
			{ command: { id: "s1", type: "get_state" } },
			{ command: { id: "p1", type: "prompt", message: "hello" }, wait: settled },
			{ command: { id: "m1", type: "get_messages" } },
			{ command: { id: "t1", type: "get_last_assistant_text" } },
			{ command: { id: "st", type: "get_session_stats" } },
			{ command: { id: "s2", type: "get_state" } },
		],
	},
	{
		name: "prompt-rlm-twice",
		args: MODEL,
		replies: [toolReply("toolu_01", "x = 6 * 7\nprint('x is', x)\nx + 1"), textReply("Done: 43."), toolReply("toolu_02", "x * 2"), textReply("84")],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "compute" }, wait: settled },
			{ command: { id: "p2", type: "prompt", message: "again" }, wait: settled },
			{ command: { id: "m1", type: "get_messages" } },
			{ command: { id: "e1", type: "get_entries" } },
			{ command: { id: "tr", type: "get_tree" } },
			{ command: { id: "fm", type: "get_fork_messages" } },
			{ command: { id: "st", type: "get_session_stats" } },
		],
	},
	{
		name: "prompt-rlm-error",
		args: [...MODEL, "--no-session"],
		replies: [toolReply("toolu_03", "1/0"), textReply("It failed.")],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "fail" }, wait: settled },
			{ command: { id: "s1", type: "get_state" } },
		],
	},
	{
		name: "settings",
		args: MODEL,
		replies: [],
		steps: [
			{ command: { id: "a", type: "get_available_thinking_levels" } },
			{ command: { id: "b", type: "set_thinking_level", level: "high" } },
			{ command: { id: "c", type: "cycle_thinking_level" } },
			{ command: { id: "d", type: "set_steering_mode", mode: "one-at-a-time" } },
			{ command: { id: "e", type: "set_follow_up_mode", mode: "one-at-a-time" } },
			{ command: { id: "f", type: "set_steering_mode", mode: "sometimes" } },
			{ command: { id: "g", type: "set_auto_compaction", enabled: false } },
			{ command: { id: "h", type: "set_auto_retry", enabled: false } },
			{ command: { id: "i", type: "abort_retry" } },
			{ command: { id: "j", type: "abort" } },
			{ command: { id: "k", type: "clear_queue" } },
			{ command: { id: "l", type: "set_session_name", name: "  " } },
			{ command: { id: "m", type: "set_session_name", name: " My RPC " } },
			{ command: { id: "n", type: "get_state" } },
			{ command: { id: "o", type: "set_model", provider: "anthropic", modelId: "nope" } },
			{ command: { id: "p", type: "set_model", provider: "anthropic", modelId: "claude-opus-4-1" } },
			{ command: { id: "q", type: "get_last_assistant_text" } },
			{ command: { id: "r", type: "get_entries", since: "missing" } },
			{ command: { id: "s", type: "frobnicate" } },
			{ command: "{not json", wait: [] },
			{ command: { id: "t", type: "prompt" } },
			{ command: { id: "u", type: "steer", message: "later" } },
			{ command: { id: "v", type: "fork", entryId: "nope" } },
			{ command: { id: "w", type: "clone" } },
			{ command: { id: "x", type: "get_state" } },
		],
	},
	{
		name: "bash",
		args: MODEL,
		replies: [textReply("saw it")],
		steps: [
			{ command: { id: "b1", type: "bash", command: "echo hi; echo err >&2; exit 3" } },
			{ command: { id: "b2", type: "bash", command: "echo hidden", excludeFromContext: true } },
			{ command: { id: "ab", type: "abort_bash" } },
			{ command: { id: "m1", type: "get_messages" } },
			{ command: { id: "p1", type: "prompt", message: "what ran?" }, wait: settled },
			{ command: { id: "e1", type: "get_entries" } },
		],
	},
	{
		name: "sessions",
		args: MODEL,
		replies: [textReply("one"), textReply("two")],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "first" }, wait: settled },
			{ command: { id: "p2", type: "prompt", message: "second" }, wait: settled },
			{ command: { id: "fm", type: "get_fork_messages" } },
			{ command: { id: "s1", type: "get_state" } },
			{ command: { id: "cl", type: "clone" } },
			{ command: { id: "s2", type: "get_state" } },
			{ command: { id: "m2", type: "get_messages" } },
			{ command: { id: "ns", type: "new_session" } },
			{ command: { id: "s3", type: "get_state" } },
		],
	},
	{
		name: "names-and-queues",
		args: MODEL,
		replies: [textReply("ack")],
		steps: [
			{ command: { id: "n1", type: "set_session_name", name: "Named" } },
			{ command: { id: "e1", type: "get_entries" } },
			{ command: { id: "f1", type: "follow_up", message: "queued follow" } },
			{ command: { id: "s1", type: "get_state" } },
			{ command: { id: "c1", type: "clear_queue" } },
			{ command: { id: "s2", type: "get_state" } },
			{ command: { id: "p1", type: "prompt", message: "go" }, wait: settled },
			{ command: { id: "s3", type: "get_state" } },
			{ command: { id: "e2", type: "get_entries" } },
		],
	},
	{
		name: "steer-follow-up-while-running",
		args: MODEL,
		// The cell runs until the client has queued its steer and follow-up (`touch`), not for a fixed time: a 2 s sleep
		// could end first on a loaded machine.
		replies: [
			toolReply("toolu_10", "import os, time\nwhile not os.path.exists('queued'):\n    time.sleep(0.01)\n'slept'"),
			textReply("first done"),
			textReply("follow done"),
		],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "sleep" }, wait: ["response", "tool_execution_start"] },
			{ command: { id: "p2", type: "prompt", message: "not now" } },
			{ command: { id: "st", type: "steer", message: "steer me" } },
			{ command: { id: "fu", type: "prompt", message: "then this", streamingBehavior: "followUp" } },
			// The state is read while the cell still runs (both messages queued); only then may the cell finish.
			{ command: { id: "gs", type: "get_state" }, wait: ["response", "agent_settled"], touch: "queued", touchAfter: "response" },
			{ command: { id: "m1", type: "get_messages" } },
		],
	},
	{
		name: "abort-running-cell",
		args: MODEL,
		replies: [toolReply("toolu_11", "import time\ntime.sleep(30)"), textReply("unused")],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "hang" }, wait: ["response", "tool_execution_start"] },
			{ command: { id: "ab", type: "abort" }, wait: ["response", "agent_settled"] },
			{ command: { id: "m1", type: "get_messages" } },
			{ command: { id: "t1", type: "get_last_assistant_text" } },
		],
	},
	{
		name: "compact",
		args: MODEL,
		replies: [textReply("one"), textReply("two"), textReply("## Goal\nSummary of the chat.")],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "first" }, wait: settled },
			{ command: { id: "p2", type: "prompt", message: "second" }, wait: settled },
			{ command: { id: "c1", type: "compact", customInstructions: "Be brief." } },
			{ command: { id: "m1", type: "get_messages" } },
			{ command: { id: "st", type: "get_session_stats" } },
		],
	},
	{
		name: "models-and-commands",
		args: MODEL,
		replies: [],
		steps: [
			{ command: { id: "am", type: "get_available_models" } },
			{ command: { id: "sm", type: "set_model", provider: "anthropic", modelId: "claude-haiku-4-5" } },
			{ command: { id: "tl", type: "get_available_thinking_levels" } },
			{ command: { id: "cm", type: "cycle_model" } },
			{ command: { id: "s1", type: "get_state" } },
			{ command: { id: "gc", type: "get_commands" } },
			{ command: { id: "ex", type: "export_html" } },
		],
	},
	{
		name: "fork-and-switch",
		args: MODEL,
		replies: [textReply("one"), textReply("two")],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "first" }, wait: settled },
			{ command: { id: "p2", type: "prompt", message: "second" }, wait: settled },
			{ command: { id: "s1", type: "get_state" } },
			{ command: { id: "fm", type: "get_fork_messages" } },
			{ command: { id: "fk", type: "fork", entryId: "$fm.data.messages.1.entryId" } },
			{ command: { id: "s2", type: "get_state" } },
			{ command: { id: "m2", type: "get_messages" } },
			{ command: { id: "sw", type: "switch_session", sessionPath: "$s1.data.sessionId" } },
			{ command: { id: "s3", type: "get_state" } },
			{ command: { id: "m3", type: "get_messages" } },
			{ command: { id: "bad", type: "switch_session", sessionPath: "/nonexistent/x.jsonl" } },
		],
	},
];

if (import.meta.url === `file://${process.argv[1]}`) {
	const out = [];
	for (const c of cases) {
		const r = await run(c);
		out.push({
			name: c.name,
			args: c.args,
			replies: c.replies,
			steps: c.steps,
			code: r.code,
			stderr: r.stderr,
			...canonical(r.lines, r.tmp),
			requests: scrub(r.requests, r.tmp),
			sessions: scrub(r.sessions, r.tmp),
		});
	}
	writeFileSync(new URL("./rpc_mode.json", import.meta.url), `${JSON.stringify(out, null, 1)}\n`);
}
