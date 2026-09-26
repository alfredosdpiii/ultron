/**
 * Data model and defensive parsing for the native TUI's live RLM views (see rlm-graph.ts for the rendering).
 *
 * The TUI polls read-only inspection requests (`agents.status {graph: true}`, `instances.list`, `rlm.pool`,
 * `rlm.frames`, `ctx.state`, `progress.assess`) and reads the root turn's `rlm` cells from the replicated
 * transcript. This module parses those payloads into an `RlmSnapshot`.
 */

export type RlmTaskState = "admitted" | "running" | "completed" | "failed" | "cancelled" | "interrupted";

export interface RlmTask {
	readonly id: string;
	readonly definition: string;
	readonly state: string;
	readonly parentId?: string;
	readonly result?: {
		readonly status?: string;
		readonly value?: unknown;
		readonly error?: string;
		/** Bounded result preview (`agents.status {graph: true}`). */
		readonly preview?: string;
	};
	/** Graph fields from `agents.status {graph: true}`; present only for tasks the worker ran this process. */
	readonly lane?: string;
	readonly startedAt?: number;
	readonly endedAt?: number;
	readonly cost?: number;
	readonly tokens?: number;
	readonly input?: string;
	readonly fetch?: string;
	readonly workflow?: RlmWorkflowMembership;
}

export interface RlmWorkflowMembership {
	readonly run: string;
	readonly node: string;
	readonly dependsOn: readonly string[];
	readonly join: string;
}

export interface RlmUsage {
	readonly rootId?: string;
	readonly admittedTasks?: number;
	readonly remainingWallMs?: number | null;
	readonly startedAt?: number | null;
	readonly deadlineAt?: number | null;
	readonly usage?: { readonly cost?: number | null; readonly totalTokens?: number | null; readonly calls?: number };
	readonly reservations?: readonly { readonly taskId?: string; readonly admittedAt?: number }[];
	readonly cost?: { readonly spentUsd?: number; readonly maxCostUsd?: number | null };
	readonly turns?: {
		readonly turns?: number;
		readonly tokens?: number;
		readonly maxTotalTurns?: number | null;
		readonly maxTotalTokens?: number | null;
	};
}

export interface RlmLimits {
	readonly maxAdmittedTasks?: number | null;
	readonly maxWallMs?: number | null;
	readonly maxCostUsd?: number | null;
	readonly maxTotalTokens?: number | null;
	readonly maxTotalTurns?: number | null;
}

export interface RlmPool {
	readonly live: number;
	readonly maxLive: number;
	readonly lanes: readonly {
		readonly lane: string;
		readonly running: number;
		readonly pinnedBy: readonly string[];
		readonly memoryBytes?: number;
	}[];
	readonly evictions?: number;
	/** Summed kernel tree memory, when the worker can read it. */
	readonly memoryBytes?: number;
	readonly memoryCapBytes?: number;
}

/** One bounded inference frame (`rlm.infer`/`rlm.map`), from `rlm.frames`. */
export interface RlmFrame {
	readonly id: string;
	readonly status: string;
	readonly reason?: string;
	readonly task: string;
	readonly spent: { readonly calls: number; readonly tokens: number };
	readonly startedAt?: number;
	readonly endedAt?: number;
	/** The frame's `rlm-frame@1` task (null while queued behind the map's concurrency). */
	readonly taskId?: string | null;
	/** `rlm.map` or `rlm.infer`, and the map's frame count. */
	readonly kind?: string;
	readonly batch?: number;
	/** Task whose cell asked for the frame (null: the root lane). */
	readonly callerTaskId?: string | null;
	readonly lane?: string | null;
	/** The shared budget node (one per `rlm.map`/`rlm.infer` call) and its limits. */
	readonly budget?: {
		readonly id: string;
		readonly calls: number | null;
		readonly tokens: number | null;
		readonly depth: number;
	} | null;
}

/** One host-owned shell job (`bash(cmd, yield_after=...)`), from `agents.status`. */
export interface RlmJob {
	readonly id: string;
	readonly status: string;
	readonly command: string;
	readonly exitCode: number | null;
	readonly startedAt?: number;
	readonly endedAt?: number | null;
	/** Lane that started the job ("main" for the root); a child task owns it through its lane. */
	readonly lane?: string;
	/** Last output, bounded. */
	readonly tail?: string;
}

export interface RlmRootCell {
	readonly toolCallId: string;
	readonly code: string;
	readonly status: "running" | "ok" | "error";
	readonly startedAt?: number;
	readonly endedAt?: number;
	/** First part of the cell's text output (used for the error line). */
	readonly output?: string;
}

export interface RlmProgress {
	readonly classification: string;
	readonly receipts: number;
}

export interface RlmTiming {
	readonly startedAt?: number;
	readonly endedAt?: number;
}

export interface RlmSnapshot {
	readonly now: number;
	readonly tasks: readonly RlmTask[];
	readonly usage?: RlmUsage | null;
	readonly limits?: RlmLimits | null;
	readonly pool?: RlmPool | null;
	readonly rootCell?: RlmRootCell | null;
	/** Task ids that belong to an open retained instance. */
	readonly retained?: ReadonlySet<string>;
	readonly progress?: ReadonlyMap<string, RlmProgress>;
	readonly timing?: ReadonlyMap<string, RlmTiming>;
	/** Recent inference frames, newest first. */
	readonly frames?: readonly RlmFrame[];
	/** Host-owned shell jobs, newest first. */
	readonly jobs?: readonly RlmJob[];
	/** Last inspection failure, shown instead of stale data being mistaken for live data. */
	readonly error?: string;
	/** What the root model forgot, pinned, or noted in its own context (`ctx.state`). */
	readonly context?: RlmContextState | null;
	/** Root `rlm` cells of the current turn, oldest first (from the transcript). */
	readonly cells?: readonly RlmRootCell[];
	/** The current root turn: when its user message arrived, and a preview of it. */
	readonly turn?: { readonly startedAt?: number; readonly prompt?: string } | null;
	/** Tasks the worker left out of the bounded graph listing. */
	readonly truncatedTasks?: number;
}

/** Color hooks; the TUI passes the theme, tests pass identity functions. */
export interface RlmStyle {
	fg(
		color: "accent" | "success" | "error" | "warning" | "muted" | "dim" | "text" | "toolTitle" | "mdCodeBlock",
		text: string,
	): string;
	bold(text: string): string;
}

export const PLAIN_STYLE: RlmStyle = { fg: (_color, text) => text, bold: (text) => text };

const TERMINAL = new Set(["completed", "failed", "cancelled", "interrupted"]);

export function isActiveState(state: string): boolean {
	return !TERMINAL.has(state);
}

function oneLine(text: string): string {
	const flat = text.replace(/\s+/g, " ").trim();
	return flat.length > 200 ? `${flat.slice(0, 199)}…` : flat;
}

export function shortId(id: string): string {
	return id.replace(/^ultron-(?:task|instance)-/, "").slice(0, 8);
}

export function formatDuration(ms: number): string {
	if (ms < 1000) return `${Math.round(ms)}ms`;
	const seconds = ms / 1000;
	if (seconds < 10) return `${seconds.toFixed(1)}s`;
	if (seconds < 60) return `${Math.floor(seconds)}s`;
	const minutes = Math.floor(seconds / 60);
	if (minutes < 60) {
		const rest = Math.floor(seconds % 60);
		return rest === 0 || minutes >= 10 ? `${minutes}m` : `${minutes}m${rest}s`;
	}
	const hours = Math.floor(minutes / 60);
	const restMinutes = minutes % 60;
	return restMinutes === 0 ? `${hours}h` : `${hours}h${restMinutes}m`;
}

/**
 * Tracks first-seen and finished times. Task records carry no timestamps, so a task's start is its
 * usage reservation's admission time when available, else when the panel first saw it active; a
 * task first seen already terminal has no elapsed time rather than a misleading one.
 */
export class RlmClock {
	readonly #marks = new Map<string, { startedAt?: number; endedAt?: number }>();

	mark(key: string, active: boolean, now: number, knownStart?: number): RlmTiming {
		let mark = this.#marks.get(key);
		if (mark === undefined) {
			mark = active ? { startedAt: knownStart ?? now } : {};
			this.#marks.set(key, mark);
		} else if (active) {
			if (knownStart !== undefined) mark.startedAt = knownStart;
			mark.startedAt ??= now;
			mark.endedAt = undefined;
		} else if (mark.startedAt !== undefined && mark.endedAt === undefined) {
			mark.endedAt = now;
		}
		return { ...mark };
	}

	timings(tasks: readonly RlmTask[], usage: RlmUsage | null | undefined, now: number): Map<string, RlmTiming> {
		const admitted = new Map<string, number>();
		for (const reservation of usage?.reservations ?? []) {
			if (reservation.taskId !== undefined && typeof reservation.admittedAt === "number") {
				admitted.set(reservation.taskId, reservation.admittedAt);
			}
		}
		const result = new Map<string, RlmTiming>();
		for (const task of tasks) {
			result.set(task.id, this.mark(`task:${task.id}`, isActiveState(task.state), now, admitted.get(task.id)));
		}
		return result;
	}
}

// ---------------------------------------------------------------------------------------------
// Defensive parsing of inspection payloads (they cross a process boundary as JSON).

function record(value: unknown): Record<string, unknown> | undefined {
	return value !== null && typeof value === "object" && !Array.isArray(value)
		? (value as Record<string, unknown>)
		: undefined;
}

function pickStrings(source: Record<string, unknown>, keys: readonly string[]): Record<string, string> {
	const picked: Record<string, string> = {};
	for (const key of keys) if (typeof source[key] === "string") picked[key] = source[key] as string;
	return picked;
}

function pickNumbers(source: Record<string, unknown>, keys: readonly string[]): Record<string, number> {
	const picked: Record<string, number> = {};
	for (const key of keys) {
		const item = source[key];
		if (typeof item === "number" && Number.isFinite(item)) picked[key] = item;
	}
	return picked;
}

export function parseAgentsStatus(value: unknown): {
	truncatedTasks?: number;
	tasks: RlmTask[];
	usage: RlmUsage | null;
	limits: RlmLimits | null;
	jobs: RlmJob[];
} {
	const body = record(value);
	const tasks: RlmTask[] = [];
	for (const item of Array.isArray(body?.tasks) ? body.tasks : []) {
		const task = record(item);
		if (task === undefined || typeof task.id !== "string" || typeof task.state !== "string") continue;
		const result = record(task.result);
		const workflow = record(task.workflow);
		tasks.push({
			id: task.id,
			definition: typeof task.definition === "string" ? task.definition : "task",
			state: task.state,
			...(typeof task.parentId === "string" ? { parentId: task.parentId } : {}),
			...(result === undefined
				? {}
				: {
						result: {
							...(typeof result.status === "string" ? { status: result.status } : {}),
							...("value" in result ? { value: result.value } : {}),
							...(typeof result.error === "string" ? { error: result.error } : {}),
							...(typeof result.preview === "string" ? { preview: result.preview } : {}),
						},
					}),
			...pickStrings(task, ["lane", "input", "fetch"]),
			...pickNumbers(task, ["startedAt", "endedAt", "cost", "tokens"]),
			...(workflow !== undefined && typeof workflow.run === "string" && typeof workflow.node === "string"
				? {
						workflow: {
							run: workflow.run,
							node: workflow.node,
							dependsOn: Array.isArray(workflow.dependsOn)
								? workflow.dependsOn.filter((item): item is string => typeof item === "string")
								: [],
							join: typeof workflow.join === "string" ? workflow.join : "all",
						},
					}
				: {}),
		});
	}
	const usage = record(body?.usage);
	const limits = record(body?.limits) ?? record(usage?.limits);
	const jobs: RlmJob[] = [];
	for (const item of Array.isArray(body?.jobs) ? body.jobs : []) {
		const job = record(item);
		if (job === undefined || typeof job.id !== "string" || typeof job.status !== "string") continue;
		jobs.push({
			id: job.id,
			status: job.status,
			command: typeof job.command === "string" ? job.command : "",
			exitCode: typeof job.exitCode === "number" ? job.exitCode : null,
			...(typeof job.startedAt === "number" ? { startedAt: job.startedAt } : {}),
			...(typeof job.endedAt === "number" ? { endedAt: job.endedAt } : {}),
			...pickStrings(job, ["lane", "tail"]),
		});
	}
	return {
		...(typeof body?.truncatedTasks === "number" ? { truncatedTasks: body.truncatedTasks } : {}),
		tasks,
		usage: (usage as RlmUsage | undefined) ?? null,
		limits: (limits as RlmLimits | undefined) ?? null,
		jobs,
	};
}

/** Task ids owned by open retained instances (the retaining task and each invocation). */
export function parseRetained(value: unknown): Set<string> {
	const retained = new Set<string>();
	for (const item of Array.isArray(value) ? value : []) {
		const instance = record(item);
		if (instance === undefined || instance.state !== "open") continue;
		if (typeof instance.task_id === "string") retained.add(instance.task_id);
		for (const invocation of Array.isArray(instance.invocations) ? instance.invocations : []) {
			const taskId = record(invocation)?.task_id;
			if (typeof taskId === "string") retained.add(taskId);
		}
	}
	return retained;
}

export function parsePool(value: unknown): RlmPool | null {
	const body = record(value);
	if (body === undefined || typeof body.live !== "number" || typeof body.maxLive !== "number") return null;
	const lanes: { lane: string; running: number; pinnedBy: string[]; memoryBytes?: number }[] = [];
	for (const item of Array.isArray(body.lanes) ? body.lanes : []) {
		const lane = record(item);
		if (lane === undefined || typeof lane.lane !== "string") continue;
		lanes.push({
			lane: lane.lane,
			running: typeof lane.running === "number" ? lane.running : 0,
			pinnedBy: Array.isArray(lane.pinnedBy) ? lane.pinnedBy.filter((pin) => typeof pin === "string") : [],
			...pickNumbers(lane, ["memoryBytes"]),
		});
	}
	return {
		live: body.live,
		maxLive: body.maxLive,
		lanes,
		...(typeof body.evictions === "number" ? { evictions: body.evictions } : {}),
		...pickNumbers(body, ["memoryBytes", "memoryCapBytes"]),
	};
}

export function parseFrames(value: unknown): RlmFrame[] {
	const frames: RlmFrame[] = [];
	for (const item of Array.isArray(record(value)?.frames) ? (record(value)!.frames as unknown[]) : []) {
		const frame = record(item);
		if (frame === undefined || typeof frame.id !== "string" || typeof frame.status !== "string") continue;
		const spent = record(frame.spent);
		const budget = record(frame.budget);
		frames.push({
			id: frame.id,
			status: frame.status,
			...(typeof frame.reason === "string" ? { reason: frame.reason } : {}),
			task: typeof frame.task === "string" ? frame.task : "",
			spent: {
				calls: typeof spent?.calls === "number" ? spent.calls : 0,
				tokens: typeof spent?.tokens === "number" ? spent.tokens : 0,
			},
			...(typeof frame.startedAt === "number" ? { startedAt: frame.startedAt } : {}),
			...(typeof frame.endedAt === "number" ? { endedAt: frame.endedAt } : {}),
			...(typeof frame.taskId === "string" || frame.taskId === null ? { taskId: frame.taskId } : {}),
			...(typeof frame.callerTaskId === "string" || frame.callerTaskId === null
				? { callerTaskId: frame.callerTaskId }
				: {}),
			...pickStrings(frame, ["kind", "lane"]),
			...pickNumbers(frame, ["batch"]),
			...(budget !== undefined && typeof budget.id === "string"
				? {
						budget: {
							id: budget.id,
							calls: typeof budget.calls === "number" ? budget.calls : null,
							tokens: typeof budget.tokens === "number" ? budget.tokens : null,
							depth: typeof budget.depth === "number" ? budget.depth : 0,
						},
					}
				: {}),
		});
	}
	return frames;
}

export function parseProgress(value: unknown): RlmProgress | undefined {
	const body = record(value);
	if (body === undefined || typeof body.classification !== "string") return undefined;
	return { classification: body.classification, receipts: Array.isArray(body.receipts) ? body.receipts.length : 0 };
}

// ---------------------------------------------------------------------------------------------
// Root kernel cell from the replicated main-lane transcript.

interface TranscriptLike {
	readonly transcript: readonly unknown[];
	readonly operation?: {
		readonly runningTools?: readonly {
			readonly status: string;
			readonly toolCallId: string;
			readonly toolName: string;
			readonly args?: unknown;
			readonly result?: unknown;
			readonly isError?: boolean;
		}[];
	} | null;
}

function codeOf(args: unknown): string {
	const code = record(args)?.code;
	return typeof code === "string" ? code : "";
}

function textOf(result: unknown): string | undefined {
	const content = record(result)?.content;
	if (!Array.isArray(content)) return undefined;
	const text = content
		.map((part) => {
			const item = record(part);
			return item?.type === "text" && typeof item.text === "string" ? item.text : "";
		})
		.join("");
	return text.length > 0 ? text : undefined;
}

/**
 * The current or last root `rlm` cell: a running tool from the open operation first, else the last
 * `rlm` tool result in the transcript paired with its call. `clock` supplies start times for running
 * cells, which carry no timestamp of their own.
 */
export function extractRootCell(
	snapshot: TranscriptLike | undefined,
	clock: RlmClock,
	now: number,
): RlmRootCell | null {
	if (snapshot === undefined) return null;
	const running = [...(snapshot.operation?.runningTools ?? [])].reverse().find((tool) => tool.toolName === "rlm");
	if (running !== undefined) {
		const active = running.status === "running";
		const timing = clock.mark(`cell:${running.toolCallId}`, active, now);
		const output = active ? undefined : textOf(running.result);
		return {
			toolCallId: running.toolCallId,
			code: codeOf(running.args),
			status: active ? "running" : running.isError ? "error" : "ok",
			...(timing.startedAt === undefined ? {} : { startedAt: timing.startedAt }),
			...(timing.endedAt === undefined ? {} : { endedAt: timing.endedAt }),
			...(output === undefined ? {} : { output }),
		};
	}
	const entries = snapshot.transcript;
	for (let index = entries.length - 1; index >= 0; index--) {
		const message = record(record(entries[index])?.message);
		if (message?.role !== "toolResult" || message.toolName !== "rlm" || typeof message.toolCallId !== "string")
			continue;
		const toolCallId = message.toolCallId;
		let code = "";
		let startedAt: number | undefined;
		for (let back = index - 1; back >= 0; back--) {
			const assistant = record(record(entries[back])?.message);
			if (assistant?.role !== "assistant" || !Array.isArray(assistant.content)) continue;
			const call = assistant.content.map(record).find((part) => part?.type === "toolCall" && part.id === toolCallId);
			if (call === undefined) continue;
			code = codeOf(call.arguments);
			if (typeof assistant.timestamp === "number") startedAt = assistant.timestamp;
			break;
		}
		// Prefer the times observed live; fall back to message timestamps for cells from before the panel opened.
		const observed = clock.mark(`cell:${toolCallId}`, false, now);
		const endedAt = observed.endedAt ?? (typeof message.timestamp === "number" ? message.timestamp : undefined);
		const start = observed.startedAt ?? startedAt;
		const output = textOf(message);
		return {
			toolCallId,
			code,
			status: message.isError === true ? "error" : "ok",
			...(start === undefined ? {} : { startedAt: start }),
			...(endedAt === undefined ? {} : { endedAt }),
			...(output === undefined ? {} : { output }),
		};
	}
	return null;
}

/**
 * The current root turn from the transcript: the last user message (its time and a preview) and every root `rlm`
 * cell after it, oldest first, with running cells from the open operation. Bounded to the newest `maxCells`.
 */
export function extractTurn(
	snapshot: TranscriptLike | undefined,
	clock: RlmClock,
	now: number,
	maxCells = 30,
): { turn: { startedAt?: number; prompt?: string } | null; cells: RlmRootCell[] } {
	if (snapshot === undefined) return { turn: null, cells: [] };
	const entries = snapshot.transcript;
	let start = -1;
	for (let index = entries.length - 1; index >= 0; index--) {
		if (record(record(entries[index])?.message)?.role === "user") {
			start = index;
			break;
		}
	}
	let turn: { startedAt?: number; prompt?: string } | null = null;
	if (start >= 0) {
		const entry = record(entries[start]);
		const message = record(entry?.message);
		const content = message?.content;
		const text =
			typeof content === "string"
				? content
				: Array.isArray(content)
					? content
							.map((part) => {
								const item = record(part);
								return item?.type === "text" && typeof item.text === "string" ? item.text : "";
							})
							.join(" ")
					: "";
		const at = typeof entry?.timestamp === "number" ? entry.timestamp : message?.timestamp;
		turn = { ...(typeof at === "number" ? { startedAt: at } : {}), prompt: oneLine(text) };
	}
	const cells = new Map<string, RlmRootCell>();
	for (const item of entries.slice(start + 1)) {
		const message = record(record(item)?.message);
		if (message?.role === "assistant" && Array.isArray(message.content)) {
			for (const part of message.content.map(record)) {
				if (part?.type !== "toolCall" || part.name !== "rlm" || typeof part.id !== "string") continue;
				const observed = clock.mark(`cell:${part.id}`, false, now);
				const startedAt =
					observed.startedAt ?? (typeof message.timestamp === "number" ? message.timestamp : undefined);
				cells.set(part.id, {
					toolCallId: part.id,
					code: codeOf(part.arguments),
					status: "running",
					...(startedAt === undefined ? {} : { startedAt }),
				});
			}
		} else if (
			message?.role === "toolResult" &&
			message.toolName === "rlm" &&
			typeof message.toolCallId === "string"
		) {
			const cell = cells.get(message.toolCallId);
			const observed = clock.mark(`cell:${message.toolCallId}`, false, now);
			const endedAt = observed.endedAt ?? (typeof message.timestamp === "number" ? message.timestamp : undefined);
			const output = textOf(message);
			cells.set(message.toolCallId, {
				toolCallId: message.toolCallId,
				code: cell?.code ?? "",
				status: message.isError === true ? "error" : "ok",
				...(cell?.startedAt === undefined ? {} : { startedAt: cell.startedAt }),
				...(endedAt === undefined ? {} : { endedAt }),
				...(output === undefined ? {} : { output }),
			});
		}
	}
	for (const tool of snapshot.operation?.runningTools ?? []) {
		if (tool.toolName !== "rlm") continue;
		const active = tool.status === "running";
		const timing = clock.mark(`cell:${tool.toolCallId}`, active, now);
		const known = cells.get(tool.toolCallId);
		const output = active ? undefined : textOf(tool.result);
		cells.set(tool.toolCallId, {
			toolCallId: tool.toolCallId,
			code: codeOf(tool.args) || (known?.code ?? ""),
			status: active ? "running" : tool.isError ? "error" : "ok",
			...((timing.startedAt ?? known?.startedAt) === undefined
				? {}
				: { startedAt: (timing.startedAt ?? known?.startedAt) as number }),
			...(timing.endedAt === undefined ? {} : { endedAt: timing.endedAt }),
			...(output === undefined ? {} : { output }),
		});
	}
	// A call without a result after the operation ended was cut off (abort); it is no longer running.
	const operationOpen = snapshot.operation !== null && snapshot.operation !== undefined;
	const list = [...cells.values()].map((cell) =>
		cell.status === "running" && !operationOpen
			? { ...cell, status: "error" as const, output: cell.output ?? "interrupted" }
			: cell,
	);
	return { turn, cells: list.slice(-maxCells) };
}

/** Root context control state (`ctx.state`): forgotten, pinned, and noted items. */
export interface RlmContextState {
	readonly forgotten: readonly {
		readonly id: string;
		readonly kind?: string;
		readonly preview?: string;
		readonly source?: string;
		readonly reason?: string;
	}[];
	readonly forgottenCount: number;
	readonly collapsedCount: number;
	readonly pinned: readonly { readonly id: string; readonly kind?: string; readonly preview?: string }[];
	readonly pinnedCount: number;
	readonly notes: number;
}

export function parseContextState(value: unknown): RlmContextState | null {
	if (value === null || typeof value !== "object") return null;
	const record = value as Record<string, unknown>;
	const list = (items: unknown) =>
		Array.isArray(items)
			? items.filter(
					(item): item is { id: string } =>
						item !== null && typeof item === "object" && typeof (item as { id?: unknown }).id === "string",
				)
			: [];
	const count = (item: unknown) => (typeof item === "number" && Number.isFinite(item) ? item : 0);
	return {
		forgotten: list(record.forgotten),
		forgottenCount: count(record.forgottenCount),
		collapsedCount: count(record.collapsedCount),
		pinned: list(record.pinned),
		pinnedCount: count(record.pinnedCount),
		notes: count(record.notes),
	};
}

/** Context lines of the panel: a summary line, then the latest forgotten and pinned items (at most three each). */
export function renderContextLines(context: RlmContextState | null | undefined, style: RlmStyle): string[] {
	if (!context) return [];
	const { forgottenCount, collapsedCount, pinnedCount, notes } = context;
	if (forgottenCount + collapsedCount + pinnedCount + notes === 0) return [];
	const lines = [
		style.fg(
			"muted",
			`context: ${forgottenCount} forgotten · ${collapsedCount} collapsed · ${pinnedCount} pinned · ${notes} notes`,
		),
	];
	for (const item of context.forgotten.slice(-3)) {
		const why = item.reason ? ` (${oneLine(item.reason)})` : item.source ? ` (${item.source})` : "";
		lines.push(
			style.fg("dim", `  forgot ${shortId(item.id)} ${item.kind ?? ""} ${oneLine(item.preview ?? "")}${why}`),
		);
	}
	for (const item of context.pinned.slice(-3))
		lines.push(style.fg("dim", `  pinned ${shortId(item.id)} ${item.kind ?? ""} ${oneLine(item.preview ?? "")}`));
	return lines;
}
