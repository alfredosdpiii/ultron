/**
 * Situational hints (nano-rlm's supervisor hints): the host watches how a lane uses the runtime and ends a cell's
 * result with at most one short tagged line, `[hint:<tag>] ...`, when something worth knowing happened in that cell
 * (a command detached into a job, a long wait on a job whose completion would have arrived as an event, polling,
 * truncated output, a large file read into a string, the same exception three cells in a row, a lane that looks stuck
 * in a loop of failing or alternating cells; see loop-detector.ts). A hint is part of
 * the rlm tool result, which is appended, so it never changes an earlier message.
 *
 * A lane mutes tags it has understood with `await hints.mute(tag)` (`hints.unmute`, `hints.muted()`); each tag
 * fires at most `maxPerTag` (3) times per lane in a session. Mutes and counts are the session value
 * `ultron.module/hints`. `ULTRON_HINTS=off` disables hints; `ULTRON_HINTS_MAX` changes the per-tag cap.
 */
import type { JsonValue } from "@ultron/chord";
import { readVersioned } from "../format-version.ts";
import type { HostModuleStore, NativeHostModule } from "./host-module.ts";
import { type LoopLimits, loopHintText, ToolLoopDetector } from "./loop-detector.ts";

export const HINT_TAGS = [
	"stuck-loop",
	"job-detached",
	"blocked-on-job",
	"poll-loop",
	"repeated-failure",
	"large-read",
	"output-truncated",
] as const;
export type HintTag = (typeof HINT_TAGS)[number];

export const DEFAULT_HINT_MAX_PER_TAG = 3;
export const DEFAULT_BLOCKED_SECONDS = 60;
export const DEFAULT_READ_HANDLE_BYTES = 256 * 1024;
/** Same exception type this many cells in a row. */
const FAILURE_STREAK = 3;
/** Same status call this many times in one cell. */
const POLL_REPEATS = 3;
const JOURNAL_VERSION = 1;

/**
 * Waits that hold the cell until detached work ends. Waiting for subagents (`rlm.collect`, `agents.result`) is
 * not among them: once a root has no work of its own left, that wait is what the guide asks for (it costs no
 * model turns), so it earns no hint.
 */
const BLOCKING_WAITS = new Set(["shell.result", "background.result"]);

/** Status checks of running work; repeating one is polling. */
const STATUS_CALLS = new Set([
	"shell.get",
	"shell.result",
	"shell.list",
	"background.inspect",
	"background.list",
	"agents.inspect",
	"agents.tasks",
	"agents.status",
	"rlm.list_subagents",
]);

const TRUNCATION_MARKER = /\[\.\.\. \d+ bytes truncated \.\.\.\]/;
const FILE_READ = /\.read\(\s*\)|\.read_text\(|\.read_bytes\(|\.readlines\(|\bopen\(/;
const BIG_STRING = /^<str: ([\d,]+) chars/m;
const SLEEP = /\b(?:time|asyncio)\.sleep\(/;
const LOOP = /^\s*(?:while|for|async\s+for)\b/m;
const STATUS_CHECK =
	/\.running\b|\.status\b|\.done\(|\.result\(|\.inspect\(|rlm\.jobs\(|rlm\.job\(|list_subagents\(|agents\.tasks\(|agents\.status\(|background\.list\(/;

export function hintsEnabled(env: NodeJS.ProcessEnv = process.env): boolean {
	const value = env.ULTRON_HINTS?.trim().toLowerCase();
	return !value || !["off", "0", "false", "no"].includes(value);
}

export function hintMaxPerTag(env: NodeJS.ProcessEnv = process.env): number {
	const raw = env.ULTRON_HINTS_MAX?.trim();
	const value = raw ? Number(raw) : Number.NaN;
	return Number.isInteger(value) && value >= 0 ? value : DEFAULT_HINT_MAX_PER_TAG;
}

export function readHandleBytes(env: NodeJS.ProcessEnv = process.env): number {
	const value = Number(env.ULTRON_READ_HANDLE_BYTES?.trim());
	return Number.isInteger(value) && value > 0 ? value : DEFAULT_READ_HANDLE_BYTES;
}

export interface CellHintsOptions {
	store: HostModuleStore;
	/** ULTRON_HINTS; default on. */
	enabled?: boolean;
	/** Times each tag may fire per lane in a session. */
	maxPerTag?: number;
	/** Whether completions are announced (ULTRON_ASYNC_EVENTS): the wait and polling hints point at events. */
	asyncEvents?: boolean;
	/**
	 * `next-call`: nothing wakes the root lane ("main") between turns (Claude Code drives it over MCP), so its hints
	 * point at the next rlm result instead of a new turn, and waiting for a needed result earns no hint.
	 */
	rootDelivery?: "wake" | "next-call";
	/** A cell waiting at least this long on detached work gets `blocked-on-job`. */
	blockedSeconds?: number;
	/** `read` returns a handle above this size (ULTRON_READ_HANDLE_BYTES). */
	readHandleBytes?: number;
	/** Thresholds of the stuck-loop detector. */
	loopLimits?: Partial<LoopLimits>;
	now?: () => number;
}

/** How a cell ended, as the rlm tool reports it. */
export interface CellOutcome {
	/** The result text (after truncation). */
	readonly text: string;
	/** Exception type when the cell failed. */
	readonly ename?: string;
}

type Detached = { id: string; waited: number | undefined };

type Cell = {
	code: string;
	detached: Detached[];
	waits: Array<[number, number]>;
	waitTypes: Set<string>;
	statusCalls: Map<string, number>;
	/** The cell's last `bash` command exited non-zero. */
	bashFailed: boolean;
};

type LaneState = {
	cell?: Cell;
	/** Status calls of the lane's previous cell. */
	previousStatus: Set<string>;
	failure?: { ename: string; count: number };
};

type Persisted = { muted: string[]; fired: Partial<Record<HintTag, number>> };

export class CellHints {
	readonly #options: CellHintsOptions;
	readonly #lanes = new Map<string, LaneState>();
	readonly #loops: ToolLoopDetector;
	#persisted: Record<string, Persisted> = {};
	#loaded?: Promise<void>;
	#writes: Promise<void> = Promise.resolve();

	constructor(options: CellHintsOptions) {
		this.#options = options;
		this.#loops = new ToolLoopDetector(options.loopLimits);
	}

	get enabled(): boolean {
		return this.#options.enabled !== false;
	}

	get #now(): number {
		return (this.#options.now ?? Date.now)();
	}

	/** `hints.mute`, `hints.unmute`, `hints.muted` for the calling lane. */
	readonly module: NativeHostModule = {
		prefixes: ["hints."],
		start: () => this.#load(),
		handle: async (request) => {
			await this.#load();
			const lane = this.#lane(request.caller.lane);
			const tags = request.type === "hints.muted" ? [] : this.#tags(request.payload.tags);
			if (request.type === "hints.mute") lane.muted = [...new Set([...lane.muted, ...tags])].sort();
			else if (request.type === "hints.unmute")
				lane.muted = lane.muted.filter((tag) => !(tags as string[]).includes(tag));
			else if (request.type !== "hints.muted")
				throw new Error(`Ultron RLM host request is not wired: ${request.type}`);
			if (request.type !== "hints.muted") this.#persist();
			return { muted: [...lane.muted] };
		},
	};

	#tags(value: unknown): HintTag[] {
		if (!Array.isArray(value) || value.length === 0) throw new Error("hint tags must be one or more strings");
		for (const tag of value)
			if (typeof tag !== "string" || !(HINT_TAGS as readonly string[]).includes(tag))
				throw new Error(`Unknown hint tag ${JSON.stringify(tag)}; tags: ${HINT_TAGS.join(", ")}`);
		return value as HintTag[];
	}

	#load(): Promise<void> {
		this.#loaded ??= (async () => {
			const saved = readVersioned("ultron.module/hints", await this.#options.store.read());
			const lanes =
				saved &&
				typeof saved === "object" &&
				!Array.isArray(saved) &&
				saved.lanes &&
				typeof saved.lanes === "object"
					? (saved.lanes as unknown as Record<string, Persisted>)
					: {};
			for (const [lane, entry] of Object.entries(lanes))
				this.#persisted[lane] = {
					muted: Array.isArray(entry?.muted) ? entry.muted.filter((tag) => typeof tag === "string") : [],
					fired: entry?.fired && typeof entry.fired === "object" ? { ...entry.fired } : {},
				};
		})();
		return this.#loaded;
	}

	#lane(lane: string): Persisted {
		this.#persisted[lane] ??= { muted: [], fired: {} };
		return this.#persisted[lane]!;
	}

	#persist(): void {
		const document = structuredClone({ version: JOURNAL_VERSION, lanes: this.#persisted }) as unknown as JsonValue;
		this.#writes = this.#writes.then(() => this.#options.store.write(document)).catch(() => {});
	}

	/** `lane`'s run ended: failure streaks start over with its next run. */
	runEnded(lane: string): void {
		this.#loops.reset(lane);
		const state = this.#lanes.get(lane);
		if (state) state.failure = undefined;
	}

	/** Wait for journal writes (tests). */
	settled(): Promise<void> {
		return this.#writes;
	}

	#state(lane: string): LaneState {
		let state = this.#lanes.get(lane);
		if (!state) {
			state = { previousStatus: new Set() };
			this.#lanes.set(lane, state);
		}
		return state;
	}

	/** A cell starts on `lane`. */
	beginCell(lane: string, code: string): void {
		if (!this.enabled) return;
		this.#state(lane).cell = {
			code,
			detached: [],
			waits: [],
			waitTypes: new Set(),
			statusCalls: new Map(),
			bashFailed: false,
		};
	}

	/** A host request from `lane`'s kernel finished (`result` is undefined when it failed). */
	observe(lane: string, type: string, payload: Record<string, unknown>, result: unknown, startedAt: number): void {
		const cell = this.#lanes.get(lane)?.cell;
		if (!cell) return;
		const reply = result && typeof result === "object" ? (result as Record<string, unknown>) : undefined;
		if (type === "bash" && reply && reply.running !== true)
			cell.bashFailed = bashFailed(String(payload.command ?? ""), reply.exit_code, reply.timed_out === true);
		if ((type === "bash" || type === "shell.bash") && reply?.running === true) {
			const job = reply.job as { id?: unknown } | undefined;
			cell.detached.push({
				id: typeof job?.id === "string" ? job.id : "a job",
				waited: typeof payload.yield_after === "number" ? payload.yield_after : undefined,
			});
		}
		if (BLOCKING_WAITS.has(type)) {
			cell.waits.push([startedAt, this.#now]);
			cell.waitTypes.add(type);
		}
		if (STATUS_CALLS.has(type)) {
			// A status call that found the work finished is a result fetch, not a poll.
			if ((type === "shell.get" || type === "shell.result") && reply?.running !== true) return;
			const id = typeof payload.id === "string" ? `:${payload.id}` : "";
			const key = `${type}${id}`;
			cell.statusCalls.set(key, (cell.statusCalls.get(key) ?? 0) + 1);
		}
	}

	/** The cell on `lane` ended: the hint line to append to its result, if any. */
	async endCell(lane: string, outcome: CellOutcome): Promise<string | undefined> {
		if (!this.enabled) return undefined;
		const state = this.#state(lane);
		const cell = state.cell;
		state.cell = undefined;
		if (!cell) return undefined;
		const candidates = this.#candidates(
			state,
			cell,
			outcome,
			lane === "main" && this.#options.rootDelivery === "next-call",
		);
		const trip = this.#loops.cellEnded(
			lane,
			cell.code,
			outcome.ename !== undefined || cell.bashFailed,
			outcome.ename,
		);
		if (trip) candidates.unshift(["stuck-loop", loopHintText(trip)]);
		state.previousStatus = new Set(cell.statusCalls.keys());
		if (candidates.length === 0) return undefined;
		await this.#load();
		const persisted = this.#lane(lane);
		const max = this.#options.maxPerTag ?? DEFAULT_HINT_MAX_PER_TAG;
		const usable = (tag: HintTag) => !persisted.muted.includes(tag) && (persisted.fired[tag] ?? 0) < max;
		// While the loop detector handles a stuck phase, the narrower repeated-failure hint would only repeat it.
		const stuck = this.#loops.level(lane) > 0 && usable("stuck-loop");
		for (const [tag, text] of candidates) {
			if (!usable(tag) || (stuck && tag === "repeated-failure")) continue;
			persisted.fired[tag] = (persisted.fired[tag] ?? 0) + 1;
			this.#persist();
			return `[hint:${tag}] ${text} (Mute this hint: \`await hints.mute("${tag}")\`.)`;
		}
		return undefined;
	}

	/** Every hint this cell earned, most specific first. */
	#candidates(state: LaneState, cell: Cell, outcome: CellOutcome, nextCall: boolean): Array<[HintTag, string]> {
		const events = this.#options.asyncEvents !== false;
		const found: Array<[HintTag, string]> = [];
		// Failure streaks count every cell, whether or not a hint fires for it.
		if (outcome.ename) {
			state.failure =
				state.failure?.ename === outcome.ename
					? { ename: outcome.ename, count: state.failure.count + 1 }
					: { ename: outcome.ename, count: 1 };
		} else state.failure = undefined;

		const detached = cell.detached[0];
		if (detached) {
			const after = detached.waited === undefined ? "" : ` after ${detached.waited} s`;
			found.push([
				"job-detached",
				events && nextCall
					? `The command was still running${after}, so it continues as job ${detached.id}. Do other work meanwhile: its completion is reported as a <runtime_event> at the top of a later rlm result, and \`await <result>.job.result()\` waits for it when your answer needs it (nothing wakes you after you reply). (\`yield_after=None\` blocks until a command ends; \`yield_after=0\` detaches at once.)`
					: events
						? `The command was still running${after}, so it continues as job ${detached.id}. Do not wait for it: do other work or end your turn; its completion arrives as a <runtime_event> that starts your next turn. (\`yield_after=None\` blocks until a command ends; \`yield_after=0\` detaches at once.)`
						: `The command was still running${after}, so it continues as job ${detached.id}; \`await <result>.job.result()\` waits for it. (\`yield_after=None\` blocks until a command ends.)`,
			]);
		}
		const waitedMs = unionMs(cell.waits);
		// Without wake-ups, waiting for a result the answer needs is the right move, not a mistake.
		if (events && !nextCall && waitedMs >= (this.#options.blockedSeconds ?? DEFAULT_BLOCKED_SECONDS) * 1000) {
			const calls = [...cell.waitTypes].map(waitCall).join(", ");
			found.push([
				"blocked-on-job",
				`This cell spent ${Math.round(waitedMs / 1000)} s waiting in ${calls}. You could have ended your turn instead: completions arrive as <runtime_event> messages that start a new turn, so wait only when you need the result now.`,
			]);
		}
		const repeated = [...cell.statusCalls].find(([, count]) => count >= POLL_REPEATS)?.[0];
		const again = [...cell.statusCalls.keys()].find((key) => state.previousStatus.has(key));
		const sleepLoop = SLEEP.test(cell.code) && LOOP.test(cell.code) && STATUS_CHECK.test(cell.code);
		if (repeated || again || sleepLoop) {
			const what = repeated ?? again;
			found.push([
				"poll-loop",
				`${what ? `Repeated status checks (${what.replace(":", " ")})` : "Sleeping in a loop around a status check"} look like polling. ${
					events && nextCall
						? "Completions are reported on their own at the top of your next rlm result: do other work instead of checking or sleeping, and when only that result is left, wait once (`await job.result()`, `await rlm.collect(...)`)."
						: events
							? "Completions arrive as <runtime_event> messages on their own: do other work or end your turn instead of checking or sleeping."
							: "Wait once with `await job.result()`, `await rlm.collect(...)` or `await agents.result(id)` instead of checking and sleeping."
				}`,
			]);
		}
		if (state.failure && state.failure.count >= FAILURE_STREAK) {
			found.push([
				"repeated-failure",
				`${state.failure.ename} ${state.failure.count} cells in a row. Read the traceback carefully (the failing line and the message) and change your approach instead of retrying a variant of the same cell.`,
			]);
		}
		const truncated = TRUNCATION_MARKER.test(outcome.text);
		const bigString = BIG_STRING.exec(outcome.text);
		const handleBytes = this.#options.readHandleBytes ?? DEFAULT_READ_HANDLE_BYTES;
		const bigStringChars = bigString ? Number(bigString[1]!.replace(/,/g, "")) : 0;
		if (FILE_READ.test(cell.code) && (truncated || bigStringChars > handleBytes)) {
			found.push([
				"large-read",
				`This cell read a large file into a string. \`await read(path)\` returns a ContextHandle for files over ${Math.round(handleBytes / 1024)} KiB: use h.search(regex), h.lines(a, b), h.chunks(n) or rlm.map(task, h.chunks(n)) and print only what you need.`,
			]);
		}
		if (truncated) {
			found.push([
				"output-truncated",
				"The output was cut in the middle. Keep large data in variables and print slices, counts or matches, or load big text as a handle (`h = await rlm.load(text_or_path)`; `h.search`, `h.lines`).",
			]);
		}
		return found;
	}
}

/** Commands whose exit status 1 means "no match" or "differs", not an error. */
const QUERY_COMMANDS = /^\s*(?:grep|egrep|fgrep|rg|diff|cmp|test|\[)\b/;

/** Whether a finished `bash` command failed: a timeout, or a non-zero exit (1 is an answer for a query command). */
export function bashFailed(command: string, exitCode: unknown, timedOut = false): boolean {
	if (timedOut) return true;
	if (typeof exitCode !== "number" || exitCode === 0) return false;
	return !(exitCode === 1 && QUERY_COMMANDS.test(command));
}

function waitCall(type: string): string {
	return type === "shell.result" ? "job.result()" : "background.result()";
}

/** Total length of possibly overlapping intervals (waits run concurrently under asyncio.gather). */
function unionMs(intervals: ReadonlyArray<[number, number]>): number {
	const sorted = [...intervals].sort((a, b) => a[0] - b[0]);
	let total = 0;
	let end = Number.NEGATIVE_INFINITY;
	for (const [start, stop] of sorted) {
		if (stop <= end) continue;
		total += stop - Math.max(start, end);
		end = stop;
	}
	return total;
}
