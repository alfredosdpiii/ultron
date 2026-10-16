// Generates crates/ai/fixtures/proxy-env.json: which proxy undici's EnvHttpProxyAgent (the TypeScript product's HTTP
// stack, `configureHttpDispatcher` in packages/coding-agent/src/core/http-dispatcher.ts) picks for a URL under a set
// of proxy variables. Each case builds the agent with only those variables in process.env, swaps the dispatch of its
// three inner agents (direct, HTTP proxy, HTTPS proxy) for recorders, and dispatches one request: nothing connects.
// tests/proxy_env.rs replays the cases through `ai::net::ProxyEnv::proxy_for`.
// Usage: node crates/ai/fixtures/generate-proxy-env.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const require = createRequire(join(root, "packages/coding-agent/package.json"));
const undici = require("undici");

const NAMES = ["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY", "no_proxy", "NO_PROXY", "all_proxy", "ALL_PROXY"];
for (const name of NAMES) delete process.env[name];

const inner = (agent, description) => {
	const symbol = Object.getOwnPropertySymbols(agent).find((s) => s.description === description);
	return agent[symbol];
};

function pick(env, url) {
	for (const name of NAMES) delete process.env[name];
	Object.assign(process.env, env);
	const agent = new undici.EnvHttpProxyAgent();
	const direct = inner(agent, "no proxy agent");
	const http = inner(agent, "http proxy agent");
	const https = inner(agent, "https proxy agent");
	const httpValue = env.http_proxy ?? env.HTTP_PROXY;
	const httpsValue = env.https_proxy ?? env.HTTPS_PROXY;
	let chosen;
	const record = (label) => () => {
		chosen = label;
		return true;
	};
	// Own properties shadow the prototype's dispatch; an agent used for two roles keeps its first label.
	https.dispatch = record(https === direct ? null : https === http ? httpValue : httpsValue);
	http.dispatch = record(http === direct ? null : httpValue);
	direct.dispatch = record(null);
	const parsed = new URL(url);
	agent.dispatch({ origin: parsed.origin, path: parsed.pathname, method: "GET" }, { onError() {}, onRequestStart() {} });
	for (const name of NAMES) delete process.env[name];
	return chosen;
}

const P = "http://proxy.test:3128";
const S = "http://secure-proxy.test:8443";
const cases = [];
const add = (env, urls) => {
	for (const url of urls) cases.push({ env, url, proxy: pick(env, url) });
};
const urls = [
	"http://example.com/x",
	"https://example.com/x",
	"https://api.example.com:8443/v1",
	"http://localhost:8080/",
	"https://127.0.0.1/",
	"http://[::1]:9000/",
	"https://EXAMPLE.org./",
	"https://sub.internal.test/",
	"https://internal.test/",
	"https://notinternal.test/",
];
add({}, urls);
add({ HTTP_PROXY: P }, urls);
add({ http_proxy: P }, urls);
add({ HTTPS_PROXY: S }, urls);
add({ HTTP_PROXY: P, HTTPS_PROXY: S }, urls);
add({ http_proxy: "", HTTP_PROXY: P }, urls);
add({ https_proxy: S, HTTPS_PROXY: "http://ignored.test:1" , HTTP_PROXY: P }, urls);
add({ HTTPS_PROXY: "", HTTP_PROXY: P }, urls);
add({ HTTP_PROXY: P, NO_PROXY: "*" }, urls);
add({ HTTP_PROXY: P, NO_PROXY: "localhost,127.0.0.1,::1" }, urls);
add({ HTTP_PROXY: P, NO_PROXY: "internal.test" }, urls);
add({ HTTP_PROXY: P, NO_PROXY: ".internal.test" }, urls);
add({ HTTP_PROXY: P, NO_PROXY: "*.internal.test" }, urls);
add({ HTTP_PROXY: P, NO_PROXY: "example.com:443 api.example.com:8443" }, urls);
add({ HTTP_PROXY: P, NO_PROXY: "[::1]:9000, EXAMPLE.ORG." }, urls);
add({ HTTP_PROXY: P, NO_PROXY: "", no_proxy: "example.com" }, urls);
add({ HTTP_PROXY: P, no_proxy: "", NO_PROXY: "example.com" }, urls);
add({ HTTP_PROXY: P, NO_PROXY: "localhost:9999,*,example.com" }, urls);
add({ HTTPS_PROXY: S, NO_PROXY: "example.com" }, urls);
add({ ALL_PROXY: P }, urls);

assertOffline();
const out = join(here, "proxy-env.json");
writeFileSync(out, `${JSON.stringify({ generator: "crates/ai/fixtures/generate-proxy-env.mjs", undici: require("undici/package.json").version, cases }, null, "\t")}\n`);
console.log(`wrote ${cases.length} cases to ${out}`);
