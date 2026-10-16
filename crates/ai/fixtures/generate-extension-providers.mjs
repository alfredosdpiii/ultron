// Generates crates/ai/fixtures/extension-providers.json from this worktree's TypeScript model runtime: providers an
// extension registers (`pi.registerProvider(name, config)`, applied by `agent-session-services.ts` through
// `ModelRuntime.registerProvider`) composed with the built-in catalog and models.json (`provider-composer.ts`
// `applyExtension` after `applyModelsJson`, models.json `modelOverrides` last), the registration's validation error,
// re-registration merging, availability, request auth (key and headers) and `getError`.
// Each case writes its models.json to a fresh temp dir and runs with only the environment it names (no provider keys,
// PATH=/usr/bin:/bin, PI_OFFLINE=1), so nothing contacts a provider; offline-guard.mjs checks that at the end.
// Usage: node crates/ai/fixtures/generate-extension-providers.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const scratch = mkdtempSync(join(tmpdir(), "ai-extension-providers-"));
const home = join(scratch, "home");
mkdirSync(home);
const baseEnv = {
	HOME: home,
	PATH: "/usr/bin:/bin",
	PI_OFFLINE: "1",
	ULTRON_CODING_AGENT_DIR: join(scratch, "agent"),
	ULTRON_SERVER_DIR: join(scratch, "server"),
};
for (const key of Object.keys(process.env)) delete process.env[key];
Object.assign(process.env, baseEnv);

const core = (file) => import(pathToFileURL(join(root, "packages/coding-agent/src/core", file)).href);
const { ModelRuntime } = await core("model-runtime.ts");
const { resolveCliModel } = await core("model-resolver.ts");
const { clearConfigValueCache } = await core("resolve-config-value.ts");

const cost = { input: 1, output: 2, cacheRead: 0.1, cacheWrite: 0.5 };
const model = (id, extra = {}) => ({
	id,
	name: `Model ${id}`,
	reasoning: false,
	input: ["text"],
	cost,
	contextWindow: 64000,
	maxTokens: 4096,
	...extra,
});

const cases = [
	{
		name: "new provider with models, a literal key, headers and authHeader",
		models: null,
		env: { PROXY_TOKEN: "tok" },
		register: [
			[
				"my-proxy",
				{
					baseUrl: "http://127.0.0.1:9/v1",
					apiKey: "proxy-key",
					api: "anthropic-messages",
					headers: { "x-proxy": "$PROXY_TOKEN", "x-static": "s" },
					authHeader: true,
					models: [
						model("claude-proxy", { reasoning: true, input: ["text", "image"], headers: { "x-model": "m" } }),
						model("responses", { api: "openai-responses", baseUrl: "http://127.0.0.1:9/alt" }),
					],
				},
			],
		],
		providers: ["my-proxy"],
		auth: [["my-proxy", "claude-proxy"], ["my-proxy", "responses", "flag-key"]],
		resolve: [[undefined, "my-proxy/claude-proxy"], [undefined, "claude-proxy"]],
	},
	{
		name: "a base URL for a built-in provider moves every model",
		models: null,
		env: { DEEPSEEK_API_KEY: "ds-env" },
		register: [["deepseek", { baseUrl: "http://127.0.0.1:9/ds" }]],
		providers: ["deepseek"],
		auth: [["deepseek", "deepseek-flash"]],
		resolve: [[undefined, "deepseek/deepseek-flash"]],
	},
	{
		name: "extension models replace a built-in provider's models, defaults from the built-in model of that id",
		models: null,
		env: { DEEPSEEK_API_KEY: "ds-env" },
		register: [["deepseek", { models: [model("deepseek-flash", { name: "Flash via ext" }), model("ext-only", { api: "openai-completions" })] }]],
		providers: ["deepseek"],
		auth: [["deepseek", "ext-only"]],
		resolve: [[undefined, "deepseek/ext-only"], [undefined, "deepseek/deepseek-v4-pro"]],
	},
	{
		name: "extension over models.json: its key and headers win, models.json overrides apply last",
		models: {
			providers: {
				gw: {
					baseUrl: "http://127.0.0.1:9/json",
					api: "openai-completions",
					apiKey: "json-key",
					headers: { "x-json": "j", "x-both": "json" },
					models: [{ id: "json-model" }],
					modelOverrides: { "ext-model": { name: "Overridden", maxTokens: 777 } },
				},
			},
		},
		env: {},
		register: [["gw", { apiKey: "ext-key", headers: { "x-both": "ext" }, baseUrl: "http://127.0.0.1:9/ext", models: [model("ext-model")] }]],
		providers: ["gw"],
		auth: [["gw", "ext-model"]],
		resolve: [[undefined, "gw/json-model"], [undefined, "gw/ext-model"]],
	},
	{
		name: "a second registration of a name merges over the first",
		models: null,
		env: {},
		register: [
			["merged", { baseUrl: "http://127.0.0.1:9/one", api: "openai-completions", apiKey: "one", models: [model("m1")] }],
			["merged", { baseUrl: "http://127.0.0.1:9/two", apiKey: "two" }],
		],
		providers: ["merged"],
		auth: [["merged", "m1"]],
		resolve: [],
	},
	{
		name: "an env key that is not set leaves the provider unavailable",
		models: null,
		env: {},
		register: [["envp", { baseUrl: "http://h/v1", api: "openai-completions", apiKey: "$EXT_KEY", models: [model("e")] }]],
		providers: ["envp"],
		auth: [["envp", "e"]],
		resolve: [],
	},
	{
		name: "an env key that is set",
		models: null,
		env: { EXT_KEY: "from-env" },
		register: [["envp", { baseUrl: "http://h/v1", api: "openai-completions", apiKey: "$EXT_KEY", models: [model("e")] }]],
		providers: ["envp"],
		auth: [["envp", "e"]],
		resolve: [],
	},
	{
		name: "registrations that do not validate are refused and change nothing",
		models: null,
		env: {},
		register: [
			["noapi", { baseUrl: "http://h", apiKey: "k", models: [model("x")] }],
			["nourl", { api: "openai-completions", apiKey: "k", models: [model("x")] }],
			["ok", { baseUrl: "http://h", api: "openai-completions", apiKey: "k", models: [model("fine")] }],
			["ok", { models: [model("broken")] }],
		],
		providers: ["noapi", "nourl", "ok"],
		auth: [["ok", "fine"]],
		resolve: [[undefined, "ok/fine"]],
	},
	{
		name: "a new provider without a key lists its models, none available",
		models: null,
		env: {},
		register: [["nokey", { baseUrl: "http://h/v1", api: "openai-completions", models: [model("n")] }]],
		providers: ["nokey"],
		auth: [],
		resolve: [[undefined, "nokey/n"]],
	},
];

const json = (value) => (value === undefined ? null : JSON.parse(JSON.stringify(value)));

const out = [];
for (const spec of cases) {
	const dir = join(scratch, `case-${out.length}`);
	mkdirSync(dir);
	const modelsPath = join(dir, "models.json");
	if (spec.models) writeFileSync(modelsPath, JSON.stringify(spec.models, null, 2));
	for (const key of Object.keys(process.env)) delete process.env[key];
	Object.assign(process.env, baseEnv, spec.env);
	clearConfigValueCache();
	const runtime = await ModelRuntime.create({ authPath: join(dir, "auth.json"), modelsPath });
	const registered = [];
	for (const [name, config] of spec.register) {
		try {
			runtime.registerProvider(name, config);
			registered.push(null);
		} catch (error) {
			registered.push(error.message);
		}
	}
	await runtime.refresh({ allowNetwork: false });
	const models = Object.fromEntries(spec.providers.map((id) => [id, json(runtime.getModels(id))]));
	const available = runtime
		.getAvailableSnapshot()
		.filter((m) => spec.providers.includes(m.provider))
		.map((m) => `${m.provider}/${m.id}`);
	const auth = [];
	for (const [provider, id, apiKey] of spec.auth) {
		const found = runtime.getModel(provider, id);
		let result;
		try {
			const resolution = await runtime.getAuth(found, apiKey === undefined ? {} : { apiKey });
			result = resolution ? { apiKey: resolution.auth.apiKey ?? null, headers: resolution.auth.headers ?? null } : null;
		} catch (error) {
			result = { error: error.message };
		}
		auth.push({ model: `${provider}/${id}`, apiKey: apiKey ?? null, result });
	}
	const resolve = spec.resolve.map(([cliProvider, cliModel]) => {
		const result = resolveCliModel({ cliProvider, cliModel, modelRuntime: runtime });
		return { provider: cliProvider ?? null, model: cliModel, result: json(result.model), error: result.error ?? null };
	});
	const error = runtime.getError()?.replaceAll(dir, "<dir>") ?? null;
	out.push({
		name: spec.name,
		models: spec.models,
		env: spec.env,
		register: spec.register,
		registered,
		error,
		providers: models,
		available,
		auth,
		resolve,
	});
}

rmSync(scratch, { recursive: true, force: true });
writeFileSync(join(here, "extension-providers.json"), `${JSON.stringify({ cases: out }, null, "\t")}\n`);
assertOffline();
