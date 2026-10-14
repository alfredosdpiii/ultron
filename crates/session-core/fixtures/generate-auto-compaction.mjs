// Generates crates/session-core/fixtures/auto_compaction.json from the TypeScript sources automatic compaction runs
// on: pi-ai `isContextOverflow` / `isRecoverableLength` (packages/ai/src/utils/overflow.ts) over each provider's
// overflow error text, silent and length-stop overflows and the errors that are not overflows; `shouldCompact`
// (packages/agent/src/harness/compaction/compaction.ts) at and around the threshold; and the settings manager's
// `getCompactionSettings(model)` (per-model overrides, defaults, invalid values). The Rust test
// (crates/session-core/tests/auto_compaction.rs) replays every case and compares exactly.
// Usage: node crates/session-core/fixtures/generate-auto-compaction.mjs   (from the repository root; deterministic)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { isContextOverflow, isRecoverableLength } from "../../../packages/ai/src/utils/overflow.ts";
import { shouldCompact } from "../../../packages/agent/src/harness/compaction/compaction.ts";
import { SettingsManager } from "../../../packages/coding-agent/src/core/settings-manager.ts";

const here = dirname(fileURLToPath(import.meta.url));

const usage = (input, output, cacheRead = 0) => ({
	input, output, cacheRead, cacheWrite: 0, totalTokens: input + output + cacheRead,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
});
const message = (stopReason, errorMessage, provider = "anthropic", u = usage(0, 0)) => ({
	role: "assistant", content: [], api: "anthropic-messages", provider, model: "m", usage: u, stopReason,
	...(errorMessage === undefined ? {} : { errorMessage }), timestamp: 1,
});

// Each provider's overflow text (overflow.ts), and errors that only look like one.
const ERRORS = [
	["anthropic", "prompt is too long: 213462 tokens > 200000 maximum"],
	["anthropic", '400 {"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 1213462 tokens > 1000000 maximum"}}'],
	["anthropic", '413 {"error":{"type":"request_too_large","message":"Request exceeds the maximum size"}}'],
	["amazon-bedrock", "Input is too long for requested model."],
	["openai", "Your input exceeds the context window of this model"],
	["openai", "Requested token count exceeds the model's maximum context length of 131072 tokens"],
	["openai", "Input length (265330) exceeds model's maximum context length (262144)."],
	["google", "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)"],
	["xai", "This model's maximum prompt length is 131072 but the request contains 537812 tokens"],
	["groq", "Please reduce the length of the messages or completion"],
	["openrouter", "This endpoint's maximum context length is 200000 tokens. However, you requested about 250000 tokens"],
	["openrouter", "Input length 300000 exceeds the maximum allowed input length of 262144 tokens."],
	["together", "The input (300000 tokens) is longer than the model's context length (131072 tokens)."],
	["llama.cpp", "the request exceeds the available context size, try increasing it"],
	["lmstudio", "tokens to keep from the initial prompt is greater than the context length"],
	["github-copilot", "prompt token count of 200000 exceeds the limit of 128000"],
	["minimax", "invalid params, context window exceeds limit"],
	["kimi-coding", "Your request exceeded model token limit: 262144 (requested: 300000)"],
	["ds4", "Prompt has 300,000 tokens, but the configured context size is 262,144 tokens"],
	["mistral", "Prompt contains 300000 tokens and 0 draft tokens, too large for model with 262144 maximum context length"],
	["zai", '{"code":"1261","message":"Prompt too long"}'],
	["zai", "model_context_window_exceeded"],
	["ollama", "prompt too long; exceeded max context length by 1200 tokens"],
	["dashscope", "Range of input length should be [1, 98304]"],
	["generic", "context_length_exceeded"],
	["generic", "too many tokens in the request"],
	["generic", "token limit exceeded"],
	["cerebras", "400 status code (no body)"],
	["cerebras", "413 (no body)"],
	["openai", "400 status code (no body)"],
	["amazon-bedrock", "ThrottlingException: Too many tokens, please wait before trying again."],
	["amazon-bedrock", "Throttling error: Too many tokens, please wait before trying again."],
	["amazon-bedrock", "Service unavailable: prompt is too long"],
	["openai", "Rate limit reached: too many tokens per minute"],
	["openai", "429 Too Many Requests: context_length_exceeded"],
	["anthropic", "overloaded_error: Overloaded"],
	["anthropic", "Connection error."],
	["anthropic", ""],
];

const overflow = [];
for (const [provider, text] of ERRORS) {
	const m = message("error", text, provider);
	overflow.push({ message: m, contextWindow: 200000, overflow: isContextOverflow(m, 200000) });
}
// An overflow pattern only counts on an error.
overflow.push({ message: message("stop", "prompt is too long"), contextWindow: 200000, overflow: isContextOverflow(message("stop", "prompt is too long"), 200000) });
// Silent overflow (input + cache read over the window on a successful response) and the length stop that filled it.
for (const [stopReason, u, window] of [
	["stop", usage(200001, 5), 200000],
	["stop", usage(200000, 5), 200000],
	["stop", usage(150000, 5, 50001), 200000],
	["stop", usage(200001, 5), 0],
	["length", usage(198000, 0), 200000],
	["length", usage(197999, 0), 200000],
	["length", usage(199000, 1), 200000],
	["length", usage(100000, 0, 98000), 200000],
	["toolUse", usage(300000, 5), 200000],
	["error", usage(300000, 5), 200000],
]) {
	const m = message(stopReason, undefined, "anthropic", u);
	overflow.push({ message: m, contextWindow: window, overflow: isContextOverflow(m, window || undefined) });
}

const recoverable = [];
for (const [stopReason, output, desired] of [
	["length", 100, 64000],
	["length", 64000, 64000],
	["length", 0, 0],
	["length", 63999, 64000],
	["stop", 100, 64000],
	["error", 100, 64000],
]) {
	const m = message(stopReason, undefined, "anthropic", usage(10, output));
	recoverable.push({ message: m, desiredMaxOutput: desired, recoverable: isRecoverableLength(m, desired) });
}

const threshold = [];
for (const [tokens, window, enabled, reserve] of [
	[983616, 1000000, true, 16384],
	[983617, 1000000, true, 16384],
	[990005, 1000000, true, 16384],
	[990005, 1000000, false, 16384],
	[3616, 20000, true, 16384],
	[3617, 20000, true, 16384],
	[10, 8000, true, 16384],
	[0, 0, true, 16384],
	[1, 0, true, 0],
	[100, 100, true, 0],
	[101, 100, true, 0],
]) {
	const settings = { enabled, reserveTokens: reserve, keepRecentTokens: 20000 };
	threshold.push({ tokens, contextWindow: window, settings, compact: shouldCompact(tokens, window, settings) });
}

const MODEL = { provider: "anthropic", id: "claude-sonnet-4-5" };
const settings = [];
for (const configured of [
	{},
	{ compaction: { enabled: false } },
	{ compaction: { reserveTokens: 8000, keepRecentTokens: 1000 } },
	{ compaction: { reserveTokens: 0 } },
	{ compaction: { reserveTokens: 8000, modelOverrides: { "anthropic/claude-sonnet-4-5": { reserveTokens: 30000 } } } },
	{ compaction: { modelOverrides: { "anthropic/claude-sonnet-4-5": { keepRecentTokens: 5000 } } } },
	{ compaction: { modelOverrides: { "openai/gpt-5": { reserveTokens: 30000 } } } },
	{ compaction: { reserveTokens: -1 } },
	{ compaction: { reserveTokens: 1.5 } },
	{ compaction: { keepRecentTokens: "100" } },
	{ compaction: { modelOverrides: { "anthropic/claude-sonnet-4-5": 5 } } },
	{ compaction: { modelOverrides: { "anthropic/claude-sonnet-4-5": { reserveTokens: -5 } } } },
]) {
	let result;
	try {
		result = { settings: SettingsManager.inMemory(configured).getCompactionSettings(MODEL) };
	} catch (error) {
		result = { error: error.message };
	}
	settings.push({ configured, model: MODEL, ...result });
}

const out = { overflow, recoverable, threshold, settings };
writeFileSync(join(here, "auto_compaction.json"), `${JSON.stringify(out, null, 1)}\n`);
