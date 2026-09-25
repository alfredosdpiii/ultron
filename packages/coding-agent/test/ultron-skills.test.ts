import { createHash } from "node:crypto";
import { describe, expect, test } from "vitest";
import {
	createMemoryModuleStore,
	type HostCaller,
	type HostModuleStore,
	type HostTaskRequest,
	type NativeHostApi,
	ROOT_CALLER,
} from "../src/ultron/rlm/host-module.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";
import { createSkillModule, type SkillSource } from "../src/ultron/skills.ts";

const context = {} as never;
const sha = (text: string) => createHash("sha256").update(text).digest("hex");
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

const REVIEW = `---
name: review
description: Review a diff for correctness bugs
---
Read the diff, then report bugs with file and line.`;

const DEPLOY = `---
name: deploy
description: Ship a release
allowed-tools: [bash, write]
tools: ["*"]
grants: { network: true }
permissions: admin
model: anthropic/claude-opus
---
Run the release script. Review the changelog first.`;

const PRIVATE = `---
name: private
description: Rotate review credentials
disable-model-invocation: true
---
Only a user may run this.`;

function source(name: string, description: string, content: string, disableModelInvocation?: boolean): SkillSource {
	return { name, description, filePath: `/skills/${name}/SKILL.md`, content, disableModelInvocation };
}

function catalog() {
	const skills = new Map<string, SkillSource>([
		["review", source("review", "Review a diff for correctness bugs", REVIEW)],
		["deploy", source("deploy", "Ship a release", DEPLOY)],
		["private", source("private", "Rotate review credentials", PRIVATE, true)],
	]);
	return {
		skills,
		loadSkills: async () => {
			await tick();
			return [...skills.values()].map((skill) => ({ ...skill }));
		},
	};
}

function fakeHost(callers: Record<string, string> = {}) {
	const spawned: Array<{ request: HostTaskRequest; parent: string | null }> = [];
	const host = {
		callerTaskId: (caller: HostCaller) => callers[caller.lane] ?? null,
		spawn: async (request: HostTaskRequest, parent: string | null) => {
			spawned.push({ request: structuredClone(request), parent });
			return { id: `task-${spawned.length}`, definition: request.definition, state: "queued" };
		},
		now: () => 0,
	} as unknown as NativeHostApi;
	return { host, spawned };
}

function setup(store: HostModuleStore = createMemoryModuleStore()) {
	const skills = catalog();
	const module = createSkillModule({ store, loadSkills: skills.loadSkills, now: () => 1000 });
	const { host, spawned } = fakeHost({ "ultron.rlm-child.parent-task": "parent-task" });
	const call = (type: string, payload: Record<string, unknown> = {}, caller: HostCaller = ROOT_CALLER) =>
		module.handle({ type, payload, caller, context }, host) as Promise<any>;
	return { skills: skills.skills, module, call, spawned, store };
}

describe("skills module (A31)", () => {
	test("lists skills with content-hash versions and invocability", async () => {
		const { call } = setup();
		const listed = await call("skills.list");
		expect(listed.skills).toEqual([
			{ name: "review", description: "Review a diff for correctness bugs", version: sha(REVIEW), invocable: true },
			{ name: "deploy", description: "Ship a release", version: sha(DEPLOY), invocable: true },
			{ name: "private", description: "Rotate review credentials", version: sha(PRIVATE), invocable: false },
		]);
	});

	test("select explains each candidate, ranks deterministically, and records the decision", async () => {
		const { call, store } = setup();
		const first = await call("skills.select", { query: "review the diff" });
		const second = await call("skills.select", { query: "review the diff" });
		expect(first.candidates).toEqual(second.candidates);
		expect(first.id).not.toBe(second.id);
		expect(first.candidates.map((candidate: { name: string }) => candidate.name)).toEqual(["review", "deploy"]);
		expect(first.candidates[0]).toEqual({
			name: "review",
			version: sha(REVIEW),
			score: 3 + 2 + 2 + 1,
			matches: [
				{ term: "review", fields: ["name", "description"] },
				{ term: "diff", fields: ["description", "body"] },
			],
		});
		expect(first.candidates[1].matches).toEqual([{ term: "review", fields: ["body"] }]);
		expect(first.considered).toEqual({ deploy: sha(DEPLOY), private: sha(PRIVATE), review: sha(REVIEW) });
		// Non-invocable skills are considered but never selected, even when they match.
		expect(first.excluded).toEqual([{ name: "private", version: sha(PRIVATE), reason: "not_invocable" }]);
		expect(first.caller_task_id).toBeNull();
		expect(first.chosen).toEqual([
			{ name: "review", version: sha(REVIEW) },
			{ name: "deploy", version: sha(DEPLOY) },
		]);
		const limited = await call("skills.select", { query: "review", limit: 1 });
		expect(limited.candidates).toHaveLength(1);
		const document = (await store.read()) as { decisions: Array<{ id: string }> };
		expect(document.decisions.map((decision) => decision.id)).toEqual([first.id, second.id, limited.id]);
	});

	test("why returns the original decision after the catalog changes", async () => {
		const { call, skills } = setup();
		const decision = await call("skills.select", { query: "release" }, { lane: "ultron.rlm-child.parent-task" });
		expect(decision.caller_task_id).toBe("parent-task");
		skills.set("deploy", source("deploy", "Ship nothing", "---\nname: deploy\n---\nDo nothing."));
		const refreshed = await call("skills.refresh");
		expect(refreshed.changes).toEqual([
			{ at: 1000, generation: 2, name: "deploy", from: sha(DEPLOY), to: sha("---\nname: deploy\n---\nDo nothing.") },
		]);
		expect(await call("skills.why", { decision_id: decision.id })).toEqual(decision);
		expect((await call("skills.select", { query: "release" })).candidates).toEqual([]);
		await expect(call("skills.why", { decision_id: "missing" })).rejects.toThrow(/Unknown skill decision/);
	});

	test("version-pinned load succeeds and a stale version is rejected", async () => {
		const { call, skills, store } = setup();
		const pinned = await call("skills.load", { name: "review", version: sha(REVIEW) });
		expect(pinned).toMatchObject({
			name: "review",
			version: sha(REVIEW),
			content: REVIEW,
			context_chars: REVIEW.length,
		});
		expect(pinned.ignored_capability_requests).toEqual({});
		const changed = `${REVIEW}\nAlso check tests.`;
		skills.set("review", source("review", "Review a diff for correctness bugs", changed));
		await call("skills.refresh");
		await expect(call("skills.load", { name: "review", version: sha(REVIEW) })).rejects.toThrow(
			/Stale skill version: review@[a-f0-9]{64} was requested but the catalog has review@/,
		);
		await expect(call("skills.invoke", { name: "review", version: sha(REVIEW), input: {} })).rejects.toThrow(
			/Stale skill version/,
		);
		expect((await call("skills.load", { name: "review" })).content).toBe(changed);
		skills.delete("review");
		await call("skills.refresh");
		await expect(call("skills.load", { name: "review", version: sha(changed) })).rejects.toThrow(/skill removed/);
		const document = (await store.read()) as {
			loads: Array<{ name: string; version: string; caller_task_id: null }>;
		};
		expect(document.loads.map(({ name, version, caller_task_id }) => ({ name, version, caller_task_id }))).toEqual([
			{ name: "review", version: sha(REVIEW), caller_task_id: null },
			{ name: "review", version: sha(changed), caller_task_id: null },
		]);
	});

	test("capability-granting frontmatter is reported and never reaches the spawned child", async () => {
		const { call, spawned } = setup();
		const loaded = await call("skills.load", { name: "deploy" });
		expect(loaded.ignored_capability_requests).toEqual({
			"allowed-tools": ["bash", "write"],
			tools: ["*"],
			grants: { network: true },
			permissions: "admin",
			model: "anthropic/claude-opus",
		});
		const invoked = await call(
			"skills.invoke",
			{ name: "deploy", version: sha(DEPLOY), input: { tag: "v1" } },
			{ lane: "ultron.rlm-child.parent-task" },
		);
		expect(invoked).toMatchObject({ task_id: "task-1", name: "deploy", version: sha(DEPLOY) });
		expect(spawned).toHaveLength(1);
		const { request, parent } = spawned[0];
		expect(parent).toBe("parent-task");
		expect(Object.keys(request).sort()).toEqual(["definition", "input", "key"]);
		expect(request.definition).toBe("rlm-child@1");
		expect(Object.keys(request.input as object)).toEqual(["prompt"]);
		const prompt = (request.input as { prompt: string }).prompt;
		expect(prompt).toContain("Run the release script. Review the changelog first.");
		expect(prompt).toContain(`Skill version: ${sha(DEPLOY)}`);
		expect(prompt).toContain('{"tag":"v1"}');
		expect(prompt).not.toContain("allowed-tools");
	});

	test("invoke through the real host keeps the child's model and tools and links the parent", async () => {
		const setModels: unknown[] = [];
		const prompts: string[] = [];
		const lane = {
			findEntries: async () => [],
			getActiveTools: async () => [],
			setModel: async (model: unknown) => {
				setModels.push(model);
			},
			setActiveTools: async () => {
				throw new Error("skills must not change tools");
			},
			prompt: (text: string) => {
				prompts.push(text);
				return new Promise<never>(() => {});
			},
			abort: async () => ({ ok: true }),
			steer: async () => ({ ok: true, value: {} }),
		};
		let taskState: unknown;
		const taskStore: NativeHostStore = {
			read: async () => taskState as never,
			write: async (next) => {
				taskState = structuredClone(next);
			},
		};
		const skills = catalog();
		const host = new NativeRlmHost({ lane: async () => lane } as never, lane as never, {
			store: taskStore,
			modules: [createSkillModule({ store: createMemoryModuleStore(), loadSkills: skills.loadSkills })],
		});
		const parent = (await host.handle("background.start", { prompt: "wait" }, context)) as { id: string };
		await new Promise((resolve) => setTimeout(resolve, 10));
		const childLane = { lane: `ultron.background-job.${parent.id}` };
		const invoked = (await host.handle(
			"skills.invoke",
			{ name: "deploy", version: sha(DEPLOY), input: {} },
			context,
			childLane,
		)) as { task_id: string };
		await new Promise((resolve) => setTimeout(resolve, 10));
		const tasks = (await host.handle("agents.tasks", {}, context)) as {
			tasks: Array<{ id: string; definition: string; parentId?: string }>;
		};
		expect(tasks.tasks.find((task) => task.id === invoked.task_id)).toMatchObject({
			definition: "rlm-child@1",
			parentId: parent.id,
		});
		expect(setModels).toEqual([]);
		expect(prompts.some((text) => text.includes("Run the release script."))).toBe(true);
		await host.close();
	});

	test("concurrent loads, selects, and invokes each observe one catalog snapshot while content changes", async () => {
		const { call, skills, spawned } = setup();
		await call("skills.list");
		const versions = [REVIEW];
		const jobs: Promise<unknown>[] = [];
		for (let round = 1; round <= 20; round += 1) {
			const content = `${REVIEW}\nRevision ${round}.`;
			versions.push(content);
			jobs.push(
				(async () => {
					skills.set("review", source("review", "Review a diff for correctness bugs", content));
					await call("skills.refresh");
				})(),
			);
			for (const pinned of [REVIEW, versions[round - 1]]) {
				jobs.push(
					call("skills.load", { name: "review", version: sha(pinned) }).then(
						(loaded) => {
							expect(loaded.content).toBe(pinned);
							expect(loaded.version).toBe(sha(pinned));
							return "loaded";
						},
						(error: Error) => {
							expect(error.message).toMatch(/^Stale skill version/);
							return "stale";
						},
					),
				);
				jobs.push(
					call("skills.invoke", { name: "review", version: sha(pinned), input: { round } }).catch(
						(error: Error) => {
							expect(error.message).toMatch(/^Stale skill version/);
						},
					),
				);
			}
			jobs.push(
				call("skills.load", { name: "review" }).then((loaded) => {
					expect(loaded.version).toBe(sha(loaded.content));
					expect(versions).toContain(loaded.content);
				}),
			);
			jobs.push(
				call("skills.select", { query: "review diff" }).then((decision) => {
					const chosen = decision.chosen.find((item: { name: string }) => item.name === "review");
					expect(chosen.version).toBe(decision.considered.review);
				}),
			);
		}
		const outcomes = await Promise.all(jobs);
		// The race is real: some pinned loads win against a refresh and some lose.
		expect(outcomes).toContain("loaded");
		expect(outcomes).toContain("stale");
		for (const { request } of spawned) {
			const prompt = (request.input as { prompt: string }).prompt;
			const version = /Skill version: ([a-f0-9]{64})/.exec(prompt)![1];
			const content = versions.find((candidate) => sha(candidate) === version)!;
			expect(prompt).toContain(content.slice(content.indexOf("---\n", 3) + 4).trim());
		}
		expect((await call("skills.list")).skills[0].version).toBe(sha(versions[20]));
	});

	test("disableModelInvocation skills are listed but cannot be invoked", async () => {
		const { call, spawned } = setup();
		expect(
			(await call("skills.list")).skills.find((skill: { name: string }) => skill.name === "private"),
		).toMatchObject({ invocable: false });
		await expect(call("skills.invoke", { name: "private", version: sha(PRIVATE), input: {} })).rejects.toThrow(
			/disables model invocation/,
		);
		expect(spawned).toEqual([]);
	});

	test("rejects unknown payload fields, forged identity, and malformed arguments", async () => {
		const { call } = setup();
		await expect(call("skills.list", { verbose: true })).rejects.toThrow(/Unknown payload field: verbose/);
		await expect(call("skills.refresh", { force: true })).rejects.toThrow(/Unknown payload field/);
		await expect(call("skills.select", { query: "review", caller_task_id: "root" })).rejects.toThrow(
			/Unknown payload field: caller_task_id/,
		);
		await expect(call("skills.load", { name: "review", tools: ["bash"] })).rejects.toThrow(/Unknown payload field/);
		await expect(
			call("skills.invoke", { name: "review", version: sha(REVIEW), input: {}, model: "a/b" }),
		).rejects.toThrow(/Unknown payload field: model/);
		await expect(call("skills.invoke", { name: "review", version: sha(REVIEW) })).rejects.toThrow(/input/);
		await expect(call("skills.why", { decision_id: "x", sender: "root" })).rejects.toThrow(/Unknown payload field/);
		await expect(call("skills.load", { name: "review", version: "v1" })).rejects.toThrow(/sha256/);
		await expect(call("skills.select", { query: "  " })).rejects.toThrow(/query/);
		await expect(call("skills.select", { query: "x", limit: 0 })).rejects.toThrow(/limit/);
		await expect(call("skills.grant", {})).rejects.toThrow(/Unsupported skills request/);
		await expect(call("skills.load", { name: "missing" })).rejects.toThrow(/Unknown skill: missing/);
	});

	test("durable state survives a new module instance and records catalog changes across restarts", async () => {
		const store = createMemoryModuleStore();
		const first = setup(store);
		const decision = await first.call("skills.select", { query: "review" });
		const second = setup(store);
		second.skills.set("review", source("review", "Review", "changed"));
		expect(await second.call("skills.why", { decision_id: decision.id })).toEqual(decision);
		await second.call("skills.list");
		const document = (await store.read()) as {
			generation: number;
			changes: Array<{ name: string; from: string | null }>;
		};
		expect(document.generation).toBe(2);
		expect(document.changes.at(-1)).toMatchObject({ name: "review", from: sha(REVIEW) });
	});
});
