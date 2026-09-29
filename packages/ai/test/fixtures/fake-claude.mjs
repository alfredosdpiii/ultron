#!/usr/bin/env node
/**
 * A stand-in for the Claude Code CLI used by claude-code-cli.test.ts. It records every invocation (argv, cwd,
 * selected environment, the stream-json prompt) as JSON lines in $FAKE_CLAUDE_LOG/calls.jsonl and answers with
 * canned stream-json chosen by $FAKE_CLAUDE_SCENARIO. It never touches real credentials or the network.
 */
import { appendFileSync, mkdirSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const env = process.env;
const argv = process.argv.slice(2);
const logDir = env.FAKE_CLAUDE_LOG;
const scenario = env.FAKE_CLAUDE_SCENARIO ?? "text";

const KNOWN_FLAGS = [
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
	"--append-system-prompt",
	"--settings",
	"--json-schema",
	"--effort",
	"--help",
	"--version",
];
const omitted = new Set((env.FAKE_CLAUDE_HELP_OMIT ?? "").split(",").filter(Boolean));
const flags = KNOWN_FLAGS.filter((flag) => !omitted.has(flag));

function log(record) {
	if (!logDir) return;
	mkdirSync(logDir, { recursive: true });
	appendFileSync(join(logDir, "calls.jsonl"), `${JSON.stringify({ pid: process.pid, at: Date.now(), ...record })}\n`);
}

function out(event) {
	process.stdout.write(`${JSON.stringify(event)}\n`);
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

if (argv[0] === "--version") {
	console.log("9.9.9 (Claude Code)");
	process.exit(0);
}
if (argv[0] === "--help") {
	console.log(`Usage: claude [options] [command] [prompt]\n\nOptions:\n${flags.map((flag) => `  ${flag} <value>   described`).join("\n")}`);
	process.exit(0);
}
if (argv[0] === "auth" && argv[1] === "status") {
	log({ phase: "auth", argv });
	const auth = env.FAKE_CLAUDE_AUTH ?? "ok";
	const status = {
		loggedIn: auth !== "logged_out",
		authMethod: auth === "logged_out" ? "none" : auth === "api_key" ? "api_key" : "claude.ai",
		apiProvider: auth === "third_party" ? "bedrock" : "firstParty",
		...(auth === "ok" ? { subscriptionType: "max" } : {}),
	};
	console.log(JSON.stringify(status, null, 2));
	process.exit(status.loggedIn ? 0 : 1);
}

for (const arg of argv) {
	if (!arg.startsWith("--")) continue;
	const flag = arg.split("=")[0];
	if (!flags.includes(flag)) {
		process.stderr.write(`error: unknown option '${flag}'\n`);
		process.exit(1);
	}
}

const option = (name) => {
	const found = argv.find((arg) => arg.startsWith(`${name}=`));
	return found === undefined ? undefined : found.slice(name.length + 1);
};

log({
	phase: "spawn",
	argv,
	cwd: process.cwd(),
	env: {
		CLAUDECODE: env.CLAUDECODE ?? null,
		CLAUDE_CODE_SESSION_ID: env.CLAUDE_CODE_SESSION_ID ?? null,
		CLAUDE_CONFIG_DIR: env.CLAUDE_CONFIG_DIR ?? null,
		CLAUDE_CODE_MAX_OUTPUT_TOKENS: env.CLAUDE_CODE_MAX_OUTPUT_TOKENS ?? null,
	},
});

// Like the real CLI in stream-json input mode, nothing is emitted before the first user message arrives.
let buffer = "";
const firstMessage = await new Promise((resolve) => {
	process.stdin.setEncoding("utf8");
	process.stdin.on("data", (chunk) => {
		buffer += chunk;
		const index = buffer.indexOf("\n");
		if (index >= 0) resolve(JSON.parse(buffer.slice(0, index)));
	});
	process.stdin.on("end", () => resolve(undefined));
});
if (!firstMessage) process.exit(0);
log({ phase: "prompt", message: firstMessage });

const activeDir = logDir ? join(logDir, "active") : undefined;
if (activeDir) {
	mkdirSync(activeDir, { recursive: true });
	writeFileSync(join(activeDir, String(process.pid)), "");
	appendFileSync(join(logDir, "active.log"), `${readdirSync(activeDir).length}\n`);
}
const done = () => {
	if (activeDir) rmSync(join(activeDir, String(process.pid)), { force: true });
};

const model = option("--model") ?? "default";
const schema = option("--json-schema");
const text = env.FAKE_CLAUDE_TEXT ?? "Hello from fake claude";
const id = `msg_${process.pid}`;
const usage = {
	input_tokens: 120,
	output_tokens: 7,
	cache_read_input_tokens: 30,
	cache_creation_input_tokens: 10,
	output_tokens_details: { thinking_tokens: scenario === "thinking" ? 3 : 0 },
	cache_creation: { ephemeral_1h_input_tokens: 0, ephemeral_5m_input_tokens: 10 },
};

out({
	type: "system",
	subtype: "init",
	cwd: process.cwd(),
	tools: [],
	mcp_servers: [],
	model: `resolved-${model}`,
	apiKeySource: scenario === "api_key_source" ? "ANTHROPIC_API_KEY" : "none",
});

if (env.FAKE_CLAUDE_DELAY_MS) await sleep(Number(env.FAKE_CLAUDE_DELAY_MS));
if (scenario === "slow") await sleep(60_000);
if (scenario === "crash") {
	process.stderr.write("boom: the fake CLI crashed\n");
	done();
	process.exit(3);
}

function streamText(content) {
	out({ type: "stream_event", event: { type: "message_start", message: { id, model: `resolved-${model}`, usage } } });
	let index = 0;
	if (scenario === "thinking") {
		out({ type: "stream_event", event: { type: "content_block_start", index, content_block: { type: "thinking", thinking: "" } } });
		out({ type: "stream_event", event: { type: "content_block_delta", index, delta: { type: "thinking_delta", thinking: "pondering" } } });
		out({ type: "stream_event", event: { type: "content_block_stop", index } });
		index += 1;
	}
	out({ type: "stream_event", event: { type: "content_block_start", index, content_block: { type: "text", text: "" } } });
	const third = Math.ceil(content.length / 3);
	for (const piece of [content.slice(0, third), content.slice(third, 2 * third), content.slice(2 * third)])
		if (piece) out({ type: "stream_event", event: { type: "content_block_delta", index, delta: { type: "text_delta", text: piece } } });
	out({ type: "stream_event", event: { type: "content_block_stop", index } });
	out({ type: "stream_event", event: { type: "message_delta", delta: { stop_reason: "end_turn" }, usage } });
	out({ type: "stream_event", event: { type: "message_stop" } });
	out({ type: "assistant", message: { id, model: `resolved-${model}`, role: "assistant", content: [{ type: "text", text: content }] } });
}

function result(fields) {
	out({
		type: "result",
		subtype: "success",
		is_error: false,
		stop_reason: "end_turn",
		num_turns: 1,
		usage,
		...(scenario === "no_cost" ? {} : { total_cost_usd: 0.0012 }),
		...fields,
	});
}

if (scenario === "rate_limit") {
	out({ type: "rate_limit_event", rate_limit_info: { status: "rejected", rateLimitType: "five_hour", resetsAt: 1790700600 } });
	result({ is_error: true, result: "Claude AI usage limit reached|1790700600", total_cost_usd: 0, usage: { input_tokens: 0, output_tokens: 0 } });
} else if (scenario === "rate_429") {
	result({ is_error: true, api_error_status: 429, result: "API Error: 429 Too many requests", total_cost_usd: 0 });
} else if (scenario === "max_tokens") {
	streamText("Rivers run to the");
	result({ is_error: true, stop_reason: "stop_sequence", result: "API Error: Claude's response exceeded the 20 output token maximum. To configure this behavior, set the CLAUDE_CODE_MAX_OUTPUT_TOKENS environment variable." });
} else if (schema !== undefined && scenario === "schema_reject") {
	result({ is_error: true, api_error_status: 400, result: "API Error: 400 tools.0.custom.input_schema.type: Input should be 'object'", total_cost_usd: 0 });
} else if (schema !== undefined) {
	streamText("I'll provide the structured output.");
	const structured = env.FAKE_CLAUDE_STRUCTURED ? JSON.parse(env.FAKE_CLAUDE_STRUCTURED) : { ok: true };
	result({ num_turns: 2, result: JSON.stringify(structured), structured_output: structured });
} else if (scenario === "no_partial") {
	out({ type: "assistant", message: { id, model: `resolved-${model}`, role: "assistant", content: [{ type: "text", text }] } });
	result({ result: text });
} else {
	streamText(text);
	out({ type: "rate_limit_event", rate_limit_info: { status: scenario === "near_limit" ? "allowed_warning" : "allowed", rateLimitType: "five_hour", resetsAt: 1790700600 } });
	result({ result: text });
}
done();

// The real CLI waits for more stream-json input; it exits once stdin closes.
await new Promise((resolve) => {
	process.stdin.on("end", resolve);
	if (process.stdin.readableEnded) resolve();
});
process.exit(0);
