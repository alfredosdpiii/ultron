// Generates crates/ai/fixtures/provider-headers.json from this worktree's TypeScript model runtime (`ModelRuntime`):
// request headers a provider or SDK adds on its own.
// - OpenCode Zen and OpenCode Go (providers/opencode-headers.ts `withOpenCodeSessionHeader`): every API sends
//   `x-opencode-session: <sessionId>` unless the caller set that header (any case); no session id, no header.
// - The OpenAI SDK client (node_modules/openai/client.js: `organization = readEnv('OPENAI_ORG_ID')`,
//   `project = readEnv('OPENAI_PROJECT_ID')`, sent as OpenAI-Organization/OpenAI-Project under the auth, default and
//   request headers): OpenAI completions, responses and Azure (AzureOpenAI passes them through to OpenAI); not the
//   Anthropic SDK.
// Each case: `completeSimple` with the case's options, captured by a fetch stand-in that answers 400 (nothing leaves
// the process); recorded are the request's URL and the values of the headers above. Fresh temp dir per case with only
// the environment it names (PATH=/usr/bin:/bin, PI_OFFLINE=1).
// Usage: node crates/ai/fixtures/generate-provider-headers.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const scratch = mkdtempSync(join(tmpdir(), "ai-provider-headers-"));
const baseEnv = (dir) => ({
	HOME: join(dir, "home"),
	PATH: "/usr/bin:/bin",
	PI_OFFLINE: "1",
	ULTRON_CODING_AGENT_DIR: join(dir, "agent"),
	ULTRON_SERVER_DIR: join(dir, "server"),
});
for (const key of Object.keys(process.env)) delete process.env[key];
Object.assign(process.env, baseEnv(scratch));

const { ModelRuntime } = await import(pathToFileURL(join(root, "packages/coding-agent/src/core/model-runtime.ts")).href);

const NAMES = ["x-opencode-session", "openai-organization", "openai-project"];
const sent = [];
globalThis.fetch = async (input, init = {}) => {
	const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
	const headers = new Headers(init.headers ?? (typeof input === "object" && "headers" in input ? input.headers : {}));
	const seen = {};
	for (const name of NAMES) if (headers.has(name)) seen[name] = headers.get(name);
	sent.push({ url, headers: seen });
	return new Response(JSON.stringify({ error: { message: "recorded" } }), {
		status: 400,
		headers: { "content-type": "application/json" },
	});
};

const OPENCODE_ENV = { OPENCODE_API_KEY: "oc-fake" };
const OPENAI_ENV = { OPENAI_API_KEY: "sk-openai-fake" };
const cases = [
	{ name: "opencode completions with a session", call: ["opencode", "big-pickle"], env: OPENCODE_ENV, options: { sessionId: "sess-1" } },
	{ name: "opencode anthropic with a session", call: ["opencode", "claude-fable-5"], env: OPENCODE_ENV, options: { sessionId: "sess-2" } },
	{ name: "opencode google with a session", call: ["opencode", "gemini-3-flash"], env: OPENCODE_ENV, options: { sessionId: "sess-3" } },
	{ name: "opencode responses with a session", call: ["opencode", "gpt-5"], env: OPENCODE_ENV, options: { sessionId: "sess-4" } },
	{ name: "opencode go anthropic with a session", call: ["opencode-go", "claude-haiku-5-5"], env: OPENCODE_ENV, options: { sessionId: "sess-5" } },
	{ name: "opencode go completions with a session", call: ["opencode-go", "deepseek-v4-flash"], env: OPENCODE_ENV, options: { sessionId: "sess-6" } },
	{ name: "opencode without a session", call: ["opencode", "big-pickle"], env: OPENCODE_ENV, options: {} },
	{ name: "opencode with an empty session id", call: ["opencode", "big-pickle"], env: OPENCODE_ENV, options: { sessionId: "" } },
	{
		name: "opencode caller header wins in any case",
		call: ["opencode", "big-pickle"],
		env: OPENCODE_ENV,
		options: { sessionId: "sess-7", headers: { "X-OpenCode-Session": "mine" } },
	},
	{ name: "openai responses with a session has no opencode header", call: ["openai", "gpt-4"], env: OPENAI_ENV, options: { sessionId: "sess-8" } },
	{
		name: "openai responses organization and project",
		call: ["openai", "gpt-4"],
		env: { ...OPENAI_ENV, OPENAI_ORG_ID: " org-fake ", OPENAI_PROJECT_ID: "proj-fake" },
		options: {},
	},
	{
		name: "openai blank organization is unset",
		call: ["openai", "gpt-4"],
		env: { ...OPENAI_ENV, OPENAI_ORG_ID: "  ", OPENAI_PROJECT_ID: "" },
		options: {},
	},
	{
		name: "caller headers override the organization",
		call: ["openai", "gpt-4"],
		env: { ...OPENAI_ENV, OPENAI_ORG_ID: "org-fake", OPENAI_PROJECT_ID: "proj-fake" },
		options: { headers: { "openai-organization": "org-caller" } },
	},
	{
		name: "opencode completions organization",
		call: ["opencode", "big-pickle"],
		env: { ...OPENCODE_ENV, OPENAI_ORG_ID: "org-fake" },
		options: {},
	},
	{
		name: "azure responses organization and project",
		call: ["azure-openai-responses", "gpt-4"],
		env: {
			AZURE_OPENAI_API_KEY: "az-fake",
			AZURE_OPENAI_BASE_URL: "https://fake-resource.openai.azure.com/openai/v1",
			OPENAI_ORG_ID: "org-fake",
			OPENAI_PROJECT_ID: "proj-fake",
		},
		options: {},
	},
	{
		name: "anthropic sends no organization",
		call: ["anthropic", "claude-haiku-4-5"],
		env: { ANTHROPIC_API_KEY: "sk-ant-fake", OPENAI_ORG_ID: "org-fake", OPENAI_PROJECT_ID: "proj-fake" },
		options: {},
	},
];

const out = [];
for (const spec of cases) {
	const dir = join(scratch, `case-${out.length}`);
	mkdirSync(join(dir, "home"), { recursive: true });
	mkdirSync(join(dir, "agent"), { recursive: true });
	writeFileSync(join(dir, "agent", "models.json"), "{}");
	writeFileSync(join(dir, "agent", "auth.json"), "{}");
	for (const key of Object.keys(process.env)) delete process.env[key];
	Object.assign(process.env, baseEnv(dir), spec.env);
	const runtime = await ModelRuntime.create({
		authPath: join(dir, "agent", "auth.json"),
		modelsPath: join(dir, "agent", "models.json"),
	});
	const [provider, id] = spec.call;
	const model = runtime.getModel(provider, id);
	if (!model) throw new Error(`${spec.name}: no model ${provider}/${id}`);
	sent.length = 0;
	const message = await runtime.completeSimple(
		model,
		{ messages: [{ role: "user", content: "go", timestamp: 0 }] },
		{ maxRetries: 0, ...spec.options },
	);
	if (sent.length !== 1) throw new Error(`${spec.name}: ${sent.length} requests (${message.errorMessage})`);
	out.push({ name: spec.name, model: `${provider}/${id}`, env: spec.env, options: spec.options, request: sent[0] });
}

rmSync(scratch, { recursive: true, force: true });
writeFileSync(join(here, "provider-headers.json"), `${JSON.stringify({ cases: out }, null, "\t")}\n`);
assertOffline();
