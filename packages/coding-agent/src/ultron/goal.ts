/**
 * Session goal (`/goal`), RLM-first: a person sets one objective, and a background agent (a `background-job@1` task
 * with its own REPL) works on it in its own cells until the goal ends. The root is never re-prompted for it; it stays
 * free for the user and hears the job's end as a `task_done` event.
 *
 * - Only a person sets, edits, pauses, resumes or clears the goal (`/goal ...`). The model reads it (`goal.get()`),
 *   tests with the host's check (`goal.check()`) and ends it (`goal.complete`, `goal.blocked`), naming the revision
 *   it worked on, so an edit made meanwhile is never completed by mistake.
 * - No wall-clock limit and no budget of its own: the job is untimed and runs under the goal's own usage root
 *   (`goal:<id>`), exempt from the per-root wall deadline and task cap (explicit token, turn and cost caps still
 *   apply). Attempts the job spawns are its children in that root.
 * - The goal ends when `goal.complete` passes (with a check command, only if the host's own run of it exits 0;
 *   without one the completion is recorded as unverified), when the model declares it blocked (after
 *   BLOCK_MIN_CHECKS checks when there is a check command), when STUCK_CHECKS failing checks in a row fail
 *   identically (paused), or when the user pauses or clears it.
 * - A job that ends while the goal is still active is started again only if its run made progress (a check whose
 *   result differed from the one before); otherwise the goal pauses. There is no restart count.
 *
 * The goal is the session value `ultron.module/goal`.
 */
import { createHash, randomUUID } from "node:crypto";
import type { Context } from "@ultron/agent-core";
import type { JsonValue } from "@ultron/chord";
import { BACKGROUND_CONTEXT } from "@ultron/chord/context";
import { readVersioned } from "./format-version.ts";
import type { HostModuleRequest, HostModuleStore, NativeHostModule } from "./rlm/host-module.ts";
import { GOAL_USAGE_ROOT_PREFIX } from "./usage.ts";

/** The worker request behind `/goal` (its payload: `{ args }`). */
export const GOAL_REQUEST = "goal.command";
/** Failing checks in a row with the same result that pause the goal. */
export const STUCK_CHECKS = 10;
/** Checks needed before the model may declare the goal blocked (when there is a check command). */
export const BLOCK_MIN_CHECKS = 3;
/** A check command that hangs is stopped after this long (the goal itself has no limit). */
const CHECK_TIMEOUT_SECONDS = 600;
const CHECK_OUTPUT_CHARS = 2000;
const TEXT_MAX_CHARS = 4000;
const EVIDENCE_MAX = 20;
const DOCUMENT_VERSION = 1;

export type GoalStatus = "active" | "paused" | "complete" | "blocked";

export interface GoalCheckRun {
	command: string;
	passed: boolean;
	exit_code: number | null;
	timed_out: boolean;
	/** The end of the command's output. */
	output: string;
	at: number;
}

export interface GoalRecord {
	id: string;
	/** Bumped by every edit a person makes (objective or check); the model names it when it ends the goal. */
	revision: number;
	objective: string;
	/** Shell command that must exit 0 for the goal to complete; null: completion is unverified. */
	check: string | null;
	status: GoalStatus;
	/** Why the goal is paused or blocked. */
	reason: string | null;
	/** The running job's task id (null when none runs). */
	job: string | null;
	/** Jobs started for this goal. */
	runs: number;
	/** Checks run for this goal, and failing ones in a row with the same result. */
	checks: number;
	stuck: number;
	/** Fingerprint of the last check's result, and whether the current run changed it. */
	last_signature: string | null;
	run_progress: boolean;
	last_check: GoalCheckRun | null;
	/** The previous job's final answer, for the next run's brief. */
	last_answer: string | null;
	created_at: number;
	updated_at: number;
	completion: { summary: string; evidence: string[]; verified: boolean; at: number } | null;
}

export interface GoalCheckResult {
	exit_code: number | null;
	timed_out: boolean;
	output: string;
}

export interface GoalDriverOptions {
	store: HostModuleStore;
	/** Run the check command in the session's working directory. */
	runCheck: (command: string, timeoutSeconds: number) => Promise<GoalCheckResult>;
	/** Start the goal's background job; returns its task id. */
	startJob: (prompt: string, options: { key: string; usageRoot: string }, context: Context) => Promise<string>;
	stopJob: (taskId: string, reason: string) => Promise<void>;
	/** The lane a job runs on (it may call goal.* besides the root). */
	laneOfJob: (taskId: string) => string | undefined;
	now?: () => number;
}

/** A check result's fingerprint: exit status and output, with durations and addresses left out. */
export function checkSignature(result: GoalCheckResult): string {
	const output = result.output
		.slice(-CHECK_OUTPUT_CHARS)
		.replace(/0x[0-9a-f]+/gi, "0x")
		.replace(/\b\d+(?:\.\d+)?\s?(?:ms|s|sec|secs|seconds)\b/g, "<t>")
		.replace(/\s+/g, " ")
		.trim();
	return createHash("sha256")
		.update(`${result.timed_out ? "timeout" : result.exit_code}\n${output}`)
		.digest("hex")
		.slice(0, 16);
}

/** The brief of the goal's background job. */
export function goalBrief(goal: GoalRecord): string {
	const check =
		goal.check === null
			? "There is no check command: when you call goal.complete, give concrete evidence (it is recorded as unverified)."
			: `Check: \`${goal.check}\`. \`await goal.check()\` runs it on the host and returns {passed, exit_code, output}; goal.complete runs it again and succeeds only if it exits 0.`;
	return [
		`You are working on the session goal (revision ${goal.revision}) on your own, as a background job: nobody is waiting on your turns, and nothing limits your time.`,
		`Objective: ${goal.objective}`,
		check,
		"Work in your REPL: keep what you tried and learned in Python (`state['goal']`), change the code, test, and use the result to choose the next step. Delegate independent attempts with `rlm.spawn(brief, name=..., worktree=True)` and `await rlm.merge(...)` the one that works.",
		`Keep going until \`await goal.complete(${goal.revision}, summary, evidence=[...])\` succeeds. Only if something outside you blocks the goal after real attempts: \`await goal.blocked(${goal.revision}, reason)\`. Ten identical failing checks in a row pause the goal, so change your approach when a check keeps failing the same way. The user may change the goal: \`await goal.get()\` shows the current revision.`,
		...(goal.last_answer === null ? [] : [`The previous run on this goal ended with: ${goal.last_answer}`]),
	].join("\n\n");
}

/** A short status for `/goal`. */
export function goalStatusText(goal: GoalRecord | null): string {
	if (goal === null) return "No goal. Set one with /goal <objective>.";
	const lines = [
		`Goal (revision ${goal.revision}, ${goal.status}${goal.reason ? `: ${goal.reason}` : ""}): ${goal.objective}`,
		`Check: ${goal.check ?? "none (completion is unverified)"}. Runs: ${goal.runs}. Checks: ${goal.checks}${goal.stuck > 1 ? ` (${goal.stuck} identical failures in a row)` : ""}.`,
	];
	if (goal.job !== null) lines.push(`Job: ${goal.job} (running).`);
	if (goal.last_check)
		lines.push(
			`Last check: ${goal.last_check.passed ? "passed" : goal.last_check.timed_out ? "timed out" : `exit ${goal.last_check.exit_code}`} (${goal.last_check.command}).`,
		);
	if (goal.completion)
		lines.push(
			`Completed${goal.completion.verified ? " (check passed)" : " (unverified)"}: ${goal.completion.summary}`,
		);
	return lines.join("\n");
}

export class GoalDriver {
	readonly #options: GoalDriverOptions;
	#goal: GoalRecord | null = null;
	#loaded?: Promise<void>;
	#writes: Promise<void> = Promise.resolve();
	#queue: Promise<unknown> = Promise.resolve();
	#closed = false;

	constructor(options: GoalDriverOptions) {
		this.#options = options;
	}

	get #now(): number {
		return (this.#options.now ?? Date.now)();
	}

	/** Host module for the kernel's `goal.*`. */
	readonly module: NativeHostModule = {
		prefixes: ["goal."],
		start: () => this.#load(),
		handle: (request) => this.#handle(request),
	};

	/** The goal now (a copy), after the journal loads. */
	async get(): Promise<GoalRecord | null> {
		await this.#load();
		return this.#goal === null ? null : structuredClone(this.#goal);
	}

	/** Wait for queued goal changes and journal writes (tests, shutdown). */
	async settled(): Promise<void> {
		await this.#queue.catch(() => {});
		await this.#writes;
	}

	/** Run `work` after earlier goal changes (job starts and stops are serialized). */
	#serial<T>(work: () => Promise<T>): Promise<T> {
		const next = this.#queue.then(work);
		this.#queue = next.catch(() => {});
		return next;
	}

	/** `/goal [objective | pause | resume | clear | check [command] | status]`; returns the text to show. */
	command(args: string, context: Context = BACKGROUND_CONTEXT): Promise<string> {
		return this.#serial(async () => {
			await this.#load();
			const text = args.trim();
			const goal = this.#goal;
			if (text === "" || text === "status") return goalStatusText(goal);
			if (text === "clear") {
				if (goal === null) return "No goal to clear.";
				await this.#stopJob(goal, "goal cleared");
				this.#goal = null;
				this.#persist();
				return "Goal cleared.";
			}
			if (text === "pause") {
				if (goal?.status !== "active") return goalStatusText(goal);
				await this.#stopJob(goal, "goal paused");
				this.#setStatus(goal, "paused", "paused by you; /goal resume continues");
				return goalStatusText(goal);
			}
			if (text === "resume") {
				if (goal === null || goal.status === "complete" || goal.status === "active") return goalStatusText(goal);
				this.#setStatus(goal, "active", null);
				goal.stuck = 0;
				await this.#startJob(goal, context);
				return goalStatusText(goal);
			}
			if (text === "check" || text.startsWith("check ")) {
				if (goal === null) return "Set a goal first: /goal <objective>.";
				const command = text.slice("check".length).trim();
				goal.check = command === "" ? null : command.slice(0, TEXT_MAX_CHARS);
				goal.revision += 1;
				goal.stuck = 0;
				goal.last_signature = null;
				goal.updated_at = this.#now;
				this.#persist();
				return goalStatusText(goal);
			}
			if (goal !== null) await this.#stopJob(goal, "goal replaced");
			const now = this.#now;
			const next: GoalRecord = {
				id: `goal-${randomUUID().slice(0, 8)}`,
				revision: (goal?.revision ?? 0) + 1,
				objective: text.slice(0, TEXT_MAX_CHARS),
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
				created_at: now,
				updated_at: now,
				completion: null,
			};
			this.#goal = next;
			await this.#startJob(next, context);
			return `${goalStatusText(next)}\nAdd the check that decides when it is done: /goal check <command>.`;
		});
	}

	/** A task ended (the worker's task-end observer): a goal job that ended early is started again or the goal pauses. */
	taskEnded(task: { id: string; result?: { status?: string; value?: unknown; error?: unknown } | null }): void {
		void this.#serial(async () => {
			await this.#load();
			const goal = this.#goal;
			if (this.#closed || goal === null || goal.job !== task.id) return;
			goal.job = null;
			const answer = typeof task.result?.value === "string" ? task.result.value : String(task.result?.error ?? "");
			goal.last_answer = answer.length > 1000 ? `${answer.slice(0, 999)}…` : answer || null;
			goal.updated_at = this.#now;
			if (goal.status !== "active") {
				this.#persist();
				return;
			}
			if (goal.run_progress) {
				await this.#startJob(goal, BACKGROUND_CONTEXT);
				return;
			}
			this.#setStatus(goal, "paused", "the goal job ended without progress; /goal resume continues");
		});
	}

	async #startJob(goal: GoalRecord, context: Context): Promise<void> {
		if (this.#closed) return;
		goal.runs += 1;
		goal.run_progress = false;
		goal.updated_at = this.#now;
		try {
			goal.job = await this.#options.startJob(
				goalBrief(goal),
				{ key: `${goal.id}:${goal.runs}`, usageRoot: `${GOAL_USAGE_ROOT_PREFIX}${goal.id}` },
				context,
			);
		} catch (error) {
			goal.job = null;
			goal.status = "paused";
			goal.reason = `the goal job could not start: ${error instanceof Error ? error.message : String(error)}`;
		}
		this.#persist();
	}

	async #stopJob(goal: GoalRecord, reason: string): Promise<void> {
		const job = goal.job;
		goal.job = null;
		if (job !== null) await this.#options.stopJob(job, reason).catch(() => {});
	}

	#setStatus(goal: GoalRecord, status: GoalStatus, reason: string | null): void {
		goal.status = status;
		goal.reason = reason;
		goal.updated_at = this.#now;
		this.#persist();
	}

	async #handle(request: HostModuleRequest): Promise<unknown> {
		await this.#load();
		const { type, payload, caller } = request;
		if (type === "goal.get") {
			fields(payload, []);
			return this.#goal === null ? null : publicGoal(this.#goal);
		}
		const goal = this.#goal;
		if (goal === null) throw new Error("There is no session goal");
		const job = goal.job === null ? undefined : this.#options.laneOfJob(goal.job);
		if (caller.lane !== "main" && caller.lane !== job)
			throw new Error("Only the goal's job or the root agent works the session goal; subagents report to it");
		if (type === "goal.check") {
			fields(payload, []);
			if (goal.check === null)
				throw new Error("This goal has no check command (the user sets one with /goal check)");
			const run = await this.#runCheck(goal);
			if (goal.status === "active" && goal.stuck >= STUCK_CHECKS) {
				this.#setStatus(
					goal,
					"paused",
					`${goal.stuck} failing checks in a row gave the same result; /goal resume continues`,
				);
				// Stopped after this reply reaches the job's cell.
				const stopping = goal.job;
				goal.job = null;
				if (stopping !== null)
					setTimeout(() => void this.#options.stopJob(stopping, "goal stuck").catch(() => {}), 100).unref();
				return { ...run, paused: true, reason: goal.reason };
			}
			return { ...run, paused: false };
		}
		if (type === "goal.complete") {
			fields(payload, ["revision", "summary", "evidence"]);
			this.#current(goal, payload.revision);
			const summary = text(payload.summary, "summary");
			const evidence = evidenceList(payload.evidence);
			if (goal.check !== null) {
				const run = await this.#runCheck(goal);
				if (!run.passed)
					return {
						complete: false,
						status: goal.status,
						check: run,
						message: "The check failed, so the goal is not complete: fix the cause and call goal.complete again.",
					};
			}
			goal.completion = { summary, evidence, verified: goal.check !== null, at: this.#now };
			this.#setStatus(goal, "complete", null);
			return { complete: true, status: goal.status, verified: goal.check !== null, check: goal.last_check };
		}
		if (type === "goal.blocked") {
			fields(payload, ["revision", "reason"]);
			this.#current(goal, payload.revision);
			const reason = text(payload.reason, "reason");
			if (goal.check !== null && goal.checks < BLOCK_MIN_CHECKS)
				throw new Error(
					`goal.blocked is accepted after ${BLOCK_MIN_CHECKS} checks (${goal.checks} so far): keep working, test with goal.check(), and try another approach`,
				);
			this.#setStatus(goal, "blocked", reason);
			return { status: goal.status, reason };
		}
		throw new Error(`Ultron RLM host request is not wired: ${type}`);
	}

	async #runCheck(goal: GoalRecord): Promise<GoalCheckRun> {
		const command = goal.check!;
		const result = await this.#options.runCheck(command, CHECK_TIMEOUT_SECONDS);
		const passed = result.exit_code === 0 && !result.timed_out;
		const signature = checkSignature(result);
		if (signature !== goal.last_signature) goal.run_progress = true;
		goal.stuck = passed ? 0 : signature === goal.last_signature ? goal.stuck + 1 : 1;
		goal.last_signature = signature;
		goal.checks += 1;
		goal.last_check = {
			command,
			passed,
			exit_code: result.exit_code,
			timed_out: result.timed_out,
			output: tail(result.output),
			at: this.#now,
		};
		goal.updated_at = this.#now;
		this.#persist();
		return goal.last_check;
	}

	/** The model's call must name the active goal's current revision. */
	#current(goal: GoalRecord, revision: unknown): void {
		if (goal.status !== "active") throw new Error(`The session goal is ${goal.status}, not active`);
		if (revision !== goal.revision)
			throw new Error(
				`The goal is at revision ${goal.revision} (the user changed it): read it with goal.get() and work on that`,
			);
	}

	#load(): Promise<void> {
		this.#loaded ??= (async () => {
			const saved = readVersioned("ultron.module/goal", await this.#options.store.read());
			const goal =
				saved && typeof saved === "object" && !Array.isArray(saved) && saved.goal && typeof saved.goal === "object"
					? (saved.goal as unknown as GoalRecord)
					: null;
			// A job does not outlive the worker that ran it: an active goal whose job was running is paused.
			if (goal !== null && goal.status === "active" && goal.job !== null) {
				goal.job = null;
				goal.status = "paused";
				goal.reason = "Ultron restarted while the goal job ran; /goal resume continues";
			}
			this.#goal ??= goal;
		})();
		return this.#loaded;
	}

	#persist(): void {
		const snapshot = structuredClone({ version: DOCUMENT_VERSION, goal: this.#goal } as unknown as JsonValue);
		this.#writes = this.#writes.then(() => this.#options.store.write(snapshot)).catch(() => {});
	}

	async close(): Promise<void> {
		this.#closed = true;
		await this.settled();
	}
}

function publicGoal(goal: GoalRecord): JsonValue {
	const { id, revision, objective, check, status, reason, runs, checks, stuck, last_check, completion } = goal;
	return structuredClone({
		id,
		revision,
		objective,
		check,
		status,
		reason,
		runs,
		checks,
		stuck,
		last_check,
		completion,
	}) as unknown as JsonValue;
}

function fields(payload: Record<string, unknown>, allowed: readonly string[]): void {
	for (const key of Object.keys(payload)) if (!allowed.includes(key)) throw new Error(`Unknown payload field: ${key}`);
}

function text(value: unknown, name: string): string {
	if (typeof value !== "string" || !value.trim()) throw new Error(`${name} must be a non-empty string`);
	return value.slice(0, TEXT_MAX_CHARS);
}

function evidenceList(value: unknown): string[] {
	if (!Array.isArray(value) || value.length === 0)
		throw new Error(
			'evidence must be a non-empty list of strings: commands with their outcome ("npm test: exit 0") or files with lines',
		);
	if (!value.every((item) => typeof item === "string" && item.trim()))
		throw new Error("every evidence item must be a non-empty string");
	return (value as string[]).slice(0, EVIDENCE_MAX).map((item) => item.slice(0, TEXT_MAX_CHARS));
}

function tail(output: string): string {
	return output.length > CHECK_OUTPUT_CHARS ? `…${output.slice(output.length - CHECK_OUTPUT_CHARS + 1)}` : output;
}
