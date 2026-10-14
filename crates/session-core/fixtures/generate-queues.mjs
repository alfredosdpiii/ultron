// Generates crates/session-core/fixtures/queues.json: runs the TypeScript Agent (packages/agent/src/agent.ts, which
// drives agent-loop.ts) against the TypeScript faux provider on queue-ordering scenarios: steering and follow-up
// messages queued while the model produces its final answer. Records, per scenario, the user texts of every provider
// request in order. The Rust test (crates/cli/tests/daemon_queues.rs) queues the same messages at the same point of a
// real durable session and requires the same requests.
// Usage: node crates/session-core/fixtures/generate-queues.mjs   (from the repository root; deterministic)
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createAssistantMessageEventStream, createFauxCore, fauxAssistantMessage } from "../../../packages/ai/src/index.ts";
import { Agent } from "../../../packages/agent/src/agent.ts";

const here = dirname(fileURLToPath(import.meta.url));
let clock = 1_700_000_000_000;
Date.now = () => clock++;

const user = (text) => ({ role: "user", content: [{ type: "text", text }], timestamp: 1 });
const msg = (text) => fauxAssistantMessage(text, { timestamp: 2 });

// Each scenario: the prompt, the answers in order, and what is queued while the first answer streams (before it is
// final). Steering and follow-up modes are "all", as the session's defaults.
const SCENARIOS = [
	{ name: "steer_and_follow_up_at_a_final_answer", prompt: "first", answers: ["A.", "B.", "C."], steers: ["steer me"], followUps: ["follow me"] },
	{ name: "follow_up_queued_before_steer", prompt: "first", answers: ["A.", "B.", "C."], followUps: ["follow me"], steers: ["steer me"], followUpFirst: true },
	{ name: "two_steers_two_follow_ups", prompt: "first", answers: ["A.", "B.", "C."], steers: ["s1", "s2"], followUps: ["f1", "f2"] },
	{ name: "follow_up_only", prompt: "first", answers: ["A.", "B."], steers: [], followUps: ["follow me"] },
];

const text = (message) =>
	typeof message.content === "string"
		? message.content
		: message.content
				.filter((block) => block.type === "text")
				.map((block) => block.text)
				.join("");

const scenarios = [];
for (const scenario of SCENARIOS) {
	const core = createFauxCore({ api: "faux", provider: "faux", tokenSize: { min: 3, max: 3 }, tokensPerSecond: 1e9 });
	core.setResponses(scenario.answers.map(msg));
	const requests = [];
	const streamFn = (model, context, options) => {
		requests.push(context.messages.filter((m) => m.role === "user").map(text));
		const inner = core.streamSimple(model, context, options);
		const outer = createAssistantMessageEventStream();
		(async () => {
			for await (const event of inner) outer.push(event);
			outer.end(await inner.result());
		})();
		return outer;
	};
	const agent = new Agent({
		initialState: { model: core.getModel(), tools: [] },
		streamFn,
		followUpMode: "all",
		steeringMode: "all",
	});
	let queued = false;
	agent.subscribe((event) => {
		if (queued || event.type !== "message_start" || event.message.role !== "assistant") return;
		queued = true;
		if (scenario.followUpFirst) for (const t of scenario.followUps) agent.followUp(user(t));
		for (const t of scenario.steers) agent.steer(user(t));
		if (!scenario.followUpFirst) for (const t of scenario.followUps) agent.followUp(user(t));
	});
	await agent.prompt(scenario.prompt);
	scenarios.push({
		name: scenario.name,
		prompt: scenario.prompt,
		answers: scenario.answers,
		steers: scenario.steers,
		followUps: scenario.followUps,
		followUpFirst: scenario.followUpFirst ?? false,
		requests,
		transcript: agent.state.messages.map((m) => `${m.role}: ${text(m)}`),
	});
}
writeFileSync(join(here, "queues.json"), `${JSON.stringify({ scenarios }, null, 1)}\n`);
