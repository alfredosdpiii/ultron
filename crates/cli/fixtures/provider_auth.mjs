// Records `ultron auth check|print-api-key|print-bearer-token` of the TypeScript CLI for built-in and models.json
// providers: expired OAuth tokens of every built-in OAuth provider refreshed through its token endpoint (Anthropic,
// OpenAI Codex, GitHub Copilot, xAI, Kimi For Coding, Meta; OpenRouter keys never expire), a failed refresh, and
// models.json keys as literals, `$VAR` templates and `!commands`, stored keys and composition errors, and
// `--model` resolved alone or under `--provider` (`resolveCliModel`), across the providers with stored credentials.
// Each case runs `node packages/coding-agent/src/cli.ts auth ...` in a private temp HOME / agent dir with only the
// environment it names (PATH=/usr/bin:/bin, PI_OFFLINE=1). offline-guard.mjs is loaded into the process: no socket
// leaves loopback, and the providers' endpoints are answered by the case's stubs (ULTRON_FETCH_STUBS), every request
// logged; the generator fails when anything was blocked. Recorded: stdout, stderr, exit code, the requests, and
// the credential left in auth.json (times normalized).
// Usage: node crates/cli/fixtures/provider_auth.mjs
import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { assertOffline, guardNodeOptions } from "./offline-guard.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const FAR = 4102444800000;
const started = Date.now();

const jwt = (payload) =>
	`${Buffer.from('{"alg":"none"}').toString("base64")}.${Buffer.from(JSON.stringify(payload)).toString("base64")}.sig`;
const CODEX_JWT = jwt({ "https://api.openai.com/auth": { chatgpt_account_id: "account-789" } });
const COPILOT_TOKEN = "tid=abc;exp=4102444800;proxy-ep=proxy.individual.githubcopilot.com;sku=x";

/** Every provider's refresh endpoint, answered successfully. */
const stubs = {
	"POST https://platform.claude.com/v1/oauth/token": [
		{ body: { access_token: "anthropic-new", refresh_token: "anthropic-refresh-new", expires_in: 3600 } },
	],
	"POST https://auth.openai.com/oauth/token": [
		{ body: { access_token: CODEX_JWT, refresh_token: "codex-refresh-new", expires_in: 3600 } },
	],
	"GET https://api.github.com/copilot_internal/v2/token": [{ body: { token: COPILOT_TOKEN, expires_at: 4102444800 } }],
	"GET https://api.individual.githubcopilot.com/models": [
		{
			body: {
				data: [
					{ id: "gpt-5.4", model_picker_enabled: true, policy: { state: "enabled" } },
					{ id: "claude-sonnet-5", model_picker_enabled: true, policy: { state: "enabled" } },
				],
			},
		},
	],
	"POST https://auth.x.ai/oauth2/token": [
		{ body: { access_token: "xai-new", refresh_token: "xai-refresh-new", expires_in: 3600, token_type: "Bearer" } },
	],
	"POST https://auth.kimi.com/api/oauth/token": [
		{ body: { access_token: "kimi-new", refresh_token: "kimi-refresh-new", expires_in: 3600, token_type: "Bearer" } },
	],
	"POST https://api.meta.ai/muse-code/key": [{ body: { api_key: "meta-key-new" } }],
};
const failing = {
	...stubs,
	"POST https://platform.claude.com/v1/oauth/token": [{ status: 400, body: { error: "invalid_grant" } }],
};

const oauth = (provider, access, expires, extra = {}) => ({
	[provider]: { type: "oauth", access, refresh: `${provider}-refresh`, expires, ...extra },
});
const proxy = (extra) => ({ providers: { proxy: { baseUrl: "http://127.0.0.1:9/v1", api: "openai-completions", models: [{ id: "m" }], ...extra } } });

const cases = [
	// Built-in OAuth providers with an expired token: the refresh goes through each provider's endpoint.
	{ args: ["auth", "check", "--provider", "anthropic", "--json"], auth: oauth("anthropic", "old", 1) },
	{ args: ["auth", "print-bearer-token", "--provider", "anthropic"], auth: oauth("anthropic", "old", 1) },
	{ args: ["auth", "check", "--provider", "anthropic", "--json", "--credentials"], auth: oauth("anthropic", "old", 1) },
	{ args: ["auth", "print-bearer-token", "--provider", "openai-codex"], auth: oauth("openai-codex", "old", 1, { accountId: "a" }) },
	{ args: ["auth", "print-bearer-token", "--provider", "github-copilot"], auth: oauth("github-copilot", "old", 1) },
	{ args: ["auth", "print-bearer-token", "--provider", "xai"], auth: oauth("xai", "old", 1) },
	{ args: ["auth", "print-bearer-token", "--provider", "kimi-coding"], auth: oauth("kimi-coding", "old", 1) },
	{ args: ["auth", "print-bearer-token", "--provider", "meta"], auth: oauth("meta", "old", 1) },
	{ args: ["auth", "print-bearer-token", "--provider", "openrouter"], auth: oauth("openrouter", "sk-or-key", 9007199254740991) },
	{ args: ["auth", "check", "--provider", "xai", "--json"], auth: oauth("xai", "valid", FAR) },
	{ args: ["auth", "print-api-key", "--provider", "github-copilot"], auth: oauth("github-copilot", "valid", FAR) },
	// A refresh the provider refuses.
	{ args: ["auth", "check", "--provider", "anthropic", "--json"], auth: oauth("anthropic", "old", 1), stubs: failing },
	{ args: ["auth", "print-bearer-token", "--provider", "anthropic"], auth: oauth("anthropic", "old", 1), stubs: failing },
	// models.json providers and keys.
	{ args: ["auth", "check", "--provider", "proxy", "--json"], models: proxy({ apiKey: "literal-key" }) },
	{ args: ["auth", "print-api-key", "--provider", "proxy"], models: proxy({ apiKey: "literal-key" }) },
	{ args: ["auth", "check", "--provider", "proxy", "--json"], models: proxy({ apiKey: "$PROXY_KEY" }) },
	{ args: ["auth", "print-api-key", "--provider", "proxy"], models: proxy({ apiKey: "pre-$PROXY_KEY" }), env: { PROXY_KEY: "env" } },
	{ args: ["auth", "print-api-key", "--provider", "proxy"], models: proxy({ apiKey: "!printf ' cmd-key '" }) },
	{ args: ["auth", "check", "--provider", "proxy", "--json"], models: proxy({ apiKey: "!exit 3" }) },
	{ args: ["auth", "print-api-key", "--provider", "proxy"], models: proxy({ apiKey: "!exit 3" }) },
	{ args: ["auth", "check", "--provider", "proxy", "--json"], models: proxy({}) },
	{ args: ["auth", "check", "--provider", "proxy", "--json", "--credentials"], models: proxy({}), auth: { proxy: { type: "api_key", key: "stored-key" } } },
	{ args: ["auth", "print-bearer-token", "--provider", "proxy"], models: proxy({ apiKey: "literal-key" }) },
	{ args: ["auth", "print-api-key", "--provider", "openai"], models: { providers: { openai: { apiKey: "models-openai" } } }, env: { OPENAI_API_KEY: "env-openai" } },
	{ args: ["auth", "check", "--provider", "anthropic", "--json"], models: { providers: { broken: {} } }, env: { ANTHROPIC_API_KEY: "k" } },
	{ args: ["auth", "print-api-key", "--provider", "nope"], models: proxy({}) },
	// --model: resolved as `resolveCliModel` does (auth-check.ts, credential-print.ts).
	{ args: ["auth", "check", "--model", "claude-sonnet-4-5", "--json"], env: { ANTHROPIC_API_KEY: "k" } },
	{ args: ["auth", "check", "--model", "no-such-model-xyz", "--json"] },
	{ args: ["auth", "check", "--provider", "proxy", "--model", "m", "--json"], models: proxy({ apiKey: "literal-key" }) },
	{ args: ["auth", "check", "--model", "claude-sonnet-4-5", "--json"], models: { providers: { broken: {} } }, env: { ANTHROPIC_API_KEY: "k" } },
	{ args: ["auth", "print-api-key", "--model", "m"], models: proxy({}), auth: { proxy: { type: "api_key", key: "stored-key" } } },
	{ args: ["auth", "print-api-key", "--model", "no-such-model-xyz"], models: proxy({}), auth: { proxy: { type: "api_key", key: "stored-key" } } },
	{ args: ["auth", "print-api-key", "--model", "gpt-5.4"], auth: { openai: { type: "api_key", key: "openai-key" }, openrouter: { type: "api_key", key: "openrouter-key" } } },
	{ args: ["auth", "print-api-key", "--provider", "proxy", "--model", "zzz"], models: proxy({ apiKey: "literal-key" }) },
	{ args: ["auth", "print-bearer-token", "--model", "k3"], auth: oauth("kimi-coding", "kimi-valid", FAR) },
	{ args: ["auth", "print-bearer-token", "--provider", "anthropic", "--model", "claude-sonnet-4-5"], auth: oauth("anthropic", "anthropic-valid", FAR) },
	{ args: ["auth", "print-api-key", "--provider", "anthropic", "--model", "no-such-model-xyz"], env: { ANTHROPIC_API_KEY: "k" } },
	{ args: ["auth", "print-api-key", "--model", "m"], models: proxy({ apiKey: "literal-key" }) },
	{ args: ["auth", "check", "--provider", "anthropic", "--model", "claude-sonnet-4-5", "--json"], env: { ANTHROPIC_API_KEY: "k" } },
	{ args: ["auth", "check", "--model", "anthropic/claude-sonnet-4-5", "--json"], env: { ANTHROPIC_API_KEY: "k" } },
	{ args: ["auth", "check", "--model", "m", "--json"], models: proxy({ apiKey: "literal-key" }) },
	{ args: ["auth", "check", "--model", "proxy/m"], models: proxy({ apiKey: "literal-key" }) },
	{ args: ["auth", "check", "--provider", "anthropic", "--model", "no-such-model-xyz", "--json"], env: { ANTHROPIC_API_KEY: "k" } },
];

const out = [];
for (const spec of cases) {
	const tmp = mkdtempSync(join(tmpdir(), "provider-auth-"));
	const agent = join(tmp, "agent");
	mkdirSync(agent, { recursive: true });
	if (spec.models) writeFileSync(join(agent, "models.json"), JSON.stringify(spec.models));
	if (spec.auth) writeFileSync(join(agent, "auth.json"), JSON.stringify(spec.auth));
	writeFileSync(join(tmp, "stubs.json"), JSON.stringify(spec.stubs ?? stubs));
	const result = await new Promise((resolve) => {
		const proc = spawn(process.execPath, [join(root, "packages/coding-agent/src/cli.ts"), ...spec.args], {
			cwd: tmp,
			env: {
				PATH: "/usr/bin:/bin",
				HOME: tmp,
				PI_OFFLINE: "1",
				NODE_OPTIONS: guardNodeOptions,
				ULTRON_GUARD_LOG: join(tmp, "guard.log"),
				ULTRON_FETCH_STUBS: join(tmp, "stubs.json"),
				ULTRON_FETCH_LOG: join(tmp, "fetch.log"),
				ULTRON_CODING_AGENT_DIR: agent,
				ULTRON_SERVER_DIR: join(tmp, "server"),
				...(spec.env ?? {}),
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
	let requests = [];
	try {
		requests = readFileSync(join(tmp, "fetch.log"), "utf8")
			.split("\n")
			.filter(Boolean)
			.map((line) => JSON.parse(line));
	} catch {}
	let stored = null;
	try {
		stored = JSON.parse(readFileSync(join(agent, "auth.json"), "utf8"));
	} catch {}
	assertOffline(join(tmp, "guard.log"));
	rmSync(tmp, { recursive: true, force: true });
	const times = (value) =>
		JSON.parse(JSON.stringify(value), (key, v) =>
			key === "expires" && typeof v === "number" && v >= started && v < started + 7 * 86_400_000 ? "<now+>" : v,
		);
	out.push({
		args: spec.args,
		models: spec.models ?? null,
		auth: spec.auth ?? null,
		env: spec.env ?? {},
		stubs: spec.stubs === failing ? "failing" : "default",
		stdout: result.stdout,
		stderr: result.stderr,
		code: result.code,
		requests,
		stored: times(stored),
	});
}
writeFileSync(join(here, "provider_auth.json"), `${JSON.stringify({ stubs, failing, cases: out }, null, "\t")}\n`);
