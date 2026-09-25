import { describe, expect, test } from "vitest";
import { createMemoryModuleStore, type HostModuleStore } from "../src/ultron/rlm/host-module.ts";
import { NativeRlmHost } from "../src/ultron/rlm/native-host.ts";
import type { NativeHostStore } from "../src/ultron/rlm/task-store.ts";
import {
	createScheduleModule,
	type GoalRecord,
	type ScheduleRecord,
	type TickReport,
} from "../src/ultron/schedules.ts";
import { NativeUsageLedger } from "../src/ultron/usage.ts";

const context = {} as never;

function memoryStore(): NativeHostStore {
	let value: unknown;
	return {
		read: async () => value as never,
		write: async (next) => {
			value = structuredClone(next);
		},
	};
}

function idleLane() {
	return {
		findEntries: async () => [],
		getActiveTools: async () => [],
		setModel: async () => {},
		prompt: () => new Promise<never>(() => {}),
		abort: async () => ({ ok: true }),
		steer: async () => ({ ok: true, value: {} }),
	};
}

type Fixture = {
	host: NativeRlmHost;
	clock: { now: number };
	call<T = unknown>(type: string, payload?: Record<string, unknown>): Promise<T>;
	tasks(): Promise<Array<{ id: string; key: string; definition: string }>>;
};

function fixture(
	options: {
		moduleStore?: HostModuleStore;
		taskStore?: NativeHostStore;
		clock?: { now: number };
		tickIntervalMs?: number;
		usage?: NativeUsageLedger;
	} = {},
): Fixture {
	const clock = options.clock ?? { now: 1_000_000 };
	const lane = idleLane();
	const module = createScheduleModule({
		store: options.moduleStore ?? createMemoryModuleStore(),
		now: () => clock.now,
		tickIntervalMs: options.tickIntervalMs ?? 0,
	});
	const host = new NativeRlmHost({ lane: async () => lane } as never, lane as never, {
		store: options.taskStore ?? memoryStore(),
		modules: [module],
		usage: options.usage,
		// Replaces identity@1: echoes input, fails on request so checks can report failure.
		deterministic: async ({ input }) => {
			if ((input as { fail?: boolean }).fail) throw new Error("check crashed");
			return input;
		},
	});
	return {
		host,
		clock,
		call: (type, payload = {}) => host.handle(type, payload, context) as never,
		tasks: async () => {
			await host.handle("ping", {}, context);
			return host.api.tasks();
		},
	};
}

describe("Ultron schedules and goals (A23)", () => {
	test("duplicate and concurrent ticks in the same slot run the slot once", async () => {
		const f = fixture();
		const schedule = await f.call<ScheduleRecord>("schedules.create", {
			definition: "identity@1",
			input: { n: 1 },
			every_ms: 1000,
		});
		const first = await f.call<TickReport>("schedules.tick");
		expect(first.fired).toMatchObject([{ schedule_id: schedule.id, slot: 0, skipped: 0 }]);
		f.clock.now += 500;
		expect((await f.call<TickReport>("schedules.tick")).fired).toEqual([]);
		f.clock.now += 500;
		const concurrent = await Promise.all([
			f.call<TickReport>("schedules.tick"),
			f.call<TickReport>("schedules.tick"),
		]);
		expect(concurrent.flatMap((report) => report.fired)).toMatchObject([{ slot: 1 }]);
		const tasks = await f.tasks();
		expect(tasks.map((task) => task.key)).toEqual([`schedule:${schedule.id}:0`, `schedule:${schedule.id}:1`]);
		await f.host.close();
	});

	test("a restart on the same stores does not re-run a completed or claimed slot", async () => {
		const moduleStore = createMemoryModuleStore();
		const taskStore = memoryStore();
		const clock = { now: 1_000_000 };
		const before = fixture({ moduleStore, taskStore, clock });
		const schedule = await before.call<ScheduleRecord>("schedules.create", {
			definition: "identity@1",
			input: {},
			every_ms: 1000,
		});
		const fired = await before.call<TickReport>("schedules.tick");
		await before.host.close();

		const after = fixture({ moduleStore, taskStore, clock });
		expect((await after.call<TickReport>("schedules.tick")).fired).toEqual([]);
		expect(await after.tasks()).toHaveLength(1);
		await after.host.close();

		// Crash after claiming but before recording the admission: completion reuses the slot key.
		const saved = (await moduleStore.read()) as { schedules: ScheduleRecord[] };
		saved.schedules[0].pending_slot = 0;
		await moduleStore.write(saved as never);
		const recovered = fixture({ moduleStore, taskStore, clock });
		const report = await recovered.call<TickReport>("schedules.tick");
		expect(report.fired).toEqual([
			{ schedule_id: schedule.id, slot: 0, task_id: fired.fired[0].task_id, skipped: 0 },
		]);
		expect(await recovered.tasks()).toHaveLength(1);
		await recovered.host.close();
	});

	test("missed ticks coalesce into one run for the latest slot and record the skipped count", async () => {
		const f = fixture();
		const schedule = await f.call<ScheduleRecord>("schedules.create", {
			definition: "identity@1",
			input: {},
			every_ms: 1000,
		});
		await f.call("schedules.tick");
		f.clock.now += 5500;
		const report = await f.call<TickReport>("schedules.tick");
		expect(report.fired).toMatchObject([{ schedule_id: schedule.id, slot: 5, skipped: 4 }]);
		const [listed] = await f.call<ScheduleRecord[]>("schedules.list");
		expect(listed).toMatchObject({ runs: 2, last_slot: 5, last_skipped: 4, skipped_total: 4 });
		expect(await f.tasks()).toHaveLength(2);
		await f.host.close();
	});

	test("max_runs completes a schedule and pause stops firings", async () => {
		const f = fixture();
		const schedule = await f.call<ScheduleRecord>("schedules.create", {
			definition: "identity@1",
			input: {},
			every_ms: 1000,
			max_runs: 2,
		});
		await f.call("schedules.tick");
		await f.call("schedules.pause", { id: schedule.id });
		f.clock.now += 1000;
		expect((await f.call<TickReport>("schedules.tick")).fired).toEqual([]);
		await f.call("schedules.resume", { id: schedule.id });
		expect((await f.call<TickReport>("schedules.tick")).fired).toHaveLength(1);
		f.clock.now += 1000;
		expect((await f.call<TickReport>("schedules.tick")).fired).toEqual([]);
		expect((await f.call<ScheduleRecord[]>("schedules.list"))[0]).toMatchObject({ state: "completed", runs: 2 });
		await f.call("schedules.delete", { id: schedule.id });
		expect(await f.call("schedules.list")).toEqual([]);
		await f.host.close();
	});

	test("a paused goal stops attached schedule firings and resume re-enables them", async () => {
		const f = fixture();
		const goal = await f.call<GoalRecord>("goals.create", { title: "keep green", required_checks: ["identity@1"] });
		const schedule = await f.call<ScheduleRecord>("schedules.create", {
			definition: "identity@1",
			input: { poll: true },
			every_ms: 1000,
			goal_id: goal.id,
		});
		await f.call("goals.pause", { id: goal.id });
		expect((await f.call<TickReport>("schedules.tick")).fired).toEqual([]);
		f.clock.now += 2000;
		expect((await f.call<TickReport>("schedules.tick")).fired).toEqual([]);
		await expect(f.call("goals.verify", { id: goal.id })).rejects.toThrow("Goal is paused");
		expect(await f.tasks()).toEqual([]);

		await f.call("goals.resume", { id: goal.id });
		const report = await f.call<TickReport>("schedules.tick");
		expect(report.fired).toMatchObject([{ schedule_id: schedule.id, slot: 2, skipped: 2 }]);
		expect((await f.call<GoalRecord>("goals.get", { id: goal.id })).task_ids).toEqual([report.fired[0].task_id]);

		// An achieved goal is finished: its schedules stop.
		await f.call("goals.verify", { id: goal.id, input: { passed: true } });
		f.clock.now += 1000;
		expect((await f.call<TickReport>("schedules.tick")).fired).toEqual([]);
		await f.host.close();
	});

	test("required gate: achieved only when every required check succeeds with passed === true", async () => {
		const f = fixture();
		const goal = await f.call<GoalRecord>("goals.create", { title: "ship", required_checks: ["identity@1"] });

		const failedValue = await f.call<GoalRecord>("goals.verify", { id: goal.id, input: { passed: false } });
		expect(failedValue).toMatchObject({ state: "active", verification: { status: "unachieved" } });
		const nonBoolean = await f.call<GoalRecord>("goals.verify", { id: goal.id, input: { passed: "yes" } });
		expect(nonBoolean.verification).toMatchObject({
			status: "unachieved",
			reasons: ["identity@1 did not report a boolean passed value"],
		});
		const missingField = await f.call<GoalRecord>("goals.verify", { id: goal.id });
		expect(missingField.verification?.status).toBe("unachieved");
		const crashed = await f.call<GoalRecord>("goals.verify", { id: goal.id, input: { fail: true, passed: true } });
		expect(crashed.verification?.checks).toMatchObject([{ status: "failed", passed: false }]);
		expect(crashed.state).toBe("active");

		const passed = await f.call<GoalRecord>("goals.verify", { id: goal.id, input: { passed: true } });
		expect(passed).toMatchObject({ state: "achieved", verification: { status: "achieved", round: 5 } });
		expect(passed.task_ids).toHaveLength(5);
		await expect(f.call("goals.verify", { id: goal.id })).rejects.toThrow("already achieved");

		// One unknown check keeps the gate closed even when the others pass.
		const partial = await f.call<GoalRecord>("goals.create", {
			title: "partial",
			required_checks: ["identity@1", "not-registered@1"],
		});
		const partialResult = await f.call<GoalRecord>("goals.verify", { id: partial.id, input: { passed: true } });
		expect(partialResult.state).toBe("active");
		expect(partialResult.verification?.checks).toMatchObject([
			{ definition: "identity@1", status: "succeeded", passed: true },
			{ definition: "not-registered@1", status: "missing", passed: false, task_id: null },
		]);

		const unchecked = await f.call<GoalRecord>("goals.create", { title: "vibes", required_checks: [] });
		const uncheckedResult = await f.call<GoalRecord>("goals.verify", { id: unchecked.id, input: { passed: true } });
		expect(uncheckedResult).toMatchObject({ state: "active", verification: { status: "unverified" } });
		await f.host.close();
	});

	test("max_tasks caps schedule firings, verification, and attachments", async () => {
		const f = fixture();
		const goal = await f.call<GoalRecord>("goals.create", {
			title: "capped",
			required_checks: ["identity@1"],
			max_tasks: 2,
		});
		const schedule = await f.call<ScheduleRecord>("schedules.create", {
			definition: "identity@1",
			input: {},
			every_ms: 1000,
			goal_id: goal.id,
		});
		await f.call("schedules.tick");
		f.clock.now += 1000;
		await f.call("schedules.tick");
		f.clock.now += 1000;
		const capped = await f.call<TickReport>("schedules.tick");
		expect(capped.failed).toEqual([{ schedule_id: schedule.id, slot: 2, error: "Goal task limit reached (2)" }]);
		await expect(f.call("goals.verify", { id: goal.id, input: { passed: true } })).rejects.toThrow(
			"Goal task limit reached",
		);
		const extra = (await f.host.handle("agents.spawn", { definition: "identity@1", input: {} }, context)) as {
			id: string;
		};
		await expect(f.call("goals.attach", { id: goal.id, task_id: extra.id })).rejects.toThrow("Goal task limit");
		expect((await f.call<GoalRecord>("goals.get", { id: goal.id })).task_ids).toHaveLength(2);
		expect(await f.tasks()).toHaveLength(3);
		await f.host.close();
	});

	test("admission failure from the usage budget is recorded once per slot without a retry storm", async () => {
		const usage = new NativeUsageLedger(undefined, { limits: { maxAdmittedTasks: 0 } });
		const f = fixture({ usage });
		const schedule = await f.call<ScheduleRecord>("schedules.create", {
			definition: "identity@1",
			input: {},
			every_ms: 1000,
		});
		const failed = await f.call<TickReport>("schedules.tick");
		expect(failed.failed).toMatchObject([{ schedule_id: schedule.id, slot: 0 }]);
		expect(failed.failed[0].error).toContain("admitted-task limit");
		for (let i = 0; i < 5; i++) expect(await f.call<TickReport>("schedules.tick")).toEqual({ fired: [], failed: [] });
		f.clock.now += 1000;
		expect((await f.call<TickReport>("schedules.tick")).failed).toMatchObject([{ slot: 1 }]);
		const [listed] = await f.call<ScheduleRecord[]>("schedules.list");
		expect(listed).toMatchObject({
			runs: 0,
			failures: 2,
			last_slot: 1,
			pending_slot: null,
			last_failure: { slot: 1 },
		});
		expect(await f.tasks()).toHaveLength(0);
		await f.host.close();
	});

	test("the background timer fires due slots and stops on close", async () => {
		const f = fixture({ tickIntervalMs: 5 });
		const schedule = await f.call<ScheduleRecord>("schedules.create", {
			definition: "identity@1",
			input: {},
			every_ms: 1000,
		});
		await new Promise((resolve) => setTimeout(resolve, 40));
		expect((await f.tasks()).map((task) => task.key)).toEqual([`schedule:${schedule.id}:0`]);
		await f.host.close();
	});

	test("rejects unknown payload fields and forged identity", async () => {
		const f = fixture();
		await expect(
			f.call("schedules.create", { definition: "identity@1", input: {}, every_ms: 1000, owner: "task-x" }),
		).rejects.toThrow("Unknown payload field: owner");
		await expect(f.call("schedules.tick", { force: true })).rejects.toThrow("Unknown payload field: force");
		await expect(f.call("goals.create", { title: "t", required_checks: [], state: "achieved" })).rejects.toThrow(
			"Unknown payload field: state",
		);
		await expect(f.call("goals.verify", { id: "goal-x", passed: true })).rejects.toThrow(
			"Unknown payload field: passed",
		);
		await expect(f.call("goals.create", { title: "t", required_checks: ["nope"] })).rejects.toThrow("id@version");
		await expect(f.call("schedules.create", { definition: "identity@1", input: {}, every_ms: 0 })).rejects.toThrow(
			"every_ms",
		);
		const schedule = await f.call<ScheduleRecord>("schedules.create", {
			definition: "identity@1",
			input: {},
			every_ms: 1000,
		});
		expect(schedule.owner).toBeNull();
		await f.host.close();
	});
});
