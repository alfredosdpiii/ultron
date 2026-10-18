// Records the `provider` section of `/bug`'s report.json (`collectBugReportMetadata` -> `describeProvider`,
// packages/coding-agent/src/core/bug-report.ts) for the model's provider as a session's `ModelRuntime` sees it:
// built-in providers with a key from the environment, a stored key, a stored OAuth login or nothing; a models.json
// override of a built-in (baseUrl with credentials and secret query parameters, headers); models.json providers with
// a `$VAR` key (set and unset), a literal key and a `!command` key. Each case runs in its own node process with a
// private temp HOME / agent dir and only the environment it names (PATH=/usr/bin:/bin, PI_OFFLINE=1), behind
// offline-guard.mjs; the generator fails when anything tried to leave loopback. crates/cli/tests/bug_report.rs
// replays every case. Run twice, same sha256: node crates/cli/fixtures/bug_report_provider.mjs
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { assertOffline, guardNodeOptions } from "./offline-guard.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const FAR = 4102444800000;

if (process.argv[2] === "--case") {
	// One case, in a process whose environment is the case's.
	const src = (path) => pathToFileURL(join(root, "packages/coding-agent/src", path)).href;
	const { ModelRuntime } = await import(src("core/model-runtime.ts"));
	const { collectBugReportMetadata } = await import(src("core/bug-report.ts"));
	const [provider, id] = process.argv.slice(3);
	const agent = process.env.ULTRON_CODING_AGENT_DIR;
	const runtime = await ModelRuntime.create({
		authPath: join(agent, "auth.json"),
		modelsPath: join(agent, "models.json"),
		allowModelNetwork: false,
	});
	const describe = (model) =>
		collectBugReportMetadata({
			sessionId: "s",
			cwd: "/",
			includeSession: false,
			includeSummary: false,
			messageCount: 0,
			model,
			modelRuntime: runtime,
			thinkingLevel: "off",
			extensions: [],
			extensionErrors: [],
			globalSettings: {},
			projectSettings: {},
		}).provider;
	if (provider === "--every-builtin") {
		// Every built-in provider with a static catalog, through its first model.
		const all = {};
		for (const p of runtime.getProviders()) {
			const first = runtime.getModels(p.id)[0];
			if (first) all[p.id] = { model: first.id, provider: describe(first) };
		}
		process.stdout.write(JSON.stringify(all));
		process.exit(0);
	}
	const model = runtime.getModel(provider, id);
	if (!model) throw new Error(`no model ${provider}/${id}`);
	process.stdout.write(JSON.stringify(describe(model)));
	process.exit(0);
}

// Every built-in provider's API-key variable at once: each provider reports the one it reads.
const everyKey = Object.fromEntries(
	[
		"COPILOT_GITHUB_TOKEN", "ANTHROPIC_API_KEY", "ANT_LING_API_KEY", "QWEN_TOKEN_PLAN_API_KEY", "QWEN_TOKEN_PLAN_CN_API_KEY",
		"OPENAI_API_KEY", "AZURE_OPENAI_API_KEY", "NVIDIA_API_KEY", "DEEPSEEK_API_KEY", "GEMINI_API_KEY", "GOOGLE_CLOUD_API_KEY",
		"GROQ_API_KEY", "CEREBRAS_API_KEY", "XAI_API_KEY", "RADIUS_API_KEY", "OPENROUTER_API_KEY", "AI_GATEWAY_API_KEY",
		"ZAI_API_KEY", "ZAI_CODING_CN_API_KEY", "MISTRAL_API_KEY", "MINIMAX_API_KEY", "MINIMAX_CN_API_KEY", "MOONSHOT_API_KEY",
		"HF_TOKEN", "FIREWORKS_API_KEY", "TOGETHER_API_KEY", "BASETEN_API_KEY", "OPENCODE_API_KEY", "KIMI_API_KEY",
		"META_API_KEY", "CLOUDFLARE_API_KEY", "XIAOMI_API_KEY", "XIAOMI_TOKEN_PLAN_CN_API_KEY",
		"XIAOMI_TOKEN_PLAN_AMS_API_KEY", "XIAOMI_TOKEN_PLAN_SGP_API_KEY",
	].map((name) => [name, "k"]),
);
const oauth = (provider) => ({ [provider]: { type: "oauth", access: "access-token", refresh: "refresh-token", expires: FAR } });
const customModels = (apiKey) => ({
	providers: {
		local: {
			name: "Local Proxy",
			baseUrl: "http://127.0.0.1:9/v1",
			api: "openai-completions",
			...(apiKey === undefined ? {} : { apiKey }),
			headers: { "X-Trace": "on", Authorization: "Bearer h" },
			models: [{ id: "m" }],
		},
	},
});

const cases = [
	{ name: "every-builtin-unconfigured", model: ["--every-builtin", ""] },
	{ name: "every-builtin-env-keys", model: ["--every-builtin", ""], env: everyKey },
	{ name: "anthropic-every-env-var", model: ["anthropic", "claude-sonnet-4-5"], env: { ANTHROPIC_API_KEY: "k", ANTHROPIC_OAUTH_TOKEN: "t", ANTHROPIC_AUTH_TOKEN: "a" } },
	{ name: "bedrock-access-keys", model: ["amazon-bedrock", "amazon.nova-2-lite-v1:0"], env: { AWS_ACCESS_KEY_ID: "id", AWS_SECRET_ACCESS_KEY: "s" } },
	{ name: "bedrock-profile", model: ["amazon-bedrock", "amazon.nova-2-lite-v1:0"], env: { AWS_PROFILE: "p" } },
	{ name: "bedrock-bearer", model: ["amazon-bedrock", "amazon.nova-2-lite-v1:0"], env: { AWS_BEARER_TOKEN_BEDROCK: "b" } },
	{ name: "vertex-api-key", model: ["google-vertex", "gemini-2.5-flash"], env: { GOOGLE_CLOUD_API_KEY: "k" } },
	{ name: "bedrock-bearer-over-profile", model: ["amazon-bedrock", "amazon.nova-2-lite-v1:0"], env: { AWS_BEARER_TOKEN_BEDROCK: "b", AWS_PROFILE: "p", AWS_ACCESS_KEY_ID: "id", AWS_SECRET_ACCESS_KEY: "s" } },
	{ name: "bedrock-key-id-alone", model: ["amazon-bedrock", "amazon.nova-2-lite-v1:0"], env: { AWS_ACCESS_KEY_ID: "id" } },
	{ name: "bedrock-ecs-relative", model: ["amazon-bedrock", "amazon.nova-2-lite-v1:0"], env: { AWS_CONTAINER_CREDENTIALS_RELATIVE_URI: "/x" } },
	{ name: "bedrock-ecs-full", model: ["amazon-bedrock", "amazon.nova-2-lite-v1:0"], env: { AWS_CONTAINER_CREDENTIALS_FULL_URI: "http://127.0.0.1:9/x" } },
	{ name: "bedrock-web-identity", model: ["amazon-bedrock", "amazon.nova-2-lite-v1:0"], env: { AWS_WEB_IDENTITY_TOKEN_FILE: "/x" } },
	{ name: "bedrock-stored-key", model: ["amazon-bedrock", "amazon.nova-2-lite-v1:0"], auth: { "amazon-bedrock": { type: "api_key", key: "k" } } },
	{ name: "bedrock-empty-profile", model: ["amazon-bedrock", "amazon.nova-2-lite-v1:0"], env: { AWS_PROFILE: "" } },
	{
		name: "vertex-adc",
		model: ["google-vertex", "gemini-2.5-flash"],
		files: { "adc.json": "{}" },
		env: { GOOGLE_APPLICATION_CREDENTIALS: "${TMP}/adc.json", GOOGLE_CLOUD_PROJECT: "p", GOOGLE_CLOUD_LOCATION: "l" },
	},
	{
		name: "vertex-adc-gcloud-project",
		model: ["google-vertex", "gemini-2.5-flash"],
		files: { "adc.json": "{}" },
		env: { GOOGLE_APPLICATION_CREDENTIALS: "${TMP}/adc.json", GCLOUD_PROJECT: "p", GOOGLE_CLOUD_LOCATION: "l" },
	},
	{
		name: "vertex-adc-default-path",
		model: ["google-vertex", "gemini-2.5-flash"],
		files: { ".config/gcloud/application_default_credentials.json": "{}" },
		env: { GOOGLE_CLOUD_PROJECT: "p", GOOGLE_CLOUD_LOCATION: "l" },
	},
	{
		name: "vertex-adc-missing-file",
		model: ["google-vertex", "gemini-2.5-flash"],
		env: { GOOGLE_APPLICATION_CREDENTIALS: "${TMP}/none.json", GOOGLE_CLOUD_PROJECT: "p", GOOGLE_CLOUD_LOCATION: "l" },
	},
	{ name: "vertex-adc-no-location", model: ["google-vertex", "gemini-2.5-flash"], files: { "adc.json": "{}" }, env: { GOOGLE_APPLICATION_CREDENTIALS: "${TMP}/adc.json", GOOGLE_CLOUD_PROJECT: "p" } },
	{ name: "cloudflare-workers-key-and-account", model: ["cloudflare-workers-ai", "@cf/deepseek-ai/deepseek-v4-flash-0731"], env: { CLOUDFLARE_API_KEY: "k", CLOUDFLARE_ACCOUNT_ID: "a" } },
	{ name: "cloudflare-gateway-no-gateway-id", model: ["cloudflare-ai-gateway", "claude-fable-5"], env: { CLOUDFLARE_API_KEY: "k", CLOUDFLARE_ACCOUNT_ID: "a" } },
	{ name: "cloudflare-gateway-complete", model: ["cloudflare-ai-gateway", "claude-fable-5"], env: { CLOUDFLARE_API_KEY: "k", CLOUDFLARE_ACCOUNT_ID: "a", CLOUDFLARE_GATEWAY_ID: "g" } },
	// A stand-in file for the Claude Code CLI: it only has to exist (findCli never runs it).
	{ name: "claude-code-cli-present", model: ["claude-code", "claude-opus-5-5"], files: { "claude": "" }, env: { ULTRON_CLAUDE_CODE_BIN: "${TMP}/claude" } },
	{ name: "openai-empty-key", model: ["openai", "gpt-5.4"], env: { OPENAI_API_KEY: "" } },
	{ name: "copilot-stored-oauth", model: ["github-copilot", "claude-fable-5"], auth: oauth("github-copilot") },
	{ name: "anthropic-env-key", model: ["anthropic", "claude-sonnet-4-5"], env: { ANTHROPIC_API_KEY: "k" } },
	{ name: "anthropic-oauth-env-token", model: ["anthropic", "claude-sonnet-4-5"], env: { ANTHROPIC_OAUTH_TOKEN: "t" } },
	{ name: "anthropic-stored-key", model: ["anthropic", "claude-sonnet-4-5"], auth: { anthropic: { type: "api_key", key: "stored" } } },
	{ name: "anthropic-stored-oauth", model: ["anthropic", "claude-sonnet-4-5"], auth: oauth("anthropic") },
	{ name: "anthropic-unconfigured", model: ["anthropic", "claude-sonnet-4-5"] },
	{ name: "openai-env-key", model: ["openai", "gpt-5.4"], env: { OPENAI_API_KEY: "k" } },
	{ name: "openrouter-unconfigured", model: ["openrouter", "anthropic/claude-sonnet-4.5"] },
	{ name: "xai-stored-oauth", model: ["xai", "grok-4.3"], auth: oauth("xai") },
	{
		name: "anthropic-models-json-override",
		model: ["anthropic", "claude-sonnet-4-5"],
		env: { ANTHROPIC_API_KEY: "k" },
		models: {
			providers: {
				anthropic: {
					baseUrl: "https://user:pass@proxy.example.test/v1?api_key=abc&region=eu",
					headers: { "X-Team": "core", "x-api-key": "secret" },
				},
			},
		},
	},
	{ name: "custom-env-template-set", model: ["local", "m"], models: customModels("$LOCAL_KEY"), env: { LOCAL_KEY: "k" } },
	{ name: "custom-env-template-unset", model: ["local", "m"], models: customModels("$LOCAL_KEY") },
	{ name: "custom-literal-key", model: ["local", "m"], models: customModels("literal") },
	{ name: "custom-command-key", model: ["local", "m"], models: customModels("!printf k") },
	{ name: "custom-two-vars-one-set", model: ["local", "m"], models: customModels("${KEY_A}-${KEY_B}"), env: { KEY_A: "a" } },
	{ name: "custom-two-vars-both-set", model: ["local", "m"], models: customModels("${KEY_A}-${KEY_B}"), env: { KEY_A: "a", KEY_B: "b" } },
	{
		name: "custom-unnamed-no-key",
		model: ["bare", "m"],
		models: { providers: { bare: { baseUrl: "http://127.0.0.1:9/v1", api: "openai-completions", models: [{ id: "m" }] } } },
	},
	{ name: "custom-literal-key-stored-key", model: ["local", "m"], models: customModels("literal"), auth: { local: { type: "api_key", key: "s" } } },
];

const out = [];
for (const spec of cases) {
	const tmp = mkdtempSync(join(tmpdir(), "bug-report-provider-"));
	const agent = join(tmp, "agent");
	mkdirSync(agent, { recursive: true });
	if (spec.models) writeFileSync(join(agent, "models.json"), JSON.stringify(spec.models));
	if (spec.auth) writeFileSync(join(agent, "auth.json"), JSON.stringify(spec.auth));
	for (const [path, content] of Object.entries(spec.files ?? {})) {
		mkdirSync(dirname(join(tmp, path)), { recursive: true });
		writeFileSync(join(tmp, path), content);
	}
	const caseEnv = Object.fromEntries(Object.entries(spec.env ?? {}).map(([k, v]) => [k, v.replaceAll("${TMP}", tmp)]));
	const result = await new Promise((resolve) => {
		const proc = spawn(process.execPath, [fileURLToPath(import.meta.url), "--case", ...spec.model], {
			cwd: tmp,
			env: {
				PATH: "/usr/bin:/bin",
				HOME: tmp,
				PI_OFFLINE: "1",
				NODE_OPTIONS: guardNodeOptions,
				ULTRON_GUARD_LOG: join(tmp, "guard.log"),
				ULTRON_CODING_AGENT_DIR: agent,
				ULTRON_SERVER_DIR: join(tmp, "server"),
				...caseEnv,
			},
		});
		let stdout = "";
		let stderr = "";
		proc.stdout.on("data", (d) => {
			stdout += d;
		});
		proc.stderr.on("data", (d) => {
			stderr += d;
		});
		proc.on("close", (code) => resolve({ stdout, stderr, code }));
	});
	assertOffline(join(tmp, "guard.log"));
	rmSync(tmp, { recursive: true, force: true });
	if (result.code !== 0) throw new Error(`${spec.name}: exit ${result.code}\n${result.stderr}`);
	out.push({
		name: spec.name,
		model: { provider: spec.model[0], id: spec.model[1] },
		models: spec.models ?? null,
		auth: spec.auth ?? null,
		env: spec.env ?? {},
		files: spec.files ?? {},
		provider: JSON.parse(result.stdout),
	});
}
writeFileSync(join(here, "bug_report_provider.json"), `${JSON.stringify(out, null, "\t")}\n`);
