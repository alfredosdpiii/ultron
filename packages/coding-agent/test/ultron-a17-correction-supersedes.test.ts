import { createHash } from "node:crypto";
import { describe, expect, test } from "vitest";
import { MemoryError } from "../src/ultron/memory.ts";
import { durableStore, FakeHindsight, open, scopes } from "./ultron-fake-hindsight.ts";

const sha256 = (text: string) => createHash("sha256").update(text).digest("hex");
const STALE = "The Pi launcher is /usr/local/bin/pi.";
const CORRECTED = "The Pi launcher is ~/.local/bin/pi (a wrapper script), not /usr/local/bin/pi.";
const NODE = "The project pins Node 22.";

describe("A17 correction supersedes stale claims; hypotheses remain distinguishable", () => {
	test("a corrected Pi launcher path is current and the stale claim superseded in later fresh sessions", async () => {
		const hindsight = new FakeHindsight();
		const store = durableStore(); // one durable journal, reopened by each fresh session

		// Session 1: the assistant guesses the launcher path; a tool observes the Node version.
		const first = open(hindsight, store);
		const guess = await first.propose({
			text: STALE,
			evidence: [{ ref: "assistant:session-1:turn-3" }],
			scope: "project",
		});
		const observed = await first.propose({
			text: NODE,
			evidence: [{ ref: "tool:node --version", sha256: sha256("v22.4.0") }],
			scope: "project",
			evidenceClass: "tool_evidence",
		});
		// The gate kept it, but a guess stays an unconfirmed hypothesis.
		expect(guess).toMatchObject({ evidenceClass: "hypothesis", gate: { action: "keep" } });
		expect(observed).toMatchObject({ evidenceClass: "tool_evidence" });
		await first.get(guess.memoryId!);
		await first.get(observed.memoryId!);

		// Session 2 (fresh instance): the hypothesis is recalled but visibly marked as unconfirmed.
		const second = open(hindsight, store);
		const before = await second.prepare({ query: "where is the Pi launcher", scope: "project", taskId: "task-2" });
		expect(before.results.map((item) => [item.memoryId, item.evidenceClass])).toEqual([
			[guess.memoryId, "hypothesis"],
			[observed.memoryId, "tool_evidence"],
		]);
		expect(before.context).toContain(`[unconfirmed hypothesis] ${STALE}`);
		expect(before.context).toContain(`[tool evidence] ${NODE}`);

		// The user corrects it. A user correction is a user statement, not verified tool evidence.
		const correction = await second.correct(guess.memoryId!, {
			text: CORRECTED,
			evidence: [{ ref: "user:session-2:message-7" }],
		});
		expect(correction).toMatchObject({
			kind: "correct",
			memoryId: guess.memoryId,
			evidenceClass: "user_statement",
			supersedes: guess.id,
			state: "accepted",
		});
		await expect(second.get(guess.memoryId!)).resolves.toMatchObject({ state: "stored", content: CORRECTED });

		// The backend index still returns the replaced unit (stale derived data). It must not win.
		hindsight.stale.push({
			id: "stale-unit",
			text: STALE,
			tags: scopes.project,
			type: "world",
			document_id: guess.memoryId!,
			metadata: { ultron_operation: guess.id, ultron_evidence_class: "hypothesis" },
		});

		// Session 3 (fresh instance): prepare, why and list all present the correction as current.
		const third = open(hindsight, store);
		const after = await third.prepare({ query: "where is the Pi launcher", scope: "project", taskId: "task-3" });
		expect(after.results.map((item) => [item.memoryId, item.evidenceClass, item.text])).toEqual([
			[guess.memoryId, "user_statement", CORRECTED],
			[observed.memoryId, "tool_evidence", NODE],
		]);
		expect(after.context).toContain(`[user statement] ${CORRECTED}`);
		expect(after.context.split("\n").some((line) => line.endsWith(STALE))).toBe(false);

		const [why] = await third.why("task-3");
		expect(why.references).toEqual([
			{
				id: expect.any(String),
				textHash: sha256(CORRECTED),
				memoryId: guess.memoryId,
				evidenceClass: "user_statement",
			},
			{
				id: expect.any(String),
				textHash: sha256(NODE),
				memoryId: observed.memoryId,
				evidenceClass: "tool_evidence",
			},
		]);
		expect(why.excluded).toEqual([{ id: "stale-unit", memoryId: guess.memoryId, reason: "superseded" }]);

		const list = await third.list();
		const claims = list.filter((op) => op.memoryId === guess.memoryId && op.phase === "backend");
		const proposeOp = claims.find((op) => op.kind === "propose")!;
		const correctOp = claims.find((op) => op.kind === "correct")!;
		// History is linked, not rewritten: the stale claim keeps its own hash and class.
		expect(proposeOp).toMatchObject({
			evidenceClass: "hypothesis",
			textHash: sha256(STALE),
			state: "stored",
			supersededBy: correctOp.id,
		});
		expect(correctOp).toMatchObject({
			evidenceClass: "user_statement",
			textHash: sha256(CORRECTED),
			supersedes: proposeOp.id,
			state: "stored",
		});
		expect(correctOp.supersededBy).toBeUndefined();
		expect(list.find((op) => op.id === observed.id)!.supersededBy).toBeUndefined();

		// Derived supersession is never written into the durable journal.
		expect(JSON.stringify(store.value)).not.toContain("supersededBy");
		expect(JSON.stringify(store.value)).not.toContain("launcher");
	});

	test("hypotheses, user statements, tool evidence and verified conclusions stay distinct and validated", async () => {
		const hindsight = new FakeHindsight();
		const store = durableStore();
		const memory = open(hindsight, store);
		const classes = ["hypothesis", "user_statement", "tool_evidence", "verified"] as const;
		for (const evidenceClass of classes)
			await memory.propose({
				text: `claim ${evidenceClass}`,
				evidence: [{ ref: `e:${evidenceClass}` }],
				evidenceClass,
			});
		const unmarked = await memory.propose({ text: "claim unmarked", evidence: [{ ref: "e:none" }] });
		expect(unmarked.evidenceClass).toBe("hypothesis");

		// The class travels with the write, so any reader of the backend can tell them apart.
		expect(hindsight.retains().map((call) => (call.body!.items as { metadata: object }[])[0].metadata)).toEqual(
			[...classes, "hypothesis"].map((evidenceClass) => ({
				ultron_operation: expect.any(String),
				ultron_evidence_class: evidenceClass,
			})),
		);
		const prepared = await open(hindsight, store).prepare({ query: "claims", taskId: "task" });
		const labels = prepared.context.split("\n").slice(1);
		expect(labels).toEqual([
			"1. [unconfirmed hypothesis] claim hypothesis",
			"2. [user statement] claim user_statement",
			"3. [tool evidence] claim tool_evidence",
			"4. [verified conclusion] claim verified",
			"5. [unconfirmed hypothesis] claim unmarked",
		]);
		await expect(
			memory.propose({ text: "x", evidence: [{ ref: "e" }], evidenceClass: "confirmed" as never }),
		).rejects.toSatisfy((error: unknown) => error instanceof MemoryError && error.code === "INVALID_INPUT");

		// A journal claiming a class on a non-claim operation, or a supersession link to another
		// document, is rejected on reopen rather than trusted.
		const journal = structuredClone(store.value) as { operations: Record<string, unknown>[] };
		const first = journal.operations.find((op) => op.kind === "propose" && op.phase === "backend")!;
		const target = journal.operations.find((op) => op.id === unmarked.id)!;
		const withCorrection = (supersedes: unknown) => {
			const reopened = durableStore();
			reopened.value = {
				...journal,
				operations: [
					...journal.operations,
					{ ...target, id: "added-correction", kind: "correct", supersedes, updatedAt: target.updatedAt },
				],
			} as never;
			return open(hindsight, reopened).list();
		};
		// Control: a link to the same document's earlier claim loads.
		await expect(withCorrection(target.id)).resolves.toBeDefined();
		// A link to another document's claim is a forgery.
		await expect(withCorrection(first.id)).rejects.toSatisfy(
			(error: unknown) => error instanceof MemoryError && error.code === "INVALID_JOURNAL",
		);
	});
});
