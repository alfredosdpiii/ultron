/**
 * Tool-loop detection for a lane's cells. A lane that keeps failing, or flips between two near-identical cells,
 * is usually retrying variants of one idea instead of finding out why it fails. The detector watches each lane's
 * cells and trips on:
 * - `consecutive` failed cells in a row (a cell fails when it raised, or when its last `bash` exited non-zero);
 * - an alternation A, B, A, B (`alternation` cells) between two cells that differ only in whitespace and numbers,
 *   when at least half of those cells failed;
 * - `total` failed cells in one run, however they are spread.
 *
 * It trips at most twice per phase: once, then once more (escalated) at twice the threshold if the lane goes on.
 * A phase ends with a successful cell that is not a repeat of the cell before last; `reset` (the lane's run ended)
 * starts over. CellHints turns a trip into the `stuck-loop` hint, so `hints.mute("stuck-loop")` silences it.
 */
import { createHash } from "node:crypto";

export interface LoopLimits {
	/** Failed cells in a row. */
	readonly consecutive: number;
	/** Cells in an A, B, A, B run. */
	readonly alternation: number;
	/** Failed cells in one run. */
	readonly total: number;
}

export const DEFAULT_LOOP_LIMITS: LoopLimits = { consecutive: 3, alternation: 4, total: 8 };

export type LoopReason = "consecutive" | "alternation" | "total";

export interface LoopTrip {
	/** 1 the first time in a phase, 2 the escalation. */
	readonly level: 1 | 2;
	readonly reason: LoopReason;
	/** The count that tripped: failures in a row, cells in the alternation, or failures in the run. */
	readonly count: number;
	/** The exception type when every failure in the streak raised the same one. */
	readonly ename?: string;
}

type Cell = { readonly hash: string; readonly failed: boolean };

type LaneLoop = {
	history: Cell[];
	consecutive: number;
	/** Exception type shared by the current streak, "" once they differ. */
	streakEname?: string;
	total: number;
	/** Trips in the current phase. */
	level: 0 | 1 | 2;
	/** Trips caused by the run total (never reset by a phase). */
	totalLevel: 0 | 1 | 2;
};

/** Code with whitespace and digits removed, so cells differing only in those compare equal. */
export function normalizeCell(code: string): string {
	return code.replace(/\s+/g, "").replace(/[0-9]+/g, "");
}

function cellHash(code: string): string {
	return createHash("sha1").update(normalizeCell(code)).digest("hex").slice(0, 16);
}

/** Length of the A, B, A, B run (A differs from B) that ends the history; 0 when there is none. */
export function alternationLength(hashes: readonly string[]): number {
	const n = hashes.length;
	if (n < 2 || hashes[n - 1] === hashes[n - 2]) return 0;
	let length = 2;
	for (let i = n - 3; i >= 0 && hashes[i] === hashes[i + 2]; i--) length++;
	return length;
}

export class ToolLoopDetector {
	readonly #limits: LoopLimits;
	readonly #lanes = new Map<string, LaneLoop>();

	constructor(limits: Partial<LoopLimits> = {}) {
		this.#limits = { ...DEFAULT_LOOP_LIMITS, ...limits };
	}

	/** Trips of the lane's current phase (0 when it is not stuck). */
	level(lane: string): 0 | 1 | 2 {
		return this.#lanes.get(lane)?.level ?? 0;
	}

	/** A cell on `lane` ended; returns a trip when this cell makes the lane look stuck. */
	cellEnded(lane: string, code: string, failed: boolean, ename?: string): LoopTrip | undefined {
		let state = this.#lanes.get(lane);
		if (!state) {
			state = { history: [], consecutive: 0, total: 0, level: 0, totalLevel: 0 };
			this.#lanes.set(lane, state);
		}
		const limits = this.#limits;
		const hash = cellHash(code);
		state.history.push({ hash, failed });
		if (state.history.length > limits.alternation * 2 + 2) state.history.shift();
		if (failed) {
			state.consecutive++;
			state.total++;
			state.streakEname =
				state.consecutive === 1 ? (ename ?? "") : state.streakEname === (ename ?? "") ? state.streakEname : "";
		} else {
			state.consecutive = 0;
			state.streakEname = undefined;
		}
		const history = state.history;
		const repeat = history.length >= 3 && history[history.length - 3]!.hash === hash;
		// Progress: a success that is not part of a back-and-forth ends the phase.
		if (!failed && !repeat) state.level = 0;

		const run = alternationLength(history.map((cell) => cell.hash));
		const runFailures = history.slice(history.length - run).filter((cell) => cell.failed).length;
		const alternating = run >= limits.alternation && runFailures * 2 >= run;
		const trip = (level: 1 | 2, reason: LoopReason, count: number): LoopTrip => {
			state.level = level;
			if (reason === "total") state.totalLevel = level;
			const shared = reason === "consecutive" && state.streakEname ? { ename: state.streakEname } : {};
			return { level, reason, count, ...shared };
		};
		if (state.level === 0) {
			if (limits.consecutive > 0 && state.consecutive >= limits.consecutive)
				return trip(1, "consecutive", state.consecutive);
			if (limits.alternation > 0 && alternating) return trip(1, "alternation", run);
		} else if (state.level === 1) {
			if (limits.consecutive > 0 && state.consecutive >= limits.consecutive * 2)
				return trip(2, "consecutive", state.consecutive);
			if (limits.alternation > 0 && alternating && run >= limits.alternation * 2) return trip(2, "alternation", run);
		}
		// The run total spans phases: it trips once, and escalates once, per run.
		if (limits.total > 0 && failed) {
			if (state.totalLevel === 0 && state.level === 0 && state.total >= limits.total)
				return trip(1, "total", state.total);
			if (state.totalLevel === 1 && state.level < 2 && state.total >= limits.total * 2)
				return trip(2, "total", state.total);
		}
		return undefined;
	}

	/** The lane's run ended: its next run starts clean. */
	reset(lane: string): void {
		this.#lanes.delete(lane);
	}
}

/** The `stuck-loop` hint text for a trip. */
export function loopHintText(trip: LoopTrip): string {
	const what =
		trip.reason === "consecutive"
			? `The last ${trip.count} cells failed${trip.ename ? ` (${trip.ename} each time)` : ""}`
			: trip.reason === "alternation"
				? `The last ${trip.count} cells alternate between two near-identical versions`
				: `${trip.count} cells have failed in this run`;
	return trip.level === 1
		? `${what}; you look stuck. Before the next cell, state three different hypotheses for why it fails, then run one cell that tests the most likely one instead of retrying a variant.`
		: `${what}, even after the last hint. Stop repeating this approach: name three hypotheses you have not tested yet and test the most likely; if none holds, say what blocks you.`;
}
