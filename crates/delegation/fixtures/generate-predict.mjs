// Generates crates/delegation/fixtures/predict.json from createPredictAdapter (packages/coding-agent/src/ultron/
// predict-adapter.ts) with a scripted completeSimple: for each call (definition, input, repair, model reply) the
// system prompt and user message it sends, and the value it returns (or its error).
// Usage: node crates/delegation/fixtures/generate-predict.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const { createPredictAdapter } = await import(join(here, "../../../packages/coding-agent/src/ultron/predict-adapter.ts"));

const definition = {
	id: "classify-severity",
	version: "1",
	strategy: "predict",
	instructions: "Classify the severity of the reported defect.",
	inputSchema: { type: "object", required: ["report"], properties: { report: { type: "string" } } },
	outputSchema: { type: "object", required: ["severity"], additionalProperties: false, properties: { severity: { enum: ["low", "high"] } } },
	maxRepairs: 1,
	inputDescription: "A defect report",
	outputDescription: "{severity: low|high}",
};
const cases = [
	{ input: { report: "data loss on save" }, reply: '{"severity": "urgent"}' },
	{ input: { report: "data loss on save" }, repair: { attempt: 1, previous: { severity: "urgent" }, error: "classify-severity@1 output does not match its schema" }, reply: "```json\n{\"severity\": \"high\"}\n```" },
	{ input: { report: "ünïcode — \"quoted\"\n" }, reply: "  not json  " },
	{ input: { report: "x" }, repair: { attempt: 2, previous: "not json", error: "e" }, reply: "```\n[1, 2.5, null]\n```" },
	{ input: { report: "x" }, reply: "", stop: "error", message: "rate limited" },
	{ input: { report: "x" }, reply: "", stop: "aborted" },
];
const results = [];
for (const item of cases) {
	let sent;
	const adapter = createPredictAdapter({
		models: {
			completeSimple: async (_model, request) => {
				sent = { system: request.systemPrompt, user: request.messages[0].content };
				return { role: "assistant", stopReason: item.stop ?? "stop", errorMessage: item.message, content: [{ type: "text", text: item.reply }] };
			},
		},
		model: () => ({ id: "m" }),
	});
	try {
		const value = await adapter({ definition, input: item.input, repair: item.repair, signal: new AbortController().signal, context: {} });
		results.push({ ...item, sent, value });
	} catch (error) {
		results.push({ ...item, sent, error: error.message });
	}
}
writeFileSync(join(here, "predict.json"), `${JSON.stringify({ definition, results }, null, 1)}\n`);
