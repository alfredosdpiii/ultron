// Generates crates/ai/fixtures/providers.json from this worktree's TypeScript packages/ai providers.
// Each scenario feeds canned HTTP/SSE responses to the TS provider through a stubbed fetch and records the request
// (URL, method, headers minus SDK telemetry, parsed JSON body), every pushed stream event (cloned at push time), and the
// final message. The Rust tests replay the same responses through the Rust providers and compare exactly.
// Usage: node crates/ai/fixtures/generate.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const src = (path) => import(pathToFileURL(join(root, "packages/ai/src", path)).href);

// The SDKs and providers read these from the process environment; the fixture must not depend on the machine.
for (const name of Object.keys(process.env)) {
	if (/^(PI_|OPENAI_|ANTHROPIC_)/.test(name)) delete process.env[name];
}
const NOW = 1767225600000;
Date.now = () => NOW;

const anthropic = await src("api/anthropic-messages.ts");
const completions = await src("api/openai-completions.ts");
const responses = await src("api/openai-responses.ts");
const { AssistantMessageEventStream } = await src("utils/event-stream.ts");
const { getPiUserAgent } = await src("utils/pi-user-agent.ts");
const { MODELS } = await src("models.generated.ts");

const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

// Record every event at push time: `partial` is a live object that later events mutate.
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

const UA = getPiUserAgent();
function requestHeaders(init) {
	const out = {};
	new Headers(init.headers).forEach((value, key) => {
		if (key.startsWith("x-stainless-")) return;
		out[key] = key === "user-agent" && value === UA ? "<pi-user-agent>" : value;
	});
	return Object.fromEntries(Object.entries(out).sort(([a], [b]) => (a < b ? -1 : 1)));
}

function abortError() {
	return new DOMException("This operation was aborted", "AbortError");
}

function stubFetch(queue, requests) {
	return async (url, init) => {
		requests.push({
			url: String(url),
			method: init.method,
			headers: requestHeaders(init),
			body: init.body ? JSON.parse(init.body) : null,
		});
		const canned = queue.shift();
		if (!canned) throw new Error("unexpected request");
		const encoder = new TextEncoder();
		const chunks = canned.chunks.map((c) => (typeof c === "string" ? encoder.encode(c) : new Uint8Array(c)));
		const signal = init.signal;
		const body = new ReadableStream({
			start(controller) {
				signal?.addEventListener("abort", () => {
					try {
						controller.error(abortError());
					} catch {}
				});
			},
			pull(controller) {
				if (signal?.aborted) return controller.error(abortError());
				const next = chunks.shift();
				if (next === undefined) controller.close();
				else controller.enqueue(next);
			},
		});
		return new Response(body, { status: canned.status ?? 200, headers: canned.headers ?? { "content-type": "text/event-stream" } });
	};
}

const API = {
	"anthropic-messages": anthropic,
	"openai-completions": completions,
	"openai-responses": responses,
};

async function run(scenario) {
	const queue = scenario.responses.map((r) => ({ ...r, chunks: [...r.chunks] }));
	const requests = [];
	const controller = new AbortController();
	const options = {
		...(scenario.options.env ? {} : { env: { PI_CACHE_RETENTION: "unset" } }),
		...scenario.options,
	};
	const record = {
		name: scenario.name,
		fn: scenario.fn ?? "stream",
		model: scenario.model,
		context: scenario.context,
		options: clone(options),
		responses: scenario.responses,
		abortOn: scenario.abortOn ?? null,
		payloadPatch: scenario.payloadPatch ?? null,
	};
	recording = { events: [], abortOn: scenario.abortOn, seen: 0, controller };
	const module = API[scenario.model.api];
	try {
		const s = module[record.fn](scenario.model, scenario.context, {
			...options,
			fetch: stubFetch(queue, requests),
			signal: controller.signal,
			...(scenario.payloadPatch ? { onPayload: (p) => ({ ...p, ...scenario.payloadPatch }) } : {}),
		});
		record.result = clone(await s.result());
		record.thrown = null;
	} catch (e) {
		record.result = null;
		record.thrown = e.message;
	}
	record.events = recording.events;
	record.requests = requests;
	recording = null;
	return record;
}

// ---------------------------------------------------------------------------------------------- SSE helpers

const sse = (events) => events.map(([event, data]) => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
const data = (items) => items.map((d) => `data: ${typeof d === "string" ? d : JSON.stringify(d)}\n\n`);
const ok = (chunks) => ({ chunks });
const json = (status, body, headers = {}) => ({
	status,
	headers: { "content-type": "application/json", ...headers },
	chunks: [typeof body === "string" ? body : JSON.stringify(body)],
});

// ---------------------------------------------------------------------------------------------- shared inputs

const tools = [
	{
		name: "read",
		description: "Read a file",
		parameters: {
			type: "object",
			properties: { path: { type: "string" }, limit: { type: "number" } },
			required: ["path"],
		},
	},
	{
		name: "bash",
		description: "Run a command",
		parameters: { type: "object", properties: { command: { type: "string" } }, required: ["command"] },
		constrainedSampling: { type: "json_schema", strict: "prefer" },
	},
];
const grammarTool = {
	name: "apply_patch",
	description: "Apply a patch",
	parameters: { type: "object", properties: { patch: { type: "string" } }, required: ["patch"] },
	constrainedSampling: { type: "grammar", variants: { openai_lark: "start: /.+/s" } },
};
const IMG = { type: "image", data: "aGVsbG8=", mimeType: "image/png" };
const usage0 = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };

const system = (content, extra = {}) => ({ role: "system", content, timestamp: 1, ...extra });
const user = (content, timestamp = 2) => ({ role: "user", content, timestamp });
const assistant = (model, content, extra = {}) => ({
	role: "assistant",
	content,
	api: model.api,
	provider: model.provider,
	model: model.id,
	usage: usage0,
	stopReason: "stop",
	timestamp: 3,
	...extra,
});
const toolResult = (toolCallId, toolName, content, isError = false) => ({
	role: "toolResult",
	toolCallId,
	toolName,
	content,
	isError,
	timestamp: 4,
});

function model(api, provider, baseUrl, extra = {}) {
	return {
		id: extra.id ?? "test-model",
		name: "Test Model",
		api,
		provider,
		baseUrl,
		reasoning: true,
		input: ["text", "image"],
		cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 },
		contextWindow: 200000,
		maxTokens: 32000,
		...extra,
	};
}

/** A transcript with prior turns of the same model, a foreign model, an orphaned tool call, and images. */
function history(m, foreign) {
	return [
		system("You are helpful.", { toolsAdded: tools, sections: { rules: "<rules>be brief</rules>" } }),
		user("List files"),
		assistant(m, [
			{ type: "thinking", thinking: "I should read.", thinkingSignature: m.api === "openai-responses" ? JSON.stringify({ type: "reasoning", id: "rs_1", summary: [{ type: "summary_text", text: "I should read." }], encrypted_content: "enc1" }) : "sig-1" },
			{ type: "text", text: "Reading.", ...(m.api === "openai-responses" ? { textSignature: JSON.stringify({ v: 1, id: "msg_1", phase: "commentary" }) } : {}) },
			{ type: "toolCall", id: m.api === "openai-responses" ? "call_1|fc_1" : "call_1", name: "read", arguments: { path: "a.txt" } },
		], { stopReason: "toolUse" }),
		toolResult(m.api === "openai-responses" ? "call_1|fc_1" : "call_1", "read", [{ type: "text", text: "contents of a" }, IMG]),
		user([{ type: "text", text: "Now look at this" }, IMG, { type: "text", text: "" }]),
		assistant(foreign, [
			{ type: "thinking", thinking: "Foreign thoughts", thinkingSignature: "x" },
			{ type: "text", text: "Foreign answer" },
			{ type: "toolCall", id: "call|weird+id/with=chars", name: "bash", arguments: { command: "ls" } },
		], { stopReason: "toolUse" }),
		user("Continue"),
		assistant(m, [{ type: "text", text: "partial" }], { stopReason: "error", errorMessage: "boom" }),
		user("Again"),
	];
}

const foreignModel = { api: "google-generative-ai", provider: "google", id: "gemini-x" };
const simpleContext = (text = "hi") => ({ messages: [system("sys", { toolsAdded: tools }), user(text)] });

// ---------------------------------------------------------------------------------------------- Anthropic

const aModel = model("anthropic-messages", "anthropic", "https://api.anthropic.com");
const aStart = (extra = {}) => ["message_start", { type: "message_start", message: { id: "msg_01", type: "message", role: "assistant", model: "test-model", content: [], usage: { input_tokens: 120, output_tokens: 1, cache_read_input_tokens: 50, cache_creation_input_tokens: 10, ...(extra.usage ?? {}) }, ...extra.message } }];
const aStop = ["message_stop", { type: "message_stop" }];
const aDelta = (stop_reason, usage = { output_tokens: 42 }, extra = {}) => ["message_delta", { type: "message_delta", delta: { stop_reason, stop_sequence: null, ...extra }, usage }];
const block = (index, content_block) => ["content_block_start", { type: "content_block_start", index, content_block }];
const delta = (index, d) => ["content_block_delta", { type: "content_block_delta", index, delta: d }];
const blockStop = (index) => ["content_block_stop", { type: "content_block_stop", index }];

const anthropicFull = [
	aStart(),
	["ping", { type: "ping" }],
	block(0, { type: "thinking", thinking: "", signature: "" }),
	delta(0, { type: "thinking_delta", thinking: "Let me " }),
	delta(0, { type: "thinking_delta", thinking: "think." }),
	delta(0, { type: "signature_delta", signature: "sig-abc" }),
	blockStop(0),
	block(1, { type: "text", text: "" }),
	delta(1, { type: "text_delta", text: "Hello " }),
	delta(1, { type: "text_delta", text: "world 🌍" }),
	blockStop(1),
	block(2, { type: "tool_use", id: "toolu_1", name: "read", input: {} }),
	delta(2, { type: "input_json_delta", partial_json: "" }),
	delta(2, { type: "input_json_delta", partial_json: '{"path": "src/ma' }),
	delta(2, { type: "input_json_delta", partial_json: 'in.rs", "limit": 1' }),
	delta(2, { type: "input_json_delta", partial_json: "0}" }),
	blockStop(2),
	aDelta("tool_use", { output_tokens: 42, output_tokens_details: { thinking_tokens: 7 } }),
	aStop,
];

const anthropicScenarios = [
	{
		name: "anthropic/thinking-text-tool-history",
		model: aModel,
		context: { messages: history(aModel, foreignModel) },
		options: { apiKey: "sk-ant-test", thinkingEnabled: true, thinkingBudgetTokens: 4000, maxTokens: 9000, temperature: 0.5, metadata: { user_id: "u-1", other: 1 }, toolChoice: "auto", sessionId: "sess-1" },
		responses: [ok(sse(anthropicFull))],
	},
	{
		name: "anthropic/split-bytes-crlf",
		model: { ...aModel, input: ["text"] },
		context: { messages: history(aModel, foreignModel) },
		options: { apiKey: "sk-ant-test", cacheRetention: "none" },
		responses: [{ chunks: (() => {
			const text = sse([aStart(), block(0, { type: "text", text: "" }), delta(0, { type: "text_delta", text: "héllo ✓ 🌍" }), blockStop(0), aDelta("end_turn"), aStop]).join("").replaceAll("\n", "\r\n");
			const bytes = [...new TextEncoder().encode(text)];
			const out = [];
			// Split inside multi-byte characters, but never between a CR and its LF: upstream reads a CR that ends
			// a chunk as a line break and the following LF as an empty line (see the Rust divergence test).
			let start = 0;
			while (start < bytes.length) {
				let end = Math.min(start + 7, bytes.length);
				if (bytes[end - 1] === 13) end += 1;
				out.push(bytes.slice(start, end));
				start = end;
			}
			return out;
		})() }],
	},
	{
		name: "anthropic/redacted-length-long-cache",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test", cacheRetention: "long", thinkingEnabled: false },
		responses: [ok(sse([
			aStart({ usage: { cache_creation_input_tokens: 300, cache_creation: { ephemeral_1h_input_tokens: 200 } } }),
			block(0, { type: "redacted_thinking", data: "opaque-redacted" }),
			blockStop(0),
			block(1, { type: "text", text: "Trunc" }),
			delta(1, { type: "text_delta", text: "ated" }),
			blockStop(1),
			aDelta("max_tokens", { input_tokens: 130, output_tokens: 99, cache_read_input_tokens: null }),
			aStop,
		]))],
	},
	{
		name: "anthropic/env-long-retention",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test", env: { PI_CACHE_RETENTION: "long" } },
		responses: [ok(sse([aStart(), block(0, { type: "text", text: "" }), delta(0, { type: "text_delta", text: "ok" }), blockStop(0), aDelta("end_turn"), aStop]))],
	},
	{
		name: "anthropic/http-400",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test" },
		responses: [json(400, { type: "error", error: { type: "invalid_request_error", message: "prompt is too long" } })],
	},
	{
		name: "anthropic/retry-429-then-ok",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test", maxRetries: 2 },
		responses: [
			json(429, { type: "error", error: { type: "rate_limit_error", message: "slow down" } }, { "retry-after-ms": "0" }),
			json(529, { type: "error", error: { type: "overloaded_error", message: "Overloaded" } }, { "retry-after": "0" }),
			ok(sse([aStart(), block(0, { type: "text", text: "" }), delta(0, { type: "text_delta", text: "finally" }), blockStop(0), aDelta("end_turn"), aStop])),
		],
	},
	{
		name: "anthropic/retry-delay-too-long",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test", maxRetries: 2 },
		responses: [json(429, { type: "error", error: { type: "rate_limit_error", message: "slow down" } }, { "retry-after": "120" })],
	},
	{
		name: "anthropic/no-retry-should-retry-false",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test", maxRetries: 2 },
		responses: [json(500, "upstream exploded", { "x-should-retry": "false", "content-type": "text/plain" })],
	},
	{
		name: "anthropic/sse-error-event",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test" },
		responses: [ok([...sse([aStart(), block(0, { type: "text", text: "" }), delta(0, { type: "text_delta", text: "par" })]), `event: error\ndata: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}\n\n`])],
	},
	{
		name: "anthropic/ended-before-stop",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test" },
		responses: [ok(sse([aStart(), block(0, { type: "tool_use", id: "toolu_9", name: "read", input: {} }), delta(0, { type: "input_json_delta", partial_json: '{"path":"x' })]))],
	},
	{
		name: "anthropic/refusal",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test" },
		responses: [ok(sse([aStart(), aDelta("refusal", { output_tokens: 3 }, { stop_details: { type: "refusal", explanation: "Not allowed" } }), aStop]))],
	},
	{
		name: "anthropic/unknown-stop-reason",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test" },
		responses: [ok(sse([aStart(), aDelta("new_reason"), aStop]))],
	},
	{
		name: "anthropic/sensitive-and-pause",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test" },
		responses: [ok(sse([aStart(), aDelta("sensitive"), aStop]))],
	},
	{
		name: "anthropic/abort-after-first-delta",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test" },
		abortOn: { type: "text_delta", nth: 1 },
		responses: [ok(sse([aStart(), block(0, { type: "text", text: "" }), delta(0, { type: "text_delta", text: "one" }), delta(0, { type: "text_delta", text: "two" }), blockStop(0), aDelta("end_turn"), aStop]))],
	},
	{
		name: "anthropic/missing-key",
		model: aModel,
		context: simpleContext(),
		options: {},
		responses: [],
	},
	{
		name: "anthropic/missing-key-simple-throws",
		fn: "streamSimple",
		model: aModel,
		context: simpleContext(),
		options: {},
		responses: [],
	},
	{
		name: "anthropic/oauth-claude-code",
		model: aModel,
		context: { messages: [system("Project rules", { toolsAdded: [{ ...tools[0], name: "bash" }, { ...tools[0], name: "todowrite" }] }), user("go"), assistant(aModel, [{ type: "toolCall", id: "t1", name: "todowrite", arguments: {} }], { stopReason: "toolUse" }), toolResult("t1", "todowrite", [{ type: "text", text: "ok" }])] },
		options: { apiKey: "sk-ant-oat01-example", thinkingEnabled: false },
		responses: [ok(sse([aStart(), block(0, { type: "tool_use", id: "toolu_2", name: "Bash", input: {} }), delta(0, { type: "input_json_delta", partial_json: '{"path":"p"}' }), blockStop(0), aDelta("tool_use"), aStop]))],
	},
	{
		name: "anthropic/fallback-model-and-transformations",
		model: { ...aModel, compat: { allowedFallbackModels: [{ provider: "anthropic", model: "fallback-model", cost: { input: 1, output: 2, cacheRead: 0.1, cacheWrite: 1.25 } }] } },
		context: simpleContext(),
		options: { apiKey: "sk-ant-test" },
		responses: [ok(sse([
			aStart({ message: { model: "fallback-model", input_transformations: [{ type: "thinking_dropped", path: "messages.0", reason: "mismatch" }] } }),
			block(0, { type: "fallback" }),
			block(1, { type: "text", text: "" }),
			delta(1, { type: "text_delta", text: "from fallback" }),
			blockStop(1),
			aDelta("end_turn"),
			aStop,
		]))],
	},
	{
		name: "anthropic/managed-effort-mid-convo",
		model: { ...aModel, compat: { supportsMidConvoEffort: true, supportsMidConvoSystemMessages: true, supportsMidConvoToolChanges: true, supportsStrictTools: true } },
		context: { messages: [
			system("Base", { toolsAdded: tools }),
			user("one"),
			assistant({ ...aModel }, [{ type: "text", text: "first" }], { providerThinkingLevel: "low" }),
			system("Extra instruction", { sections: { rules: null, mode: "fast" }, toolsAdded: [grammarTool], toolsRemoved: [{ name: "read" }] }),
			user("two"),
		] },
		options: { apiKey: "sk-ant-test", effort: "medium", temperature: 1 },
		responses: [ok(sse([aStart(), block(0, { type: "text", text: "" }), delta(0, { type: "text_delta", text: "ok" }), blockStop(0), aDelta("end_turn"), aStop]))],
	},
	{
		name: "anthropic/beta-header-configured-and-suppressed",
		model: { ...aModel, headers: { "anthropic-beta": "custom-a, custom-b,custom-a" }, compat: { supportsEagerToolInputStreaming: false, supportsCacheControlOnTools: false } },
		context: simpleContext(),
		options: { apiKey: "sk-ant-test", headers: { "X-Extra": "1", accept: null } },
		responses: [ok(sse([aStart(), aDelta("end_turn"), aStop]))],
	},
	{
		name: "anthropic/beta-null-suppresses",
		model: { ...aModel, compat: { supportsEagerToolInputStreaming: false } },
		context: simpleContext(),
		options: { apiKey: "sk-ant-test", headers: { "anthropic-beta": null } },
		responses: [ok(sse([aStart(), aDelta("end_turn"), aStop]))],
	},
	{
		name: "anthropic/copilot-and-header-auth",
		model: { ...aModel, provider: "github-copilot", baseUrl: "https://api.githubcopilot.com" },
		context: { messages: [system("s"), user([{ type: "text", text: "see" }, IMG])] },
		options: { apiKey: "gho_test" },
		responses: [ok(sse([aStart(), aDelta("end_turn"), aStop]))],
	},
	{
		name: "anthropic/openrouter-session-affinity-header-auth",
		model: { ...aModel, provider: "openrouter", baseUrl: "https://openrouter.ai/api/v1" },
		context: simpleContext(),
		options: { headers: { Authorization: "Bearer or-test" }, sessionId: "sess-9" },
		responses: [ok(sse([aStart(), aDelta("end_turn"), aStop]))],
	},
	{
		name: "anthropic/repair-control-chars",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test" },
		responses: [ok([...sse([aStart(), block(0, { type: "text", text: "" })]), 'event: content_block_delta\ndata: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"tab\there \\q"}}\n\n', ...sse([blockStop(0), aDelta("end_turn"), aStop])])],
	},
	{
		name: "anthropic/simple-reasoning-budget",
		fn: "streamSimple",
		model: aModel,
		context: { messages: history(aModel, foreignModel) },
		options: { apiKey: "sk-ant-test", reasoning: "medium", thinkingBudgets: { medium: 5000 }, maxTokens: 3000 },
		responses: [ok(sse([aStart(), aDelta("end_turn"), aStop]))],
	},
	{
		name: "anthropic/simple-adaptive-catalog-model",
		fn: "streamSimple",
		model: MODELS.anthropic["claude-fable-5"],
		context: { messages: [system("Base", { toolsAdded: tools }), user("one"), system("Later", { toolsAdded: [{ ...tools[0], name: "write" }] }), user("two")] },
		options: { apiKey: "sk-ant-test", reasoning: "xhigh" },
		responses: [ok(sse([aStart({ message: { model: "claude-fable-5" } }), aDelta("end_turn"), aStop]))],
	},
	{
		name: "anthropic/simple-no-reasoning",
		fn: "streamSimple",
		model: { ...aModel, thinkingLevelMap: { off: null } },
		context: simpleContext(),
		options: { apiKey: "sk-ant-test", toolChoice: "none", samplingParams: { top_k: 5 } },
		responses: [ok(sse([aStart(), aDelta("end_turn"), aStop]))],
	},
	{
		name: "anthropic/on-payload",
		model: aModel,
		context: simpleContext(),
		options: { apiKey: "sk-ant-test", toolChoice: { type: "tool", name: "read" } },
		payloadPatch: { top_p: 0.9, stream: false },
		responses: [ok(sse([aStart(), aDelta("end_turn"), aStop]))],
	},
];

// ---------------------------------------------------------------------------------------------- OpenAI Completions

const cModel = model("openai-completions", "openai", "https://api.openai.com/v1");
const chunk = (delta, extra = {}) => ({ id: "chatcmpl-1", object: "chat.completion.chunk", created: 1, model: "test-model", choices: [{ index: 0, delta, finish_reason: null, ...extra }] });
const finish = (finish_reason, usage) => ({ id: "chatcmpl-1", object: "chat.completion.chunk", created: 1, model: "test-model-2024", choices: [{ index: 0, delta: {}, finish_reason }], ...(usage ? { usage } : {}) });
const usageChunk = (usage) => ({ id: "chatcmpl-1", object: "chat.completion.chunk", created: 1, model: "test-model", choices: [], usage });

const completionsScenarios = [
	{
		name: "completions/reasoning-text-parallel-tools",
		model: cModel,
		context: { messages: history(cModel, foreignModel) },
		options: { apiKey: "sk-openai-test", sessionId: "sess-1", maxTokens: 1000, temperature: 0.2, reasoningEffort: "high", toolChoice: "auto" },
		responses: [ok(data([
			chunk({ role: "assistant", content: null, reasoning_content: "Think" }),
			chunk({ reasoning_content: "ing", reasoning: "ignored" }),
			chunk({ content: "Hi " }),
			chunk({ content: "there" }),
			chunk({ tool_calls: [{ index: 0, id: "call_a", type: "function", function: { name: "read", arguments: "" } }] }),
			chunk({ tool_calls: [{ index: 0, function: { arguments: '{"path":' } }] }),
			chunk({ tool_calls: [{ index: 1, id: "call_b", type: "function", function: { name: "bash", arguments: '{"command":"l' } }] }),
			chunk({ tool_calls: [{ index: 0, function: { arguments: '"a.txt"}' } }] }),
			chunk({ tool_calls: [{ index: 1, function: { arguments: 's"}' } }] }),
			finish("tool_calls"),
			usageChunk({ prompt_tokens: 100, completion_tokens: 20, prompt_tokens_details: { cached_tokens: 30 }, completion_tokens_details: { reasoning_tokens: 5 } }),
			"[DONE]",
		]))],
	},
	{
		name: "completions/openrouter-anthropic-cache-reasoning-details",
		model: { ...cModel, id: "anthropic/claude-x", provider: "openrouter", baseUrl: "https://openrouter.ai/api/v1", compat: { openRouterRouting: { only: ["anthropic"] } }, thinkingLevelMap: { off: "none", high: "max-ish" } },
		context: { messages: [
			system("sys", { toolsAdded: tools }),
			user("q"),
			assistant({ ...cModel, id: "anthropic/claude-x", provider: "openrouter" }, [{ type: "thinking", thinking: "t", thinkingSignature: JSON.stringify([{ type: "reasoning.text", text: "t", signature: "s1" }]) }, { type: "text", text: "a" }]),
			user("q2"),
		] },
		options: { apiKey: "sk-or-test", sessionId: "sess-2", reasoningEffort: "high" },
		responses: [ok(data([
			chunk({ reasoning: "R1", reasoning_details: [{ type: "reasoning.text", text: "R1", format: "anthropic-claude-v1", index: 0 }] }),
			chunk({ reasoning: "R2", reasoning_details: [{ type: "reasoning.text", text: "R2", signature: "sig", index: 0 }, { type: "reasoning.encrypted", data: "enc", id: "e1" }, { type: "bogus" }] }),
			chunk({ content: "Answer" }),
			finish("stop", { prompt_tokens: 50, completion_tokens: 5, prompt_tokens_details: { cached_tokens: 10, cache_write_tokens: 20 } }),
			"[DONE]",
		]))],
	},
	{
		name: "completions/deepseek-detection",
		model: { ...cModel, provider: "deepseek", baseUrl: "https://api.deepseek.com" },
		context: { messages: history({ ...cModel, provider: "deepseek" }, foreignModel) },
		options: { apiKey: "sk-ds-test", maxTokens: 500, reasoningEffort: "low", cacheRetention: "long" },
		responses: [ok(data([chunk({ content: "ok" }), finish("stop"), usageChunk({ prompt_tokens: 10, completion_tokens: 2, prompt_cache_hit_tokens: 4 }), "[DONE]"]))],
	},
	{
		name: "completions/zai-tool-stream",
		model: { ...cModel, provider: "zai", baseUrl: "https://api.z.ai/api/paas/v4", compat: { zaiToolStream: true, supportsReasoningEffort: true }, thinkingLevelMap: { medium: null } },
		context: simpleContext(),
		options: { apiKey: "zai-test", reasoningEffort: "medium" },
		responses: [ok(data([chunk({ content: "z" }), finish("end"), "[DONE]"]))],
	},
	{
		name: "completions/long-session-id-cache-key",
		model: cModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test", sessionId: "k".repeat(40) + "z".repeat(40) },
		responses: [ok(data([chunk({ content: "ok" }), finish("stop"), "[DONE]"]))],
	},
	{
		name: "completions/http-400-json",
		model: cModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test" },
		responses: [json(400, { error: { message: "Invalid schema", type: "invalid_request_error", param: "tools", code: null } })],
	},
	{
		name: "completions/http-502-text",
		model: cModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test" },
		responses: [json(502, "<html>Bad gateway</html>", { "content-type": "text/html" })],
	},
	{
		name: "completions/http-403-json-without-error",
		model: cModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test" },
		responses: [json(403, { detail: "forbidden" })],
	},
	{
		name: "completions/retry-500-then-ok",
		model: cModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test", maxRetries: 1 },
		responses: [json(500, { error: { message: "oops" } }, { "retry-after-ms": "0" }), ok(data([chunk({ content: "ok" }), finish("stop"), "[DONE]"]))],
	},
	{
		name: "completions/in-stream-error",
		model: cModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test" },
		responses: [ok(data([chunk({ content: "part" }), { error: { message: "Provider overloaded", code: 529, metadata: { raw: "raw upstream detail" } } }]))],
	},
	{
		name: "completions/content-filter",
		model: cModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test" },
		responses: [ok(data([chunk({ content: "bad" }), finish("content_filter"), "[DONE]"]))],
	},
	{
		name: "completions/no-finish-reason",
		model: cModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test" },
		responses: [ok(data([chunk({ content: "x" }), "[DONE]"]))],
	},
	{
		name: "completions/finish-inferred-tool-use",
		model: { ...cModel, compat: { supportsFinishReason: false, supportsUsageInStreaming: false, supportsStore: false, maxTokensField: "max_tokens" } },
		context: simpleContext(),
		options: { apiKey: "sk-openai-test", maxTokens: 77 },
		responses: [ok(data([chunk({ tool_calls: [{ id: "c1", function: { name: "read", arguments: '{"path":"p"}' } }] }), "[DONE]", chunk({ content: "after done is ignored" })]))],
	},
	{
		name: "completions/abort",
		model: cModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test" },
		abortOn: { type: "text_delta", nth: 1 },
		responses: [ok(data([chunk({ content: "a" }), chunk({ content: "b" }), finish("stop"), "[DONE]"]))],
	},
	{
		name: "completions/moonshot-choice-usage",
		model: { ...cModel, provider: "moonshotai", baseUrl: "https://api.moonshot.ai/v1" },
		context: simpleContext(),
		options: { apiKey: "ms-test", maxTokens: 64 },
		responses: [ok(data([chunk({ content: "m" }), finish("length"), { id: "chatcmpl-1", choices: [{ index: 0, delta: {}, finish_reason: "length", usage: { prompt_tokens: 9, completion_tokens: 64, cached_tokens: 3 } }] }, "[DONE]"]))],
	},
	{
		name: "completions/grammar-custom-tool",
		model: { ...cModel, compat: { supportsOpenAIGrammarTools: true, supportsStrictMode: true } },
		context: { messages: [
			system("s", { toolsAdded: [...tools, grammarTool] }),
			user("patch it"),
			assistant(cModel, [{ type: "toolCall", id: "ct_1", name: "apply_patch", arguments: { patch: "*** old" } }], { stopReason: "toolUse" }),
			toolResult("ct_1", "apply_patch", [{ type: "text", text: "done" }]),
		] },
		options: { apiKey: "sk-openai-test" },
		responses: [ok(data([
			chunk({ tool_calls: [{ index: 0, id: "ct_2", type: "custom", custom: { name: "apply_patch", input: "*** Be" } }] }),
			chunk({ tool_calls: [{ index: 0, custom: { input: 'gin "x"\n' } }] }),
			finish("tool_calls"),
			"[DONE]",
		]))],
	},
	{
		name: "completions/simple-chat-template-clamped",
		fn: "streamSimple",
		model: { ...cModel, provider: "custom", baseUrl: "http://localhost:8000/v1", thinkingLevelMap: { minimal: null, low: "lo" }, compat: { thinkingFormat: "chat-template", chatTemplateKwargs: { enable: { $var: "thinking.enabled" }, effort: { $var: "thinking.effort" }, budget: { $var: "thinking.budget" }, gone: { $var: "thinking.effort", omitWhenOff: true }, fixed: 3 }, thinkingTokenBudgetField: "thinking_budget", vllmPriority: -1 } },
		context: simpleContext(),
		options: { apiKey: "local", reasoning: "minimal", maxTokens: 4000, samplingParams: { top_p: 0.8 } },
		responses: [ok(data([chunk({ content: "x" }), finish("stop"), "[DONE]"]))],
	},
	{
		name: "completions/simple-off-qwen",
		fn: "streamSimple",
		model: { ...cModel, provider: "custom", baseUrl: "http://localhost:8000/v1", compat: { thinkingFormat: "qwen" }, samplingParams: { top_k: 20 } },
		context: simpleContext(),
		options: { apiKey: "local" },
		responses: [ok(data([chunk({ content: "x" }), finish("stop"), "[DONE]"]))],
	},
	{
		name: "completions/requires-flags-and-tool-images",
		model: { ...cModel, provider: "custom", baseUrl: "http://localhost:1/v1", compat: { requiresAssistantAfterToolResult: true, requiresToolResultName: true, requiresThinkingAsText: true, supportsDeveloperRole: false, thinkingFormat: "openrouter" } },
		context: { messages: history({ ...cModel, provider: "custom" }, { api: "openai-responses", provider: "openai-codex", id: "gpt" }) },
		options: { apiKey: "k" },
		responses: [ok(data([chunk({ content: "x" }), finish("stop"), "[DONE]"]))],
	},
	{
		name: "completions/missing-key-throws-simple",
		fn: "streamSimple",
		model: cModel,
		context: simpleContext(),
		options: {},
		responses: [],
	},
	{
		name: "completions/header-auth-and-on-payload",
		model: { ...cModel, provider: "openrouter", baseUrl: "https://openrouter.ai/api/v1", headers: { "HTTP-Referer": "pi" } },
		context: simpleContext(),
		options: { headers: { authorization: "Bearer from-header" }, sessionId: "s" },
		payloadPatch: { custom_field: true },
		responses: [ok(data([chunk({ content: "x" }), finish("stop"), "[DONE]"]))],
	},
];

// ---------------------------------------------------------------------------------------------- OpenAI Responses

const rModel = model("openai-responses", "openai", "https://api.openai.com/v1");
const ev = (type, fields) => ({ type, ...fields });
const resp = (status, extra = {}) => ({ id: "resp_1", object: "response", status, output: [], usage: { input_tokens: 200, output_tokens: 50, total_tokens: 250, input_tokens_details: { cached_tokens: 80 }, output_tokens_details: { reasoning_tokens: 20 } }, ...extra });

const responsesFull = [
	ev("response.created", { response: { id: "resp_1", status: "in_progress" } }),
	ev("response.output_item.added", { output_index: 0, item: { type: "reasoning", id: "rs_2", summary: [] } }),
	ev("response.reasoning_summary_text.delta", { output_index: 0, summary_index: 0, delta: "Plan " }),
	ev("response.reasoning_summary_text.delta", { output_index: 0, summary_index: 0, delta: "steps" }),
	ev("response.reasoning_summary_part.done", { output_index: 0, summary_index: 0 }),
	ev("response.output_item.done", { output_index: 0, item: { type: "reasoning", id: "rs_2", summary: [{ type: "summary_text", text: "Plan steps" }], encrypted_content: "ENC" } }),
	ev("response.output_item.added", { output_index: 1, item: { type: "message", id: "msg_2", role: "assistant", content: [], status: "in_progress" } }),
	ev("response.output_text.delta", { output_index: 1, content_index: 0, delta: "Hel" }),
	ev("response.output_text.delta", { output_index: 1, content_index: 0, delta: "lo" }),
	ev("response.output_item.done", { output_index: 1, item: { type: "message", id: "msg_2", role: "assistant", status: "completed", content: [{ type: "output_text", text: "Hello", annotations: [] }] } }),
	ev("response.output_item.added", { output_index: 2, item: { type: "function_call", id: "fc_2", call_id: "call_2", name: "read", arguments: "" } }),
	ev("response.function_call_arguments.delta", { output_index: 2, delta: '{"path":' }),
	ev("response.function_call_arguments.delta", { output_index: 2, delta: '"b.txt"' }),
	ev("response.function_call_arguments.done", { output_index: 2, arguments: '{"path":"b.txt"}' }),
	ev("response.output_item.done", { output_index: 2, item: { type: "function_call", id: "fc_2", call_id: "call_2", name: "read", arguments: '{"path":"b.txt"}' } }),
	ev("response.completed", { response: resp("completed") }),
];

const responsesScenarios = [
	{
		name: "responses/reasoning-text-tool-history",
		model: rModel,
		context: { messages: history(rModel, foreignModel) },
		options: { apiKey: "sk-openai-test", sessionId: "sess-r", reasoningEffort: "high", maxTokens: 8, temperature: 0.3, toolChoice: "auto" },
		responses: [ok(data(responsesFull))],
	},
	{
		name: "responses/incomplete-max-output",
		model: { ...rModel, thinkingLevelMap: { off: "minimal" } },
		context: simpleContext(),
		options: { apiKey: "sk-openai-test" },
		responses: [ok(data([
			ev("response.output_item.added", { output_index: 0, item: { type: "message", id: "msg_3", role: "assistant", content: [] } }),
			ev("response.output_text.delta", { output_index: 0, delta: "cut" }),
			ev("response.incomplete", { response: resp("incomplete", { incomplete_details: { reason: "max_output_tokens" } }) }),
		]))],
	},
	{
		name: "responses/incomplete-content-filter",
		model: rModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test" },
		responses: [ok(data([ev("response.incomplete", { response: resp("incomplete", { incomplete_details: { reason: "content_filter" } }) })]))],
	},
	{
		name: "responses/failed",
		model: rModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test" },
		responses: [ok(data([ev("response.created", { response: { id: "resp_f" } }), ev("response.failed", { response: { id: "resp_f", status: "failed", error: { code: "server_error", message: "The model crashed" } } })]))],
	},
	{
		name: "responses/error-event",
		model: rModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test" },
		responses: [ok(data([ev("error", { code: "rate_limit_exceeded", message: "Too many", param: null, sequence_number: 1 })]))],
	},
	{
		name: "responses/no-terminal-event",
		model: rModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test" },
		responses: [ok(data([ev("response.created", { response: { id: "resp_x" } })]))],
	},
	{
		name: "responses/http-401",
		model: rModel,
		context: simpleContext(),
		options: { apiKey: "sk-bad" },
		responses: [json(401, { error: { message: "Incorrect API key provided", type: "invalid_request_error", code: "invalid_api_key" } })],
	},
	{
		name: "responses/http-401-other-provider",
		model: { ...rModel, provider: "openrouter", baseUrl: "https://openrouter.ai/api/v1" },
		context: simpleContext(),
		options: { apiKey: "sk-bad", sessionId: "s1" },
		responses: [json(401, { error: { message: "No auth" } })],
	},
	{
		name: "responses/grammar-custom-tool-and-priority-tier",
		model: { ...rModel, id: "gpt-5.5", compat: { supportsOpenAIGrammarTools: true, supportsStrictMode: true } },
		context: { messages: [
			system("s", { toolsAdded: [...tools, grammarTool] }),
			user("patch"),
			assistant({ ...rModel, id: "gpt-5.5" }, [{ type: "toolCall", id: "call_p|ctc_p", name: "apply_patch", arguments: { patch: "*** x" }, namespace: "ns" }], { stopReason: "toolUse" }),
			toolResult("call_p|ctc_p", "apply_patch", [{ type: "text", text: "ok" }]),
		] },
		options: { apiKey: "sk-openai-test", serviceTier: "priority", reasoningSummary: "concise" },
		responses: [ok(data([
			ev("response.output_item.added", { output_index: 0, item: { type: "custom_tool_call", id: "ctc_9", call_id: "call_9", name: "apply_patch", input: "" } }),
			ev("response.custom_tool_call_input.delta", { output_index: 0, delta: "*** Begin" }),
			ev("response.custom_tool_call_input.delta", { output_index: 0, delta: ' "q"' }),
			ev("response.custom_tool_call_input.done", { output_index: 0, input: '*** Begin "q"' }),
			ev("response.output_item.done", { output_index: 0, item: { type: "custom_tool_call", id: "ctc_9", call_id: "call_9", name: "apply_patch", input: '*** Begin "q"', namespace: "ns2" } }),
			ev("response.completed", { response: resp("completed", { service_tier: "priority" }) }),
		]))],
	},
	{
		name: "responses/refusal-and-final-answer-phase",
		model: rModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test", serviceTier: "flex" },
		responses: [ok(data([
			ev("response.output_item.added", { output_index: 0, item: { type: "message", id: "msg_r", role: "assistant", content: [], phase: "final_answer" } }),
			ev("response.refusal.delta", { output_index: 0, delta: "I can't" }),
			ev("response.output_item.done", { output_index: 0, item: { type: "message", id: "msg_r", role: "assistant", phase: "final_answer", content: [{ type: "refusal", refusal: "I can't help" }] } }),
			ev("response.completed", { response: resp("completed") }),
		]))],
	},
	{
		name: "responses/backfill-encrypted-reasoning",
		model: rModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test", reasoningEffort: "low" },
		responses: [ok(data([
			ev("response.output_item.added", { output_index: 0, item: { type: "reasoning", id: "rs_b", summary: [] } }),
			ev("response.reasoning_text.delta", { output_index: 0, delta: "raw reasoning" }),
			ev("response.output_item.done", { output_index: 0, item: { type: "reasoning", id: "rs_b", summary: [], content: [{ type: "reasoning_text", text: "raw reasoning" }] } }),
			ev("response.output_item.added", { output_index: 1, item: { type: "function_call", id: "fc_b", call_id: "call_b", name: "read", arguments: "" } }),
			ev("response.function_call_arguments.delta", { output_index: 1, delta: '{"path":"z"}' }),
			ev("response.output_item.done", { output_index: 1, item: { type: "function_call", id: "fc_b", call_id: "call_b", name: "read", arguments: '{"path":"z"}' } }),
			ev("response.completed", { response: resp("completed", { output: [{ type: "reasoning", id: "rs_b", summary: [], encrypted_content: "LATE" }] }) }),
		]))],
	},
	{
		name: "responses/abort",
		model: rModel,
		context: simpleContext(),
		options: { apiKey: "sk-openai-test" },
		abortOn: { type: "text_delta", nth: 1 },
		responses: [ok(data([
			ev("response.output_item.added", { output_index: 0, item: { type: "message", id: "m", role: "assistant", content: [] } }),
			ev("response.output_text.delta", { output_index: 0, delta: "a" }),
			ev("response.output_text.delta", { output_index: 0, delta: "b" }),
			ev("response.completed", { response: resp("completed") }),
		]))],
	},
	{
		name: "responses/xai-copilot-no-max-output",
		model: { ...rModel, provider: "github-copilot", baseUrl: "https://api.githubcopilot.com", compat: { supportsMaxOutputTokens: false, supportsDeveloperRole: false, supportsExplicitPromptCacheMode: true } },
		context: { messages: [system("s"), user("u"), assistant({ ...rModel, provider: "github-copilot" }, [{ type: "text", text: "a" }])] },
		options: { apiKey: "gho", maxTokens: 100, cacheRetention: "none", sessionId: "ignored" },
		responses: [ok(data([ev("response.completed", { response: resp("completed", { usage: undefined }) })]))],
	},
	{
		name: "responses/simple-long-cache",
		fn: "streamSimple",
		model: { ...rModel, provider: "xai", baseUrl: "https://api.x.ai/v1", thinkingLevelMap: { xhigh: "xhigh" } },
		context: simpleContext(),
		options: { apiKey: "xai-test", reasoning: "max", cacheRetention: "long", sessionId: "x".repeat(70) },
		responses: [ok(data([ev("response.completed", { response: resp("in_progress") })]))],
	},
];


// ---------------------------------------------------------------------------------------------- unit fixtures

const { parseStreamingJson, parseJsonWithRepair, repairJson } = await src("utils/json-parse.ts");
const { shortHash } = await src("utils/hash.ts");
const { calculateCost, clampThinkingLevel, getSupportedThinkingLevels } = await src("models.ts");
const { findEnvKeys, getEnvApiKey } = await src("env-api-keys.ts");
const { makeStrictJsonSchema } = await src("api/constrained-sampling.ts");
const { estimateContextTokens } = await src("utils/estimate.ts");
const { transformMessages } = await src("api/transform-messages.ts");
const { adjustMaxTokensForThinking } = await src("api/simple-options.ts");

const toolArgs = '{"path": "src/main.rs", "limit": 10, "nested": {"list": [1, 2.5, -3e2, true, false, null, "s\\"q\\u00e9"]}, "empty": {}}';
const partials = [];
for (let i = 0; i <= toolArgs.length; i++) partials.push(toolArgs.slice(0, i));
partials.push("", "   ", "not json", '{"a": tr', '{"a": nul', '{"a": -', '{"a": 1e', '[1, 2', '{"bad\\x": 1}', '{"s": "raw\ttab"}', '"str', "-", "12", "{\"a\":\"x\\", '{"k": "v"} trailing', "[]", "{}", '{"a": [{"b": "c', '{"\\u12', '{"x": NaN}', '{"x": Infinity', '{"x": -Inf');
const repairs = ['{"a":"x\\qy"}', '{"a":"line\nbreak"}', '{"a":"\\u12zz"}', '{"a":"end\\', 'plain \\ outside', '{"a":"\\u00e9 ok"}', '{"a":"\u0001"}'];

const costModel = { cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75, tiers: [{ inputTokensAbove: 200000, input: 6, output: 22.5, cacheRead: 0.6, cacheWrite: 7.5 }, { inputTokensAbove: 100000, input: 4, output: 20, cacheRead: 0.4, cacheWrite: 5 }] } };
const usages = [
	{ input: 1000, output: 500, cacheRead: 200, cacheWrite: 100 },
	{ input: 150000, output: 10, cacheRead: 0, cacheWrite: 0 },
	{ input: 199000, output: 1, cacheRead: 1000, cacheWrite: 1, cacheWrite1h: 1 },
	{ input: 0, output: 0, cacheRead: 0, cacheWrite: 300, cacheWrite1h: 200 },
].map((u) => ({ usage: u, cost: clone(calculateCost(costModel, { ...u, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } })) }));

const levelModels = [
	{ reasoning: false },
	{ reasoning: true },
	{ reasoning: true, thinkingLevelMap: { xhigh: "xhigh", minimal: null } },
	{ reasoning: true, thinkingLevelMap: { off: null, low: null, medium: null, high: null, max: "max" } },
	{ reasoning: true, thinkingLevelMap: { off: null, minimal: null, low: null, medium: null, high: null } },
];
const levels = ["off", "minimal", "low", "medium", "high", "xhigh", "max", "bogus"];
const thinking = levelModels.map((m) => ({ model: m, supported: getSupportedThinkingLevels(m), clamped: Object.fromEntries(levels.map((l) => [l, clampThinkingLevel(m, l)])) }));

const envCases = [
	["openai", { OPENAI_API_KEY: "sk-o" }],
	["openai", {}],
	["anthropic", { ANTHROPIC_AUTH_TOKEN: "tok", ANTHROPIC_API_KEY: "key" }],
	["anthropic", { ANTHROPIC_AUTH_TOKEN: "tok" }],
	["anthropic", { ANTHROPIC_OAUTH_TOKEN: "oat", ANTHROPIC_API_KEY: "key" }],
	["github-copilot", { COPILOT_GITHUB_TOKEN: "gh" }],
	["moonshotai-cn", { MOONSHOT_API_KEY: "m" }],
	["amazon-bedrock", { AWS_PROFILE: "p" }],
	["amazon-bedrock", { AWS_ACCESS_KEY_ID: "a" }],
	["amazon-bedrock", { AWS_ACCESS_KEY_ID: "a", AWS_SECRET_ACCESS_KEY: "s" }],
	["amazon-bedrock", { AWS_WEB_IDENTITY_TOKEN_FILE: "/f" }],
	["unknown-provider", { OPENAI_API_KEY: "x" }],
	["zai", { ZAI_API_KEY: "" }],
	["google-vertex", { GOOGLE_CLOUD_API_KEY: "g" }],
	["google-vertex", { GOOGLE_APPLICATION_CREDENTIALS: "/nonexistent/creds.json", GOOGLE_CLOUD_PROJECT: "p", GOOGLE_CLOUD_LOCATION: "l" }],
];
// Every lookup must be answered by the scoped env: clear the process variables these providers read.
for (const name of Object.keys(process.env)) {
	if (/(_API_KEY|_TOKEN|^AWS_|^GOOGLE_|^GCLOUD_|^HF_TOKEN)/.test(name)) delete process.env[name];
}
const env = envCases.map(([provider, e]) => ({ provider, env: e, found: findEnvKeys(provider, e) ?? null, key: getEnvApiKey(provider, e) ?? null }));

const schemas = [
	{ type: "object", properties: { a: { type: "string" }, b: { type: "number" } }, required: ["a"] },
	{ type: "object", properties: { o: { type: "object", properties: { x: { type: "string" } } }, l: { type: "array", items: { type: "string" } }, n: { type: ["string", "null"] } } },
	{ type: "object", properties: { u: { anyOf: [{ type: "string" }, { type: "number" }] } }, required: ["u"] },
	{ type: "object", properties: { u: { anyOf: [{ type: "object" }, { type: "string" }] } } },
	{ type: "object", properties: { r: { $ref: "#/x" } } },
	{ type: "object", additionalProperties: true },
	{ type: "object", properties: { a: { type: "string" } }, required: ["b"] },
	{ type: "string" },
	{ type: "object", properties: { t: { type: "array", items: [{ type: "string" }] } } },
	{ type: "object", properties: { e: { enum: ["a", null] }, c: { const: null } } },
].map((schema) => {
	try {
		return { schema, strict: makeStrictJsonSchema(schema) };
	} catch (e) {
		return { schema, error: e.message };
	}
});

const estimateContexts = [
	{ messages: history(aModel, foreignModel) },
	{ messages: [...history(aModel, foreignModel).slice(0, 3), { ...assistant(aModel, [{ type: "text", text: "x" }]), usage: { ...usage0, input: 1000, output: 50, totalTokens: 0 }, timestamp: 10 }, user("after usage", 11), toolResult("z", "read", [IMG])] },
	{ messages: [system("s", { toolsAdded: tools, toolsRemoved: [{ name: "x" }] })] },
].map((context) => ({ context, estimate: estimateContextTokens(context) }));

const maxTokens = [[undefined, 32000, "high"], [1000, 32000, "medium"], [30000, 32000, "high"], [undefined, 1500, "minimal"], [100, 100000, "xhigh", { high: 4000 }]].map(([base, cap, level, budgets]) => ({ base: base ?? null, cap, level, budgets: budgets ?? null, result: adjustMaxTokensForThinking(base, cap, level, budgets) }));

const transformTarget = { ...aModel, input: ["text"] };
const transform = transformMessages(history(aModel, foreignModel).map((m) => clone(m)), transformTarget, (id) => id.replace(/[^a-zA-Z0-9_-]/g, "_").slice(0, 64));

const unitsOut = join(here, "units.json");
writeFileSync(unitsOut, `${JSON.stringify({
	partialJson: partials.map((input) => ({ input, output: clone(parseStreamingJson(input)) })),
	repairJson: repairs.map((input) => ({ input, output: repairJson(input), parsed: (() => { try { return clone(parseJsonWithRepair(input)); } catch { return "<error>"; } })() })),
	shortHash: ["", "a", "call_1|fc_abcdef", "x".repeat(500), "émoji 🌍", "fc_" + "z".repeat(100)].map((input) => ({ input, output: shortHash(input) })),
	cost: { model: costModel, cases: usages },
	thinking,
	env,
	strictSchemas: schemas,
	estimate: estimateContexts,
	maxTokens,
	transform: { model: transformTarget, input: history(aModel, foreignModel), output: clone(transform) },
}, null, "\t")}\n`);
console.log(`wrote ${unitsOut}`);

const scenarios = [...anthropicScenarios, ...completionsScenarios, ...responsesScenarios];
const results = [];
for (const scenario of scenarios) results.push(await run(scenario));
const out = join(here, "providers.json");
writeFileSync(out, `${JSON.stringify({ generator: "crates/ai/fixtures/generate.mjs", now: NOW, scenarios: results }, null, "\t")}\n`);
console.log(`wrote ${results.length} scenarios to ${out}`);
