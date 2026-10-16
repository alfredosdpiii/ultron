// Generates crates/session-core/fixtures/review_command.json: what the TypeScript worker's AgentController
// (packages/coding-agent/src/experimental/services/agent-controller-provider.ts `toTextPrompt`, which applies
// src/ultron/review.ts `expandReviewCommand`) hands its lane for each of `prompt`, `steer`, `followUp` and `nextRun`,
// for typed texts with and without `/review`. A recording lane stands in for the AgentLane; nothing reaches a model.
// The Rust tests (crates/session-core/tests/review_command.rs, crates/cli/tests/daemon_review_command.rs) expand the same
// texts and require the same messages.
// Usage: node crates/session-core/fixtures/generate-review-command.mjs   (from the repository root; deterministic)
import "../../cli/fixtures/offline-guard.mjs";
import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createAgentController } from "../../../packages/coding-agent/src/experimental/services/agent-controller-provider.ts";
import { assertOffline } from "../../cli/fixtures/offline-guard.mjs";

const here = dirname(fileURLToPath(import.meta.url));

const TEXTS = [
	"/review",
	"  /review  ",
	"/review main",
	'/review main --only sec,bugs "src/a b.py"',
	"/review 12 --post",
	"/review --post=yes",
	"/review --poster",
	"/review\tsrc/x.py\n--budget 300k",
	"/review src/\"a\"\\b",
	"/review ünïcode ✓",
	"/review a\u0001b\u007f",
	"/review\u00a0x",
	"\ufeff/review x\ufeff",
	"/review\u0085x",
	"/review\u2028x\u3000",
	"/review x\u0085",
	"/review --post\u00a0",
	"/review\u180ex",
	"/reviewer x",
	"please /review this",
	"/Review",
	"plain text",
	"",
];

const seen = [];
const record = (operation) => async (message, images) => {
	seen.push({ operation, message, images: images ?? null });
	return operation === "prompt"
		? { ok: true, value: { operationId: "op-1", status: "completed" } }
		: { ok: true, value: { entryId: "e-1" } };
};
const lane = {
	prompt: record("prompt"),
	steer: record("steer"),
	followUp: record("followUp"),
	nextRun: record("nextRun"),
};
const controller = createAgentController(lane);
const cases = [];
for (const text of TEXTS) {
	for (const operation of ["prompt", "steer", "followUp", "nextRun"]) {
		seen.length = 0;
		const answer = await controller[operation]({ message: text, images: null }, {});
		cases.push({ operation, text, message: seen[0].message, answer });
	}
}
assertOffline();
writeFileSync(join(here, "review_command.json"), `${JSON.stringify({ cases }, null, 1)}\n`);
