// Generates crates/ai/fixtures/model-auth.json from this worktree's TypeScript model runtime (`ModelRuntime`):
// request auth that depends on models.json and auth.json together.
// - GitHub Copilot OAuth with a models.json `baseUrl`: what `getAuth` returns (the token's proxy endpoint is the
//   request base URL, `agent-session.ts` `result.auth.baseUrl ? { ...model, baseUrl } : model`).
// - `!command` API keys and headers: run again for every `getAuth` (no cache), counted by the commands themselves.
// - Kimi For Coding OAuth: its auth is an `Authorization: Bearer <access>` header and no key (`toAuth`), merged under
//   the models.json headers (`getAuth`'s `mergeHeaders(auth.headers, configuredHeaders)`).
// - Radius OAuth for a models.json provider (`oauth: "radius"`, `model-runtime.ts` `configureRadiusProviders`): its
//   models (the bundled Radius catalog only on the default gateway, plus models-store.json), availability, the
//   stored access token, the `RADIUS_API_KEY` fallback, and a refresh of an expired token against the provider's
//   own gateway (a loopback server standing in for it; its port is recorded as <port>).
// Each case runs in a fresh temp dir with only the environment it names (PATH=/usr/bin:/bin, PI_OFFLINE=1); the
// only network traffic is to the loopback gateway. Paths are recorded as <dir>.
// Usage: node crates/ai/fixtures/generate-model-auth.mjs
// Every socket must stay on loopback (offline-guard.mjs): checked at the end.
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const scratch = mkdtempSync(join(tmpdir(), "ai-model-auth-"));
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

// The loopback Radius gateway: answers token requests and records them.
const requests = [];
const server = createServer((req, res) => {
	let body = "";
	req.on("data", (chunk) => {
		body += chunk;
	});
	req.on("end", () => {
		requests.push({ method: req.method, path: req.url, body: Object.fromEntries(new URLSearchParams(body)) });
		res.setHeader("content-type", "application/json");
		res.end(JSON.stringify({ access_token: "refreshed-access", refresh_token: "refreshed-refresh", expires_in: 3600, scope: "gateway" }));
	});
});
await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
const port = server.address().port;

const FAR = 4102444800000; // 2100-01-01
const counter = (name, prefix) =>
	`!n=$(cat <dir>/${name} 2>/dev/null || echo 0); n=$((n+1)); echo $n > <dir>/${name}; echo ${prefix}$n`;
const piModel = (id) => ({
	id,
	name: `Stored ${id}`,
	api: "pi-messages",
	provider: "gw",
	baseUrl: "http://127.0.0.1:<port>/gw",
	reasoning: true,
	input: ["text"],
	cost: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 100000,
	maxTokens: 8000,
});

const cases = [
	{
		name: "copilot oauth sends to the token's proxy endpoint over a models.json baseUrl",
		models: { providers: { "github-copilot": { baseUrl: "http://127.0.0.1:9/cp" } } },
		auth: { "github-copilot": { type: "oauth", access: "tid=x;exp=9999999999;proxy-ep=proxy.business.githubcopilot.com;", refresh: "r", expires: FAR } },
		env: {},
		calls: [["github-copilot", "gpt-5.4", 1]],
	},
	{
		name: "copilot oauth without a proxy endpoint uses the enterprise endpoint",
		models: { providers: { "github-copilot": { baseUrl: "http://127.0.0.1:9/cp" } } },
		auth: { "github-copilot": { type: "oauth", access: "tid=x;exp=9999999999;", refresh: "r", expires: FAR, enterpriseUrl: "ghe.example.com" } },
		env: {},
		calls: [["github-copilot", "gpt-5.4", 1]],
	},
	{
		name: "command keys and headers run for every request",
		models: {
			providers: {
				cmdp: {
					baseUrl: "http://127.0.0.1:9/v1",
					api: "openai-completions",
					apiKey: counter("key", "key-"),
					headers: { "x-provider": counter("provider", "p-") },
					models: [{ id: "m", headers: { "x-model": counter("model", "m-") } }],
				},
			},
		},
		auth: {},
		env: {},
		calls: [["cmdp", "m", 3]],
	},
	{
		name: "radius provider on the default gateway lists the bundled catalog under its own id",
		models: { providers: { myradius: { oauth: "radius", baseUrl: "https://radius.pi.dev/v1", name: "My Radius" } } },
		auth: { myradius: { type: "oauth", access: "radius-access", refresh: "radius-refresh", expires: FAR } },
		env: {},
		calls: [["myradius", "balanced", 1]],
	},
	{
		name: "radius provider without credentials falls back to RADIUS_API_KEY",
		models: { providers: { myradius: { oauth: "radius", baseUrl: "https://radius.pi.dev/v1" } } },
		auth: {},
		env: { RADIUS_API_KEY: "env-radius" },
		calls: [["myradius", "balanced", 1]],
	},
	{
		name: "radius provider without any credential is unavailable",
		models: { providers: { myradius: { oauth: "radius", baseUrl: "https://radius.pi.dev/v1" } } },
		auth: {},
		env: {},
		calls: [["myradius", "balanced", 1]],
	},
	{
		name: "radius provider on its own gateway lists the stored catalog and refreshes there",
		models: { providers: { gw: { oauth: "radius", baseUrl: "http://127.0.0.1:<port>/v1" } } },
		store: { gw: { models: [piModel("fast"), piModel("deep")], checkedAt: 1 } },
		auth: { gw: { type: "oauth", access: "old-access", refresh: "old-refresh", expires: 1 } },
		env: {},
		calls: [["gw", "deep", 1]],
	},
	{
		name: "radius provider on its own gateway with a valid token",
		models: { providers: { gw: { oauth: "radius", baseUrl: "http://127.0.0.1:<port>/v1/", headers: { "x-gw": "$GW_HEADER" } } } },
		store: { gw: { models: [piModel("fast")], checkedAt: 1 } },
		auth: { gw: { type: "oauth", access: "gw-access", refresh: "gw-refresh", expires: FAR } },
		env: { GW_HEADER: "h" },
		calls: [["gw", "fast", 1]],
	},
	{
		name: "built-in radius with a stored oauth credential",
		models: { providers: {} },
		auth: { radius: { type: "oauth", access: "builtin-access", refresh: "builtin-refresh", expires: FAR } },
		env: {},
		calls: [["radius", "balanced", 1]],
	},
	{
		name: "kimi oauth sends its access token as an Authorization header",
		models: { providers: {} },
		auth: { "kimi-coding": { type: "oauth", access: "kimi-access", refresh: "kimi-refresh", expires: FAR } },
		env: {},
		calls: [["kimi-coding", "k3", 1]],
	},
	{
		name: "kimi oauth headers go out with the models.json headers",
		models: {
			providers: {
				"kimi-coding": { headers: { "x-kimi": "$KIMI_HEADER" } },
			},
		},
		auth: { "kimi-coding": { type: "oauth", access: "kimi-access", refresh: "kimi-refresh", expires: FAR } },
		env: { KIMI_HEADER: "k" },
		calls: [["kimi-coding", "k3", 1]],
	},
];

const fill = (text, dir) => text.replaceAll("<dir>", dir).replaceAll("<port>", String(port));
const unfill = (text, dir) => text.replaceAll(dir, "<dir>").replaceAll(String(port), "<port>");
const json = (value, dir) => (value === undefined ? null : JSON.parse(unfill(JSON.stringify(value), dir)));

const out = [];
for (const spec of cases) {
	const dir = join(scratch, `case-${out.length}`);
	mkdirSync(join(dir, "home"), { recursive: true });
	const modelsPath = join(dir, "models.json");
	writeFileSync(modelsPath, fill(JSON.stringify(spec.models, null, 2), dir));
	writeFileSync(join(dir, "auth.json"), JSON.stringify(spec.auth));
	if (spec.store) writeFileSync(join(dir, "models-store.json"), fill(JSON.stringify(spec.store), dir));
	for (const key of Object.keys(process.env)) delete process.env[key];
	Object.assign(process.env, baseEnv(dir), spec.env);
	requests.length = 0;
	const runtime = await ModelRuntime.create({ authPath: join(dir, "auth.json"), modelsPath });
	const providers = [...new Set(spec.calls.map(([provider]) => provider))];
	const models = Object.fromEntries(providers.map((p) => [p, runtime.getModels(p).map((m) => `${m.provider}/${m.id}`)]));
	const available = runtime
		.getAvailableSnapshot()
		.filter((m) => providers.includes(m.provider))
		.map((m) => `${m.provider}/${m.id}`);
	const calls = [];
	for (const [provider, id, times] of spec.calls) {
		const model = runtime.getModel(provider, id);
		const results = [];
		for (let i = 0; i < times; i++) {
			try {
				const resolution = model ? await runtime.getAuth(model) : undefined;
				results.push(
					resolution
						? { apiKey: resolution.auth.apiKey ?? null, baseUrl: resolution.auth.baseUrl ?? null, headers: resolution.auth.headers ?? null }
						: null,
				);
			} catch (error) {
				results.push({ error: error.message });
			}
		}
		calls.push({ model: `${provider}/${id}`, found: json(model, dir), results: json(results, dir) });
	}
	const stored = JSON.parse(readFileSync(join(dir, "auth.json"), "utf8"));
	const credentials = Object.fromEntries(
		Object.entries(stored).map(([p, c]) => [p, { type: c.type, access: c.access ?? null, refresh: c.refresh ?? null }]),
	);
	out.push({
		name: spec.name,
		models: spec.models,
		store: spec.store ?? null,
		auth: spec.auth,
		env: spec.env,
		providers: models,
		available,
		calls,
		gatewayRequests: json(requests, dir),
		credentials,
	});
}

server.close();
rmSync(scratch, { recursive: true, force: true });
writeFileSync(join(here, "model-auth.json"), `${JSON.stringify({ cases: out }, null, "\t")}\n`);
assertOffline();
