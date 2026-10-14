// Generates crates/session-core/fixtures/request-key.json from this worktree's TypeScript model runtime
// (packages/coding-agent/src/core/model-runtime.ts `ModelRuntime.getAuth`, with `setRuntimeApiKey` for `--api-key` as
// main.ts does): which key a request carries given `--api-key`, models.json `apiKey`, auth.json credentials and the
// environment. Each case writes a fresh models.json and auth.json in a private temp agent dir, sets the process
// environment to exactly the case's variables (provider key variables cleared), and records the resolved apiKey (or
// the error message). Fake keys only; no network (PI_OFFLINE, refreshOnCreate off).
// Usage: node crates/session-core/fixtures/generate-request-key.mjs
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const scratch = mkdtempSync(join(tmpdir(), "request-key-"));
const home = join(scratch, "home");
mkdirSync(home);
process.env.HOME = home;
process.env.ULTRON_CODING_AGENT_DIR = join(scratch, "agent");
process.env.ULTRON_SERVER_DIR = join(scratch, "server");
process.env.PI_OFFLINE = "1";

const { ModelRuntime } = await import(pathToFileURL(join(root, "packages/coding-agent/src/core/model-runtime.ts")).href);

// Dates the OAuth credentials are judged against: far from expiry, so no refresh (network) happens.
const FAR = 4102444800000;
const CLEARED = [
	"ANTHROPIC_API_KEY",
	"ANTHROPIC_AUTH_TOKEN",
	"ANTHROPIC_OAUTH_TOKEN",
	"OPENAI_API_KEY",
	"GEMINI_API_KEY",
	"OPENROUTER_API_KEY",
	"STUB_KEY",
	"STORED_VAR",
	"UNSET_VAR",
];

const stub = {
	baseUrl: "http://127.0.0.1:9/v1",
	api: "openai-completions",
	models: [{ id: "sol" }],
};

// [name, provider/model, models.json providers (null: no file), auth.json (object, string = raw text, null: none),
//  env, --api-key]
const cases = [
	["nothing configured", "anthropic/claude-sonnet-4-5", null, null, {}, null],
	["environment only", "anthropic/claude-sonnet-4-5", null, null, { ANTHROPIC_API_KEY: "env-key" }, null],
	["stored api key beats environment", "anthropic/claude-sonnet-4-5", null, { anthropic: { type: "api_key", key: "stored-key" } }, { ANTHROPIC_API_KEY: "env-key" }, null],
	["stored api key alone", "openai/gpt-4o", null, { openai: { type: "api_key", key: "stored-openai" } }, {}, null],
	["models.json key beats environment", "anthropic/claude-sonnet-4-5", { anthropic: { apiKey: "json-key" } }, null, { ANTHROPIC_API_KEY: "env-key" }, null],
	["stored key beats models.json key", "anthropic/claude-sonnet-4-5", { anthropic: { apiKey: "json-key" } }, { anthropic: { type: "api_key", key: "stored-key" } }, { ANTHROPIC_API_KEY: "env-key" }, null],
	["flag beats everything", "anthropic/claude-sonnet-4-5", { anthropic: { apiKey: "json-key" } }, { anthropic: { type: "api_key", key: "stored-key" } }, { ANTHROPIC_API_KEY: "env-key" }, "flag-key"],
	["flag beats oauth", "anthropic/claude-sonnet-4-5", null, { anthropic: { type: "oauth", access: "sk-ant-oat-stored", refresh: "r", expires: FAR } }, {}, "flag-key"],
	["stored template from credential env", "anthropic/claude-sonnet-4-5", null, { anthropic: { type: "api_key", key: "$STORED_VAR", env: { STORED_VAR: "from-credential-env" } } }, { STORED_VAR: "from-process" }, null],
	["stored template from process env", "anthropic/claude-sonnet-4-5", null, { anthropic: { type: "api_key", key: "pre-${STORED_VAR}" } }, { STORED_VAR: "process" }, null],
	["stored template unset falls to environment", "anthropic/claude-sonnet-4-5", { anthropic: { apiKey: "json-key" } }, { anthropic: { type: "api_key", key: "$UNSET_VAR" } }, { ANTHROPIC_API_KEY: "env-key" }, null],
	["stored credential without key falls to environment", "anthropic/claude-sonnet-4-5", { anthropic: { apiKey: "json-key" } }, { anthropic: { type: "api_key" } }, { ANTHROPIC_API_KEY: "env-key" }, null],
	["stored oauth token for anthropic", "anthropic/claude-sonnet-4-5", null, { anthropic: { type: "oauth", access: "sk-ant-oat-stored", refresh: "r", expires: FAR } }, { ANTHROPIC_API_KEY: "env-key" }, null],
	["stored oauth beats models.json key", "anthropic/claude-sonnet-4-5", { anthropic: { apiKey: "json-key" } }, { anthropic: { type: "oauth", access: "sk-ant-oat-stored", refresh: "r", expires: FAR } }, {}, null],
	["oauth for a provider without oauth owns it", "openai/gpt-4o", null, { openai: { type: "oauth", access: "tok", refresh: "r", expires: FAR } }, { OPENAI_API_KEY: "env-openai" }, null],
	["unknown credential type", "anthropic/claude-sonnet-4-5", null, { anthropic: { type: "token", key: "x" } }, { ANTHROPIC_API_KEY: "env-key" }, null],
	["stored key of another provider", "anthropic/claude-sonnet-4-5", null, { openai: { type: "api_key", key: "stored-openai" } }, { ANTHROPIC_API_KEY: "env-key" }, null],
	["malformed auth.json fails the request", "anthropic/claude-sonnet-4-5", { anthropic: { apiKey: "json-key" } }, "{not json", { ANTHROPIC_API_KEY: "env-key" }, null],
	["custom provider stored key beats models.json key", "stubproxy/sol", { stubproxy: { ...stub, apiKey: "$STUB_KEY" } }, { stubproxy: { type: "api_key", key: "stored-stub" } }, { STUB_KEY: "json-stub" }, null],
	["custom provider stored key with unset models.json variable", "stubproxy/sol", { stubproxy: { ...stub, apiKey: "$STUB_KEY" } }, { stubproxy: { type: "api_key", key: "stored-stub" } }, {}, null],
	["custom provider models.json key", "stubproxy/sol", { stubproxy: { ...stub, apiKey: "$STUB_KEY" } }, null, { STUB_KEY: "json-stub" }, null],
	["custom provider unset models.json variable", "stubproxy/sol", { stubproxy: { ...stub, apiKey: "$STUB_KEY" } }, null, {}, null],
	["custom provider stored credential without key", "stubproxy/sol", { stubproxy: { ...stub, apiKey: "json-literal" } }, { stubproxy: { type: "api_key" } }, {}, null],
	["custom provider stored key only", "stubproxy/sol", { stubproxy: { ...stub, apiKey: "json-literal" } }, { stubproxy: { type: "api_key", key: "stored-stub" } }, {}, null],
];

const results = [];
for (const [name, ref, models, auth, env, flag] of cases) {
	const dir = mkdtempSync(join(scratch, "case-"));
	const modelsPath = join(dir, "models.json");
	const authPath = join(dir, "auth.json");
	if (models) writeFileSync(modelsPath, JSON.stringify({ providers: models }));
	if (auth !== null) writeFileSync(authPath, typeof auth === "string" ? auth : JSON.stringify(auth));
	for (const key of CLEARED) delete process.env[key];
	Object.assign(process.env, env);
	const [provider, id] = ref.split("/");
	let outcome;
	try {
		const runtime = await ModelRuntime.create({ authPath, modelsPath, refreshOnCreate: false });
		if (flag) await runtime.setRuntimeApiKey(provider, flag);
		const model = runtime.getModel ? runtime.getModel(provider, id) : undefined;
		const resolution = await runtime.getAuth(model ?? provider);
		outcome = { apiKey: resolution?.auth.apiKey ?? null };
	} catch (error) {
		outcome = { error: error instanceof Error ? error.message : String(error) };
	}
	results.push({ name, provider, id, models, auth, env, apiKeyFlag: flag, ...outcome });
	rmSync(dir, { recursive: true, force: true });
}
rmSync(scratch, { recursive: true, force: true });
writeFileSync(join(here, "request-key.json"), `${JSON.stringify(results, null, "\t")}\n`);
