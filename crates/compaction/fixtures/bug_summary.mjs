// Records `generateBugReportSummary` (packages/coding-agent/src/core/bug-report.ts): the summary request it makes
// (captured through `streamFn`, retries off) for scripted conversations, hints, models and thinking levels, and
// what it makes of scripted responses (the report text, or the error it throws).
// crates/compaction/tests/bug_summary.rs replays every case. Usage: node crates/compaction/fixtures/bug_summary.mjs
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const { generateBugReportSummary } = await import(
	pathToFileURL(join(repo, "packages/coding-agent/src/core/bug-report.ts")).href
);

const NOW = 1_760_000_000_000;
Date.now = () => NOW;
const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

const usage = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };
const text = (t) => ({ type: "text", text: t });
const user = (t) => ({ role: "user", content: t, timestamp: 1 });
const assistant = (content, extra = {}) => ({
	role: "assistant", content, api: "anthropic-messages", provider: "anthropic", model: "m1", usage, stopReason: "stop", timestamp: 2, ...extra,
});
const toolResult = (id, t) => ({ role: "toolResult", toolCallId: id, toolName: "rlm", content: [text(t)], isError: true, timestamp: 3 });
const bash = (command, output) => ({ role: "bashExecution", command, output, exitCode: 1, cancelled: false, truncated: false, timestamp: 4 });
const long = (word, n) => Array.from({ length: n }, (_, i) => `${word}${i}`).join(" ");

const conversation = [
	user("run the tests"),
	assistant([text("Running them."), { type: "toolCall", id: "t1", name: "rlm", arguments: { code: "print(1/0)" } }]),
	toolResult("t1", "ZeroDivisionError: division by zero"),
	bash("npm test", "1 failing"),
	assistant([text("The cell crashed.")]),
];
const big = [user(long("early", 400)), assistant([text(long("middle", 400))]), user("now it hangs")];

const model = (over = {}) => ({
	id: "sum-1", name: "Summarizer", api: "anthropic-messages", provider: "anthropic", baseUrl: "https://example.invalid",
	reasoning: true, input: ["text"], cost: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 }, contextWindow: 100000, maxTokens: 8000, ...over,
});

const ok = assistant([text("  ## What the user was doing\nTesting.  ")]);
const inputs = [
	{ name: "hint-and-thinking", messages: conversation, hint: "  the cell crashed  ", thinkingLevel: "high", sessionId: "sess-1", response: ok },
	{ name: "no-hint", messages: conversation, thinkingLevel: "off", response: ok },
	{ name: "blank-hint-no-reasoning", messages: conversation, hint: "   ", thinkingLevel: "low", model: { reasoning: false, maxTokens: 0 }, response: ok },
	{ name: "small-max-tokens", messages: conversation, model: { maxTokens: 1000 }, response: ok },
	{ name: "trimmed-to-window", messages: big, model: { contextWindow: 1000 }, response: ok },
	{ name: "budget-edge", messages: big, model: { contextWindow: 1700 }, response: ok },
	{ name: "newest-only", messages: big, model: { contextWindow: 10 }, response: ok },
	{ name: "zero-window", messages: big, model: { contextWindow: 0 }, response: ok },
	{ name: "aborted", messages: conversation, response: assistant([], { stopReason: "aborted" }) },
	{ name: "error", messages: conversation, response: assistant([], { stopReason: "error", errorMessage: "overloaded" }) },
	{ name: "error-empty", messages: conversation, response: assistant([], { stopReason: "error" }) },
	{ name: "length", messages: conversation, response: assistant([text("partial")], { stopReason: "length" }) },
	{ name: "tool-call", messages: conversation, response: assistant([text("x"), { type: "toolCall", id: "c", name: "read", arguments: {} }]) },
	{ name: "empty", messages: conversation, response: assistant([text("   "), { type: "thinking", thinking: "hm" }]) },
	{ name: "two-texts", messages: conversation, response: assistant([text("first"), text("second")]) },
];

const cases = [];
for (const input of inputs) {
	const requests = [];
	const streamFn = async (m, context, options) => {
		const { signal: _signal, ...rest } = options;
		const sessionId = UUID.test(rest.sessionId ?? "") ? "<fresh>" : rest.sessionId;
		requests.push({ model: m.id, context: clone(context), options: clone({ ...rest, sessionId }) });
		return { result: async () => input.response };
	};
	let outcome;
	try {
		outcome = {
			ok: await generateBugReportSummary({
				messages: input.messages, hint: input.hint, model: model(input.model), apiKey: "key",
				signal: new AbortController().signal, thinkingLevel: input.thinkingLevel, streamFn, sessionId: input.sessionId,
			}),
		};
	} catch (error) {
		outcome = { error: error.message };
	}
	cases.push({ ...input, model: model(input.model), requests, outcome });
}
writeFileSync(join(here, "bug_summary.json"), `${JSON.stringify({ now: NOW, cases }, null, 1)}\n`);
