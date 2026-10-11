// Generates crates/ai/fixtures/images.json from this worktree's TypeScript packages/ai images API.
// Each case calls `generateImages` (images.ts dispatch) or `builtinImagesModels().generateImages` with a stubbed
// fetch serving canned responses, and records every request (URL, method, headers minus SDK telemetry, JSON body) and
// the AssistantImages result or thrown message.
// Usage: node crates/ai/fixtures/generate-images.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const src = (path) => import(pathToFileURL(join(root, "packages/ai/src", path)).href);
for (const name of Object.keys(process.env)) {
	if (/^(PI_|OPENAI_|OPENROUTER_)/.test(name)) delete process.env[name];
}
const NOW = 1767225600000;
Date.now = () => NOW;

const { generateImages } = await src("images.ts");
const { builtinImagesModels } = await src("providers/all.ts");
const { getImageModel } = await src("image-models.ts");

function requestHeaders(init) {
	const out = {};
	new Headers(init.headers).forEach((value, key) => {
		if (key.startsWith("x-stainless-")) return;
		out[key] = value;
	});
	return Object.fromEntries(Object.entries(out).sort(([a], [b]) => (a < b ? -1 : 1)));
}

function stubFetch(queue, requests) {
	return async (url, init) => {
		requests.push({
			url: String(url),
			method: init.method,
			headers: requestHeaders(init),
			body: init.body ? JSON.parse(init.body) : null,
		});
		const canned = queue.shift();
		if (!canned) throw new Error("unexpected request");
		return new Response(typeof canned.body === "string" ? canned.body : JSON.stringify(canned.body), {
			status: canned.status ?? 200,
			headers: { "content-type": "application/json", ...(canned.headers ?? {}) },
		});
	};
}

const flux = getImageModel("openrouter", "black-forest-labs/flux.2-pro");
const custom = {
	id: "google/gemini-3.1-flash-image-preview",
	name: "Gemini 3.1 Flash Image Preview",
	api: "openrouter-images",
	provider: "openrouter",
	baseUrl: "https://openrouter.ai/api/v1/",
	input: ["text", "image"],
	output: ["text", "image"],
	cost: { input: 0.015, output: 0.03, cacheRead: 0.5, cacheWrite: 2 },
	headers: { "HTTP-Referer": "https://example.com", "X-Title": "model" },
};
const ok = (message, extra = {}) => ({ body: { id: "img-1", choices: [{ message }], ...extra } });
const textPrompt = { input: [{ type: "text", text: "Generate a dog" }] };

const cases = [
	{
		name: "text plus images with usage",
		model: custom,
		context: textPrompt,
		options: { apiKey: "test" },
		responses: [
			ok(
				{ content: "Here is your image.", images: [{ image_url: "data:image/png;base64,ZmFrZS1wbmc=" }] },
				{ usage: { prompt_tokens: 12, completion_tokens: 34, prompt_tokens_details: { cached_tokens: 0 } } },
			),
		],
	},
	{
		name: "image only model with image input and mixed image urls",
		model: flux,
		context: {
			input: [
				{ type: "text", text: "Make it blue" },
				{ type: "image", data: "aGk=", mimeType: "image/jpeg" },
			],
		},
		options: { apiKey: "k" },
		responses: [
			ok({
				content: "",
				images: [
					{ image_url: { url: "data:image/webp;base64,AAAA" } },
					{ image_url: "https://example.com/x.png" },
					{ image_url: "data:image/png,notbase64" },
					{ image_url: {} },
					{},
					{ image_url: "data:image/gif;base64,R0lG" },
					{ image_url: "data:image/png;charset=x;base64,QUJD" },
					{ image_url: "data:image/png;base64," },
					{ image_url: "data:;base64,QUJD" },
					{ image_url: "data:image/png;base64,QU\nJD" },
					{ image_url: "data:image/x;base64,a;base64,b" },
				],
			}),
		],
	},
	{
		name: "cache read and write usage",
		model: custom,
		context: textPrompt,
		options: { apiKey: "k" },
		responses: [
			ok(
				{ content: null },
				{
					usage: {
						prompt_tokens: 1000,
						completion_tokens: 10,
						prompt_tokens_details: { cached_tokens: 300, cache_write_tokens: 100 },
					},
				},
			),
		],
	},
	{
		name: "cache write exceeding reported cache",
		model: custom,
		context: textPrompt,
		options: { apiKey: "k" },
		responses: [
			ok(
				{ content: "x" },
				{ usage: { prompt_tokens: 50, prompt_tokens_details: { cached_tokens: 10, cache_write_tokens: 100 } } },
			),
		],
	},
	{ name: "no choices", model: flux, context: textPrompt, options: { apiKey: "k" }, responses: [{ body: { id: "e", choices: [] } }] },
	{ name: "missing api key", model: flux, context: textPrompt, options: {}, responses: [] },
	{ name: "empty api key", model: flux, context: textPrompt, options: { apiKey: "" }, responses: [] },
	{
		name: "http error with error body",
		model: flux,
		context: textPrompt,
		options: { apiKey: "k" },
		responses: [{ status: 400, body: { error: { message: "bad prompt", code: 400 } } }],
	},
	{
		name: "http error with non json body",
		model: flux,
		context: textPrompt,
		options: { apiKey: "k" },
		responses: [{ status: 502, body: "upstream down" }],
	},
	{
		name: "http error body without message",
		model: flux,
		context: textPrompt,
		options: { apiKey: "k" },
		responses: [{ status: 403, body: { error: { code: "forbidden", metadata: { raw: "r" } } } }],
	},
	{
		name: "retry after 429",
		model: flux,
		context: textPrompt,
		options: { apiKey: "k", maxRetries: 2 },
		responses: [
			{ status: 429, body: { error: { message: "slow down" } }, headers: { "retry-after-ms": "1" } },
			ok({ content: "done" }),
		],
	},
	{
		name: "no retry without maxRetries",
		model: flux,
		context: textPrompt,
		options: { apiKey: "k" },
		responses: [{ status: 429, body: { error: { message: "slow down" } }, headers: { "retry-after-ms": "1" } }],
	},
	{
		name: "option headers override model headers and null suppresses",
		model: custom,
		context: textPrompt,
		options: { apiKey: "k", headers: { "X-Title": "opt", "HTTP-Referer": null, "X-Extra": "1" } },
		responses: [ok({ content: "x" })],
	},
	{
		name: "payload hook replaces params",
		model: flux,
		context: textPrompt,
		options: { apiKey: "k" },
		payloadPatch: { seed: 7 },
		responses: [ok({ content: "x" })],
	},
	{ name: "aborted before send", model: flux, context: textPrompt, options: { apiKey: "k" }, abort: true, responses: [] },
	{ name: "unregistered api", model: { ...flux, api: "nope-images" }, context: textPrompt, options: { apiKey: "k" }, responses: [] },
	// Through builtinImagesModels: auth resolved from the env and merged into the request.
	{ name: "collection env key", via: "collection", env: { OPENROUTER_API_KEY: "env-key" }, model: flux, context: textPrompt, options: {}, responses: [ok({ content: "x" })] },
	{ name: "collection explicit key wins", via: "collection", env: { OPENROUTER_API_KEY: "env-key" }, model: flux, context: textPrompt, options: { apiKey: "explicit" }, responses: [ok({ content: "x" })] },
	{ name: "collection unconfigured", via: "collection", env: {}, model: flux, context: textPrompt, options: {}, responses: [] },
	{ name: "collection unknown provider", via: "collection", env: {}, model: { ...flux, provider: "ghost" }, context: textPrompt, options: {}, responses: [] },
];

const out = [];
for (const c of cases) {
	const queue = c.responses.map((r) => ({ ...r }));
	const requests = [];
	const controller = new AbortController();
	if (c.abort) controller.abort();
	const options = {
		...c.options,
		fetch: stubFetch(queue, requests),
		signal: controller.signal,
		...(c.payloadPatch ? { onPayload: (p) => ({ ...p, ...c.payloadPatch }) } : {}),
	};
	let result = null;
	let thrown = null;
	try {
		if (c.via === "collection") {
			const models = builtinImagesModels({
				authContext: { env: async (n) => c.env[n], fileExists: async () => false },
			});
			result = await models.generateImages(c.model, c.context, options);
		} else {
			result = await generateImages(c.model, c.context, options);
		}
	} catch (e) {
		thrown = e.message;
	}
	out.push({
		name: c.name,
		via: c.via ?? "api",
		env: c.env ?? null,
		model: c.model,
		context: c.context,
		options: c.options,
		abort: c.abort ?? false,
		payloadPatch: c.payloadPatch ?? null,
		responses: c.responses,
		requests,
		result: result && JSON.parse(JSON.stringify(result)),
		thrown,
	});
}
writeFileSync(join(here, "images.json"), `${JSON.stringify(out, null, "\t")}\n`);
console.log(`wrote ${out.length} cases`);
