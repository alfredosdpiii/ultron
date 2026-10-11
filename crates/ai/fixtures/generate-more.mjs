// Generates crates/ai/fixtures/google.json and mistral.json from this worktree's TypeScript google-generative-ai and
// mistral-conversations providers. Canned responses are served through a stubbed `fetch` (`globalThis.fetch` too: the
// @google/genai SDK rejects a custom one); each scenario records the request (URL, method, headers, JSON body), every
// pushed event (cloned at push time) and the final message. tests/providers_more.rs replays them through the Rust ports.
// Usage: node crates/ai/fixtures/generate-more.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const src = (path) => import(pathToFileURL(join(root, "packages/ai/src", path)).href);

for (const name of Object.keys(process.env)) {
	if (/^(PI_|GOOGLE_|GEMINI_|GCLOUD_|MISTRAL_)/.test(name)) delete process.env[name];
}
const NOW = 1767225600000;
Date.now = () => NOW;

const google = await src("api/google-generative-ai.ts");
const mistral = await src("api/mistral-conversations.ts");
const vertex = await src("api/google-vertex.ts");
const API = { "google-generative-ai": google, "google-vertex": vertex, "mistral-conversations": mistral };
const shared = await src("api/google-shared.ts");
const { AssistantMessageEventStream } = await src("utils/event-stream.ts");
const { getPiUserAgent } = await src("utils/pi-user-agent.ts");

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

const UA = getPiUserAgent();
function requestHeaders(init) {
	const out = {};
	new Headers(init.headers).forEach((value, key) => {
		// SDK telemetry naming the Node version; the Rust port does not send it.
		if (key === "x-goog-api-client") return;
		out[key] = value.replaceAll(UA, "<pi-user-agent>");
	});
	return Object.fromEntries(Object.entries(out).sort(([a], [b]) => (a < b ? -1 : 1)));
}

const abortError = () => new DOMException("This operation was aborted", "AbortError");

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
		const chunks = canned.chunks.map((c) => encoder.encode(c));
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

const realFetch = globalThis.fetch;
async function run(scenario) {
	const queue = scenario.responses.map((r) => ({ ...r, chunks: [...r.chunks] }));
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
		const fetch = scenario.model.api.startsWith("google") ? {} : { fetch: globalThis.fetch };
		const s = API[scenario.model.api][record.fn](scenario.model, scenario.context, { ...scenario.options, ...fetch, signal: controller.signal });
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

const data = (items) => items.map((d) => `data: ${typeof d === "string" ? d : JSON.stringify(d)}\r\n\r\n`);
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
		parameters: {
			$schema: "http://json-schema.org/draft-07/schema#",
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
const IMG = { type: "image", data: "aGVsbG8=", mimeType: "image/png" };
const usage0 = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, totalTokens: 0, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };
const system = (content, extra = {}) => ({ role: "system", content, timestamp: 1, ...extra });
const user = (content, timestamp = 2) => ({ role: "user", content, timestamp });
const assistant = (model, content, extra = {}) => ({ role: "assistant", content, api: model.api, provider: model.provider, model: model.id, usage: usage0, stopReason: "stop", timestamp: 3, ...extra });
const toolResult = (toolCallId, toolName, content, isError = false) => ({ role: "toolResult", toolCallId, toolName, content, isError, timestamp: 4 });

function model(id, extra = {}) {
	return {
		id,
		name: "Test Model",
		api: "google-generative-ai",
		provider: "google",
		baseUrl: "https://generativelanguage.googleapis.com/v1beta",
		reasoning: true,
		input: ["text", "image"],
		cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 },
		contextWindow: 200000,
		maxTokens: 32000,
		...extra,
	};
}

const SIG = "c2lnbmF0dXJl";
function history(m) {
	return [
		system("You are helpful.", { toolsAdded: tools, sections: { rules: "<rules>be brief</rules>" } }),
		user("List files"),
		assistant(m, [
			{ type: "thinking", thinking: "I should read.", thinkingSignature: SIG },
			{ type: "thinking", thinking: "", thinkingSignature: "bad sig!" },
			{ type: "text", text: "", textSignature: SIG },
			{ type: "text", text: "Reading.", textSignature: "not-base64" },
			{ type: "text", text: "Odd signature length.", textSignature: "abcde" },
			{ type: "toolCall", id: "call_1", name: "read", arguments: { path: "a.txt" }, thoughtSignature: SIG },
			{ type: "toolCall", id: "call_2", name: "read", arguments: { path: "b.txt" } },
		], { stopReason: "toolUse" }),
		toolResult("call_1", "read", [{ type: "text", text: "contents of a" }, IMG]),
		toolResult("call_2", "read", [IMG], true),
		user([{ type: "text", text: "Now look at this" }, IMG]),
		assistant({ api: "anthropic-messages", provider: "anthropic", id: "claude-x" }, [
			{ type: "thinking", thinking: "Foreign thoughts", thinkingSignature: SIG },
			{ type: "thinking", thinking: "  ", thinkingSignature: SIG },
			{ type: "text", text: "Foreign answer", textSignature: SIG },
			{ type: "toolCall", id: "call|weird+id/with=chars", name: "bash", arguments: { command: "ls" } },
		], { stopReason: "toolUse" }),
		system("Mid-conversation rule", { toolsAdded: [{ name: "grep", description: "Search", parameters: { type: "object", properties: {} } }] }),
		user("Continue"),
		assistant(m, [{ type: "text", text: "partial" }], { stopReason: "error", errorMessage: "boom" }),
		user("Again"),
	];
}
const simple = (text = "hi") => ({ messages: [system("sys", { toolsAdded: tools }), user(text)] });

const g3 = model("gemini-3-pro-preview", { thinkingLevelMap: { off: null, minimal: "low", low: "low", medium: "high", high: "high" } });
const g25 = model("gemini-2.5-flash");
const glite = model("gemini-2.5-flash-lite", { headers: { "X-Model": "m" } });
const gNoReason = model("gemma-3-27b", { reasoning: false, input: ["text"] });

const usage = (extra = {}) => ({ promptTokenCount: 120, cachedContentTokenCount: 20, candidatesTokenCount: 30, thoughtsTokenCount: 7, totalTokenCount: 157, ...extra });
const cand = (parts, finishReason) => ({ candidates: [{ content: { role: "model", parts }, ...(finishReason ? { finishReason } : {}), index: 0 }] });

const scenarios = [
	{
		name: "google/gemini3-thinking-text-tools-history",
		model: g3,
		context: { messages: history(g3) },
		options: { apiKey: "g-key", temperature: 0.4, maxTokens: 9000, toolChoice: "auto", thinking: { enabled: true, level: "HIGH" }, headers: { "X-Extra": "1", "X-Drop": null } },
		responses: [ok(data([
			{ ...cand([{ text: "Let me ", thought: true, thoughtSignature: SIG }]), responseId: "resp-1" },
			{ ...cand([{ text: "think.", thought: true }]), responseId: "resp-2" },
			cand([{ text: "Hello " }]),
			cand([{ text: "world 🌍", thoughtSignature: "dGV4dA==" }, { thought: true, text: "again" }]),
			cand([{ functionCall: { id: "fc_1", name: "read", args: { path: "src/main.rs" } }, thoughtSignature: SIG }]),
			cand([{ functionCall: { id: "fc_1", name: "bash", args: { command: "ls" } } }, { functionCall: { name: "read" } }]),
			{ ...cand([], "STOP"), usageMetadata: usage() },
		]))],
	},
	{
		name: "google/gemini25-budget-length",
		model: g25,
		context: { messages: history(g25) },
		options: { apiKey: "g-key", toolChoice: "any", thinking: { enabled: true, budgetTokens: 1024 } },
		responses: [ok(data([
			cand([{ text: "Trunc" }]),
			{ ...cand([{ text: "ated" }], "MAX_TOKENS"), usageMetadata: { promptTokenCount: 10, candidatesTokenCount: 5, totalTokenCount: 15 } },
		]))],
	},
	{
		name: "google/thinking-disabled-none",
		model: g25,
		context: simple(),
		options: { apiKey: "g-key", toolChoice: "none", thinking: { enabled: false } },
		responses: [ok(data([{ ...cand([{ text: "ok" }], "STOP"), usageMetadata: usage({ cachedContentTokenCount: undefined }) }]))],
	},
	{
		name: "google/gemini3-thinking-disabled",
		model: g3,
		context: simple(),
		options: { apiKey: "g-key", thinking: { enabled: false } },
		responses: [ok(data([cand([{ text: "ok" }], "STOP")]))],
	},
	{
		name: "google/no-reasoning-model",
		model: gNoReason,
		context: { messages: history(gNoReason) },
		options: { apiKey: "g-key", thinking: { enabled: true, level: "LOW" } },
		responses: [ok(data([cand([{ text: "ok" }], "STOP")]))],
	},
	{
		name: "google/no-reasoning-thinking-disabled",
		model: gNoReason,
		context: simple(),
		options: { apiKey: "g-key", thinking: { enabled: false } },
		responses: [ok(data([cand([{ text: "ok" }], "STOP")]))],
	},
	{
		name: "google/safety",
		model: g25,
		context: simple(),
		options: { apiKey: "g-key" },
		responses: [ok(data([cand([{ text: "par" }]), cand([], "SAFETY")]))],
	},
	{
		name: "google/no-finish-reason",
		model: g25,
		context: simple(),
		options: { apiKey: "g-key" },
		responses: [ok(data([cand([{ text: "par" }])]))],
	},
	{
		name: "google/http-400",
		model: g25,
		context: simple(),
		options: { apiKey: "g-key" },
		responses: [json(400, { error: { code: 400, message: "API key not valid", status: "INVALID_ARGUMENT" } })],
	},
	{
		name: "google/http-500-text",
		model: g25,
		context: simple(),
		options: { apiKey: "g-key" },
		responses: [json(500, "upstream exploded", { "content-type": "text/plain" })],
	},
	{
		name: "google/retry-503-then-ok",
		model: g25,
		context: simple(),
		options: { apiKey: "g-key", maxRetries: 2 },
		responses: [json(503, { error: { code: 503, message: "overloaded", status: "UNAVAILABLE" } }, { "retry-after": "0" }), ok(data([cand([{ text: "finally" }], "STOP")]))],
	},
	{
		name: "google/abort-after-first-delta",
		model: g25,
		context: simple(),
		options: { apiKey: "g-key" },
		abortOn: { type: "text_delta", nth: 1 },
		responses: [ok(data([cand([{ text: "one" }]), cand([{ text: "two" }]), cand([], "STOP")]))],
	},
	{ name: "google/missing-key", model: g25, context: simple(), options: {}, responses: [] },
	{ name: "google/simple-no-reasoning", fn: "streamSimple", model: g25, context: simple(), options: { apiKey: "g-key", maxTokens: 500, toolChoice: "auto" }, responses: [ok(data([cand([{ text: "ok" }], "STOP")]))] },
	{ name: "google/simple-medium-flash", fn: "streamSimple", model: g25, context: simple(), options: { apiKey: "g-key", reasoning: "medium" }, responses: [ok(data([cand([{ text: "ok" }], "STOP")]))] },
	{ name: "google/simple-high-lite-custom", fn: "streamSimple", model: glite, context: simple(), options: { apiKey: "g-key", reasoning: "high", thinkingBudgets: { low: 7 } }, responses: [ok(data([cand([{ text: "ok" }], "STOP")]))] },
	{ name: "google/simple-low-lite-custom", fn: "streamSimple", model: glite, context: simple(), options: { apiKey: "g-key", reasoning: "low", thinkingBudgets: { low: 7 } }, responses: [ok(data([cand([{ text: "ok" }], "STOP")]))] },
	{ name: "google/simple-gemini3-medium", fn: "streamSimple", model: g3, context: simple(), options: { apiKey: "g-key", reasoning: "medium" }, responses: [ok(data([cand([{ text: "ok" }], "STOP")]))] },
	{ name: "google/simple-gemini3-minimal", fn: "streamSimple", model: g3, context: simple(), options: { apiKey: "g-key", reasoning: "minimal" }, responses: [ok(data([cand([{ text: "ok" }], "STOP")]))] },
	{ name: "google/simple-other-xhigh", fn: "streamSimple", model: model("gemini-1.5-pro"), context: simple(), options: { apiKey: "g-key", reasoning: "xhigh" }, responses: [ok(data([cand([{ text: "ok" }], "STOP")]))] },
	{ name: "google/simple-2.5-pro-minimal", fn: "streamSimple", model: model("gemini-2.5-pro"), context: simple(), options: { apiKey: "g-key", reasoning: "minimal" }, responses: [ok(data([cand([{ text: "ok" }], "STOP")]))] },
	{ name: "google/simple-missing-key", fn: "streamSimple", model: g25, context: simple(), options: {}, responses: [] },
];

// ---------------------------------------------------------------------------------------------- Mistral

const mModel = (id, extra = {}) => ({ ...model(id), api: "mistral-conversations", provider: "mistral", baseUrl: "https://api.mistral.ai", ...extra });
const mLarge = mModel("mistral-large-latest", { reasoning: false });
const mMag = mModel("magistral-medium-latest");
const mSmall = mModel("mistral-small-latest", { thinkingLevelMap: { minimal: "none", low: "none", medium: "high", high: "high" } });
const mText = mModel("codestral-latest", { input: ["text"], reasoning: false, baseUrl: "https://proxy.example.com/mistral/?x=1", headers: { "X-Model": "m", "x-affinity": "pinned" } });
const mChunk = (delta, extra = {}) => ({ id: "cmpl-1", choices: [{ index: 0, delta, finish_reason: null, ...extra }] });
const mDone = (reason, usage) => ({ id: "cmpl-1", choices: [{ index: 0, delta: {}, finish_reason: reason }], ...(usage ? { usage } : {}) });
const sseData = (items) => items.map((d) => `data: ${typeof d === "string" ? d : JSON.stringify(d)}\n\n`);
function mHistory(m) {
	return [
		system("You are helpful.", { toolsAdded: tools, sections: { rules: "<rules>be brief</rules>" } }),
		user("List files"),
		assistant(m, [
			{ type: "thinking", thinking: "I should read." },
			{ type: "thinking", thinking: "   " },
			{ type: "text", text: "Reading." },
			{ type: "text", text: "" },
			{ type: "toolCall", id: "abcDEF123", name: "read", arguments: { path: "a.txt" } },
			{ type: "toolCall", id: "call_with_a_long_id_1", name: "read", arguments: { path: "b.txt" } },
		], { stopReason: "toolUse" }),
		toolResult("abcDEF123", "read", [{ type: "text", text: "  contents of a  " }, IMG]),
		toolResult("call_with_a_long_id_1", "read", [IMG], true),
		user([{ type: "text", text: "Now look at this" }, IMG]),
		user([IMG]),
		assistant({ api: "anthropic-messages", provider: "anthropic", id: "claude-x" }, [
			{ type: "thinking", thinking: "Foreign thoughts", thinkingSignature: "x" },
			{ type: "toolCall", id: "call|weird+id/with=chars", name: "bash", arguments: { command: "ls" } },
		], { stopReason: "toolUse" }),
		toolResult("call|weird+id/with=chars", "bash", [], false),
		system("Mid-conversation rule", { toolsAdded: [{ name: "grep", description: "Search", parameters: { type: "object", properties: {} } }] }),
		user("Continue"),
		assistant(m, [{ type: "text", text: "partial" }], { stopReason: "error", errorMessage: "boom" }),
		user("Again"),
	];
}
const mistralScenarios = [
	{
		name: "mistral/thinking-text-tools-history",
		model: mMag,
		context: { messages: mHistory(mMag) },
		options: { apiKey: "m-key", temperature: 0.3, maxTokens: 800, toolChoice: "any", promptMode: "reasoning", sessionId: "sess-1", headers: { "X-Extra": "1", accept: null } },
		responses: [ok(sseData([
			mChunk({ role: "assistant", content: [{ type: "thinking", thinking: [{ type: "text", text: "Let me " }, { type: "text", text: "" }] }] }),
			mChunk({ content: [{ type: "thinking", thinking: [] }] }),
			mChunk({ content: [{ type: "thinking", thinking: [{ type: "text", text: "think." }] }] }),
			mChunk({ content: "Hello " }),
			mChunk({ content: [{ type: "text", text: "world 🌍" }, { type: "reference", reference_ids: [1] }] }),
			mChunk({ tool_calls: [{ id: "tc1", index: 0, function: { name: "read", arguments: '{"path": "src/ma' } }] }),
			mChunk({ tool_calls: [{ id: "tc1", index: 0, function: { name: "read", arguments: 'in.rs"}' } }] }),
			mChunk({ tool_calls: [{ id: "null", function: { name: "bash", arguments: { command: "ls" } } }] }),
			mDone("tool_calls", { prompt_tokens: 120, completion_tokens: 30, total_tokens: 150, prompt_tokens_details: { cached_tokens: 20 } }),
			"[DONE]",
			mChunk({ content: "ignored after done" }),
		]))],
	},
	{
		name: "mistral/crlf-split-length-cached-camel",
		model: mText,
		context: { messages: mHistory(mText) },
		options: { apiKey: "m-key", sessionId: "sess-2", toolChoice: { type: "function", function: { name: "read", extra: 1 } } },
		responses: [{ chunks: (() => {
			const text = sseData([mChunk({ content: "héllo ✓" }), mDone("model_length", { prompt_tokens: 10, completion_tokens: 5, promptTokensDetails: { cachedTokens: 50 } })]).join("").replaceAll("\n", "\r\n");
			const out = [];
			for (let i = 0; i < text.length; i += 9) out.push(text.slice(i, i + 9));
			return out;
		})() }],
	},
	{
		name: "mistral/no-cache-retention-none",
		model: mLarge,
		context: simple(),
		options: { apiKey: "m-key", sessionId: "sess-3", cacheRetention: "none" },
		responses: [ok(sseData([mChunk({ content: "ok" }), mDone("stop", { prompt_tokens: 7, completion_tokens: 2, num_cached_tokens: "x" })]))],
	},
	{
		name: "mistral/trailing-event-without-blank-line",
		model: mLarge,
		context: simple(),
		options: { apiKey: "m-key" },
		responses: [ok([`data: ${JSON.stringify(mChunk({ content: "a" }))}\n\n`, `: comment\nevent: x\ndata: ${JSON.stringify(mDone("stop"))}`])],
	},
	{ name: "mistral/error-finish", model: mLarge, context: simple(), options: { apiKey: "m-key" }, responses: [ok(sseData([mChunk({ content: "a" }), mDone("error")]))] },
	{ name: "mistral/unknown-finish", model: mLarge, context: simple(), options: { apiKey: "m-key" }, responses: [ok(sseData([mDone("weird_reason")]))] },
	{ name: "mistral/no-finish", model: mLarge, context: simple(), options: { apiKey: "m-key" }, responses: [ok(sseData([mChunk({ content: "a" }), { id: "x", choices: [] }]))] },
	{ name: "mistral/invalid-event", model: mLarge, context: simple(), options: { apiKey: "m-key" }, responses: [ok(sseData([{ id: "x" }]))] },
	{ name: "mistral/http-429-body", model: mLarge, context: simple(), options: { apiKey: "m-key" }, responses: [json(429, { object: "error", message: "rate limited" })] },
	{ name: "mistral/http-500-empty", model: mLarge, context: simple(), options: { apiKey: "m-key" }, responses: [json(500, "   ")] },
	{ name: "mistral/abort-after-first-delta", model: mLarge, context: simple(), options: { apiKey: "m-key" }, abortOn: { type: "text_delta", nth: 1 }, responses: [ok(sseData([mChunk({ content: "one" }), mChunk({ content: "two" }), mDone("stop")]))] },
	{ name: "mistral/missing-key", model: mLarge, context: simple(), options: {}, responses: [] },
	{ name: "mistral/simple-effort-small", fn: "streamSimple", model: mSmall, context: simple(), options: { apiKey: "m-key", reasoning: "low", toolChoice: "auto" }, responses: [ok(sseData([mDone("stop")]))] },
	{ name: "mistral/simple-effort-medium-default", fn: "streamSimple", model: mModel("mistral-medium-2508"), context: simple(), options: { apiKey: "m-key", reasoning: "medium" }, responses: [ok(sseData([mDone("stop")]))] },
	{ name: "mistral/simple-prompt-mode", fn: "streamSimple", model: mMag, context: simple(), options: { apiKey: "m-key", reasoning: "high" }, responses: [ok(sseData([mDone("stop")]))] },
	{ name: "mistral/simple-no-reasoning", fn: "streamSimple", model: mMag, context: simple(), options: { apiKey: "m-key" }, responses: [ok(sseData([mDone("stop")]))] },
	{ name: "mistral/simple-non-reasoning-model", fn: "streamSimple", model: mLarge, context: simple(), options: { apiKey: "m-key", reasoning: "high" }, responses: [ok(sseData([mDone("stop")]))] },
	{ name: "mistral/simple-missing-key", fn: "streamSimple", model: mLarge, context: simple(), options: {}, responses: [] },
];

// ---------------------------------------------------------------------------------------------- Vertex (API-key mode)
// ADC mode is not recorded: google-auth-library would read the machine's credentials.

const vModel = (id, extra = {}) => ({ ...model(id), api: "google-vertex", provider: "google-vertex", baseUrl: "https://{location}-aiplatform.googleapis.com", ...extra });
const v3 = vModel("gemini-3-pro-preview", { thinkingLevelMap: { off: null, minimal: "low", low: "low", medium: "high", high: "high" } });
const vlite = vModel("gemini-2.5-flash-lite", { baseUrl: "https://proxy.example.com/vertex/v1beta1", headers: { "X-Model": "m" } });
const vertexScenarios = [
	{
		name: "vertex/gemini3-history-tools",
		model: v3,
		context: { messages: history(v3) },
		options: { apiKey: " v-key ", temperature: 0.2, maxTokens: 1000, toolChoice: "auto", thinking: { enabled: true, level: "LOW" } },
		responses: [ok(data([
			{ ...cand([{ text: "Hmm", thought: true, thoughtSignature: SIG }]), responseId: "vr-1" },
			cand([{ text: "Done." }]),
			cand([{ functionCall: { name: "read", args: { path: "x" } } }]),
			{ ...cand([], "STOP"), usageMetadata: usage() },
		]))],
	},
	{ name: "vertex/custom-base-url-lite", model: vlite, context: simple(), options: { apiKey: "v-key", headers: { "X-Extra": "1" } }, responses: [ok(data([cand([{ text: "ok" }], "MAX_TOKENS")]))] },
	{ name: "vertex/custom-base-url-no-version", model: vModel("gemini-2.5-pro", { baseUrl: "https://proxy.example.com/vertex/" }), context: simple(), options: { apiKey: "v-key" }, responses: [ok(data([cand([{ text: "ok" }], "STOP")]))] },
	{ name: "vertex/no-finish-reason", model: v3, context: simple(), options: { apiKey: "v-key" }, responses: [ok(data([cand([{ text: "par" }])]))] },
	{ name: "vertex/http-403", model: v3, context: simple(), options: { apiKey: "v-key" }, responses: [json(403, { error: { code: 403, message: "denied", status: "PERMISSION_DENIED" } })] },
	{ name: "vertex/placeholder-key-needs-project", model: v3, context: simple(), options: { apiKey: "<authenticated>" }, responses: [] },
	{ name: "vertex/marker-key-needs-location", model: v3, context: simple(), options: { apiKey: "gcp-vertex-credentials", project: "p-1" }, responses: [] },
	{ name: "vertex/simple-lite-minimal", fn: "streamSimple", model: vlite, context: simple(), options: { apiKey: "v-key", reasoning: "minimal" }, responses: [ok(data([cand([{ text: "ok" }], "STOP")]))] },
	{ name: "vertex/simple-no-key-needs-project", fn: "streamSimple", model: v3, context: simple(), options: { reasoning: "low" }, responses: [] },
];

const results = [];
for (const scenario of scenarios) results.push(await run(scenario));
const vertexResults = [];
for (const scenario of vertexScenarios) vertexResults.push(await run(scenario));
const mistralResults = [];
for (const scenario of mistralScenarios) mistralResults.push(await run(scenario));

const units = {
	usesThinkingLevel: ["gemini-3-flash-preview", "gemini-3.1-pro-preview", "Gemini-3.8-Flash", "gemini-2.5-pro", "gemini-flash-latest", "gemini-flash-lite-latest", "gemma-4-it", "gemma4-x", "gemma-3", "gemini-3-nano"].map((id) => ({ id, output: shared.usesGoogleThinkingLevel({ id }) })),
	requiresToolCallId: ["claude-3", "gpt-oss-20b", "gemini-3-pro", "gemini-live-3-x", "gemini-2.5-pro", "gemini-10-x", "gemma-3", "Gemini-3-pro"].map((id) => ({ id, output: shared.requiresToolCallId(id) })),
	stopReasonString: ["STOP", "MAX_TOKENS", "SAFETY", "", "OTHER"].map((reason) => ({ reason, output: shared.mapStopReasonString(reason) })),
};

const out = join(here, "google.json");
writeFileSync(out, `${JSON.stringify({ generator: "crates/ai/fixtures/generate-more.mjs", now: NOW, units, scenarios: results }, null, "\t")}\n`);
console.log(`wrote ${results.length} scenarios to ${out}`);
const vertexOut = join(here, "google-vertex.json");
writeFileSync(vertexOut, `${JSON.stringify({ generator: "crates/ai/fixtures/generate-more.mjs", now: NOW, scenarios: vertexResults }, null, "\t")}\n`);
console.log(`wrote ${vertexResults.length} scenarios to ${vertexOut}`);
const mistralOut = join(here, "mistral.json");
writeFileSync(mistralOut, `${JSON.stringify({ generator: "crates/ai/fixtures/generate-more.mjs", now: NOW, scenarios: mistralResults }, null, "\t")}\n`);
console.log(`wrote ${mistralResults.length} scenarios to ${mistralOut}`);
