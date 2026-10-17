// Generates crates/ai/fixtures/images-timeout.json from this worktree's TypeScript packages/ai images API: the
// OpenRouter images request with `timeoutMs` (api/openrouter-images.ts passes it to the OpenAI SDK as the request
// `timeout`). A stubbed fetch that never answers until its signal aborts stands for a server that hangs; another
// answers at once. Recorded per case: how many requests went out and the AssistantImages result.
// Usage: node crates/ai/fixtures/generate-images-timeout.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
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
const { getImageModel } = await src("image-models.ts");
const flux = getImageModel("openrouter", "black-forest-labs/flux.2-pro");

/** `script`: per request, "hang" (until aborted) or "ok". */
function stubFetch(script, count) {
	return (_url, init) => {
		const step = script[count.n++] ?? "hang";
		if (step === "ok") {
			return Promise.resolve(
				new Response(JSON.stringify({ id: "img-1", choices: [{ message: { content: "done" } }] }), {
					status: 200,
					headers: { "content-type": "application/json" },
				}),
			);
		}
		return new Promise((_resolve, reject) => {
			init.signal?.addEventListener("abort", () => reject(new DOMException("This operation was aborted", "AbortError")));
		});
	};
}

const cases = [
	{ name: "a hanging request times out", options: { apiKey: "k", timeoutMs: 30 }, script: ["hang"] },
	{ name: "a timed out request is retried", options: { apiKey: "k", timeoutMs: 30, maxRetries: 1 }, script: ["hang", "ok"] },
	{ name: "every attempt times out", options: { apiKey: "k", timeoutMs: 30, maxRetries: 1 }, script: ["hang", "hang"] },
	{ name: "a quick answer is within the timeout", options: { apiKey: "k", timeoutMs: 5000 }, script: ["ok"] },
];

const out = [];
for (const c of cases) {
	const count = { n: 0 };
	const result = await generateImages(flux, { input: [{ type: "text", text: "Generate a dog" }] }, {
		...c.options,
		fetch: stubFetch(c.script, count),
	});
	out.push({ name: c.name, options: c.options, script: c.script, requests: count.n, result: JSON.parse(JSON.stringify(result)) });
}
writeFileSync(join(here, "images-timeout.json"), `${JSON.stringify({ model: flux, cases: out }, null, "\t")}\n`);
assertOffline();
