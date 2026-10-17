// Generates crates/ai/fixtures/bedrock-proxy.json from this worktree's TypeScript Bedrock provider
// (`api/bedrock-converse-stream.ts`): when `resolveHttpProxyUrlForTarget(model.baseUrl, options.env)` names a proxy
// (utils/node-http-proxy.ts: `<protocol>_proxy` before `all_proxy`, lower case first, `no_proxy` exceptions), the
// client's request handler is NodeHttpHandler (HTTP/1.1 with proxy agents) instead of the default NodeHttp2Handler;
// a proxy URL that is not HTTP(S) or does not parse is an unhandled rejection (the stream never ends). The handlers' `handle` is replaced by a recorder that throws,
// so nothing is sent; recorded per case are the handler class (or none) and the unhandled rejection of a proxy that cannot be used. AWS_BEDROCK_SKIP_AUTH=1
// keeps credentials out. Each case has a fresh temp dir with only the environment it names (PATH=/usr/bin:/bin,
// PI_OFFLINE=1). `<port>` stands for the loopback port the replay chooses.
// Usage: node crates/ai/fixtures/generate-bedrock-proxy.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { NodeHttp2Handler, NodeHttpHandler } from "@smithy/node-http-handler";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const scratch = mkdtempSync(join(tmpdir(), "ai-bedrock-proxy-"));
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
const PORT = "45999";
const REGION = { AWS_REGION: "us-west-2" };
const PROXY = "http://127.0.0.1:9";
const cases = [
	{ name: "HTTPS_PROXY: HTTP/1.1 through the proxy", env: { ...REGION, HTTPS_PROXY: PROXY } },
	{ name: "a lowercase https_proxy without a scheme", env: { ...REGION, https_proxy: "127.0.0.1:9" } },
	{ name: "ALL_PROXY alone", env: { ...REGION, ALL_PROXY: PROXY } },
	{ name: "HTTP_PROXY does not serve an https target", env: { ...REGION, HTTP_PROXY: PROXY } },
	{ name: "NO_PROXY naming the host keeps HTTP/2", env: { ...REGION, HTTPS_PROXY: PROXY, NO_PROXY: "localhost, 127.0.0.1" } },
	{ name: "NO_PROXY for another port", env: { ...REGION, HTTPS_PROXY: PROXY, NO_PROXY: "127.0.0.1:1" } },
	{ name: "NO_PROXY=*", env: { ...REGION, HTTPS_PROXY: PROXY, no_proxy: "*" } },
	{ name: "a SOCKS proxy fails the request", env: { ...REGION, HTTPS_PROXY: "socks5://127.0.0.1:9" } },
	{ name: "a proxy URL that does not parse fails the request", env: { ...REGION, HTTPS_PROXY: "http://[bad" } },
];

const out = [];
for (const spec of cases) {
	const dir = join(scratch, `case-${out.length}`);
	mkdirSync(join(dir, "home"), { recursive: true });
	for (const key of Object.keys(process.env)) delete process.env[key];
	Object.assign(process.env, baseEnv(dir), spec.env);
	writeFileSync(join(dir, "aws-config"), "");
	writeFileSync(join(dir, "aws-credentials"), "");
	const model = { ...MODEL, baseUrl: `https://127.0.0.1:${PORT}` };
	sent.length = 0;
	// A proxy that cannot be used throws while the client is created, before the stream's own error handling: the
	// stream never ends and the rejection is unhandled. Recorded is that rejection's message.
	let unhandled = null;
	const result = await new Promise((done) => {
		const onRejection = (error) => {
			unhandled = error.message;
			done(null);
		};
		process.once("unhandledRejection", onRejection);
		bedrockStream(model, { messages: [{ role: "user", content: "go", timestamp: 0 }] }, { maxRetries: 0 })
			.result()
			.then((message) => {
				process.removeListener("unhandledRejection", onRejection);
				done(message);
			});
	});
	void result;
	if (sent.length > 1) throw new Error(`${spec.name}: ${sent.length} requests`);
	out.push({ name: spec.name, env: spec.env, handler: sent[0]?.handler ?? null, unhandled });
}

rmSync(scratch, { recursive: true, force: true });
writeFileSync(join(here, "bedrock-proxy.json"), `${JSON.stringify({ cases: out }, null, "\t")}\n`);
assertOffline();
