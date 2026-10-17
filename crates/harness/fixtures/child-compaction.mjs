// Records the summary requests a child lane's compaction makes (packages/agent/src/harness/compaction/compaction.ts
// `compactWithRequest`, the compaction of every lane but the root's in the TypeScript worker): for scripted
// preparations (a first summary, an update of a previous summary, a split turn with its prefix summary, a split turn
// with nothing before it, custom instructions, file lists from earlier compactions), every request (system prompt,
// user text, maxTokens, reasoning) and the summary it makes of scripted responses.
// crates/harness/tests/child_compaction.rs replays every case. Usage: node crates/harness/fixtures/child-compaction.mjs [out]
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, "../../..");
const mod = (p) => import(pathToFileURL(join(repo, "packages/agent/src/harness", p)).href);
const { compactWithRequest } = await mod("compaction/compaction.ts");
const { BACKGROUND_CONTEXT } = await mod("context.ts");

const NOW = 1_760_000_000_000;
Date.now = () => NOW;

const usage = { input: 1, output: 2, cacheRead: 0, cacheWrite: 0, totalTokens: 3, cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 } };
const text = (t) => ({ type: "text", text: t });
const user = (t, at = 1) => ({ role: "user", content: [text(t)], timestamp: at });
const call = (id, code) => ({ type: "toolCall", id, name: "rlm", arguments: { code } });
const assistant = (content, at = 2) => ({
	role: "assistant", content, api: "anthropic-messages", provider: "anthropic", model: "m1", usage, stopReason: "stop", timestamp: at,
});
const result = (id, t, at = 3) => ({ role: "toolResult", toolCallId: id, toolName: "rlm", content: [text(t)], isError: false, timestamp: at });

const model = { id: "m1", name: "m1", api: "anthropic-messages", provider: "anthropic", baseUrl: "http://127.0.0.1:1", reasoning: false, input: ["text"], cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 }, contextWindow: 200000, maxTokens: 4096 };
const reasoningModel = { ...model, id: "m2", reasoning: true, maxTokens: 64000 };
const settings = { enabled: true, reserveTokens: 16384, keepRecentTokens: 20000 };
const ops = (read = [], written = [], edited = []) => ({ read: new Set(read), written: new Set(written), edited: new Set(edited) });

const history = [
	user("Find the bug in parser.py"),
	assistant([text("Reading it."), call("c1", "print(await read('parser.py'))")]),
	result("c1", "def parse(x):\n    return x[1:]"),
	assistant([text("The slice drops the first character.")]),
];
const prefix = [
	user("Now fix it and run the tests", 10),
	assistant([call("c2", "await edit('parser.py', 'x[1:]', 'x')")], 11),
	result("c2", "edited", 12),
];

const cases = [
	{ name: "first summary", prep: { messagesToSummarize: history, turnPrefixMessages: [], isSplitTurn: false, previousSummary: undefined, fileOps: ops() } },
	{ name: "update of a previous summary with file lists", prep: { messagesToSummarize: history, turnPrefixMessages: [], isSplitTurn: false, previousSummary: "## Goal\nFix the parser.", fileOps: ops(["notes.md", "parser.py"], ["out.txt"], ["parser.py"]) } },
	{ name: "split turn", prep: { messagesToSummarize: history, turnPrefixMessages: prefix, isSplitTurn: true, previousSummary: "## Goal\nFix the parser.", fileOps: ops(["parser.py"]) } },
	{ name: "split turn with nothing before it", prep: { messagesToSummarize: [], turnPrefixMessages: prefix, isSplitTurn: true, previousSummary: "## Goal\nFix the parser.", fileOps: ops() } },
	{ name: "custom instructions on a reasoning model", model: reasoningModel, thinking: "high", instructions: "Keep the test names.", prep: { messagesToSummarize: history, turnPrefixMessages: [], isSplitTurn: false, previousSummary: undefined, fileOps: ops([], [], ["parser.py"]) } },
];

const out = [];
for (const c of cases) {
	const requests = [];
	let n = 0;
	const request = async (aiContext, options) => {
		n += 1;
		requests.push({
			systemPrompt: aiContext.systemPrompt,
			text: aiContext.messages[0].content[0].text,
			maxTokens: options.maxTokens,
			reasoning: options.reasoning ?? null,
		});
		return { role: "assistant", content: [text(`RESPONSE-${n}`)], api: "anthropic-messages", provider: "anthropic", model: "m1", usage, stopReason: "stop", timestamp: NOW };
	};
	const result = await compactWithRequest(
		{ ...c.prep, retainedTail: [], tokensBefore: 100, settings },
		{ model: c.model ?? model, customInstructions: c.instructions, thinkingLevel: c.thinking },
		request,
		BACKGROUND_CONTEXT,
	);
	if (!result.ok) throw new Error(`${c.name}: ${result.error.message}`);
	out.push({
		name: c.name,
		model: c.model ?? model,
		thinking: c.thinking ?? null,
		instructions: c.instructions ?? null,
		reserveTokens: settings.reserveTokens,
		history: c.prep.messagesToSummarize,
		prefix: c.prep.turnPrefixMessages,
		splitTurn: c.prep.isSplitTurn,
		previousSummary: c.prep.previousSummary ?? null,
		fileOps: { read: [...c.prep.fileOps.read], written: [...c.prep.fileOps.written], edited: [...c.prep.fileOps.edited] },
		requests,
		summary: result.value.summary,
		details: result.value.details,
	});
}
const target = process.argv[2] ?? join(here, "child-compaction.json");
writeFileSync(target, `${JSON.stringify(out, null, "\t")}\n`);
