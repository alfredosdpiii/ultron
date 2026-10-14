// Generates crates/kernel-apis/fixtures/skills-invoke.json: `skills.invoke` of createSkillModule (ultron/skills.ts)
// over the three-skill catalog of packages/coding-agent/test/ultron-skills.test.ts and a host that records every
// spawn (the test's `fakeHost`). Records each invoke's reply or error, the spawned requests with their parents, and
// the module document's invocations. Random ids are numbered by first appearance.
// Usage: node crates/kernel-apis/fixtures/generate-skills-invoke.mjs
import { writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = join(resolve(here, "../../.."), "packages/coding-agent");
const { createSkillModule } = await import(pathToFileURL(join(pkg, "src/ultron/skills.ts")).href);
const { createMemoryModuleStore } = await import(pathToFileURL(join(pkg, "src/ultron/rlm/host-module.ts")).href);

const REVIEW = "---\nname: review\ndescription: Review a diff for correctness bugs\n---\nRead the diff, then report bugs with file and line.";
const DEPLOY = '---\nname: deploy\ndescription: Ship a release\nallowed-tools: [bash, write]\ntools: ["*"]\ngrants: { network: true }\npermissions: admin\nmodel: anthropic/claude-opus\n---\nRun the release script. Review the changelog first.';
const PRIVATE = "---\nname: private\ndescription: Rotate review credentials\ndisable-model-invocation: true\n---\nOnly a user may run this.";
const NESTED = "---\nname: nested\ndescription: Nested\n---\n  Body with \"quotes\" & <tags>.\n\n";
const skills = [
	{ name: "review", description: "Review a diff for correctness bugs", filePath: "/skills/review/SKILL.md", content: REVIEW },
	{ name: "deploy", description: "Ship a release", filePath: "/skills/deploy/SKILL.md", content: DEPLOY },
	{ name: "private", description: "Rotate review credentials", filePath: "/skills/private/SKILL.md", content: PRIVATE, disableModelInvocation: true },
	{ name: "nested", description: "Nested", filePath: "relative.md", content: NESTED },
];

const store = createMemoryModuleStore();
const module = createSkillModule({ store, loadSkills: async () => skills.map((s) => ({ ...s })), now: () => 1000 });
const spawned = [];
let failNext = false;
const host = {
	callerTaskId: (caller) => (caller.lane === "ultron.rlm-child.parent-task" ? "parent-task" : null),
	spawn: async (request, parent) => {
		spawned.push({ request: structuredClone(request), parent });
		if (failNext) {
			failNext = false;
			throw new Error("admission refused: budget exhausted");
		}
		return { id: `task-${spawned.length}`, definition: request.definition, state: "queued" };
	},
	now: () => 0,
};
const sha = async (text) => {
	const { createHash } = await import("node:crypto");
	return createHash("sha256").update(text).digest("hex");
};
const calls = [
	["main", { name: "deploy", version: await sha(DEPLOY), input: { tag: "v1" } }],
	["ultron.rlm-child.parent-task", { name: "review", version: await sha(REVIEW), input: [1, "two", { three: null }] }],
	["main", { name: "nested", version: await sha(NESTED), input: "plain" }],
	["main", { name: "review", version: await sha(REVIEW), input: null }, "fail"],
	["main", { name: "private", version: await sha(PRIVATE), input: {} }],
	["main", { name: "review", version: "0".repeat(64), input: {} }],
];
const results = [];
for (const [lane, payload, fail] of calls) {
	failNext = fail === "fail";
	try {
		results.push({ ok: await module.handle({ type: "skills.invoke", payload, caller: { lane }, context: {} }, host) });
	} catch (error) {
		results.push({ error: error.message });
	}
}
const document = await store.read();
const out = { calls, results, spawned, invocations: document.invocations };
// Random ids (node:crypto randomUUID) numbered by first appearance.
const seen = new Map();
const text = JSON.stringify(out, null, "\t").replace(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/g, (id) => {
	if (!seen.has(id)) seen.set(id, `uuid-${seen.size + 1}`);
	return seen.get(id);
});
writeFileSync(join(here, "skills-invoke.json"), `${text}\n`);
