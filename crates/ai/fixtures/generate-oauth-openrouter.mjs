// Generates crates/ai/fixtures/oauth-openrouter.json from this worktree's TypeScript OpenRouter OAuth
// (`packages/ai/src/auth/oauth/openrouter.ts`): the PKCE login with its one-shot loopback callback server on a
// system-chosen port, raced against the paste prompt; the key exchange; refresh and toAuth. fetch is stubbed with
// canned responses (nothing leaves the machine; offline-guard.mjs refuses any other socket); randomness is a byte
// counter, so the verifier, challenge and callback path are deterministic. Each case scripts the paste prompt's
// answer and the browser's callback hits, and records prompts, notify events, requests, callback responses and the
// credential or error. The callback port is recorded as <port>.
// Usage: node crates/ai/fixtures/generate-oauth-openrouter.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import nodeCrypto from "node:crypto";
import { writeFileSync } from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
delete process.env.ULTRON_OAUTH_CALLBACK_HOST;
delete process.env.PI_OAUTH_CALLBACK_HOST;

let counter = 0;
const nextBytes = (n) => Array.from({ length: n }, () => counter++ & 255);
globalThis.crypto.getRandomValues = (array) => {
	array.set(nextBytes(array.length));
	return array;
};
nodeCrypto.randomBytes = (n) => Buffer.from(nextBytes(n));
const uuid = () => {
	const b = nextBytes(16);
	b[6] = (b[6] & 0x0f) | 0x40;
	b[8] = (b[8] & 0x3f) | 0x80;
	const h = Buffer.from(b).toString("hex");
	return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
};
globalThis.crypto.randomUUID = uuid;
nodeCrypto.randomUUID = uuid;
syncBuiltinESMExports();

const { openRouterOAuth } = await import(pathToFileURL(join(root, "packages/ai/src/auth/oauth/openrouter.ts")).href);

const realFetch = globalThis.fetch;
let queue = [];
let requests = [];
globalThis.fetch = async (url, init = {}) => {
	const target = String(url);
	if (target.startsWith("http://127.0.0.1")) return realFetch(url, init);
	const headers = {};
	for (const [k, v] of Object.entries(init.headers ?? {})) headers[k.toLowerCase()] = String(v);
	requests.push({ url: target, method: init.method ?? "GET", headers, body: init.body === undefined ? null : String(init.body) });
	const next = queue.shift();
	if (!next) throw new Error("unexpected request");
	if (next.status === 0) throw new TypeError("fetch failed");
	return new Response(next.body, { status: next.status });
};

const KEY = '{"key":"sk-or-v1-from-oauth","user_id":"u"}';
// [name, answer (paste prompt; null: never answered), callback hits (query strings), responses]
const cases = [
	["paste redirect url", "{callback}?code=pasted-code", [], [[200, KEY]]],
	["paste bare code", "  bare-code  ", [], [[200, KEY]]],
	["paste query string", "code=query-code&other=1", [], [[200, KEY]]],
	["paste empty", "   ", [], []],
	["exchange http error", "the-code", [], [[401, '{"error":{"message":"Invalid code"}}']]],
	["exchange http error without detail", "the-code", [], [[500, "<html>"]]],
	["exchange without key", "the-code", [], [[200, '{"user_id":"u"}']]],
	["exchange invalid json", "the-code", [], [[200, "<html>"]]],
	["exchange network error", "the-code", [], [[0, ""]]],
	["callback completes", null, ["code=browser-code"], [[200, KEY]]],
	["callback denied", null, ["error=access_denied&error_description=No+thanks"], []],
	["callback without code then paste", "late-code", ["state=x"], [[200, KEY]]],
	["callback wrong route then completes", null, ["@/elsewhere", "code=browser-code"], [[200, KEY]]],
];

const out = [];
for (const [name, answer, hits, responses] of cases) {
	counter = 0;
	queue = responses.map(([status, body]) => ({ status, body }));
	requests = [];
	const prompts = [];
	const events = [];
	const callbackResponses = [];
	const controller = new AbortController();
	let callbackUrl;
	let releasePrompt;
	const interaction = {
		signal: controller.signal,
		prompt: async (request) => {
			const { signal, ...shown } = request;
			prompts.push(shown);
			if (answer === null) {
				// Never answered: withdrawn once a callback settles the login.
				return new Promise((_resolve, reject) => {
					signal?.addEventListener("abort", () => reject(new Error("withdrawn")), { once: true });
				});
			}
			await new Promise((resolve) => {
				releasePrompt = resolve;
				if (hits.length === 0) resolve();
			});
			return answer.replace("{callback}", callbackUrl);
		},
		notify: (event) => {
			events.push(event);
			if (event.type === "auth_url") {
				callbackUrl = new URL(event.url).searchParams.get("callback_url");
				void (async () => {
					for (const hit of hits) {
						const target = hit.startsWith("@") ? new URL(hit.slice(1), callbackUrl).href : `${callbackUrl}?${hit}`;
						const response = await realFetch(target);
						callbackResponses.push({ status: response.status, body: await response.text() });
					}
					releasePrompt?.();
				})();
			}
		},
	};
	let result = null;
	let error = null;
	try {
		const credential = await openRouterOAuth.login(interaction);
		result = { credential, toAuth: await openRouterOAuth.toAuth(credential), refreshed: await openRouterOAuth.refresh(credential) };
	} catch (e) {
		error = e.message;
	}
	await new Promise((resolve) => setTimeout(resolve, 50));
	const port = callbackUrl ? new URL(callbackUrl).port : "";
	const text = JSON.stringify({ name, answer, hits, responses, prompts, events, requests, callbackResponses, result, error });
	const named = port
		? text.split(`127.0.0.1:${port}`).join("127.0.0.1:<port>").split(`127.0.0.1%3A${port}`).join("127.0.0.1%3A<port>")
		: text;
	out.push(JSON.parse(named));
}
assertOffline();
writeFileSync(join(here, "oauth-openrouter.json"), `${JSON.stringify({ cases: out }, null, "\t")}\n`);
