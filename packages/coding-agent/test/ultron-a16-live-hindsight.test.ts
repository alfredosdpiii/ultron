/**
 * A16 against a live Hindsight: scopes survive Hindsight's own consolidation.
 *
 * Opt-in (ULTRON_LIVE_HINDSIGHT=1; server from ULTRON_LIVE_HINDSIGHT_URL, default http://localhost:8888).
 * Uses a disposable bank that is always deleted. Consolidation runs Hindsight's configured LLM.
 */
import { randomUUID } from "node:crypto";
import { mkdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { describe, expect, test } from "vitest";
import { createHindsightBackend, type JsonValue, NativeMemoryService } from "../src/ultron/memory.ts";

const LIVE = process.env.ULTRON_LIVE_HINDSIGHT === "1";
const BASE = process.env.ULTRON_LIVE_HINDSIGHT_URL ?? "http://localhost:8888";
const RESULTS_DIR = resolve(__dirname, "../../../acceptance/demonstrations");
const scopes = {
	session: ["ultron:session:a16-live"],
	project: ["ultron:project:a16-live"],
	global: ["ultron:global:a16-live"],
};

async function until<T>(read: () => Promise<T | undefined>, timeoutMs: number): Promise<T | undefined> {
	const deadline = Date.now() + timeoutMs;
	while (Date.now() < deadline) {
		const value = await read();
		if (value !== undefined) return value;
		await new Promise((resolve) => setTimeout(resolve, 3000));
	}
	return undefined;
}

describe.skipIf(!LIVE)("A16 live Hindsight consolidation", () => {
	test(
		"recall after a real consolidation returns only items carrying exactly the requested scope's tags",
		async () => {
			const bank = `ultron-a16-live-${randomUUID().slice(0, 8)}`;
			const bankUrl = `${BASE}/v1/default/banks/${bank}`;
			await fetch(bankUrl, { method: "PUT", headers: { "content-type": "application/json" }, body: "{}" });
			const record: Record<string, unknown> = {
				model: `hindsight@${BASE}`,
				bank,
				recordedAt: new Date().toISOString(),
			};
			try {
				let stored: JsonValue | undefined;
				const memory = new NativeMemoryService({
					backend: createHindsightBackend({ baseUrl: BASE, bankId: bank, scopeTags: scopes, timeoutMs: 120_000 }),
					gate: async (request) => (request.action === "recall" ? { retrieve: true } : { action: "keep" }),
					store: {
						read: async () => structuredClone(stored),
						write: async (next) => {
							stored = structuredClone(next);
						},
					},
				});
				const sessionFact = await memory.propose({
					text: "In this repository the package manager is pnpm.",
					evidence: [{ ref: "task:1" }],
				});
				const projectFact = await memory.propose({
					text: "The project's tests live in the test/ directory and run with vitest.",
					evidence: [{ ref: "task:2" }],
					scope: "project",
				});
				for (const fact of [sessionFact, projectFact]) {
					const document = await until(async () => {
						const current = await memory.get(fact.memoryId!);
						return current.state === "stored" ? current : undefined;
					}, 180_000);
					expect(document, `retention of ${fact.memoryId} completed`).toBeDefined();
				}
				await memory.correct(sessionFact.memoryId!, {
					text: "Correction: in this repository the package manager is npm, not pnpm.",
					evidence: [{ ref: "user:3" }],
				});
				await until(async () => {
					const current = await memory.get(sessionFact.memoryId!);
					return current.state === "stored" ? current : undefined;
				}, 180_000);

				const consolidation = await fetch(`${bankUrl}/consolidate`, { method: "POST" });
				record.consolidateStatus = consolidation.status;
				// Poll recall until Hindsight has produced observations for both scopes.
				let session = await memory.prepare({ query: "package manager and tests", taskId: "live-a", refresh: true });
				let project = await memory.prepare({
					query: "package manager and tests",
					taskId: "live-b",
					scope: "project",
					refresh: true,
				});
				await until(async () => {
					session = await memory.prepare({ query: "package manager and tests", taskId: "live-a", refresh: true });
					project = await memory.prepare({
						query: "package manager and tests",
						taskId: "live-b",
						scope: "project",
						refresh: true,
					});
					return session.results.some((item) => item.type === "observation") &&
						project.results.some((item) => item.type === "observation")
						? true
						: undefined;
				}, 240_000);
				record.sessionResults = session.results.map((item) => ({
					type: item.type,
					tags: item.tags,
					text: item.text,
				}));
				record.projectResults = project.results.map((item) => ({
					type: item.type,
					tags: item.tags,
					text: item.text,
				}));

				// The service rejects any recalled item whose tags differ from the scope's (INVALID_RESPONSE), so
				// reaching here already means every result, observations included, carried exactly its scope's tags.
				for (const item of session.results) expect(item.tags).toEqual(scopes.session);
				for (const item of project.results) expect(item.tags).toEqual(scopes.project);
				expect(session.results.some((item) => item.type === "observation")).toBe(true);
				expect(project.results.some((item) => item.type === "observation")).toBe(true);
				expect(session.context).not.toContain("vitest");
				expect(project.context).not.toMatch(/pnpm|npm/);
				record.passed = true;
			} catch (error) {
				record.passed = false;
				record.error = error instanceof Error ? error.message : String(error);
				throw error;
			} finally {
				await fetch(bankUrl, { method: "DELETE" }).catch(() => {});
				mkdirSync(RESULTS_DIR, { recursive: true });
				writeFileSync(join(RESULTS_DIR, "a16-live-hindsight.json"), `${JSON.stringify(record, null, 2)}\n`);
			}
		},
		15 * 60 * 1000,
	);
});
