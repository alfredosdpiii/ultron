// Generates crates/ai/fixtures/model-registry.json from this worktree's TypeScript model runtime: models.json custom
// providers and models composed over the built-in catalog (`provider-composer.ts` via `ModelRuntime`), request auth
// (`ModelRuntime.getAuth`: the configured API key as a literal, `$VAR` template or `!command`, provider and model
// headers, `authHeader`), availability, `resolveCliModel` (`model-resolver.ts`) and `listModels` (`cli/list-models.ts`).
// Each case writes its models.json to a fresh temp dir and runs with only the environment it names (no provider keys,
// PATH=/usr/bin:/bin, network off), so nothing contacts a provider.
// Usage: node crates/ai/fixtures/generate-model-registry.mjs
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const scratch = mkdtempSync(join(tmpdir(), "ai-model-registry-"));
const home = join(scratch, "home");
mkdirSync(home);
for (const key of Object.keys(process.env)) delete process.env[key];
const baseEnv = {
	HOME: home,
	PATH: "/usr/bin:/bin",
	PI_OFFLINE: "1",
	ULTRON_CODING_AGENT_DIR: join(scratch, "agent"),
	ULTRON_SERVER_DIR: join(scratch, "server"),
};
Object.assign(process.env, baseEnv);

const core = (file) => import(pathToFileURL(join(root, "packages/coding-agent/src/core", file)).href);
const { ModelRuntime } = await core("model-runtime.ts");
const { resolveCliModel } = await core("model-resolver.ts");
const { clearConfigValueCache } = await core("resolve-config-value.ts");
const { listModels } = await import(pathToFileURL(join(root, "packages/coding-agent/src/cli/list-models.ts")).href);

const cost = { input: 1, output: 2, cacheRead: 0.1, cacheWrite: 0.5 };

const proxy = {
	baseUrl: "http://127.0.0.1:9/v1",
	api: "openai-completions",
	apiKey: "proxy-key",
	headers: { "x-proxy-token": "$PROXY_TOKEN", "x-static": "static" },
	compat: { supportsStore: false, openRouterRouting: { only: ["a"] } },
	models: [
		{
			id: "gpt-6.1-sol",
			name: "GPT 6.1 Sol",
			reasoning: true,
			thinkingLevelMap: { off: null, high: "high" },
			input: ["text", "image"],
			contextWindow: 400000,
			maxTokens: 128000,
			cost,
			headers: { "x-model": "sol", "X-Static": "model-wins" },
			compat: { maxTokensField: "max_tokens", openRouterRouting: { order: ["b"] } },
		},
		{ id: "plain" },
		{ id: "responses", api: "openai-responses", baseUrl: "http://127.0.0.1:9/alt" },
	],
	modelOverrides: {
		plain: { contextWindow: 9000, maxTokens: 900, headers: { "x-override": "o" }, cost: { output: 7 } },
		"not-listed": { name: "ignored" },
	},
};

const cases = [
	{
		name: "custom provider with literal key, headers, compat and overrides",
		models: { providers: { cliproxyapi: proxy } },
		env: { PROXY_TOKEN: "tok-1" },
		auth: [["cliproxyapi", "gpt-6.1-sol"], ["cliproxyapi", "plain"], ["cliproxyapi", "plain", "override-key"]],
		resolve: [
			[undefined, "cliproxyapi/gpt-6.1-sol"],
			["cliproxyapi", "gpt-6.1-sol"],
			["cliproxyapi", "cliproxyapi/plain"],
			[undefined, "cliproxyapi/responses"],
			[undefined, "nosuch/model"],
			["nosuch", "model"],
		],
		list: "cliproxyapi",
	},
	{
		name: "authHeader adds a bearer header",
		models: { providers: { gw: { baseUrl: "http://127.0.0.1:9/v1", api: "anthropic-messages", apiKey: "gw-key", authHeader: true, models: [{ id: "m" }] } } },
		env: {},
		auth: [["gw", "m"], ["gw", "m", "flag-key"]],
		resolve: [[undefined, "gw/m"]],
		list: "gw",
	},
	{
		name: "env template key resolved",
		models: { providers: { envp: { baseUrl: "http://h/v1", api: "openai-completions", apiKey: "pre-${KEY_A}-$KEY_B", models: [{ id: "e" }] } } },
		env: { KEY_A: "a", KEY_B: "b" },
		auth: [["envp", "e"]],
		resolve: [[undefined, "envp/e"]],
		list: "envp",
	},
	{
		name: "env template key with missing variables",
		models: { providers: { envp: { baseUrl: "http://h/v1", api: "openai-completions", apiKey: "pre-${KEY_A}-$KEY_B", models: [{ id: "e" }] } } },
		env: {},
		auth: [["envp", "e"]],
		resolve: [[undefined, "envp/e"]],
		list: "envp",
	},
	{
		name: "single env variable key missing",
		models: { providers: { envp: { baseUrl: "http://h/v1", api: "openai-completions", apiKey: "$ONLY_KEY", models: [{ id: "e" }] } } },
		env: {},
		auth: [["envp", "e"]],
		resolve: [],
		list: "envp",
	},
	{
		name: "command key",
		models: {
			providers: {
				cmd: { baseUrl: "http://h/v1", api: "openai-completions", apiKey: "!printf ' cmd-key \\n'", models: [{ id: "c" }] },
				bad: { baseUrl: "http://h/v1", api: "openai-completions", apiKey: "!exit 3", models: [{ id: "b" }] },
			},
		},
		env: {},
		auth: [["cmd", "c"], ["bad", "b"]],
		resolve: [[undefined, "cmd/c"]],
		list: "c",
	},
	{
		name: "header from a missing variable fails the request",
		models: { providers: { hp: { baseUrl: "http://h/v1", api: "openai-completions", apiKey: "k", headers: { "x-h": "$NO_SUCH_HEADER" }, models: [{ id: "h", headers: { "x-m": "${NO_MODEL_HEADER}" } }] } } },
		env: {},
		auth: [["hp", "h"]],
		resolve: [],
		list: "hp",
	},
	{
		name: "custom provider without a key is not available",
		models: { providers: { nokey: { baseUrl: "http://h/v1", api: "openai-completions", models: [{ id: "n" }] } } },
		env: {},
		auth: [["nokey", "n"], ["nokey", "n", "given"]],
		resolve: [[undefined, "nokey/n"]],
		list: "nokey",
	},
	{
		name: "built-in provider overrides",
		models: {
			providers: {
				deepseek: {
					baseUrl: "http://127.0.0.1:9/ds",
					headers: { "x-ds": "1" },
					compat: { supportsStore: true },
					models: [{ id: "deepseek-v4-pro", contextWindow: 5000 }, { id: "brand-new", name: "Brand New" }],
					modelOverrides: {
						"deepseek-flash": {
							name: "Flash Custom",
							reasoning: false,
							input: ["text"],
							maxTokens: 1234,
							cost: { input: 9, tiers: [{ inputTokensAbove: 100, ...cost }] },
							thinkingLevelMap: { low: null, xhigh: "xhigh" },
							promptCache: { short: 60 },
							samplingParams: { top_k: 3 },
							inputLimits: { maxRequestBytes: 10, images: { maxPerMessage: 2, resize: { maxWidth: 512 } } },
							compat: { thinkingFormat: "openai" },
						},
					},
				},
			},
		},
		env: { DEEPSEEK_API_KEY: "ds-env" },
		auth: [["deepseek", "deepseek-flash"], ["deepseek", "brand-new"]],
		resolve: [[undefined, "deepseek/brand-new"], [undefined, "deepseek/deepseek-flash"]],
		list: "deepseek",
	},
	{
		name: "built-in provider with a models.json key ignores the environment key",
		models: { providers: { cerebras: { apiKey: "$CEREBRAS_CUSTOM" } } },
		env: { CEREBRAS_API_KEY: "env-key" },
		auth: [["cerebras", "gpt-oss-120b"]],
		resolve: [],
		list: "cerebras",
	},
	{
		name: "built-in provider with only a base URL keeps the environment key",
		models: { providers: { cerebras: { baseUrl: "http://127.0.0.1:9/c" } } },
		env: { CEREBRAS_API_KEY: "env-key" },
		auth: [["cerebras", "gpt-oss-120b"]],
		resolve: [[undefined, "cerebras/gpt-oss-120b"]],
		list: "cerebras",
	},
	{
		name: "composition errors",
		models: {
			providers: {
				empty: {},
				noapi: { baseUrl: "http://h", models: [{ id: "x" }] },
				nourl: { api: "openai-completions", models: [{ id: "x" }] },
				zero: { baseUrl: "http://h", api: "openai-completions", models: [{ id: "x", contextWindow: 0 }] },
				negative: { baseUrl: "http://h", api: "openai-completions", models: [{ id: "x", maxTokens: -1 }] },
				oauthnourl: { oauth: "radius" },
				deepseek: {},
				ok: { baseUrl: "http://h", api: "openai-completions", apiKey: "k", models: [{ id: "fine" }] },
			},
		},
		env: {},
		auth: [["ok", "fine"]],
		resolve: [[undefined, "noapi/x"], [undefined, "deepseek/deepseek-flash"], [undefined, "ok/fine"]],
		list: "o",
	},
	{
		name: "invalid schema loads no custom providers",
		models: { providers: { cliproxyapi: { ...proxy, apiKey: 5 } } },
		env: {},
		auth: [],
		resolve: [[undefined, "cliproxyapi/gpt-6.1-sol"]],
		list: "cliproxyapi",
	},
];

function captureConsole(run) {
	const out = [];
	const err = [];
	const log = console.log;
	const error = console.error;
	console.log = (...args) => out.push(`${args.join(" ")}\n`);
	console.error = (...args) => err.push(`${args.join(" ")}\n`);
	return run().finally(() => {
		console.log = log;
		console.error = error;
	}).then(() => ({ stdout: out.join(""), stderr: err.join("") }));
}

const json = (value) => (value === undefined ? null : JSON.parse(JSON.stringify(value)));

const out = [];
for (const spec of cases) {
	const dir = join(scratch, `case-${out.length}`);
	mkdirSync(dir);
	const modelsPath = join(dir, "models.json");
	writeFileSync(modelsPath, JSON.stringify(spec.models, null, 2));
	for (const key of Object.keys(process.env)) delete process.env[key];
	Object.assign(process.env, baseEnv, spec.env);
	clearConfigValueCache();
	const runtime = await ModelRuntime.create({ authPath: join(dir, "auth.json"), modelsPath });
	const providers = Object.keys(spec.models.providers);
	const models = Object.fromEntries(providers.map((id) => [id, json(runtime.getModels(id))]));
	const available = runtime
		.getAvailableSnapshot()
		.filter((m) => providers.includes(m.provider))
		.map((m) => `${m.provider}/${m.id}`);
	const auth = [];
	for (const [provider, id, apiKey] of spec.auth) {
		const model = runtime.getModel(provider, id);
		let result;
		try {
			const resolution = await runtime.getAuth(model, apiKey === undefined ? {} : { apiKey });
			result = resolution ? { apiKey: resolution.auth.apiKey ?? null, headers: resolution.auth.headers ?? null } : null;
		} catch (error) {
			result = { error: error.message, cause: error.cause?.message ?? null };
		}
		auth.push({ model: `${provider}/${id}`, apiKey: apiKey ?? null, result });
	}
	const resolve = spec.resolve.map(([cliProvider, cliModel]) => {
		const result = resolveCliModel({ cliProvider, cliModel, modelRuntime: runtime });
		return { provider: cliProvider ?? null, model: cliModel, result: json(result.model), error: result.error ?? null };
	});
	const captured = await captureConsole(() => listModels(runtime, spec.list));
	const docs = join(root, "packages/coding-agent/docs");
	const list = { stdout: captured.stdout.replaceAll(docs, "<docs>"), stderr: captured.stderr.replaceAll(dir, "<dir>") };
	const error = runtime.getError()?.replaceAll(dir, "<dir>") ?? null;
	out.push({ name: spec.name, models: spec.models, env: spec.env, error, providers: models, available, auth, resolve, list: { search: spec.list, ...list } });
}

rmSync(scratch, { recursive: true, force: true });
writeFileSync(join(here, "model-registry.json"), `${JSON.stringify({ cases: out }, null, "\t")}\n`);
