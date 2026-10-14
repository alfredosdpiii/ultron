// Generates crates/ai/fixtures/oauth-login.json from this worktree's TypeScript packages/ai OAuth login flows.
// fetch is stubbed with canned responses; setTimeout advances a fake clock instead of waiting; crypto randomness is a
// byte counter, so PKCE verifiers and states are deterministic. Each case scripts the interaction's prompt answers and
// records prompts, notify events, every request (with the fake time it was sent at) and the credential or error.
// Cases with `hits` drive the real callback server over loopback and record its responses.
// Usage: node crates/ai/fixtures/generate-oauth-login.mjs
import nodeCrypto from "node:crypto";
import { writeFileSync } from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const src = (path) => import(pathToFileURL(join(root, "packages/ai/src", path)).href);

delete process.env.ULTRON_OAUTH_CALLBACK_HOST;
delete process.env.PI_OAUTH_CALLBACK_HOST;

const START = 1767225600000;
let now = START;
Date.now = () => now;
let counter = 0;
const nextBytes = (n) => Array.from({ length: n }, () => counter++ & 255);
globalThis.crypto.getRandomValues = (array) => {
	array.set(nextBytes(array.length));
	return array;
};
nodeCrypto.randomBytes = (n) => Buffer.from(nextBytes(n));
syncBuiltinESMExports();
// Only timers set by packages/ai code are faked; node internals (undici) keep real timers.
const realSetTimeout = globalThis.setTimeout;
const realClearTimeout = globalThis.clearTimeout;
const fromProduct = () => (new Error().stack ?? "").includes("/packages/ai/src/");
globalThis.setTimeout = (fn, ms = 0, ...args) => {
	if (!fromProduct()) return realSetTimeout(fn, ms, ...args);
	now += Math.max(0, ms);
	setImmediate(fn);
	return { fake: true };
};
globalThis.clearTimeout = (id) => {
	if (!id?.fake) realClearTimeout(id);
};

const { anthropicOAuth } = await src("auth/oauth/anthropic.ts");
const { openaiCodexOAuth } = await src("auth/oauth/openai-codex.ts");
const { githubCopilotOAuth } = await src("auth/oauth/github-copilot.ts");
const { oauthErrorHtml, oauthSuccessHtml } = await src("auth/oauth/oauth-page.ts");
const auths = { anthropic: anthropicOAuth, "openai-codex": openaiCodexOAuth, "github-copilot": githubCopilotOAuth };
const ports = { anthropic: 53692, "openai-codex": 1455 };

const STATUS_TEXT = {
	200: "OK",
	400: "Bad Request",
	401: "Unauthorized",
	403: "Forbidden",
	404: "Not Found",
	429: "Too Many Requests",
	502: "Bad Gateway",
	500: "Internal Server Error",
};
const realFetch = globalThis.fetch;
let queue = [];
let requests = [];
let abortAt = 0;
let controller;
globalThis.fetch = async (url, init = {}) => {
	if (init.signal?.aborted) throw new DOMException("This operation was aborted", "AbortError");
	const headers = {};
	for (const [k, v] of Object.entries(init.headers ?? {})) headers[k.toLowerCase()] = String(v);
	requests.push({
		at: now,
		url: String(url),
		method: init.method ?? "GET",
		headers,
		body: init.body === undefined ? "" : String(init.body),
	});
	if (requests.length === abortAt) controller.abort();
	const next = queue.shift();
	if (!next) throw new Error("unexpected request");
	if (next.status === 0) throw new TypeError("fetch failed");
	return new Response(next.body, { status: next.status, statusText: STATUS_TEXT[next.status] ?? "", headers: next.headers });
};

const jwt = (payload) =>
	`${Buffer.from('{"alg":"none"}').toString("base64")}.${Buffer.from(JSON.stringify(payload)).toString("base64")}.sig`;
const CODEX_JWT = jwt({ "https://api.openai.com/auth": { chatgpt_account_id: "account-789" } });
const codexTokens = JSON.stringify({ access_token: CODEX_JWT, refresh_token: "r2", expires_in: 3600 });
const anthropicTokens = '{"access_token":"access-token","refresh_token":"refresh-token","expires_in":3600}';
const COPILOT_TOKEN = "tid=abc;exp=1767229200;proxy-ep=proxy.individual.githubcopilot.com;sku=x";
const COPILOT_ENT_TOKEN = "tid=abc;exp=1767229200;sku=x";
const copilotToken = (token) => JSON.stringify({ token, expires_at: 1767229200 });
const deviceCode = (extra = {}) =>
	JSON.stringify({
		device_code: "dc",
		user_code: "ABCD-1234",
		verification_uri: "https://github.com/login/device",
		interval: 5,
		expires_in: 900,
		...extra,
	});
const models = (items) => JSON.stringify({ data: items });
const policyModels = models([
	{ id: "gpt-5.4", model_picker_enabled: true, policy: { state: "enabled" } },
	{ id: "claude-sonnet-5", model_picker_enabled: true, policy: { state: "unconfigured" } },
	{ id: "claude-opus-5", model_picker_enabled: true, policy: { state: "unconfigured" } },
	{ id: "grok-4.7", model_picker_enabled: true, policy: { state: "unconfigured" } },
	{ id: "not-in-catalog", model_picker_enabled: true, policy: { state: "unconfigured" } },
]);
const fallbackModels = models([
	{ id: "gpt-5.4", model_picker_enabled: false, policy: { state: "enabled" } },
	{ id: "claude-sonnet-5", model_picker_enabled: false, policy: { state: "unconfigured" } },
]);
const MANUAL = "{redirect}?code=manual-code&state={state}";
const WAIT = { wait: true };
const reject = (message) => ({ reject: message });
const callbackHits = (paths) => paths;

// [name, provider, answers, responses, options]
const cases = [
	// Anthropic browser login: a pasted code in each accepted form, then failures.
	["anthropic manual redirect url", "anthropic", [MANUAL], [[200, anthropicTokens]]],
	["anthropic manual url without scheme", "anthropic", ["localhost:53692/callback?code=pasted-code&state={state}"], [[200, anthropicTokens]]],
	["anthropic manual query string", "anthropic", ["code=pasted-code&state={state}"], [[200, anthropicTokens]]],
	["anthropic manual code#state", "anthropic", ["pasted-code#{state}#extra"], [[200, anthropicTokens]]],
	["anthropic manual bare code", "anthropic", ["  the-code  "], [[200, anthropicTokens]]],
	["anthropic manual state mismatch", "anthropic", ["code=x&state=other"], []],
	["anthropic manual empty", "anthropic", ["   "], []],
	["anthropic prompt rejects", "anthropic", [reject("Login cancelled")], []],
	["anthropic exchange http error", "anthropic", [MANUAL], [[400, '{"error":"invalid_grant"}']]],
	["anthropic exchange network error", "anthropic", [MANUAL], [[0, ""]]],
	["anthropic exchange invalid json", "anthropic", [MANUAL], [[200, "<html>"]]],
	[
		"anthropic callback server",
		"anthropic",
		[WAIT],
		[[200, anthropicTokens]],
		{
			hits: callbackHits([
				"/other",
				"/callback?error=access_denied&code=x&state={state}",
				"/callback?code=x",
				"/callback?code=x&state=wrong",
				"/callback?code=browser-code&state={state}",
			]),
		},
	],

	// OpenAI Codex: browser login (manual paste and callback server) and device-code login.
	["codex browser manual", "openai-codex", ["browser", MANUAL], [[200, codexTokens]]],
	["codex browser code#state", "openai-codex", ["browser", "pasted#{state}"], [[200, codexTokens]]],
	["codex browser bare code", "openai-codex", ["browser", "pasted"], [[200, codexTokens]]],
	["codex browser state mismatch", "openai-codex", ["browser", "http://localhost:1455/auth/callback?code=x&state=nope"], []],
	["codex browser empty", "openai-codex", ["browser", ""], []],
	["codex browser exchange missing fields", "openai-codex", ["browser", MANUAL], [[200, '{"access_token":"a","refresh_token":"","expires_in":1}']]],
	["codex browser exchange null", "openai-codex", ["browser", MANUAL], [[200, "null"]]],
	["codex browser exchange http error", "openai-codex", ["browser", MANUAL], [[401, ""]]],
	["codex browser exchange no account", "openai-codex", ["browser", MANUAL], [[200, JSON.stringify({ access_token: "opaque", refresh_token: "r", expires_in: 1 })]]],
	[
		"codex callback server",
		"openai-codex",
		["browser", WAIT],
		[[200, codexTokens]],
		{
			hits: callbackHits([
				"/callback",
				"/auth/callback?code=x&state=bad",
				"/auth/callback?state={state}",
				"/auth/callback?code=browser-code&state={state}",
			]),
		},
	],
	["codex unknown method", "openai-codex", ["sideways"], []],
	["codex method prompt rejects", "openai-codex", [reject("Login cancelled")], []],
	[
		"codex device code",
		"openai-codex",
		["device_code"],
		[
			[200, '{"device_auth_id":"dev-1","user_code":"CODE-1","interval":" 5 "}'],
			[403, ""],
			[404, ""],
			[400, '{"error":"deviceauth_authorization_pending"}'],
			[400, '{"error":{"code":"slow_down"}}'],
			[200, '{"authorization_code":"oauth-code","code_verifier":"device-code-verifier"}'],
			[200, codexTokens],
		],
	],
	["codex device not enabled", "openai-codex", ["device_code"], [[404, "nope"]]],
	["codex device request failed", "openai-codex", ["device_code"], [[500, "boom"]]],
	["codex device request failed empty", "openai-codex", ["device_code"], [[502, ""]]],
	["codex device invalid interval", "openai-codex", ["device_code"], [[200, '{"device_auth_id":"d","user_code":"u","interval":-1}']]],
	["codex device interval text", "openai-codex", ["device_code"], [[200, '{"device_auth_id":"d","user_code":"u","interval":"soon"}']]],
	["codex device missing code", "openai-codex", ["device_code"], [[200, '{"device_auth_id":"","user_code":"u","interval":5}']]],
	[
		"codex device poll failure",
		"openai-codex",
		["device_code"],
		[
			[200, '{"device_auth_id":"d","user_code":"u","interval":0}'],
			[500, '{"error":{"code":"server_error"}}'],
		],
	],
	[
		"codex device poll invalid success",
		"openai-codex",
		["device_code"],
		[
			[200, '{"device_auth_id":"d","user_code":"u","interval":1}'],
			[200, '{"authorization_code":"c"}'],
		],
	],
	[
		"codex device timeout",
		"openai-codex",
		["device_code"],
		[[200, '{"device_auth_id":"d","user_code":"u","interval":120}'], ...Array.from({ length: 8 }, () => [403, ""])],
	],
	[
		"codex device slow_down timeout",
		"openai-codex",
		["device_code"],
		[[200, '{"device_auth_id":"d","user_code":"u","interval":300}'], [400, '{"error":"slow_down"}'], [403, ""], [403, ""]],
	],
	[
		"codex device cancelled",
		"openai-codex",
		["device_code"],
		[[200, '{"device_auth_id":"d","user_code":"u","interval":5}'], [403, ""]],
		{ abortAt: 2 },
	],

	// GitHub Copilot device-code login.
	[
		"copilot login",
		"github-copilot",
		[""],
		[
			[200, deviceCode()],
			[200, '{"error":"authorization_pending"}'],
			[200, '{"error":"slow_down","interval":10}'],
			[200, '{"error":"slow_down"}'],
			[200, '{"access_token":"gho_1","token_type":"bearer"}'],
			[200, copilotToken(COPILOT_TOKEN)],
			[200, policyModels],
			[200, ""],
			[500, ""],
			[0, ""],
		],
	],
	[
		"copilot login enterprise",
		"github-copilot",
		["  https://Company.GHE.com/some/path "],
		[
			[200, deviceCode({ verification_uri: "HTTPS://Company.GHE.com/login/device", interval: undefined })],
			[200, '{"access_token":"gho_2"}'],
			[200, copilotToken(COPILOT_ENT_TOKEN)],
			[200, fallbackModels],
		],
	],
	[
		"copilot login policy fallback",
		"github-copilot",
		[""],
		[
			[200, deviceCode({ interval: 1 })],
			[200, '{"access_token":"gho_3"}'],
			[200, copilotToken(COPILOT_TOKEN)],
			[200, fallbackModels],
			[200, ""],
		],
	],
	[
		"copilot login models rate limited",
		"github-copilot",
		[""],
		[
			[200, deviceCode({ interval: 1 })],
			[200, '{"access_token":"gho_4"}'],
			[200, copilotToken(COPILOT_TOKEN)],
			[429, "slow", { "retry-after": "1.5" }],
			[429, "slow", { "retry-after": "Thu, 01 Jan 2026 00:00:01 GMT" }],
			[200, policyModels],
			[429, "", { "retry-after": "0" }],
			[429, "", { "retry-after": "0" }],
			[429, "busy"],
			[200, ""],
		],
	],
	[
		"copilot login models rate limit exhausted",
		"github-copilot",
		[""],
		[
			[200, deviceCode({ interval: 1 })],
			[200, '{"access_token":"gho_5"}'],
			[200, copilotToken(COPILOT_TOKEN)],
			[429, "a"],
			[429, "b"],
			[429, "c"],
		],
	],
	[
		"copilot login retry-after beyond budget",
		"github-copilot",
		[""],
		[
			[200, deviceCode({ interval: 1 })],
			[200, '{"access_token":"gho_6"}'],
			[200, copilotToken(COPILOT_TOKEN)],
			[429, "wait", { "retry-after": "10" }],
		],
	],
	[
		"copilot login retry-after unparseable",
		"github-copilot",
		[""],
		[
			[200, deviceCode({ interval: 1 })],
			[200, '{"access_token":"gho_7"}'],
			[200, copilotToken(COPILOT_TOKEN)],
			[429, "wait", { "retry-after": "soon" }],
		],
	],
	["copilot invalid enterprise", "github-copilot", ["http://[bad"], []],
	["copilot device code http error", "github-copilot", [""], [[500, "down"]]],
	["copilot device code invalid", "github-copilot", [""], [[200, "[]"]]],
	["copilot device code null interval", "github-copilot", [""], [[200, deviceCode({ interval: null })]]],
	["copilot device code untrusted uri", "github-copilot", [""], [[200, deviceCode({ verification_uri: "file:///etc/passwd" })]]],
	["copilot device code unparseable uri", "github-copilot", [""], [[200, deviceCode({ verification_uri: "not a url" })]]],
	[
		"copilot device denied",
		"github-copilot",
		[""],
		[[200, deviceCode({ interval: 1 })], [200, '{"error":"access_denied","error_description":"The user denied"}']],
	],
	["copilot device invalid token response", "github-copilot", [""], [[200, deviceCode({ interval: 1 })], [200, '{"x":1}']]],
	["copilot device poll http error", "github-copilot", [""], [[200, deviceCode({ interval: 1 })], [502, "gateway"]]],
	[
		"copilot device timeout",
		"github-copilot",
		[""],
		[[200, deviceCode({ interval: 5, expires_in: 12 })], [200, '{"error":"authorization_pending"}'], [200, '{"error":"authorization_pending"}'], [200, '{"error":"authorization_pending"}']],
	],
	["copilot prompt rejects", "github-copilot", [reject("Login cancelled")], []],
	[
		"copilot cancelled while polling",
		"github-copilot",
		[""],
		[[200, deviceCode({ interval: 1 })], [200, '{"error":"authorization_pending"}']],
		{ abortAt: 2 },
	],
];

const prettyPrompt = (prompt) => {
	const out = { type: prompt.type, message: prompt.message };
	if (prompt.placeholder !== undefined) out.placeholder = prompt.placeholder;
	if (prompt.options !== undefined) out.options = prompt.options;
	out.hasSignal = prompt.signal !== undefined;
	return out;
};

const out = [];
for (const [name, provider, answers, responses, options = {}] of cases) {
	queue = responses.map(([status, body, headers]) => ({ status, body, headers }));
	requests = [];
	counter = 0;
	now = START;
	abortAt = options.abortAt ?? 0;
	controller = new AbortController();
	const events = [];
	const prompts = [];
	const hits = [];
	const pending = [...answers];
	let state = "";
	let redirect = "";
	let hitsDone = Promise.resolve();
	const fill = (text) => text.replaceAll("{state}", state).replaceAll("{redirect}", redirect);
	const interaction = {
		signal: controller.signal,
		notify(event) {
			events.push(JSON.parse(JSON.stringify(event)));
			if (event.type !== "auth_url") return;
			const url = new URL(event.url);
			state = url.searchParams.get("state") ?? "";
			redirect = url.searchParams.get("redirect_uri") ?? "";
			if (!options.hits) return;
			hitsDone = (async () => {
				for (const path of options.hits) {
					const response = await realFetch(`http://127.0.0.1:${ports[provider]}${fill(path)}`).catch((e) => e);
					if (response instanceof Error) {
						hits.push({ path, error: "connection refused" });
						continue;
					}
					hits.push({
						path,
						status: response.status,
						contentType: response.headers.get("content-type"),
						body: await response.text(),
					});
				}
			})();
		},
		prompt(prompt) {
			prompts.push(prettyPrompt(prompt));
			const answer = pending.shift();
			if (answer === undefined) return Promise.reject(new Error("unexpected prompt"));
			if (typeof answer === "string") return Promise.resolve(fill(answer));
			if (answer.reject) return Promise.reject(new Error(answer.reject));
			return new Promise((_resolve, rejectPrompt) => {
				prompt.signal?.addEventListener("abort", () => rejectPrompt(new Error("Login cancelled")), { once: true });
			});
		},
	};
	let result = null;
	let error = null;
	try {
		result = await auths[provider].login(interaction);
	} catch (e) {
		// The node stack TS appends to Anthropic error details holds machine paths; tests/oauth_login.rs compares only
		// the text before `; stack=`, so the frames are not recorded.
		error = e.message.replace(/; stack=[\s\S]*$/, "; stack=<omitted>");
	}
	await hitsDone;
	out.push({
		name,
		provider,
		answers,
		responses,
		abortAt: options.abortAt ?? null,
		hits: options.hits ?? null,
		prompts,
		events,
		requests,
		callbackResponses: hits,
		result: result && JSON.parse(JSON.stringify(result)),
		error,
	});
}

const pages = {
	success: oauthSuccessHtml("Done <now> & \"then\" 'go'"),
	error: oauthErrorHtml("Failed.", "Error: <x>"),
	errorNoDetails: oauthErrorHtml("Failed."),
};
writeFileSync(join(here, "oauth-login.json"), `${JSON.stringify({ cases: out, pages }, null, "\t")}\n`);
process.exit(0);
