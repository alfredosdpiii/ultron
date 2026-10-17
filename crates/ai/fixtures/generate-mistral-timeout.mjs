// Generates crates/ai/fixtures/mistral-timeout.json from this worktree's TypeScript packages/ai Mistral API: the
// whole request, its stream included, is bounded by `AbortSignal.timeout(timeoutMs ?? 60000)`
// (api/mistral-conversations.ts `requestMistralStream`). A stubbed fetch stands for the server: it never answers
// (rejecting with the signal's reason when it aborts, as fetch does), or answers and then stalls mid-stream, or
// answers a whole stream in time. Recorded per case: the final message's stopReason, errorMessage and content.
// Usage: node crates/ai/fixtures/generate-mistral-timeout.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const src = (path) => import(pathToFileURL(join(root, "packages/ai/src", path)).href);
for (const name of Object.keys(process.env)) {
	if (/^(PI_|MISTRAL_)/.test(name)) delete process.env[name];
}
const NOW = 1767225600000;
Date.now = () => NOW;

const { stream } = await src("api/mistral-conversations.ts");

const model = {
	id: "mistral-test",
	name: "Test Model",
	api: "mistral-conversations",
	provider: "mistral",
	baseUrl: "https://mistral.invalid",
	reasoning: false,
	input: ["text"],
	cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 },
	contextWindow: 200000,
	maxTokens: 32000,
};
const context = { messages: [{ role: "user", content: "hi", timestamp: 2 }] };
const chunk = (delta, finish = null) =>
	`data: ${JSON.stringify({ id: "c1", object: "chat.completion.chunk", model: "mistral-test", choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;

/** The chunks a stream sends; `stall`: it then never ends (until the request is aborted). */
function respond(chunks, stall, init) {
	const encoder = new TextEncoder();
	const body = new ReadableStream({
		start(controller) {
			for (const text of chunks) controller.enqueue(encoder.encode(text));
			if (!stall) controller.close();
			else init.signal?.addEventListener("abort", () => controller.error(init.signal.reason));
		},
	});
	return new Response(body, { status: 200, headers: { "content-type": "text/event-stream" } });
}

const cases = [
	{ name: "never-answers", timeoutMs: 40, script: "hang" },
	{ name: "stalls-mid-stream", timeoutMs: 40, script: "stall", chunks: [chunk({ role: "assistant", content: "par" })] },
	{
		name: "answers-in-time",
		timeoutMs: 5000,
		script: "ok",
		chunks: [chunk({ role: "assistant", content: "done" }), chunk({}, "stop"), "data: [DONE]\n\n"],
	},
];

const results = [];
for (const c of cases) {
	const fetch = (_url, init) => {
		if (c.script === "hang")
			return new Promise((_resolve, reject) => init.signal?.addEventListener("abort", () => reject(init.signal.reason)));
		return Promise.resolve(respond(c.chunks, c.script === "stall", init));
	};
	// AbortSignal.timeout's timer does not keep the process alive.
	const alive = setInterval(() => {}, 1000);
	const message = await stream(model, context, { apiKey: "test-key", timeoutMs: c.timeoutMs, fetch }).result();
	clearInterval(alive);
	results.push({
		...c,
		result: { stopReason: message.stopReason, errorMessage: message.errorMessage ?? null, content: message.content },
	});
}
assertOffline();
const out = join(here, "mistral-timeout.json");
writeFileSync(out, `${JSON.stringify({ generator: "crates/ai/fixtures/generate-mistral-timeout.mjs", model, context, cases: results }, null, "\t")}\n`);
console.log(`wrote ${results.length} cases to ${out}`);
