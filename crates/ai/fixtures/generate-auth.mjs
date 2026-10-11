// Generates crates/ai/fixtures/auth.json from this worktree's TypeScript packages/ai auth resolution
// (`resolveProviderAuth` over the built-in providers' auth: anthropic, openai, openai-codex, github-copilot, google,
// google-vertex). Each case seeds an InMemoryCredentialStore, an injected AuthContext (env map, existing files) and
// request overrides, stubs fetch for OAuth refresh, and records the auth result or ModelsError plus the stored
// credential afterwards. Usage: node crates/ai/fixtures/generate-auth.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const src = (path) => import(pathToFileURL(join(root, "packages/ai/src", path)).href);
const NOW = 1767225600000;
Date.now = () => NOW;

const { resolveProviderAuth } = await src("auth/resolve.ts");
const { InMemoryCredentialStore } = await src("auth/credential-store.ts");
const providers = {
	anthropic: (await src("providers/anthropic.ts")).anthropicProvider(),
	openai: (await src("providers/openai.ts")).openaiProvider(),
	"openai-codex": (await src("providers/openai-codex.ts")).openaiCodexProvider(),
	"github-copilot": (await src("providers/github-copilot.ts")).githubCopilotProvider(),
	google: (await src("providers/google.ts")).googleProvider(),
	"google-vertex": (await src("providers/google-vertex.ts")).googleVertexProvider(),
};

let queue = [];
let requests = [];
globalThis.fetch = async (url, init = {}) => {
	const headers = {};
	for (const [k, v] of Object.entries(init.headers ?? {})) headers[k.toLowerCase()] = String(v);
	requests.push({ url: String(url), method: init.method ?? "GET", headers, body: init.body === undefined ? "" : String(init.body) });
	const next = queue.shift();
	if (!next) throw new Error("unexpected request");
	return new Response(next[1], { status: next[0], statusText: next[0] === 200 ? "OK" : "Bad Request" });
};

const soon = NOW + 60_000;
const later = NOW + 60 * 60_000;
const COPILOT = "tid=1;proxy-ep=proxy.business.githubcopilot.com;x";
const anthropicTokens = '{"access_token":"new-access","refresh_token":"new-refresh","expires_in":3600}';
const ADC = "~/.config/gcloud/application_default_credentials.json";

// [name, provider, stored, env, files, overrides, responses]
const cases = [
	["anthropic nothing configured", "anthropic", null, {}, [], {}, []],
	["anthropic auth token env", "anthropic", null, { ANTHROPIC_AUTH_TOKEN: "tok", ANTHROPIC_API_KEY: "key" }, [], {}, []],
	["anthropic oauth token env", "anthropic", null, { ANTHROPIC_OAUTH_TOKEN: "oat", ANTHROPIC_API_KEY: "key" }, [], {}, []],
	["anthropic api key env", "anthropic", null, { ANTHROPIC_API_KEY: "key" }, [], {}, []],
	["anthropic stored key beats env", "anthropic", { type: "api_key", key: "stored", env: { X: "1" } }, { ANTHROPIC_API_KEY: "key" }, [], {}, []],
	["anthropic stored key without key falls to env", "anthropic", { type: "api_key" }, { ANTHROPIC_API_KEY: "key" }, [], {}, []],
	["anthropic explicit key", "anthropic", { type: "oauth", access: "a", refresh: "r", expires: later }, {}, [], { apiKey: "explicit", env: { A: "b" } }, []],
	["anthropic explicit empty key", "anthropic", null, { ANTHROPIC_API_KEY: "key" }, [], { apiKey: "" }, []],
	["anthropic override env feeds ambient", "anthropic", null, {}, [], { env: { ANTHROPIC_API_KEY: "from-request" } }, []],
	["anthropic override env merges into stored", "anthropic", { type: "api_key", env: { A: "1", B: "2" } }, { ANTHROPIC_API_KEY: "k" }, [], { env: { B: "3" } }, []],
	["anthropic override env merges into stored key", "anthropic", { type: "api_key", key: "s", env: { A: "1", B: "2" } }, {}, [], { env: { B: "3", C: "4" } }, []],
	["anthropic valid oauth", "anthropic", { type: "oauth", access: "a", refresh: "r", expires: later }, { ANTHROPIC_API_KEY: "key" }, [], {}, []],
	["anthropic oauth expiring refreshes", "anthropic", { type: "oauth", access: "a", refresh: "r", expires: soon }, {}, [], {}, [[200, anthropicTokens]]],
	["anthropic oauth expired refresh fails", "anthropic", { type: "oauth", access: "a", refresh: "r", expires: 0 }, {}, [], {}, [[400, "invalid_grant"]]],
	["anthropic oauth min validity refreshes", "anthropic", { type: "oauth", access: "a", refresh: "r", expires: NOW + 10 * 60_000 }, {}, [], { minOAuthValidityMs: 30 * 60_000 }, [[200, anthropicTokens]]],
	["anthropic oauth min validity too short", "anthropic", { type: "oauth", access: "a", refresh: "r", expires: 0 }, {}, [], { minOAuthValidityMs: 2 * 60 * 60_000 }, [[200, anthropicTokens]]],
	["openai env", "openai", null, { OPENAI_API_KEY: "sk" }, [], {}, []],
	["openai stored oauth has no handler", "openai", { type: "oauth", access: "a", refresh: "r", expires: later }, { OPENAI_API_KEY: "sk" }, [], {}, []],
	["codex no credential", "openai-codex", null, { OPENAI_API_KEY: "sk" }, [], {}, []],
	["codex stored api key has no handler", "openai-codex", { type: "api_key", key: "k" }, {}, [], {}, []],
	["codex explicit key ignored without api-key auth", "openai-codex", null, {}, [], { apiKey: "k" }, []],
	["codex oauth", "openai-codex", { type: "oauth", access: "jwt", refresh: "r", expires: later, accountId: "acct" }, {}, [], {}, []],
	["copilot env token", "github-copilot", null, { COPILOT_GITHUB_TOKEN: "ghu", GITHUB_TOKEN: "x" }, [], {}, []],
	["copilot generic github token ignored", "github-copilot", null, { GITHUB_TOKEN: "x", GH_TOKEN: "y" }, [], {}, []],
	["copilot oauth base url", "github-copilot", { type: "oauth", access: COPILOT, refresh: "gho", expires: later }, {}, [], {}, []],
	["copilot oauth enterprise", "github-copilot", { type: "oauth", access: "plain", refresh: "gho", expires: later, enterpriseUrl: "https://ghe.example.com/" }, {}, [], {}, []],
	[
		"copilot oauth refresh",
		"github-copilot",
		{ type: "oauth", access: "old", refresh: "gho", expires: 0, availableModelIds: ["x"] },
		{},
		[],
		{},
		[
			[200, JSON.stringify({ token: COPILOT, expires_at: (NOW + 3_600_000) / 1000 })],
			[200, JSON.stringify({ data: [{ id: "gpt-5.4", model_picker_enabled: true }] })],
		],
	],
	["google env", "google", null, { GEMINI_API_KEY: "g", GOOGLE_API_KEY: "ignored" }, [], {}, []],
	["google nothing", "google", null, { GOOGLE_API_KEY: "ignored" }, [], {}, []],
	["vertex api key env", "google-vertex", null, { GOOGLE_CLOUD_API_KEY: "vk" }, [ADC], {}, []],
	["vertex stored key", "google-vertex", { type: "api_key", key: "stored" }, { GOOGLE_CLOUD_API_KEY: "vk" }, [], {}, []],
	["vertex adc", "google-vertex", null, { GOOGLE_CLOUD_PROJECT: "p", GOOGLE_CLOUD_LOCATION: "us" }, [ADC], {}, []],
	["vertex adc gcloud project", "google-vertex", null, { GCLOUD_PROJECT: "p", GOOGLE_CLOUD_LOCATION: "us" }, [ADC], {}, []],
	["vertex adc missing file", "google-vertex", null, { GOOGLE_CLOUD_PROJECT: "p", GOOGLE_CLOUD_LOCATION: "us" }, [], {}, []],
	["vertex adc missing location", "google-vertex", null, { GOOGLE_CLOUD_PROJECT: "p" }, [ADC], {}, []],
	["vertex service account env", "google-vertex", null, { GOOGLE_APPLICATION_CREDENTIALS: "/sa.json", GOOGLE_CLOUD_PROJECT: "p", GOOGLE_CLOUD_LOCATION: "us" }, ["/sa.json"], {}, []],
	["vertex service account env missing file", "google-vertex", null, { GOOGLE_APPLICATION_CREDENTIALS: "/sa.json", GOOGLE_CLOUD_PROJECT: "p", GOOGLE_CLOUD_LOCATION: "us" }, [ADC], {}, []],
	[
		"vertex stored env",
		"google-vertex",
		{ type: "api_key", env: { GOOGLE_CLOUD_PROJECT: "sp", GOOGLE_CLOUD_LOCATION: "sl", GOOGLE_APPLICATION_CREDENTIALS: "/stored.json" } },
		{},
		["/stored.json"],
		{},
		[],
	],
	["vertex stored env partial", "google-vertex", { type: "api_key", env: { GOOGLE_CLOUD_PROJECT: "sp" } }, { GOOGLE_CLOUD_LOCATION: "el" }, [ADC], {}, []],
	["vertex stored empty project", "google-vertex", { type: "api_key", env: { GOOGLE_CLOUD_PROJECT: "", GOOGLE_CLOUD_LOCATION: "l" } }, { GOOGLE_CLOUD_PROJECT: "env-p" }, [ADC], {}, []],
	["vertex override env", "google-vertex", null, {}, [ADC], { env: { GOOGLE_CLOUD_PROJECT: "op", GOOGLE_CLOUD_LOCATION: "ol" } }, []],
	["vertex whitespace env ignored", "google-vertex", null, { GOOGLE_CLOUD_API_KEY: "   " }, [], {}, []],
];

const out = [];
for (const [name, providerId, stored, env, files, overrides, responses] of cases) {
	queue = [...responses];
	requests = [];
	const credentials = new InMemoryCredentialStore();
	if (stored) await credentials.modify(providerId, async () => stored);
	const authContext = {
		env: async (key) => {
			const value = env[key];
			return typeof value === "string" && value.trim().length > 0 ? value : undefined;
		},
		fileExists: async (path) => files.includes(path),
	};
	let result = null;
	let error = null;
	try {
		result = (await resolveProviderAuth(providers[providerId], credentials, authContext, overrides)) ?? null;
	} catch (e) {
		error = { name: e.name, code: e.code ?? null, message: e.message };
	}
	out.push({
		name,
		provider: providerId,
		stored,
		env,
		files,
		overrides,
		responses,
		result: result && JSON.parse(JSON.stringify(result)),
		error,
		storedAfter: (await credentials.read(providerId)) ?? null,
		requests,
	});
}
writeFileSync(join(here, "auth.json"), `${JSON.stringify(out, null, "\t")}\n`);
