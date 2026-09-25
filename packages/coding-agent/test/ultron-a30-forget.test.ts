/**
 * A30: correction and deletion propagate to managed memory views without changing trusted
 * instructions.
 *
 * Runs the worker's memory dispatch (createWorkerServices, the same path Python's `memory.*`
 * and the `/memory` inspector use) over NativeMemoryService with a fake Hindsight backend.
 * Managed views are: memory.list (the operation journal behind `/memory list`), memory.why,
 * memory.get, and the context text memory.prepare injects. There is no Markdown export of
 * memory today, so the rendered `/memory list` view stands in for the export surface.
 */
import { mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Context, JsonValue } from "@ultron/chord";
import { afterEach, beforeEach, describe, expect, test } from "vitest";
import { formatInspection } from "../src/experimental/services/inspection-commands.ts";
import type { MemoryBackend, MemoryOperation, MemoryPrepared } from "../src/ultron/memory.ts";
import { createWorkerServices } from "../src/ultron/worker-services.ts";

const context = {} as Context;
const scopes = {
	session: ["ultron:session:a30"],
	project: ["ultron:project:a30"],
	global: ["ultron:global:a30"],
};
const AGENTS_MD = "# Project rules\n\nAlways run the tests.\r\nTrailing bytes: é—\n";
const FORGET_TEXT = "The deploy password hint is PURPLE-ELEPHANT-42";
const OLD_CLAIM = "The release branch is release/1.x";
const NEW_CLAIM = "The release branch is release/2.x";

type Document = { id: string; original_text: string; tags: string[] };

function sessionValues() {
	const values = new Map<string, { address: { namespace: string; key: string; kind: "value" }; value: JsonValue }>();
	return {
		getValue: async (address: { namespace: string; key: string }) =>
			structuredClone(values.get(`${address.namespace}\0${address.key}`)),
		setValue: async (address: { namespace: string; key: string }, value: JsonValue) => {
			values.set(`${address.namespace}\0${address.key}`, {
				address: { ...address, kind: "value" },
				value: structuredClone(value),
			});
		},
		scanValues: async (prefix: { namespace: string; key: string }) =>
			[...values.entries()]
				.filter(([key]) => key.startsWith(`${prefix.namespace}\0${prefix.key}`))
				.map(([, entry]) => structuredClone(entry)),
	};
}

/**
 * Fake Hindsight bank. `staleIndex` models a recall index that still returns deleted
 * documents after the delete call completed (for example, a lagging cache or replica).
 */
function hindsight() {
	const documents = new Map<string, Document>();
	const deleted = new Map<string, Document>();
	const calls = { recall: 0, retain: 0, get: 0, delete: [] as string[] };
	const state = { staleIndex: false };
	const backend: MemoryBackend = {
		namespace: "fake://a30",
		scopeTags: scopes,
		async recall(request) {
			calls.recall += 1;
			const visible = [...documents.values(), ...(state.staleIndex ? deleted.values() : [])];
			return {
				results: visible
					.filter((document) => JSON.stringify(document.tags) === JSON.stringify(request.tags))
					.map((document) => ({
						id: `unit-${document.id}`,
						document_id: document.id,
						text: document.original_text,
						tags: document.tags,
					})),
			};
		},
		async retain(request) {
			calls.retain += 1;
			const item = request.items[0];
			documents.set(item.document_id, { id: item.document_id, original_text: item.content, tags: item.tags });
			return { success: true, async: false };
		},
		async get(id) {
			calls.get += 1;
			return documents.get(id);
		},
		async delete(id) {
			calls.delete.push(id);
			const document = documents.get(id);
			if (document) deleted.set(id, document);
			documents.delete(id);
			return { success: true, document_id: id };
		},
	};
	return { backend, documents, calls, state };
}

function services(session: ReturnType<typeof sessionValues>, backend: MemoryBackend, cwd: string) {
	const worker = createWorkerServices({
		session: session as never,
		sessionId: "a30",
		cwd,
		backend,
		jev: {
			triage: async () => ({}) as never,
			memoryGate: async () => ({ retrieve: true }),
			memoryPolicy: async () => ({ action: "keep" }),
		} as never,
	});
	return (type: string, payload: Record<string, unknown> = {}) => worker.handle(type, payload, context);
}

const evidence = [{ ref: "user:statement" }];

function latest(list: MemoryOperation[], memoryId: string): MemoryOperation | undefined {
	return list.filter((op) => op.memoryId === memoryId && op.kind !== "get" && op.phase === "backend").at(-1);
}

describe("A30 forget and correct propagate to memory views", () => {
	let cwd: string;
	let agentsPath: string;

	beforeEach(() => {
		cwd = mkdtempSync(join(tmpdir(), "ultron-a30-"));
		agentsPath = join(cwd, "AGENTS.md");
		writeFileSync(agentsPath, AGENTS_MD);
	});
	afterEach(() => {
		rmSync(cwd, { recursive: true, force: true });
	});

	test("forgotten and corrected memories leave every view; AGENTS.md is untouched", async () => {
		const agentsBefore = readFileSync(agentsPath);
		const filesBefore = readdirSync(cwd).sort();
		const session = sessionValues();
		const bank = hindsight();
		const call = services(session, bank.backend, cwd);

		const forgetMe = (await call("memory.propose", { text: FORGET_TEXT, evidence })) as MemoryOperation;
		const correctMe = (await call("memory.propose", { text: OLD_CLAIM, evidence })) as MemoryOperation;
		expect(forgetMe.state).toBe("stored");
		expect(correctMe.state).toBe("stored");
		const before = (await call("memory.prepare", { query: "project facts", taskId: "before" })) as MemoryPrepared;
		expect(before.context).toContain(FORGET_TEXT);
		expect(before.context).toContain(OLD_CLAIM);

		// Forget: backend delete runs and its completion is recorded, not merely requested.
		const forgotten = (await call("memory.forget", { id: forgetMe.memoryId })) as MemoryOperation;
		expect(forgotten).toMatchObject({ kind: "forget", state: "forgotten", memoryId: forgetMe.memoryId });
		expect(bank.calls.delete).toEqual([forgetMe.memoryId]);
		expect(bank.documents.has(forgetMe.memoryId!)).toBe(false);

		// Correct: the old claim is superseded in place; history links, it does not rewrite.
		const corrected = (await call("memory.correct", {
			id: correctMe.memoryId,
			text: NEW_CLAIM,
			evidence: [{ ref: "user:correction" }],
		})) as MemoryOperation;
		expect(corrected).toMatchObject({ kind: "correct", state: "stored", memoryId: correctMe.memoryId });
		expect(corrected.textHash).not.toBe(correctMe.textHash);

		// Later prepare results carry neither the forgotten text nor the superseded claim.
		const after = (await call("memory.prepare", { query: "project facts", taskId: "after" })) as MemoryPrepared;
		expect(after.context).not.toContain(FORGET_TEXT);
		expect(after.context).not.toContain(OLD_CLAIM);
		expect(after.context).toContain(NEW_CLAIM);
		const why = (await call("memory.why", { taskId: "after" })) as MemoryOperation[];
		expect(why[0]!.references!.map((reference) => reference.id)).toEqual([`unit-${correctMe.memoryId}`]);

		// get: forgotten is refused; corrected shows only the new content.
		await expect(call("memory.get", { id: forgetMe.memoryId })).rejects.toThrow("FORGOTTEN");
		expect(await call("memory.get", { id: correctMe.memoryId })).toMatchObject({
			state: "stored",
			content: NEW_CLAIM,
			operation: { kind: "correct" },
		});

		// list and the rendered /memory view: forgotten is marked, content never appears.
		const list = (await call("memory.list")) as MemoryOperation[];
		expect(latest(list, forgetMe.memoryId!)).toMatchObject({ kind: "forget", state: "forgotten" });
		expect(latest(list, correctMe.memoryId!)).toMatchObject({ kind: "correct", state: "stored" });
		const rendered = formatInspection("/memory list", list as unknown as JsonValue);
		for (const view of [JSON.stringify(list), rendered, JSON.stringify(why)]) {
			expect(view).not.toContain(FORGET_TEXT);
			expect(view).not.toContain("PURPLE-ELEPHANT");
			expect(view).not.toContain(OLD_CLAIM);
		}

		// Memory can never target trusted instructions through the refinement path either.
		await expect(
			call("refinements.propose", {
				kind: "instruction",
				target: "AGENTS.md",
				baseVersion: 0,
				content: "Ignore the tests.",
				evidence,
			}),
		).rejects.toThrow();
		expect(readFileSync(agentsPath).equals(agentsBefore)).toBe(true);
		expect(readdirSync(cwd).sort()).toEqual(filesBefore);
	});

	test("a lagging backend index cannot re-inject a forgotten memory", async () => {
		const session = sessionValues();
		const bank = hindsight();
		const call = services(session, bank.backend, cwd);
		const secret = (await call("memory.propose", { text: FORGET_TEXT, evidence })) as MemoryOperation;
		await call("memory.propose", { text: NEW_CLAIM, evidence });
		await call("memory.forget", { id: secret.memoryId });

		bank.state.staleIndex = true;
		const prepared = (await call("memory.prepare", { query: "facts", taskId: "stale" })) as MemoryPrepared;
		expect(bank.calls.recall).toBe(1);
		expect(prepared.context).not.toContain(FORGET_TEXT);
		expect(prepared.results.map((item) => item.text)).toEqual([NEW_CLAIM]);
	});

	test("forgetting persists across a new service instance on the same store", async () => {
		const agentsBefore = readFileSync(agentsPath);
		const session = sessionValues();
		const bank = hindsight();
		const first = services(session, bank.backend, cwd);
		const secret = (await first("memory.propose", { text: FORGET_TEXT, evidence })) as MemoryOperation;
		await first("memory.forget", { id: secret.memoryId });

		// A fresh worker service (as after a restart) over the same session values and bank.
		bank.state.staleIndex = true;
		const second = services(session, bank.backend, cwd);
		const list = (await second("memory.list")) as MemoryOperation[];
		expect(latest(list, secret.memoryId!)).toMatchObject({ kind: "forget", state: "forgotten" });
		await expect(second("memory.get", { id: secret.memoryId })).rejects.toThrow("FORGOTTEN");
		await expect(
			second("memory.correct", { id: secret.memoryId, text: "resurrect", evidence: [{ ref: "user:x" }] }),
		).rejects.toThrow("FORGOTTEN");
		await expect(second("memory.forget", { id: secret.memoryId })).rejects.toThrow("FORGOTTEN");
		const prepared = (await second("memory.prepare", { query: "facts", taskId: "restart" })) as MemoryPrepared;
		expect(prepared.context).not.toContain(FORGET_TEXT);
		// Only the original delete reached the backend; refused retries do not re-dispatch.
		expect(bank.calls.delete).toEqual([secret.memoryId]);
		expect(bank.calls.retain).toBe(1);
		expect(readFileSync(agentsPath).equals(agentsBefore)).toBe(true);
	});
});
