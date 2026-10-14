// Generates crates/ai/fixtures/stream-errors.json: the `stopReason` / `errorMessage` the TypeScript providers end a
// stream with when the server answers an error, recorded against a real local HTTP server (no stubbed fetch, so the
// SDKs' own status handling runs): Anthropic's SSE `error` event and HTTP 529 / 429 / 500 bodies, and the same kind
// of failures for OpenAI completions and responses, Google and Mistral. Retries are off (`maxRetries: 0`) so each
// scenario is one response. The Rust test (tests/stream_errors.rs) serves the same responses and compares.
// Usage: node crates/ai/fixtures/generate-stream-errors.mjs
import { writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const src = (path) => import(pathToFileURL(join(root, "packages/ai/src", path)).href);

for (const name of Object.keys(process.env)) {
	if (/^(PI_|OPENAI_|ANTHROPIC_|GEMINI_|GOOGLE_|MISTRAL_)/.test(name)) delete process.env[name];
}
const NOW = 1767225600000;
Date.now = () => NOW;

const API = {
	"anthropic-messages": await src("api/anthropic-messages.ts"),
	"openai-completions": await src("api/openai-completions.ts"),
	"openai-responses": await src("api/openai-responses.ts"),
	"google-generative-ai": await src("api/google-generative-ai.ts"),
	"mistral-conversations": await src("api/mistral-conversations.ts"),
};

// One canned response per request, in order.
let queue = [];
const server = createServer((req, res) => {
	req.resume();
	req.on("end", () => {
		const canned = queue.shift() ?? { status: 599, headers: { "content-type": "text/plain" }, body: "unexpected request" };
		res.writeHead(canned.status, canned.headers);
		res.end(canned.body);
	});
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const base = `http://127.0.0.1:${server.address().port}`;

const sseBody = (events) => events.map(([event, data]) => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`).join("");
const dataBody = (items) => items.map((d) => `data: ${typeof d === "string" ? d : JSON.stringify(d)}\n\n`).join("");
const stream = (body) => ({ status: 200, headers: { "content-type": "text/event-stream" }, body });
const json = (status, body) => ({ status, headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
const text = (status, body) => ({ status, headers: { "content-type": "text/plain" }, body });

const model = (api, provider, path, id = "test-model") => ({
	id,
	name: "Test Model",
	api,
	provider,
	baseUrl: `{base}${path}`,
	reasoning: false,
	input: ["text"],
	cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 },
	contextWindow: 200000,
	maxTokens: 32000,
});
const context = { messages: [{ role: "user", content: "hi", timestamp: 2 }] };

const aModel = model("anthropic-messages", "anthropic", "");
const aStart = ["message_start", { type: "message_start", message: { id: "msg_01", type: "message", role: "assistant", model: "test-model", content: [], usage: { input_tokens: 10, output_tokens: 1 } } }];
const aError = (type, message) => ({ type: "error", error: { type, message } });
const cModel = model("openai-completions", "openai", "/v1");
const rModel = model("openai-responses", "openai", "/v1");
const gModel = model("google-generative-ai", "google", "", "gemini-test");
const mModel = model("mistral-conversations", "mistral", "", "mistral-test");

const scenarios = [
	{ name: "anthropic/sse-error-overloaded", model: aModel, responses: [stream(sseBody([aStart, ["error", aError("overloaded_error", "Overloaded")]]))] },
	{ name: "anthropic/sse-error-first", model: aModel, responses: [stream(sseBody([["error", aError("overloaded_error", "Overloaded")]]))] },
	{ name: "anthropic/http-529", model: aModel, responses: [json(529, aError("overloaded_error", "Overloaded"))] },
	{ name: "anthropic/http-429", model: aModel, responses: [json(429, aError("rate_limit_error", "Number of request tokens has exceeded your per-minute rate limit"))] },
	{ name: "anthropic/http-500-json", model: aModel, responses: [json(500, aError("api_error", "Internal server error"))] },
	{ name: "anthropic/http-500-text", model: aModel, responses: [text(500, "upstream exploded")] },
	{ name: "openai-completions/http-429", model: cModel, responses: [json(429, { error: { message: "Rate limit reached", type: "rate_limit_exceeded", code: "rate_limit_exceeded" } })] },
	{ name: "openai-completions/http-500", model: cModel, responses: [json(500, { error: { message: "The server had an error", type: "server_error" } })] },
	{ name: "openai-completions/stream-error-chunk", model: cModel, responses: [stream(dataBody([{ id: "c1", object: "chat.completion.chunk", choices: [{ index: 0, delta: { content: "par" } }] }, { error: { message: "Provider overloaded", code: 529 } }]))] },
	{ name: "openai-responses/http-500", model: rModel, responses: [json(500, { error: { message: "The server had an error", type: "server_error" } })] },
	{ name: "openai-responses/sse-error-event", model: rModel, responses: [stream(sseBody([["error", { type: "error", code: "server_error", message: "The model is overloaded", sequence_number: 1 }]]))] },
	{ name: "openai-responses/response-failed", model: rModel, responses: [stream(sseBody([["response.failed", { type: "response.failed", sequence_number: 1, response: { id: "resp_1", status: "failed", error: { code: "server_error", message: "Something went wrong" }, output: [] } }]]))] },
	{ name: "google/http-503", model: gModel, responses: [json(503, { error: { code: 503, message: "The model is overloaded. Please try again later.", status: "UNAVAILABLE" } })] },
	{ name: "mistral/http-429", model: mModel, responses: [json(429, { object: "error", message: "Requests rate limit exceeded", type: "rate_limited", code: "1300" })] },
];

const options = { apiKey: "test-key", maxRetries: 0, env: { PI_CACHE_RETENTION: "unset" } };
const live = (m) => ({ ...m, baseUrl: m.baseUrl.replace("{base}", base) });
const results = [];
for (const scenario of scenarios) {
	queue = scenario.responses.map((r) => ({ ...r }));
	const record = { name: scenario.name, model: scenario.model, context, options, responses: scenario.responses };
	try {
		const message = await API[scenario.model.api].stream(live(scenario.model), context, options).result();
		record.result = { stopReason: message.stopReason, errorMessage: message.errorMessage ?? null };
		record.thrown = null;
	} catch (e) {
		record.result = null;
		record.thrown = e.message;
	}
	record.unanswered = queue.length;
	results.push(record);
}
server.close();
const out = join(here, "stream-errors.json");
writeFileSync(out, `${JSON.stringify({ generator: "crates/ai/fixtures/generate-stream-errors.mjs", scenarios: results }, null, "\t")}\n`);
console.log(`wrote ${results.length} scenarios to ${out}`);
