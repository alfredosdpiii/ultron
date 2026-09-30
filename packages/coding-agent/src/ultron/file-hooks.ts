/**
 * Generic file-write hooks for the RLM REPL, where the model's only tool is Python and Pi's edit/write tool events
 * never fire.
 *
 * 1. Before a write (blocking). The kernel's `edit()` and `write()` skills send `{path, content}` to the host before
 *    touching the file (request `files.before_write`). Every guard's `beforeWrite` sees it; a guard that blocks makes
 *    the skill raise ValueError with the guard's diagnostic, and nothing is written. Concurrent writes (for example
 *    `asyncio.gather(edit(...), write(...))`) are checked in parallel. A guard that does not answer within its
 *    timeout (default 5 s) or fails lets the write proceed with a visible note ("Loki did not check this write"):
 *    it fails open, never silently.
 * 2. After a cell (advisory). Files a cell changed by other means (`bash('sed -i ...')`, `Path.write_text`) are found
 *    by comparing workspace snapshots (workspace-snapshot.ts) taken between cells, minus the writes checked in (1)
 *    whose content is unchanged. Guards' `afterCellChanges` run in the background, so a cell returns as fast as
 *    before, and what they report reaches the model with the lane's next cell result.
 *
 * The module knows nothing about any particular guard; Loki (loki.ts) and Pi extensions (the `before_file_write`
 * and `after_cell_changes` events) register through the same interface.
 */
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { isAbsolute, relative, resolve, sep } from "node:path";
import { diffSnapshots, snapshotWorkspace, type WorkspaceSnapshot } from "./workspace-snapshot.ts";

/** The host request the kernel's write skills send before writing. */
export const BEFORE_WRITE_REQUEST = "files.before_write";

/** A write the kernel proposes: the absolute path and the complete new text. */
export interface ProposedWrite {
	readonly path: string;
	readonly content: string;
}

export interface FileHookContext {
	readonly lane: string;
	/** The lane's working directory: the session's, or a worktree subagent's own. */
	readonly cwd: string;
	/**
	 * Set for a worktree subagent's lane (`rlm.spawn(..., worktree=True)`): the root of its Git worktree, which a
	 * guard checks instead of the session's repository.
	 */
	readonly root?: string;
	/** Aborted when the guard's time is up or the cell is cancelled. */
	readonly signal: AbortSignal;
}

/** A guard's answer to a proposed write: block it with a reason, or let it through with an optional note. */
export type BeforeWriteVerdict = { readonly block: true; readonly reason: string } | { readonly message?: string };

/** Files one cell changed. Paths are absolute. */
export interface CellChanges {
	/** Added or modified files no before-write check saw (or whose checked content was changed again). */
	readonly files: readonly string[];
	/** Files written through a checked `edit()`/`write()` whose content is what was checked. */
	readonly checked: readonly string[];
	readonly deleted: readonly string[];
	/** False when a snapshot hit its size bound, so some changes may be missing. */
	readonly complete: boolean;
}

export interface FileWriteGuard {
	/** Shown in notes and statistics ("Loki"). */
	readonly name: string;
	/** Per-call time limit for `beforeWrite` in milliseconds (default 5000). */
	readonly timeoutMs?: number;
	/** Per-call time limit for `afterCellChanges` (default 120000). */
	readonly afterTimeoutMs?: number;
	/** False while the guard has nothing to do (checked per call). */
	enabled?(): boolean;
	/** False when `afterCellChanges` would do nothing now, so no workspace snapshots are taken for it. */
	watchesCells?(): boolean;
	beforeWrite?(write: ProposedWrite, context: FileHookContext): Promise<BeforeWriteVerdict | undefined>;
	afterCellChanges?(changes: CellChanges, context: FileHookContext): Promise<string | undefined>;
}

/** What the kernel gets back for one proposed write. */
export interface BeforeWriteResult {
	readonly blocked: boolean;
	readonly reason?: string;
	/** Advisory text and fail-open notes, printed in the cell's output. */
	readonly notes: string[];
}

/** One guard's time and outcomes, for the RLM pane and diagnostics. */
export interface GuardStats {
	readonly name: string;
	/** Before-write checks run, writes blocked, checks that timed out or failed (the write proceeded unchecked). */
	checks: number;
	blocked: number;
	unchecked: number;
	/** After-cell checks run, and how many reported something. */
	afterChecks: number;
	afterFindings: number;
	/** Time spent in the guard: in all, and since the current root turn began. */
	totalMs: number;
	turnMs: number;
}

export const DEFAULT_BEFORE_WRITE_TIMEOUT_MS = 5_000;
const DEFAULT_AFTER_CELL_TIMEOUT_MS = 120_000;
/** Findings delivered with one cell result are capped; the rest are summarized. */
const MAX_PENDING_CHARS = 6_000;

type Snapshotter = (cwd: string) => Promise<WorkspaceSnapshot>;

export interface FileHooksOptions {
	readonly cwd: string;
	/** Snapshot function (tests); default snapshotWorkspace. */
	readonly snapshot?: Snapshotter;
	readonly now?: () => number;
	/** Every guard outcome, for logs (ULTRON_LOKI_LOG). */
	readonly onRecord?: (record: GuardRecord) => void;
	/**
	 * A lane that works elsewhere than `cwd` (a worktree subagent): its directory, and the root of its worktree.
	 * Its writes resolve against that directory and its cells are compared against their own snapshots.
	 */
	readonly laneRoot?: (lane: string) => { cwd: string; root: string } | undefined;
}

export interface GuardRecord {
	readonly guard: string;
	readonly phase: "before_write" | "after_cell";
	readonly outcome: "allowed" | "blocked" | "unchecked" | "clean" | "findings";
	readonly ms: number;
	readonly paths: readonly string[];
	readonly lane: string;
	readonly detail?: string;
}

function sha256(text: string | Buffer): string {
	return createHash("sha256").update(text).digest("hex");
}

function errorText(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

export class FileHooks {
	readonly cwd: string;
	readonly #guards: FileWriteGuard[] = [];
	readonly #stats = new Map<FileWriteGuard, GuardStats>();
	readonly #snapshot: Snapshotter;
	readonly #now: () => number;
	readonly #onRecord: ((record: GuardRecord) => void) | undefined;
	/** Checked writes since the last after-cell snapshot: absolute path -> sha256 of the checked content. */
	readonly #checked = new Map<string, string>();
	readonly #pending = new Map<string, string[]>();
	/** The last snapshot per directory: the session's, and each worktree subagent's own. */
	readonly #baselines = new Map<string, Promise<WorkspaceSnapshot | undefined>>();
	#chain: Promise<void> = Promise.resolve();
	readonly #laneRoot: FileHooksOptions["laneRoot"];

	constructor(options: FileHooksOptions) {
		this.cwd = resolve(options.cwd);
		this.#snapshot = options.snapshot ?? ((cwd) => snapshotWorkspace(cwd));
		this.#now = options.now ?? Date.now;
		this.#onRecord = options.onRecord;
		this.#laneRoot = options.laneRoot;
	}

	/** Where a lane works: its own directory and worktree root, or the session's directory. */
	#where(lane: string | undefined): { cwd: string; root?: string } {
		const own = lane === undefined ? undefined : this.#laneRoot?.(lane);
		return own === undefined ? { cwd: this.cwd } : { cwd: resolve(own.cwd), root: resolve(own.root) };
	}

	/** Register a guard; returns its removal. */
	add(guard: FileWriteGuard): () => void {
		this.#guards.push(guard);
		this.#stats.set(guard, {
			name: guard.name,
			checks: 0,
			blocked: 0,
			unchecked: 0,
			afterChecks: 0,
			afterFindings: 0,
			totalMs: 0,
			turnMs: 0,
		});
		return () => {
			const index = this.#guards.indexOf(guard);
			if (index !== -1) this.#guards.splice(index, 1);
			this.#stats.delete(guard);
		};
	}

	#enabled(guard: FileWriteGuard): boolean {
		try {
			return guard.enabled?.() ?? true;
		} catch {
			return false;
		}
	}

	/** True when some guard wants to see changes after each cell (snapshots are taken only then). */
	get watchesCells(): boolean {
		return this.#guards.some((guard) => this.#watching(guard));
	}

	#watching(guard: FileWriteGuard): boolean {
		return guard.afterCellChanges !== undefined && this.#enabled(guard) && (guard.watchesCells?.() ?? true);
	}

	/** A new root turn began: per-turn times start from zero. */
	beginTurn(): void {
		for (const stats of this.#stats.values()) stats.turnMs = 0;
	}

	stats(): GuardStats[] {
		return [...this.#stats.values()].map((stats) => ({ ...stats }));
	}

	#charge(guard: FileWriteGuard, ms: number): void {
		const stats = this.#stats.get(guard);
		if (!stats) return;
		stats.totalMs += ms;
		stats.turnMs += ms;
	}

	#display(path: string, cwd = this.cwd): string {
		const rel = relative(cwd, path);
		return rel && !rel.startsWith("..") && !isAbsolute(rel) ? rel.split(sep).join("/") : path;
	}

	/** Run a guard call with its time limit; resolves `{ timedOut: true }` when it does not answer in time. */
	async #timed<T>(
		timeoutMs: number,
		signal: AbortSignal | undefined,
		call: (signal: AbortSignal) => Promise<T>,
	): Promise<{ value?: T; error?: unknown; timedOut?: boolean }> {
		const controller = new AbortController();
		const abort = () => controller.abort(signal?.reason);
		signal?.addEventListener("abort", abort, { once: true });
		let timer: NodeJS.Timeout | undefined;
		try {
			return await Promise.race([
				call(controller.signal).then(
					(value) => ({ value }),
					(error: unknown) => ({ error }),
				),
				new Promise<{ timedOut: true }>((done) => {
					timer = setTimeout(() => {
						controller.abort(new Error("timed out"));
						done({ timedOut: true });
					}, timeoutMs);
					timer.unref?.();
				}),
			]);
		} finally {
			if (timer) clearTimeout(timer);
			signal?.removeEventListener("abort", abort);
		}
	}

	/** Check proposed writes with every enabled guard (writes and guards in parallel). */
	async beforeWrite(
		writes: readonly ProposedWrite[],
		options: { lane: string; signal?: AbortSignal },
	): Promise<BeforeWriteResult[]> {
		const guards = this.#guards.filter((guard) => guard.beforeWrite !== undefined && this.#enabled(guard));
		const where = this.#where(options.lane);
		return Promise.all(
			writes.map(async (write): Promise<BeforeWriteResult> => {
				const path = resolve(where.cwd, write.path);
				const proposed = { path, content: write.content };
				const display = this.#display(path, where.cwd);
				const outcomes = await Promise.all(
					guards.map(async (guard) => {
						const timeoutMs = guard.timeoutMs ?? DEFAULT_BEFORE_WRITE_TIMEOUT_MS;
						const started = this.#now();
						const result = await this.#timed(timeoutMs, options.signal, (signal) =>
							guard.beforeWrite!(proposed, {
								lane: options.lane,
								cwd: where.cwd,
								...(where.root === undefined ? {} : { root: where.root }),
								signal,
							}),
						);
						const ms = this.#now() - started;
						this.#charge(guard, ms);
						const stats = this.#stats.get(guard);
						if (stats) stats.checks += 1;
						let outcome: GuardRecord["outcome"] = "allowed";
						let note: string | undefined;
						let reason: string | undefined;
						if (result.timedOut || result.error !== undefined) {
							outcome = "unchecked";
							if (stats) stats.unchecked += 1;
							note = result.timedOut
								? `[${guard.name}] ${guard.name} did not check this write to ${display} (no answer within ${Math.round(timeoutMs / 100) / 10} s); it was written unchecked.`
								: `[${guard.name}] ${guard.name} did not check this write to ${display} (${errorText(result.error)}); it was written unchecked.`;
						} else if (result.value && "block" in result.value && result.value.block) {
							outcome = "blocked";
							if (stats) stats.blocked += 1;
							reason = `[${guard.name}] ${result.value.reason}`;
						} else if (result.value && "message" in result.value && result.value.message) {
							note = `[${guard.name}] ${result.value.message}`;
						}
						this.#onRecord?.({
							guard: guard.name,
							phase: "before_write",
							outcome,
							ms,
							paths: [display],
							lane: options.lane,
							...(reason === undefined ? {} : { detail: reason }),
						});
						return { reason, note };
					}),
				);
				const reasons = outcomes.map((outcome) => outcome.reason).filter((reason) => reason !== undefined);
				const notes = outcomes.map((outcome) => outcome.note).filter((note) => note !== undefined);
				if (reasons.length > 0) return { blocked: true, reason: reasons.join("\n"), notes };
				// The write will happen with this content; an after-cell diff need not report it again.
				this.#checked.set(path, sha256(write.content));
				return { blocked: false, notes };
			}),
		);
	}

	/** A cell is starting (on `lane`): make sure a baseline snapshot of its directory exists to compare its end with. */
	cellStarted(lane?: string): void {
		const { cwd } = this.#where(lane);
		if (!this.watchesCells || this.#baselines.has(cwd)) return;
		this.#baselines.set(
			cwd,
			this.#snapshot(cwd).catch(() => undefined),
		);
	}

	/** Take the first baseline now (at session start), so the first cell's changes are seen too. */
	start(): Promise<void> {
		this.cellStarted();
		return (this.#baselines.get(this.cwd) ?? Promise.resolve()).then(() => {});
	}

	/**
	 * A cell ended: in the background, diff the workspace against the previous snapshot and give the guards the
	 * files changed by other means than checked writes. Returns at once; `settled()` waits for the work.
	 */
	cellEnded(lane: string): void {
		if (!this.watchesCells) return;
		if (!this.#baselines.has(this.#where(lane).cwd)) {
			// No baseline yet (a guard became interested mid-session): this cell's changes cannot be told apart.
			this.cellStarted(lane);
			return;
		}
		this.#chain = this.#chain.then(() => this.#afterCell(lane)).catch(() => {});
	}

	/** Wait for after-cell checks already started. */
	settled(): Promise<void> {
		return this.#chain;
	}

	/** Findings from after-cell checks not yet shown to `lane`, as one block of text (and forget them). */
	takePending(lane: string): string | undefined {
		const pending = this.#pending.get(lane);
		if (!pending || pending.length === 0) return undefined;
		this.#pending.delete(lane);
		const text = pending.join("\n");
		return text.length > MAX_PENDING_CHARS
			? `${text.slice(0, MAX_PENDING_CHARS)}\n… (${text.length - MAX_PENDING_CHARS} more characters of findings)`
			: text;
	}

	async #afterCell(lane: string): Promise<void> {
		const where = this.#where(lane);
		const before = await this.#baselines.get(where.cwd);
		const after = await this.#snapshot(where.cwd).catch(() => undefined);
		if (after !== undefined) this.#baselines.set(where.cwd, Promise.resolve(after));
		if (before === undefined || after === undefined) return;
		const diff = diffSnapshots(before, after);
		if (diff.changed.length === 0) return;
		const files: string[] = [];
		const checked: string[] = [];
		for (const relativePath of [...diff.added, ...diff.modified]) {
			const path = resolve(after.root, relativePath);
			const approved = this.#checked.get(path);
			if (approved !== undefined) {
				this.#checked.delete(path);
				const current = await readFile(path).catch(() => undefined);
				if (current !== undefined && sha256(current) === approved) {
					checked.push(path);
					continue;
				}
			}
			files.push(path);
		}
		const deleted = diff.deleted.map((relativePath) => resolve(after.root, relativePath));
		for (const path of deleted) this.#checked.delete(path);
		const changes: CellChanges = { files, checked, deleted, complete: diff.complete };
		const guards = this.#guards.filter((guard) => this.#watching(guard));
		const messages = await Promise.all(
			guards.map(async (guard) => {
				const started = this.#now();
				const result = await this.#timed(
					guard.afterTimeoutMs ?? DEFAULT_AFTER_CELL_TIMEOUT_MS,
					undefined,
					(signal) =>
						guard.afterCellChanges!(changes, {
							lane,
							cwd: where.cwd,
							...(where.root === undefined ? {} : { root: where.root }),
							signal,
						}),
				);
				const ms = this.#now() - started;
				this.#charge(guard, ms);
				const stats = this.#stats.get(guard);
				if (stats) stats.afterChecks += 1;
				const text = result.timedOut
					? `${guard.name} did not finish checking the files this cell changed.`
					: result.error !== undefined
						? `${guard.name} could not check the files this cell changed: ${errorText(result.error)}`
						: result.value;
				if (text && stats) stats.afterFindings += 1;
				this.#onRecord?.({
					guard: guard.name,
					phase: "after_cell",
					outcome: result.timedOut || result.error !== undefined ? "unchecked" : text ? "findings" : "clean",
					ms,
					paths: [...files, ...checked].map((path) => this.#display(path, where.cwd)),
					lane,
					...(text ? { detail: text } : {}),
				});
				return text ? `[${guard.name}] ${text}` : undefined;
			}),
		);
		const found = messages.filter((message) => message !== undefined);
		if (found.length === 0) return;
		const pending = this.#pending.get(lane) ?? [];
		pending.push(...found);
		this.#pending.set(lane, pending);
	}
}
