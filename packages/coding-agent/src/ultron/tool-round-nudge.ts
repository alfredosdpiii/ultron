/**
 * Research-loop brake for the root agent. A model given an open-ended request can keep calling tools
 * (web search, subagents) for many rounds without answering. After `threshold` consecutive tool-call
 * rounds in one run, the agent is steered once to answer with what it has; at twice the threshold it is
 * steered again, more firmly. It never aborts: work already done is kept, and Esc still aborts.
 *
 * While subagents or tasks the root started are still running, busy rounds are usually the root checking on
 * them (listing their files, tailing their logs) instead of waiting, which costs a model turn each time while
 * waiting costs nothing. So after `waitRounds` consecutive tool rounds with work still running, and in place
 * of the brake while it runs, the root is steered to do only separate work of its own and otherwise wait:
 * `await rlm.collect(...)`, or end its turn and be woken by the completion events. When the last of that work
 * ends, the streak starts over: reconciling the results is a new phase, not more of the same research.
 */
export const DEFAULT_TOOL_ROUNDS_NUDGE = 10;

/** Consecutive tool rounds with subagents still running before the root is told to wait instead of checking. */
export const DEFAULT_WAIT_ROUNDS = 3;

export function toolRoundsNudgeFromEnv(value: string | undefined): number {
	if (value === undefined || value.trim() === "") return DEFAULT_TOOL_ROUNDS_NUDGE;
	const parsed = Number(value);
	return Number.isInteger(parsed) && parsed >= 0 ? parsed : DEFAULT_TOOL_ROUNDS_NUDGE;
}

export function nudgeMessage(rounds: number, final: boolean): string {
	return final
		? `[Ultron] You have now used ${rounds} rounds of tool calls in this turn. Stop calling tools and write your answer now from what you already have. List anything still unknown instead of searching further.`
		: `[Ultron] You have used ${rounds} rounds of tool calls in this turn without answering. Unless one more specific call is essential, stop gathering and answer now with what you have, noting what remains unknown.`;
}

/**
 * The steer while `running` subagents or tasks the root started have not ended. With completion events off
 * (ULTRON_ASYNC_EVENTS=off) nothing wakes an idle root, so the only way to wait is `rlm.collect`.
 */
export function waitNudgeMessage(running: number, asyncEvents = true, nextCall = false): string {
	const what = running === 1 ? "1 subagent or task you started is" : `${running} subagents or tasks you started are`;
	if (asyncEvents && nextCall)
		return `[Ultron] ${what} still running; each end is reported as a \`child_done\` (or \`task_done\`) event at the top of a later rlm result. Do only separate work of your own and do not check on them through their files, logs or progress. If nothing of your own is left, wait for free: \`await rlm.collect(handles)\` (nothing wakes you after you reply).`;
	return asyncEvents
		? `[Ultron] ${what} still running; each result comes to you as a \`child_done\` (or \`task_done\`) event. Do only separate work of your own and do not check on them through their files, logs or progress. If nothing of your own is left, wait for free: \`await rlm.collect(handles)\`, or end your turn and the events wake you.`
		: `[Ultron] ${what} still running. Do only separate work of your own and do not check on them through their files, logs or progress. If nothing of your own is left, wait for free with \`await rlm.collect(handles)\` (or \`await agents.result(id)\`).`;
}

export interface ToolRoundNudgerOptions {
	/** Whether completions are announced (ULTRON_ASYNC_EVENTS); picks the wait wording. */
	readonly asyncEvents?: boolean;
	/** Tool rounds with work still running before the wait steer; 0 disables it (the brake still adapts). */
	readonly waitRounds?: number;
	/** Nothing wakes the root between turns (Claude Code over MCP): the wait steer says to collect, not to end the turn. */
	readonly nextCall?: boolean;
}

type RunRounds = {
	/** Consecutive tool rounds (the brake's count). */
	rounds: number;
	/** Consecutive tool rounds that ended with root-started work still running. */
	waiting: number;
	/** Work was still running when the previous round ended. */
	wasRunning: boolean;
	/** The wait steer fired in the current running phase. */
	waitNudged: boolean;
};

export class ToolRoundNudger {
	readonly #threshold: number;
	readonly #steer: (message: string) => Promise<unknown>;
	readonly #asyncEvents: boolean;
	readonly #waitRounds: number;
	readonly #nextCall: boolean;
	readonly #runs = new Map<string, RunRounds>();

	constructor(threshold: number, steer: (message: string) => Promise<unknown>, options: ToolRoundNudgerOptions = {}) {
		this.#threshold = threshold;
		this.#steer = steer;
		this.#asyncEvents = options.asyncEvents !== false;
		this.#waitRounds = options.waitRounds ?? DEFAULT_WAIT_ROUNDS;
		this.#nextCall = options.nextCall === true;
	}

	/**
	 * Record one model turn of a run; `toolCalls` is how many tool calls that turn made, `running` how many
	 * subagents or tasks the root started are still running as it ends.
	 */
	turnEnded(runId: string, toolCalls: number, running = 0): void {
		if (this.#threshold <= 0) return;
		const run = this.#runs.get(runId) ?? { rounds: 0, waiting: 0, wasRunning: false, waitNudged: false };
		this.#runs.set(runId, run);
		// A turn that answers without tools ends the streak, and so does the end of the last running child.
		if (toolCalls === 0) run.rounds = 0;
		else run.rounds = run.wasRunning && running === 0 ? 1 : run.rounds + 1;
		if (running === 0) run.waitNudged = false;
		run.waiting = running > 0 && toolCalls > 0 ? run.waiting + 1 : 0;
		run.wasRunning = running > 0;
		const brake = run.rounds === this.#threshold || run.rounds === this.#threshold * 2;
		const wait = running > 0 && !run.waitNudged && this.#waitRounds > 0 && run.waiting >= this.#waitRounds;
		if (running > 0 && (brake || wait)) {
			run.waitNudged = true;
			this.#send(waitNudgeMessage(running, this.#asyncEvents, this.#nextCall));
		} else if (brake) this.#send(nudgeMessage(run.rounds, run.rounds === this.#threshold * 2));
	}

	runEnded(runId: string): void {
		this.#runs.delete(runId);
	}

	#send(message: string): void {
		void this.#steer(message).catch(() => {});
	}
}

/**
 * Procedural-memory hint for the root agent. After `threshold` consecutive tool rounds in one run whose
 * tool calls all succeeded, the agent is steered once, in one line, to consider saving the procedure as a
 * tested code skill. A round with a failed tool call or a turn without tools resets the streak, and so does a
 * round that ends with subagents or tasks the root started still running: those rounds are coordination (or
 * checking on the children), not a procedure worth keeping.
 */
export const DEFAULT_SKILL_NUDGE = 8;

export function skillNudgeFromEnv(value: string | undefined): number {
	if (value === undefined || value.trim() === "") return DEFAULT_SKILL_NUDGE;
	if (["off", "false", "no"].includes(value.trim().toLowerCase())) return 0;
	const parsed = Number(value);
	return Number.isInteger(parsed) && parsed >= 0 ? parsed : DEFAULT_SKILL_NUDGE;
}

export function skillNudgeMessage(rounds: number): string {
	return `[Ultron] ${rounds} tool rounds in a row succeeded. If this procedure will recur, once it works save it as a tested skill with \`await skills.propose_code(name, source, test_source, evidence)\`; otherwise carry on.`;
}

export class SkillExtractionNudger {
	readonly #threshold: number;
	readonly #steer: (message: string) => Promise<unknown>;
	readonly #streaks = new Map<string, number>();
	readonly #nudged = new Set<string>();

	constructor(threshold: number, steer: (message: string) => Promise<unknown>) {
		this.#threshold = threshold;
		this.#steer = steer;
	}

	/** `toolCalls` made in the turn, `failed` of whose results were errors; `running` root-started work still running. */
	turnEnded(runId: string, toolCalls: number, failed: number, running = 0): void {
		if (this.#threshold <= 0 || this.#nudged.has(runId)) return;
		const streak = toolCalls > 0 && failed === 0 && running === 0 ? (this.#streaks.get(runId) ?? 0) + 1 : 0;
		this.#streaks.set(runId, streak);
		if (streak < this.#threshold) return;
		this.#nudged.add(runId);
		this.#streaks.delete(runId);
		void this.#steer(skillNudgeMessage(streak)).catch(() => {});
	}

	runEnded(runId: string): void {
		this.#streaks.delete(runId);
		this.#nudged.delete(runId);
	}
}
