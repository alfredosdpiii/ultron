// Generates crates/delegation/fixtures/agents.json from NativeRlmHost (packages/coding-agent/src/ultron/rlm/
// native-host.ts) with its definition registry, a NativeUsageLedger and scripted work, all deterministic:
// - deterministic adapter: "det"/"doubler" double `n` (a negative `n` answers schema-invalid); "step" returns its
//   input, or fails with "step crashed" when `input.fail`; "draft" returns {rev: input.round ?? 1}; "review" returns
//   {approved: input.rev >= 2}; "check-pass" returns {passed: true};
// - predict adapter: "pred" doubles like "det"; "judge" answers {doubled:-4} first, then {doubled: 2n} on a repair
//   when n == 2, always {doubled:-4} when n == 3, and fails "provider refused" when n == 5;
// - lanes: an `rlm` definition's prompt carries "Input data:\n<json>\n": "loop" doubles `n` (negative `n`:
//   schema-invalid, n == 3: a fenced reply); a subagent (`rlm-child@1`) whose brief starts "finish:<status>" calls
//   rlm.finish from its lane first; a brief "empty" replies ""; otherwise "reply to <brief>"; background jobs reply
//   "done: <prompt>". Every lane prompt and its model (setModel) is recorded.
// Each scenario runs its operations one after another (every task ends before the next operation) on a fresh host
// with a fixed clock; it records each result or error, the detached-end events, the adapter calls, the lane prompts,
// and the final task journal, definition document and usage ledger (scenarios whose work runs in parallel have no
// ledger: its call order would depend on scheduling). Random ids are numbered by first appearance.
// Usage: node crates/delegation/fixtures/generate-agents.mjs
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = join(here, "../../../packages/coding-agent/src/ultron");
const { NativeRlmHost } = await import(join(src, "rlm/native-host.ts"));
const { NativeUsageLedger } = await import(join(src, "usage.ts"));

let usageIds = 0;
Object.defineProperty(globalThis.crypto, "randomUUID", { value: () => `id-${++usageIds}`, configurable: true });

const clone = (v) => (v === undefined ? null : JSON.parse(JSON.stringify(v)));
const memory = () => {
	let value;
	return { read: async () => clone(value) ?? undefined, write: async (d) => void (value = clone(d)), get: () => clone(value) };
};

const doubled = (n) => (n < 0 ? { doubled: "invalid" } : { doubled: n * 2 });

function deterministic(calls) {
	return async ({ definition, input }) => {
		calls.push({ strategy: "deterministic", definition: definition.id, input: clone(input), repair: null });
		if (definition.id === "det" || definition.id === "doubler") return doubled(input.n);
		if (definition.id === "step") {
			if (input && input.fail) throw new Error("step crashed");
			return input;
		}
		if (definition.id === "draft") return { rev: input?.round ?? 1 };
		if (definition.id === "review") return { approved: (input?.rev ?? 0) >= 2 };
		if (definition.id === "check-pass") return { passed: true };
		throw new Error(`no deterministic script for ${definition.id}`);
	};
}

function predict(calls) {
	return async ({ definition, input, repair }) => {
		calls.push({ strategy: "predict", definition: definition.id, input: clone(input), repair: clone(repair) });
		if (definition.id === "pred") return doubled(input.n);
		if (definition.id === "judge") {
			if (input.n === 5) throw new Error("provider refused");
			if (input.n === 2 && repair !== undefined) return { doubled: 4 };
			return { doubled: -4 };
		}
		throw new Error(`no predict script for ${definition.id}`);
	};
}

function harness(prompts, host) {
	const lanes = new Map();
	return {
		lane: async (name) => {
			let lane = lanes.get(name);
			if (!lane) {
				let turn = 0;
				let text = "";
				let model = null;
				lane = {
					getActiveTools: async () => [],
					setModel: async (value) => {
						model = `${value.provider}/${value.modelId}`;
					},
					abort: async () => ({ ok: true }),
					steer: async () => ({ ok: true, value: {} }),
					prompt: async (prompt) => {
						prompts.push({ lane: name, prompt, model });
						const input = /Input data:\n(.*)\n/.exec(prompt);
						if (name.startsWith("ultron.rlm-child.")) {
							const finish = /^finish:(\w+)/.exec(prompt);
							if (finish)
								try {
									await host().handle(
										"rlm.finish",
										{ status: finish[1], summary: `did ${finish[1]}`, evidence: ["cargo test: exit 0, 4 passed"], changed_files: [] },
										{},
										{ lane: name },
									);
								} catch (error) {
									prompts.push({ lane: name, finishError: error.message });
								}
							text = prompt === "empty" ? "" : `reply to ${prompt}`;
						} else if (name.startsWith("ultron.background-job.")) text = `done: ${prompt}`;
						else if (input) {
							const n = JSON.parse(input[1]).n;
							const value = JSON.stringify(doubled(n));
							text = n === 3 ? `\`\`\`json\n${value}\n\`\`\`` : value;
						} else text = "unscripted";
						turn += 1;
						return { ok: true, value: { status: "completed", tipId: `${name}#${turn}` } };
					},
					findEntries: async () => [
						{ id: `${name}#${turn}`, type: "message", message: { role: "assistant", content: [{ type: "text", text }] } },
					],
				};
				lanes.set(name, lane);
			}
			return lane;
		},
	};
}

const definition = (id, strategy, overrides = {}) => ({
	id,
	version: "1",
	strategy,
	instructions: `Run ${id}.`,
	inputSchema: { type: "object", properties: { n: { type: "integer" } }, required: ["n"], additionalProperties: false },
	outputSchema: { type: "object", properties: { doubled: { type: "integer" } }, required: ["doubled"], additionalProperties: false },
	maxRepairs: 0,
	inputDescription: "{n:integer}",
	outputDescription: "{doubled:integer}",
	...overrides,
});
const any = { inputSchema: {}, outputSchema: {} };
const constrained = {
	outputSchema: {
		type: "object",
		properties: { doubled: { type: "integer", minimum: 0 } },
		required: ["doubled"],
		additionalProperties: false,
	},
};

const register = (item) => ["agents.register", { definition: item }];
const strategies = [
	register(definition("det", "deterministic")),
	register(definition("pred", "predict")),
	register(definition("loop", "rlm")),
	register(definition("judge", "predict", { ...constrained, maxRepairs: 2 })),
	register(definition("judge1", "predict", { ...constrained, maxRepairs: 1, id: "judge1" })),
	register(definition("step", "deterministic", any)),
	register(definition("draft", "deterministic", any)),
	register(definition("review", "deterministic", any)),
	register(definition("modeled", "rlm", { model: "faux/defined-model" })),
];

const scenarios = [
	{
		name: "strategies share one contract",
		usage: true,
		limits: {},
		ops: [
			...strategies,
			["agents.list", {}],
			["agents.invoke", { definition: "det@1", input: { n: 4 } }],
			["agents.invoke", { definition: "pred@1", input: { n: 4 } }],
			["agents.invoke", { definition: "loop@1", input: { n: 4 } }],
			["agents.invoke", { definition: "loop@1", input: { n: 3 } }],
			["agents.invoke", { definition: "det@1", input: { n: -1 } }],
			["agents.invoke", { definition: "pred@1", input: { n: -1 } }],
			["agents.invoke", { definition: "loop@1", input: { n: -1 } }],
			["agents.invoke", { definition: "det@1", input: { n: "4" } }],
			["agents.invoke", { definition: "loop@1", input: { n: "4" } }],
			["agents.invoke", { definition: "judge@1", input: { n: 2 } }],
			["agents.invoke", { definition: "judge@1", input: { n: 3 } }],
			["agents.invoke", { definition: "judge1@1", input: { n: 3 } }],
			["agents.invoke", { definition: "judge@1", input: { n: 5 } }],
			["agents.invoke", { definition: "det@1", input: { n: 1 }, key: "same" }],
			["agents.invoke", { definition: "det@1", input: { n: 1 }, key: "same" }],
			["agents.invoke", { definition: "det@1", input: { n: 2 }, key: "same" }],
			["agents.invoke", { definition: "identity@1", input: { any: ["thing"] } }],
			["agents.invoke", { definition: "loop@1", input: { n: 6 }, model: "faux/explicit" }],
			["agents.invoke", { definition: "modeled@1", input: { n: 6 } }],
			["agents.invoke", { definition: "modeled@1", input: { n: 7 }, model: "faux/override" }],
			["agents.invoke", { definition: "loop@1", input: { n: 1 }, model: "no-slash" }],
			["agents.invoke", { definition: "loop@1", input: { n: 1 }, timeout_ms: 0 }],
			["agents.invoke", { definition: "loop@1", input: { n: 1 }, timeout_ms: 1.5 }],
			["agents.invoke", { definition: "loop@1", input: { n: 1 }, key: "  " }],
			["agents.invoke", { definition: "missing@1", input: {} }],
			["agents.invoke", { definition: "Missing", input: {} }],
			["agents.invoke", { definition: "loop@1" }],
			["agents.invoke", { definition: "loop@1", input: { n: 1 }, retries: 1 }],
			["agents.spawn", { definition: "det@1", input: { n: 9 } }],
			["agents.result", { id: "$last" }],
			["agents.inspect", { id: "$last" }],
			["agents.cancel", { id: "$last" }],
			["agents.result", { id: "missing" }],
			["agents.inspect", { id: "" }],
			["agents.inspect", { id: "x", extra: 1 }],
			["agents.tasks", {}],
			["agents.tasks", { extra: 1 }],
			["agents.status", {}],
			["agents.status", { graph: false }],
			["agents.register", { definition: definition("det", "deterministic", { instructions: "Changed." }) }],
			["agents.register", {}],
			["ping", {}],
			["nope.request", {}],
		],
	},
	{
		name: "workflow graph contract",
		usage: false,
		limits: {},
		ops: [
			...strategies,
			["workflows.run", { nodes: "not a list" }],
			["workflows.run", { nodes: [{ id: "a", definition: "step@1", input: 1, dependsOn: ["b"] }, { id: "b", definition: "step@1", input: 2, dependsOn: ["a"] }] }],
			["workflows.run", { nodes: [{ id: "self", definition: "step@1", input: 1, dependsOn: ["self"] }] }],
			["workflows.run", { nodes: [{ id: "a", definition: "step@1", input: 1, dependsOn: ["ghost"] }] }],
			["workflows.run", { nodes: [{ id: "a", definition: "step@1", input: 1 }, { id: "a", definition: "step@1", input: 1 }] }],
			["workflows.run", { nodes: [{ id: "a", definition: "det@1", input: { n: "x" } }] }],
			["workflows.run", { nodes: [{ id: "a", definition: "missing@1", input: 1 }] }],
			["workflows.run", { nodes: [{ id: "first", definition: "step@1", input: 1 }, { id: "a", definition: "step@1", inputFrom: "first" }] }],
			["workflows.run", { nodes: [{ id: "first", definition: "step@1", input: 1 }, { id: "a", definition: "step@1", input: 1, inputFrom: "first", dependsOn: ["first"] }] }],
			["workflows.run", { nodes: [{ id: "first", definition: "step@1", input: 1 }, { id: "a", definition: "step@1", input: 1, when: { node: "first", equals: 1 } }] }],
			["workflows.run", { nodes: [{ id: "first", definition: "step@1", input: 1 }, { id: "a", definition: "step@1", input: 1, dependsOn: ["first"], when: { node: "first" } }] }],
			["workflows.run", { nodes: [{ id: "a", definition: "step@1", input: 1, retries: 3 }] }],
			["workflows.run", { nodes: [{ id: "a", definition: "step@1", input: 1, dependsOn: "first" }] }],
			["workflows.run", { nodes: [{ id: "a", definition: "step@1", input: 1, join: "some" }] }],
			["workflows.run", { nodes: [{ id: "x", definition: "step@1", input: 1 }, { id: "a", definition: "step@1", dependsOn: ["x"], inputFrom: ["x"], join: "any" }] }],
			["workflows.run", { nodes: [{ id: "a", definition: "step@1", input: 1, worktree: true }] }],
			["workflows.run", { nodes: [{ id: "a", definition: "step@1", input: 1, worktree: "sometimes" }] }],
			["workflows.run", { nodes: [{ id: "w", definition: "draft@1", input: {}, revise: { from: "w", until: { equals: 1 }, max_rounds: 11 } }] }],
			["workflows.run", { nodes: [{ id: "w", definition: "draft@1", input: {}, revise: { from: "ghost", until: { equals: 1 }, max_rounds: 2 } }] }],
			["workflows.run", { nodes: [{ id: "w", definition: "draft@1", input: {}, revise: { from: "r", max_rounds: 2 } }, { id: "r", definition: "review@1", dependsOn: ["w"], inputFrom: "w" }] }],
			["workflows.run", { nodes: [{ id: "w", definition: "draft@1", input: {}, revise: { from: "r", until: { equals: 1 }, max_rounds: 2 } }, { id: "r", definition: "review@1", input: {} }] }],
			["workflows.run", { key: " ", nodes: [] }],
			["agents.tasks", {}],
			["workflows.run", { nodes: [
				{ id: "left", definition: "step@1", input: { side: "left" } },
				{ id: "right", definition: "det@1", input: { n: 4 } },
				{ id: "join", definition: "step@1", dependsOn: ["left", "right"], inputFrom: ["left", "right"] },
				{ id: "single", definition: "step@1", dependsOn: ["right"], inputFrom: "right" },
			] }],
			["workflows.run", { nodes: [
				{ id: "broken", definition: "step@1", input: { fail: true } },
				{ id: "after", definition: "step@1", input: 1, dependsOn: ["broken"] },
				{ id: "later", definition: "step@1", input: 2, dependsOn: ["after"] },
				{ id: "independent", definition: "step@1", input: { ok: true } },
			] }],
			["workflows.run", { nodes: [
				{ id: "classify", definition: "step@1", input: { route: "fix", detail: 7 } },
				{ id: "fix", definition: "step@1", dependsOn: ["classify"], inputFrom: "classify", when: { node: "classify", field: "route", equals: "fix" } },
				{ id: "ship", definition: "step@1", input: { shipping: true }, dependsOn: ["classify"], when: { node: "classify", field: "route", equals: "ship" } },
				{ id: "announce", definition: "step@1", input: 1, dependsOn: ["ship"] },
				{ id: "whole", definition: "step@1", input: 2, dependsOn: ["classify"], when: { node: "classify", equals: { detail: 7, route: "fix" } } },
			] }],
			["workflows.run", { nodes: [
				{ id: "source", definition: "step@1", input: { x: 1 } },
				{ id: "consumer", definition: "det@1", dependsOn: ["source"], inputFrom: "source" },
				{ id: "downstream", definition: "step@1", input: 1, dependsOn: ["consumer"] },
				{ id: "sibling", definition: "det@1", input: { n: 2 } },
			] }],
			["workflows.run", { nodes: [
				{ id: "ok", definition: "step@1", input: { v: 1 } },
				{ id: "bad", definition: "step@1", input: { fail: true } },
				{ id: "either", definition: "step@1", dependsOn: ["ok", "bad"], inputFrom: ["ok", "bad"], join: "any" },
				{ id: "none", definition: "step@1", dependsOn: ["bad", "after-bad"], inputFrom: ["bad", "after-bad"], join: "any" },
				{ id: "after-bad", definition: "step@1", input: 1, dependsOn: ["bad"] },
			] }],
			["workflows.run", { key: "loop", nodes: [
				{ id: "w", definition: "draft@1", input: {}, revise: { from: "r", until: { field: "approved", equals: true }, max_rounds: 3 } },
				{ id: "r", definition: "review@1", dependsOn: ["w"], inputFrom: "w" },
				{ id: "after", definition: "step@1", dependsOn: ["w", "r"], inputFrom: ["w", "r"] },
			] }],
			["workflows.run", { nodes: [
				{ id: "w", definition: "draft@1", input: {}, revise: { from: "r", until: { field: "approved", equals: "never" }, max_rounds: 2 } },
				{ id: "r", definition: "review@1", dependsOn: ["w"], inputFrom: "w" },
				{ id: "after", definition: "step@1", input: 1, dependsOn: ["w"] },
			] }],
			["workflows.run", { key: "loop", nodes: [
				{ id: "w", definition: "draft@1", input: {}, revise: { from: "r", until: { field: "approved", equals: true }, max_rounds: 3 } },
				{ id: "r", definition: "review@1", dependsOn: ["w"], inputFrom: "w" },
				{ id: "after", definition: "step@1", dependsOn: ["w", "r"], inputFrom: ["w", "r"] },
			] }],
			["workflows.run", { nodes: [{ id: "only", definition: "loop@1", input: { n: 21 } }] }],
			["agents.status", { graph: true }],
		],
	},
	{
		name: "subagents, verdicts and background jobs",
		usage: false,
		limits: {},
		ops: [
			["agents.invoke", { definition: "rlm-child@1", input: { prompt: "plain" } }],
			["agents.invoke", { definition: "rlm-child@1", input: { prompt: "finish:passed then reply" } }],
			["agents.invoke", { definition: "rlm-child@1", input: { prompt: "empty" } }],
			["agents.invoke", { definition: "rlm-child@1", input: { prompt: "" } }],
			["workflows.run", { nodes: [
				{ id: "good", definition: "rlm-child@1", input: { prompt: "finish:passed good" } },
				// After "good": a subagent's `check.concurrent` lists the lanes running beside it, which only a
				// sequence makes deterministic.
				{ id: "bad", definition: "rlm-child@1", input: { prompt: "finish:failed bad" }, dependsOn: ["good"] },
				{ id: "after-good", definition: "identity@1", dependsOn: ["good"], inputFrom: "good" },
				{ id: "after-bad", definition: "identity@1", dependsOn: ["bad"], inputFrom: "bad" },
			] }],
			["rlm.finish", { status: "passed" }],
			["background.start", { prompt: "summarize" }],
			["background.result", { id: "$last" }],
			["background.inspect", { id: "$last" }],
			["background.stop", { id: "$last" }],
			["background.list", {}],
			["background.start", { prompt: "keyed", key: "k1" }],
			["background.start", { prompt: "keyed", key: "k1" }],
			["background.start", { prompt: "" }],
			["background.start", { prompt: "x", extra: 1 }],
			["background.result", { id: "missing" }],
			["background.list", { extra: 1 }],
			["agents.spawn", { definition: "rlm-child@1", input: { prompt: "spawned" }, model: "faux/child" }],
			["agents.result", { id: "$last" }],
			["agents.status", {}],
		],
	},
	{
		name: "usage accounting",
		usage: true,
		limits: { maxAdmittedTasks: 3, maxWallMs: 600_000 },
		ops: [
			...strategies,
			["agents.invoke", { definition: "det@1", input: { n: 1 } }],
			["agents.invoke", { definition: "pred@1", input: { n: 1 } }],
			["agents.invoke", { definition: "judge@1", input: { n: 2 } }],
			["agents.invoke", { definition: "loop@1", input: { n: 1 } }],
			["agents.invoke", { definition: "loop@1", input: { n: -1 } }],
			["agents.invoke", { definition: "det@1", input: { n: 1 }, key: "k" }],
			["agents.invoke", { definition: "det@1", input: { n: 1 }, key: "k" }],
			["agents.invoke", { definition: "rlm-child@1", input: { prompt: "metered" } }],
			["agents.status", {}],
		],
	},
];

const results = [];
for (const scenario of scenarios) {
	usageIds = 0;
	const tasks = memory();
	const definitions = memory();
	const ledgerStore = memory();
	const adapterCalls = [];
	const prompts = [];
	const events = [];
	let host;
	const usage = new NativeUsageLedger(ledgerStore, { limits: scenario.limits, now: () => 1000 });
	host = new NativeRlmHost(harness(prompts, () => host), {}, {
		store: tasks,
		definitionStore: definitions,
		...(scenario.usage ? { usage } : {}),
		deterministic: deterministic(adapterCalls),
		predict: predict(adapterCalls),
		now: () => 1000,
		onDetachedEnd: (end) =>
			events.push({ task: clone(end.task), kind: end.kind, ownerLane: end.ownerLane, rootId: end.rootId ?? null, awaited: end.awaited, fetch: end.fetch, cost: end.cost }),
	});
	const steps = [];
	let last;
	for (const [type, payload] of scenario.ops) {
		const resolved = JSON.parse(JSON.stringify(payload).replace(/"\$last"/g, JSON.stringify(last ?? "none")));
		try {
			const result = clone(await host.handle(type, resolved, {}));
			if (result && typeof result === "object" && typeof result.id === "string") last = result.id;
			steps.push({ op: [type, payload], result });
		} catch (error) {
			steps.push({ op: [type, payload], error: error.message });
		}
		// Detached work ends before the next operation.
		for (let n = 0; n < 30; n++) await new Promise((r) => setImmediate(r));
	}
	await host.close();
	results.push({
		name: scenario.name,
		limits: scenario.limits,
		ledger: scenario.usage,
		steps,
		events,
		adapterCalls,
		prompts,
		journal: tasks.get(),
		definitions: definitions.get(),
		usage: ledgerStore.get(),
	});
}

const numbered = (pattern, label) => {
	const seen = new Map();
	return (text) =>
		text.replace(pattern, (match) => {
			if (!seen.has(match)) seen.set(match, `${label}#${seen.size}`);
			return seen.get(match);
		});
};
let text = JSON.stringify(results, null, "\t");
text = numbered(/ultron-task-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g, "ultron-task-")(text);
text = numbered(/\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b/g, "key-")(text);
text = numbered(/\bwf-[0-9a-f]{8}\b/g, "wf-")(text);
writeFileSync(join(here, "agents.json"), `${text}\n`);
