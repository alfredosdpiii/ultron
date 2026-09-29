#!/usr/bin/env node
/**
 * A stand-in for the Claude Code CLI in headless stream-json mode with an MCP server, for `ultron --claude` tests
 * (ultron-claude-root.test.ts). It never touches credentials or the network. It records every invocation in
 * $FAKE_CLAUDE_LOG/calls.jsonl, keeps "sessions" in $FAKE_CLAUDE_LOG/sessions (so `--resume` of an unknown id fails
 * like the real CLI), starts the MCP server named in --mcp-config, and answers each user message by what it says:
 *
 * - `CELL: <code>`: calls the `rlm` MCP tool with the code (as `mcp__ultron__rlm`, with `_meta` carrying the
 *   tool-use id), then answers "Cell said: <result>" (with "Also: <text>" for messages queued meanwhile);
 * - `USAGE_LIMIT`: fails the turn like an exhausted subscription window;
 * - `SLOW`: streams a little text and then waits (until killed);
 * - anything else: "Echo: <message>".
 */
import { spawn } from "node:child_process";
import { appendFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const env = process.env;
const argv = process.argv.slice(2);
const logDir = env.FAKE_CLAUDE_LOG;
const FLAGS = [
	"--print",
	"--output-format",
	"--input-format",
	"--verbose",
	"--include-partial-messages",
	"--tools",
	"--strict-mcp-config",
	"--mcp-config",
	"--setting-sources",
	"--permission-prompts",
	"--disable-slash-commands",
	"--no-session-persistence",
	"--model",
	"--system-prompt",
	"--system-prompt-file",
	"--settings",
	"--json-schema",
	"--effort",
	"--session-id",
	"--resume",
	"--allowedTools",
];

function log(record) {
	if (!logDir) return;
	mkdirSync(logDir, { recursive: true });
	appendFileSync(join(logDir, "calls.jsonl"), `${JSON.stringify({ pid: process.pid, at: Date.now(), ...record })}\n`);
}
const out = (event) => process.stdout.write(`${JSON.stringify(event)}\n`);
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

if (argv[0] === "--version") {
	console.log("9.9.9 (Claude Code)");
	process.exit(0);
}
if (argv[0] === "--help") {
	console.log(`Usage: claude [options]\n\nOptions:\n${FLAGS.map((flag) => `  ${flag} <value>  described`).join("\n")}`);
	process.exit(0);
}
if (argv[0] === "auth") {
	const loggedIn = env.FAKE_CLAUDE_AUTH !== "logged_out";
	console.log(JSON.stringify({ loggedIn, authMethod: loggedIn ? "claude.ai" : "none", apiProvider: "firstParty" }));
	process.exit(loggedIn ? 0 : 1);
}

const option = (name) => {
	for (let index = 0; index < argv.length; index += 1) {
		const arg = argv[index];
		if (arg.startsWith(`${name}=`)) return arg.slice(name.length + 1);
		if (arg === name) return argv[index + 1];
	}
	return undefined;
};

const sessionsDir = join(logDir ?? ".", "sessions");
mkdirSync(sessionsDir, { recursive: true });
const resumeId = option("--resume");
const sessionId = resumeId ?? option("--session-id") ?? "no-session";
const sessionPath = join(sessionsDir, `${sessionId}.json`);
if (resumeId !== undefined && !existsSync(sessionPath)) {
	process.stderr.write(`No conversation found with session ID: ${resumeId}\n`);
	log({ phase: "resume-failed", sessionId });
	process.exit(1);
}
const session = existsSync(sessionPath) ? JSON.parse(readFileSync(sessionPath, "utf8")) : { id: sessionId, prompts: [] };
const saveSession = () => writeFileSync(sessionPath, JSON.stringify(session));
const systemPromptFile = option("--system-prompt-file");
log({
	phase: "spawn",
	argv,
	cwd: process.cwd(),
	pgid: process.pid,
	sessionId,
	resumed: resumeId !== undefined,
	systemPrompt: systemPromptFile ? readFileSync(systemPromptFile, "utf8") : null,
	env: {
		CLAUDECODE: env.CLAUDECODE ?? null,
		CLAUDE_CODE_SESSION_ID: env.CLAUDE_CODE_SESSION_ID ?? null,
		MCP_TOOL_TIMEOUT: env.MCP_TOOL_TIMEOUT ?? null,
	},
});

// The MCP server (Ultron's bridge).
const mcpConfig = JSON.parse(option("--mcp-config") ?? '{"mcpServers":{}}');
const server = mcpConfig.mcpServers?.ultron;
let mcp;
let tools = [];
const pendingRpc = new Map();
let rpcId = 0;
function rpc(method, params) {
	const id = ++rpcId;
	mcp.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
	return new Promise((resolve) => pendingRpc.set(id, resolve));
}
if (server) {
	mcp = spawn(server.command, server.args ?? [], { stdio: ["pipe", "pipe", "inherit"], env: { ...env, ...(server.env ?? {}) } });
	let buffer = "";
	mcp.stdout.setEncoding("utf8");
	mcp.stdout.on("data", (chunk) => {
		buffer += chunk;
		let index = buffer.indexOf("\n");
		while (index >= 0) {
			const line = buffer.slice(0, index);
			buffer = buffer.slice(index + 1);
			index = buffer.indexOf("\n");
			if (!line.trim()) continue;
			const message = JSON.parse(line);
			pendingRpc.get(message.id)?.(message);
			pendingRpc.delete(message.id);
		}
	});
	await rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "fake-claude" } });
	mcp.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`);
	tools = (await rpc("tools/list", {})).result?.tools ?? [];
	log({ phase: "mcp-ready", tools: tools.map((tool) => tool.name) });
}

out({
	type: "system",
	subtype: "init",
	session_id: sessionId,
	cwd: process.cwd(),
	tools: tools.map((tool) => `mcp__ultron__${tool.name}`),
	mcp_servers: server ? [{ name: "ultron", status: "connected" }] : [],
	model: option("--model"),
	apiKeySource: "none",
});

// Stdin: user messages, one per line. Messages that arrive during a turn wait in `queued`.
const queued = [];
let wake;
let stdinEnded = false;
let inBuffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
	inBuffer += chunk;
	let index = inBuffer.indexOf("\n");
	while (index >= 0) {
		const line = inBuffer.slice(0, index);
		inBuffer = inBuffer.slice(index + 1);
		index = inBuffer.indexOf("\n");
		if (!line.trim()) continue;
		queued.push(JSON.parse(line));
		wake?.();
	}
});
process.stdin.on("end", () => {
	stdinEnded = true;
	wake?.();
});
const nextMessage = async () => {
	while (queued.length === 0) {
		if (stdinEnded) return undefined;
		await new Promise((resolve) => {
			wake = resolve;
		});
	}
	return queued.shift();
};
const textOf = (message) =>
	(Array.isArray(message.message.content) ? message.message.content : [{ type: "text", text: message.message.content }])
		.filter((part) => part.type === "text")
		.map((part) => part.text)
		.join("");

let messages = 0;
let cost = 0;
const usage = (output) => ({
	input_tokens: 1000,
	output_tokens: output,
	cache_read_input_tokens: 500,
	cache_creation_input_tokens: 100,
});
const rateLimit = (status = "allowed") =>
	out({
		type: "rate_limit_event",
		rate_limit_info: {
			status,
			rateLimitType: "five_hour",
			resetsAt: 1_900_000_000,
			unifiedWindows: {
				five_hour: { utilization: 0.25, resetsAt: 1_900_000_000 },
				seven_day: { utilization: 0.5, resetsAt: 1_900_500_000 },
			},
		},
	});

/** Stream one API message: thinking (redacted, as the real CLI streams it), optional text, optional tool use. */
async function streamMessage({ text, toolUse, stop }) {
	messages += 1;
	const id = `msg_${process.pid}_${messages}`;
	const stream = (event) => out({ type: "stream_event", event });
	stream({ type: "message_start", message: { id, model: "claude-fake-1", usage: usage(1) } });
	let index = 0;
	stream({ type: "content_block_start", index, content_block: { type: "thinking", thinking: "", signature: "" } });
	stream({ type: "content_block_delta", index, delta: { type: "thinking_delta", thinking: "" } });
	stream({ type: "content_block_delta", index, delta: { type: "signature_delta", signature: "sig" } });
	stream({ type: "content_block_stop", index });
	const content = [];
	if (text) {
		index += 1;
		stream({ type: "content_block_start", index, content_block: { type: "text", text: "" } });
		for (const piece of text.match(/.{1,12}/gs) ?? []) {
			stream({ type: "content_block_delta", index, delta: { type: "text_delta", text: piece } });
			await sleep(2);
		}
		stream({ type: "content_block_stop", index });
		content.push({ type: "text", text });
	}
	if (toolUse) {
		index += 1;
		stream({
			type: "content_block_start",
			index,
			content_block: { type: "tool_use", id: toolUse.id, name: "mcp__ultron__rlm", input: {} },
		});
		const json = JSON.stringify(toolUse.input);
		for (const piece of json.match(/.{1,10}/gs) ?? [])
			stream({ type: "content_block_delta", index, delta: { type: "input_json_delta", partial_json: piece } });
		stream({ type: "content_block_stop", index });
		content.push({ type: "tool_use", id: toolUse.id, name: "mcp__ultron__rlm", input: toolUse.input });
	}
	stream({ type: "message_delta", delta: { stop_reason: stop }, usage: usage(40) });
	stream({ type: "message_stop" });
	out({ type: "assistant", message: { id, model: "claude-fake-1", content, stop_reason: null } });
	return id;
}

function result(text, extra = {}) {
	cost += 0.01;
	out({
		type: "result",
		subtype: "success",
		is_error: false,
		result: text,
		stop_reason: "end_turn",
		session_id: sessionId,
		total_cost_usd: Number(cost.toFixed(4)),
		usage: usage(40),
		...extra,
	});
}

for (;;) {
	const message = await nextMessage();
	if (!message) break;
	const prompt = textOf(message);
	// Directives count only in the new message, not in an earlier conversation rendered into it.
	const directive = prompt.replace(/<conversation_so_far>[\s\S]*<\/conversation_so_far>/, "");
	session.prompts.push(prompt);
	saveSession();
	log({ phase: "prompt", prompt, content: message.message.content });
	rateLimit();
	if (/USAGE_LIMIT/.test(directive)) {
		rateLimit("rejected");
		out({
			type: "result",
			subtype: "error_during_execution",
			is_error: true,
			result: "Claude AI usage limit reached|1900000000",
			session_id: sessionId,
			total_cost_usd: cost,
			usage: usage(0),
		});
		continue;
	}
	if (/^SLOW/m.test(directive)) {
		await streamMessage({ text: "working on it", stop: "end_turn" }).catch(() => {});
		log({ phase: "slow" });
		await sleep(600_000);
		continue;
	}
	const cellMatch = /CELL: (.+)$/m.exec(directive);
	if (cellMatch && mcp) {
		const toolUseId = `toolu_${process.pid}_${messages + 1}`;
		const code = cellMatch[1].replace(/\\n/g, "\n");
		await streamMessage({ text: "Running a cell.", toolUse: { id: toolUseId, input: { code } }, stop: "tool_use" });
		const response = await rpc("tools/call", {
			name: "rlm",
			arguments: { code },
			_meta: { "claudecode/toolUseId": toolUseId, progressToken: 1 },
		});
		const resultText = (response.result?.content ?? []).map((part) => part.text ?? "").join("");
		log({ phase: "tool-result", toolUseId, result: response.result });
		out({
			type: "user",
			message: { role: "user", content: [{ type: "tool_result", tool_use_id: toolUseId, content: response.result?.content ?? [] }] },
		});
		// Messages written while the tool ran are handed to the model with the result (like Claude Code's queue).
		await sleep(50);
		const also = queued.splice(0).map(textOf);
		if (also.length > 0) log({ phase: "steered", also });
		const answer = `Cell said: ${resultText.trim().slice(0, 200)}${also.length ? ` Also: ${also.join(" | ")}` : ""}`;
		await streamMessage({ text: answer, stop: "end_turn" });
		result(answer);
		continue;
	}
	const answer = `Echo: ${prompt.slice(0, 300)}`;
	await streamMessage({ text: answer, stop: "end_turn" });
	result(answer);
}
mcp?.stdin.end();
mcp?.kill();
log({ phase: "exit" });
process.exit(0);
