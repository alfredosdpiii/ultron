/**
 * Research-loop brake for the root agent. A model given an open-ended request can keep calling tools
 * (web search, subagents) for many rounds without answering. After `threshold` consecutive tool-call
 * rounds in one run, the agent is steered once to answer with what it has; at twice the threshold it is
 * steered again, more firmly. It never aborts: work already done is kept, and Esc still aborts.
 */
export const DEFAULT_TOOL_ROUNDS_NUDGE = 10;

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

export class ToolRoundNudger {
	readonly #threshold: number;
	readonly #steer: (message: string) => Promise<unknown>;
	readonly #rounds = new Map<string, number>();

	constructor(threshold: number, steer: (message: string) => Promise<unknown>) {
		this.#threshold = threshold;
		this.#steer = steer;
	}

	/** Record one model turn of a run; `toolCalls` is how many tool calls that turn made. */
	turnEnded(runId: string, toolCalls: number): void {
		if (this.#threshold <= 0) return;
		// A turn that answers without tools ends the streak.
		const rounds = toolCalls > 0 ? (this.#rounds.get(runId) ?? 0) + 1 : 0;
		this.#rounds.set(runId, rounds);
		if (rounds === this.#threshold || rounds === this.#threshold * 2) {
			void this.#steer(nudgeMessage(rounds, rounds === this.#threshold * 2)).catch(() => {});
		}
	}

	runEnded(runId: string): void {
		this.#rounds.delete(runId);
	}
}
