// Generates crates/agent/fixtures/loop.json: runs the TypeScript Agent (packages/agent/src/agent.ts, which drives
// agent-loop.ts) against the TypeScript faux provider (packages/ai/src/providers/faux.ts) on scripted scenarios and
// records, per scenario, every provider call (the transcript it received and the stream events it produced) and every
// AgentEvent the agent emitted, plus the final agent state. Rust tests replay the provider events through a scripted
// stream function and require the same agent events and state.
// The faux provider mutates one partial message in place; a timer between chunks (tokensPerSecond) keeps it from
// running ahead of the consumer, so each recorded snapshot is the state at its event.
// Usage: node crates/agent/fixtures/generate.mjs   (from the repository root; deterministic)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createAssistantMessageEventStream, createFauxCore, fauxAssistantMessage, fauxText, fauxToolCall } from "../../../packages/ai/src/index.ts";
import { Agent } from "../../../packages/agent/src/agent.ts";

const here = dirname(fileURLToPath(import.meta.url));
const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));
let clock = 1_700_000_000_000;
Date.now = () => clock++;

const schema = { type: "object", properties: { text: { type: "string" } }, required: ["text"] };
const echo = {
	name: "echo",
	label: "Echo",
	description: "Echo the text back",
	parameters: schema,
	execute: async (_id, args, _signal, onUpdate) => {
		onUpdate?.({ content: [{ type: "text", text: "working" }], details: { step: 1 } });
		return { content: [{ type: "text", text: `echo: ${args.text}` }], details: { text: args.text } };
	},
};
const fail = {
	name: "fail",
	label: "Fail",
	description: "Always throws",
	parameters: schema,
	execute: async () => {
		throw new Error("tool failed");
	},
};
const stop = {
	name: "stop",
	label: "Stop",
	description: "Ends the run",
	parameters: schema,
	execute: async () => ({ content: [{ type: "text", text: "stopping" }], details: {}, terminate: true }),
};
const TOOLS = { echo, fail, stop };
const user = (text) => ({ role: "user", content: [{ type: "text", text }], timestamp: 1 });
const msg = (content, options = {}) => fauxAssistantMessage(content, { timestamp: 2, ...options });

// Each scenario: tools, optional system prompt, queue modes, scripted responses, the prompt, and listener actions
// keyed by an event predicate (fired once each).
const SCENARIOS = [
	{ name: "text", responses: [msg("Hello there, how are you today?")], prompt: "hi" },
	{
		name: "tool_round_trip",
		tools: ["echo"],
		systemPrompt: "You are a test.",
		responses: [msg([fauxText("Calling."), fauxToolCall("echo", { text: "ping" }, { id: "call_1" })], { stopReason: "toolUse" }), msg("All done.")],
		prompt: "echo ping",
	},
	{
		name: "two_tools_parallel",
		tools: ["echo", "fail"],
		responses: [
			msg([fauxToolCall("echo", { text: "a" }, { id: "call_a" }), fauxToolCall("fail", { text: "b" }, { id: "call_b" }), fauxToolCall("missing", { text: "c" }, { id: "call_c" })], { stopReason: "toolUse" }),
			msg("Recovered."),
		],
		prompt: "run two",
	},
	{
		name: "two_tools_sequential",
		tools: ["echo", "fail"],
		toolExecution: "sequential",
		responses: [
			msg([fauxToolCall("fail", { text: "x" }, { id: "s1" }), fauxToolCall("echo", { text: "y" }, { id: "s2" })], { stopReason: "toolUse" }),
			msg("ok"),
		],
		prompt: "sequential",
	},
	{
		name: "abort_mid_stream",
		responses: [msg("This is a long answer that will be cut off by an abort in the middle of streaming.")],
		prompt: "talk",
		actions: [{ on: (e) => e.type === "message_update" && e.assistantMessageEvent.type === "text_delta", do: "abort" }],
	},
	{
		name: "steering",
		tools: ["echo"],
		responses: [msg([fauxToolCall("echo", { text: "one" }, { id: "t1" })], { stopReason: "toolUse" }), msg("Steered answer.")],
		prompt: "start",
		actions: [{ on: (e) => e.type === "tool_execution_start", do: "steer", text: "change of plan" }],
	},
	{
		name: "follow_up",
		responses: [msg("First."), msg("Second."), msg("Third.")],
		followUpMode: "all",
		prompt: "go",
		actions: [{ on: (e) => e.type === "message_end" && e.message.role === "assistant", do: "followUps", texts: ["more", "and more"] }],
	},
	{
		name: "steering_one_at_a_time",
		tools: ["echo"],
		responses: [msg([fauxToolCall("echo", { text: "q" }, { id: "q1" })], { stopReason: "toolUse" }), msg("A."), msg("B.")],
		prompt: "queue",
		actions: [{ on: (e) => e.type === "tool_execution_end", do: "steers", texts: ["s1", "s2"] }],
	},
	{ name: "provider_error", responses: [msg("", { stopReason: "error", errorMessage: "rate limited" })], prompt: "fail please" },
	{ name: "no_response_queued", responses: [], prompt: "nothing" },
	{
		name: "length_stop",
		tools: ["echo"],
		responses: [msg([fauxToolCall("echo", { text: "trunc" }, { id: "L1" })], { stopReason: "length" }), msg("Retried.")],
		prompt: "long",
	},
	{
		name: "terminate",
		tools: ["stop"],
		responses: [msg([fauxToolCall("stop", { text: "now" }, { id: "T1" })], { stopReason: "toolUse" })],
		prompt: "stop it",
	},
	{
		name: "terminate_mixed",
		tools: ["stop", "echo"],
		responses: [msg([fauxToolCall("stop", { text: "now" }, { id: "M1" }), fauxToolCall("echo", { text: "go" }, { id: "M2" })], { stopReason: "toolUse" }), msg("Continued.")],
		prompt: "mixed",
	},
	{
		name: "abort_with_queued_follow_up",
		responses: [msg("An answer long enough to be aborted while it streams."), msg("Never sent.")],
		prompt: "talk",
		actions: [{ on: (e) => e.type === "message_update" && e.assistantMessageEvent.type === "text_delta", do: "abortFollowUp", texts: ["later"] }],
	},
	{
		name: "abort_during_sequential_tools",
		tools: ["echo"],
		toolExecution: "sequential",
		responses: [msg([fauxToolCall("echo", { text: "1" }, { id: "A1" }), fauxToolCall("echo", { text: "2" }, { id: "A2" })], { stopReason: "toolUse" }), msg("Not reached.")],
		prompt: "abort tools",
		actions: [{ on: (e) => e.type === "tool_execution_end", do: "abort" }],
	},
	{
		name: "tools_added_later",
		systemPrompt: "Base prompt.",
		addTools: ["echo"],
		responses: [msg("Seen.")],
		prompt: "tools?",
	},
];

const scenarios = [];
for (const scenario of SCENARIOS) {
	const core = createFauxCore({ api: "faux", provider: "faux", tokenSize: { min: 3, max: 3 }, tokensPerSecond: 1e9 });
	core.setResponses(scenario.responses);
	const calls = [];
	const streamFn = (model, context, options) => {
		const call = { context: clone(context), events: [] };
		calls.push(call);
		const inner = core.streamSimple(model, context, options);
		const outer = createAssistantMessageEventStream();
		(async () => {
			for await (const event of inner) {
				call.events.push(clone(event));
				outer.push(event);
			}
			outer.end(await inner.result());
		})();
		return outer;
	};
	const agent = new Agent({
		initialState: {
			model: core.getModel(),
			systemPrompt: scenario.systemPrompt,
			tools: (scenario.tools ?? []).map((name) => TOOLS[name]),
		},
		streamFn,
		toolExecution: scenario.toolExecution,
		followUpMode: scenario.followUpMode,
		steeringMode: scenario.steeringMode,
	});
	if (scenario.addTools) agent.state.tools = scenario.addTools.map((name) => TOOLS[name]);
	const events = [];
	const pending = [...(scenario.actions ?? [])];
	agent.subscribe((event) => {
		events.push(clone(event));
		const index = pending.findIndex((action) => action.on(event));
		if (index < 0) return;
		const [action] = pending.splice(index, 1);
		if (action.do === "abort") agent.abort();
		if (action.do === "abortFollowUp") {
			for (const text of action.texts) agent.followUp(user(text));
			agent.abort();
		}
		if (action.do === "steer") agent.steer(user(action.text));
		if (action.do === "steers") for (const text of action.texts) agent.steer(user(text));
		if (action.do === "followUps") for (const text of action.texts) agent.followUp(user(text));
	});
	await agent.prompt(scenario.prompt);
	scenarios.push({
		name: scenario.name,
		tools: scenario.tools ?? [],
		addTools: scenario.addTools ?? [],
		systemPrompt: scenario.systemPrompt ?? null,
		toolExecution: scenario.toolExecution ?? "parallel",
		followUpMode: scenario.followUpMode ?? "one-at-a-time",
		steeringMode: scenario.steeringMode ?? "one-at-a-time",
		actions: (scenario.actions ?? []).map((a) => ({ on: a.on.toString(), do: a.do, texts: a.texts ?? (a.text ? [a.text] : []) })),
		prompt: scenario.prompt,
		model: clone(core.getModel()),
		calls,
		events,
		state: {
			messages: clone(agent.state.messages),
			isStreaming: agent.state.isStreaming,
			pendingToolCalls: [...agent.state.pendingToolCalls],
			errorMessage: agent.state.errorMessage ?? null,
			queued: agent.hasQueuedMessages(),
		},
	});
}
writeFileSync(join(here, "loop.json"), `${JSON.stringify({ scenarios }, null, 1)}\n`);
