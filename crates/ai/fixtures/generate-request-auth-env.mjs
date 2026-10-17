// Generates crates/ai/fixtures/request-auth-env.json from this worktree's TypeScript model runtime (`ModelRuntime`):
// the api-key auth that adds headers or provider env to a request, and the request that goes out.
// - Anthropic (providers/anthropic.ts): a stored key first, then ANTHROPIC_AUTH_TOKEN as `Authorization: Bearer`
//   (no key, so no x-api-key; it wins over ANTHROPIC_API_KEY), then ANTHROPIC_OAUTH_TOKEN/ANTHROPIC_API_KEY.
//   A models.json apiKey is resolved as the credential's key (provider-composer.ts composeApiKeyAuth).
// - Cloudflare (providers/cloudflare-auth.ts): the key and the account (and gateway) id, merged per field from the
//   stored /login credential and the environment, become the request env (`applyAuth`), which fills the base URL
//   placeholders; the AI Gateway sends `cf-aig-authorization: Bearer <key>` with Authorization/x-api-key removed.
// - A stored api-key credential's `env` (Vertex project/location, any provider) is the request env.
// For each case: `getAuth` (apiKey, headers, env) and, for wire cases, the request `completeSimple` sends (URL and
// auth headers), captured by a fetch stand-in that answers 400 (nothing leaves the process).
// Each case runs in a fresh temp dir with only the environment it names (PATH=/usr/bin:/bin, PI_OFFLINE=1). Paths are
// recorded as <dir>.
// Usage: node crates/ai/fixtures/generate-request-auth-env.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const scratch = mkdtempSync(join(tmpdir(), "ai-request-auth-env-"));
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

// The fetch stand-in: records what the SDK clients send and answers 400 without touching the network.
const sent = [];
globalThis.fetch = async (input, init = {}) => {
	const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
	const headers = new Headers(init.headers ?? (typeof input === "object" && "headers" in input ? input.headers : {}));
	const auth = {};
	for (const name of ["authorization", "x-api-key", "cf-aig-authorization"]) {
		if (headers.has(name)) auth[name] = headers.get(name);
	}
	sent.push({ url, headers: auth });
	return new Response(JSON.stringify({ error: { message: "recorded" } }), {
		status: 400,
		headers: { "content-type": "application/json" },
	});
};

const GATEWAY_ANTHROPIC = "claude-fable-5";
const GATEWAY_OPENAI = "workers-ai/@cf/deepseek-ai/deepseek-v4-flash-0731";
const WORKERS = "@cf/deepseek-ai/deepseek-v4-flash-0731";
const ANTHROPIC = "claude-haiku-4-5";

const cases = [
	{
		name: "anthropic auth token wins over the api key",
		models: {},
		auth: {},
		env: { ANTHROPIC_AUTH_TOKEN: "tok-env", ANTHROPIC_API_KEY: "sk-env" },
		call: ["anthropic", ANTHROPIC],
		wire: true,
	},
	{
		name: "anthropic stored key wins over the auth token",
		models: {},
		auth: { anthropic: { type: "api_key", key: "sk-stored" } },
		env: { ANTHROPIC_AUTH_TOKEN: "tok-env", ANTHROPIC_API_KEY: "sk-env" },
		call: ["anthropic", ANTHROPIC],
		wire: true,
	},
	{
		name: "anthropic stored credential without a key uses the auth token",
		models: {},
		auth: { anthropic: { type: "api_key", env: { EXTRA: "x" } } },
		env: { ANTHROPIC_AUTH_TOKEN: "tok-env", ANTHROPIC_API_KEY: "sk-env" },
		call: ["anthropic", ANTHROPIC],
		wire: true,
	},
	{
		name: "anthropic models.json apiKey wins over the auth token",
		models: { providers: { anthropic: { apiKey: "sk-models" } } },
		auth: {},
		env: { ANTHROPIC_AUTH_TOKEN: "tok-env" },
		call: ["anthropic", ANTHROPIC],
		wire: true,
	},
	{
		name: "anthropic api key alone",
		models: {},
		auth: {},
		env: { ANTHROPIC_API_KEY: "sk-env" },
		call: ["anthropic", ANTHROPIC],
		wire: true,
	},
	{
		name: "anthropic stored key with an env",
		models: {},
		auth: { anthropic: { type: "api_key", key: "sk-stored", env: { ANTHROPIC_EXTRA: "e" } } },
		env: {},
		call: ["anthropic", ANTHROPIC],
		wire: false,
	},
	{
		name: "workers ai stored credential names the account",
		models: {},
		auth: { "cloudflare-workers-ai": { type: "api_key", key: "cf-stored", env: { CLOUDFLARE_ACCOUNT_ID: "acct-stored" } } },
		env: {},
		call: ["cloudflare-workers-ai", WORKERS],
		wire: true,
	},
	{
		name: "workers ai from the environment",
		models: {},
		auth: {},
		env: { CLOUDFLARE_API_KEY: "cf-env", CLOUDFLARE_ACCOUNT_ID: "acct-env" },
		call: ["cloudflare-workers-ai", WORKERS],
		wire: true,
	},
	{
		name: "workers ai stored key takes the account from the environment",
		models: {},
		auth: { "cloudflare-workers-ai": { type: "api_key", key: "cf-stored" } },
		env: { CLOUDFLARE_API_KEY: "cf-env", CLOUDFLARE_ACCOUNT_ID: "acct-env" },
		call: ["cloudflare-workers-ai", WORKERS],
		wire: true,
	},
	{
		name: "ai gateway anthropic from the stored login",
		models: {},
		auth: {
			"cloudflare-ai-gateway": {
				type: "api_key",
				key: "cf-gw",
				env: { CLOUDFLARE_ACCOUNT_ID: "acct-gw", CLOUDFLARE_GATEWAY_ID: "gw-1" },
			},
		},
		env: { ANTHROPIC_API_KEY: "sk-ignored" },
		call: ["cloudflare-ai-gateway", GATEWAY_ANTHROPIC],
		wire: true,
	},
	{
		name: "ai gateway openai completions from the environment",
		models: {},
		auth: {},
		env: { CLOUDFLARE_API_KEY: "cf-env", CLOUDFLARE_ACCOUNT_ID: "acct-env", CLOUDFLARE_GATEWAY_ID: "gw-env" },
		call: ["cloudflare-ai-gateway", GATEWAY_OPENAI],
		wire: true,
	},
	{
		name: "ai gateway stored key with the ids from the environment",
		models: {},
		auth: { "cloudflare-ai-gateway": { type: "api_key", key: "cf-gw" } },
		env: { CLOUDFLARE_ACCOUNT_ID: "acct-env", CLOUDFLARE_GATEWAY_ID: "gw-env" },
		call: ["cloudflare-ai-gateway", GATEWAY_OPENAI],
		wire: true,
	},
	{
		name: "ai gateway without a gateway id is not configured",
		models: {},
		auth: {},
		env: { CLOUDFLARE_API_KEY: "cf-env", CLOUDFLARE_ACCOUNT_ID: "acct-env" },
		call: ["cloudflare-ai-gateway", GATEWAY_OPENAI],
		wire: false,
	},
	{
		name: "vertex stored api key: no env",
		models: {},
		auth: {
			"google-vertex": {
				type: "api_key",
				key: "vk-stored",
				env: { GOOGLE_CLOUD_PROJECT: "proj-stored", GOOGLE_CLOUD_LOCATION: "europe-west4" },
			},
		},
		env: {},
		call: ["google-vertex", "gemini-2.5-flash"],
		wire: false,
	},
	{
		name: "vertex stored adc credential with project and location",
		models: {},
		auth: {
			"google-vertex": {
				type: "api_key",
				env: {
					GOOGLE_APPLICATION_CREDENTIALS: "<dir>/adc.json",
					GOOGLE_CLOUD_PROJECT: "proj-stored",
					GOOGLE_CLOUD_LOCATION: "europe-west4",
				},
			},
		},
		env: {},
		call: ["google-vertex", "gemini-2.5-flash"],
		wire: false,
	},
	{
		name: "custom provider stored credential env",
		models: {
			providers: {
				custom: { baseUrl: "http://127.0.0.1:9/v1", api: "openai-completions", models: [{ id: "m" }] },
			},
		},
		auth: { custom: { type: "api_key", key: "ck", env: { PI_CACHE_RETENTION: "long" } } },
		env: {},
		call: ["custom", "m"],
		wire: false,
	},
];

const unfill = (text, dir) => text.replaceAll(dir, "<dir>");
const json = (value, dir) => (value === undefined ? null : JSON.parse(unfill(JSON.stringify(value), dir)));
const sorted = (object) =>
	object ? Object.fromEntries(Object.entries(object).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))) : null;

const out = [];
for (const spec of cases) {
	const dir = join(scratch, `case-${out.length}`);
	mkdirSync(join(dir, "home"), { recursive: true });
	mkdirSync(join(dir, "agent"), { recursive: true });
	const modelsPath = join(dir, "agent", "models.json");
	writeFileSync(modelsPath, JSON.stringify(spec.models, null, 2));
	writeFileSync(join(dir, "agent", "auth.json"), JSON.stringify(spec.auth).replaceAll("<dir>", dir));
	writeFileSync(join(dir, "adc.json"), "{}");
	for (const key of Object.keys(process.env)) delete process.env[key];
	Object.assign(process.env, baseEnv(dir), spec.env);
	const runtime = await ModelRuntime.create({ authPath: join(dir, "agent", "auth.json"), modelsPath });
	const [provider, id] = spec.call;
	const model = runtime.getModel(provider, id);
	if (!model) throw new Error(`${spec.name}: no model ${provider}/${id}`);
	let auth;
	try {
		const resolution = await runtime.getAuth(model);
		auth = resolution
			? {
					apiKey: resolution.auth.apiKey ?? null,
					headers: resolution.auth.headers ?? null,
					env: sorted(resolution.env),
				}
			: null;
	} catch (error) {
		auth = { error: error.message };
	}
	let request = null;
	if (spec.wire) {
		sent.length = 0;
		const message = await runtime.completeSimple(
			model,
			{ messages: [{ role: "user", content: "go", timestamp: 0 }] },
			{ maxRetries: 0 },
		);
		if (sent.length !== 1) throw new Error(`${spec.name}: ${sent.length} requests (${message.errorMessage})`);
		request = sent[0];
	}
	out.push({
		name: spec.name,
		models: spec.models,
		auth: spec.auth,
		env: spec.env,
		model: `${provider}/${id}`,
		baseUrl: model.baseUrl,
		getAuth: json(auth, dir),
		request: json(request, dir),
	});
}

rmSync(scratch, { recursive: true, force: true });
writeFileSync(join(here, "request-auth-env.json"), `${JSON.stringify({ cases: out }, null, "\t")}\n`);
assertOffline();
