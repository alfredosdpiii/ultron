// Generates crates/kernel-apis/fixtures/family.json: `agent_message.*` cells (rlm/family_api.py) through the real
// runtime.py and kernel.ts, served by NativeRlmHost with the family module of ultron/family.ts. The module gets a
// scripted NativeHostApi (a fixed task tree, the calling lane's task, a steer log, a clock); a `test.*` module moves
// the calling lane and the clock between cells. tests/family.rs implements the same fakes.
// Usage: node crates/kernel-apis/fixtures/generate-family.mjs
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { attempt, FIXED_NOW, record, show } from "./generate.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const pkg = join(resolve(here, "../../.."), "packages/coding-agent");
const load = (path) => import(pathToFileURL(join(pkg, path)).href);
const { createFamilyModule } = await load("src/ultron/family.ts");
const { createMemoryModuleStore } = await load("src/ultron/rlm/host-module.ts");

/** The task tree: root → parent → {child, child2, done, verifier}; root → other. */
export const TASKS = [
	{ id: "t-parent", definition: "worker@1", state: "running" },
	{ id: "t-other", definition: "worker@2", state: "running" },
	{ id: "t-child", definition: "helper@1", state: "running", parentId: "t-parent" },
	{ id: "t-child2", definition: "helper@1", state: "admitted", parentId: "t-parent" },
	{ id: "t-done", definition: "helper@1", state: "completed", parentId: "t-parent" },
	{ id: "t-verifier", definition: "code-reviewer@1", state: "running", parentId: "t-parent" },
];
/** Lane → task; "main" is the root. */
export const LANES = { main: null, "lane-parent": "t-parent", "lane-child": "t-child", "lane-verifier": "t-verifier" };
/** steer(): true for t-child, false for t-other, throws for anything else. */
export const STEERABLE = ["t-child"];

function familyHost() {
	const store = createMemoryModuleStore();
	const current = { lane: "main", now: FIXED_NOW };
	const steered = [];
	const fake = {
		callerTaskId: () => LANES[current.lane],
		taskLane: () => null,
		strategy: () => "rlm",
		tasks: async () => TASKS.map((task) => ({ ...task, key: task.id })),
		spawn: async () => {
			throw new Error("unused");
		},
		result: async () => {
			throw new Error("unused");
		},
		cancel: async () => {
			throw new Error("unused");
		},
		steer: async (taskId, message) => {
			steered.push([taskId, message]);
			if (STEERABLE.includes(taskId)) return true;
			if (taskId === "t-other") return false;
			throw new Error("lane is gone");
		},
		usage: async () => null,
		now: () => current.now,
	};
	const module = createFamilyModule({ store, maxInbox: 3 });
	return {
		modules: [
			{ prefixes: module.prefixes, handle: (request) => module.handle(request, fake) },
			{
				prefixes: ["test."],
				handle: async (request) => {
					if (request.type === "test.lane") current.lane = request.payload.lane;
					if (request.type === "test.clock") current.now += request.payload.ms;
					return { lane: current.lane, now: current.now };
				},
			},
		],
		state: async () => ({ document: (await store.read()) ?? null, steered }),
	};
}

const lane = (name) => `await rlm.host_request("test.lane", {"lane": "${name}"})`;
const tries = (calls) => `import json as _json\n${calls.map(attempt).join("\n")}`;

const scenarios = [
	[
		"send, receive, list across the family",
		[
			show('await agent_message.send("hello parent task", receiver_role="child", receiver_id="t-parent")'),
			tries([
				'agent_message.send("x")',
				'agent_message.send("x", receiver_role="child")',
				'agent_message.send("x", receiver_role="child", receiver_id="t-child")',
				'agent_message.send("x", receiver_role="child", receiver_name="helper")',
				'agent_message.send("x", receiver_role="child", receiver_id="t-parent", receiver_name="helper")',
				'agent_message.send("x", receiver_role="child", receiver_id="t-parent", ttl_ms=0)',
				'agent_message.send("x", receiver_role="child", receiver_id="t-parent", ttl_ms=86400001)',
				'agent_message.send("x", receiver_role="child", receiver_id="t-parent", ttl_ms=1.5)',
				'agent_message.send("x", receiver_role="child", receiver_id="t-parent", key="k" * 257)',
				'agent_message.send("x", receiver_role="child", receiver_id="   ")',
				'agent_message.send("é" * 9000, receiver_role="child", receiver_id="t-parent")',
				'rlm.host_request("agent_message.send", {"message": "  "})',
				'rlm.host_request("agent_message.send", {"message": "x", "receiver_role": "sibling"})',
				'rlm.host_request("agent_message.send", {"message": "x", "receiver_role": "child", "receiver_id": "t-parent", "steer": "yes"})',
				'rlm.host_request("agent_message.send", {"message": "x", "receiver_role": "child", "receiver_id": "t-parent", "ttl_ms": True})',
				'rlm.host_request("agent_message.send", {"message": "x", "receiver_role": "child", "receiver_id": "t-parent", "bogus": 1})',
				'rlm.host_request("agent_message.send", {"message": "x", "receiver_role": None, "receiver_id": None, "key": None})',
				'rlm.host_request("agent_message.nope", {})',
				'agent_message.send("nudge", receiver_role="child", receiver_id="t-other", steer=True)',
				'agent_message.receive(limit=0)',
				'agent_message.receive(limit=4)',
			]),
			show("await agent_message.list()"),
			`${lane("lane-parent")}\n${show("await agent_message.receive()")}`,
			show("await agent_message.receive()"),
			tries([
				'agent_message.send("report one", key="k1")',
				'agent_message.send("report one", key="k1")',
				'agent_message.send("report two", key="k1")',
				'agent_message.send("x", receiver_name="worker")',
				'agent_message.send("x", receiver_name="other")',
				'agent_message.send("x", receiver_id="nobody")',
				'agent_message.send("x", receiver_role="child", receiver_name="helper")',
				'agent_message.send("x", receiver_role="child", receiver_id="t-done")',
				'agent_message.send("x", receiver_role="child", receiver_id="t-other")',
				'agent_message.send("steer me", receiver_role="child", receiver_id="t-child", steer=True)',
				'agent_message.send("steer me", receiver_role="child", receiver_id="t-child", steer=True, key="s1")',
				'agent_message.send("steer me", receiver_role="child", receiver_id="t-child", steer=True, key="s1")',
				'agent_message.send("no lane", receiver_role="child", receiver_id="t-child2", steer=True)',
				'agent_message.send("check this", receiver_role="child", receiver_name="code-reviewer")',
				'agent_message.send("x", receiver_role="child", receiver_id="t-verifier", steer=True)',
			]),
			`${lane("lane-verifier")}\n${tries([
				'agent_message.send("x", receiver_role="child", receiver_id="t-child")',
				'agent_message.send("verdict: ok")',
				"agent_message.receive()",
			])}`,
			`${lane("lane-child")}\n${tries([
				"agent_message.receive()",
				'agent_message.send("one")',
				'agent_message.send("two", ttl_ms=1000)',
				"agent_message.list()",
			])}`,
			`${lane("lane-parent")}\n${tries(["agent_message.receive(limit=2)", "agent_message.list()"])}`,
			`${lane("lane-child")}\n${tries([
				'agent_message.send("three")',
				'agent_message.send("four")',
				'agent_message.send("five")',
			])}`,
			`await rlm.host_request("test.clock", {"ms": 600000})\n${lane("lane-parent")}\n${tries([
				"agent_message.list()",
				"agent_message.receive()",
				"agent_message.receive()",
			])}`,
			`${lane("main")}\n${show("await agent_message.list()")}`,
		],
	],
];

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url))
	await record({ family: [familyHost, scenarios] });
