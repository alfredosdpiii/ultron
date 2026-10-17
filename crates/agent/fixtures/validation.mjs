// Generates crates/agent/fixtures/validation.json from the TypeScript reference:
// - `cases`: `validateToolArguments` (packages/ai/src/utils/validation.ts) over TypeBox tool schemas (the agent
//   loop's tools are TypeBox schemas in TS) and a matrix of arguments: the validated (coerced) arguments or the
//   thrown error text.
// - `scenarios`: `runAgentLoop` (packages/agent/src/agent-loop.ts) over a scripted stream function, with tools that
//   have bad arguments, a `prepareArguments` tool, and `beforeToolCall` / `afterToolCall` hooks; every AgentEvent and
//   every hook invocation is recorded. Rust tests replay the same responses and compare.
// Usage: node crates/agent/fixtures/validation.mjs   (from the repository root, behind the offline guard;
// deterministic: run twice, same sha256)
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { Type } from "typebox";
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";
import {
	createAssistantMessageEventStream,
	createFauxCore,
	fauxAssistantMessage,
	fauxText,
	fauxToolCall,
	validateToolArguments,
} from "../../../packages/ai/src/index.ts";
import { runAgentLoop } from "../../../packages/agent/src/agent-loop.ts";

const here = dirname(fileURLToPath(import.meta.url));
const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));
let clock = 1_700_000_000_000;
Date.now = () => clock++;

const SCHEMAS = {
	rlm: Type.Object({
		code: Type.String({ description: "Python code for persistent RLM computation, typed agents, and data processing" }),
	}),
	lane: Type.Object({ code: Type.String(), lane: Type.Optional(Type.String()) }),
	rich: Type.Object({
		count: Type.Integer(),
		ratio: Type.Number(),
		flag: Type.Boolean(),
		name: Type.Optional(Type.String()),
		tags: Type.Optional(Type.Array(Type.String())),
		ids: Type.Optional(Type.Array(Type.Integer())),
		mode: Type.Optional(Type.Union([Type.Literal("fast"), Type.Literal("slow")])),
		limit: Type.Optional(Type.Union([Type.Number(), Type.Null()])),
		nested: Type.Optional(Type.Object({ depth: Type.Integer(), label: Type.Optional(Type.String()) })),
		on: Type.Optional(Type.Literal(true)),
		nothing: Type.Optional(Type.Null()),
		level: Type.Optional(Type.Literal(2)),
	}),
	extra: Type.Object(
		{ n: Type.Number(), flag: Type.Optional(Type.Boolean()) },
		{ additionalProperties: Type.Number() },
	),
	closed: Type.Object({ n: Type.Number() }, { additionalProperties: false }),
	bounded: Type.Object({
		text: Type.String({ minLength: 2, maxLength: 5 }),
		n: Type.Optional(Type.Number({ minimum: 0, maximum: 10 })),
	}),
};

const base = { count: 1, ratio: 0.5, flag: true };
const CASES = [
	["rlm", { code: "print(1)" }],
	["rlm", {}],
	["rlm", { code: 5 }],
	["rlm", { code: 1.5 }],
	["rlm", { code: true }],
	["rlm", { code: null }],
	["rlm", { code: ["x"] }],
	["rlm", { code: { a: 1 } }],
	["rlm", { code: "x", extra: 1 }],
	["rlm", { cell: "x" }],
	["lane", { code: "x", lane: "work" }],
	["lane", { code: "x", lane: null }],
	["lane", { code: "x", lane: 7 }],
	["lane", { code: "x", lane: false }],
	["lane", { lane: "work" }],
	["rich", base],
	["rich", { count: "3", ratio: "2.5", flag: "true" }],
	["rich", { count: "3.5", ratio: "", flag: "1" }],
	["rich", { count: 2.7, ratio: "1e3", flag: "FALSE" }],
	["rich", { count: true, ratio: false, flag: 0 }],
	["rich", { count: null, ratio: null, flag: null }],
	["rich", { count: "abc", ratio: "x", flag: "yes" }],
	["rich", { count: " 4 ", ratio: "0x10", flag: 2 }],
	["rich", { count: "12n", ratio: "-0", flag: "0" }],
	["rich", { ...base, name: 42, tags: "solo", ids: ["1", "2", "x"] }],
	["rich", { ...base, name: null, tags: null, mode: null, limit: null, nested: null }],
	["rich", { ...base, mode: "fast", limit: "7" }],
	["rich", { ...base, mode: "medium", limit: "seven" }],
	["rich", { ...base, mode: 1, limit: true }],
	["rich", { ...base, nested: { depth: "2", label: 3 } }],
	["rich", { ...base, nested: { label: "x" } }],
	["rich", { ...base, nested: "deep" }],
	["rich", { ...base, on: "true", nothing: "null" }],
	["rich", { ...base, on: 1, nothing: "" }],
	["rich", { ...base, on: false, nothing: 0 }],
	["rich", { ...base, tags: [1, true, null] }],
	["rich", { ...base, level: "3" }],
	["rich", { ...base, level: "2" }],
	["rich", { ...base, level: "TRUE" }],
	["rich", { ...base, limit: "TRUE" }],
	["rich", {}],
	["rich", { ratio: [], flag: {} }],
	["extra", { n: "1", x: "2", flag: "true" }],
	["extra", { n: 1, x: "y" }],
	["extra", { n: 1, x: "TRUE" }],
	["extra", { n: "3", flag: "0", z: true }],
	["closed", { n: "4" }],
	["closed", { n: 4, other: 1 }],
	["bounded", { text: "abc" }],
	["bounded", { text: "a" }],
	["bounded", { text: "abcdef", n: 11 }],
	["bounded", { text: 12345, n: "-1" }],
	["bounded", { text: 123456 }],
];

const cases = CASES.map(([schema, args]) => {
	const tool = { name: schema, description: "", parameters: SCHEMAS[schema] };
	try {
		return { schema, args, ok: clone(validateToolArguments(tool, { type: "toolCall", id: "c", name: schema, arguments: clone(args) })) };
	} catch (error) {
		return { schema, args, error: error.message };
	}
});

// Loop scenarios.
const textSchema = Type.Object({ text: Type.String() });
const echo = {
	name: "echo",
	label: "Echo",
	description: "Echo the text back",
	parameters: textSchema,
	execute: async (_id, args, _signal, onUpdate) => {
		onUpdate?.({ content: [{ type: "text", text: "working" }], details: { step: 1 } });
		return { content: [{ type: "text", text: `echo: ${args.text}` }], details: { text: args.text } };
	},
};
const legacy = {
	name: "legacy",
	label: "Legacy",
	description: "Accepts the old argument name",
	parameters: textSchema,
	prepareArguments: (args) => {
		if (args && typeof args === "object" && args.boom) throw new Error("cannot prepare");
		if (args && typeof args === "object" && "old" in args) return { text: args.old };
		return args;
	},
	execute: async (_id, args) => ({ content: [{ type: "text", text: `legacy: ${JSON.stringify(args)}` }], details: {} }),
};
const TOOLS = { echo, legacy };
const user = (text) => ({ role: "user", content: [{ type: "text", text }], timestamp: 1 });
const msg = (content, options = {}) => fauxAssistantMessage(content, { timestamp: 2, ...options });
const toolUse = (...calls) => msg(calls.map(([name, args, id]) => fauxToolCall(name, args, { id })), { stopReason: "toolUse" });

// before: (args) => decision; after: (callId, args, result, isError) => replacement. Hooks are keyed by behaviour so the
// Rust test can implement the same ones.
const HOOKS = {
	policy: {
		before: ({ toolCall, args }) => {
			if (args.text === "block") return { block: true, reason: "blocked by policy" };
			if (args.text === "quiet") return { block: true };
			if (args.text === "halt") return { block: true, reason: "halted", terminate: true };
			if (args.text === "explode") throw new Error("hook exploded");
			if (args.text === "pass") return { block: false };
			return undefined;
		},
		after: ({ toolCall, args, result, isError }) => {
			if (args.text === "swap") return { content: [{ type: "text", text: "replaced" }], isError: true, details: { swapped: true } };
			if (args.text === "stop") return { terminate: true };
			if (args.text === "boom") throw new Error("after exploded");
			return undefined;
		},
	},
};

const SCENARIOS = [
	{
		name: "invalid_parallel",
		tools: ["echo"],
		responses: [toolUse(["echo", {}, "p1"], ["echo", { text: 5 }, "p2"], ["echo", { text: { a: 1 } }, "p3"], ["missing", {}, "p4"]), msg("done")],
	},
	{
		name: "invalid_sequential",
		tools: ["echo"],
		toolExecution: "sequential",
		responses: [toolUse(["echo", { text: [1] }, "s1"], ["echo", { text: "ok" }, "s2"]), msg("done")],
	},
	{
		name: "prepare_arguments",
		tools: ["legacy"],
		responses: [toolUse(["legacy", { old: "hi" }, "l1"], ["legacy", { boom: true }, "l2"], ["legacy", { text: 3 }, "l3"], ["legacy", { old: 4 }, "l4"]), msg("done")],
	},
	{
		name: "before_hooks",
		tools: ["echo"],
		hooks: "policy",
		responses: [
			toolUse(["echo", { text: "block" }, "b1"], ["echo", { text: "quiet" }, "b2"], ["echo", { text: "explode" }, "b3"], ["echo", { text: "pass" }, "b4"], ["echo", { text: 9 }, "b5"]),
			msg("done"),
		],
	},
	{
		name: "before_hooks_sequential",
		tools: ["echo"],
		hooks: "policy",
		toolExecution: "sequential",
		responses: [toolUse(["echo", { text: "block" }, "q1"], ["echo", { text: "fine" }, "q2"]), msg("done")],
	},
	{
		name: "before_terminate",
		tools: ["echo"],
		hooks: "policy",
		responses: [toolUse(["echo", { text: "halt" }, "h1"]), msg("never")],
	},
	{
		name: "after_hooks",
		tools: ["echo"],
		hooks: "policy",
		responses: [toolUse(["echo", { text: "swap" }, "a1"], ["echo", { text: "boom" }, "a2"], ["echo", { text: "plain" }, "a3"]), msg("done")],
	},
	{
		name: "after_terminate",
		tools: ["echo"],
		hooks: "policy",
		responses: [toolUse(["echo", { text: "stop" }, "t1"]), msg("never")],
	},
	{
		name: "abort_and_block_in_before",
		tools: ["echo"],
		hooks: "abortBlock",
		responses: [toolUse(["echo", { text: "x" }, "y1"]), msg("never")],
	},
	{
		name: "abort_in_before",
		tools: ["echo"],
		hooks: "abort",
		responses: [toolUse(["echo", { text: "x" }, "x1"], ["echo", { text: "y" }, "x2"]), msg("never")],
	},
];

const core = createFauxCore({ api: "faux", provider: "faux" });
const model = core.getModel();
const scenarios = [];
for (const scenario of SCENARIOS) {
	const responses = [...scenario.responses];
	const contexts = [];
	const streamFn = (_model, context) => {
		contexts.push(clone(context));
		const message = responses.shift() ?? msg("", { stopReason: "error", errorMessage: "no response" });
		const stream = createAssistantMessageEventStream();
		queueMicrotask(() => {
			stream.push({ type: "done", reason: message.stopReason, message });
			stream.end(message);
		});
		return stream;
	};
	const controller = new AbortController();
	const hookCalls = [];
	const config = {
		model,
		convertToLlm: (messages) => messages,
		toolExecution: scenario.toolExecution ?? "parallel",
	};
	if (scenario.hooks === "policy") {
		config.beforeToolCall = async ({ toolCall, args }) => {
			hookCalls.push({ hook: "before", id: toolCall.id, toolCallArgs: clone(toolCall.arguments), args: clone(args) });
			return HOOKS.policy.before({ toolCall, args });
		};
		config.afterToolCall = async ({ toolCall, args, result, isError }) => {
			hookCalls.push({ hook: "after", id: toolCall.id, args: clone(args), result: clone(result), isError });
			return HOOKS.policy.after({ toolCall, args, result, isError });
		};
	}
	if (scenario.hooks === "abortBlock") {
		config.beforeToolCall = async ({ toolCall, args }) => {
			hookCalls.push({ hook: "before", id: toolCall.id, toolCallArgs: clone(toolCall.arguments), args: clone(args) });
			controller.abort();
			return { block: true, reason: "blocked after the abort" };
		};
	}
	if (scenario.hooks === "abort") {
		config.beforeToolCall = async ({ toolCall, args }) => {
			hookCalls.push({ hook: "before", id: toolCall.id, toolCallArgs: clone(toolCall.arguments), args: clone(args) });
			controller.abort();
			return undefined;
		};
	}
	const events = [];
	const context = { systemPrompt: "", messages: [], tools: scenario.tools.map((name) => TOOLS[name]) };
	const messages = await runAgentLoop([user("go")], context, config, (event) => events.push(clone(event)), controller.signal, streamFn);
	scenarios.push({
		name: scenario.name,
		tools: scenario.tools,
		hooks: scenario.hooks ?? null,
		toolExecution: config.toolExecution,
		responses: clone(scenario.responses),
		calls: contexts.length,
		events,
		hookCalls,
		messages: clone(messages),
	});
}

assertOffline();
writeFileSync(join(here, "validation.json"), `${JSON.stringify({ model: clone(model), schemas: clone(SCHEMAS), toolSchema: clone(textSchema), cases, scenarios }, null, 1)}\n`);
