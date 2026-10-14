// Records the TypeScript `ultron --mode rpc` compacting on its own (the session worker's harness): a response whose
// usage crosses the context threshold (`contextWindow - reserveTokens`) compacts at the run's checkpoint, before the
// run ends or before its next request; a context-overflow error compacts and retries the request once; a second
// overflow fails the run; `set_auto_compaction false` turns the threshold off. (Not recorded: the run after a
// threshold compaction, which the TypeScript harness compacts again from the kept answer's stale pre-compaction usage;
// the Rust build estimates without it, as `estimateProjectedContextTokens` and AgentSession do.) Same recording as rpc_mode.mjs (its
// `run`, `canonical` and `scrub`), written to rpc_auto_compaction.json; tests/rpc_mode.rs replays it.
// Usage: node crates/cli/fixtures/rpc_auto_compaction.mjs
import { writeFileSync } from "node:fs";
import { canonical, numberIds, run, scrub } from "./rpc_mode.mjs";

function sse(events) {
	return events.map((e) => `event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`).join("");
}
const start = (id, input) => ({
	type: "message_start",
	message: { id, type: "message", role: "assistant", model: "claude-sonnet-4-5", content: [], stop_reason: null, usage: { input_tokens: input, output_tokens: 1 } },
});
/** A text answer that reports `input` prompt tokens (claude-sonnet-4-5's window is 1,000,000). */
function textReply(text, input = 10) {
	return sse([
		start("msg_1", input),
		{ type: "content_block_start", index: 0, content_block: { type: "text", text: "" } },
		{ type: "content_block_delta", index: 0, delta: { type: "text_delta", text } },
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { output_tokens: 5 } },
		{ type: "message_stop" },
	]);
}
function toolReply(id, code, input = 10) {
	return sse([
		start("msg_2", input),
		{ type: "content_block_start", index: 0, content_block: { type: "tool_use", id, name: "rlm", input: {} } },
		{ type: "content_block_delta", index: 0, delta: { type: "input_json_delta", partial_json: JSON.stringify({ code }) } },
		{ type: "content_block_stop", index: 0 },
		{ type: "message_delta", delta: { stop_reason: "tool_use" }, usage: { output_tokens: 7 } },
		{ type: "message_stop" },
	]);
}
/** Anthropic's answer to a request over the window (HTTP 400). */
const overflow = {
	status: 400,
	contentType: "application/json",
	body: JSON.stringify({ type: "error", error: { type: "invalid_request_error", message: "prompt is too long: 1213462 tokens > 1000000 maximum" } }),
};
const OVER = 990_000;

const MODEL = ["--model", "anthropic/claude-sonnet-4-5"];
const settled = ["response", "agent_settled"];
export const cases = [
	{
		name: "auto-compaction-threshold",
		args: MODEL,
		replies: [textReply("one"), textReply("two", OVER), textReply("## Goal\nThreshold summary.")],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "first" }, wait: settled },
			{ command: { id: "p2", type: "prompt", message: "second" }, wait: settled },
			{ command: { id: "m1", type: "get_messages" } },
			{ command: { id: "s1", type: "get_state" } },
		],
	},
	{
		name: "auto-compaction-mid-run",
		args: MODEL,
		replies: [toolReply("toolu_mid", "6 * 7", OVER), textReply("## Goal\nMid-run summary."), textReply("Forty-two.")],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "compute" }, wait: settled },
			{ command: { id: "m1", type: "get_messages" } },
		],
	},
	{
		name: "auto-compaction-overflow",
		args: MODEL,
		replies: [textReply("one"), overflow, textReply("## Goal\nOverflow summary."), textReply("retried")],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "first" }, wait: settled },
			{ command: { id: "p2", type: "prompt", message: "second" }, wait: settled },
			{ command: { id: "m1", type: "get_messages" } },
			{ command: { id: "s1", type: "get_state" } },
		],
	},
	{
		// The summary request fails (HTTP 400): the run fails with the compaction's error.
		name: "auto-compaction-summary-fails",
		args: MODEL,
		replies: [
			textReply("one"),
			textReply("two", OVER),
			{ status: 400, contentType: "application/json", body: JSON.stringify({ type: "error", error: { type: "invalid_request_error", message: "summary refused" } }) },
			textReply("three"),
		],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "first" }, wait: settled },
			{ command: { id: "p2", type: "prompt", message: "second" }, wait: settled },
			{ command: { id: "m1", type: "get_messages" } },
			{ command: { id: "p3", type: "prompt", message: "third" }, wait: settled },
		],
	},
	{
		// The summary request of a compaction before a request fails: the run fails with it.
		name: "auto-compaction-mid-run-summary-fails",
		args: MODEL,
		replies: [
			toolReply("toolu_mid", "6 * 7", OVER),
			{ status: 400, contentType: "application/json", body: JSON.stringify({ type: "error", error: { type: "invalid_request_error", message: "summary refused" } }) },
		],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "compute" }, wait: settled },
			{ command: { id: "m1", type: "get_messages" } },
			{ command: { id: "s1", type: "get_state" } },
		],
	},
	{
		// The summary request of an overflow compaction fails: the run fails with it, the overflow stays committed.
		name: "auto-compaction-overflow-summary-fails",
		args: MODEL,
		replies: [
			textReply("one"),
			overflow,
			{ status: 400, contentType: "application/json", body: JSON.stringify({ type: "error", error: { type: "invalid_request_error", message: "summary refused" } }) },
		],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "first" }, wait: settled },
			{ command: { id: "p2", type: "prompt", message: "second" }, wait: settled },
			{ command: { id: "m1", type: "get_messages" } },
		],
	},
	{
		name: "auto-compaction-overflow-twice",
		args: MODEL,
		replies: [textReply("one"), overflow, textReply("## Goal\nOverflow summary."), overflow],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "first" }, wait: settled },
			{ command: { id: "p2", type: "prompt", message: "second" }, wait: settled },
			{ command: { id: "m1", type: "get_messages" } },
		],
	},
	{
		// A 20,000-token window (models.json `modelOverrides`): the answer crosses the threshold (3,616), and the
		// `before_compaction` hook trims the long cell output the model answered after instead (ContextControl).
		// `maxTokens` stays under the room the window leaves, so the request's `max_tokens` is never clamped by the
		// prompt's length (the system prompt names the checkout's skills directory, whose length varies by checkout).
		name: "auto-compaction-trimmed",
		args: MODEL,
		provider: { modelOverrides: { "claude-sonnet-4-5": { contextWindow: 20_000, maxTokens: 4_096 } } },
		replies: [toolReply("toolu_long", "print('y' * 9000)"), textReply("done", 3_700), textReply("unused")],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "print a lot" }, wait: settled },
			{ command: { id: "m1", type: "get_messages" } },
		],
	},
	{
		// The same before a request in the middle of a run: the trim waits for the next boundary, the run goes on.
		name: "auto-compaction-trimmed-mid-run",
		args: MODEL,
		provider: { modelOverrides: { "claude-sonnet-4-5": { contextWindow: 20_000, maxTokens: 4_096 } } },
		replies: [toolReply("toolu_long", "print('y' * 9000)"), toolReply("toolu_next", "6 * 7", 3_700), textReply("done"), textReply("unused")],
		steps: [
			{ command: { id: "p1", type: "prompt", message: "print a lot" }, wait: settled },
			{ command: { id: "m1", type: "get_messages" } },
		],
	},
	{
		name: "auto-compaction-off",
		args: MODEL,
		replies: [textReply("one", OVER), textReply("two")],
		steps: [
			{ command: { id: "a1", type: "set_auto_compaction", enabled: false } },
			{ command: { id: "p1", type: "prompt", message: "first" }, wait: settled },
			{ command: { id: "s1", type: "get_state" } },
			{ command: { id: "p2", type: "prompt", message: "second" }, wait: settled },
			{ command: { id: "m1", type: "get_messages" } },
		],
	},
];

if (import.meta.url === `file://${process.argv[1]}`) {
	const out = [];
	for (const c of cases) {
		const r = await run(c);
		out.push({
			name: c.name,
			args: c.args,
			...(c.provider ? { provider: c.provider } : {}),
			replies: c.replies,
			steps: c.steps,
			code: r.code,
			stderr: r.stderr,
			...canonical(r.lines, r.tmp),
			// Ids numbered: a trimmed result names its entry (`ctx.get("<id>")`) in later requests.
			requests: numberIds(scrub(r.requests, r.tmp)),
			sessions: scrub(r.sessions, r.tmp),
		});
	}
	writeFileSync(new URL("./rpc_auto_compaction.json", import.meta.url), `${JSON.stringify(out, null, 1)}\n`);
}
