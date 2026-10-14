// Generates the fixtures of the remaining providers from this worktree's TypeScript sources: azure-openai-responses,
// openai-codex-responses, pi-messages, cloudflare, bedrock, claude-code-cli, the Models auth runtime, and every OAuth
// flow's refresh/toAuth/device-code logins (oauth.json). Canned HTTP/SSE responses go through a stubbed `fetch`; each
// scenario records the request (URL, method, headers, JSON body), every pushed event (cloned at push time) and the
// final message. tests/providers_rest.rs replays them through the Rust ports.
// Usage: node crates/ai/fixtures/generate-rest.mjs [section...]
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const src = (path) => import(pathToFileURL(join(root, "packages/ai/src", path)).href);

for (const name of Object.keys(process.env)) {
	if (/^(PI_|AZURE_|OPENAI_|CLOUDFLARE_|ULTRON_|KIMI_|AWS_)/.test(name)) delete process.env[name];
}
const NOW = 1767225600000;
Date.now = () => NOW;

const { AssistantMessageEventStream } = await src("utils/event-stream.ts");
const { getPiUserAgent } = await src("utils/pi-user-agent.ts");

export const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));

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
		out[key] = value.replaceAll(UA, "<pi-user-agent>");
	});
	return Object.fromEntries(Object.entries(out).sort(([a], [b]) => (a < b ? -1 : 1)));
}

const abortError = () => new DOMException("This operation was aborted", "AbortError");
const STATUS_TEXT = { 200: "OK", 400: "Bad Request", 401: "Unauthorized", 403: "Forbidden", 404: "Not Found", 429: "Too Many Requests", 500: "Internal Server Error", 502: "Bad Gateway", 503: "Service Unavailable" };

function bodyText(body) {
	if (body === undefined || body === null) return null;
	if (typeof body === "string") return body;
	if (body instanceof URLSearchParams) return body.toString();
	return new TextDecoder().decode(body);
}

function stubFetch(queue, requests) {
	return async (url, init = {}) => {
		const text = bodyText(init.body);
		let body = null;
		if (text !== null) {
			try {
				body = JSON.parse(text);
			} catch {
				body = text;
			}
		}
		requests.push({ url: String(url), method: init.method ?? "GET", headers: requestHeaders(init), body });
		const canned = queue.shift();
		if (!canned) throw new Error("unexpected request");
		if (canned.networkError) throw new TypeError("fetch failed");
		const encoder = new TextEncoder();
		const chunks = canned.chunks.map((c) => (typeof c === "string" ? encoder.encode(c) : new Uint8Array(c)));
		const signal = init.signal;
		const stream = new ReadableStream({
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
		const status = canned.status ?? 200;
		return new Response(stream, {
			status,
			statusText: STATUS_TEXT[status] ?? "",
			headers: canned.headers ?? { "content-type": "text/event-stream" },
		});
	};
}

const realFetch = globalThis.fetch;
/** Run one scenario of `api` (a module with stream/streamSimple). `fetchOption` passes the stub as `options.fetch`. */
async function run(api, scenario, fetchOption = true) {
	const queue = scenario.responses.map((r) => ({ ...r, chunks: [...(r.chunks ?? [])] }));
	const requests = [];
	const controller = new AbortController();
	const record = {
		name: scenario.name,
		fn: scenario.fn ?? "stream",
		model: scenario.model,
		context: scenario.context,
		options: clone(scenario.options),
		responses: scenario.responses,
		abortOn: scenario.abortOn ?? null,
	};
	recording = { events: [], abortOn: scenario.abortOn, seen: 0, controller };
	globalThis.fetch = stubFetch(queue, requests);
	try {
		const extra = fetchOption ? { fetch: globalThis.fetch } : {};
		const s = api[record.fn](scenario.model, scenario.context, { ...scenario.options, ...extra, signal: controller.signal });
		record.result = clone(await s.result());
		record.thrown = null;
	} catch (e) {
		record.result = null;
		record.thrown = e.message;
	}
	globalThis.fetch = realFetch;
	record.events = recording.events;
	record.requests = requests;
	recording = null;
	return record;
}

const data = (items) => items.map((d) => `data: ${typeof d === "string" ? d : JSON.stringify(d)}\n\n`);
const ok = (chunks) => ({ chunks });
const json = (status, body, headers = {}) => ({
	status,
	headers: { "content-type": "application/json", ...headers },
	chunks: [typeof body === "string" ? body : JSON.stringify(body)],
});

const tools = [
	{
		name: "read",
		description: "Read a file",
		parameters: { type: "object", properties: { path: { type: "string" }, limit: { type: "number" } }, required: ["path"] },
	},
	{
		name: "bash",
		description: "Run a command",
		parameters: { type: "object", properties: { command: { type: "string" } }, required: ["command"] },
		constrainedSampling: { type: "json_schema", strict: "prefer" },
	},
];
const IMG = { type: "image", data: "aGVsbG8=", mimeType: "image/png" };
const usage0 = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };
const system = (content, extra = {}) => ({ role: "system", content, timestamp: 1, ...extra });
const user = (content, timestamp = 2) => ({ role: "user", content, timestamp });
const assistant = (model, content, extra = {}) => ({ role: "assistant", content, api: model.api, provider: model.provider, model: model.id, usage: usage0, stopReason: "stop", timestamp: 3, ...extra });
const toolResult = (toolCallId, toolName, content, isError = false) => ({ role: "toolResult", toolCallId, toolName, content, isError, timestamp: 4 });

function model(api, provider, baseUrl, extra = {}) {
	return {
		id: "test-model",
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

const responsesSig = JSON.stringify({ type: "reasoning", id: "rs_1", summary: [{ type: "summary_text", text: "I should read." }], encrypted_content: "enc1" });
/** A transcript with prior Responses turns, a foreign model, an orphaned tool call, images and a mid-conversation system message. */
function history(m) {
	return [
		system("You are helpful.", { toolsAdded: tools, sections: { rules: "<rules>be brief</rules>" } }),
		user("List files"),
		assistant(m, [
			{ type: "thinking", thinking: "I should read.", thinkingSignature: responsesSig },
			{ type: "text", text: "Reading.", textSignature: JSON.stringify({ v: 1, id: "msg_1", phase: "commentary" }) },
			{ type: "toolCall", id: "call_1|fc_1", name: "read", arguments: { path: "a.txt" } },
		], { stopReason: "toolUse" }),
		toolResult("call_1|fc_1", "read", [{ type: "text", text: "contents of a" }, IMG]),
		user([{ type: "text", text: "Now look at this" }, IMG]),
		assistant({ api: "anthropic-messages", provider: "anthropic", id: "claude-x" }, [
			{ type: "thinking", thinking: "Foreign thoughts", thinkingSignature: "x" },
			{ type: "text", text: "Foreign answer" },
			{ type: "toolCall", id: "call|weird+id/with=chars", name: "bash", arguments: { command: "ls" } },
		], { stopReason: "toolUse" }),
		system("Mid-conversation rule"),
		user("Continue"),
		assistant(m, [{ type: "text", text: "partial" }], { stopReason: "error", errorMessage: "boom" }),
		user("Again"),
	];
}
const simple = (text = "hi") => ({ messages: [system("sys", { toolsAdded: tools }), user(text)] });

// ---------------------------------------------------------------------------------------------- Responses events

const ev = (type, fields) => ({ type, ...fields });
const resp = (status, extra = {}) => ({ id: "resp_1", object: "response", status, output: [], usage: { input_tokens: 200, output_tokens: 50, total_tokens: 250, input_tokens_details: { cached_tokens: 80 }, output_tokens_details: { reasoning_tokens: 20 } }, ...extra });
const responsesFull = (terminal = "response.completed", extra = {}) => [
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
	ev(terminal, { response: resp("completed", extra) }),
];
const textOnly = (text = "ok", terminal = "response.completed", extra = {}) => [
	ev("response.output_item.added", { output_index: 0, item: { type: "message", id: "msg_3", role: "assistant", content: [] } }),
	ev("response.output_text.delta", { output_index: 0, delta: text }),
	ev("response.output_item.done", { output_index: 0, item: { type: "message", id: "msg_3", role: "assistant", status: "completed", content: [{ type: "output_text", text, annotations: [] }] } }),
	ev(terminal, { response: resp("completed", extra) }),
];

const sections = {};

// ---------------------------------------------------------------------------------------------- Azure OpenAI Responses

sections.azure = async () => {
	const api = await src("api/azure-openai-responses.ts");
	const aModel = (extra = {}) => model("azure-openai-responses", "azure-openai-responses", "https://res-a.openai.azure.com/openai/v1", extra);
	const longSession = `${"s".repeat(62)}🌍🌍🌍`;
	const scenarios = [
		{
			name: "azure/history-reasoning-tools",
			model: aModel({ headers: { "X-Model": "m", "X-Drop": "d" } }),
			context: { messages: history(aModel()) },
			options: { apiKey: "az-key", sessionId: longSession, reasoningEffort: "high", reasoningSummary: "detailed", maxTokens: 5, temperature: 0.2, toolChoice: "auto", samplingParams: { top_p: 0.9, temperature: 0.7 }, headers: { "X-Extra": "1", "x-drop": null } },
			responses: [ok(data(responsesFull("response.completed", { service_tier: "priority" })))],
		},
		{
			name: "azure/resource-name-deployment-map-api-version",
			model: aModel({ baseUrl: "" }),
			context: simple(),
			options: { apiKey: "az-key", azureApiVersion: "2025-04-01-preview", env: { AZURE_OPENAI_RESOURCE_NAME: "my-res", AZURE_OPENAI_DEPLOYMENT_NAME_MAP: " other=x , test-model = dep-1 ,bad, =y" } },
			responses: [ok(data(textOnly()))],
		},
		{
			name: "azure/explicit-deployment-and-base-url",
			model: aModel(),
			context: simple(),
			options: { apiKey: "az-key", azureDeploymentName: "dep-explicit", azureBaseUrl: "  https://res-b.cognitiveservices.azure.com/openai/v1/responses?x=1  ", env: { AZURE_OPENAI_API_VERSION: "2024-10-21" } },
			responses: [ok(data(textOnly()))],
		},
		{
			name: "azure/env-base-url-openai-path",
			model: aModel(),
			context: simple(),
			options: { apiKey: "az-key", env: { AZURE_OPENAI_BASE_URL: "https://res-c.ai.azure.com/openai//" } },
			responses: [ok(data(textOnly()))],
		},
		{
			name: "azure/proxy-base-url-kept",
			model: aModel({ baseUrl: "https://proxy.example.com/azure/v1/" }),
			context: simple(),
			options: { apiKey: "az-key" },
			responses: [ok(data(textOnly()))],
		},
		{ name: "azure/no-base-url", model: aModel({ baseUrl: "" }), context: simple(), options: { apiKey: "az-key" }, responses: [] },
		{ name: "azure/invalid-base-url", model: aModel({ baseUrl: "not a url" }), context: simple(), options: { apiKey: "az-key" }, responses: [] },
		{ name: "azure/missing-key", model: aModel(), context: simple(), options: {}, responses: [] },
		{
			name: "azure/reasoning-off-mapped",
			model: aModel({ thinkingLevelMap: { off: "minimal" } }),
			context: simple(),
			options: { apiKey: "az-key" },
			responses: [ok(data(textOnly()))],
		},
		{
			name: "azure/reasoning-off-unsupported",
			model: aModel({ thinkingLevelMap: { off: null } }),
			context: simple(),
			options: { apiKey: "az-key", reasoningSummary: "concise" },
			responses: [ok(data(textOnly()))],
		},
		{
			name: "azure/strict-unsupported-grammar-off",
			model: aModel({ reasoning: false, compat: { supportsStrictMode: false } }),
			context: simple(),
			options: { apiKey: "az-key", reasoningEffort: "high" },
			responses: [ok(data(textOnly()))],
		},
		{ name: "azure/http-400", model: aModel(), context: simple(), options: { apiKey: "az-key" }, responses: [json(400, { error: { message: "Invalid deployment", type: "invalid_request_error", code: "DeploymentNotFound" } })] },
		{ name: "azure/http-429-retry", model: aModel(), context: simple(), options: { apiKey: "az-key", maxRetries: 1 }, responses: [json(429, { error: { message: "slow" } }, { "retry-after-ms": "0" }), ok(data(textOnly("after retry")))] },
		{ name: "azure/incomplete", model: aModel(), context: simple(), options: { apiKey: "az-key" }, responses: [ok(data([ev("response.incomplete", { response: resp("incomplete", { incomplete_details: { reason: "content_filter" } }) })]))] },
		{ name: "azure/failed", model: aModel(), context: simple(), options: { apiKey: "az-key" }, responses: [ok(data([ev("response.failed", { response: { id: "r", status: "failed", error: { code: "server_error", message: "crash" } } })]))] },
		{ name: "azure/no-terminal", model: aModel(), context: simple(), options: { apiKey: "az-key" }, responses: [ok(data([ev("response.created", { response: { id: "resp_x" } })]))] },
		{ name: "azure/abort-mid-stream", model: aModel(), context: simple(), options: { apiKey: "az-key" }, abortOn: { type: "text_delta", nth: 1 }, responses: [ok(data([...textOnly("one").slice(0, 2), ev("response.output_text.delta", { output_index: 0, delta: "two" }), ...textOnly("one").slice(2)]))] },
		{ name: "azure/simple-reasoning-clamped", fn: "streamSimple", model: aModel({ thinkingLevelMap: { xhigh: "high" } }), context: simple(), options: { apiKey: "az-key", reasoning: "max", toolChoice: "none", maxTokens: 100 }, responses: [ok(data(textOnly()))] },
		{ name: "azure/simple-off", fn: "streamSimple", model: aModel(), context: simple(), options: { apiKey: "az-key", reasoning: "off" }, responses: [ok(data(textOnly()))] },
		{ name: "azure/simple-missing-key", fn: "streamSimple", model: aModel(), context: simple(), options: {}, responses: [] },
	];
	const results = [];
	for (const scenario of scenarios) results.push(await run(api, scenario));
	return { file: "azure.json", scenarios: results };
};

// ---------------------------------------------------------------------------------------------- OpenAI Codex Responses

/** Diagnostics carry the JS stack; the Rust port has none. */
function stripStacks(value) {
	if (Array.isArray(value)) return value.forEach(stripStacks);
	if (value && typeof value === "object") {
		if (Array.isArray(value.diagnostics)) for (const d of value.diagnostics) if (d.error) delete d.error.stack;
		for (const v of Object.values(value)) stripStacks(v);
	}
}

sections.codex = async () => {
	const api = await src("api/openai-codex-responses.ts");
	const b64 = (v) => Buffer.from(JSON.stringify(v)).toString("base64").replace(/=+$/, "");
	// Unsigned fakes; the signature says so, which also keeps scripts/secret-scan.mjs from taking them for real tokens.
	const jwt = (claims) => `${b64({ alg: "none" })}.${b64(claims)}.example-sig`;
	const token = jwt({ "https://api.openai.com/auth": { chatgpt_account_id: "acc-123" } });
	const cModel = (extra = {}) => model("openai-codex-responses", "openai-codex", "https://chatgpt.com/backend-api", { id: "gpt-5.5", ...extra });
	const done = (extra = {}, type = "response.done") => ev(type, { response: resp("completed", extra) });
	const text = (t = "ok") => [
		ev("response.output_item.added", { output_index: 0, item: { type: "message", id: "msg_3", role: "assistant", content: [] } }),
		ev("response.output_text.delta", { output_index: 0, delta: t }),
		ev("response.output_item.done", { output_index: 0, item: { type: "message", id: "msg_3", role: "assistant", status: "completed", content: [{ type: "output_text", text: t, annotations: [] }] } }),
	];
	const sse = { apiKey: token, transport: "sse" };
	const scenarios = [
		{
			name: "codex/history-tools-sse",
			model: cModel({ headers: { "X-Model": "m", "x-drop": "d" } }),
			context: { messages: history(cModel()) },
			options: { ...sse, sessionId: "c".repeat(70), reasoningEffort: "high", reasoningSummary: "detailed", textVerbosity: "medium", serviceTier: "priority", temperature: 0.4, toolChoice: "required", headers: { "X-Extra": "1", "X-Drop": null } },
			responses: [ok(data([...responsesFull().slice(0, -1), done({ service_tier: "default", end_turn: true })]))],
		},
		{ name: "codex/auto-falls-back-to-sse", model: cModel(), context: simple(), options: { apiKey: token, sessionId: "sess-fb" }, responses: [ok(data([...text(), done()]))] },
		{ name: "codex/auto-fallback-remembered", model: cModel(), context: simple(), options: { apiKey: token, sessionId: "sess-fb", transport: "websocket" }, responses: [ok(data([...text(), done({ end_turn: false })]))] },
		{ name: "codex/websocket-cached-no-session", model: cModel(), context: simple(), options: { apiKey: token, transport: "websocket-cached", cacheRetention: "none", sessionId: "ignored" }, responses: [ok(data([...text(), done()]))] },
		{ name: "codex/no-system-prompt-flex", model: cModel({ id: "gpt-5", baseUrl: "https://proxy.example.com/codex/" }), context: { messages: [user("hi")] }, options: { ...sse, serviceTier: "flex" }, responses: [ok(data([...text(), done({ service_tier: "flex" }, "response.completed")]))] },
		{ name: "codex/base-url-codex-responses", model: cModel({ baseUrl: "https://proxy.example.com/x/codex/responses//" }), context: simple(), options: sse, responses: [ok(data([...text(), done()]))] },
		{ name: "codex/blank-base-url", model: cModel({ baseUrl: "   " }), context: simple(), options: sse, responses: [ok(data([...text(), done()]))] },
		{ name: "codex/effort-none-off-missing", model: cModel(), context: simple(), options: { ...sse, reasoningEffort: "none", reasoningSummary: null }, responses: [ok(data([...text(), done()]))] },
		{ name: "codex/effort-none-off-null", model: cModel({ thinkingLevelMap: { off: null } }), context: simple(), options: { ...sse, reasoningEffort: "none" }, responses: [ok(data([...text(), done()]))] },
		{ name: "codex/effort-mapped", model: cModel({ thinkingLevelMap: { high: "xhigh", low: null } }), context: simple(), options: { ...sse, reasoningEffort: "low", reasoningSummary: "off" }, responses: [ok(data([...text(), done()]))] },
		{ name: "codex/default-reasoning-off-mapped", model: cModel({ thinkingLevelMap: { off: "minimal" } }), context: simple(), options: sse, responses: [ok(data([...text(), done()]))] },
		{ name: "codex/non-reasoning-model", model: cModel({ reasoning: false, compat: { supportsStrictMode: false } }), context: simple(), options: sse, responses: [ok(data([...text(), done()]))] },
		{ name: "codex/bad-token", model: cModel(), context: simple(), options: { apiKey: "not-a-jwt", transport: "sse" }, responses: [] },
		{ name: "codex/token-without-account", model: cModel(), context: simple(), options: { apiKey: jwt({ sub: "x" }), transport: "sse" }, responses: [] },
		{ name: "codex/missing-key", model: cModel(), context: simple(), options: {}, responses: [] },
		{ name: "codex/usage-limit-429", model: cModel(), context: simple(), options: sse, responses: [json(429, { error: { code: "usage_limit_reached", message: "limit", plan_type: "PLUS", resets_at: 1 } })] },
		{ name: "codex/usage-limit-no-message", model: cModel(), context: simple(), options: { ...sse, maxRetries: 2 }, responses: [json(400, { error: { type: "usage_not_included" } })] },
		{ name: "codex/http-400-message", model: cModel(), context: simple(), options: sse, responses: [json(400, { error: { message: "Bad input" } })] },
		{ name: "codex/http-500-text", model: cModel(), context: simple(), options: sse, responses: [json(500, "", { "content-type": "text/plain" })] },
		{ name: "codex/http-502-retry-then-ok", model: cModel(), context: simple(), options: { ...sse, maxRetries: 1 }, responses: [json(502, "upstream connect error", { "retry-after-ms": "0", "content-type": "text/plain" }), ok(data([...text("retried"), done()]))] },
		{ name: "codex/retry-delay-exceeded", model: cModel(), context: simple(), options: { ...sse, maxRetries: 1, maxRetryDelayMs: 1000 }, responses: [json(503, { error: { message: "busy" } }, { "retry-after": "120" })] },
		{ name: "codex/non-retryable-is-retried-by-catch", model: cModel(), context: simple(), options: { ...sse, maxRetries: 1 }, responses: [json(400, { error: { message: "first" } }), ok(data([...text(), done()]))] },
		{ name: "codex/network-error", model: cModel(), context: simple(), options: sse, responses: [{ networkError: true }] },
		{ name: "codex/error-event", model: cModel(), context: simple(), options: sse, responses: [ok(data([...text(), ev("error", { error: { code: "server_overloaded" } })]))] },
		{ name: "codex/error-event-message", model: cModel(), context: simple(), options: sse, responses: [ok(data([ev("error", { code: "x", message: "Detailed" })]))] },
		{ name: "codex/error-event-bare", model: cModel(), context: simple(), options: sse, responses: [ok(data([ev("error", { sequence_number: 3 })]))] },
		{ name: "codex/response-failed", model: cModel(), context: simple(), options: sse, responses: [ok(data([ev("response.failed", { response: { error: { code: "c", message: "Model failed" } } })]))] },
		{ name: "codex/response-failed-bare", model: cModel(), context: simple(), options: sse, responses: [ok(data([ev("response.failed", {})]))] },
		{ name: "codex/incomplete-max-tokens", model: cModel(), context: simple(), options: sse, responses: [ok(data([...text(), ev("response.incomplete", { response: resp("incomplete", { incomplete_details: { reason: "max_output_tokens" } }) })]))] },
		{ name: "codex/unknown-status-normalized", model: cModel(), context: simple(), options: sse, responses: [ok(data([...text(), ev("response.done", { response: resp("weird") })]))] },
		{ name: "codex/no-terminal", model: cModel(), context: simple(), options: sse, responses: [ok(data([...text(), "[DONE]"]))] },
		{ name: "codex/events-after-terminal-ignored", model: cModel(), context: simple(), options: sse, responses: [ok(data([...text(), done(), ev("error", { message: "late" })]))] },
		{ name: "codex/untyped-and-done-skipped", model: cModel(), context: simple(), options: sse, responses: [ok([...data([{ foo: 1 }, "[DONE]"]), "event: x\n: comment\n\n", ...data([...text(), done()])])] },
		{ name: "codex/crlf-single-frame-at-eof", model: cModel(), context: simple(), options: sse, responses: [ok([`data: ${JSON.stringify(done({ status: "completed" }))}\r\n\r\n`])] },
		{ name: "codex/multiline-data-at-eof", model: cModel(), context: simple(), options: sse, responses: [ok(["data: {\"type\":\"response.done\",\ndata:  \"response\":{\"status\":\"completed\"}}"])] },
		{ name: "codex/split-utf8", model: cModel(), context: simple(), options: sse, responses: [ok([...(() => { const b = Buffer.from(data([ev("response.output_item.added", { output_index: 0, item: { type: "message", id: "m", role: "assistant", content: [] } }), ev("response.output_text.delta", { output_index: 0, delta: "héllo 🌍" })]).join("")); return [Array.from(b.subarray(0, b.length - 9)), Array.from(b.subarray(b.length - 9))]; })(), ...data([done()])])] },
		{ name: "codex/abort-mid-stream", model: cModel(), context: simple(), options: sse, abortOn: { type: "text_delta", nth: 1 }, responses: [ok(data([...text("one"), ev("response.output_text.delta", { output_index: 0, delta: "two" }), done()]))] },
		{ name: "codex/invalid-websocket-timeout", model: cModel(), context: simple(), options: { ...sse, websocketConnectTimeoutMs: -1 }, responses: [] },
		{ name: "codex/simple-reasoning-sse", fn: "streamSimple", model: cModel({ thinkingLevelMap: { xhigh: "xhigh" } }), context: simple(), options: { apiKey: token, transport: "sse", reasoning: "max", toolChoice: "none", maxTokens: 50 }, responses: [ok(data([...text(), done()]))] },
		{ name: "codex/simple-missing-key", fn: "streamSimple", model: cModel(), context: simple(), options: {}, responses: [] },
	];
	const savedWebSocket = globalThis.WebSocket;
	const savedBuiltin = process.getBuiltinModule;
	delete globalThis.WebSocket;
	process.getBuiltinModule = (id) => (id === "node:zlib" ? undefined : savedBuiltin.call(process, id));
	api.resetOpenAICodexWebSocketDebugStats();
	const results = [];
	for (const scenario of scenarios) results.push(await run(api, scenario, false));
	const stats = Object.fromEntries(["sess-fb", "ignored", "unused"].map((id) => [id, clone(api.getOpenAICodexWebSocketDebugStats(id))]));
	globalThis.WebSocket = savedWebSocket;
	process.getBuiltinModule = savedBuiltin;
	stripStacks(results);
	return { file: "codex.json", stats, scenarios: results };
};

// ---------------------------------------------------------------------------------------------- pi-messages

sections.pi = async () => {
	const api = await src("api/pi-messages.ts");
	const pModel = (extra = {}) => model("pi-messages", "radius", "https://gateway.example.com/v1//", extra);
	const usage = { input: 10, output: 5, cacheRead: 2, cacheWrite: 1, totalTokens: 18, cost: { input: 0.1, output: 0.2, cacheRead: 0.01, cacheWrite: 0.02, total: 0.33 } };
	const full = [
		{ type: "start" },
		{ type: "thinking_start", contentIndex: 0 },
		{ type: "thinking_delta", contentIndex: 0, delta: "Hmm " },
		{ type: "thinking_delta", contentIndex: 0, delta: "ok" },
		{ type: "thinking_end", contentIndex: 0, content: "Hmm ok", contentSignature: "tsig", redacted: false },
		{ type: "text_start", contentIndex: 1 },
		{ type: "text_delta", contentIndex: 1, delta: "Hel" },
		{ type: "text_delta", contentIndex: 1, delta: "lo 🌍" },
		{ type: "text_end", contentIndex: 1, content: "Hello 🌍", contentSignature: "xsig" },
		{ type: "toolcall_start", contentIndex: 2, id: "call_1", toolName: "read" },
		{ type: "toolcall_delta", contentIndex: 2, delta: '{"path":"a' },
		{ type: "toolcall_delta", contentIndex: 2, delta: '.txt"}' },
		{ type: "toolcall_end", contentIndex: 2, toolCall: { type: "toolCall", id: "call_1", name: "read", arguments: { path: "a.txt" }, thoughtSignature: "ts" } },
		{ type: "done", reason: "toolUse", usage, responseId: "resp-9", providerThinkingLevel: "high", rewrite: { policyId: "p1", policyVersion: 2, changed: true, tokenCountChange: -10, messageCountChange: 0, systemPromptChanged: false } },
	];
	const scenarios = [
		{ name: "pi/full-stream", model: pModel(), context: { messages: history(pModel()) }, options: { apiKey: "rk", temperature: 0.5, maxTokens: 100, reasoning: "high", sessionId: "s-1", toolChoice: { type: "function", function: { name: "read" } }, debug: true, headers: { "X-Extra": "1", "X-Null": null } }, responses: [ok(data(full))] },
		{ name: "pi/crlf-and-trailing-frame", model: pModel({ baseUrl: "https://gateway.example.com/v1?team=a" }), context: simple(), options: { apiKey: "rk", env: { PI_CACHE_RETENTION: "long" }, debug: true }, responses: [ok(["data: {\"type\":\"start\"}\r\n\r\n", ": comment\r\n\r\n", "data: [DONE]\r\n\r\n", `event: x\ndata: ${JSON.stringify({ type: "done", reason: "stop", usage })}`])] },
		{ name: "pi/error-terminal", model: pModel(), context: simple(), options: { apiKey: "rk", cacheRetention: "none" }, responses: [ok(data([{ type: "start" }, { type: "text_start", contentIndex: 0 }, { type: "text_delta", contentIndex: 0, delta: "par" }, { type: "error", reason: "error", usage, errorMessage: "upstream failed", rewrite: { policyId: "p", policyVersion: 1, changed: false, tokenCountChange: 0, messageCountChange: 0, systemPromptChanged: false } }]))] },
		{ name: "pi/no-terminal", model: pModel(), context: simple(), options: { apiKey: "rk" }, responses: [ok(data([{ type: "start" }, { type: "text_start", contentIndex: 0 }]))] },
		{ name: "pi/missing-key", model: pModel(), context: simple(), options: {}, responses: [] },
		{ name: "pi/http-error-json", model: pModel(), context: simple(), options: { apiKey: "rk" }, responses: [json(403, { error: { message: "denied", code: "forbidden", details: { team: "a" } } })] },
		{ name: "pi/http-error-json-no-code", model: pModel(), context: simple(), options: { apiKey: "rk" }, responses: [json(429, { error: { message: 5 } })] },
		{ name: "pi/http-error-text", model: pModel(), context: simple(), options: { apiKey: "rk" }, responses: [json(502, "bad gateway ".repeat(1000), { "content-type": "text/plain" })] },
		{ name: "pi/http-error-array", model: pModel(), context: simple(), options: { apiKey: "rk" }, responses: [json(500, { error: ["x"] })] },
		{ name: "pi/network-error", model: pModel(), context: simple(), options: { apiKey: "rk" }, responses: [{ networkError: true }] },
		{ name: "pi/abort", model: pModel(), context: simple(), options: { apiKey: "rk" }, abortOn: { type: "text_delta", nth: 1 }, responses: [ok(data([{ type: "start" }, { type: "text_start", contentIndex: 0 }, { type: "text_delta", contentIndex: 0, delta: "a" }, { type: "text_delta", contentIndex: 0, delta: "b" }, { type: "done", reason: "stop", usage }]))] },
		{ name: "pi/simple", fn: "streamSimple", model: pModel(), context: simple(), options: { apiKey: "rk", reasoning: "low", toolChoice: "auto", maxTokens: 999999 }, responses: [ok(data([{ type: "done", reason: "length", usage }]))] },
	];
	const results = [];
	for (const scenario of scenarios) results.push({ ...(await run(api, scenario)), unorderedBody: true });
	stripStacks(results);
	return { file: "pi-messages.json", scenarios: results };
};

// ---------------------------------------------------------------------------------------------- Cloudflare (HTTP)

sections.cloudflare = async () => {
	const { cloudflareStreams } = await src("providers/cloudflare-stream.ts");
	// The API modules directly: the lazy wrappers re-push every event into an outer stream.
	const cf = await src("api/cloudflare.ts");
	const apis = {
		"openai-completions": cloudflareStreams(await src("api/openai-completions.ts")),
		"openai-responses": cloudflareStreams(await src("api/openai-responses.ts")),
		"anthropic-messages": cloudflareStreams(await src("api/anthropic-messages.ts")),
	};
	const chunk = (delta, finish = null) => ({ id: "c1", object: "chat.completion.chunk", choices: [{ index: 0, delta, finish_reason: finish }] });
	const workers = model("openai-completions", "cloudflare-workers-ai", cf.CLOUDFLARE_WORKERS_AI_BASE_URL, { id: "@cf/meta/llama", reasoning: false });
	const gateway = (api, base) => model(api, "cloudflare-ai-gateway", base, { id: "x/model" });
	const env = { CLOUDFLARE_ACCOUNT_ID: "acct", CLOUDFLARE_GATEWAY_ID: "gw" };
	const gatewayHeaders = { "cf-aig-authorization": "Bearer cf-key", Authorization: null, "x-api-key": null };
	const scenarios = [
		{ name: "cloudflare/workers-ai", model: workers, context: simple(), options: { apiKey: "cf-key", env }, responses: [ok(data([chunk({ content: "ok" }), chunk({}, "stop"), "[DONE]"]))] },
		{ name: "cloudflare/workers-ai-no-env", model: workers, context: simple(), options: { apiKey: "cf-key" }, responses: [ok(data([chunk({ content: "ok" }), chunk({}, "stop"), "[DONE]"]))] },
		{ name: "cloudflare/gateway-compat-missing-gateway", model: gateway("openai-completions", cf.CLOUDFLARE_AI_GATEWAY_COMPAT_BASE_URL), context: simple(), options: { headers: gatewayHeaders, env: { CLOUDFLARE_ACCOUNT_ID: "acct", CLOUDFLARE_GATEWAY_ID: "" } }, responses: [ok(data([chunk({ content: "ok" }), chunk({}, "stop"), "[DONE]"]))] },
		{ name: "cloudflare/gateway-openai-responses", model: gateway("openai-responses", cf.CLOUDFLARE_AI_GATEWAY_OPENAI_BASE_URL), context: simple(), options: { headers: gatewayHeaders, env }, responses: [ok(data(textOnly()))] },
		{ name: "cloudflare/gateway-anthropic", model: gateway("anthropic-messages", cf.CLOUDFLARE_AI_GATEWAY_ANTHROPIC_BASE_URL), context: simple(), options: { headers: gatewayHeaders, env }, responses: [json(400, { type: "error", error: { type: "invalid_request_error", message: "nope" } })] },
		{ name: "cloudflare/binding-route-simple", fn: "streamSimple", model: gateway("openai-completions", "https://workers-binding.ai/ai-gateway/gateways/gw/compat"), context: simple(), options: { headers: { "cf-aig-authorization": `Bearer ${(await src("api/cloudflare-ai-binding.ts")).CLOUDFLARE_GATEWAY_BINDING_AUTH_SENTINEL}`, Authorization: null }, env }, responses: [ok(data([chunk({ content: "ok" }), chunk({}, "stop"), "[DONE]"]))] },
	];
	const results = [];
	for (const scenario of scenarios) results.push({ ...(await run(apis[scenario.model.api], scenario)), wrapper: "cloudflare" });
	return { file: "cloudflare.json", scenarios: results };
};

// ---------------------------------------------------------------------------------------------- Bedrock Converse stream
// The AWS SDK sends through its NodeHttp2Handler, not fetch: its `handle` is replaced to record the signed request and
// return canned event-stream frames (encoded with the SDK's own EventStreamCodec). Credentials and profiles come from
// fixtures/aws (fake keys); headers that carry the SDK version, invocation ids or the wall clock are not recorded.
// The SigV4 signer is recorded separately with a fixed signing date.

const UNRECORDED_AWS_HEADERS = new Set(["authorization", "amz-sdk-invocation-id", "amz-sdk-request", "user-agent", "x-amz-user-agent", ":authority", "host", "content-length", "x-amz-date", "x-amz-content-sha256"]);

sections.bedrock = async () => {
	const { NodeHttp2Handler, NodeHttpHandler } = await import("@smithy/node-http-handler");
	const { HttpResponse } = await import("@smithy/core/transport");
	const { EventStreamCodec } = await import("@smithy/core/event-streams");
	const { SignatureV4 } = await import("@smithy/signature-v4");
	const { createHash, createHmac } = await import("node:crypto");
	const { Readable } = await import("node:stream");
	const encoder = new TextEncoder();
	const decoder = new TextDecoder();
	const codec = new EventStreamCodec((b) => decoder.decode(b), (t) => encoder.encode(t));
	const header = (value) => ({ type: "string", value });
	const frame = (headers, body) => Array.from(codec.encode({ headers, body: typeof body === "string" ? encoder.encode(body) : encoder.encode(JSON.stringify(body)) }));
	const event = (type, body) => frame({ ":event-type": header(type), ":message-type": header("event"), ":content-type": header("application/json") }, body);
	const exception = (type, body) => frame({ ":exception-type": header(type), ":message-type": header("exception"), ":content-type": header("application/json") }, body);
	const unmodeled = (code, message) => frame({ ":error-code": header(code), ":error-message": header(message), ":message-type": header("error") }, "");
	const stream = (frames) => ({ status: 200, headers: { "content-type": "application/vnd.amazon.eventstream", "x-amzn-requestid": "req-1" }, chunks: frames });
	const awsError = (status, body, headers = {}) => ({ status, headers: { "content-type": "application/json", "x-amzn-requestid": "req-err", ...headers }, chunks: [typeof body === "string" ? body : JSON.stringify(body)] });

	const fixtures = join(here, "aws");
	process.env.AWS_SHARED_CREDENTIALS_FILE = join(fixtures, "credentials");
	process.env.AWS_CONFIG_FILE = join(fixtures, "config");
	process.env.AWS_EC2_METADATA_DISABLED = "true";
	for (const name of ["AWS_PROFILE", "AWS_REGION", "AWS_DEFAULT_REGION", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_BEARER_TOKEN_BEDROCK", "AWS_BEDROCK_SKIP_AUTH", "AWS_BEDROCK_FORCE_CACHE", "AWS_BEDROCK_FORCE_HTTP1", "AWS_MAX_ATTEMPTS", "AWS_RETRY_MODE"]) delete process.env[name];
	const fileEnv = { AWS_SHARED_CREDENTIALS_FILE: "<fixtures>/aws/credentials", AWS_CONFIG_FILE: "<fixtures>/aws/config" };

	let current = null;
	const handle = async function (request, { abortSignal } = {}) {
		const headers = {};
		for (const [key, value] of Object.entries(request.headers)) if (!UNRECORDED_AWS_HEADERS.has(key.toLowerCase())) headers[key.toLowerCase()] = value;
		if (current.recordAuthorization) headers.authorization = request.headers.authorization;
		const query = new URLSearchParams(request.query ?? {}).toString();
		const bodyText = typeof request.body === "string" ? request.body : request.body ? decoder.decode(request.body) : null;
		current.requests.push({
			url: `${request.protocol}//${request.hostname}${request.port ? `:${request.port}` : ""}${request.path}${query ? `?${query}` : ""}`,
			method: request.method,
			headers: Object.fromEntries(Object.entries(headers).sort(([a], [b]) => (a < b ? -1 : 1))),
			body: bodyText === null ? null : JSON.parse(bodyText),
			signedHeaders: /SignedHeaders=([^,]+)/.exec(request.headers.authorization ?? "")?.[1] ?? null,
		});
		const canned = current.queue.shift();
		if (!canned) throw new Error("unexpected request");
		const chunks = canned.chunks.map((c) => Buffer.from(typeof c === "string" ? encoder.encode(c) : new Uint8Array(c)));
		async function* body() {
			for (const chunk of chunks) {
				if (abortSignal?.aborted) {
					const error = new Error("Request aborted");
					error.name = "AbortError";
					throw error;
				}
				yield chunk;
				await new Promise((resolve) => setImmediate(resolve));
			}
		}
		return { response: new HttpResponse({ statusCode: canned.status ?? 200, headers: canned.headers ?? {}, body: Readable.from(body()) }) };
	};
	NodeHttp2Handler.prototype.handle = handle;
	NodeHttpHandler.prototype.handle = handle;

	const api = await src("api/bedrock-converse-stream.ts");
	async function runBedrock(scenario) {
		const queue = scenario.responses.map((r) => ({ ...r, chunks: [...r.chunks] }));
		const controller = new AbortController();
		current = { queue, requests: [], recordAuthorization: scenario.recordAuthorization ?? false };
		for (const [name, value] of Object.entries(scenario.processEnv ?? {})) process.env[name] = value;
		recording = { events: [], abortOn: scenario.abortOn, seen: 0, controller };
		const options = JSON.parse(JSON.stringify(scenario.options).replaceAll("<fixtures>", here));
		const record = { name: scenario.name, fn: scenario.fn ?? "stream", model: scenario.model, context: scenario.context, options: clone(scenario.options), processEnv: scenario.processEnv ?? null, responses: scenario.responses, abortOn: scenario.abortOn ?? null };
		try {
			const s = api[record.fn](scenario.model, scenario.context, { ...options, signal: controller.signal });
			record.result = clone(await s.result());
			record.thrown = null;
		} catch (e) {
			record.result = null;
			record.thrown = e.message;
		}
		for (const name of Object.keys(scenario.processEnv ?? {})) delete process.env[name];
		record.events = recording.events;
		record.requests = current.requests;
		recording = null;
		return record;
	}

	const bModel = (id, extra = {}) => model("bedrock-converse-stream", "amazon-bedrock", "https://bedrock-runtime.us-east-1.amazonaws.com", { id, name: extra.name ?? id, maxTokens: 8192, ...extra });
	const sonnet = bModel("us.anthropic.claude-sonnet-4-5-20250929-v1:0", { name: "Claude Sonnet 4.5" });
	const opus46 = bModel("global.anthropic.claude-opus-4-6-v1", { name: "Claude Opus 4.6" });
	const opus47 = bModel("anthropic.claude-opus-4-7", { name: "Claude Opus 4.7" });
	const gptOss = bModel("openai.gpt-oss-120b-1:0", { name: "gpt-oss-120b" });
	const nova = bModel("amazon.nova-pro-v1:0", { name: "Nova Pro", reasoning: false });
	const keys = { ...fileEnv, AWS_ACCESS_KEY_ID: "AKIDEXAMPLE", AWS_SECRET_ACCESS_KEY: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY", AWS_REGION: "us-west-2" };
	const usage = { inputTokens: 100, outputTokens: 20, totalTokens: 150, cacheReadInputTokens: 10, cacheWriteInputTokens: 20, cacheDetails: [{ ttl: "1h", inputTokens: 15 }, { ttl: "5m", inputTokens: 5 }] };
	const textFrames = (text = "ok", stop = "end_turn") => [event("messageStart", { role: "assistant" }), event("contentBlockDelta", { contentBlockIndex: 0, delta: { text } }), event("contentBlockStop", { contentBlockIndex: 0 }), event("messageStop", { stopReason: stop }), event("metadata", { usage: { inputTokens: 5, outputTokens: 1, totalTokens: 6 }, metrics: { latencyMs: 3 } })];
	const full = [
		event("messageStart", { role: "assistant" }),
		event("contentBlockDelta", { contentBlockIndex: 0, delta: { reasoningContent: { text: "Let me " } } }),
		event("contentBlockDelta", { contentBlockIndex: 0, delta: { reasoningContent: { text: "think." } } }),
		event("contentBlockDelta", { contentBlockIndex: 0, delta: { reasoningContent: { signature: "sig-" } } }),
		event("contentBlockDelta", { contentBlockIndex: 0, delta: { reasoningContent: { signature: "abc" } } }),
		event("contentBlockStop", { contentBlockIndex: 0 }),
		event("contentBlockDelta", { contentBlockIndex: 1, delta: { text: "Hello " } }),
		event("contentBlockDelta", { contentBlockIndex: 1, delta: { text: "world 🌍" } }),
		event("contentBlockStop", { contentBlockIndex: 1 }),
		event("contentBlockStart", { contentBlockIndex: 2, start: { toolUse: { toolUseId: "tooluse_1", name: "read" } } }),
		event("contentBlockDelta", { contentBlockIndex: 2, delta: { toolUse: { input: '{"path": "src/' } } }),
		event("contentBlockDelta", { contentBlockIndex: 2, delta: { toolUse: { input: 'main.rs"}' } } }),
		event("contentBlockStop", { contentBlockIndex: 2 }),
		event("futureEvent", { anything: 1 }),
		event("messageStop", { stopReason: "tool_use" }),
		event("metadata", { usage, metrics: { latencyMs: 12 } }),
	];
	const hist = (m) => [
		system("You are helpful.", { toolsAdded: tools, sections: { rules: "<rules>be brief</rules>" } }),
		user("List files"),
		assistant(m, [
			{ type: "thinking", thinking: "I should read.", thinkingSignature: "sig-1" },
			{ type: "thinking", thinking: "Unsigned thought" },
			{ type: "thinking", thinking: "", thinkingSignature: "x", redacted: true },
			{ type: "thinking", thinking: "[Reasoning redacted]", thinkingSignature: "cmVkYWN0ZWQ=", redacted: true },
			{ type: "thinking", thinking: "  " },
			{ type: "text", text: "Reading." },
			{ type: "text", text: "   " },
			{ type: "toolCall", id: "call_1", name: "read", arguments: { path: "a.txt", "": "dropped", nested: { "": 1, ok: [1, { "": 2 }] } } },
			{ type: "toolCall", id: "call_2", name: "read", arguments: { path: "b.txt" } },
		], { stopReason: "toolUse" }),
		toolResult("call_1", "read", [{ type: "text", text: "contents of a" }, IMG]),
		toolResult("call_2", "read", [{ type: "text", text: "  " }], true),
		user([{ type: "text", text: "Now look at this" }, { ...IMG, mimeType: "image/jpeg" }, { type: "text", text: "" }]),
		assistant({ api: "anthropic-messages", provider: "anthropic", id: "claude-x" }, [
			{ type: "thinking", thinking: "Foreign thoughts", thinkingSignature: "x" },
			{ type: "toolCall", id: "call|weird+id/with=chars", name: "bash", arguments: { command: "ls" } },
		], { stopReason: "toolUse" }),
		assistant(m, [], { stopReason: "aborted" }),
		system("Mid-conversation rule"),
		user(""),
		user([]),
		user("Again"),
	];
	const scenarios = [
		{ name: "bedrock/claude-history-thinking-tools", model: sonnet, context: { messages: hist(sonnet) }, options: { env: keys, reasoning: "high", thinkingBudgets: { high: 4000 }, maxTokens: 6000, temperature: 0.3, toolChoice: "auto", requestMetadata: { team: "a" }, cacheRetention: "long", headers: { "X-Trace": "t", "x-amz-foo": "no", Authorization: "no", host: "no", "X-Null": null } }, responses: [stream(full)] },
		{ name: "bedrock/adaptive-xhigh", model: opus47, context: simple(), options: { env: keys, reasoning: "xhigh", toolChoice: { type: "tool", name: "bash" } }, responses: [stream(textFrames())] },
		{ name: "bedrock/adaptive-mapped-govcloud", model: { ...opus46, thinkingLevelMap: { medium: "low" } }, context: simple(), options: { env: { ...keys, AWS_REGION: "us-gov-west-1" }, reasoning: "medium", thinkingDisplay: "omitted", toolChoice: "none" }, responses: [stream(textFrames())] },
		{ name: "bedrock/budget-minimal-no-interleaved", model: sonnet, context: simple(), options: { env: keys, reasoning: "max", interleavedThinking: false, thinkingDisplay: "omitted", cacheRetention: "none" }, responses: [stream(textFrames("cut", "max_tokens"))] },
		{ name: "bedrock/non-claude-redacted-reasoning", model: gptOss, context: { messages: hist(gptOss) }, options: { env: { ...keys, AWS_SESSION_TOKEN: "session-token" }, reasoning: "low", toolChoice: "any" }, responses: [stream([event("messageStart", { role: "assistant" }), event("contentBlockDelta", { contentBlockIndex: 0, delta: { reasoningContent: { text: "visible" } } }), event("contentBlockDelta", { contentBlockIndex: 0, delta: { reasoningContent: { redactedContent: "AAEC" } } }), event("contentBlockDelta", { contentBlockIndex: 0, delta: { reasoningContent: { signature: "ignored", redactedContent: "AwQF" } } }), event("contentBlockDelta", { contentBlockIndex: 1, delta: { text: "answer" } }), event("messageStop", { stopReason: "model_context_window_exceeded" }), event("metadata", { usage: { inputTokens: 7, outputTokens: 3 } })])] },
		{ name: "bedrock/app-profile-arn-force-cache", model: bModel("arn:aws:bedrock:eu-central-1:123456789012:application-inference-profile/abc123", { name: "my profile" }), context: simple(), options: { env: { ...keys, AWS_BEDROCK_FORCE_CACHE: "1" } }, responses: [stream(textFrames())] },
		{ name: "bedrock/arn-claude-name-cache", model: bModel("arn:aws:bedrock:us-east-2:123456789012:application-inference-profile/xyz", { name: "anthropic.claude-haiku-4-5" }), context: simple(), options: { env: { ...keys, PI_CACHE_RETENTION: "long" } }, responses: [stream(textFrames())] },
		{ name: "bedrock/bearer-token", model: nova, context: simple(), options: { apiKey: "bedrock-api-key", env: fileEnv }, recordAuthorization: true, responses: [stream(textFrames())] },
		{ name: "bedrock/bearer-token-env", model: nova, context: simple(), options: { env: { ...fileEnv, AWS_BEARER_TOKEN_BEDROCK: "env-token", AWS_REGION: "eu-west-3" } }, recordAuthorization: true, responses: [stream(textFrames())] },
		{ name: "bedrock/skip-auth-custom-endpoint", model: bModel("meta.llama3", { baseUrl: "https://bedrock.proxy.example.com/v1", reasoning: false }), context: simple(), options: { apiKey: "ignored", env: { ...fileEnv, AWS_BEDROCK_SKIP_AUTH: "1" } }, responses: [stream(textFrames())] },
		{ name: "bedrock/standard-endpoint-region", model: { ...nova, baseUrl: "https://bedrock-runtime.ap-northeast-1.amazonaws.com" }, context: simple(), options: { env: { ...fileEnv, AWS_ACCESS_KEY_ID: "AKIDEXAMPLE", AWS_SECRET_ACCESS_KEY: "secret" } }, responses: [stream(textFrames())] },
		{ name: "bedrock/explicit-region-option", model: nova, context: simple(), options: { region: "ca-central-1", env: keys }, responses: [stream(textFrames())] },
		{ name: "bedrock/profile-credentials", model: nova, context: simple(), options: { profile: "dev", env: { ...fileEnv, AWS_ACCESS_KEY_ID: "AKIDIGNORED", AWS_SECRET_ACCESS_KEY: "ignored" } }, responses: [stream(textFrames())] },
		{ name: "bedrock/default-profile-region-us-east-1", model: nova, context: simple(), options: { env: fileEnv }, responses: [stream(textFrames())] },
		{ name: "bedrock/stop-guardrail", model: nova, context: simple(), options: { env: keys }, responses: [stream(textFrames("x", "guardrail_intervened"))] },
		{ name: "bedrock/stop-missing", model: nova, context: simple(), options: { env: keys }, responses: [stream([event("messageStart", { role: "assistant" }), event("contentBlockDelta", { contentBlockIndex: 0, delta: { text: "x" } })])] },
		{ name: "bedrock/user-message-start", model: nova, context: simple(), options: { env: keys }, responses: [stream([event("messageStart", { role: "user" })])] },
		{ name: "bedrock/stream-exception", model: nova, context: simple(), options: { env: keys }, responses: [stream([event("messageStart", { role: "assistant" }), event("contentBlockDelta", { contentBlockIndex: 0, delta: { text: "par" } }), exception("throttlingException", { message: "Too many tokens, please wait" })])] },
		{ name: "bedrock/stream-internal-exception", model: nova, context: simple(), options: { env: keys }, responses: [stream([event("messageStart", { role: "assistant" }), exception("internalServerException", { message: "boom" })])] },
		{ name: "bedrock/stream-validation-exception", model: nova, context: simple(), options: { env: keys }, responses: [stream([exception("validationException", { message: "data retention mode 'default' is not available" })])] },
		{ name: "bedrock/stream-unknown-exception", model: nova, context: simple(), options: { env: keys }, responses: [stream([exception("weirdException", { message: "odd" })])] },
		{ name: "bedrock/stream-unmodeled-error", model: nova, context: simple(), options: { env: keys }, responses: [stream([unmodeled("InternalFailure", "The request processing has failed")])] },
		{ name: "bedrock/stream-throttling-first-frame", model: nova, context: simple(), options: { env: keys }, responses: [stream([exception("throttlingException", { message: "slow first" })]), stream([exception("throttlingException", { message: "slow second" })]), stream(textFrames("third time"))] },
		{ name: "bedrock/stream-unknown-exception-later", model: nova, context: simple(), options: { env: keys }, responses: [stream([event("messageStart", { role: "assistant" }), exception("weirdException", { message: "odd later" })])] },
		{ name: "bedrock/stream-unmodeled-error-later", model: nova, context: simple(), options: { env: keys }, responses: [stream([event("messageStart", { role: "assistant" }), unmodeled("Overloaded", "")])] },
		{ name: "bedrock/stream-model-stream-error", model: nova, context: simple(), options: { env: keys }, responses: [stream([event("messageStart", { role: "assistant" }), exception("modelStreamErrorException", { message: "model broke", originalStatusCode: 500 })])] },
		{ name: "bedrock/http-code-field", model: nova, context: simple(), options: { env: keys }, responses: [awsError(424, { Code: "ModelErrorException:extra", message: "model failed", originalStatusCode: 500 })] },
		{ name: "bedrock/http-type-hash-unknown", model: nova, context: simple(), options: { env: keys }, responses: [awsError(400, { __type: "aws.protocols#FooException", Message: "foo" })] },
		{ name: "bedrock/http-null-body", model: nova, context: simple(), options: { env: keys }, responses: [awsError(400, "null")] },
		{ name: "bedrock/http-bad-json", model: nova, context: simple(), options: { env: keys }, responses: [awsError(500, '{"message": "trunc', { "x-amzn-errortype": "InternalServerException" }), awsError(500, '{"message": "trunc', { "x-amzn-errortype": "InternalServerException" }), awsError(500, '{"message": "trunc', { "x-amzn-errortype": "InternalServerException" })] },
		{ name: "bedrock/truncated-frame", model: nova, context: simple(), options: { env: keys }, responses: [stream([event("messageStart", { role: "assistant" }), event("contentBlockDelta", { contentBlockIndex: 0, delta: { text: "x" } }).slice(0, 30)])] },
		{ name: "bedrock/bad-message-crc", model: nova, context: simple(), options: { env: keys }, responses: [stream([event("messageStart", { role: "assistant" }), (() => { const f = event("contentBlockDelta", { contentBlockIndex: 0, delta: { text: "x" } }); f[f.length - 1] ^= 0xff; return f; })()])] },
		{ name: "bedrock/missing-profile", model: nova, context: simple(), options: { profile: "nobody", env: fileEnv }, responses: [] },
		{ name: "bedrock/http-validation", model: nova, context: simple(), options: { env: keys }, responses: [awsError(400, { message: "Malformed input request" }, { "x-amzn-errortype": "ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/" })] },
		{ name: "bedrock/http-access-denied-type", model: nova, context: simple(), options: { env: keys }, responses: [awsError(403, { __type: "com.amazon.coral#AccessDeniedException", Message: "You don't have access" })] },
		{ name: "bedrock/http-not-found", model: nova, context: simple(), options: { env: keys }, responses: [awsError(404, { message: "model not found" }, { "x-amzn-errortype": "ResourceNotFoundException" })] },
		{ name: "bedrock/http-html-403", model: nova, context: simple(), options: { env: keys }, responses: [awsError(403, "<html>Forbidden by gateway</html>", { "content-type": "text/html" })] },
		{ name: "bedrock/http-empty-401", model: nova, context: simple(), options: { env: keys }, responses: [awsError(401, "", { "content-type": "text/plain" })] },
		{ name: "bedrock/http-throttling-retried", model: nova, context: simple(), options: { env: keys }, responses: [awsError(429, { message: "Rate exceeded" }, { "x-amzn-errortype": "ThrottlingException" }), awsError(429, { message: "Rate exceeded" }, { "x-amzn-errortype": "ThrottlingException" }), awsError(429, { message: "Rate exceeded again" }, { "x-amzn-errortype": "ThrottlingException" })] },
		{ name: "bedrock/http-503-then-ok", model: nova, context: simple(), options: { env: keys }, responses: [awsError(503, { message: "busy" }, { "x-amzn-errortype": "ServiceUnavailableException" }), stream(textFrames("recovered"))] },
		{ name: "bedrock/split-frames", model: nova, context: simple(), options: { env: keys }, responses: [stream((() => { const all = textFrames("split").flat(); return [all.slice(0, 7), all.slice(7, 40), all.slice(40)]; })())] },
		{ name: "bedrock/abort-mid-stream", model: nova, context: simple(), options: { env: keys }, abortOn: { type: "text_delta", nth: 1 }, responses: [stream([event("messageStart", { role: "assistant" }), event("contentBlockDelta", { contentBlockIndex: 0, delta: { text: "one" } }), event("contentBlockDelta", { contentBlockIndex: 0, delta: { text: "two" } }), event("contentBlockStop", { contentBlockIndex: 0 }), event("messageStop", { stopReason: "end_turn" })])] },
		{ name: "bedrock/unknown-image-type", model: nova, context: { messages: [user([{ type: "image", data: "aGk=", mimeType: "image/bmp" }])] }, options: { env: keys }, responses: [] },
		{ name: "bedrock/non-claude-thinking-replay", model: gptOss, context: { messages: [user("q"), assistant(gptOss, [{ type: "thinking", thinking: "t", thinkingSignature: "s" }, { type: "text", text: "a" }]), user("q2")] }, options: { env: keys }, responses: [stream(textFrames())] },
		{ name: "bedrock/simple-budget", fn: "streamSimple", model: sonnet, context: simple(), options: { env: keys, reasoning: "medium", maxTokens: 2000, thinkingBudgets: { low: 10 } }, responses: [stream(textFrames())] },
		{ name: "bedrock/simple-budget-default-max", fn: "streamSimple", model: { ...sonnet, contextWindow: 9000 }, context: simple(), options: { env: keys, reasoning: "xhigh" }, responses: [stream(textFrames())] },
		{ name: "bedrock/simple-adaptive", fn: "streamSimple", model: opus46, context: simple(), options: { env: keys, reasoning: "high", toolChoice: "auto" }, responses: [stream(textFrames())] },
		{ name: "bedrock/simple-non-claude", fn: "streamSimple", model: gptOss, context: simple(), options: { env: keys, reasoning: "low" }, responses: [stream(textFrames())] },
		{ name: "bedrock/simple-no-reasoning", fn: "streamSimple", model: sonnet, context: simple(), options: { env: keys, maxTokens: 100 }, responses: [stream(textFrames())] },
	];
	const results = [];
	for (const scenario of scenarios) results.push(await runBedrock(scenario));
	stripStacks(results);

	// SigV4 with a fixed date, through the SDK's signer (node:crypto as its SHA-256).
	class Sha256 {
		constructor(secret) {
			this.hash = secret ? createHmac("sha256", secret) : createHash("sha256");
		}
		update(data) {
			this.hash.update(data);
		}
		async digest() {
			return new Uint8Array(this.hash.digest());
		}
	}
	const signingCases = [
		{ credentials: { accessKeyId: "AKIDEXAMPLE", secretAccessKey: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY" }, region: "us-east-1", date: "2026-01-01T00:00:00Z", request: { method: "POST", hostname: "bedrock-runtime.us-east-1.amazonaws.com", path: "/model/us.anthropic.claude-sonnet-4-5%3A0/converse-stream", query: {}, headers: { host: "bedrock-runtime.us-east-1.amazonaws.com", "content-type": "application/json", "X-Custom": "  a   b  " }, body: '{"messages":[]}' } },
		{ credentials: { accessKeyId: "AKID2", secretAccessKey: "secret2", sessionToken: "tok/en+=" }, region: "eu-west-1", date: "2026-10-11T23:59:59Z", request: { method: "POST", hostname: "proxy.example.com", path: "/base//./x/../model/a b/converse-stream", query: { b: "2", a: "1 1", "x*": ["z", "y"] }, headers: { host: "proxy.example.com", "user-agent": "skip", "proxy-thing": "skip", "x-amz-meta": "kept" }, body: "" } },
	];
	const signing = [];
	for (const c of signingCases) {
		const signer = new SignatureV4({ credentials: c.credentials, region: c.region, service: "bedrock", sha256: Sha256 });
		const signed = await signer.sign({ protocol: "https:", ...c.request, headers: { ...c.request.headers } }, { signingDate: new Date(c.date) });
		signing.push({ ...c, signed: Object.fromEntries(Object.entries(signed.headers).sort(([a], [b]) => (a < b ? -1 : 1))) });
	}
	const jsonInputs = ["<html>Forbidden by gateway</html>", "<x>", "", "   ", "{", '{"a":1} x', '{"message": "trunc', "nul", "[1,]", '{"a" 1}', "tru", "{'a':1}", "12abc", "\u00e9t\u00e9 is long enough text", '{"a":"b"}}'];
	const jsonErrors = jsonInputs.map((input) => {
		try {
			JSON.parse(input);
			return { input, message: null };
		} catch (e) {
			return { input, message: e.message };
		}
	});
	return { file: "bedrock.json", signing, jsonErrors, scenarios: results };
};

// ---------------------------------------------------------------------------------------------- Claude Code CLI
// The provider spawns `claude`; fixtures/fake-claude/claude stands in for it (never the real CLI). Each scenario writes
// the fake's replies to a private temp dir, runs the TS provider, and records the events, the result and the fake's
// log (argv, selected env, the stdin message). Warm spares are off (ULTRON_CLAUDE_CODE_WARM=0) so the log is exact.

sections.claude = async () => {
	const { mkdtempSync, readFileSync, rmSync, existsSync } = await import("node:fs");
	const { tmpdir } = await import("node:os");
	const api = await src("api/claude-code-cli.ts");
	api.resetClaudeCodeCliState();
	const bin = join(here, "fake-claude", "claude");
	const ccModel = (extra = {}) => ({
		id: "claude-opus-5-5", name: "Claude Opus 5.5 (Claude Code CLI)", api: "claude-code-cli", provider: "claude-code", baseUrl: "claude-code://cli", reasoning: true,
		thinkingLevelMap: { minimal: "low", low: "low", medium: "medium", high: "high", xhigh: "xhigh", max: "max" },
		input: ["text", "image"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 200000, maxTokens: 32000, ...extra,
	});
	const lines = (...events) => events.map((e) => (typeof e === "string" ? e : JSON.stringify(e))).join("\n") + "\n";
	const init = (extra = {}) => ({ type: "system", subtype: "init", apiKeySource: "none", ...extra });
	const se = (event) => ({ type: "stream_event", event });
	const result = (extra = {}) => ({ type: "result", subtype: "success", is_error: false, result: "", stop_reason: "end_turn", total_cost_usd: 0.0123, usage: { input_tokens: 10, output_tokens: 5, cache_read_input_tokens: 2, cache_creation_input_tokens: 3, cache_creation: { ephemeral_1h_input_tokens: 1 }, output_tokens_details: { thinking_tokens: 4 } }, ...extra });
	const streamed = [
		init(),
		se({ type: "message_start", message: { id: "msg_1", model: "claude-opus-5-5-20260101" } }),
		se({ type: "content_block_start", index: 0, content_block: { type: "thinking", thinking: "" } }),
		se({ type: "content_block_delta", index: 0, delta: { type: "thinking_delta", thinking: "Hmm" } }),
		se({ type: "content_block_stop", index: 0 }),
		se({ type: "content_block_start", index: 1, content_block: { type: "text", text: "" } }),
		se({ type: "content_block_delta", index: 1, delta: { type: "text_delta", text: "Hello " } }),
		se({ type: "content_block_delta", index: 1, delta: { type: "text_delta", text: "🌍" } }),
		se({ type: "content_block_stop", index: 1 }),
		{ type: "assistant", message: { id: "msg_1", model: "x", content: [{ type: "text", text: "dup" }] } },
		"not json",
		"{broken",
		result({ result: "Hello 🌍" }),
	];
	const base = { ULTRON_CLAUDE_CODE_BIN: bin, ULTRON_CLAUDE_CODE_WARM: "0", CLAUDECODE: "1", CLAUDE_CODE_SESSION_ID: "parent" };
	const conversation = { messages: [system("Be terse.", { sections: { s: "Extra rule." } }), user("first"), assistant(ccModel(), [{ type: "text", text: "reply" }, { type: "toolCall", id: "t1", name: "read", arguments: { path: "a" } }]), toolResult("t1", "read", [{ type: "text", text: "data" }, IMG], true), user([{ type: "text", text: "now " }, { type: "text", text: "this" }, IMG])] };
	const scenarios = [
		{ name: "claude/streamed-text-and-thinking", model: ccModel(), context: { messages: [system("Be terse."), user([{ type: "text", text: "hi " }, { type: "text", text: "there" }, IMG])] }, options: { reasoning: "high", maxTokens: 1000 }, replies: { "": lines(...streamed) } },
		{ name: "claude/conversation-transcript-text-only-model", model: ccModel({ input: ["text"], maxTokens: 500 }), context: conversation, options: { maxTokens: 9999 }, replies: { "": lines(init({ apiKeySource: "ANTHROPIC_API_KEY" }), { type: "assistant", message: { id: "m2", content: [{ type: "text", text: "From assistant event" }, { type: "tool_use" }] } }, result({ result: "ignored", stop_reason: "max_tokens" })) } },
		{ name: "claude/result-only-text", model: ccModel(), context: { messages: [user("")] }, options: {}, replies: { "": lines(result({ result: "Only result", total_cost_usd: undefined, usage: {} })) } },
		{ name: "claude/synthetic-assistant-skipped", model: ccModel(), context: { messages: [user("q")] }, options: { reasoning: "off" }, replies: { "": lines({ type: "assistant", message: { model: "<synthetic>", content: [{ type: "text", text: "nope" }] } }, result({ result: "" })) } },
		{ name: "claude/json-schema-structured", model: ccModel(), context: { messages: [user("q")] }, options: { jsonSchema: { type: "object", properties: { a: { type: "number" } } } }, replies: { "": lines(se({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }), se({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "raw" } }), result({ structured_output: { a: 1 }, result: "x" })) } },
		{ name: "claude/json-schema-rejected-falls-back", model: ccModel(), context: { messages: [user("q")] }, options: { jsonSchema: { type: "object" } }, replies: { "": lines(result({ is_error: true, api_error_status: 400, result: "tools.0.input_schema: invalid" })), "2": lines(result({ result: "{\"ok\":true}" })) } },
		{ name: "claude/json-schema-not-object-in-prompt", model: ccModel(), context: { messages: [user("q")] }, options: { jsonSchema: { type: "array" } }, replies: { "": lines(result({ result: "[]" })) } },
		{ name: "claude/usage-limit", model: ccModel(), context: { messages: [user("q")] }, options: {}, replies: { "": lines({ type: "rate_limit_event", rate_limit_info: { status: "rejected", rateLimitType: "five_hour", resetsAt: 1767240000 } }, result({ is_error: true, result: "Claude AI usage limit reached" })) } },
		{ name: "claude/near-limit-warning", model: ccModel(), context: { messages: [user("q")] }, options: {}, replies: { "": lines({ type: "rate_limit_event", rate_limit_info: { status: "allowed_warning", rateLimitType: "seven_day", resetsAt: 1767240000000 } }, { type: "rate_limit_event", rate_limit_info: { status: "allowed_warning", rateLimitType: "seven_day", resetsAt: 1767240000000 } }, { type: "rate_limit_event" }, result({ result: "ok" })) } },
		{ name: "claude/rate-limited", model: ccModel(), context: { messages: [user("q")] }, options: {}, replies: { "": lines(result({ is_error: true, api_error_status: 429, result: "slow down" })) } },
		{ name: "claude/overloaded", model: ccModel(), context: { messages: [user("q")] }, options: {}, replies: { "": lines(result({ is_error: true, result: "API Error: Overloaded" })) } },
		{ name: "claude/auth-failed", model: ccModel(), context: { messages: [user("q")] }, options: {}, replies: { "": lines(result({ is_error: true, result: "Invalid API key · Please run /login" })) } },
		{ name: "claude/generic-error", model: ccModel(), context: { messages: [user("q")] }, options: {}, replies: { "": lines(result({ is_error: true, api_error_status: 500, result: "boom" })) } },
		{ name: "claude/output-limit-is-length", model: ccModel(), context: { messages: [user("q")] }, options: {}, replies: { "": lines(se({ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } }), se({ type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "part" } }), result({ is_error: true, result: "Claude's response exceeded the 32000 output token maximum." })) } },
		{ name: "claude/exit-without-result", model: ccModel(), context: { messages: [user("q")] }, options: { env: { FAKE_CLAUDE_EXIT: "3", FAKE_CLAUDE_STDERR: "something broke" } }, replies: { "": lines(init()) } },
		{ name: "claude/unknown-option", model: ccModel(), context: { messages: [user("q")] }, options: { env: { FAKE_CLAUDE_EXIT: "1", FAKE_CLAUDE_STDERR: "error: unknown option '--include-partial-messages'" } }, replies: { "": "" } },
		{ name: "claude/tools-declared", model: ccModel(), context: simple(), options: {}, replies: {} },
		{ name: "claude/missing-optional-flag", model: ccModel(), context: { messages: [user("q")] }, options: { reasoning: "low", env: { FAKE_CLAUDE_NO_FLAGS: "--effort", CLAUDE_CONFIG_DIR: "/cfg/noeffort" } }, replies: {} },
		{ name: "claude/missing-required-flag", model: ccModel(), context: { messages: [user("q")] }, options: { env: { FAKE_CLAUDE_NO_FLAGS: "--tools --settings", CLAUDE_CONFIG_DIR: "/cfg/old" } }, replies: {} },
		{ name: "claude/not-logged-in", model: ccModel(), context: { messages: [user("q")] }, options: { env: { FAKE_CLAUDE_AUTH: '{"loggedIn":false}', CLAUDE_CONFIG_DIR: "/cfg/out" } }, replies: {} },
		{ name: "claude/no-auth-status", model: ccModel(), context: { messages: [user("q")] }, options: { env: { FAKE_CLAUDE_AUTH: "garbage", CLAUDE_CONFIG_DIR: "/cfg/garbage" } }, replies: {} },
		{ name: "claude/third-party-refused", model: ccModel(), context: { messages: [user("q")] }, options: { env: { FAKE_CLAUDE_AUTH: '{"loggedIn":true,"apiProvider":"bedrock"}', CLAUDE_CONFIG_DIR: "/cfg/bedrock" } }, replies: {} },
		{ name: "claude/third-party-allowed-api-key", model: ccModel(), context: { messages: [user("q")] }, options: { env: { FAKE_CLAUDE_AUTH: '{"loggedIn":true,"apiProvider":"vertex","authMethod":"api_key"}', CLAUDE_CONFIG_DIR: "/cfg/vertex", ULTRON_CLAUDE_CODE_ALLOW_THIRD_PARTY: "1" } }, replies: { "": lines(result({ result: "ok" })) } },
		{ name: "claude/bin-not-executable", model: ccModel(), context: { messages: [user("q")] }, options: { env: { ULTRON_CLAUDE_CODE_BIN: "/nonexistent/claude" } }, replies: {} },
		{ name: "claude/long-system-prompt-in-message", model: ccModel(), context: { messages: [system("S".repeat(100001)), user("q")] }, options: { reasoning: "medium", env: { FAKE_EXTRA: "x" } }, replies: { "": lines(result({ result: "ok" })) } },
	];
	const results = [];
	for (const scenario of scenarios) {
		const dir = mkdtempSync(join(tmpdir(), "fake-claude-"));
		for (const [suffix, text] of Object.entries(scenario.replies)) writeFileSync(join(dir, suffix ? `reply.${suffix}` : "reply"), text);
		const log = join(dir, "log");
		writeFileSync(log, "");
		const env = { ...base, FAKE_CLAUDE_REPLY: join(dir, "reply"), FAKE_CLAUDE_LOG: log, ...(scenario.options.env ?? {}) };
		const record = { name: scenario.name, fn: "streamSimple", model: scenario.model, context: scenario.context, options: clone({ ...scenario.options, env: undefined }), env: Object.fromEntries(Object.entries(scenario.options.env ?? {})), replies: scenario.replies };
		recording = { events: [], seen: 0 };
		try {
			const s = api.streamSimple(scenario.model, scenario.context, { ...scenario.options, env });
			record.result = clone(await s.result());
			record.thrown = null;
		} catch (e) {
			record.result = null;
			record.thrown = e.message;
		}
		record.events = recording.events;
		recording = null;
		record.cli = existsSync(log) ? readFileSync(log, "utf8").replaceAll(dir, "<dir>").replaceAll(here, "<fixtures>") : "";
		const scrub = (v) => JSON.parse(JSON.stringify(v).replaceAll(here, "<fixtures>"));
		record.result = scrub(record.result);
		record.events = scrub(record.events);
		rmSync(dir, { recursive: true, force: true });
		results.push(record);
	}
	const units = {
		parseHelpFlags: ["  -p, --print  Print\n  --json-schema <s>,--Effort x", "--a-b --9x --c1"].map((help) => ({ help, flags: [...api.parseHelpFlags(help)] })),
		usage: [result(), { usage: { input_tokens: -1, output_tokens: "x" }, total_cost_usd: -1 }, {}].map((r) => ({ result: r, usage: clone(api.usageFromResult(r)) })),
		reset: [undefined, { resetsAt: 0 }, { resetsAt: 1767240000 }, { resetsAt: 1767240000123 }].map((info) => ({ info: info ?? null, text: api.resetText(info) })),
	};
	return { file: "claude-code.json", units, scenarios: results };
};

// ---------------------------------------------------------------------------------------------- Models / auth
// A scripted sequence of Models operations over scripted providers (defined identically in tests/providers_rest.rs):
// each step records its result or ModelsError. Clock: NOW. The file store is checked against coding-agent's
// AuthStorage writing the same credentials.

sections.auth = async () => {
	const { createModels, createProvider } = await src("models.ts");
	const { InMemoryCredentialStore } = await src("auth/credential-store.ts");
	const { envApiKeyAuth } = await src("auth/helpers.ts");
	const { cloudflareWorkersAIAuth, cloudflareAIGatewayAuth } = await src("providers/cloudflare-auth.ts");
	const { AssistantMessageEventStream: Stream } = await src("utils/event-stream.ts");
	let env = {};
	const authContext = { env: async (name) => (env[name]?.trim() ? env[name] : undefined), fileExists: async () => false };
	const store = new InMemoryCredentialStore();
	const models = createModels({ credentials: store, authContext });
	const refreshes = {};
	const fakeOAuth = (id, fail = false) => ({
		name: `${id} OAuth`,
		login: async (interaction) => {
			const code = await interaction.prompt({ type: "text", message: "Code?" });
			interaction.notify({ type: "progress", message: `got ${code}` });
			return { type: "oauth", refresh: `r-${code}`, access: `a-${code}`, expires: NOW + 3600000 };
		},
		refresh: async (credential) => {
			refreshes[id] = (refreshes[id] ?? 0) + 1;
			if (fail) throw new Error("invalid_grant: token revoked");
			return { type: "oauth", refresh: `${credential.refresh}+`, access: `fresh-${refreshes[id]}`, expires: NOW + 3600000 };
		},
		toAuth: async (credential) => ({ apiKey: credential.access, headers: { "X-Auth": "a", Authorization: `Bearer ${credential.access}` }, ...(id === "oauth-prov" ? { baseUrl: "https://per-credential.example.com" } : {}) }),
	});
	const echo = { stream: () => { throw new Error("unused"); }, streamSimple: (model, context, options) => { const s = new Stream(); const message = { role: "assistant", content: [], api: model.api, provider: model.provider, model: model.id, usage: usage0, stopReason: "error", errorMessage: JSON.stringify({ baseUrl: model.baseUrl, apiKey: options.apiKey ?? null, headers: options.headers ?? null, env: options.env ?? null }), timestamp: NOW }; queueMicrotask(() => { s.push({ type: "error", reason: "error", error: message }); s.end(message); }); return s; } };
	const pm = (provider, id, extra = {}) => model("echo", provider, "https://default.example.com", { id, ...extra });
	const providers = [
		{ id: "env-prov", auth: { apiKey: envApiKeyAuth("Env key", ["ENV_KEY_A", "ENV_KEY_B"]) }, models: [pm("env-prov", "e1", { headers: { "X-Model": "1", authorization: "model-level" } }), pm("env-prov", "e2")] },
		{ id: "oauth-prov", auth: { oauth: fakeOAuth("oauth-prov") }, models: [pm("oauth-prov", "o1")] },
		{ id: "fail-prov", auth: { oauth: fakeOAuth("fail-prov", true) }, models: [pm("fail-prov", "f1")] },
		{ id: "both-prov", auth: { apiKey: envApiKeyAuth("Both key", ["BOTH_KEY"]), oauth: fakeOAuth("both-prov") }, models: [pm("both-prov", "b1")], filterModels: (list, credential) => list.filter((m) => credential?.type !== "oauth" || m.id !== "b-api-only") },
		{ id: "check-prov", auth: { apiKey: { name: "Checked", check: async ({ credential }) => ({ source: credential ? "checked stored" : "checked ambient", type: "api_key" }), resolve: async () => { throw new Error("resolve must not run during checks"); } } }, models: [pm("check-prov", "c1")] },
		{ id: "throw-prov", auth: { apiKey: { name: "Thrower", resolve: async () => { throw new Error("keychain locked"); } } }, models: [pm("throw-prov", "t1")] },
		{ id: "dup-prov", auth: { apiKey: { name: "Dup", resolve: async () => { throw new Error("provider dup-prov"); } } }, models: [] },
		{ id: "blank-prov", auth: { apiKey: { name: "Blank", resolve: async () => { throw new Error("   "); } } }, models: [] },
		{ id: "cloudflare-workers-ai", auth: { apiKey: cloudflareWorkersAIAuth() }, models: [pm("cloudflare-workers-ai", "cf1")] },
		{ id: "cloudflare-ai-gateway", auth: { apiKey: cloudflareAIGatewayAuth() }, models: [pm("cloudflare-ai-gateway", "gw1")] },
	];
	for (const p of providers) models.setProvider(createProvider({ ...p, api: echo }));
	const script = [
		{ op: "getAuth", provider: "env-prov" },
		{ op: "checkAuth", provider: "env-prov" },
		{ op: "getAvailable" },
		{ op: "setEnv", env: { ENV_KEY_B: "env-b", ENV_KEY_A: "  ", CLOUDFLARE_API_KEY: "cf-env", CLOUDFLARE_ACCOUNT_ID: "acct-env" } },
		{ op: "getAuth", provider: "env-prov" },
		{ op: "getAuth", model: { provider: "env-prov", id: "e1" } },
		{ op: "getAuth", provider: "env-prov", overrides: { apiKey: "override-key", env: { X: "1" } } },
		{ op: "getAuth", provider: "unknown" },
		{ op: "checkAuth", provider: "unknown" },
		{ op: "getAvailable" },
		{ op: "getAvailable", provider: "check-prov" },
		{ op: "store", provider: "env-prov", credential: { type: "api_key", key: "stored-key", env: { REGION: "eu" } } },
		{ op: "getAuth", provider: "env-prov", overrides: { env: { REGION: "us", EXTRA: "x" } } },
		{ op: "store", provider: "env-prov", credential: { type: "oauth", refresh: "r", access: "a", expires: NOW + 3600000 } },
		{ op: "getAuth", provider: "env-prov" },
		{ op: "checkAuth", provider: "env-prov" },
		{ op: "getAuth", provider: "oauth-prov" },
		{ op: "store", provider: "oauth-prov", credential: { type: "oauth", refresh: "r0", access: "a0", expires: NOW + 3600000 } },
		{ op: "getAuth", provider: "oauth-prov" },
		{ op: "refreshes", provider: "oauth-prov" },
		{ op: "store", provider: "oauth-prov", credential: { type: "oauth", refresh: "r1", access: "a1", expires: NOW + 60000 } },
		{ op: "concurrentGetAuth", provider: "oauth-prov", n: 3 },
		{ op: "refreshes", provider: "oauth-prov" },
		{ op: "read", provider: "oauth-prov" },
		{ op: "getAuth", provider: "oauth-prov", overrides: { minOAuthValidityMs: 7200000 } },
		{ op: "refreshes", provider: "oauth-prov" },
		{ op: "store", provider: "fail-prov", credential: { type: "oauth", refresh: "rf", access: "af", expires: NOW - 1 } },
		{ op: "getAuth", provider: "fail-prov" },
		{ op: "read", provider: "fail-prov" },
		{ op: "checkAuth", provider: "fail-prov" },
		{ op: "getAuth", provider: "throw-prov" },
		{ op: "checkAuth", provider: "throw-prov" },
		{ op: "getAuth", provider: "dup-prov" },
		{ op: "getAuth", provider: "blank-prov" },
		{ op: "getAvailable", provider: "throw-prov" },
		{ op: "checkAuth", provider: "check-prov" },
		{ op: "store", provider: "check-prov", credential: { type: "api_key", key: "k" } },
		{ op: "checkAuth", provider: "check-prov" },
		{ op: "login", provider: "oauth-prov", type: "oauth", answers: ["xyz"] },
		{ op: "read", provider: "oauth-prov" },
		{ op: "login", provider: "env-prov", type: "api_key", answers: ["typed-key"] },
		{ op: "read", provider: "env-prov" },
		{ op: "login", provider: "env-prov", type: "oauth", answers: [] },
		{ op: "login", provider: "nope", type: "oauth", answers: [] },
		{ op: "login", provider: "cloudflare-ai-gateway", type: "api_key", answers: ["cf-key", "acct", "gw"] },
		{ op: "getAuth", provider: "cloudflare-ai-gateway" },
		{ op: "getAuth", provider: "cloudflare-workers-ai" },
		{ op: "store", provider: "cloudflare-workers-ai", credential: { type: "api_key", key: "cf-stored" } },
		{ op: "getAuth", provider: "cloudflare-workers-ai" },
		{ op: "store", provider: "both-prov", credential: { type: "oauth", refresh: "rb", access: "ab", expires: NOW + 3600000 } },
		{ op: "getAvailable", provider: "both-prov" },
		{ op: "list" },
		{ op: "logout", provider: "env-prov" },
		{ op: "read", provider: "env-prov" },
		{ op: "stream", model: { provider: "env-prov", id: "e1" }, options: { headers: { AUTHORIZATION: "request", "X-Req": "r" }, env: { REQ: "1" } } },
		{ op: "stream", model: { provider: "oauth-prov", id: "o1" }, options: { apiKey: "explicit" }, transform: true },
		{ op: "stream", model: { provider: "cloudflare-ai-gateway", id: "gw1" }, options: { env: { CLOUDFLARE_GATEWAY_ID: "gw-req" } } },
		{ op: "stream", model: { provider: "throw-prov", id: "t1" }, options: {} },
		{ op: "stream", model: { provider: "missing", id: "m" }, options: {} },
		{ op: "setEnv", env: {} },
		{ op: "stream", model: { provider: "check-prov", id: "c1" }, options: {} },
	];
	const err = (e) => ({ error: { code: e.code ?? null, message: e.message } });
	const results = [];
	for (const step of script) {
		let out;
		try {
			switch (step.op) {
				case "setEnv": env = step.env; out = null; break;
				case "store": await store.modify(step.provider, async () => step.credential); out = null; break;
				case "read": out = (await store.read(step.provider)) ?? null; break;
				case "list": out = await store.list(); break;
				case "refreshes": out = refreshes[step.provider] ?? 0; break;
				case "getAuth": out = (await models.getAuth(step.model ? models.getModel(step.model.provider, step.model.id) : step.provider, step.overrides)) ?? null; break;
				case "concurrentGetAuth": out = await Promise.all(Array.from({ length: step.n }, () => models.getAuth(step.provider))); break;
				case "checkAuth": out = (await models.checkAuth(step.provider)) ?? null; break;
				case "getAvailable": out = (await models.getAvailable(step.provider)).map((m) => `${m.provider}/${m.id}`); break;
				case "logout": await models.logout(step.provider); out = null; break;
				case "login": {
					const answers = [...step.answers];
					const prompts = [];
					const events = [];
					const interaction = { prompt: async (p) => { prompts.push({ ...p, signal: undefined }); if (!answers.length) throw new Error("no answer"); return answers.shift(); }, notify: (e) => events.push(e) };
					let credential;
					try { credential = await models.login(step.provider, step.type, interaction); } catch (e) { credential = err(e); }
					out = { credential, prompts: clone(prompts), events };
					break;
				}
				case "stream": {
					const m = models.getModel(step.model.provider, step.model.id) ?? model("echo", step.model.provider, "https://default.example.com", { id: step.model.id });
					const options = { ...step.options, ...(step.transform ? { transformHeaders: (h) => ({ ...h, "X-Transformed": "yes" }) } : {}) };
					const result = await models.streamSimple(m, { messages: [user("hi")] }, options).result();
					out = result.errorMessage?.startsWith("{") ? { request: JSON.parse(result.errorMessage) } : { setupError: result.errorMessage };
					break;
				}
			}
		} catch (e) {
			out = err(e);
		}
		results.push({ step, out: clone(out) });
	}

	// auth.json written by coding-agent's AuthStorage.
	const { AuthStorage } = await import(pathToFileURL(join(root, "packages/coding-agent/src/core/auth-storage.ts")).href);
	const { mkdtempSync, readFileSync, rmSync } = await import("node:fs");
	const { tmpdir } = await import("node:os");
	const dir = mkdtempSync(join(tmpdir(), "auth-json-"));
	const authPath = join(dir, "auth.json");
	const fileOps = [
		{ op: "modify", provider: "anthropic", credential: { type: "api_key", key: "sk-1" } },
		{ op: "modify", provider: "openai-codex", credential: { type: "oauth", refresh: "r", access: "a", expires: 1767225600000, accountId: "acc" } },
		{ op: "modify", provider: "anthropic", credential: { type: "api_key", key: "$MY_KEY", env: { MY_KEY: "from-env" } } },
		{ op: "read", provider: "anthropic" },
		{ op: "delete", provider: "openai-codex" },
		{ op: "modify", provider: "cloudflare-workers-ai", credential: { type: "api_key", key: "cf", env: { CLOUDFLARE_ACCOUNT_ID: "acct" } } },
		{ op: "list" },
	];
	const storage = AuthStorage.create(authPath);
	const fileResults = [];
	for (const step of fileOps) {
		let out = null;
		if (step.op === "modify") await storage.modify(step.provider, async () => step.credential);
		if (step.op === "read") out = (await storage.read(step.provider)) ?? null;
		if (step.op === "delete") await storage.delete(step.provider);
		if (step.op === "list") out = await storage.list();
		fileResults.push({ step, out: clone(out) });
	}
	const fileText = readFileSync(authPath, "utf8");
	rmSync(dir, { recursive: true, force: true });
	return { file: "models-auth.json", script: results, fileOps: fileResults, fileText };
};

// ---------------------------------------------------------------------------------------------- OAuth providers
// xAI, Kimi Code, Meta and Radius flows against canned responses (global fetch stubbed). Timers run at once on a
// fake clock that advances by each delay; random PKCE/state values are masked as <random>.

sections.oauth = async () => {
	const { xaiOAuth } = await src("auth/oauth/xai.ts");
	const { kimiCodingOAuth } = await src("auth/oauth/kimi-coding.ts");
	const { metaOAuth } = await src("auth/oauth/meta.ts");
	const { createRadiusOAuth } = await src("auth/oauth/radius.ts");
	const flows = { xai: xaiOAuth, kimi: kimiCodingOAuth, meta: metaOAuth, radius: createRadiusOAuth({ name: "Radius", gateway: "gateway.example.com/" }) };
	const realSetTimeout = globalThis.setTimeout;
	let clock = NOW;
	const mask = (text) => text.replace(/(code_verifier=|code_challenge=|state=|"state":")[A-Za-z0-9_%-]+/g, "$1<random>").replace(/[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}/g, "<random>");
	async function runFlow(scenario) {
		clock = NOW;
		Date.now = () => clock;
		globalThis.setTimeout = (fn, ms, ...args) => {
			clock += Math.max(0, ms ?? 0);
			return realSetTimeout(fn, 0, ...args);
		};
		const queue = scenario.responses.map((r) => ({ ...r, chunks: [...(r.chunks ?? [])] }));
		const requests = [];
		globalThis.fetch = stubFetch(queue, requests);
		const answers = [...(scenario.answers ?? [])];
		const prompts = [];
		const events = [];
		let lastAuthUrl = "";
		const interaction = {
			signal: new AbortController().signal,
			prompt: async (p) => {
				const { signal: _signal, ...rest } = p;
				prompts.push(rest);
				if (!answers.length) throw new Error("no answer");
				const state = new URL(lastAuthUrl || "http://x/").searchParams.get("state") ?? "";
				return answers.shift().replace("{STATE}", state);
			},
			notify: (e) => {
				if (e.type === "auth_url") lastAuthUrl = e.url;
				events.push(e);
			},
		};
		const record = { name: scenario.name, flow: scenario.flow, action: scenario.action, credential: scenario.credential ?? null, answers: scenario.answers ?? [], responses: scenario.responses };
		try {
			const flow = flows[scenario.flow];
			const result = scenario.action === "login" ? await flow.login(interaction) : await flow.refresh(scenario.credential, new AbortController().signal);
			record.result = clone(result);
			record.thrown = null;
		} catch (e) {
			record.result = null;
			record.thrown = e.message;
		}
		globalThis.setTimeout = realSetTimeout;
		globalThis.fetch = realFetch;
		Date.now = () => NOW;
		record.requests = JSON.parse(mask(JSON.stringify(requests)));
		record.prompts = prompts;
		record.events = JSON.parse(mask(JSON.stringify(events)));
		record.toAuth = record.result ? clone(await flows[scenario.flow].toAuth(record.result)) : null;
		return record;
	}
	const j = (body, status = 200) => json(status, body);
	const text = (status, body) => ({ status, headers: { "content-type": "text/plain" }, chunks: [body] });
	const cred = (extra = {}) => ({ type: "oauth", refresh: "old-refresh", access: "old-access", expires: NOW - 1, ...extra });
	const xDevice = { device_code: "dev-1", user_code: "ABCD-EFGH", verification_uri: "https://accounts.x.ai/device", verification_uri_complete: "https://accounts.x.ai/device?code=ABCD", interval: 1, expires_in: 600 };
	const kDevice = { device_code: "kdev", user_code: "K-1", verification_uri: "https://www.kimi.com/device", verification_uri_complete: "https://www.kimi.com/device?u=K-1" };
	const mDevice = { device_code: "mdev", user_code: "M-1", verification_uri: "https://meta.com/device", interval: 2, expires_in: 300 };
	const scenarios = [
		{ name: "xai/login", flow: "xai", action: "login", responses: [j(xDevice), j({ error: "authorization_pending" }, 400), j({ error: "slow_down", interval: 3 }, 400), j({ access_token: "xa", refresh_token: "xr", expires_in: 7200 })] },
		{ name: "xai/login-slow-down-no-interval", flow: "xai", action: "login", responses: [j({ ...xDevice, interval: 0, verification_uri_complete: "" }), j({ error: "slow_down" }, 400), j({ access_token: "xa", refresh_token: "xr" })] },
		{ name: "xai/login-denied", flow: "xai", action: "login", responses: [j(xDevice), j({ error: "access_denied" }, 403)] },
		{ name: "xai/login-expired-code", flow: "xai", action: "login", responses: [j(xDevice), j({ error: "expired_token" }, 400)] },
		{ name: "xai/login-poll-other-error", flow: "xai", action: "login", responses: [j(xDevice), j({ error: "server_error", error_description: "oops" }, 500)] },
		{ name: "xai/login-untrusted-uri", flow: "xai", action: "login", responses: [j({ ...xDevice, verification_uri: "http://evil.example.com" })] },
		{ name: "xai/login-device-http-error", flow: "xai", action: "login", responses: [j({ error: "invalid_client", error_description: "bad client" }, 401)] },
		{ name: "xai/login-invalid-json", flow: "xai", action: "login", responses: [text(502, "<html>")] },
		{ name: "xai/login-missing-field", flow: "xai", action: "login", responses: [j({ ...xDevice, user_code: "" })] },
		{ name: "xai/login-times-out", flow: "xai", action: "login", responses: [j({ ...xDevice, expires_in: 3 }), j({ error: "authorization_pending" }, 400), j({ error: "authorization_pending" }, 400), j({ error: "authorization_pending" }, 400)] },
		{ name: "xai/login-times-out-after-slow-down", flow: "xai", action: "login", responses: [j({ ...xDevice, expires_in: 2 }), j({ error: "slow_down" }, 400)] },
		{ name: "xai/refresh-keeps-refresh-token", flow: "xai", action: "refresh", credential: cred(), responses: [j({ access_token: "na", expires_in: 100 })] },
		{ name: "xai/refresh-rotates", flow: "xai", action: "refresh", credential: cred(), responses: [j({ access_token: "na", refresh_token: "nr" })] },
		{ name: "xai/refresh-fails", flow: "xai", action: "refresh", credential: cred(), responses: [j({ error: "invalid_grant" }, 400)] },
		{ name: "xai/refresh-bad-expiry", flow: "xai", action: "refresh", credential: cred(), responses: [j({ access_token: "na", expires_in: -5 })] },
		{ name: "kimi/login", flow: "kimi", action: "login", responses: [j(kDevice), j({ error: "authorization_pending" }), j({ access_token: "ka", refresh_token: "kr", expires_in: 900 })] },
		{ name: "kimi/login-slow-down", flow: "kimi", action: "login", responses: [j({ ...kDevice, interval: 2, expires_in: 60 }), j({ error: "slow_down", interval: 4 }, 400), j({ access_token: "ka", refresh_token: "kr", expires_in: 900 })] },
		{ name: "kimi/login-device-error", flow: "kimi", action: "login", responses: [text(400, "bad request")] },
		{ name: "kimi/login-invalid-device", flow: "kimi", action: "login", responses: [j({ ...kDevice, verification_uri_complete: undefined })] },
		{ name: "kimi/login-poll-500", flow: "kimi", action: "login", responses: [j(kDevice), text(503, "down")] },
		{ name: "kimi/login-expired", flow: "kimi", action: "login", responses: [j(kDevice), j({ error: "expired_token" }, 400)] },
		{ name: "kimi/login-denied", flow: "kimi", action: "login", responses: [j(kDevice), j({ error: "access_denied" }, 400)] },
		{ name: "kimi/login-other", flow: "kimi", action: "login", responses: [j(kDevice), j({ error: "weird", error_description: "huh" }, 400)] },
		{ name: "kimi/login-token-missing-fields", flow: "kimi", action: "login", responses: [j(kDevice), j({ access_token: "ka" })] },
		{ name: "kimi/refresh", flow: "kimi", action: "refresh", credential: cred(), responses: [j({ access_token: "ka2", refresh_token: "kr2", expires_in: 600 })] },
		{ name: "kimi/refresh-unauthorized", flow: "kimi", action: "refresh", credential: cred(), responses: [j({ error: "invalid_grant", error_description: "revoked" }, 400)] },
		{ name: "kimi/refresh-retries", flow: "kimi", action: "refresh", credential: cred(), responses: [text(503, "x"), j({}, 429), j({ access_token: "ka3", refresh_token: "kr3", expires_in: 60 })] },
		{ name: "kimi/refresh-exhausts-retries", flow: "kimi", action: "refresh", credential: cred(), responses: [text(500, "x"), text(500, "x"), text(500, "x"), j({ message: "still down" }, 500)] },
		{ name: "kimi/refresh-network-errors", flow: "kimi", action: "refresh", credential: cred(), responses: [{ networkError: true }, { networkError: true }, { networkError: true }, { networkError: true }] },
		{ name: "kimi/refresh-bad-request", flow: "kimi", action: "refresh", credential: cred(), responses: [text(400, "nope")] },
		{ name: "meta/login", flow: "meta", action: "login", responses: [j(mDevice), j({ error: "authorization_pending" }, 400), j({ access_token: "identity" }), j({ api_key: "meta-key" })] },
		{ name: "meta/login-complete-uri", flow: "meta", action: "login", responses: [j({ ...mDevice, verification_uri_complete: "https://meta.com/device?c=M-1", interval: undefined, expires_in: undefined }), j({ access_token: "identity" }), j({ api_key: "k" })] },
		{ name: "meta/login-mint-expired", flow: "meta", action: "login", responses: [j(mDevice), j({ access_token: "identity" }), j({ detail: " no session " }, 403)] },
		{ name: "meta/login-no-key", flow: "meta", action: "login", responses: [j(mDevice), j({ access_token: "identity" }), j({ action_url: "https://meta.ai/setup" })] },
		{ name: "meta/login-device-failure", flow: "meta", action: "login", responses: [j({ error_description: "client blocked" }, 400)] },
		{ name: "meta/login-invalid-device", flow: "meta", action: "login", responses: [j({ device_code: "d", user_code: "u", verification_uri: "javascript:alert(1)" })] },
		{ name: "meta/login-poll-failure", flow: "meta", action: "login", responses: [j(mDevice), j({ message: "boom" }, 500)] },
		{ name: "meta/refresh", flow: "meta", action: "refresh", credential: cred({ refresh: "identity" }), responses: [j({ api_key: "fresh-key" })] },
		{ name: "meta/refresh-mint-error", flow: "meta", action: "refresh", credential: cred({ refresh: "identity" }), responses: [j({ error: "x" }, 500)] },
		{ name: "radius/device-login", flow: "radius", action: "login", answers: ["device-code"], responses: [j({ device_code: "rd", user_code: "R-1", verification_uri: "https://gateway.example.com/device", expires_in: 120, interval: 1 }), j({ error: "authorization_pending" }, 400), j({ error: "slow_down" }, 400), j({ access_token: "ra", refresh_token: "rr", expires_in: 3600, scope: "gateway" })] },
		{ name: "radius/device-expired", flow: "radius", action: "login", answers: ["device-code"], responses: [j({ device_code: "rd", user_code: "R-1", verification_uri: "https://gateway.example.com/device", expires_in: 120 }), j({ error: "expired_token" }, 400)] },
		{ name: "radius/device-other-error", flow: "radius", action: "login", answers: ["device-code"], responses: [j({ device_code: "rd", user_code: "R-1", verification_uri: "https://gateway.example.com/device", expires_in: 120 }), text(500, "gateway broke")] },
		{ name: "radius/device-missing-fields", flow: "radius", action: "login", answers: ["device-code"], responses: [j({ device_code: "rd" })] },
		{ name: "radius/device-http-error", flow: "radius", action: "login", answers: ["device-code"], responses: [j({ error: "unauthorized_client" }, 401)] },
		{ name: "radius/browser-pasted-redirect", flow: "radius", action: "login", answers: ["browser", "http://127.0.0.1:1456/oauth/callback?code=pasted-code&state={STATE}"], responses: [j({ authorizationEndpoint: "https://login.example.com/authorize?old=1" }), j({ access_token: "ba", refresh_token: "br", expires_in: 60 })] },
		{ name: "radius/browser-pasted-code", flow: "radius", action: "login", answers: ["browser", "  bare-code  "], responses: [j({ authorizationEndpoint: "https://login.example.com/authorize" }), j({ access_token: "ba", refresh_token: "br", expires_in: 60 })] },
		{ name: "radius/browser-state-mismatch", flow: "radius", action: "login", answers: ["browser", "code=c&state=wrong"], responses: [j({ authorizationEndpoint: "https://login.example.com/authorize" })] },
		{ name: "radius/browser-empty-paste", flow: "radius", action: "login", answers: ["browser", "   "], responses: [j({ authorizationEndpoint: "https://login.example.com/authorize" })] },
		{ name: "radius/browser-bad-discovery", flow: "radius", action: "login", answers: ["browser"], responses: [text(404, "no oauth here")] },
		{ name: "radius/browser-discovery-shape", flow: "radius", action: "login", answers: ["browser"], responses: [j({ authorization: "x" })] },
		{ name: "radius/unknown-method", flow: "radius", action: "login", answers: ["carrier-pigeon"], responses: [] },
		{ name: "radius/refresh", flow: "radius", action: "refresh", credential: cred(), responses: [j({ access_token: "na", refresh_token: "nr", expires_in: 3600 })] },
		{ name: "radius/refresh-invalid-grant", flow: "radius", action: "refresh", credential: cred(), responses: [j({ error: "invalid_grant", error_description: "expired" }, 400)] },
		{ name: "radius/refresh-text-error", flow: "radius", action: "refresh", credential: cred(), responses: [text(503, "maintenance")] },
		{ name: "radius/refresh-empty-error", flow: "radius", action: "refresh", credential: cred(), responses: [text(500, "")] },
	];
	const results = [];
	for (const scenario of scenarios) results.push(await runFlow(scenario));
	const subscription = await subscriptionCases();
	const pkce = await (await src("auth/oauth/pkce.ts")).generatePKCE();
	const { createHash } = await import("node:crypto");
	const pages = await src("auth/oauth/oauth-page.ts");
	const { normalizeRadiusGatewayUrl } = await src("providers/radius-config.ts");
	return {
		file: "oauth.json",
		units: {
			pkceChallengeMatchesVerifier: createHash("sha256").update(pkce.verifier).digest("base64url") === pkce.challenge,
			challenge: { verifier: "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk", challenge: createHash("sha256").update("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk").digest("base64url") },
			pages: { success: pages.oauthSuccessHtml("Signed in to <Radius> & co."), error: pages.oauthErrorHtml("Bad \"state\"", "it's <bad>"), errorNoDetails: pages.oauthErrorHtml("Nope") },
			gateways: ["gateway.example.com/", "HTTP://x.example.com//", "https://y.example.com"].map((input) => ({ input, output: normalizeRadiusGatewayUrl(input) })),
		},
		scenarios: results,
		subscription,
	};
};

// Anthropic (Claude Pro/Max), OpenAI Codex and GitHub Copilot `OAuthAuth.refresh`/`toAuth` (tests/oauth.rs). Each case
// stubs fetch with canned responses and records every request (URL, method, lower-cased headers, body text) and the
// resulting credential or error message.
async function subscriptionCases() {
	const { anthropicOAuth } = await src("auth/oauth/anthropic.ts");
	const { openaiCodexOAuth } = await src("auth/oauth/openai-codex.ts");
	const { githubCopilotOAuth } = await src("auth/oauth/github-copilot.ts");
	const auths = { anthropic: anthropicOAuth, "openai-codex": openaiCodexOAuth, "github-copilot": githubCopilotOAuth };
	let queue = [];
	let requests = [];
	const stub = async (url, init = {}) => {
		const headers = {};
		for (const [k, v] of Object.entries(init.headers ?? {})) headers[k.toLowerCase()] = String(v);
		requests.push({ url: String(url), method: init.method ?? "GET", headers, body: init.body === undefined ? "" : String(init.body) });
		const next = queue.shift();
		if (!next) throw new Error("unexpected request");
		return new Response(next.body, { status: next.status, statusText: STATUS_TEXT[next.status] ?? "" });
	};
	// Unsigned fakes; the signature says so, which also keeps scripts/secret-scan.mjs from taking them for real tokens.
	const jwt = (payload) => `${Buffer.from('{"alg":"none"}').toString("base64")}.${Buffer.from(JSON.stringify(payload)).toString("base64")}.example-sig`;
	const CODEX_JWT = jwt({ "https://api.openai.com/auth": { chatgpt_account_id: "acct_123" }, sub: "u" });
	const CODEX_JWT_NO_ACCOUNT = jwt({ "https://api.openai.com/auth": { chatgpt_account_id: "" } });
	const COPILOT_TOKEN = "tid=abc;exp=1767229200;proxy-ep=proxy.individual.githubcopilot.com;sku=x";
	const COPILOT_ENT_TOKEN = "tid=abc;exp=1767229200;sku=x";
	const models = (items) => JSON.stringify({ data: items });
	const copilotModels = models([
		{ id: "gpt-4o", model_picker_enabled: true, policy: { state: "enabled" } },
		{ id: "claude-sonnet-4.5", model_picker_enabled: true, policy: { state: "unconfigured" } },
		{ id: "no-tools", model_picker_enabled: true, capabilities: { supports: { tool_calls: false } } },
		{ id: "disabled-one", model_picker_enabled: true, policy: { state: "disabled" } },
		{ id: 42 },
		"junk",
	]);
	const fallbackModels = models([
		{ id: "gpt-4o", model_picker_enabled: false, policy: { state: "enabled" } },
		{ id: "gpt-5", model_picker_enabled: false, policy: { state: "disabled" } },
	]);
	const copilotToken = JSON.stringify({ token: COPILOT_TOKEN, expires_at: 1767229200 });
	const cases = [
		["anthropic refresh ok", "anthropic", "refresh", { refresh: "r1", access: "a0", expires: 0 }, [[200, '{"access_token":"a1","refresh_token":"r2","expires_in":3600,"scope":"x"}']]],
		["anthropic refresh http error", "anthropic", "refresh", { refresh: "r1", access: "a0", expires: 0 }, [[400, '{"error":"invalid_grant"}']]],
		["anthropic refresh invalid json", "anthropic", "refresh", { refresh: "r1", access: "a0", expires: 0 }, [[200, "not json"]]],
		["codex refresh ok", "openai-codex", "refresh", { refresh: "r & =1", access: "a0", expires: 0, accountId: "old" }, [[200, JSON.stringify({ access_token: CODEX_JWT, refresh_token: "r2", expires_in: 864000 })]]],
		["codex refresh no account", "openai-codex", "refresh", { refresh: "r1", access: "a0", expires: 0 }, [[200, JSON.stringify({ access_token: CODEX_JWT_NO_ACCOUNT, refresh_token: "r2", expires_in: 10 })]]],
		["codex refresh not a jwt", "openai-codex", "refresh", { refresh: "r1", access: "a0", expires: 0 }, [[200, JSON.stringify({ access_token: "opaque", refresh_token: "r2", expires_in: 10 })]]],
		["codex refresh missing fields", "openai-codex", "refresh", { refresh: "r1", access: "a0", expires: 0 }, [[200, '{"access_token":"x","expires_in":"10"}']]],
		["codex refresh http error", "openai-codex", "refresh", { refresh: "r1", access: "a0", expires: 0 }, [[401, '{"error":"bad"}']]],
		["codex refresh http error empty body", "openai-codex", "refresh", { refresh: "r1", access: "a0", expires: 0 }, [[500, ""]]],
		["copilot refresh ok", "github-copilot", "refresh", { refresh: "gho_1", access: "old", expires: 0 }, [[200, copilotToken], [200, copilotModels]]],
		["copilot refresh policy fallback", "github-copilot", "refresh", { refresh: "gho_1", access: "old", expires: 0 }, [[200, copilotToken], [200, fallbackModels]]],
		["copilot refresh enterprise", "github-copilot", "refresh", { refresh: "gho_1", access: "old", expires: 0, enterpriseUrl: "https://Company.GHE.com/some/path" }, [[200, JSON.stringify({ token: COPILOT_ENT_TOKEN, expires_at: 1767229200 })], [200, fallbackModels]]],
		["copilot refresh invalid fields", "github-copilot", "refresh", { refresh: "gho_1", access: "old", expires: 0 }, [[200, JSON.stringify({ token: 5, expires_at: 1 })]]],
		["copilot refresh token http error", "github-copilot", "refresh", { refresh: "gho_1", access: "old", expires: 0 }, [[403, "nope"]]],
		["copilot refresh models http error", "github-copilot", "refresh", { refresh: "gho_1", access: "old", expires: 0 }, [[200, copilotToken], [500, "boom"]]],
		["copilot refresh models invalid", "github-copilot", "refresh", { refresh: "gho_1", access: "old", expires: 0 }, [[200, copilotToken], [200, '{"data":{}}']]],
		["copilot toAuth proxy", "github-copilot", "toAuth", { refresh: "r", access: COPILOT_TOKEN, expires: 0 }, []],
		["copilot toAuth enterprise", "github-copilot", "toAuth", { refresh: "r", access: COPILOT_ENT_TOKEN, expires: 0, enterpriseUrl: "ghe.example.com" }, []],
		["copilot toAuth bad enterprise", "github-copilot", "toAuth", { refresh: "r", access: "plain", expires: 0, enterpriseUrl: "http://[bad" }, []],
		["copilot toAuth default", "github-copilot", "toAuth", { refresh: "r", access: "plain", expires: 0, enterpriseUrl: 7 }, []],
		["anthropic toAuth", "anthropic", "toAuth", { refresh: "r", access: "sk-ant-oat", expires: 0 }, []],
		["codex toAuth", "openai-codex", "toAuth", { refresh: "r", access: CODEX_JWT, expires: 0 }, []],
	];
	const out = [];
	for (const [name, provider, op, credential, responses] of cases) {
		queue = responses.map(([status, body]) => ({ status, body }));
		requests = [];
		globalThis.fetch = stub;
		Date.now = () => NOW;
		const full = { type: "oauth", ...credential };
		let result = null;
		let error = null;
		try {
			result = op === "refresh" ? await auths[provider].refresh(full, new AbortController().signal) : await auths[provider].toAuth(full);
		} catch (e) {
			// The `; stack=<node stack>` Anthropic appends names this checkout's paths; it is not ported.
			error = e.message.split("; stack=")[0];
		}
		globalThis.fetch = realFetch;
		out.push({ name, provider, op, credential: full, responses, requests, result: result && JSON.parse(JSON.stringify(result)), error });
	}
	return out;
}

// ---------------------------------------------------------------------------------------------- main

const wanted = process.argv.slice(2);
for (const [name, section] of Object.entries(sections)) {
	if (wanted.length > 0 && !wanted.includes(name)) continue;
	const { file, ...content } = await section();
	const out = join(here, file);
	writeFileSync(out, `${JSON.stringify({ generator: "crates/ai/fixtures/generate-rest.mjs", now: NOW, ...content }, null, "\t")}\n`);
	console.log(`wrote ${out}`);
}
