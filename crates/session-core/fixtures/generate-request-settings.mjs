// Generates crates/session-core/fixtures/request-settings.json: what the `thinkingBudgets` setting does to a model
// request. Pi's session hands `settingsManager.getThinkingBudgets()` to its Agent (sdk.ts), which passes it to every
// `streamSimple` call; recorded here is the Anthropic Messages payload (`onPayload`) for a budget-based thinking
// model at each reasoning level, without and with budgets. The request goes to 127.0.0.1:9 (nothing listens; the
// offline guard blocks anything else), so no provider is contacted.
// crates/session-core/tests/request_settings.rs replays it through the session's Provider.
// Usage: node crates/session-core/fixtures/generate-request-settings.mjs
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "../../..");
const { streamSimple } = await import(pathToFileURL(join(root, "packages/ai/src/api/anthropic-messages.ts")).href);

const model = {
	id: "claude-sonnet-4-5",
	name: "Claude Sonnet 4.5",
	api: "anthropic-messages",
	provider: "anthropic",
	baseUrl: "http://127.0.0.1:9",
	reasoning: true,
	input: ["text"],
	cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 },
	contextWindow: 200000,
	maxTokens: 64000,
};
const context = { messages: [{ role: "user", content: "hi", timestamp: 1 }] };
const BUDGETS = [null, { minimal: 512, low: 2000, medium: 6000, high: 20000 }, { low: 3000 }];
const cases = [];
for (const thinkingBudgets of BUDGETS) {
	for (const reasoning of ["minimal", "low", "medium", "high"]) {
		let payload;
		const stream = streamSimple(model, context, {
			apiKey: "sk-ant-api-fixture",
			reasoning,
			...(thinkingBudgets ? { thinkingBudgets } : {}),
			maxRetries: 0,
			onPayload: (params) => {
				payload = params;
				return undefined;
			},
		});
		for await (const _event of stream) {
		}
		cases.push({ thinkingBudgets, reasoning, thinking: payload.thinking, max_tokens: payload.max_tokens });
	}
}
assertOffline();
writeFileSync(join(here, "request-settings.json"), `${JSON.stringify({ model, cases }, null, 1)}\n`);
console.log(`wrote ${cases.length} cases`);
