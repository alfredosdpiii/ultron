/**
 * Durable counters for the session report (`/usage`, `ultron usage`; see session-report.ts): what the runtime sees
 * while a session runs and no other session value keeps. They are the session value `ultron.module/stats`.
 *
 * Cells are counted where they run (the rlm tool), so the counts are the same whoever drives the root lane: an
 * Ultron lane (`ultron`, `ultron --claude`) or Claude Code over MCP (`ultron claude`), whose cells never enter
 * Ultron's transcript. Guard outcomes (Loki), masked secrets, steers, refused work and merge outcomes are otherwise
 * kept in memory only and would be gone once the worker exits.
 *
 * Nothing here holds code, output, paths or prompts: only counts, API names, task ids and times.
 */
import type { JsonValue } from "@ultron/chord";
import { readVersioned, UnsupportedFormatVersionError } from "./format-version.ts";
import type { HostModuleStore } from "./rlm/host-module.ts";

export const SESSION_STATS_MODULE = "stats";
const STATS_VERSION = 1;
/** Distinct host request types kept (later ones are not added), and task ids kept per map (the newest). */
const MAX_KEYS = 200;

/** REPL APIs a cell's code can name, in the order the report lists them. */
export const CELL_APIS = [
	"bash",
	"read",
	"edit",
	"write",
	"rlm.load",
	"rlm.infer",
	"rlm.map",
	"rlm.spawn",
	"rlm.collect",
	"rlm.merge",
	"mcp.call",
	"tools.call",
	"workflows.run",
	"agents.*",
	"background.start",
	"view_image",
] as const;
export type CellApi = (typeof CELL_APIS)[number];

const API_PATTERNS: ReadonlyArray<readonly [CellApi, RegExp]> = [
	["bash", /(?<![\w.])bash\s*\(/],
	["read", /(?<![\w.])read\s*\(/],
	["edit", /(?<![\w.])edit\s*\(/],
	["write", /(?<![\w.])write\s*\(/],
	["rlm.load", /\brlm\.load\s*\(/],
	["rlm.infer", /\brlm\.infer\s*\(/],
	["rlm.map", /\brlm\.map\s*\(/],
	["rlm.spawn", /\brlm\.spawn\s*\(/],
	["rlm.collect", /\brlm\.collect\s*\(/],
	["rlm.merge", /\brlm\.merge\s*\(/],
	["mcp.call", /\bmcp\.call\s*\(/],
	["tools.call", /\btools\.call\s*\(/],
	["workflows.run", /\bworkflows\.run\s*\(/],
	["agents.*", /\bagents\.(?:spawn|invoke|register)\s*\(/],
	["background.start", /\bbackground\.start\s*\(/],
	["view_image", /(?<![\w.])view_image\s*\(/],
];

/**
 * The REPL APIs a cell's code names (each at most once). A static read of the source: a call inside a function the
 * cell only defines counts too, and a call made through an alias does not.
 */
export function cellApis(code: string): CellApi[] {
	return API_PATTERNS.filter(([, pattern]) => pattern.test(code)).map(([name]) => name);
}

export type SessionStatsLane = "root" | "subagents" | "other";

/** Which counter a lane's cells go to: the root lane, `rlm.spawn` subagents, or anything else (typed agents, jobs). */
export function statsLane(lane: string): SessionStatsLane {
	if (lane === "main") return "root";
	return lane.startsWith("ultron.rlm-child.") ? "subagents" : "other";
}

export type SessionStatsCells = { count: number; failed: number; apis: Record<string, number> };

export type SessionStatsGuard = {
	checks: number;
	blocked: number;
	unchecked: number;
	afterChecks: number;
	afterFindings: number;
	ms: number;
};

export type SessionStatsNudges = { toolRounds: number; wait: number; skill: number };

export type SessionStatsDocument = {
	version: 1;
	/** Where the root agent runs: an Ultron lane, or outside Ultron (Claude Code over MCP, `ultron claude`). */
	root: "lane" | "external";
	/** When counting began. A session older than this was started by an Ultron that did not count. */
	since: number;
	updatedAt: number;
	cells: Record<SessionStatsLane, SessionStatsCells>;
	/** Host requests the kernels made, by request type (`shell.run`, `rlm.map`, `workflows.run`, ...). */
	hostCalls: Record<string, number>;
	/** Secrets masked in cell output (`[REDACTED:<kind>]`). */
	secretsMasked: number;
	/** File-write guards by name (Loki, extensions). */
	guards: Record<string, SessionStatsGuard>;
	/** Steers sent to the root: the research-loop brake, the wait steer, the skill suggestion. */
	nudges: SessionStatsNudges;
	/** Tool calls, model requests and admissions refused because a root's turn, token, cost, wall or task limit was hit. */
	usageLimitBlocks: number;
	/** How `rlm.merge` last ended per worktree child, by task id. */
	merges: Record<string, string>;
	/** The model each `rlm.spawn` subagent ran on, by task id (a subagent run as a process has no lane to ask). */
	childModels: Record<string, string>;
	/** Turns of an external root (Claude Code's user turns), which leave no run in the session. */
	externalTurns: { count: number; wallMs: number };
};

export type NudgeKind = keyof SessionStatsNudges;

/** Which steer a `[Ultron] ...` message is (see tool-round-nudge.ts), or undefined for other text. */
export function classifyNudge(text: string): NudgeKind | undefined {
	if (!text.startsWith("[Ultron] ")) return undefined;
	if (/^\[Ultron\] \d+ tool rounds in a row succeeded\b/.test(text)) return "skill";
	if (
		/^\[Ultron\] (?:1 subagent or task you started is|\d+ subagents or tasks you started are) still running\b/.test(
			text,
		)
	)
		return "wait";
	if (/^\[Ultron\] You have (?:now )?used \d+ rounds of tool calls\b/.test(text)) return "toolRounds";
	return undefined;
}

/** Set `key` as the newest entry of a bounded map (the oldest entries are dropped past {@link MAX_KEYS}). */
function keep(map: Record<string, string>, key: string, value: string): void {
	delete map[key];
	map[key] = value;
	const keys = Object.keys(map);
	for (const old of keys.slice(0, Math.max(0, keys.length - MAX_KEYS))) delete map[old];
}

function emptyCells(): SessionStatsCells {
	return { count: 0, failed: 0, apis: {} };
}

export function emptySessionStats(root: "lane" | "external", now: number): SessionStatsDocument {
	return {
		version: STATS_VERSION,
		root,
		since: now,
		updatedAt: now,
		cells: { root: emptyCells(), subagents: emptyCells(), other: emptyCells() },
		hostCalls: {},
		secretsMasked: 0,
		guards: {},
		nudges: { toolRounds: 0, wait: 0, skill: 0 },
		usageLimitBlocks: 0,
		merges: {},
		childModels: {},
		externalTurns: { count: 0, wallMs: 0 },
	};
}

function count(value: unknown): number {
	return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : 0;
}

function counts(value: unknown): Record<string, number> {
	const out: Record<string, number> = {};
	if (value === null || typeof value !== "object" || Array.isArray(value)) return out;
	for (const [key, item] of Object.entries(value)) if (count(item) > 0) out[key] = count(item);
	return out;
}

function cellsOf(value: unknown): SessionStatsCells {
	const item = (value ?? {}) as Partial<SessionStatsCells>;
	return { count: count(item.count), failed: count(item.failed), apis: counts(item.apis) };
}

/**
 * A stored stats document with every field present, or undefined when `saved` is not one. Unknown fields are
 * dropped and malformed counters read as zero: the counters are a report, never an input to the runtime.
 */
export function parseSessionStats(saved: unknown): SessionStatsDocument | undefined {
	if (saved === null || typeof saved !== "object" || Array.isArray(saved)) return undefined;
	const item = saved as Record<string, unknown>;
	if (item.version !== undefined && item.version !== STATS_VERSION) return undefined;
	const cells = (item.cells ?? {}) as Record<string, unknown>;
	const guards: Record<string, SessionStatsGuard> = {};
	if (item.guards !== null && typeof item.guards === "object" && !Array.isArray(item.guards)) {
		for (const [name, value] of Object.entries(item.guards)) {
			const guard = (value ?? {}) as Partial<SessionStatsGuard>;
			guards[name] = {
				checks: count(guard.checks),
				blocked: count(guard.blocked),
				unchecked: count(guard.unchecked),
				afterChecks: count(guard.afterChecks),
				afterFindings: count(guard.afterFindings),
				ms: count(guard.ms),
			};
		}
	}
	const nudges = (item.nudges ?? {}) as Partial<SessionStatsNudges>;
	const strings = (value: unknown): Record<string, string> => {
		const out: Record<string, string> = {};
		if (value === null || typeof value !== "object" || Array.isArray(value)) return out;
		for (const [key, text] of Object.entries(value)) if (typeof text === "string") out[key] = text;
		return out;
	};
	const turns = (item.externalTurns ?? {}) as { count?: unknown; wallMs?: unknown };
	return {
		version: STATS_VERSION,
		root: item.root === "external" ? "external" : "lane",
		since: count(item.since),
		updatedAt: count(item.updatedAt),
		cells: { root: cellsOf(cells.root), subagents: cellsOf(cells.subagents), other: cellsOf(cells.other) },
		hostCalls: counts(item.hostCalls),
		secretsMasked: count(item.secretsMasked),
		guards,
		nudges: { toolRounds: count(nudges.toolRounds), wait: count(nudges.wait), skill: count(nudges.skill) },
		usageLimitBlocks: count(item.usageLimitBlocks),
		merges: strings(item.merges),
		childModels: strings(item.childModels),
		externalTurns: { count: count(turns.count), wallMs: count(turns.wallMs) },
	};
}

export interface SessionStatsOptions {
	store: HostModuleStore;
	/** `external` when Claude Code drives the root over MCP. Default `lane`. */
	root?: "lane" | "external";
	now?: () => number;
	/** Changes are written this long after the first unsaved one (default 500 ms), and on `flush`. */
	flushMs?: number;
}

/** One guard outcome (file-hooks.ts `GuardRecord`, the fields counted here). */
export type GuardOutcome = {
	readonly guard: string;
	readonly phase: "before_write" | "before_shell" | "after_cell";
	readonly outcome: "allowed" | "blocked" | "unchecked" | "clean" | "advisory" | "findings";
	readonly ms: number;
};

/**
 * Counts in memory and saves the document shortly after a change. Recording never throws and never blocks the
 * caller: a store that cannot be read or written leaves the counters in memory for this worker's reports.
 */
export class SessionStatsRecorder {
	readonly #store: HostModuleStore;
	readonly #now: () => number;
	readonly #flushMs: number;
	#document: SessionStatsDocument;
	#loaded: Promise<void> | undefined;
	/** Changes made before the stored document was read; applied on top of it once it is. */
	#early: Array<(document: SessionStatsDocument) => void> | undefined = [];
	#dirty = false;
	#timer: NodeJS.Timeout | undefined;
	#writes: Promise<void> = Promise.resolve();
	#closed = false;
	/** The stored document is in a format this build does not know: count in memory and leave it as it is. */
	#foreign = false;
	readonly #openTurns = new Map<string, number>();

	constructor(options: SessionStatsOptions) {
		this.#store = options.store;
		this.#now = options.now ?? Date.now;
		this.#flushMs = options.flushMs ?? 500;
		this.#document = emptySessionStats(options.root ?? "lane", this.#now());
	}

	#load(): Promise<void> {
		this.#loaded ??= (async () => {
			let stored: SessionStatsDocument | undefined;
			try {
				const saved = await this.#store.read();
				stored = parseSessionStats(readVersioned(`ultron.module/${SESSION_STATS_MODULE}`, saved));
			} catch (error) {
				stored = undefined;
				this.#foreign = error instanceof UnsupportedFormatVersionError;
			}
			if (stored !== undefined) {
				// The session keeps the kind of root it has now (a native session resumed under `ultron claude`).
				stored.root = this.#document.root;
				this.#document = stored;
			}
			const early = this.#early ?? [];
			this.#early = undefined;
			for (const change of early) change(this.#document);
		})();
		return this.#loaded;
	}

	#change(change: (document: SessionStatsDocument) => void): void {
		if (this.#closed) return;
		if (this.#early !== undefined) {
			this.#early.push(change);
			void this.#load().then(() => this.#schedule());
			return;
		}
		change(this.#document);
		this.#schedule();
	}

	#schedule(): void {
		this.#dirty = true;
		this.#document.updatedAt = this.#now();
		if (this.#timer !== undefined || this.#closed) return;
		this.#timer = setTimeout(() => {
			this.#timer = undefined;
			void this.flush();
		}, this.#flushMs);
		this.#timer.unref?.();
	}

	/** Something ran in the session: its counters are saved even while they are all zero. */
	touch(): void {
		this.#change(() => {});
	}

	/** One cell ran on `lane`. */
	cell(lane: string, code: string, outcome: { failed: boolean; masked?: number }): void {
		const apis = cellApis(code);
		this.#change((document) => {
			const cells = document.cells[statsLane(lane)];
			cells.count += 1;
			if (outcome.failed) cells.failed += 1;
			for (const api of apis) cells.apis[api] = (cells.apis[api] ?? 0) + 1;
			document.secretsMasked += count(outcome.masked);
		});
	}

	/** A kernel made a host request of this type. */
	hostCall(type: string): void {
		this.#change((document) => {
			if (document.hostCalls[type] === undefined && Object.keys(document.hostCalls).length >= MAX_KEYS) return;
			document.hostCalls[type] = (document.hostCalls[type] ?? 0) + 1;
		});
	}

	guard(record: GuardOutcome): void {
		this.#change((document) => {
			document.guards[record.guard] ??= {
				checks: 0,
				blocked: 0,
				unchecked: 0,
				afterChecks: 0,
				afterFindings: 0,
				ms: 0,
			};
			const guard = document.guards[record.guard]!;
			guard.ms += count(record.ms);
			// A shell command checked before it runs counts as a before-write check: it is one.
			if (record.phase === "before_write" || record.phase === "before_shell") {
				guard.checks += 1;
				if (record.outcome === "blocked") guard.blocked += 1;
				if (record.outcome === "unchecked") guard.unchecked += 1;
			} else {
				guard.afterChecks += 1;
				// Advisory-only reports are not findings to fix.
				if (record.outcome === "findings" || record.outcome === "unchecked") guard.afterFindings += 1;
			}
		});
	}

	/** A steer sent to the root; text that is not one of the known steers is not counted. */
	nudge(message: string): void {
		const kind = classifyNudge(message);
		if (kind === undefined) return;
		this.#change((document) => {
			document.nudges[kind] += 1;
		});
	}

	usageLimitBlock(): void {
		this.#change((document) => {
			document.usageLimitBlocks += 1;
		});
	}

	merge(taskId: string, status: string): void {
		this.#change((document) => keep(document.merges, taskId, status));
	}

	/** The model an `rlm.spawn` subagent ran on. */
	childModel(taskId: string, model: string): void {
		this.#change((document) => keep(document.childModels, taskId, model));
	}

	/** A turn of an external root began (Claude Code's UserPromptSubmit, or its first cell). */
	externalTurnStarted(turn: string): void {
		if (this.#openTurns.has(turn)) return;
		this.#openTurns.set(turn, this.#now());
		this.#change((document) => {
			document.externalTurns.count += 1;
		});
	}

	externalTurnEnded(turn: string): void {
		const started = this.#openTurns.get(turn);
		if (started === undefined) return;
		this.#openTurns.delete(turn);
		const elapsed = Math.max(0, this.#now() - started);
		this.#change((document) => {
			document.externalTurns.wallMs += elapsed;
		});
	}

	/** The counters as they are now, saved or not. */
	async snapshot(): Promise<SessionStatsDocument> {
		await this.#load();
		return structuredClone(this.#document);
	}

	/** Save unsaved changes. Never throws. */
	flush(): Promise<void> {
		if (this.#timer !== undefined) {
			clearTimeout(this.#timer);
			this.#timer = undefined;
		}
		this.#writes = this.#writes.then(async () => {
			await this.#load();
			if (!this.#dirty || this.#foreign) return;
			this.#dirty = false;
			try {
				await this.#store.write(structuredClone(this.#document) as unknown as JsonValue);
			} catch {
				// The counters stay in memory; the next change tries again.
				this.#dirty = true;
			}
		});
		return this.#writes;
	}

	/** Save and stop recording. */
	async close(): Promise<void> {
		for (const turn of [...this.#openTurns.keys()]) this.externalTurnEnded(turn);
		await this.#load();
		await this.flush();
		this.#closed = true;
		if (this.#timer !== undefined) clearTimeout(this.#timer);
		this.#timer = undefined;
	}
}
