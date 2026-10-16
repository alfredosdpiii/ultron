// Generates crates/inference/fixtures/providers.json: one rlm.infer frame per provider API, served by the TS host
// (NativeRlmHost + InferenceRuntime of ultron/rlm/inference.ts) whose frame lanes call the real pi-ai `streamSimple`
// of the model's API (packages/ai/src/api/<api>.ts, over `normalizeContext` as compat.ts calls it) with the runtime's transform_context system prompt and its
// before_payload hook as `onPayload` (the contract schema and the output cap at the request's tranche), as the agent
// harness wires them. Requests go to a 127.0.0.1 HTTP server answering with canned responses taken from the ai
// crate's provider fixtures (a WebSocket upgrade is refused, so Codex falls back to SSE); the Claude Code CLI frame
// runs the ai crate's fake `claude` (never the real one). Recorded: each request (method, path, JSON body) or the
// fake CLI's log, and the host's rlm.infer result. Also the capOutputTokens / withContractSchema table.
// Usage: node crates/inference/fixtures/generate-providers.mjs (loopback only: offline-guard.mjs is loaded first).
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { zstdDecompressSync } from "node:zlib";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../../..");
const pkg = join(root, "packages/coding-agent");
const aiFixtures = join(root, "crates/ai/fixtures");
const load = (path) => import(pathToFileURL(join(pkg, path)).href);
const { normalizeContext } = await import(pathToFileURL(join(root, "packages/ai/src/utils/transcript.ts")).href);
const api = (name) => import(pathToFileURL(join(root, "packages/ai/src/api", `${name}.ts`)).href);

for (const name of Object.keys(process.env)) {
	if (/^(PI_|AZURE_|OPENAI_|ANTHROPIC_|GOOGLE_|GEMINI_|MISTRAL_|CLOUDFLARE_|ULTRON_|KIMI_|AWS_|VERTEX|CLAUDE)/.test(name))
		delete process.env[name];
}
process.env.AWS_EC2_METADATA_DISABLED = "true";

const { createInferenceRuntime, createMemoryFrameStore, capOutputTokens, withContractSchema } = await load("src/ultron/rlm/inference.ts");
const { NativeRlmHost } = await load("src/ultron/rlm/native-host.ts");
const { memoryStore, memoryDefinitionStore } = await load("test/ultron-host-fixtures.ts");

const scenarioOf = (file, name) => {
	const found = JSON.parse(readFileSync(join(aiFixtures, file), "utf8")).scenarios.find((s) => s.name === name);
	if (!found) throw new Error(`no scenario ${name} in ${file}`);
	return found;
};
const responsesOf = (file, name) => scenarioOf(file, name).responses;
const modelOf = (file, name) => scenarioOf(file, name).model;
/** The model with its base URL moved to the local server (`<host>` in the fixture). */
const local = (model) => {
	const url = new URL(model.baseUrl);
	return { ...model, baseUrl: `http://<host>${url.pathname === "/" ? "" : url.pathname}` };
};

const piText = {
	chunks: [
		'data: {"type":"start"}\n\n',
		'data: {"type":"text_start","contentIndex":0}\n\n',
		'data: {"type":"text_delta","contentIndex":0,"delta":"ok"}\n\n',
		'data: {"type":"text_end","contentIndex":0,"content":"ok"}\n\n',
		'data: {"type":"done","reason":"stop","usage":{"input":10,"output":5,"cacheRead":2,"cacheWrite":1,"totalTokens":18,"cost":{"input":0.1,"output":0.2,"cacheRead":0.01,"cacheWrite":0.02,"total":0.33}}}\n\n',
	],
};
const awsEnv = {
	AWS_ACCESS_KEY_ID: "AKIDEXAMPLE",
	AWS_SECRET_ACCESS_KEY: "secret",
	AWS_REGION: "us-east-1",
	AWS_BEDROCK_FORCE_HTTP1: "1",
	AWS_SHARED_CREDENTIALS_FILE: "<fixtures>/aws/credentials",
	AWS_CONFIG_FILE: "<fixtures>/aws/config",
};

// Each frame: the model (its API), the key and provider env its requests carry, the rlm.infer payload, and the
// canned responses in request order.
export const scenarios = [
	{ name: "anthropic-messages", model: local(modelOf("providers.json", "anthropic/env-long-retention")), apiKey: "sk-ant-test", payload: { task: "Say ok" }, responses: responsesOf("providers.json", "anthropic/env-long-retention") },
	{ name: "anthropic-messages-thinking", model: local(modelOf("providers.json", "anthropic/env-long-retention")), apiKey: "sk-ant-test", payload: { task: "Say ok", thinking: "high", budget: { tokens: 20000 } }, responses: responsesOf("providers.json", "anthropic/env-long-retention") },
	{ name: "openai-completions", model: local(modelOf("providers.json", "completions/deepseek-detection")), apiKey: "sk-ds-test", payload: { task: "Say ok", context: [{ kind: "text", text: "a\nb", label: "notes" }], budget: { tokens: 2000 } }, responses: responsesOf("providers.json", "completions/deepseek-detection") },
	{ name: "openai-responses", model: local(modelOf("providers.json", "responses/refusal-and-final-answer-phase")), apiKey: "sk-openai-test", payload: { task: "Say ok", contract: { type: "string" } }, responses: responsesOf("azure.json", "azure/simple-off") },
	{ name: "azure-openai-responses", model: local(modelOf("azure.json", "azure/simple-off")), apiKey: "az-key", payload: { task: "Say ok", thinking: "low" }, responses: responsesOf("azure.json", "azure/simple-off") },
	{ name: "openai-codex-responses", model: local(modelOf("codex.json", "codex/simple-reasoning-sse")), apiKey: scenarioOf("codex.json", "codex/simple-reasoning-sse").options.apiKey, payload: { task: "Say ok" }, responses: responsesOf("codex.json", "codex/simple-reasoning-sse") },
	{ name: "google-generative-ai", model: local(modelOf("google.json", "google/simple-no-reasoning")), apiKey: "g-key", payload: { task: "Say ok", budget: { tokens: 1000 } }, responses: responsesOf("google.json", "google/simple-no-reasoning") },
	{ name: "google-vertex", model: local(modelOf("google-vertex.json", "vertex/simple-lite-minimal")), apiKey: "v-key", payload: { task: "Say ok", thinking: "minimal" }, responses: responsesOf("google-vertex.json", "vertex/simple-lite-minimal") },
	{ name: "mistral-conversations", model: local(modelOf("mistral.json", "mistral/no-cache-retention-none")), apiKey: "m-key", payload: { task: "Say ok" }, responses: responsesOf("mistral.json", "mistral/no-cache-retention-none") },
	{ name: "bedrock-converse-stream", model: local(modelOf("bedrock.json", "bedrock/bearer-token")), env: awsEnv, payload: { task: "Say ok", budget: { tokens: 4000 } }, responses: responsesOf("bedrock.json", "bedrock/bearer-token") },
	{ name: "pi-messages", model: local(modelOf("pi-messages.json", "pi/simple")), apiKey: "rk", payload: { task: "Say ok" }, responses: [piText] },
	{ name: "claude-code-cli", model: modelOf("claude-code.json", "claude/result-only-text"), payload: { task: "Pick a number", contract: { type: "integer" } }, cliReply: '{"type":"result","subtype":"success","is_error":false,"result":"42","stop_reason":"end_turn","usage":{"input_tokens":30,"output_tokens":2}}\n' },
];

// The local provider: records each request, answers with the next canned response; WebSocket upgrades are refused.
let queue = [];
let requests = [];
const server = createServer((req, res) => {
	const parts = [];
	req.on("data", (chunk) => parts.push(chunk));
	req.on("end", () => {
		// Codex sends its SSE body zstd-compressed where node:zlib has zstd (the Rust port sends it plain).
		const raw = Buffer.concat(parts);
		const text = (req.headers["content-encoding"] === "zstd" ? zstdDecompressSync(raw) : raw).toString("utf8");
		let body = null;
		try {
			body = text === "" ? null : JSON.parse(text);
		} catch {
			body = text;
		}
		requests.push({ method: req.method, url: req.url, body });
		const canned = queue.shift();
		if (!canned) {
			res.writeHead(500, { "content-type": "application/json" });
			res.end('{"error":{"message":"unexpected request"}}');
			return;
		}
		res.writeHead(canned.status ?? 200, canned.headers ?? { "content-type": "text/event-stream" });
		for (const chunk of canned.chunks) res.write(typeof chunk === "string" ? chunk : Buffer.from(chunk));
		res.end();
	});
});
server.on("upgrade", (_req, socket) => {
	socket.end("HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
});
await new Promise((ready) => server.listen(0, "127.0.0.1", ready));
const host = `127.0.0.1:${server.address().port}`;

const withHost = (value) => JSON.parse(JSON.stringify(value).replaceAll("<host>", host).replaceAll("<fixtures>", aiFixtures));

function lanes(scenario, model, streamSimple, env) {
	const handlers = new Map();
	const run = async (name, event) => {
		let result;
		for (const handler of handlers.get(name) ?? []) result = (await handler(event, {})) ?? result;
		return result;
	};
	const made = new Map();
	return {
		hooks: {
			on(name, handler) {
				const list = handlers.get(name) ?? [];
				list.push(handler);
				handlers.set(name, list);
				return () => list.splice(list.indexOf(handler), 1);
			},
		},
		lane: async (name) => {
			if (made.has(name)) return made.get(name);
			const entries = [];
			const conversation = [];
			let thinking = "off";
			let counter = 0;
			const lane = {
				getActiveTools: async () => [],
				setActiveTools: async () => {},
				setModel: async () => {},
				setThinkingLevel: async (level) => {
					thinking = level;
				},
				getModel: async () => model,
				steer: async () => ({ ok: true, value: {} }),
				abort: async () => ({ ok: true }),
				prompt: async (message) => {
					const fromTipId = entries.at(-1)?.id ?? null;
					conversation.push({ role: "user", content: message, timestamp: 0 });
					const transformed = await run("transform_context", { lane: name, messages: [], systemPrompt: "ROOT" });
					const systemPrompt = transformed?.systemPrompt ?? "ROOT";
					const stream = streamSimple(model, normalizeContext({ systemPrompt, messages: conversation }), {
						...(scenario.apiKey === undefined ? {} : { apiKey: scenario.apiKey }),
						...(env === undefined ? {} : { env }),
						...(thinking === "off" ? {} : { reasoning: thinking }),
						onPayload: async (payload, requestModel) =>
							(await run("before_payload", { lane: name, model: requestModel, payload }))?.payload ?? payload,
					});
					const assistant = await stream.result();
					await run("after_response", { lane: name, message: assistant });
					conversation.push(assistant);
					counter += 1;
					const id = `${name}#${counter}`;
					entries.push({ id, type: "message", message: assistant });
					return { ok: true, value: { status: "completed", tipId: id, fromTipId } };
				},
				findEntries: async (query) => {
					const start = query?.stopAtId === undefined ? 0 : entries.findIndex((entry) => entry.id === query.stopAtId) + 1;
					return entries.slice(start).reverse();
				},
			};
			made.set(name, lane);
			return lane;
		},
	};
}

const normalize = (value) =>
	JSON.parse(JSON.stringify(value).replace(/frame-[0-9a-f-]{36}/g, "frame-ID").replaceAll(host, "<host>"));

const cwd = mkdtempSync(join(tmpdir(), "ultron-frame-providers-"));
const results = [];
try {
	for (const scenario of scenarios) {
		const model = withHost(scenario.model);
		const { streamSimple } = await api(model.api);
		let env = scenario.env === undefined ? undefined : withHost(scenario.env);
		let log;
		if (scenario.cliReply !== undefined) {
			const dir = mkdtempSync(join(cwd, "claude-"));
			writeFileSync(join(dir, "reply"), scenario.cliReply);
			log = join(dir, "log");
			writeFileSync(log, "");
			env = {
				ULTRON_CLAUDE_CODE_BIN: join(aiFixtures, "fake-claude/claude"),
				ULTRON_CLAUDE_CODE_WARM: "0",
				FAKE_CLAUDE_REPLY: join(dir, "reply"),
				FAKE_CLAUDE_LOG: log,
			};
		}
		queue = (scenario.responses ?? []).map((r) => ({ ...r, chunks: [...r.chunks] }));
		requests = [];
		const harness = lanes(scenario, model, streamSimple, env);
		// No usage ledger, as the Rust frames under test have no task host (the TS ledger also refuses the Claude Code
		// CLI's `cost.total: null`, "Invalid usage ledger cost").
		const inference = createInferenceRuntime({ contextDir: join(cwd, "context"), traces: createMemoryFrameStore(), env: {} });
		inference.install(harness);
		const nativeHost = new NativeRlmHost(harness, {}, { store: memoryStore(), definitionStore: memoryDefinitionStore(), frames: inference.executor, modules: [inference.module] });
		try {
			const result = await nativeHost.handle("rlm.infer", structuredClone(scenario.payload), {}, undefined);
			if (queue.length) throw new Error(`${scenario.name} left ${queue.length} responses unused`);
			// The frame's task id belongs to the TS task host; frames without one have none.
			delete result.task_id;
			const record = { ...scenario, result: normalize(result) };
			if (log === undefined) record.requests = normalize(requests);
			else record.cli = readFileSync(log, "utf8").replaceAll(cwd, "<dir>").replaceAll(aiFixtures, "<fixtures>").replace(/claude-[A-Za-z0-9]{6}/g, "claude-DIR");
			delete record.cliReply;
			if (scenario.cliReply !== undefined) record.cliReply = scenario.cliReply;
			results.push(record);
		} finally {
			await nativeHost.close?.();
		}
	}
} finally {
	server.close();
	rmSync(cwd, { recursive: true, force: true });
}

// capOutputTokens / withContractSchema over payload shapes of every API family.
const payloads = [
	["anthropic-messages", "anthropic", { max_tokens: 32000 }, 500],
	["anthropic-messages", "anthropic", { max_tokens: 20000, thinking: { type: "enabled", budget_tokens: 16000 } }, 4000],
	["anthropic-messages", "anthropic", { max_tokens: 3000, thinking: { type: "enabled", budget_tokens: 2000 } }, 900],
	["anthropic-messages", "anthropic", { max_tokens: 40, thinking: { type: "adaptive" } }, 64],
	["openai-completions", "openai", { model: "m" }, 300],
	["openai-completions", "azure-openai", { model: "m" }, 300],
	["openai-completions", "deepseek", { model: "m" }, 300],
	["openai-completions", "deepseek", { max_completion_tokens: 8192 }, 300],
	["openai-responses", "openai", { max_output_tokens: 8192 }, 10],
	["openai-responses", "openai", { max_output_tokens: 8 }, 300],
	["openai-responses", "openai", { model: "m" }, 300],
	["google-generative-ai", "google", { generationConfig: { maxOutputTokens: 32000, temperature: 0 } }, 700],
	["google-vertex", "google-vertex", { config: { maxOutputTokens: 100 } }, 700],
	["bedrock-converse-stream", "amazon-bedrock", { inferenceConfig: { maxTokens: 8192 } }, 1234],
	["mistral-conversations", "mistral", { maxTokens: 32000 }, 512],
	["pi-messages", "radius", { maxTokens: 50, options: {} }, 512],
	["claude-code-cli", "claude-code", { max_tokens: 32000 }, 512],
	["openai-completions", "x", [1, 2], 5],
	["openai-completions", "x", null, 5],
	["anthropic-messages", "anthropic", { max_tokens: "100" }, 5],
	["anthropic-messages", "anthropic", { max_tokens: 0.5 }, 5],
];
const contracts = [{ type: "integer" }, undefined];
const caps = [];
for (const [api, provider, payload, limit] of payloads) {
	for (const contract of contracts) {
		const out = capOutputTokens(withContractSchema(structuredClone(payload), contract, api), limit, api, provider);
		caps.push({ api, provider, payload, limit, contract: contract ?? null, out: out ?? null });
	}
}

assertOffline();
writeFileSync(join(here, "providers.json"), `${JSON.stringify({ scenarios: results, caps }, null, "\t")}\n`);
