// Generates crates/ai/fixtures/header-env.json from this worktree's TypeScript model runtime (`ModelRuntime`):
// models.json header templates (`$VAR`, `${VAR}`) of a provider and of its model see the stored credential's `env`
// before the environment (provider-composer.ts composeApiKeyAuth: `configContextEnv(..., explicitEnv)`;
// model-runtime.ts getAuth: `resolveConfiguredModelHeaders(..., { ...resolution.env, ...overrides.env })`); an
// empty value reads as unset (`env[name] || process.env[name]`). For each case: `getAuth` (headers, or its error)
// and the headers the request `completeSimple` sends, captured by a fetch stand-in that answers 400 (nothing leaves
// the process). Each case runs in a fresh temp dir with only the environment it names (PATH=/usr/bin:/bin,
// PI_OFFLINE=1).
// Usage: node crates/ai/fixtures/generate-header-env.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const scratch = mkdtempSync(join(tmpdir(), "ai-header-env-"));
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

const NAMES = ["x-team", "x-both", "x-model", "authorization"];
const sent = [];
globalThis.fetch = async (input, init = {}) => {
	const headers = new Headers(init.headers ?? (typeof input === "object" && "headers" in input ? input.headers : {}));
	const picked = {};
	for (const name of NAMES) if (headers.has(name)) picked[name] = headers.get(name);
	sent.push(picked);
	return new Response(JSON.stringify({ error: { message: "recorded" } }), {
		status: 400,
		headers: { "content-type": "application/json" },
	});
};

const models = {
	providers: {
		custom: {
			baseUrl: "http://127.0.0.1:9/v1",
			api: "openai-completions",
			headers: { "x-team": "$TEAM_ID", "x-both": "${PART_A}-${PART_B}" },
			models: [{ id: "m", headers: { "x-model": "$MODEL_VAR" } }],
		},
	},
};

const cases = [
	{
		name: "the stored env fills provider and model headers before the environment",
		auth: { custom: { type: "api_key", key: "ck", env: { TEAM_ID: "team-stored", MODEL_VAR: "model-stored", PART_A: "a-stored" } } },
		env: { TEAM_ID: "team-env", MODEL_VAR: "model-env", PART_A: "a-env", PART_B: "b-env" },
	},
	{
		name: "without a stored env the environment fills them",
		auth: { custom: { type: "api_key", key: "ck" } },
		env: { TEAM_ID: "team-env", MODEL_VAR: "model-env", PART_A: "a-env", PART_B: "b-env" },
	},
	{
		name: "an empty stored value reads as unset",
		auth: { custom: { type: "api_key", key: "ck", env: { TEAM_ID: "", MODEL_VAR: "", PART_A: "a-stored", PART_B: "b-stored" } } },
		env: { TEAM_ID: "team-env", MODEL_VAR: "model-env" },
	},
	{
		name: "a variable only the stored env has",
		auth: { custom: { type: "api_key", key: "ck", env: { TEAM_ID: "t", MODEL_VAR: "m", PART_A: "a", PART_B: "b" } } },
		env: {},
	},
];

const out = [];
for (const spec of cases) {
	const dir = join(scratch, `case-${out.length}`);
	mkdirSync(join(dir, "home"), { recursive: true });
	mkdirSync(join(dir, "agent"), { recursive: true });
	const modelsPath = join(dir, "agent", "models.json");
	writeFileSync(modelsPath, JSON.stringify(models, null, 2));
	writeFileSync(join(dir, "agent", "auth.json"), JSON.stringify(spec.auth));
	for (const key of Object.keys(process.env)) delete process.env[key];
	Object.assign(process.env, baseEnv(dir), spec.env);
	const runtime = await ModelRuntime.create({ authPath: join(dir, "agent", "auth.json"), modelsPath });
	const model = runtime.getModel("custom", "m");
	let auth;
	try {
		const resolution = await runtime.getAuth(model);
		auth = { headers: resolution?.auth.headers ?? null };
	} catch (error) {
		auth = { error: error.message };
	}
	sent.length = 0;
	const message = await runtime.completeSimple(model, { messages: [{ role: "user", content: "go", timestamp: 0 }] }, { maxRetries: 0 });
	out.push({
		name: spec.name,
		auth: spec.auth,
		env: spec.env,
		getAuth: auth,
		request: sent.length === 1 ? sent[0] : null,
		errorMessage: sent.length === 1 ? null : message.errorMessage ?? null,
	});
}

rmSync(scratch, { recursive: true, force: true });
writeFileSync(join(here, "header-env.json"), `${JSON.stringify({ models, cases: out }, null, "\t")}\n`);
assertOffline();
