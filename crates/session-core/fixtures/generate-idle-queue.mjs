// Generates crates/session-core/fixtures/idle_queue.json: steering, follow-up and next-run messages queued while the
// lane is idle, then a prompt, through the TypeScript AgentHarness lane (packages/agent/src/harness/runtime/lane.ts:
// `enqueue` keeps them in the lane's inbox; the prompt's admission takes them by `selectAcceptedInbox` and places them
// before the prompt; what it leaves is delivered during the run) against the faux provider. Records, per scenario,
// the user texts of every provider request in order, the branch's user texts, and the lane's inbox right after the
// admission. The Rust test (crates/session-core/tests/idle_queue.rs) queues the same messages on the session actor.
// Usage: node crates/session-core/fixtures/generate-idle-queue.mjs   (from the repository root; deterministic)
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createModels, fauxAssistantMessage, fauxProvider } from "../../../packages/ai/src/index.ts";
import { AgentHarness } from "../../../packages/agent/src/harness/agent-harness.ts";
import { BACKGROUND_CONTEXT } from "../../../packages/agent/src/harness/context.ts";
import { MemoryStorage } from "../../../packages/agent/src/harness/session/memory.ts";
import { StorageBackedSession } from "../../../packages/agent/src/harness/session/session.ts";
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";

const here = dirname(fileURLToPath(import.meta.url));
let clock = 1_700_000_000_000;
Date.now = () => clock++;

// Each scenario: the queue modes, what is queued (in order) while idle, the prompt and the answers.
const SCENARIOS = [
	{
		name: "all_modes_take_every_queued_message_before_the_prompt",
		steeringMode: "all",
		followUpMode: "all",
		queued: [
			["steer", "s1"],
			["followUp", "f1"],
			["nextRun", "n1"],
			["steer", "s2"],
		],
		answers: ["A."],
	},
	{
		name: "one_at_a_time_takes_the_first_of_each_kind",
		steeringMode: "one-at-a-time",
		followUpMode: "one-at-a-time",
		queued: [
			["followUp", "f1"],
			["steer", "s1"],
			["followUp", "f2"],
			["nextRun", "n1"],
			["steer", "s2"],
		],
		answers: ["A.", "B.", "C."],
	},
	{
		name: "steering_one_at_a_time_follow_ups_all",
		steeringMode: "one-at-a-time",
		followUpMode: "all",
		queued: [
			["steer", "s1"],
			["steer", "s2"],
			["followUp", "f1"],
			["followUp", "f2"],
		],
		answers: ["A.", "B."],
	},
	{
		name: "follow_up_only",
		steeringMode: "all",
		followUpMode: "all",
		queued: [["followUp", "f1"]],
		answers: ["A."],
	},
];

const userText = (message) =>
	typeof message.content === "string"
		? message.content
		: message.content
				.filter((block) => block.type === "text")
				.map((block) => block.text)
				.join("");
const users = (messages) => messages.filter((m) => m.role === "user").map(userText);

const scenarios = [];
for (const [index, scenario] of SCENARIOS.entries()) {
	const session = new StorageBackedSession({ id: `idle-${index}`, createdAt: 1, storageVersion: 1 }, new MemoryStorage());
	const faux = fauxProvider();
	const models = createModels();
	models.setProvider(faux.provider);
	const requests = [];
	faux.setResponses(
		scenario.answers.map((answer) => (context) => {
			requests.push(users(context.messages));
			return fauxAssistantMessage(answer, { timestamp: 2 });
		}),
	);
	const { harness } = await AgentHarness.create(
		{
			session,
			models,
			model: faux.getModel(),
			steeringMode: scenario.steeringMode,
			followUpMode: scenario.followUpMode,
		},
		BACKGROUND_CONTEXT,
	);
	const lane = await harness.lane("main", BACKGROUND_CONTEXT);
	const ids = {};
	for (const [kind, text] of scenario.queued) {
		const queued = await lane[kind](text, undefined, BACKGROUND_CONTEXT);
		if (!queued.ok) throw new Error(`${kind} refused: ${queued.error.message}`);
		ids[queued.value.entryId] = text;
	}
	let inboxAfterAdmission = null;
	harness.events.on("run_start", () => {
		inboxAfterAdmission = lane.state.inbox.map((item) => [item.kind, ids[item.entryId]]);
	});
	const ended = await lane.prompt("request", undefined, BACKGROUND_CONTEXT);
	if (!ended.ok) throw new Error(`prompt refused: ${ended.error.message}`);
	const tip = lane.state.tipId;
	const branch = await session.scanBranch({ start: tip, order: "oldestFirst" }, BACKGROUND_CONTEXT);
	scenarios.push({
		name: scenario.name,
		steeringMode: scenario.steeringMode,
		followUpMode: scenario.followUpMode,
		queued: scenario.queued,
		answers: scenario.answers,
		inboxAfterAdmission,
		requests,
		branch: branch
			.filter((entry) => entry.type === "message")
			.map((entry) => `${entry.message.role}: ${userText(entry.message)}`),
		pendingResponses: faux.getPendingResponseCount(),
	});
	await session.close(BACKGROUND_CONTEXT);
}
assertOffline();
writeFileSync(join(here, "idle_queue.json"), `${JSON.stringify({ scenarios }, null, 1)}\n`);
