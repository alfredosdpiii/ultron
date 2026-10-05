/**
 * The session goal (`/goal`, goal.ts), RLM-first: a person sets an objective and a background job (its own REPL,
 * no wall-clock limit, its own usage root) works on it. The goal ends through the model's `goal.complete` (which
 * needs the host's check to pass, when there is one) or `goal.blocked` (after three checks); ten identical failing
 * checks in a row pause it; a job that ends early is started again only if its run changed the check result; the
 * user pauses, resumes, edits and clears it; the model names the revision; subagents cannot end it.
 */
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { AgentHarness, MemorySessionRepo } from "@ultron/agent-core";
import { createModels, fauxAssistantMessage, fauxProvider } from "@ultron/ai";
import type { JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { describe, expect, test } from "vitest";
import { checkSignature, type GoalCheckResult, GoalDriver, goalBrief, STUCK_CHECKS } from "../src/ultron/goal.ts";
import { createMemoryModuleStore, type HostModuleStore, type NativeHostApi } from "../src/ultron/rlm/host-module.ts";
import { RlmKernel } from "../src/ultron/rlm/kernel.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";
import { GOAL_USAGE_ROOT_PREFIX, NativeUsageLedger } from "../src/ultron/usage.ts";
import { waitFor } from "./ultron-host-fixtures.ts";

const runtimePath = fileURLToPath(new URL("../src/ultron/rlm/runtime.py", import.meta.url));
const FAIL: GoalCheckResult = { exit_code: 1, timed_out: false, output: "1 failed in 0.31s" };
const PASS: GoalCheckResult = { exit_code: 0, timed_out: false, output: "3 passed in 0.12s" };

/** A driver over fake jobs: each start is recorded with its brief; `lane(id)` is the job's lane. */
function setup(checks: GoalCheckResult[] = [], store: HostModuleStore = createMemoryModuleStore()) {
	const started: Array<{ id: string; prompt: string; key: string; usageRoot: string }> = [];
	const stopped: string[] = [];
	const ran: string[] = [];
	const lane = (id: string) => `ultron.background-job.${id}`;
	const driver = new GoalDriver({
		store,
		runCheck: async (command) => {
			ran.push(command);
			return checks.shift() ?? PASS;
		},
		startJob: async (prompt, options) => {
			const id = `task-${started.length + 1}`;
			started.push({ id, prompt, ...options });
			return id;
		},
		stopJob: async (id) => {
			stopped.push(id);
		},
		laneOfJob: (id) => lane(id),
	});
	const call = (type: string, payload: Record<string, unknown>, caller = "main") =>
		driver.module.handle(
			{ type, payload, caller: { lane: caller }, context: BACKGROUND_CONTEXT },
			{} as NativeHostApi,
		) as Promise<Record<string, unknown>>;
	return { driver, started, stopped, ran, lane, call };
}

describe("session goal", () => {
	test("/goal starts one background job with the RLM brief under the goal's own usage root", async () => {
		const { driver, started } = setup();
		const shown = await driver.command("ship the parser");
		expect(shown).toContain("Goal (revision 1, active): ship the parser");
		expect(started).toHaveLength(1);
		const goal = (await driver.get())!;
		expect(started[0]).toMatchObject({ key: `${goal.id}:1`, usageRoot: `${GOAL_USAGE_ROOT_PREFIX}${goal.id}` });
		expect(started[0]!.prompt).toContain("Objective: ship the parser");
		expect(started[0]!.prompt).toContain("nothing limits your time");
		expect(started[0]!.prompt).toContain("rlm.spawn(brief, name=..., worktree=True)");
		expect(started[0]!.prompt).toContain("await goal.complete(1, summary, evidence=[...])");
		expect(goal).toMatchObject({ status: "active", job: "task-1", runs: 1 });
	});

	test("the job completes the goal only when the host's check passes", async () => {
		const { driver, call, lane, ran } = setup([FAIL]);
		await driver.command("make the tests pass");
		await driver.command("check pytest -q");
		const job = lane("task-1");
		expect(await call("goal.complete", { revision: 2, summary: "s", evidence: ["e"] }, job)).toMatchObject({
			complete: false,
			check: { passed: false, exit_code: 1 },
		});
		expect(
			await call("goal.complete", { revision: 2, summary: "fixed", evidence: ["pytest: exit 0"] }, job),
		).toMatchObject({ complete: true, verified: true });
		expect(ran).toEqual(["pytest -q", "pytest -q"]);
		// The job ends after completing: nothing is started again.
		driver.taskEnded({ id: "task-1", result: { status: "succeeded", value: "done" } });
		await driver.settled();
		expect(await driver.get()).toMatchObject({ status: "complete", job: null, runs: 1 });
	});

	test(`${STUCK_CHECKS} identical failing checks in a row pause the goal and stop its job`, async () => {
		const { driver, call, lane, stopped } = setup(
			// Durations differ but the result is the same; one different failure resets the streak.
			[
				...Array.from({ length: 5 }, (_, i) => ({ ...FAIL, output: `1 failed in 0.${i}1s` })),
				{ ...FAIL, output: "2 failed" },
				...Array.from({ length: STUCK_CHECKS }, () => FAIL),
			],
		);
		await driver.command("fix it");
		await driver.command("check pytest");
		const results: Array<Record<string, unknown>> = [];
		for (let i = 0; i < 5 + 1 + STUCK_CHECKS; i++) results.push(await call("goal.check", {}, lane("task-1")));
		expect(results.slice(0, -1).every((result) => result.paused === false)).toBe(true);
		expect(results.at(-1)).toMatchObject({ paused: true, passed: false });
		expect((await driver.get())!.reason).toContain(`${STUCK_CHECKS} failing checks in a row gave the same result`);
		await waitFor(() => stopped.includes("task-1"));
	});

	test("a job that ends early is started again only after a run that changed the check result", async () => {
		const { driver, call, lane, started } = setup([FAIL, FAIL]);
		await driver.command("fix it");
		await driver.command("check pytest");
		await call("goal.check", {}, lane("task-1"));
		driver.taskEnded({ id: "task-1", result: { status: "succeeded", value: "I will continue later" } });
		await driver.settled();
		expect(started).toHaveLength(2);
		expect(started[1]!.prompt).toContain("The previous run on this goal ended with: I will continue later");
		// The second run checks once with the same result: no progress, so the goal pauses instead.
		await call("goal.check", {}, lane("task-2"));
		driver.taskEnded({ id: "task-2", result: { status: "failed", error: "provider error" } });
		await driver.settled();
		expect(started).toHaveLength(2);
		expect(await driver.get()).toMatchObject({ status: "paused", job: null });
		expect((await driver.get())!.reason).toContain("ended without progress");
		// Ends of other tasks are ignored.
		driver.taskEnded({ id: "task-9", result: { status: "succeeded" } });
		await driver.command("resume");
		expect(started).toHaveLength(3);
	});

	test("blocked needs three checks; stale revisions, subagents and a paused goal are refused", async () => {
		const { driver, call, lane, stopped } = setup([FAIL, FAIL, FAIL]);
		await driver.command("deploy");
		await driver.command("check ./deploy --dry-run");
		const job = lane("task-1");
		await expect(call("goal.blocked", { revision: 2, reason: "no network" }, job)).rejects.toThrow(
			/accepted after 3 checks \(0 so far\)/,
		);
		await expect(call("goal.complete", { revision: 1, summary: "s", evidence: ["e"] }, job)).rejects.toThrow(
			/at revision 2 \(the user changed it\)/,
		);
		await expect(
			call("goal.complete", { revision: 2, summary: "s", evidence: ["e"] }, "ultron.rlm-child.t1"),
		).rejects.toThrow(/Only the goal's job or the root agent/);
		await expect(call("goal.complete", { revision: 2, summary: "s", evidence: [] }, job)).rejects.toThrow(
			/evidence must be a non-empty list/,
		);
		for (let i = 0; i < 3; i++) await call("goal.check", {}, job);
		expect(await call("goal.blocked", { revision: 2, reason: "no network" }, job)).toMatchObject({
			status: "blocked",
		});
		expect(await call("goal.get", {}, "ultron.rlm-child.t1")).toMatchObject({ status: "blocked", checks: 3 });
		await driver.command("resume");
		await driver.command("pause");
		expect(stopped).toEqual(["task-2"]);
		await expect(call("goal.complete", { revision: 2, summary: "s", evidence: ["e"] })).rejects.toThrow(
			/is paused, not active/,
		);
	});

	test("a new objective replaces the job; clear stops it; a restarted worker pauses a goal whose job was running", async () => {
		const store = createMemoryModuleStore();
		const { driver, started, stopped } = setup([], store);
		await driver.command("first");
		await driver.command("second");
		expect(stopped).toEqual(["task-1"]);
		expect(started[1]!.prompt).toContain("Objective: second");
		expect(await driver.get()).toMatchObject({ revision: 2, job: "task-2" });
		await driver.settled();
		const again = setup([], store).driver;
		expect(await again.get()).toMatchObject({ status: "paused", job: null });
		expect((await again.get())!.reason).toContain("restarted while the goal job ran");
		expect(await again.command("clear")).toBe("Goal cleared.");
		expect(await again.command("")).toBe("No goal. Set one with /goal <objective>.");
	});

	test("check signatures ignore durations and addresses, not counts or exit codes", () => {
		expect(checkSignature({ ...FAIL, output: "1 failed in 0.31s at 0x7f00" })).toBe(
			checkSignature({ ...FAIL, output: "1 failed in 12.5s at 0x1234" }),
		);
		expect(checkSignature({ ...FAIL, output: "1 failed" })).not.toBe(checkSignature({ ...FAIL, output: "2 failed" }));
		expect(checkSignature({ ...FAIL, exit_code: 2 })).not.toBe(checkSignature(FAIL));
		expect(
			goalBrief({
				id: "g",
				revision: 3,
				objective: "o",
				check: null,
				status: "active",
				reason: null,
				job: null,
				runs: 0,
				checks: 0,
				stuck: 0,
				last_signature: null,
				run_progress: false,
				last_check: null,
				last_answer: null,
				created_at: 0,
				updated_at: 0,
				completion: null,
			}),
		).toContain("There is no check command");
	});
});

describe("goal jobs in the host", () => {
	test("a goal root has no wall deadline and no task cap; other roots keep them", async () => {
		const ledger = new NativeUsageLedger(undefined, { limits: { maxWallMs: 1000, maxAdmittedTasks: 1 } });
		const goal = `${GOAL_USAGE_ROOT_PREFIX}g1`;
		const first = await ledger.reserve({ kind: "task", rootId: goal, requestKey: "a" });
		const second = await ledger.reserve({ kind: "task", rootId: goal, requestKey: "b" });
		expect([first.deadlineAt, second.deadlineAt]).toEqual([null, null]);
		await ledger.reserve({ kind: "task", rootId: "turn:x", requestKey: "c" });
		await expect(ledger.reserve({ kind: "task", rootId: "turn:x", requestKey: "d" })).rejects.toThrow(
			/admitted-task limit/,
		);
	});

	test("startGoalJob runs a background job untimed under the goal root; stopTask cancels it", async () => {
		const repo = new MemorySessionRepo();
		const session = await repo.create({ id: "goaljob" }, BACKGROUND_CONTEXT);
		const faux = fauxProvider();
		let release: (() => void) | undefined;
		faux.setResponses([
			async () => fauxAssistantMessage("first job done"),
			async () => {
				await new Promise<void>((resolve) => {
					release = resolve;
				});
				return fauxAssistantMessage("late");
			},
		] as never);
		const models = createModels();
		models.setProvider(faux.provider);
		const { harness } = await AgentHarness.create(
			{ session, models, model: faux.getModel(), activeToolNames: [] },
			BACKGROUND_CONTEXT,
		);
		const main = await harness.lane("main", BACKGROUND_CONTEXT);
		let value: JsonValue | undefined;
		const store: NativeHostStore = {
			read: async () => structuredClone(value) as never,
			write: async (next) => {
				value = structuredClone(next) as JsonValue;
			},
		};
		const ledger = new NativeUsageLedger(undefined, { limits: { maxWallMs: 1000, maxAdmittedTasks: 1 } });
		const host = new NativeRlmHost(harness, main, { store, usage: ledger });
		try {
			const id = await host.startGoalJob("work on it", { key: "g1:1", usageRoot: "goal:g1" }, BACKGROUND_CONTEXT);
			expect(await host.handle("background.result", { id }, BACKGROUND_CONTEXT)).toMatchObject({
				status: "succeeded",
				value: "first job done",
			});
			// Past the 1 s wall limit other roots have, a second goal job is still admitted and runs untimed.
			await new Promise((resolve) => setTimeout(resolve, 1100));
			const second = await host.startGoalJob("again", { key: "g1:2", usageRoot: "goal:g1" }, BACKGROUND_CONTEXT);
			await waitFor(() => release !== undefined);
			expect(host.laneOfTask(second)).toBe(`ultron.background-job.${second}`);
			await host.stopTask(second, "goal paused");
			expect(await host.handle("background.result", { id: second }, BACKGROUND_CONTEXT)).toMatchObject({
				status: "cancelled",
			});
			release?.();
		} finally {
			await host.close();
			await harness.close(BACKGROUND_CONTEXT);
		}
	});
});

describe("goal in the kernel", () => {
	test("goal.get, goal.check, goal.complete and goal.blocked are host requests", async () => {
		const requests: Array<[string, unknown]> = [];
		const kernel = new RlmKernel({ cwd: tmpdir(), runtimePath }, async (type, payload) => {
			requests.push([type, payload]);
			return type === "goal.get" ? { revision: 4 } : { status: "ok" };
		});
		try {
			const result = await kernel.execute(
				"g = await goal.get()\nawait goal.check()\nawait goal.complete(g['revision'], 'done', ('pytest: exit 0',))\nawait goal.blocked(4, 'no access')\nrepr(goal)",
			);
			expect(result).toMatchObject({ status: "ok" });
			expect(result.result).toContain("goal.check()");
			expect(requests).toEqual([
				["goal.get", {}],
				["goal.check", {}],
				["goal.complete", { revision: 4, summary: "done", evidence: ["pytest: exit 0"] }],
				["goal.blocked", { revision: 4, reason: "no access" }],
			]);
		} finally {
			await kernel.shutdown();
		}
	});
});
