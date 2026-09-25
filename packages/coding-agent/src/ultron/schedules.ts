import { randomUUID } from "node:crypto";
import type { Context } from "@ultron/agent-core";
import { isJsonValue, type JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import type {
	HostModuleRequest,
	HostModuleStore,
	HostTaskRequest,
	NativeHostApi,
	NativeHostModule,
} from "./rlm/host-module.ts";
import type { NativeResult, NativeTask } from "./rlm/task-store.ts";

type Payload = Record<string, unknown>;

export type ScheduleState = "active" | "paused" | "completed";
export type GoalState = "active" | "paused" | "achieved";

export type ScheduleFailure = { slot: number; error: string; at: number };

export type ScheduleRecord = {
	id: string;
	definition: string;
	input: JsonValue;
	every_ms: number;
	start_at: number;
	max_runs: number | null;
	goal_id: string | null;
	model: string | null;
	/** Task that created the schedule (null for root); firings are spawned as its children. */
	owner: string | null;
	state: ScheduleState;
	created_at: number;
	runs: number;
	/** Highest slot claimed. A claimed slot is never fired again. */
	last_slot: number | null;
	/** Slot claimed but not yet admitted; completed with the same idempotency key after a crash. */
	pending_slot: number | null;
	last_task_id: string | null;
	last_fired_at: number | null;
	last_skipped: number;
	skipped_total: number;
	last_failure: ScheduleFailure | null;
	failures: number;
};

export type CheckOutcome = {
	definition: string;
	task_id: string | null;
	status: NativeResult["status"] | "missing";
	passed: boolean;
	reason?: string;
};

export type GoalVerification = {
	round: number;
	status: "achieved" | "unachieved" | "unverified";
	at: number;
	checks: CheckOutcome[];
	reasons: string[];
};

export type GoalRecord = {
	id: string;
	title: string;
	required_checks: string[];
	max_tasks: number | null;
	owner: string | null;
	state: GoalState;
	created_at: number;
	task_ids: string[];
	verify_rounds: number;
	verification: GoalVerification | null;
};

type ScheduleDocument = { version: 1; schedules: ScheduleRecord[]; goals: GoalRecord[] };

export type TickReport = {
	fired: Array<{ schedule_id: string; slot: number; task_id: string; skipped: number }>;
	failed: Array<{ schedule_id: string; slot: number; error: string }>;
};

const DEFINITION_PATTERN = /^[a-z][a-z0-9-]*@[0-9]+$/;
const MODEL_PATTERN = /^[^/\s]+\/[^/\s]+(?:\/[^/\s]+)*$/;
const MAX_EVERY_MS = 366 * 24 * 60 * 60 * 1000;
const DEFAULT_TICK_INTERVAL_MS = 1000;

function fields(payload: Payload, allowed: string[]): void {
	for (const key of Object.keys(payload)) {
		if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
	}
}

function nonemptyString(value: unknown, name: string): string {
	if (typeof value !== "string" || !value.trim()) throw new Error(`${name} must be a nonempty string`);
	return value;
}

function definitionKey(value: unknown, name = "definition"): string {
	if (typeof value !== "string" || !DEFINITION_PATTERN.test(value)) throw new Error(`${name} must be id@version`);
	return value;
}

function integer(value: unknown, name: string, min: number, max = Number.MAX_SAFE_INTEGER): number {
	if (typeof value !== "number" || !Number.isSafeInteger(value) || value < min || value > max)
		throw new Error(`${name} must be an integer between ${min} and ${max}`);
	return value;
}

function optionalInteger(value: unknown, name: string, min: number): number | null {
	return value === undefined || value === null ? null : integer(value, name, min);
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function emptyDocument(): ScheduleDocument {
	return { version: 1, schedules: [], goals: [] };
}

function validateDocument(saved: JsonValue): ScheduleDocument {
	const invalid = () => new Error("Invalid schedule document");
	if (saved === null || typeof saved !== "object" || Array.isArray(saved)) throw invalid();
	const { version, schedules, goals } = saved as Record<string, unknown>;
	if (version !== 1 || !Array.isArray(schedules) || !Array.isArray(goals)) throw invalid();
	const ids = new Set<string>();
	for (const item of [...schedules, ...goals]) {
		if (item === null || typeof item !== "object" || Array.isArray(item)) throw invalid();
		const id = (item as { id?: unknown }).id;
		if (typeof id !== "string" || ids.has(id)) throw invalid();
		ids.add(id);
	}
	return structuredClone(saved) as unknown as ScheduleDocument;
}

/** Slot index for `now`; negative before the schedule starts. */
export function scheduleSlot(schedule: Pick<ScheduleRecord, "start_at" | "every_ms">, now: number): number {
	return Math.floor((now - schedule.start_at) / schedule.every_ms);
}

function scheduleKey(schedule: ScheduleRecord, slot: number): string {
	return `schedule:${schedule.id}:${slot}`;
}

function checkOutcome(definition: string, taskId: string | null, result: NativeResult): CheckOutcome {
	const outcome: CheckOutcome = { definition, task_id: taskId, status: result.status, passed: false };
	if (result.status !== "succeeded") {
		outcome.reason = `${definition} ${result.status}${result.error ? `: ${result.error}` : ""}`;
		return outcome;
	}
	const value = result.value;
	const passed =
		value !== null && typeof value === "object" && !Array.isArray(value)
			? (value as Record<string, JsonValue>).passed
			: undefined;
	if (typeof passed !== "boolean") {
		outcome.reason = `${definition} did not report a boolean passed value`;
		return outcome;
	}
	outcome.passed = passed;
	if (!passed) outcome.reason = `${definition} reported passed=false`;
	return outcome;
}

/**
 * Schedules and goals. Firings are admitted through {@link NativeHostApi.spawn}, so the usage
 * ledger and task idempotency apply; the slot-derived key makes each slot run at most once.
 */
class ScheduleModule implements NativeHostModule {
	readonly prefixes = ["schedules.", "goals."] as const;
	private readonly store: HostModuleStore;
	private readonly clock: (() => number) | undefined;
	private readonly tickIntervalMs: number;
	private document?: ScheduleDocument;
	private loading?: Promise<ScheduleDocument>;
	private tail: Promise<void> = Promise.resolve();
	private host?: NativeHostApi;
	private timer?: ReturnType<typeof setInterval>;
	private closed = false;

	constructor(options: { store: HostModuleStore; now?: () => number; tickIntervalMs?: number }) {
		this.store = options.store;
		this.clock = options.now;
		this.tickIntervalMs = integer(
			options.tickIntervalMs ?? DEFAULT_TICK_INTERVAL_MS,
			"tickIntervalMs",
			0,
			MAX_EVERY_MS,
		);
	}

	private now(): number {
		return this.clock ? this.clock() : (this.host?.now() ?? Date.now());
	}

	private load(): Promise<ScheduleDocument> {
		this.loading ??= (async () => {
			const saved = await this.store.read();
			this.document = saved === undefined ? emptyDocument() : validateDocument(saved);
			return this.document;
		})();
		return this.loading;
	}

	/** Serialize every read-modify-write so concurrent ticks and requests see one history. */
	private exclusive<T>(action: (document: ScheduleDocument) => Promise<T>): Promise<T> {
		const run = this.tail.then(async () => {
			await this.load();
			return action(this.document!);
		});
		this.tail = run.then(
			() => {},
			() => {},
		);
		return run;
	}

	/** Commit a new document, then publish it in memory; a failed write leaves the old state. */
	private async commit(next: ScheduleDocument): Promise<void> {
		await this.store.write(structuredClone(next) as unknown as JsonValue);
		this.document = next;
	}

	private async update<T>(mutate: (draft: ScheduleDocument) => T | Promise<T>): Promise<T> {
		const draft = structuredClone(this.document!);
		const result = await mutate(draft);
		await this.commit(draft);
		return result;
	}

	async start(host: NativeHostApi): Promise<void> {
		this.host = host;
		await this.exclusive(() => this.reconcile(host));
		if (this.tickIntervalMs > 0 && !this.closed) {
			this.timer = setInterval(() => {
				if (this.closed) return;
				void this.exclusive(() => this.tick(host, BACKGROUND_CONTEXT)).catch(() => {});
			}, this.tickIntervalMs);
			this.timer.unref?.();
		}
	}

	close(): void {
		this.closed = true;
		if (this.timer) clearInterval(this.timer);
		this.timer = undefined;
	}

	/** Recover goal task links written by keys the host already admitted before a crash. */
	private async reconcile(host: NativeHostApi): Promise<void> {
		const document = this.document!;
		if (document.goals.length === 0) return;
		const tasks = await host.tasks();
		const byKey = new Map(tasks.map((task) => [task.key, task]));
		let changed = false;
		const draft = structuredClone(document);
		for (const goal of draft.goals) {
			const known = new Set(goal.task_ids);
			for (const task of tasks) {
				if (task.key.startsWith(`goal:${goal.id}:`) && !known.has(task.id)) {
					goal.task_ids.push(task.id);
					known.add(task.id);
					changed = true;
				}
			}
		}
		for (const schedule of draft.schedules) {
			if (schedule.goal_id === null) continue;
			const goal = draft.goals.find((item) => item.id === schedule.goal_id);
			const slot = schedule.pending_slot ?? schedule.last_slot;
			const task = slot === null ? undefined : byKey.get(scheduleKey(schedule, slot));
			if (goal && task && !goal.task_ids.includes(task.id)) {
				goal.task_ids.push(task.id);
				changed = true;
			}
		}
		if (changed) await this.commit(draft);
	}

	async handle(request: HostModuleRequest, host: NativeHostApi): Promise<unknown> {
		if (this.closed) throw new Error("Schedule module is closed");
		this.host ??= host;
		const { type, payload, caller, context } = request;
		const callerTask = host.callerTaskId(caller);
		switch (type) {
			case "schedules.create":
				return this.exclusive(() => this.createSchedule(payload, callerTask));
			case "schedules.list":
				fields(payload, []);
				return this.exclusive(async (document) => structuredClone(document.schedules));
			case "schedules.pause":
				return this.exclusive(() => this.setScheduleState(payload, "paused"));
			case "schedules.resume":
				return this.exclusive(() => this.setScheduleState(payload, "active"));
			case "schedules.delete":
				return this.exclusive(() => this.deleteSchedule(payload));
			case "schedules.tick":
				fields(payload, []);
				// Firings outlive the request that happened to drive the tick.
				return this.exclusive(() => this.tick(host, BACKGROUND_CONTEXT));
			case "goals.create":
				return this.exclusive(() => this.createGoal(payload, callerTask));
			case "goals.list":
				fields(payload, []);
				return this.exclusive(async (document) => structuredClone(document.goals));
			case "goals.get":
				fields(payload, ["id"]);
				return this.exclusive(async (document) => structuredClone(this.goal(document, payload.id)));
			case "goals.pause":
				return this.exclusive(() => this.setGoalState(payload, "paused"));
			case "goals.resume":
				return this.exclusive(() => this.setGoalState(payload, "active"));
			case "goals.attach":
				return this.exclusive(() => this.attach(payload, host));
			case "goals.verify":
				return this.verify(payload, host, callerTask, context);
			default:
				throw new Error(`Unknown schedule request: ${type}`);
		}
	}

	private schedule(document: ScheduleDocument, id: unknown): ScheduleRecord {
		const key = nonemptyString(id, "id");
		const schedule = document.schedules.find((item) => item.id === key);
		if (!schedule) throw new Error("Unknown schedule");
		return schedule;
	}

	private goal(document: ScheduleDocument, id: unknown): GoalRecord {
		const key = nonemptyString(id, "id");
		const goal = document.goals.find((item) => item.id === key);
		if (!goal) throw new Error("Unknown goal");
		return goal;
	}

	private capacity(goal: GoalRecord): number {
		return goal.max_tasks === null ? Number.POSITIVE_INFINITY : goal.max_tasks - goal.task_ids.length;
	}

	private createSchedule(payload: Payload, owner: string | null): Promise<ScheduleRecord> {
		fields(payload, ["definition", "input", "every_ms", "start_at", "max_runs", "goal_id", "model"]);
		const definition = definitionKey(payload.definition);
		if (!isJsonValue(payload.input)) throw new Error("input must be JSON");
		const everyMs = integer(payload.every_ms, "every_ms", 1, MAX_EVERY_MS);
		const now = this.now();
		const startAt = optionalInteger(payload.start_at, "start_at", 0) ?? now;
		const maxRuns = optionalInteger(payload.max_runs, "max_runs", 1);
		let model: string | null = null;
		if (payload.model !== undefined && payload.model !== null) {
			if (typeof payload.model !== "string" || !MODEL_PATTERN.test(payload.model))
				throw new Error("model must be provider/model");
			model = payload.model;
		}
		const goalId = payload.goal_id === undefined || payload.goal_id === null ? null : payload.goal_id;
		return this.update((draft) => {
			if (goalId !== null && this.goal(draft, goalId).state === "achieved")
				throw new Error("Cannot attach a schedule to an achieved goal");
			const schedule: ScheduleRecord = {
				id: `schedule-${randomUUID()}`,
				definition,
				input: payload.input as JsonValue,
				every_ms: everyMs,
				start_at: startAt,
				max_runs: maxRuns,
				goal_id: goalId as string | null,
				model,
				owner,
				state: "active",
				created_at: now,
				runs: 0,
				last_slot: null,
				pending_slot: null,
				last_task_id: null,
				last_fired_at: null,
				last_skipped: 0,
				skipped_total: 0,
				last_failure: null,
				failures: 0,
			};
			draft.schedules.push(schedule);
			return structuredClone(schedule);
		});
	}

	private setScheduleState(payload: Payload, state: "active" | "paused"): Promise<ScheduleRecord> {
		fields(payload, ["id"]);
		return this.update((draft) => {
			const schedule = this.schedule(draft, payload.id);
			if (schedule.state === "completed") throw new Error("Schedule is completed");
			schedule.state = state;
			return structuredClone(schedule);
		});
	}

	private deleteSchedule(payload: Payload): Promise<{ deleted: true }> {
		fields(payload, ["id"]);
		return this.update((draft) => {
			const schedule = this.schedule(draft, payload.id);
			draft.schedules = draft.schedules.filter((item) => item !== schedule);
			return { deleted: true as const };
		});
	}

	private createGoal(payload: Payload, owner: string | null): Promise<GoalRecord> {
		fields(payload, ["title", "required_checks", "max_tasks"]);
		const title = nonemptyString(payload.title, "title");
		if (!Array.isArray(payload.required_checks)) throw new Error("required_checks must be an array");
		const checks = payload.required_checks.map((item) => definitionKey(item, "required_checks entry"));
		if (new Set(checks).size !== checks.length) throw new Error("required_checks must not repeat");
		const maxTasks = optionalInteger(payload.max_tasks, "max_tasks", 0);
		return this.update((draft) => {
			const goal: GoalRecord = {
				id: `goal-${randomUUID()}`,
				title,
				required_checks: checks,
				max_tasks: maxTasks,
				owner,
				state: "active",
				created_at: this.now(),
				task_ids: [],
				verify_rounds: 0,
				verification: null,
			};
			draft.goals.push(goal);
			return structuredClone(goal);
		});
	}

	private setGoalState(payload: Payload, state: "active" | "paused"): Promise<GoalRecord> {
		fields(payload, ["id"]);
		return this.update((draft) => {
			const goal = this.goal(draft, payload.id);
			if (goal.state === "achieved") throw new Error("Goal is already achieved");
			goal.state = state;
			return structuredClone(goal);
		});
	}

	private async attach(payload: Payload, host: NativeHostApi): Promise<GoalRecord> {
		fields(payload, ["id", "task_id"]);
		const taskId = nonemptyString(payload.task_id, "task_id");
		this.goal(this.document!, payload.id);
		if (!(await host.tasks()).some((task) => task.id === taskId)) throw new Error("Unknown Ultron task");
		return this.update((draft) => {
			const goal = this.goal(draft, payload.id);
			if (!goal.task_ids.includes(taskId)) {
				if (this.capacity(goal) < 1) throw new Error(`Goal task limit reached (${goal.max_tasks})`);
				goal.task_ids.push(taskId);
			}
			return structuredClone(goal);
		});
	}

	/**
	 * Fire due schedules. Each slot is claimed durably before admission and admitted with a
	 * slot-derived key, so duplicate or concurrent ticks and restarts never run a slot twice.
	 */
	private async tick(host: NativeHostApi, context: Context): Promise<TickReport> {
		const report: TickReport = { fired: [], failed: [] };
		const now = this.now();
		for (const { id } of this.document!.schedules) {
			const current = this.document!.schedules.find((item) => item.id === id);
			if (!current || current.state !== "active") continue;
			const slot = current.pending_slot ?? scheduleSlot(current, now);
			if (slot < 0) continue;
			const resuming = current.pending_slot !== null;
			if (!resuming && current.last_slot !== null && slot <= current.last_slot) continue;
			const goal =
				current.goal_id === null ? undefined : this.document!.goals.find((item) => item.id === current.goal_id);
			// A missing goal cannot be verified or paused, so its schedules stay idle.
			if (current.goal_id !== null && goal?.state !== "active") continue;
			const skipped = resuming ? current.last_skipped : slot - (current.last_slot ?? -1) - 1;
			const claimed = await this.update((draft) => {
				const schedule = this.schedule(draft, id);
				schedule.last_slot = slot;
				schedule.pending_slot = slot;
				schedule.last_skipped = skipped;
				if (!resuming) schedule.skipped_total += skipped;
				return structuredClone(schedule);
			});
			let task: NativeTask | undefined;
			let failure: string | undefined;
			if (goal && this.capacity(goal) < 1) failure = `Goal task limit reached (${goal.max_tasks})`;
			else {
				const request: HostTaskRequest = {
					definition: claimed.definition,
					input: claimed.input,
					key: scheduleKey(claimed, slot),
					...(claimed.model === null ? {} : { model: claimed.model }),
				};
				try {
					task = await host.spawn(request, claimed.owner, context);
				} catch (error) {
					failure = errorMessage(error);
				}
			}
			// The slot stays claimed on failure: the next attempt is the next slot, never a retry loop.
			await this.update((draft) => {
				const schedule = this.schedule(draft, id);
				schedule.pending_slot = null;
				if (task) {
					schedule.runs += 1;
					schedule.last_task_id = task.id;
					schedule.last_fired_at = now;
					if (schedule.max_runs !== null && schedule.runs >= schedule.max_runs) schedule.state = "completed";
					const linked =
						schedule.goal_id === null ? undefined : draft.goals.find((g) => g.id === schedule.goal_id);
					if (linked && !linked.task_ids.includes(task.id)) linked.task_ids.push(task.id);
				} else {
					schedule.failures += 1;
					schedule.last_failure = { slot, error: failure ?? "unknown failure", at: now };
				}
			});
			if (task) report.fired.push({ schedule_id: id, slot, task_id: task.id, skipped });
			else report.failed.push({ schedule_id: id, slot, error: failure ?? "unknown failure" });
		}
		return report;
	}

	/**
	 * Run every required check and accept the goal only on unanimous, explicit passes. Admission
	 * is serialized with other mutations; waiting for results is not, so ticks keep running.
	 */
	private async verify(
		payload: Payload,
		host: NativeHostApi,
		callerTask: string | null,
		context: Context,
	): Promise<GoalRecord> {
		fields(payload, ["id", "input"]);
		if (payload.input !== undefined && !isJsonValue(payload.input)) throw new Error("input must be JSON");
		const admitted = await this.exclusive(async (document) => {
			const goal = this.goal(document, payload.id);
			if (goal.state === "achieved") throw new Error("Goal is already achieved");
			if (goal.state === "paused") throw new Error("Goal is paused");
			if (this.capacity(goal) < goal.required_checks.length)
				throw new Error(
					`Goal task limit reached: verification needs ${goal.required_checks.length} tasks, ${Math.max(0, this.capacity(goal))} remain`,
				);
			const round = await this.update((draft) => {
				const target = this.goal(draft, goal.id);
				target.verify_rounds += 1;
				return target.verify_rounds;
			});
			const input = (payload.input as JsonValue | undefined) ?? { goal_id: goal.id, title: goal.title };
			const spawned: Array<{ definition: string; task?: NativeTask; error?: string }> = [];
			for (const [index, definition] of goal.required_checks.entries()) {
				try {
					const task = await host.spawn(
						{ definition, input, key: `goal:${goal.id}:verify:${round}:${index}` },
						callerTask,
						context,
					);
					spawned.push({ definition, task });
				} catch (error) {
					spawned.push({ definition, error: errorMessage(error) });
				}
			}
			await this.update((draft) => {
				const target = this.goal(draft, goal.id);
				for (const { task } of spawned)
					if (task && !target.task_ids.includes(task.id)) target.task_ids.push(task.id);
			});
			return { goalId: goal.id, round, spawned };
		});
		const checks = await Promise.all(
			admitted.spawned.map(async ({ definition, task, error }): Promise<CheckOutcome> => {
				if (!task)
					return {
						definition,
						task_id: null,
						status: "missing",
						passed: false,
						reason: `${definition}: ${error}`,
					};
				try {
					return checkOutcome(definition, task.id, await host.result(task.id));
				} catch (resultError) {
					return {
						definition,
						task_id: task.id,
						status: "missing",
						passed: false,
						reason: `${definition}: ${errorMessage(resultError)}`,
					};
				}
			}),
		);
		return this.exclusive(() =>
			this.update((draft) => {
				const goal = this.goal(draft, admitted.goalId);
				const reasons = checks.flatMap((check) => (check.reason ? [check.reason] : []));
				let status: GoalVerification["status"];
				if (checks.length === 0) {
					status = "unverified";
					reasons.push("Goal has no required checks");
				} else status = checks.every((check) => check.passed) ? "achieved" : "unachieved";
				// A newer round supersedes this one; its record is the one that counts.
				if (goal.verification === null || goal.verification.round < admitted.round) {
					goal.verification = { round: admitted.round, status, at: this.now(), checks, reasons };
					if (status === "achieved") goal.state = "achieved";
				}
				return structuredClone(goal);
			}),
		);
	}
}

export function createScheduleModule(options: {
	store: HostModuleStore;
	now?: () => number;
	tickIntervalMs?: number;
}): NativeHostModule {
	return new ScheduleModule(options);
}
