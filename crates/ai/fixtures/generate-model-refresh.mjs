// Generates crates/ai/fixtures/model-refresh.json from this worktree's TypeScript model runtime: stored model
// catalogs (models-store.json) and their refresh.
// - Offline restore (`ModelRuntime.create`, every start): a pi.dev catalog overlay applies only when its
//   `lastModified` is newer than the bundled data (`withRemoteCatalog`); a Radius provider's stored catalog applies as
//   is; without one, a legacy OAuth credential's `gatewayConfig` is imported and persisted (`radiusProvider`).
// - Network refresh as `ultron update --models` runs it (`refreshModelCatalogs`: `refresh({ allowNetwork: true,
//   force: true })`): providers with a credential fetch `/api/models/providers/<id>` from the catalog server (a
//   loopback stand-in for pi.dev: 200 object/array bodies, 304 against the stored etag, 404, a persistent 500) and
//   Radius providers fetch `/v1/config` from their gateway (a loopback stand-in) with their key, refreshing an
//   expired OAuth token first.
// Each case runs in a fresh temp dir with only the environment it names (PATH=/usr/bin:/bin, PI_OFFLINE unset only
// for the network case, no RADIUS_API_KEY so the built-in Radius provider never refreshes); the only traffic is to
// the loopback servers. Ports are recorded as <port>, times after
// the run starts as <now>.
// Usage: node crates/ai/fixtures/generate-model-refresh.mjs
// Every socket must stay on loopback (offline-guard.mjs): checked at the end.
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const scratch = mkdtempSync(join(tmpdir(), "ai-model-refresh-"));
const baseEnv = (dir) => ({
	HOME: join(dir, "home"),
	PATH: "/usr/bin:/bin",
	ULTRON_CODING_AGENT_DIR: join(dir, "agent"),
	ULTRON_SERVER_DIR: join(dir, "server"),
});
for (const key of Object.keys(process.env)) delete process.env[key];
Object.assign(process.env, baseEnv(scratch), { PI_OFFLINE: "1" });

const { ModelRuntime } = await import(pathToFileURL(join(root, "packages/coding-agent/src/core/model-runtime.ts")).href);
const manifest = JSON.parse(readFileSync(join(root, "packages/ai/src/providers/data/.manifest.json"), "utf8"));
const generatedAt = Date.parse(manifest.generatedAt);
const started = Date.now();

const remote = (provider, id, extra = {}) => ({
	id,
	name: `Remote ${id}`,
	api: "openai-completions",
	provider,
	baseUrl: `https://${provider}.invalid/v1`,
	reasoning: false,
	input: ["text"],
	cost: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 64000,
	maxTokens: 4000,
	...extra,
});
const gatewayModel = (id) => ({
	id,
	name: `Gateway ${id}`,
	reasoning: true,
	input: ["text"],
	cost: { input: 3, output: 4, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 200000,
	maxTokens: 9000,
});

// The loopback servers record what they were sent (headers that matter to the protocol only).
const requests = [];
const record = (server, req, body) =>
	requests.push({
		server,
		method: req.method,
		path: req.url,
		accept: req.headers.accept ?? null,
		authorization: req.headers.authorization ?? null,
		ifNoneMatch: req.headers["if-none-match"] ?? null,
		userAgent: req.headers["user-agent"]?.startsWith("pi/") ? "pi/<version>" : (req.headers["user-agent"] ?? null),
		body: body ? Object.fromEntries(new URLSearchParams(body)) : null,
	});
const listen = (handler) =>
	new Promise((resolve) => {
		const server = createServer((req, res) => {
			let body = "";
			req.on("data", (chunk) => {
				body += chunk;
			});
			req.on("end", () => handler(req, res, body));
		});
		server.listen(0, "127.0.0.1", () => resolve(server));
	});
const json = (res, status, value, headers = {}) => {
	res.writeHead(status, { "content-type": "application/json", ...headers });
	res.end(JSON.stringify(value));
};
const newer = new Date(generatedAt + 86_400_000).toUTCString();
const older = new Date(generatedAt - 86_400_000).toUTCString();
const catalog = await listen((req, res, body) => {
	record("catalog", req, body);
	const provider = decodeURIComponent(req.url.split("/").pop());
	if (provider === "deepseek") {
		json(res, 200, { models: [remote("ignored", "deepseek-remote"), { name: "no id" }] }, { "last-modified": newer, etag: '"d1"' });
	} else if (provider === "xai") {
		if (req.headers["if-none-match"] === '"x1"') {
			res.writeHead(304);
			res.end();
		} else json(res, 200, [remote("xai", "grok-fresh")], { "last-modified": newer, etag: '"x2"' });
	} else if (provider === "cerebras") {
		json(res, 404, { error: "not found" });
	} else if (provider === "mistral") {
		json(res, 200, { "mistral-remote": remote("mistral", "mistral-remote") }, { "last-modified": older });
	} else json(res, 500, { error: "boom" });
});
const gateway = await listen((req, res, body) => {
	record("gateway", req, body);
	const port = gateway.address().port;
	if (req.url === "/v1/oauth/token") {
		json(res, 200, { access_token: "gw2-new", refresh_token: "gw2-refresh-new", expires_in: 3600 });
		return;
	}
	const token = (req.headers.authorization ?? "").replace(/^Bearer /, "");
	if (token === "fail-key") {
		res.writeHead(503, { "content-type": "text/plain" });
		res.end("  gateway down  ");
		return;
	}
	json(res, 200, {
		baseUrl: `http://127.0.0.1:${port}/served/${token}`,
		models: [gatewayModel(`m-${token}`), { id: "broken" }],
	});
});
const catalogPort = catalog.address().port;
const gatewayPort = gateway.address().port;
const gatewayUrl = `http://127.0.0.1:${gatewayPort}/v1`;
const FAR = 4102444800000;

const cases = [
	{
		name: "offline restore of stored catalogs",
		network: false,
		models: { providers: { gw: { oauth: "radius", baseUrl: gatewayUrl } } },
		auth: {},
		env: {},
		store: {
			deepseek: { models: [remote("deepseek", "deepseek-remote")], lastModified: generatedAt + 1000, checkedAt: 1 },
			cerebras: { models: [remote("cerebras", "cerebras-old")], lastModified: generatedAt - 1000, checkedAt: 1 },
			groq: { models: [remote("groq", "groq-unstamped")], checkedAt: 1 },
			xai: {
				models: [remote("xai", "grok-4.7", { name: "Grok Restored" }), remote("other", "foreign")],
				lastModified: generatedAt + 1,
				checkedAt: 1,
			},
			gw: { models: [{ ...gatewayModel("stored"), api: "pi-messages", provider: "gw", baseUrl: "http://gw.invalid" }], checkedAt: 1 },
		},
		providers: ["deepseek", "cerebras", "groq", "xai", "gw"],
	},
	{
		name: "legacy radius credential catalog is imported",
		network: false,
		models: { providers: { gw: { oauth: "radius", baseUrl: gatewayUrl } } },
		auth: {
			gw: {
				type: "oauth",
				access: "a",
				refresh: "r",
				expires: FAR,
				gatewayConfig: { baseUrl: "http://legacy.invalid/api", models: [gatewayModel("legacy"), { id: "broken" }] },
			},
		},
		env: {},
		store: null,
		providers: ["gw"],
	},
	{
		name: "update --models refreshes every provider with a credential",
		network: true,
		models: {
			providers: {
				gw: { oauth: "radius", baseUrl: gatewayUrl },
				gw2: { oauth: "radius", baseUrl: `${gatewayUrl}/` },
				gw3: { oauth: "radius", baseUrl: gatewayUrl, apiKey: "gw3" },
				gw4: { oauth: "radius", baseUrl: gatewayUrl, apiKey: "fail-key" },
			},
		},
		auth: {
			gw: { type: "oauth", access: "gw1", refresh: "r1", expires: FAR },
			gw2: { type: "oauth", access: "gw2-old", refresh: "gw2-refresh", expires: 1 },
		},
		env: {
			DEEPSEEK_API_KEY: "k",
			XAI_API_KEY: "k",
			CEREBRAS_API_KEY: "k",
			GROQ_API_KEY: "k",
			MISTRAL_API_KEY: "k",
		},
		store: {
			xai: { models: [remote("xai", "grok-cached")], lastModified: generatedAt + 1, checkedAt: 1, etag: '"x1"' },
			groq: { models: [remote("groq", "groq-cached")], lastModified: generatedAt + 1, checkedAt: 1, etag: '"g1"' },
		},
		providers: ["deepseek", "xai", "cerebras", "groq", "mistral", "gw", "gw2", "gw3", "gw4"],
	},
];

const fill = (text) => text.replaceAll("<gateway>", gatewayUrl);
const unfill = (value) =>
	JSON.parse(
		JSON.stringify(value)
			.replaceAll(String(catalogPort), "<catalog-port>")
			.replaceAll(String(gatewayPort), "<gateway-port>"),
	);
const times = (value) =>
	JSON.parse(JSON.stringify(value), (key, v) =>
		(key === "checkedAt" || key === "expires") && typeof v === "number" && v >= started && v < started + 86_400_000
			? "<now>"
			: v,
	);

const sortKeys = (value) =>
	value && Object.fromEntries(Object.entries(value).sort(([a], [b]) => a.localeCompare(b)));

const out = [];
for (const spec of cases) {
	const dir = join(scratch, `case-${out.length}`);
	mkdirSync(join(dir, "home"), { recursive: true });
	const modelsPath = join(dir, "models.json");
	writeFileSync(modelsPath, fill(JSON.stringify(spec.models)));
	writeFileSync(join(dir, "auth.json"), JSON.stringify(spec.auth));
	if (spec.store) writeFileSync(join(dir, "models-store.json"), JSON.stringify(spec.store));
	for (const key of Object.keys(process.env)) delete process.env[key];
	Object.assign(process.env, baseEnv(dir), spec.env, spec.network ? {} : { PI_OFFLINE: "1" });
	requests.length = 0;
	const runtime = await ModelRuntime.create({
		authPath: join(dir, "auth.json"),
		modelsPath,
		allowModelNetwork: false,
		catalogBaseUrl: `http://127.0.0.1:${catalogPort}`,
	});
	let errors = null;
	if (spec.network) {
		const result = await runtime.refresh({ allowNetwork: true, force: true });
		errors = Object.fromEntries([...result.errors].map(([p, e]) => [p, e.message]).sort(([a], [b]) => a.localeCompare(b)));
	}
	const models = Object.fromEntries(spec.providers.map((p) => [p, runtime.getModels(p)]));
	const read = (file) => {
		try {
			return JSON.parse(readFileSync(join(dir, file), "utf8"));
		} catch {
			return null;
		}
	};
	const sorted = [...requests].sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b)));
	out.push(
		unfill(
			times({
				name: spec.name,
				network: spec.network,
				models: spec.models,
				auth: spec.auth,
				env: spec.env,
				store: spec.store,
				errors,
				catalogs: models,
				requests: sorted,
				// Providers refresh in parallel: their entries land in completion order, recorded here by provider.
				storeAfter: sortKeys(read("models-store.json")),
				authAfter: read("auth.json"),
			}),
		),
	);
}

catalog.close();
gateway.close();
rmSync(scratch, { recursive: true, force: true });
writeFileSync(
	join(here, "model-refresh.json"),
	`${JSON.stringify({ generatedAt: manifest.generatedAt, cases: out }, null, "\t")}\n`,
);
assertOffline();
