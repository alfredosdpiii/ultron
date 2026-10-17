// Generates crates/ai/fixtures/bedrock-endpoint.json from this worktree's TypeScript Bedrock provider
// (`api/bedrock-converse-stream.ts` over `@aws-sdk/client-bedrock-runtime`): which endpoint a Converse stream request
// goes to and which request handler sends it.
// - With a standard catalog base URL and a configured region the client sets no `endpoint`, so the SDK's configured
//   endpoint applies (`@smithy/core` getEndpointFromConfig("Bedrock Runtime")): AWS_IGNORE_CONFIGURED_ENDPOINT_URLS,
//   AWS_ENDPOINT_URL_BEDROCK_RUNTIME, AWS_ENDPOINT_URL, the profile's services section, the profile's endpoint_url.
// - AWS_BEDROCK_FORCE_HTTP1=1 swaps the default NodeHttp2Handler for NodeHttpHandler.
// The handlers' `handle` is replaced by a recorder that throws, so nothing is sent; recorded per case are the handler
// class and the request URL. AWS_BEDROCK_SKIP_AUTH=1 keeps credentials out. Each case has a fresh temp dir with only
// the environment it names (PATH=/usr/bin:/bin, PI_OFFLINE=1; AWS_CONFIG_FILE / AWS_SHARED_CREDENTIALS_FILE inside
// it). Paths are recorded as <dir>; `<port>` stands for a loopback port the replay chooses.
// Usage: node crates/ai/fixtures/generate-bedrock-endpoint.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { NodeHttp2Handler, NodeHttpHandler } from "@smithy/node-http-handler";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const scratch = mkdtempSync(join(tmpdir(), "ai-bedrock-endpoint-"));
const baseEnv = (dir) => ({
	HOME: join(dir, "home"),
	PATH: "/usr/bin:/bin",
	PI_OFFLINE: "1",
	ULTRON_CODING_AGENT_DIR: join(dir, "agent"),
	ULTRON_SERVER_DIR: join(dir, "server"),
	AWS_CONFIG_FILE: join(dir, "aws-config"),
	AWS_SHARED_CREDENTIALS_FILE: join(dir, "aws-credentials"),
	AWS_BEDROCK_SKIP_AUTH: "1",
});
for (const key of Object.keys(process.env)) delete process.env[key];
Object.assign(process.env, baseEnv(scratch));

const sent = [];
for (const Handler of [NodeHttp2Handler, NodeHttpHandler]) {
	Handler.prototype.handle = async function (request) {
		const port = request.port ? `:${request.port}` : "";
		sent.push({ handler: Handler.name, url: `${request.protocol}//${request.hostname}${port}${request.path}` });
		throw new Error("recorded");
	};
}

const { stream: bedrockStream } = await import(
	pathToFileURL(join(root, "packages/ai/src/api/bedrock-converse-stream.ts")).href
);

const MODEL = {
	id: "amazon.nova-lite-v1:0",
	name: "Nova Lite",
	api: "bedrock-converse-stream",
	provider: "amazon-bedrock",
	baseUrl: "https://bedrock-runtime.us-east-1.amazonaws.com",
	reasoning: false,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 300000,
	maxTokens: 5000,
};
// Stands in for the replay's loopback port (nothing listens on it here: the handlers never send).
const PORT = "45999";
const REGION = { AWS_REGION: "us-west-2" };
const cases = [
	{ name: "default endpoint for the region", env: REGION },
	{ name: "AWS_ENDPOINT_URL", env: { ...REGION, AWS_ENDPOINT_URL: "http://127.0.0.1:<port>/global" } },
	{
		name: "the service variable wins over AWS_ENDPOINT_URL",
		env: {
			...REGION,
			AWS_ENDPOINT_URL: "http://127.0.0.1:<port>/global",
			AWS_ENDPOINT_URL_BEDROCK_RUNTIME: "http://127.0.0.1:<port>/service",
		},
	},
	{
		name: "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS=true ignores them",
		env: { ...REGION, AWS_ENDPOINT_URL: "http://127.0.0.1:<port>/global", AWS_IGNORE_CONFIGURED_ENDPOINT_URLS: "true" },
	},
	{
		name: "AWS_IGNORE_CONFIGURED_ENDPOINT_URLS=false",
		env: { ...REGION, AWS_ENDPOINT_URL: "http://127.0.0.1:<port>/global", AWS_IGNORE_CONFIGURED_ENDPOINT_URLS: "false" },
	},
	{
		name: "an ignore value that is not a boolean falls through to the profile",
		env: { ...REGION, AWS_ENDPOINT_URL: "http://127.0.0.1:<port>/global", AWS_IGNORE_CONFIGURED_ENDPOINT_URLS: "yes" },
		config: "[default]\nignore_configured_endpoint_urls = true\n",
	},
	{ name: "the profile's endpoint_url", env: REGION, config: "[default]\nendpoint_url = http://127.0.0.1:<port>/profile\n" },
	{
		name: "the services section wins over the profile's endpoint_url",
		env: REGION,
		config:
			"[default]\nendpoint_url = http://127.0.0.1:<port>/profile\nservices = mine\n\n[services mine]\nbedrock_runtime =\n  endpoint_url = http://127.0.0.1:<port>/services\n",
	},
	{
		name: "a services section without bedrock_runtime uses the profile's endpoint_url",
		env: REGION,
		config:
			"[default]\nendpoint_url = http://127.0.0.1:<port>/profile\nservices = mine\n\n[services mine]\ns3 =\n  endpoint_url = http://127.0.0.1:<port>/s3\n",
	},
	{
		name: "a missing services section uses no configured endpoint",
		env: REGION,
		config: "[default]\nendpoint_url = http://127.0.0.1:<port>/profile\nservices = absent\n",
	},
	{
		name: "AWS_PROFILE selects the profile",
		env: { ...REGION, AWS_PROFILE: "p" },
		config: "[default]\nendpoint_url = http://127.0.0.1:<port>/default\n\n[profile p]\nendpoint_url = http://127.0.0.1:<port>/p\n",
	},
	{
		name: "the credentials file's endpoint_url under the config file's",
		env: REGION,
		credentials: "[default]\nendpoint_url = http://127.0.0.1:<port>/credentials\n",
	},
	{
		name: "the profile's ignore_configured_endpoint_urls",
		env: { ...REGION, AWS_ENDPOINT_URL: "http://127.0.0.1:<port>/global" },
		config: "[default]\nignore_configured_endpoint_urls = true\n",
	},
	{
		name: "a custom base URL is the endpoint: configured ones do not apply",
		env: { ...REGION, AWS_ENDPOINT_URL: "http://127.0.0.1:<port>/global" },
		baseUrl: "http://127.0.0.1:<port>/proxy",
	},
	{
		name: "no region: the catalog endpoint is explicit",
		env: { AWS_ENDPOINT_URL: "http://127.0.0.1:<port>/global" },
	},
	{ name: "AWS_BEDROCK_FORCE_HTTP1=1 uses HTTP/1.1", env: { ...REGION, AWS_BEDROCK_FORCE_HTTP1: "1" } },
	{ name: "AWS_BEDROCK_FORCE_HTTP1=true does not", env: { ...REGION, AWS_BEDROCK_FORCE_HTTP1: "true" } },
];

const out = [];
for (const spec of cases) {
	const dir = join(scratch, `case-${out.length}`);
	mkdirSync(join(dir, "home"), { recursive: true });
	for (const key of Object.keys(process.env)) delete process.env[key];
	const fill = (text) => text.replaceAll("<port>", PORT);
	Object.assign(process.env, baseEnv(dir), JSON.parse(fill(JSON.stringify(spec.env))));
	writeFileSync(join(dir, "aws-config"), fill(spec.config ?? ""));
	writeFileSync(join(dir, "aws-credentials"), fill(spec.credentials ?? ""));
	const model = { ...MODEL, ...(spec.baseUrl ? { baseUrl: fill(spec.baseUrl) } : {}) };
	sent.length = 0;
	const stream = bedrockStream(
		model,
		{ messages: [{ role: "user", content: "go", timestamp: 0 }] },
		{ maxRetries: 0 },
	);
	const message = await stream.result();
	if (sent.length !== 1) throw new Error(`${spec.name}: ${sent.length} requests (${message.errorMessage})`);
	out.push({
		name: spec.name,
		env: spec.env,
		config: spec.config ?? null,
		credentials: spec.credentials ?? null,
		baseUrl: spec.baseUrl ?? model.baseUrl,
		request: JSON.parse(JSON.stringify(sent[0]).replaceAll(dir, "<dir>").replaceAll(PORT, "<port>")),
	});
}

rmSync(scratch, { recursive: true, force: true });
writeFileSync(join(here, "bedrock-endpoint.json"), `${JSON.stringify({ cases: out }, null, "\t")}\n`);
assertOffline();
