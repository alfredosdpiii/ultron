import type { JsonValue } from "@earendil-works/chord";
import { afterEach, describe, expect, test } from "vitest";
import { checkGrant, createGrantModule } from "../src/ultron/grants.ts";
import { createProgressModule } from "../src/ultron/progress.ts";
import { createReleaseGateModule } from "../src/ultron/release-gate.ts";
import { createMemoryModuleStore } from "../src/ultron/rlm/host-module.ts";
import { createScheduleModule } from "../src/ultron/schedules.ts";
import { aborted, definition, hostFixture, journal, memoryStore } from "./ultron-host-fixtures.ts";

/**
 * A14: no checks, an incomplete or malformed review, or a stale receipt can never imply verified
 * completion. Every task result is "unverified"; only an explicit, host-run passing check can move
 * a progress claim or goal to verified/achieved, and the positive controls prove those paths exist.
 */

const fixtures: Array<ReturnType<typeof hostFixture>> = [];
afterEach(async () => {
	for (const fixture of fixtures.splice(0)) await fixture.host.close();
});

const FIXTURE_A = "a".repeat(64);
const FIXTURE_B = "b".repeat(64);
const REV_A = "a".repeat(40);
const REV_B = "b".repeat(40);
const anySchema = { inputSchema: {}, outputSchema: {} };

/** Reviewer lanes answer with the JSON text given in their request. */
function reviewerScript(_lane: string, prompt: string, laneContext: never): Promise<string> | string {
	const request = (JSON.parse(/Input data:\n(.*)\n/.exec(prompt)![1]) as { request: string }).request;
	if (request === "hang") return aborted(laneContext);
	return request;
}

async function verificationFixture() {
	const fixture = hostFixture({
		script: reviewerScript as never,
		// Checks and verifiers echo `input.verdict`, or crash on `input.fail`.
		deterministic: async ({ input }) => {
			const value = input as Record<string, JsonValue>;
			if (value.fail) throw new Error("check crashed");
			return value.verdict ?? value;
		},
		predict: async ({ input }) => input,
		modules: [
			createProgressModule({ store: createMemoryModuleStore() }),
			createScheduleModule({ store: createMemoryModuleStore(), tickIntervalMs: 0 }),
			createReleaseGateModule({ store: createMemoryModuleStore() }),
			createGrantModule({ store: createMemoryModuleStore() }),
		],
	});
	fixtures.push(fixture);
	await fixture.call("agents.register", { definition: definition("check", "deterministic", anySchema) });
	await fixture.call("agents.register", { definition: definition("guess", "predict", anySchema) });
	return fixture;
}

const review = (request: string) => ({ definition: "security-reviewer@1", input: { request } });

describe("A14 no checks, incomplete review, or stale receipt implies verified completion", () => {
	test("every task result path is unverified, and a stored 'verified' result is refused", async () => {
		const fixture = await verificationFixture();
		const results = [
			await fixture.call("agents.invoke", { definition: "check@1", input: { verdict: { passed: true } } }),
			await fixture.call("agents.invoke", { definition: "check@1", input: { fail: true } }),
			await fixture.call("agents.invoke", { definition: "guess@1", input: { passed: true } }),
			await fixture.call("agents.invoke", review('{"outcome":"no_findings","findings":[]}')),
			await fixture.call("agents.invoke", review("not json")),
			await fixture.call("agents.invoke", { ...review("hang"), timeout_ms: 5 }),
			await fixture.call("agents.invoke", { ...review('{"outcome":"no_findings","findings":[]}'), key: "k" }),
			await fixture.call("agents.invoke", { ...review('{"outcome":"no_findings","findings":[]}'), key: "k" }),
		] as Array<{ status: string; verification: string }>;
		expect(results.map((result) => result.status)).toEqual([
			"succeeded",
			"failed",
			"succeeded",
			"succeeded",
			"failed",
			"cancelled",
			"succeeded",
			"succeeded",
		]);
		for (const result of results) expect(result.verification).toBe("unverified");
		for (const task of await journal(fixture)) expect(task.result?.verification).toBe("unverified");

		// An owner that ended mid-task recovers the task as interrupted, still unverified.
		const store = memoryStore();
		const running = {
			id: "ultron-task-crashed",
			key: "crashed",
			fingerprint: "c".repeat(64),
			definition: "rlm-child@1",
			state: "running",
		};
		await store.write({ version: 1, tasks: [running] });
		const reopened = hostFixture({ store });
		fixtures.push(reopened);
		expect(await reopened.call("agents.result", { id: running.id })).toEqual({
			status: "interrupted",
			error: "Owner ended; automatic replay is disabled",
			verification: "unverified",
		});
		// A journal claiming verified completion is rejected outright.
		const forged = memoryStore();
		await forged.write({
			version: 1,
			tasks: [
				{
					...running,
					state: "completed",
					result: { status: "succeeded", value: "done", verification: "verified" },
				},
			],
		});
		const refused = hostFixture({ store: forged });
		fixtures.push(refused);
		await expect(refused.call("agents.tasks")).rejects.toThrow("Invalid task document");
	});

	test("reviewer output missing required fields fails; an incomplete review verifies nothing", async () => {
		const fixture = await verificationFixture();
		for (const malformed of [
			'{"outcome":"no_findings"}',
			'{"findings":[]}',
			'{"outcome":"findings","findings":[{"file":"a.ts","severity":"high","explanation":"x"}]}',
			'{"outcome":"clean","findings":[]}',
		])
			expect(await fixture.call("agents.invoke", review(malformed))).toEqual({
				status: "failed",
				error: "security-reviewer@1 output does not match its schema",
				verification: "unverified",
			});

		const incomplete = await fixture.call<{ id: string }>(
			"agents.spawn",
			review('{"outcome":"incomplete","findings":[]}'),
		);
		expect(await fixture.call("agents.result", { id: incomplete.id })).toEqual({
			status: "succeeded",
			value: { outcome: "incomplete", findings: [] },
			verification: "unverified",
		});
		// Claiming the incomplete review complete, verified by another reviewer, stays unverified:
		// reviewer output carries no boolean `passed`.
		const decision = await fixture.call<{ claim: { decision: string; reason: string } }>("progress.reassess", {
			task_id: incomplete.id,
			claim: "complete",
			verifier: "correctness-reviewer@1",
			verifier_input: { request: '{"outcome":"no_findings","findings":[]}' },
		});
		expect(decision.claim).toMatchObject({
			decision: "unverified",
			reason: "Verifier output has no boolean passed field",
		});
		// A failed review cannot even be claimed complete.
		const failed = await fixture.call<{ id: string }>("agents.spawn", review('{"outcome":"no_findings"}'));
		await fixture.call("agents.result", { id: failed.id });
		expect(
			(
				await fixture.call<{ claim: { decision: string } }>("progress.reassess", {
					task_id: failed.id,
					claim: "complete",
				})
			).claim.decision,
		).toBe("rejected");
	});

	test("a progress completion claim is verified only by a host-run verifier that returns passed: true", async () => {
		const fixture = await verificationFixture();
		const task = await fixture.call<{ id: string }>("agents.spawn", { definition: "check@1", input: { work: 1 } });
		await fixture.call("agents.result", { id: task.id });
		const claim = async (extra: Record<string, unknown>) =>
			(
				await fixture.call<{ claim: { decision: string; reason: string } }>("progress.reassess", {
					task_id: task.id,
					claim: "complete",
					...extra,
				})
			).claim;
		expect(await claim({})).toMatchObject({
			decision: "unverified",
			reason: "No verifier was named; a task result alone is not verification",
		});
		expect(await claim({ verifier: "check@1", verifier_input: { verdict: { passed: false } } })).toMatchObject({
			decision: "unverified",
		});
		expect(await claim({ verifier: "check@1", verifier_input: { verdict: { passed: "yes" } } })).toMatchObject({
			decision: "unverified",
		});
		expect(await claim({ verifier: "check@1", verifier_input: { fail: true } })).toMatchObject({
			decision: "unverified",
		});
		expect(await claim({ verifier: "missing@1" })).toMatchObject({ decision: "unverified" });
		// Positive control: the verified path exists and needs an explicit pass.
		expect(await claim({ verifier: "check@1", verifier_input: { verdict: { passed: true } } })).toMatchObject({
			decision: "verified",
		});
	});

	test("a goal with no required checks, or a check without an explicit pass, is never achieved", async () => {
		const fixture = await verificationFixture();
		const empty = await fixture.call<{ id: string }>("goals.create", { title: "no checks", required_checks: [] });
		const unchecked = await fixture.call<{ state: string; verification: { status: string; reasons: string[] } }>(
			"goals.verify",
			{ id: empty.id },
		);
		expect(unchecked.state).toBe("active");
		expect(unchecked.verification).toMatchObject({ status: "unverified", reasons: ["Goal has no required checks"] });

		const goal = await fixture.call<{ id: string }>("goals.create", {
			title: "checked",
			required_checks: ["check@1"],
		});
		for (const input of [{ verdict: { passed: "yes" } }, { verdict: { passed: false } }, { fail: true }]) {
			const result = await fixture.call<{ state: string; verification: { status: string } }>("goals.verify", {
				id: goal.id,
				input,
			});
			expect(result).toMatchObject({ state: "active", verification: { status: "unachieved" } });
		}
		expect(await fixture.call("goals.verify", { id: goal.id, input: { verdict: { passed: true } } })).toMatchObject({
			state: "achieved",
			verification: { status: "achieved" },
		});
	});

	test("a release gate blocks on a missing required check or a stale fixture", async () => {
		const fixture = await verificationFixture();
		await fixture.call("gates.define", {
			id: "release",
			fixture_hash: FIXTURE_A,
			checks: [
				{ name: "unit", required: true },
				{ name: "lint", required: false },
			],
		});
		const run = (variant: string, results: Record<string, string>, fixtureHash = FIXTURE_A) => ({
			variant,
			fixture_hash: fixtureHash,
			results,
		});
		const compare = (candidate: unknown) =>
			fixture.call<{ decision: string; reasons: string[] }>("gates.compare", {
				gate_id: "release",
				baseline: run("main", { unit: "passed", lint: "passed" }),
				candidate,
			});
		const missing = await compare(run("branch", { lint: "passed" }));
		expect(missing.decision).toBe("blocked");
		expect(missing.reasons).toContain("Required check unit regressed: passed in baseline, missing in candidate");
		const stale = await compare(run("branch", { unit: "passed", lint: "passed" }, FIXTURE_B));
		expect(stale.decision).toBe("blocked");
		expect(stale.reasons[0]).toMatch(/^Fixture mismatch: candidate branch ran against/);
		expect((await compare(run("branch", { unit: "passed" }))).decision).toBe("passed");
	});

	test("a receipt for a stale revision is denied", async () => {
		const fixture = await verificationFixture();
		const grant = await fixture.call<{ id: string }>("grants.issue", {
			scope: "repo:/work",
			revision: REV_A,
			policy: "apply-patch@1",
			action: "apply",
			ttl_ms: 60_000,
		});
		const check = (revision: string) =>
			fixture.call<{ allowed: boolean; reasons: string[] }>("grants.check", {
				id: grant.id,
				scope: "repo:/work",
				revision,
				policy: "apply-patch@1",
				action: "apply",
			});
		const stale = await check(REV_B);
		expect(stale.allowed).toBe(false);
		expect(stale.reasons).toEqual([`Base changed: grant was approved against ${REV_A}, request is for ${REV_B}`]);
		expect((await check(REV_A)).allowed).toBe(true);
		// The pure evaluator agrees: nothing is cached from an earlier allowed check.
		const document = {
			version: 1 as const,
			grants: [
				{
					id: "g",
					scope: "repo:/work",
					revision: REV_A,
					policy: "apply-patch@1",
					action: "apply",
					owner_task_id: null,
					issued_at: 0,
					expires_at: 100,
					revoked_at: null,
					revoked_by: null,
				},
			],
		};
		const request = {
			id: "g",
			scope: "repo:/work",
			policy: "apply-patch@1",
			action: "apply",
			callerTaskId: null,
			now: 1,
			tasks: [],
		};
		expect(checkGrant(document, { ...request, revision: REV_A }).allowed).toBe(true);
		expect(checkGrant(document, { ...request, revision: REV_B }).allowed).toBe(false);
	});
});
