import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type JsonlSessionMetadata, JsonlSessionRepo, type Session } from "@ultron/agent-core";
import { NodeExecutionEnv } from "@ultron/agent-core/node";
import type { JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { afterEach, describe, expect, test } from "vitest";
import type { MemoryBackend } from "../src/ultron/memory.ts";
import { createProgressModule } from "../src/ultron/progress.ts";
import { createSessionDefinitionStore } from "../src/ultron/rlm/definition-registry.ts";
import { createSessionModuleStore } from "../src/ultron/rlm/host-module.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import { createSessionTaskStore } from "../src/ultron/rlm/task-store.ts";
import { createSessionUsageLedger } from "../src/ultron/usage.ts";
import { createWorkerServices } from "../src/ultron/worker-services.ts";

const ctx = BACKGROUND_CONTEXT;

/** External Hindsight stand-in shared by every session: documents survive session forks and deletes. */
function hindsight() {
	const documents = new Map<string, { id: string; original_text: string; tags: string[] }>();
	const operations = new Map<string, string>();
	return {
		documents,
		backendFor(sessionId: string): MemoryBackend {
			return {
				namespace: "fake://hindsight/a07",
				scopeTags: {
					session: [`ultron:session:${sessionId}`],
					project: ["ultron:project:a07"],
					global: ["ultron:global:a07"],
				},
				recall: async (request) => ({
					results: [...documents.values()]
						.filter(
							(document) =>
								document.tags.length === request.tags.length &&
								document.tags.every((tag) => request.tags.includes(tag)),
						)
						.map((document) => ({ id: document.id, text: document.original_text, tags: document.tags })),
				}),
				retain: async (request) => {
					const item = request.items[0];
					documents.set(item.document_id, { id: item.document_id, original_text: item.content, tags: item.tags });
					operations.set(request.operation_id, "completed");
					return { success: true, async: true, operation_id: request.operation_id };
				},
				operation: async (id) => ({ operation_id: id, status: operations.get(id) ?? "not_found" }),
				get: async (id) => documents.get(id),
				delete: async (id) => {
					documents.delete(id);
					return { success: true, document_id: id };
				},
			};
		},
	};
}

/** Model lanes that echo their prompt, so an injected refinement is observable in the prompt text. */
function echoHarness(prompts: string[]) {
	return {
		lane: async () => ({
			getActiveTools: async () => [],
			setModel: async () => {},
			abort: async () => ({ ok: true }),
			prompt: async (text: string) => {
				prompts.push(text);
				return { ok: true, value: { status: "completed", tipId: `tip-${prompts.length}`, fromTipId: null } };
			},
			findEntries: async () => [
				{
					id: `tip-${prompts.length}`,
					type: "message",
					message: { role: "assistant", content: [{ type: "text", text: `ran ${prompts.length}` }] },
				},
			],
		}),
	};
}

const step = {
	id: "count-step",
	version: "1",
	strategy: "deterministic",
	instructions: "Record one external step.",
	inputSchema: {},
	outputSchema: {},
	maxRepairs: 0,
	inputDescription: "Any JSON value",
	outputDescription: "Any JSON value",
};

type Harness = Awaited<ReturnType<typeof environment>>;

async function environment() {
	const root = mkdtempSync(join(tmpdir(), "ultron-a07-"));
	const fileSystem = new NodeExecutionEnv({ cwd: root });
	const repo = new JsonlSessionRepo({ fileSystem, sessionsRoot: join(root, "sessions") });
	const memory = hindsight();
	const effects: JsonValue[] = [];
	const prompts: string[] = [];
	const open = new Set<{ close(): Promise<void> }>();

	async function metadata(id: string): Promise<JsonlSessionMetadata> {
		const found = (await repo.list(undefined, ctx)).filter((candidate) => candidate.id === id);
		if (found.length !== 1) throw new Error(`expected one session ${id}`);
		return found[0]!;
	}

	/** Open a session and build the same Ultron stack the session worker builds over its values. */
	async function resume(id: string) {
		const session = await repo.open(await metadata(id), ctx);
		const services = createWorkerServices({
			session: session as Session,
			sessionId: id,
			cwd: root,
			jev: {
				triage: async () => ({}) as never,
				memoryGate: async () => ({ retrieve: true }) as never,
				memoryPolicy: async () => ({ action: "keep" }) as never,
			},
			backend: memory.backendFor(id),
		});
		const host = new NativeRlmHost(echoHarness(prompts) as never, {} as never, {
			store: createSessionTaskStore(session),
			definitionStore: createSessionDefinitionStore(session),
			usage: createSessionUsageLedger(session),
			services,
			deterministic: async ({ input }) => {
				effects.push(input);
				return input;
			},
			refinements: async (definitionId, context) => {
				const current = (await services.handle(
					"refinements.current",
					{ kind: "instruction", target: `instruction:${definitionId}` },
					context,
				)) as { id: string; version: number | null; content: unknown } | null;
				return current ? [{ id: current.id, version: current.version, text: String(current.content) }] : [];
			},
			modules: [createProgressModule({ store: createSessionModuleStore(session, "progress") })],
		});
		const handle = {
			call: <T = Record<string, unknown>>(type: string, payload: Record<string, unknown> = {}) =>
				host.handle(type, payload, ctx) as Promise<T>,
			async close() {
				open.delete(handle);
				await host.close();
				await session.close(ctx);
			},
			async runChild(prompt: string) {
				const task = await handle.call<{ id: string }>("agents.spawn", {
					definition: "rlm-child@1",
					input: { prompt },
				});
				expect(await handle.call("agents.result", { id: task.id })).toMatchObject({ status: "succeeded" });
				return { id: task.id, prompt: prompts.at(-1)! };
			},
			async learn(lesson: string) {
				const current = await handle.call<{ version: number } | null>("refinements.current", {
					kind: "instruction",
					target: "instruction:rlm-child",
				});
				const proposal = await handle.call<{ id: string }>("refinements.propose", {
					kind: "instruction",
					target: "instruction:rlm-child",
					baseVersion: current?.version ?? 0,
					content: lesson,
					evidence: [{ ref: `lesson:${lesson}` }],
				});
				await handle.call("refinements.activate", { id: proposal.id });
				return proposal.id;
			},
			async snapshot() {
				const status = await handle.call<{ tasks: unknown[]; usage: { usage: unknown } }>("agents.status");
				return {
					tasks: status.tasks,
					usage: status.usage.usage,
					refinements: await handle.call("refinements.list"),
					memory: await handle.call("memory.list"),
				};
			},
		};
		open.add(handle);
		return handle;
	}

	return {
		root,
		repo,
		memory,
		effects,
		prompts,
		metadata,
		resume,
		async create(id: string) {
			const session = await repo.create({ id, cwd: root }, ctx);
			await session.close(ctx);
			return resume(id);
		},
		async fork(source: string, id: string) {
			const forked = await repo.fork(await metadata(source), { scope: "tree", id }, ctx);
			await forked.close(ctx);
		},
		async cleanup() {
			for (const handle of [...open]) await handle.close().catch(() => {});
			await repo.close(ctx);
			await fileSystem.cleanup(ctx);
			rmSync(root, { recursive: true, force: true });
		},
	};
}

const environments: Harness[] = [];
afterEach(async () => {
	for (const item of environments.splice(0)) await item.cleanup();
});

async function setup() {
	const created = await environment();
	environments.push(created);
	return created;
}

describe("A07 branch/fork/resume restores matching state without abandoned lesson leaks", () => {
	test("resume after restart restores tasks, definitions, lessons, progress, memory and usage from real session values", async () => {
		const env = await setup();
		const first = await env.create("s-main");
		await first.call("agents.register", { definition: step });
		const done = await first.call("agents.invoke", { definition: "count-step@1", input: { n: 1 }, key: "step-1" });
		const lessonId = await first.learn("Always check empty inputs.");
		const child = await first.runChild("analyze");
		expect(child.prompt).toContain("Always check empty inputs.");
		await first.call("progress.report", {
			task_id: child.id,
			summary: "analysis written",
			evidence: [{ kind: "file", ref: "analysis.md" }],
		});
		const remembered = await first.call<{ memoryId: string }>("memory.propose", {
			text: "The launcher lives in bin/pi",
			evidence: [{ ref: `task:${child.id}` }],
		});
		const before = await first.snapshot();
		await first.close();

		const resumed = await env.resume("s-main");
		expect(await resumed.snapshot()).toEqual(before);
		// The durable task answers the same key with no re-execution; the registered definition was restored.
		expect(
			await resumed.call("agents.invoke", { definition: "count-step@1", input: { n: 1 }, key: "step-1" }),
		).toEqual(done);
		expect(env.effects).toEqual([{ n: 1 }]);
		expect(await resumed.call("progress.history", { task_id: child.id })).toMatchObject({
			receipts: [{ summary: "analysis written" }],
		});
		expect(
			await resumed.call("refinements.current", { kind: "instruction", target: "instruction:rlm-child" }),
		).toMatchObject({ id: lessonId, content: "Always check empty inputs." });
		expect((await resumed.runChild("again")).prompt).toContain("Always check empty inputs.");
		const recalled = await resumed.call<{ results: Array<{ id: string; text: string }> }>("memory.prepare", {
			query: "launcher",
			taskId: "after-resume",
		});
		expect(recalled.results).toEqual([
			expect.objectContaining({ id: remembered.memoryId, text: "The launcher lives in bin/pi" }),
		]);
		await resumed.close();
	});

	test("a fork starts from the source state, then lessons written on either side never leak to the other across restarts", async () => {
		const env = await setup();
		const source = await env.create("s-source");
		await source.call("agents.register", { definition: step });
		const shared = await source.call("agents.invoke", { definition: "count-step@1", input: { n: 1 }, key: "shared" });
		const sharedLesson = await source.learn("Shared lesson before the fork.");
		const atFork = await source.snapshot();
		await source.close();

		await env.fork("s-source", "s-fork");
		const fork = await env.resume("s-fork");
		// The fork carries the source state at the fork point, including its effect history.
		expect(await fork.snapshot()).toEqual(atFork);
		expect(await fork.call("agents.invoke", { definition: "count-step@1", input: { n: 1 }, key: "shared" })).toEqual(
			shared,
		);
		expect(env.effects).toEqual([{ n: 1 }]);

		// Work and lessons on the fork branch, which will later be abandoned.
		const forkLesson = await fork.learn("Fork-only lesson: skip validation.");
		const forkTask = await fork.runChild("fork work");
		expect(forkTask.prompt).toContain("Fork-only lesson");
		await fork.call("progress.report", {
			task_id: forkTask.id,
			summary: "fork progress",
			evidence: [{ kind: "file", ref: "fork.md" }],
		});
		await fork.call("memory.propose", { text: "Fork-only memory", evidence: [{ ref: "fork" }] });
		await fork.close();

		// Back on the source branch after a restart: nothing from the fork is visible or applied.
		const resumedSource = await env.resume("s-source");
		const sourceState = await resumedSource.snapshot();
		expect(sourceState).toEqual(atFork);
		expect(JSON.stringify(sourceState)).not.toContain(forkLesson);
		expect(JSON.stringify(sourceState)).not.toContain(forkTask.id);
		const sourceRun = await resumedSource.runChild("source work");
		expect(sourceRun.prompt).toContain("Shared lesson before the fork.");
		expect(sourceRun.prompt).not.toContain("Fork-only lesson");
		await expect(resumedSource.call("progress.history", { task_id: forkTask.id })).rejects.toThrow(
			"Unknown Ultron task",
		);
		const sourceRecall = await resumedSource.call<{ results: Array<{ text: string }> }>("memory.prepare", {
			query: "memory",
			taskId: "source-recall",
		});
		expect(sourceRecall.results.map((result) => result.text)).not.toContain("Fork-only memory");
		// A lesson learned on the source after the fork does not flow into the fork either.
		const sourceLesson = await resumedSource.learn("Source-only lesson after the fork.");
		await resumedSource.close();

		const resumedFork = await env.resume("s-fork");
		const forkState = await resumedFork.snapshot();
		expect(JSON.stringify(forkState)).not.toContain(sourceLesson);
		expect(JSON.stringify(forkState)).not.toContain(sourceRun.id);
		const forkRun = await resumedFork.runChild("fork again");
		expect(forkRun.prompt).toContain("Fork-only lesson");
		expect(forkRun.prompt).not.toContain("Source-only lesson");
		const forkRecall = await resumedFork.call<{ results: Array<{ text: string }> }>("memory.prepare", {
			query: "memory",
			taskId: "fork-recall",
		});
		expect(forkRecall.results.map((result) => result.text)).toContain("Fork-only memory");
		await resumedFork.close();

		// Abandon the fork entirely: the source keeps exactly its own state.
		await env.repo.delete(await env.metadata("s-fork"), BACKGROUND_CONTEXT);
		const finalSource = await env.resume("s-source");
		const refinements = (await finalSource.call("refinements.list")) as Array<{ id: string; content: string }>;
		expect(refinements.map((record) => record.id)).toEqual([sharedLesson, sourceLesson]);
		expect(
			await finalSource.call("refinements.current", { kind: "instruction", target: "instruction:rlm-child" }),
		).toMatchObject({ id: sourceLesson });
		await finalSource.close();
	});

	test("rolling back a lesson on one branch before a restart does not resurrect it, and the other branch keeps its own", async () => {
		const env = await setup();
		const source = await env.create("s-rollback");
		const base = await source.learn("Base lesson.");
		await source.close();
		await env.fork("s-rollback", "s-rollback-fork");

		const fork = await env.resume("s-rollback-fork");
		const bad = await fork.learn("Harmful lesson.");
		await fork.call("refinements.rollback", { id: bad });
		await fork.close();

		const resumedFork = await env.resume("s-rollback-fork");
		const forkRun = await resumedFork.runChild("after rollback");
		expect(forkRun.prompt).not.toContain("Harmful lesson.");
		expect(forkRun.prompt).toContain("Base lesson.");
		await resumedFork.close();

		const resumedSource = await env.resume("s-rollback");
		expect(
			await resumedSource.call("refinements.current", { kind: "instruction", target: "instruction:rlm-child" }),
		).toMatchObject({ id: base });
		expect(JSON.stringify(await resumedSource.call("refinements.list"))).not.toContain(bad);
		await resumedSource.close();
	});
});
